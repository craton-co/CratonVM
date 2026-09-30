// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! RETIRED-SHADOW ANCESTOR GATE (round 13 wave 13, lane shadow5; proposal
//! SH4-1 of `docs/internal/jit-proposals/jit-r13-shadow4-proposals-RETIRED-20260929.md`).
//!
//! # The species
//!
//! A retired triple `(C, m, d)` is re-tagged `SyntheticStub` and refused
//! under `--jdk-only`, so the lookup keyed on `C` finds nothing. It does not
//! follow that `C`'s bytecode runs. Two doors then walk the SUPERCLASS chain
//! when `C` does not itself declare `(m, d)` -- `invoke_or_native`
//! (`vm/src/vm/vm_exec.rs`, from the call site's class) and the virtual-invoke
//! cache populate (`dispatch_virtual.rs` `populate_virtual_invoke_cache`, the
//! "FJP fix" walk, from the RECEIVER's class, so interface call sites too):
//! at each ancestor `S` they take a registered native if there is one
//! (`InvokeOrNativeParent` / `InvokeOrNativeParentShadow`), and stop at the
//! first ancestor that declares the method. So a live `Bridge` `(S, m, d)`
//! above a retired, non-declaring `C` hands every `C` receiver straight back
//! to a native -- the table says retired, the run says native.
//!
//! That is what lane T's 2026-09-11 pull-back measured without knowing it: a
//! one-class `ParseException` arm printed the captured trace because the
//! INHERITED native printer on `Exception`/`Throwable` answered, and the
//! result was read as a defect of the real bytecode (lane shadow4, wave 12).
//! On its first reading this gate found the same species on a second family
//! (`VirtualMachineError.getMessage`/`toString` above lane T's retired
//! `InternalError`/`OutOfMemoryError`/`StackOverflowError` rows), now retired
//! in `native-api/src/retired_shadow.rs`.
//!
//! # What it reads
//!
//! * The retired population: the COMPATIBLE boot registry's rows that
//!   `triple_is_retired_shadow` claims (constructors excluded: the walk skips
//!   `<init>`, constructors are not inherited).
//! * The live `Bridge` population: the `--jdk-only` boot registry's `Bridge`
//!   rows, i.e. what survives the strict refusal.
//! * Two JDK 25 image facts no registry carries, stated below as tables:
//!   the superclass of every class a walk can visit ([`JDK25_SUPERCLASS`]) and
//!   the declarations that stop a walk ([`DECLARED_ON_JDK25`]). They were
//!   written from the JDK 25 sources; `javap -p <class>` confirms any row.
//!
//! # What it cannot see, stated
//!
//! * Interfaces. The walk is the superclass chain, as `invoke_or_native`'s
//!   is; a live `Bridge` on a superinterface is not modelled.
//! * `Intrinsic` ancestors. A reviewed intrinsic above a retired row is a
//!   decision, not this species.
//! * The reverse case: the virtual-invoke cache's exact lookup on the
//!   receiver's class reaches a live SUBCLASS row whose declarer's row is
//!   retired. That is why `StringBuilder`'s five inherited rows were retired
//!   in the same wave; this gate does not enumerate it.
//!
//! # The dispatch mask (round 14 wave 2, lane shadow)
//!
//! Since round 14 every ancestor walk asks
//! `NativeMethodRegistry::retired_row_masks_ancestor_bridges` for each class it
//! passes, and ignores a `Bridge` found above a retired row
//! (`vm/src/runtime/interpreter/native_override.rs`
//! `retired_row_masks_ancestor_bridge`; kill switch
//! `CRATONVM_RETIRED_SHADOW_MASKS_ANCESTOR_BRIDGES=0`). A pair the walk below
//! finds is therefore a half-retirement only if the strict registry does NOT
//! mask it; the masked ones are printed as a census, and
//! [`KNOWN_HALF_RETIREMENTS`] went from 19 to none. Expected on 25/linux after
//! the wave: 18 masked (the 16 `AbstractCollection.toArray` views,
//! `CharBuffer.session` / `checkSession`); the 19th,
//! `ByteArrayOutputStream.write([B)` under `OutputStream`, left the population
//! when the same wave retired the `OutputStream.write([B)V` row itself.
//!
//! # When it fails
//!
//! A new line under "half-retired" is a retirement whose ancestor still
//! answers although the mask is on: the mask stopped covering it (read
//! `retired_row_masks_ancestor_bridges`). Retire the ancestor row with it
//! (the fix for `VirtualMachineError`), or record the pair in
//! [`KNOWN_HALF_RETIREMENTS`] with a page, never silently. With the kill
//! switch off in the environment the check is reported, not failed. A line under "unmapped" is a retired class
//! (or an ancestor of one) with no row in [`JDK25_SUPERCLASS`]: add its
//! superclass from `javap`. A declaration fact that is wrong in the
//! "declared" direction hides an offender, so add one only from `javap -p`.
//!
//! ```text
//! cargo test -p cratonvm-native-builtins --test r13_shadow5_retired_ancestor_gate -- --nocapture
//! ```

use std::collections::BTreeSet;

use cratonvm_native_api::retired_shadow::triple_is_retired_shadow;
use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
use cratonvm_types::compat::CompatibilityMode;

/// The one model of `vm_init`'s real-JDK boot path, shared with
/// `stub_ratchet.rs`, `duplicate_registration_gate.rs` and
/// `essential_wiring_ratchet.rs`.
#[path = "common/vm_init_boot_path.rs"]
mod boot_path;

type Triple = (String, String, String);

const OBJECT: &str = "java/lang/Object";

/// JDK 25 superclass of every retired class and of every class on its chain
/// up to `java/lang/Object` (an interface's class-file superclass is
/// `java/lang/Object`, which is what the VM's walk follows too).
const JDK25_SUPERCLASS: &[(&str, &str)] = &[
    ("java/beans/PropertyChangeEvent", "java/util/EventObject"),
    ("java/beans/PropertyChangeSupport", "java/lang/Object"),
    ("java/beans/VetoableChangeSupport", "java/lang/Object"),
    ("java/io/BufferedOutputStream", "java/io/FilterOutputStream"),
    ("java/io/ByteArrayInputStream", "java/io/InputStream"),
    ("java/io/ByteArrayOutputStream", "java/io/OutputStream"),
    ("java/io/DataInputStream", "java/io/FilterInputStream"),
    ("java/io/DataOutputStream", "java/io/FilterOutputStream"),
    ("java/io/EOFException", "java/io/IOException"),
    ("java/io/File", "java/lang/Object"),
    ("java/io/FileCleanable", "jdk/internal/ref/PhantomCleanable"),
    ("java/io/FileDescriptor", "java/lang/Object"),
    ("java/io/FileDescriptor$1", "java/lang/Object"),
    ("java/io/FileNotFoundException", "java/io/IOException"),
    ("java/io/FileOutputStream", "java/io/OutputStream"),
    ("java/io/FileSystem", "java/lang/Object"),
    ("java/io/FilterInputStream", "java/io/InputStream"),
    ("java/io/FilterOutputStream", "java/io/OutputStream"),
    ("java/io/IOException", "java/lang/Exception"),
    ("java/io/InputStream", "java/lang/Object"),
    ("java/io/LineNumberInputStream", "java/io/FilterInputStream"),
    ("java/io/NotSerializableException", "java/io/ObjectStreamException"),
    ("java/io/ObjectInputStream", "java/io/InputStream"),
    ("java/io/ObjectStreamClass$RecordSupport", "java/lang/Object"),
    ("java/io/ObjectStreamException", "java/io/IOException"),
    ("java/io/OutputStream", "java/lang/Object"),
    ("java/io/PrintStream", "java/io/FilterOutputStream"),
    ("java/io/PrintWriter", "java/io/Writer"),
    ("java/io/StringBufferInputStream", "java/io/InputStream"),
    ("java/io/UncheckedIOException", "java/lang/RuntimeException"),
    ("java/io/UnixFileSystem", "java/io/FileSystem"),
    ("java/io/UnsupportedEncodingException", "java/io/IOException"),
    ("java/io/WinNTFileSystem", "java/io/FileSystem"),
    ("java/io/Writer", "java/lang/Object"),
    ("java/lang/AbstractMethodError", "java/lang/IncompatibleClassChangeError"),
    ("java/lang/AbstractStringBuilder", "java/lang/Object"),
    ("java/lang/ArithmeticException", "java/lang/RuntimeException"),
    ("java/lang/ArrayIndexOutOfBoundsException", "java/lang/IndexOutOfBoundsException"),
    ("java/lang/AssertionError", "java/lang/Error"),
    ("java/lang/Character", "java/lang/Object"),
    ("java/lang/Class", "java/lang/Object"),
    ("java/lang/ClassCastException", "java/lang/RuntimeException"),
    ("java/lang/ClassLoader", "java/lang/Object"),
    ("java/lang/ClassNotFoundException", "java/lang/ReflectiveOperationException"),
    ("java/lang/CloneNotSupportedException", "java/lang/Exception"),
    ("java/lang/Enum", "java/lang/Object"),
    ("java/lang/Error", "java/lang/Throwable"),
    ("java/lang/Exception", "java/lang/Throwable"),
    ("java/lang/ExceptionInInitializerError", "java/lang/LinkageError"),
    ("java/lang/IllegalAccessError", "java/lang/IncompatibleClassChangeError"),
    ("java/lang/IllegalAccessException", "java/lang/ReflectiveOperationException"),
    ("java/lang/IllegalArgumentException", "java/lang/RuntimeException"),
    ("java/lang/IllegalStateException", "java/lang/RuntimeException"),
    ("java/lang/IllegalThreadStateException", "java/lang/IllegalArgumentException"),
    ("java/lang/IncompatibleClassChangeError", "java/lang/LinkageError"),
    ("java/lang/IndexOutOfBoundsException", "java/lang/RuntimeException"),
    ("java/lang/InstantiationException", "java/lang/ReflectiveOperationException"),
    ("java/lang/InternalError", "java/lang/VirtualMachineError"),
    ("java/lang/InterruptedException", "java/lang/Exception"),
    ("java/lang/LinkageError", "java/lang/Error"),
    ("java/lang/MatchException", "java/lang/RuntimeException"),
    ("java/lang/NamedPackage", "java/lang/Object"),
    ("java/lang/NegativeArraySizeException", "java/lang/RuntimeException"),
    ("java/lang/NoClassDefFoundError", "java/lang/LinkageError"),
    ("java/lang/NoSuchFieldError", "java/lang/IncompatibleClassChangeError"),
    ("java/lang/NoSuchFieldException", "java/lang/ReflectiveOperationException"),
    ("java/lang/NoSuchMethodError", "java/lang/IncompatibleClassChangeError"),
    ("java/lang/NoSuchMethodException", "java/lang/ReflectiveOperationException"),
    ("java/lang/NullPointerException", "java/lang/RuntimeException"),
    ("java/lang/Number", "java/lang/Object"),
    ("java/lang/NumberFormatException", "java/lang/IllegalArgumentException"),
    ("java/lang/OutOfMemoryError", "java/lang/VirtualMachineError"),
    ("java/lang/Package", "java/lang/NamedPackage"),
    ("java/lang/ReflectiveOperationException", "java/lang/Exception"),
    ("java/lang/RuntimeException", "java/lang/Exception"),
    ("java/lang/SecurityException", "java/lang/RuntimeException"),
    ("java/lang/StackOverflowError", "java/lang/VirtualMachineError"),
    ("java/lang/StringBuilder", "java/lang/AbstractStringBuilder"),
    ("java/lang/StringIndexOutOfBoundsException", "java/lang/IndexOutOfBoundsException"),
    ("java/lang/StringUTF16", "java/lang/Object"),
    ("java/lang/System$1", "java/lang/Object"),
    ("java/lang/System$2", "java/lang/Object"),
    ("java/lang/Thread$FieldHolder", "java/lang/Object"),
    ("java/lang/Thread$State", "java/lang/Enum"),
    ("java/lang/Throwable", "java/lang/Object"),
    ("java/lang/TypeNotPresentException", "java/lang/RuntimeException"),
    ("java/lang/UnsatisfiedLinkError", "java/lang/LinkageError"),
    ("java/lang/UnsupportedOperationException", "java/lang/RuntimeException"),
    ("java/lang/VerifyError", "java/lang/LinkageError"),
    ("java/lang/VirtualMachineError", "java/lang/Error"),
    ("java/lang/invoke/MethodType", "java/lang/Object"),
    ("java/lang/management/ManagementFactory", "java/lang/Object"),
    ("java/lang/management/MemoryUsage", "java/lang/Object"),
    ("java/lang/module/Configuration", "java/lang/Object"),
    ("java/lang/module/ModuleDescriptor", "java/lang/Object"),
    ("java/lang/module/ModuleDescriptor$Exports", "java/lang/Object"),
    ("java/lang/module/ModuleDescriptor$Opens", "java/lang/Object"),
    ("java/lang/module/ModuleDescriptor$Provides", "java/lang/Object"),
    ("java/lang/module/ModuleDescriptor$Requires", "java/lang/Object"),
    ("java/lang/module/ModuleDescriptor$Version", "java/lang/Object"),
    ("java/lang/module/ModuleFinder", "java/lang/Object"),
    ("java/lang/module/ModuleReference", "java/lang/Object"),
    ("java/lang/ref/PhantomReference", "java/lang/ref/Reference"),
    ("java/lang/ref/Reference", "java/lang/Object"),
    ("java/lang/reflect/AccessibleObject", "java/lang/Object"),
    // Round 14 wave 6 (lane compat6): `Array.newInstance`.
    ("java/lang/reflect/Array", "java/lang/Object"),
    ("java/lang/reflect/Constructor", "java/lang/reflect/Executable"),
    ("java/lang/reflect/Executable", "java/lang/reflect/AccessibleObject"),
    ("java/lang/reflect/Field", "java/lang/reflect/AccessibleObject"),
    ("java/lang/reflect/InaccessibleObjectException", "java/lang/RuntimeException"),
    ("java/lang/reflect/InvocationTargetException", "java/lang/ReflectiveOperationException"),
    ("java/lang/reflect/Method", "java/lang/reflect/Executable"),
    ("java/math/BigInteger", "java/lang/Number"),
    ("java/net/HttpURLConnection", "java/net/URLConnection"),
    ("java/net/MalformedURLException", "java/io/IOException"),
    ("java/net/ProxySelector", "java/lang/Object"),
    ("java/net/URL", "java/lang/Object"),
    ("java/net/URLClassLoader", "java/security/SecureClassLoader"),
    ("java/net/URLConnection", "java/lang/Object"),
    ("java/net/UnknownHostException", "java/io/IOException"),
    ("java/nio/Buffer", "java/lang/Object"),
    ("java/nio/Buffer$2", "java/lang/Object"),
    ("java/nio/ByteBuffer", "java/nio/Buffer"),
    ("java/nio/ByteBufferAsCharBufferB", "java/nio/CharBuffer"),
    ("java/nio/ByteBufferAsCharBufferL", "java/nio/CharBuffer"),
    ("java/nio/ByteBufferAsCharBufferRB", "java/nio/ByteBufferAsCharBufferB"),
    ("java/nio/ByteBufferAsCharBufferRL", "java/nio/ByteBufferAsCharBufferL"),
    ("java/nio/ByteOrder", "java/lang/Object"),
    ("java/nio/CharBuffer", "java/nio/Buffer"),
    ("java/nio/DirectByteBuffer", "java/nio/MappedByteBuffer"),
    ("java/nio/DoubleBuffer", "java/nio/Buffer"),
    ("java/nio/FloatBuffer", "java/nio/Buffer"),
    ("java/nio/HeapByteBuffer", "java/nio/ByteBuffer"),
    ("java/nio/HeapCharBuffer", "java/nio/CharBuffer"),
    ("java/nio/HeapCharBufferR", "java/nio/HeapCharBuffer"),
    ("java/nio/IntBuffer", "java/nio/Buffer"),
    ("java/nio/LongBuffer", "java/nio/Buffer"),
    ("java/nio/MappedByteBuffer", "java/nio/ByteBuffer"),
    ("java/nio/ShortBuffer", "java/nio/Buffer"),
    ("java/nio/StringCharBuffer", "java/nio/CharBuffer"),
    ("java/nio/channels/FileChannel", "java/nio/channels/spi/AbstractInterruptibleChannel"),
    ("java/nio/channels/spi/AbstractInterruptibleChannel", "java/lang/Object"),
    ("java/nio/charset/CoderResult", "java/lang/Object"),
    ("java/nio/file/FileStore", "java/lang/Object"),
    ("java/nio/file/FileSystem", "java/lang/Object"),
    ("java/nio/file/FileSystems", "java/lang/Object"),
    ("java/nio/file/FileVisitResult", "java/lang/Enum"),
    ("java/nio/file/Files", "java/lang/Object"),
    ("java/nio/file/Path", "java/lang/Object"),
    ("java/nio/file/Paths", "java/lang/Object"),
    ("java/nio/file/SimpleFileVisitor", "java/lang/Object"),
    ("java/nio/file/attribute/FileTime", "java/lang/Object"),
    ("java/nio/file/attribute/PosixFilePermission", "java/lang/Enum"),
    ("java/nio/file/attribute/PosixFilePermissions", "java/lang/Object"),
    ("java/nio/file/spi/FileSystemProvider", "java/lang/Object"),
    ("java/security/BasicPermission", "java/security/Permission"),
    ("java/security/Permission", "java/lang/Object"),
    ("java/security/SecureClassLoader", "java/lang/ClassLoader"),
    // Round 14 wave 5 (lane compat5): the `java.sql` date/time family.
    ("java/sql/Date", "java/util/Date"),
    ("java/sql/Time", "java/util/Date"),
    ("java/sql/Timestamp", "java/util/Date"),
    ("java/text/BreakIterator", "java/lang/Object"),
    ("java/text/DecimalFormatSymbols", "java/lang/Object"),
    ("java/text/Normalizer", "java/lang/Object"),
    ("java/text/ParseException", "java/lang/Exception"),
    ("java/time/Duration", "java/lang/Object"),
    ("java/time/ZoneId", "java/lang/Object"),
    ("java/util/AbstractCollection", "java/lang/Object"),
    ("java/util/AbstractList", "java/util/AbstractCollection"),
    ("java/util/AbstractMap", "java/lang/Object"),
    ("java/util/AbstractQueue", "java/util/AbstractCollection"),
    ("java/util/AbstractSequentialList", "java/util/AbstractList"),
    ("java/util/AbstractSet", "java/util/AbstractCollection"),
    ("java/util/ArrayDeque", "java/util/AbstractCollection"),
    ("java/util/ArrayList", "java/util/AbstractList"),
    ("java/util/ArrayList$Itr", "java/lang/Object"),
    ("java/util/ArrayList$ListItr", "java/util/ArrayList$Itr"),
    ("java/util/ArrayList$SubList", "java/util/AbstractList"),
    ("java/util/ArrayList$SubList$1", "java/lang/Object"),
    ("java/util/Arrays", "java/lang/Object"),
    ("java/util/Arrays$ArrayList", "java/util/AbstractList"),
    ("java/util/Collections", "java/lang/Object"),
    ("java/util/Collections$EmptyEnumeration", "java/lang/Object"),
    ("java/util/Collections$EmptyIterator", "java/lang/Object"),
    ("java/util/Collections$EmptyListIterator", "java/util/Collections$EmptyIterator"),
    ("java/util/Collections$SetFromMap", "java/util/AbstractSet"),
    ("java/util/ConcurrentModificationException", "java/lang/RuntimeException"),
    ("java/util/Date", "java/lang/Object"),
    ("java/util/Dictionary", "java/lang/Object"),
    ("java/util/EventObject", "java/lang/Object"),
    ("java/util/FormatterClosedException", "java/lang/IllegalStateException"),
    ("java/util/HashMap", "java/util/AbstractMap"),
    ("java/util/HashMap$EntryIterator", "java/util/HashMap$HashIterator"),
    ("java/util/HashMap$EntrySet", "java/util/AbstractSet"),
    ("java/util/HashMap$HashIterator", "java/lang/Object"),
    ("java/util/HashMap$KeyIterator", "java/util/HashMap$HashIterator"),
    ("java/util/HashMap$KeySet", "java/util/AbstractSet"),
    ("java/util/HashMap$Node", "java/lang/Object"),
    ("java/util/HashMap$ValueIterator", "java/util/HashMap$HashIterator"),
    ("java/util/HashMap$Values", "java/util/AbstractCollection"),
    ("java/util/HashSet", "java/util/AbstractSet"),
    ("java/util/Hashtable", "java/util/Dictionary"),
    ("java/util/Hashtable$Entry", "java/lang/Object"),
    ("java/util/Hashtable$EntrySet", "java/util/AbstractSet"),
    ("java/util/Hashtable$KeySet", "java/util/AbstractSet"),
    ("java/util/Hashtable$ValueCollection", "java/util/AbstractCollection"),
    ("java/util/InputMismatchException", "java/util/NoSuchElementException"),
    ("java/util/LinkedHashMap", "java/util/HashMap"),
    ("java/util/LinkedHashMap$Entry", "java/util/HashMap$Node"),
    ("java/util/LinkedHashMap$LinkedEntryIterator", "java/util/LinkedHashMap$LinkedHashIterator"),
    ("java/util/LinkedHashMap$LinkedEntrySet", "java/util/AbstractSet"),
    ("java/util/LinkedHashMap$LinkedHashIterator", "java/lang/Object"),
    ("java/util/LinkedHashMap$LinkedKeyIterator", "java/util/LinkedHashMap$LinkedHashIterator"),
    ("java/util/LinkedHashMap$LinkedKeySet", "java/util/AbstractSet"),
    ("java/util/LinkedHashMap$LinkedValueIterator", "java/util/LinkedHashMap$LinkedHashIterator"),
    ("java/util/LinkedHashMap$LinkedValues", "java/util/AbstractCollection"),
    ("java/util/LinkedHashSet", "java/util/HashSet"),
    ("java/util/LinkedList", "java/util/AbstractSequentialList"),
    ("java/util/LinkedList$ListItr", "java/lang/Object"),
    ("java/util/MissingResourceException", "java/lang/RuntimeException"),
    ("java/util/NoSuchElementException", "java/lang/RuntimeException"),
    ("java/util/Optional", "java/lang/Object"),
    ("java/util/OptionalDouble", "java/lang/Object"),
    ("java/util/OptionalInt", "java/lang/Object"),
    ("java/util/OptionalLong", "java/lang/Object"),
    ("java/util/PriorityQueue", "java/util/AbstractQueue"),
    ("java/util/PriorityQueue$Itr", "java/lang/Object"),
    ("java/util/Properties", "java/util/Hashtable"),
    ("java/util/Stack", "java/util/Vector"),
    ("java/util/TreeMap", "java/util/AbstractMap"),
    ("java/util/TreeMap$Entry", "java/lang/Object"),
    ("java/util/TreeMap$EntryIterator", "java/util/TreeMap$PrivateEntryIterator"),
    ("java/util/TreeMap$EntrySet", "java/util/AbstractSet"),
    ("java/util/TreeMap$KeyIterator", "java/util/TreeMap$PrivateEntryIterator"),
    ("java/util/TreeMap$KeySet", "java/util/AbstractSet"),
    ("java/util/TreeMap$PrivateEntryIterator", "java/lang/Object"),
    ("java/util/TreeMap$ValueIterator", "java/util/TreeMap$PrivateEntryIterator"),
    ("java/util/TreeMap$Values", "java/util/AbstractCollection"),
    ("java/util/TreeSet", "java/util/AbstractSet"),
    ("java/util/Vector", "java/util/AbstractList"),
    ("java/util/concurrent/AbstractExecutorService", "java/lang/Object"),
    ("java/util/concurrent/BrokenBarrierException", "java/lang/Exception"),
    ("java/util/concurrent/CancellationException", "java/lang/IllegalStateException"),
    ("java/util/concurrent/CompletableFuture", "java/lang/Object"),
    ("java/util/concurrent/CompletionException", "java/lang/RuntimeException"),
    ("java/util/concurrent/ConcurrentHashMap", "java/util/AbstractMap"),
    (
        "java/util/concurrent/ConcurrentHashMap$BaseIterator",
        "java/util/concurrent/ConcurrentHashMap$Traverser",
    ),
    ("java/util/concurrent/ConcurrentHashMap$CollectionView", "java/lang/Object"),
    (
        "java/util/concurrent/ConcurrentHashMap$EntryIterator",
        "java/util/concurrent/ConcurrentHashMap$BaseIterator",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$EntrySetView",
        "java/util/concurrent/ConcurrentHashMap$CollectionView",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeyIterator",
        "java/util/concurrent/ConcurrentHashMap$BaseIterator",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$KeySetView",
        "java/util/concurrent/ConcurrentHashMap$CollectionView",
    ),
    ("java/util/concurrent/ConcurrentHashMap$MapEntry", "java/lang/Object"),
    ("java/util/concurrent/ConcurrentHashMap$Traverser", "java/lang/Object"),
    (
        "java/util/concurrent/ConcurrentHashMap$ValueIterator",
        "java/util/concurrent/ConcurrentHashMap$BaseIterator",
    ),
    (
        "java/util/concurrent/ConcurrentHashMap$ValuesView",
        "java/util/concurrent/ConcurrentHashMap$CollectionView",
    ),
    ("java/util/concurrent/CopyOnWriteArrayList", "java/lang/Object"),
    ("java/util/concurrent/ExecutionException", "java/lang/Exception"),
    ("java/util/concurrent/PriorityBlockingQueue", "java/util/AbstractQueue"),
    ("java/util/concurrent/RejectedExecutionException", "java/lang/RuntimeException"),
    ("java/util/concurrent/ScheduledThreadPoolExecutor", "java/util/concurrent/ThreadPoolExecutor"),
    ("java/util/concurrent/ThreadPoolExecutor", "java/util/concurrent/AbstractExecutorService"),
    ("java/util/concurrent/TimeUnit", "java/lang/Enum"),
    ("java/util/concurrent/TimeoutException", "java/lang/Exception"),
    ("java/util/concurrent/atomic/AtomicBoolean", "java/lang/Object"),
    ("java/util/concurrent/atomic/AtomicInteger", "java/lang/Number"),
    ("java/util/concurrent/atomic/AtomicIntegerArray", "java/lang/Object"),
    ("java/util/concurrent/atomic/AtomicLong", "java/lang/Number"),
    ("java/util/concurrent/atomic/AtomicLongArray", "java/lang/Object"),
    ("java/util/concurrent/atomic/AtomicMarkableReference", "java/lang/Object"),
    ("java/util/concurrent/atomic/AtomicReference", "java/lang/Object"),
    ("java/util/concurrent/atomic/AtomicStampedReference", "java/lang/Object"),
    ("java/util/concurrent/atomic/DoubleAdder", "java/util/concurrent/atomic/Striped64"),
    ("java/util/concurrent/atomic/LongAdder", "java/util/concurrent/atomic/Striped64"),
    ("java/util/concurrent/atomic/Striped64", "java/lang/Number"),
    ("java/util/concurrent/locks/AbstractOwnableSynchronizer", "java/lang/Object"),
    (
        "java/util/concurrent/locks/AbstractQueuedLongSynchronizer",
        "java/util/concurrent/locks/AbstractOwnableSynchronizer",
    ),
    ("java/util/concurrent/locks/LockSupport", "java/lang/Object"),
    ("java/util/jar/Attributes", "java/lang/Object"),
    ("java/util/jar/Attributes$Name", "java/lang/Object"),
    ("java/util/jar/JarEntry", "java/util/zip/ZipEntry"),
    ("java/util/jar/JarFile", "java/util/zip/ZipFile"),
    ("java/util/jar/Manifest", "java/lang/Object"),
    ("java/util/logging/FileHandler", "java/util/logging/StreamHandler"),
    ("java/util/logging/Handler", "java/lang/Object"),
    ("java/util/logging/Level", "java/lang/Object"),
    ("java/util/logging/LogManager", "java/lang/Object"),
    ("java/util/logging/LogRecord", "java/lang/Object"),
    ("java/util/logging/Logger", "java/lang/Object"),
    ("java/util/logging/LoggingPermission", "java/security/BasicPermission"),
    ("java/util/logging/StreamHandler", "java/util/logging/Handler"),
    ("java/util/stream/AbstractPipeline", "java/util/stream/PipelineHelper"),
    ("java/util/stream/Collectors", "java/lang/Object"),
    ("java/util/stream/DoubleStream", "java/lang/Object"),
    ("java/util/stream/IntStream", "java/lang/Object"),
    ("java/util/stream/LongStream", "java/lang/Object"),
    ("java/util/stream/PipelineHelper", "java/lang/Object"),
    ("java/util/stream/ReferencePipeline", "java/util/stream/AbstractPipeline"),
    ("java/util/stream/Stream", "java/lang/Object"),
    ("java/util/zip/CRC32", "java/lang/Object"),
    ("java/util/zip/CRC32C", "java/lang/Object"),
    ("java/util/zip/ZipEntry", "java/lang/Object"),
    ("java/util/zip/ZipFile", "java/lang/Object"),
    ("java/util/zip/ZipFile$1", "java/lang/Object"),
    (
        "jdk/internal/foreign/layout/AbstractGroupLayout",
        "jdk/internal/foreign/layout/AbstractLayout",
    ),
    ("jdk/internal/foreign/layout/AbstractLayout", "java/lang/Object"),
    ("jdk/internal/foreign/layout/PaddingLayoutImpl", "jdk/internal/foreign/layout/AbstractLayout"),
    (
        "jdk/internal/foreign/layout/SequenceLayoutImpl",
        "jdk/internal/foreign/layout/AbstractLayout",
    ),
    (
        "jdk/internal/foreign/layout/StructLayoutImpl",
        "jdk/internal/foreign/layout/AbstractGroupLayout",
    ),
    (
        "jdk/internal/foreign/layout/UnionLayoutImpl",
        "jdk/internal/foreign/layout/AbstractGroupLayout",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$AbstractValueLayout",
        "jdk/internal/foreign/layout/AbstractLayout",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
        "jdk/internal/foreign/layout/ValueLayouts$AbstractValueLayout",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
        "jdk/internal/foreign/layout/ValueLayouts$AbstractValueLayout",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
        "jdk/internal/foreign/layout/ValueLayouts$AbstractValueLayout",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
        "jdk/internal/foreign/layout/ValueLayouts$AbstractValueLayout",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
        "jdk/internal/foreign/layout/ValueLayouts$AbstractValueLayout",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
        "jdk/internal/foreign/layout/ValueLayouts$AbstractValueLayout",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
        "jdk/internal/foreign/layout/ValueLayouts$AbstractValueLayout",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
        "jdk/internal/foreign/layout/ValueLayouts$AbstractValueLayout",
    ),
    (
        "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
        "jdk/internal/foreign/layout/ValueLayouts$AbstractValueLayout",
    ),
    ("jdk/internal/loader/URLClassPath", "java/lang/Object"),
    ("jdk/internal/misc/ScopedMemoryAccess", "java/lang/Object"),
    ("jdk/internal/misc/Unsafe", "java/lang/Object"),
    ("jdk/internal/misc/VM", "java/lang/Object"),
    ("jdk/internal/ref/PhantomCleanable", "java/lang/ref/PhantomReference"),
    ("jdk/internal/util/Preconditions", "java/lang/Object"),
    ("sun/misc/Unsafe", "java/lang/Object"),
    ("sun/nio/ch/FileChannelImpl", "java/nio/channels/FileChannel"),
    ("sun/nio/ch/FileChannelImpl$Closer", "java/lang/Object"),
    ("sun/nio/ch/NativeThreadSet", "java/lang/Object"),
    ("sun/nio/ch/Util", "java/lang/Object"),
    ("sun/nio/fs/AbstractFileSystemProvider", "java/nio/file/spi/FileSystemProvider"),
    ("sun/nio/fs/WindowsDirectoryStream", "java/lang/Object"),
    ("sun/nio/fs/WindowsFileAttributes", "java/lang/Object"),
    ("sun/nio/fs/WindowsFileStore", "java/nio/file/FileStore"),
    ("sun/nio/fs/WindowsFileSystem", "java/nio/file/FileSystem"),
    ("sun/nio/fs/WindowsFileSystemProvider", "sun/nio/fs/AbstractFileSystemProvider"),
    ("sun/nio/fs/WindowsPath", "java/lang/Object"),
    ("sun/util/calendar/ZoneInfoFile", "java/lang/Object"),
    (
        "sun/util/locale/provider/JRELocaleProviderAdapter",
        "sun/util/locale/provider/LocaleProviderAdapter",
    ),
    ("sun/util/locale/provider/LocaleProviderAdapter", "java/lang/Object"),
    ("sun/util/resources/LocaleData", "java/lang/Object"),
];

/// Ancestor rows whose JDK 25 method is `ACC_NATIVE`: the registered native
/// IS the implementation there, so reaching it is what the real bytecode
/// would do too (`Object.hashCode` under a retired `hashCode` that the class
/// does not override). The walk stops at them without a finding.
const ACC_NATIVE_ANCESTOR_ROWS: &[(&str, &str, &str)] = &[
    ("java/lang/Object", "clone", "()Ljava/lang/Object;"),
    ("java/lang/Object", "getClass", "()Ljava/lang/Class;"),
    ("java/lang/Object", "hashCode", "()I"),
    ("java/lang/Object", "notify", "()V"),
    ("java/lang/Object", "notifyAll", "()V"),
];

/// JDK 25 declarations (with `Code`, or abstract) that stop a walk before a
/// live ancestor native: on a retired class itself, `invoke_or_native` does
/// not walk at all (`has_own_bytecode`); on an intermediate class, the walk
/// breaks there. Only the facts some walk needs are listed -- every row here
/// is a class whose OWN retired row, or whose subclass's, would otherwise
/// reach a live `Bridge` further up. From the JDK 25 sources; `javap -p`
/// confirms each.
const DECLARED_ON_JDK25: &[(&str, &str, &str)] = &[
    ("java/io/BufferedOutputStream", "write", "(I)V"),
    ("java/io/ByteArrayOutputStream", "write", "(I)V"),
    ("java/io/DataOutputStream", "write", "(I)V"),
    ("java/io/FileOutputStream", "write", "(I)V"),
    ("java/io/FileOutputStream", "write", "([B)V"),
    ("java/io/FilterInputStream", "read", "()I"),
    ("java/io/FilterOutputStream", "write", "(I)V"),
    ("java/io/FilterOutputStream", "write", "([B)V"),
    ("java/io/LineNumberInputStream", "read", "()I"),
    // Retired itself in round 14 wave 2; declared with `Code`, so no walk
    // starts from it.
    ("java/io/OutputStream", "write", "([B)V"),
    ("java/io/PrintStream", "write", "(I)V"),
    ("java/io/StringBufferInputStream", "read", "()I"),
    ("java/lang/StringBuilder", "toString", "()Ljava/lang/String;"),
    ("java/lang/Throwable", "getLocalizedMessage", "()Ljava/lang/String;"),
    ("java/lang/Throwable", "getMessage", "()Ljava/lang/String;"),
    ("java/lang/Throwable", "toString", "()Ljava/lang/String;"),
    // Round 14 wave 6 (lane compat6): both overloads are declared with `Code`
    // on `Array` itself, so no walk starts from them.
    (
        "java/lang/reflect/Array",
        "newInstance",
        "(Ljava/lang/Class;I)Ljava/lang/Object;",
    ),
    (
        "java/lang/reflect/Array",
        "newInstance",
        "(Ljava/lang/Class;[I)Ljava/lang/Object;",
    ),
    ("java/lang/reflect/Method", "setAccessible", "(Z)V"),
    (
        "java/net/URLClassLoader",
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
    ),
    ("java/nio/ByteBufferAsCharBufferB", "get", "()C"),
    ("java/nio/ByteBufferAsCharBufferB", "get", "(I)C"),
    // Round 14 wave 5 (lane compat5): the `java.sql` date/time family. `Date`
    // and `Time` inherit `getTime()` from `java/util/Date`, which declares it.
    ("java/sql/Date", "toString", "()Ljava/lang/String;"),
    ("java/sql/Time", "toString", "()Ljava/lang/String;"),
    ("java/sql/Timestamp", "getTime", "()J"),
    ("java/sql/Timestamp", "toLocalDateTime", "()Ljava/time/LocalDateTime;"),
    ("java/sql/Timestamp", "toString", "()Ljava/lang/String;"),
    ("java/util/Date", "getTime", "()J"),
    ("java/nio/ByteBufferAsCharBufferB", "order", "()Ljava/nio/ByteOrder;"),
    ("java/nio/ByteBufferAsCharBufferB", "toString", "(II)Ljava/lang/String;"),
    ("java/nio/ByteBufferAsCharBufferL", "get", "()C"),
    ("java/nio/ByteBufferAsCharBufferL", "get", "(I)C"),
    ("java/nio/ByteBufferAsCharBufferL", "order", "()Ljava/nio/ByteOrder;"),
    ("java/nio/ByteBufferAsCharBufferL", "toString", "(II)Ljava/lang/String;"),
    ("java/nio/DirectByteBuffer", "get", "()B"),
    ("java/nio/DirectByteBuffer", "get", "(I)B"),
    ("java/nio/DirectByteBuffer", "getChar", "(I)C"),
    ("java/nio/DirectByteBuffer", "getFloat", "(I)F"),
    ("java/nio/DirectByteBuffer", "getInt", "(I)I"),
    ("java/nio/DirectByteBuffer", "getLong", "(I)J"),
    ("java/nio/DirectByteBuffer", "getShort", "(I)S"),
    ("java/nio/DirectByteBuffer", "isDirect", "()Z"),
    ("java/nio/DirectByteBuffer", "isReadOnly", "()Z"),
    ("java/nio/DirectByteBuffer", "put", "(B)Ljava/nio/ByteBuffer;"),
    ("java/nio/DirectByteBuffer", "put", "(IB)Ljava/nio/ByteBuffer;"),
    ("java/nio/DirectByteBuffer", "putChar", "(IC)Ljava/nio/ByteBuffer;"),
    ("java/nio/DirectByteBuffer", "putInt", "(II)Ljava/nio/ByteBuffer;"),
    ("java/nio/DirectByteBuffer", "putLong", "(IJ)Ljava/nio/ByteBuffer;"),
    ("java/nio/DirectByteBuffer", "putShort", "(IS)Ljava/nio/ByteBuffer;"),
    ("java/nio/HeapByteBuffer", "isDirect", "()Z"),
    ("java/nio/HeapByteBuffer", "isReadOnly", "()Z"),
    ("java/nio/HeapCharBuffer", "toString", "(II)Ljava/lang/String;"),
    ("java/nio/StringCharBuffer", "toString", "(II)Ljava/lang/String;"),
    ("java/util/ArrayDeque", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayDeque", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/ArrayDeque", "toArray", "([Ljava/lang/Object;)[Ljava/lang/Object;"),
    ("java/util/ArrayList", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayList", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/ArrayList", "toArray", "([Ljava/lang/Object;)[Ljava/lang/Object;"),
    ("java/util/ArrayList$SubList", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/ArrayList$SubList", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/ArrayList$SubList", "toArray", "([Ljava/lang/Object;)[Ljava/lang/Object;"),
    ("java/util/Arrays$ArrayList", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/Arrays$ArrayList", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/Arrays$ArrayList", "toArray", "([Ljava/lang/Object;)[Ljava/lang/Object;"),
    ("java/util/Collections$SetFromMap", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/Collections$SetFromMap", "toArray", "()[Ljava/lang/Object;"),
    (
        "java/util/Collections$SetFromMap",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    ("java/util/HashMap$EntrySet", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/HashMap$KeySet", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/HashMap$KeySet", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/HashMap$KeySet", "toArray", "([Ljava/lang/Object;)[Ljava/lang/Object;"),
    ("java/util/HashMap$Values", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/HashMap$Values", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/HashMap$Values", "toArray", "([Ljava/lang/Object;)[Ljava/lang/Object;"),
    ("java/util/HashSet", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/HashSet", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/HashSet", "toArray", "([Ljava/lang/Object;)[Ljava/lang/Object;"),
    ("java/util/Hashtable$EntrySet", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/Hashtable$KeySet", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/Hashtable$ValueCollection", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/LinkedHashMap$LinkedEntrySet", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/LinkedHashMap$LinkedKeySet", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/LinkedHashMap$LinkedKeySet", "toArray", "()[Ljava/lang/Object;"),
    (
        "java/util/LinkedHashMap$LinkedKeySet",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    ("java/util/LinkedHashMap$LinkedValues", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/LinkedHashMap$LinkedValues", "toArray", "()[Ljava/lang/Object;"),
    (
        "java/util/LinkedHashMap$LinkedValues",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    ),
    ("java/util/LinkedList", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/LinkedList", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/LinkedList", "toArray", "([Ljava/lang/Object;)[Ljava/lang/Object;"),
    ("java/util/PriorityQueue", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/PriorityQueue", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/PriorityQueue", "toArray", "([Ljava/lang/Object;)[Ljava/lang/Object;"),
    ("java/util/TreeMap$EntrySet", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/TreeMap$KeySet", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/TreeMap$Values", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/TreeSet", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/Vector", "contains", "(Ljava/lang/Object;)Z"),
    ("java/util/Vector", "toArray", "()[Ljava/lang/Object;"),
    ("java/util/Vector", "toArray", "([Ljava/lang/Object;)[Ljava/lang/Object;"),
    ("sun/nio/ch/FileChannelImpl", "truncate", "(J)Ljava/nio/channels/FileChannel;"),
    (
        "sun/nio/fs/AbstractFileSystemProvider",
        "delete",
        "(Ljava/nio/file/Path;)V",
    ),
    (
        "sun/nio/fs/AbstractFileSystemProvider",
        "setAttribute",
        "(Ljava/nio/file/Path;Ljava/lang/String;Ljava/lang/Object;[Ljava/nio/file/LinkOption;)V",
    ),
    ("sun/nio/fs/WindowsFileStore", "getAttribute", "(Ljava/lang/String;)Ljava/lang/Object;"),
    ("sun/nio/fs/WindowsFileStore", "getBlockSize", "()J"),
    (
        "sun/nio/fs/WindowsFileStore",
        "getFileStoreAttributeView",
        "(Ljava/lang/Class;)Ljava/nio/file/attribute/FileStoreAttributeView;",
    ),
    ("sun/nio/fs/WindowsFileStore", "getTotalSpace", "()J"),
    ("sun/nio/fs/WindowsFileStore", "getUnallocatedSpace", "()J"),
    ("sun/nio/fs/WindowsFileStore", "getUsableSpace", "()J"),
    ("sun/nio/fs/WindowsFileStore", "isReadOnly", "()Z"),
    ("sun/nio/fs/WindowsFileStore", "name", "()Ljava/lang/String;"),
    ("sun/nio/fs/WindowsFileStore", "supportsFileAttributeView", "(Ljava/lang/Class;)Z"),
    ("sun/nio/fs/WindowsFileStore", "supportsFileAttributeView", "(Ljava/lang/String;)Z"),
    ("sun/nio/fs/WindowsFileStore", "toString", "()Ljava/lang/String;"),
    ("sun/nio/fs/WindowsFileStore", "type", "()Ljava/lang/String;"),
    ("sun/nio/fs/WindowsFileSystem", "close", "()V"),
    ("sun/nio/fs/WindowsFileSystem", "getFileStores", "()Ljava/lang/Iterable;"),
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
    ("sun/nio/fs/WindowsFileSystem", "getRootDirectories", "()Ljava/lang/Iterable;"),
    ("sun/nio/fs/WindowsFileSystem", "getSeparator", "()Ljava/lang/String;"),
    ("sun/nio/fs/WindowsFileSystem", "isOpen", "()Z"),
    ("sun/nio/fs/WindowsFileSystem", "isReadOnly", "()Z"),
    (
        "sun/nio/fs/WindowsFileSystem",
        "provider",
        "()Ljava/nio/file/spi/FileSystemProvider;",
    ),
    ("sun/nio/fs/WindowsFileSystem", "supportedFileAttributeViews", "()Ljava/util/Set;"),
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
    ("sun/nio/fs/WindowsFileSystemProvider", "isHidden", "(Ljava/nio/file/Path;)Z"),
    (
        "sun/nio/fs/WindowsFileSystemProvider",
        "isSameFile",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;)Z",
    ),
    (
        "sun/nio/fs/WindowsFileSystemProvider",
        "move",
        "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)V",
    ),
];

/// Half-retirements this gate found and that are NOT fixed yet, frozen
/// `(retired class, method, descriptor, ancestor with the live Bridge)`.
/// Each must be open on a page saying why neither the ancestor row nor the
/// dispatch mask covers it. The list may only shrink.
///
/// Round 14 wave 2 (lane shadow): 19 -> 0. The 19 pairs of
/// `r13w13-shadow5-ancestor-bridges-over-retired-rows-FIXED-20260929.md` are masked
/// at dispatch (module doc); the passing test prints them as `masked`.
const KNOWN_HALF_RETIREMENTS: &[(&str, &str, &str, &str)] = &[];

/// A floor under the masked-pair count (18 expected on 25/linux after the wave;
/// kept below it because an arm may not register every ancestor row). Not an
/// exact freeze: a family retired later may add pairs (the mask covers them),
/// but fewer than this means the walk or the image tables stopped seeing the
/// species, and the gate would pass on nothing.
const MASKED_PAIRS_FLOOR: usize = 10;

/// Is the dispatch mask turned off in this test's environment? Then a found
/// pair is reported, not failed: that arm is the half-retirement on purpose.
fn mask_switched_off() -> bool {
    std::env::var("CRATONVM_RETIRED_SHADOW_MASKS_ANCESTOR_BRIDGES").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    })
}

fn superclass_of(class: &str) -> Option<&'static str> {
    JDK25_SUPERCLASS
        .iter()
        .find(|(c, _)| *c == class)
        .map(|(_, s)| *s)
}

fn declares(class: &str, method: &str, descriptor: &str) -> bool {
    DECLARED_ON_JDK25
        .iter()
        .any(|(c, m, d)| *c == class && *m == method && *d == descriptor)
}

fn acc_native_ancestor(class: &str, method: &str, descriptor: &str) -> bool {
    ACC_NATIVE_ANCESTOR_ROWS
        .iter()
        .any(|(c, m, d)| *c == class && *m == method && *d == descriptor)
}

fn known_half_retirement(class: &str, method: &str, descriptor: &str, ancestor: &str) -> bool {
    KNOWN_HALF_RETIREMENTS
        .iter()
        .any(|(c, m, d, a)| *c == class && *m == method && *d == descriptor && *a == ancestor)
}

/// One walk result: the retired row and the ancestor whose live `Bridge`
/// answers for it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct HalfRetirement {
    class: String,
    method: String,
    descriptor: String,
    ancestor: &'static str,
}

/// `invoke_or_native`'s superclass walk, over sets instead of a live class
/// manager. Returns the half-retirements found and the classes the walk
/// could not continue from (no [`JDK25_SUPERCLASS`] row).
fn walk_every_retired_row(
    retired: &BTreeSet<Triple>,
    live_bridges: &BTreeSet<Triple>,
) -> (Vec<HalfRetirement>, BTreeSet<String>, usize) {
    let mut found = Vec::new();
    let mut unmapped = BTreeSet::new();
    let mut walks = 0usize;
    for (class, method, descriptor) in retired {
        if method == "<init>" || method == "<clinit>" {
            continue;
        }
        // The class declares the method: `has_own_bytecode`, no walk.
        if declares(class, method, descriptor) {
            continue;
        }
        walks += 1;
        let mut cur: &str = class.as_str();
        while cur != OBJECT {
            let Some(parent) = superclass_of(cur) else {
                unmapped.insert(cur.to_string());
                break;
            };
            let key = (parent.to_string(), method.clone(), descriptor.clone());
            if live_bridges.contains(&key) {
                if !acc_native_ancestor(parent, method, descriptor) {
                    found.push(HalfRetirement {
                        class: class.clone(),
                        method: method.clone(),
                        descriptor: descriptor.clone(),
                        ancestor: parent,
                    });
                }
                break;
            }
            if declares(parent, method, descriptor) {
                break;
            }
            cur = parent;
        }
    }
    (found, unmapped, walks)
}

fn boot_registry(mode: Option<CompatibilityMode>) -> NativeMethodRegistry {
    let mut registry = NativeMethodRegistry::new();
    if let Some(mode) = mode {
        // Before registration, as `vm_init` does: a mode set afterwards would
        // leave every refused row in the table.
        registry.set_compatibility_mode(mode);
    }
    boot_path::vm_init_real_jdk_boot_path(&mut registry);
    registry
}

fn rows_of(registry: &NativeMethodRegistry) -> Vec<(String, String, String, NativeKind)> {
    registry
        .dump_registrations()
        .into_iter()
        .map(|(c, m, d, k)| (c.to_string(), m.to_string(), d.to_string(), k))
        .collect()
}

fn triple_of(t: &HalfRetirement) -> String {
    format!(
        "    ({:?}, {:?}, {:?}, {:?}),",
        t.class, t.method, t.descriptor, t.ancestor
    )
}

#[test]
fn no_retired_triple_has_a_live_bridge_on_an_ancestor() {
    let retired: BTreeSet<Triple> = rows_of(&boot_registry(None))
        .into_iter()
        .filter(|(c, m, d, _)| triple_is_retired_shadow(c, m, d))
        .map(|(c, m, d, _)| (c, m, d))
        .collect();
    let strict = boot_registry(Some(CompatibilityMode::JdkOnly));
    let live_bridges: BTreeSet<Triple> = rows_of(&strict)
        .into_iter()
        .filter(|(_, _, _, k)| *k == NativeKind::Bridge)
        .map(|(c, m, d, _)| (c, m, d))
        .collect();

    // Vacuity floors: a boot path that stopped registering, or a predicate
    // that stopped answering, passes every set comparison below.
    assert!(
        retired.len() >= 500,
        "only {} retired rows in the compatible boot registry -- the scope or \
         `triple_is_retired_shadow` broke, and this gate would pass on nothing",
        retired.len()
    );
    assert!(
        live_bridges.len() >= 2000,
        "only {} live Bridge rows in the strict boot registry",
        live_bridges.len()
    );

    let (found, unmapped, walks) = walk_every_retired_row(&retired, &live_bridges);
    let switched_off = mask_switched_off();
    let mut new: Vec<String> = Vec::new();
    let mut masked = 0usize;
    let mut seen_known: BTreeSet<(String, String, String, &'static str)> = BTreeSet::new();
    for t in &found {
        // Round 14: the strict registry masks the ancestor's Bridge for a
        // walk that passed this retired row, so the pair is answered at
        // dispatch (module doc).
        if strict.retired_row_masks_ancestor_bridges(&t.class, &t.method, &t.descriptor) {
            masked += 1;
            println!("retired-ancestor gate: masked {}", triple_of(t).trim());
        } else if known_half_retirement(&t.class, &t.method, &t.descriptor, t.ancestor) {
            seen_known.insert((
                t.class.clone(),
                t.method.clone(),
                t.descriptor.clone(),
                t.ancestor,
            ));
        } else {
            new.push(triple_of(t));
        }
    }
    println!(
        "retired-ancestor gate: {} retired rows, {walks} walked, {} pairs, {masked} masked \
         at dispatch, {} half-retired ({} known, {} new), {} unmapped classes",
        retired.len(),
        found.len(),
        seen_known.len() + new.len(),
        seen_known.len(),
        new.len(),
        unmapped.len()
    );
    if switched_off {
        // The kill-switch arm: the half-retirements are back on purpose.
        println!(
            "retired-ancestor gate: CRATONVM_RETIRED_SHADOW_MASKS_ANCESTOR_BRIDGES is off; \
             {} unmasked pair(s) reported, not failed",
            new.len()
        );
        for line in &new {
            println!("retired-ancestor gate: unmasked {}", line.trim());
        }
        new.clear();
    } else {
        assert!(
            masked >= MASKED_PAIRS_FLOOR,
            "only {masked} masked pair(s), under the floor of {MASKED_PAIRS_FLOOR}: the walk \
             or the image tables stopped seeing the species, so this gate would pass on nothing"
        );
    }
    // An entry this boot path does not observe is reported, not failed: the
    // registrars differ by platform and feature arm, so one arm's absence is
    // not a fix. Delete an entry when every arm prints it here.
    for (c, m, d, a) in KNOWN_HALF_RETIREMENTS {
        let key = (c.to_string(), m.to_string(), d.to_string(), *a);
        if !seen_known.contains(&key) {
            println!("retired-ancestor gate: STALE (not observed here): {c}.{m}{d} <- {a}");
        }
    }

    assert!(
        unmapped.is_empty(),
        "the walk cannot continue from {} class(es) with no JDK25_SUPERCLASS row; add \
         each one's superclass (`javap -p <class>`, first line):\n  {}",
        unmapped.len(),
        unmapped.iter().cloned().collect::<Vec<_>>().join("\n  ")
    );
    assert!(
        new.is_empty(),
        "{} retired triple(s) are HALF-RETIRED: the class does not declare the \
         method, so `invoke_or_native` walks up and a live Bridge on an ancestor \
         answers every receiver -- the table says bytecode, the run says native \
         (lane T's 2026-09-11 pull-back was this) -- and the strict registry's \
         `retired_row_masks_ancestor_bridges` does not mask it. Retire the ancestor row with \
         it, or, if that cannot be done yet, record the pair in \
         KNOWN_HALF_RETIREMENTS with a page. If the class DOES declare the \
         method (`javap -p`), add it to DECLARED_ON_JDK25 instead.\n{}",
        new.len(),
        new.join("\n")
    );
}

/// The walk itself, on hand-built sets, so the gate cannot pass by being
/// unable to find anything. The first case is the `VirtualMachineError`
/// shape this gate found; the others are the two ways a walk must stop.
#[test]
fn the_walk_finds_the_species_and_stops_where_invoke_or_native_stops() {
    let t = |c: &str, m: &str, d: &str| (c.to_string(), m.to_string(), d.to_string());
    let get_message = ("getMessage", "()Ljava/lang/String;");

    // A retired subclass row under a live, non-declaring ancestor: found.
    let retired: BTreeSet<Triple> =
        [t("java/lang/InternalError", get_message.0, get_message.1)].into_iter().collect();
    let live: BTreeSet<Triple> =
        [t("java/lang/VirtualMachineError", get_message.0, get_message.1)].into_iter().collect();
    let (found, unmapped, walks) = walk_every_retired_row(&retired, &live);
    assert!(unmapped.is_empty());
    assert_eq!(walks, 1);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].ancestor, "java/lang/VirtualMachineError");

    // A declaring intermediate class stops the walk: `LinkedHashSet` reaches
    // `HashSet.contains` (declared) before `AbstractCollection`'s native.
    let contains = ("contains", "(Ljava/lang/Object;)Z");
    let retired: BTreeSet<Triple> =
        [t("java/util/LinkedHashSet", contains.0, contains.1)].into_iter().collect();
    let live: BTreeSet<Triple> =
        [t("java/util/AbstractCollection", contains.0, contains.1)].into_iter().collect();
    let (found, _, _) = walk_every_retired_row(&retired, &live);
    assert!(found.is_empty(), "{found:?}");

    // An ACC_NATIVE ancestor is the implementation, not a shadow.
    let retired: BTreeSet<Triple> =
        [t("java/util/Optional", "hashCode", "()I")].into_iter().collect();
    let live: BTreeSet<Triple> = [t(OBJECT, "hashCode", "()I")].into_iter().collect();
    let (found, _, _) = walk_every_retired_row(&retired, &live);
    assert!(found.is_empty(), "{found:?}");

    // Constructors are not inherited and are never walked.
    let retired: BTreeSet<Triple> = [t("java/util/jar/JarFile", "<init>", "(Ljava/io/File;Z)V")]
        .into_iter()
        .collect();
    let live: BTreeSet<Triple> = [t("java/util/zip/ZipFile", "<init>", "(Ljava/io/File;Z)V")]
        .into_iter()
        .collect();
    let (found, _, walks) = walk_every_retired_row(&retired, &live);
    assert!(found.is_empty() && walks == 0);
}

/// The two image tables are well formed: every superclass chain ends at
/// `java/lang/Object` without a cycle, no class has two rows, and every
/// known half-retirement names an ancestor that really is one.
#[test]
fn the_image_tables_are_well_formed() {
    let mut seen = BTreeSet::new();
    for (c, s) in JDK25_SUPERCLASS {
        assert!(seen.insert(*c), "{c} has two JDK25_SUPERCLASS rows");
        assert_ne!(c, s, "{c} is its own superclass");
        let mut cur = *c;
        let mut steps = 0;
        while cur != OBJECT {
            cur = superclass_of(cur).unwrap_or_else(|| {
                panic!("{c}'s chain stops at {cur}, which has no JDK25_SUPERCLASS row")
            });
            steps += 1;
            assert!(steps < 16, "{c}'s superclass chain does not reach {OBJECT}");
        }
    }
    let mut declared = BTreeSet::new();
    for row in DECLARED_ON_JDK25 {
        assert!(declared.insert(*row), "{row:?} is listed twice");
    }
    for (c, m, d, a) in KNOWN_HALF_RETIREMENTS {
        assert!(!declares(c, m, d), "{c}.{m}{d} is both declared and half-retired");
        let mut cur = *c;
        let mut is_ancestor = false;
        while let Some(parent) = superclass_of(cur) {
            if parent == *a {
                is_ancestor = true;
                break;
            }
            cur = parent;
        }
        assert!(is_ancestor, "{a} is not a superclass of {c}");
    }
}
