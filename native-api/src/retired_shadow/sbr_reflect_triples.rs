#[allow(unused_imports)]
use super::*;

/// SBR wave 3, lane REFLECT (2026-09-29): core reflection. 25 triples over
/// eight classes retire; `Field`'s value accessors are HELD on measured cost,
/// and the object model underneath (the copy door, annotations, `Proxy`) is
/// HELD, each part with a measured reason.
///
/// # What retires, and why the state under it is real
///
/// * **The four generic carriers** (`sun/reflect/generics/reflectiveObjects/
///   {ParameterizedType,TypeVariable,WildcardType,GenericArrayType}Impl`), 15
///   rows: every accessor, `toString` and the inherited `getTypeName`. Under
///   `--real-jdk` every instance is minted on the REAL class with its REAL
///   named fields -- through the JDK's own `make` factories
///   (`generics::real_parameterized_type`, the `GenericArrayTypeImpl` arm) or
///   through `typesig_to_real_type` / `real_wildcard_type`, which write
///   `actualTypeArguments`/`rawType`/`ownerType`, `upperBounds`/`lowerBounds`
///   by name (`class_manager` pins the JDK declaration order). The real
///   bodies are field reads plus the lazy `reifyBounds`, which the natives
///   were re-implementing by hand. Yielding FIXES one row HotSpot disagrees
///   with: `Outer<String>.Inner<Integer>.toString()` renders the owner's
///   arguments (`Outer<java.lang.String>$Inner<java.lang.Integer>`) where the
///   native printed `Outer$Inner<java.lang.Integer>`.
/// * **`Field.setAccessible`, `canAccess`, `getGenericType`**, 3 triples (4
///   registrations: `setAccessible` is registered twice). The real bodies are
///   `AccessibleObject.checkCanSetAccessible` / `verifyAccess` over real
///   fields and the generic-signature parser, and this wave makes the one
///   field they need that the mint did not write REAL, in
///   `lang_class::create_field_object`: `signature` (the JVMS §4.7.9
///   attribute, as `create_method_object` has carried a method's since WP2.1).
///   Without it the real `getGenericType()` erases `Map<String, List<T>>` to
///   `Map` -- exactly why lane L3 held it (`RJdkReflect` "generic field type"
///   under the dial). The same commit writes HotSpot's `trustedFinal`
///   (`final && (static || hidden || record)`, previously hard-coded `false`),
///   which the real accessor factory keys its READ-ONLY accessor on; the
///   accessors below are held, so today it is read only by the yielded
///   `MethodHandle`-based path, but it is the state they need when they go.
///   Every native reader of the accessible flag (`read_*_accessible`,
///   `accessible_override_is_set`) consults the JDK `override` field FIRST,
///   so the real `setAccessible0` is seen by the natives that stay.
/// * **`Method.canAccess`, `getGenericReturnType`, `getGenericParameterTypes`,
///   `getTypeParameters`; `Constructor.canAccess`, `setAccessible`.** The
///   `Method` mint already carries `signature`; the two `setAccessible`/
///   `canAccess` pairs are `AccessibleObject.checkCanSetAccessible` /
///   `verifyAccess` over real fields. Yielding restores HotSpot's
///   `InaccessibleObjectException` message (it names the member and the
///   module: "Unable to make java.lang.invoke.MethodHandles$Lookup(java.lang.Class)
///   accessible: ..." for "Unable to make member accessible: ...").
///   `getGenericParameterTypes` needs the carrier rows above in the SAME
///   table: the real reifier hands back a real `WildcardTypeImpl` whose
///   `upperBounds` are still `FieldTypeSignature` tree nodes, and only the
///   real lazy `getUpperBounds` reifies them (under the dial with the native
///   still bound warm, `? extends U` printed as
///   `sun.reflect.generics.tree.TypeVariableSignature@..`).
/// * **`ReflectionFactory.getExecutableSharedParameterTypes`.** The native
///   worked around a null `langReflectAccess` that cannot happen here: this
///   VM's `SharedSecrets.getJavaLangReflectAccess` always returns the real
///   `ReflectAccess`, whose body is bytecode (`ex.getSharedParameterTypes()`,
///   i.e. the populated `parameterTypes`).
///
/// # HELD, with the measurement that held each
///
/// * **`Field`'s value accessors** -- `get`/`set` and the sixteen typed
///   `getX`/`setX`, 18 triples -- are CORRECT when yielded (every typed row of
///   the probe, widening, narrowing, final, static and null-receiver, matches
///   HotSpot once `trustedFinal` is real; the null receiver's NPE even gains
///   HotSpot's helpful message) and are held on COST: on a trial binary that
///   retired them, the real `MethodHandle`-based field accessor measured
///   2.6-4.0x slower than the native with the JIT and 4.5-6.1x `--nojit`
///   (`getInt` 1.9-2.8 us -> 7.3-7.7 us). They go when the `MethodHandle`
///   field-accessor path is as cheap as the native. SBR wave 5 lane REFLACC
///   linked the field-handle `invokeExact` (`lang_invoke::mh_field_link_fast`:
///   `MethodHandle.invokeExact` on a field getter 4.8-5.9 us -> 1.9-2.2 us)
///   and still HELD them: retired, `getInt` is 1.9-2.5x the native with the
///   JIT and 4.2-5.1x `--nojit`, the rest being `Reflection.getCallerClass`'s
///   full stack capture and the VM's un-memoized signature-polymorphic
///   dispatch -- `docs/feature-designs/native-dispatch-architecture.md`.
/// * **`java/lang/reflect/Proxy`** (4 rows) and `InvocationHandler.
///   invokeDefault`: the VM generates its own `$ProxyN` classes, which the real
///   `ProxyBuilder`'s `reverseProxyCache` has never seen. Dial on `Proxy`:
///   `isProxyClass(proxyClass)` and `getProxyClass(..)` answer `false`,
///   `getInvocationHandler` throws "not a proxy instance", annotation
///   instances stop being proxies; suite `RArrayStoreInterfaces`, `RJdkStrict`,
///   `RJdkProxy`, `RJdkJmx` red (each re-run alone).
/// * **The copy door** -- `ReflectionFactory.copyField/copyMethod/
///   copyConstructor` (native-won in every vector: boot) and
///   `Field/Method/Constructor.copy`. They are IDENTITY: the VM hands out one
///   shared object per member, and its `Method`/`Constructor`/`Field` carry
///   CratonVM extra slots past the JDK layout (descriptor, parameter count,
///   the pre-`override` accessible flag, the `METH` trust marker) that the
///   real `copy()` -- `new Method(...)` through `<init>` -- would not
///   allocate. Retiring the copy door needs those slots gone first.
/// * **Annotations** -- `get(Declared)Annotation(s)(ByType)`,
///   `isAnnotationPresent`, `getParameterAnnotations`, `getDefaultValue`,
///   `getAnnotatedType`/`getAnnotatedParameterTypes` on all three, and the
///   seven annotation getters of `TypeVariableImpl` (`getAnnotatedBounds`
///   included). The mint sites write `annotations`/`parameterAnnotations`/
///   `annotationDefault` null on purpose, and even with bytes the real
///   `AnnotationParser` needs a `jdk.internal.reflect.ConstantPool`, which
///   this VM does not provide (`ReflectionFactory.getConstantPool` answers
///   null -- also held). Dial: every annotation row reads empty/null
///   (`RAnnotationProxyGate`, `RJdkReflect` red). The `TypeVariableImpl`
///   getters are `BAD -> BAD` on an annotated type parameter (HotSpot `[@Tag,
///   @TagU]`, native AND bytecode `[]`), so yielding them buys nothing.
/// * **`Reflection.ensureNativeAccess`**: the real body reads
///   `Module.enableNativeAccess`, which `--enable-native-access=ALL-UNNAMED`
///   never sets here (the grant lives in `panama::NativeAccessPolicy`).
///   Measured on `RJdkJni`: with the flag, the yielded path still prints the
///   JDK's four-line restricted-method WARNING; HotSpot and the native print
///   none.
/// * `Method.invoke`, `Constructor.newInstance`, `*.trySetAccessible`,
///   `*.getRoot`, `*.getParameters`: not native-won in the census and not
///   measured here (L3's `BAD -> BAD` verdict on the first two stands).
///
/// # Measured
///
/// Azure linux, JDK 25.0.4+7, control `cvm-sbr-int2` (sbr-orch `35aa5e5fc`)
/// against this table (`cvm-sbr-reflect-2`):
///
/// ```text
///   regression suite SUITE=all --real-jdk   control 152/0   lane 152/0   per-vector identical
///     native-won triples (census UNION)     639 -> 627 (13 family rows gone, +1 WeakReference.refersTo)
///   SbrW3Reflect vs HotSpot (146 rows)      control 13 differ   lane 10 (3 fixed, 0 broken); drift 0,
///                                           --nojit identical; unretire == control; --compatible == control
///   strict corpus                           same 4 real divergences as control
///   SbrW3ReflectBench (ns/op, ranges)       pt accessors 1.1-1.5x, pt.toString 1.8-3.2x JIT / 4-8x --nojit
/// ```
///
/// The record below has the per-arm detail.
///
/// docs/feature-designs/native-dispatch-architecture.md
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_REFLECT_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/reflect/Constructor",
        "canAccess",
        "(Ljava/lang/Object;)Z",
    ),
    ("java/lang/reflect/Constructor", "setAccessible", "(Z)V"),
    (
        "java/lang/reflect/Field",
        "canAccess",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "java/lang/reflect/Field",
        "getGenericType",
        "()Ljava/lang/reflect/Type;",
    ),
    ("java/lang/reflect/Field", "setAccessible", "(Z)V"),
    (
        "java/lang/reflect/Method",
        "canAccess",
        "(Ljava/lang/Object;)Z",
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
        "jdk/internal/reflect/ReflectionFactory",
        "getExecutableSharedParameterTypes",
        "(Ljava/lang/reflect/Executable;)[Ljava/lang/Class;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/GenericArrayTypeImpl",
        "getGenericComponentType",
        "()Ljava/lang/reflect/Type;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/ParameterizedTypeImpl",
        "getActualTypeArguments",
        "()[Ljava/lang/reflect/Type;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/ParameterizedTypeImpl",
        "getOwnerType",
        "()Ljava/lang/reflect/Type;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/ParameterizedTypeImpl",
        "getRawType",
        "()Ljava/lang/Class;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/ParameterizedTypeImpl",
        "getTypeName",
        "()Ljava/lang/String;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/ParameterizedTypeImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/TypeVariableImpl",
        "getBounds",
        "()[Ljava/lang/reflect/Type;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/TypeVariableImpl",
        "getName",
        "()Ljava/lang/String;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/TypeVariableImpl",
        "getTypeName",
        "()Ljava/lang/String;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/WildcardTypeImpl",
        "getLowerBounds",
        "()[Ljava/lang/reflect/Type;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/WildcardTypeImpl",
        "getTypeName",
        "()Ljava/lang/String;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/WildcardTypeImpl",
        "getUpperBounds",
        "()[Ljava/lang/reflect/Type;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/WildcardTypeImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
];

/// SBR wave 2, lane JCA2 (2026-09-29): the provider side of the JCA, and the
/// engines wave 1 held behind it. 73 triples, 73 registrations, six classes:
/// `Provider` (21), `Provider$Service` (5), `KeyFactory` (10),
/// `KeyPairGenerator` (12), `Signature` (22, including its no-op `<clinit>`)
/// and `sun/security/util/SignatureUtil` (3).
///
/// # Why it is safe now and was not in wave 1
///
/// Wave 1 measured the blocker: the providers were `make_provider` allocations
/// whose service map was a native side table carrying `.Native` marker class
/// names, so a real engine met a `Service` it could not instantiate. Wave 1
/// retired `Security` and `GetInstance`, which made `Security.getProvider(s)`
/// the JDK's own constructed providers (`sun.security.provider.Sun`, ...).
/// Retiring the `Provider` family here makes their service maps real too:
/// `putService` keeps the provider's OWN `Service` objects (SunEC's
/// `ProviderService` subclass with its `newInstance` override, attributes
/// included), so the dotted SunEC labels, `supportsParameter` and
/// `getAttribute` all answer as on HotSpot. What wave 1 held for exactly that
/// reason -- `KeyPairGenerator`, `KeyFactory`, `Signature` -- then runs its
/// real bytecode over real SPIs.
///
/// Two readers in `native-builtins/src/jca/provider_chain.rs` keep the engines
/// that STAY native (`Cipher`, `MessageDigest`, `Mac`, `KeyAgreement`, `KEM`,
/// `KeyStore(String, Provider)`) coherent with that, both keyed on these rows
/// being retired (so `CRATONVM_UNRETIRE_NATIVE_SHADOW` turns them off too):
/// `make_provider` answers the INSTALLED provider object, and the native
/// `getInstance`s mirror the JDK's provider list and every third-party
/// provider's services before resolving (`sync_chain_from_real`; skipped while
/// `Providers.providerList` is the same object). Without the mirror, an
/// application's `Security.addProvider` -- real bytecode since wave 1 -- was
/// invisible to every native engine (`SbrW1Jca` `tp.all`, a wave-1 regression
/// under `--real-jdk`).
///
/// # Measured (JDK 25.0.4, linux; control `cvm-sbr-int1`, final `cvm-sbr-jca2-4`)
///
/// ```text
///   suite (152):   control 152/152; jca2-4 152/152 (jca2-1 and jca2-3 too)
///   dial screen:   int1 + these prefixes 146/152 (+6: RCrypto RSslLiveSession
///                  RSslEndpointIdentification RBigIntMontgomery RJdkSecurity
///                  RJdkX509Intercept) -- pessimistic: the dial cannot drop the
///                  no-op Signature.<clinit>; the real retirement passes all 152
///   SbrW1Jca probe (151 lines) vs HotSpot 25, drift 0 in every arm:
///     int1 --real-jdk 16   ->  jca2-4 --real-jdk 1 (MessageDigest's class name)
///     jca2-4 + UNRETIRE=<the six>   = int1 except five lines, all moved TO
///                                     HotSpot by both-mode native fixes
///     --compatible: those same five lines moved to HotSpot, nothing else
/// ```
///
/// # Held, with the measured reason
///
/// * `java/security/MessageDigest` (17) and `javax/crypto/Mac` (17 triples):
///   correct retired (trial binary `cvm-sbr-jca2-1`, suite 152/152, probe 1
///   line) and far too slow: SHA-256 276-293 ms/MiB against the native's
///   4.1-4.4 (65x), HMAC-SHA256 280-302 against 26 (11x), a 64-byte
///   `getInstance`+`digest` 50 us against 3.4 (15x). `SHA2.implCompress0` IS
///   JIT-compiled; the cost is in the compiled body (see the lane record). A
///   reviewed `Intrinsic` for `SHA2.implCompress0` / `DigestBase.
///   implCompressMultiBlock0` (the methods HotSpot intrinsifies) is what would
///   let these go.
/// * `javax/crypto/Cipher` stays held (wave 1, AES-GCM 4x; user decision).
/// * `Provider.parseLegacyPut`, `Provider$ServiceKey.<clinit>`,
///   `Provider$EngineDescription.<clinit>`: declared on no image, nothing to
///   yield to.
///
/// Record: `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_JCA2_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/security/KeyFactory", "<clinit>", "()V"),
    ("java/security/KeyFactory", "generatePrivate", "(Ljava/security/spec/KeySpec;)Ljava/security/PrivateKey;"),
    ("java/security/KeyFactory", "generatePublic", "(Ljava/security/spec/KeySpec;)Ljava/security/PublicKey;"),
    ("java/security/KeyFactory", "getAlgorithm", "()Ljava/lang/String;"),
    ("java/security/KeyFactory", "getInstance", "(Ljava/lang/String;)Ljava/security/KeyFactory;"),
    ("java/security/KeyFactory", "getInstance", "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/KeyFactory;"),
    ("java/security/KeyFactory", "getInstance", "(Ljava/lang/String;Ljava/security/Provider;)Ljava/security/KeyFactory;"),
    ("java/security/KeyFactory", "getKeySpec", "(Ljava/security/Key;Ljava/lang/Class;)Ljava/security/spec/KeySpec;"),
    ("java/security/KeyFactory", "getProvider", "()Ljava/security/Provider;"),
    ("java/security/KeyFactory", "translateKey", "(Ljava/security/Key;)Ljava/security/Key;"),
    ("java/security/KeyPairGenerator", "<clinit>", "()V"),
    ("java/security/KeyPairGenerator", "genKeyPair", "()Ljava/security/KeyPair;"),
    ("java/security/KeyPairGenerator", "generateKeyPair", "()Ljava/security/KeyPair;"),
    ("java/security/KeyPairGenerator", "getAlgorithm", "()Ljava/lang/String;"),
    ("java/security/KeyPairGenerator", "getInstance", "(Ljava/lang/String;)Ljava/security/KeyPairGenerator;"),
    ("java/security/KeyPairGenerator", "getInstance", "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/KeyPairGenerator;"),
    ("java/security/KeyPairGenerator", "getInstance", "(Ljava/lang/String;Ljava/security/Provider;)Ljava/security/KeyPairGenerator;"),
    ("java/security/KeyPairGenerator", "getProvider", "()Ljava/security/Provider;"),
    ("java/security/KeyPairGenerator", "initialize", "(I)V"),
    ("java/security/KeyPairGenerator", "initialize", "(ILjava/security/SecureRandom;)V"),
    ("java/security/KeyPairGenerator", "initialize", "(Ljava/security/spec/AlgorithmParameterSpec;)V"),
    ("java/security/KeyPairGenerator", "initialize", "(Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)V"),
    ("java/security/Provider", "containsKey", "(Ljava/lang/Object;)Z"),
    ("java/security/Provider", "elements", "()Ljava/util/Enumeration;"),
    ("java/security/Provider", "entrySet", "()Ljava/util/Set;"),
    ("java/security/Provider", "get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
    ("java/security/Provider", "getInfo", "()Ljava/lang/String;"),
    ("java/security/Provider", "getName", "()Ljava/lang/String;"),
    ("java/security/Provider", "getProperty", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("java/security/Provider", "getService", "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/Provider$Service;"),
    ("java/security/Provider", "getServices", "()Ljava/util/Set;"),
    ("java/security/Provider", "getVersion", "()D"),
    ("java/security/Provider", "getVersionStr", "()Ljava/lang/String;"),
    ("java/security/Provider", "isEmpty", "()Z"),
    ("java/security/Provider", "keySet", "()Ljava/util/Set;"),
    ("java/security/Provider", "keys", "()Ljava/util/Enumeration;"),
    ("java/security/Provider", "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
    ("java/security/Provider", "putId", "()V"),
    ("java/security/Provider", "putService", "(Ljava/security/Provider$Service;)V"),
    ("java/security/Provider", "size", "()I"),
    ("java/security/Provider", "stringPropertyNames", "()Ljava/util/Set;"),
    ("java/security/Provider", "toString", "()Ljava/lang/String;"),
    ("java/security/Provider", "values", "()Ljava/util/Collection;"),
    ("java/security/Provider$Service", "<init>", "(Ljava/security/Provider;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/util/List;Ljava/util/Map;)V"),
    ("java/security/Provider$Service", "getAttribute", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("java/security/Provider$Service", "getClassName", "()Ljava/lang/String;"),
    ("java/security/Provider$Service", "newInstance", "(Ljava/lang/Object;)Ljava/lang/Object;"),
    ("java/security/Provider$Service", "toString", "()Ljava/lang/String;"),
    ("java/security/Signature", "<clinit>", "()V"),
    ("java/security/Signature", "getAlgorithm", "()Ljava/lang/String;"),
    ("java/security/Signature", "getInstance", "(Ljava/lang/String;)Ljava/security/Signature;"),
    ("java/security/Signature", "getInstance", "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/Signature;"),
    ("java/security/Signature", "getInstance", "(Ljava/lang/String;Ljava/security/Provider;)Ljava/security/Signature;"),
    ("java/security/Signature", "getParameter", "(Ljava/lang/String;)Ljava/lang/Object;"),
    ("java/security/Signature", "getParameters", "()Ljava/security/AlgorithmParameters;"),
    ("java/security/Signature", "getProvider", "()Ljava/security/Provider;"),
    ("java/security/Signature", "initSign", "(Ljava/security/PrivateKey;)V"),
    ("java/security/Signature", "initSign", "(Ljava/security/PrivateKey;Ljava/security/SecureRandom;)V"),
    ("java/security/Signature", "initVerify", "(Ljava/security/PublicKey;)V"),
    ("java/security/Signature", "initVerify", "(Ljava/security/cert/Certificate;)V"),
    ("java/security/Signature", "setParameter", "(Ljava/lang/String;Ljava/lang/Object;)V"),
    ("java/security/Signature", "setParameter", "(Ljava/security/spec/AlgorithmParameterSpec;)V"),
    ("java/security/Signature", "sign", "()[B"),
    ("java/security/Signature", "sign", "([BII)I"),
    ("java/security/Signature", "update", "(B)V"),
    ("java/security/Signature", "update", "(Ljava/nio/ByteBuffer;)V"),
    ("java/security/Signature", "update", "([B)V"),
    ("java/security/Signature", "update", "([BII)V"),
    ("java/security/Signature", "verify", "([B)Z"),
    ("java/security/Signature", "verify", "([BII)Z"),
    ("sun/security/util/SignatureUtil", "initSignWithParam", "(Ljava/security/Signature;Ljava/security/PrivateKey;Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)V"),
    ("sun/security/util/SignatureUtil", "initVerifyWithParam", "(Ljava/security/Signature;Ljava/security/PublicKey;Ljava/security/spec/AlgorithmParameterSpec;)V"),
    ("sun/security/util/SignatureUtil", "initVerifyWithParam", "(Ljava/security/Signature;Ljava/security/cert/Certificate;Ljava/security/spec/AlgorithmParameterSpec;)V"),
];

/// SBR wave 2, lane `fs`, 2026-09-28/29 -- the Linux `sun/nio/fs` family: 75
/// triples on `UnixPath` (20), `UnixFileAttributes` (9, the Linux arm
/// `the_unix_attribute_carrier_is_retired_with_its_linux_arm` used to demand),
/// `LinuxFileSystemProvider` and `UnixFileSystemProvider` (18 each),
/// `UnixDirectoryStream` (3) and `UnixSecureDirectoryStream` (7).
///
/// # Why the state is real now
///
/// * **`UnixPath`.** Every native producer mints it concrete and fills `fs`
///   (the real `LinuxFileSystem` singleton, built by its own constructor),
///   `path` (normalized bytes) and `stringValue` (`path_layout`). Real bytecode
///   producers (`FileSystem.getPath`, `resolve`, ...) leave `stringValue` null
///   until `toString()`: `p57_path_slot_string` now decodes `path` for those
///   under `--real-jdk` instead of answering `None` -- the reader that owned
///   both ends of the convention.
/// * **`UnixFileAttributes`.** Real `stat0`/`lstat0`/`fstat0`/`fstatat0` fill
///   every field (`unix_attrs_fill`), and under `--real-jdk` the native
///   `p59_files_read_attributes` fills through the same function, so no
///   producer is left writing a dir/regular-only `st_mode`, millisecond times or
///   `ctime = mtime`. `init()` claims `SUPPORTS_BIRTHTIME` with the `statx`
///   birth time filled, as HotSpot 25 does on Linux.
/// * **The providers.** The provider object is minted without `<init>`, but
///   its one real field (`theFileSystem`) is linked; every retired body
///   reads only that and the Path. The layer below is the genuine
///   `UnixNativeDispatcher`/`LinuxNativeDispatcher` natives, registered (both modes)
///   for exactly the calls the real bodies make (`opendir0`/
///   `readdir0`/`closedir`, the `SUPPORTS_OPENAT` set for the secure stream,
///   `futimens0`/`utimensat0`, `fchmod0`/`fchown0`, `directCopy0`/
///   `bufferedCopy0`, the xattr trio `Files.copy(COPY_ATTRIBUTES)` calls).
///   `UnixFileSystemProvider` retires with `LinuxFileSystemProvider` because
///   the Linux bodies call `super.m(..)`, an `invokespecial` its mirrored row
///   caught.
/// * **The directory streams** are built by the real provider through their
///   constructors now; nothing mints one natively under `--real-jdk`.
///
/// The abstract `java/nio/file/spi/FileSystemProvider` and interface
/// `BasicFileAttributes` rows bind the same callbacks and are not this
/// lane's; a call through those symbolic classes still reached the native
/// (measured). Each such callback yields to the receiver's bytecode when the
/// receiver's own triple is retired (`yield_to_retired_receiver`,
/// `native-builtins/src/phases_late/nio_file.rs`), so the unretire switch
/// flips both together.
///
/// # Measured
///
/// Linux/x86_64, JDK 25.0.4+7, control `cvm-sbr-int1` (`a1b75dc2a`):
///
/// ```text
///   SbrW2Fs, 27 rows vs HotSpot 25         control --real-jdk    8 rows differ, drift 600
///                                          this    --real-jdk    0 rows differ, drift 0
///     + UNRETIRE (the six classes, 75 rows) == control but /dev/null isOther (the stat fill)
///     --compatible                          == control --compatible
///   dial pre-screen (int1 + ENFORCE)       147/152: RFileTimes RJdkNio RJdkWatchService
///                                            RJdkAsyncChannel RCrypto RJdkSecurity RJfrRecordingStream
///   SUITE=all --real-jdk                   152/152 both, vector-identical
///   census native-won, sun/nio/fs rows     672 -> 12 (the held rows); all rows 10 701 -> 10 037
///   SbrW2FsPerf path algebra               1.3x-2.3x faster (3 interleaved pairs)
/// ```
///
/// Strict corpus and the rest: the record,
/// `docs/feature-designs/native-dispatch-architecture.md`.
///
/// # Held, each on a named blocker
///
/// * The `WatchService` pipeline -- `LinuxWatchService` (4),
///   `LinuxWatchService$LinuxWatchKey` (5), `LinuxFileSystem.newWatchService`,
///   `UnixPath.register` (2): real `LinuxWatchService.<init>` calls
///   `inotifyInit`, and none of `inotifyInit`/`inotifyAddWatch`/
///   `inotifyRmWatch`/`configureBlocking`/`socketpair`/`poll`/`eventSize`/
///   `eventOffsets` is implemented (the dial run: `RJdkWatchService` dies in
///   `LinuxWatchService.<init>`). `watch.rs` keeps the inotify engine they
///   would be re-pointed at.
/// * `DefaultFileSystemProvider.instance`/`theFileSystem`: real `<clinit>`
///   builds a SECOND `LinuxFileSystemProvider` and `LinuxFileSystem` beside
///   the VM's singleton, splitting the identity `UnixDomainSockets` compares
///   with `!=`. Retirable once the singleton IS the one `<clinit>` builds.
/// * `getFileStore` (real body reads `/proc/mounts` through `setmntent0`/
///   `getmntent0`, unimplemented) and `newAsynchronousFileChannel` (lands on
///   the NIO-channels lane's `SimpleAsynchronousFileChannelImpl`).
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_FS_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "sun/nio/fs/LinuxFileSystemProvider",
        "checkAccess",
        "(Ljava/nio/file/Path;[Ljava/nio/file/AccessMode;)V",
    ),
    (
        "sun/nio/fs/LinuxFileSystemProvider",
        "copy",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)V",
    ),
    (
        "sun/nio/fs/LinuxFileSystemProvider",
        "createDirectory",
        "(Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)V",
    ),
    (
        "sun/nio/fs/LinuxFileSystemProvider",
        "delete",
        "(Ljava/nio/file/Path;)V",
    ),
    (
        "sun/nio/fs/LinuxFileSystemProvider",
        "getFileAttributeView",
        "(Ljava/nio/file/Path;Ljava/lang/Class;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/FileAttributeView;",
    ),
    (
        "sun/nio/fs/LinuxFileSystemProvider",
        "getFileSystem",
        "(Ljava/net/URI;)Ljava/nio/file/FileSystem;",
    ),
    (
        "sun/nio/fs/LinuxFileSystemProvider",
        "getPath",
        "(Ljava/net/URI;)Ljava/nio/file/Path;",
    ),
    (
        "sun/nio/fs/LinuxFileSystemProvider",
        "getScheme",
        "()Ljava/lang/String;",
    ),
    (
        "sun/nio/fs/LinuxFileSystemProvider",
        "isHidden",
        "(Ljava/nio/file/Path;)Z",
    ),
    (
        "sun/nio/fs/LinuxFileSystemProvider",
        "isSameFile",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;)Z",
    ),
    (
        "sun/nio/fs/LinuxFileSystemProvider",
        "move",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)V",
    ),
    (
        "sun/nio/fs/LinuxFileSystemProvider",
        "newByteChannel",
        "(Ljava/nio/file/Path;Ljava/util/Set;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/SeekableByteChannel;",
    ),
    (
        "sun/nio/fs/LinuxFileSystemProvider",
        "newDirectoryStream",
        "(Ljava/nio/file/Path;Ljava/nio/file/DirectoryStream$Filter;)Ljava/nio/file/DirectoryStream;",
    ),
    (
        "sun/nio/fs/LinuxFileSystemProvider",
        "newFileSystem",
        "(Ljava/net/URI;Ljava/util/Map;)Ljava/nio/file/FileSystem;",
    ),
    (
        "sun/nio/fs/LinuxFileSystemProvider",
        "newFileSystem",
        "(Ljava/nio/file/Path;Ljava/util/Map;)Ljava/nio/file/FileSystem;",
    ),
    (
        "sun/nio/fs/LinuxFileSystemProvider",
        "readAttributes",
        "(Ljava/nio/file/Path;Ljava/lang/Class;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/BasicFileAttributes;",
    ),
    (
        "sun/nio/fs/LinuxFileSystemProvider",
        "readAttributes",
        "(Ljava/nio/file/Path;Ljava/lang/String;[Ljava/nio/file/LinkOption;)Ljava/util/Map;",
    ),
    (
        "sun/nio/fs/LinuxFileSystemProvider",
        "setAttribute",
        "(Ljava/nio/file/Path;Ljava/lang/String;Ljava/lang/Object;[Ljava/nio/file/LinkOption;)V",
    ),
    ("sun/nio/fs/UnixDirectoryStream", "close", "()V"),
    (
        "sun/nio/fs/UnixDirectoryStream",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "sun/nio/fs/UnixDirectoryStream",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
    (
        "sun/nio/fs/UnixFileAttributes",
        "creationTime",
        "()Ljava/nio/file/attribute/FileTime;",
    ),
    (
        "sun/nio/fs/UnixFileAttributes",
        "fileKey",
        "()Ljava/lang/Object;",
    ),
    ("sun/nio/fs/UnixFileAttributes", "isDirectory", "()Z"),
    ("sun/nio/fs/UnixFileAttributes", "isOther", "()Z"),
    ("sun/nio/fs/UnixFileAttributes", "isRegularFile", "()Z"),
    ("sun/nio/fs/UnixFileAttributes", "isSymbolicLink", "()Z"),
    (
        "sun/nio/fs/UnixFileAttributes",
        "lastAccessTime",
        "()Ljava/nio/file/attribute/FileTime;",
    ),
    (
        "sun/nio/fs/UnixFileAttributes",
        "lastModifiedTime",
        "()Ljava/nio/file/attribute/FileTime;",
    ),
    ("sun/nio/fs/UnixFileAttributes", "size", "()J"),
    (
        "sun/nio/fs/UnixFileSystemProvider",
        "checkAccess",
        "(Ljava/nio/file/Path;[Ljava/nio/file/AccessMode;)V",
    ),
    (
        "sun/nio/fs/UnixFileSystemProvider",
        "copy",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)V",
    ),
    (
        "sun/nio/fs/UnixFileSystemProvider",
        "createDirectory",
        "(Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)V",
    ),
    (
        "sun/nio/fs/UnixFileSystemProvider",
        "delete",
        "(Ljava/nio/file/Path;)V",
    ),
    (
        "sun/nio/fs/UnixFileSystemProvider",
        "getFileAttributeView",
        "(Ljava/nio/file/Path;Ljava/lang/Class;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/FileAttributeView;",
    ),
    (
        "sun/nio/fs/UnixFileSystemProvider",
        "getFileSystem",
        "(Ljava/net/URI;)Ljava/nio/file/FileSystem;",
    ),
    (
        "sun/nio/fs/UnixFileSystemProvider",
        "getPath",
        "(Ljava/net/URI;)Ljava/nio/file/Path;",
    ),
    (
        "sun/nio/fs/UnixFileSystemProvider",
        "getScheme",
        "()Ljava/lang/String;",
    ),
    (
        "sun/nio/fs/UnixFileSystemProvider",
        "isHidden",
        "(Ljava/nio/file/Path;)Z",
    ),
    (
        "sun/nio/fs/UnixFileSystemProvider",
        "isSameFile",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;)Z",
    ),
    (
        "sun/nio/fs/UnixFileSystemProvider",
        "move",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)V",
    ),
    (
        "sun/nio/fs/UnixFileSystemProvider",
        "newByteChannel",
        "(Ljava/nio/file/Path;Ljava/util/Set;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/SeekableByteChannel;",
    ),
    (
        "sun/nio/fs/UnixFileSystemProvider",
        "newDirectoryStream",
        "(Ljava/nio/file/Path;Ljava/nio/file/DirectoryStream$Filter;)Ljava/nio/file/DirectoryStream;",
    ),
    (
        "sun/nio/fs/UnixFileSystemProvider",
        "newFileSystem",
        "(Ljava/net/URI;Ljava/util/Map;)Ljava/nio/file/FileSystem;",
    ),
    (
        "sun/nio/fs/UnixFileSystemProvider",
        "newFileSystem",
        "(Ljava/nio/file/Path;Ljava/util/Map;)Ljava/nio/file/FileSystem;",
    ),
    (
        "sun/nio/fs/UnixFileSystemProvider",
        "readAttributes",
        "(Ljava/nio/file/Path;Ljava/lang/Class;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/BasicFileAttributes;",
    ),
    (
        "sun/nio/fs/UnixFileSystemProvider",
        "readAttributes",
        "(Ljava/nio/file/Path;Ljava/lang/String;[Ljava/nio/file/LinkOption;)Ljava/util/Map;",
    ),
    (
        "sun/nio/fs/UnixFileSystemProvider",
        "setAttribute",
        "(Ljava/nio/file/Path;Ljava/lang/String;Ljava/lang/Object;[Ljava/nio/file/LinkOption;)V",
    ),
    ("sun/nio/fs/UnixPath", "compareTo", "(Ljava/nio/file/Path;)I"),
    ("sun/nio/fs/UnixPath", "endsWith", "(Ljava/nio/file/Path;)Z"),
    ("sun/nio/fs/UnixPath", "equals", "(Ljava/lang/Object;)Z"),
    ("sun/nio/fs/UnixPath", "getFileName", "()Ljava/nio/file/Path;"),
    (
        "sun/nio/fs/UnixPath",
        "getFileSystem",
        "()Ljava/nio/file/FileSystem;",
    ),
    ("sun/nio/fs/UnixPath", "getName", "(I)Ljava/nio/file/Path;"),
    ("sun/nio/fs/UnixPath", "getNameCount", "()I"),
    ("sun/nio/fs/UnixPath", "getParent", "()Ljava/nio/file/Path;"),
    ("sun/nio/fs/UnixPath", "getRoot", "()Ljava/nio/file/Path;"),
    ("sun/nio/fs/UnixPath", "hashCode", "()I"),
    ("sun/nio/fs/UnixPath", "isAbsolute", "()Z"),
    ("sun/nio/fs/UnixPath", "normalize", "()Ljava/nio/file/Path;"),
    (
        "sun/nio/fs/UnixPath",
        "relativize",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
    ),
    (
        "sun/nio/fs/UnixPath",
        "resolve",
        "(Ljava/nio/file/Path;)Ljava/nio/file/Path;",
    ),
    ("sun/nio/fs/UnixPath", "startsWith", "(Ljava/nio/file/Path;)Z"),
    ("sun/nio/fs/UnixPath", "subpath", "(II)Ljava/nio/file/Path;"),
    ("sun/nio/fs/UnixPath", "toAbsolutePath", "()Ljava/nio/file/Path;"),
    (
        "sun/nio/fs/UnixPath",
        "toRealPath",
        "([Ljava/nio/file/LinkOption;)Ljava/nio/file/Path;",
    ),
    ("sun/nio/fs/UnixPath", "toString", "()Ljava/lang/String;"),
    ("sun/nio/fs/UnixPath", "toUri", "()Ljava/net/URI;"),
    ("sun/nio/fs/UnixSecureDirectoryStream", "close", "()V"),
    (
        "sun/nio/fs/UnixSecureDirectoryStream",
        "deleteDirectory",
        "(Ljava/lang/Object;)V",
    ),
    (
        "sun/nio/fs/UnixSecureDirectoryStream",
        "deleteFile",
        "(Ljava/lang/Object;)V",
    ),
    (
        "sun/nio/fs/UnixSecureDirectoryStream",
        "getFileAttributeView",
        "(Ljava/lang/Object;Ljava/lang/Class;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/FileAttributeView;",
    ),
    (
        "sun/nio/fs/UnixSecureDirectoryStream",
        "iterator",
        "()Ljava/util/Iterator;",
    ),
    (
        "sun/nio/fs/UnixSecureDirectoryStream",
        "newDirectoryStream",
        "(Ljava/lang/Object;[Ljava/nio/file/LinkOption;)Ljava/nio/file/SecureDirectoryStream;",
    ),
    (
        "sun/nio/fs/UnixSecureDirectoryStream",
        "spliterator",
        "()Ljava/util/Spliterator;",
    ),
];

/// SBR wave 2, lane `netaddr`, part (a), 2026-09-28 -- `java/net/InetAddress`,
/// `Inet4Address` and `Inet6Address`: every `Bridge` over a method with `Code`
/// on 17, 21 and 25 (declared, or inherited from `InetAddress` with `Code`),
/// plus the `JavaNetInetAddressAccess` accessor `InetAddress$1.getOriginalHostName`.
/// 39 triples.
///
/// # Why it is safe: the state was already real
///
/// * **Every address this VM mints carries a real holder.** `alloc_inet_address`
///   (behind `getByName`, the genuine `Inet{4,6}AddressImpl.lookupAllHostAddr`
///   natives, `NetworkInterface`, the socket natives) writes a real
///   `InetAddress$InetAddressHolder` (`hostName`, `address`, `family`) into
///   `holder` and, for v6, a real `Inet6Address$Inet6AddressHolder`
///   (`ipaddress`, `scope_id`, `scope_id_set`) into `holder6`;
///   `re8_make_interface` sets `scope_ifname` to the interface it builds, as
///   the JDK's `getAll0` does. The side table the natives read is a copy of
///   that, not a second truth. The one field it did not write was
///   `holder.originalHostName`, which the JDK writes together with
///   `hostName` everywhere (`InetAddressHolder(String,int,int)`, `init`,
///   `net_util.c`'s `setInetAddress_hostName`) and which the retired
///   `getOriginalHostName` reads: now written, under `--real-jdk`.
/// * **The genuine natives are not here and stay bridges.** `init`,
///   `isIPv4Available`, `isIPv6Supported` (ACC_NATIVE on 21/25, absent on
///   17), and `Inet{4,6}AddressImpl.lookupAllHostAddr` / `getHostByAddr` /
///   `getLocalHostName` / `isReachable0` are what the real factories reach,
///   and are registered. `InetAddress.getHostByAddr([B)`,
///   `lookupAllHostAddr(String)` and `InetAddress$1.getHostFromNameService`
///   are registered but are no JDK method at all: nothing to yield to.
/// * **Three socket-layer READERS owned both ends of a convention** (the
///   defect class `RETIRED_SHADOW_L4_BACKED_OUT_TRIPLES` names): they looked an
///   address up in the side table only, then fell back to raw slots. With
///   the factories retired, `getByName("127.0.0.1")` is the JDK's own
///   `new Inet4Address(null, bytes)`, which has no side-table row, and
///   `read_inet_socket_address` read `holder.originalHostName` (slot 0, null
///   for a literal) and answered `0.0.0.0` -- the wildcard. Under
///   `--real-jdk` they now ask `inet_addr_resolve` (side table first, then the
///   real holders): `read_inet_socket_address` (`net_phase_e.rs`), and via
///   `inet_addr_get_for_socket` `plain_socket.rs`'s `read_inet_addr` and the
///   two `MulticastSocket` membership natives in `phases_late/net_channels.rs`.
///
/// # Measured
///
/// linux/x86_64, JDK 25.0.4+7, control `cvm-sbr-int1` (`claude/sbr-orch`
/// `a1b75dc2a`, `SUITE=all --real-jdk` 152/152). Dial preview
/// `CRATONVM_ENFORCE_NATIVE_SHADOW=java/net/InetAddress,java/net/Inet4Address,java/net/Inet6Address`:
/// 152/152, the 44 native-won `Inet*` census rows over 17 vectors -> 0.
///
/// ```text
///   SbrW2Netaddr.java (74 rows + 3 one-shot, vs HotSpot 25)
///     control --real-jdk 8 rows + drift differ   this --real-jdk 0 differ,
///                                                drift == HotSpot's (the
///                                                cached negative lookup)
///     this + CRATONVM_UNRETIRE_NATIVE_SHADOW (49 rows)  == control except
///                                                the 4 URL authority rows
///     this --compatible                          == control --compatible
///   suite.sh SUITE=all --real-jdk   152/152, vector-identical to the control
///   census native-won               Inet* 44 -> 0, URL 22 -> 0
/// ```
///
/// Full record: `docs/feature-designs/native-dispatch-architecture.md`.
///
/// # Left out
///
/// `NetworkInterface` (its `Code` rows `getHardwareAddress` / `getMTU` never
/// won a dispatch in the control census; the rest are genuine natives),
/// `InetSocketAddress` (its natives are `Intrinsic`, not `Bridge`: a
/// different decision), and `InetAddress$InetAddressHolder` (nothing
/// registered on it).
///
/// `--compatible` is unchanged: the re-tag makes these `SyntheticStub`, which
/// that mode registers and dispatches exactly as it did the `Bridge`, and
/// every producer/reader change above is behind `is_jdk_only()`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_NETADDR_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/net/Inet4Address", "equals", "(Ljava/lang/Object;)Z"),
    ("java/net/Inet4Address", "getAddress", "()[B"),
    (
        "java/net/Inet4Address",
        "getCanonicalHostName",
        "()Ljava/lang/String;",
    ),
    (
        "java/net/Inet4Address",
        "getHostAddress",
        "()Ljava/lang/String;",
    ),
    (
        "java/net/Inet4Address",
        "getHostName",
        "()Ljava/lang/String;",
    ),
    ("java/net/Inet4Address", "hashCode", "()I"),
    ("java/net/Inet4Address", "isAnyLocalAddress", "()Z"),
    ("java/net/Inet4Address", "isLoopbackAddress", "()Z"),
    ("java/net/Inet4Address", "isMulticastAddress", "()Z"),
    ("java/net/Inet4Address", "toString", "()Ljava/lang/String;"),
    ("java/net/Inet6Address", "equals", "(Ljava/lang/Object;)Z"),
    ("java/net/Inet6Address", "getAddress", "()[B"),
    (
        "java/net/Inet6Address",
        "getCanonicalHostName",
        "()Ljava/lang/String;",
    ),
    (
        "java/net/Inet6Address",
        "getHostAddress",
        "()Ljava/lang/String;",
    ),
    (
        "java/net/Inet6Address",
        "getHostName",
        "()Ljava/lang/String;",
    ),
    ("java/net/Inet6Address", "hashCode", "()I"),
    ("java/net/Inet6Address", "isAnyLocalAddress", "()Z"),
    ("java/net/Inet6Address", "isLoopbackAddress", "()Z"),
    ("java/net/Inet6Address", "isMulticastAddress", "()Z"),
    ("java/net/Inet6Address", "toString", "()Ljava/lang/String;"),
    ("java/net/InetAddress", "equals", "(Ljava/lang/Object;)Z"),
    ("java/net/InetAddress", "getAddress", "()[B"),
    (
        "java/net/InetAddress",
        "getAllByName",
        "(Ljava/lang/String;)[Ljava/net/InetAddress;",
    ),
    (
        "java/net/InetAddress",
        "getByAddress",
        "([B)Ljava/net/InetAddress;",
    ),
    (
        "java/net/InetAddress",
        "getByName",
        "(Ljava/lang/String;)Ljava/net/InetAddress;",
    ),
    (
        "java/net/InetAddress",
        "getCanonicalHostName",
        "()Ljava/lang/String;",
    ),
    (
        "java/net/InetAddress",
        "getHostAddress",
        "()Ljava/lang/String;",
    ),
    (
        "java/net/InetAddress",
        "getHostName",
        "()Ljava/lang/String;",
    ),
    (
        "java/net/InetAddress",
        "getLocalHost",
        "()Ljava/net/InetAddress;",
    ),
    (
        "java/net/InetAddress",
        "getLoopbackAddress",
        "()Ljava/net/InetAddress;",
    ),
    ("java/net/InetAddress", "hashCode", "()I"),
    ("java/net/InetAddress", "isAnyLocalAddress", "()Z"),
    ("java/net/InetAddress", "isLinkLocalAddress", "()Z"),
    ("java/net/InetAddress", "isLoopbackAddress", "()Z"),
    ("java/net/InetAddress", "isMulticastAddress", "()Z"),
    ("java/net/InetAddress", "isReachable", "(I)Z"),
    ("java/net/InetAddress", "isSiteLocalAddress", "()Z"),
    ("java/net/InetAddress", "toString", "()Ljava/lang/String;"),
    (
        "java/net/InetAddress$1",
        "getOriginalHostName",
        "(Ljava/net/InetAddress;)Ljava/lang/String;",
    ),
];

/// SBR wave 2, lane `netaddr`, part (b), 2026-09-28 -- `java/net/URL`, the ten
/// `Bridge` registrations wave 1's URI lane held on a named precondition:
/// "every minted `URL` built by the JDK constructor (or given a real
/// `handler`)". Every JDK body here goes through `handler` (`toString` ->
/// `toExternalForm` -> `handler.toExternalForm(this)`; `getDefaultPort`,
/// `hashCode`, `equals`, `sameFile`, `openConnection` likewise; `openStream` is
/// `openConnection().getInputStream()`; `toURI` checks
/// `isBuiltinStreamHandler(handler)`), and `setURLStreamHandlerFactory` is the
/// JDK's own install (throwing `Error("factory already defined")` on a second
/// one, as HotSpot does).
///
/// # The precondition, and what now meets it
///
/// Under `--real-jdk` every mint site builds the URL with the JDK's
/// `URL(String)` (`jdk_only_constructed_url`), falling back to
/// `URL(String protocol, String host, int port, String file)` when the
/// handler's `parseURL` refuses the text: that constructor looks the handler
/// up the same way (the installed factory first) and stores it, without
/// parsing. Sites converted by this lane: the `ProtectionDomain` code source
/// (`lib.rs`), `Class.getResource` (`servlet.rs`, now the class path's real
/// URL instead of `classpath:`), the three `register_p61_classloader`
/// natives (`phases_late.rs`; only `findResource` is left there since
/// interpreter round i2 wave 56, the other two were dead copies), `p59_alloc_url` (`jar_manifest.rs`) and the
/// JBoss system-resource fallthrough (`jboss_module_loader.rs`, the system
/// loader's real URL instead of `classpath:/`). `lang_class.rs`'s code source
/// and `build_synthetic_url` already went through the constructor; the old
/// `URI.toURL` fallback in `net_phase_e.rs` is unreachable under `--real-jdk`
/// because `URI.toURL` is retired (`RETIRED_SHADOW_SBR_URI_TRIPLES`).
///
/// **What is left handler-less:** a spec whose protocol has NO handler even
/// by name -- the VM-invented `classpath:` a JBoss or `Class.getResource`
/// lookup still falls back to when the class path names no URL for a
/// resource only `find_resource` can see. HotSpot has no URL for that text at
/// all. On such a URL the real `toString()` throws `NullPointerException`.
///
/// # Measured
///
/// Dial preview `CRATONVM_ENFORCE_NATIVE_SHADOW=java/net/URL` on the control:
/// 152/152, the 22 native-won `URL` census rows over 15 vectors -> 0. The
/// dial was green where the first REAL build was not: `RFsSingleton` and
/// `RServiceLoaderDoubleSource` died of `StackOverflowError`, because the
/// `JavaRuntimeURLConnection.getInputStream` native (kept for the carrier the
/// old `openConnection` minted) delegated to `url.openStream()`, which is now
/// `openConnection().getInputStream()` -- itself. Under `--real-jdk` it serves
/// the module resource's bytes directly. With that, the suite is 152/152 and
/// vector-identical to the control, and `SbrW2Netaddr`'s URL rows match
/// HotSpot 25, including `http:` through the JDK's own handler, `jar:`,
/// `jrt:`, a custom `URLStreamHandlerFactory` and its second-install `Error`.
/// Full record: `docs/feature-designs/native-dispatch-architecture.md`.
///
/// `--compatible` is unchanged: the re-tag makes these `SyntheticStub`, which
/// that mode registers and dispatches exactly as it did the `Bridge`, and
/// every mint-site change is behind `is_jdk_only()`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_NETADDR_URL_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/net/URL", "equals", "(Ljava/lang/Object;)Z"),
    ("java/net/URL", "getDefaultPort", "()I"),
    ("java/net/URL", "hashCode", "()I"),
    (
        "java/net/URL",
        "openConnection",
        "()Ljava/net/URLConnection;",
    ),
    ("java/net/URL", "openStream", "()Ljava/io/InputStream;"),
    ("java/net/URL", "sameFile", "(Ljava/net/URL;)Z"),
    (
        "java/net/URL",
        "setURLStreamHandlerFactory",
        "(Ljava/net/URLStreamHandlerFactory;)V",
    ),
    ("java/net/URL", "toExternalForm", "()Ljava/lang/String;"),
    ("java/net/URL", "toString", "()Ljava/lang/String;"),
    ("java/net/URL", "toURI", "()Ljava/net/URI;"),
];

/// SBR wave 4, lane FJP (2026-09-29): the `ForkJoinPool` family, the
/// `StackWalker` factories, `Runtime`/`Runtime$Version` and
/// `jdk/internal/misc/Signal`'s state. 83 triples, 119 registrations (the
/// `bridge` rows of the 25/linux kind map over them): `ForkJoinPool` 19 (27),
/// `ForkJoinTask` 25 (38), `RecursiveTask` 14 (18), `RecursiveAction` 12
/// (17), `StackWalker.getInstance` x4 (4), `Runtime` 4 (5),
/// `Runtime$Version` 2 (4), `Signal` 3 (6). Every row has `Code` in the JDK 25
/// image (`javap -p -c`; the task-class rows inherited from `ForkJoinTask`).
///
/// # ForkJoin: the whole model moves, or none of it
///
/// Under `--real-jdk` the pool used to be the VM's own: `commonPool()` minted
/// an unconstructed `ForkJoinPool` (a different object from the real
/// `ForkJoinPool.common`), `invoke`/`fork`/`join` ran every task INLINE on the
/// caller, and completion lived in the `fjp_state` side table rather than in
/// `ForkJoinTask.status`. The lane records say why it could never be retired
/// piecemeal (`jdk-only-forkjoin-completion-claim-race-FIXED-20260917.md`):
/// arm the pool alone and real workers run tasks the side table never hears
/// of; arm pool and tasks through the dial and the JIT-bound doors leak, so the
/// two models still meet (measured again here: the dial over all five classes
/// failed `RJdkForkJoin` with a `null` join result, a wrong `CountedCompleter`
/// sum and `inForkJoinPool()` false inside `pool.invoke`). Retired as ONE
/// table, nothing native is left on these classes under `--real-jdk`, and the
/// real pool is coherent: real `common`, real workers, real `status`.
///
/// **The keep arms.** `registry.rs`'s `keep_real_forkjoinpool_bridge` and
/// `keep_real_forkjointask_bridge` keep only a `Bridge` on these classes in
/// every real-JDK mode, and the re-tag in `register` has made a retired triple
/// a `SyntheticStub` before they ask -- so a table row used to DELETE the
/// native in `--compatible` too (`real_layout_bridge_keeps_are_not_retired_shadows`).
/// Both arms now also keep a registration `register` is re-tagging as a
/// retired shadow (`retagging_retired_shadow`): `--compatible` keeps and
/// dispatches it exactly as before, and `--real-jdk` has already dropped it as
/// a stub (that refusal runs before the keep arms).
///
/// # `StackWalker`: the factories only
///
/// `getInstance` x4 are retired: the walker is the JDK's own object (its real
/// `options`/`retainClassRef` fields, which the native walk reads by name).
/// **`walk`, `forEach` and `getCallerClass` are held for cost**: the JDK's
/// `StackStreamFactory` path is correct on this VM (probe and suite green with
/// them retired, trial `cvm-sbr-fjp-2`) and 18-19x slower -- walk at depth 20
/// 1406-1456 us against 73-76 (JIT), 810-990 against 90-157 (`--nojit`);
/// `getCallerClass` 53-55 us against 2.9-3.0 (JIT). `LogRecord`'s
/// `inferCaller` walks on every JUL record that asks for its source. What
/// `--real-jdk` must not keep is the synthetic stream the native handed out
/// (`cratonvm/stream/LazyOp` refused on every `filter`/`skip`), so the held
/// natives now hand real `StackFrameInfo` carriers in a real
/// `Arrays.stream(Object[])` pipeline under `--real-jdk`, closed after the
/// function returns so an escaped stream throws as on HotSpot
/// (`reflect_invoke.rs` `p59_sw_stream`).
///
/// # `Runtime`, `Runtime$Version`, `Signal`
///
/// `getRuntime` read and wrote the real `currentRuntime` static; `version()`
/// built its own `Runtime$Version` (the JDK's `version()` never touches the
/// regex parser the native's comment feared: it is `VersionProps` fields and a
/// `'.'` split) and kept it in a per-VM global root because a native store to
/// the real `version` static "did not stick"; `feature`/`build` go with it
/// (a native `feature` over a real `Version` read `version.get(0)` anyway).
/// `addShutdownHook`/`removeShutdownHook` kept hooks in a Rust list; retired,
/// they land in the real `ApplicationShutdownHooks`, which is slot 1 of
/// `java.lang.Shutdown.hooks` -- and `lang_system::run_jdk_shutdown_slots`
/// already runs every slot on each exit path (RShutdownHooks green, the probe's
/// `hook ran` line printed). `Signal.<init>` wrote `name`/`number` by name into
/// the real fields; the real constructor asks the registered `findSignal0` and
/// throws the same `IllegalArgumentException`; `getName`/`getNumber` are bare
/// field reads. Held: `Runtime.load0`/`loadLibrary0` (the dial failed
/// `RJdkJni`: real `ClassLoader.loadLibrary` NPEs on a null
/// `NativeLibraries` -- the loaders' native-library state is not real),
/// `Runtime.exit` (the VM's exit path), `Signal.raise` (its native runs the
/// handler; real `raise` reaches the no-op `raise0`), and all of
/// `sun/misc/Signal` (its natives keep `name`/`number` on an object whose real
/// class holds a `jdk.internal.misc.Signal`; outside this lane's census).
///
/// # Measured (JDK 25.0.4, linux; control `cvm-sbr-int5`, final `cvm-sbr-fjp-4`)
///
/// ```text
///   suite (152):     int5 152/152; fjp-4 152/152 (fjp-1..3 too)
///   dial screen:     ForkJoin 5 classes 151/152 (RJdkForkJoin -- the dial's
///                    JIT-door leak, not the family); StackWalker 152/152;
///                    Runtime+Signal 151/152 (RJdkJni: load0/loadLibrary0, held)
///   strict corpus:   int5 0 divergent sections; fjp-4 0
///   SbrW4Fjp (67 rows) vs HotSpot 25:  int5 --real-jdk 10 lines, drift 1;
///                    fjp-4 --real-jdk 3 (all sun.misc.Signal), drift 0;
///                    + UNRETIRE (8 classes) = int5 but 4 lines moved TO HotSpot
///                    (the both-mode StackWalker fixes); --compatible: 4 lines
///                    moved TO HotSpot, none away
///   ForkJoin timing (us/op, 3 interleaved rounds, int5 -> fjp):
///     recursive sum 1M, JIT      1957-3295 -> 2139-2829   (noise)
///     recursive sum 1M, --nojit  59936-74177 -> 30963-38948
///     parallel LongStream sum, JIT  31370-50278 -> 18710-19563
///     parallel LongStream sum, --nojit 1.62-2.18 s -> 0.89-0.95 s
///   held walk with real carriers + stream (int5 -> fjp-4), depth 20:
///     JIT 39.6-40.3 -> 50.7-51.3; getCallerClass unchanged
/// ```
///
/// Record: `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_FJP_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/Runtime",
        "addShutdownHook",
        "(Ljava/lang/Thread;)V",
    ),
    ("java/lang/Runtime", "getRuntime", "()Ljava/lang/Runtime;"),
    (
        "java/lang/Runtime",
        "removeShutdownHook",
        "(Ljava/lang/Thread;)Z",
    ),
    (
        "java/lang/Runtime",
        "version",
        "()Ljava/lang/Runtime$Version;",
    ),
    (
        "java/lang/Runtime$Version",
        "build",
        "()Ljava/util/Optional;",
    ),
    ("java/lang/Runtime$Version", "feature", "()I"),
    (
        "java/lang/StackWalker",
        "getInstance",
        "()Ljava/lang/StackWalker;",
    ),
    (
        "java/lang/StackWalker",
        "getInstance",
        "(Ljava/lang/StackWalker$Option;)Ljava/lang/StackWalker;",
    ),
    (
        "java/lang/StackWalker",
        "getInstance",
        "(Ljava/util/Set;)Ljava/lang/StackWalker;",
    ),
    (
        "java/lang/StackWalker",
        "getInstance",
        "(Ljava/util/Set;I)Ljava/lang/StackWalker;",
    ),
    (
        "java/util/concurrent/ForkJoinPool",
        "awaitQuiescence",
        "(JLjava/util/concurrent/TimeUnit;)Z",
    ),
    (
        "java/util/concurrent/ForkJoinPool",
        "commonPool",
        "()Ljava/util/concurrent/ForkJoinPool;",
    ),
    (
        "java/util/concurrent/ForkJoinPool",
        "execute",
        "(Ljava/lang/Runnable;)V",
    ),
    (
        "java/util/concurrent/ForkJoinPool",
        "execute",
        "(Ljava/util/concurrent/ForkJoinTask;)V",
    ),
    (
        "java/util/concurrent/ForkJoinPool",
        "externalSubmit",
        "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;",
    ),
    (
        "java/util/concurrent/ForkJoinPool",
        "getCommonPoolParallelism",
        "()I",
    ),
    (
        "java/util/concurrent/ForkJoinPool",
        "getFactory",
        "()Ljava/util/concurrent/ForkJoinPool$ForkJoinWorkerThreadFactory;",
    ),
    ("java/util/concurrent/ForkJoinPool", "getParallelism", "()I"),
    (
        "java/util/concurrent/ForkJoinPool",
        "invoke",
        "(Ljava/util/concurrent/ForkJoinTask;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ForkJoinPool",
        "invokeAll",
        "(Ljava/util/Collection;)Ljava/util/List;",
    ),
    (
        "java/util/concurrent/ForkJoinPool",
        "invokeAll",
        "(Ljava/util/Collection;JLjava/util/concurrent/TimeUnit;)Ljava/util/List;",
    ),
    (
        "java/util/concurrent/ForkJoinPool",
        "invokeAllUninterruptibly",
        "(Ljava/util/Collection;)Ljava/util/List;",
    ),
    (
        "java/util/concurrent/ForkJoinPool",
        "invokeAny",
        "(Ljava/util/Collection;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ForkJoinPool",
        "invokeAny",
        "(Ljava/util/Collection;JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ForkJoinPool",
        "lazySubmit",
        "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;",
    ),
    (
        "java/util/concurrent/ForkJoinPool",
        "submit",
        "(Ljava/lang/Runnable;)Ljava/util/concurrent/ForkJoinTask;",
    ),
    (
        "java/util/concurrent/ForkJoinPool",
        "submit",
        "(Ljava/lang/Runnable;Ljava/lang/Object;)Ljava/util/concurrent/ForkJoinTask;",
    ),
    (
        "java/util/concurrent/ForkJoinPool",
        "submit",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/ForkJoinTask;",
    ),
    (
        "java/util/concurrent/ForkJoinPool",
        "submit",
        "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;",
    ),
    ("java/util/concurrent/ForkJoinTask", "cancel", "(Z)Z"),
    (
        "java/util/concurrent/ForkJoinTask",
        "complete",
        "(Ljava/lang/Object;)V",
    ),
    (
        "java/util/concurrent/ForkJoinTask",
        "completeExceptionally",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/concurrent/ForkJoinTask",
        "fork",
        "()Ljava/util/concurrent/ForkJoinTask;",
    ),
    (
        "java/util/concurrent/ForkJoinTask",
        "get",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ForkJoinTask",
        "get",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ForkJoinTask",
        "getException",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/ForkJoinTask",
        "getPool",
        "()Ljava/util/concurrent/ForkJoinPool;",
    ),
    ("java/util/concurrent/ForkJoinTask", "inForkJoinPool", "()Z"),
    (
        "java/util/concurrent/ForkJoinTask",
        "invoke",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ForkJoinTask",
        "invokeAll",
        "(Ljava/util/Collection;)Ljava/util/Collection;",
    ),
    (
        "java/util/concurrent/ForkJoinTask",
        "invokeAll",
        "(Ljava/util/concurrent/ForkJoinTask;Ljava/util/concurrent/ForkJoinTask;)V",
    ),
    (
        "java/util/concurrent/ForkJoinTask",
        "invokeAll",
        "([Ljava/util/concurrent/ForkJoinTask;)V",
    ),
    ("java/util/concurrent/ForkJoinTask", "isCancelled", "()Z"),
    (
        "java/util/concurrent/ForkJoinTask",
        "isCompletedAbnormally",
        "()Z",
    ),
    (
        "java/util/concurrent/ForkJoinTask",
        "isCompletedNormally",
        "()Z",
    ),
    ("java/util/concurrent/ForkJoinTask", "isDone", "()Z"),
    (
        "java/util/concurrent/ForkJoinTask",
        "join",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/ForkJoinTask",
        "quietlyComplete",
        "()V",
    ),
    ("java/util/concurrent/ForkJoinTask", "quietlyInvoke", "()V"),
    ("java/util/concurrent/ForkJoinTask", "quietlyJoin", "()V"),
    (
        "java/util/concurrent/ForkJoinTask",
        "quietlyJoin",
        "(JLjava/util/concurrent/TimeUnit;)Z",
    ),
    (
        "java/util/concurrent/ForkJoinTask",
        "quietlyJoinPoolInvokeAllTask",
        "(J)V",
    ),
    (
        "java/util/concurrent/ForkJoinTask",
        "quietlyJoinUninterruptibly",
        "(JLjava/util/concurrent/TimeUnit;)Z",
    ),
    ("java/util/concurrent/ForkJoinTask", "reinitialize", "()V"),
    ("java/util/concurrent/RecursiveAction", "cancel", "(Z)Z"),
    (
        "java/util/concurrent/RecursiveAction",
        "complete",
        "(Ljava/lang/Object;)V",
    ),
    (
        "java/util/concurrent/RecursiveAction",
        "completeExceptionally",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/concurrent/RecursiveAction",
        "fork",
        "()Ljava/util/concurrent/ForkJoinTask;",
    ),
    (
        "java/util/concurrent/RecursiveAction",
        "getException",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/RecursiveAction",
        "getPool",
        "()Ljava/util/concurrent/ForkJoinPool;",
    ),
    (
        "java/util/concurrent/RecursiveAction",
        "getRawResult",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/RecursiveAction",
        "inForkJoinPool",
        "()Z",
    ),
    (
        "java/util/concurrent/RecursiveAction",
        "invoke",
        "()Ljava/lang/Object;",
    ),
    ("java/util/concurrent/RecursiveAction", "isDone", "()Z"),
    (
        "java/util/concurrent/RecursiveAction",
        "join",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/RecursiveAction",
        "reinitialize",
        "()V",
    ),
    ("java/util/concurrent/RecursiveTask", "cancel", "(Z)Z"),
    (
        "java/util/concurrent/RecursiveTask",
        "complete",
        "(Ljava/lang/Object;)V",
    ),
    (
        "java/util/concurrent/RecursiveTask",
        "completeExceptionally",
        "(Ljava/lang/Throwable;)V",
    ),
    (
        "java/util/concurrent/RecursiveTask",
        "fork",
        "()Ljava/util/concurrent/ForkJoinTask;",
    ),
    (
        "java/util/concurrent/RecursiveTask",
        "get",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/RecursiveTask",
        "getException",
        "()Ljava/lang/Throwable;",
    ),
    (
        "java/util/concurrent/RecursiveTask",
        "getPool",
        "()Ljava/util/concurrent/ForkJoinPool;",
    ),
    (
        "java/util/concurrent/RecursiveTask",
        "getRawResult",
        "()Ljava/lang/Object;",
    ),
    (
        "java/util/concurrent/RecursiveTask",
        "inForkJoinPool",
        "()Z",
    ),
    (
        "java/util/concurrent/RecursiveTask",
        "invoke",
        "()Ljava/lang/Object;",
    ),
    ("java/util/concurrent/RecursiveTask", "isDone", "()Z"),
    (
        "java/util/concurrent/RecursiveTask",
        "join",
        "()Ljava/lang/Object;",
    ),
    ("java/util/concurrent/RecursiveTask", "reinitialize", "()V"),
    (
        "java/util/concurrent/RecursiveTask",
        "setRawResult",
        "(Ljava/lang/Object;)V",
    ),
    (
        "jdk/internal/misc/Signal",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "jdk/internal/misc/Signal",
        "getName",
        "()Ljava/lang/String;",
    ),
    ("jdk/internal/misc/Signal", "getNumber", "()I"),
];

/// SBR wave 4, lane LOCALE2, 2026-09-29: the `ResourceBundle` / `Bundles.of` /
/// `LocaleResources` half of the locale family that wave 1 HELD -- 15 rows on
/// `java/util/ResourceBundle` (eight `getBundle` overloads, `getObject`,
/// `getString`, `getStringArray`, `containsKey`, `keySet`, `getLocale`,
/// `getBaseBundleName`), `sun/util/resources/Bundles.of`, and the six
/// `LocaleResources` readers. One `Bridge` registration each in the 25/linux
/// kind map: 22 re-tags. `Code` on 17, 21 and 25 for every row.
///
/// # Why it is safe now and was not in wave 1
///
/// Wave 1 held these because `rb_get_bundle` minted instances of the ABSTRACT
/// `java/util/ResourceBundle` (a hidden map in slot 0) and `Bundles.of` handed
/// out the same carrier: a real reader meeting it reaches an abstract
/// `handleGetObject`. The only doors into `rb_get_bundle` are the eight
/// `getBundle` rows, `Bundles.of` and `LocaleData.getBundle` (retired by
/// `RETIRED_SHADOW_L1_LP_TRIPLES`), so retiring the MINT SITES in the same table
/// as the READERS leaves no native `ResourceBundle` under `--real-jdk`: every
/// bundle is the image's own class, loaded by the real `LocaleProviderAdapter`
/// -> `LocaleData` -> `Bundles` chain. The JVMS 6.5 uninstantiable-receiver
/// census stops naming `ResourceBundle` (control: 2 classes, retirement: 1).
/// The same argument releases wave 6's BreakIterator "floor"
/// (`the_two_locale_resources_readers_are_retired_only_with_their_carrier`).
///
/// # Measured (JDK 25.0.4, linux; control `cvm-sbr-int5`, final `cvm-sbr-locale2-4`)
///
/// ```text
///   suite (152):  control 152/152; locale2-1/-2/-4 152/152 each;
///                 --compatible control 150/152, locale2-4 the same two
///   strict:       control 0 divergent; locale2-1 and locale2-4 0 (67/67)
///   dial screen:  int3 + the three classes 152/152, but the PROBE read worse
///                 (AbstractMethodError, BreakIterator drift 507): under the dial
///                 `rb_get_bundle` still ran through `ctx.invoke`, i.e. the
///                 half-retirement; none of it is on the trial binaries
///   SbrW4Locale2 (419 rows, 15 locales) vs HotSpot, drift 0 in every arm:
///     int5 --real-jdk 32 -> locale2-4 --real-jdk 3 (java.time am/pm text:
///                           CalendarDataUtility, held, not this table)
///     locale2-4 + UNRETIRE=<the three classes> = int5 except the 2
///                           default-locale rows (a both-mode property fix)
///     --compatible: int5 53, locale2-4 51 (the same 2 rows, toward HotSpot)
///   SbrW1Locale (328 rows): int5 9 -> locale2-4 1
///   timing, ms per 20,000 calls, interleaved, load ~2:
///     NumberFormat.getInstance+format   314-332 -> 215-252
///     DateFormat.getDateTimeInstance    1371-1425 -> 1617-1692 (+15-20%; ALL of
///       it `getDateTimePattern`: unretiring that one row gives 1353-1459. The
///       real body runs MessageFormat.format(p.replaceAll(..)) per call, ~11 us
///       here vs ~0.3 us on HotSpot; the native is wrong for th-TH's Buddhist
///       patterns, so the row stays retired)
///     String.format(locale, ..)         253-268 -> 197-202
///     ResourceBundle.getBundle+getString 667-697 -> 320-336
/// ```
///
/// # Left out
///
/// * `ResourceBundle.handleGetObject` / `getKeys`: ABSTRACT on every image, no
///   `Code` to yield to (they stay registered for `--compatible`'s carrier).
/// * `ResourceBundle$1` (the `JavaUtilResourceBundleAccess` shims) and
///   `CalendarDataUtility`: other registrars' families, not re-measured.
///
/// Record: `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_LOCALE2_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/util/ResourceBundle", "containsKey", "(Ljava/lang/String;)Z"),
    ("java/util/ResourceBundle", "getBaseBundleName", "()Ljava/lang/String;"),
    ("java/util/ResourceBundle", "getBundle", "(Ljava/lang/String;)Ljava/util/ResourceBundle;"),
    ("java/util/ResourceBundle", "getBundle", "(Ljava/lang/String;Ljava/lang/Module;)Ljava/util/ResourceBundle;"),
    ("java/util/ResourceBundle", "getBundle", "(Ljava/lang/String;Ljava/util/Locale;)Ljava/util/ResourceBundle;"),
    ("java/util/ResourceBundle", "getBundle", "(Ljava/lang/String;Ljava/util/Locale;Ljava/lang/ClassLoader;)Ljava/util/ResourceBundle;"),
    ("java/util/ResourceBundle", "getBundle", "(Ljava/lang/String;Ljava/util/Locale;Ljava/lang/ClassLoader;Ljava/util/ResourceBundle$Control;)Ljava/util/ResourceBundle;"),
    ("java/util/ResourceBundle", "getBundle", "(Ljava/lang/String;Ljava/util/Locale;Ljava/lang/Module;)Ljava/util/ResourceBundle;"),
    ("java/util/ResourceBundle", "getBundle", "(Ljava/lang/String;Ljava/util/Locale;Ljava/util/ResourceBundle$Control;)Ljava/util/ResourceBundle;"),
    ("java/util/ResourceBundle", "getBundle", "(Ljava/lang/String;Ljava/util/ResourceBundle$Control;)Ljava/util/ResourceBundle;"),
    ("java/util/ResourceBundle", "getLocale", "()Ljava/util/Locale;"),
    ("java/util/ResourceBundle", "getObject", "(Ljava/lang/String;)Ljava/lang/Object;"),
    ("java/util/ResourceBundle", "getString", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("java/util/ResourceBundle", "getStringArray", "(Ljava/lang/String;)[Ljava/lang/String;"),
    ("java/util/ResourceBundle", "keySet", "()Ljava/util/Set;"),
    ("sun/util/locale/provider/LocaleResources", "getBreakIteratorInfo", "(Ljava/lang/String;)Ljava/lang/Object;"),
    ("sun/util/locale/provider/LocaleResources", "getBreakIteratorResources", "(Ljava/lang/String;)[B"),
    ("sun/util/locale/provider/LocaleResources", "getDateTimePattern", "(IILjava/util/Calendar;)Ljava/lang/String;"),
    ("sun/util/locale/provider/LocaleResources", "getDecimalFormatSymbolsData", "()[Ljava/lang/Object;"),
    ("sun/util/locale/provider/LocaleResources", "getJavaTimeDateTimePattern", "(IILjava/lang/String;)Ljava/lang/String;"),
    ("sun/util/locale/provider/LocaleResources", "getNumberPatterns", "()[Ljava/lang/String;"),
    ("sun/util/resources/Bundles", "of", "(Ljava/lang/String;Ljava/util/Locale;Lsun/util/resources/Bundles$Strategy;)Ljava/util/ResourceBundle;"),
];

/// SBR wave 4, lane SSL (2026-09-29): the X.500 name carriers, `SSLEngineResult`
/// and `KeyStore.getInstance(String, Provider)`. 17 triples, 17 registrations:
/// `X500Principal` (11), `X500Name.asX500Principal`, `SSLEngineResult`'s four
/// getters, and the `KeyStore` factory overload. Every row has `Code` on 17, 21
/// and 25. The JSSE handshake half is HELD native (user decision, measured).
///
/// # Why these are safe
///
/// * `X500Principal`: every native mint site now builds a REAL principal
///   through `X500Principal(byte[])` when these rows are retired
///   (`ssl_security::real_x500_principal`: `SSLSession.getPeerPrincipal` /
///   `getLocalPrincipal`, the `X509Certificate` DN accessors, and
///   `t27_tls::build_issuer_principals`). Lane 6's "X500Principal alone breaks
///   RSslLiveSession" was exactly those 1-slot synthetics meeting real
///   `getName`/`equals` bytecode.
/// * `SSLEngineResult`: the native engines already build it with its real
///   constructor and real enum constants (`t27_tls::alloc_engine_result`).
/// * `KeyStore.getInstance(String, Provider)`: real `GetInstance` over the
///   real providers (JCA2); the SPI it returns is the one the still-native
///   engine surface serves.
///
/// # Measured (JDK 25.0.4, linux; control `cvm-sbr-int7`, final `cvm-sbr-ssl-6`)
///
/// ```text
///   SbrW4Ssl probe (lines != HotSpot 25, drift 0 everywhere):
///     int7 --real-jdk 22 -> ssl-6 --real-jdk 17 (the four X500Principal lines
///     and the SSLSocket peer-principal line move to HotSpot, plus the three
///     AES-GCM-16 lines of a both-mode Cipher fix); --compatible: only those
///     three AES-GCM lines move, to HotSpot
///   suite (152): int7 152/152, ssl-6 see the lane record
///   timing (SSLEngine loopback, TLS 1.3, RSA-2048, interleaved with int7):
///     handshake within noise of int7 (see the lane record)
/// ```
///
/// # Held, with the measured reason and what would unblock it
///
/// (Wave 5, lane JSSE, retired everything in this section except
/// `HttpsURLConnection` and `engineGetAttributes`, by maintainer decision:
/// `RETIRED_SHADOW_SBR_JSSE_TRIPLES`. What follows is wave 4's record.)
///
/// * The handshake half: `SSLContext`, `SSLEngine`/`SSLEngineImpl`,
///   `SSLSocket`/`SSLSocketImpl` (+ `$AppInputStream`/`$AppOutputStream`),
///   `SSLServerSocket`, `SSLServerSocketFactory`, `SSLSocketFactory`.
///   Retired (trial `cvm-sbr-ssl-4`, 273 rows: probe 0 lines from HotSpot,
///   suite 152/152) the TLS handshake ran as bytecode at 24-77 ms against
///   2-6 ms native (8-20x), handshake + 1 MiB 4.5-6.6x. The cost is the
///   RSA-2048 `CertificateVerify` over real `Signature` bytecode and PKIX
///   cert-path validation in bytecode. Unblocks: a reviewed `Intrinsic` for
///   the `BigInteger` modPow / RSA core (the methods HotSpot intrinsifies:
///   `BigInteger.implMontgomeryMultiply`/`implMontgomerySquare`/`implMulAdd`/
///   `implSquareToLen`) and a JIT fast enough on `sun.security.provider.certpath`
///   validation; then re-run `SbrW4Ssl perf` against the control.
/// * `SSLParameters`: the native engines keep ALPN in a side table
///   (`t27_tls::sslparams_alpn_table`) that the real `setApplicationProtocols`
///   would bypass. Goes with the handshake half.
/// * `KeyManagerFactory`/`TrustManagerFactory`, the `sun/security/ssl` manager
///   and factory SPIs, and the six PKCS12/JKS keystore SPI classes: with the
///   context native, real managers and a real keystore leave the native
///   `SSLContext.init` without a server identity (measured on `cvm-sbr-ssl-5`:
///   `No TLS key/cert configured`, every SSLEngine/SSLSocket probe section and
///   `RSslLiveSession` fail). They move with `SSLContext`.
/// * `HttpsURLConnection`: the native HTTPS client, its own family.
/// * The keystore SPIs' `engineGetAttributes`: declared on 21/25, not 17.
///
/// Native code kept for the day the handshake half moves, inert until then
/// (keyed on `SSLContext.getInstance` being retired, `t27_tls::real_jsse_family`):
/// the `HttpsURLConnection` real-trust wiring and the JDK default factory /
/// context answers. Both-mode fixes that land now: AES-GCM with a 16-byte IV
/// (`jca/cipher.rs`) and bulk `byte[]` copies in the `Cipher` natives.
///
/// Record: `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_SSL_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/security/KeyStore",
        "getInstance",
        "(Ljava/lang/String;Ljava/security/Provider;)Ljava/security/KeyStore;",
    ),
    ("javax/net/ssl/SSLEngineResult", "bytesConsumed", "()I"),
    ("javax/net/ssl/SSLEngineResult", "bytesProduced", "()I"),
    (
        "javax/net/ssl/SSLEngineResult",
        "getHandshakeStatus",
        "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;",
    ),
    (
        "javax/net/ssl/SSLEngineResult",
        "getStatus",
        "()Ljavax/net/ssl/SSLEngineResult$Status;",
    ),
    (
        "javax/security/auth/x500/X500Principal",
        "<init>",
        "(Ljava/io/InputStream;)V",
    ),
    (
        "javax/security/auth/x500/X500Principal",
        "<init>",
        "(Ljava/lang/String;)V",
    ),
    (
        "javax/security/auth/x500/X500Principal",
        "<init>",
        "(Ljava/lang/String;Ljava/util/Map;)V",
    ),
    ("javax/security/auth/x500/X500Principal", "<init>", "([B)V"),
    (
        "javax/security/auth/x500/X500Principal",
        "equals",
        "(Ljava/lang/Object;)Z",
    ),
    (
        "javax/security/auth/x500/X500Principal",
        "getEncoded",
        "()[B",
    ),
    (
        "javax/security/auth/x500/X500Principal",
        "getName",
        "()Ljava/lang/String;",
    ),
    (
        "javax/security/auth/x500/X500Principal",
        "getName",
        "(Ljava/lang/String;)Ljava/lang/String;",
    ),
    (
        "javax/security/auth/x500/X500Principal",
        "getName",
        "(Ljava/lang/String;Ljava/util/Map;)Ljava/lang/String;",
    ),
    ("javax/security/auth/x500/X500Principal", "hashCode", "()I"),
    (
        "javax/security/auth/x500/X500Principal",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "sun/security/x509/X500Name",
        "asX500Principal",
        "()Ljavax/security/auth/x500/X500Principal;",
    ),
];

/// SBR wave 5, lane `fs2`, 2026-09-29 -- the `sun/nio/fs` rows wave 2 held:
/// the Linux `WatchService` pipeline (12), the `DefaultFileSystemProvider`
/// doors (2) and the `FileStore` family (`LinuxFileStore`/`UnixFileStore` 12
/// each, `getFileStore` on both providers) -- 40 triples.
///
/// # Why the state is real now
///
/// * **WatchService.** The eight genuine `LinuxWatchService` natives
///   (`inotifyInit`, `inotifyAddWatch`, `inotifyRmWatch`, `configureBlocking`,
///   `socketpair`, `poll(int,int)`, `eventSize`, `eventOffsets`) and
///   `UnixNativeDispatcher.read0`/`write0` exist (`native-io`, both modes,
///   explicitly `Bridge`), so the JDK's own `LinuxWatchService.<init>` builds
///   its poller thread and every key and event through their constructors.
///   The inotify instance and the wakeup socketpair are `FdTable` ids like
///   every `UnixNativeDispatcher` fd, so the real `close0` closes them.
///   The family retires together: a natively minted service handed to real
///   `UnixPath.register` has no poller.
/// * **Interface / abstract rows.** `java/nio/file/{FileSystem,Path,
///   WatchService,WatchKey,WatchEvent}` and the `AbstractWatch*` classes bind
///   the same callbacks and are not retired (the abstract rows serve Windows'
///   natively minted watch objects). Each callback yields: `newWatchService` /
///   `register` when the receiver's own triple is retired, the service / key /
///   event accessors when the receiver is not one of this VM's mints (no slot
///   map above its declared fields) -- `native-io` `watch_yield_if_*`.
/// * **Default provider.** Under `--real-jdk` the VM's default filesystem is
///   now `DefaultFileSystemProvider.INSTANCE.theFileSystem`, built by the real
///   `<clinit>` (`p57_default_filesystem_from_jdk`), so the retired real
///   `instance()`/`theFileSystem()` answer the object every native door
///   answers.
/// * **FileStore.** Real `getFileStore` builds `LinuxFileStore` through its
///   constructor (`stat`, `realpath`, the `/proc/self/mountinfo` walk through
///   the new `setmntent0`/`getmntent0`/`endmntent`/`getlinelen`/`rewind`);
///   `statvfs0` was already there. `UnixFileStore` retires with
///   `LinuxFileStore` (its `super.supportsFileAttributeView` is an
///   `invokespecial` a mirrored row would catch). Nothing mints a
///   `FileStore` natively under `--real-jdk` any more (`Files.getFileStore`'s
///   row is a stub; `getFileStores` answers an empty list and mints none), and
///   the abstract `java/nio/file/FileStore` callbacks yield to a retired
///   receiver.
///
/// # Held
///
/// * `newAsynchronousFileChannel` on both providers: its real body builds a
///   real `sun.nio.ch.SimpleAsynchronousFileChannelImpl`, whose `read`/
///   `write`/`lock`/`size`/... are still native rows of the NIO-channels
///   family (they decode this VM's own carrier). Retirable with that family.
///
/// # Measured
///
/// Linux/x86_64, JDK 25.0.4+7, control `cvm-sbr-int8` (`fc24028b2`), lane `cvm-sbr-fs2-1`:
///
/// ```text
///   SbrW2Fs, 30 rows vs HotSpot 25        control --real-jdk    2 differ (watch, fileStore), drift 0
///                                         this    --real-jdk    0 differ, drift 0
///     + UNRETIRE (9 rules, 40 rows)       == control --real-jdk, row for row
///     --compatible                        == control --compatible (11 rows, drift 600)
///   dial pre-screen (int8 + ENFORCE)      151/152: RJdkWatchService (inotifyInit)
///   SUITE=all --real-jdk                  152/152 both, vector-identical
///   census native-won distinct triples    529 -> 517 (sun/nio/fs 12 -> 0), none new
/// ```
///
/// Record (strict corpus, unit tests, registration delta):
/// `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_FS2_TRIPLES: &[(&str, &str, &str)] = &[
    ("sun/nio/fs/DefaultFileSystemProvider", "instance", "()Lsun/nio/fs/LinuxFileSystemProvider;"),
    ("sun/nio/fs/DefaultFileSystemProvider", "theFileSystem", "()Ljava/nio/file/FileSystem;"),
    ("sun/nio/fs/LinuxFileStore", "getAttribute", "(Ljava/lang/String;)Ljava/lang/Object;"),
    ("sun/nio/fs/LinuxFileStore", "getBlockSize", "()J"),
    ("sun/nio/fs/LinuxFileStore", "getFileStoreAttributeView", "(Ljava/lang/Class;)Ljava/nio/file/attribute/FileStoreAttributeView;"),
    ("sun/nio/fs/LinuxFileStore", "getTotalSpace", "()J"),
    ("sun/nio/fs/LinuxFileStore", "getUnallocatedSpace", "()J"),
    ("sun/nio/fs/LinuxFileStore", "getUsableSpace", "()J"),
    ("sun/nio/fs/LinuxFileStore", "isReadOnly", "()Z"),
    ("sun/nio/fs/LinuxFileStore", "name", "()Ljava/lang/String;"),
    ("sun/nio/fs/LinuxFileStore", "supportsFileAttributeView", "(Ljava/lang/Class;)Z"),
    ("sun/nio/fs/LinuxFileStore", "supportsFileAttributeView", "(Ljava/lang/String;)Z"),
    ("sun/nio/fs/LinuxFileStore", "toString", "()Ljava/lang/String;"),
    ("sun/nio/fs/LinuxFileStore", "type", "()Ljava/lang/String;"),
    ("sun/nio/fs/LinuxFileSystem", "newWatchService", "()Ljava/nio/file/WatchService;"),
    ("sun/nio/fs/LinuxFileSystemProvider", "getFileStore", "(Ljava/nio/file/Path;)Ljava/nio/file/FileStore;"),
    ("sun/nio/fs/LinuxWatchService", "close", "()V"),
    ("sun/nio/fs/LinuxWatchService", "poll", "()Ljava/nio/file/WatchKey;"),
    ("sun/nio/fs/LinuxWatchService", "poll", "(JLjava/util/concurrent/TimeUnit;)Ljava/nio/file/WatchKey;"),
    ("sun/nio/fs/LinuxWatchService", "take", "()Ljava/nio/file/WatchKey;"),
    ("sun/nio/fs/LinuxWatchService$LinuxWatchKey", "cancel", "()V"),
    ("sun/nio/fs/LinuxWatchService$LinuxWatchKey", "isValid", "()Z"),
    ("sun/nio/fs/LinuxWatchService$LinuxWatchKey", "pollEvents", "()Ljava/util/List;"),
    ("sun/nio/fs/LinuxWatchService$LinuxWatchKey", "reset", "()Z"),
    ("sun/nio/fs/LinuxWatchService$LinuxWatchKey", "watchable", "()Ljava/nio/file/Watchable;"),
    ("sun/nio/fs/UnixFileStore", "getAttribute", "(Ljava/lang/String;)Ljava/lang/Object;"),
    ("sun/nio/fs/UnixFileStore", "getBlockSize", "()J"),
    ("sun/nio/fs/UnixFileStore", "getFileStoreAttributeView", "(Ljava/lang/Class;)Ljava/nio/file/attribute/FileStoreAttributeView;"),
    ("sun/nio/fs/UnixFileStore", "getTotalSpace", "()J"),
    ("sun/nio/fs/UnixFileStore", "getUnallocatedSpace", "()J"),
    ("sun/nio/fs/UnixFileStore", "getUsableSpace", "()J"),
    ("sun/nio/fs/UnixFileStore", "isReadOnly", "()Z"),
    ("sun/nio/fs/UnixFileStore", "name", "()Ljava/lang/String;"),
    ("sun/nio/fs/UnixFileStore", "supportsFileAttributeView", "(Ljava/lang/Class;)Z"),
    ("sun/nio/fs/UnixFileStore", "supportsFileAttributeView", "(Ljava/lang/String;)Z"),
    ("sun/nio/fs/UnixFileStore", "toString", "()Ljava/lang/String;"),
    ("sun/nio/fs/UnixFileStore", "type", "()Ljava/lang/String;"),
    ("sun/nio/fs/UnixFileSystemProvider", "getFileStore", "(Ljava/nio/file/Path;)Ljava/nio/file/FileStore;"),
    ("sun/nio/fs/UnixPath", "register", "(Ljava/nio/file/WatchService;[Ljava/nio/file/WatchEvent$Kind;)Ljava/nio/file/WatchKey;"),
    ("sun/nio/fs/UnixPath", "register", "(Ljava/nio/file/WatchService;[Ljava/nio/file/WatchEvent$Kind;[Ljava/nio/file/WatchEvent$Modifier;)Ljava/nio/file/WatchKey;"),
];

/// SBR wave 5, lane JSSE (2026-09-29): the JSSE handshake half and its
/// companions, retired together so the JDK's own JSSE runs the TLS handshake
/// under `--real-jdk`. 258 triples over 24 classes: `SSLContext` (14),
/// `SSLEngine` (2) + `SSLEngineImpl` (34), `SSLSocket` (23, the concrete and
/// `Socket`-inherited rows) + `SSLSocketImpl$AppInputStream`/`$AppOutputStream`
/// (9), `SSLServerSocket` (18), `SSLSocketFactory` and `SSLServerSocketFactory`
/// (`getDefault` and the inherited no-arg `createSocket`/`createServerSocket`,
/// 2 each), `SSLParameters` (2, the ALPN pair), `KeyManagerFactory` and
/// `TrustManagerFactory` (6 each), the `sun/security/ssl` managers and factory
/// SPIs (`X509TrustManagerImpl` 7, `X509KeyManagerImpl` 6,
/// `SunX509KeyManagerImpl` 6, `TrustManagerFactoryImpl$PKIXFactory` 3 /
/// `$SimpleFactory` 2, `KeyManagerFactoryImpl$SunX509` 2), and the six
/// PKCS12/JKS keystore SPI classes (19 each). Every row has `Code` on 17, 21
/// and 25 (reflection over each image; the kind map's other rows on these
/// classes are abstract or undeclared and stay registered).
///
/// # Why it is safe now
///
/// The family is ONE unit (wave 4, trial `cvm-sbr-ssl-4` and `ssl-5`): the
/// natives mint carriers (`SSLContext` with a protocol string in slot 0, raw
/// `SSLEngine`/`SSLSocket` objects, 3-slot factories, keystore entries behind a
/// side-table id) and every other native reads them by that convention. A
/// native `SSLContext` beside real managers finds no server identity
/// (`ssl-5`), so everything moves at once and no carrier is minted under
/// `--real-jdk`. What makes the JDK's own objects safe with the rest of the VM:
/// * the wave-4 pieces that were inert until this table: the native
///   `HttpsURLConnection` (held) reads a REAL factory's trust and key managers
///   (`t27_tls::attach_real_jsse_managers`), and the default factory / context
///   helpers answer the JDK's own objects (`t27_tls::real_jsse_family`, keyed on
///   `SSLContext.getInstance` being retired, so they switch on with this table);
/// * the dispatch doors' by-name arms that routed a real
///   `SSLContextImpl`/`SSLSocketFactoryImpl`/`SSLSocketImpl`/
///   `SSLServerSocketFactoryImpl` to the natives on the public class
///   (`vm_exec.rs`, `invoke.rs`) are closed under `--real-jdk` with this table
///   live -- the abstract `SSLSocket`/`SSLSocketFactory`/`SSLServerSocketFactory`
///   natives (no `Code`, not here) stay registered for `--compatible`'s carriers
///   and are no longer reachable from a real object;
/// * `SSLSocketFactory`'s two rows are kept in `--compatible` by
///   `register_inner`'s real-net-socket arm honouring
///   `retagging_retired_shadow` (it drops `SSLSocketFactory` stubs; a re-tagged
///   `Bridge` is not one);
/// * ALPN: the native engines keep it in `t27_tls::sslparams_alpn_table`;
///   retired with the engines, the real `SSLParameters.applicationProtocols`
///   field is the only store, and no native still live under `--real-jdk`
///   reads the table.
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW` (a row, a class, or `SSLContext.getInstance`
/// for the gates) restores the pre-wave VM.
///
/// # Measured (JDK 25.0.4, linux; control `cvm-sbr-int8`, final `cvm-sbr-jsse-1`)
///
/// ```text
///   probes vs HotSpot 25 (HotSpot lines without a match; drift 0 everywhere):
///     SbrW4Ssl (106 lines): int8 --real-jdk 42 -> jsse-1 --real-jdk 0
///     SbrW5Jsse (29 lines, ALPN/selector/EIA/defaults/HUC): int8 11 -> jsse-1 0
///     jsse-1 + UNRETIRE=<the lane's classes> (258 rows armed) == int8, both probes
///     --compatible: jsse-1 == int8, both probes, line for line
///   suite (152): int8 152/152, jsse-1 152/152; native-won 529 -> 489, family
///     56 -> 1 (HttpsURLConnection.<init>, held)
///   strict corpus: only KeyStoreTypeProbe/strict/[scalar-deopt] (the known
///     unfiltered JIT exit line; all 50 probe rows match)
///   registrations re-tagged: 268 (25/linux kind map)
///   timing: the handshake as JDK bytecode is ~10x the native control on an
///     SSLEngine loopback (typ. 51 vs 5 ms, RSA-2048 TLS 1.3), ~5x HotSpot;
///     profile flat (lane record)
/// ```
///
/// # Held, with the reason
///
/// * `javax/net/ssl/HttpsURLConnection` / `sun/net/www/protocol/https`: the
///   native HTTPS client, its own family; it now reads real JSSE trust.
/// * the keystore SPIs' `engineGetAttributes`: declared on 21/25, not on 17.
/// * the abstract rows (`SSLSocket.startHandshake`, `SSLSocketFactory.createSocket`
///   overloads, `SSLEngine.beginHandshake`, ...): no `Code` to yield to.
///
/// Record: `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_JSSE_TRIPLES: &[(&str, &str, &str)] = &[
    ("javax/net/ssl/KeyManagerFactory", "getAlgorithm", "()Ljava/lang/String;"),
    ("javax/net/ssl/KeyManagerFactory", "getDefaultAlgorithm", "()Ljava/lang/String;"),
    ("javax/net/ssl/KeyManagerFactory", "getInstance", "(Ljava/lang/String;)Ljavax/net/ssl/KeyManagerFactory;"),
    ("javax/net/ssl/KeyManagerFactory", "getKeyManagers", "()[Ljavax/net/ssl/KeyManager;"),
    ("javax/net/ssl/KeyManagerFactory", "init", "(Ljava/security/KeyStore;[C)V"),
    ("javax/net/ssl/KeyManagerFactory", "init", "(Ljavax/net/ssl/ManagerFactoryParameters;)V"),
    ("javax/net/ssl/SSLContext", "createSSLEngine", "()Ljavax/net/ssl/SSLEngine;"),
    ("javax/net/ssl/SSLContext", "createSSLEngine", "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;"),
    ("javax/net/ssl/SSLContext", "getClientSessionContext", "()Ljavax/net/ssl/SSLSessionContext;"),
    ("javax/net/ssl/SSLContext", "getDefault", "()Ljavax/net/ssl/SSLContext;"),
    ("javax/net/ssl/SSLContext", "getDefaultSSLParameters", "()Ljavax/net/ssl/SSLParameters;"),
    ("javax/net/ssl/SSLContext", "getInstance", "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;"),
    ("javax/net/ssl/SSLContext", "getProtocol", "()Ljava/lang/String;"),
    ("javax/net/ssl/SSLContext", "getProvider", "()Ljava/security/Provider;"),
    ("javax/net/ssl/SSLContext", "getServerSessionContext", "()Ljavax/net/ssl/SSLSessionContext;"),
    ("javax/net/ssl/SSLContext", "getServerSocketFactory", "()Ljavax/net/ssl/SSLServerSocketFactory;"),
    ("javax/net/ssl/SSLContext", "getSocketFactory", "()Ljavax/net/ssl/SSLSocketFactory;"),
    ("javax/net/ssl/SSLContext", "getSupportedSSLParameters", "()Ljavax/net/ssl/SSLParameters;"),
    ("javax/net/ssl/SSLContext", "init", "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V"),
    ("javax/net/ssl/SSLContext", "setDefault", "(Ljavax/net/ssl/SSLContext;)V"),
    ("javax/net/ssl/SSLEngine", "unwrap", "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"),
    ("javax/net/ssl/SSLEngine", "wrap", "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"),
    ("javax/net/ssl/SSLParameters", "getApplicationProtocols", "()[Ljava/lang/String;"),
    ("javax/net/ssl/SSLParameters", "setApplicationProtocols", "([Ljava/lang/String;)V"),
    ("javax/net/ssl/SSLServerSocket", "accept", "()Ljava/net/Socket;"),
    ("javax/net/ssl/SSLServerSocket", "bind", "(Ljava/net/SocketAddress;)V"),
    ("javax/net/ssl/SSLServerSocket", "close", "()V"),
    ("javax/net/ssl/SSLServerSocket", "getInetAddress", "()Ljava/net/InetAddress;"),
    ("javax/net/ssl/SSLServerSocket", "getLocalPort", "()I"),
    ("javax/net/ssl/SSLServerSocket", "getLocalSocketAddress", "()Ljava/net/SocketAddress;"),
    ("javax/net/ssl/SSLServerSocket", "getOption", "(Ljava/net/SocketOption;)Ljava/lang/Object;"),
    ("javax/net/ssl/SSLServerSocket", "getReceiveBufferSize", "()I"),
    ("javax/net/ssl/SSLServerSocket", "getReuseAddress", "()Z"),
    ("javax/net/ssl/SSLServerSocket", "getSoTimeout", "()I"),
    ("javax/net/ssl/SSLServerSocket", "isBound", "()Z"),
    ("javax/net/ssl/SSLServerSocket", "isClosed", "()Z"),
    ("javax/net/ssl/SSLServerSocket", "setOption", "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/net/ServerSocket;"),
    ("javax/net/ssl/SSLServerSocket", "setReceiveBufferSize", "(I)V"),
    ("javax/net/ssl/SSLServerSocket", "setReuseAddress", "(Z)V"),
    ("javax/net/ssl/SSLServerSocket", "setSoTimeout", "(I)V"),
    ("javax/net/ssl/SSLServerSocket", "supportedOptions", "()Ljava/util/Set;"),
    ("javax/net/ssl/SSLServerSocket", "toString", "()Ljava/lang/String;"),
    ("javax/net/ssl/SSLServerSocketFactory", "createServerSocket", "()Ljava/net/ServerSocket;"),
    ("javax/net/ssl/SSLServerSocketFactory", "getDefault", "()Ljavax/net/ServerSocketFactory;"),
    ("javax/net/ssl/SSLSocket", "close", "()V"),
    ("javax/net/ssl/SSLSocket", "connect", "(Ljava/net/SocketAddress;)V"),
    ("javax/net/ssl/SSLSocket", "connect", "(Ljava/net/SocketAddress;I)V"),
    ("javax/net/ssl/SSLSocket", "getApplicationProtocol", "()Ljava/lang/String;"),
    ("javax/net/ssl/SSLSocket", "getHandshakeApplicationProtocol", "()Ljava/lang/String;"),
    ("javax/net/ssl/SSLSocket", "getHandshakeSession", "()Ljavax/net/ssl/SSLSession;"),
    ("javax/net/ssl/SSLSocket", "getInetAddress", "()Ljava/net/InetAddress;"),
    ("javax/net/ssl/SSLSocket", "getInputStream", "()Ljava/io/InputStream;"),
    ("javax/net/ssl/SSLSocket", "getLocalAddress", "()Ljava/net/InetAddress;"),
    ("javax/net/ssl/SSLSocket", "getLocalPort", "()I"),
    ("javax/net/ssl/SSLSocket", "getLocalSocketAddress", "()Ljava/net/SocketAddress;"),
    ("javax/net/ssl/SSLSocket", "getOutputStream", "()Ljava/io/OutputStream;"),
    ("javax/net/ssl/SSLSocket", "getPort", "()I"),
    ("javax/net/ssl/SSLSocket", "getRemoteSocketAddress", "()Ljava/net/SocketAddress;"),
    ("javax/net/ssl/SSLSocket", "getSSLParameters", "()Ljavax/net/ssl/SSLParameters;"),
    ("javax/net/ssl/SSLSocket", "getSoTimeout", "()I"),
    ("javax/net/ssl/SSLSocket", "isBound", "()Z"),
    ("javax/net/ssl/SSLSocket", "isClosed", "()Z"),
    ("javax/net/ssl/SSLSocket", "isConnected", "()Z"),
    ("javax/net/ssl/SSLSocket", "isInputShutdown", "()Z"),
    ("javax/net/ssl/SSLSocket", "isOutputShutdown", "()Z"),
    ("javax/net/ssl/SSLSocket", "setSSLParameters", "(Ljavax/net/ssl/SSLParameters;)V"),
    ("javax/net/ssl/SSLSocket", "setSoTimeout", "(I)V"),
    ("javax/net/ssl/SSLSocketFactory", "createSocket", "()Ljava/net/Socket;"),
    ("javax/net/ssl/SSLSocketFactory", "getDefault", "()Ljavax/net/SocketFactory;"),
    ("javax/net/ssl/TrustManagerFactory", "getAlgorithm", "()Ljava/lang/String;"),
    ("javax/net/ssl/TrustManagerFactory", "getDefaultAlgorithm", "()Ljava/lang/String;"),
    ("javax/net/ssl/TrustManagerFactory", "getInstance", "(Ljava/lang/String;)Ljavax/net/ssl/TrustManagerFactory;"),
    ("javax/net/ssl/TrustManagerFactory", "getTrustManagers", "()[Ljavax/net/ssl/TrustManager;"),
    ("javax/net/ssl/TrustManagerFactory", "init", "(Ljava/security/KeyStore;)V"),
    ("javax/net/ssl/TrustManagerFactory", "init", "(Ljavax/net/ssl/ManagerFactoryParameters;)V"),
    ("sun/security/pkcs12/PKCS12KeyStore", "engineAliases", "()Ljava/util/Enumeration;"),
    ("sun/security/pkcs12/PKCS12KeyStore", "engineContainsAlias", "(Ljava/lang/String;)Z"),
    ("sun/security/pkcs12/PKCS12KeyStore", "engineDeleteEntry", "(Ljava/lang/String;)V"),
    ("sun/security/pkcs12/PKCS12KeyStore", "engineEntryInstanceOf", "(Ljava/lang/String;Ljava/lang/Class;)Z"),
    ("sun/security/pkcs12/PKCS12KeyStore", "engineGetCertificate", "(Ljava/lang/String;)Ljava/security/cert/Certificate;"),
    ("sun/security/pkcs12/PKCS12KeyStore", "engineGetCertificateAlias", "(Ljava/security/cert/Certificate;)Ljava/lang/String;"),
    ("sun/security/pkcs12/PKCS12KeyStore", "engineGetCertificateChain", "(Ljava/lang/String;)[Ljava/security/cert/Certificate;"),
    ("sun/security/pkcs12/PKCS12KeyStore", "engineGetCreationDate", "(Ljava/lang/String;)Ljava/util/Date;"),
    ("sun/security/pkcs12/PKCS12KeyStore", "engineGetEntry", "(Ljava/lang/String;Ljava/security/KeyStore$ProtectionParameter;)Ljava/security/KeyStore$Entry;"),
    ("sun/security/pkcs12/PKCS12KeyStore", "engineGetKey", "(Ljava/lang/String;[C)Ljava/security/Key;"),
    ("sun/security/pkcs12/PKCS12KeyStore", "engineIsCertificateEntry", "(Ljava/lang/String;)Z"),
    ("sun/security/pkcs12/PKCS12KeyStore", "engineIsKeyEntry", "(Ljava/lang/String;)Z"),
    ("sun/security/pkcs12/PKCS12KeyStore", "engineLoad", "(Ljava/io/InputStream;[C)V"),
    ("sun/security/pkcs12/PKCS12KeyStore", "engineSetCertificateEntry", "(Ljava/lang/String;Ljava/security/cert/Certificate;)V"),
    ("sun/security/pkcs12/PKCS12KeyStore", "engineSetEntry", "(Ljava/lang/String;Ljava/security/KeyStore$Entry;Ljava/security/KeyStore$ProtectionParameter;)V"),
    ("sun/security/pkcs12/PKCS12KeyStore", "engineSetKeyEntry", "(Ljava/lang/String;Ljava/security/Key;[C[Ljava/security/cert/Certificate;)V"),
    ("sun/security/pkcs12/PKCS12KeyStore", "engineSetKeyEntry", "(Ljava/lang/String;[B[Ljava/security/cert/Certificate;)V"),
    ("sun/security/pkcs12/PKCS12KeyStore", "engineSize", "()I"),
    ("sun/security/pkcs12/PKCS12KeyStore", "engineStore", "(Ljava/io/OutputStream;[C)V"),
    ("sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12", "engineAliases", "()Ljava/util/Enumeration;"),
    ("sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12", "engineContainsAlias", "(Ljava/lang/String;)Z"),
    ("sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12", "engineDeleteEntry", "(Ljava/lang/String;)V"),
    ("sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12", "engineEntryInstanceOf", "(Ljava/lang/String;Ljava/lang/Class;)Z"),
    ("sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12", "engineGetCertificate", "(Ljava/lang/String;)Ljava/security/cert/Certificate;"),
    ("sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12", "engineGetCertificateAlias", "(Ljava/security/cert/Certificate;)Ljava/lang/String;"),
    ("sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12", "engineGetCertificateChain", "(Ljava/lang/String;)[Ljava/security/cert/Certificate;"),
    ("sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12", "engineGetCreationDate", "(Ljava/lang/String;)Ljava/util/Date;"),
    ("sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12", "engineGetEntry", "(Ljava/lang/String;Ljava/security/KeyStore$ProtectionParameter;)Ljava/security/KeyStore$Entry;"),
    ("sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12", "engineGetKey", "(Ljava/lang/String;[C)Ljava/security/Key;"),
    ("sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12", "engineIsCertificateEntry", "(Ljava/lang/String;)Z"),
    ("sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12", "engineIsKeyEntry", "(Ljava/lang/String;)Z"),
    ("sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12", "engineLoad", "(Ljava/io/InputStream;[C)V"),
    ("sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12", "engineSetCertificateEntry", "(Ljava/lang/String;Ljava/security/cert/Certificate;)V"),
    ("sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12", "engineSetEntry", "(Ljava/lang/String;Ljava/security/KeyStore$Entry;Ljava/security/KeyStore$ProtectionParameter;)V"),
    ("sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12", "engineSetKeyEntry", "(Ljava/lang/String;Ljava/security/Key;[C[Ljava/security/cert/Certificate;)V"),
    ("sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12", "engineSetKeyEntry", "(Ljava/lang/String;[B[Ljava/security/cert/Certificate;)V"),
    ("sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12", "engineSize", "()I"),
    ("sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12", "engineStore", "(Ljava/io/OutputStream;[C)V"),
    ("sun/security/provider/JavaKeyStore", "engineAliases", "()Ljava/util/Enumeration;"),
    ("sun/security/provider/JavaKeyStore", "engineContainsAlias", "(Ljava/lang/String;)Z"),
    ("sun/security/provider/JavaKeyStore", "engineDeleteEntry", "(Ljava/lang/String;)V"),
    ("sun/security/provider/JavaKeyStore", "engineEntryInstanceOf", "(Ljava/lang/String;Ljava/lang/Class;)Z"),
    ("sun/security/provider/JavaKeyStore", "engineGetCertificate", "(Ljava/lang/String;)Ljava/security/cert/Certificate;"),
    ("sun/security/provider/JavaKeyStore", "engineGetCertificateAlias", "(Ljava/security/cert/Certificate;)Ljava/lang/String;"),
    ("sun/security/provider/JavaKeyStore", "engineGetCertificateChain", "(Ljava/lang/String;)[Ljava/security/cert/Certificate;"),
    ("sun/security/provider/JavaKeyStore", "engineGetCreationDate", "(Ljava/lang/String;)Ljava/util/Date;"),
    ("sun/security/provider/JavaKeyStore", "engineGetEntry", "(Ljava/lang/String;Ljava/security/KeyStore$ProtectionParameter;)Ljava/security/KeyStore$Entry;"),
    ("sun/security/provider/JavaKeyStore", "engineGetKey", "(Ljava/lang/String;[C)Ljava/security/Key;"),
    ("sun/security/provider/JavaKeyStore", "engineIsCertificateEntry", "(Ljava/lang/String;)Z"),
    ("sun/security/provider/JavaKeyStore", "engineIsKeyEntry", "(Ljava/lang/String;)Z"),
    ("sun/security/provider/JavaKeyStore", "engineLoad", "(Ljava/io/InputStream;[C)V"),
    ("sun/security/provider/JavaKeyStore", "engineSetCertificateEntry", "(Ljava/lang/String;Ljava/security/cert/Certificate;)V"),
    ("sun/security/provider/JavaKeyStore", "engineSetEntry", "(Ljava/lang/String;Ljava/security/KeyStore$Entry;Ljava/security/KeyStore$ProtectionParameter;)V"),
    ("sun/security/provider/JavaKeyStore", "engineSetKeyEntry", "(Ljava/lang/String;Ljava/security/Key;[C[Ljava/security/cert/Certificate;)V"),
    ("sun/security/provider/JavaKeyStore", "engineSetKeyEntry", "(Ljava/lang/String;[B[Ljava/security/cert/Certificate;)V"),
    ("sun/security/provider/JavaKeyStore", "engineSize", "()I"),
    ("sun/security/provider/JavaKeyStore", "engineStore", "(Ljava/io/OutputStream;[C)V"),
    ("sun/security/provider/JavaKeyStore$CaseExactJKS", "engineAliases", "()Ljava/util/Enumeration;"),
    ("sun/security/provider/JavaKeyStore$CaseExactJKS", "engineContainsAlias", "(Ljava/lang/String;)Z"),
    ("sun/security/provider/JavaKeyStore$CaseExactJKS", "engineDeleteEntry", "(Ljava/lang/String;)V"),
    ("sun/security/provider/JavaKeyStore$CaseExactJKS", "engineEntryInstanceOf", "(Ljava/lang/String;Ljava/lang/Class;)Z"),
    ("sun/security/provider/JavaKeyStore$CaseExactJKS", "engineGetCertificate", "(Ljava/lang/String;)Ljava/security/cert/Certificate;"),
    ("sun/security/provider/JavaKeyStore$CaseExactJKS", "engineGetCertificateAlias", "(Ljava/security/cert/Certificate;)Ljava/lang/String;"),
    ("sun/security/provider/JavaKeyStore$CaseExactJKS", "engineGetCertificateChain", "(Ljava/lang/String;)[Ljava/security/cert/Certificate;"),
    ("sun/security/provider/JavaKeyStore$CaseExactJKS", "engineGetCreationDate", "(Ljava/lang/String;)Ljava/util/Date;"),
    ("sun/security/provider/JavaKeyStore$CaseExactJKS", "engineGetEntry", "(Ljava/lang/String;Ljava/security/KeyStore$ProtectionParameter;)Ljava/security/KeyStore$Entry;"),
    ("sun/security/provider/JavaKeyStore$CaseExactJKS", "engineGetKey", "(Ljava/lang/String;[C)Ljava/security/Key;"),
    ("sun/security/provider/JavaKeyStore$CaseExactJKS", "engineIsCertificateEntry", "(Ljava/lang/String;)Z"),
    ("sun/security/provider/JavaKeyStore$CaseExactJKS", "engineIsKeyEntry", "(Ljava/lang/String;)Z"),
    ("sun/security/provider/JavaKeyStore$CaseExactJKS", "engineLoad", "(Ljava/io/InputStream;[C)V"),
    ("sun/security/provider/JavaKeyStore$CaseExactJKS", "engineSetCertificateEntry", "(Ljava/lang/String;Ljava/security/cert/Certificate;)V"),
    ("sun/security/provider/JavaKeyStore$CaseExactJKS", "engineSetEntry", "(Ljava/lang/String;Ljava/security/KeyStore$Entry;Ljava/security/KeyStore$ProtectionParameter;)V"),
    ("sun/security/provider/JavaKeyStore$CaseExactJKS", "engineSetKeyEntry", "(Ljava/lang/String;Ljava/security/Key;[C[Ljava/security/cert/Certificate;)V"),
    ("sun/security/provider/JavaKeyStore$CaseExactJKS", "engineSetKeyEntry", "(Ljava/lang/String;[B[Ljava/security/cert/Certificate;)V"),
    ("sun/security/provider/JavaKeyStore$CaseExactJKS", "engineSize", "()I"),
    ("sun/security/provider/JavaKeyStore$CaseExactJKS", "engineStore", "(Ljava/io/OutputStream;[C)V"),
    ("sun/security/provider/JavaKeyStore$DualFormatJKS", "engineAliases", "()Ljava/util/Enumeration;"),
    ("sun/security/provider/JavaKeyStore$DualFormatJKS", "engineContainsAlias", "(Ljava/lang/String;)Z"),
    ("sun/security/provider/JavaKeyStore$DualFormatJKS", "engineDeleteEntry", "(Ljava/lang/String;)V"),
    ("sun/security/provider/JavaKeyStore$DualFormatJKS", "engineEntryInstanceOf", "(Ljava/lang/String;Ljava/lang/Class;)Z"),
    ("sun/security/provider/JavaKeyStore$DualFormatJKS", "engineGetCertificate", "(Ljava/lang/String;)Ljava/security/cert/Certificate;"),
    ("sun/security/provider/JavaKeyStore$DualFormatJKS", "engineGetCertificateAlias", "(Ljava/security/cert/Certificate;)Ljava/lang/String;"),
    ("sun/security/provider/JavaKeyStore$DualFormatJKS", "engineGetCertificateChain", "(Ljava/lang/String;)[Ljava/security/cert/Certificate;"),
    ("sun/security/provider/JavaKeyStore$DualFormatJKS", "engineGetCreationDate", "(Ljava/lang/String;)Ljava/util/Date;"),
    ("sun/security/provider/JavaKeyStore$DualFormatJKS", "engineGetEntry", "(Ljava/lang/String;Ljava/security/KeyStore$ProtectionParameter;)Ljava/security/KeyStore$Entry;"),
    ("sun/security/provider/JavaKeyStore$DualFormatJKS", "engineGetKey", "(Ljava/lang/String;[C)Ljava/security/Key;"),
    ("sun/security/provider/JavaKeyStore$DualFormatJKS", "engineIsCertificateEntry", "(Ljava/lang/String;)Z"),
    ("sun/security/provider/JavaKeyStore$DualFormatJKS", "engineIsKeyEntry", "(Ljava/lang/String;)Z"),
    ("sun/security/provider/JavaKeyStore$DualFormatJKS", "engineLoad", "(Ljava/io/InputStream;[C)V"),
    ("sun/security/provider/JavaKeyStore$DualFormatJKS", "engineSetCertificateEntry", "(Ljava/lang/String;Ljava/security/cert/Certificate;)V"),
    ("sun/security/provider/JavaKeyStore$DualFormatJKS", "engineSetEntry", "(Ljava/lang/String;Ljava/security/KeyStore$Entry;Ljava/security/KeyStore$ProtectionParameter;)V"),
    ("sun/security/provider/JavaKeyStore$DualFormatJKS", "engineSetKeyEntry", "(Ljava/lang/String;Ljava/security/Key;[C[Ljava/security/cert/Certificate;)V"),
    ("sun/security/provider/JavaKeyStore$DualFormatJKS", "engineSetKeyEntry", "(Ljava/lang/String;[B[Ljava/security/cert/Certificate;)V"),
    ("sun/security/provider/JavaKeyStore$DualFormatJKS", "engineSize", "()I"),
    ("sun/security/provider/JavaKeyStore$DualFormatJKS", "engineStore", "(Ljava/io/OutputStream;[C)V"),
    ("sun/security/provider/JavaKeyStore$JKS", "engineAliases", "()Ljava/util/Enumeration;"),
    ("sun/security/provider/JavaKeyStore$JKS", "engineContainsAlias", "(Ljava/lang/String;)Z"),
    ("sun/security/provider/JavaKeyStore$JKS", "engineDeleteEntry", "(Ljava/lang/String;)V"),
    ("sun/security/provider/JavaKeyStore$JKS", "engineEntryInstanceOf", "(Ljava/lang/String;Ljava/lang/Class;)Z"),
    ("sun/security/provider/JavaKeyStore$JKS", "engineGetCertificate", "(Ljava/lang/String;)Ljava/security/cert/Certificate;"),
    ("sun/security/provider/JavaKeyStore$JKS", "engineGetCertificateAlias", "(Ljava/security/cert/Certificate;)Ljava/lang/String;"),
    ("sun/security/provider/JavaKeyStore$JKS", "engineGetCertificateChain", "(Ljava/lang/String;)[Ljava/security/cert/Certificate;"),
    ("sun/security/provider/JavaKeyStore$JKS", "engineGetCreationDate", "(Ljava/lang/String;)Ljava/util/Date;"),
    ("sun/security/provider/JavaKeyStore$JKS", "engineGetEntry", "(Ljava/lang/String;Ljava/security/KeyStore$ProtectionParameter;)Ljava/security/KeyStore$Entry;"),
    ("sun/security/provider/JavaKeyStore$JKS", "engineGetKey", "(Ljava/lang/String;[C)Ljava/security/Key;"),
    ("sun/security/provider/JavaKeyStore$JKS", "engineIsCertificateEntry", "(Ljava/lang/String;)Z"),
    ("sun/security/provider/JavaKeyStore$JKS", "engineIsKeyEntry", "(Ljava/lang/String;)Z"),
    ("sun/security/provider/JavaKeyStore$JKS", "engineLoad", "(Ljava/io/InputStream;[C)V"),
    ("sun/security/provider/JavaKeyStore$JKS", "engineSetCertificateEntry", "(Ljava/lang/String;Ljava/security/cert/Certificate;)V"),
    ("sun/security/provider/JavaKeyStore$JKS", "engineSetEntry", "(Ljava/lang/String;Ljava/security/KeyStore$Entry;Ljava/security/KeyStore$ProtectionParameter;)V"),
    ("sun/security/provider/JavaKeyStore$JKS", "engineSetKeyEntry", "(Ljava/lang/String;Ljava/security/Key;[C[Ljava/security/cert/Certificate;)V"),
    ("sun/security/provider/JavaKeyStore$JKS", "engineSetKeyEntry", "(Ljava/lang/String;[B[Ljava/security/cert/Certificate;)V"),
    ("sun/security/provider/JavaKeyStore$JKS", "engineSize", "()I"),
    ("sun/security/provider/JavaKeyStore$JKS", "engineStore", "(Ljava/io/OutputStream;[C)V"),
    ("sun/security/ssl/KeyManagerFactoryImpl$SunX509", "engineGetKeyManagers", "()[Ljavax/net/ssl/KeyManager;"),
    ("sun/security/ssl/KeyManagerFactoryImpl$SunX509", "engineInit", "(Ljava/security/KeyStore;[C)V"),
    ("sun/security/ssl/SSLEngineImpl", "beginHandshake", "()V"),
    ("sun/security/ssl/SSLEngineImpl", "closeInbound", "()V"),
    ("sun/security/ssl/SSLEngineImpl", "closeOutbound", "()V"),
    ("sun/security/ssl/SSLEngineImpl", "getApplicationProtocol", "()Ljava/lang/String;"),
    ("sun/security/ssl/SSLEngineImpl", "getDelegatedTask", "()Ljava/lang/Runnable;"),
    ("sun/security/ssl/SSLEngineImpl", "getEnabledCipherSuites", "()[Ljava/lang/String;"),
    ("sun/security/ssl/SSLEngineImpl", "getEnabledProtocols", "()[Ljava/lang/String;"),
    ("sun/security/ssl/SSLEngineImpl", "getHandshakeApplicationProtocol", "()Ljava/lang/String;"),
    ("sun/security/ssl/SSLEngineImpl", "getHandshakeApplicationProtocolSelector", "()Ljava/util/function/BiFunction;"),
    ("sun/security/ssl/SSLEngineImpl", "getHandshakeSession", "()Ljavax/net/ssl/SSLSession;"),
    ("sun/security/ssl/SSLEngineImpl", "getHandshakeStatus", "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;"),
    ("sun/security/ssl/SSLEngineImpl", "getNeedClientAuth", "()Z"),
    ("sun/security/ssl/SSLEngineImpl", "getSSLParameters", "()Ljavax/net/ssl/SSLParameters;"),
    ("sun/security/ssl/SSLEngineImpl", "getSession", "()Ljavax/net/ssl/SSLSession;"),
    ("sun/security/ssl/SSLEngineImpl", "getSupportedCipherSuites", "()[Ljava/lang/String;"),
    ("sun/security/ssl/SSLEngineImpl", "getSupportedProtocols", "()[Ljava/lang/String;"),
    ("sun/security/ssl/SSLEngineImpl", "getUseClientMode", "()Z"),
    ("sun/security/ssl/SSLEngineImpl", "getWantClientAuth", "()Z"),
    ("sun/security/ssl/SSLEngineImpl", "isInboundDone", "()Z"),
    ("sun/security/ssl/SSLEngineImpl", "isOutboundDone", "()Z"),
    ("sun/security/ssl/SSLEngineImpl", "setEnabledCipherSuites", "([Ljava/lang/String;)V"),
    ("sun/security/ssl/SSLEngineImpl", "setEnabledProtocols", "([Ljava/lang/String;)V"),
    ("sun/security/ssl/SSLEngineImpl", "setHandshakeApplicationProtocolSelector", "(Ljava/util/function/BiFunction;)V"),
    ("sun/security/ssl/SSLEngineImpl", "setNeedClientAuth", "(Z)V"),
    ("sun/security/ssl/SSLEngineImpl", "setSSLParameters", "(Ljavax/net/ssl/SSLParameters;)V"),
    ("sun/security/ssl/SSLEngineImpl", "setUseClientMode", "(Z)V"),
    ("sun/security/ssl/SSLEngineImpl", "setWantClientAuth", "(Z)V"),
    ("sun/security/ssl/SSLEngineImpl", "toString", "()Ljava/lang/String;"),
    ("sun/security/ssl/SSLEngineImpl", "unwrap", "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"),
    ("sun/security/ssl/SSLEngineImpl", "unwrap", "(Ljava/nio/ByteBuffer;[Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"),
    ("sun/security/ssl/SSLEngineImpl", "unwrap", "(Ljava/nio/ByteBuffer;[Ljava/nio/ByteBuffer;II)Ljavax/net/ssl/SSLEngineResult;"),
    ("sun/security/ssl/SSLEngineImpl", "wrap", "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"),
    ("sun/security/ssl/SSLEngineImpl", "wrap", "([Ljava/nio/ByteBuffer;IILjava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"),
    ("sun/security/ssl/SSLEngineImpl", "wrap", "([Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"),
    ("sun/security/ssl/SSLSocketImpl$AppInputStream", "available", "()I"),
    ("sun/security/ssl/SSLSocketImpl$AppInputStream", "close", "()V"),
    ("sun/security/ssl/SSLSocketImpl$AppInputStream", "read", "()I"),
    ("sun/security/ssl/SSLSocketImpl$AppInputStream", "read", "([BII)I"),
    ("sun/security/ssl/SSLSocketImpl$AppInputStream", "skip", "(J)J"),
    ("sun/security/ssl/SSLSocketImpl$AppOutputStream", "close", "()V"),
    ("sun/security/ssl/SSLSocketImpl$AppOutputStream", "flush", "()V"),
    ("sun/security/ssl/SSLSocketImpl$AppOutputStream", "write", "(I)V"),
    ("sun/security/ssl/SSLSocketImpl$AppOutputStream", "write", "([BII)V"),
    ("sun/security/ssl/SunX509KeyManagerImpl", "chooseClientAlias", "([Ljava/lang/String;[Ljava/security/Principal;Ljava/net/Socket;)Ljava/lang/String;"),
    ("sun/security/ssl/SunX509KeyManagerImpl", "chooseServerAlias", "(Ljava/lang/String;[Ljava/security/Principal;Ljava/net/Socket;)Ljava/lang/String;"),
    ("sun/security/ssl/SunX509KeyManagerImpl", "getCertificateChain", "(Ljava/lang/String;)[Ljava/security/cert/X509Certificate;"),
    ("sun/security/ssl/SunX509KeyManagerImpl", "getClientAliases", "(Ljava/lang/String;[Ljava/security/Principal;)[Ljava/lang/String;"),
    ("sun/security/ssl/SunX509KeyManagerImpl", "getPrivateKey", "(Ljava/lang/String;)Ljava/security/PrivateKey;"),
    ("sun/security/ssl/SunX509KeyManagerImpl", "getServerAliases", "(Ljava/lang/String;[Ljava/security/Principal;)[Ljava/lang/String;"),
    ("sun/security/ssl/TrustManagerFactoryImpl$PKIXFactory", "engineGetTrustManagers", "()[Ljavax/net/ssl/TrustManager;"),
    ("sun/security/ssl/TrustManagerFactoryImpl$PKIXFactory", "engineInit", "(Ljava/security/KeyStore;)V"),
    ("sun/security/ssl/TrustManagerFactoryImpl$PKIXFactory", "engineInit", "(Ljavax/net/ssl/ManagerFactoryParameters;)V"),
    ("sun/security/ssl/TrustManagerFactoryImpl$SimpleFactory", "engineGetTrustManagers", "()[Ljavax/net/ssl/TrustManager;"),
    ("sun/security/ssl/TrustManagerFactoryImpl$SimpleFactory", "engineInit", "(Ljava/security/KeyStore;)V"),
    ("sun/security/ssl/X509KeyManagerImpl", "chooseClientAlias", "([Ljava/lang/String;[Ljava/security/Principal;Ljava/net/Socket;)Ljava/lang/String;"),
    ("sun/security/ssl/X509KeyManagerImpl", "chooseServerAlias", "(Ljava/lang/String;[Ljava/security/Principal;Ljava/net/Socket;)Ljava/lang/String;"),
    ("sun/security/ssl/X509KeyManagerImpl", "getCertificateChain", "(Ljava/lang/String;)[Ljava/security/cert/X509Certificate;"),
    ("sun/security/ssl/X509KeyManagerImpl", "getClientAliases", "(Ljava/lang/String;[Ljava/security/Principal;)[Ljava/lang/String;"),
    ("sun/security/ssl/X509KeyManagerImpl", "getPrivateKey", "(Ljava/lang/String;)Ljava/security/PrivateKey;"),
    ("sun/security/ssl/X509KeyManagerImpl", "getServerAliases", "(Ljava/lang/String;[Ljava/security/Principal;)[Ljava/lang/String;"),
    ("sun/security/ssl/X509TrustManagerImpl", "checkClientTrusted", "([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V"),
    ("sun/security/ssl/X509TrustManagerImpl", "checkClientTrusted", "([Ljava/security/cert/X509Certificate;Ljava/lang/String;Ljava/net/Socket;)V"),
    ("sun/security/ssl/X509TrustManagerImpl", "checkClientTrusted", "([Ljava/security/cert/X509Certificate;Ljava/lang/String;Ljavax/net/ssl/SSLEngine;)V"),
    ("sun/security/ssl/X509TrustManagerImpl", "checkServerTrusted", "([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V"),
    ("sun/security/ssl/X509TrustManagerImpl", "checkServerTrusted", "([Ljava/security/cert/X509Certificate;Ljava/lang/String;Ljava/net/Socket;)V"),
    ("sun/security/ssl/X509TrustManagerImpl", "checkServerTrusted", "([Ljava/security/cert/X509Certificate;Ljava/lang/String;Ljavax/net/ssl/SSLEngine;)V"),
    ("sun/security/ssl/X509TrustManagerImpl", "getAcceptedIssuers", "()[Ljava/security/cert/X509Certificate;"),
];

/// SBR wave 5, lane FJP2, 2026-09-29: the rows wave 4 FJP held
/// (`RETIRED_SHADOW_SBR_FJP_TRIPLES`'s doc) -- `Runtime.load0`/`loadLibrary0`,
/// `jdk/internal/misc/Signal.raise` and `sun/misc/Signal` -- 8 triples, 14
/// `bridge` registrations in the 25/linux kind map. `Code` on 17, 21 and 25 for
/// every row (`sun/misc/Signal` lives in `jdk.unsupported`).
///
/// # What had to become real first
///
/// * **`ClassLoader.libraries`.** The real `ClassLoader.loadLibrary` reads the
///   loader's `NativeLibraries`, which the JDK sets in a field initialiser
///   inside every `ClassLoader` constructor. The built-in loaders are minted
///   without one and user loaders go through the `<init>` bridges, so it was
///   null (the wave 4 dial's `RJdkJni` NPE). `classloader::
///   init_loader_native_libraries` now runs the JDK's own
///   `NativeLibraries.newInstance(loader)` for the app/platform loaders and in
///   `init_classloader_common_fields`, `--real-jdk` only. The ACC_NATIVE
///   `NativeLibraries` natives (`load`, `unload`, `findBuiltinLib`,
///   `findEntry0`) were already genuine in both modes.
/// * **`Signal.handle0`/`raise0`** (both modes; only toward HotSpot). `handle0`
///   answered `0` always: a re-registration was told SIG_DFL. It now keeps a
///   per-VM disposition table (seeded from the process's OS disposition, as
///   `JVM_RegisterSignal` answers) and refuses QUIT/SEGV/FPE/ILL with `-1`.
///   `raise0` was a no-op; with a Java handler installed it now calls
///   `Signal.dispatch(number)`, what HotSpot's dispatcher thread calls.
/// * **`sun/misc/Signal`.** Its natives wrote `name`/`number` into a class
///   whose only field is `iSignal` (by-name writes dropped, the slot-0 fallback
///   put a `String` where the `jdk.internal.misc.Signal` belongs). Retired, the
///   real class holds a real `jdk.internal.misc.Signal`, whose own rows wave 4
///   retired.
///
/// # Held (still native)
///
/// * `StackWalker.walk`/`forEach`/`getCallerClass`, AGAIN, for cost. The JDK
///   path is correct (every `SbrW5Fjp2` StackWalker row = HotSpot on the trial
///   `cvm-sbr-fjp2-1`) but 9.6x / 12x / 14x the held natives: the JDK's
///   `StackFrameBuffer.fill` calls the reflective `Constructor.newInstance` once
///   per buffer slot (53 per depth-30 walk) at 3-18 us each here (HotSpot
///   0.2-0.6), plus one `getDeclaredConstructor` per walk (10 us; HotSpot 0.1).
///   The "slower with JIT" was each `newInstance` resolving its caller twice
///   through a whole-stack walk, which costs more per compiled frame than per
///   interpreted one. What unblocks it is the reflection natives, not the
///   `StackStreamFactory` natives.
/// * `Runtime.exit` (the VM's exit path); dead rows (`Signal.number`, the
///   `jdk/internal/misc/Signal.handle` descriptor no image declares).
///
/// Record: `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_FJP2_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/Runtime",
        "load0",
        "(Ljava/lang/Class;Ljava/lang/String;)V",
    ),
    (
        "java/lang/Runtime",
        "loadLibrary0",
        "(Ljava/lang/Class;Ljava/lang/String;)V",
    ),
    (
        "jdk/internal/misc/Signal",
        "raise",
        "(Ljdk/internal/misc/Signal;)V",
    ),
    ("sun/misc/Signal", "<init>", "(Ljava/lang/String;)V"),
    ("sun/misc/Signal", "getName", "()Ljava/lang/String;"),
    ("sun/misc/Signal", "getNumber", "()I"),
    (
        "sun/misc/Signal",
        "handle",
        "(Lsun/misc/Signal;Lsun/misc/SignalHandler;)Lsun/misc/SignalHandler;",
    ),
    ("sun/misc/Signal", "raise", "(Lsun/misc/Signal;)V"),
];

/// SBR wave 5, lane HTTP (2026-09-29): the plain-HTTP `HttpURLConnection`
/// family, `java/net/URLConnection` and the `jrt:` connection. 58 triples, 64
/// registrations (the `bridge` rows of the 25/linux kind map over them):
/// `sun/net/www/protocol/http/HttpURLConnection` 29 (29),
/// `java/net/HttpURLConnection` 15 (21), `java/net/URLConnection` 7 (7),
/// `sun/net/www/protocol/jrt/JavaRuntimeURLConnection` 7 (7). Every row has
/// `Code` on 17, 21 and 25, declared or inherited (`javap -p`); the rows
/// without it stay registered -- `connect`/`disconnect`/`usingProxy` on
/// `java/net/HttpURLConnection` and `connect` on `java/net/URLConnection`
/// (abstract), both `<init>` rows on the `sun` class (it declares neither
/// `(URL)` nor `()`; constructors are not inherited).
///
/// # Why it is safe
///
/// Under `--real-jdk` no native mints these carriers any more: `URL.openConnection`
/// is retired (`RETIRED_SHADOW_SBR_NETADDR_URL_TRIPLES`), so the JDK's own
/// `http`/`jrt` handlers CONSTRUCT the connection, and the natives were serving
/// a real object from side tables its bytecode never reads -- the census never
/// saw it, because the by-name `real_http_url_connection_native` arm
/// (`native_override.rs`) forces them without counting an invocation. Measured
/// on the control (`SbrW5Http`, `--real-jdk`): chunked streaming sent
/// fixed-length, `getInstanceFollowRedirects()` answered `true` after
/// `setInstanceFollowRedirects(false)`, `setRequestProperty` after `connect()`
/// did not throw `IllegalStateException`, and a missing `jrt:` resource threw
/// `FileNotFoundException` where HotSpot throws `IOException`.
///
/// **The arm's fallback is a reader that owned both ends.** With the `sun`
/// rows retired it looks the triple up again on `java/net/HttpURLConnection`,
/// where `connect()` is still registered (abstract there, kept for
/// `--compatible`'s minted carrier): the first trial binary forced the native
/// `connect()` onto the real connection and every request died `NPE: "this.http"
/// is null`. Fixed in the family's own code, not the door:
/// `http_url_connection::subclass_runs_its_own_bytecode` hands a native reached
/// with the JDK's plain-`http` carrier to the receiver's bytecode under
/// `--real-jdk` when that carrier's own row is retired (so
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW` restores the pre-wave VM). The HTTPS
/// carrier (lane `jsse`) is untouched. The door's fallback itself should ask
/// whether the receiver's row was retired before falling back; that is door
/// code, reported rather than edited.
///
/// # HELD: `com.sun.net.httpserver` (19 rows with `Code`: `HttpServer.create` x2,
/// `sun/net/httpserver/HttpServerImpl` 11, `HttpContextImpl` 6) -- it HANGS
///
/// Retired on the trial binaries (`cvm-sbr-http-1/-2`) the REAL server
/// (`ServerImpl` over `ServerSocketChannel` + `Selector`) answers every
/// `SbrW5Http` row as HotSpot does -- where the native server is wrong on six
/// (a duplicate `createContext` silently REPLACES the handler instead of throwing,
/// `HttpContext.getFilters` is `AbstractMethodError`, `getExecutor()` is `null`,
/// chunked responses go out fixed-length, the 404 body and the 204 length differ)
/// -- and the suite is 152/152. But over the VM's current selector natives it
/// HANGS: after some tens of keep-alive requests one request's bytes sit unread
/// in the server socket (`ss`: `ESTAB` server side, `Recv-Q 115`) while the
/// dispatcher is parked in `epoll_wait`, and the client waits forever (the
/// probe hung at iteration 11 of 20 on `-2`, at iteration 0 under the dial). The
/// native server under the same clients runs all 20 iterations, drift 0. What
/// would have to exist: a selector that does not lose a re-registered channel's
/// readiness (the NIO family, lane `nioreal`); then re-run `SbrW5Http 20` with
/// these 19 rows retired. Retire them WITH or after the client below: the
/// native `java.net.http` client against the real server timed out
/// (`HttpClient request timed out`).
///
/// # HELD: the `java.net.http` client (50 rows with `Code`) -- 10-50x slower
///
/// `HttpClient` 7 (`newBuilder`, `newHttpClient`, the five JDK 21+ lifecycle
/// defaults), `HttpHeaders` 5 (a final class), `HttpRequest.newBuilder` x2,
/// `BodyPublishers` 5, `HttpRequest$Builder.HEAD` (21+), `BodyHandlers` 4, and
/// the rows registered on `jdk/internal/net/http` (`HttpClientImpl` 10,
/// `HttpRequestImpl` 7, `HttpResponseImpl` 7, the `Http1HeaderParser` and
/// `Http2ClientImpl` constructors). They go as ONE table or not at all: the
/// native client is an object model (`newHttpClient` mints an unconstructed
/// ABSTRACT `HttpClient`), and the real `HttpClientImpl` it would give way to
/// has registered natives of its own. Retired together (trials `-1`..`-3`,
/// suite 152/152) every `hc.*` row of `SbrW5Http` matches HotSpot, drift 0 over
/// 20 iterations (the native client gets keep-alive reuse and the refused
/// connection's exception class wrong). Held for COST over the current NIO
/// natives the real client drives (a `Selector` and non-blocking
/// `SocketChannel`s), `SbrW5HttpBench`, us/request, 4 interleaved rounds, load
/// 16-20: GET 756-3978 -> 7649-12247, POST 962-5644 -> 47824-52522 (HotSpot
/// 436-1788). `--nojit` costs the same, so it is waiting, not bytecode; the
/// POST's steady ~50 ms is the shape of a Nagle / delayed-ACK stall (one
/// `TCP_NODELAY` `setsockopt` reaches the kernel against HotSpot's two). What
/// would have to exist: the NIO family's socket options and selector wake-ups
/// (lane `nioreal`); then retire the 50 rows (listed in the record) and re-run
/// the bench.
///
/// # Measured (JDK 25.0.4, linux; control `cvm-sbr-int8`, final `cvm-sbr-http-5`)
///
/// ```text
///   suite (152):   int8 152/152; http-5 152/152 (http-1 151: the NPE above)
///   strict corpus: int8 0 divergent; http-5 0
///   SbrW5Http (46 rows, 20 hot iterations) vs HotSpot, drift 0 in every arm:
///     int8 --real-jdk 26 lines -> http-5 22 (every one the held server /
///     client, or `Net.connect`'s refused text); + UNRETIRE = int8 exactly;
///     --compatible = int8 --compatible exactly
///   registry (--real-jdk boot): 7503 -> 7439 registrations (64 re-tags)
///   SbrW5HttpBench us/req, 4 interleaved rounds, load 15-19:
///     huc.get 576-734 -> 1627-1929, huc.post 1279-2140 -> 3993-4358
///     (HotSpot 279-576 / 1413-1664): the JDK's own sun.net.www client as
///     bytecode, user CPU 2x. Kept: the native answered four rows wrong and
///     this is not a tight loop -- a maintainer call if it should be held.
/// ```
///
/// Record: `docs/feature-designs/native-dispatch-architecture.md`.
///
/// `--compatible` is unchanged: the re-tag makes these `SyntheticStub`, which
/// that mode registers and dispatches exactly as it did the `Bridge`, and the
/// one native-code change is behind `is_jdk_only()`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_HTTP_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/net/HttpURLConnection", "getContentLength", "()I"),
    ("java/net/HttpURLConnection", "getContentLengthLong", "()J"),
    (
        "java/net/HttpURLConnection",
        "getErrorStream",
        "()Ljava/io/InputStream;",
    ),
    (
        "java/net/HttpURLConnection",
        "getHeaderField",
        "(I)Ljava/lang/String;",
    ),
    (
        "java/net/HttpURLConnection",
        "getHeaderField",
        "(Ljava/lang/String;)Ljava/lang/String;",
    ),
    (
        "java/net/HttpURLConnection",
        "getHeaderFieldKey",
        "(I)Ljava/lang/String;",
    ),
    (
        "java/net/HttpURLConnection",
        "getHeaderFields",
        "()Ljava/util/Map;",
    ),
    (
        "java/net/HttpURLConnection",
        "getInputStream",
        "()Ljava/io/InputStream;",
    ),
    ("java/net/HttpURLConnection", "getLastModified", "()J"),
    (
        "java/net/HttpURLConnection",
        "getOutputStream",
        "()Ljava/io/OutputStream;",
    ),
    ("java/net/HttpURLConnection", "getResponseCode", "()I"),
    (
        "java/net/HttpURLConnection",
        "getResponseMessage",
        "()Ljava/lang/String;",
    ),
    ("java/net/HttpURLConnection", "setDoInput", "(Z)V"),
    (
        "java/net/HttpURLConnection",
        "setFixedLengthStreamingMode",
        "(J)V",
    ),
    (
        "java/net/HttpURLConnection",
        "setInstanceFollowRedirects",
        "(Z)V",
    ),
    ("java/net/URLConnection", "getContentLength", "()I"),
    ("java/net/URLConnection", "getContentLengthLong", "()J"),
    (
        "java/net/URLConnection",
        "getInputStream",
        "()Ljava/io/InputStream;",
    ),
    ("java/net/URLConnection", "getLastModified", "()J"),
    (
        "java/net/URLConnection",
        "setContentHandlerFactory",
        "(Ljava/net/ContentHandlerFactory;)V",
    ),
    ("java/net/URLConnection", "setDefaultUseCaches", "(Z)V"),
    ("java/net/URLConnection", "setUseCaches", "(Z)V"),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "addRequestProperty",
        "(Ljava/lang/String;Ljava/lang/String;)V",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "connect",
        "()V",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "disconnect",
        "()V",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "getContentLength",
        "()I",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "getContentLengthLong",
        "()J",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "getErrorStream",
        "()Ljava/io/InputStream;",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "getHeaderField",
        "(I)Ljava/lang/String;",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "getHeaderField",
        "(Ljava/lang/String;)Ljava/lang/String;",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "getHeaderFieldKey",
        "(I)Ljava/lang/String;",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "getHeaderFields",
        "()Ljava/util/Map;",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "getInputStream",
        "()Ljava/io/InputStream;",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "getInstanceFollowRedirects",
        "()Z",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "getOutputStream",
        "()Ljava/io/OutputStream;",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "getRequestMethod",
        "()Ljava/lang/String;",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "getRequestProperties",
        "()Ljava/util/Map;",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "getRequestProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "getResponseCode",
        "()I",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "getResponseMessage",
        "()Ljava/lang/String;",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "setChunkedStreamingMode",
        "(I)V",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "setConnectTimeout",
        "(I)V",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "setDoInput",
        "(Z)V",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "setDoOutput",
        "(Z)V",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "setFixedLengthStreamingMode",
        "(I)V",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "setFixedLengthStreamingMode",
        "(J)V",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "setInstanceFollowRedirects",
        "(Z)V",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "setReadTimeout",
        "(I)V",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "setRequestMethod",
        "(Ljava/lang/String;)V",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "setRequestProperty",
        "(Ljava/lang/String;Ljava/lang/String;)V",
    ),
    (
        "sun/net/www/protocol/http/HttpURLConnection",
        "usingProxy",
        "()Z",
    ),
    (
        "sun/net/www/protocol/jrt/JavaRuntimeURLConnection",
        "connect",
        "()V",
    ),
    (
        "sun/net/www/protocol/jrt/JavaRuntimeURLConnection",
        "getContentLength",
        "()I",
    ),
    (
        "sun/net/www/protocol/jrt/JavaRuntimeURLConnection",
        "getContentLengthLong",
        "()J",
    ),
    (
        "sun/net/www/protocol/jrt/JavaRuntimeURLConnection",
        "getInputStream",
        "()Ljava/io/InputStream;",
    ),
    (
        "sun/net/www/protocol/jrt/JavaRuntimeURLConnection",
        "getLastModified",
        "()J",
    ),
    (
        "sun/net/www/protocol/jrt/JavaRuntimeURLConnection",
        "setDefaultUseCaches",
        "(Z)V",
    ),
    (
        "sun/net/www/protocol/jrt/JavaRuntimeURLConnection",
        "setUseCaches",
        "(Z)V",
    ),
];

/// SBR wave 5, lane REFLMETA (2026-09-29): the reflection object model wave 3
/// HELD for state -- annotations, the copy door, and
/// `Reflection.ensureNativeAccess`. 40 triples, 41 registrations
/// (`ensureNativeAccess` is registered twice). Every row has `Code` on JDK
/// 17, 21 and 25 (declared or inherited from `AccessibleObject`/`Executable`,
/// checked by reflection on all three images) except `ensureNativeAccess`,
/// which is declared on 25 only -- on 17/21 the triple is never called.
///
/// # What made the state real
///
/// * **Annotation bytes at the mints.** `create_field_object`,
///   `create_method_object` and `create_constructor_object` write HotSpot's
///   `annotations` / `parameterAnnotations` / `annotationDefault` `byte[]`
///   (the attribute BODY, `null` when absent) under `--real-jdk`
///   (`NativeClassAccess::member_raw_annotation_attributes`), and
///   `getTypeAnnotationBytes0` answers the member's real
///   `RuntimeVisibleTypeAnnotations`. `Class.getRawAnnotations` /
///   `getRawTypeAnnotations` answer real bytes (the class-level type
///   annotations kept by `ClassManager::class_type_annotations`, since the
///   class-bytes cache is FIFO-evicted -- reading it made a type parameter's
///   annotations drift to `[]` after ~50 hot iterations on the first build).
/// * **A working `jdk.internal.reflect.ConstantPool`.** Its ACC_NATIVE
///   `getSize0`/`getUTF8At0`/`getStringAt0`/`getInt|Long|Float|DoubleAt0`/
///   `getTagAt0` were never registered (a real parse ended in
///   `UnsatisfiedLinkError`); they are now `Bridge` natives serving the pool of
///   the class whose mirror `Class.getConstantPool` puts in the real
///   `constantPoolOop` field, as HotSpot does.
/// * **The copy door, real.** `ReflectionFactory.copy*` and
///   `ReflectAccess.copy*` (the half wave 3 did not list) were identities, so
///   `setAccessible` on one `getMethods()` result leaked into the next. Real
///   `copy()` builds `new Method(..)` through `<init>` with exactly the JDK
///   layout; every reader of the CratonVM metadata tail
///   (`read_field_*`, `read_method_*`, `read_constructor_*`,
///   `method_descriptor_for_invoke`, the accessible writers) now falls back to
///   the JDK fields or the `root` mint when the object has no tail.
/// * **`AnnotatedType`s the real factory builds.** The accessor natives on
///   `AnnotatedTypeBaseImpl` / `AnnotatedParameterizedTypeImpl` read a
///   CratonVM-stashed `Annotation[]`; for a real instance (a `Map` in
///   `annotations`, `--real-jdk` only) they now run the JDK body.
/// * **`--enable-native-access` in the real `Module` state.** The grant is
///   applied to `Module.ALL_UNNAMED_MODULE.enableNativeAccess` and to the named
///   modules `ModuleBootstrap.boot` would grant (java.base and the JDK's
///   `nativeAccessModules` list), so the real `Module.ensureNativeAccess`
///   prints HotSpot's four-line restricted-method WARNING without the flag and
///   nothing with it (`RJdkJni`, both arms, matched against HotSpot).
///
/// # Measured (JDK 25.0.4, linux; control `cvm-sbr-int8`, final `cvm-sbr-reflmeta-2`)
///
/// ```text
///   SbrW5Reflmeta (201 rows, 300 hot iterations), rows != HotSpot 25:
///     int8 --real-jdk 30 -> reflmeta-2 --real-jdk 9, drift 0 (--nojit identical)
///     reflmeta-2 + CRATONVM_UNRETIRE_NATIVE_SHADOW=<the 40 rows>: int8's rows
///       except 2 repeatable-annotation lines, which move TO HotSpot (the mint
///       bytes feed the real AnnotationSupport under the natives too)
///     --compatible: identical to int8 --compatible
///   suite SUITE=all --real-jdk: 152/152 (int8 152/152)
///   native-won census: 529 -> 524 triples
/// ```
///
/// # Held, with the measured reason
///
/// * `java/lang/reflect/Proxy` (4) and `InvocationHandler.invokeDefault`
///   (`the_sbr_reflmeta_wave_retires_the_held_object_model` pins them). Trial
///   `cvm-sbr-reflmeta-1` retired them: the real `ProxyBuilder` runs, but its
///   per-loader cache never answers (`proxy.same class twice` false, a new
///   `$ProxyN` per call) and its reverse map never holds the class
///   (`isProxyClass` false, `getInvocationHandler` IAE), and a real
///   `ProxyGenerator` class collided with a VM-generated one of the same name
///   (`NoSuchFieldError: jdk.proxy1.$Proxy0 ... m_0`): RJdkProxy, RJdkStrict,
///   RArrayStoreInterfaces, RAnnotationProxyGate and RJdkReflect failed.
///   Unblocks: the `ClassLoaderValue` side table answering the real
///   `ProxyBuilder`'s `proxyCache`/`reverseProxyCache` sub-keys, and the VM's
///   own `$ProxyN` generation (annotation proxies, `Proxy$Instance`) retired
///   so the two generators never share a namespace.
/// * `ReflectionFactory.getConstantPool`: no bytecode on 17/21/25.
///
/// Record: `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_REFLMETA_TRIPLES: &[(&str, &str, &str)] = &[
    (
        "java/lang/reflect/Constructor",
        "copy",
        "()Ljava/lang/reflect/Constructor;",
    ),
    (
        "java/lang/reflect/Constructor",
        "getAnnotatedParameterTypes",
        "()[Ljava/lang/reflect/AnnotatedType;",
    ),
    (
        "java/lang/reflect/Constructor",
        "getAnnotation",
        "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
    ),
    (
        "java/lang/reflect/Constructor",
        "getAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
    ),
    (
        "java/lang/reflect/Constructor",
        "getDeclaredAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
    ),
    (
        "java/lang/reflect/Constructor",
        "getParameterAnnotations",
        "()[[Ljava/lang/annotation/Annotation;",
    ),
    (
        "java/lang/reflect/Constructor",
        "isAnnotationPresent",
        "(Ljava/lang/Class;)Z",
    ),
    (
        "java/lang/reflect/Field",
        "copy",
        "()Ljava/lang/reflect/Field;",
    ),
    (
        "java/lang/reflect/Field",
        "getAnnotatedType",
        "()Ljava/lang/reflect/AnnotatedType;",
    ),
    (
        "java/lang/reflect/Field",
        "getAnnotation",
        "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
    ),
    (
        "java/lang/reflect/Field",
        "getAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
    ),
    (
        "java/lang/reflect/Field",
        "getDeclaredAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
    ),
    (
        "java/lang/reflect/Field",
        "isAnnotationPresent",
        "(Ljava/lang/Class;)Z",
    ),
    (
        "java/lang/reflect/Method",
        "copy",
        "()Ljava/lang/reflect/Method;",
    ),
    (
        "java/lang/reflect/Method",
        "getAnnotatedParameterTypes",
        "()[Ljava/lang/reflect/AnnotatedType;",
    ),
    (
        "java/lang/reflect/Method",
        "getAnnotation",
        "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
    ),
    (
        "java/lang/reflect/Method",
        "getAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
    ),
    (
        "java/lang/reflect/Method",
        "getAnnotationsByType",
        "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
    ),
    (
        "java/lang/reflect/Method",
        "getDeclaredAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
    ),
    (
        "java/lang/reflect/Method",
        "getDeclaredAnnotationsByType",
        "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
    ),
    (
        "java/lang/reflect/Method",
        "getDefaultValue",
        "()Ljava/lang/Object;",
    ),
    (
        "java/lang/reflect/Method",
        "getParameterAnnotations",
        "()[[Ljava/lang/annotation/Annotation;",
    ),
    (
        "java/lang/reflect/Method",
        "isAnnotationPresent",
        "(Ljava/lang/Class;)Z",
    ),
    (
        "java/lang/reflect/Parameter",
        "getAnnotatedType",
        "()Ljava/lang/reflect/AnnotatedType;",
    ),
    (
        "java/lang/reflect/ReflectAccess",
        "copyConstructor",
        "(Ljava/lang/reflect/Constructor;)Ljava/lang/reflect/Constructor;",
    ),
    (
        "java/lang/reflect/ReflectAccess",
        "copyField",
        "(Ljava/lang/reflect/Field;)Ljava/lang/reflect/Field;",
    ),
    (
        "java/lang/reflect/ReflectAccess",
        "copyMethod",
        "(Ljava/lang/reflect/Method;)Ljava/lang/reflect/Method;",
    ),
    (
        "java/lang/reflect/ReflectAccess",
        "getExecutableTypeAnnotationBytes",
        "(Ljava/lang/reflect/Executable;)[B",
    ),
    (
        "jdk/internal/reflect/Reflection",
        "ensureNativeAccess",
        "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/String;Z)V",
    ),
    (
        "jdk/internal/reflect/ReflectionFactory",
        "copyConstructor",
        "(Ljava/lang/reflect/Constructor;)Ljava/lang/reflect/Constructor;",
    ),
    (
        "jdk/internal/reflect/ReflectionFactory",
        "copyField",
        "(Ljava/lang/reflect/Field;)Ljava/lang/reflect/Field;",
    ),
    (
        "jdk/internal/reflect/ReflectionFactory",
        "copyMethod",
        "(Ljava/lang/reflect/Method;)Ljava/lang/reflect/Method;",
    ),
    (
        "jdk/internal/reflect/ReflectionFactory",
        "getExecutableTypeAnnotationBytes",
        "(Ljava/lang/reflect/Executable;)[B",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/TypeVariableImpl",
        "getAnnotatedBounds",
        "()[Ljava/lang/reflect/AnnotatedType;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/TypeVariableImpl",
        "getAnnotation",
        "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/TypeVariableImpl",
        "getAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/TypeVariableImpl",
        "getAnnotationsByType",
        "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/TypeVariableImpl",
        "getDeclaredAnnotation",
        "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
    ),
    (
        "sun/reflect/generics/reflectiveObjects/TypeVariableImpl",
        "getDeclaredAnnotationsByType",
        "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
    ),
];

/// SBR wave 5, lane CRYPTOENG (2026-09-29): `java/security/MessageDigest`, the
/// one of the three throughput-held JCA engines that is now fast enough real.
/// 17 triples, 17 registrations, every one with `Code` on 17, 21 and 25.
///
/// # What unblocked it
///
/// Retired in wave 2's trial, SHA-256 ran 65x slower: the SUN engine bytecode
/// spent its time in the digest compression. This wave registers reviewed
/// `NativeKind::Intrinsic`s for the methods the JDK marks `@IntrinsicCandidate`
/// (`native-builtins/src/crypto_intrinsics.rs`, kernels in
/// `native-builtins-crypto/src/jdk_intrinsics.rs`): `SHA`/`SHA2`/`SHA5`/`MD5`/
/// `SHA3.implCompress0`, `DigestBase.implCompressMultiBlock0`, plus the
/// `ByteArrayAccess` `i2b*`/`l2b*` byte-view writers (VarHandle byte views cost
/// ~1 us per access in this VM) — and, for the day `Cipher` follows, the AES,
/// AES-mode, GHASH, ChaCha20 and Poly1305 kernels. Each reads and writes the
/// same fields its Java body does. No native mints a `MessageDigest` except the
/// retired `getInstance` (checked: `message_digest.rs` is the only minter), so
/// the class moves alone.
///
/// # Measured (JDK 25.0.4, linux; control `cvm-sbr-int8`)
///
/// ```text
///   SHA-256 1 MiB  int8 4.2-4.7 ms -> 0.68-0.69 (HotSpot 0.51)   [trial cvm-sbr-cryptoeng-1]
///   SHA-384 1 MiB  int8 2.7-2.8  -> 2.1                (HotSpot 1.3-1.5)
///   64-B SHA-256 getInstance+digest: int8 3.7-4.1 us -> ~10 (HotSpot 0.2)
///   handshake (mix with JSSE, SSLEngine / SSLSocket loopback, ms): MessageDigest
///   retired alone = all three native within noise; Cipher+Mac retired too:
///   +30-60% engine, ~2x socket
///   SbrW1Jca probe: int8 14 lines off HotSpot -> 13 (md.class; the 13 left are
///   the held native Cipher), 0 with all three retired (trial); drift 0;
///   --compatible identical to int8
///   suite 152/152 (final and trial); strict: only the [scalar-deopt] exit line
/// ```
///
/// # Held, with the measured reason
///
/// * `javax/crypto/Cipher` (35) and `javax/crypto/Mac` (17 triples, 19
///   registrations). Correct retired (trial `cvm-sbr-cryptoeng-1`: probe 0 lines
///   off HotSpot, suite 152/152) and FAST on bulk data (AES-GCM 1 MiB 1.4 ms,
///   HMAC-SHA256 0.8 ms/MiB), but per-object setup is bytecode: real
///   `Cipher.getInstance` 158-165 us against the native's 3-6 (HotSpot 7-37),
///   `init`+`doFinal(100 B)` 31-37 us against 6; `Mac.getInstance` 33-46 us
///   against 2. TLS pays that per handshake: with JSSE retired the SSLSocket
///   handshake went 85-125 ms -> 170-370 ms. Unblocks: a faster real
///   `GetInstance`/`Provider.getService`/`Constructor.newInstance` path
///   (interpreter/JIT, not this family), then re-run `Hs` and the TLS A/B.
///
/// Record: `docs/feature-designs/native-dispatch-architecture.md`.
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_CRYPTOENG_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/security/MessageDigest", "<clinit>", "()V"),
    (
        "java/security/MessageDigest",
        "clone",
        "()Ljava/lang/Object;",
    ),
    ("java/security/MessageDigest", "digest", "()[B"),
    ("java/security/MessageDigest", "digest", "([B)[B"),
    ("java/security/MessageDigest", "digest", "([BII)I"),
    (
        "java/security/MessageDigest",
        "getAlgorithm",
        "()Ljava/lang/String;",
    ),
    ("java/security/MessageDigest", "getDigestLength", "()I"),
    (
        "java/security/MessageDigest",
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/MessageDigest;",
    ),
    (
        "java/security/MessageDigest",
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/MessageDigest;",
    ),
    (
        "java/security/MessageDigest",
        "getInstance",
        "(Ljava/lang/String;Ljava/security/Provider;)Ljava/security/MessageDigest;",
    ),
    (
        "java/security/MessageDigest",
        "getProvider",
        "()Ljava/security/Provider;",
    ),
    ("java/security/MessageDigest", "reset", "()V"),
    (
        "java/security/MessageDigest",
        "toString",
        "()Ljava/lang/String;",
    ),
    ("java/security/MessageDigest", "update", "(B)V"),
    (
        "java/security/MessageDigest",
        "update",
        "(Ljava/nio/ByteBuffer;)V",
    ),
    ("java/security/MessageDigest", "update", "([B)V"),
    ("java/security/MessageDigest", "update", "([BII)V"),
];

/// SBR wave 5, lane NIOREAL (2026-09-29): the NIO socket / channel / selector /
/// datagram family wave 2 HELD, retired whole once its native layer was made
/// real -- 287 triples: `System$1.blockedOn` (the family's interrupt hook, see
/// its row) and 286 over `SocketChannelImpl`, `ServerSocketChannelImpl`,
/// `DatagramChannelImpl`, `DatagramSocketAdaptor`, `EPollSelectorImpl`,
/// `PollSelectorImpl`, `SelectorImpl`, `SelectionKeyImpl`, the two selector
/// providers, `MembershipKeyImpl` (+ `$Type4`/`$Type6`), the
/// `java/nio/channels` factories and final/concrete methods in front of them
/// (`SocketChannel`, `ServerSocketChannel`, `DatagramChannel`, `Selector`,
/// `SelectionKey`, `SelectableChannel`, `spi/AbstractSelectableChannel`), and
/// `java/net/DatagramSocket` / `MulticastSocket`.
///
/// # Why it is safe now (wave 2's four preconditions, in its order)
///
/// `docs/feature-designs/profile-guided-inlining.md` section 4 named
/// what had to exist first. Each now does (`native-io/src/net_nio_real.rs`):
///
/// 1. **The kernel sees real descriptors.** Java still holds `sun.nio.ch.Net`
///    ids (`net_sockets()`, from `0x4000_0000`) -- the `java.net.Socket` path
///    rides on them -- but `EPoll.ctl` now hands the kernel the OS descriptor
///    behind an id while the epoll `data` word keeps the id, so the real
///    `EPollSelectorImpl.fdToKey` lookup sees what it registered; and
///    `EPoll.wait` is a real `epoll_wait` (it was a stub answering 0).
/// 2. **UDP exists in `Net`.** `socket0(.., stream = false, ..)` makes a real
///    UDP socket (it built a `TcpListener` at `bind0`); `DatagramChannelImpl.
///    send0`/`receive0`/`disconnect0`, `DatagramDispatcher.*`, the multicast
///    membership/interface natives, `Net.pollConnect`, `SocketDispatcher.
///    readv0`/`writev0` and `NativeThread.signal0` are registered (they were
///    `UnsatisfiedLinkError`). Every one is a genuine `ACC_NATIVE` method,
///    registered in BOTH modes with its kind stated.
/// 3. **Construction goes through `<init>`.** No producer is left to fix: the
///    factories that minted the impl classes without their constructors are
///    rows of THIS table, so under `--real-jdk` `SocketChannel.open()` runs the
///    real `SelectorProvider.provider().openSocketChannel()` and the real
///    `SocketChannelImpl.<init>` -- whose fd comes from `Net.socket0`.
/// 4. **The family moves as one.** Wave 2's dial measured that any part retired
///    alone meets the rest still native (`NPE this.readLock` /
///    `ClosedSelectorException`); this table is the whole of it, so no real
///    channel ever meets a native selector or vice versa. The `--compatible`
///    carriers (`chan_fields`, `tcp_registry`, the native selector table) are
///    untouched: every row here still registers and dispatches there as a
///    `SyntheticStub`.
///
/// Every triple has `Code` in the JDK 25 image, declared or inherited
/// (classified against HotSpot 25.0.4 with `ShadowTripleClassifier.java`).
///
/// # Left out, with the reason
///
/// * Rows with NO bytecode to yield to (abstract or not declared on 25 --
///   e.g. `SocketChannel.read(ByteBuffer)`, `SelectionKey.interestOps()`,
///   `Selector.select()`, `SocketChannelImpl.read0`, the `NioSocketImpl`
///   `socket*` rows of the pre-JDK-13 impl): retiring them would turn a
///   native into `AbstractMethodError`. A real receiver declares the method
///   itself, which is what dispatch keys on.
/// * `NativeThread.current`/`signal` and `NativeDispatcher.preClose`: held with
///   wave 2's reason (`current` is `ACC_NATIVE` on JDK 17).
/// * The asynchronous channels (`Unix/AsynchronousServerSocketChannelImpl`,
///   `...SocketChannelImpl`, `AsynchronousChannelGroup`, `EPollPort`) and the
///   async FILE channel: not measured this wave (see the lane record).
///
/// # Windows: the table is EMPTY (2026-10-02, measured)
///
/// Everything above was measured on Linux, where `EPollSelectorProvider` is
/// retired too and the JDK's own constructors build the channels (real `fd`,
/// real locks, real `provider`). On Windows the provider is
/// `WEPollSelectorProvider`, whose `sun.nio.ch.WEPoll` layer this VM does not
/// implement, so `ServerSocketChannel.open()` / `SocketChannel.open()` still
/// answer a channel this VM minted (`alloc_channel_as_impl`: no constructor,
/// `fd` / `localAddress` / `readLock` null, state in the `chan_fields` side
/// table) and the retired `Impl` methods ran real bytecode over it:
/// `Net.bind0` on a null `FileDescriptor`, `getLocalAddress()` null,
/// `this.readLock` null. Eight vectors (`RChannelToString`, `RSocketChannelInterrupt`,
/// `RSslEndpointIdentification`, `RSocketFastIo`, `RMulticastSocketLoopback`,
/// `RJdkHttpServer`, `RJdkNio`, `RJdkNet`) were red in the default `--real-jdk`
/// suite on Windows; with every class of this table un-retired
/// (`CRATONVM_UNRETIRE_NATIVE_SHADOW`) all ten net vectors pass. Windows keeps the
/// natives until a Windows channel layer exists.
#[cfg(not(windows))]
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_NIOREAL_TRIPLES: &[(&str, &str, &str)] = &[
    // The interrupt half of the family: `AbstractInterruptibleChannel.begin()`
    // and `AbstractSelector.begin()` publish their `Interruptible` through
    // `JavaLangAccess.blockedOn` -> `Thread.blockedOn`, which sets
    // `Thread.nioBlocker`. The shadow was a no-op, so a real channel's blocker
    // was never recorded and an interrupt could not reach it; the
    // interruptible waits in `native-io/src/net.rs` read `nioBlocker`.
    (
        "java/lang/System$1",
        "blockedOn",
        "(Lsun/nio/ch/Interruptible;)V",
    ),
    ("java/net/DatagramSocket", "<init>", "()V"),
    ("java/net/DatagramSocket", "<init>", "(I)V"),
    (
        "java/net/DatagramSocket",
        "<init>",
        "(ILjava/net/InetAddress;)V",
    ),
    (
        "java/net/DatagramSocket",
        "<init>",
        "(Ljava/net/SocketAddress;)V",
    ),
    (
        "java/net/DatagramSocket",
        "bind",
        "(Ljava/net/SocketAddress;)V",
    ),
    ("java/net/DatagramSocket", "close", "()V"),
    (
        "java/net/DatagramSocket",
        "connect",
        "(Ljava/net/InetAddress;I)V",
    ),
    (
        "java/net/DatagramSocket",
        "connect",
        "(Ljava/net/SocketAddress;)V",
    ),
    ("java/net/DatagramSocket", "disconnect", "()V"),
    ("java/net/DatagramSocket", "getBroadcast", "()Z"),
    (
        "java/net/DatagramSocket",
        "getInetAddress",
        "()Ljava/net/InetAddress;",
    ),
    (
        "java/net/DatagramSocket",
        "getLocalAddress",
        "()Ljava/net/InetAddress;",
    ),
    ("java/net/DatagramSocket", "getLocalPort", "()I"),
    (
        "java/net/DatagramSocket",
        "getLocalSocketAddress",
        "()Ljava/net/SocketAddress;",
    ),
    (
        "java/net/DatagramSocket",
        "getOption",
        "(Ljava/net/SocketOption;)Ljava/lang/Object;",
    ),
    ("java/net/DatagramSocket", "getPort", "()I"),
    ("java/net/DatagramSocket", "getReceiveBufferSize", "()I"),
    (
        "java/net/DatagramSocket",
        "getRemoteSocketAddress",
        "()Ljava/net/SocketAddress;",
    ),
    ("java/net/DatagramSocket", "getReuseAddress", "()Z"),
    ("java/net/DatagramSocket", "getSendBufferSize", "()I"),
    ("java/net/DatagramSocket", "getSoTimeout", "()I"),
    ("java/net/DatagramSocket", "getTrafficClass", "()I"),
    ("java/net/DatagramSocket", "isBound", "()Z"),
    ("java/net/DatagramSocket", "isClosed", "()Z"),
    ("java/net/DatagramSocket", "isConnected", "()Z"),
    (
        "java/net/DatagramSocket",
        "receive",
        "(Ljava/net/DatagramPacket;)V",
    ),
    (
        "java/net/DatagramSocket",
        "send",
        "(Ljava/net/DatagramPacket;)V",
    ),
    ("java/net/DatagramSocket", "setBroadcast", "(Z)V"),
    (
        "java/net/DatagramSocket",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/net/DatagramSocket;",
    ),
    ("java/net/DatagramSocket", "setReceiveBufferSize", "(I)V"),
    ("java/net/DatagramSocket", "setReuseAddress", "(Z)V"),
    ("java/net/DatagramSocket", "setSendBufferSize", "(I)V"),
    ("java/net/DatagramSocket", "setSoTimeout", "(I)V"),
    ("java/net/DatagramSocket", "setTrafficClass", "(I)V"),
    (
        "java/net/DatagramSocket",
        "supportedOptions",
        "()Ljava/util/Set;",
    ),
    ("java/net/MulticastSocket", "<init>", "()V"),
    ("java/net/MulticastSocket", "<init>", "(I)V"),
    ("java/net/MulticastSocket", "close", "()V"),
    ("java/net/MulticastSocket", "getLoopbackMode", "()Z"),
    (
        "java/net/MulticastSocket",
        "getOption",
        "(Ljava/net/SocketOption;)Ljava/lang/Object;",
    ),
    ("java/net/MulticastSocket", "getSoTimeout", "()I"),
    ("java/net/MulticastSocket", "getTimeToLive", "()I"),
    ("java/net/MulticastSocket", "isClosed", "()Z"),
    (
        "java/net/MulticastSocket",
        "joinGroup",
        "(Ljava/net/InetAddress;)V",
    ),
    (
        "java/net/MulticastSocket",
        "joinGroup",
        "(Ljava/net/SocketAddress;Ljava/net/NetworkInterface;)V",
    ),
    (
        "java/net/MulticastSocket",
        "leaveGroup",
        "(Ljava/net/InetAddress;)V",
    ),
    (
        "java/net/MulticastSocket",
        "leaveGroup",
        "(Ljava/net/SocketAddress;Ljava/net/NetworkInterface;)V",
    ),
    (
        "java/net/MulticastSocket",
        "receive",
        "(Ljava/net/DatagramPacket;)V",
    ),
    (
        "java/net/MulticastSocket",
        "send",
        "(Ljava/net/DatagramPacket;)V",
    ),
    ("java/net/MulticastSocket", "setLoopbackMode", "(Z)V"),
    (
        "java/net/MulticastSocket",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/net/DatagramSocket;",
    ),
    ("java/net/MulticastSocket", "setSoTimeout", "(I)V"),
    ("java/net/MulticastSocket", "setTimeToLive", "(I)V"),
    ("java/nio/channels/DatagramChannel", "close", "()V"),
    (
        "java/nio/channels/DatagramChannel",
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
    ),
    (
        "java/nio/channels/DatagramChannel",
        "implCloseChannel",
        "()V",
    ),
    ("java/nio/channels/DatagramChannel", "isBlocking", "()Z"),
    ("java/nio/channels/DatagramChannel", "isOpen", "()Z"),
    ("java/nio/channels/DatagramChannel", "isRegistered", "()Z"),
    (
        "java/nio/channels/DatagramChannel",
        "keyFor",
        "(Ljava/nio/channels/Selector;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/DatagramChannel",
        "open",
        "()Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "java/nio/channels/DatagramChannel",
        "open",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "java/nio/channels/DatagramChannel",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
    ),
    (
        "java/nio/channels/DatagramChannel",
        "read",
        "([Ljava/nio/ByteBuffer;)J",
    ),
    (
        "java/nio/channels/DatagramChannel",
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/DatagramChannel",
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/DatagramChannel",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/NetworkChannel;",
    ),
    (
        "java/nio/channels/DatagramChannel",
        "write",
        "([Ljava/nio/ByteBuffer;)J",
    ),
    (
        "java/nio/channels/SelectableChannel",
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/SelectionKey",
        "attach",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "java/nio/channels/SelectionKey",
        "attachment",
        "()Ljava/lang/Object;",
    ),
    (
        "java/nio/channels/Selector",
        "open",
        "()Ljava/nio/channels/Selector;",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/NetworkChannel;",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/ServerSocketChannel;",
    ),
    ("java/nio/channels/ServerSocketChannel", "close", "()V"),
    (
        "java/nio/channels/ServerSocketChannel",
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "implCloseChannel",
        "()V",
    ),
    ("java/nio/channels/ServerSocketChannel", "isBlocking", "()Z"),
    ("java/nio/channels/ServerSocketChannel", "isOpen", "()Z"),
    (
        "java/nio/channels/ServerSocketChannel",
        "isRegistered",
        "()Z",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "keyFor",
        "(Ljava/nio/channels/Selector;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "open",
        "()Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "open",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/ServerSocketChannel",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/NetworkChannel;",
    ),
    (
        "java/nio/channels/SocketChannel",
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/NetworkChannel;",
    ),
    ("java/nio/channels/SocketChannel", "close", "()V"),
    (
        "java/nio/channels/SocketChannel",
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
    ),
    ("java/nio/channels/SocketChannel", "implCloseChannel", "()V"),
    ("java/nio/channels/SocketChannel", "isBlocking", "()Z"),
    ("java/nio/channels/SocketChannel", "isOpen", "()Z"),
    ("java/nio/channels/SocketChannel", "isRegistered", "()Z"),
    (
        "java/nio/channels/SocketChannel",
        "keyFor",
        "(Ljava/nio/channels/Selector;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/SocketChannel",
        "open",
        "()Ljava/nio/channels/SocketChannel;",
    ),
    (
        "java/nio/channels/SocketChannel",
        "open",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/SocketChannel;",
    ),
    (
        "java/nio/channels/SocketChannel",
        "open",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/SocketChannel;",
    ),
    (
        "java/nio/channels/SocketChannel",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
    ),
    (
        "java/nio/channels/SocketChannel",
        "read",
        "([Ljava/nio/ByteBuffer;)J",
    ),
    (
        "java/nio/channels/SocketChannel",
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/SocketChannel",
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/SocketChannel",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/NetworkChannel;",
    ),
    (
        "java/nio/channels/SocketChannel",
        "write",
        "([Ljava/nio/ByteBuffer;)J",
    ),
    (
        "java/nio/channels/spi/AbstractSelectableChannel",
        "isRegistered",
        "()Z",
    ),
    (
        "java/nio/channels/spi/AbstractSelectableChannel",
        "keyFor",
        "(Ljava/nio/channels/Selector;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/spi/AbstractSelectableChannel",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
    ),
    (
        "java/nio/channels/spi/AbstractSelectableChannel",
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "java/nio/channels/spi/AbstractSelectableChannel",
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/DatagramChannel;",
    ),
    ("sun/nio/ch/DatagramChannelImpl", "close", "()V"),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "connect",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "disconnect",
        "()Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "getLocalAddress",
        "()Ljava/net/SocketAddress;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "getOption",
        "(Ljava/net/SocketOption;)Ljava/lang/Object;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "getRemoteAddress",
        "()Ljava/net/SocketAddress;",
    ),
    ("sun/nio/ch/DatagramChannelImpl", "implCloseChannel", "()V"),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "implCloseSelectableChannel",
        "()V",
    ),
    ("sun/nio/ch/DatagramChannelImpl", "isBlocking", "()Z"),
    ("sun/nio/ch/DatagramChannelImpl", "isConnected", "()Z"),
    ("sun/nio/ch/DatagramChannelImpl", "isOpen", "()Z"),
    ("sun/nio/ch/DatagramChannelImpl", "isRegistered", "()Z"),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "join",
        "(Ljava/net/InetAddress;Ljava/net/NetworkInterface;)Ljava/nio/channels/MembershipKey;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "keyFor",
        "(Ljava/nio/channels/Selector;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "localAddress",
        "()Ljava/net/InetSocketAddress;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "open",
        "()Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "open",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "read",
        "(Ljava/nio/ByteBuffer;)I",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "read",
        "([Ljava/nio/ByteBuffer;)J",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "read",
        "([Ljava/nio/ByteBuffer;II)J",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "receive",
        "(Ljava/nio/ByteBuffer;)Ljava/net/SocketAddress;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "remoteAddress",
        "()Ljava/net/InetSocketAddress;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "send",
        "(Ljava/nio/ByteBuffer;Ljava/net/SocketAddress;)I",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/NetworkChannel;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "socket",
        "()Ljava/net/DatagramSocket;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "supportedOptions",
        "()Ljava/util/Set;",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "write",
        "(Ljava/nio/ByteBuffer;)I",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "write",
        "([Ljava/nio/ByteBuffer;)J",
    ),
    (
        "sun/nio/ch/DatagramChannelImpl",
        "write",
        "([Ljava/nio/ByteBuffer;II)J",
    ),
    (
        "sun/nio/ch/DatagramSocketAdaptor",
        "bind",
        "(Ljava/net/SocketAddress;)V",
    ),
    ("sun/nio/ch/DatagramSocketAdaptor", "isConnected", "()Z"),
    (
        "sun/nio/ch/DatagramSocketAdaptor",
        "setReceiveBufferSize",
        "(I)V",
    ),
    (
        "sun/nio/ch/DatagramSocketAdaptor",
        "setReuseAddress",
        "(Z)V",
    ),
    (
        "sun/nio/ch/DatagramSocketAdaptor",
        "setSendBufferSize",
        "(I)V",
    ),
    ("sun/nio/ch/DatagramSocketAdaptor", "setSoTimeout", "(I)V"),
    (
        "sun/nio/ch/DatagramSocketAdaptor",
        "setTrafficClass",
        "(I)V",
    ),
    ("sun/nio/ch/EPollSelectorImpl", "close", "()V"),
    ("sun/nio/ch/EPollSelectorImpl", "isOpen", "()Z"),
    ("sun/nio/ch/EPollSelectorImpl", "keys", "()Ljava/util/Set;"),
    (
        "sun/nio/ch/EPollSelectorImpl",
        "lockAndDoSelect",
        "(Ljava/util/function/Consumer;J)I",
    ),
    (
        "sun/nio/ch/EPollSelectorImpl",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
    ),
    ("sun/nio/ch/EPollSelectorImpl", "select", "()I"),
    ("sun/nio/ch/EPollSelectorImpl", "select", "(J)I"),
    ("sun/nio/ch/EPollSelectorImpl", "selectNow", "()I"),
    (
        "sun/nio/ch/EPollSelectorImpl",
        "selectedKeys",
        "()Ljava/util/Set;",
    ),
    (
        "sun/nio/ch/EPollSelectorImpl",
        "wakeup",
        "()Ljava/nio/channels/Selector;",
    ),
    (
        "sun/nio/ch/EPollSelectorProvider",
        "openDatagramChannel",
        "()Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "sun/nio/ch/EPollSelectorProvider",
        "openDatagramChannel",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "sun/nio/ch/EPollSelectorProvider",
        "openSelector",
        "()Ljava/nio/channels/spi/AbstractSelector;",
    ),
    (
        "sun/nio/ch/EPollSelectorProvider",
        "openServerSocketChannel",
        "()Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "sun/nio/ch/EPollSelectorProvider",
        "openServerSocketChannel",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "sun/nio/ch/EPollSelectorProvider",
        "openSocketChannel",
        "()Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/EPollSelectorProvider",
        "openSocketChannel",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/MembershipKeyImpl",
        "block",
        "(Ljava/net/InetAddress;)Ljava/nio/channels/MembershipKey;",
    ),
    ("sun/nio/ch/MembershipKeyImpl", "drop", "()V"),
    ("sun/nio/ch/MembershipKeyImpl", "isValid", "()Z"),
    (
        "sun/nio/ch/MembershipKeyImpl",
        "unblock",
        "(Ljava/net/InetAddress;)Ljava/nio/channels/MembershipKey;",
    ),
    (
        "sun/nio/ch/MembershipKeyImpl$Type4",
        "block",
        "(Ljava/net/InetAddress;)Ljava/nio/channels/MembershipKey;",
    ),
    ("sun/nio/ch/MembershipKeyImpl$Type4", "drop", "()V"),
    ("sun/nio/ch/MembershipKeyImpl$Type4", "isValid", "()Z"),
    (
        "sun/nio/ch/MembershipKeyImpl$Type4",
        "unblock",
        "(Ljava/net/InetAddress;)Ljava/nio/channels/MembershipKey;",
    ),
    (
        "sun/nio/ch/MembershipKeyImpl$Type6",
        "block",
        "(Ljava/net/InetAddress;)Ljava/nio/channels/MembershipKey;",
    ),
    ("sun/nio/ch/MembershipKeyImpl$Type6", "drop", "()V"),
    ("sun/nio/ch/MembershipKeyImpl$Type6", "isValid", "()Z"),
    (
        "sun/nio/ch/MembershipKeyImpl$Type6",
        "unblock",
        "(Ljava/net/InetAddress;)Ljava/nio/channels/MembershipKey;",
    ),
    ("sun/nio/ch/PollSelectorImpl", "close", "()V"),
    ("sun/nio/ch/PollSelectorImpl", "isOpen", "()Z"),
    ("sun/nio/ch/PollSelectorImpl", "keys", "()Ljava/util/Set;"),
    (
        "sun/nio/ch/PollSelectorImpl",
        "lockAndDoSelect",
        "(Ljava/util/function/Consumer;J)I",
    ),
    (
        "sun/nio/ch/PollSelectorImpl",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
    ),
    ("sun/nio/ch/PollSelectorImpl", "select", "()I"),
    ("sun/nio/ch/PollSelectorImpl", "select", "(J)I"),
    ("sun/nio/ch/PollSelectorImpl", "selectNow", "()I"),
    (
        "sun/nio/ch/PollSelectorImpl",
        "selectedKeys",
        "()Ljava/util/Set;",
    ),
    (
        "sun/nio/ch/PollSelectorImpl",
        "wakeup",
        "()Ljava/nio/channels/Selector;",
    ),
    (
        "sun/nio/ch/SelectionKeyImpl",
        "attach",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    ),
    (
        "sun/nio/ch/SelectionKeyImpl",
        "attachment",
        "()Ljava/lang/Object;",
    ),
    ("sun/nio/ch/SelectionKeyImpl", "cancel", "()V"),
    (
        "sun/nio/ch/SelectionKeyImpl",
        "channel",
        "()Ljava/nio/channels/SelectableChannel;",
    ),
    ("sun/nio/ch/SelectionKeyImpl", "interestOps", "()I"),
    (
        "sun/nio/ch/SelectionKeyImpl",
        "interestOps",
        "(I)Ljava/nio/channels/SelectionKey;",
    ),
    ("sun/nio/ch/SelectionKeyImpl", "isValid", "()Z"),
    ("sun/nio/ch/SelectionKeyImpl", "readyOps", "()I"),
    (
        "sun/nio/ch/SelectionKeyImpl",
        "selector",
        "()Ljava/nio/channels/Selector;",
    ),
    ("sun/nio/ch/SelectorImpl", "close", "()V"),
    ("sun/nio/ch/SelectorImpl", "isOpen", "()Z"),
    ("sun/nio/ch/SelectorImpl", "keys", "()Ljava/util/Set;"),
    (
        "sun/nio/ch/SelectorImpl",
        "lockAndDoSelect",
        "(Ljava/util/function/Consumer;J)I",
    ),
    (
        "sun/nio/ch/SelectorImpl",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
    ),
    ("sun/nio/ch/SelectorImpl", "select", "()I"),
    ("sun/nio/ch/SelectorImpl", "select", "(J)I"),
    ("sun/nio/ch/SelectorImpl", "selectNow", "()I"),
    (
        "sun/nio/ch/SelectorImpl",
        "selectedKeys",
        "()Ljava/util/Set;",
    ),
    (
        "sun/nio/ch/SelectorProviderImpl",
        "openDatagramChannel",
        "()Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "sun/nio/ch/SelectorProviderImpl",
        "openDatagramChannel",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/DatagramChannel;",
    ),
    (
        "sun/nio/ch/SelectorProviderImpl",
        "openServerSocketChannel",
        "()Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "sun/nio/ch/SelectorProviderImpl",
        "openServerSocketChannel",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "sun/nio/ch/SelectorProviderImpl",
        "openSocketChannel",
        "()Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/SelectorProviderImpl",
        "openSocketChannel",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "accept",
        "()Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "bind",
        "(Ljava/net/SocketAddress;I)Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "blockingAccept",
        "(J)Ljava/nio/channels/SocketChannel;",
    ),
    ("sun/nio/ch/ServerSocketChannelImpl", "close", "()V"),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "getLocalAddress",
        "()Ljava/net/SocketAddress;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "getOption",
        "(Ljava/net/SocketOption;)Ljava/lang/Object;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "implCloseChannel",
        "()V",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "implCloseSelectableChannel",
        "()V",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "implConfigureBlocking",
        "(Z)V",
    ),
    ("sun/nio/ch/ServerSocketChannelImpl", "isBlocking", "()Z"),
    ("sun/nio/ch/ServerSocketChannelImpl", "isBound", "()Z"),
    ("sun/nio/ch/ServerSocketChannelImpl", "isOpen", "()Z"),
    ("sun/nio/ch/ServerSocketChannelImpl", "isRegistered", "()Z"),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "keyFor",
        "(Ljava/nio/channels/Selector;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "localAddress",
        "()Ljava/net/SocketAddress;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "open",
        "()Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "open",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/ServerSocketChannel;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "socket",
        "()Ljava/net/ServerSocket;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "supportedOptions",
        "()Ljava/util/Set;",
    ),
    (
        "sun/nio/ch/ServerSocketChannelImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "blockingConnect",
        "(Ljava/net/SocketAddress;J)V",
    ),
    ("sun/nio/ch/SocketChannelImpl", "blockingRead", "([BIIJ)I"),
    (
        "sun/nio/ch/SocketChannelImpl",
        "blockingWriteFully",
        "([BII)V",
    ),
    ("sun/nio/ch/SocketChannelImpl", "close", "()V"),
    (
        "sun/nio/ch/SocketChannelImpl",
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "connect",
        "(Ljava/net/SocketAddress;)Z",
    ),
    ("sun/nio/ch/SocketChannelImpl", "finishConnect", "()Z"),
    (
        "sun/nio/ch/SocketChannelImpl",
        "getLocalAddress",
        "()Ljava/net/SocketAddress;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "getOption",
        "(Ljava/net/SocketOption;)Ljava/lang/Object;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "getRemoteAddress",
        "()Ljava/net/SocketAddress;",
    ),
    ("sun/nio/ch/SocketChannelImpl", "implCloseChannel", "()V"),
    (
        "sun/nio/ch/SocketChannelImpl",
        "implCloseSelectableChannel",
        "()V",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "implConfigureBlocking",
        "(Z)V",
    ),
    ("sun/nio/ch/SocketChannelImpl", "isBlocking", "()Z"),
    ("sun/nio/ch/SocketChannelImpl", "isConnected", "()Z"),
    ("sun/nio/ch/SocketChannelImpl", "isConnectionPending", "()Z"),
    ("sun/nio/ch/SocketChannelImpl", "isInputOpen", "()Z"),
    ("sun/nio/ch/SocketChannelImpl", "isOpen", "()Z"),
    ("sun/nio/ch/SocketChannelImpl", "isOutputOpen", "()Z"),
    ("sun/nio/ch/SocketChannelImpl", "isRegistered", "()Z"),
    (
        "sun/nio/ch/SocketChannelImpl",
        "keyFor",
        "(Ljava/nio/channels/Selector;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "localAddress",
        "()Ljava/net/SocketAddress;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "open",
        "()Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "open",
        "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "open",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "read",
        "(Ljava/nio/ByteBuffer;)I",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "read",
        "([Ljava/nio/ByteBuffer;)J",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "read",
        "([Ljava/nio/ByteBuffer;II)J",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "remoteAddress",
        "()Ljava/net/SocketAddress;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "shutdownInput",
        "()Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "shutdownOutput",
        "()Ljava/nio/channels/SocketChannel;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "socket",
        "()Ljava/net/Socket;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "supportedOptions",
        "()Ljava/util/Set;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "toString",
        "()Ljava/lang/String;",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "write",
        "(Ljava/nio/ByteBuffer;)I",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "write",
        "([Ljava/nio/ByteBuffer;)J",
    ),
    (
        "sun/nio/ch/SocketChannelImpl",
        "write",
        "([Ljava/nio/ByteBuffer;II)J",
    ),
];
#[cfg(windows)]
pub(in crate::retired_shadow) static RETIRED_SHADOW_SBR_NIOREAL_TRIPLES: &[(&str, &str, &str)] = &[];
