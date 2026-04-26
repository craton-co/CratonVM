//! NEW-16: JCK-style java.base conformance harness.
//!
//! Runs the curated `Tck*` test corpus under `tests/resources/rustjvm/` through
//! the VM and records pass/fail per java.base category (Lang, Util, Io, Nio,
//! Net, Security, Reflect, Loading, ClassFile, Instructions, Jdbc). Produces a
//! machine-checkable summary and a regression gate.
//!
//! The baseline report lives at `docs/jdk-regression-baseline.md`. Each category
//! has a floor — CI fails if the pass count drops below the committed floor.
//!
//! Convention for the corpus:
//!   - Every test is `public static int testName()` returning 1 on pass, 0 on fail.
//!   - Tests must not take arguments, perform I/O, or depend on external state.
//!
//! Prerequisites: javac on PATH (see `build.rs`). If class files are missing at
//! runtime, the harness is skipped — it does not fail the build.

use rustjvm_vm::config::VmConfig;
use rustjvm_vm::types::Value;
use rustjvm_vm::vm::Vm;

// ---------------------------------------------------------------------------
// Test corpus — one entry per (category, class, method) triple.
// ---------------------------------------------------------------------------

/// A category name grouping tests by java.base area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Category(&'static str);

struct TckTest {
    category: Category,
    class: &'static str,
    method: &'static str,
}

const LANG: Category = Category("Lang");
const UTIL: Category = Category("Util");
const IO: Category = Category("Io");
const NIO: Category = Category("Nio");
const NET: Category = Category("Net");
const SECURITY: Category = Category("Security");
const REFLECT: Category = Category("Reflect");
const LOADING: Category = Category("Loading");
const CLASSFILE: Category = Category("ClassFile");
const INSTRUCTIONS: Category = Category("Instructions");
const JDBC: Category = Category("Jdbc");
// T4.2-T4.6 new categories
const CONCURRENT: Category = Category("Concurrent");
const REGEX: Category = Category("Regex");
const TIME: Category = Category("Time");
const TEXT: Category = Category("Text");
const MATH: Category = Category("Math");
const HTTP: Category = Category("Http");
const SQL: Category = Category("Sql");
const MANAGEMENT: Category = Category("Management");

const CORPUS: &[TckTest] = &[
    // =====================================================================
    // ClassFile (JVMS Ch. 4)
    // =====================================================================
    tck(CLASSFILE, "rustjvm/TckClassFile", "testMagicNumber"),
    tck(CLASSFILE, "rustjvm/TckClassFile", "testClassVersion"),
    tck(CLASSFILE, "rustjvm/TckClassFile", "testConstantPool"),
    tck(CLASSFILE, "rustjvm/TckClassFile", "testFieldAccess"),
    tck(CLASSFILE, "rustjvm/TckClassFile", "testMethodAccess"),
    // =====================================================================
    // Loading / init
    // =====================================================================
    tck(LOADING, "rustjvm/TckLoading", "testClassLoading"),
    tck(LOADING, "rustjvm/TckLoading", "testStaticInit"),
    tck(LOADING, "rustjvm/TckLoading", "testInterfaceInit"),
    tck(LOADING, "rustjvm/TckLoading", "testArrayCreation"),
    tck(LOADING, "rustjvm/TckLoading", "testInheritance"),
    // =====================================================================
    // Instructions (JVMS Ch. 6)
    // =====================================================================
    tck(INSTRUCTIONS, "rustjvm/TckInstructions", "testIntArithmetic"),
    tck(INSTRUCTIONS, "rustjvm/TckInstructions", "testLongArithmetic"),
    tck(INSTRUCTIONS, "rustjvm/TckInstructions", "testFloatArithmetic"),
    tck(INSTRUCTIONS, "rustjvm/TckInstructions", "testComparisons"),
    tck(INSTRUCTIONS, "rustjvm/TckInstructions", "testTableswitch"),
    tck(INSTRUCTIONS, "rustjvm/TckInstructions", "testLookupswitch"),
    tck(INSTRUCTIONS, "rustjvm/TckInstructions", "testFieldOps"),
    tck(INSTRUCTIONS, "rustjvm/TckInstructions", "testArrayOps"),
    tck(INSTRUCTIONS, "rustjvm/TckInstructions", "testInvokeVirtual"),
    tck(INSTRUCTIONS, "rustjvm/TckInstructions", "testInvokeStatic"),
    tck(INSTRUCTIONS, "rustjvm/TckInstructions", "testExceptionHandling"),
    tck(INSTRUCTIONS, "rustjvm/TckInstructions", "testCheckcast"),
    tck(INSTRUCTIONS, "rustjvm/TckInstructions", "testInstanceof"),
    // =====================================================================
    // java.lang — T4.2.1 Object, T4.2.2 Class, T4.2.3 String, T4.2.4 StringBuilder
    // =====================================================================
    // Object
    tck(LANG, "rustjvm/TckLang", "obj_hashCode_consistent"),
    tck(LANG, "rustjvm/TckLang", "obj_equals_identity"),
    tck(LANG, "rustjvm/TckLang", "obj_equals_different"),
    tck(LANG, "rustjvm/TckLang", "obj_getClass"),
    tck(LANG, "rustjvm/TckLang", "obj_toString"),
    // String
    tck(LANG, "rustjvm/TckLang", "str_equals"),
    tck(LANG, "rustjvm/TckLang", "str_compareTo"),
    tck(LANG, "rustjvm/TckLang", "str_contains"),
    tck(LANG, "rustjvm/TckLang", "str_isEmpty"),
    tck(LANG, "rustjvm/TckLang", "str_startsEndsWith"),
    tck(LANG, "rustjvm/TckLang", "str_replace"),
    tck(LANG, "rustjvm/TckLang", "str_toCharArray"),
    tck(LANG, "rustjvm/TckLang", "str_toLowerCase"),
    tck(LANG, "rustjvm/TckLang", "str_toUpperCase"),
    tck(LANG, "rustjvm/TckLang", "str_valueOf_int"),
    tck(LANG, "rustjvm/TckLang", "str_valueOf_bool"),
    tck(LANG, "rustjvm/TckLang", "str_concat"),
    tck(LANG, "rustjvm/TckLang", "str_constructor_chars"),
    // Integer/Long/Double/Float/Boolean wrappers
    tck(LANG, "rustjvm/TckLang", "int_autobox_cache"),
    tck(LANG, "rustjvm/TckLang", "int_compareTo"),
    tck(LANG, "rustjvm/TckLang", "int_constants"),
    tck(LANG, "rustjvm/TckLang", "long_toString"),
    tck(LANG, "rustjvm/TckLang", "long_maxValue"),
    tck(LANG, "rustjvm/TckLang", "double_isNaN"),
    tck(LANG, "rustjvm/TckLang", "double_isInfinite"),
    tck(LANG, "rustjvm/TckLang", "double_bits_roundtrip"),
    tck(LANG, "rustjvm/TckLang", "float_isNaN"),
    tck(LANG, "rustjvm/TckLang", "float_bits_roundtrip"),
    tck(LANG, "rustjvm/TckLang", "bool_parseBoolean"),
    tck(LANG, "rustjvm/TckLang", "bool_valueOf"),
    tck(LANG, "rustjvm/TckLang", "bool_toString"),
    // Byte/Short/Character
    tck(LANG, "rustjvm/TckLang", "byte_constants"),
    tck(LANG, "rustjvm/TckLang", "short_constants"),
    tck(LANG, "rustjvm/TckLang", "char_isDigit"),
    tck(LANG, "rustjvm/TckLang", "char_isLetter"),
    tck(LANG, "rustjvm/TckLang", "char_case"),
    tck(LANG, "rustjvm/TckLang", "char_convert"),
    tck(LANG, "rustjvm/TckLang", "char_isWhitespace"),
    // Math
    tck(LANG, "rustjvm/TckLang", "math_abs"),
    tck(LANG, "rustjvm/TckLang", "math_maxMin"),
    tck(LANG, "rustjvm/TckLang", "math_sqrt"),
    tck(LANG, "rustjvm/TckLang", "math_pow"),
    tck(LANG, "rustjvm/TckLang", "math_floorCeil"),
    tck(LANG, "rustjvm/TckLang", "math_round"),
    tck(LANG, "rustjvm/TckLang", "math_constants"),
    tck(LANG, "rustjvm/TckLang", "math_sinCos"),
    tck(LANG, "rustjvm/TckLang", "math_logExp"),
    // System
    tck(LANG, "rustjvm/TckLang", "sys_currentTimeMillis"),
    tck(LANG, "rustjvm/TckLang", "sys_nanoTime"),
    tck(LANG, "rustjvm/TckLang", "sys_arraycopy"),
    tck(LANG, "rustjvm/TckLang", "sys_identityHashCode"),
    // Runtime
    tck(LANG, "rustjvm/TckLang", "rt_availableProcessors"),
    tck(LANG, "rustjvm/TckLang", "rt_memory"),
    // StringBuilder (T4.2.4)
    tck(LANG, "rustjvm/TckLang", "sb_basic"),
    tck(LANG, "rustjvm/TckLang", "sb_appendInt"),
    tck(LANG, "rustjvm/TckLang", "sb_chain"),
    tck(LANG, "rustjvm/TckLang", "sb_reverse"),
    tck(LANG, "rustjvm/TckLang", "sb_delete"),
    // Exceptions
    tck(LANG, "rustjvm/TckLang", "exc_getMessage"),
    tck(LANG, "rustjvm/TckLang", "exc_getCause"),
    tck(LANG, "rustjvm/TckLang", "exc_tryCatch"),
    tck(LANG, "rustjvm/TckLang", "exc_hierarchy"),
    tck(LANG, "rustjvm/TckLang", "exc_npe_class"),
    tck(LANG, "rustjvm/TckLang", "exc_finally"),
    // Class (T4.2.2)
    tck(LANG, "rustjvm/TckLang", "cls_getName"),
    tck(LANG, "rustjvm/TckLang", "cls_isInterface"),
    tck(LANG, "rustjvm/TckLang", "cls_isPrimitive"),
    tck(LANG, "rustjvm/TckLang", "cls_isArray"),
    tck(LANG, "rustjvm/TckLang", "cls_getSuperclass"),
    // Thread (T4.2.18)
    tck(LANG, "rustjvm/TckLang", "thread_currentThread"),
    tck(LANG, "rustjvm/TckLang", "thread_isAlive"),
    // Casting
    tck(LANG, "rustjvm/TckLang", "cast_int_to_long"),
    tck(LANG, "rustjvm/TckLang", "cast_int_to_float"),
    tck(LANG, "rustjvm/TckLang", "autobox_int"),
    tck(LANG, "rustjvm/TckLang", "autobox_boolean"),
    // T4.2.4 — TckStringBuilder (dedicated)
    tck(LANG, "rustjvm/TckStringBuilder", "sb_empty"),
    tck(LANG, "rustjvm/TckStringBuilder", "sb_append_string"),
    tck(LANG, "rustjvm/TckStringBuilder", "sb_append_int"),
    tck(LANG, "rustjvm/TckStringBuilder", "sb_append_char"),
    tck(LANG, "rustjvm/TckStringBuilder", "sb_append_boolean"),
    tck(LANG, "rustjvm/TckStringBuilder", "sb_append_double"),
    tck(LANG, "rustjvm/TckStringBuilder", "sb_capacity"),
    tck(LANG, "rustjvm/TckStringBuilder", "sb_charAt"),
    tck(LANG, "rustjvm/TckStringBuilder", "sb_length"),
    tck(LANG, "rustjvm/TckStringBuilder", "sb_reverse"),
    tck(LANG, "rustjvm/TckStringBuilder", "sb_delete"),
    tck(LANG, "rustjvm/TckStringBuilder", "sb_insert"),
    tck(LANG, "rustjvm/TckStringBuilder", "sb_replace"),
    tck(LANG, "rustjvm/TckStringBuilder", "sb_substring"),
    tck(LANG, "rustjvm/TckStringBuilder", "sb_indexOf"),
    // T4.2.18 — TckThread (dedicated)
    tck(LANG, "rustjvm/TckThread", "thread_currentThread"),
    tck(LANG, "rustjvm/TckThread", "thread_getName"),
    tck(LANG, "rustjvm/TckThread", "thread_isAlive"),
    tck(LANG, "rustjvm/TckThread", "thread_priority"),
    tck(LANG, "rustjvm/TckThread", "thread_isDaemon"),
    tck(LANG, "rustjvm/TckThread", "thread_id"),
    tck(LANG, "rustjvm/TckThread", "thread_new_name"),
    tck(LANG, "rustjvm/TckThread", "thread_start_join"),
    tck(LANG, "rustjvm/TckThread", "thread_sleep"),
    tck(LANG, "rustjvm/TckThread", "thread_interrupt"),
    tck(LANG, "rustjvm/TckThread", "thread_state"),
    // T4.2.19 — TckStackTrace (dedicated)
    tck(LANG, "rustjvm/TckStackTrace", "ste_getClassName"),
    tck(LANG, "rustjvm/TckStackTrace", "ste_getMethodName"),
    tck(LANG, "rustjvm/TckStackTrace", "ste_getFileName"),
    tck(LANG, "rustjvm/TckStackTrace", "ste_getLineNumber"),
    tck(LANG, "rustjvm/TckStackTrace", "ste_toString"),
    tck(LANG, "rustjvm/TckStackTrace", "ste_constructor"),
    tck(LANG, "rustjvm/TckStackTrace", "ste_isNativeMethod"),
    tck(LANG, "rustjvm/TckStackTrace", "ste_equals"),
    // =====================================================================
    // java.util — T4.2.5 Collections
    // =====================================================================
    tck(UTIL, "rustjvm/TckUtil", "testArrayListBasic"),
    tck(UTIL, "rustjvm/TckUtil", "testArrayListMutations"),
    tck(UTIL, "rustjvm/TckUtil", "testArrayListGrow"),
    tck(UTIL, "rustjvm/TckUtil", "testArrayListIterator"),
    tck(UTIL, "rustjvm/TckUtil", "testArrayListInsert"),
    tck(UTIL, "rustjvm/TckUtil", "testArrayListLastIndexOf"),
    tck(UTIL, "rustjvm/TckUtil", "testHashMapBasic"),
    tck(UTIL, "rustjvm/TckUtil", "testHashMapMutations"),
    tck(UTIL, "rustjvm/TckUtil", "testHashMapIntegerKeys"),
    tck(UTIL, "rustjvm/TckUtil", "testHashMapGetOrDefault"),
    tck(UTIL, "rustjvm/TckUtil", "testHashMapPutIfAbsent"),
    tck(UTIL, "rustjvm/TckUtil", "testHashSetBasic"),
    tck(UTIL, "rustjvm/TckUtil", "testHashSetIterator"),
    tck(UTIL, "rustjvm/TckUtil", "testArraysSort"),
    tck(UTIL, "rustjvm/TckUtil", "testArraysCopyOf"),
    tck(UTIL, "rustjvm/TckUtil", "testArraysAsList"),
    tck(UTIL, "rustjvm/TckUtil", "testCollectionsEmptyList"),
    tck(UTIL, "rustjvm/TckUtil", "testCollectionsSingletonList"),
    tck(UTIL, "rustjvm/TckUtil", "testCollectionsReverse"),
    tck(UTIL, "rustjvm/TckUtil", "testOptionalBasic"),
    tck(UTIL, "rustjvm/TckUtil", "testOptionalOrElse"),
    tck(UTIL, "rustjvm/TckUtil", "testFrequencyMap"),
    tck(UTIL, "rustjvm/TckUtil", "testDeduplication"),
    tck(UTIL, "rustjvm/TckUtil", "testHashMapKeySet"),
    tck(UTIL, "rustjvm/TckUtil", "testArrayListCapacity"),
    tck(UTIL, "rustjvm/TckUtil", "testHashMapCapacity"),
    tck(UTIL, "rustjvm/TckUtil", "testArrayListToArray"),
    tck(UTIL, "rustjvm/TckUtil", "testHashMapNullKey"),
    tck(UTIL, "rustjvm/TckUtil", "linkedlist_basic"),
    tck(UTIL, "rustjvm/TckUtil", "treemap_basic"),
    tck(UTIL, "rustjvm/TckUtil", "arrays_sort"),
    tck(UTIL, "rustjvm/TckUtil", "optional_basic"),
    tck(UTIL, "rustjvm/TckUtil", "iterator_basic"),
    // T4.2.5 — TckCollections (dedicated)
    tck(UTIL, "rustjvm/TckCollections", "sort_integers"),
    tck(UTIL, "rustjvm/TckCollections", "unmodifiable_list"),
    tck(UTIL, "rustjvm/TckCollections", "singleton_list"),
    tck(UTIL, "rustjvm/TckCollections", "empty_list"),
    tck(UTIL, "rustjvm/TckCollections", "empty_map"),
    tck(UTIL, "rustjvm/TckCollections", "empty_set"),
    tck(UTIL, "rustjvm/TckCollections", "frequency"),
    tck(UTIL, "rustjvm/TckCollections", "max_min"),
    tck(UTIL, "rustjvm/TckCollections", "reverse"),
    tck(UTIL, "rustjvm/TckCollections", "singleton_map"),
    // T4.2.6 — TckAtomic (concurrent.atomic)
    tck(CONCURRENT, "rustjvm/TckAtomic", "atomic_int_get_set"),
    tck(CONCURRENT, "rustjvm/TckAtomic", "atomic_int_getAndSet"),
    tck(CONCURRENT, "rustjvm/TckAtomic", "atomic_int_cas"),
    tck(CONCURRENT, "rustjvm/TckAtomic", "atomic_int_cas_fail"),
    tck(CONCURRENT, "rustjvm/TckAtomic", "atomic_int_incr"),
    tck(CONCURRENT, "rustjvm/TckAtomic", "atomic_int_addAndGet"),
    tck(CONCURRENT, "rustjvm/TckAtomic", "atomic_long_basic"),
    tck(CONCURRENT, "rustjvm/TckAtomic", "atomic_long_cas"),
    tck(CONCURRENT, "rustjvm/TckAtomic", "atomic_ref_basic"),
    tck(CONCURRENT, "rustjvm/TckAtomic", "atomic_ref_cas"),
    tck(CONCURRENT, "rustjvm/TckAtomic", "atomic_boolean"),
    // T4.2.7 — TckLocks (concurrent.locks)
    tck(CONCURRENT, "rustjvm/TckLocks", "reentrant_lock_unlock"),
    tck(CONCURRENT, "rustjvm/TckLocks", "reentrant_held"),
    tck(CONCURRENT, "rustjvm/TckLocks", "reentrant_tryLock"),
    tck(CONCURRENT, "rustjvm/TckLocks", "reentrant_reentrant"),
    tck(CONCURRENT, "rustjvm/TckLocks", "rwlock_read"),
    tck(CONCURRENT, "rustjvm/TckLocks", "rwlock_write"),
    tck(CONCURRENT, "rustjvm/TckLocks", "condition_basic"),
    tck(CONCURRENT, "rustjvm/TckLocks", "stampedlock_basic"),
    // T4.2.8 — TckRegex (regex.Pattern)
    tck(REGEX, "rustjvm/TckRegex", "pattern_compile"),
    tck(REGEX, "rustjvm/TckRegex", "pattern_matches"),
    tck(REGEX, "rustjvm/TckRegex", "pattern_no_match"),
    tck(REGEX, "rustjvm/TckRegex", "matcher_find"),
    tck(REGEX, "rustjvm/TckRegex", "matcher_group"),
    tck(REGEX, "rustjvm/TckRegex", "matcher_replaceAll"),
    tck(REGEX, "rustjvm/TckRegex", "matcher_replaceFirst"),
    tck(REGEX, "rustjvm/TckRegex", "pattern_split"),
    tck(REGEX, "rustjvm/TckRegex", "pattern_quote"),
    tck(REGEX, "rustjvm/TckRegex", "pattern_flags"),
    tck(REGEX, "rustjvm/TckRegex", "pattern_toString"),
    // =====================================================================
    // java.io — T4.2.9 Reader, T4.2.10 PrintStream
    // =====================================================================
    tck(IO, "rustjvm/TckIo", "baos_basic"),
    tck(IO, "rustjvm/TckIo", "baos_size"),
    tck(IO, "rustjvm/TckIo", "baos_reset"),
    tck(IO, "rustjvm/TckIo", "bais_readAll"),
    tck(IO, "rustjvm/TckIo", "bais_available"),
    tck(IO, "rustjvm/TckIo", "bais_skip"),
    tck(IO, "rustjvm/TckIo", "baos_toString"),
    tck(IO, "rustjvm/TckIo", "sw_basic"),
    tck(IO, "rustjvm/TckIo", "sr_readChar"),
    tck(IO, "rustjvm/TckIo", "file_createDeleteExists"),
    tck(IO, "rustjvm/TckIo", "fos_writeSingleByte"),
    tck(IO, "rustjvm/TckIo", "fos_writeBulk"),
    tck(IO, "rustjvm/TckIo", "fis_readEof"),
    tck(IO, "rustjvm/TckIo", "fis_available"),
    tck(IO, "rustjvm/TckIo", "fis_skip"),
    tck(IO, "rustjvm/TckIo", "e2e_writeReadRoundtrip"),
    tck(IO, "rustjvm/TckIo", "e2e_baosToInputStream"),
    tck(IO, "rustjvm/TckIo", "bytearray_inputstream_basic"),
    tck(IO, "rustjvm/TckIo", "buffered_output_basic"),
    // T4.2.9 — TckReader (dedicated)
    tck(IO, "rustjvm/TckReader", "stringreader_read"),
    tck(IO, "rustjvm/TckReader", "stringreader_readArray"),
    tck(IO, "rustjvm/TckReader", "stringreader_mark_reset"),
    tck(IO, "rustjvm/TckReader", "stringreader_ready"),
    tck(IO, "rustjvm/TckReader", "stringreader_close"),
    tck(IO, "rustjvm/TckReader", "bufferedreader_readLine"),
    tck(IO, "rustjvm/TckReader", "bufferedreader_mark_reset"),
    tck(IO, "rustjvm/TckReader", "inputstreamreader_basic"),
    // T4.2.10 — TckPrintStream (dedicated)
    tck(IO, "rustjvm/TckPrintStream", "baos_write_toByteArray"),
    tck(IO, "rustjvm/TckPrintStream", "baos_size"),
    tck(IO, "rustjvm/TckPrintStream", "baos_reset"),
    tck(IO, "rustjvm/TckPrintStream", "baos_toString"),
    tck(IO, "rustjvm/TckPrintStream", "printstream_print"),
    tck(IO, "rustjvm/TckPrintStream", "printstream_println"),
    tck(IO, "rustjvm/TckPrintStream", "printwriter_basic"),
    tck(IO, "rustjvm/TckPrintStream", "printwriter_println"),
    tck(IO, "rustjvm/TckPrintStream", "data_io_int"),
    tck(IO, "rustjvm/TckPrintStream", "data_io_utf"),
    // =====================================================================
    // java.nio — T4.2.11 FileChannel, T4.2.12 Files
    // =====================================================================
    tck(NIO, "rustjvm/TckNio", "bb_allocate"),
    tck(NIO, "rustjvm/TckNio", "bb_wrap"),
    tck(NIO, "rustjvm/TckNio", "bb_put_get_sequential"),
    tck(NIO, "rustjvm/TckNio", "bb_flip"),
    tck(NIO, "rustjvm/TckNio", "bb_clear"),
    tck(NIO, "rustjvm/TckNio", "bb_put_int"),
    tck(NIO, "rustjvm/TckNio", "bb_little_endian"),
    tck(NIO, "rustjvm/TckNio", "bb_remaining"),
    tck(NIO, "rustjvm/TckNio", "ib_wrap"),
    tck(NIO, "rustjvm/TckNio", "bb_has_remaining"),
    tck(NIO, "rustjvm/TckNio", "bb_duplicate"),
    tck(NIO, "rustjvm/TckNio", "bb_slice"),
    tck(NIO, "rustjvm/TckNio", "bb_array"),
    // T4.2.11 — TckFileChannel (dedicated)
    tck(NIO, "rustjvm/TckFileChannel", "fc_open_write_read"),
    tck(NIO, "rustjvm/TckFileChannel", "fc_position"),
    tck(NIO, "rustjvm/TckFileChannel", "fc_size"),
    tck(NIO, "rustjvm/TckFileChannel", "fc_truncate"),
    // T4.2.12 — TckFiles (dedicated)
    tck(NIO, "rustjvm/TckFiles", "files_createTempFile"),
    tck(NIO, "rustjvm/TckFiles", "files_write_readAllBytes"),
    tck(NIO, "rustjvm/TckFiles", "files_delete"),
    tck(NIO, "rustjvm/TckFiles", "files_isDirectory"),
    tck(NIO, "rustjvm/TckFiles", "files_isRegularFile"),
    tck(NIO, "rustjvm/TckFiles", "files_size"),
    tck(NIO, "rustjvm/TckFiles", "files_copy"),
    tck(NIO, "rustjvm/TckFiles", "files_move"),
    // =====================================================================
    // java.net
    // =====================================================================
    tck(NET, "rustjvm/TckNet", "url_getProtocol"),
    tck(NET, "rustjvm/TckNet", "url_getHost"),
    tck(NET, "rustjvm/TckNet", "url_getPort_explicit"),
    tck(NET, "rustjvm/TckNet", "url_getPort_default"),
    tck(NET, "rustjvm/TckNet", "url_getPath"),
    tck(NET, "rustjvm/TckNet", "url_getQuery"),
    tck(NET, "rustjvm/TckNet", "url_malformed_throws"),
    tck(NET, "rustjvm/TckNet", "uri_parse"),
    tck(NET, "rustjvm/TckNet", "uri_getScheme"),
    tck(NET, "rustjvm/TckNet", "uri_relative"),
    // =====================================================================
    // java.security — T4.5.1-T4.5.6
    // =====================================================================
    tck(SECURITY, "rustjvm/TckSecurity", "md_sha256_empty"),
    tck(SECURITY, "rustjvm/TckSecurity", "md_sha256_abc"),
    tck(SECURITY, "rustjvm/TckSecurity", "md_sha1_length"),
    tck(SECURITY, "rustjvm/TckSecurity", "md_md5_length"),
    tck(SECURITY, "rustjvm/TckSecurity", "md_getAlgorithm"),
    tck(SECURITY, "rustjvm/TckSecurity", "md_reset"),
    tck(SECURITY, "rustjvm/TckSecurity", "md_unknown_throws"),
    tck(SECURITY, "rustjvm/TckSecurity", "sr_nextBytes"),
    tck(SECURITY, "rustjvm/TckSecurity", "sr_nextInt"),
    tck(SECURITY, "rustjvm/TckSecurity", "md_incremental_update"),
    // T4.5 extended — Cipher, KeyStore, Signature, Provider, KeyPairGenerator, Mac
    tck(SECURITY, "rustjvm/TckSecurity", "cipher_getInstance"),
    tck(SECURITY, "rustjvm/TckSecurity", "cipher_getAlgorithm"),
    tck(SECURITY, "rustjvm/TckSecurity", "keyStore_getInstance"),
    tck(SECURITY, "rustjvm/TckSecurity", "signature_getInstance"),
    tck(SECURITY, "rustjvm/TckSecurity", "signature_getAlgorithm"),
    tck(SECURITY, "rustjvm/TckSecurity", "provider_getName"),
    tck(SECURITY, "rustjvm/TckSecurity", "keypairgen_getInstance"),
    tck(SECURITY, "rustjvm/TckSecurity", "keypairgen_getAlgorithm"),
    tck(SECURITY, "rustjvm/TckSecurity", "mac_getInstance"),
    // =====================================================================
    // java.lang.reflect
    // =====================================================================
    tck(REFLECT, "rustjvm/TckReflect", "cls_forName"),
    tck(REFLECT, "rustjvm/TckReflect", "cls_getName"),
    tck(REFLECT, "rustjvm/TckReflect", "cls_getSimpleName"),
    tck(REFLECT, "rustjvm/TckReflect", "cls_getSuperclass"),
    tck(REFLECT, "rustjvm/TckReflect", "cls_isInterface"),
    tck(REFLECT, "rustjvm/TckReflect", "cls_isPrimitive"),
    tck(REFLECT, "rustjvm/TckReflect", "cls_isArray"),
    tck(REFLECT, "rustjvm/TckReflect", "cls_isEnum"),
    tck(REFLECT, "rustjvm/TckReflect", "cls_getModifiers"),
    tck(REFLECT, "rustjvm/TckReflect", "cls_isAssignableFrom"),
    tck(REFLECT, "rustjvm/TckReflect", "cls_isInstance"),
    tck(REFLECT, "rustjvm/TckReflect", "cls_getInterfaces"),
    tck(REFLECT, "rustjvm/TckReflect", "cls_getComponentType"),
    tck(REFLECT, "rustjvm/TckReflect", "cls_cast"),
    tck(REFLECT, "rustjvm/TckReflect", "cls_newInstance"),
    tck(REFLECT, "rustjvm/TckReflect", "meth_getDeclaredMethod"),
    tck(REFLECT, "rustjvm/TckReflect", "meth_invokeInstance"),
    tck(REFLECT, "rustjvm/TckReflect", "meth_invokeStatic"),
    tck(REFLECT, "rustjvm/TckReflect", "meth_invokePrivate"),
    tck(REFLECT, "rustjvm/TckReflect", "meth_getReturnType"),
    tck(REFLECT, "rustjvm/TckReflect", "meth_getParameterTypes"),
    // =====================================================================
    // java.sql — T4.4.1-T4.4.10
    // =====================================================================
    tck(JDBC, "rustjvm/TckJdbc", "open_inmemory_connection"),
    tck(JDBC, "rustjvm/TckJdbc", "statement_ddl_dml_query"),
    tck(JDBC, "rustjvm/TckJdbc", "prepared_statement_binds_and_executes"),
    tck(JDBC, "rustjvm/TckJdbc", "rollback_discards_changes"),
    tck(JDBC, "rustjvm/TckJdbc", "savepoint_rollback_and_release"),
    tck(JDBC, "rustjvm/TckJdbc", "blob_round_trip"),
    tck(JDBC, "rustjvm/TckJdbc", "clob_round_trip"),
    tck(JDBC, "rustjvm/TckJdbc", "callable_inherits_prepared"),
    tck(JDBC, "rustjvm/TckJdbc", "metadata_identifies_sqlite"),
    tck(JDBC, "rustjvm/TckJdbc", "driver_register_list_deregister"),
    tck(JDBC, "rustjvm/TckJdbc", "e2e_mini_app"),
    // T4.4 — TckSql (SQL constants, no DB)
    tck(SQL, "rustjvm/TckSql", "types_integer"),
    tck(SQL, "rustjvm/TckSql", "types_varchar"),
    tck(SQL, "rustjvm/TckSql", "types_bigint"),
    tck(SQL, "rustjvm/TckSql", "types_double"),
    tck(SQL, "rustjvm/TckSql", "types_timestamp"),
    tck(SQL, "rustjvm/TckSql", "sqlException_message"),
    tck(SQL, "rustjvm/TckSql", "sqlException_state"),
    tck(SQL, "rustjvm/TckSql", "sqlException_code"),
    tck(SQL, "rustjvm/TckSql", "sqlException_chain"),
    tck(SQL, "rustjvm/TckSql", "resultSet_types"),
    tck(SQL, "rustjvm/TckSql", "connection_isolation"),
    tck(SQL, "rustjvm/TckSql", "statement_no_keys"),
    // =====================================================================
    // java.time — T4.2.13 Instant, T4.2.14 ZonedDateTime, T4.2.15 DateTimeFormatter
    // =====================================================================
    tck(TIME, "rustjvm/TckInstant", "instant_now"),
    tck(TIME, "rustjvm/TckInstant", "instant_epoch"),
    tck(TIME, "rustjvm/TckInstant", "instant_ofEpochSecond"),
    tck(TIME, "rustjvm/TckInstant", "instant_plusSeconds"),
    tck(TIME, "rustjvm/TckInstant", "instant_minusSeconds"),
    tck(TIME, "rustjvm/TckInstant", "instant_compareTo"),
    tck(TIME, "rustjvm/TckInstant", "instant_isBefore"),
    tck(TIME, "rustjvm/TckInstant", "instant_isAfter"),
    tck(TIME, "rustjvm/TckInstant", "instant_toEpochMilli"),
    tck(TIME, "rustjvm/TckInstant", "instant_toString"),
    tck(TIME, "rustjvm/TckInstant", "instant_equals"),
    tck(TIME, "rustjvm/TckZonedDateTime", "zdt_now"),
    tck(TIME, "rustjvm/TckZonedDateTime", "zdt_of"),
    tck(TIME, "rustjvm/TckZonedDateTime", "zdt_getMonth"),
    tck(TIME, "rustjvm/TckZonedDateTime", "zdt_getDayOfMonth"),
    tck(TIME, "rustjvm/TckZonedDateTime", "zdt_getHour"),
    tck(TIME, "rustjvm/TckZonedDateTime", "zdt_getZone"),
    tck(TIME, "rustjvm/TckZonedDateTime", "zdt_toInstant"),
    tck(TIME, "rustjvm/TckZonedDateTime", "zdt_plusDays"),
    tck(TIME, "rustjvm/TckZonedDateTime", "zdt_minusHours"),
    tck(TIME, "rustjvm/TckZonedDateTime", "zdt_withZoneSameInstant"),
    tck(TIME, "rustjvm/TckLocalDate", "ld_now"),
    tck(TIME, "rustjvm/TckLocalDate", "ld_of"),
    tck(TIME, "rustjvm/TckLocalDate", "ld_parse"),
    tck(TIME, "rustjvm/TckLocalDate", "ld_plusDays"),
    tck(TIME, "rustjvm/TckLocalDate", "ld_minusMonths"),
    tck(TIME, "rustjvm/TckLocalDate", "ld_isLeapYear"),
    tck(TIME, "rustjvm/TckLocalDate", "ld_dayOfWeek"),
    tck(TIME, "rustjvm/TckLocalDate", "ld_compareTo"),
    tck(TIME, "rustjvm/TckLocalDate", "ld_format"),
    tck(TIME, "rustjvm/TckLocalDate", "ld_toString"),
    // =====================================================================
    // java.text — T4.2.16 DecimalFormat, T4.2.17 MessageFormat
    // =====================================================================
    tck(TEXT, "rustjvm/TckDecimalFormat", "df_format_int"),
    tck(TEXT, "rustjvm/TckDecimalFormat", "df_format_double"),
    tck(TEXT, "rustjvm/TckDecimalFormat", "df_pattern"),
    tck(TEXT, "rustjvm/TckDecimalFormat", "df_parse"),
    tck(TEXT, "rustjvm/TckDecimalFormat", "df_negative"),
    tck(TEXT, "rustjvm/TckDecimalFormat", "df_grouping"),
    tck(TEXT, "rustjvm/TckDecimalFormat", "df_percent"),
    tck(TEXT, "rustjvm/TckDecimalFormat", "df_max_fraction"),
    tck(TEXT, "rustjvm/TckMessageFormat", "mf_basic"),
    tck(TEXT, "rustjvm/TckMessageFormat", "mf_multiple_args"),
    tck(TEXT, "rustjvm/TckMessageFormat", "mf_number"),
    tck(TEXT, "rustjvm/TckMessageFormat", "mf_repeated_arg"),
    tck(TEXT, "rustjvm/TckMessageFormat", "mf_no_args"),
    tck(TEXT, "rustjvm/TckMessageFormat", "mf_null_safe"),
    tck(TEXT, "rustjvm/TckMessageFormat", "mf_escape_single_quote"),
    tck(TEXT, "rustjvm/TckMessageFormat", "mf_toPattern"),
    // =====================================================================
    // java.math — T4.2.20 BigInteger/BigDecimal
    // =====================================================================
    tck(MATH, "rustjvm/TckBigMath", "bi_add"),
    tck(MATH, "rustjvm/TckBigMath", "bi_subtract"),
    tck(MATH, "rustjvm/TckBigMath", "bi_multiply"),
    tck(MATH, "rustjvm/TckBigMath", "bi_divide"),
    tck(MATH, "rustjvm/TckBigMath", "bi_mod"),
    tck(MATH, "rustjvm/TckBigMath", "bi_compareTo"),
    tck(MATH, "rustjvm/TckBigMath", "bi_toString"),
    tck(MATH, "rustjvm/TckBigMath", "bi_valueOf"),
    tck(MATH, "rustjvm/TckBigMath", "bi_bitLength"),
    tck(MATH, "rustjvm/TckBigMath", "bd_add"),
    tck(MATH, "rustjvm/TckBigMath", "bd_subtract"),
    tck(MATH, "rustjvm/TckBigMath", "bd_multiply"),
    tck(MATH, "rustjvm/TckBigMath", "bd_scale"),
    tck(MATH, "rustjvm/TckBigMath", "bd_toString"),
    tck(MATH, "rustjvm/TckBigMath", "bd_compareTo"),
    // =====================================================================
    // java.net.http — T4.3.1-T4.3.10
    // =====================================================================
    tck(HTTP, "rustjvm/TckHttpClient", "httpClient_newBuilder"),
    tck(HTTP, "rustjvm/TckHttpClient", "httpClient_version"),
    tck(HTTP, "rustjvm/TckHttpClient", "httpClient_followRedirects"),
    tck(HTTP, "rustjvm/TckHttpClient", "httpRequest_builder"),
    tck(HTTP, "rustjvm/TckHttpClient", "httpRequest_method"),
    tck(HTTP, "rustjvm/TckHttpClient", "httpRequest_uri"),
    tck(HTTP, "rustjvm/TckHttpClient", "httpRequest_timeout"),
    tck(HTTP, "rustjvm/TckHttpClient", "httpRequest_headers_empty"),
    tck(HTTP, "rustjvm/TckHttpClient", "bodyHandlers_ofString"),
    tck(HTTP, "rustjvm/TckHttpClient", "bodyHandlers_discarding"),
    // =====================================================================
    // java.management — T4.6.1-T4.6.3
    // =====================================================================
    tck(MANAGEMENT, "rustjvm/TckManagement", "mbean_server"),
    tck(MANAGEMENT, "rustjvm/TckManagement", "runtime_mxbean"),
    tck(MANAGEMENT, "rustjvm/TckManagement", "runtime_name"),
    tck(MANAGEMENT, "rustjvm/TckManagement", "memory_mxbean"),
    tck(MANAGEMENT, "rustjvm/TckManagement", "thread_mxbean"),
    tck(MANAGEMENT, "rustjvm/TckManagement", "thread_count"),
    tck(MANAGEMENT, "rustjvm/TckManagement", "classloading_mxbean"),
    tck(MANAGEMENT, "rustjvm/TckManagement", "os_mxbean"),
    tck(MANAGEMENT, "rustjvm/TckManagement", "os_name"),
];

const fn tck(category: Category, class: &'static str, method: &'static str) -> TckTest {
    TckTest { category, class, method }
}

// ---------------------------------------------------------------------------
// Execution + classification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Pass,
    Fail,
    Error,
}

fn run_one(t: &TckTest) -> Outcome {
    // Use a fresh VM per test. Sharing one VM across the full corpus
    // causes accumulated state (e.g. cached class-loader errors) to bleed
    // from one test into the next and produce spurious failures. This
    // mirrors how `interpreter_tests.rs` constructs a VM per test.
    let mut vm = test_vm();
    match vm.invoke(t.class, t.method, "()I", &[]) {
        Ok(Some(Value::Int(1))) => Outcome::Pass,
        Ok(Some(Value::Int(_))) => Outcome::Fail,
        Ok(_) => Outcome::Fail,
        Err(_) => Outcome::Error,
    }
}

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn class_files_available() -> bool {
    let dir = test_resources_dir();
    std::path::Path::new(&format!("{dir}/rustjvm/TckClassFile.class")).exists()
}

fn test_vm() -> Vm {
    let config = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    Vm::new(config)
}

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    pass: u32,
    fail: u32,
    error: u32,
}

impl Tally {
    fn total(&self) -> u32 {
        self.pass + self.fail + self.error
    }
    fn record(&mut self, o: Outcome) {
        match o {
            Outcome::Pass => self.pass += 1,
            Outcome::Fail => self.fail += 1,
            Outcome::Error => self.error += 1,
        }
    }
}

/// Run the full corpus and return a map of category -> tally.
fn run_corpus() -> std::collections::BTreeMap<&'static str, Tally> {
    let mut tallies: std::collections::BTreeMap<&'static str, Tally> =
        std::collections::BTreeMap::new();
    for t in CORPUS {
        let outcome = run_one(t);
        tallies.entry(t.category.0).or_default().record(outcome);
        if outcome != Outcome::Pass {
            eprintln!(
                "[jck] {:?} {}::{}",
                outcome, t.class, t.method
            );
        }
    }
    tallies
}

// ---------------------------------------------------------------------------
// Baseline report (committed under docs/jdk-regression-baseline.md)
// ---------------------------------------------------------------------------

/// Per-category pass floors. CI fails if the current pass count drops below
/// any of these values. Raise the floor only after a deliberate improvement
/// has landed AND the new number is reproducible on a clean build.
///
/// These numbers must match `docs/jdk-regression-baseline.md`.
const BASELINE_FLOORS: &[(&str, u32)] = &[
    // Updated 2026-04-16 after T4.2-T4.6 corpus expansion.
    // Total corpus: 421 tests, 109 pass on first run.
    ("ClassFile", 4),     // 4/5 pass
    ("Concurrent", 8),    // 8/19 — AtomicInteger/Long basic ops pass
    ("Http", 0),          // 0/10 — java.net.http not yet wired
    ("Instructions", 9),  // 9/13
    ("Io", 18),           // 18-20/37 — BAOS, BAIS, File I/O, StringWriter (slight variance)
    ("Jdbc", 0),          // 0/11 — JDBC wired but not through TCK path
    ("Lang", 34),         // 34/109 — core types, wrappers, math, system
    ("Loading", 4),       // 4/5
    ("Management", 0),    // 0/9 — MXBeans not yet wired
    ("Math", 3),          // 3/15 — BigInteger basic ops
    ("Net", 0),           // 0/10 — URL/URI constructors
    ("Nio", 11),          // 11/25 — ByteBuffer core ops
    ("Reflect", 5),       // 5/21 — Class metadata basics
    ("Regex", 0),         // 0/11 — Pattern/Matcher not through TCK path
    ("Security", 0),      // 0/19 — crypto not through TCK path
    ("Sql", 8),           // 8/12 — java.sql constants pass
    ("Text", 0),          // 0/16 — DecimalFormat/MessageFormat
    ("Time", 0),          // 0/31 — java.time not yet wired
    ("Util", 3),          // 3/43 — basic collections
];

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn jck_full_corpus_runs() {
    if !class_files_available() {
        eprintln!("Skipping jck_full_corpus_runs: .class files not available");
        return;
    }
    let tallies = run_corpus();

    // Print full summary
    eprintln!();
    eprintln!("=== NEW-16 JCK Conformance Summary ===");
    let mut grand = Tally::default();
    for (cat, t) in &tallies {
        eprintln!(
            "  {:<14} pass={:>3}  fail={:>3}  error={:>3}  total={:>3}",
            cat, t.pass, t.fail, t.error, t.total()
        );
        grand.pass += t.pass;
        grand.fail += t.fail;
        grand.error += t.error;
    }
    eprintln!(
        "  {:<14} pass={:>3}  fail={:>3}  error={:>3}  total={:>3}",
        "TOTAL", grand.pass, grand.fail, grand.error, grand.total()
    );
    eprintln!();

    assert!(
        grand.total() > 0,
        "JCK corpus ran zero tests — check the CORPUS table"
    );
}

#[test]
fn jck_regression_gate() {
    if !class_files_available() {
        eprintln!("Skipping jck_regression_gate: .class files not available");
        return;
    }
    let tallies = run_corpus();

    let mut failures: Vec<String> = Vec::new();
    for (cat, floor) in BASELINE_FLOORS {
        let current = tallies.get(cat).copied().unwrap_or_default().pass;
        if current < *floor {
            failures.push(format!(
                "regression in {cat}: current pass={current} < floor={floor}"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "NEW-16 regression gate tripped:\n  {}\n\n\
         The regression gate enforces the committed baseline in \
         docs/jdk-regression-baseline.md. If a regression is intentional \
         (e.g. a test was removed), update BASELINE_FLOORS and the baseline \
         doc together in the same commit.",
        failures.join("\n  ")
    );
}
