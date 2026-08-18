// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! NEW-16: JCK-style java.base conformance harness.
//!
//! Runs the curated `Tck*` test corpus under `tests/resources/cratonvm/` through
//! the VM and records pass/fail per java.base category (Lang, Util, Io, Nio,
//! Net, Security, Reflect, Loading, ClassFile, Instructions, Jdbc). Produces a
//! machine-checkable summary and a regression gate.
//!
//! The baseline report lives at `internal/gaps/jdk-regression-baseline.md`.
//! Each category has a floor — CI fails if the pass count drops below the
//! committed floor.
//!
//! Convention for the corpus:
//!   - Every test is `public static int testName()` returning 1 on pass, 0 on fail.
//!   - Tests must not take arguments, perform I/O, or depend on external state.
//!
//! # Two layers, and why the file-level `#[cfg]` came off (2026-08-13)
//!
//! Line 4 of this file used to be `#![cfg(feature = "synthetic-jdk")]`, so the
//! entire harness — including the thing named `jck_regression_gate`, whose own
//! message says it "enforces the committed baseline" — **did not exist in the
//! configuration CI builds**. No job runs `vm/tests/*` with that feature: the
//! `synthetic-jdk` job only `cargo check --all-targets`s them and runs `--lib`
//! scopes, while the blocking `cargo test --workspace` job uses default
//! features. Behind that, `jck_regression_gate` opened with
//! `if !class_files_available() { eprintln!("Skipping…"); return; }`, so even
//! with the feature it returned green in 0.00 s on a machine with no corpus.
//! Two dark layers over a gate. This document's own "How to run" section still
//! recommends `cargo test -p cratonvm-vm --test jck_conformance` with default
//! features — a command that ran zero tests. (E25 sweep,
//! `docs/known-issues/jdk-only/E25-R11-GUARD-POPULATION-SWEEP-20260813.md`
//! section 4.1, row 32.)
//!
//! The harness is now split by what it needs, not by cargo feature:
//!
//!   * **The tables** — `CORPUS` and `BASELINE_FLOORS` — need no VM and no
//!     `.class` files, so the checks over them are UNGATED and run in
//!     `cargo test --workspace`. They cross-check the two tables against each
//!     other and against the committed baseline document, which is a file
//!     outside this crate that a person edits by hand.
//!   * **The corpus run** needs a VM built against synthetic-JDK stubs and a
//!     `javac`-produced corpus. That stays behind
//!     `#[cfg(feature = "synthetic-jdk")]` — the original comment's reason is a
//!     good one ("the default VM build uses real JDK bytecode and must not run
//!     this long synthetic harness") — and its skip is now LOUD and promotable
//!     to a failure through `CRATONVM_REQUIRE_E2E` (`vm/tests/common/mod.rs`).
//!
//! Prerequisites for the gated half: javac on PATH (see `build.rs`).

// Every VM-touching item in this file carries the same gate. It is applied
// per-item rather than as a `#![cfg]` on the file so that the table checks
// below stay in the default build.
#[cfg(feature = "synthetic-jdk")]
use cratonvm_vm::config::VmConfig;
#[cfg(feature = "synthetic-jdk")]
use cratonvm_vm::types::Value;
#[cfg(feature = "synthetic-jdk")]
use cratonvm_vm::vm::Vm;

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
    tck(CLASSFILE, "cratonvm/TckClassFile", "testMagicNumber"),
    tck(CLASSFILE, "cratonvm/TckClassFile", "testClassVersion"),
    tck(CLASSFILE, "cratonvm/TckClassFile", "testConstantPool"),
    tck(CLASSFILE, "cratonvm/TckClassFile", "testFieldAccess"),
    tck(CLASSFILE, "cratonvm/TckClassFile", "testMethodAccess"),
    // =====================================================================
    // Loading / init
    // =====================================================================
    tck(LOADING, "cratonvm/TckLoading", "testClassLoading"),
    tck(LOADING, "cratonvm/TckLoading", "testStaticInit"),
    tck(LOADING, "cratonvm/TckLoading", "testInterfaceInit"),
    tck(LOADING, "cratonvm/TckLoading", "testArrayCreation"),
    tck(LOADING, "cratonvm/TckLoading", "testInheritance"),
    // =====================================================================
    // Instructions (JVMS Ch. 6)
    // =====================================================================
    tck(
        INSTRUCTIONS,
        "cratonvm/TckInstructions",
        "testIntArithmetic",
    ),
    tck(
        INSTRUCTIONS,
        "cratonvm/TckInstructions",
        "testLongArithmetic",
    ),
    tck(
        INSTRUCTIONS,
        "cratonvm/TckInstructions",
        "testFloatArithmetic",
    ),
    tck(INSTRUCTIONS, "cratonvm/TckInstructions", "testComparisons"),
    tck(INSTRUCTIONS, "cratonvm/TckInstructions", "testTableswitch"),
    tck(INSTRUCTIONS, "cratonvm/TckInstructions", "testLookupswitch"),
    tck(INSTRUCTIONS, "cratonvm/TckInstructions", "testFieldOps"),
    tck(INSTRUCTIONS, "cratonvm/TckInstructions", "testArrayOps"),
    tck(
        INSTRUCTIONS,
        "cratonvm/TckInstructions",
        "testInvokeVirtual",
    ),
    tck(INSTRUCTIONS, "cratonvm/TckInstructions", "testInvokeStatic"),
    tck(
        INSTRUCTIONS,
        "cratonvm/TckInstructions",
        "testExceptionHandling",
    ),
    tck(INSTRUCTIONS, "cratonvm/TckInstructions", "testCheckcast"),
    tck(INSTRUCTIONS, "cratonvm/TckInstructions", "testInstanceof"),
    // =====================================================================
    // java.lang — T4.2.1 Object, T4.2.2 Class, T4.2.3 String, T4.2.4 StringBuilder
    // =====================================================================
    // Object
    tck(LANG, "cratonvm/TckLang", "obj_hashCode_consistent"),
    tck(LANG, "cratonvm/TckLang", "obj_equals_identity"),
    tck(LANG, "cratonvm/TckLang", "obj_equals_different"),
    tck(LANG, "cratonvm/TckLang", "obj_getClass"),
    tck(LANG, "cratonvm/TckLang", "obj_toString"),
    // String
    tck(LANG, "cratonvm/TckLang", "str_equals"),
    tck(LANG, "cratonvm/TckLang", "str_compareTo"),
    tck(LANG, "cratonvm/TckLang", "str_contains"),
    tck(LANG, "cratonvm/TckLang", "str_isEmpty"),
    tck(LANG, "cratonvm/TckLang", "str_startsEndsWith"),
    tck(LANG, "cratonvm/TckLang", "str_replace"),
    tck(LANG, "cratonvm/TckLang", "str_toCharArray"),
    tck(LANG, "cratonvm/TckLang", "str_toLowerCase"),
    tck(LANG, "cratonvm/TckLang", "str_toUpperCase"),
    tck(LANG, "cratonvm/TckLang", "str_valueOf_int"),
    tck(LANG, "cratonvm/TckLang", "str_valueOf_bool"),
    tck(LANG, "cratonvm/TckLang", "str_concat"),
    tck(LANG, "cratonvm/TckLang", "str_constructor_chars"),
    // Integer/Long/Double/Float/Boolean wrappers
    tck(LANG, "cratonvm/TckLang", "int_autobox_cache"),
    tck(LANG, "cratonvm/TckLang", "int_compareTo"),
    tck(LANG, "cratonvm/TckLang", "int_constants"),
    tck(LANG, "cratonvm/TckLang", "long_toString"),
    tck(LANG, "cratonvm/TckLang", "long_maxValue"),
    tck(LANG, "cratonvm/TckLang", "double_isNaN"),
    tck(LANG, "cratonvm/TckLang", "double_isInfinite"),
    tck(LANG, "cratonvm/TckLang", "double_bits_roundtrip"),
    tck(LANG, "cratonvm/TckLang", "float_isNaN"),
    tck(LANG, "cratonvm/TckLang", "float_bits_roundtrip"),
    tck(LANG, "cratonvm/TckLang", "bool_parseBoolean"),
    tck(LANG, "cratonvm/TckLang", "bool_valueOf"),
    tck(LANG, "cratonvm/TckLang", "bool_toString"),
    // Byte/Short/Character
    tck(LANG, "cratonvm/TckLang", "byte_constants"),
    tck(LANG, "cratonvm/TckLang", "short_constants"),
    tck(LANG, "cratonvm/TckLang", "char_isDigit"),
    tck(LANG, "cratonvm/TckLang", "char_isLetter"),
    tck(LANG, "cratonvm/TckLang", "char_case"),
    tck(LANG, "cratonvm/TckLang", "char_convert"),
    tck(LANG, "cratonvm/TckLang", "char_isWhitespace"),
    // Math
    tck(LANG, "cratonvm/TckLang", "math_abs"),
    tck(LANG, "cratonvm/TckLang", "math_maxMin"),
    tck(LANG, "cratonvm/TckLang", "math_sqrt"),
    tck(LANG, "cratonvm/TckLang", "math_pow"),
    tck(LANG, "cratonvm/TckLang", "math_floorCeil"),
    tck(LANG, "cratonvm/TckLang", "math_round"),
    tck(LANG, "cratonvm/TckLang", "math_constants"),
    tck(LANG, "cratonvm/TckLang", "math_sinCos"),
    tck(LANG, "cratonvm/TckLang", "math_logExp"),
    // System
    tck(LANG, "cratonvm/TckLang", "sys_currentTimeMillis"),
    tck(LANG, "cratonvm/TckLang", "sys_nanoTime"),
    tck(LANG, "cratonvm/TckLang", "sys_arraycopy"),
    tck(LANG, "cratonvm/TckLang", "sys_identityHashCode"),
    // Runtime
    tck(LANG, "cratonvm/TckLang", "rt_availableProcessors"),
    tck(LANG, "cratonvm/TckLang", "rt_memory"),
    // StringBuilder (T4.2.4)
    tck(LANG, "cratonvm/TckLang", "sb_basic"),
    tck(LANG, "cratonvm/TckLang", "sb_appendInt"),
    tck(LANG, "cratonvm/TckLang", "sb_chain"),
    tck(LANG, "cratonvm/TckLang", "sb_reverse"),
    tck(LANG, "cratonvm/TckLang", "sb_delete"),
    // Exceptions
    tck(LANG, "cratonvm/TckLang", "exc_getMessage"),
    tck(LANG, "cratonvm/TckLang", "exc_getCause"),
    tck(LANG, "cratonvm/TckLang", "exc_tryCatch"),
    tck(LANG, "cratonvm/TckLang", "exc_hierarchy"),
    tck(LANG, "cratonvm/TckLang", "exc_npe_class"),
    tck(LANG, "cratonvm/TckLang", "exc_finally"),
    // Class (T4.2.2)
    tck(LANG, "cratonvm/TckLang", "cls_getName"),
    tck(LANG, "cratonvm/TckLang", "cls_isInterface"),
    tck(LANG, "cratonvm/TckLang", "cls_isPrimitive"),
    tck(LANG, "cratonvm/TckLang", "cls_isArray"),
    tck(LANG, "cratonvm/TckLang", "cls_getSuperclass"),
    // Thread (T4.2.18)
    tck(LANG, "cratonvm/TckLang", "thread_currentThread"),
    tck(LANG, "cratonvm/TckLang", "thread_isAlive"),
    // Casting
    tck(LANG, "cratonvm/TckLang", "cast_int_to_long"),
    tck(LANG, "cratonvm/TckLang", "cast_int_to_float"),
    tck(LANG, "cratonvm/TckLang", "autobox_int"),
    tck(LANG, "cratonvm/TckLang", "autobox_boolean"),
    // T4.2.4 — TckStringBuilder (dedicated)
    tck(LANG, "cratonvm/TckStringBuilder", "sb_empty"),
    tck(LANG, "cratonvm/TckStringBuilder", "sb_append_string"),
    tck(LANG, "cratonvm/TckStringBuilder", "sb_append_int"),
    tck(LANG, "cratonvm/TckStringBuilder", "sb_append_char"),
    tck(LANG, "cratonvm/TckStringBuilder", "sb_append_boolean"),
    tck(LANG, "cratonvm/TckStringBuilder", "sb_append_double"),
    tck(LANG, "cratonvm/TckStringBuilder", "sb_capacity"),
    tck(LANG, "cratonvm/TckStringBuilder", "sb_charAt"),
    tck(LANG, "cratonvm/TckStringBuilder", "sb_length"),
    tck(LANG, "cratonvm/TckStringBuilder", "sb_reverse"),
    tck(LANG, "cratonvm/TckStringBuilder", "sb_delete"),
    tck(LANG, "cratonvm/TckStringBuilder", "sb_insert"),
    tck(LANG, "cratonvm/TckStringBuilder", "sb_replace"),
    tck(LANG, "cratonvm/TckStringBuilder", "sb_substring"),
    tck(LANG, "cratonvm/TckStringBuilder", "sb_indexOf"),
    // JsonReader's deprecation short reason uses BreakIterator sentence boundaries.
    tck(
        LANG,
        "cratonvm/JsonReaderSubstringProbe",
        "preservesSentenceAfterDottedIdentifier",
    ),
    // T4.2.18 — TckThread (dedicated)
    tck(LANG, "cratonvm/TckThread", "thread_currentThread"),
    tck(LANG, "cratonvm/TckThread", "thread_getName"),
    tck(LANG, "cratonvm/TckThread", "thread_isAlive"),
    tck(LANG, "cratonvm/TckThread", "thread_priority"),
    tck(LANG, "cratonvm/TckThread", "thread_isDaemon"),
    tck(LANG, "cratonvm/TckThread", "thread_id"),
    tck(LANG, "cratonvm/TckThread", "thread_new_name"),
    tck(LANG, "cratonvm/TckThread", "thread_start_join"),
    tck(LANG, "cratonvm/TckThread", "thread_sleep"),
    tck(LANG, "cratonvm/TckThread", "thread_interrupt"),
    tck(LANG, "cratonvm/TckThread", "thread_state"),
    // T4.2.19 — TckStackTrace (dedicated)
    tck(LANG, "cratonvm/TckStackTrace", "ste_getClassName"),
    tck(LANG, "cratonvm/TckStackTrace", "ste_getMethodName"),
    tck(LANG, "cratonvm/TckStackTrace", "ste_getFileName"),
    tck(LANG, "cratonvm/TckStackTrace", "ste_getLineNumber"),
    tck(LANG, "cratonvm/TckStackTrace", "ste_toString"),
    tck(LANG, "cratonvm/TckStackTrace", "ste_constructor"),
    tck(LANG, "cratonvm/TckStackTrace", "ste_isNativeMethod"),
    tck(LANG, "cratonvm/TckStackTrace", "ste_equals"),
    // =====================================================================
    // java.util — T4.2.5 Collections
    // =====================================================================
    tck(UTIL, "cratonvm/TckUtil", "testArrayListBasic"),
    tck(UTIL, "cratonvm/TckUtil", "testArrayListMutations"),
    tck(UTIL, "cratonvm/TckUtil", "testArrayListGrow"),
    tck(UTIL, "cratonvm/TckUtil", "testArrayListIterator"),
    tck(UTIL, "cratonvm/TckUtil", "testArrayListInsert"),
    tck(UTIL, "cratonvm/TckUtil", "testArrayListLastIndexOf"),
    tck(UTIL, "cratonvm/TckUtil", "testHashMapBasic"),
    tck(UTIL, "cratonvm/TckUtil", "testHashMapMutations"),
    tck(UTIL, "cratonvm/TckUtil", "testHashMapIntegerKeys"),
    tck(UTIL, "cratonvm/TckUtil", "testHashMapGetOrDefault"),
    tck(UTIL, "cratonvm/TckUtil", "testHashMapPutIfAbsent"),
    tck(UTIL, "cratonvm/TckUtil", "testHashSetBasic"),
    tck(UTIL, "cratonvm/TckUtil", "testHashSetIterator"),
    tck(UTIL, "cratonvm/TckUtil", "testArraysSort"),
    tck(UTIL, "cratonvm/TckUtil", "testArraysCopyOf"),
    tck(UTIL, "cratonvm/TckUtil", "testArraysAsList"),
    tck(UTIL, "cratonvm/TckUtil", "testCollectionsEmptyList"),
    tck(UTIL, "cratonvm/TckUtil", "testCollectionsSingletonList"),
    tck(UTIL, "cratonvm/TckUtil", "testCollectionsReverse"),
    tck(UTIL, "cratonvm/TckUtil", "testOptionalBasic"),
    tck(UTIL, "cratonvm/TckUtil", "testOptionalOrElse"),
    tck(UTIL, "cratonvm/TckUtil", "testFrequencyMap"),
    tck(UTIL, "cratonvm/TckUtil", "testDeduplication"),
    tck(UTIL, "cratonvm/TckUtil", "testHashMapKeySet"),
    tck(UTIL, "cratonvm/TckUtil", "testArrayListCapacity"),
    tck(UTIL, "cratonvm/TckUtil", "testHashMapCapacity"),
    tck(UTIL, "cratonvm/TckUtil", "testArrayListToArray"),
    tck(UTIL, "cratonvm/TckUtil", "testHashMapNullKey"),
    tck(UTIL, "cratonvm/TckUtil", "linkedlist_basic"),
    tck(
        UTIL,
        "cratonvm/TckUtil",
        "linkedlist_remove_if_iterator_remove",
    ),
    tck(UTIL, "cratonvm/TckUtil", "treemap_basic"),
    tck(UTIL, "cratonvm/TckUtil", "arrays_sort"),
    tck(UTIL, "cratonvm/TckUtil", "optional_basic"),
    tck(UTIL, "cratonvm/TckUtil", "iterator_basic"),
    // T4.2.5 — TckCollections (dedicated)
    tck(UTIL, "cratonvm/TckCollections", "sort_integers"),
    tck(UTIL, "cratonvm/TckCollections", "unmodifiable_list"),
    tck(UTIL, "cratonvm/TckCollections", "singleton_list"),
    tck(UTIL, "cratonvm/TckCollections", "empty_list"),
    tck(UTIL, "cratonvm/TckCollections", "empty_map"),
    tck(UTIL, "cratonvm/TckCollections", "empty_set"),
    tck(UTIL, "cratonvm/TckCollections", "frequency"),
    tck(UTIL, "cratonvm/TckCollections", "max_min"),
    tck(UTIL, "cratonvm/TckCollections", "reverse"),
    tck(UTIL, "cratonvm/TckCollections", "singleton_map"),
    tck(
        UTIL,
        "cratonvm/TckCollections",
        "singleton_wrappers_are_real_and_immutable",
    ),
    // T4.2.6 — TckAtomic (concurrent.atomic)
    tck(CONCURRENT, "cratonvm/TckAtomic", "atomic_int_get_set"),
    tck(CONCURRENT, "cratonvm/TckAtomic", "atomic_int_getAndSet"),
    tck(CONCURRENT, "cratonvm/TckAtomic", "atomic_int_cas"),
    tck(CONCURRENT, "cratonvm/TckAtomic", "atomic_int_cas_fail"),
    tck(CONCURRENT, "cratonvm/TckAtomic", "atomic_int_incr"),
    tck(CONCURRENT, "cratonvm/TckAtomic", "atomic_int_addAndGet"),
    tck(CONCURRENT, "cratonvm/TckAtomic", "atomic_long_basic"),
    tck(CONCURRENT, "cratonvm/TckAtomic", "atomic_long_cas"),
    tck(CONCURRENT, "cratonvm/TckAtomic", "atomic_ref_basic"),
    tck(CONCURRENT, "cratonvm/TckAtomic", "atomic_ref_cas"),
    tck(CONCURRENT, "cratonvm/TckAtomic", "atomic_boolean"),
    // T4.2.7 — TckLocks (concurrent.locks)
    tck(CONCURRENT, "cratonvm/TckLocks", "reentrant_lock_unlock"),
    tck(CONCURRENT, "cratonvm/TckLocks", "reentrant_held"),
    tck(CONCURRENT, "cratonvm/TckLocks", "reentrant_tryLock"),
    tck(CONCURRENT, "cratonvm/TckLocks", "reentrant_reentrant"),
    tck(CONCURRENT, "cratonvm/TckLocks", "rwlock_read"),
    tck(CONCURRENT, "cratonvm/TckLocks", "rwlock_write"),
    tck(CONCURRENT, "cratonvm/TckLocks", "condition_basic"),
    tck(CONCURRENT, "cratonvm/TckLocks", "stampedlock_basic"),
    // T4.2.8 — TckRegex (regex.Pattern)
    tck(REGEX, "cratonvm/TckRegex", "pattern_compile"),
    tck(REGEX, "cratonvm/TckRegex", "pattern_matches"),
    tck(REGEX, "cratonvm/TckRegex", "pattern_no_match"),
    tck(REGEX, "cratonvm/TckRegex", "matcher_find"),
    tck(REGEX, "cratonvm/TckRegex", "matcher_group"),
    tck(REGEX, "cratonvm/TckRegex", "matcher_replaceAll"),
    tck(REGEX, "cratonvm/TckRegex", "matcher_replaceFirst"),
    tck(REGEX, "cratonvm/TckRegex", "pattern_split"),
    tck(REGEX, "cratonvm/TckRegex", "pattern_quote"),
    tck(REGEX, "cratonvm/TckRegex", "pattern_flags"),
    tck(REGEX, "cratonvm/TckRegex", "pattern_toString"),
    // =====================================================================
    // java.io — T4.2.9 Reader, T4.2.10 PrintStream
    // =====================================================================
    tck(IO, "cratonvm/TckIo", "baos_basic"),
    tck(IO, "cratonvm/TckIo", "baos_size"),
    tck(IO, "cratonvm/TckIo", "baos_reset"),
    tck(IO, "cratonvm/TckIo", "bais_readAll"),
    tck(IO, "cratonvm/TckIo", "bais_available"),
    tck(IO, "cratonvm/TckIo", "bais_skip"),
    tck(IO, "cratonvm/TckIo", "baos_toString"),
    tck(IO, "cratonvm/TckIo", "sw_basic"),
    tck(IO, "cratonvm/TckIo", "sr_readChar"),
    tck(IO, "cratonvm/TckIo", "file_createDeleteExists"),
    tck(IO, "cratonvm/TckIo", "fos_writeSingleByte"),
    tck(IO, "cratonvm/TckIo", "fos_writeBulk"),
    tck(IO, "cratonvm/TckIo", "fis_readEof"),
    tck(IO, "cratonvm/TckIo", "fis_available"),
    tck(IO, "cratonvm/TckIo", "fis_skip"),
    tck(IO, "cratonvm/TckIo", "e2e_writeReadRoundtrip"),
    tck(IO, "cratonvm/TckIo", "e2e_baosToInputStream"),
    tck(IO, "cratonvm/TckIo", "bytearray_inputstream_basic"),
    tck(IO, "cratonvm/TckIo", "buffered_output_basic"),
    // T4.2.9 — TckReader (dedicated)
    tck(IO, "cratonvm/TckReader", "stringreader_read"),
    tck(IO, "cratonvm/TckReader", "stringreader_readArray"),
    tck(IO, "cratonvm/TckReader", "stringreader_mark_reset"),
    tck(IO, "cratonvm/TckReader", "stringreader_ready"),
    tck(IO, "cratonvm/TckReader", "stringreader_close"),
    tck(IO, "cratonvm/TckReader", "bufferedreader_readLine"),
    tck(IO, "cratonvm/TckReader", "bufferedreader_mark_reset"),
    tck(IO, "cratonvm/TckReader", "inputstreamreader_basic"),
    // T4.2.10 — TckPrintStream (dedicated)
    tck(IO, "cratonvm/TckPrintStream", "baos_write_toByteArray"),
    tck(IO, "cratonvm/TckPrintStream", "baos_size"),
    tck(IO, "cratonvm/TckPrintStream", "baos_reset"),
    tck(IO, "cratonvm/TckPrintStream", "baos_toString"),
    tck(IO, "cratonvm/TckPrintStream", "printstream_print"),
    tck(IO, "cratonvm/TckPrintStream", "printstream_println"),
    tck(IO, "cratonvm/TckPrintStream", "printwriter_basic"),
    tck(IO, "cratonvm/TckPrintStream", "printwriter_println"),
    tck(IO, "cratonvm/TckPrintStream", "data_io_int"),
    tck(IO, "cratonvm/TckPrintStream", "data_io_utf"),
    // =====================================================================
    // java.nio — T4.2.11 FileChannel, T4.2.12 Files
    // =====================================================================
    tck(NIO, "cratonvm/TckNio", "bb_allocate"),
    tck(NIO, "cratonvm/TckNio", "bb_wrap"),
    tck(NIO, "cratonvm/TckNio", "bb_put_get_sequential"),
    tck(NIO, "cratonvm/TckNio", "bb_flip"),
    tck(NIO, "cratonvm/TckNio", "bb_clear"),
    tck(NIO, "cratonvm/TckNio", "bb_put_int"),
    tck(NIO, "cratonvm/TckNio", "bb_little_endian"),
    tck(NIO, "cratonvm/TckNio", "bb_remaining"),
    tck(NIO, "cratonvm/TckNio", "ib_wrap"),
    tck(NIO, "cratonvm/TckNio", "bb_has_remaining"),
    tck(NIO, "cratonvm/TckNio", "bb_duplicate"),
    tck(NIO, "cratonvm/TckNio", "bb_slice"),
    tck(NIO, "cratonvm/TckNio", "bb_array"),
    // T4.2.11 — TckFileChannel (dedicated)
    tck(NIO, "cratonvm/TckFileChannel", "fc_open_write_read"),
    tck(NIO, "cratonvm/TckFileChannel", "fc_position"),
    tck(NIO, "cratonvm/TckFileChannel", "fc_size"),
    tck(NIO, "cratonvm/TckFileChannel", "fc_truncate"),
    // T4.2.12 — TckFiles (dedicated)
    tck(NIO, "cratonvm/TckFiles", "files_createTempFile"),
    tck(NIO, "cratonvm/TckFiles", "files_write_readAllBytes"),
    tck(NIO, "cratonvm/TckFiles", "files_delete"),
    tck(NIO, "cratonvm/TckFiles", "files_isDirectory"),
    tck(NIO, "cratonvm/TckFiles", "files_isRegularFile"),
    tck(NIO, "cratonvm/TckFiles", "files_size"),
    tck(NIO, "cratonvm/TckFiles", "files_copy"),
    tck(NIO, "cratonvm/TckFiles", "files_move"),
    // =====================================================================
    // java.net
    // =====================================================================
    tck(NET, "cratonvm/TckNet", "url_getProtocol"),
    tck(NET, "cratonvm/TckNet", "url_getHost"),
    tck(NET, "cratonvm/TckNet", "url_getPort_explicit"),
    tck(NET, "cratonvm/TckNet", "url_getPort_default"),
    tck(NET, "cratonvm/TckNet", "url_getPath"),
    tck(NET, "cratonvm/TckNet", "url_getQuery"),
    tck(NET, "cratonvm/TckNet", "url_malformed_throws"),
    tck(NET, "cratonvm/TckNet", "uri_parse"),
    tck(NET, "cratonvm/TckNet", "uri_getScheme"),
    tck(NET, "cratonvm/TckNet", "uri_relative"),
    // =====================================================================
    // java.security — T4.5.1-T4.5.6
    // =====================================================================
    tck(SECURITY, "cratonvm/TckSecurity", "md_sha256_empty"),
    tck(SECURITY, "cratonvm/TckSecurity", "md_sha256_abc"),
    tck(SECURITY, "cratonvm/TckSecurity", "md_sha1_length"),
    tck(SECURITY, "cratonvm/TckSecurity", "md_md5_length"),
    tck(SECURITY, "cratonvm/TckSecurity", "md_getAlgorithm"),
    tck(SECURITY, "cratonvm/TckSecurity", "md_reset"),
    tck(SECURITY, "cratonvm/TckSecurity", "md_unknown_throws"),
    tck(SECURITY, "cratonvm/TckSecurity", "sr_nextBytes"),
    tck(SECURITY, "cratonvm/TckSecurity", "sr_nextInt"),
    tck(SECURITY, "cratonvm/TckSecurity", "md_incremental_update"),
    // T4.5 extended — Cipher, KeyStore, Signature, Provider, KeyPairGenerator, Mac
    tck(SECURITY, "cratonvm/TckSecurity", "cipher_getInstance"),
    tck(SECURITY, "cratonvm/TckSecurity", "cipher_getAlgorithm"),
    tck(SECURITY, "cratonvm/TckSecurity", "keyStore_getInstance"),
    tck(SECURITY, "cratonvm/TckSecurity", "signature_getInstance"),
    tck(SECURITY, "cratonvm/TckSecurity", "signature_getAlgorithm"),
    tck(SECURITY, "cratonvm/TckSecurity", "provider_getName"),
    tck(SECURITY, "cratonvm/TckSecurity", "keypairgen_getInstance"),
    tck(SECURITY, "cratonvm/TckSecurity", "keypairgen_getAlgorithm"),
    tck(SECURITY, "cratonvm/TckSecurity", "mac_getInstance"),
    // =====================================================================
    // java.lang.reflect
    // =====================================================================
    tck(REFLECT, "cratonvm/TckReflect", "cls_forName"),
    tck(REFLECT, "cratonvm/TckReflect", "cls_getName"),
    tck(REFLECT, "cratonvm/TckReflect", "cls_getSimpleName"),
    tck(REFLECT, "cratonvm/TckReflect", "cls_getSuperclass"),
    tck(REFLECT, "cratonvm/TckReflect", "cls_isInterface"),
    tck(REFLECT, "cratonvm/TckReflect", "cls_isPrimitive"),
    tck(REFLECT, "cratonvm/TckReflect", "cls_isArray"),
    tck(REFLECT, "cratonvm/TckReflect", "cls_isEnum"),
    tck(REFLECT, "cratonvm/TckReflect", "cls_getModifiers"),
    tck(REFLECT, "cratonvm/TckReflect", "cls_isAssignableFrom"),
    tck(REFLECT, "cratonvm/TckReflect", "cls_isInstance"),
    tck(REFLECT, "cratonvm/TckReflect", "cls_getInterfaces"),
    tck(REFLECT, "cratonvm/TckReflect", "cls_getComponentType"),
    tck(REFLECT, "cratonvm/TckReflect", "cls_cast"),
    tck(REFLECT, "cratonvm/TckReflect", "cls_newInstance"),
    tck(REFLECT, "cratonvm/TckReflect", "meth_getDeclaredMethod"),
    tck(REFLECT, "cratonvm/TckReflect", "meth_invokeInstance"),
    tck(REFLECT, "cratonvm/TckReflect", "meth_invokeStatic"),
    tck(REFLECT, "cratonvm/TckReflect", "meth_invokePrivate"),
    tck(REFLECT, "cratonvm/TckReflect", "meth_getReturnType"),
    tck(REFLECT, "cratonvm/TckReflect", "meth_getParameterTypes"),
    // =====================================================================
    // java.sql — T4.4.1-T4.4.10
    // =====================================================================
    tck(JDBC, "cratonvm/TckJdbc", "open_inmemory_connection"),
    tck(JDBC, "cratonvm/TckJdbc", "statement_ddl_dml_query"),
    tck(
        JDBC,
        "cratonvm/TckJdbc",
        "prepared_statement_binds_and_executes",
    ),
    tck(JDBC, "cratonvm/TckJdbc", "rollback_discards_changes"),
    tck(JDBC, "cratonvm/TckJdbc", "savepoint_rollback_and_release"),
    tck(JDBC, "cratonvm/TckJdbc", "blob_round_trip"),
    tck(JDBC, "cratonvm/TckJdbc", "clob_round_trip"),
    tck(JDBC, "cratonvm/TckJdbc", "callable_inherits_prepared"),
    tck(JDBC, "cratonvm/TckJdbc", "metadata_identifies_sqlite"),
    tck(JDBC, "cratonvm/TckJdbc", "driver_register_list_deregister"),
    tck(JDBC, "cratonvm/TckJdbc", "e2e_mini_app"),
    // T4.4 — TckSql (SQL constants, no DB)
    tck(SQL, "cratonvm/TckSql", "types_integer"),
    tck(SQL, "cratonvm/TckSql", "types_varchar"),
    tck(SQL, "cratonvm/TckSql", "types_bigint"),
    tck(SQL, "cratonvm/TckSql", "types_double"),
    tck(SQL, "cratonvm/TckSql", "types_timestamp"),
    tck(SQL, "cratonvm/TckSql", "sqlException_message"),
    tck(SQL, "cratonvm/TckSql", "sqlException_state"),
    tck(SQL, "cratonvm/TckSql", "sqlException_code"),
    tck(SQL, "cratonvm/TckSql", "sqlException_chain"),
    tck(SQL, "cratonvm/TckSql", "resultSet_types"),
    tck(SQL, "cratonvm/TckSql", "connection_isolation"),
    tck(SQL, "cratonvm/TckSql", "statement_no_keys"),
    // =====================================================================
    // java.time — T4.2.13 Instant, T4.2.14 ZonedDateTime, T4.2.15 DateTimeFormatter
    // =====================================================================
    tck(TIME, "cratonvm/TckInstant", "instant_now"),
    tck(TIME, "cratonvm/TckInstant", "instant_epoch"),
    tck(TIME, "cratonvm/TckInstant", "instant_ofEpochSecond"),
    tck(TIME, "cratonvm/TckInstant", "instant_plusSeconds"),
    tck(TIME, "cratonvm/TckInstant", "instant_minusSeconds"),
    tck(TIME, "cratonvm/TckInstant", "instant_compareTo"),
    tck(TIME, "cratonvm/TckInstant", "instant_isBefore"),
    tck(TIME, "cratonvm/TckInstant", "instant_isAfter"),
    tck(TIME, "cratonvm/TckInstant", "instant_toEpochMilli"),
    tck(TIME, "cratonvm/TckInstant", "instant_toString"),
    tck(TIME, "cratonvm/TckInstant", "instant_equals"),
    tck(TIME, "cratonvm/TckZonedDateTime", "zdt_now"),
    tck(TIME, "cratonvm/TckZonedDateTime", "zdt_of"),
    tck(TIME, "cratonvm/TckZonedDateTime", "zdt_getMonth"),
    tck(TIME, "cratonvm/TckZonedDateTime", "zdt_getDayOfMonth"),
    tck(TIME, "cratonvm/TckZonedDateTime", "zdt_getHour"),
    tck(TIME, "cratonvm/TckZonedDateTime", "zdt_getZone"),
    tck(TIME, "cratonvm/TckZonedDateTime", "zdt_toInstant"),
    tck(TIME, "cratonvm/TckZonedDateTime", "zdt_plusDays"),
    tck(TIME, "cratonvm/TckZonedDateTime", "zdt_minusHours"),
    tck(TIME, "cratonvm/TckZonedDateTime", "zdt_withZoneSameInstant"),
    tck(TIME, "cratonvm/TckLocalDate", "ld_now"),
    tck(TIME, "cratonvm/TckLocalDate", "ld_of"),
    tck(TIME, "cratonvm/TckLocalDate", "ld_parse"),
    tck(TIME, "cratonvm/TckLocalDate", "ld_plusDays"),
    tck(TIME, "cratonvm/TckLocalDate", "ld_minusMonths"),
    tck(TIME, "cratonvm/TckLocalDate", "ld_isLeapYear"),
    tck(TIME, "cratonvm/TckLocalDate", "ld_dayOfWeek"),
    tck(TIME, "cratonvm/TckLocalDate", "ld_compareTo"),
    tck(TIME, "cratonvm/TckLocalDate", "ld_format"),
    tck(TIME, "cratonvm/TckLocalDate", "ld_toString"),
    // =====================================================================
    // java.text — T4.2.16 DecimalFormat, T4.2.17 MessageFormat
    // =====================================================================
    tck(TEXT, "cratonvm/TckDecimalFormat", "df_format_int"),
    tck(TEXT, "cratonvm/TckDecimalFormat", "df_format_double"),
    tck(TEXT, "cratonvm/TckDecimalFormat", "df_pattern"),
    tck(TEXT, "cratonvm/TckDecimalFormat", "df_parse"),
    tck(TEXT, "cratonvm/TckDecimalFormat", "df_negative"),
    tck(TEXT, "cratonvm/TckDecimalFormat", "df_grouping"),
    tck(TEXT, "cratonvm/TckDecimalFormat", "df_percent"),
    tck(TEXT, "cratonvm/TckDecimalFormat", "df_max_fraction"),
    tck(TEXT, "cratonvm/TckMessageFormat", "mf_basic"),
    tck(TEXT, "cratonvm/TckMessageFormat", "mf_multiple_args"),
    tck(TEXT, "cratonvm/TckMessageFormat", "mf_number"),
    tck(TEXT, "cratonvm/TckMessageFormat", "mf_repeated_arg"),
    tck(TEXT, "cratonvm/TckMessageFormat", "mf_no_args"),
    tck(TEXT, "cratonvm/TckMessageFormat", "mf_null_safe"),
    tck(TEXT, "cratonvm/TckMessageFormat", "mf_escape_single_quote"),
    tck(TEXT, "cratonvm/TckMessageFormat", "mf_toPattern"),
    // =====================================================================
    // java.math — T4.2.20 BigInteger/BigDecimal
    // =====================================================================
    tck(MATH, "cratonvm/TckBigMath", "bi_add"),
    tck(MATH, "cratonvm/TckBigMath", "bi_subtract"),
    tck(MATH, "cratonvm/TckBigMath", "bi_multiply"),
    tck(MATH, "cratonvm/TckBigMath", "bi_divide"),
    tck(MATH, "cratonvm/TckBigMath", "bi_mod"),
    tck(MATH, "cratonvm/TckBigMath", "bi_compareTo"),
    tck(MATH, "cratonvm/TckBigMath", "bi_toString"),
    tck(MATH, "cratonvm/TckBigMath", "bi_valueOf"),
    tck(MATH, "cratonvm/TckBigMath", "bi_bitLength"),
    tck(MATH, "cratonvm/TckBigMath", "bd_add"),
    tck(MATH, "cratonvm/TckBigMath", "bd_subtract"),
    tck(MATH, "cratonvm/TckBigMath", "bd_multiply"),
    tck(MATH, "cratonvm/TckBigMath", "bd_scale"),
    tck(MATH, "cratonvm/TckBigMath", "bd_toString"),
    tck(MATH, "cratonvm/TckBigMath", "bd_compareTo"),
    // =====================================================================
    // java.net.http — T4.3.1-T4.3.10
    // =====================================================================
    tck(HTTP, "cratonvm/TckHttpClient", "httpClient_newBuilder"),
    tck(HTTP, "cratonvm/TckHttpClient", "httpClient_version"),
    tck(HTTP, "cratonvm/TckHttpClient", "httpClient_followRedirects"),
    tck(HTTP, "cratonvm/TckHttpClient", "httpRequest_builder"),
    tck(HTTP, "cratonvm/TckHttpClient", "httpRequest_method"),
    tck(HTTP, "cratonvm/TckHttpClient", "httpRequest_uri"),
    tck(HTTP, "cratonvm/TckHttpClient", "httpRequest_timeout"),
    tck(HTTP, "cratonvm/TckHttpClient", "httpRequest_headers_empty"),
    tck(HTTP, "cratonvm/TckHttpClient", "bodyHandlers_ofString"),
    tck(HTTP, "cratonvm/TckHttpClient", "bodyHandlers_discarding"),
    // =====================================================================
    // java.management — T4.6.1-T4.6.3
    // =====================================================================
    tck(MANAGEMENT, "cratonvm/TckManagement", "mbean_server"),
    tck(MANAGEMENT, "cratonvm/TckManagement", "runtime_mxbean"),
    tck(MANAGEMENT, "cratonvm/TckManagement", "runtime_name"),
    tck(MANAGEMENT, "cratonvm/TckManagement", "memory_mxbean"),
    tck(MANAGEMENT, "cratonvm/TckManagement", "thread_mxbean"),
    tck(MANAGEMENT, "cratonvm/TckManagement", "thread_count"),
    tck(MANAGEMENT, "cratonvm/TckManagement", "classloading_mxbean"),
    tck(MANAGEMENT, "cratonvm/TckManagement", "os_mxbean"),
    tck(MANAGEMENT, "cratonvm/TckManagement", "os_name"),
];

const fn tck(category: Category, class: &'static str, method: &'static str) -> TckTest {
    TckTest {
        category,
        class,
        method,
    }
}

// ---------------------------------------------------------------------------
// Execution + classification — needs a VM and a `javac`-produced corpus.
// ---------------------------------------------------------------------------

#[cfg(feature = "synthetic-jdk")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Pass,
    Fail,
    Error,
}

#[cfg(feature = "synthetic-jdk")]
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

#[cfg(feature = "synthetic-jdk")]
fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

#[cfg(feature = "synthetic-jdk")]
fn class_files_available() -> bool {
    let dir = test_resources_dir();
    std::path::Path::new(&format!("{dir}/cratonvm/TckClassFile.class")).exists()
}

/// The skip note both corpus-running tests print, and the switch that turns it
/// into a failure.
///
/// Until 2026-08-13 the two call sites read
/// `eprintln!("Skipping …"); return;` — in cargo's output that is
/// `test jck_regression_gate ... ok` in 0.00 s, byte-for-byte identical to a
/// gate that ran the whole 424-test corpus and passed. `CRATONVM_REQUIRE_E2E`
/// is this suite's existing answer to exactly that (`vm/tests/common/mod.rs`,
/// `require_fixture`): unset, the behaviour is the historical skip, but now
/// loud; set, a missing corpus is a panic, so at least one configuration cannot
/// go green by having no inputs.
#[cfg(feature = "synthetic-jdk")]
fn skip_or_fail_without_corpus(test_name: &str) -> bool {
    if class_files_available() {
        return false;
    }
    let require = std::env::var("CRATONVM_REQUIRE_E2E")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);
    let dir = test_resources_dir();
    if require {
        panic!(
            "{test_name}: the Tck corpus is not compiled ({dir}/cratonvm/TckClassFile.class \
             is missing) and CRATONVM_REQUIRE_E2E is set. This gate enforces the committed \
             baseline in internal/gaps/jdk-regression-baseline.md; with no corpus it \
             enforces nothing, and would have reported `ok` in 0.00s."
        );
    }
    eprintln!(
        "[jck] SKIPPING {test_name} — {dir}/cratonvm/TckClassFile.class is missing, so THIS \
         TEST ASSERTED NOTHING. Set CRATONVM_REQUIRE_E2E=1 to make this a failure."
    );
    true
}

#[cfg(feature = "synthetic-jdk")]
fn test_vm() -> Vm {
    let config = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    Vm::new(config)
}

#[cfg(feature = "synthetic-jdk")]
#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    pass: u32,
    fail: u32,
    error: u32,
}

#[cfg(feature = "synthetic-jdk")]
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
#[cfg(feature = "synthetic-jdk")]
fn run_corpus() -> std::collections::BTreeMap<&'static str, Tally> {
    let mut tallies: std::collections::BTreeMap<&'static str, Tally> =
        std::collections::BTreeMap::new();
    for t in CORPUS {
        let outcome = run_one(t);
        tallies.entry(t.category.0).or_default().record(outcome);
        if outcome != Outcome::Pass {
            eprintln!("[jck] {:?} {}::{}", outcome, t.class, t.method);
        }
    }
    tallies
}

// ---------------------------------------------------------------------------
// Baseline report (committed under internal/gaps/jdk-regression-baseline.md)
// ---------------------------------------------------------------------------

/// Per-category `(name, pass floor, corpus population)`.
///
/// The third column used to be a trailing `// 4/5` comment. It is data now,
/// because it is the only thing that ties this table to `CORPUS`: a category
/// that gains or loses a test changes its population, and
/// `every_category_population_matches_the_corpus` says so. As a comment it said
/// nothing, and it had already rotted — `Lang` and `Util` are 110 and 45 here,
/// not the 109 and 43 the committed baseline document still records.
///
/// The floors themselves must match the Floor column of
/// `internal/gaps/jdk-regression-baseline.md`; that is asserted, against
/// the file, by `the_committed_baseline_document_and_this_table_agree`.
const BASELINE_FLOORS: &[(&str, u32, u32)] = &[
    // Floors updated 2026-04-16 after T4.2-T4.6 corpus expansion.
    // Populations re-counted from CORPUS on 2026-08-13.
    ("ClassFile", 4, 5),
    ("Concurrent", 8, 19), // AtomicInteger/Long basic ops pass
    ("Http", 0, 10),       // java.net.http not yet wired
    ("Instructions", 9, 13),
    ("Io", 18, 37),    // BAOS, BAIS, File I/O, StringWriter (slight variance)
    ("Jdbc", 0, 11),   // JDBC wired but not through TCK path
    ("Lang", 34, 110), // core types, wrappers, math, system
    ("Loading", 4, 5),
    ("Management", 0, 9), // MXBeans not yet wired
    ("Math", 3, 15),      // BigInteger basic ops
    ("Net", 0, 10),       // URL/URI constructors
    ("Nio", 11, 25),      // ByteBuffer core ops
    ("Reflect", 5, 21),   // Class metadata basics
    ("Regex", 0, 11),     // Pattern/Matcher not through TCK path
    ("Security", 0, 19),  // crypto not through TCK path
    ("Sql", 8, 12),       // java.sql constants pass
    ("Text", 0, 16),      // DecimalFormat/MessageFormat
    ("Time", 0, 31),      // java.time not yet wired
    ("Util", 3, 45),      // basic collections
];

// ---------------------------------------------------------------------------
// Table checks — no VM, no corpus, no cargo feature. These run in
// `cargo test --workspace`, which is the configuration CI actually executes.
// ---------------------------------------------------------------------------

/// The committed baseline document, and where it was found.
///
/// Two candidates because the harness doc comment said `gaps/…` for a long time
/// while the file has lived under `internal/gaps/…`, and `docs/internal` is
/// being removed from history by a separate effort. A gate whose committed
/// baseline cannot be located enforces nothing, so this panics rather than
/// skipping — the whole subject of this file's 2026-08-13 rewrite.
fn baseline_document() -> (String, String) {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let candidates = [
        format!("{manifest_dir}/../docs/internal/gaps/jdk-regression-baseline.md"),
        format!("{manifest_dir}/../gaps/jdk-regression-baseline.md"),
    ];
    for c in &candidates {
        if let Ok(text) = std::fs::read_to_string(c) {
            return (c.clone(), text);
        }
    }
    panic!(
        "the committed JCK baseline document was not found. Searched:\n  {}\n\n\
         `jck_regression_gate`'s failure message names this document as the \
         thing it enforces, and BASELINE_FLOORS is supposed to mirror its Floor \
         column. If the document MOVED, update the candidate list here in the \
         same commit; if it was DELETED, delete the claim as well, because a \
         regression gate with no committed baseline is not one.",
        candidates.join("\n  ")
    );
}

/// `(category, floor, total)` rows of the baseline table, plus the `TOTAL` row.
///
/// Deliberately tolerant of the surrounding prose: it accepts any pipe table
/// row whose first cell is a bare alphabetic word and whose next two cells parse
/// as integers, so the `## History` table (first cell is a date) and the
/// `## Categories added` table (second cell is prose) are skipped without
/// needing to know where they are.
fn baseline_rows(md: &str) -> Vec<(String, u32, u32)> {
    md.lines()
        .filter(|l| l.trim_start().starts_with('|'))
        .filter_map(|l| {
            let cells: Vec<&str> = l.split('|').map(|c| c.trim().trim_matches('*')).collect();
            if cells.len() < 5 {
                return None;
            }
            let name = cells[1];
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphabetic()) {
                return None;
            }
            let floor = cells[2].parse::<u32>().ok()?;
            let total = cells[3].parse::<u32>().ok()?;
            Some((name.to_string(), floor, total))
        })
        .collect()
}

/// Rows where this file and the committed document disagree TODAY, named.
///
/// `(category-or-"TOTAL", field, value here, value in the document)`.
///
/// This is an exemption list, and it expires the moment it stops being true:
/// `the_committed_baseline_document_and_this_table_agree` fails if a listed row
/// starts agreeing (delete the row), if the disagreement changes shape
/// (re-measure it), or if an unlisted row starts disagreeing. There is no way
/// to add a row without stating both numbers, and no way for a row to outlive
/// the discrepancy it excuses — which is what happened to the `already_triaged`
/// list E20 found, where 3 of 6 rows had rotted into standing permission.
///
/// Measured 2026-08-13 by counting `CORPUS` and parsing the document. All four
/// are the document being stale, not this file: the corpus grew by 3 tests
/// (Lang +1, Util +2) after the document's last update on 2026-04-16, and the
/// `Io` floor was lowered here from 20 to 18 with the reason recorded only in a
/// trailing comment ("slight variance"). Fixing them means editing a document
/// this lane does not own — see NOM E33-3.
const BASELINE_DOC_DRIFT: &[(&str, &str, u32, u32)] = &[
    ("Io", "floor", 18, 20),
    ("Lang", "total", 110, 109),
    ("Util", "total", 45, 43),
    ("TOTAL", "total", 424, 421),
];

#[test]
fn every_category_population_matches_the_corpus() {
    let mut wrong: Vec<String> = Vec::new();
    for (cat, floor, total) in BASELINE_FLOORS {
        let actual = CORPUS.iter().filter(|t| t.category.0 == *cat).count() as u32;
        if actual != *total {
            wrong.push(format!(
                "{cat}: BASELINE_FLOORS says {total} tests, CORPUS has {actual}"
            ));
        }
        if floor > total {
            wrong.push(format!(
                "{cat}: floor {floor} exceeds the population {total}, so the gate can never pass"
            ));
        }
    }
    // Two-sided: a category present in one table and not the other.
    for t in CORPUS {
        if !BASELINE_FLOORS.iter().any(|(c, _, _)| *c == t.category.0) {
            wrong.push(format!(
                "{}: CORPUS has tests in this category and BASELINE_FLOORS has no row, so its \
                 pass count is floored by nothing",
                t.category.0
            ));
        }
    }
    for (cat, _, _) in BASELINE_FLOORS {
        if !CORPUS.iter().any(|t| t.category.0 == *cat) {
            wrong.push(format!(
                "{cat}: BASELINE_FLOORS has a row and CORPUS has no test in it — \
                 `tallies.get(cat).unwrap_or_default()` makes that row a permanent no-op"
            ));
        }
    }
    wrong.sort();
    wrong.dedup();
    assert!(
        wrong.is_empty(),
        "BASELINE_FLOORS and CORPUS have drifted apart:\n  {}",
        wrong.join("\n  ")
    );
    // Anti-vacuity: an empty CORPUS would pass every loop above.
    assert!(
        CORPUS.len() > 400,
        "CORPUS has only {} entries; the checks above would be near-vacuous",
        CORPUS.len()
    );
}

#[test]
fn the_corpus_lists_no_test_twice() {
    let mut seen: Vec<(&str, &str)> = CORPUS.iter().map(|t| (t.class, t.method)).collect();
    let before = seen.len();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen.len(),
        before,
        "CORPUS lists the same (class, method) more than once. A duplicate inflates a \
         category's population and its pass count together, so the floor still passes \
         while the corpus covers less than it claims."
    );
}

/// The one check in this file whose expectation lives OUTSIDE the crate: a
/// markdown file a person edits by hand, in a different directory, which
/// `jck_regression_gate`'s own failure text names as the thing it enforces.
#[test]
fn the_committed_baseline_document_and_this_table_agree() {
    let (path, md) = baseline_document();
    let rows = baseline_rows(&md);
    assert!(
        rows.len() >= 20,
        "parsed only {} rows out of {path} — the document's table format changed and this \
         check has stopped reading it. Fix the parser; do not delete the check.",
        rows.len()
    );

    // Build every (category, field) comparison, then subtract the named drift.
    let mut disagreements: Vec<(String, String, u32, u32)> = Vec::new();
    for (cat, floor, total) in BASELINE_FLOORS {
        match rows.iter().find(|(name, _, _)| name.as_str() == *cat) {
            None => disagreements.push((cat.to_string(), "row".into(), 1, 0)),
            Some((_, doc_floor, doc_total)) => {
                if floor != doc_floor {
                    disagreements.push((cat.to_string(), "floor".into(), *floor, *doc_floor));
                }
                if total != doc_total {
                    disagreements.push((cat.to_string(), "total".into(), *total, *doc_total));
                }
            }
        }
    }
    let corpus_len = CORPUS.len() as u32;
    match rows.iter().find(|(name, _, _)| name.as_str() == "TOTAL") {
        None => disagreements.push(("TOTAL".into(), "row".into(), 1, 0)),
        Some((_, _, doc_total)) => {
            if corpus_len != *doc_total {
                disagreements.push(("TOTAL".into(), "total".into(), corpus_len, *doc_total));
            }
        }
    }

    let mut unexpected: Vec<String> = Vec::new();
    for (cat, field, here, there) in &disagreements {
        if !BASELINE_DOC_DRIFT.iter().any(|(c, f, h, t)| {
            *c == cat.as_str() && *f == field.as_str() && h == here && t == there
        }) {
            unexpected.push(format!(
                "{cat}.{field}: this file says {here}, {path} says {there}"
            ));
        }
    }
    let mut stale: Vec<String> = Vec::new();
    for (cat, field, here, there) in BASELINE_DOC_DRIFT {
        if !disagreements.iter().any(|(c, f, h, t)| {
            c.as_str() == *cat && f.as_str() == *field && h == here && t == there
        }) {
            stale.push(format!(
                "{cat}.{field}: BASELINE_DOC_DRIFT still excuses \"{here} here vs {there} in the \
                 document\", but that is no longer the disagreement"
            ));
        }
    }

    assert!(
        unexpected.is_empty(),
        "this file and the committed baseline document disagree, and the disagreement is not \
         in BASELINE_DOC_DRIFT:\n  {}\n\n\
         Update {path} and this table in the same commit — that is what its \"Regression \
         floors\" section asks for. If the divergence is deliberate and cannot be fixed here, \
         add it to BASELINE_DOC_DRIFT with BOTH numbers and a reason.",
        unexpected.join("\n  ")
    );
    assert!(
        stale.is_empty(),
        "BASELINE_DOC_DRIFT has rows that no longer describe a real disagreement:\n  {}\n\n\
         Delete them. An exemption that outlives its exception is standing permission, which \
         is the failure mode this whole file was rewritten for on 2026-08-13.",
        stale.join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// Corpus-running tests (need `--features synthetic-jdk` AND a javac corpus)
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "synthetic-jdk")]
fn jck_full_corpus_runs() {
    if skip_or_fail_without_corpus("jck_full_corpus_runs") {
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
            cat,
            t.pass,
            t.fail,
            t.error,
            t.total()
        );
        grand.pass += t.pass;
        grand.fail += t.fail;
        grand.error += t.error;
    }
    eprintln!(
        "  {:<14} pass={:>3}  fail={:>3}  error={:>3}  total={:>3}",
        "TOTAL",
        grand.pass,
        grand.fail,
        grand.error,
        grand.total()
    );
    eprintln!();

    assert!(
        grand.total() > 0,
        "JCK corpus ran zero tests — check the CORPUS table"
    );
}

#[test]
#[cfg(feature = "synthetic-jdk")]
fn jck_regression_gate() {
    if skip_or_fail_without_corpus("jck_regression_gate") {
        return;
    }
    let tallies = run_corpus();

    let mut failures: Vec<String> = Vec::new();
    for (cat, floor, _total) in BASELINE_FLOORS {
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
         internal/gaps/jdk-regression-baseline.md. If a regression is \
         intentional (e.g. a test was removed), update BASELINE_FLOORS and the \
         baseline doc together in the same commit — \
         `the_committed_baseline_document_and_this_table_agree` will tell you \
         if you update only one of them.",
        failures.join("\n  ")
    );
}
