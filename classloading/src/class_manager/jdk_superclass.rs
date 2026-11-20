#[allow(unused_imports)]
use super::*;

pub(in crate::class_manager) fn jdk_superclass(name: &str) -> &'static str {
    match name {
        // Throwable hierarchy
        "java/lang/Throwable" => "java/lang/Object",

        // Error hierarchy
        "java/lang/Error" => "java/lang/Throwable",
        "java/lang/StackOverflowError"
        | "java/lang/OutOfMemoryError"
        | "java/lang/VirtualMachineError"
        | "java/lang/InternalError"
        | "java/lang/AssertionError" => "java/lang/Error",

        // Exception hierarchy
        "java/lang/Exception" => "java/lang/Throwable",
        // Two checked exceptions that extend `Exception` DIRECTLY on HotSpot
        // and had no arm here AT ALL. That was invisible while every throwable
        // re-declared the three-slot model; once the model moved to
        // inherit-not-redeclare it is what `CRATONVM_DBG_LAYOUT=1` reports:
        //
        // ```text
        //   java/lang/InterruptedException        num_total_fields=9 declared=6 PADDED by 3
        //   java/lang/CloneNotSupportedException  num_total_fields=9 declared=6 PADDED by 3
        // ```
        //
        // A missing arm means a blanket `java/lang/Object` parent, so
        // `jdk_chain_reaches_throwable` answers no, so each re-declares the
        // three and is floored to nine against a real six -- and one padded
        // slot costs the whole class its compact layout. `Thread.sleep` and
        // `Object.clone` are on ordinary paths, so those two pads were paid by
        // ordinary programs.
        "java/lang/InterruptedException" | "java/lang/CloneNotSupportedException" => {
            "java/lang/Exception"
        }
        "java/io/IOException" => "java/lang/Exception",
        "java/io/FileNotFoundException" => "java/io/IOException",
        // Serialization's own IOException subtree. Needed as soon as anything
        // throws one of these by class rather than as a plain `IOException`
        // carrying the name in its message: without the chain a fabricated
        // `NotSerializableException` extends `java.lang.Object`, so
        // `catch (IOException)` — and even `catch (Exception)` — does not
        // match it, and the throw escapes the handler that was written for it.
        // The same argument as the serialization subtree, for the `IOException`
        // subclasses `native-builtins`' `THROWABLE_FAMILY_CLASSES` registers
        // constructors on. Without an edge a fabricated `EOFException` extends
        // `java.lang.Object`, and MEASURED on `--synthetic-jdk` 2026-09-12:
        //
        // ```text
        //   new EOFException("m") instanceof IOException   HotSpot true
        //                                                  CratonVM false
        // ```
        //
        // -- so `catch (IOException)` around a stream read does not match the
        // end-of-file it was written for, and the throw escapes. `readObject`
        // loops are written exactly that way.
        "java/io/EOFException"
        | "java/io/UnsupportedEncodingException"
        | "java/io/InterruptedIOException" => "java/io/IOException",
        // `java.net`'s two, which are `IOException`s on HotSpot and are reached
        // the same way (`URL` / `Socket` code catches `IOException`).
        "java/net/MalformedURLException" | "java/net/UnknownHostException" => "java/io/IOException",
        // NOT an `IOException`: `UncheckedIOException` is the wrapper that
        // exists precisely so it does not have to be caught as one.
        "java/io/UncheckedIOException" => "java/lang/RuntimeException",
        "java/io/ObjectStreamException" => "java/io/IOException",
        "java/io/NotSerializableException"
        | "java/io/InvalidClassException"
        | "java/io/InvalidObjectException"
        | "java/io/StreamCorruptedException"
        | "java/io/OptionalDataException"
        | "java/io/WriteAbortedException" => "java/io/ObjectStreamException",

        // RuntimeException hierarchy
        "java/lang/RuntimeException" => "java/lang/Exception",
        "java/lang/NullPointerException"
        | "java/lang/ArithmeticException"
        | "java/lang/ArrayIndexOutOfBoundsException"
        | "java/lang/IndexOutOfBoundsException"
        | "java/lang/StringIndexOutOfBoundsException"
        | "java/lang/ClassCastException"
        | "java/lang/IllegalArgumentException"
        | "java/lang/IllegalStateException"
        | "java/lang/UnsupportedOperationException"
        | "java/lang/ClassNotFoundException"
        | "java/lang/NoSuchMethodException"
        | "java/lang/NoSuchFieldException"
        | "java/lang/NegativeArraySizeException"
        | "java/lang/ArrayStoreException"
        | "java/lang/NumberFormatException"
        | "java/lang/IllegalAccessException"
        | "java/lang/IllegalMonitorStateException"
        | "java/lang/UnsatisfiedLinkError"
        | "java/lang/SecurityException"
        | "java/lang/MatchException" => "java/lang/RuntimeException",

        // java.util exceptions
        "java/util/NoSuchElementException"
        | "java/util/ConcurrentModificationException"
        // `EmptyStackException` extends `RuntimeException` DIRECTLY, not
        // `NoSuchElementException` — see types/src/error.rs's
        // `RuntimeError::EmptyStackException`, whose doc says the same thing for
        // the same reason: a `catch (NoSuchElementException)` must NOT catch it.
        // docs/feature-designs/class-resolution-and-caching.md
        | "java/util/EmptyStackException"
        | "java/util/InputMismatchException" => "java/lang/RuntimeException",

        // Linkage errors
        "java/lang/LinkageError" => "java/lang/Error",
        "java/lang/NoClassDefFoundError"
        | "java/lang/IncompatibleClassChangeError"
        | "java/lang/ClassFormatError"
        | "java/lang/VerifyError"
        | "java/lang/NoSuchFieldError"
        | "java/lang/NoSuchMethodError"
        | "java/lang/IllegalAccessError"
        | "java/lang/AbstractMethodError"
        | "java/lang/ExceptionInInitializerError"
        | "java/lang/BootstrapMethodError" => "java/lang/LinkageError",
        // `InstantiationError` is an `IncompatibleClassChangeError`, not a
        // direct `LinkageError` -- it is the error a `new` of an abstract class
        // raises, which is precisely an incompatible class change. It had no
        // arm at all and was `PADDED by 3` for the same reason as the two
        // checked exceptions above; the extra level is free, because the
        // three slots are inherited either way and only the chain differs.
        "java/lang/InstantiationError" => "java/lang/IncompatibleClassChangeError",

        // java.lang.reflect (JDK hierarchy for reflective wrappers)
        //
        // `ReflectiveOperationException` lives in `java.lang`, NOT
        // `java.lang.reflect` -- only `InvocationTargetException` below is in
        // the subpackage. The `reflect/` spelling was the only one here, so
        // the real `java/lang/ReflectiveOperationException` had no edge at
        // all: it took the throwable arm's three own fields and was floored to
        // nine against a real six (`CRATONVM_DBG_LAYOUT=1`: `PADDED by 3`).
        // `InvocationTargetException`'s arm points at the misspelled name, so
        // its fabricated chain stopped at `java/lang/Object` rather than
        // reaching `Throwable`.
        //
        // Its siblings were never affected by this: `ClassNotFoundException`,
        // `NoSuchMethodException` and `NoSuchFieldException` are edged to
        // `RuntimeException` above, so their chains always reached
        // `Throwable`, and `InterruptedException`, `CloneNotSupportedException`
        // and `InstantiationError` had no arm of their own -- each is fixed
        // where it belongs, not here.
        //
        // The misspelling is kept beside the real name: a name nothing
        // resolves costs nothing, `InvocationTargetException` below still
        // spells it that way, and removing it is a separate question from
        // fixing the real one.
        "java/lang/ReflectiveOperationException"
        | "java/lang/reflect/ReflectiveOperationException" => "java/lang/Exception",
        // `InstantiationException` -- the CHECKED one, raised by
        // `Class.newInstance` -- is a `ReflectiveOperationException` on
        // HotSpot. `native-builtins` registers constructors for it (the
        // message-only ctor family), so this VM can hand one out, and without
        // an arm it had the same no-chain pad as the three above.
        "java/lang/InstantiationException" => "java/lang/ReflectiveOperationException",
        "java/lang/reflect/InvocationTargetException" => {
            "java/lang/reflect/ReflectiveOperationException"
        }

        // java.security exception hierarchy. These classes appear in
        // exception tables / `athrow` sites of `jrt:`-resident classes
        // (e.g. `SecureRandom.getDefaultPRNG`) and the verifier must be
        // able to prove they are assignable to `Throwable` even before
        // the concrete class file has been loaded.
        "java/security/GeneralSecurityException" => "java/lang/Exception",
        "java/security/NoSuchAlgorithmException"
        | "java/security/NoSuchProviderException"
        | "java/security/KeyException"
        | "java/security/KeyStoreException"
        | "java/security/DigestException"
        | "java/security/SignatureException"
        | "java/security/InvalidAlgorithmParameterException"
        | "java/security/UnrecoverableEntryException"
        | "java/security/spec/InvalidKeySpecException"
        | "java/security/cert/CertificateException" => "java/security/GeneralSecurityException",
        // `UnrecoverableKeyException extends UnrecoverableEntryException`, not
        // `GeneralSecurityException` directly — measured on Temurin 25.0.3+9,
        // `probes/JcaExceptionTypeProbe.java` section H. Listing it one level
        // too high still reached `GeneralSecurityException` transitively, so
        // "is it a Throwable" stayed right and only the one question in between
        // — does `catch (UnrecoverableEntryException)` match — came out wrong.
        "java/security/UnrecoverableKeyException" => "java/security/UnrecoverableEntryException",
        "java/security/InvalidKeyException" => "java/security/KeyException",
        // `InvalidKeySpecException` lives in `java.security.spec`, so the
        // `java/security/InvalidKeySpecException` key this arm used to carry
        // named a class that does not exist and could never be looked up, while
        // the real name was absent from the table entirely. Its superclass is
        // `GeneralSecurityException`, not `KeyException` — measured.
        //
        // THE `javax.crypto` HALF OF THE SAME HIERARCHY was absent entirely, so
        // this fallback answered "not a subclass" for `BadPaddingException` /
        // `IllegalBlockSizeException` / `ShortBufferException` against
        // `GeneralSecurityException`, and `static_common_superclass_lookup`
        // widened any two of them to `Object`. Every one is a class this VM now
        // RAISES from the RSA, AES-GCM and key-wrap paths, so a classfile
        // catching them is exactly the caller W7-71 exists to unblock.
        //
        // `AEADBadTagException` is deliberately NOT flattened to
        // `GeneralSecurityException`: it extends `BadPaddingException`, and
        // that link is what makes `catch (BadPaddingException)` catch a GCM tag
        // failure. Collapsing it would repeat the `UnrecoverableKeyException`
        // mistake above on the one class where the intermediate step is the
        // whole point.
        "javax/crypto/BadPaddingException"
        | "javax/crypto/IllegalBlockSizeException"
        | "javax/crypto/NoSuchPaddingException"
        | "javax/crypto/ShortBufferException"
        | "javax/crypto/ExemptionMechanismException" => "java/security/GeneralSecurityException",
        "javax/crypto/AEADBadTagException" => "javax/crypto/BadPaddingException",
        "java/security/AccessControlException" | "java/security/ProviderException" => {
            "java/lang/RuntimeException"
        }
        "java/security/PrivilegedActionException" => "java/lang/Exception",

        // java.nio.charset exception hierarchy.
        "java/nio/charset/CharacterCodingException" => "java/io/IOException",
        "java/nio/charset/MalformedInputException"
        | "java/nio/charset/UnmappableCharacterException" => {
            "java/nio/charset/CharacterCodingException"
        }
        "java/nio/charset/IllegalCharsetNameException"
        | "java/nio/charset/UnsupportedCharsetException" => "java/lang/IllegalArgumentException",

        // java.nio.file.attribute — enum PosixFilePermission extends Enum
        "java/nio/file/attribute/PosixFilePermission" => "java/lang/Enum",

        // The three enums added 2026-09-02 with `enum_constant_fields`. This
        // row is not decoration: `Enum.name()`/`ordinal()`/`compareTo()` are
        // registered on `java/lang/Enum`, so a stub without it publishes its
        // constants correctly and then answers `NoSuchMethodError` the moment
        // anyone calls `name()` on one — measured for both `HttpClient` enums
        // before this row existed, with `MONDAY` working beside them.
        //
        // It is the same row `posix_publish_constants`' doc calls out as the
        // thing that made an INVERTED name/ordinal pair merely latent rather
        // than fatal: "one missing superclass row, or one enum copied from this
        // model without that row, and every constant is NAMELESS".
        //
        // `TimeUnit` is here for the same reason though it long predates them:
        // it had a full field table, a `<clinit>` and working `name()`/
        // `ordinal()`/`values()`, and no superclass row — so
        // `class_is_declared_enum` (which requires ACC_ENUM *and* a
        // `java/lang/Enum` parent) said false, `Class.getEnumConstants`
        // returned null, and `EnumSet.allOf(TimeUnit.class)` answered 0 against
        // HotSpot's 7 while every direct use of the enum was correct.
        "java/time/DayOfWeek"
        | "java/net/http/HttpClient$Version"
        | "java/net/http/HttpClient$Redirect"
        | "java/util/concurrent/TimeUnit" => "java/lang/Enum",

        // E36-1 N1 / E40-1 N3 follow-up (2026-09-20): `StackWalker$Option`
        // and `System$Logger$Level` were missing from this table too, for the
        // exact reason the comment above names — `Enum.name()`/`ordinal()`/
        // `compareTo()` are registered on `java/lang/Enum`, so a stub without
        // this row publishes its constants and then answers
        // `NoSuchMethodError` the moment anyone calls `name()` on one.
        // `System$Logger$Level` additionally NEEDS the row for its instance
        // FIELD layout to be right, not just its methods: `severity` is its
        // one own field, and without the row it is laid out as though the
        // class had no `java.lang.Enum` superclass at all (0 inherited
        // fields) rather than after the 2 real ones — which collided
        // `severity` onto the same slot `system_logger_level_clinit`'s
        // positional fallback uses for `name` (slot 0) the moment the by-name
        // write for `severity` succeeded and the by-name write for `name`
        // did not.
        "java/lang/StackWalker$Option" | "java/lang/System$Logger$Level" => "java/lang/Enum",

        // Number type hierarchy
        "java/lang/Integer" | "java/lang/Long" | "java/lang/Short" | "java/lang/Byte"
        | "java/lang/Float" | "java/lang/Double" => "java/lang/Number",
        "java/lang/Number" => "java/lang/Object",

        // JDBC legacy date/time wrappers extend java.util.Date.
        "java/sql/Date" | "java/sql/Time" | "java/sql/Timestamp" => "java/util/Date",

        // Record hierarchy (JEP 395, Java 16+)
        "java/lang/Record" => "java/lang/Object",

        // ---- java.io hierarchy ----
        // Abstract base classes extend Object
        "java/io/InputStream" | "java/io/OutputStream" | "java/io/Reader" | "java/io/Writer" => {
            "java/lang/Object"
        }

        // Filter streams wrap another stream
        "java/io/FilterInputStream" => "java/io/InputStream",
        "java/io/BufferedInputStream" | "java/io/DataInputStream" => "java/io/FilterInputStream",
        "java/io/FilterOutputStream" => "java/io/OutputStream",
        "java/io/BufferedOutputStream" | "java/io/DataOutputStream" => "java/io/FilterOutputStream",

        // File streams extend base streams directly
        "java/io/FileInputStream"
        | "java/io/ByteArrayInputStream"
        | "java/io/ObjectInputStream"
        | "cratonvm/synthetic/ProcessPipeInputStream" => "java/io/InputStream",
        "java/io/FileOutputStream"
        | "java/io/ByteArrayOutputStream"
        | "java/io/ObjectOutputStream"
        | "cratonvm/synthetic/ProcessPipeOutputStream" => "java/io/OutputStream",

        // PrintStream extends FilterOutputStream
        "java/io/PrintStream" => "java/io/FilterOutputStream",

        // Reader/Writer subclasses
        "java/io/BufferedReader" | "java/io/InputStreamReader" | "java/io/StringReader" => {
            "java/io/Reader"
        }
        "java/io/BufferedWriter"
        | "java/io/OutputStreamWriter"
        | "java/io/PrintWriter"
        | "java/io/StringWriter" => "java/io/Writer",

        // FileReader/FileWriter extend stream reader/writer
        "java/io/FileReader" => "java/io/InputStreamReader",
        "java/io/FileWriter" => "java/io/OutputStreamWriter",

        // ---- javax.naming hierarchy ----
        "javax/naming/NamingException" => "java/lang/Exception",
        "javax/naming/NameNotFoundException" | "javax/naming/InvalidNameException" => {
            "javax/naming/NamingException"
        }

        // ---- java.nio hierarchy ----
        "java/nio/Buffer" => "java/lang/Object",
        "java/nio/ByteBuffer"
        | "java/nio/CharBuffer"
        | "java/nio/ShortBuffer"
        | "java/nio/IntBuffer"
        | "java/nio/LongBuffer"
        | "java/nio/FloatBuffer"
        | "java/nio/DoubleBuffer" => "java/nio/Buffer",
        "java/nio/HeapByteBuffer" => "java/nio/ByteBuffer",
        "java/nio/HeapCharBuffer" => "java/nio/CharBuffer",
        "java/nio/charset/Charset"
        | "java/nio/charset/CharsetDecoder"
        | "java/nio/charset/CharsetEncoder"
        | "java/nio/charset/CodingErrorAction" => "java/lang/Object",

        // ---- T19.H5: AtomicReferenceFieldUpdater / AtomicIntegerFieldUpdater
        // / AtomicLongFieldUpdater synthetic impl subclasses. Real Java
        // bytecode that calls `newUpdater(...)` then implicitly casts the
        // result to the abstract base — the cast only succeeds if the
        // returned object's class chain reaches the abstract base. So we
        // declare each `$RustJvmImpl` as a direct subclass of the
        // corresponding factory.
        "java/util/concurrent/atomic/AtomicReferenceFieldUpdater$RustJvmImpl" => {
            "java/util/concurrent/atomic/AtomicReferenceFieldUpdater"
        }
        "java/util/concurrent/atomic/AtomicIntegerFieldUpdater$RustJvmImpl" => {
            "java/util/concurrent/atomic/AtomicIntegerFieldUpdater"
        }
        "java/util/concurrent/atomic/AtomicLongFieldUpdater$RustJvmImpl" => {
            "java/util/concurrent/atomic/AtomicLongFieldUpdater"
        }

        // ---- S-trinity #1: jdk.internal.loader ClassLoader chain.
        // Without these the synthetic-stub for `ClassLoaders$AppClassLoader`
        // / `ClassLoaders$PlatformClassLoader` defaults to `java/lang/Object`
        // as superclass (and worse, the `$` in the name flips the
        // access-flags heuristic at `class_manager.rs:2803` to mark them
        // as interfaces). Both effects break
        // `(ClassLoader) priv.run()` checkcasts in callers like
        // `LoaderUtil.getClassLoader` because our app-loader instances
        // (allocated by `alloc_classloader` with class
        // `jdk/internal/loader/ClassLoaders$AppClassLoader`) end up not
        // being recognised as a `ClassLoader`.
        "jdk/internal/loader/ClassLoaders$AppClassLoader"
        | "jdk/internal/loader/ClassLoaders$PlatformClassLoader" => {
            "jdk/internal/loader/BuiltinClassLoader"
        }
        "jdk/internal/loader/BuiltinClassLoader" => "java/security/SecureClassLoader",
        "java/security/SecureClassLoader" => "java/lang/ClassLoader",
        "java/net/URLClassLoader" => "java/security/SecureClassLoader",
        "java/lang/ClassLoader" => "java/lang/Object",

        // ---- java.net connection hierarchy ----
        "java/net/URLConnection" => "java/lang/Object",
        "java/net/HttpURLConnection" => "java/net/URLConnection",
        "java/net/JarURLConnection" => "java/net/URLConnection",
        "javax/net/ssl/HttpsURLConnection" => "java/net/HttpURLConnection",
        "sun/net/www/protocol/http/HttpURLConnection" => "java/net/HttpURLConnection",
        "sun/net/www/protocol/https/HttpsURLConnectionImpl" => "javax/net/ssl/HttpsURLConnection",

        // ---- LETSGO_S1: java.util collections compatibility layer ----
        //
        // Real JDK declares an `Abstract*` skeletal hierarchy under every
        // concrete collection. Without this chain, synthetic-stub
        // dispatch resolves `LinkedHashSet.add(Object)Z` as
        // `LinkedHashSet.<class chain only Object>.add` and surfaces a
        // `NoSuchMethodError` (failure mode observed during letsgo-main
        // boot). Wiring the chain lets `find_method_recursive` and the
        // native-dispatch fallback walk parents until they locate the
        // method/native registered on the closest concrete ancestor
        // (e.g. `HashSet`).
        //
        // ONLY edges where the parent contributes zero (or matching)
        // synthetic fields are listed here so we don't perturb existing
        // field-slot layouts that natives depend on. In particular,
        // `Properties` and `Stack` keep their direct `Object` parent because
        // their `synthetic_stub_fields` already count fields the candidate
        // parent would also declare.
        //
        // `LinkedHashMap` used to be in that list, and is no longer, because
        // the condition it names stopped holding: its arm declared five OWN
        // fields, three of which were `HashMap`'s, so naming `HashMap` as its
        // parent would have counted those three twice. Its arm now declares
        // the two it actually owns (`head`, `tail`), so the edge is honest and
        // the chain reaches the same five slots -- `LHM_FIELD_HEAD = 3` and
        // `LHM_FIELD_TAIL = 4` land exactly where they always did, after
        // `HashMap`'s three. Naming the edge is what lets
        // `synthetic_stub_total_field_count` see those five.
        //
        // Abstract bases (no synthetic fields):
        "java/util/AbstractCollection" => "java/lang/Object",
        "java/util/AbstractList" => "java/util/AbstractCollection",
        "java/util/AbstractSet" => "java/util/AbstractCollection",
        "java/util/AbstractMap" => "java/lang/Object",
        "java/util/AbstractQueue" => "java/util/AbstractCollection",
        "java/util/AbstractSequentialList" => "java/util/AbstractList",
        "java/util/Dictionary" => "java/lang/Object",

        // Concrete Set hierarchy:
        "java/util/HashSet" => "java/util/AbstractSet",
        "java/util/LinkedHashSet" => "java/util/HashSet",
        "java/util/TreeSet" => "java/util/AbstractSet",
        "java/util/EnumSet" => "java/util/AbstractSet",
        "java/util/concurrent/CopyOnWriteArraySet" => "java/util/AbstractSet",
        // `ConcurrentHashMap.newKeySet()` / `keySet(V)`. The real JDK parent is
        // the package-private `CollectionView`, which contributes the same one
        // field (`map`) the stub layout below already declares by name; naming
        // `AbstractSet` here instead gives the synthetic stub the `Set` dispatch
        // chain without a second stub class whose only job is to hold `map`.
        "java/util/concurrent/ConcurrentHashMap$KeySetView" => "java/util/AbstractSet",
        "java/util/concurrent/ConcurrentSkipListSet" => "java/util/AbstractSet",

        // The carrier classes a map's `values()`/`entrySet()` view is minted
        // under (native-collections' `MAP_VIEW_CARRIERS`). In the real JDK
        // every one of them extends `AbstractCollection`; with no class file
        // the default `java/lang/Object` arm below would leave the view
        // outside the Collection dispatch chain entirely.
        "java/util/HashMap$Values"
        | "java/util/LinkedHashMap$LinkedValues"
        | "java/util/TreeMap$Values"
        | "java/util/TreeMap$EntrySet"
        | "java/util/Hashtable$ValueCollection"
        | "java/util/concurrent/ConcurrentHashMap$ValuesView" => "java/util/AbstractCollection",

        // The SET-shaped half of the same family (native-collections'
        // `SET_VIEW_CARRIERS`, plus `TreeMap$KeySet`). Every one of them extends
        // `AbstractSet` in the real JDK; without the arm the default below would
        // leave `hashMap.keySet()` outside the Set dispatch chain.
        "java/util/HashMap$KeySet"
        | "java/util/HashMap$EntrySet"
        | "java/util/LinkedHashMap$LinkedKeySet"
        | "java/util/LinkedHashMap$LinkedEntrySet"
        | "java/util/Hashtable$KeySet"
        | "java/util/Hashtable$EntrySet"
        | "java/util/TreeMap$KeySet"
        | "java/util/concurrent/ConcurrentHashMap$EntrySetView" => "java/util/AbstractSet",

        // Concrete List/Queue hierarchy:
        "java/util/ArrayList" => "java/util/AbstractList",
        "java/util/LinkedList" => "java/util/AbstractSequentialList",
        "java/util/Vector" => "java/util/AbstractList",
        "java/util/ArrayDeque" => "java/util/AbstractCollection",
        "java/util/PriorityQueue" => "java/util/AbstractQueue",
        "java/util/concurrent/CopyOnWriteArrayList" => "java/util/AbstractList",
        "java/util/concurrent/ConcurrentLinkedQueue" => "java/util/AbstractQueue",
        "java/util/concurrent/ConcurrentLinkedDeque" => "java/util/AbstractCollection",
        "java/util/concurrent/LinkedBlockingDeque" => "java/util/AbstractQueue",

        // Concrete Map hierarchy:
        "java/util/HashMap" => "java/util/AbstractMap",
        "java/util/LinkedHashMap" => "java/util/HashMap",
        "java/util/TreeMap" => "java/util/AbstractMap",
        "java/util/IdentityHashMap" => "java/util/AbstractMap",
        "java/util/WeakHashMap" => "java/util/AbstractMap",
        "java/util/EnumMap" => "java/util/AbstractMap",
        "java/util/concurrent/ConcurrentHashMap" => "java/util/AbstractMap",
        "java/util/concurrent/ConcurrentSkipListMap" => "java/util/AbstractMap",
        "java/util/Hashtable" => "java/util/Dictionary",
        "java/util/Collections$SynchronizedObject" => "java/lang/Object",
        "java/util/Collections$SynchronizedCollection" => {
            "java/util/Collections$SynchronizedObject"
        }
        "java/util/Collections$SynchronizedSet" => "java/util/Collections$SynchronizedCollection",
        "java/util/Collections$SynchronizedMap" => "java/util/Collections$SynchronizedObject",
        "java/util/Collections$SingletonList" | "java/util/Collections$EmptyList" => {
            "java/util/AbstractList"
        }
        "java/util/Collections$SingletonSet" | "java/util/Collections$EmptySet" => {
            "java/util/AbstractSet"
        }
        "java/util/Collections$SingletonMap" | "java/util/Collections$EmptyMap" => {
            "java/util/AbstractMap"
        }

        // Default: everything else extends Object
        _ => "java/lang/Object",
    }
}

/// Return the interfaces implemented by well-known JDK stub classes.
///
/// This is critical for bytecode verification: without correct interface
/// declarations, the verifier rejects `String` where `CharSequence` is expected.
pub(in crate::class_manager) fn jdk_interfaces(name: &str) -> &'static [&'static str] {
    match name {
        // The process `ProcessBuilder.start()` hands back in compatible mode
        // is `cratonvm/synthetic/Process` (native-io's `spawn_and_wrap`), and
        // nothing linked it to `java/lang/Process`: `instanceof Process` was
        // false and every `checkcast java/lang/Process` raised
        // ClassCastException. javac emits that checkcast wherever the static
        // type is erased — `Process p = p.onExit().get()` is the canonical
        // case, so `Process.onExit()` stayed unusable even once the native
        // behind it existed.
        //
        // Recorded as a supertype HERE rather than as the `superclass` in
        // `fabricate_class` (where SSLSocketInputStream/OutputStream solve the
        // same checkcast problem that way) because `java.lang.Process` is not
        // field-less: it declares six instance fields (the
        // `inputReader`/`outputWriter`/… caches, JDK 17+). A superclass link
        // would put those at the very slot indices the synthetic Process
        // already uses for its exit code and pipe fds, so any real
        // `java.lang.Process` bytecode reaching this receiver would read an
        // `int` fd as an object reference. Listing it here keeps
        // checkcast/instanceof honest — what this table is for — without
        // putting an aliased field layout into the dispatch chain.
        "cratonvm/synthetic/Process" => &["java/lang/Process"],
        // The platform MXBean *extension* interfaces. `ManagementFactory
        // .getThreadMXBean()` / `.getOperatingSystemMXBean()` fabricate their
        // beans under these names so that `instanceof com.sun.management.…`
        // answers true, the way it does on every real JVM (see
        // `native-builtins/src/jmx.rs::alloc_extension_mxbean`). In real-JDK
        // mode the extends-relation comes from the loaded class file; in
        // synthetic-JDK mode there is no class file, so without these entries
        // the fabricated stub would satisfy the extension `instanceof` and
        // FAIL the base one — trading one broken type test for another, in the
        // direction that breaks existing callers.
        "com/sun/management/ThreadMXBean" => &["java/lang/management/ThreadMXBean"],
        "com/sun/management/OperatingSystemMXBean" => {
            &["java/lang/management/OperatingSystemMXBean"]
        }
        "java/lang/String" => &[
            "java/io/Serializable",
            "java/lang/Comparable",
            "java/lang/CharSequence",
            "java/lang/constant/Constable",
        ],
        "java/lang/StringBuilder" | "java/lang/StringBuffer" => &[
            "java/io/Serializable",
            "java/lang/Comparable",
            "java/lang/CharSequence",
        ],
        "java/lang/Integer" | "java/lang/Long" | "java/lang/Short" | "java/lang/Byte"
        | "java/lang/Float" | "java/lang/Double" => {
            &["java/io/Serializable", "java/lang/Comparable"]
        }
        "java/lang/Boolean" | "java/lang/Character" => {
            &["java/io/Serializable", "java/lang/Comparable"]
        }
        // The RANDOM-ACCESS lists. `ArrayList`, `Vector` and
        // `CopyOnWriteArrayList` all declare `implements RandomAccess` on
        // HotSpot; `LinkedList` deliberately does not, which is why it is no
        // longer grouped with them.
        //
        // `RandomAccess` is a marker with no methods, and grouping the four
        // cost the three that have it exactly that marker — measured
        // 2026-09-02 in `--synthetic-jdk`:
        //
        //     ArrayList instanceof RandomAccess   HotSpot true   CratonVM false
        //     Collections.unmodifiableList(list)  HotSpot $UnmodifiableRandomAccessList
        //                                         CratonVM $UnmodifiableList
        //
        // The `Collections` code was right in both: `unmodifiableList` picks
        // its view class BY that marker. And the marker is not decoration —
        // `Collections.binarySearch`, `reverse`, `shuffle` and `fill` each
        // branch on it to choose indexed access over an iterator, so an
        // `ArrayList` without it silently took the linked-list path in every
        // one. `W7-63-jca-advertise-vs-serve.md` §8 records that
        // `--synthetic-jdk` "has never been built by any lane"; it builds, and
        // this is what the first run of it found.
        "java/util/ArrayList"
        | "java/util/Vector"
        | "java/util/concurrent/CopyOnWriteArrayList" => &[
            "java/util/List",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/util/RandomAccess",
            "java/lang/Cloneable",
            "java/io/Serializable",
        ],
        // `LinkedList` — no `RandomAccess`, matching HotSpot.
        //
        // It is also a `Deque` there (and so a `Queue`), and that is NOT added
        // here on purpose: this VM's `LinkedList` carries most of the deque
        // surface (`addFirst`/`addLast`/`pollFirst`/`pollLast`/`removeFirst`/
        // `removeLast`/`descendingIterator`) and not all of it, so declaring
        // the interface would turn a clean `ClassCastException` into a missing
        // method at the point of use — a worse divergence than the one it
        // closes. Left as a named gap rather than a half-kept promise.
        "java/util/LinkedList" => &[
            "java/util/List",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/lang/Cloneable",
            "java/io/Serializable",
        ],
        // A map view is a `Collection`, and deliberately NOT a `List` — that
        // divergence (`hashMap.values() instanceof List` answering true) is
        // half of what giving these views their own carrier class fixes. The
        // `EntrySet` carrier is a `Set` for the same reason its JDK twin is.
        "java/util/HashMap$Values"
        | "java/util/LinkedHashMap$LinkedValues"
        | "java/util/TreeMap$Values"
        | "java/util/Hashtable$ValueCollection"
        | "java/util/concurrent/ConcurrentHashMap$ValuesView" => {
            &["java/util/Collection", "java/lang/Iterable"]
        }
        "java/util/TreeMap$EntrySet"
        | "java/util/HashMap$KeySet"
        | "java/util/HashMap$EntrySet"
        | "java/util/LinkedHashMap$LinkedKeySet"
        | "java/util/LinkedHashMap$LinkedEntrySet"
        | "java/util/Hashtable$KeySet"
        | "java/util/Hashtable$EntrySet"
        | "java/util/concurrent/ConcurrentHashMap$EntrySetView" => &[
            "java/util/Set",
            "java/util/Collection",
            "java/lang/Iterable",
        ],
        // `TreeMap.keySet()` is declared to return a `NavigableSet`, and the
        // carrier `native_tm_key_set` mints must satisfy that checkcast — a
        // plain `Set` here would fail every `(NavigableSet) tm.keySet()`.
        "java/util/TreeMap$KeySet" => &[
            "java/util/NavigableSet",
            "java/util/SortedSet",
            "java/util/Set",
            "java/util/Collection",
            "java/lang/Iterable",
        ],
        // The SORTED maps, split out for the same reason as the sorted sets
        // below and the `ArrayList`/`LinkedList` split further down: a group
        // costs its members the markers that distinguish them. Measured in
        // `--synthetic-jdk` with `apps/probes/CollectionViewTypes` —
        // `aTreeMap instanceof SortedMap` was `false`, so `firstKey()`,
        // `headMap()` and `comparator()` were all unreachable through the
        // interface and `(SortedMap<?, ?>) aTreeMap` threw.
        "java/util/TreeMap" | "java/util/concurrent/ConcurrentSkipListMap" => &[
            "java/util/NavigableMap",
            "java/util/SortedMap",
            "java/util/Map",
            "java/io/Serializable",
        ],
        "java/util/HashMap"
        | "java/util/LinkedHashMap"
        | "java/util/IdentityHashMap"
        | "java/util/WeakHashMap"
        | "java/util/EnumMap"
        | "java/util/concurrent/ConcurrentHashMap" => {
            &["java/util/Map", "java/io/Serializable"]
        }
        // The SORTED sets, split out of the group below for the reason the
        // `ArrayList`/`LinkedList` split above records: a group costs its
        // members exactly the markers that distinguish them. `TreeSet` and
        // `ConcurrentSkipListSet` are `NavigableSet`s (and so `SortedSet`s) on
        // HotSpot and were neither here — measured with
        // `apps/probes/CollectionViewTypes` in `--synthetic-jdk`:
        // `TreeSet instanceof SortedSet` was `false`, so
        // `(SortedSet<?>) aTreeSet` threw and `first()`/`last()`/`headSet` were
        // unreachable through the interface. `EnumSet` and
        // `CopyOnWriteArraySet` are plain `Set`s on HotSpot and stay below.
        "java/util/TreeSet" | "java/util/concurrent/ConcurrentSkipListSet" => &[
            "java/util/NavigableSet",
            "java/util/SortedSet",
            "java/util/Set",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/io/Serializable",
        ],
        "java/util/HashSet"
        | "java/util/LinkedHashSet"
        | "java/util/EnumSet"
        | "java/util/concurrent/CopyOnWriteArraySet"
        // `newKeySet()`'s product. Without `Set` here the synthetic stub is not
        // `instanceof Set`, and `AbstractSet.equals`'s "a Set equals only
        // another Set" guard answers false for a set that is plainly equal.
        | "java/util/concurrent/ConcurrentHashMap$KeySetView" => &[
            "java/util/Set",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/io/Serializable",
        ],
        // LETSGO_S1: Skeletal abstract bases — declare the same root
        // interfaces as their concrete subclasses so `instanceof` checks
        // travelling through the abstract base land on the right answer.
        "java/util/AbstractCollection" => &["java/util/Collection", "java/lang/Iterable"],
        "java/util/AbstractList" | "java/util/AbstractSequentialList" => &[
            "java/util/List",
            "java/util/Collection",
            "java/lang/Iterable",
        ],
        "java/util/AbstractSet" => &[
            "java/util/Set",
            "java/util/Collection",
            "java/lang/Iterable",
        ],
        "java/util/AbstractMap" => &["java/util/Map"],
        // `AbstractMap$SimpleEntry` / `$SimpleImmutableEntry` implement
        // `Map.Entry` — and until this arm existed they implemented nothing,
        // which is not a cosmetic gap: `Map.Entry.equals` is SPECIFIED against
        // any other `Map.Entry`, so every implementation of it starts with
        // `if (!(o instanceof Map.Entry)) return false;`. With the interface
        // unlinked that test answered false for CratonVM's own entries, so two
        // entries with equal keys and values compared UNEQUAL — and so did
        // every user `instanceof Map.Entry` and every `checkcast` javac emits
        // for a `Map.Entry`-typed expression.
        "java/util/AbstractMap$SimpleEntry" | "java/util/AbstractMap$SimpleImmutableEntry" => {
            &["java/util/Map$Entry", "java/io/Serializable"]
        }
        // `Map.entry(k, v)`'s product. HotSpot answers `java.util.KeyValueHolder`
        // for `Map.entry(..).getClass()`, and the reason it has a class of its
        // own is exactly the one CratonVM ran into: `Map.entry`'s entry is
        // immutable and `setValue` must throw, while the entry-set views mint
        // `java/util/Map$Entry` with a third write-through `sourceMap` slot so
        // that `setValue` writes back into the map. One name cannot carry both
        // contracts, and last-write-wins decided which one shipped.
        //
        // NOT `Serializable`: `Map.entry`'s return is specified as not
        // serializable, unlike `AbstractMap$SimpleImmutableEntry` above, and
        // `KeyValueHolder` implements `Map.Entry` only. The interface link is
        // load-bearing rather than cosmetic — `Map.Entry.equals` is specified
        // against any other `Map.Entry`, so both `native_entry_equals`
        // (native-collections) and `register_entry_value_semantics`
        // (native-builtins) open with an `instanceof Map.Entry` test. Without
        // this arm a `KeyValueHolder` would compare unequal to an equal
        // `SimpleEntry` in one direction and equal in the other, which is the
        // asymmetry that blocked this class from landing on 2026-08-06.
        "java/util/KeyValueHolder" => &["java/util/Map$Entry"],
        "java/util/AbstractQueue" => &[
            "java/util/Queue",
            "java/util/Collection",
            "java/lang/Iterable",
        ],
        "java/util/Hashtable" | "java/util/Properties" => &[
            "java/util/Map",
            "java/io/Serializable",
            "java/lang/Cloneable",
        ],
        "java/util/Collections$SynchronizedCollection" => &[
            "java/util/Collection",
            "java/lang/Iterable",
            "java/io/Serializable",
        ],
        "java/util/Collections$SynchronizedSet" => &[
            "java/util/Set",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/io/Serializable",
        ],
        "java/util/Collections$SynchronizedMap" => &["java/util/Map", "java/io/Serializable"],
        // The UNMODIFIABLE and IMMUTABLE views — `Collections.unmodifiable*`,
        // `List/Set/Map.of`, `Arrays.asList`.
        //
        // None of these names had an arm here, so all of them fell to this
        // match's `_ => &[]` default and declared NOTHING. That was invisible
        // because `interpreter::typecheck::synthetic_implements` has a
        // name-word fallback which reads `List` out of `UnmodifiableList` and
        // `Set` out of `Set12` — so the coarse questions answered correctly and
        // only the ones the NAME does not spell got the default. Measured in
        // `--synthetic-jdk` against HotSpot 25.0.3 with
        // `apps/probes/CollectionViewTypes` (24 rows x 10 interfaces):
        //
        //     unmodifiableList(ArrayList)  Iterable  HotSpot T  CratonVM F
        //                                  RandomAccess         T          F
        //     Arrays.asList                RandomAccess         T          F
        //                                  Serializable         T          F
        //     List.of(..)                  Iterable             T          F
        //                                  RandomAccess         T          F
        //     Set.of(..)                   Iterable             T          F
        //
        // A `Collection` that is not an `Iterable` is the worst of those: every
        // for-each through an erased type is a `checkcast java/lang/Iterable`.
        //
        // The sets are FLATTENED, like the `ArrayList` arm above and unlike
        // HotSpot's own class files, where `$UnmodifiableRandomAccessList`
        // declares `RandomAccess` alone and inherits the rest. There is no
        // superclass chain to inherit through here.
        "java/util/Collections$UnmodifiableRandomAccessList" => &[
            "java/util/List",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/util/RandomAccess",
            "java/io/Serializable",
        ],
        // The non-random-access twin. Listed separately and NOT grouped with
        // the arm above: the whole point of the pair is the one marker that
        // differs, and grouping four list classes that way is what cost
        // `ArrayList` its `RandomAccess` until 2026-09-02 (see that arm).
        "java/util/Collections$UnmodifiableList" => &[
            "java/util/List",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/io/Serializable",
        ],
        "java/util/Collections$UnmodifiableCollection" => &[
            "java/util/Collection",
            "java/lang/Iterable",
            "java/io/Serializable",
        ],
        "java/util/Collections$UnmodifiableSet" => &[
            "java/util/Set",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/io/Serializable",
        ],
        "java/util/Collections$UnmodifiableSortedSet" => &[
            "java/util/SortedSet",
            "java/util/Set",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/io/Serializable",
        ],
        "java/util/Collections$UnmodifiableNavigableSet" => &[
            "java/util/NavigableSet",
            "java/util/SortedSet",
            "java/util/Set",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/io/Serializable",
        ],
        "java/util/Collections$UnmodifiableMap" => &["java/util/Map", "java/io/Serializable"],
        "java/util/Collections$UnmodifiableSortedMap" => &[
            "java/util/SortedMap",
            "java/util/Map",
            "java/io/Serializable",
        ],
        // `Arrays.asList`'s view. `RandomAccess` and `Serializable` were the
        // two the name could not spell.
        "java/util/Arrays$ArrayList" => &[
            "java/util/List",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/util/RandomAccess",
            "java/io/Serializable",
        ],
        // `List.of(..)`. Both size forms, because they are supertype-identical
        // on HotSpot — `List12` and `ListN` each extend
        // `ImmutableCollections$AbstractImmutableList`, which is where their
        // `RandomAccess` comes from, and nothing distinguishes them but size.
        "java/util/ImmutableCollections$List12" | "java/util/ImmutableCollections$ListN" => &[
            "java/util/List",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/util/RandomAccess",
            "java/io/Serializable",
        ],
        "java/util/ImmutableCollections$Set12" | "java/util/ImmutableCollections$SetN" => &[
            "java/util/Set",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/io/Serializable",
        ],
        "java/util/ImmutableCollections$Map1" | "java/util/ImmutableCollections$MapN" => {
            &["java/util/Map", "java/io/Serializable"]
        }
        "java/util/Collections$SingletonList" | "java/util/Collections$EmptyList" => &[
            "java/util/List",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/util/RandomAccess",
            "java/io/Serializable",
        ],
        "java/util/Collections$SingletonSet" | "java/util/Collections$EmptySet" => &[
            "java/util/Set",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/io/Serializable",
        ],
        "java/util/Collections$SingletonMap" | "java/util/Collections$EmptyMap" => {
            &["java/util/Map", "java/io/Serializable"]
        }
        "java/util/Collections$EmptyIterator" => &["java/util/Iterator", "java/io/Serializable"],
        "java/util/Collections$EmptyListIterator" => &[
            "java/util/ListIterator",
            "java/util/Iterator",
            "java/io/Serializable",
        ],
        "java/util/Collections$EmptyEnumeration" => {
            &["java/util/Enumeration", "java/io/Serializable"]
        }
        // RE.5 JDK-HttpClient reactive path: the one-shot replay subscription
        // handed to a real `BodySubscriber` (`net_phase_e.rs::
        // RE5_REPLAY_SUBSCRIPTION`). Real JDK bytecode checkcasts it —
        // `ResponseSubscribers$PublishingBodySubscriber.onSubscribe` completes
        // a `CompletableFuture<Flow.Subscription>` whose downstream stage casts
        // the value — so the wrapper class must genuinely implement the
        // interface or Spring's reactive `JdkClientHttpConnector` dies with
        // "HttpBodyReplaySubscription cannot be cast to Flow$Subscription"
        // (WebClientIntegrationTests "[2] JDK", 40 sub-tests).
        "cratonvm/net/HttpBodyReplaySubscription" => &["java/util/concurrent/Flow$Subscription"],
        "java/util/ArrayList$Itr" => &["java/util/Iterator"],
        // The iterator carriers `native_hs_iterator` mints since 2026-08-13
        // (native-collections' `MAP_KEY_ITR_CARRIERS`), replacing the fabricated
        // `java/util/HashMap$KeyItr` no JDK declares. In the real JDK they
        // implement `Iterator` through `HashMap$HashIterator`; with no class
        // file the default `_ => &[]` would leave `instanceof Iterator` false
        // and every `(Iterator) set.iterator()` a ClassCastException.
        "java/util/HashMap$KeyIterator"
        | "java/util/HashMap$EntryIterator"
        | "java/util/LinkedHashMap$LinkedKeyIterator"
        | "java/util/LinkedHashMap$LinkedEntryIterator" => &["java/util/Iterator"],
        "java/util/ArrayList$ListItr" => &["java/util/ListIterator", "java/util/Iterator"],
        // `ArrayList.subList()`'s backed-view object (native-collections'
        // `ASL_CLASS`, allocated under this internal name rather than the
        // real `java/util/ArrayList$SubList` since it has its own native
        // field layout — see native-collections/src/lib.rs's "ArrayList
        // subList backed view" section). With no entry here it fell to this
        // match's `_ => &[]` default, so the returned view declared NO
        // interfaces at all — not even `List`, let alone the `Collection`/
        // `Iterable` it transitively implies. Any checkcast/instanceof
        // against `List`/`Collection`/`Iterable` on a `subList()` result
        // (e.g. AssertJ's `Iterable`-typed `satisfies`/`contains` overloads)
        // failed with `ArrayListSubList cannot be cast to java.lang.Iterable`
        // even though every real `List` is trivially an `Iterable`. Mirrors
        // the real `java.util.ArrayList$SubList`, which extends
        // `AbstractList` (itself `implements List`) and separately
        // `implements RandomAccess`.
        // Both sublist carriers. `java/util/ArrayList$SubList` is the class
        // `alloc_asl_view` mints under whenever the real one resolves; in
        // synthetic-JDK mode there is no class file for it, so without this arm
        // the fabricated stub would declare no interfaces and every
        // `(List) list.subList(..)` would be a ClassCastException — the same
        // failure this entry's internal twin was added for.
        "cratonvm/internal/ArrayListSubList" | "java/util/ArrayList$SubList" => {
            &["java/util/List", "java/util/RandomAccess"]
        }
        // The object `linkedList.listIterator()` hands back. Same reason as the
        // entry above, as `java/util/ArrayList$ListItr` two entries up, and as
        // `cratonvm/synthetic/Process`: with no arm here it fell to this
        // match's `_ => &[]`, so the carrier declared NO interfaces at all —
        // `instanceof ListIterator` was `false` and every erased-type
        // `(ListIterator) x` raised
        // `ClassCastException: cratonvm.internal.LinkedListSnapshotListItr
        // cannot be cast to java.util.ListIterator`.
        //
        // `AbstractList.equals`/`hashCode`/`indexOf` never trip it, because
        // their receiver is already typed `ListIterator` and javac emits no
        // `checkcast` — which is exactly why the family worked at all and why
        // this went unnoticed. It is user code assigning through `Object` (or
        // any erased generic) that meets it.
        //
        // Recorded HERE rather than as a `superclass` link, unlike
        // `SSLSocketInputStream`/`OutputStream` which solve the same
        // checkcast problem that way: `java.util.ListIterator` is an interface
        // with no fields, so there is no layout to alias and nothing to gain
        // from the heavier mechanism.
        //
        // `Iterator` is listed explicitly even though `ListIterator extends
        // Iterator` and `is_assignable_to_name_inner` walks super-interfaces
        // transitively. It costs one `load_class` and it means an
        // `(Iterator) x` cast does not depend on the image having resolved
        // `ListIterator`'s own hierarchy — the same belt-and-braces the
        // `java/util/ArrayList$ListItr` arm above already uses.
        //
        // This arm reaches the carrier through `fabricate_class`, which is the
        // shared body of ALL THREE `ensure_*_class` entry points, so it applies
        // to the `ClassOrigin::VmInternal` door the carrier is minted through
        // since 6ae3ca634 as much as it did to the compatibility door before
        // it. That matters: the mint landing in strict mode without this arm is
        // what took the ClassCastException from Compatible-only to reachable in
        // BOTH modes.
        //
        // HotSpot parity, and therefore permitted in Compatible mode under the
        // contract's §5 freeze: real `java.util.LinkedList$ListItr` implements
        // `ListIterator`, so every cast this admits is one HotSpot admits.
        // It does NOT make `listIterator().getClass()` answer
        // `java.util.LinkedList$ListItr`, and it does not touch the
        // native-owned-`LinkedList`-state defect behind the carrier
        // (`ListItr.remove()` leaves `size` stale) — those are a collections
        // reclassification, not an interface list.
        // W7-16-arraydeque-and-linkedlist-residuals.md
        // W7-20-refusal-laundered-into-wrong-answer-RETIRED-20260917.md
        // W7-62-ratchets-and-dead-code.md
        "cratonvm/internal/LinkedListSnapshotListItr" => {
            &["java/util/ListIterator", "java/util/Iterator"]
        }
        "java/util/Dictionary" => &[],
        "java/util/ArrayDeque" => &[
            "java/util/Deque",
            "java/util/Queue",
            "java/util/Collection",
            "java/lang/Iterable",
        ],
        "java/util/PriorityQueue" => &[
            "java/util/Queue",
            "java/util/Collection",
            "java/lang/Iterable",
        ],
        "java/util/Stack" => &[
            "java/util/List",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/io/Serializable",
        ],
        "java/lang/Throwable"
        | "java/lang/Exception"
        | "java/lang/RuntimeException"
        | "java/lang/Error" => &["java/io/Serializable"],
        "java/lang/Enum" => &["java/io/Serializable", "java/lang/Comparable"],
        "java/lang/Class" => &[
            "java/io/Serializable",
            "java/lang/reflect/GenericDeclaration",
            "java/lang/reflect/Type",
            "java/lang/reflect/AnnotatedElement",
        ],
        // ---- sun.reflect.generics reified types ----
        //
        // `Field.getGenericType()`, `Class.getGenericSuperclass()` and the
        // rest of the WP2.8 reifier deliberately hand back a REAL
        // `sun.reflect.generics.reflectiveObjects.*Impl` rather than the bare
        // interface name, so `getTypeName()`/`toString()` render like HotSpot
        // (`generics::typesig_to_real_type`, `build_wildcard_type`). In a
        // synthetic-library build those `*Impl` names have no class file, so
        // they were fabricated here — and, with no arm in this table, they
        // were fabricated implementing NOTHING. `getGenericType()` then
        // returned an object that answered `false` to `instanceof
        // ParameterizedType`, which is the check every consumer of the API
        // opens with:
        //
        //     Type t = f.getGenericType();
        //     if (!(t instanceof ParameterizedType)) return 0;   // taken
        //
        // So the four `GenericReflectionTest` entries pinned in
        // `KNOWN_SYNTHETIC_JDK_GAPS` ("`getGenericType` / `getGenericSuper-
        // class` never surface `ParameterizedType` or `WildcardType`") were
        // not a missing reifier: the reifier ran, produced the right raw type
        // and the right actual type arguments, and handed them back on an
        // object no `instanceof` could recognise. Measured under
        // `--synthetic-jdk`, the object's `getClass().getName()` was already
        // `sun.reflect.generics.reflectiveObjects.ParameterizedTypeImpl` while
        // `t instanceof ParameterizedType` was `false`.
        //
        // `java/lang/reflect/Type` is listed on each explicitly: these classes
        // are fabricated with `java/lang/Object` as their superclass, so they
        // inherit no interface, and code that erases to `Type` (every `Type[]`
        // element store, `getActualTypeArguments`, `getBounds`) needs the
        // supertype to be true of them.
        "sun/reflect/generics/reflectiveObjects/ParameterizedTypeImpl" => &[
            "java/lang/reflect/ParameterizedType",
            "java/lang/reflect/Type",
        ],
        "sun/reflect/generics/reflectiveObjects/WildcardTypeImpl" => &[
            "java/lang/reflect/WildcardType",
            "java/lang/reflect/Type",
        ],
        "sun/reflect/generics/reflectiveObjects/TypeVariableImpl" => &[
            "java/lang/reflect/TypeVariable",
            "java/lang/reflect/GenericDeclaration",
            "java/lang/reflect/AnnotatedElement",
            "java/lang/reflect/Type",
        ],
        "sun/reflect/generics/reflectiveObjects/GenericArrayTypeImpl" => &[
            "java/lang/reflect/GenericArrayType",
            "java/lang/reflect/Type",
        ],
        // The bare-interface synthetics the same reifier mints on its
        // fallback paths (`generics::type_sig_to_java`) are `instanceof`-
        // correct for their own name for free, but nothing linked them to
        // `Type` — the erased element type of every array they are stored
        // into.
        "java/lang/reflect/ParameterizedType"
        | "java/lang/reflect/WildcardType"
        | "java/lang/reflect/GenericArrayType" => &["java/lang/reflect/Type"],
        "java/lang/reflect/TypeVariable" => &[
            "java/lang/reflect/Type",
            "java/lang/reflect/AnnotatedElement",
        ],
        // ---- java.io / java.nio interfaces ----
        "java/io/InputStream" => &["java/io/Closeable"],
        "java/io/OutputStream" => &["java/io/Closeable", "java/io/Flushable"],
        "java/io/Reader" => &["java/lang/Readable", "java/io/Closeable"],
        "java/io/Writer" => &[
            "java/lang/Appendable",
            "java/io/Closeable",
            "java/io/Flushable",
        ],
        "java/io/Closeable" => &["java/lang/AutoCloseable"],
        "java/io/PrintStream" => &["java/lang/Appendable"],
        "java/nio/charset/Charset" => &["java/lang/Comparable"],

        // Synthetic functional interface composition classes (M3 fix)
        "java/util/function/UnaryOperator" => &["java/util/function/Function"],
        "java/util/function/Function$AndThen" | "java/util/function/Function$Compose" => {
            &["java/util/function/Function"]
        }
        "java/util/function/Function$Identity" => &[
            "java/util/function/UnaryOperator",
            "java/util/function/Function",
        ],
        "java/util/function/Consumer$AndThen" => &["java/util/function/Consumer"],
        "java/util/function/Predicate$$Lambda$And"
        | "java/util/function/Predicate$$Lambda$Or"
        | "java/util/function/Predicate$$Lambda$Negate" => &["java/util/function/Predicate"],

        // S111r17 — Our internal `AnnotationProxy` must declare
        // `java.lang.annotation.Annotation` as a superinterface so that
        // class-graph walks (`is_subclass_of`, `array_is_assignable_to`)
        // recognise an `[Ljava/lang/annotation/AnnotationProxy;` array as
        // an `[Ljava/lang/annotation/Annotation;` array.  Spring 5.x
        // (SB2) `AnnotationUtils.adaptValue` does exactly that
        // `instanceof [Ljava.lang.annotation.Annotation;` check before
        // converting nested-annotation arrays to `AnnotationAttributes[]`,
        // and without the implements-edge the conversion silently
        // skips, leaving Spring to feed the raw `AnnotationProxy[]`
        // into `AnnotationAttributes.assertAttributeType` which then
        // throws `IllegalArgumentException`.
        "java/lang/annotation/AnnotationProxy" => &["java/lang/annotation/Annotation"],
        _ => &[],
    }
}

/// Detect JBoss-Logging i18n locale-suffix probes (`_$logger_<locale>` /
/// `_$bundle_<locale>`).
///
/// `Logger.doGetMessageLogger` walks a chain of generated implementation
/// class names — most-specific locale variant down to the locale-less
/// `_$logger` / `_$bundle` shipped in the JAR — wrapping each
/// `Lookup.findClass` in a `try/catch (ClassNotFoundException)`. The
/// locale-suffixed variants are *intentionally absent*; the catch is the
/// signal to try the next variant. Without this special-case, our
/// `is_jdk_class("org/jboss/...")` synthetic-stub fallback would succeed
/// and the heuristic in `create_synthetic_stub` (treating any `$`-bearing
/// name as an interface) leads to a `Class.asSubclass` CCE that escapes
/// the caller's CNFE catch.
pub(in crate::class_manager) fn is_jboss_logging_locale_lookup(name: &str) -> bool {
    let suffix_start = name
        .rfind("_$logger_")
        .map(|i| i + "_$logger_".len())
        .or_else(|| name.rfind("_$bundle_").map(|i| i + "_$bundle_".len()));
    let Some(start) = suffix_start else {
        return false;
    };
    let suffix = &name[start..];
    !suffix.is_empty()
        && suffix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Check if a class name belongs to the JDK (should get a synthetic stub
/// when its .class file is not found).
/// Scan ES IMPL-JARS flat layout for `internal_name` (slash-format).
///
/// ES outer module JARs store inner-jar contents as individual ZIP entries
/// under `IMPL-JARS/<module>/<jar-name>/<path>`, NOT as binary JAR blobs.
/// LISTING.TXT enumerates which inner JAR names are present.
///
/// For ES packages, a fast derivation maps the class prefix to the module.
/// For third-party packages (e.g. `com/fasterxml/jackson`), we fall back to
/// scanning all known IMPL-JARS modules (cheap: only 2 in ES 8.15.5).
pub(in crate::class_manager) fn find_in_impl_jars(app_cp: &ClassPath, internal_name: &str) -> Option<Vec<u8>> {
    const KNOWN_PREFIX_TO_MODULE: &[(&str, &str)] = &[
        ("org/elasticsearch/xcontent", "x-content"),
        ("org/elasticsearch/xpack", "x-pack"),
        ("org/elasticsearch/transport", "transport"),
        ("org/elasticsearch/common", "common"),
        ("org/elasticsearch/core", "core"),
    ];
    // All IMPL-JARS modules present in ES 8.15.5; used for non-ES packages
    // (e.g. com/fasterxml/jackson lives in x-content's IMPL-JARS).
    const ALL_MODULES: &[&str] = &["x-content", "native-access-jna"];

    // Derive candidate module(s) to search
    let fast_module: Option<String> = KNOWN_PREFIX_TO_MODULE
        .iter()
        .find(|(prefix, _)| internal_name.starts_with(prefix))
        .map(|(_, m)| m.to_string())
        .or_else(|| {
            internal_name
                .strip_prefix("org/elasticsearch/")
                .and_then(|rest| rest.split('/').next())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_lowercase())
        });

    let class_file = format!("{internal_name}.class");
    let modules: Vec<String> = match fast_module {
        Some(m) => vec![m],
        None => ALL_MODULES.iter().map(|s| s.to_string()).collect(),
    };

    for module_name in &modules {
        let listing_path = format!("IMPL-JARS/{module_name}/LISTING.TXT");
        let Some(listing_bytes) = app_cp.find_resource(&listing_path) else {
            continue;
        };
        let listing_text = String::from_utf8_lossy(&listing_bytes).into_owned();
        for jar_name in listing_text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && l.ends_with(".jar"))
        {
            let entry_path = format!("IMPL-JARS/{module_name}/{jar_name}/{class_file}");
            if let Some(bytes) = app_cp.find_resource(&entry_path) {
                return Some(bytes);            }        }    }
    None
}

/// The standard JDK namespaces for which a real boot jimage/jmods is the
/// authoritative source of truth. A delegated miss for one of these in
/// real-JDK mode means the class genuinely does not exist (→ CNFE), as opposed
/// to the enterprise app prefixes in [`is_jdk_class`] (which use the synthetic
/// stub mechanism) or array classes.
pub(in crate::class_manager) fn is_standard_jdk_namespace(name: &str) -> bool {
    name.starts_with("java/")
        || name.starts_with("javax/")
        || name.starts_with("sun/")
        || name.starts_with("jdk/")
        || name.starts_with("com/sun/")
}

/// JDK API types whose behavior CratonVM supplies through native bridges even
/// when the host JDK image cannot provide their class files.
pub(in crate::class_manager) fn is_native_backed_jdk_stub(name: &str) -> bool {
    matches!(
        name,
        "java/lang/ProcessHandle" | "java/lang/ProcessHandle$Info"
    )
}

/// Non-JDK package prefixes whose classes get a synthetic-stub fallback when
/// absent from the classpath. These exist so WildFly / Quarkus *bytecode* can
/// link against types CratonVM handles natively or that are genuinely optional
/// (e.g. `org.jboss.modules.Module`); a real jar on the classpath still loads
/// normally because the fallback only fires after classpath lookup has failed.
///
/// A reflective `Class.forName` / `isPresent` probe must NOT be satisfied by one
/// of these stubs — see [`cratonvm_types::reflective_probe`] and the gate in
/// [`ClassManager::load_class`].
pub(in crate::class_manager) fn is_enterprise_stub_prefix(name: &str) -> bool {
    name.starts_with("org/jboss/")
        || name.starts_with("org/wildfly/")
        || name.starts_with("org/xnio/")
        || name.starts_with("org/infinispan/")
        || name.starts_with("io/quarkus/")
        || name.starts_with("io/agroal/")
        || name.starts_with("io/undertow/")
        || name.starts_with("io/smallrye/")
}

/// The last `/`-separated segment of an internal class name.
///
/// The generated-class conventions below are all statements about the simple
/// name, not the package: `com/sun/proxy/$Proxy0` and `com/example/$Proxy0` are
/// both proxies, and a package that merely *contains* the string `$Proxy` is
/// not.
pub(in crate::class_manager) fn simple_name_of(internal_name: &str) -> &str {
    match internal_name.rfind('/') {
        Some(slash) => &internal_name[slash + 1..],
        None => internal_name,
    }
}

/// W3-2 — must a class-file `NestHost` attribute be discarded at definition
/// time because the class is hidden and no defining `Lookup` claimed it?
///
/// # The rule
///
/// `Lookup.defineHiddenClass(bytes, initialize)` **without**
/// `ClassOption.NESTMATE` produces a hidden class in its **own** nest:
/// `hc.getNestHost() == hc`. With `NESTMATE`, the hidden class joins the
/// lookup class's nest and the defining `Lookup` supplies the host name in
/// [`DefineClassOptions::nest_host_class_name`].
///
/// # Why the class file's own attribute must not be honoured
///
/// The `NestHost` attribute is emitted by javac for **every** nested class,
/// so re-defining an already-compiled nested class as a hidden class hands
/// this code a `NestHost` that the caller never asked for. That is exactly
/// what `regression-suite/src/RJdkHidden.java:147` does: it reads
/// `RJdkHidden$Payload.class` back off the class path (attribute
/// `NestHost: RJdkHidden`, confirmed with `javap -v`) and defines those bytes
/// a second time with no `ClassOption`. Adopting the attribute made
/// `Class.getNestHost()` answer `RJdkHidden`, failing `RJdkHidden.java:151`
/// on both `--fast-boot` and `--real-jdk` while HotSpot passes.
///
/// HotSpot reaches "itself" by a route this VM cannot reproduce verbatim:
/// `InstanceKlass::nest_host()` resolves the `NestHost` attribute and then
/// requires the resolved host to list the claimant in its own `NestMembers`.
/// A hidden class is registered under a mangled, per-definition name
/// (`RJdkHidden$Payload/0x1f`) that no compile-time `NestMembers` attribute
/// can ever spell, so the round-trip fails and HotSpot silently falls back to
/// self-nest. That failure is unconditional *by construction*, not incidental
/// — which is why evaluating the predicate once here, at definition, is
/// equivalent to evaluating it on every query. It is the mirror image of the
/// exemption `access_control::confirmed_nest_host` already records: there, a
/// hidden class's *supplied* host is trusted precisely because the
/// `NestMembers` round-trip is unsatisfiable for a mangled name.
///
/// # What is deliberately NOT touched
///
/// * `options.nest_host_class_name = Some(..)` (the NESTMATE arm, and
///   `Unsafe.defineAnonymousClass`'s host) stays authoritative: only the
///   defining call could have made that claim, and the caller must already
///   have had nest-level access to mint the `Lookup`.
/// * Non-hidden classes are untouched — an ordinary nested class keeps its
///   `NestHost` attribute and still goes through the full bidirectional
///   confirmation in `access_control::confirmed_nest_host`.
///
/// All four producers of `hidden: true` agree on the discriminator: they set
/// `nest_host_class_name` if and only if NESTMATE was requested — see
/// `native-builtins/src/lookup_define.rs` (both variants),
/// `native-builtins/src/classloader.rs::lk_define_hidden_class`, and
/// `native-builtins/src/lang_system.rs::native_classloader_define_class0`.
pub(crate) fn hidden_class_drops_class_file_nest_host(
    hidden: bool,
    explicit_nest_host: Option<&str>,
) -> bool {
    hidden && explicit_nest_host.is_none()
}

/// The six class-level attributes that place a class among others — its nest
/// (`NestHost`, `NestMembers`), its record components (`Record`), its sealed
/// hierarchy (`PermittedSubclasses`) and its nesting (`InnerClasses`,
/// `EnclosingMethod`) — as the runtime `Class` stores them. The first
/// occurrence of each attribute wins, as the define path always had it.
///
/// One reader for the two places a `Class` is filled from a class file:
/// `define_class_with_options`, which feeds it from its single attribute pass
/// ([`Self::absorb`]), and `upgrade_synthetic_class`, which until interpreter
/// round i1 wave 27 (lane L5) left the stub's literals in place
/// ([`Self::of`]).
#[derive(Default)]
pub(in crate::class_manager) struct NestingAttributes {
    pub(in crate::class_manager) nest_host: Option<String>,
    pub(in crate::class_manager) nest_members: Vec<String>,
    pub(in crate::class_manager) record_components: Vec<RecordComponentInfo>,
    /// A `Record` attribute was seen, even one with no component
    /// (`ClassManager::empty_record_classes`; i4 wave 84, lane L3).
    pub(in crate::class_manager) has_record_attribute: bool,
    pub(in crate::class_manager) permitted_subclasses: Vec<String>,
    pub(in crate::class_manager) inner_classes: Vec<InnerClassEntry>,
    pub(in crate::class_manager) enclosing_method: Option<Box<EnclosingMethodInfo>>,
    /// An `InnerClasses` attribute was seen, even one with no entry (JIT
    /// round 21 wave 26, lane nesting; [`Self::hotspot_stored`]).
    pub(in crate::class_manager) has_inner_classes_attribute: bool,
}



impl NestingAttributes {
    /// Read them from a class file's (decoded) class-level attributes.
    pub(in crate::class_manager) fn of(attributes: &[cratonvm_reader::attribute::LazyAttribute], cp: &ConstantPool) -> Self {
        let mut nesting = Self::default();
        for attr in attributes {
            if let Some(attr) = attr.as_decoded() {
                nesting.absorb(attr, cp);
            }
        }
        nesting.hotspot_stored(true)
    }

    /// The nesting as HotSpot's `ClassFileParser` STORES it: an
    /// `EnclosingMethod` attribute is dropped when the class has no
    /// `InnerClasses` attribute (with `needs_inner_classes`).
    ///
    /// HotSpot keeps the two `EnclosingMethod` indices as the tail of
    /// `InstanceKlass::_inner_classes`, written by
    /// `parse_classfile_inner_classes_attribute`. Its call site passes
    /// `parsed_innerclasses_attribute` as the parameter named
    /// `parsed_enclosingmethod_attribute`, so the array gets the two-slot tail
    /// only when an `InnerClasses` attribute was parsed, and with
    /// `EnclosingMethod` alone it is empty: `enclosing_method_class_index()`
    /// is 0. Every Java-visible answer follows. `getEnclosingMethod0` is
    /// null, so the class is top level (`isAnonymousClass` / `isLocalClass`
    /// false, `getSimpleName` the binary-name tail, `getCanonicalName` its
    /// name, `getEnclosingClass` null), and the `EnclosingMethod` class is
    /// never resolved: a missing one throws nothing and the loader is not
    /// asked for it. Measured on HotSpot 25 by five classes of four probes,
    /// all `EnclosingMethod` with no `InnerClasses`: `R21Fz6InnerRows1`'s
    /// X$4, `R21Fz8Nesting1`'s Y$1, `R21Fz8ReviewEmGone1`'s $E and
    /// `R21Fz9EmMissing1`'s C$E (both naming a missing class), and
    /// `R21Fz9ReviewEmUser1`'s hidden $H. javac never writes this shape: it
    /// gives every local and anonymous class its own `InnerClasses` row.
    ///
    /// An `InnerClasses` attribute HotSpot ignores (a circular or repeated
    /// table, `inner_classes_ignored`) still keeps `EnclosingMethod`: the
    /// parser reallocates the two-slot tail.
    pub(in crate::class_manager) fn hotspot_stored(mut self, needs_inner_classes: bool) -> Self {
        if needs_inner_classes && !self.has_inner_classes_attribute {
            self.enclosing_method = None;
        }
        self
    }

    /// Take `attr` if it is one of the six and the first of its kind; ignore
    /// anything else.
    pub(in crate::class_manager) fn absorb(&mut self, attr: &Attribute, cp: &ConstantPool) {
        match attr {
            Attribute::NestHost { host_class_index } => {
                if self.nest_host.is_none() {
                    self.nest_host = cp.get_class_name(*host_class_index).map(|s| s.to_string());
                }
            }
            Attribute::NestMembers { classes } => {
                if self.nest_members.is_empty() {
                    self.nest_members = classes
                        .iter()
                        .filter_map(|idx| cp.get_class_name(*idx).map(|s| s.to_string()))
                        .collect();
                }
            }
            Attribute::Record(components) => {
                if !self.has_record_attribute {
                    self.has_record_attribute = true;
                    self.record_components = components
                        .iter()
                        .filter_map(|rc| {
                            let n = cp.get_utf8(rc.name_index)?;
                            let d = cp.get_utf8(rc.descriptor_index)?;
                            // Interpreter round i2 wave 58, lane L3: the
                            // component's annotation bodies, for reflection.
                            let body = |visible_type: bool| {
                                rc.attributes
                                    .iter()
                                    .find(|a| match a {
                                        Attribute::RuntimeVisibleAnnotations(_) => !visible_type,
                                        Attribute::RuntimeVisibleTypeAnnotations(_) => visible_type,
                                        _ => false,
                                    })
                                    .and_then(cratonvm_reader::attribute::annotation_attribute_body)
                                    .map(std::sync::Arc::from)
                            };
                            Some(RecordComponentInfo {
                                name: n.to_string(),
                                descriptor: d.to_string(),
                                annotations: body(false),
                                type_annotations: body(true),
                                // Interpreter round i5 wave 101, lane L3:
                                // the component's own Signature.
                                signature: rc.attributes.iter().find_map(|a| match a {
                                    Attribute::Signature(s) => Some(std::sync::Arc::clone(s)),
                                    _ => None,
                                }),
                            })
                        })
                        .collect();
                }
            }
            Attribute::PermittedSubclasses { classes } => {
                if self.permitted_subclasses.is_empty() {
                    self.permitted_subclasses = classes
                        .iter()
                        .filter_map(|idx| cp.get_class_name(*idx).map(|s| s.to_string()))
                        .collect();
                }
            }
            Attribute::InnerClasses(entries) => {
                self.has_inner_classes_attribute = true;
                if self.inner_classes.is_empty() {
                    // zt t3/fp (zs3rv2 §9): the interned `""` once per
                    // attribute, not once per entry (a pool lock and a hash
                    // each); every empty name shares it.
                    let empty_name = ThinArcStr::from(cratonvm_types::intern_arc(""));
                    self.inner_classes = entries
                        .iter()
                        .filter_map(|ic| {
                            // zs s2/fp2: share the pool's interned strings.
                            let empty = || empty_name.clone();
                            let inner = cp.get_class_name_arc(ic.inner_class_info_index)?;
                            let outer = if ic.outer_class_info_index == 0 {
                                None
                            } else {
                                cp.get_class_name_arc(ic.outer_class_info_index)
                            };
                            let inner_name = if ic.inner_name_index == 0 {
                                None
                            } else {
                                cp.get_utf8_arc(ic.inner_name_index)
                            };
                            Some(InnerClassEntry {
                                inner_class: ThinArcStr::from(inner),
                                outer_class: outer.map_or_else(empty, ThinArcStr::from),
                                inner_name: inner_name.map_or_else(empty, ThinArcStr::from),
                                access_flags: ic.inner_class_access_flags,
                            })
                        })
                        .collect();
                }
            }
            Attribute::EnclosingMethod {
                class_index,
                method_index,
            } => {
                if self.enclosing_method.is_none() {
                    if let Some(class_name) = cp.get_class_name(*class_index).map(|s| s.to_string())
                    {
                        let (method_name, method_descriptor) = if *method_index == 0 {
                            (String::new(), String::new())
                        } else {
                            cp.get_name_and_type(*method_index)
                                .map(|(n, d)| (n.to_string(), d.to_string()))
                                .unwrap_or_default()
                        };
                        self.enclosing_method = Some(Box::new(EnclosingMethodInfo {
                            class_name,
                            method_name,
                            method_descriptor,
                        }));
                    }
                }
            }
            _ => {}
        }
    }
}

/// A lambda / method-reference implementation class spun by
/// `LambdaMetafactory` (HotSpot: `Owner$$Lambda$N` or, since JDK 21,
/// `Owner$$Lambda/0x…`), or by this VM's own invokedynamic path.
///
/// Matched anywhere in the name because the marker sits between the capturing
/// class and the counter, never at a segment boundary.
pub(in crate::class_manager) fn is_generated_lambda_name(name: &str) -> bool {
    name.contains("$$Lambda")
}

/// A `java.lang.reflect.Proxy` implementation class: simple name `$ProxyN`.
///
/// The digits matter. `$Proxy` with a non-numeric tail is an ordinary nested
/// class of an outer type called `Proxy` (this VM's own
/// `java/lang/reflect/Proxy$Instance` is exactly that, and must NOT be counted
/// as a generated proxy).
pub(in crate::class_manager) fn is_generated_proxy_name(name: &str) -> bool {
    match simple_name_of(name).strip_prefix("$Proxy") {
        Some(counter) => !counter.is_empty() && counter.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// A reflection / serialization accessor emitted by the JDK's bytecode
/// accessor generator: `GeneratedMethodAccessorN`,
/// `GeneratedConstructorAccessorN`,
/// `GeneratedSerializationConstructorAccessorN`.
pub(in crate::class_manager) fn is_reflection_accessor_name(name: &str) -> bool {
    let simple = simple_name_of(name);
    simple.starts_with("Generated") && simple.contains("Accessor")
}

/// This VM's own invented supertype for dynamic proxies.
///
/// `java/lang/reflect/Proxy$Instance` is a name no JDK declares and no class
/// file can ever back. `proxy_gen` emits every `$ProxyN` extending it (unless
/// `CRATONVM_REAL_PROXY_SUPER=0` is unset — by default `real_proxy_super()` is
/// on and `$ProxyN` extends the real `java.lang.reflect.Proxy` instead), and
/// `reflect_annotations.rs` registers `<init>` / `getProxyInterfacesNative`
/// natives on it. That makes it a *generation artefact*, not a stand-in for
/// bytes that should have been found.
pub(crate) fn is_vm_proxy_supertype_name(name: &str) -> bool {
    name == "java/lang/reflect/Proxy$Instance"
}

/// This VM's own invented carrier for an annotation's captured member values.
///
/// `java/lang/annotation/AnnotationProxy` is the 4-slot tuple (type descriptor,
/// type mirror, element names, element values) that
/// `native-builtins/src/lang_class.rs::create_annotation_proxy_with_type`
/// mints as the invocation handler behind every generated annotation
/// `$ProxyN`. It plays the role HotSpot gives to
/// `sun.reflect.annotation.AnnotationInvocationHandler`, under a name of this
/// VM's own choosing: `javap java.lang.annotation.AnnotationProxy` against the
/// JDK 25 image answers "class not found", so no class file can ever back it
/// and it is a *generation artefact*, not a stand-in for bytes that should
/// have been found. Exactly the argument one function up, for exactly the same
/// species — see `docs/feature-designs/class-resolution-and-caching.md`
/// and `W7-17-vm-internal-door-sweep.md`.
///
/// # Yes, this binds by NAME. Read this before "fixing" it.
///
/// This campaign's most expensive rule is *never bind by name* — an
/// `invokestatic` owner, a `$ProxyN`'s interface, a reflect stub's rendered
/// name, a MIC's owner and a shape test on a class name were six separate
/// defects, all of the same shape: a **same-named class from another loader**
/// was the correct answer and the string picked the wrong one. That failure
/// mode is unreachable here, and not by luck:
///
/// * **There is no other copy to confuse this with.**
///   [`ClassManager::fabricated_origin_for_name`] is consulted only from
///   [`ClassManager::try_ensure_synthetic_class`] →
///   [`ClassManager::fabricate_class`], which by then has already had
///   `get_loaded_class_id` answer `None` *and* `find_class_bytes_delegated`
///   fail. The name resolves to nothing, in any loader, on any classpath
///   entry. Every one of the six defects was a decision taken while two live
///   classes existed; this one is taken only when none does.
/// * **No other party may occupy the name.** `java/lang/annotation/` is a
///   package no non-bootstrap loader is permitted to define into, and the
///   bootstrap loader defines only what the image declares — which, per the
///   `javap` above, is not this. The string is not a guess at an identity, it
///   is the identity.
/// * **It classifies, it does not dispatch.** The six defects all *bound* a
///   call, a cast or a field to a class. This answers "what provenance does a
///   class the VM is about to invent deserve", the question this function
///   exists for and whose only input is the invented name.
///
/// # What binding by name here does cost, stated plainly
///
/// [`ClassManager::fabricate_class`] runs its ambiguity gate
/// (`classify_loaded_name` → `ambiguous_stand_in_refused`) **only** for
/// compatibility-stub origins, so a name routed here skips it — deliberately,
/// per that gate's own comment about `ensure_generated_class` minting under a
/// name it just constructed. For this name that is inert for the second reason
/// above: the package cannot hold a second definition. Any future entry added
/// beside it must be able to make the same statement, or it is trading a
/// census label for the type confusion the ambiguity gate exists to stop.
pub(crate) fn is_vm_annotation_carrier_name(name: &str) -> bool {
    name == "java/lang/annotation/AnnotationProxy"
}

/// This VM's reserved `CratonVM$…` namespace for the small carriers it invents
/// to hold state between two of its own natives.
///
/// Two names live here today, both 1–2 slot `Runnable`s with a native `run()V`
/// and nothing else:
///
/// * `CratonVM$HttpServerLoop` — `native-builtins/src/net_phase_e.rs`, minted
///   once per `HS_DISPATCHER_POOL` dispatcher to carry a `server_id` from
///   `re10_spawn_dispatcher` to `re10_serve_loop_run`. This is the class
///   `W7-17-vm-internal-door-sweep.md` §6A measured taking
///   `HttpServer.start()` down under `--real-jdk` with
///   `NoClassDefFoundError: CratonVM$HttpServerLoop`.
/// * `CratonVM$StsForkRunner` — `native-builtins/src/jdk25_concurrency.rs`,
///   the JEP 505 `StructuredTaskScope.fork()` worker body carrying the
///   `Callable` and the `Subtask`. It is minted with a bare
///   `try_alloc_concurrent_synthetic` and has **no** `ensure_vm_internal_class`
///   pre-mint of its own, so before this arm it was the same door defect one
///   file over, unrecorded.
///
/// # Why the prefix is admissible where a name usually is not
///
/// `is_vm_annotation_carrier_name` above carries this campaign's *never bind
/// by name* argument in full; the same three statements hold here and the
/// second is the one that changes shape, so it is restated rather than
/// referenced:
///
/// * **There is no other copy to confuse this with.**
///   [`fabricated_origin_for_name`] is reached only from
///   [`ClassManager::try_ensure_synthetic_class`] →
///   [`ClassManager::fabricate_class`], by which point `get_loaded_class_id`
///   has answered `None` **and** `find_class_bytes_delegated` has failed. If an
///   application really did put a `CratonVM$…` class on its classpath, the
///   bytes would have been found and this function would never run.
/// * **No other party is minting into it.** `java/lang/annotation/` is closed
///   by the JVM's package rules; `CratonVM$` is closed by convention instead —
///   it is this VM's own prefix, every use of it in the workspace is one of the
///   two names above, and neither is a JDK name (`javap CratonVM$HttpServerLoop`
///   against the JDK 25 image answers "class not found"; the string is in no
///   JDK namespace at all). That is a weaker guarantee than the package rule
///   and it is why the statement is written down: **anything added under this
///   prefix must be a carrier the VM invents, never a stand-in for bytes some
///   image declares.**
/// * **It classifies, it does not dispatch.** Same as above — the only input is
///   the invented name, and the answer is a provenance label.
///
/// # Gate 2 is already open for both, which is what makes this sufficient
///
/// `W7-17` §3's rule is that the door is only half the refusal: a class whose
/// natives strict mode also drops buys an `UnsatisfiedLinkError` at the first
/// call instead of a `NoClassDefFoundError` at the mint. Checked per name, not
/// by analogy — neither appears in any table in
/// `native-api/src/no_image_receiver.rs`, and
/// `receiver_declared_by_no_supported_image` returns `false` for a name on
/// none of them, so nothing re-tags either class's `run()V` `SyntheticStub`.
///
/// # Cost, stated as `is_vm_annotation_carrier_name` requires
///
/// A name routed to [`ClassOrigin::VmInternal`] skips
/// [`ClassManager::fabricate_class`]'s ambiguity gate, which runs only for
/// compatibility-stub origins. Inert here for the first bullet's reason: the
/// gate exists to refuse a stand-in for a name several live classes already
/// hold, and this path is reached only when no supplier answered at all.
/// [`ClassManager::classify_defined_origin`]'s asymmetry does not arise either
/// — no class file can ever back these names, so they never arrive with real
/// bytes for the two paths to disagree about.
pub(crate) fn is_vm_reserved_namespace_name(name: &str) -> bool {
    name.starts_with("CratonVM$")
}

/// The origin a **fabricated** class deserves.
///
/// The answer is [`ClassOrigin::CompatibilityStub`] for everything except three
/// exact identities the VM invents. It is deliberately NOT a classifier over
/// name *shapes* any more — see "The three generated-name families are NOT
/// here" below, which is the `--real-jdk` policy hole this function used to be.
///
/// [`ClassManager::ensure_synthetic_class`] and
/// [`ClassManager::try_ensure_synthetic_class`] used to hard-code
/// [`ClassOrigin::CompatibilityStub`] for everything they minted. That is right
/// for the overwhelming majority — a name whose real bytes were not found — and
/// wrong for the handful of names the VM *invents*, which have no real bytes to
/// find and are not standing in for anyone's class:
///
/// * `java/lang/reflect/Proxy$Instance` → [`ClassOrigin::VmInternal`].
///   `is_generated_proxy_name` above already says in terms that it "must NOT be
///   counted as a generated proxy", and [`ClassOrigin::GeneratedProxy`] carries
///   an `interfaces` list the shared *supertype* has no value for. The
///   dispatch consequences of the flip are handled by
///   [`Class::dispatch_lacks_class_file`](crate::Class::dispatch_lacks_class_file),
///   which is what the three read sites that can observe this class now ask.
/// * `java/lang/annotation/AnnotationProxy` → [`ClassOrigin::VmInternal`], the
///   same species and the same argument one class later — see
///   [`is_vm_annotation_carrier_name`], which carries the "yes, this binds by
///   name" justification for both. `GeneratedProxy` is as wrong for it as for
///   the supertype above, and for the same reason: it carries an `interfaces`
///   list a carrier has no value for. The authoritative half of this fix is at
///   the mint site (`lang_class.rs`, which now pre-mints through
///   `ensure_vm_internal_class`); this arm exists so a second minting route
///   cannot silently re-acquire the wrong label, which is precisely the
///   pairing `Proxy$Instance` already has.
/// * `CratonVM$…` → [`ClassOrigin::VmInternal`] — see
///   [`is_vm_reserved_namespace_name`], which carries the prefix argument and
///   the per-name gate-2 check. This is the pairing `W7-17` §6A left optional;
///   it is taken because a SECOND minting route exists and has no pre-mint of
///   its own (`CratonVM$StsForkRunner`, `jdk25_concurrency.rs`), so here the
///   arm is not belt-and-braces — it is the only thing covering that name.
///
/// `host`/`interfaces` do not appear anywhere below: this path has no class
/// file, so there is no nest host to read and no resolved interface set, and a
/// fabrication is not a place to invent metadata.
///
/// # The three generated-name families are NOT here — REMOVED 2026-08-12 (A17)
///
/// This function used to carry three more arms: a name containing `$$Lambda`
/// became [`ClassOrigin::GeneratedLambda`], a `$ProxyN` simple name became
/// [`ClassOrigin::GeneratedProxy`], and a `Generated*Accessor*` simple name
/// became [`ClassOrigin::ReflectionAccessor`]. All three are non-stub origins,
/// so [`ClassManager::fabricate_class`]'s `--real-jdk` refusal — which is gated
/// on `origin.is_compatibility_stub()` — did not fire for them. **A fabricated
/// class could exempt itself from the strict-mode policy by choosing its own
/// name.**
///
/// That was not hypothetical. Measured 2026-08-12 with
/// `cratonvm --real-jdk --dump-class-origins`: the run's census had
/// `compatibility-stub 0` — the refusal working — and exactly one
/// `generated-lambda` row, `java/util/function/Predicate$$Lambda$And`, a
/// compatibility stand-in minted by `phases_late/streams.rs` and created in
/// strict mode purely because of the `$$Lambda$` infix in the name its author
/// picked. Its sibling `java/util/function/Consumer$AndThen`, the same species
/// of stand-in with no infix, was correctly refused at the door. The two
/// failures looked unrelated in the field — `NoClassDefFoundError` for one,
/// `AbstractMethodError: … has no Code attribute` for the other — which is why
/// the shared cause went untraced for a week.
///
/// ## The measurement the old comment carried was false, not merely stale
///
/// It read: *"Measured 2026-08-05 (L7) and again 2026-08-06: none of the three
/// fires on a strict boot, `JdkOnlyCensusLoadProbe` or `JdkOnlyBreadthProbe`."*
/// That statement was true **of those two probes** and false of the VM: neither
/// probe ever calls a `Predicate` combinator, so neither could reach the one
/// mint site that fires the arm. A narrow probe reports its own reach, not the
/// defect. Recorded rather than deleted because the next person to add a
/// name-shape arm here will reach for the same evidence.
///
/// ## Why removing them cannot break a real lambda, proxy or accessor
///
/// The discriminator is **structural, not lexical**: it is which door the class
/// came through, and that is decided before this function is consulted.
///
/// * A genuine `LambdaMetafactory`/`$ProxyN`/accessor class **has bytes**, is
///   defined through [`ClassManager::define_class_shared_with_options`], and is
///   classified by [`ClassManager::classify_defined_origin`] — which still has
///   all three arms, unchanged, and whose own doc states the invariant that
///   makes them right there: *"by the time this runs, real class bytes have
///   been parsed."*
/// * This function is reached **only** from
///   [`ClassManager::try_ensure_synthetic_class`] →
///   [`ClassManager::fabricate_class`], by which point `get_loaded_class_id` has
///   answered `None` **and** `find_class_bytes_delegated` has failed. The name
///   resolves to nothing, in any loader, on any classpath entry. A `$$Lambda`
///   with no bytes anywhere was not spun by `LambdaMetafactory`; it is a
///   hand-minted stand-in that chose a generated-looking name.
/// * CratonVM's **own** invokedynamic path never arrives here either, and not by
///   luck: `invokedynamic.rs::link_lambda_call_site` takes a `ClassId` from
///   `SharedVm::alloc_lambda_proxy_id` (a bare counter from `0x8000_0000`) and
///   files the call site in the `lambda_proxies` side table. A CratonVM lambda
///   proxy has **no `Class` in the store at all**; the `Host$$Lambda/0x…` string
///   is rendered on demand by `lang_class.rs::lambda_class_name` for
///   `Class.getName`, and is never fed back into a mint. Consistent with the
///   census above: a lambda-heavy strict run produced *zero* `generated-lambda`
///   rows for its real lambdas and one for the fabrication.
///
/// So the old comment's symmetry argument — "neither path may report the same
/// class differently depending on whether it happened to be fabricated" —
/// inverted the question. The two paths are not looking at the same class.
/// Having real bytes or not is *precisely* what separates a generated class
/// from a stand-in, and asking the name instead is what let a stand-in answer.
///
/// ## What survives here, and why these three are exact identities
///
/// The three `VmInternal` arms above are not name *shapes*: two are exact class
/// names in packages no other party may define into, and the third is this VM's
/// own reserved `CratonVM$` prefix. Each carries its own "yes, this binds by
/// name" justification, and each exists as the second half of a pairing whose
/// authoritative half is an `ensure_vm_internal_class` pre-mint at the mint
/// site. **Anything added beside them must be able to make the same statement —
/// a name nothing but this VM can ever mint — and must not be a shape a caller
/// could fall into.** That is the rule the three deleted arms broke.
pub(in crate::class_manager) fn fabricated_origin_for_name(name: &str) -> ClassOrigin {
    if is_vm_proxy_supertype_name(name) {
        return ClassOrigin::VmInternal;
    }
    if is_vm_annotation_carrier_name(name) {
        return ClassOrigin::VmInternal;
    }
    if is_vm_reserved_namespace_name(name) {
        return ClassOrigin::VmInternal;
    }
    // No generated-name-family arms. See the section above: a class that gets
    // here has no bytes on any classpath entry, in any loader, so whatever its
    // name looks like it is a fabricated stand-in and the policy must see it as
    // one. `is_generated_lambda_name` / `is_generated_proxy_name` /
    // `is_reflection_accessor_name` remain live — on the REAL-BYTES path, in
    // `classify_defined_origin`, which is where they were always correct.
    ClassOrigin::compatibility_stub(ENSURE_SYNTHETIC_STUB_REASON)
}

pub(in crate::class_manager) fn is_jdk_class(name: &str) -> bool {
    name.starts_with("java/")
        || name.starts_with("javax/")
        || name.starts_with("sun/")
        || name.starts_with("jdk/")
        || name.starts_with("com/sun/")
        || name.starts_with("[") // array type descriptors like [I, [Ljava/lang/String;
}

/// Create the field declarations for well-known JDK stub classes.
///
/// Most stubs have no fields. Special cases provide the fields that native
/// implementations expect, so that `new` + `<init>` works correctly.
/// How many INSTANCE fields a bytecode `new` of the synthesized class `name`
/// produces.
///
/// The gate `t9c_synthetic_field_tables_cover_their_factories`
/// (`vm/tests/tier1_tests.rs`) compares this against every literal
/// `alloc_concurrent_synthetic(ctx, "name", n)` site in the tree. The two must
/// agree, because a native written against the factory shape that writes past
/// the end of a `new`-created object has its write SILENTLY DISCARDED by
/// `set_field` â five separate bugs of exactly that shape were found in two
/// days. Calling the real function rather than parsing its source is what makes
/// the gate exact: the arms use several different construction styles, and no
/// regex over them stays honest for long.
pub fn synthetic_stub_instance_field_count(name: &str) -> usize {
    synthetic_stub_fields(name)
        .iter()
        .filter(|f| !f.access_flags.contains(FieldAccessFlags::STATIC))
        .count()
}

/// The synthetic slot extent of `name`: its own fabricated instance fields
/// PLUS every ancestor's, down the `jdk_superclass` chain.
///
/// This is the number a bytecode `new` of the stub actually sizes to, and so
/// the number an absolute slot index has to fit inside.
/// [`synthetic_stub_instance_field_count`] is the class's OWN contribution and
/// is the wrong thing to compare an absolute index against: `LinkedHashMap`
/// keeps `LHM_FIELD_HEAD = 3` and `LHM_FIELD_TAIL = 4`, which are its own two
/// fields sitting after `HashMap`'s three — an extent of 5 from an own-count
/// of 2. Comparing 2 against 5 reads as a three-slot shortfall that does not
/// exist, and the way to make that comparison pass is to over-declare the
/// table, which is how `LinkedHashMap` came to claim five own fields for two.
///
/// The chain is `jdk_superclass`, whose default arm is `java/lang/Object`, so
/// the walk always terminates; a class with no arm contributes zero.
#[must_use]
pub fn synthetic_stub_total_field_count(name: &str) -> usize {
    let mut total = 0usize;
    let mut cur = name;
    loop {
        total += synthetic_stub_instance_field_count(cur);
        if cur == "java/lang/Object" {
            break;
        }
        let parent = jdk_superclass(cur);
        if parent == cur {
            break;
        }
        cur = parent;
    }
    total
}

/// The classes whose synthetic slot floor describes the SYNTHETIC layout ONLY,
/// with the real instance-field extent each one has when it is defined from
/// real class-file bytes.
///
/// # Why a floor can be mode-specific at all
///
/// [`synthetic_stub_fields`] is one number consulted in two places that mean
/// two different things:
///
/// * **Fabricating a stub** (`synthesize_stub_class`) — the table IS the
///   layout. The natives that serve the class index it from 0 and nothing else
///   declares those slots, so the count is a definition, not a floor.
/// * **Defining a class from real bytes** (`define_class_with_options`, and the
///   stub-upgrade path) — the table is a FLOOR, padding the real layout up to
///   the width native code was written against. `java.net.InetSocketAddress`
///   is the case that needs it: it declares one instance field (`holder`) and
///   its native `<init>` writes raw synthetic indices on the REAL class, which
///   an unpadded object has no room for. An out-of-range `set_field` is
///   DROPPED, not raised, so removing that floor fails in silence.
///
/// For the classes below the second reading is false: in real-JDK mode nothing
/// writes a raw absolute slot past what the class itself declares, because the
/// natives that serve them resolve by NAME (`hs_map_slot`,
/// `props_defaults_slot`, `receiver_table_slot`) or are shadowed out by the
/// class's own bytecode. Applying the floor to them costs the whole object:
/// `ClassStore::build_compact_layout` refuses a compact layout to any class
/// with a padded slot — a padded slot has no descriptor, so its oop-map entry
/// would be a guess — and the class falls back to the legacy uniform 16-byte
/// tagged cell for EVERY slot. Padding by one costs 2x-4.5x.
///
/// # The number beside each name
///
/// It is the instance fields the REAL chain declares, measured on JDK 25
/// Temurin `25.0.3+9` with `CRATONVM_DBG_LAYOUT=1`
/// (`declared_instance_fields=`). It is not used to size anything — the real
/// class file does that — it is what
/// [`floor_exempt_real_extent_disagreement`] compares the loaded class against
/// so a JDK whose shape has moved is reported rather than silently exempted,
/// and what the `t9d_floor_exempt_classes_have_no_oversized_factories` gate
/// (`vm/tests/tier1_tests.rs`) compares every literal
/// `alloc_concurrent_synthetic(ctx, "name", n)` site against.
///
/// The third number is the real SUPERCLASS's extent on the same JDK. It sizes
/// nothing either: it is what lets the gate compute the floor the define path
/// would REALLY apply ([`synthetic_floor_over_real_parent`]: the loaded
/// parent's total plus the stub's own fields), not the stub's total in its own
/// fabricated hierarchy ([`synthetic_stub_total_field_count`]). The two differ
/// exactly where the real parent is wider than the stub's — `StringBuilder`
/// (stub parent `Object`, real parent `AbstractStringBuilder` with four) and
/// `ArrayList$ListItr` (stub parent `Object`, real parent `Itr` with four) —
/// and only the first one says whether an exemption pads anything away
/// (`docs/feature-designs/by-name-field-reads.md`,
/// FS16-2).
///
/// # Adding a class here
///
/// The screen is mechanical: a class may be exempt **iff** no native writes a
/// raw absolute slot at or past its real extent on a receiver of that class in
/// real-JDK mode. Both halves of that population are greppable — literal
/// factories (which the gate reads) and raw `ctx.set_field(obj, N, ..)` on a
/// receiver the caller did not allocate. Converting the writer to
/// `set_field_by_name`, or to the class's own `<init>`, is what clears a class;
/// lowering the number without doing that is the silent failure above.
///
/// `java/util/ArrayDeque` is deliberately NOT here: its fourth slot became
/// dead when `ad_state` started deriving the count from `head`/`tail`, so its
/// table was simply narrowed to the three the real class declares and the
/// class needs no exemption in either mode.
pub const FLOOR_EXEMPT_CLASSES: &[(&str, usize, usize)] = &[
    // One real field, `map` (`Ljava/util/HashMap;`). `hs_map_slot` answers
    // absolute 0 for it in BOTH modes, which is the only index the
    // `native_hs_*` surface uses. The synthetic table keeps three because
    // `wildfly_security::count_carrying_hash_set` writes a count at slot 1 —
    // under an explicit `is_class_synthetic_stub` gate, so that shape is
    // reachable only where it is the layout.
    ("java/util/HashSet", 1, 0),
    // Declares no instance fields of its own; it is `HashSet`'s one field,
    // inherited, and it was padded for exactly the same reason.
    ("java/util/LinkedHashSet", 1, 1),
    // One real field, `al` (`Ljava/util/concurrent/CopyOnWriteArrayList;`).
    // `hs_map_slot` names it explicitly beside `HashSet` and answers absolute
    // 0; the synthetic table's second slot mirrors `CopyOnWriteArrayList`'s
    // `(data, size)` and is written only on the fabricated stub.
    ("java/util/concurrent/CopyOnWriteArraySet", 1, 0),
    // Two real fields, `head` and `tail` (both `Node`). The four-slot table is
    // `native_lbq_*`'s `(head, tail, size, capacity)`, which serves the
    // fabricated stub; a real one runs its own lock-free bytecode, and a fresh
    // instance has `head == tail == new Node<>()` built by it.
    ("java/util/concurrent/ConcurrentLinkedQueue", 2, 0),
    ("java/util/concurrent/ConcurrentLinkedDeque", 2, 0),
    // Ten across the chain: `Hashtable`'s eight plus `Properties`' two. The
    // sixteen-slot table is the fabricated model, and every native that serves
    // a real `Properties` resolves its slot on the RECEIVER —
    // `props_defaults_slot` by name with the model slot as a fallback,
    // `publish_map_table` through `receiver_table_slot`. The one factory that
    // asks for sixteen (`system_properties_object`) writes NO slot at all: the
    // singleton's entries live in `properties_sidetable`'s side table.
    ("java/util/Properties", 10, 8),
    // Four real fields: `cursor`, `lastRet`, `expectedModCount` (all `int`)
    // and `this$0` (`Ljava/util/ArrayList;`). The five-slot table is the
    // fabricated stub's, and padding the real class by one cost it the compact
    // layout, so the JIT's CP field resolver had no compact slot for any of its
    // fields (`docs/feature-designs/by-name-field-reads.md`).
    //
    // Screened in JIT round 16 wave 8 (lane fieldsite). In a default build every
    // writer on a real receiver resolves its slot BY NAME:
    // `native-collections` `alloc_arraylist_iterator_as` / `al_itr_slots`
    // (`max(cursor, this$0) + 1`, so 4) and `native-builtins`
    // `native_arraylist_list_itr_slots`. The one literal factory
    // (`LinkedBlockingQueue.iterator`, 3 slots) is `synthetic-jdk` only and
    // asks for less than the real extent. `NativeContext::set_field` coerces a
    // mistyped raw write to the slot's descriptor in either layout, so
    // compactness changes no native's result.
    //
    // Behind `CRATONVM_ARRAYLIST_ITR_COMPACT_LAYOUT` (default ON since wave 10,
    // `=0` restores the padded layout); see [`floor_is_synthetic_only`].
    (ARRAYLIST_ITR, 4, 0),
    // Five real fields: `Itr`'s four, inherited at the same indices, plus its
    // OWN `this$0` (`Ljava/util/ArrayList;`) at 4 (javap -p on Temurin
    // 25.0.3+9). The stub table repeats all five on a stub whose
    // `jdk_superclass` is `Object`, so the define-time floor stacked them on
    // top of the real `Itr`: nine (ten with `Itr` padded), never compact.
    //
    // Screened in JIT round 16 wave 11 (lane overpad). Every writer on a real
    // receiver resolves its slot BY NAME: `native-builtins`
    // `native_arraylist_list_itr_slots` (`Itr.cursor` / `lastRet` /
    // `expectedModCount` / `this$0` and `ListItr.this$0`; the literal
    // fallbacks 1 / 2 / 0 are taken only when the class model cannot answer,
    // and are below the extent anyway), every write through
    // `set_field_if_present` or one of those resolved slots; the
    // `Iterator.remove` route (`native_al_itr_remove`) reads `al_itr_slots`,
    // by name, and `al_itr_alt_base` refuses the class (not a
    // `VALUES_ITR_CARRIERS` carrier). The one literal factory,
    // `native_arraylist_list_iterator`'s `try_alloc_concurrent_synthetic(..,
    // 5)`, asks for exactly the real extent. Dropping the padding moves no
    // declared index: padding is appended after the class's own fields.
    //
    // Behind `CRATONVM_ARRAYLIST_LISTITR_COMPACT_LAYOUT` (default ON since
    // 2026-11-12: `ListItr extends Itr`, and a compact `Itr` over a legacy
    // `ListItr` broke compiled `Itr` field reads), and
    // only together with the `Itr` exemption: a padded `Itr` leaves an
    // undescribed slot inside `ListItr` that `build_compact_layout` refuses
    // whatever this entry says.
    (ARRAYLIST_LISTITR, 5, 4),
    // Four real fields, ALL inherited from `AbstractStringBuilder`: `value`
    // (`[B`), `coder` (`B`), `maybeLatin1` (`Z`), `count` (`I`); the class
    // declares none (javap -p on Temurin 25.0.3+9). The stub's two
    // (`char[]` payload, count) sit on a stub whose `jdk_superclass` is
    // `Object`, so the floor stacked them on top of the real parent: six.
    //
    // Screened in JIT round 16 wave 11 (lane overpad), `native-builtins`
    // `lang_string.rs` (every `native_sb_*`, which `intrinsics/stringbuilder.rs`
    // forwards to unchanged): raw absolute slots on a builder are only `value`
    // at 0 (`sb_view`, `sb_value_units`, `sb_alloc_payload`, `sb_store_units`)
    // and the self-checking `coder` guess at 1 (`sb_view`); `count`, `coder`
    // and `toStringCache` resolve BY NAME (`sb_field_slot` / `sb_class_slot` /
    // `sb_count_slot`), each bounded by `object_num_fields`. The literal slot
    // 1 / 2 fallbacks in `sb_state` / `sb_set_count_resolved` /
    // `sb_raw_count_heuristic` run only where the class model cannot answer
    // (the unit-test mock, the two-slot synthetic object) and stay below the
    // extent. No native allocates a builder at a literal width: the three
    // native mints (`apps_h2`, `lang_string`, `native-builtins` `lib.rs`) all go
    // through `new_object_initialized(.., "()V")`. The JIT's call-site
    // intrinsics (`jit` `StringBuilderFieldLayout`) carry a compact and a
    // legacy address per field and dispatch per object on `GC_FLAG_COMPACT`,
    // exactly as the `String` twin does.
    //
    // Behind `CRATONVM_STRING_BUILDER_COMPACT_LAYOUT` (default OFF).
    (STRING_BUILDER, 4, 4),
    // Five real fields: `AbstractStringBuilder`'s four plus its own
    // `toStringCache` (`Ljava/lang/String;`) at 4. Same natives and same
    // screen as `StringBuilder` above (the JIT intrinsics guard on
    // `StringBuilder`'s exact class id and never see one); `toStringCache` is
    // written only through `sb_class_slot(.., "toStringCache")`, bounded by
    // `object_num_fields`. Same switch.
    (STRING_BUFFER, 5, 4),
    // Two real fields, `volatile Object result` and `volatile Completion stack`
    // (javap -p on Temurin 25.0.3+9; real parent `Object`). The four-slot
    // table (`result, done, source, handler`) is the fabricated model the
    // `CompletableFuture` Bridges in `native-builtins` (`phases_late/concurrent.rs`,
    // `util_concurrent_ext.rs`) write by absolute index; padding the real
    // class to it cost every future the compact layout, so neither JIT tier
    // could inline `RESULT.setRelease` / `STACK.compareAndSet` (no compact
    // displacement for `vh_const_field_disp`) and every `new` took the
    // post-init calls (`docs/feature-designs/class-resolution-and-caching.md`,
    // wave 7 and wave 8).
    //
    // Screened in JIT round 23 wave 8 (lane W8-L2). Every literal factory that
    // asks for more than two slots (`phases_late/concurrent.rs`' three-slot
    // model, `util_concurrent_ext.rs`' four-slot one) now goes through
    // `util_concurrent_ext::try_alloc_completable_future_model`, which asks
    // for the fabricated width only under `ctx.is_class_synthetic_stub(..)`
    // and for the real two otherwise. Those factories are `Bridge`s on the
    // class itself, so over a real JDK the class's own bytecode is
    // authoritative and they are not dispatched in either mode. The width-2
    // producers reachable over a real JDK (`p58_new_cf`, `http2`
    // `completed_future_of`, `native-collections` `CF_NUM_FIELDS`) write
    // only slots 0 and 1, which keep their indices: padding is appended after
    // the class's own fields, and `NativeContext::set_field` coerces a
    // mistyped raw write to the slot's descriptor in either layout.
    //
    // Behind `CRATONVM_COMPLETABLE_FUTURE_COMPACT_LAYOUT` (default ON, `=0`
    // restores the padded layout).
    (COMPLETABLE_FUTURE, 2, 0),
];

/// `java/util/ArrayList$Itr`, a [`FLOOR_EXEMPT_CLASSES`] entry behind a
/// switch.
pub(in crate::class_manager) const ARRAYLIST_ITR: &str = "java/util/ArrayList$Itr";
/// `java/util/ArrayList$ListItr`, behind
/// `CRATONVM_ARRAYLIST_LISTITR_COMPACT_LAYOUT` (and the `Itr` switch).
pub(in crate::class_manager) const ARRAYLIST_LISTITR: &str = "java/util/ArrayList$ListItr";
/// `java/lang/StringBuilder` and `java/lang/StringBuffer`, behind
/// `CRATONVM_STRING_BUILDER_COMPACT_LAYOUT`.
pub(in crate::class_manager) const STRING_BUILDER: &str = "java/lang/StringBuilder";
pub(in crate::class_manager) const STRING_BUFFER: &str = "java/lang/StringBuffer";
/// `java/util/concurrent/CompletableFuture`, behind
/// `CRATONVM_COMPLETABLE_FUTURE_COMPACT_LAYOUT`.
pub(in crate::class_manager) const COMPLETABLE_FUTURE: &str = "java/util/concurrent/CompletableFuture";



/// `CRATONVM_ARRAYLIST_ITR_COMPACT_LAYOUT` (default ON since JIT round 16
/// wave 10; `0` restores the padded layout): exempt the real
/// `java/util/ArrayList$Itr` from the synthetic slot floor, so it gets a
/// compact layout as a user class of the same shape does. Wave 8, lane
/// fieldsite. Read only when that one class is defined (once per VM), so it
/// needs no cache.
pub(in crate::class_manager) fn arraylist_itr_compact_layout_enabled() -> bool {
    true
}





/// Whether `name`'s synthetic slot floor must NOT be applied to a class being
/// defined from real class-file bytes. See [`FLOOR_EXEMPT_CLASSES`].
#[must_use]
pub fn floor_is_synthetic_only(name: &str) -> bool {
    if !FLOOR_EXEMPT_CLASSES.iter().any(|(c, _, _)| *c == name) {
        return false;
    }
    match name {
        ARRAYLIST_ITR => arraylist_itr_compact_layout_enabled(),
        ARRAYLIST_LISTITR => {
            arraylist_itr_compact_layout_enabled()
        }
        STRING_BUILDER | STRING_BUFFER => true,
        COMPLETABLE_FUTURE => true,
        _ => true,
    }
}

/// Whether `name` has a row in [`FLOOR_EXEMPT_CLASSES`], whatever its switch
/// says. Such a class is decided by its row and its row's switch alone; the
/// stub-extent screen ([`synthetic_floor_capped_at_stub_extent`]) never
/// overrides a row.
pub(in crate::class_manager) fn floor_exempt_row(name: &str) -> bool {
    FLOOR_EXEMPT_CLASSES.iter().any(|(c, _, _)| *c == name)
}

/// The root of the GC-owned `Reference` family. See
/// `ClassManager::real_chain_reaches_reference`.
pub(in crate::class_manager) const REFERENCE_ROOT: &str = "java/lang/ref/Reference";







/// The synthetic slot floor capped at the stub's own extent: the smaller of
/// [`synthetic_floor_over_real_parent`] (the define-time rule) and
/// [`synthetic_stub_total_field_count`] (the width a FABRICATED stub of `name`
/// has, so the largest absolute index a native written against the fabricated
/// layout can reach).
///
/// # Why this is the screen, not a list
///
/// The define-time floor is `real parent total + stub's own fields`. Where the
/// stub's `jdk_superclass` chain is narrower than the real parent (the stub of
/// `StringBuilder` has no `AbstractStringBuilder`, the stub of
/// `InputStreamReader` has no `Reader` fields, the stub of
/// `ArrayList$SubList` has no `AbstractList.modCount`), that sum counts the
/// parent twice. The slots it adds past the stub's extent can be reached only
/// by a native that indexes the class's OWN slot `k` at
/// `first_field_index + k` on the real class, and the wave-13 screen found no
/// such native on any class this cap moves (the defect page's wave-13 table).
/// Every absolute index below the stub's extent is kept, so a native written
/// against the fabricated layout loses nothing; a stub WIDER than the real
/// class (`Locale`, `Optional`, `Date`, `Logger`'s `_vm12`) keeps its full
/// floor, because there the floor is doing its job.
///
/// It only ever LOWERS a floor (`min`), so it cannot pad a class that the
/// define-time rule leaves unpadded.
#[must_use]
pub fn synthetic_floor_capped_at_stub_extent(name: &str, real_parent_total: usize) -> usize {
    synthetic_floor_over_real_parent(name, real_parent_total)
        .min(synthetic_stub_total_field_count(name))
}

/// The synthetic slot floor the define path applies to `name` when its loaded
/// superclass has `real_parent_total` instance slots: that total plus the
/// stub's OWN fabricated instance fields.
///
/// One function for the define path (`ClassManager::apply_synthetic_floor`)
/// and for the T9D gate, so the gate measures the rule the VM runs rather than
/// [`synthetic_stub_total_field_count`] — the stub's extent in its OWN
/// hierarchy, which is narrower wherever the real parent is wider than the
/// stub's (`StringBuilder`: 2 against a define-time 6; `ArrayList$ListItr`: 5
/// against 9). FS16-2.
#[must_use]
pub fn synthetic_floor_over_real_parent(name: &str, real_parent_total: usize) -> usize {
    real_parent_total + synthetic_stub_instance_field_count(name)
}

/// `Some((expected, actual))` when `name` is floor-exempt and the real class
/// just defined does not have the instance-field extent
/// [`FLOOR_EXEMPT_CLASSES`] recorded for it.
///
/// An exemption is only sound while the real layout is the one it was screened
/// against. A JDK that grows or drops a field on one of these classes moves
/// every absolute index the natives were checked at, and the failure mode is
/// the silent one — a dropped `set_field`. This turns that into a line on
/// stderr naming the class and both counts.
#[must_use]
pub(in crate::class_manager) fn floor_exempt_real_extent_disagreement(name: &str, declared: usize) -> Option<(usize, usize)> {
    FLOOR_EXEMPT_CLASSES
        .iter()
        .find(|(c, _, _)| *c == name)
        .map(|(_, expected, _)| (*expected, declared))
        .filter(|(expected, actual)| expected != actual)
}

/// CratonVM's fabricated slot **model** for `name` — the same table that sizes
/// a bytecode `new` of the stub and that pads a real class up to the count
/// native code was written against.
///
/// Exposed so the overlay detector can diff the model against the real layout
/// (`crate::shadow_layout`). Reading it is the only way to say what a
/// hand-numbered positional access *meant*; the value's type tag alone cannot,
/// which is the blind spot L4 closes.
#[must_use]
pub fn synthetic_stub_field_model(name: &str) -> Vec<cratonvm_reader::field::ClassFileField> {
    synthetic_stub_fields(name)
}

