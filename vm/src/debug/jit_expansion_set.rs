// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Which JDK classes' methods the JIT may expand at a call site WITHOUT a
//! record the scoped withdrawal reads -- step 1 of
//! `docs/feature-designs/jvmti-delivery-threading.md`
//! (interpreter round i6 wave 117, lane L1;
//! `CRATONVM_JDWP_JDK_BREAKPOINT_EXPANSION_SET`, default ON since the i6 wave-126 flip, `=0` off).
//!
//! Since wave 40 a breakpoint in ANY JDK class (`java/`, `javax/`, `jdk/`,
//! `sun/`, `com/sun/`) keeps every method of the VM interpreted
//! (`interpreter::jit_may_expand_a_method_of_without_a_record`): the JIT's
//! call-site intrinsics and IR expanders replace a JDK method at the call site
//! from fixed tables that no compile door is asked about, so a caller compiled
//! after the breakpoint could run the method inline and miss it. Those tables
//! are keyed by the SITE's class name, a string literal in the JIT's sources
//! (`try_resolve_*_intrinsic`, `try_ir_*_intrinsic`, the IR builder's
//! `try_*_intrinsic` arms, the single-pass arms, the mutator-side resolvers of
//! `jit_bridge.rs`). So a method can only be expanded when:
//!
//! * its class is such a literal class (or inside a literal package prefix),
//!   or a SUPERTYPE of one (a site naming the literal class whose method is
//!   inherited from it: `StringBuilder.append` -> `AbstractStringBuilder`);
//! * a site naming a SUBCLASS of a literal class that inherits its method
//!   (the IR unbox arm matches an `AtomicInteger` subclass as `AtomicInteger`)
//!   still expands the literal class's own method, which the first rule names;
//! * an interface site (`CharSequence.length`) expands only under a receiver
//!   guard, and every guard class is itself a literal.
//!
//! [`CLASSES`] is therefore the literal classes of those sources plus their
//! transitive supertypes, and [`PREFIXES`] the literal package prefixes:
//! computed on HotSpot JDK 25.0.3 (`Class.getSuperclass` /
//! `getInterfaces`, and the jrt image for the prefixes' members) from the
//! literals the test below extracts. A breakpoint in a JDK class outside both
//! (`java/util/TreeMap`, `java/text/...`, `sun/security/...`) takes the
//! per-method path of an application class's breakpoint.
//!
//! The test [`tests::every_jit_class_literal_is_covered`] re-extracts the
//! literals from the sources at test time and fails when the JIT names a
//! JDK class (or package) the set does not hold: a new intrinsic family is
//! then a red test, not a silently missed breakpoint. The whole-package
//! prefixes `java/`, `jdk/`, `sun/`, `javax/`, `com/sun/` are not expansion
//! keys today. Two JIT-side checks classify typecheck and virtual-thread
//! behavior; the vm-side checks cover linking and field sites. The test
//! reviews those scopes so a new broad prefix is noticed.
//!
//! Not covered: an expansion keyed on something other than a class-name
//! literal, or a JDK hierarchy that differs from JDK 25's. JDI runs of
//! `L1W40JdiBreakpointInJitIntrinsicCallee` and its `HashMap`-free variant
//! are still needed to validate this set end to end.

/// Package prefixes the JIT's sources name (every class inside expands as
/// far as this set is concerned).
pub(crate) const PREFIXES: &[&str] = &[
    "java/lang/invoke/",
    "java/lang/reflect/",
    "java/util/Collections$",
    "java/util/ImmutableCollections$",
    "java/util/jar/",
    "java/util/zip/",
    "jdk/internal/foreign/HeapMemorySegmentImpl$Of",
    "jdk/internal/foreign/layout/ValueLayouts$Of",
    "jdk/internal/loader/",
    "jdk/internal/reflect/",
    "sun/net/www/protocol/file/",
    "sun/net/www/protocol/jar/",
];

/// The JIT's literal JDK classes and their transitive supertypes, outside
/// [`PREFIXES`], sorted (byte order) for the binary search.
///
/// JIT round 22 wave 6 (lane review): `AtomicIntegerFieldUpdater` and its
/// `$AtomicIntegerFieldUpdaterImpl` (the `jit_bridge.rs` updater fast path),
/// and `jdk/internal/reflect/Direct{Method,Constructor}HandleAccessor` with
/// their supertypes `{Method,Constructor}AccessorImpl` and the
/// `{Method,Constructor}Accessor` interfaces (the round-21 reflective link
/// redirects of `jit/helpers.rs`) -- literals already on `dev` that this list
/// missed. The supertypes are JDK 25's by reading; confirm with the
/// `getSuperclass` / `getInterfaces` run the module doc describes.
pub(crate) const CLASSES: &[&str] = &[
    "com/sun/tools/javac/code/Types",
    "java/io/BufferedInputStream",
    "java/io/Closeable",
    "java/io/FilterInputStream",
    "java/io/FilterOutputStream",
    "java/io/Flushable",
    "java/io/IOException",
    "java/io/InputStream",
    "java/io/OutputStream",
    "java/io/Serializable",
    "java/io/UncheckedIOException",
    "java/lang/AbstractMethodError",
    "java/lang/AbstractStringBuilder",
    "java/lang/Appendable",
    "java/lang/ArithmeticException",
    "java/lang/ArrayIndexOutOfBoundsException",
    "java/lang/AutoCloseable",
    "java/lang/Boolean",
    "java/lang/Byte",
    "java/lang/CharSequence",
    "java/lang/Character",
    "java/lang/CharacterData",
    "java/lang/CharacterDataLatin1",
    "java/lang/Class",
    "java/lang/ClassCastException",
    "java/lang/ClassLoader",
    "java/lang/ClassValue",
    "java/lang/ClassValue$Entry",
    "java/lang/CloneNotSupportedException",
    "java/lang/Cloneable",
    "java/lang/Comparable",
    "java/lang/Double",
    "java/lang/Enum",
    "java/lang/Error",
    "java/lang/Exception",
    "java/lang/Float",
    "java/lang/IllegalAccessError",
    "java/lang/IllegalArgumentException",
    "java/lang/IllegalMonitorStateException",
    "java/lang/IllegalStateException",
    "java/lang/IncompatibleClassChangeError",
    "java/lang/IndexOutOfBoundsException",
    "java/lang/InheritableThreadLocal",
    "java/lang/Integer",
    "java/lang/InternalError",
    "java/lang/InterruptedException",
    "java/lang/Iterable",
    "java/lang/LinkageError",
    "java/lang/Long",
    "java/lang/Math",
    "java/lang/Mathx",
    "java/lang/NegativeArraySizeException",
    "java/lang/NoClassDefFoundError",
    "java/lang/NoSuchMethodError",
    "java/lang/NullPointerException",
    "java/lang/Number",
    "java/lang/NumberFormatException",
    "java/lang/Object",
    "java/lang/Objectx",
    "java/lang/OutOfMemoryError",
    "java/lang/Readable",
    "java/lang/Record",
    "java/lang/ReflectiveOperationException",
    "java/lang/Runnable",
    "java/lang/RuntimeException",
    "java/lang/Short",
    "java/lang/StackOverflowError",
    "java/lang/StrictMath",
    "java/lang/String",
    "java/lang/StringBuffer",
    "java/lang/StringBuilder",
    "java/lang/StringCoding",
    "java/lang/StringIndexOutOfBoundsException",
    "java/lang/StringLatin1",
    "java/lang/StringUTF16",
    "java/lang/Strings",
    "java/lang/System",
    "java/lang/Thread",
    "java/lang/ThreadLocal",
    "java/lang/ThreadLocal$SuppliedThreadLocal",
    "java/lang/ThreadLocal$ThreadLocalMap",
    "java/lang/ThreadLocal$ThreadLocalMap$Entry",
    "java/lang/Throwable",
    "java/lang/UnsupportedOperationException",
    "java/lang/VirtualMachineError",
    "java/lang/annotation/Annotation",
    "java/lang/annotation/AnnotationProxy",
    "java/lang/constant/Constable",
    "java/lang/constant/ConstantDesc",
    "java/lang/constant/DynamicConstantDesc",
    "java/lang/foreign/AddressLayout",
    "java/lang/foreign/DowncallHandle",
    "java/lang/foreign/GroupLayout",
    "java/lang/foreign/MemoryLayout",
    "java/lang/foreign/MemorySegment",
    "java/lang/foreign/MemorySegment$Scope",
    "java/lang/foreign/SegmentAllocator",
    "java/lang/foreign/StructLayout",
    "java/lang/foreign/ValueLayout",
    "java/lang/foreign/ValueLayout$OfBoolean",
    "java/lang/foreign/ValueLayout$OfInt",
    "java/lang/foreign/ValueLayout$OfLong",
    "java/lang/instrument/ClassFileTransformer",
    "java/lang/module/ModuleReader",
    "java/lang/ref/PhantomReference",
    "java/lang/ref/Reference",
    "java/lang/ref/SoftReference",
    "java/lang/ref/WeakReference",
    "java/lang/reflect/AnnotatedElement",
    "java/lang/reflect/Array",
    "java/lang/reflect/GenericDeclaration",
    "java/lang/reflect/Member",
    "java/lang/reflect/Proxy",
    "java/lang/reflect/Proxy$Instance",
    "java/lang/reflect/Type",
    "java/math/MutableBigInteger",
    "java/net/JarURLConnection",
    "java/net/URI",
    "java/net/URL",
    "java/net/URLClassLoader",
    "java/net/URLConnection",
    "java/net/URLStreamHandler",
    "java/nio/Buffer",
    "java/nio/BufferOverflowException",
    "java/nio/BufferUnderflowException",
    "java/nio/ByteBuffer",
    "java/nio/ByteBufferAsIntBufferB",
    "java/nio/ByteOrder",
    "java/nio/CharBuffer",
    "java/nio/DirectByteBuffer",
    "java/nio/DirectByteBufferR",
    "java/nio/HeapByteBuffer",
    "java/nio/HeapByteBufferR",
    "java/nio/HeapCharBuffer",
    "java/nio/IntBuffer",
    "java/nio/MappedByteBuffer",
    "java/nio/charset/CharacterCodingException",
    "java/nio/charset/Charset",
    "java/nio/charset/CharsetDecoder",
    "java/nio/charset/CoderResult",
    "java/security/CodeSource",
    "java/security/MessageDigest",
    "java/security/MessageDigestSpi",
    "java/security/SecureClassLoader",
    "java/util/AbstractCollection",
    "java/util/AbstractList",
    "java/util/AbstractMap",
    "java/util/AbstractSequentialList",
    "java/util/ArrayList",
    "java/util/Arrays",
    "java/util/Collection",
    "java/util/Comparator",
    "java/util/ConcurrentModificationException",
    "java/util/Deque",
    "java/util/Dictionary",
    "java/util/Enumeration",
    "java/util/HashMap",
    "java/util/HashMap$Node",
    "java/util/Hashtable",
    "java/util/Iterator",
    "java/util/LinkedHashMap",
    "java/util/LinkedList",
    "java/util/List",
    "java/util/Locale",
    "java/util/Map",
    "java/util/Map$Entry",
    "java/util/NoSuchElementException",
    "java/util/Objects",
    "java/util/Properties",
    "java/util/Queue",
    "java/util/RandomAccess",
    "java/util/SequencedCollection",
    "java/util/SequencedMap",
    "java/util/Spliterator",
    "java/util/Spliterators$AbstractSpliterator",
    "java/util/TimeZone",
    "java/util/WeakHashMap$Entry",
    "java/util/concurrent/CompletableFuture",
    "java/util/concurrent/CompletableFuture$AsynchronousCompletionTask",
    "java/util/concurrent/CompletableFuture$Completion",
    "java/util/concurrent/CompletionStage",
    "java/util/concurrent/ConcurrentHashMap",
    "java/util/concurrent/ConcurrentMap",
    "java/util/concurrent/CountedCompleter",
    "java/util/concurrent/ForkJoinTask",
    "java/util/concurrent/Future",
    "java/util/concurrent/RecursiveAction",
    "java/util/concurrent/RecursiveTask",
    "java/util/concurrent/atomic/AtomicBoolean",
    "java/util/concurrent/atomic/AtomicInteger",
    "java/util/concurrent/atomic/AtomicIntegerFieldUpdater",
    "java/util/concurrent/atomic/AtomicIntegerFieldUpdater$AtomicIntegerFieldUpdaterImpl",
    "java/util/concurrent/atomic/AtomicLong",
    "java/util/concurrent/locks/AbstractOwnableSynchronizer",
    "java/util/concurrent/locks/Lock",
    "java/util/concurrent/locks/ReentrantLock",
    "java/util/function/BiFunction",
    "java/util/function/Consumer",
    "java/util/function/Function",
    "java/util/function/IntFunction",
    "java/util/function/IntSupplier",
    "java/util/function/Supplier",
    "java/util/function/ToDoubleFunction",
    "java/util/function/ToIntFunction",
    "java/util/function/ToLongFunction",
    "java/util/regex/ASCII",
    "java/util/regex/MatchResult",
    "java/util/regex/Matcher",
    "java/util/regex/Pattern$BmpCharPropertyGreedy",
    "java/util/regex/Pattern$CharPredicate",
    "java/util/regex/Pattern$CharPropertyGreedy",
    "java/util/regex/Pattern$Node",
    "java/util/zipper/X",
    "jdk/internal/access/JavaLangInvokeAccess",
    "jdk/internal/access/JavaUtilJarAccess",
    "jdk/internal/access/JavaUtilZipFileAccess",
    "jdk/internal/foreign/AbstractMemorySegmentImpl",
    "jdk/internal/foreign/ConfinedSession",
    "jdk/internal/foreign/GlobalSession",
    "jdk/internal/foreign/GlobalSession$HeapSession",
    "jdk/internal/foreign/HeapMemorySegmentImpl",
    "jdk/internal/foreign/ImplicitSession",
    "jdk/internal/foreign/MappedMemorySegmentImpl",
    "jdk/internal/foreign/MemorySessionImpl",
    "jdk/internal/foreign/NativeMemorySegmentImpl",
    "jdk/internal/foreign/SharedSession",
    "jdk/internal/foreign/layout/AbstractGroupLayout",
    "jdk/internal/foreign/layout/AbstractLayout",
    "jdk/internal/foreign/layout/StructLayoutImpl",
    "jdk/internal/foreign/layout/ValueLayouts$AbstractValueLayout",
    "jdk/internal/misc/CarrierThreadLocal",
    "jdk/internal/misc/ScopedMemoryAccess",
    "jdk/internal/misc/TerminatingThreadLocal",
    "jdk/internal/misc/Unsafe",
    "jdk/internal/reflect/ConstructorAccessor",
    "jdk/internal/reflect/ConstructorAccessorImpl",
    "jdk/internal/reflect/DirectConstructorHandleAccessor",
    "jdk/internal/reflect/DirectMethodHandleAccessor",
    "jdk/internal/reflect/MethodAccessor",
    "jdk/internal/reflect/MethodAccessorImpl",
    "jdk/internal/util/ArraysSupport",
    "jdk/internal/util/ByteArray",
    "jdk/internal/util/DecimalDigits",
    "jdk/internal/util/Preconditions",
    "jdk/proxy12/$Proxy0",
    "sun/misc/Unsafe",
    "sun/net/www/ParseUtil",
    "sun/net/www/ParseUtils",
    "sun/net/www/URLConnection",
    "sun/nio/ch/DirectBuffer",
    "sun/nio/cs/HistoricallyNamedCharset",
    "sun/nio/cs/ISO_8859_1",
    "sun/nio/cs/ISO_8859_1$Decoder",
];

/// May the JIT expand a method of the JDK class `class_name` (internal form)
/// at a call site without a record ([`CLASSES`], [`PREFIXES`])? Pure.
pub(crate) fn may_expand(class_name: &str) -> bool {
    PREFIXES.iter().any(|p| class_name.starts_with(p)) || CLASSES.binary_search(&class_name).is_ok()
}

#[cfg(test)]
mod tests;
