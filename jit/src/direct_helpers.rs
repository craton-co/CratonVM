// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The VM's thin direct-call helper addresses, as one per-compile table.
//!
//! These used to be 24 process-wide `AtomicUsize` cells, written by 21
//! `set_*_direct_fn` setters from the VM's `build_helpers` and read by the
//! compile driver and the single-pass emitter
//! (`jit-god-functions-and-request-side-channels-FIXED-20260912.md`). The VM
//! now builds a `DirectHelperTable` and hands it to each compile, through
//! [`crate::CompileRequest`] and `x64::compile_with_param_slots`. A `0` entry
//! means "not wired": the site keeps its generic dispatch path.
//!
//! # This is a SECOND helper-call ABI, and it is described here
//!
//! [`crate::JitRuntimeHelpers`] has a typed ABI description next to it
//! (`jit-api/src/helpers_abi.rs`): a `HelperFn*` alias per callable slot, a
//! golden byte-offset table, a revision ledger, a startup validator and 59
//! producer-side `let _: HelperFn* = jit_*;` coercion checks. This table had
//! **none of that** — 28 scalar callable slots plus three `VarHandle` arrays,
//! every one of them filled on the VM side with `jit_foo as *const () as
//! usize`, a cast that type-checks against nothing, and then baked into a
//! `CALL`. Four further words (`static_base_resolver` and its context, and the
//! two served-class predicates) are called by the compiler rather than by
//! generated code and had the same problem one level worse: two of them were
//! transmuted to `extern "Rust"` fn pointers, an unspecified convention.
//!
//! [`DIRECT_HELPER_FN_SIGS`] below is the missing description. Each row is
//! derived from a `DirectHelperFn*` alias rather than transcribed, exactly as
//! `HELPER_FN_SIGS` is, so the arity and widths cannot drift from the alias.
//! The alias in turn is tied to the real function by a producer-side coercion
//! check that has to live in `vm/src/jit/helpers.rs` next to
//! `direct_helper_table()` — see `docs/jit/helper-abi.md` §10 for the block and
//! why it cannot live here (this crate cannot name `jit_integer_value_of_direct`).
//!
//! # The three call paths, and why the Win64 census has to distinguish them
//!
//! `HELPERS_NEEDING_WIN64_STACK_ARGS` is asserted `== 1` in `jit-api` because
//! exactly one `JitRuntimeHelpers` slot exceeds Win64's four integer argument
//! registers and its call site writes the overflow to `[RSP+32]`/`[RSP+40]` by
//! hand. Applying that same pin to this table would be wrong in both
//! directions, because these slots are not all reached the same way:
//!
//! * [`DirectHelperCallPath::HandWritten`] — the emitter loads `ENTRY_ABI_REGS`
//!   itself and calls an absolute address (`runtime_lowering::emit_monitor_stub`,
//!   the prologue/epilogue savebase watch). These have no stack-argument path
//!   at all, so arity **must** stay within Win64's four.
//! * [`DirectHelperCallPath::JavaArgMarshalled`] — the slot is installed as a
//!   `JitDirectCall` target and reached through the backends' general Java-call
//!   marshalling, whose `emit_stack_arg_setup` already places arguments beyond
//!   the register file. `varhandle_cas` (five words) and the
//!   `ScopedMemoryAccess` accessors (six and seven) live here legitimately.
//! * [`DirectHelperCallPath::CompilerOnly`] — never in generated code; the
//!   compiler itself calls it while emitting (`static_base_resolver`, the two
//!   served-class predicates). The calling convention that matters for these is
//!   the one Rust uses for the `transmute` in this file, which is why those
//!   three are `extern "C"` on both ends and not `extern "Rust"`.

/// Every thin direct-call helper address a compile may bake into a `CALL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectHelperTable {
    /// The VM-side `jit_integer_value_of_direct(vm_ptr, value) -> i64` thin
    /// helper, filled in by the VM's `direct_helper_table`. Avoids a
    /// `JitRuntimeHelpers` ABI change. `0` = not wired → the recognition below
    /// is skipped and `Integer.valueOf` sites use the generic dispatch helper.
    ///
    /// Why: `invokestatic Integer.valueOf(I)` is statically bound and its
    /// callee is a registered native, so the generic `jit_invoke_dispatch`
    /// round trip (info decode, per-call thread-local cache probes, argument
    /// buffer build, safe-native-call wrapper) is pure fixed overhead on one
    /// of the hottest autoboxing paths (three `valueOf` calls per
    /// `HashMap<Integer,Integer>` put+get pair). A direct `CALL` to the thin
    /// helper keeps the exact allocation, `-128..=127` identity-cache, and
    /// pending-return rooting semantics while skipping the dispatch machinery.
    pub integer_value_of: usize,

    /// `Integer.intValue()` sibling of [`INTEGER_VALUE_OF_DIRECT_FN`].
    /// `java/lang/Integer` is `final`, so an `invokevirtual` site whose
    /// constant-pool class is exactly `Integer` is statically monomorphic and
    /// can take the plain (guard-free) virtual direct-call path; the thin
    /// helper handles the null-receiver NPE itself.
    pub integer_int_value: usize,

    /// `Long.valueOf(J)` / `Long.longValue()` twins of the two `Integer` cells
    /// above. `0` = not wired → the recognition is skipped and the sites use the
    /// generic dispatch helper.
    ///
    /// **Why the twin was missing, and what it cost.** `Integer.valueOf`/`intValue`
    /// have had thin binds since 2026-07; `Long.valueOf`/`longValue` are registered
    /// natives on exactly the same terms (`lang_math.rs::register_wrapper_natives`:
    /// a 256-entry `-128..=127` identity cache and a field-0 read) and were not.
    /// Measured on ONE binary, same run, `probes/BoxRungRate.java`:
    ///
    /// | rung | HotSpot 25 | CratonVM |
    /// |---|---:|---:|
    /// | `Integer.valueOf` alone | 1.9 ns | 153 ns |
    /// | `Long.valueOf` alone | 2.6 ns | **296 ns** |
    /// | `Integer.valueOf` + `intValue` | 0.24 ns | 151 ns |
    /// | `Long.valueOf` + `longValue` | 0.26 ns | **503 ns** |
    ///
    /// The `Integer` PAIR costs the same as `Integer.valueOf` ALONE — `intValue`'s
    /// bind makes the unbox free. The `Long` pair costs `valueOf` plus another
    /// ~207 ns, which is `longValue()` paying the generic funnel. The arms differ
    /// only in which `*_DIRECT_FN` cell exists.
    ///
    /// Censused with `--dump-native-registry` on `probes/HwtScaleProbe.java` (the
    /// `HashedWheelTimerTest.testExecutionOnTime` workload): `Long.valueOf` 99 496 +
    /// `Long.longValue` 100 000 calls for 100 000 expired timer tasks — one box and
    /// one unbox each, because the queue under test is a `BlockingQueue<Long>`.
    /// `Long` boxing is the `Integer` case's equal on every `Map<Long, …>`, every
    /// row identifier that reaches a collection, and every `AtomicLong` readout that
    /// is stored rather than consumed.
    pub long_value_of: usize,

    pub long_long_value: usize,

    /// Exact-HashMap `put`/`get` thin direct-call helpers
    /// (perf/halfgap-20260717). `java/util/HashMap` is NOT final, so these
    /// register guard-free (`guard_class_id: 0`) and the helpers themselves
    /// verify the receiver's EXACT class, falling back to the full generic
    /// dispatcher for subclasses (LinkedHashMap at a HashMap-declared site),
    /// non-Integer keys, materialized maps, and redefine windows. The fast
    /// path is the Integer-overlay probe with no `safe_native_call` wrapper —
    /// the same wrapper-free contract as `INTEGER_INT_VALUE_DIRECT_FN`.
    pub hashmap_put: usize,

    pub hashmap_get: usize,

    /// Static `StringLatin1.toLowerCase` helper for the compact-string hot path.
    pub string_latin1_lower: usize,

    pub concurrent_hashmap_get: usize,

    pub nio_bytebuffer_put_byte: usize,

    pub nio_bytebuffer_get_byte: usize,

    /// `java/nio/Buffer.session()Ljdk/internal/foreign/MemorySessionImpl;` thin
    /// direct-call helper. `0` = not wired.
    ///
    /// # The census that named it
    ///
    /// `--dump-native-registry` over `probes/OnlyHeapGetInt.java` — 200 000
    /// `HeapByteBuffer.getInt(int)` calls and nothing else — divides exactly:
    ///
    /// ```text
    /// 200000  java/nio/HeapByteBuffer.session()Ljdk/internal/foreign/MemorySessionImpl;  [bridge]
    /// 200000  jdk/internal/misc/ScopedMemoryAccess.getIntUnaligned(...)I                 [synthetic-stub]
    /// ```
    ///
    /// One `session()` crossing per multi-byte accessor, and its registered body
    /// is `Ok(Some(Value::Object(None)))` — a constant null. That is the same
    /// shape as `Reference.reachabilityFence`, whose own doc says of an equally
    /// empty body that *"paying ~160 ns of generic native funnel for that is pure
    /// loss"*, and which was given a thin helper for exactly this reason.
    pub buffer_session: usize,

    /// `ScopedMemoryAccess` thin direct-call helper entries for multi-byte unaligned
    /// scalar accessors (`getIntUnaligned`, `putIntUnaligned`, etc.).
    pub scoped_memory_get_int: usize,
    pub scoped_memory_get_short: usize,
    pub scoped_memory_get_long: usize,
    pub scoped_memory_get_char: usize,
    pub scoped_memory_put_int: usize,
    pub scoped_memory_put_short: usize,
    pub scoped_memory_put_long: usize,
    pub scoped_memory_put_char: usize,

    /// `*Internal` twins of the eight slots above -- see `ScopedMemoryOp`'s
    /// own doc comment for why these are separate slots rather than shared
    /// ones (a decline must name the method that was actually intercepted).
    pub scoped_memory_get_byte_internal: usize,
    pub scoped_memory_put_byte_internal: usize,
    pub scoped_memory_get_int_internal: usize,
    pub scoped_memory_get_short_internal: usize,
    pub scoped_memory_get_long_internal: usize,
    pub scoped_memory_get_char_internal: usize,
    pub scoped_memory_put_int_internal: usize,
    pub scoped_memory_put_short_internal: usize,
    pub scoped_memory_put_long_internal: usize,
    pub scoped_memory_put_char_internal: usize,

    /// `jdk/internal/misc/Unsafe.get/put{Byte,Short,Int,Long}(Object,long[,V])`
    /// thin direct-call helper entries.
    ///
    /// Unlike `ScopedMemoryAccess`'s accessors these take no
    /// `MemorySessionImpl` session and have no scoped-arena liveness to
    /// check. They are also genuinely `ACC_NATIVE`: the real JDK declares
    /// `public native int getInt(Object o, long offset);` with no bytecode
    /// body at all, so there is nothing for a helper to shadow -- see the
    /// retired `heap-bytebuffer-and-chm-run-interpreted-under-jdk-only-20260918`
    /// write-up (`docs/feature-designs/jit-ir-optimizations.md
    /// under-jdk-only-20260918.md`), "What landed" section 4. `0` = not wired.
    pub unsafe_get_byte: usize,
    pub unsafe_get_short: usize,
    pub unsafe_get_int: usize,
    pub unsafe_get_long: usize,
    /// `jdk/internal/misc/Unsafe.getReference` / `getReferenceVolatile`
    /// `(Object,long)Object` (SBR round 2, wave 8, lane `tlocal`): the
    /// reference-ARRAY read every `ConcurrentHashMap.tabAt` makes (22 native
    /// crossings per `Cipher.getInstance`). Serves an in-bounds, element-aligned
    /// read of a reference array and declines everything else (instance field,
    /// synthetic or static offset, null base) to the registered native. `0` = not wired.
    pub unsafe_get_reference: usize,
    pub unsafe_get_reference_volatile: usize,
    pub unsafe_put_byte: usize,
    pub unsafe_put_short: usize,
    pub unsafe_put_int: usize,
    pub unsafe_put_long: usize,
    /// `jdk/internal/misc/Unsafe.compareAndSet{Int,Long}(Object,long,X,X)Z`
    /// thin direct-call helper entries. Instance-field CAS only; every other
    /// shape declines to the generic dispatcher. See
    /// [`UnsafeAccessorOp::CasInt`]. `0` = not wired.
    pub unsafe_cas_int: usize,
    pub unsafe_cas_long: usize,
    /// nio21 wave 3, lane L4 (page
    /// `r21-jdk-only-volatile-unsafe-refersto0-and-carrier-thread-are-generic-native-calls-FIXED-20261012.md`):
    /// the `ACC_NATIVE` triples the `HttpClient` census found compiled code
    /// dispatching generically. `Unsafe.get{Int,Long}Volatile` /
    /// `put{Int,Long}Volatile` / `putReferenceVolatile` /
    /// `compareAndSetReference` serve an instance field (and, for the two
    /// reference forms, a reference-array element) through the SAME
    /// `NativeContext` accessor the registered native reaches, so ordering and
    /// GC barriers are the native's by construction; `fullFence` is one
    /// `MFENCE`. `0` = not wired. See [`UnsafeAccessorOp`].
    pub unsafe_get_int_volatile: usize,
    pub unsafe_get_long_volatile: usize,
    pub unsafe_put_int_volatile: usize,
    pub unsafe_put_long_volatile: usize,
    pub unsafe_put_reference_volatile: usize,
    pub unsafe_cas_reference: usize,
    pub unsafe_full_fence: usize,
    /// `java/lang/ref/Reference.refersTo0(Object)Z` and its
    /// `PhantomReference` redeclaration (nio21 wave 3, lane L4): the referent
    /// slot read WITHOUT the keep-alive barrier, as the registered native
    /// reads it, and one compare. Two slots because a decline must dispatch the
    /// triple the site names. `0` = not wired.
    pub reference_refers_to0: usize,
    pub phantom_reference_refers_to0: usize,
    /// `java/lang/Thread.currentCarrierThread()` (nio21 wave 3, lane L4): the
    /// registered native IS `currentThread`'s (`native_thread_current_thread`),
    /// so the helper is [`Self::thread_current_thread`]'s body with this
    /// triple's own gate and decline edge. `0` = not wired.
    pub thread_current_carrier_thread: usize,
    /// NIO19-2 (round nio20, wave 1, lane L2): which `Unsafe` primitive
    /// accessors the optimizing tier may answer INLINE for a heap primitive
    /// array (`ir_lower`'s `try_emit_ir_unsafe_access_prefix`), one bit per
    /// [`UnsafeInlineAccess::GATE_BITS`] row. A set bit is the VM's
    /// compile-time statement that the per-call policy the thin helper (or the
    /// funnel) would ask for that triple can only admit: this VM's capability
    /// policy is absent or `Permissive` (the default; the VM makes the
    /// first-use record itself at compile time), and the `--jdk-only` tail
    /// would run the registered native. `0` (the `EMPTY` value, and every compile without a
    /// VM) keeps every site on its call. Filled by `direct_helper_table_for`.
    /// A `usize` like every other word here, so the size census below stays a
    /// word count; only bits 0..16 are used (10..14, the volatile rows, since
    /// nio21 wave 4 L4; 14 and 15, the CAS rows the RMW prefixes ask, since
    /// nio21 wave 5 L3, cleared by the VM under `CRATONVM_UNSAFE_ARRAY_HW_CAS=0`).
    pub unsafe_inline_admitted: usize,

    /// `fn(class_id: u32) -> bool` — "has the `session()` shim actually run for a
    /// receiver of this class?"
    ///
    /// Published by `build_helpers` from `cratonvm_native_builtins::
    /// buffer_session::class_is_served`, because this crate depends on neither
    /// `native-builtins` nor `native-io` and must not.
    pub buffer_session_served_class: usize,

    /// `fn(class_id: u32, write: bool) -> bool` — "has the `ByteBuffer` element
    /// funnel actually served a receiver of this class with the modelled layout?"
    ///
    /// This crate cannot call `cratonvm_native_io::direct_buffer::elem_fastpath::
    /// class_is_served` directly (it does not depend on `native-io`, and must not:
    /// the JIT sits below the native layer), so the VM publishes the predicate as a
    /// pointer in `build_helpers`, exactly as it publishes the helper entries above.
    /// `0` = not wired, which every caller must read as "cannot prove served".
    pub nio_byte_element_served_class: usize,

    /// `MessageDigest.update(byte)` thin direct-call bind.
    ///
    /// The third rung of the same census. `testHugeDecompress` feeds SHA-256 a byte
    /// at a time 536 million times (268 M on the compress side, 268 M more through
    /// `ByteProcessor.process` on the decompress side) at 216 ns/call against
    /// HotSpot's 8.6 ns. `update` is FINAL on `MessageDigest`, so a provider's
    /// subclass cannot override it and the helper has to check the receiver itself
    /// — it serves only the exact class this VM's own `getInstance` builds, and
    /// declines everything else to the funnel, which forwards to `engineUpdate`.
    pub md_update_byte: usize,

    /// `Thread.currentThread()` thin direct-call helper — the JIT half of the
    /// funnel bypass the interpreter already has.
    ///
    /// The interpreter answers `Thread.currentThread()` inline from the thread's
    /// own `java_thread_obj` mirror (see `InterpIntrinsic::ThreadCurrentThread`),
    /// which took it from 306 ns to 136 ns under `--nojit`. **With the JIT on that
    /// fix is inert**: compiled code never consults the interpreter's inline
    /// cache, and an `invokestatic` whose callee is a registered native has no
    /// compiled body to bind, so every call fell through `jit_invoke_dispatch` to
    /// `vm_exec::invoke_or_native` — a by-name resolution *plus* the native funnel,
    /// measured at ~400 ns/call
    /// (`native-call-funnel-per-call-floor-item2-20260805.md`).
    ///
    /// The JDK leans on it constantly: **two calls per uncontended
    /// `ReentrantLock.lock()`/`unlock()` pair**, censused with
    /// `--dump-native-registry` (`probes/LockNativeCensusProbe.java`), plus every
    /// AQS ownership check and thread-local lookup.
    ///
    /// `0` = not wired → the recognition below is skipped and the site keeps the
    /// generic dispatch helper, exactly like every other `*_DIRECT_FN`.
    pub thread_current_thread: usize,

    /// Direct thin-lock monitor helpers registered by the VM at bootstrap.
    ///
    /// They stay outside `JitRuntimeHelpers` to avoid expanding that stable
    /// cross-crate ABI for process-lifetime addresses. Generated code reaches
    /// them through `runtime_lowering::emit_monitor_stub`.
    pub monitor_enter: usize,

    pub monitor_exit: usize,

    /// The DEFERRED field site (interpreter round i4 wave 92, lane L2; stage 2
    /// of `i50-L1-proposal-compile-doors-resolve-nothing`, the
    /// `i38-L2-proposal-constant-pool-indexed-field-sites` helper; switch
    /// `CRATONVM_JIT_DEFERRED_FIELD_SITES`, default OFF):
    /// `(vm_ptr, cp_holder_word(holder), packed, operands) -> value | i64::MIN`.
    /// `packed` is `cp_index | opcode << 16 | descriptor tag << 24 | NPE trap
    /// key << 32`; `operands` points at the operand block the
    /// single-pass emitter homes in the frame (`op_field.rs`
    /// `emit_deferred_field_site`, which documents its layout). The helper
    /// resolves the site through the holder's loader (JVMS §5.4.3; a failure
    /// is recorded and rethrown), runs the §6.5 linking checks and class
    /// initialisation at the program point and performs the access, so a hot
    /// method with a cold field site of a class not loaded at compile time
    /// compiles instead of being refused. HAND-WRITTEN: four words, the Win64
    /// register file. `0` = not wired: such a site keeps refusing the method.
    pub field_site_cp: usize,

    /// `jdk/internal/util/Preconditions.checkIndex(II[BiFunction])I` thin
    /// direct-call helper. Top of the `--dump-native-registry` invocation census on
    /// `probes/NioAccessorRate.java`: 4 000 000 calls for 800 000 `ByteBuffer`
    /// accessor operations, ahead of the store itself. `0` = not wired.
    pub preconditions_check_index: usize,

    /// `jdk/internal/util/DecimalDigits.uncheckedGetCharsLatin1(II[B)I` /
    /// `(JI[B)I` thin direct-call helpers (JIT round 16, lane strings,
    /// STR16-6): the digits of an `int`/`long` written into a `byte[]` in one
    /// call instead of the JDK body's per-digit `Unsafe.putByte` chain.
    /// `(vm_ptr, value, index, buf) -> charPos`. `0` = not wired.
    pub decimal_digits_latin1_int: usize,
    pub decimal_digits_latin1_long: usize,

    /// `jdk/internal/util/ArraysSupport.vectorizedHashCode(Object,IIII)I` thin
    /// direct-call helper (strings round 2, `str17`): the hash of a primitive
    /// array range folded over its payload in one call, in place of the
    /// registered `Intrinsic` native's generic door (~500 ns a fresh
    /// `String.hashCode()`). `(vm_ptr, array, fromIndex, length,
    /// initialValue, basicType) -> hash`. `0` = not wired.
    pub vectorized_hash_code: usize,

    /// `jdk/internal/util/ArraysSupport.vectorizedMismatch(Object,long,Object,long,II)I`
    /// thin direct-call helper (JIT round 22 wave 1, lane intrinsics,
    /// INTRINSICS21-1; `CRATONVM_JIT_VECTORIZED_MISMATCH_DIRECT`, opt-in): two
    /// on-heap primitive arrays of the scaled width compared in place, every
    /// other shape handed to the registered native through the generic
    /// dispatcher. `(vm_ptr, a, aOffset, b, bOffset, length,
    /// log2ArrayIndexScale) -> index | -1`. `0` = not wired.
    pub vectorized_mismatch: usize,

    /// `java/util/zip/CRC32.updateBytes0(I[BII)I` / `updateBytes(I[BII)I` and
    /// `java/util/zip/Adler32.updateBytes(I[BII)I` thin direct-call helpers (JIT
    /// round 22 wave 2, lane intrinsics, INTRINSICS22-2;
    /// `CRATONVM_JIT_ZIP_CHECKSUM_DIRECT`, opt-in): zlib's checksum over a
    /// validated `byte[]` range folded in place; every other shape goes to the
    /// registered native through the generic dispatcher. `(vm_ptr, checksum,
    /// b, off, len) -> checksum`. `0` = not wired.
    pub crc32_update_bytes0: usize,
    pub crc32_update_bytes: usize,
    pub adler32_update_bytes: usize,

    /// `byte[].clone()` thin direct-call helper (strings round 2, `str17`): a
    /// TLAB bump and a copy, in place of the inline-cache slow path an array
    /// receiver takes on every call (`Arrays.copyOfRange(v, 0, v.length)` is
    /// `v.clone()`, so every `toString()` of a full builder).
    /// `(vm_ptr, receiver) -> copy | i64::MIN`. `0` = not wired.
    pub byte_array_clone: usize,

    /// `Object.clone()` of a plain instance at an `invokespecial
    /// java/lang/Object.clone` site (`super.clone()`): a TLAB bump and the
    /// heap's one-validation field copy, in place of the inline-cache door
    /// into the registered native. `(vm_ptr, receiver) -> copy | i64::MIN`.
    /// `0` = not wired.
    pub object_clone: usize,

    /// j27 wave 3 (lane CLONE): a VIRTUAL `clone()` -- an array of any element
    /// type, or an instance whose class is plain and inherits `Object.clone`
    /// -- copied in place of the inline-cache slow path into the registered
    /// native; every other receiver is the site's virtual call through the
    /// generic dispatcher. `(vm_ptr, receiver) -> copy | i64::MIN`. `0` = not
    /// wired.
    pub object_clone_virtual: usize,

    /// CompilerOnly (`DirectHelperFnObjectCloneServedClass`, j27 wave 3, lane
    /// CLONE): does [`Self::object_clone_virtual`] serve an instance of this
    /// class? Asked with [`Self::static_base_resolver_ctx`] at compile time
    /// ([`Self::object_clone_class_is_served`]), never baked into a CALL.
    pub object_clone_served_class: usize,

    /// The `StringBuilder` append intrinsics' growth (strings round 2,
    /// `str17`): a full LATIN1 builder's `value` replaced by a copy grown by
    /// `newCapacity`'s rule, after which the inline body runs again, in place
    /// of the decline edge's generic dispatch into the JDK's growth chain.
    /// `(vm_ptr, builder, value_offset, min_capacity) -> builder | 0`
    /// (`0` = not served; the edge dispatches). Hand-written by the intrinsic.
    /// `0` = not wired.
    pub sb_grow: usize,

    /// `java/lang/ref/Reference.reachabilityFence(Object)V` thin direct-call
    /// helper. Second on the same census (3 200 000 calls), and its registered
    /// native does nothing but be opaque about its argument. `0` = not wired.
    pub reachability_fence: usize,

    /// `java/lang/Object.getClass()Class` thin direct-call helper (JIT round 17
    /// wave 1, lane nio, NIO2-2; bound only under
    /// `CRATONVM_JIT_OBJECT_GET_CLASS_DIRECT`). `(vm_ptr, receiver) -> mirror`
    /// for a plain object whose class has a mirror and a memoised
    /// stamped-class display verdict; `i64::MIN` with the JEP 358 receiver NPE
    /// for null; every other receiver declines to the generic dispatcher.
    /// `0` = not wired.
    pub object_get_class: usize,

    /// `java/lang/Class.getName()String` thin direct-call helper (JIT round 22
    /// wave 2, lane runtime, RUNTIME22-2; bound only under
    /// `CRATONVM_JIT_CLASS_GET_NAME_DIRECT`). `(vm_ptr, receiver) -> name`
    /// from the `getName()` memos with no native funnel; `i64::MIN` with the
    /// JEP 358 receiver NPE for null; every miss declines to the generic
    /// dispatcher. `0` = not wired.
    pub class_get_name: usize,

    /// `java/lang/ThreadLocal.get()Object` thin direct-call helper (SBR round 2,
    /// wave 8, lane `tlocal`). Reads the REAL `Thread.threadLocals` table for the
    /// found-key case and declines everything else to the generic dispatch. The
    /// receiver-class, policy and mode screens live in the helper. `0` = not wired.
    pub thread_local_get: usize,

    /// `java/lang/ThreadLocal.set(Object)V` thin direct-call helper (SBR round 2,
    /// wave 8, lane `tlocal`): overwrites the value of an entry already in the
    /// current thread's real table (SATB pre-barrier and card barrier through the
    /// heap's own store) and declines everything else. `0` = not wired.
    pub thread_local_set: usize,

    /// Process-lifetime bridge for `invokedynamic` sites lowered by the
    /// single-pass backend rather than trapped. It stays outside the stable
    /// helper-table ABI because the address is installed once at VM start, not per
    /// compiled artifact.
    ///
    /// ONE cell for BOTH bridged kinds — `StringConcatFactory` and, since
    /// 2026-08-23, `LambdaMetafactory`. The site metadata carries a `kind` tag the
    /// VM-side entry reads (`invokedynamic::jit_indy_site_kind`), so the call
    /// sequence and this cell stay single.
    pub indy_bridge: usize,

    /// JIT round 19 proposal wave 4, lane strgap4 (STRGAP19-3,
    /// `CRATONVM_JIT_CONCAT_INLINE`, default OFF): the compile-time reader of a
    /// concat site's inline plan, a [`DirectHelperFnConcatInlinePlan`] that
    /// maps an `indy_bridge` site pointer to its process-lived
    /// [`crate::concat_inline_plan::JitConcatInlinePlan`], or `0` for a site
    /// the plan does not admit (or a non-concat site). CompilerOnly: called by
    /// the single-pass emitter at compile time through
    /// [`DirectHelperTable::concat_inline_plan`], never baked into a CALL, so
    /// no signature row. `0` = not wired: every site keeps its bridge call.
    pub concat_inline_plan: usize,

    /// `VarHandle` READ-mode thin direct-call helper cells, one per
    /// (access mode, primitive return kind) pair — see
    /// [`varhandle_read_helper_slot`] for the index. `0` = not wired, and the
    /// recognition is skipped so the site uses the generic dispatch helper.
    ///
    /// **Why this one.** `--dump-native-registry` on `NettyZipBombPhases snappy 4`
    /// (`HttpContentDecompressorTest`'s wall) counts **21 368 822**
    /// `java/lang/invoke/VarHandle.get` calls out of 26 673 142 native calls total
    /// — ~5.1 per output byte. `DataCompressionHttp2Test` censuses the same
    /// signature at 5 356 015 of 18 371 699 (29 %). Both are netty 4.2's reference
    /// count check: `AbstractByteBuf`'s checked accessors call `ensureAccessible()`
    /// -> `RefCnt.isLiveNonVolatile`, which is `(int) VH.get(instance)` on an
    /// ordinary `int` instance field.
    ///
    /// A VM-side fast path for exactly this shape already exists
    /// (`vm/src/jit/helpers.rs::try_varhandle_instance_field_read`, keyed on the
    /// handle's identity hash), and it saves the field resolution and the boxing
    /// round trip — but it sits INSIDE `jit_invoke_dispatch`, so every call still
    /// pays the SATB flush, the reference-argument forwarding, the site-key
    /// revalidation and two thread-local map probes before reaching it. That is
    /// the per-call floor `Preconditions.checkIndex` and
    /// `Reference.reachabilityFence` were taken off for 143 -> 23 ns.
    ///
    /// **Scope: primitive returns, and reference returns in two classified kinds.**
    /// Reference returns were out of scope until 2026-09-01, on the argument that
    /// the generic path applies `unbox_poly_return_checked`, whose W6-1 rule turns
    /// *a boxed primitive reaching a non-`Object` reference return* into a
    /// `WrongMethodTypeException` — and that rule reads the CALL SITE's own
    /// descriptor, which a thin helper does not have (a baked direct call has no
    /// `JitInvokeInfo`), so the synthetic call site these helpers fall back through
    /// carries an erased `(Ljava/lang/Object;)X` descriptor, indistinguishable from
    /// the real one for a primitive `X` and NOT for a reference one.
    ///
    /// That argument was right about the erased descriptor and wrong about the
    /// conclusion, because it priced only the COLD arm. Two facts settle it:
    ///
    /// * the FAST arm cannot lose W6-1. `varhandle_instance_field_read_bits`
    ///   refuses unless the variable's own kind agrees with the site's — a
    ///   reference site over a PRIMITIVE variable, which is W6-1's entire fire
    ///   set, is declined there. The identical refusal already governs
    ///   `try_varhandle_instance_field_read`, the funnel's copy of this read,
    ///   which has served reference returns since it was written and returns raw
    ///   bits WITHOUT reaching `unbox_poly_return_checked` at all. So compiled
    ///   code's reference reads are already outside W6-1 today; binding them
    ///   changes their cost, not their semantics;
    /// * the COLD arm keeps W6-1 by CLASSIFYING the site at compile time instead
    ///   of carrying its descriptor. A boxed primitive is assignable to exactly
    ///   fourteen reference types — `java/lang/Object`, the five shared wrapper
    ///   supertypes and the eight wrappers themselves. So a reference site is one
    ///   of three things, and only the third would need the descriptor it cannot
    ///   have: `Ljava/lang/Object;` ([`VARHANDLE_READ_KIND_REF_OBJECT`]), where
    ///   the erased stand-in IS the real descriptor and W6-1 can never fire; one
    ///   of the other thirteen ([`VARHANDLE_BOX_ACCEPTING_RETURNS`]), where the
    ///   answer depends on WHICH wrapper arrived, so the site is not bound at all;
    ///   and anything else ([`VARHANDLE_READ_KIND_REF_STRICT`]), where NO boxed
    ///   primitive is assignable, so "the cold arm produced a box" is a W6-1 fire
    ///   with no further information needed — which is what
    ///   `varhandle_read_direct_impl` checks and raises on.
    ///
    /// `RJdkHandles`' `String bogus = (String) vi.get(h)` over an `int` field is a
    /// `REF_STRICT` site, and the vector that holds this honest.
    pub varhandle_read: [usize; crate::VARHANDLE_READ_SLOTS],

    /// Plain byte[] view reads, with [handle, byte[], index] coordinates.
    pub varhandle_byte_array_view_read: [usize; 6],
    /// Judged plain ByteBuffer view stores: handle, buffer, index, value.
    pub varhandle_byte_buffer_view_write: [usize; 6],
    /// Checked primitive arraycopy; refusals use the ordinary native road.
    pub system_arraycopy: usize,
    /// JIT round 23 wave 3, lane L8 (`CRATONVM_JIT_IR_FFM_ELEMENT_DIRECT`): the
    /// optimizing tier's thin `MemorySegment` element read / write, `(site, segment,
    /// layout, index[, value]) -> bits`. The first word is a JUDGED site record
    /// (`crate::VarHandleJudgedSite`, tagged): the helper serves the access on the
    /// FFM element door's cached road and otherwise dispatches the site itself.
    pub ffm_element_get: usize,
    /// See [`Self::ffm_element_get`].
    pub ffm_element_set: usize,
    /// JIT round 23 wave 4, lane L5 (`CRATONVM_JIT_REFLECT_NEW_ARRAY_DIRECT`):
    /// `java/lang/reflect/Array.newArray(Class, int)` (the native behind
    /// `Array.newInstance`, `Arrays.copyOf(.., Class)` and
    /// `Collection.toArray(T[])`), `(vm, component mirror, length) -> array |
    /// i64::MIN`. Serves a reference component's array from the caller's TLAB
    /// and dispatches the registered native for everything else.
    pub reflect_new_array: usize,
    /// JIT round 23 wave 4, lane L5 (`CRATONVM_JIT_STRING_REGEX_DIRECT`):
    /// `String.matches(String)`, `(vm, receiver, regex) -> 0/1 | i64::MIN`. Serves
    /// the registered native's exact path and dispatches the native otherwise.
    pub string_matches: usize,
    /// The same for `String.replaceAll` (`[0]`) / `replaceFirst` (`[1]`),
    /// `(vm, receiver, regex, replacement) -> String | i64::MIN`.
    pub string_replace: [usize; 2],

    /// Thin direct-call helper per (write mode, value kind), or `0` for a slot that
    /// is not served. Indexed by [`varhandle_write_helper_slot`].
    pub varhandle_write: [usize; crate::VARHANDLE_WRITE_SLOTS],

    /// Thin direct-call helper per value kind, or `0` for a slot that is not
    /// served. Indexed by [`varhandle_cas_helper_slot`].
    pub varhandle_cas: [usize; crate::VARHANDLE_CAS_SLOTS],

    /// `extern "C" fn(vm_ctx, class_id, field_index) -> i64`: the VM's
    /// compile-time static-slot resolver (`jit_resolve_static_base`), called by
    /// the backends while emitting, never from generated code. `0` = no
    /// resolver, and every `getstatic` keeps the helper. See
    /// [`DirectHelperTable::resolve_static_base`].
    pub static_base_resolver: usize,

    /// The `SharedVm` the resolver answers for. `ClassId`s are per VM, so the
    /// pair travels together on the compile it belongs to.
    pub static_base_resolver_ctx: usize,

    /// spring-bug-10 watchpoint (`CRATONVM_SHADOW_WATCH`): the VM's
    /// `jit_arm_savebase_watch(addr)`, called from the prologue. `0` = none.
    pub arm_savebase_watch: usize,

    /// The matching `jit_disarm_savebase_watch()`, called from the epilogue.
    pub disarm_savebase_watch: usize,

    /// `extern "C" fn(vm_ctx, class_id) -> i64`: `1` when the class is
    /// initialised in the VM `static_base_resolver_ctx` names, `0` when that
    /// cannot be proven. Called by the optimizing tier's `Op::New` lowering
    /// while emitting, never from generated code. `0` = not wired. See
    /// [`DirectHelperTable::class_is_initialized`].
    pub class_initialized: usize,

    /// `extern "C" fn(vm_ctx, which) -> class_id | 0`: the `ClassId` of one
    /// well-known bootstrap class ([`WELL_KNOWN_CLASS_INTEGER`] today) in the
    /// VM `static_base_resolver_ctx` names, or `0` when the VM cannot answer
    /// without blocking or the name is absent or ambiguous. Called by the
    /// single-pass emitter while emitting, never from generated code. `0` =
    /// not wired. See [`DirectHelperTable::integer_class_id`].
    ///
    /// Round 11 wave 12, lane `hashmap`: the receiver guard of the inline
    /// `Integer.hashCode()` / `Integer.equals(Object)` prefix at an
    /// `Object`-typed call site (`HashMap.hash`, `HashMap.getNode`). That
    /// site's constant pool names `java/lang/Object`, so the per-site class-id
    /// resolver the other intrinsics use cannot supply `Integer`'s id.
    pub well_known_class_id: usize,
    /// gcg g4/oo hunk 5.2 (b), applied by g4/jm: the address of this VM's
    /// Generational inline-start latch byte (`ConcurrentGcState::
    /// inline_start_flag_addr`, stable for the VM's life), published only
    /// under `CRATONVM_GEN_CONC_OWED_POLL` (`direct_helper_table_for`). Not a
    /// helper: a DATA address, carried here for the same reason as the rest
    /// of this table (no `JitRuntimeHelpers` ABI change). Non-zero, every
    /// cooperative poll of both x64 tiers also tests this byte and takes its
    /// existing slow path when it is set, where the VM runs the owed
    /// concurrent cycle outside any allocation
    /// (`gen_owed_concurrent_cycle_at_poll`). `0` = no second test: every poll
    /// is byte for byte the poll before it.
    pub gen_owed_poll_flag_addr: usize,
    /// nio20 wave 1, lane L3 (NIO19-5): the address of this VM's
    /// compiled-`getClass()` word table (`ClassRealm::getclass_mirror_words`
    /// in the VM crate): word 0 the capacity, word `1 + class_id` the class's
    /// `Class` mirror or `0`. A DATA address like
    /// [`Self::gen_owed_poll_flag_addr`], stable for the VM's life, published
    /// by `direct_helper_table_for` only under [`crate::getclass_inline_enabled`].
    /// The optimizing tier's `getClass()` prefix
    /// (`ir_lower::try_emit_ir_getclass_prefix`) bakes it. `0` = no prefix.
    pub getclass_mirror_words: usize,
    /// nio20 wave 4, lane L3 (page
    /// `r20-sp-thread-current-thread-prefix-needs-a-per-vm-thread-word-FIXED-20261012.md`):
    /// the byte offset of the current thread's `java.lang.Thread` mirror word
    /// (`JvmThread::java_thread_obj`, an `Option<ObjectRef>`: one pointer, `0`
    /// for `None`) from the `JvmThread*` the `JIT_THREAD` TLS mirror holds. A
    /// DATA word like [`Self::unsafe_inline_admitted`], never baked into a
    /// CALL. Non-zero only when this VM's per-call gate for
    /// `Thread.currentThread()` can never refuse for its life (not `--jdk-only`,
    /// no capability classification for the triple), which is when
    /// [`Self::thread_current_thread`]'s fast arm always serves a thread with
    /// a mirror. The single-pass prefix (`x64/op_invoke.rs`
    /// `try_emit_sp_current_thread_prefix`) reads it; `0` = no prefix.
    pub thread_java_obj_offset: usize,
    /// nio21 wave 7, lane L3 (W6-L3-1): the address of this VM's `Unsafe`
    /// instance-field cell table (`JitRealm::unsafe_field_cells` in the VM
    /// crate), a DATA word like [`Self::getclass_mirror_words`], stable for the
    /// VM's life and never baked into a CALL. Word 0 is the layout-replacement
    /// epoch the entries are valid for; words `1..=`[`UNSAFE_FIELD_CELL_COUNT`]
    /// are [`unsafe_field_cell_word`]s published by the VM's hardware field CAS
    /// (`vm_exec::hw_atomic_addr`'s compact arm). The RMW prefixes of both tiers
    /// bake it for their instance-field arm
    /// ([`unsafe_inline_field_cell_bytes`]). `0` = no field arm: every field
    /// site keeps its call, as before.
    pub unsafe_field_cells: usize,
    /// nio21 wave 19, lane L3: `extern "C" fn(vm_ctx, out, n) -> words | 0`,
    /// the VM's `jit_thread_local_ir_layout`: CompilerOnly, called with
    /// [`Self::static_base_resolver_ctx`] while the optimizing tier builds an
    /// `invokevirtual ThreadLocal.get()` site, never from generated code. It
    /// fills [`THREAD_LOCAL_IR_LAYOUT_WORDS`] words (the thread-word offset,
    /// the class ids and field indices the expansion bakes; see
    /// [`DirectHelperTable::thread_local_ir_layout_words`]) or answers `0`
    /// where the VM's own thin helper would not serve. `0` = not wired: every
    /// site keeps its call.
    pub thread_local_ir_layout: usize,
    /// t1 wave 8, lane SAFEPOLL (`CRATONVM_JIT_PAGE_POLL`, default ON on Linux and Windows x86-64 since j27): the base
    /// of this VM's poll page pair (`crate::poll_page::map_pair`, mapped by
    /// the VM's barrier in the low 2 GiB, stable for the VM's life). A DATA
    /// word like [`Self::gen_owed_poll_flag_addr`], never baked into a CALL:
    /// the single-pass poll bakes it as the disp32 of its page touch. `0` =
    /// no page form: every poll is the byte poll.
    pub page_poll_base: usize,
    /// j27 wave 4 (lane CLONEIR, `CRATONVM_JIT_IR_CLONE_INLINE`): the address
    /// of this VM's C JVMTI event word (`DebuggerGates::native_events`, an
    /// `AtomicU32` stable for the VM's life), published only by a VM built
    /// with `experimental-debug`. A DATA word like [`Self::page_poll_base`],
    /// never baked into a CALL. The inline array `clone()` tests
    /// [`JVMTI_ALLOC_EVENT_BITS`] in it per clone and declines to the thin
    /// helper while a `VMObjectAlloc` or `SampledObjectAlloc` listener is
    /// armed, as the helper itself declines to the native
    /// (`clone_vm_object_alloc_armed`). `0` = no such event can ever be armed
    /// (a build without the JVMTI surface): no test.
    pub jvmti_alloc_events_word: usize,
}

/// The bits of [`DirectHelperTable::jvmti_alloc_events_word`] that arm an
/// allocation event: `NativeEvent::VmObjectAlloc` (4) and
/// `NativeEvent::SampledAlloc` (8). The VM asserts the pair at compile time
/// where it publishes the word (`direct_helper_table_for`).
pub const JVMTI_ALLOC_EVENT_BITS: u32 = 4 | 8;

/// `which` argument of [`DirectHelperTable::well_known_class_id`]:
/// `java/lang/Integer`.
pub const WELL_KNOWN_CLASS_INTEGER: i64 = 1;

/// `which` argument of [`DirectHelperTable::well_known_class_id`]:
/// `java/lang/String`, answered only for the `byte[]`/`coder` layout whose
/// `hash` is an `int` (the shape the single-pass String prefix requires,
/// `StringFieldLayout::has_coder`). See [`DirectHelperTable::string_hash_field`].
pub const WELL_KNOWN_CLASS_STRING: i64 = 2;

/// `which` argument of [`DirectHelperTable::well_known_class_id`]: NOT a
/// class id, but `java/lang/String.hash`'s own field slot index PLUS ONE
/// (`0` = unknown), under the same layout screen as
/// [`WELL_KNOWN_CLASS_STRING`]. Round 11 wave 14, lane `hashmap`: the
/// optimizing tier's `Object.hashCode()` prefix reads the cached hash, and the
/// IR lowerer carries no `StringFieldLayout` to find it in.
pub const WELL_KNOWN_STRING_HASH_FIELD: i64 = 3;

/// `which` argument of [`DirectHelperTable::well_known_class_id`]: NOT a
/// class id, but the address of this VM's `compact_strings` flag byte (an
/// `AtomicBool` of the VM's class state, stable for the VM's life), or `0`.
/// JIT round 19 proposal wave 4, lane strgap4 (STRGAP19-3): the inline concat
/// tests it per evaluation, so a LATIN1 result is never built while compact
/// strings are off (`-XX:-CompactStrings`: every String is UTF16). A sparse
/// value (not `9`) so a sibling lane's next code cannot collide with it.
pub const WELL_KNOWN_COMPACT_STRINGS_FLAG: i64 = 1904;

/// `which` arguments of [`DirectHelperTable::well_known_class_id`]: NOT class
/// ids, but the addresses of this VM's GC-maintained slots holding its
/// interned `"null"` and `""` literals (JIT round 19 proposal wave 5, lane
/// concat5, STRGAP4-2), or `0` while the literal is not pooled yet. Each slot
/// is a JNI-global-reference word of THIS VM (a root the collector remaps in
/// place while every mutator is parked), so compiled code may read it raw;
/// see `vm::jit::helpers::string_literal_slot_for`. Sparse values, like
/// [`WELL_KNOWN_COMPACT_STRINGS_FLAG`].
pub const WELL_KNOWN_NULL_LITERAL_SLOT: i64 = 1905;
/// See [`WELL_KNOWN_NULL_LITERAL_SLOT`].
pub const WELL_KNOWN_EMPTY_LITERAL_SLOT: i64 = 1906;

/// `which` arguments of [`DirectHelperTable::well_known_class_id`]: NOT class
/// ids, but the addresses of the VM's LEAF TWINS of two thin helpers (JIT
/// round 19 proposal wave 7, lane tail7, OSRFRAME18-2): a twin runs only its
/// helper's fast arm and answers [`ThinHelperVmReach::LeafTwin::decline`]
/// wherever the helper would have taken its VM-reaching cold arm, so it can
/// neither allocate, raise, run Java nor walk a stack. `0` = no twin (the site
/// keeps its ordinary call). Sparse values, like
/// [`WELL_KNOWN_COMPACT_STRINGS_FLAG`].
pub const WELL_KNOWN_LEAF_TWIN_CURRENT_THREAD: i64 = 1907;
/// See [`WELL_KNOWN_LEAF_TWIN_CURRENT_THREAD`]: the twin of
/// [`DirectHelperTable::preconditions_check_index`].
pub const WELL_KNOWN_LEAF_TWIN_CHECK_INDEX: i64 = 1908;

/// `which` arguments of [`DirectHelperTable::well_known_class_id`]: the
/// addresses of four more LEAF TWINS (JIT round 19 proposal wave 8, lane
/// tail8, TAIL7-19-3; `CRATONVM_JIT_SP_TAIL_LEAF_HELPERS_WIDE`, opt-in), with
/// the contract of [`WELL_KNOWN_LEAF_TWIN_CURRENT_THREAD`]: the twin runs only
/// its helper's VM-free fast arm and answers the row's decline where the helper
/// would reach the VM. `0` = no twin in this VM (or not in this mode: the
/// `ThreadLocal.get` twin is published only where the helper's fast arm can
/// serve at all, i.e. under `--jdk-only`). A distinctive range, so a sibling
/// lane's sparse `19xx` code cannot collide with it.
pub const WELL_KNOWN_LEAF_TWIN_THREAD_LOCAL_GET: i64 = 0x7A18_0001;
/// See [`WELL_KNOWN_LEAF_TWIN_THREAD_LOCAL_GET`]: the twin of
/// [`DirectHelperTable::unsafe_cas_int`].
pub const WELL_KNOWN_LEAF_TWIN_UNSAFE_CAS_INT: i64 = 0x7A18_0002;
/// See [`WELL_KNOWN_LEAF_TWIN_THREAD_LOCAL_GET`]: the twin of
/// [`DirectHelperTable::unsafe_cas_long`].
pub const WELL_KNOWN_LEAF_TWIN_UNSAFE_CAS_LONG: i64 = 0x7A18_0003;
/// See [`WELL_KNOWN_LEAF_TWIN_THREAD_LOCAL_GET`]: the twin of
/// [`DirectHelperTable::vectorized_hash_code`].
pub const WELL_KNOWN_LEAF_TWIN_VECTORIZED_HASH_CODE: i64 = 0x7A18_0004;

/// TAIL7-19-3 (lane tail8): the decline of the `ArraysSupport.vectorizedHashCode`
/// twin. Its helper answers every `int` hash sign-extended, i.e. any value in
/// `[i32::MIN, i32::MAX]`, so the decline must lie outside that range; the
/// tail site compares it through a scratch register (it fits no `imm32`).
pub const LEAF_TWIN_DECLINE_OUTSIDE_INT: i64 = i64::MIN;

/// JIT round 19 proposal wave 7, lane tail7 (OSRFRAME18-2,
/// `CRATONVM_JIT_SP_TAIL_LEAF_HELPERS`, opt-in): can a context-taking thin
/// helper reach the VM, i.e. allocate, raise, run Java, block at a safepoint
/// or walk the stack?
///
/// This is the per-helper classification the sibling tail call
/// (`x64/op_invoke.rs`) asks. A helper that can reach the VM must keep its
/// caller's frame for the whole call: tearing the frame down first leaves the
/// frame-chain mirror naming a dead frame for every walk the helper makes (the
/// round-18 `--compatible` lost-frame regression,
/// `r18-compatible-osr-inlined-native-throw-loses-the-inline-frame-FIXED-20261006.md`),
/// and a throw from it loses the caller's line from the trace, which HotSpot
/// prints. The classification is a CLAIM ABOUT HELPER CODE that nothing
/// checks at run time, so it is kept as small as the code allows and every
/// slot not named in [`DirectHelperTable::thin_helper_vm_reach_with`] is
/// [`Self::MayReachVm`]: a new slot is safe by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThinHelperVmReach {
    /// No arm of the helper can reach the VM: it may be JUMPED to with the
    /// caller's frame already torn down. Today only
    /// `Reference.reachabilityFence`'s helper, whose whole body is a
    /// `black_box` of its argument.
    Never,
    /// The helper's FAST arm cannot reach the VM, its cold arm can, and the VM
    /// publishes a leaf twin (`twin`) that runs the fast arm only and answers
    /// `decline` -- a value the helper itself never returns -- where the
    /// helper would take the cold arm. A tail-position site may CALL the twin
    /// with its frame still up but without the VM-entry protocol (no
    /// safepoint id, no register spill, no post-call republish, no oop map,
    /// no sentinel check) and return the answer straight away; a decline falls
    /// through to the ordinary, unchanged call.
    LeafTwin { twin: usize, decline: i64 },
    /// Everything else, including every helper this table does not name.
    MayReachVm,
}

impl ThinHelperVmReach {
    /// May the caller's frame be gone while this helper runs?
    pub fn never_reaches_vm(self) -> bool {
        matches!(self, Self::Never)
    }
}

impl Default for DirectHelperTable {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl DirectHelperTable {
    /// No helper wired: every site keeps its generic dispatch path.
    pub const EMPTY: DirectHelperTable = DirectHelperTable {
        integer_value_of: 0,
        integer_int_value: 0,
        long_value_of: 0,
        long_long_value: 0,
        hashmap_put: 0,
        hashmap_get: 0,
        string_latin1_lower: 0,
        concurrent_hashmap_get: 0,
        nio_bytebuffer_put_byte: 0,
        nio_bytebuffer_get_byte: 0,
        buffer_session: 0,
        scoped_memory_get_int: 0,
        scoped_memory_get_short: 0,
        scoped_memory_get_long: 0,
        scoped_memory_get_char: 0,
        scoped_memory_put_int: 0,
        scoped_memory_put_short: 0,
        scoped_memory_put_long: 0,
        scoped_memory_put_char: 0,
        scoped_memory_get_byte_internal: 0,
        scoped_memory_put_byte_internal: 0,
        scoped_memory_get_int_internal: 0,
        scoped_memory_get_short_internal: 0,
        scoped_memory_get_long_internal: 0,
        scoped_memory_get_char_internal: 0,
        scoped_memory_put_int_internal: 0,
        scoped_memory_put_short_internal: 0,
        scoped_memory_put_long_internal: 0,
        scoped_memory_put_char_internal: 0,
        unsafe_get_byte: 0,
        unsafe_get_short: 0,
        unsafe_get_int: 0,
        unsafe_get_long: 0,
        unsafe_get_reference: 0,
        unsafe_get_reference_volatile: 0,
        unsafe_put_byte: 0,
        unsafe_put_short: 0,
        unsafe_put_int: 0,
        unsafe_put_long: 0,
        unsafe_cas_int: 0,
        unsafe_cas_long: 0,
        unsafe_get_int_volatile: 0,
        unsafe_get_long_volatile: 0,
        unsafe_put_int_volatile: 0,
        unsafe_put_long_volatile: 0,
        unsafe_put_reference_volatile: 0,
        unsafe_cas_reference: 0,
        unsafe_full_fence: 0,
        reference_refers_to0: 0,
        phantom_reference_refers_to0: 0,
        thread_current_carrier_thread: 0,
        unsafe_inline_admitted: 0,
        buffer_session_served_class: 0,
        nio_byte_element_served_class: 0,
        md_update_byte: 0,
        thread_current_thread: 0,
        monitor_enter: 0,
        monitor_exit: 0,
        field_site_cp: 0,
        preconditions_check_index: 0,
        decimal_digits_latin1_int: 0,
        decimal_digits_latin1_long: 0,
        vectorized_hash_code: 0,
        vectorized_mismatch: 0,
        crc32_update_bytes0: 0,
        crc32_update_bytes: 0,
        adler32_update_bytes: 0,
        byte_array_clone: 0,
        object_clone: 0,
        object_clone_virtual: 0,
        object_clone_served_class: 0,
        sb_grow: 0,
        reachability_fence: 0,
        object_get_class: 0,
        class_get_name: 0,
        thread_local_get: 0,
        thread_local_set: 0,
        indy_bridge: 0,
        concat_inline_plan: 0,
        varhandle_read: [0; crate::VARHANDLE_READ_SLOTS],
        varhandle_byte_array_view_read: [0; 6],
        varhandle_byte_buffer_view_write: [0; 6],
        system_arraycopy: 0,
        ffm_element_get: 0,
        ffm_element_set: 0,
        reflect_new_array: 0,
        string_matches: 0,
        string_replace: [0; 2],
        varhandle_write: [0; crate::VARHANDLE_WRITE_SLOTS],
        varhandle_cas: [0; crate::VARHANDLE_CAS_SLOTS],
        static_base_resolver: 0,
        static_base_resolver_ctx: 0,
        arm_savebase_watch: 0,
        disarm_savebase_watch: 0,
        class_initialized: 0,
        well_known_class_id: 0,
        gen_owed_poll_flag_addr: 0,
        getclass_mirror_words: 0,
        thread_java_obj_offset: 0,
        unsafe_field_cells: 0,
        thread_local_ir_layout: 0,
        page_poll_base: 0,
        jvmti_alloc_events_word: 0,
    };

    /// j27 wave 3 (lane CLONE): does the virtual `clone()` helper
    /// ([`Self::object_clone_virtual`]) serve an instance of class `class_id`
    /// (plain, `Object.clone` inherited)? `false` when not wired, no VM
    /// context, or class id 0. Compile time only.
    pub fn object_clone_class_is_served(&self, class_id: u32) -> bool {
        let raw = self.object_clone_served_class;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 || class_id == 0 {
            return false;
        }
        // SAFETY: the VM fills this field from `jit_object_clone_served_class`,
        // an `extern "C" fn(i64, i64) -> i64` with process lifetime; a test
        // fills it from a function of the same signature.
        let f: DirectHelperFnObjectCloneServedClass = unsafe { std::mem::transmute(raw) };
        // Casts: the context and the class id as the C ABI's i64s.
        //
        // SAFETY: `ctx` is the `SharedVm` published beside `f`, alive for
        // every compilation it runs.
        unsafe { f(ctx as i64, i64::from(class_id)) == 1 }
    }

    /// `java/lang/Integer`'s `ClassId` in this compile's VM, or `None` when
    /// it cannot be shown: no resolver, no VM context, a name that is absent
    /// or defined more than once, or a class manager the VM could not read
    /// without waiting. Class id 0 (`java/lang/Object`) is never an answer.
    pub fn integer_class_id(&self) -> Option<u32> {
        let raw = self.well_known_class_id;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 {
            return None;
        }
        // SAFETY: the VM fills this field from `jit_well_known_class_id`, an
        // `extern "C" fn(i64, i64) -> i64` with process lifetime
        // (`direct_helper_table_for`); a test fills it from a function of the
        // same signature.
        let f: DirectHelperFnWellKnownClassId = unsafe { std::mem::transmute(raw) };
        // Cast: `usize` context to the C ABI's i64 parameter.
        //
        // SAFETY: `ctx` is the `SharedVm` published beside `f`, and it
        // outlives every compilation it runs.
        let id = unsafe { f(ctx as i64, WELL_KNOWN_CLASS_INTEGER) };
        u32::try_from(id).ok().filter(|&id| id != 0)
    }

    /// `(java/lang/String's ClassId, String.hash's field slot index)` in this
    /// compile's VM, or `None` when either cannot be shown (the reasons
    /// [`Self::integer_class_id`] lists, or a String without the
    /// `byte[]`/`coder` shape). Class id 0 is never an answer. Read by the
    /// optimizing tier's `Object.hashCode()` prefix
    /// (`ir_lower::try_emit_ir_box_hash_equals_prefix`), which builds the two
    /// `hash` addresses from the index with `AtomicIntFieldLayout::new`.
    pub fn string_hash_field(&self) -> Option<(u32, usize)> {
        let raw = self.well_known_class_id;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 {
            return None;
        }
        // SAFETY: as in `integer_class_id`.
        let f: DirectHelperFnWellKnownClassId = unsafe { std::mem::transmute(raw) };
        // Cast: `usize` context to the C ABI's i64 parameter.
        //
        // SAFETY: as in `integer_class_id`.
        let id = unsafe { f(ctx as i64, WELL_KNOWN_CLASS_STRING) };
        let cid = u32::try_from(id).ok().filter(|&id| id != 0)?;
        // SAFETY: as above.
        let field_plus_one = unsafe { f(ctx as i64, WELL_KNOWN_STRING_HASH_FIELD) }; // Cast: as above
        let field = usize::try_from(field_plus_one).ok()?.checked_sub(1)?;
        Some((cid, field))
    }

    /// Is `class_id` PROVEN initialised in this compile's VM? `false` whenever
    /// that cannot be shown: no predicate, no VM context, class id 0.
    pub fn class_is_initialized(&self, class_id: u32) -> bool {
        let raw = self.class_initialized;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 || class_id == 0 {
            return false;
        }
        // SAFETY: the VM fills this field from `jit_class_initialized`, an
        // `extern "C" fn(i64, i64) -> i64` with process lifetime
        // (`direct_helper_table_for`); a test fills it from a function of the
        // same signature.
        let f: DirectHelperFnClassInitialized = unsafe { std::mem::transmute(raw) };
        // Cast: `usize` context to the C ABI's i64 parameter.
        //
        // SAFETY: `ctx` is the `SharedVm` published beside `f`, and it
        // outlives every compilation it runs.
        unsafe { f(ctx as i64, i64::from(class_id)) != 0 }
    }

    /// Ask the VM where a static field's storage base pointer lives.
    ///
    /// `None` = "not inlineable, keep the helper": no resolver on this table,
    /// or the VM declined (class not initialized, the class is
    /// `java/lang/System`, nothing published, index switched off).
    ///
    /// The resolver used to be a process-wide registration latched to the
    /// first VM, which a second VM had to poison for both. It is per compile
    /// now, so each VM's compiles ask their own VM.
    pub fn resolve_static_base(&self, class_id_raw: u32, field_index: usize) -> Option<usize> {
        let raw = self.static_base_resolver;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 {
            return None;
        }
        // SAFETY: the VM fills this field from `jit_resolve_static_base`, an
        // `extern "C" fn(i64, i64, i64) -> i64` with process lifetime
        // (`direct_helper_table_for`); a test fills it from a function of the
        // same signature. Named through the alias so the shape is stated once
        // and is what `docs/jit/helper-abi.md` §10 asks the producer to coerce
        // against.
        let f: DirectHelperFnStaticBaseResolver = unsafe { std::mem::transmute(raw) };
        // Cast: `usize`/`u32` inputs to the C ABI's i64 parameters.
        //
        // SAFETY: `ctx` was published beside `f` as the context that resolver
        // expects (the `SharedVm`), and it outlives every compilation it runs.
        let addr = unsafe { f(ctx as i64, class_id_raw as i64, field_index as i64) };
        if addr == 0 {
            None
        } else {
            // Cast: back to an address; `0` is the sentinel, everything else is
            // a real (positive, user-space) pointer.
            Some(addr as u64 as usize)
        }
    }

    /// Would the `session()` shim answer for a receiver of `class_id`? `false`
    /// whenever it cannot prove `true`: an unwired predicate or class id 0.
    pub fn buffer_session_class_is_served(&self, class_id: u32) -> bool {
        let raw = self.buffer_session_served_class;
        if raw == 0 || class_id == 0 {
            return false;
        }
        // SAFETY: the VM fills this field from a
        // `DirectHelperFnBufferSessionServedClass` item with process lifetime
        // (`direct_helper_table`), and nothing else writes it.
        //
        // `extern "C"`, NOT the bare `fn(u32) -> bool` this used to transmute
        // to. The Rust ABI is explicitly UNSPECIFIED and carries no stability
        // guarantee between separately compiled crates or across codegen
        // flags; the whole point of erasing the address to a `usize` is that
        // the two ends compile separately. See the module header's third call
        // path, and `docs/jit/helper-abi.md` §10.
        let f: DirectHelperFnBufferSessionServedClass = unsafe { std::mem::transmute(raw) };
        // SAFETY: the address is a live `extern "C"` item of exactly this
        // signature; the predicate reads a process-global served-class table
        // and touches no JIT-supplied pointer.
        unsafe { f(class_id) }
    }

    /// The inline plan of the concat `indy_bridge` site `site`, or `None`:
    /// an unwired reader, a null site, or a site the VM's plan does not admit
    /// (JIT round 19 proposal wave 4, lane strgap4, STRGAP19-3).
    pub fn concat_inline_plan(
        &self,
        site: usize,
    ) -> Option<&'static crate::concat_inline_plan::JitConcatInlinePlan> {
        let raw = self.concat_inline_plan;
        if raw == 0 || site == 0 {
            return None;
        }
        // SAFETY: the VM fills this field from `jit_concat_inline_plan`, an
        // `extern "C" fn(i64) -> i64` with process lifetime
        // (`direct_helper_table`); a test fills it from a function of the same
        // signature.
        let f: DirectHelperFnConcatInlinePlan = unsafe { std::mem::transmute(raw) };
        // Cast: the site pointer to the C ABI's i64 parameter.
        //
        // SAFETY: `site` is an `indy_bridge` site pointer the VM handed this
        // compile; the reader checks its kind before reading it as a concat
        // site.
        let plan = unsafe { f(site as i64) };
        if plan == 0 {
            return None;
        }
        // SAFETY: a non-zero answer is a pointer to a plan owned by the
        // process-lived site allocation (never freed), so `'static` holds.
        // Cast: the answer back to an address.
        Some(unsafe { &*(plan as usize as *const crate::concat_inline_plan::JitConcatInlinePlan) })
    }

    /// The address of this compile's VM's `compact_strings` flag byte, or
    /// `None` when it cannot be shown (STRGAP19-3; see
    /// [`WELL_KNOWN_COMPACT_STRINGS_FLAG`]).
    pub fn compact_strings_flag_addr(&self) -> Option<usize> {
        let raw = self.well_known_class_id;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 {
            return None;
        }
        // SAFETY: as in `integer_class_id`.
        let f: DirectHelperFnWellKnownClassId = unsafe { std::mem::transmute(raw) };
        // Cast: `usize` context to the C ABI's i64 parameter.
        //
        // SAFETY: as in `integer_class_id`.
        let addr = unsafe { f(ctx as i64, WELL_KNOWN_COMPACT_STRINGS_FLAG) };
        usize::try_from(addr).ok().filter(|&a| a != 0)
    }

    /// OSRFRAME18-2 (lane tail7): how far the thin helper at `entry` can
    /// reach. See [`ThinHelperVmReach`]. `entry == 0` and every slot not named
    /// here answer [`ThinHelperVmReach::MayReachVm`].
    ///
    /// * `reachability_fence` -- [`ThinHelperVmReach::Never`]: its body is
    ///   `note_jit_boundary()` (an empty function) and a `black_box` of the
    ///   referent, with no panic guard that could raise.
    /// * `thread_current_thread` -- a leaf twin answering `0` (the fast arm
    ///   reads the thread's own mirror word; a thread mirror is never null).
    ///   The helper's cold arm is `jit_invoke_dispatch`.
    /// * `preconditions_check_index` -- a leaf twin answering `-1` (the fast
    ///   arm answers only an index in `[0, length)`). The helper's cold arm
    ///   dispatches the registered native, which RAISES: HotSpot's trace of
    ///   that `IndexOutOfBoundsException` names the caller, so the helper
    ///   itself is never jumped to.
    ///
    /// No other helper qualifies under this switch: every other slot either
    /// allocates on its fast arm or has a dispatching cold arm and no twin
    /// (reviewed by lane tail7 against `vm/src/jit/helpers.rs`). Four more
    /// twins exist behind their own switch: [`Self::thin_helper_vm_reach_with`].
    /// Test-only since JIT round 22 wave 7 (lane review): production asks the `_with` form.
    #[cfg(test)]
    pub fn thin_helper_vm_reach(&self, entry: usize) -> ThinHelperVmReach {
        self.thin_helper_vm_reach_with(entry, true, false)
    }

    /// `thin_helper_vm_reach` over the rows each switch admits:
    /// `narrow` the wave-7 rows (`CRATONVM_JIT_SP_TAIL_LEAF_HELPERS`: the
    /// fence and the two twins above), `wide` the wave-8 rows
    /// (`CRATONVM_JIT_SP_TAIL_LEAF_HELPERS_WIDE`, lane tail8, TAIL7-19-3):
    ///
    /// * `thread_local_get` -- a leaf twin answering `-1` (never an aligned
    ///   reference). The fast arm reads the current thread's REAL
    ///   `threadLocals` table (`tl_find_entry`: field and array reads, a
    ///   per-thread layout memo, the class store under a read lock) and
    ///   allocates nothing. The cold arms are `jit_invoke_dispatch` (twice)
    ///   and the null receiver's NPE.
    /// * `unsafe_cas_int` / `unsafe_cas_long` -- leaf twins answering `-1`
    ///   (the helper answers only `0` / `1` from its fast arm). The fast arm
    ///   is one `LOCK CMPXCHG` on a declared instance field
    ///   (`unsafe_cas_serve`: the forwarding read, the capability gate, the
    ///   field-width proof). The cold arms dispatch the registered native and
    ///   raise the null receiver's NPE.
    /// * `vectorized_hash_code` -- a leaf twin answering
    ///   [`LEAF_TWIN_DECLINE_OUTSIDE_INT`] (the helper answers any `int`).
    ///   The fast arm folds a validated primitive array's payload; the cold
    ///   arm dispatches the registered native.
    ///
    /// Reviewed and NOT twinned (every other context-taking slot): the
    /// `valueOf` boxes, `byte[].clone`, `sb_grow` and `StringLatin1.toLowerCase`
    /// allocate on their fast arms; `HashMap.put` runs Java; `HashMap.get` /
    /// `ConcurrentMap.get` flush the SATB queue first and run the native
    /// funnel on their fast arms (`safe_native_call`), and `--jdk-only`
    /// refuses them; `ThreadLocal.set` writes a reference through the SATB
    /// and card barriers; the monitor helpers block; `MessageDigest.update`
    /// runs the digest's Java; the NIO / `ScopedMemoryAccess` / `VarHandle`
    /// / indy rows belong to other rounds' areas; and `Integer.intValue`,
    /// `Long.longValue`, `Object.getClass` and the eight `Unsafe` primitive
    /// accessors already have a default-ON INLINE single-pass prefix
    /// (`SP_THIN_PREFIX_ROWS`), which serves their fast arm with no call at
    /// all -- a twin would only add a call where that prefix declines.
    pub fn thin_helper_vm_reach_with(
        &self,
        entry: usize,
        narrow: bool,
        wide: bool,
    ) -> ThinHelperVmReach {
        if entry == 0 {
            return ThinHelperVmReach::MayReachVm;
        }
        if narrow && entry == self.reachability_fence {
            return ThinHelperVmReach::Never;
        }
        let (which, decline) = if narrow && entry == self.thread_current_thread {
            (WELL_KNOWN_LEAF_TWIN_CURRENT_THREAD, 0)
        } else if narrow && entry == self.preconditions_check_index {
            (WELL_KNOWN_LEAF_TWIN_CHECK_INDEX, -1)
        } else if wide && entry == self.thread_local_get {
            (WELL_KNOWN_LEAF_TWIN_THREAD_LOCAL_GET, -1)
        } else if wide && entry == self.unsafe_cas_int {
            (WELL_KNOWN_LEAF_TWIN_UNSAFE_CAS_INT, -1)
        } else if wide && entry == self.unsafe_cas_long {
            (WELL_KNOWN_LEAF_TWIN_UNSAFE_CAS_LONG, -1)
        } else if wide && entry == self.vectorized_hash_code {
            (
                WELL_KNOWN_LEAF_TWIN_VECTORIZED_HASH_CODE,
                LEAF_TWIN_DECLINE_OUTSIDE_INT,
            )
        } else {
            return ThinHelperVmReach::MayReachVm;
        };
        match self.well_known_word(which) {
            Some(twin) => ThinHelperVmReach::LeafTwin { twin, decline },
            None => ThinHelperVmReach::MayReachVm,
        }
    }

    /// One non-zero answer of the well-known channel, or `None` (no resolver,
    /// no VM context, or a `0` answer).
    fn well_known_word(&self, which: i64) -> Option<usize> {
        let raw = self.well_known_class_id;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 {
            return None;
        }
        // SAFETY: as in `integer_class_id`.
        let f: DirectHelperFnWellKnownClassId = unsafe { std::mem::transmute(raw) };
        // Cast: `usize` context to the C ABI's i64 parameter.
        //
        // SAFETY: as in `integer_class_id`.
        let word = unsafe { f(ctx as i64, which) };
        usize::try_from(word).ok().filter(|&w| w != 0)
    }

    /// The address of this compile's VM's rooted `"null"` (`empty == false`)
    /// or `""` literal slot, or `None` (STRGAP4-2; see
    /// [`WELL_KNOWN_NULL_LITERAL_SLOT`]).
    pub fn string_literal_slot(&self, empty: bool) -> Option<usize> {
        let raw = self.well_known_class_id;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 {
            return None;
        }
        let which = if empty {
            WELL_KNOWN_EMPTY_LITERAL_SLOT
        } else {
            WELL_KNOWN_NULL_LITERAL_SLOT
        };
        // SAFETY: as in `integer_class_id`.
        let f: DirectHelperFnWellKnownClassId = unsafe { std::mem::transmute(raw) };
        // Cast: `usize` context to the C ABI's i64 parameter.
        //
        // SAFETY: as in `integer_class_id`.
        let addr = unsafe { f(ctx as i64, which) };
        usize::try_from(addr).ok().filter(|&a| a != 0)
    }

    /// Would the thin `ByteBuffer` element helper SERVE a receiver of
    /// `class_id`? `false` whenever it cannot prove `true`: an unwired
    /// predicate, class id 0, or a class the funnel has not served.
    pub fn nio_byte_element_class_is_served(&self, class_id: u32, write: bool) -> bool {
        let raw = self.nio_byte_element_served_class;
        if raw == 0 || class_id == 0 {
            return false;
        }
        // SAFETY: as in `buffer_session_class_is_served` above, including why
        // the fn-pointer type is `extern "C"` rather than `extern "Rust"`.
        let f: DirectHelperFnNioByteElementServedClass = unsafe { std::mem::transmute(raw) };
        // SAFETY: see above.
        unsafe { f(class_id, write) }
    }
}

// ---------------------------------------------------------------------------
// The typed ABI description of this table (`docs/jit/helper-abi.md` §10).
//
// Mirrors `jit-api/src/helpers_abi.rs`'s `helper_fn_slots!` for the second
// helper table. Each alias carries the real C signature of the function the VM
// stores in the slot; `DIRECT_HELPER_FN_SIGS` is DERIVED from those aliases
// (via the same `HelperArgAbi`/`HelperRetAbi` classification traits), so there
// is no second list to keep in step.
//
// What this half CANNOT do, and where the other half lives: a `const _` that
// coerces `jit_integer_value_of_direct` to `DirectHelperFnIntegerValueOf` has
// to be compiled where that function is nameable, i.e. beside
// `direct_helper_table()` in `vm/src/jit/helpers.rs`. Without it these aliases
// pin the SHAPE the emitter assumes and not the function actually stored — the
// exact gap `helper-abi.md` §10 spells out with the block to paste.
// ---------------------------------------------------------------------------

use cratonvm_jit_api::{HelperArgAbi, HelperRetAbi, SYSV_INT_ARG_REGS, WIN64_INT_ARG_REGS};

/// How a [`DirectHelperTable`] slot's address reaches a `CALL`.
///
/// The distinction is load-bearing for the argument-count assertions below:
/// two of the three paths have general stack-argument marshalling and one does
/// not. Collapsing them — which is what applying `jit-api`'s flat
/// `HELPERS_NEEDING_WIN64_STACK_ARGS == 1` pin to this table would do — would
/// either reject nine legitimate rows or stop policing the four that matter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectHelperCallPath {
    /// The emitter loads the platform argument registers itself and emits an
    /// absolute `CALL`. There is no stack-argument path on this route, so the
    /// helper's arity must fit the SMALLER of the two register files (Win64's
    /// four). Today: the two monitor helpers, the two savebase watchpoints and
    /// the StringBuilder intrinsics' `sb_grow`.
    HandWritten,
    /// The address is installed as a `JitDirectCall` entry and reached through
    /// the backends' general Java-call argument marshalling, which stages
    /// arguments beyond the register file onto the stack
    /// (`emit_stack_arg_setup`). Arity above four is fine here and is why the
    /// five-word `varhandle_cas` and the six/seven-word `ScopedMemoryAccess`
    /// accessors are admitted at all.
    JavaArgMarshalled,
    /// Never present in generated code: the compiler calls it from Rust while
    /// emitting. For these the operative convention is the one the `transmute`
    /// in this file names, which is why they are `extern "C"` on both ends.
    CompilerOnly,
}

/// Signature shape of one callable [`DirectHelperTable`] slot, derived from
/// its `DirectHelperFn*` alias.
///
/// The `jit-api` twin is [`cratonvm_jit_api::HelperFnSig`]; this one adds
/// [`DirectHelperCallPath`] and drops the `accessor` field, because this table
/// has no `<field>_fn` accessors to name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectHelperFnSig {
    /// Rust field name on [`DirectHelperTable`]. For the three array slots
    /// this names the array; every element has the same signature by
    /// construction (one `const SLOT: usize` monomorphisation each).
    pub field: &'static str,
    /// Name of the `DirectHelperFn*` alias carrying the full signature.
    pub alias: &'static str,
    /// Number of declared parameters — the number of argument words the call
    /// site must produce.
    pub arity: usize,
    /// How many of those are floating-point. Zero for every row today; the
    /// field exists so a future FP thin helper cannot be absorbed silently.
    pub float_args: usize,
    /// `false` only for a `-> ()` helper.
    pub returns_value: bool,
    /// How the address reaches a `CALL`.
    pub path: DirectHelperCallPath,
}

impl DirectHelperFnSig {
    /// Parameters that consume an integer argument register.
    pub const fn int_args(&self) -> usize {
        self.arity - self.float_args
    }

    /// Parameters that do not fit Win64's four integer argument registers and
    /// must therefore be written above the 32-byte shadow space.
    pub const fn win64_stack_args(&self) -> usize {
        if self.arity > WIN64_INT_ARG_REGS {
            self.arity - WIN64_INT_ARG_REGS
        } else {
            0
        }
    }

    /// The same question for SysV's six. Non-zero for the four
    /// `ScopedMemoryAccess` *put* accessors, which take seven words and so
    /// spill on BOTH ABIs — a fact that was nowhere written down.
    pub const fn sysv_stack_args(&self) -> usize {
        if self.arity > SYSV_INT_ARG_REGS {
            self.arity - SYSV_INT_ARG_REGS
        } else {
            0
        }
    }
}

macro_rules! direct_helper_fn_slots {
    ( $( $alias:ident, $field:ident, $path:ident, ( $($arg:ty),* ) -> $ret:ty ; )* ) => {
        $(
            #[doc = concat!(
                "C signature of the `", stringify!($field),
                "` slot of [`DirectHelperTable`]."
            )]
            ///
            /// Declared `unsafe` for the same reason the `HelperFn*` aliases
            /// are: the value reached the table as a raw address and nothing in
            /// the type system ties it to the function the VM intended.
            pub type $alias = unsafe extern "C" fn( $($arg),* ) -> $ret;
        )*

        /// Per-slot C signature shape for every callable [`DirectHelperTable`]
        /// slot, derived from the same rows that declare the aliases.
        pub const DIRECT_HELPER_FN_SIGS: &[DirectHelperFnSig] = &[
            $(
                DirectHelperFnSig {
                    field: stringify!($field),
                    alias: stringify!($alias),
                    arity: 0 $( + direct_helper_fn_slots!(@one $arg) )*,
                    float_args: 0 $( + (<$arg as HelperArgAbi>::IS_FLOAT as usize) )*,
                    returns_value: <$ret as HelperRetAbi>::RETURNS_VALUE,
                    path: DirectHelperCallPath::$path,
                },
            )*
        ];

        // One pin per row: every integer or pointer argument is a full machine
        // word. A narrower one is a silent ABI lie — the caller produces a
        // whole 64-bit argument word either way, and the callee would read only
        // part of it.
        //
        // TRIPS ON: declaring an argument `i32`, `u32`, `bool`, … in a row
        // above. (The two served-class predicates DO take a `u32` and a `bool`
        // — which is why they are not rows here; they are never in generated
        // code, and their convention is pinned by the `transmute` site instead.)
        $(
            const _: () = {
                let args_are_machine_words = true
                    $( && (<$arg as HelperArgAbi>::IS_FLOAT
                           || <$arg as HelperArgAbi>::WIDTH == 8) )*;
                assert!(
                    args_are_machine_words,
                    concat!(
                        "an argument of the `", stringify!($field), "` direct \
                         helper is narrower than a machine word",
                    ),
                );
            };
        )*
    };
    (@one $arg:ty) => { 1usize };
}

direct_helper_fn_slots! {
    // Autoboxing. (vm_ptr, value) / (vm_ptr, receiver).
    DirectHelperFnIntegerValueOf, integer_value_of, JavaArgMarshalled, (i64, i64) -> i64;
    DirectHelperFnIntegerIntValue, integer_int_value, JavaArgMarshalled, (i64, i64) -> i64;
    DirectHelperFnLongValueOf, long_value_of, JavaArgMarshalled, (i64, i64) -> i64;
    DirectHelperFnLongLongValue, long_long_value, JavaArgMarshalled, (i64, i64) -> i64;

    // Collections. (vm_ptr, receiver, key[, value]).
    DirectHelperFnHashMapPut, hashmap_put, JavaArgMarshalled, (i64, i64, i64, i64) -> i64;
    DirectHelperFnHashMapGet, hashmap_get, JavaArgMarshalled, (i64, i64, i64) -> i64;
    DirectHelperFnConcurrentHashMapGet, concurrent_hashmap_get, JavaArgMarshalled,
        (i64, i64, i64) -> i64;

    // (vm_ptr, source, value, locale) — `value` is the second, unused operand
    // of the JDK's own `StringLatin1.toLowerCase(String, byte[], Locale)`.
    DirectHelperFnStringLatin1Lower, string_latin1_lower, JavaArgMarshalled,
        (i64, i64, i64, i64) -> i64;

    // NIO byte accessors. (vm_ptr, receiver, index[, value]).
    DirectHelperFnNioByteBufferPutByte, nio_bytebuffer_put_byte, JavaArgMarshalled,
        (i64, i64, i64, i64) -> i64;
    DirectHelperFnNioByteBufferGetByte, nio_bytebuffer_get_byte, JavaArgMarshalled,
        (i64, i64, i64) -> i64;

    // (vm_ptr, receiver) -> the constant-null MemorySessionImpl.
    DirectHelperFnBufferSession, buffer_session, JavaArgMarshalled, (i64, i64) -> i64;

    // `ScopedMemoryAccess.*Unaligned`. The READ accessors take
    // (vm_ptr, receiver, session, obj, offset, big_endian) — six words, one
    // over SysV's file only on Win64. The WRITE accessors interpose `val`
    // before `big_endian` for SEVEN, which overflows BOTH register files.
    // Both shapes are `JitDirectCall`-marshalled, which is what makes them
    // legal; see `DirectHelperCallPath`.
    DirectHelperFnScopedMemoryGetInt, scoped_memory_get_int, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnScopedMemoryGetShort, scoped_memory_get_short, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnScopedMemoryGetLong, scoped_memory_get_long, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnScopedMemoryGetChar, scoped_memory_get_char, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnScopedMemoryPutInt, scoped_memory_put_int, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnScopedMemoryPutShort, scoped_memory_put_short, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnScopedMemoryPutLong, scoped_memory_put_long, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnScopedMemoryPutChar, scoped_memory_put_char, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64, i64) -> i64;

    DirectHelperFnScopedMemoryGetByteInternal, scoped_memory_get_byte_internal, JavaArgMarshalled,
        (i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnScopedMemoryPutByteInternal, scoped_memory_put_byte_internal, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64) -> i64;
    // `*Internal` twins -- same shapes, see ScopedMemoryOp's doc comment.
    DirectHelperFnScopedMemoryGetIntInternal, scoped_memory_get_int_internal, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnScopedMemoryGetShortInternal, scoped_memory_get_short_internal, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnScopedMemoryGetLongInternal, scoped_memory_get_long_internal, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnScopedMemoryGetCharInternal, scoped_memory_get_char_internal, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnScopedMemoryPutIntInternal, scoped_memory_put_int_internal, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnScopedMemoryPutShortInternal, scoped_memory_put_short_internal, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnScopedMemoryPutLongInternal, scoped_memory_put_long_internal, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnScopedMemoryPutCharInternal, scoped_memory_put_char_internal, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64, i64) -> i64;

    // `Unsafe.get{Byte,Short,Int,Long}(Object,long)`. (vm_ptr, receiver, obj,
    // offset) -- four words, fits every ABI's register file. `put` interposes
    // `val` for FIVE, one over Win64's four integer registers; both shapes are
    // `JitDirectCall`-marshalled like the `ScopedMemoryAccess` rows above.
    DirectHelperFnUnsafeGetByte, unsafe_get_byte, JavaArgMarshalled, (i64, i64, i64, i64) -> i64;
    DirectHelperFnUnsafeGetShort, unsafe_get_short, JavaArgMarshalled, (i64, i64, i64, i64) -> i64;
    DirectHelperFnUnsafeGetInt, unsafe_get_int, JavaArgMarshalled, (i64, i64, i64, i64) -> i64;
    DirectHelperFnUnsafeGetLong, unsafe_get_long, JavaArgMarshalled, (i64, i64, i64, i64) -> i64;
    DirectHelperFnUnsafeGetReference, unsafe_get_reference, JavaArgMarshalled,
        (i64, i64, i64, i64) -> i64;
    DirectHelperFnUnsafeGetReferenceVolatile, unsafe_get_reference_volatile, JavaArgMarshalled,
        (i64, i64, i64, i64) -> i64;
    DirectHelperFnUnsafePutByte, unsafe_put_byte, JavaArgMarshalled,
        (i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnUnsafePutShort, unsafe_put_short, JavaArgMarshalled,
        (i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnUnsafePutInt, unsafe_put_int, JavaArgMarshalled,
        (i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnUnsafePutLong, unsafe_put_long, JavaArgMarshalled,
        (i64, i64, i64, i64, i64) -> i64;
    // `Unsafe.compareAndSet{Int,Long}(Object,long,X,X)Z`. (vm_ptr, receiver,
    // obj, offset, expected, new) -- six words, two over Win64's four integer
    // registers; `JitDirectCall`-marshalled like the seven-word
    // `ScopedMemoryAccess` puts above.
    DirectHelperFnUnsafeCasInt, unsafe_cas_int, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnUnsafeCasLong, unsafe_cas_long, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64) -> i64;
    // nio21 wave 3, lane L4: the volatile accessors, the reference store and
    // CAS, and `fullFence` -- the shapes of the rows above (a get is four
    // words, a put five, a CAS six; `fullFence` is `(vm_ptr, receiver)`).
    DirectHelperFnUnsafeGetIntVolatile, unsafe_get_int_volatile, JavaArgMarshalled,
        (i64, i64, i64, i64) -> i64;
    DirectHelperFnUnsafeGetLongVolatile, unsafe_get_long_volatile, JavaArgMarshalled,
        (i64, i64, i64, i64) -> i64;
    DirectHelperFnUnsafePutIntVolatile, unsafe_put_int_volatile, JavaArgMarshalled,
        (i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnUnsafePutLongVolatile, unsafe_put_long_volatile, JavaArgMarshalled,
        (i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnUnsafePutReferenceVolatile, unsafe_put_reference_volatile, JavaArgMarshalled,
        (i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnUnsafeCasReference, unsafe_cas_reference, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnUnsafeFullFence, unsafe_full_fence, JavaArgMarshalled, (i64, i64) -> i64;
    // (vm_ptr, receiver, other) -> 0 | 1 | i64::MIN.
    DirectHelperFnReferenceRefersTo0, reference_refers_to0, JavaArgMarshalled,
        (i64, i64, i64) -> i64;
    DirectHelperFnPhantomReferenceRefersTo0, phantom_reference_refers_to0, JavaArgMarshalled,
        (i64, i64, i64) -> i64;
    // (vm_ptr) -> the current Thread mirror, as `thread_current_thread`.
    DirectHelperFnThreadCurrentCarrierThread, thread_current_carrier_thread, JavaArgMarshalled,
        (i64) -> i64;

    // (vm_ptr, receiver, value).
    DirectHelperFnMdUpdateByte, md_update_byte, JavaArgMarshalled, (i64, i64, i64) -> i64;

    // (vm_ptr) -> the current Thread mirror.
    DirectHelperFnThreadCurrentThread, thread_current_thread, JavaArgMarshalled, (i64) -> i64;

    // (vm_ptr, obj) -> possibly-remapped obj | i64::MIN. HAND-WRITTEN:
    // `runtime_lowering::emit_monitor_stub` loads ENTRY_ABI_REGS[0..1] itself.
    DirectHelperFnMonitorEnter, monitor_enter, HandWritten, (i64, i64) -> i64;
    // (vm_ptr, obj) -> 1 | i64::MIN. NOTE the return is `1`, NOT the object —
    // see `emit_monitor_stub`, which is why the store-back is enter-only.
    DirectHelperFnMonitorExit, monitor_exit, HandWritten, (i64, i64) -> i64;
    // (vm_ptr, holder_word, packed, operands) -> value | i64::MIN. HAND-WRITTEN
    // by `op_field.rs` `emit_deferred_field_site` (interpreter round i4 wave
    // 92, lane L2). `operands` is a frame address, carried as a word.
    DirectHelperFnFieldSiteCp, field_site_cp, HandWritten, (i64, i64, i64, i64) -> i64;

    // (vm_ptr, index, length, formatter).
    DirectHelperFnPreconditionsCheckIndex, preconditions_check_index, JavaArgMarshalled,
        (i64, i64, i64, i64) -> i64;
    // (vm_ptr, value, index, buf) -> charPos. STR16-6.
    DirectHelperFnDecimalDigitsLatin1Int, decimal_digits_latin1_int, JavaArgMarshalled,
        (i64, i64, i64, i64) -> i64;
    DirectHelperFnDecimalDigitsLatin1Long, decimal_digits_latin1_long, JavaArgMarshalled,
        (i64, i64, i64, i64) -> i64;
    // (vm_ptr, array, fromIndex, length, initialValue, basicType) -> hash.
    // Six words, `JitDirectCall`-marshalled like the CAS rows. str17.
    DirectHelperFnVectorizedHashCode, vectorized_hash_code, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64) -> i64;
    // (vm_ptr, a, aOffset, b, bOffset, length, log2Scale) -> index | -1.
    // Seven words, `JitDirectCall`-marshalled like the ScopedMemoryAccess puts.
    // JIT round 22 wave 1, lane intrinsics (INTRINSICS21-1).
    DirectHelperFnVectorizedMismatch, vectorized_mismatch, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64, i64) -> i64;
    // (vm_ptr, checksum, b, off, len) -> checksum. Five words, one Win64 stack
    // argument. JIT round 22 wave 2, lane intrinsics (INTRINSICS22-2).
    DirectHelperFnCrc32UpdateBytes0, crc32_update_bytes0, JavaArgMarshalled,
        (i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnCrc32UpdateBytes, crc32_update_bytes, JavaArgMarshalled,
        (i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnAdler32UpdateBytes, adler32_update_bytes, JavaArgMarshalled,
        (i64, i64, i64, i64, i64) -> i64;
    // (vm_ptr, receiver) -> the copy | i64::MIN. str17.
    DirectHelperFnByteArrayClone, byte_array_clone, JavaArgMarshalled, (i64, i64) -> i64;
    // (vm_ptr, receiver) -> the copy | i64::MIN. `super.clone()` to `Object.clone`.
    DirectHelperFnObjectClone, object_clone, JavaArgMarshalled, (i64, i64) -> i64;
    // (vm_ptr, receiver) -> the copy | i64::MIN. A virtual `clone()` (j27 wave 3).
    DirectHelperFnObjectCloneVirtual, object_clone_virtual, JavaArgMarshalled, (i64, i64) -> i64;
    // (vm_ptr, builder, value_offset, min_capacity) -> builder | 0.
    // Called by the StringBuilder intrinsics' growth stub, no safepoint. str17.
    DirectHelperFnSbGrow, sb_grow, HandWritten, (i64, i64, i64, i64) -> i64;
    // (vm_ptr, referent) -> 0. Java-`void`, but NOT `-> ()`: every
    // `JavaArgMarshalled` slot is reached through the `JitDirectCall` sequence,
    // which compares RAX against the `i64::MIN` sentinel after the CALL even
    // for a `V` site, so a `()` return made that compare read whatever the
    // callee last left in RAX. Pinned by
    // `every_java_arg_marshalled_slot_returns_a_word` below.
    DirectHelperFnReachabilityFence, reachability_fence, JavaArgMarshalled, (i64, i64) -> i64;
    // (vm_ptr, receiver) -> the receiver's `Class` mirror (a reference word) |
    // i64::MIN. JIT round 17 wave 1, lane nio.
    DirectHelperFnObjectGetClass, object_get_class, JavaArgMarshalled, (i64, i64) -> i64;
    // (vm_ptr, receiver) -> the mirror's `getName()` String (a reference word)
    // | i64::MIN. JIT round 22 wave 2, lane runtime (RUNTIME22-2).
    DirectHelperFnClassGetName, class_get_name, JavaArgMarshalled, (i64, i64) -> i64;
    // (vm_ptr, receiver) -> the thread-local's value (a reference word).
    DirectHelperFnThreadLocalGet, thread_local_get, JavaArgMarshalled, (i64, i64) -> i64;
    // (vm_ptr, receiver, value) -> 0 (a Java `void`, but a word: see the rows above).
    DirectHelperFnThreadLocalSet, thread_local_set, JavaArgMarshalled, (i64, i64, i64) -> i64;

    // (vm_ptr, site_ptr, args_ptr, arg_count). `args_ptr` is a staged
    // descending word block the bridge reads `arg_count` words from.
    DirectHelperFnIndyBridge, indy_bridge, JavaArgMarshalled,
        (i64, i64, *const i64, i64) -> i64;

    // The three `VarHandle` arrays. One signature per array: every element is a
    // `const SLOT: usize` monomorphisation of one function, so the slot index
    // is a compile-time constant in the CALL target rather than an argument.
    // (vm_ptr, vh, receiver) / (vm_ptr, vh, receiver, value) /
    // (vm_ptr, vh, receiver, expected, new).
    //
    // The write returns `0` or `i64::MIN`: the word IS the channel that
    // delivers the native's refusal (CCE / UOE) to the caller's own handler
    // at the call bci. It was `-> ()` until round 11 wave 8, and the refusal
    // escaped the caller's `catch`.
    DirectHelperFnVarHandleRead, varhandle_read, JavaArgMarshalled, (i64, i64, i64) -> i64;
    DirectHelperFnVarHandleByteArrayViewRead, varhandle_byte_array_view_read, JavaArgMarshalled,
        (i64, i64, i64, i64) -> i64;
    DirectHelperFnVarHandleByteBufferViewWrite, varhandle_byte_buffer_view_write, JavaArgMarshalled,
        (i64, i64, i64, i64, i64) -> i64;
    DirectHelperFnSystemArraycopy, system_arraycopy, JavaArgMarshalled,
        (i64, i64, i64, i64, i64, i64) -> i64;
    // (site, segment, layout, index) -> bits | i64::MIN; (.., value) -> 0 | i64::MIN.
    // JIT round 23 wave 3, lane L8. The write's five words spill on Win64.
    DirectHelperFnFfmElementGet, ffm_element_get, JavaArgMarshalled,
        (i64, i64, i64, i64) -> i64;
    DirectHelperFnFfmElementSet, ffm_element_set, JavaArgMarshalled,
        (i64, i64, i64, i64, i64) -> i64;
    // (vm, component mirror, length) -> array | i64::MIN. JIT round 23 wave 4,
    // lane L5. Three words, all in registers on both ABIs.
    DirectHelperFnReflectNewArray, reflect_new_array, JavaArgMarshalled,
        (i64, i64, i64) -> i64;
    // (vm, receiver, regex[, replacement]) -> bits | i64::MIN. JIT round 23
    // wave 4, lane L5. Three and four words, all in registers on both ABIs.
    DirectHelperFnStringMatches, string_matches, JavaArgMarshalled, (i64, i64, i64) -> i64;
    DirectHelperFnStringReplace, string_replace, JavaArgMarshalled,
        (i64, i64, i64, i64) -> i64;
    DirectHelperFnVarHandleWrite, varhandle_write, JavaArgMarshalled,
        (i64, i64, i64, i64) -> i64;
    DirectHelperFnVarHandleCas, varhandle_cas, JavaArgMarshalled,
        (i64, i64, i64, i64, i64) -> i64;

    // spring-bug-10 watchpoints, called from the prologue/epilogue with a
    // hand-loaded ARG_REGS[0] (`x64/frames.rs`).
    DirectHelperFnArmSavebaseWatch, arm_savebase_watch, HandWritten, (i64) -> ();
    DirectHelperFnDisarmSavebaseWatch, disarm_savebase_watch, HandWritten, () -> ();
}

// The four [`DirectHelperCallPath::CompilerOnly`] slots.
//
// Not rows above because their arguments are deliberately NOT machine words:
// they are ordinary Rust calls the compiler makes while emitting, so a `u32`
// class id and a `bool` are the honest types. What they DO need — and did not
// have — is `extern "C"`.

/// `extern "C" fn(class_id) -> served` — the `buffer_session` predicate.
///
/// Published by the VM from `cratonvm_native_builtins::buffer_session::
/// class_is_served`, because this crate depends on neither `native-builtins`
/// nor `native-io` and must not.
pub type DirectHelperFnBufferSessionServedClass = unsafe extern "C" fn(u32) -> bool;

/// `extern "C" fn(class_id, write) -> served` — the `ByteBuffer` element
/// funnel's served-class predicate, from `cratonvm_native_io::direct_buffer::
/// elem_fastpath::class_is_served`.
pub type DirectHelperFnNioByteElementServedClass = unsafe extern "C" fn(u32, bool) -> bool;

/// `extern "C" fn(vm_ctx, class_id, field_index) -> base_addr | 0` — the
/// compile-time static-slot resolver, called by the backends while emitting
/// and never from generated code.
pub type DirectHelperFnStaticBaseResolver = unsafe extern "C" fn(i64, i64, i64) -> i64;

/// `extern "C" fn(vm_ctx, class_id) -> 1 | 0`: the compile-time "class is
/// initialised" predicate the optimizing tier's `Op::New` asks before taking
/// the inline TLAB bump (`r11w3-lower-ir-new-needs-a-direct-initialized-bit`).
pub type DirectHelperFnClassInitialized = unsafe extern "C" fn(i64, i64) -> i64;

/// `extern "C" fn(vm_ctx, which) -> class_id | 0`: the well-known-class
/// resolver the single-pass emitter asks for `java/lang/Integer`'s id
/// ([`DirectHelperTable::integer_class_id`]). Compiler-only, like the two
/// above.
pub type DirectHelperFnWellKnownClassId = unsafe extern "C" fn(i64, i64) -> i64;

/// `extern "C" fn(vm_ctx, class_id) -> 1 | 0`: does the virtual `clone()`
/// helper serve an instance of this class
/// ([`DirectHelperTable::object_clone_class_is_served`], j27 wave 3, lane
/// CLONE)? Compiler-only, like the two above.
pub type DirectHelperFnObjectCloneServedClass = unsafe extern "C" fn(i64, i64) -> i64;

/// `extern "C" fn(vm_ctx, out, n) -> words | 0`: the compile-time
/// `ThreadLocal.get()` layout reader ([`DirectHelperTable::thread_local_ir_layout`],
/// nio21 wave 19, lane L3). Compiler-only.
pub type DirectHelperFnThreadLocalIrLayout = unsafe extern "C" fn(i64, i64, i64) -> i64;

/// How many words [`DirectHelperTable::thread_local_ir_layout`] fills (19
/// since the referent row, word 18).
pub const THREAD_LOCAL_IR_LAYOUT_WORDS: usize = 19;

/// The `n` that asks [`DirectHelperTable::thread_local_ir_layout`] for the
/// `Reference.referent` index alone (one word), which the VM answers without
/// the `ThreadLocal` classes the full layout needs
/// ([`DirectHelperTable::reference_referent_index`], JIT round 21, lane refs2).
pub const REFERENCE_REFERENT_QUERY_WORDS: usize = 1;

/// JIT round 21 (lane g1vhsoft, `CRATONVM_JIT_IR_REFERENCE_GET_INLINE`): the
/// `n` that asks [`DirectHelperTable::thread_local_ir_layout`] for the
/// `WeakReference.get()` / `SoftReference.get()` expansion's words
/// ([`DirectHelperTable::reference_get_layout_words`]). Distinct from every
/// other query size (1, 2 and the full layout's 18 or more).
pub const REFERENCE_GET_LAYOUT_QUERY_WORDS: usize = 9;

/// JIT round 21 (lane tlsub, TL321-4): the `n` that asks
/// [`DirectHelperTable::thread_local_ir_layout`] about ONE receiver class
/// ([`DirectHelperTable::thread_local_receiver_kind`]). Word 0 carries the
/// class id in and the map kind out ([`TL_RECEIVER_KIND_PLAIN`],
/// [`TL_RECEIVER_KIND_INHERITABLE`], anything else: the class's own bytecode
/// decides); word 1 comes back `1` when that class declares no `set(Object)`
/// of its own below `ThreadLocal` either.
pub const THREAD_LOCAL_RECEIVER_QUERY_WORDS: usize = 2;

/// TLGET21-7: the `n` that asks [`DirectHelperTable::thread_local_ir_layout`]
/// whether the layout is PENDING -- this VM serves the expansion (`--jdk-only`
/// with the real-field natives) but the `ThreadLocal` classes it bakes are
/// not all loaded yet ([`DirectHelperTable::thread_local_layout_pending`]).
/// Word 0 comes back `1` for pending.
pub const THREAD_LOCAL_LAYOUT_PENDING_QUERY_WORDS: usize = 3;

/// [`THREAD_LOCAL_RECEIVER_QUERY_WORDS`]'s kind: `ThreadLocal.get` reads
/// `Thread.threadLocals` for this class (the VM's `TL_MAP_PLAIN`).
pub const TL_RECEIVER_KIND_PLAIN: u8 = 0;
/// ... reads `Thread.inheritableThreadLocals` (the VM's `TL_MAP_INHERITABLE`).
pub const TL_RECEIVER_KIND_INHERITABLE: u8 = 1;

/// t1 wave 12 (lane REFS4, `CRATONVM_JIT_IR_REFERS_TO_COMPACT_ROW`): the `n`
/// that asks [`DirectHelperTable::thread_local_ir_layout`] for
/// `Reference.referent`'s index AND its compact row
/// ([`DirectHelperTable::reference_referent_row`]).
/// [`REFERENCE_REFERENT_ROW_ANSWER_WORDS`] words come back: the index
/// (`REF_FIELD_REFERENT`, `0`), `1 + compact displacement` of the referent in
/// `java/lang/ref/Reference`'s registered compact layout (`0` = no row: not
/// loaded, not compact, or a collector whose referent reads owe a gate the
/// inline read does not run), and `Reference`'s class id (`0` = not loaded).
/// The producer tells its queries apart by `n` alone, so this is distinct
/// from [`REFERENCE_REFERENT_QUERY_WORDS`] (1),
/// [`THREAD_LOCAL_RECEIVER_QUERY_WORDS`] (2),
/// [`THREAD_LOCAL_LAYOUT_PENDING_QUERY_WORDS`] (3) and the full layout's
/// [`THREAD_LOCAL_IR_LAYOUT_WORDS`] (it was 3 until the dev merge of
/// 2026-10-27 brought TLGET21-7's pending query on 3).
pub const REFERENCE_REFERENT_ROW_QUERY_WORDS: usize = 4;

/// How many words the [`REFERENCE_REFERENT_ROW_QUERY_WORDS`] answer fills
/// (and the producer returns): index, row, class id, and (j27 wave 5, lane
/// REFS) the COMPARE-ONLY row: the same `1 + compact displacement` for a
/// read that owes no keep-alive (`refersTo`), named wherever the referent
/// read gate can never arm -- Gen, G1, and a ZGC heap without the
/// after-release referent clear -- else `0`
/// ([`DirectHelperTable::reference_referent_compare_row`]).
pub const REFERENCE_REFERENT_ROW_ANSWER_WORDS: usize = 4;

/// j27 wave 5 (lane REFS): the `n` that asks
/// [`DirectHelperTable::thread_local_ir_layout`] whether this VM's heap may
/// ever arm the ZGC referent read gate
/// ([`DirectHelperTable::referent_gate_may_arm`]). One word comes back, `1`
/// for may arm. Distinct from every other query size of the producer.
pub const REFERENT_GATE_MAY_ARM_QUERY_WORDS: usize = 5;

/// `extern "C" fn(site) -> plan | 0`: the compile-time concat inline-plan
/// reader (JIT round 19 proposal wave 4, lane strgap4, STRGAP19-3).
/// Compiler-only, like the three above.
pub type DirectHelperFnConcatInlinePlan = unsafe extern "C" fn(i64) -> i64;

/// How many direct helpers overflow Win64's four integer argument registers.
///
/// Twenty-one: the eight `ScopedMemoryAccess` accessors, their eight
/// `*Internal` twins (identical shape, six words for a get and seven for a
/// put — see `ScopedMemoryOp`'s doc comment for why they are separate slots),
/// `varhandle_cas`, and the four `Unsafe.put{Byte,Short,Int,Long}` accessors
/// (`vm_ptr, receiver, obj, offset, val` — one word over Win64's four). The
/// `Unsafe` GET accessors do NOT spill (`vm_ptr, receiver, obj, offset` fits
/// exactly), and neither do the `*Internal` GET twins (six words, same as
/// their public siblings, still over Win64's four but that is already
/// counted the same way the public GET rows are). Every spilling row is
/// [`DirectHelperCallPath::JavaArgMarshalled`], which is the property the
/// const assertion below actually pins — the count alone would be satisfied
/// by a new hand-written five-argument helper, which is the bug.
pub const DIRECT_HELPERS_NEEDING_WIN64_STACK_ARGS: usize = {
    let mut n = 0;
    let mut i = 0;
    while i < DIRECT_HELPER_FN_SIGS.len() {
        if DIRECT_HELPER_FN_SIGS[i].win64_stack_args() > 0 {
            n += 1;
        }
        i += 1;
    }
    n
};

/// How many direct helpers overflow SysV's six — i.e. spill on both ABIs.
///
/// Four: the `ScopedMemoryAccess` *put* accessors, at seven words each.
pub const DIRECT_HELPERS_NEEDING_SYSV_STACK_ARGS: usize = {
    let mut n = 0;
    let mut i = 0;
    while i < DIRECT_HELPER_FN_SIGS.len() {
        if DIRECT_HELPER_FN_SIGS[i].sysv_stack_args() > 0 {
            n += 1;
        }
        i += 1;
    }
    n
};

// Arity and calling-convention shape of the second helper table.
//
// TRIPS ON (in order): a HAND-WRITTEN helper that no longer fits the Win64
// register file — the one shape with no stack-argument path anywhere, and the
// one that would silently pass garbage; a direct helper with a floating-point
// argument, which no marshalling path here assigns; a change in how many
// helpers spill on either ABI.
const _: () = {
    let mut i = 0;
    while i < DIRECT_HELPER_FN_SIGS.len() {
        let s = DIRECT_HELPER_FN_SIGS[i];
        if matches!(s.path, DirectHelperCallPath::HandWritten) {
            assert!(
                s.win64_stack_args() == 0,
                "a hand-written direct-helper call site takes more arguments \
                 than Win64's four integer registers hold. The emitters that \
                 bake these (`runtime_lowering::emit_monitor_stub`, \
                 `x64/frames.rs`'s savebase watchpoints) load ENTRY_ABI_REGS \
                 and nothing else — there is no shadow-space stack-argument \
                 path on that route, so the overflow arguments would be \
                 whatever those registers happened to hold.",
            );
        }
        assert!(
            s.float_args == 0,
            "a direct helper declares a floating-point argument. Neither the \
             hand-written path nor `JitDirectCall`'s marshalling assigns XMM \
             argument registers for these, so the value would arrive in the \
             wrong register file.",
        );
        i += 1;
    }
    assert!(
        DIRECT_HELPERS_NEEDING_WIN64_STACK_ARGS == 37,
        "the number of direct helpers spilling arguments on Win64 changed. The \
         thirty-two include varhandle_byte_buffer_view_write, system_arraycopy and the two byte Internal accessors, eight ScopedMemoryAccess accessors, their eight \
         *Internal twins, varhandle_cas, the four Unsafe \
         put{{Byte,Short,Int,Long}} accessors, the two Unsafe \
         compareAndSet{{Int,Long}} forms, vectorized_hash_code, and (nio21 wave 3, \
         lane L4) put{{Int,Long,Reference}}Volatile and compareAndSetReference, and (JIT r22 \
         wave 1, lane intrinsics) vectorized_mismatch, and (wave 2) the three \
         CRC32 / Adler32 checksum rows, and (JIT r23 wave 3, lane L8) \
         ffm_element_set, all reached through \
         JitDirectCall's `emit_stack_arg_setup`. A new one is fine on that \
         route and is NOT fine on the hand-written route — say which by \
         giving the new row its DirectHelperCallPath, then update this count.",
    );
    assert!(
        DIRECT_HELPERS_NEEDING_SYSV_STACK_ARGS == 9,
        "the number of direct helpers spilling arguments on SysV changed. The \
         eight are the ScopedMemoryAccess `put` accessors and their `*Internal` \
         twins, at seven words each, and (JIT r22 wave 1, lane intrinsics) \
         vectorized_mismatch, also seven words.",
    );
};

// The census pin: every callable slot has a signature row.
//
// There is no reflection over a struct's fields, so the check is indirect but
// exact — `DirectHelperTable` is all `usize` and `[usize; N]`, so its size is a
// faithful count of its slots. Appending a field grows the struct, this
// assertion fails, and its message names the obligation. That is the one check
// "add a row when you add a slot" can actually have here.
//
// 157 words = 36 scalar slots + 40 field read + 6 byte[] view read + 40 write + 10 CAS + 8 Unsafe scalar
// slots (get{Byte,Short,Int,Long} + put{Byte,Short,Int,Long}) + 8
// ScopedMemoryAccess *Internal twins + 2 Unsafe compareAndSet{Int,Long}
// + 4 (SBR round 2, wave 8, lane `tlocal`): `thread_local_get`, `thread_local_set`
// and the two Unsafe reference-array reads `unsafe_get_reference{,_volatile}`
// + 2 (JIT round 16, lane strings): `decimal_digits_latin1_{int,long}`
// + 1 data word, `gen_owed_poll_flag_addr` (gcg g4/jm: the address of the
// per-VM owed-cycle latch a poll tests; never baked into a CALL, so no
// signature row).
// The 33rd scalar slot is `class_initialized` (round 11 wave 4) and the 34th
// `well_known_class_id` (round 11 wave 12): CompilerOnly predicates called by
// the compiler through a transmute, never baked into a CALL, so neither has a
// signature row (the latter's convention is pinned by
// `DirectHelperFnWellKnownClassId` and the producer-side coercion in
// `vm/src/jit/helpers.rs`).
// 53 signature rows = 39 + 8 (one per *Internal slot) + 4 (SBR2 W8) + 2 (one per
// compareAndSet slot; six words each, so no SysV spill and a two-word Win64
// one), + 2 `decimal_digits_latin1_{int,long}` (round 16, lane strings) =
// 58 rows over 157 words (the gen_owed_poll_flag_addr slot is the
// `gen_owed_poll_flag_addr` data word, which has no row). Rows are per FIELD, not per array element: the `R` kind round 11
// wave 18 added to the VarHandle write (+4) and CAS (+1) arrays grew the word
// count and left the row count alone.
// + 1 slot and 1 row (JIT round 17 wave 1, lane nio): `object_get_class` =
// 56 rows over 150 words.
// + 1 CompilerOnly word, no row (JIT round 19 proposal wave 4, lane strgap4,
// STRGAP19-3): `concat_inline_plan` = 162 words.
// + 1 data word, no row (nio20 wave 1, lane L3): `getclass_mirror_words`, the
// address of the per-VM compiled-`getClass()` table; never baked into a CALL.
// + 1 data word, no row (nio20 wave 1, lane L2): `unsafe_inline_admitted`, a
// compile-time policy bitmask; never baked into a CALL.
// + 1 data word, no row (nio20 wave 4, lane L3): `thread_java_obj_offset`, a
// `JvmThread` field offset; never baked into a CALL.
// + 10 slots and 10 rows (nio21 wave 3, lane L4): `unsafe_{get,put}_{int,long}_volatile`,
// `unsafe_put_reference_volatile`, `unsafe_cas_reference`, `unsafe_full_fence`,
// `{,phantom_}reference_refers_to0` and `thread_current_carrier_thread` =
// 73 rows over 174 words.
// + 1 data word, no row (nio21 wave 7, lane L3): `unsafe_field_cells`, the
// address of the per-VM `Unsafe` instance-field cell table; never baked into a
// CALL. 73 rows over 175 words.
// + the round 19 `concat_inline_plan` word above (merged from dev beside the
// nio21 slots): 73 rows over 176 words.
// + 1 CompilerOnly word, no row (nio21 wave 19, lane L3):
// `thread_local_ir_layout`, called at compile time through a transmute
// (`DirectHelperFnThreadLocalIrLayout`), never baked into a CALL. 73 rows over
// 178 words (including system_arraycopy).
// + 6 judged ByteBuffer-view SET words, one signature row (ABI 18):
// 185 words, 75 signature rows, including the independent field_site_cp row.
// The five-word ABI spills on Win64.
// + 1 slot and 1 row (JIT round 22 wave 1, lane intrinsics, ABI 20):
// `vectorized_mismatch`, seven words through the marshalled path = 186 words,
// 76 signature rows.
// + 1 slot and 1 row (JIT round 22 wave 2, lane runtime, ABI 21):
// `class_get_name`, two words = 187 words, 77 signature rows.
// + 3 slots and 3 rows (JIT round 22 wave 2, lane intrinsics, ABI 22):
// `crc32_update_bytes0`, `crc32_update_bytes`, `adler32_update_bytes`, five
// words each through the marshalled path = 190 words, 80 signature rows.
// + 2 slots and 2 rows (JIT round 23 wave 3, lane L8, ABI 23): `ffm_element_get`
// (four words) and `ffm_element_set` (five, one Win64 stack argument) = 192
// words, 82 signature rows.
// + 1 slot and 1 row (JIT round 23 wave 4, lane L5, ABI 24): `reflect_new_array`
// (three words) = 193 words, 83 signature rows.
// + 3 slots and 2 rows (JIT round 23 wave 4, lane L5, ABI 25): `string_matches`
// (three words) and `string_replace` (two four-word slots) = 196 words, 85 rows.
// + 1 data word, no row (t1 wave 8, lane SAFEPOLL): `page_poll_base`, the
// per-VM poll page pair a page-form poll touches; never baked into a CALL =
// 197 words, 85 rows.
// + 1 slot and 1 row (ABI 27): `object_clone` (two words) = 198 words, 86 rows.
// + 1 slot and 1 row, + 1 CompilerOnly word, no row (j27 wave 3, lane CLONE,
// ABI 28): `object_clone_virtual` (two words) and `object_clone_served_class`
// = 200 words, 87 rows.
// + 1 data word, no row (j27 wave 4, lane CLONEIR, ABI 29):
// `jvmti_alloc_events_word`; never baked into a CALL = 201 words, 87 rows.
const _: () = {
    assert!(
        core::mem::size_of::<DirectHelperTable>() == 201 * core::mem::size_of::<usize>(),
        "`DirectHelperTable` changed shape. Every slot the emitter can bake \
         into a CALL needs a `direct_helper_fn_slots!` row carrying its real C \
         signature, or it is back to being a raw `usize` that type-checks \
         against nothing — which is the entire reason this table's ABI \
         description exists. Add the row (and the producer-side coercion check \
         described in docs/jit/helper-abi.md §10), then update this word count \
         and the row count below.",
    );
    assert!(
        DIRECT_HELPER_FN_SIGS.len() == 87,
        "the number of described direct-helper slots changed without the \
         struct changing size, or vice versa — the two counts are a pair.",
    );
};

/// ABI revision of the [`DirectHelperTable`] layout and signatures.
///
/// Unlike [`cratonvm_jit_api::JIT_HELPERS_ABI_VERSION`] this table is passed by
/// value between two crates compiled together, so the number is documentation
/// and a review anchor rather than a wire contract: bump it whenever a row is
/// added, removed, or has its signature changed, so a VM-side
/// `direct_helper_table()` written against an older shape is identifiable.
///
/// 3 (round 11 wave 8): `reachability_fence` and `varhandle_write` return
/// `i64` instead of `()`.
///
/// 4 (round 11 wave 18): `varhandle_write` grows from 36 to 40 slots and
/// `varhandle_cas` from 9 to 10 — a tenth value kind `R` (a reference site
/// typed as a non-`Object`-group class or an array) per mode. Signatures
/// unchanged.
///
/// 5 (SBR round 2, wave 8, lane `tlocal`): `thread_local_get` /
/// `thread_local_set` and `unsafe_get_reference{,_volatile}` rows added.
///
/// 6 (round 16, lane strings): `decimal_digits_latin1_int` /
/// `decimal_digits_latin1_long` rows added.
///
/// 7 (gcg g4/jm, merged over dev's 5 and 6): one data word,
/// `gen_owed_poll_flag_addr`. No signature changed.
/// 8 (Netty scoped byte access): byte get/put internal rows added. Both use
/// JitDirectCall's marshalled argument path, including Win64 stack arguments.
/// 9 (Netty Snappy): six plain byte[] view read words and their four-word ABI row.
/// 10 (JIT round 17 wave 1, lane nio, merged after 9): the `object_get_class` row.
/// 11 (strings round 2, `str17`, merged after 10): `byte_array_clone`, two
/// words, `vectorized_hash_code`, six words through JitDirectCall's
/// marshalled path (two Win64 stack arguments), and `sb_grow`, four words
/// hand-written by the StringBuilder append intrinsics' growth stub.
/// 12 (nio20 wave 1, lane L3): one data word, `getclass_mirror_words`. No
/// signature changed.
/// 13 (nio20 wave 1, lane L2): one data word, `unsafe_inline_admitted`. No
/// signature changed.
/// 14 (nio20 wave 4, lane L3): one data word, `thread_java_obj_offset`. No
/// signature changed.
/// 15 (nio21 wave 3, lane L4): ten rows -- the `Unsafe` volatile accessors,
/// `putReferenceVolatile`, `compareAndSetReference`, `fullFence`, the two
/// `refersTo0` triples and `Thread.currentCarrierThread`.
/// 16 (nio21 wave 7, lane L3): one data word, `unsafe_field_cells`. No
/// signature changed.
/// 17: primitive arraycopy; 18: six judged ByteBuffer view-write slots.
/// 19: reconcile the independent deferred `field_site_cp` row with those slots.
/// 20 (JIT round 22 wave 1, lane intrinsics): the `vectorized_mismatch` row,
/// seven words through the marshalled path.
/// 21 (JIT round 22 wave 2, lane runtime): the `class_get_name` row, two words.
/// 22 (JIT round 22 wave 2, lane intrinsics): the `crc32_update_bytes0`,
/// `crc32_update_bytes` and `adler32_update_bytes` rows, five words each.
/// 23 (JIT round 23 wave 3, lane L8): the `ffm_element_get` / `ffm_element_set`
/// rows, four and five words.
/// 24 (JIT round 23 wave 4, lane L5): the `reflect_new_array` row, three words.
/// 25 (JIT round 23 wave 4, lane L5): the `string_matches` and `string_replace`
/// rows, three and four words.
/// 26 (t1 wave 8, lane SAFEPOLL): one data word, `page_poll_base`. No
/// signature changed.
/// 27 (in-process `javac` residual, 2026-10-27): the `object_clone` row, two
/// words.
/// 28 (j27 wave 3, lane CLONE): the `object_clone_virtual` row, two words,
/// and one CompilerOnly word, `object_clone_served_class`.
/// 29 (j27 wave 4, lane CLONEIR): one data word, `jvmti_alloc_events_word`.
/// No signature changed.
pub const DIRECT_HELPER_ABI_VERSION: u32 = 29;

/// Signature shape of the direct-helper slot named `field`, or `None`.
///
/// The twin of [`cratonvm_jit_api::helper_sig`], and `const fn` for the same
/// reason: the intended use is a compile-time assertion at a hand-written call
/// site, not a runtime check nobody arms.
pub const fn direct_helper_sig(field: &str) -> Option<DirectHelperFnSig> {
    let mut i = 0;
    while i < DIRECT_HELPER_FN_SIGS.len() {
        if cratonvm_jit_api::str_eq(DIRECT_HELPER_FN_SIGS[i].field, field) {
            return Some(DIRECT_HELPER_FN_SIGS[i]);
        }
        i += 1;
    }
    None
}

/// [`direct_helper_sig`] for a name the caller gives as a literal, failing the
/// build when no such slot exists.
pub const fn direct_helper_sig_of(field: &str) -> DirectHelperFnSig {
    match direct_helper_sig(field) {
        Some(sig) => sig,
        None => panic!(
            "no `direct_helper_fn_slots!` row declares this field — either the \
             name is misspelled at the call site, or a slot was added to \
             `DirectHelperTable` without a signature alias",
        ),
    }
}

/// Compile-time assertion that a hand-written direct-helper call site agrees
/// with the slot's declared signature.
///
/// The [`cratonvm_jit_api::assert_helper_call_shape`] twin for the second
/// table. See that macro for the hazard; the difference here is that it also
/// pins the row's [`DirectHelperCallPath`], because a slot that moves from
/// `JavaArgMarshalled` to `HandWritten` loses its stack-argument marshalling
/// without changing its arity.
#[macro_export]
macro_rules! assert_direct_helper_call_shape {
    ($field:literal, int_args = $n:expr, returns_value = $r:expr $(,)?) => {
        const _: () = {
            let sig = $crate::direct_helpers::direct_helper_sig_of($field);
            assert!(
                sig.int_args() == $n,
                concat!(
                    "the hand-written call site for the `",
                    $field,
                    "` direct \
                     helper loads a number of argument registers that is not \
                     the helper's declared arity.",
                ),
            );
            assert!(
                sig.returns_value == $r,
                concat!(
                    "the hand-written call site for the `",
                    $field,
                    "` direct \
                     helper disagrees with the declared return-ness.",
                ),
            );
            assert!(
                matches!(
                    sig.path,
                    $crate::direct_helpers::DirectHelperCallPath::HandWritten
                ),
                concat!(
                    "the `",
                    $field,
                    "` direct helper is declared as reached \
                     through a path other than the hand-written one, but a \
                     hand-written call site is asserting its shape.",
                ),
            );
            assert!(
                sig.win64_stack_args() == 0,
                concat!(
                    "the `",
                    $field,
                    "` direct helper overflows the Win64 \
                     integer argument registers and this call site has no \
                     stack-argument marshalling.",
                ),
            );
        };
    };
}

/// A classified `ScopedMemoryAccess.*Unaligned` operation.
///
/// # Public and `Internal` are separate variants, not one merged alias
///
/// Real `ScopedMemoryAccess.putIntUnaligned` (public) is `invokevirtual
/// putIntUnalignedInternal` on the SAME arguments, unchanged (javap-verified
/// against JDK 25: `aload_0; aload_1; aload_2; lload_3; iload 5; iload 6;
/// invokevirtual putIntUnalignedInternal`), wrapped only in a try/catch that
/// converts a `ScopedAccessError` sentinel into a real `RuntimeException`.
/// Every one of the eight pairs this enum covers has the same shape.
///
/// A single shared variant here (as this used to be) computes the SAME byte
/// copy for both names, which is correct on the SERVE path -- but the
/// DECLINE path's `jit_invoke_dispatch` call bakes in ONE fixed
/// `JitInvokeInfo`, naming ONE of the two methods by name. Once `putIntUnaligned`
/// itself gets hot enough to be JIT-compiled on its own (independent of any
/// caller's compile state -- HotSpot-style, a callee tiers up on its own call
/// count), ITS OWN `invokevirtual putIntUnalignedInternal` call site is ALSO
/// bound to this same thin helper. If a decline then always dispatches back
/// to `"putIntUnaligned"` by name -- the public method, not the internal one
/// that was actually intercepted -- that call re-enters the SAME compiled
/// method, which contains the SAME bound-and-declining call site, forever:
/// `StackOverflowError`, thousands of `ScopedMemoryAccess.putIntUnaligned`
/// frames deep. Measured with `UnsafeCrossingPerf`'s
/// `sma.putIntUnaligned(null, arr, …)` kernel, reproduces in both `default`
/// and `--jdk-only`, and stops reproducing the instant `CRATONVM_JIT_SCOPED_MEMORY_DIRECT=0`
/// disables this whole family -- narrowing it to exactly this mechanism.
///
/// Splitting into two variants per operation means a decline always names
/// the SAME method the compiled call site actually was, so it can never loop
/// back into itself. The two variants share their byte-copy core entirely
/// (`raw_accessor_read_bytes`/`raw_accessor_write_bytes`, `vm/src/jit/helpers.rs`)
/// and differ only in which `JitInvokeInfo` a decline names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopedMemoryOp {
    GetByteInternal,
    PutByteInternal,
    GetIntUnaligned,
    GetShortUnaligned,
    GetLongUnaligned,
    GetCharUnaligned,
    PutIntUnaligned,
    PutShortUnaligned,
    PutLongUnaligned,
    PutCharUnaligned,
    /// `getIntUnalignedInternal` -- see the enum's own doc comment.
    GetIntUnalignedInternal,
    GetShortUnalignedInternal,
    GetLongUnalignedInternal,
    GetCharUnalignedInternal,
    PutIntUnalignedInternal,
    PutShortUnalignedInternal,
    PutLongUnalignedInternal,
    PutCharUnalignedInternal,
}

impl ScopedMemoryOp {
    /// Number of parameters excluding receiver.
    pub const fn num_params(self) -> usize {
        match self {
            Self::GetIntUnaligned
            | Self::GetShortUnaligned
            | Self::GetLongUnaligned
            | Self::GetCharUnaligned
            | Self::GetIntUnalignedInternal
            | Self::GetShortUnalignedInternal
            | Self::GetLongUnalignedInternal
            | Self::GetCharUnalignedInternal => 4,
            Self::GetByteInternal => 3,
            Self::PutByteInternal => 4,
            Self::PutIntUnaligned
            | Self::PutShortUnaligned
            | Self::PutLongUnaligned
            | Self::PutCharUnaligned
            | Self::PutIntUnalignedInternal
            | Self::PutShortUnalignedInternal
            | Self::PutLongUnalignedInternal
            | Self::PutCharUnalignedInternal => 5,
        }
    }

    /// Return type byte.
    pub const fn return_type(self) -> u8 {
        match self {
            Self::GetByteInternal => b'B',
            Self::PutByteInternal => b'V',
            Self::GetIntUnaligned | Self::GetIntUnalignedInternal => b'I',
            Self::GetShortUnaligned | Self::GetShortUnalignedInternal => b'S',
            Self::GetLongUnaligned | Self::GetLongUnalignedInternal => b'J',
            Self::GetCharUnaligned | Self::GetCharUnalignedInternal => b'C',
            Self::PutIntUnaligned
            | Self::PutShortUnaligned
            | Self::PutLongUnaligned
            | Self::PutCharUnaligned
            | Self::PutIntUnalignedInternal
            | Self::PutShortUnalignedInternal
            | Self::PutLongUnalignedInternal
            | Self::PutCharUnalignedInternal => b'V',
        }
    }

    /// Helper address from the table.
    pub const fn helper_entry(self, table: &DirectHelperTable) -> usize {
        match self {
            Self::GetByteInternal => table.scoped_memory_get_byte_internal,
            Self::PutByteInternal => table.scoped_memory_put_byte_internal,
            Self::GetIntUnaligned => table.scoped_memory_get_int,
            Self::GetShortUnaligned => table.scoped_memory_get_short,
            Self::GetLongUnaligned => table.scoped_memory_get_long,
            Self::GetCharUnaligned => table.scoped_memory_get_char,
            Self::PutIntUnaligned => table.scoped_memory_put_int,
            Self::PutShortUnaligned => table.scoped_memory_put_short,
            Self::PutLongUnaligned => table.scoped_memory_put_long,
            Self::PutCharUnaligned => table.scoped_memory_put_char,
            Self::GetIntUnalignedInternal => table.scoped_memory_get_int_internal,
            Self::GetShortUnalignedInternal => table.scoped_memory_get_short_internal,
            Self::GetLongUnalignedInternal => table.scoped_memory_get_long_internal,
            Self::GetCharUnalignedInternal => table.scoped_memory_get_char_internal,
            Self::PutIntUnalignedInternal => table.scoped_memory_put_int_internal,
            Self::PutShortUnalignedInternal => table.scoped_memory_put_short_internal,
            Self::PutLongUnalignedInternal => table.scoped_memory_put_long_internal,
            Self::PutCharUnalignedInternal => table.scoped_memory_put_char_internal,
        }
    }
}

/// Recognise a `ScopedMemoryAccess.*Unaligned` call site by class, method name and descriptor.
pub fn is_scoped_memory_op(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<ScopedMemoryOp> {
    if class_name != "jdk/internal/misc/ScopedMemoryAccess" {
        return None;
    }
    match (method_name, descriptor) {
        ("getByteInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)B") => {
            Some(ScopedMemoryOp::GetByteInternal)
        }
        ("putByteInternal", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)V") => {
            Some(ScopedMemoryOp::PutByteInternal)
        }
        ("getIntUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)I") => {
            Some(ScopedMemoryOp::GetIntUnaligned)
        }
        (
            "getIntUnalignedInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)I",
        ) => Some(ScopedMemoryOp::GetIntUnalignedInternal),
        (
            "getShortUnaligned",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)S",
        ) => Some(ScopedMemoryOp::GetShortUnaligned),
        (
            "getShortUnalignedInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)S",
        ) => Some(ScopedMemoryOp::GetShortUnalignedInternal),
        ("getLongUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)J") => {
            Some(ScopedMemoryOp::GetLongUnaligned)
        }
        (
            "getLongUnalignedInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)J",
        ) => Some(ScopedMemoryOp::GetLongUnalignedInternal),
        ("getCharUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)C") => {
            Some(ScopedMemoryOp::GetCharUnaligned)
        }
        (
            "getCharUnalignedInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)C",
        ) => Some(ScopedMemoryOp::GetCharUnalignedInternal),
        ("putIntUnaligned", "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JIZ)V") => {
            Some(ScopedMemoryOp::PutIntUnaligned)
        }
        (
            "putIntUnalignedInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JIZ)V",
        ) => Some(ScopedMemoryOp::PutIntUnalignedInternal),
        (
            "putShortUnaligned",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSZ)V",
        ) => Some(ScopedMemoryOp::PutShortUnaligned),
        (
            "putShortUnalignedInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSZ)V",
        ) => Some(ScopedMemoryOp::PutShortUnalignedInternal),
        (
            "putLongUnaligned",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJZ)V",
        ) => Some(ScopedMemoryOp::PutLongUnaligned),
        (
            "putLongUnalignedInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJZ)V",
        ) => Some(ScopedMemoryOp::PutLongUnalignedInternal),
        (
            "putCharUnaligned",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCZ)V",
        ) => Some(ScopedMemoryOp::PutCharUnaligned),
        (
            "putCharUnalignedInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCZ)V",
        ) => Some(ScopedMemoryOp::PutCharUnalignedInternal),
        _ => None,
    }
}

/// A classified plain (aligned, `ACC_NATIVE`, no `bigEndian` argument)
/// `jdk/internal/misc/Unsafe` scalar accessor, or one of the two
/// compare-and-set forms `java.util.concurrent` is built on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsafeAccessorOp {
    GetByte,
    GetShort,
    GetInt,
    GetLong,
    /// `getReference(Object,long)Object` (SBR2 W8): the reference-array read.
    GetReference,
    /// `getReferenceVolatile(Object,long)Object` -- the same read; a separate op
    /// because the helper's decline edge dispatches to the method the site names.
    GetReferenceVolatile,
    PutByte,
    PutShort,
    PutInt,
    PutLong,
    /// `compareAndSetInt(Object,long,int,int)Z` -- AQS `compareAndSetState`,
    /// so every `CountDownLatch.countDown`, `ReentrantLock.lock` and
    /// `Semaphore.release` from compiled code.
    ///
    /// Measured before this existed (2026-09-24, Linux host): 10 M
    /// `CountDownLatch.countDown()` took 3.1 s against HotSpot's 60 ms, with
    /// `perf` putting ~55% of it in the generic native funnel around a CAS
    /// that is one `LOCK CMPXCHG` -- while `AtomicInteger.compareAndSet`,
    /// which has [`crate::JitIntrinsic::AtomicIntCompareAndSet`], ran at
    /// HotSpot speed.
    ///
    /// The helper serves only an instance field whose slot the VM's own CAS
    /// would take on the hardware path, and declines everything else (null
    /// base, array, synthetic offset, a legacy slot with the wrong tag) to the
    /// registered native. See `vm/src/jit/helpers.rs`'s `unsafe_cas_serve`.
    CasInt,
    /// `compareAndSetLong(Object,long,long,long)Z` -- the 64-bit twin.
    CasLong,
    /// nio21 wave 3, lane L4 (step 1 of
    /// `r21-jdk-only-volatile-unsafe-refersto0-and-carrier-thread-are-generic-native-calls-FIXED-20261012.md`):
    /// `getIntVolatile(Object,long)I` / `getLongVolatile(Object,long)J` -- the
    /// read half of `Unsafe.getAndAdd{Int,Long}`'s loop, so of every
    /// `AtomicLong.incrementAndGet` under `--jdk-only`. Serves an instance
    /// field through `NativeHeapAccess::get_field_volatile`, the call the
    /// registered native makes; everything else declines to it.
    GetIntVolatile,
    GetLongVolatile,
    /// `putIntVolatile(Object,long,int)V` / `putLongVolatile(Object,long,long)V`:
    /// an instance field through `set_field_volatile` (the native's call; a
    /// SeqCst store, i.e. `XCHG` or store + `MFENCE` on x86-64).
    PutIntVolatile,
    PutLongVolatile,
    /// `putReferenceVolatile(Object,long,Object)V` (step 2): an instance field
    /// through `set_field_volatile` or a reference-array element through
    /// `set_array_element`, the natives' own calls -- so the SATB pre-barrier
    /// (`publish_overwritten_*` on the Generational heap, inside the accessor
    /// on G1/ZGC) and the card barrier are exactly the native's.
    PutReferenceVolatile,
    /// `compareAndSetReference(Object,long,Object,Object)Z` (step 2): through
    /// `compare_and_swap_field_shared`, the function the native's
    /// `compare_and_swap_field` is; SATB on `expected` before the
    /// `LOCK CMPXCHG`, the card on success.
    CasReference,
    /// `fullFence()V`: one `MFENCE`.
    FullFence,
    /// `java/lang/ref/Reference.refersTo0(Object)Z` (step 4) -- NOT an
    /// `Unsafe` method, but the same receiver-plus-arguments thin-bind shape,
    /// so it rides this enumeration and its three doors. The referent slot
    /// is read as the registered native reads it (no keep-alive barrier).
    RefersTo0,
    /// `java/lang/ref/PhantomReference.refersTo0(Object)Z`: the JDK 25
    /// redeclaration; the same native, its own triple for the decline edge.
    PhantomRefersTo0,
}

impl UnsafeAccessorOp {
    /// Number of parameters excluding receiver: `(obj, offset)` for a get,
    /// `(obj, offset, value)` for a put, `(obj, offset, expected, new)` for a
    /// compare-and-set, nothing for `fullFence`, `(other)` for `refersTo0`.
    pub const fn num_params(self) -> usize {
        match self {
            Self::GetByte
            | Self::GetShort
            | Self::GetInt
            | Self::GetLong
            | Self::GetReference
            | Self::GetReferenceVolatile
            | Self::GetIntVolatile
            | Self::GetLongVolatile => 2,
            Self::PutByte
            | Self::PutShort
            | Self::PutInt
            | Self::PutLong
            | Self::PutIntVolatile
            | Self::PutLongVolatile
            | Self::PutReferenceVolatile => 3,
            Self::CasInt | Self::CasLong | Self::CasReference => 4,
            Self::FullFence => 0,
            Self::RefersTo0 | Self::PhantomRefersTo0 => 1,
        }
    }

    /// Return type byte.
    pub const fn return_type(self) -> u8 {
        match self {
            Self::GetByte => b'B',
            Self::GetShort => b'S',
            Self::GetInt | Self::GetIntVolatile => b'I',
            Self::GetLong | Self::GetLongVolatile => b'J',
            Self::GetReference | Self::GetReferenceVolatile => b'L',
            Self::PutByte
            | Self::PutShort
            | Self::PutInt
            | Self::PutLong
            | Self::PutIntVolatile
            | Self::PutLongVolatile
            | Self::PutReferenceVolatile
            | Self::FullFence => b'V',
            Self::CasInt
            | Self::CasLong
            | Self::CasReference
            | Self::RefersTo0
            | Self::PhantomRefersTo0 => b'Z',
        }
    }

    /// The compare-and-set forms, which are counted apart from the
    /// accessors: the `UNSAFE_CAS_*` census rather than the accessors' own.
    pub const fn is_cas(self) -> bool {
        matches!(self, Self::CasInt | Self::CasLong | Self::CasReference)
    }

    /// The nio21 wave-3 rows (lane L4), each behind its own step switch.
    /// Test-only since JIT round 22 wave 7 (lane review): no gate asks it.
    #[cfg(test)]
    pub const fn is_nio21_row(self) -> bool {
        matches!(
            self,
            Self::GetIntVolatile
                | Self::GetLongVolatile
                | Self::PutIntVolatile
                | Self::PutLongVolatile
                | Self::PutReferenceVolatile
                | Self::CasReference
                | Self::FullFence
                | Self::RefersTo0
                | Self::PhantomRefersTo0
        )
    }

    /// `Reference.refersTo0` / `PhantomReference.refersTo0`: private methods,
    /// so a site may arrive as `invokespecial` at the single-pass door too.
    pub const fn is_refers_to(self) -> bool {
        matches!(self, Self::RefersTo0 | Self::PhantomRefersTo0)
    }

    /// Whether this op's own switch is on. The doors ask this rather than
    /// `unsafe_accessor_direct_enabled` directly, so switching the accessors
    /// off does not switch off the CAS and vice versa.
    ///
    /// The nio21 rows read their switch per call of this function, which is
    /// once per recognised site per compile (the jit crate's statics are
    /// frozen by `process_global_statics_ratchet`, so no `OnceLock`).
    pub fn direct_enabled(self) -> bool {
        let flag = |key: &str| cratonvm_types::flags::runtime_flag_default_on(key);
        match self {
            Self::GetIntVolatile
            | Self::GetLongVolatile
            | Self::PutIntVolatile
            | Self::PutLongVolatile => {
                crate::unsafe_accessor_direct_enabled()
                    && flag("CRATONVM_JIT_UNSAFE_VOLATILE_DIRECT")
            }
            Self::PutReferenceVolatile => {
                crate::unsafe_accessor_direct_enabled()
                    && crate::unsafe_reference_direct_enabled()
                    && flag("CRATONVM_JIT_UNSAFE_REF_STORE_DIRECT")
            }
            Self::CasReference => {
                crate::unsafe_cas_direct_enabled() && flag("CRATONVM_JIT_UNSAFE_REF_STORE_DIRECT")
            }
            Self::FullFence => flag("CRATONVM_JIT_UNSAFE_FENCE_DIRECT"),
            Self::RefersTo0 | Self::PhantomRefersTo0 => flag("CRATONVM_JIT_REFERS_TO_DIRECT"),
            Self::CasInt | Self::CasLong => crate::unsafe_cas_direct_enabled(),
            Self::GetReference | Self::GetReferenceVolatile => {
                crate::unsafe_accessor_direct_enabled() && crate::unsafe_reference_direct_enabled()
            }
            Self::GetByte
            | Self::GetShort
            | Self::GetInt
            | Self::GetLong
            | Self::PutByte
            | Self::PutShort
            | Self::PutInt
            | Self::PutLong => crate::unsafe_accessor_direct_enabled(),
        }
    }

    /// Count one bind of this op at a compile door. `door` is `0` single-pass,
    /// `1` optimizing, `2` OSR -- the three per-family site counters.
    pub fn note_site(self, door: u8) {
        use std::sync::atomic::Ordering::Relaxed;
        let counter = match (self.is_cas(), door) {
            (false, 0) => &crate::UNSAFE_ACCESSOR_SITES_SP,
            (false, 1) => &crate::UNSAFE_ACCESSOR_SITES_IR,
            (false, _) => &crate::UNSAFE_ACCESSOR_SITES_OSR,
            (true, 0) => &crate::UNSAFE_CAS_SITES_SP,
            (true, 1) => &crate::UNSAFE_CAS_SITES_IR,
            (true, _) => &crate::UNSAFE_CAS_SITES_OSR,
        };
        counter.fetch_add(1, Relaxed);
    }

    /// Helper address from the table.
    pub const fn helper_entry(self, table: &DirectHelperTable) -> usize {
        match self {
            Self::GetByte => table.unsafe_get_byte,
            Self::GetShort => table.unsafe_get_short,
            Self::GetInt => table.unsafe_get_int,
            Self::GetLong => table.unsafe_get_long,
            Self::GetReference => table.unsafe_get_reference,
            Self::GetReferenceVolatile => table.unsafe_get_reference_volatile,
            Self::PutByte => table.unsafe_put_byte,
            Self::PutShort => table.unsafe_put_short,
            Self::PutInt => table.unsafe_put_int,
            Self::PutLong => table.unsafe_put_long,
            Self::CasInt => table.unsafe_cas_int,
            Self::CasLong => table.unsafe_cas_long,
            Self::GetIntVolatile => table.unsafe_get_int_volatile,
            Self::GetLongVolatile => table.unsafe_get_long_volatile,
            Self::PutIntVolatile => table.unsafe_put_int_volatile,
            Self::PutLongVolatile => table.unsafe_put_long_volatile,
            Self::PutReferenceVolatile => table.unsafe_put_reference_volatile,
            Self::CasReference => table.unsafe_cas_reference,
            Self::FullFence => table.unsafe_full_fence,
            Self::RefersTo0 => table.reference_refers_to0,
            Self::PhantomRefersTo0 => table.phantom_reference_refers_to0,
        }
    }
}

/// Recognise a plain `jdk/internal/misc/Unsafe` scalar accessor call site by
/// class, method name and descriptor. Only the aligned, no-`bigEndian`,
/// `(Object,long[,V])` shapes -- the `*Unaligned` family is
/// [`is_scoped_memory_op`]'s sibling on `ScopedMemoryAccess` -- plus the two
/// `compareAndSet{Int,Long}` forms. The `(J)X` off-heap-only overloads and
/// the weak/exchange/opaque/acquire/release family are out of scope.
///
/// nio21 wave 3, lane L4: plus the `ACC_NATIVE` volatile int/long accessors,
/// `putReferenceVolatile`, `compareAndSetReference` (whose helper reaches the
/// VM's own barriered CAS, so the SATB / card barriers are modelled by the
/// callee rather than by this bind), `fullFence`, and -- outside `Unsafe` --
/// the two `refersTo0` triples. The `Acquire`/`Release`/`Opaque` forms are
/// JDK Java bodies that call these, so they compile and reach these binds;
/// they are not shadowed (real class bytes are authoritative).
pub fn is_unsafe_accessor_op(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<UnsafeAccessorOp> {
    if method_name == "refersTo0" && descriptor == "(Ljava/lang/Object;)Z" {
        return match class_name {
            "java/lang/ref/Reference" => Some(UnsafeAccessorOp::RefersTo0),
            "java/lang/ref/PhantomReference" => Some(UnsafeAccessorOp::PhantomRefersTo0),
            _ => None,
        };
    }
    if class_name != "jdk/internal/misc/Unsafe" {
        return None;
    }
    match (method_name, descriptor) {
        ("getIntVolatile", "(Ljava/lang/Object;J)I") => Some(UnsafeAccessorOp::GetIntVolatile),
        ("getLongVolatile", "(Ljava/lang/Object;J)J") => Some(UnsafeAccessorOp::GetLongVolatile),
        ("putIntVolatile", "(Ljava/lang/Object;JI)V") => Some(UnsafeAccessorOp::PutIntVolatile),
        ("putLongVolatile", "(Ljava/lang/Object;JJ)V") => Some(UnsafeAccessorOp::PutLongVolatile),
        ("putReferenceVolatile", "(Ljava/lang/Object;JLjava/lang/Object;)V") => {
            Some(UnsafeAccessorOp::PutReferenceVolatile)
        }
        (
            "compareAndSetReference",
            "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
        ) => Some(UnsafeAccessorOp::CasReference),
        ("fullFence", "()V") => Some(UnsafeAccessorOp::FullFence),
        ("getByte", "(Ljava/lang/Object;J)B") => Some(UnsafeAccessorOp::GetByte),
        ("getShort", "(Ljava/lang/Object;J)S") => Some(UnsafeAccessorOp::GetShort),
        ("getInt", "(Ljava/lang/Object;J)I") => Some(UnsafeAccessorOp::GetInt),
        ("getLong", "(Ljava/lang/Object;J)J") => Some(UnsafeAccessorOp::GetLong),
        ("getReference", "(Ljava/lang/Object;J)Ljava/lang/Object;") => {
            Some(UnsafeAccessorOp::GetReference)
        }
        ("getReferenceVolatile", "(Ljava/lang/Object;J)Ljava/lang/Object;") => {
            Some(UnsafeAccessorOp::GetReferenceVolatile)
        }
        ("putByte", "(Ljava/lang/Object;JB)V") => Some(UnsafeAccessorOp::PutByte),
        ("putShort", "(Ljava/lang/Object;JS)V") => Some(UnsafeAccessorOp::PutShort),
        ("putInt", "(Ljava/lang/Object;JI)V") => Some(UnsafeAccessorOp::PutInt),
        ("putLong", "(Ljava/lang/Object;JJ)V") => Some(UnsafeAccessorOp::PutLong),
        ("compareAndSetInt", "(Ljava/lang/Object;JII)Z") => Some(UnsafeAccessorOp::CasInt),
        ("compareAndSetLong", "(Ljava/lang/Object;JJJ)Z") => Some(UnsafeAccessorOp::CasLong),
        _ => None,
    }
}

/// `CRATONVM_JIT_CARRIER_THREAD_DIRECT=0` -- keep every
/// `Thread.currentCarrierThread()` site on the generic native call (nio21
/// wave 3, lane L4, step 5). Default ON. Read per recognised site at compile
/// time (no cache: this crate's statics are frozen).
pub fn carrier_thread_direct_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_CARRIER_THREAD_DIRECT")
}

// ── NIO19-2: the `Unsafe` primitive-array accessors as code ──────────────
//
// Round nio20, wave 1, lane L2. The thin helpers above still cost a CALL, the
// `contain` frame, two policy gates and `is_object_address` per access; HotSpot
// emits one load. The optimizing tier now answers a heap PRIMITIVE-array access
// inline, as a guarded prefix of the call's unchanged lowering
// (`ir_lower::Lowerer::try_emit_ir_unsafe_access_prefix`), and every other
// shape -- a null base (off-heap / arena), a plain object (field offsets,
// static bases), a reference array, an offset outside the array's data --
// takes the call exactly as before.
//
// What the prefix accepts is EXACTLY `vm::jit::helpers::raw_accessor_read_bytes`
// / `raw_accessor_write_bytes`'s heap arm: any primitive element type (a
// mismatched width -- `getInt` over a `long[]`, `getLong` over a `byte[]` -- is
// a raw little-endian copy, as on HotSpot), `offset >= ARRAY_DATA_OFFSET`, and
// `offset - ARRAY_DATA_OFFSET + width <= length * element_size`; no alignment
// is required. The `*Unaligned` forms are the JDK's Java bodies, whose every
// heap outcome is that same raw read (and the `bigEndian` forms a byte swap of
// it), so they take the same prefix.

/// `CRATONVM_JIT_UNSAFE_ACCESSOR_INLINE=0` -- keep every `Unsafe` primitive
/// accessor site on its call (the thin helper or the funnel), i.e. the
/// pre-NIO19-2 code. Default ON. Off as well whenever
/// `CRATONVM_JIT_UNSAFE_ACCESSOR_DIRECT=0`, which is the family's "send every
/// accessor to the native" lever. Read per site at lowering, so no cache.
pub fn unsafe_accessor_inline_enabled() -> bool {
    crate::unsafe_accessor_direct_enabled()
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_UNSAFE_ACCESSOR_INLINE")
}

/// `CRATONVM_JIT_UNSAFE_VOLATILE_INLINE=0` (nio21 wave 4, lane L4) -- keep the
/// `{get,put}{Int,Long}Volatile` sites on their call even where the plain
/// accessors are answered inline. Default ON. Below the family switch
/// [`unsafe_accessor_inline_enabled`], which every prefix also asks. Read per
/// site at lowering (inside [`unsafe_inline_access_site`]), so no cache.
pub fn unsafe_volatile_inline_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_UNSAFE_VOLATILE_INLINE")
}

// No site counter of its own: `jit/tests/process_global_statics_ratchet.rs`
// freezes this crate's statics. `CRATONVM_DBG_JITC=1` names every emitted
// prefix instead. An inline hit runs no helper, so the thin helpers'
// `UNSAFE_ACCESSOR_SERVED` falls when this is on -- the expected reading.

/// One `Unsafe` primitive accessor the optimizing tier can answer inline. See
/// the section comment above for exactly what it serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnsafeInlineAccess {
    /// Access width in bytes: 1, 2, 4 or 8.
    pub width: u8,
    /// A `put*` (the site returns `void` and has a value operand).
    pub is_put: bool,
    /// A `get*` of `byte`/`short`/`int` sign-extends its result; `char`
    /// zero-extends. For a put it only tells `putShort` (`true`) from
    /// `putChar` (`false`) for the gate bit; the stored bytes are the same.
    pub signed: bool,
    /// The `*Unaligned(.., boolean bigEndian)` forms: the trailing operand
    /// selects a byte swap.
    pub big_endian_operand: bool,
    /// One of the `*Unaligned` forms, whose JDK body may call any narrower
    /// accessor of its direction, so its admission needs all of them.
    pub unaligned: bool,
    /// nio21 wave 4 L4: `{get,put}{Int,Long}Volatile`. Served only for a
    /// NATURALLY ALIGNED offset (the prefixes test `offset & (width - 1)`); a
    /// get is the same plain `MOV` (a volatile load on x86-TSO), a put is one
    /// `XCHG` (an implicitly locked store: the StoreLoad a volatile store
    /// owes). Never the single-pass off-heap arm.
    pub volatile: bool,
    /// JIT round 20 wave 12, lane qUnsafe (L2-2): the Java value kind the
    /// site moves. `b'I'` for every integral row (all rows before this
    /// wave); `b'Z'` for `getBoolean` / `putBoolean` (a get normalises the
    /// byte to `0`/`1`, as `native_unsafe_get_boolean_mb` and HotSpot do);
    /// `b'F'` / `b'D'` for `getFloat` / `putFloat` / `getDouble` /
    /// `putDouble`, whose value travels as its raw IEEE bits in a GPR (a
    /// `float` get ZERO-extends its 4 bytes, the bits a call's `F` return
    /// leaves in RAX). The non-`I` kinds are heap primitive-array rows only:
    /// no off-heap arm, no field arm.
    pub value_kind: u8,
    /// JIT round 20 wave 12, lane qUnsafe (L2-2): a `put{Int,Long}Release` /
    /// `put{Int,Long}Opaque` row (a `volatile` row): the store is a plain
    /// `MOV` (a release store on x86-TSO; no StoreLoad is owed), not the
    /// `XCHG` a `put*Volatile` emits. Always `false` for a get.
    pub ordered_store: bool,
}

/// JIT round 20 wave 12, lane qUnsafe (L2-2 in
/// `docs/feature-designs/jit-ir-optimizations.md`):
/// `CRATONVM_JIT_UNSAFE_QU_EXTRA_ROWS=0` keeps the rows this item added on
/// their calls -- `getBoolean` / `putBoolean` / `getFloat` / `putFloat` /
/// `getDouble` / `putDouble` (heap primitive-array elements) and the
/// `{get,put}{Int,Long}{Acquire,Opaque,Release}` Java bodies (served as the
/// volatile rows they call, a release / opaque store as a plain `MOV`) --
/// i.e. [`unsafe_inline_access_site`] recognises exactly what it did before.
/// Default ON. Read per site at lowering, so no cache.
pub fn unsafe_qu_extra_rows_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_UNSAFE_QU_EXTRA_ROWS")
}

/// JIT round 20 wave 12, lane qUnsafe (L2-2): the admission rows of the
/// `boolean` / `float` / `double` accessor natives, bits
/// [`UNSAFE_QU_EXTRA_FIRST_BIT`].. of [`DirectHelperTable::unsafe_inline_admitted`].
/// Like the `compareAndExchange*` rows (`UnsafeInlineRmw::CAE_GATE_BITS`)
/// these have no thin helper, so the VM asks their policy through per-VM
/// memos (`JitRealm::unsafe_inline.qu_extra_sites`) and ORs the answer in.
pub const UNSAFE_QU_EXTRA_GATE_BITS: [(&str, &str); 6] = [
    ("getBoolean", "(Ljava/lang/Object;J)Z"),
    ("putBoolean", "(Ljava/lang/Object;JZ)V"),
    ("getFloat", "(Ljava/lang/Object;J)F"),
    ("putFloat", "(Ljava/lang/Object;JF)V"),
    ("getDouble", "(Ljava/lang/Object;J)D"),
    ("putDouble", "(Ljava/lang/Object;JD)V"),
];

/// JIT round 20 wave 12, lane qUnsafe (L2-2): the `getBoolean` result
/// normalisation both tiers emit after the byte load (RAX = the byte,
/// sign-extended): `TEST AL, AL ; SETNE AL ; MOVZX EAX, AL`, i.e.
/// `native_unsafe_get_boolean_mb`'s `i32::from(byte != 0)`.
pub const UNSAFE_BOOLEAN_NORMALIZE_RAX: [u8; 8] = [0x84, 0xC0, 0x0F, 0x95, 0xC0, 0x0F, 0xB6, 0xC0];

/// JIT round 20, lane rev12: the `putBoolean` value normalisation both tiers
/// emit before the byte store (EDX = the value): `AND EDX, 1`, HotSpot's
/// `normalize_for_write` (`x & 1`) and `native_unsafe_put_boolean_mb`'s.
pub const UNSAFE_BOOLEAN_NORMALIZE_EDX: [u8; 3] = [0x83, 0xE2, 0x01];

/// The bit of [`UNSAFE_QU_EXTRA_GATE_BITS`]' first row: after the CAE rows
/// (16, 17) and the `compareAndSetReference` row
/// ([`UNSAFE_REF_CAS_GATE_BIT`], 18).
pub const UNSAFE_QU_EXTRA_FIRST_BIT: usize = 19;

impl UnsafeInlineAccess {
    /// The bit order of [`DirectHelperTable::unsafe_inline_admitted`]: the
    /// `(method, descriptor)` of each registered `Unsafe` native whose gate the
    /// VM asks. Gets are bits 0..5, puts 5..10; nio21 wave 4 L4 added the
    /// four volatile rows as bits 10..14 (`getIntVolatile`, `getLongVolatile`,
    /// `putIntVolatile`, `putLongVolatile`); nio21 wave 5 L3 the two CAS rows
    /// as bits 14 and 15 (`compareAndSetInt`, `compareAndSetLong`), which
    /// [`unsafe_inline_rmw_site`]'s rows ask and no accessor row does.
    pub const GATE_BITS: [(&'static str, &'static str); 16] = [
        ("getByte", "(Ljava/lang/Object;J)B"),
        ("getShort", "(Ljava/lang/Object;J)S"),
        ("getChar", "(Ljava/lang/Object;J)C"),
        ("getInt", "(Ljava/lang/Object;J)I"),
        ("getLong", "(Ljava/lang/Object;J)J"),
        ("putByte", "(Ljava/lang/Object;JB)V"),
        ("putShort", "(Ljava/lang/Object;JS)V"),
        ("putChar", "(Ljava/lang/Object;JC)V"),
        ("putInt", "(Ljava/lang/Object;JI)V"),
        ("putLong", "(Ljava/lang/Object;JJ)V"),
        ("getIntVolatile", "(Ljava/lang/Object;J)I"),
        ("getLongVolatile", "(Ljava/lang/Object;J)J"),
        ("putIntVolatile", "(Ljava/lang/Object;JI)V"),
        ("putLongVolatile", "(Ljava/lang/Object;JJ)V"),
        ("compareAndSetInt", "(Ljava/lang/Object;JII)Z"),
        ("compareAndSetLong", "(Ljava/lang/Object;JJJ)Z"),
    ];

    /// The `GATE_BITS` row of a plain (aligned-form) accessor of this shape.
    const fn plain_bit(self) -> usize {
        // JIT round 20 wave 12, lane qUnsafe (L2-2): the boolean / float /
        // double rows own bits `UNSAFE_QU_EXTRA_FIRST_BIT + 0..6`, in
        // `UNSAFE_QU_EXTRA_GATE_BITS` order (get, put per kind).
        let extra_row: usize = match self.value_kind {
            b'Z' => 0,
            b'F' => 2,
            b'D' => 4,
            _ => usize::MAX,
        };
        if extra_row != usize::MAX {
            return 1usize << (UNSAFE_QU_EXTRA_FIRST_BIT + extra_row + self.is_put as usize);
        }
        if self.volatile {
            // Bits 10..14: get int, get long, put int, put long.
            let row = 10 + (self.width == 8) as usize + 2 * (self.is_put as usize);
            return 1usize << row;
        }
        // A put has no sign; `putShort` and `putChar` are told apart by
        // `signed`, which `unsafe_inline_access_site` sets only for `putShort`.
        let row: usize = match (self.width, self.signed) {
            (1, _) => 0,
            (2, true) => 1,
            (2, false) => 2,
            (4, _) => 3,
            _ => 4,
        };
        1usize << (row + if self.is_put { 5 } else { 0 })
    }

    /// Does the VM's compile-time policy word admit this access? A plain
    /// accessor needs its own bit; an `*Unaligned` form needs every accessor of
    /// its direction, because its JDK body reaches `get/put{Byte,Short,Char,
    /// Int,Long}` depending on the offset's alignment.
    pub const fn admitted(self, mask: usize) -> bool {
        let need = if self.unaligned {
            if self.is_put {
                0b11111 << 5
            } else {
                0b11111
            }
        } else {
            self.plain_bit()
        };
        mask & need == need
    }

    /// Java parameters after the receiver: `(obj, offset)`, plus the value for
    /// a put, plus `bigEndian` for the four-operand unaligned forms.
    pub const fn num_params(self) -> usize {
        2 + self.is_put as usize + self.big_endian_operand as usize
    }

    /// JIT round 20 wave 12, lane qUnsafe (L2-2): a `float` / `double` row,
    /// whose value is raw IEEE bits in a GPR.
    pub const fn is_fp(self) -> bool {
        matches!(self.value_kind, b'F' | b'D')
    }

    /// JIT round 20 wave 12, lane qUnsafe (L2-2): one of the rows this item
    /// added that has no off-heap arm (a `boolean` / `float` / `double` row):
    /// a null base declines to the call in both tiers.
    pub const fn heap_only(self) -> bool {
        self.value_kind != b'I'
    }
}

/// Recognise an `Unsafe` primitive accessor site the optimizing tier may answer
/// inline (NIO19-2), by class, name and descriptor. Covers the plain
/// `get/put{Byte,Short,Char,Int,Long}(Object,long[,V])` natives and the
/// `{get,put}{Short,Char,Int,Long}Unaligned(Object,long[,V][,boolean])` Java
/// bodies. Not the `(J)X` off-heap overloads. Since nio21 wave 4 (lane L4) it
/// also recognises `{get,put}{Int,Long}Volatile` (`UnsafeInlineAccess::volatile`),
/// unless `CRATONVM_JIT_UNSAFE_VOLATILE_INLINE=0`. Since JIT round 20 wave 12
/// (lane qUnsafe, L2-2; off under `CRATONVM_JIT_UNSAFE_QU_EXTRA_ROWS=0`) also
/// `getBoolean` / `putBoolean` / `getFloat` / `putFloat` / `getDouble` /
/// `putDouble` (`UnsafeInlineAccess::value_kind`; the value rides a GPR as its
/// raw bits) and the int/long `Acquire` / `Opaque` / `Release` bodies (as the
/// volatile rows they call). Still not the `float` / `double` / `boolean`
/// VOLATILE rows: their registered natives (`native_unsafe_get_int_volatile`
/// & co, not the `_arena` raw forms) read a mismatched-element array
/// element-wise, which a raw inline load would not match.
pub fn unsafe_inline_access_site(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<UnsafeInlineAccess> {
    if class_name != "jdk/internal/misc/Unsafe" {
        return None;
    }
    let get = |width: u8, signed: bool, unaligned: bool, be: bool| UnsafeInlineAccess {
        width,
        is_put: false,
        signed,
        big_endian_operand: be,
        unaligned,
        volatile: false,
        value_kind: b'I',
        ordered_store: false,
    };
    let put = |width: u8, signed: bool, unaligned: bool, be: bool| UnsafeInlineAccess {
        width,
        is_put: true,
        signed,
        big_endian_operand: be,
        unaligned,
        volatile: false,
        value_kind: b'I',
        ordered_store: false,
    };
    // nio21 wave 4 L4: the volatile int/long rows, aligned primitive-array
    // elements only (see `UnsafeInlineAccess::volatile`).
    let volatile = |width: u8, is_put: bool| {
        unsafe_volatile_inline_enabled().then_some(UnsafeInlineAccess {
            width,
            is_put,
            signed: true,
            big_endian_operand: false,
            unaligned: false,
            volatile: true,
            value_kind: b'I',
            ordered_store: false,
        })
    };
    // JIT round 20 wave 12, lane qUnsafe (L2-2): the `Acquire` / `Opaque` /
    // `Release` int/long Java bodies, each `return get{X}Volatile(o, offset)`
    // or `put{X}Volatile(o, offset, x)` in JDK 25, served as that volatile
    // row (its admission bit, its alignment test, its field arm); a release
    // or opaque store is a plain `MOV` (`ordered_store`). And the `boolean`
    // / `float` / `double` natives, heap primitive-array elements only.
    let extra = unsafe_qu_extra_rows_enabled();
    let ordered = |width: u8, is_put: bool| {
        if !extra {
            return None;
        }
        volatile(width, is_put).map(|a| UnsafeInlineAccess {
            ordered_store: is_put,
            ..a
        })
    };
    let kind = |value_kind: u8, width: u8, is_put: bool| {
        extra.then_some(UnsafeInlineAccess {
            width,
            is_put,
            // `getBoolean` loads its byte sign-extended, then normalises.
            signed: value_kind == b'Z',
            big_endian_operand: false,
            unaligned: false,
            volatile: false,
            value_kind,
            ordered_store: false,
        })
    };
    Some(match (method_name, descriptor) {
        ("getIntVolatile", "(Ljava/lang/Object;J)I") => return volatile(4, false),
        ("getLongVolatile", "(Ljava/lang/Object;J)J") => return volatile(8, false),
        ("putIntVolatile", "(Ljava/lang/Object;JI)V") => return volatile(4, true),
        ("putLongVolatile", "(Ljava/lang/Object;JJ)V") => return volatile(8, true),
        ("getIntAcquire" | "getIntOpaque", "(Ljava/lang/Object;J)I") => return ordered(4, false),
        ("getLongAcquire" | "getLongOpaque", "(Ljava/lang/Object;J)J") => return ordered(8, false),
        ("putIntRelease" | "putIntOpaque", "(Ljava/lang/Object;JI)V") => return ordered(4, true),
        ("putLongRelease" | "putLongOpaque", "(Ljava/lang/Object;JJ)V") => return ordered(8, true),
        ("getBoolean", "(Ljava/lang/Object;J)Z") => return kind(b'Z', 1, false),
        ("putBoolean", "(Ljava/lang/Object;JZ)V") => return kind(b'Z', 1, true),
        ("getFloat", "(Ljava/lang/Object;J)F") => return kind(b'F', 4, false),
        ("putFloat", "(Ljava/lang/Object;JF)V") => return kind(b'F', 4, true),
        ("getDouble", "(Ljava/lang/Object;J)D") => return kind(b'D', 8, false),
        ("putDouble", "(Ljava/lang/Object;JD)V") => return kind(b'D', 8, true),
        ("getByte", "(Ljava/lang/Object;J)B") => get(1, true, false, false),
        ("getShort", "(Ljava/lang/Object;J)S") => get(2, true, false, false),
        ("getChar", "(Ljava/lang/Object;J)C") => get(2, false, false, false),
        ("getInt", "(Ljava/lang/Object;J)I") => get(4, true, false, false),
        ("getLong", "(Ljava/lang/Object;J)J") => get(8, true, false, false),
        ("putByte", "(Ljava/lang/Object;JB)V") => put(1, true, false, false),
        ("putShort", "(Ljava/lang/Object;JS)V") => put(2, true, false, false),
        ("putChar", "(Ljava/lang/Object;JC)V") => put(2, false, false, false),
        ("putInt", "(Ljava/lang/Object;JI)V") => put(4, true, false, false),
        ("putLong", "(Ljava/lang/Object;JJ)V") => put(8, true, false, false),
        ("getShortUnaligned", "(Ljava/lang/Object;J)S") => get(2, true, true, false),
        ("getShortUnaligned", "(Ljava/lang/Object;JZ)S") => get(2, true, true, true),
        ("getCharUnaligned", "(Ljava/lang/Object;J)C") => get(2, false, true, false),
        ("getCharUnaligned", "(Ljava/lang/Object;JZ)C") => get(2, false, true, true),
        ("getIntUnaligned", "(Ljava/lang/Object;J)I") => get(4, true, true, false),
        ("getIntUnaligned", "(Ljava/lang/Object;JZ)I") => get(4, true, true, true),
        ("getLongUnaligned", "(Ljava/lang/Object;J)J") => get(8, true, true, false),
        ("getLongUnaligned", "(Ljava/lang/Object;JZ)J") => get(8, true, true, true),
        ("putShortUnaligned", "(Ljava/lang/Object;JS)V") => put(2, true, true, false),
        ("putShortUnaligned", "(Ljava/lang/Object;JSZ)V") => put(2, true, true, true),
        ("putCharUnaligned", "(Ljava/lang/Object;JC)V") => put(2, false, true, false),
        ("putCharUnaligned", "(Ljava/lang/Object;JCZ)V") => put(2, false, true, true),
        ("putIntUnaligned", "(Ljava/lang/Object;JI)V") => put(4, true, true, false),
        ("putIntUnaligned", "(Ljava/lang/Object;JIZ)V") => put(4, true, true, true),
        ("putLongUnaligned", "(Ljava/lang/Object;JJ)V") => put(8, true, true, false),
        ("putLongUnaligned", "(Ljava/lang/Object;JJZ)V") => put(8, true, true, true),
        _ => return None,
    })
}

// ── nio21 wave 5 L3: `Unsafe` CAS / getAndAdd / getAndSet as code ───────
//
// `r21-unsafe-volatile-accessors-are-thin-calls-not-inline-and-getandadd-is-not-one-xadd-FIXED-20261012.md`.
// Since the same wave every CAS on a primitive-array element is one hardware
// atomic in the VM (`vm_exec::hw_cas_primitive_array_element` for an aligned
// element of the access's width, `primitive_array_atomic_update` for an aligned
// raw access over another element type), so a compiled `LOCK CMPXCHG` /
// `LOCK XADD` / `XCHG` on the same aligned payload bytes is in the natives'
// atomicity domain. The prefixes serve exactly that: a non-null base that is a
// one-dimensional PRIMITIVE array, `16 <= offset`, `offset - 16 + w <=
// payload bytes` and `offset` naturally aligned for `w`. Since nio21 wave 7 L3
// the natives test the array arm before the synthetic-offset band
// (`unsafe_natives_ext::unsafe_synthetic_field_offset`), so a band offset of a
// huge array is an element on both sides and the prefix no longer declines it;
// under `CRATONVM_UNSAFE_ARRAY_BEFORE_SYNTHETIC=0` it does, as before.
//
// nio21 wave 7 L3 (W6-L3-1) adds the INSTANCE-FIELD arm: a non-array base whose
// `(class id, offset, width)` this VM's hardware field CAS has published in the
// per-VM cell table ([`DirectHelperTable::unsafe_field_cells`]) with a compact
// displacement, on a COMPACT instance. The VM publishes a cell only from
// `vm_exec::hw_atomic_addr`'s compact arm, i.e. only where the natives
// themselves do one hardware atomic on `base + disp`, so the compiled
// `LOCK CMPXCHG` / `LOCK XADD` / `XCHG` on the same bytes is in their domain.
// Everything else -- a null base, a reference array, a misaligned offset (the
// natives' raw arm), a legacy instance, an unpublished cell -- keeps the call.

/// `CRATONVM_JIT_UNSAFE_RMW_INLINE=0` (nio21 wave 5, lane L3) -- keep every
/// `Unsafe` CAS / getAndAdd / getAndSet site on its call even where the
/// accessors are answered inline. Default ON. Below the family switch
/// [`unsafe_accessor_inline_enabled`], which every prefix also asks; the VM
/// clears the CAS admission bits under `CRATONVM_UNSAFE_ARRAY_HW_CAS=0`. Read
/// per site at lowering (inside [`unsafe_inline_rmw_site`]), so no cache.
pub fn unsafe_rmw_inline_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_UNSAFE_RMW_INLINE")
}

/// The read-modify-write an [`UnsafeInlineRmw`] site performs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsafeRmwOp {
    /// `compareAndSet{Int,Long}` and the `weakCompareAndSet{Int,Long}{,Plain,
    /// Acquire,Release}` bodies: `LOCK CMPXCHG`, the result `0`/`1`.
    Cas,
    /// `getAndAdd{Int,Long}`: `LOCK XADD`, the result the previous value.
    GetAndAdd,
    /// `getAndSet{Int,Long}`: `XCHG` (implicitly locked), the previous value.
    GetAndSet,
    /// nio21 wave 6 L3 (W5-L3-2): `getAndBitwise{Or,And,Xor}{Int,Long}{,Acquire,
    /// Release}`: a `LOCK CMPXCHG` loop applying the operation, the previous
    /// value.
    GetAndBitwise(UnsafeBitOp),
    /// nio21 wave 7 L3 (W6-L3-2): `compareAndExchange{Int,Long}{,Acquire,
    /// Release}`: `LOCK CMPXCHG` with the WITNESS kept (the value the cell
    /// held, which is `expected` exactly when the exchange happened), an `int`
    /// sign-extended.
    Cae,
}

/// The bit operation of an [`UnsafeRmwOp::GetAndBitwise`] site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsafeBitOp {
    Or,
    And,
    Xor,
}

/// `CRATONVM_JIT_UNSAFE_BITWISE_INLINE=0` (nio21 wave 6, lane L3) -- keep the
/// `Unsafe.getAndBitwise*` sites on their call (the JDK's CAS-loop Java body)
/// even where the other RMW rows are answered inline. Default ON. Below
/// [`unsafe_rmw_inline_enabled`]. Read per site at lowering (inside
/// [`unsafe_inline_rmw_site`]), so no cache.
pub fn unsafe_bitwise_inline_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_UNSAFE_BITWISE_INLINE")
}

/// `CRATONVM_JIT_UNSAFE_CAE_INLINE=0` (nio21 wave 7, lane L3) -- keep the
/// `Unsafe.compareAndExchange{Int,Long}*` sites on their call even where the
/// other RMW rows are answered inline. Default ON. Below
/// [`unsafe_rmw_inline_enabled`]; the VM must also set the rows' own admission
/// bits (16, 17). Read per site at lowering, so no cache.
pub fn unsafe_cae_inline_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_UNSAFE_CAE_INLINE")
}

/// `CRATONVM_JIT_UNSAFE_FIELD_RMW_INLINE=0` (nio21 wave 7, lane L3, W6-L3-1)
/// -- no instance-field arm in the `Unsafe` RMW prefixes: a non-array base
/// keeps its call, as before this wave. Default ON. The arm also needs the
/// VM's cell table ([`DirectHelperTable::unsafe_field_cells`] non-zero). Read
/// per site at lowering, so no cache.
pub fn unsafe_field_rmw_inline_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_UNSAFE_FIELD_RMW_INLINE")
}

/// `CRATONVM_UNSAFE_ARRAY_BEFORE_SYNTHETIC` -- the natives' arm-order switch
/// (`unsafe_natives_ext::unsafe_array_before_synthetic_enabled`, the same
/// name). While it is OFF the natives send a synthetic-band offset of an
/// array to their side store, so [`unsafe_inline_rmw_screen_bytes`] keeps
/// declining that band. Read per site at lowering, so no cache.
pub fn unsafe_array_before_synthetic_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_UNSAFE_ARRAY_BEFORE_SYNTHETIC")
}

/// Entries in the per-VM `Unsafe` instance-field cell table
/// ([`DirectHelperTable::unsafe_field_cells`]), after its epoch word. A power
/// of two: the prefix indexes with `AND`.
pub const UNSAFE_FIELD_CELL_COUNT: usize = 256;

/// The largest `Unsafe` field offset (a slot index) a cell can name: the key
/// is `offset << 1 | wide` in 16 bits.
pub const UNSAFE_FIELD_CELL_MAX_SLOT: usize = 0x7FFF;

/// One published cell: `class_id << 32 | key << 16 | disp`, where `key =
/// slot << 1 | (width == 8)` and `disp` is the field's compact displacement
/// from the object base (non-zero: a compact body starts after the header).
/// `None` when the shape does not fit the word (a slot past
/// [`UNSAFE_FIELD_CELL_MAX_SLOT`], a displacement of 0 or past `u16`), in which
/// case nothing is published and the site keeps its call. One `u64` so a
/// reader can never see half of one cell and half of another.
pub const fn unsafe_field_cell_word(
    class_id: u32,
    slot: usize,
    wide: bool,
    disp: usize,
) -> Option<u64> {
    if slot > UNSAFE_FIELD_CELL_MAX_SLOT || disp == 0 || disp > 0xFFFF {
        return None;
    }
    let key = ((slot as u64) << 1) | wide as u64; // Cast: slot <= 0x7FFF
    Some(((class_id as u64) << 32) | (key << 16) | disp as u64) // Cast: disp <= 0xFFFF
}

/// The entry index (0-based, after the epoch word) of the cell for
/// `(class_id, slot, wide)`: `(class_id ^ key) & (COUNT - 1)`, exactly what
/// [`unsafe_inline_field_cell_bytes`] computes.
pub const fn unsafe_field_cell_index(class_id: u32, slot: usize, wide: bool) -> usize {
    let key = ((slot as u32) << 1) | wide as u32; // Cast: callers pass slot <= 0x7FFF
    ((class_id ^ key) as usize) & (UNSAFE_FIELD_CELL_COUNT - 1)
}

/// JIT round 23 wave 9 (lane W9-L2, CF-W9-1;
/// [`unsafe_field_cell_two_way_enabled`]): the multiplier of the SECOND cell
/// index, a 32-bit Fibonacci constant, so two keys that share their first
/// index (`class_id ^ key`) almost never share their second.
pub const UNSAFE_FIELD_CELL_HASH2: u32 = 0x9E37_79B1;

/// CF-W9-1: the second-probe entry index of the cell for `(class_id, slot,
/// wide)`: `((class_id * HASH2) >> 24 ^ key) & (COUNT - 1)`, exactly what the
/// two-way screen ([`unsafe_inline_field_cell_bytes`]) computes after a miss
/// at [`unsafe_field_cell_index`].
pub const fn unsafe_field_cell_index2(class_id: u32, slot: usize, wide: bool) -> usize {
    let key = ((slot as u32) << 1) | wide as u32; // Cast: callers pass slot <= 0x7FFF
    (((class_id.wrapping_mul(UNSAFE_FIELD_CELL_HASH2) >> 24) ^ key) as usize)
        & (UNSAFE_FIELD_CELL_COUNT - 1)
}

/// `CRATONVM_JIT_UNSAFE_FIELD_CELL_TWO_WAY` (JIT round 23 wave 9, lane W9-L2,
/// CF-W9-1; perf, default ON, `=0` restores the one-way table): the `Unsafe`
/// field-cell table is two-way. A key whose first cell
/// ([`unsafe_field_cell_index`]) another field owns is published at its second
/// cell ([`unsafe_field_cell_index2`]), and the compiled screen probes both.
///
/// Why: the table was direct-mapped with first-owner-wins, so a hot field
/// whose first cell a colder field had claimed kept its thin call for the
/// VM's life. `R21CfChain` thenapply-pending: `CompletableFuture$UniApply`
/// (cid 667) `ForkJoinTask.status` (slot 0) shares cell 155 with
/// `ConcurrentHashMap.sizeCtl` (cid 145, slot 5). When a map resize CASed
/// `sizeCtl` first (every run order except "pending first"), every
/// `UniCompletion.claim` CAS went through `jit_unsafe_cas_int_direct`
/// (`CRATONVM_DBG=jit-method-stats`: served=1 499 704 = one per op), at
/// ~150 ns per op. Read per compile (screen bytes) and once per VM
/// (publisher), so no cache here.
pub fn unsafe_field_cell_two_way_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_UNSAFE_FIELD_CELL_TWO_WAY")
}

/// JIT round 21 wave 1, lane updaters: bit 0 of
/// [`DirectHelperTable::unsafe_field_cells`] (the table is a boxed `u64`
/// slice, so the bit is never part of its address) says this VM answers
/// `Unsafe` instance-field offsets as BYTE offsets
/// (`CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS`: `16 + slot * 8 + k`,
/// `native-builtins` `unsafe_encode_field_offset`). The cell screen
/// ([`unsafe_inline_field_cell_bytes`], [`unsafe_inline_ref_field_cell_bytes`])
/// then decodes the slot first -- `offset - 16`, `k == 0` required, `>> 3` --
/// and keys the cell by it exactly as before. The VM sets the bit only under
/// [`unsafe_field_cells_byte_offsets_enabled`]; without it a byte-offset VM
/// bakes no table, as since JIT round 20 wave 8c.
pub const UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG: usize = 1;

/// `CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS=1` (JIT round 21 wave 1,
/// lane updaters; opt-in, default OFF): give a VM running byte field offsets
/// (`CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS`, default ON since `b0dc8fb375`) the
/// `Unsafe` instance-field arm again. Since that flip the VM baked no cell
/// table (`vm::jit::helpers` "not under `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS`"),
/// so every field-updater get / set / CAS / getAndAdd, AQS `state` and
/// `LongAdder` cell access went back to its thin call or the funnel. With
/// this on the VM tags the table ([`UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG`]) and
/// the screen decodes the byte offset to the slot the cells are keyed by.
/// Read by the VM once per compile while it fills the table.
pub fn unsafe_field_cells_byte_offsets_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS")
}

/// The `SUB R11, 16 ; TEST R11B, 7 ; JNZ decline ; SHR R11, 3` decode the
/// cell screen emits first for a tagged table: R11 = a byte field offset in,
/// the slot index out. An offset below 16 wraps to a huge value whose `>> 3`
/// fails the screen's `CMP R11, 0x7FFF ; JA`, as does every synthetic-band
/// offset (`>= 0x2000_0000`) and the `Buffer.address` sentinel; an offset
/// inside a field (`k != 0`) declines here (the natives serve its bytes).
/// Returns the bytes and the offset of the `JNZ`'s rel32 field.
pub fn unsafe_byte_field_offset_decode_bytes() -> (Vec<u8>, usize) {
    let mut b = vec![0x49, 0x83, 0xEB, 0x10]; // SUB R11, 16
    b.extend_from_slice(&[0x41, 0xF6, 0xC3, 0x07]); // TEST R11B, 7
    b.extend_from_slice(&[0x0F, 0x85, 0, 0, 0, 0]); // JNZ rel32
    let edge = b.len() - 4;
    b.extend_from_slice(&[0x49, 0xC1, 0xEB, 0x03]); // SHR R11, 3
    (b, edge)
}

/// One `Unsafe` read-modify-write site the prefixes may answer inline over an
/// aligned primitive-array element (nio21 wave 5 L3; the section comment
/// above says exactly what is served) or, since nio21 wave 7 L3, a published
/// compact instance field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnsafeInlineRmw {
    /// 4 (`int`) or 8 (`long`).
    pub width: u8,
    /// The operation.
    pub op: UnsafeRmwOp,
}

impl UnsafeInlineRmw {
    /// nio21 wave 7 L3 (W6-L3-2): the admission rows of the
    /// `compareAndExchange{Int,Long}` natives, bits 16 and 17 of
    /// [`DirectHelperTable::unsafe_inline_admitted`] -- after
    /// [`UnsafeInlineAccess::GATE_BITS`]' sixteen and deliberately NOT in that
    /// list: those sixteen are the VM's thin-helper gates (`DirectHelperGate`,
    /// whose memos are per VM since nio21 wave 8 L2, in
    /// `JitRealm::direct_helper_gates`), while these two have no thin helper
    /// and their policy memo is `JitRealm::unsafe_inline.cae_sites`, so the
    /// VM asks them apart and ORs the answer in.
    pub const CAE_GATE_BITS: [(&'static str, &'static str); 2] = [
        ("compareAndExchangeInt", "(Ljava/lang/Object;JII)I"),
        ("compareAndExchangeLong", "(Ljava/lang/Object;JJJ)J"),
    ];

    /// The bit of [`Self::CAE_GATE_BITS`]' first row.
    pub const CAE_FIRST_BIT: usize = 16;

    /// Java parameters after the receiver: `(obj, offset, expected, new)` for
    /// a CAS or a compareAndExchange, `(obj, offset, value)` otherwise.
    pub const fn num_params(self) -> usize {
        match self.op {
            UnsafeRmwOp::Cas | UnsafeRmwOp::Cae => 4,
            _ => 3,
        }
    }

    /// Does this site take `(expected, new)` operands (a CAS or a
    /// compareAndExchange)?
    pub const fn has_expected(self) -> bool {
        matches!(self.op, UnsafeRmwOp::Cas | UnsafeRmwOp::Cae)
    }

    /// The JVM return type tag (`Z` for a CAS).
    pub const fn ret_type(self) -> u8 {
        match (self.op, self.width) {
            (UnsafeRmwOp::Cas, _) => b'Z',
            (_, 4) => b'I',
            _ => b'J',
        }
    }

    /// The admission bits this site needs in
    /// [`DirectHelperTable::unsafe_inline_admitted`]: the width's CAS row
    /// (bit 14 `int`, 15 `long`) always -- every one of these methods is, or
    /// reaches only, `compareAndSet*` -- and for `getAndAdd` / `getAndSet`
    /// also the width's `get*Volatile` row (10 / 11), the other native their
    /// JDK Java body calls. A `getAndBitwise*` row also needs the width's
    /// plain `get*` row (3 / 4): its `Release` body reads with `getInt` /
    /// `getLong`. A `compareAndExchange*` row needs its own bit (16 `int`, 17
    /// `long`, [`Self::CAE_GATE_BITS`]) and the width's CAS row too: the VM
    /// clears the CAS rows under `CRATONVM_UNSAFE_ARRAY_HW_CAS=0`, which is
    /// also when the natives' array exchange leaves the hardware.
    pub const fn needed_bits(self) -> usize {
        let wide = (self.width == 8) as usize;
        let cas = 1usize << (14 + wide);
        match self.op {
            UnsafeRmwOp::Cas => cas,
            UnsafeRmwOp::Cae => cas | (1usize << (Self::CAE_FIRST_BIT + wide)),
            UnsafeRmwOp::GetAndBitwise(_) => cas | (1usize << (10 + wide)) | (1usize << (3 + wide)),
            _ => cas | (1usize << (10 + wide)),
        }
    }

    /// Does the VM's compile-time policy word admit this site?
    pub const fn admitted(self, mask: usize) -> bool {
        mask & self.needed_bits() == self.needed_bits()
    }
}

/// Recognise an `Unsafe` CAS / getAndAdd / getAndSet site the prefixes may
/// answer inline (nio21 wave 5 L3), by class, name and descriptor: the
/// `compareAndSet{Int,Long}` natives, the `weakCompareAndSet{Int,Long}`
/// family (Java bodies that call them), and the `getAndAdd{Int,Long}` /
/// `getAndSet{Int,Long}` Java bodies (a `getXVolatile` + `weakCompareAndSetX`
/// loop that one `LOCK XADD` / `XCHG` performs atomically). Since nio21 wave 6
/// L3 the `getAndBitwise*` family, and since wave 7 L3 the
/// `compareAndExchange{Int,Long}{,Acquire,Release}` rows (unless
/// `CRATONVM_JIT_UNSAFE_CAE_INLINE=0`). Not the narrow or floating-point forms.
/// `None` under `CRATONVM_JIT_UNSAFE_RMW_INLINE=0`.
pub fn unsafe_inline_rmw_site(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<UnsafeInlineRmw> {
    if class_name != "jdk/internal/misc/Unsafe" {
        return None;
    }
    let rmw = |width: u8, op: UnsafeRmwOp| UnsafeInlineRmw { width, op };
    let site = match (method_name, descriptor) {
        (
            "compareAndSetInt"
            | "weakCompareAndSetInt"
            | "weakCompareAndSetIntPlain"
            | "weakCompareAndSetIntAcquire"
            | "weakCompareAndSetIntRelease",
            "(Ljava/lang/Object;JII)Z",
        ) => rmw(4, UnsafeRmwOp::Cas),
        (
            "compareAndSetLong"
            | "weakCompareAndSetLong"
            | "weakCompareAndSetLongPlain"
            | "weakCompareAndSetLongAcquire"
            | "weakCompareAndSetLongRelease",
            "(Ljava/lang/Object;JJJ)Z",
        ) => rmw(8, UnsafeRmwOp::Cas),
        ("getAndAddInt", "(Ljava/lang/Object;JI)I") => rmw(4, UnsafeRmwOp::GetAndAdd),
        ("getAndAddLong", "(Ljava/lang/Object;JJ)J") => rmw(8, UnsafeRmwOp::GetAndAdd),
        ("getAndSetInt", "(Ljava/lang/Object;JI)I") => rmw(4, UnsafeRmwOp::GetAndSet),
        ("getAndSetLong", "(Ljava/lang/Object;JJ)J") => rmw(8, UnsafeRmwOp::GetAndSet),
        // nio21 wave 7 L3 (W6-L3-2): the exchange rows. The un-suffixed form
        // is the native; `Acquire` / `Release` are Java bodies that call it.
        (
            "compareAndExchangeInt"
            | "compareAndExchangeIntAcquire"
            | "compareAndExchangeIntRelease",
            "(Ljava/lang/Object;JII)I",
        ) if unsafe_cae_inline_enabled() => rmw(4, UnsafeRmwOp::Cae),
        (
            "compareAndExchangeLong"
            | "compareAndExchangeLongAcquire"
            | "compareAndExchangeLongRelease",
            "(Ljava/lang/Object;JJJ)J",
        ) if unsafe_cae_inline_enabled() => rmw(8, UnsafeRmwOp::Cae),
        // nio21 wave 6 L3 (W5-L3-2): the bitwise rows, Java bodies all.
        (m, d) if m.starts_with("getAndBitwise") && unsafe_bitwise_inline_enabled() => {
            let rest = &m["getAndBitwise".len()..];
            let (bit, rest) = if let Some(r) = rest.strip_prefix("Or") {
                (UnsafeBitOp::Or, r)
            } else if let Some(r) = rest.strip_prefix("And") {
                (UnsafeBitOp::And, r)
            } else if let Some(r) = rest.strip_prefix("Xor") {
                (UnsafeBitOp::Xor, r)
            } else {
                return None;
            };
            let width = match (rest, d) {
                ("Int" | "IntAcquire" | "IntRelease", "(Ljava/lang/Object;JI)I") => 4,
                ("Long" | "LongAcquire" | "LongRelease", "(Ljava/lang/Object;JJ)J") => 8,
                _ => return None,
            };
            rmw(width, UnsafeRmwOp::GetAndBitwise(bit))
        }
        _ => return None,
    };
    unsafe_rmw_inline_enabled().then_some(site)
}

/// `CRATONVM_JIT_UNSAFE_RMW_BODY_NO_SPLICE=0` (JIT round 20 wave 11, lane
/// pAtom; W6-L3-3 = W5-L3-3, re-filed AT-5 in
/// `docs/feature-designs/jit-ir-optimizations.md`) -- let the optimizing
/// tier splice the JDK bodies of `Unsafe.getAndAdd*` / `getAndSet*` /
/// `getAndBitwise*` again even where [`unsafe_inline_rmw_body_splice_loses_prefix`]
/// says the RMW prefix would serve the site. Default ON. Read at inline
/// resolution (compile time), so no cache.
pub fn unsafe_rmw_body_no_splice_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_UNSAFE_RMW_BODY_NO_SPLICE")
}

/// JIT round 20 wave 11, lane pAtom (W6-L3-3 / AT-5): would splicing the JDK
/// Java body of this `Unsafe` read-modify-write callee throw away the RMW
/// prefix (`ir_lower::Lowerer::try_emit_ir_unsafe_rmw_prefix`) that would
/// otherwise serve the call as ONE `LOCK XADD` / `XCHG` / `LOCK CMPXCHG` loop?
///
/// The prefix lives on an `Op::Call`. Once the inliner splices
/// `getAndAddInt`'s body (`do { v = getIntVolatile(o, off); } while
/// (!weakCompareAndSetInt(o, off, v, v + delta));`), the call is gone and what
/// remains is that loop with a volatile-get prefix and a CAS prefix inside
/// it -- a load, a `LOCK CMPXCHG` and a back edge with its poll per
/// operation, where HotSpot (and this prefix) emit one locked instruction.
/// `--compatible` never splices it (the registered native shadows the body,
/// `native-shadow`); `--jdk-only` did.
///
/// `true` only for the Java-body rows (`getAndAdd`, `getAndSet`,
/// `getAndBitwise*`; a `compareAndSet*` / `compareAndExchange*` body is one
/// native call, which keeps its own prefix when spliced) when `admitted` (the
/// VM's [`DirectHelperTable::unsafe_inline_admitted`] for this compile) admits
/// the row and the prefix's own compile-time switches are on. A site the
/// prefix then declines at run time (a reference array, a misaligned offset,
/// an unpublished field cell, a null base) takes the call, whose JDK body
/// answers exactly as the splice did. `false` under
/// `CRATONVM_JIT_UNSAFE_RMW_BODY_NO_SPLICE=0`.
pub fn unsafe_inline_rmw_body_splice_loses_prefix(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    admitted: usize,
) -> bool {
    // The RMW prefix is the x86-64 optimizing tier's; elsewhere the splice
    // stays the fast form.
    if !cfg!(target_arch = "x86_64") || admitted == 0 || class_name != "jdk/internal/misc/Unsafe" {
        return false;
    }
    // JIT round 23 wave 1, lane L9: the `getAndSetReference*` Java bodies
    // (a `getReferenceVolatile` + `weakCompareAndSetReference` loop) where
    // the reference getAndSet field arm would serve them. That arm has no
    // array half, so it needs the field arm baked whatever
    // `CRATONVM_JIT_UNSAFE_RMW_VETO_NEEDS_FIELD_ARM` says.
    if is_unsafe_ref_get_and_set(method_name, descriptor) {
        return unsafe_ref_get_and_set_field_enabled()
            && admitted & (1usize << UNSAFE_REF_GET_AND_SET_GATE_BIT) != 0
            && admitted & (1usize << UNSAFE_FIELD_ARM_BAKED_BIT) != 0
            && unsafe_accessor_inline_enabled()
            && unsafe_field_rmw_inline_enabled()
            && crate::ir_thin_prefix_enabled()
            && unsafe_rmw_body_no_splice_enabled()
            && !crate::x64::narrow_oops_block_inline_fields();
    }
    let Some(rmw) = unsafe_inline_rmw_site(class_name, method_name, descriptor) else {
        return false;
    };
    matches!(
        rmw.op,
        UnsafeRmwOp::GetAndAdd | UnsafeRmwOp::GetAndSet | UnsafeRmwOp::GetAndBitwise(_)
    ) && rmw.admitted(admitted)
        && unsafe_accessor_inline_enabled()
        && crate::ir_thin_prefix_enabled()
        && unsafe_rmw_body_no_splice_enabled()
        // JIT round 21 wave 1, lane updaters (UPDATERS21-2, opt-in).
        && (!unsafe_rmw_veto_needs_field_arm_enabled()
            || admitted & (1usize << UNSAFE_FIELD_ARM_BAKED_BIT) != 0)
}

/// The machine code of the RMW itself, shared byte for byte by both tiers'
/// prefixes: on entry RAX = the array base, R11 = the (screened, aligned)
/// offset, RDX = the operand (`new` for a CAS and a getAndSet, the delta for
/// a getAndAdd, the mask for a getAndBitwise) and, for a CAS only, R10 =
/// `expected` (loaded by the caller, as is everything else). On exit RAX = the
/// site's result (`0`/`1` for a CAS, the previous value sign-extended for an
/// `int`). Writes RAX, RDX and the flags; a getAndBitwise also RCX and R10. A
/// CAS swaps RAX and R10 first so CMPXCHG finds `expected` in RAX and the base
/// in R10.
pub fn unsafe_inline_rmw_bytes(rmw: UnsafeInlineRmw) -> Vec<u8> {
    let w64 = rmw.width == 8;
    let mut b = Vec::with_capacity(24);
    match rmw.op {
        UnsafeRmwOp::GetAndBitwise(bit) => {
            b.extend_from_slice(&[0x49, 0x89, 0xC2]); // MOV R10, RAX: R10 = base
                                                      // MOV EAX / RAX, [R10 + R11]: the first guess.
            b.extend_from_slice(&[if w64 { 0x4B } else { 0x43 }, 0x8B, 0x04, 0x1A]);
            let top = b.len();
            if w64 {
                b.push(0x48);
            }
            b.extend_from_slice(&[0x89, 0xC1]); // MOV ECX / RCX, EAX / RAX
            if w64 {
                b.push(0x48);
            }
            let opcode = match bit {
                UnsafeBitOp::Or => 0x09,
                UnsafeBitOp::And => 0x21,
                UnsafeBitOp::Xor => 0x31,
            };
            b.extend_from_slice(&[opcode, 0xD1]); // OP ECX / RCX, EDX / RDX
                                                  // LOCK CMPXCHG [R10 + R11], ECX / RCX: on failure EAX / RAX is
                                                  // the value seen, the next guess.
            b.extend_from_slice(&[0xF0, if w64 { 0x4B } else { 0x43 }, 0x0F, 0xB1, 0x0C, 0x1A]);
            // Cast: the loop is 12 or 14 bytes, well inside a rel8.
            let back = (top as isize - (b.len() as isize + 2)) as i8;
            b.extend_from_slice(&[0x75, back as u8]); // JNE top
            if !w64 {
                b.extend_from_slice(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
            }
        }
        UnsafeRmwOp::Cas => {
            b.extend_from_slice(&[0x49, 0x92]); // XCHG RAX, R10: RAX = expected, R10 = base
                                                // LOCK CMPXCHG [R10 + R11], EDX / RDX (SIB: base R10, index R11).
            b.extend_from_slice(&[0xF0, if w64 { 0x4B } else { 0x43 }, 0x0F, 0xB1, 0x14, 0x1A]);
            b.extend_from_slice(&[0x0F, 0x94, 0xC0]); // SETE AL
            b.extend_from_slice(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
        }
        UnsafeRmwOp::Cae => {
            // nio21 wave 7 L3: the CAS row with the witness kept. CMPXCHG
            // leaves the value the cell held in EAX / RAX on both outcomes
            // (on success that IS `expected`), which is the native's answer.
            b.extend_from_slice(&[0x49, 0x92]); // XCHG RAX, R10: RAX = expected, R10 = base
            b.extend_from_slice(&[0xF0, if w64 { 0x4B } else { 0x43 }, 0x0F, 0xB1, 0x14, 0x1A]);
            if !w64 {
                b.extend_from_slice(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
            }
        }
        UnsafeRmwOp::GetAndAdd | UnsafeRmwOp::GetAndSet => {
            if rmw.op == UnsafeRmwOp::GetAndAdd {
                // LOCK XADD [RAX + R11], EDX / RDX (SIB: base RAX, index R11).
                b.extend_from_slice(&[0xF0, if w64 { 0x4A } else { 0x42 }, 0x0F, 0xC1, 0x14, 0x18]);
            } else {
                // XCHG [RAX + R11], EDX / RDX: a locked exchange.
                b.extend_from_slice(&[if w64 { 0x4A } else { 0x42 }, 0x87, 0x14, 0x18]);
            }
            if w64 {
                b.extend_from_slice(&[0x48, 0x89, 0xD0]); // MOV RAX, RDX
            } else {
                b.extend_from_slice(&[0x48, 0x63, 0xC2]); // MOVSXD RAX, EDX
            }
        }
    }
    b
}

/// The alignment screen both tiers' RMW prefixes emit after the
/// primitive-array screen (R11 = the offset): `TEST R11B, w-1 ; JNZ`. Returns
/// the bytes and the offsets of the rel32 decline fields (each patched to the
/// decline target by the caller).
///
/// Until nio21 wave 7 L3 this also declined the natives' synthetic-offset band
/// (`CMP R11, 0x2000_0000 ; JAE`), because their element arm sent that band to
/// a side store before it looked at the array. The natives now test the array
/// first (`docs/feature-designs/jit-ir-optimizations.md`),
/// so the band is an element on both sides; the decline comes back only under
/// `CRATONVM_UNSAFE_ARRAY_BEFORE_SYNTHETIC=0`, which restores the natives' old
/// order.
pub fn unsafe_inline_rmw_screen_bytes(width: u8) -> (Vec<u8>, Vec<usize>) {
    let mut b = vec![0x41, 0xF6, 0xC3, width - 1, 0x0F, 0x85, 0, 0, 0, 0];
    let mut edges = vec![6];
    if !unsafe_array_before_synthetic_enabled() {
        b.extend_from_slice(&[0x49, 0x81, 0xFB]); // CMP R11, imm32
        b.extend_from_slice(&0x2000_0000u32.to_le_bytes());
        b.extend_from_slice(&[0x0F, 0x83, 0, 0, 0, 0]); // JAE rel32
        edges.push(b.len() - 4);
    }
    (b, edges)
}

/// nio21 wave 7 L3 (W6-L3-1): the instance-field arm's screen, shared byte for
/// byte by both tiers' RMW prefixes. On entry RAX = a non-null base that is
/// NOT an array, R11 = the `Unsafe` offset (a `long`). On a hit R11 = the
/// field's compact displacement, so [`unsafe_inline_rmw_bytes`] (which
/// addresses `[RAX + R11]`) performs the RMW on the field unchanged; every
/// other shape jumps to a decline edge. Writes RCX, RDX, R10, R11 and the
/// flags. Returns the bytes and the offsets of the rel32 decline fields.
///
/// `cells` is [`DirectHelperTable::unsafe_field_cells`]; `epoch_addr` the
/// address of the process layout-replacement epoch
/// (`cratonvm_types::layout_replace_epoch_guard().0`). The sequence:
///
/// ```text
///   (a table tagged `UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG` only, JIT round 21:
///    SUB R11, 16 ; TEST R11B, 7 ; JNZ decline ; SHR R11, 3   ; byte offset -> slot)
///   CMP R11, 0x7FFF ; JA decline            ; no cell names a larger slot
///   MOV R10, cells ; MOV RCX, epoch_addr
///   MOV ECX, [RCX] ; CMP ECX, [R10] ; JNE   ; the table is of THIS layout epoch
///   TEST BYTE [RAX+GC_FLAGS], COMPACT ; JZ  ; a legacy cell keeps the call
///   LEA EDX, [R11+R11] ; (OR EDX, 1)        ; key = slot << 1 | wide
///   MOV ECX, [RAX]                          ; the class id
///   MOV R11D, ECX ; XOR R11D, EDX ; AND R11D, COUNT-1
///   MOV R11, [R10 + R11*8 + 8]              ; the cell, one load
///   MOV R10, R11 ; SHR R10, 32 ; CMP R10D, ECX ; JNE   ; class id
///   MOV R10D, R11D ; SHR R10D, 16 ; CMP R10D, EDX ; JNE ; key
///   MOVZX R11D, R11W ; TEST R11D, R11D ; JZ             ; disp (0 = empty)
/// ```
///
/// The table's epoch word is the layout-replacement epoch every entry was
/// published under (the VM resets the table when it moves), so a cell can
/// never name a displacement from a layout the class no longer has.
pub fn unsafe_inline_field_cell_bytes(
    cells: usize,
    epoch_addr: usize,
    width: u8,
) -> (Vec<u8>, Vec<usize>) {
    unsafe_inline_cell_screen_bytes(cells, epoch_addr, width, 8)
}

/// [`unsafe_inline_field_cell_bytes`] with the cell array at `cell_base`
/// bytes past the table's address: `8` for the int/long cells (right after
/// the epoch word, the bytes that function always emitted), or
/// [`UNSAFE_REF_FIELD_CELL_BASE`] for the reference cells (JIT round 20
/// wave 12, lane qUnsafe, AT-4). The only difference is the displacement of
/// the one cell load (`disp8` for 8, `disp32` otherwise).
fn unsafe_inline_cell_screen_bytes(
    cells: usize,
    epoch_addr: usize,
    width: u8,
    cell_base: i32,
) -> (Vec<u8>, Vec<usize>) {
    const GC_FLAGS: u8 = cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8; // Cast: header byte, disp8
    const _: () = assert!(cratonvm_types::GC_FLAGS_BYTE_OFFSET <= 127);
    const _: () = assert!(UNSAFE_FIELD_CELL_COUNT.is_power_of_two());
    let mut b: Vec<u8> = Vec::with_capacity(128);
    let mut edges: Vec<usize> = Vec::new();
    // JIT round 21 wave 1, lane updaters: a tagged table is a byte-offset
    // VM's (`UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG`); the address is the rest.
    let byte_offsets = cells & UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG != 0;
    let cells = cells & !UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG;
    if byte_offsets {
        let (decode, edge) = unsafe_byte_field_offset_decode_bytes();
        b.extend_from_slice(&decode);
        edges.push(edge);
    }
    let mut jcc = |b: &mut Vec<u8>, cc: u8| {
        b.extend_from_slice(&[0x0F, cc, 0, 0, 0, 0]);
        edges.push(b.len() - 4);
    };
    // CMP R11, imm32 ; JA (unsigned): a negative or too-large offset (with
    // byte offsets: the decoded slot, see above).
    b.extend_from_slice(&[0x49, 0x81, 0xFB]);
    b.extend_from_slice(&(UNSAFE_FIELD_CELL_MAX_SLOT as u32).to_le_bytes()); // Cast: 0x7FFF
    jcc(&mut b, 0x87);
    // MOV R10, imm64 (the table) ; MOV RCX, imm64 (the epoch cell).
    b.extend_from_slice(&[0x49, 0xBA]);
    b.extend_from_slice(&(cells as u64).to_le_bytes()); // Cast: an address
    b.extend_from_slice(&[0x48, 0xB9]);
    b.extend_from_slice(&(epoch_addr as u64).to_le_bytes()); // Cast: an address
    b.extend_from_slice(&[0x8B, 0x09]); // MOV ECX, [RCX]
    b.extend_from_slice(&[0x41, 0x3B, 0x0A]); // CMP ECX, [R10]
    jcc(&mut b, 0x85); // JNE
                       // TEST BYTE [RAX + GC_FLAGS], GC_FLAG_COMPACT ; JZ.
    b.extend_from_slice(&[0xF6, 0x40, GC_FLAGS, cratonvm_types::GC_FLAG_COMPACT]);
    jcc(&mut b, 0x84);
    b.extend_from_slice(&[0x43, 0x8D, 0x14, 0x1B]); // LEA EDX, [R11 + R11]
    if width == 8 {
        b.extend_from_slice(&[0x83, 0xCA, 0x01]); // OR EDX, 1
    }
    b.extend_from_slice(&[0x8B, 0x08]); // MOV ECX, [RAX] (the header's class id)
    b.extend_from_slice(&[0x41, 0x89, 0xCB]); // MOV R11D, ECX
    b.extend_from_slice(&[0x41, 0x31, 0xD3]); // XOR R11D, EDX
    // The cell load and the three compares, shared by both probes: on entry
    // R11D = the unmasked index, R10 = the table; on a hit R11 = the disp.
    // `miss` gets the rel32 field of each miss branch.
    let probe = |b: &mut Vec<u8>, miss: &mut Vec<usize>| {
        let mut jcc = |b: &mut Vec<u8>, cc: u8| {
            b.extend_from_slice(&[0x0F, cc, 0, 0, 0, 0]);
            miss.push(b.len() - 4);
        };
        b.extend_from_slice(&[0x41, 0x81, 0xE3]); // AND R11D, imm32
        b.extend_from_slice(&((UNSAFE_FIELD_CELL_COUNT - 1) as u32).to_le_bytes()); // Cast: 255
        if cell_base == 8 {
            b.extend_from_slice(&[0x4F, 0x8B, 0x5C, 0xDA, 0x08]); // MOV R11, [R10 + R11*8 + 8]
        } else {
            // MOV R11, [R10 + R11*8 + disp32] (mod=10, reg=R11, rm=SIB).
            b.extend_from_slice(&[0x4F, 0x8B, 0x9C, 0xDA]);
            b.extend_from_slice(&cell_base.to_le_bytes());
        }
        b.extend_from_slice(&[0x4D, 0x89, 0xDA]); // MOV R10, R11
        b.extend_from_slice(&[0x49, 0xC1, 0xEA, 0x20]); // SHR R10, 32
        b.extend_from_slice(&[0x41, 0x39, 0xCA]); // CMP R10D, ECX
        jcc(b, 0x85);
        b.extend_from_slice(&[0x45, 0x89, 0xDA]); // MOV R10D, R11D
        b.extend_from_slice(&[0x41, 0xC1, 0xEA, 0x10]); // SHR R10D, 16
        b.extend_from_slice(&[0x41, 0x39, 0xD2]); // CMP R10D, EDX
        jcc(b, 0x85);
        b.extend_from_slice(&[0x45, 0x0F, 0xB7, 0xDB]); // MOVZX R11D, R11W
        b.extend_from_slice(&[0x45, 0x85, 0xDB]); // TEST R11D, R11D
        jcc(b, 0x84);
    };
    if !unsafe_field_cell_two_way_enabled() {
        probe(&mut b, &mut edges);
        return (b, edges);
    }
    // CF-W9-1, two-way: the first probe's misses go to the second probe, its
    // hit jumps past it.
    let mut first_misses: Vec<usize> = Vec::new();
    probe(&mut b, &mut first_misses);
    b.extend_from_slice(&[0xE9, 0, 0, 0, 0]); // JMP rel32 (hit)
    let hit_jmp = b.len() - 4;
    let second = b.len();
    for at in first_misses {
        patch_rel32(&mut b, at, second);
    }
    // MOV R10, imm64 (the table again: the first probe consumed R10).
    b.extend_from_slice(&[0x49, 0xBA]);
    b.extend_from_slice(&(cells as u64).to_le_bytes()); // Cast: an address
    // IMUL R11D, ECX, imm32 ; SHR R11D, 24 ; XOR R11D, EDX.
    b.extend_from_slice(&[0x44, 0x69, 0xD9]);
    b.extend_from_slice(&UNSAFE_FIELD_CELL_HASH2.to_le_bytes());
    b.extend_from_slice(&[0x41, 0xC1, 0xEB, 0x18]);
    b.extend_from_slice(&[0x41, 0x31, 0xD3]);
    probe(&mut b, &mut edges);
    let end = b.len();
    patch_rel32(&mut b, hit_jmp, end);
    (b, edges)
}

/// CF-W9-1: point the rel32 field at `at` (inside `b`) at `target` (an offset
/// in `b`), relative to the end of the field.
fn patch_rel32(b: &mut [u8], at: usize, target: usize) {
    // Cast: both are offsets in a screen of a few hundred bytes, so the
    // displacement always fits rel32 (no production panic, contract C11).
    let delta = target as i64 - (at as i64 + 4);
    debug_assert!(i32::try_from(delta).is_ok(), "screen jump fits rel32");
    let rel = delta as i32;
    b[at..at + 4].copy_from_slice(&rel.to_le_bytes());
}

/// The two words both tiers bake into the instance-field arm
/// (`(cells, epoch_addr)` for [`unsafe_inline_field_cell_bytes`]), or `None`
/// when this compile has no field arm: no cell table from the VM,
/// `CRATONVM_JIT_UNSAFE_FIELD_RMW_INLINE=0`, no layout-epoch cell, or an armed
/// ZGC read barrier (which blocks every inline field access,
/// `x64::zgc_read_barrier_blocks_inline_fields`).
pub fn unsafe_field_arm_words(table: &DirectHelperTable) -> Option<(usize, usize)> {
    if table.unsafe_field_cells == 0
        || !unsafe_field_rmw_inline_enabled()
        || crate::x64::zgc_read_barrier_blocks_inline_fields()
    {
        return None;
    }
    let (epoch, _) = cratonvm_types::layout_replace_epoch_guard();
    if epoch.is_null() {
        return None;
    }
    Some((table.unsafe_field_cells, epoch as usize)) // Cast: an address
}

/// `CRATONVM_JIT_UNSAFE_VOLATILE_FIELD_INLINE=0` (JIT round 20 wave 11, lane
/// pAtom; W7-L3-1, re-filed AT-3 in `docs/feature-designs/jit-ir-optimizations.md`)
/// -- no instance-field arm in the `{get,put}{Int,Long}Volatile` accessor
/// prefixes of either tier: a non-array base keeps its call (the thin helper
/// or the funnel), as before. Default ON. Read per site at lowering, so no
/// cache.
pub fn unsafe_volatile_field_inline_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_UNSAFE_VOLATILE_FIELD_INLINE")
}

/// JIT round 20 wave 11, lane pAtom (W7-L3-1 / AT-3): the cell-table words
/// ([`unsafe_field_arm_words`]) for a `{get,put}{Int,Long}Volatile` accessor
/// site's instance-field arm, or `None` (no field arm: the site's non-array
/// bases keep their call). Both tiers' accessor prefixes ask this.
///
/// # The atomicity-domain audit (why a plain `MOV` / `XCHG` joins the natives)
///
/// A cell `(class id, slot, width) -> disp` exists only where
/// `vm_exec::hw_atomic_addr`'s COMPACT arm answered: the slot's declared
/// descriptor is exactly `I` (`J` for the wide key), the instance is compact,
/// the storage is exactly the access width and `base + disp` is naturally
/// aligned. For such an instance the natives
/// (`unsafe_natives_ext::native_unsafe_{get,put}_{int,long}_volatile`) reach
/// `NativeContext::{get,set}_field_volatile(obj, slot)` (a slot of at most
/// `0x7FFF` is below the synthetic-offset band; under
/// `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS` the table is baked only TAGGED,
/// `CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS`, and the screen admits only
/// `16 + slot * 8`, which the natives decode to `(slot, 0)` -- the whole field,
/// the same `get/set_field_volatile(obj, slot)` call), which on every collector
/// (Generational, G1, ZGC) is `get_field` / `set_field` between two SeqCst
/// fences under `collector::volatile_stripe_lock`, and the compact arm of
/// those is `cratonvm_types::{read,write}_compact_field`: ONE aligned
/// `AtomicU32` / `AtomicU64` load or store of exactly these bytes. The stripe
/// lock exists for the 16-byte LEGACY `Value` cell (tag + payload), which a
/// compact `int` / `long` has none of; it only serialises the natives among
/// themselves, and the compiled volatile `putfield` / `getfield` arms already
/// access these same compact bytes with a plain store (+ `LOCK ADD` fence) /
/// load and no lock. So the inline get (one `MOV`: an acquire on x86-TSO) and
/// put (one `XCHG`: a store with the StoreLoad a volatile store owes) are
/// single-copy atomic against every native, interpreter and compiled access
/// of the field, with the same answer the native would give. Primitive bytes
/// only: no collector barrier is owed, and references are not admitted.
pub fn unsafe_volatile_field_arm_words(
    table: &DirectHelperTable,
    access: UnsafeInlineAccess,
) -> Option<(usize, usize)> {
    if !access.volatile
        || access.unaligned
        || access.big_endian_operand
        || !matches!(access.width, 4 | 8)
        || !unsafe_volatile_field_inline_enabled()
    {
        return None;
    }
    unsafe_field_arm_words(table)
}

// ── JIT round 20 wave 12, lane qUnsafe: AT-4 / W7-L3-2 ───────────────────
//
// `Unsafe.compareAndSetReference` (and the `weakCompareAndSetReference*`
// Java bodies that call it) on a published COMPACT reference field, as ONE
// `LOCK CMPXCHG` in both tiers. Books: `jit-r21-nio-proposals-w7-L3.md`
// W7-L3-2, re-filed AT-4 in `docs/feature-designs/jit-ir-optimizations.md`.
//
// * Atomicity domain: the VM publishes a reference cell only from
//   `vm_exec::hw_atomic_addr`'s compact arm for `HwAtomicKind::Reference`,
//   i.e. exactly where `compare_and_swap_field_shared` (the native's and the
//   thin helper's CAS) is about to do ONE hardware `compare_exchange` on the
//   8-byte bare pointer at `object + disp`, on the Generational and G1 heaps
//   only (`JitRealm::unsafe_inline` never publishes one on ZGC, whose armed
//   load barrier needs the heal-then-CAS seam, AT-7). The compiled `LOCK
//   CMPXCHG` on the same bytes compares the same raw pointer words.
// * Barriers: the constant-`VarHandle` prefix's reference-store gate screen
//   EXACTLY (`ir_lower::vh_ref_store_gate_screen_bytes`, the gates of the
//   gated reference `putfield`): the SATB pre-gate byte clear, and a null new
//   value, a young receiver or no old objects -- decided BEFORE the CAS, and
//   anything else declines to the call, whose helper runs the collector's own
//   SATB-on-`expected` and card barriers. A collector that publishes no gate
//   plan keeps every site on its call.
// * The cells: [`UNSAFE_FIELD_CELL_COUNT`] more words after the int/long
//   cells, same word format and index (`unsafe_field_cell_word(class, slot,
//   true, disp)`), same epoch word -- so an int/long screen can never read a
//   reference cell, and a reset clears both halves.

/// `CRATONVM_JIT_UNSAFE_QU_REF_CAS_FIELD=0` (JIT round 20 wave 12, lane
/// qUnsafe, AT-4 / W7-L3-2): no reference-field arm for
/// `compareAndSetReference` in either tier, and the VM publishes no reference
/// cell (it arms that half only for a compile that bakes it with this on):
/// every site keeps its call (the thin helper or the funnel), exactly as
/// before. Default ON. Read per site at lowering, so no cache.
pub fn unsafe_qu_ref_cas_field_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_UNSAFE_QU_REF_CAS_FIELD")
}

/// The [`DirectHelperTable::unsafe_inline_admitted`] bit of
/// `compareAndSetReference(Object,long,Object,Object)Z`: the VM sets it from
/// the native's thin-helper gate (`UNSAFE_CAS_REFERENCE_GATE`).
pub const UNSAFE_REF_CAS_GATE_BIT: usize = 18;

/// Byte offset of the reference cells from the cell table's address: past
/// the epoch word and the [`UNSAFE_FIELD_CELL_COUNT`] int/long cells.
pub const UNSAFE_REF_FIELD_CELL_BASE: i32 = 8 * (1 + UNSAFE_FIELD_CELL_COUNT as i32); // Cast: 256

/// Recognise a `compareAndSetReference` site the reference-field arm may
/// answer: the native, and the `weakCompareAndSetReference{,Plain,Acquire,
/// Release}` Java bodies (each one `compareAndSetReference` call in JDK 25;
/// a strong `LOCK CMPXCHG` is a correct weak CAS). `false` under
/// `CRATONVM_JIT_UNSAFE_QU_REF_CAS_FIELD=0`.
pub fn unsafe_inline_ref_cas_site(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    class_name == "jdk/internal/misc/Unsafe"
        && descriptor == "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z"
        && matches!(
            method_name,
            "compareAndSetReference"
                | "weakCompareAndSetReference"
                | "weakCompareAndSetReferencePlain"
                | "weakCompareAndSetReferenceAcquire"
                | "weakCompareAndSetReferenceRelease"
        )
        && unsafe_qu_ref_cas_field_enabled()
}

/// The reference cells' screen, shared byte for byte by both tiers: the
/// int/long field arm's screen ([`unsafe_inline_field_cell_bytes`], the wide
/// key) reading the cell from the reference half. On entry RAX = a non-null
/// NON-array base, R11 = the `Unsafe` offset; on a hit R11 = the field's
/// compact displacement. Writes RCX, RDX, R10, R11 and the flags.
pub fn unsafe_inline_ref_field_cell_bytes(
    cells: usize,
    epoch_addr: usize,
) -> (Vec<u8>, Vec<usize>) {
    unsafe_inline_cell_screen_bytes(cells, epoch_addr, 8, UNSAFE_REF_FIELD_CELL_BASE)
}

/// The cell-table words both tiers bake into a `compareAndSetReference`
/// site's reference-field arm, or `None` (no arm: the site keeps its call):
/// the VM's admission bit [`UNSAFE_REF_CAS_GATE_BIT`] clear, no cell table
/// ([`unsafe_field_arm_words`]: also an armed ZGC read barrier), or narrow
/// oops (a compact reference field is then not the 8-byte bare pointer the
/// `LOCK CMPXCHG` compares; the VM would publish no cell either).
pub fn unsafe_ref_cas_field_arm_words(table: &DirectHelperTable) -> Option<(usize, usize)> {
    if table.unsafe_inline_admitted & (1usize << UNSAFE_REF_CAS_GATE_BIT) == 0
        || !unsafe_qu_ref_cas_field_enabled()
        || !unsafe_accessor_inline_enabled()
        || crate::x64::narrow_oops_block_inline_fields()
    {
        return None;
    }
    unsafe_field_arm_words(table)
}

/// The kind test that sends a non-array base to the field arm
/// ([`unsafe_inline_field_cell_bytes`]) before the primitive-array screen:
/// `MOVZX ECX, BYTE [RAX+KIND_TAGS] ; AND ECX, kind mask ; CMP ECX, Array ;
/// JNE field`. RAX = a non-null base; writes RCX. Returns the bytes and the
/// offset of the rel32 field (patched to the field arm by the caller).
pub fn unsafe_inline_not_array_bytes() -> (Vec<u8>, usize) {
    const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: header byte, disp8
    const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
    let mut b = vec![0x0F, 0xB6, 0x48, KIND_TAGS]; // MOVZX ECX, BYTE [RAX+KIND_TAGS]
    b.extend_from_slice(&[0x83, 0xE1, cratonvm_types::KIND_TAG_BYTE_MASK]); // AND ECX, mask
    b.extend_from_slice(&[0x83, 0xF9, cratonvm_types::ObjectKind::Array as u8]); // CMP ECX, Array
    b.extend_from_slice(&[0x0F, 0x85, 0, 0, 0, 0]); // JNE rel32
    let edge = b.len() - 4;
    (b, edge)
}

// ── JIT round 21, lane atomics: STATICCAS21-2 ────────────────────────────
//
// An `Unsafe` CAS / compareAndExchange / getAndAdd / getAndSet /
// getAndBitwise* of `int` / `long` width on a STATIC field -- base the
// `staticFieldBase` mirror, offset the static's minted (or scaled) synthetic
// offset -- as the same ONE locked instruction the RMW prefixes already emit
// for an array element or a published instance field, on the static's
// payload word. Page:
// `jr21-atomics-compiled-static-and-subword-atomics-FIXED-20261026.md`.
// Real users: `ThreadIdentifiers.next()` (`getAndAddLong` on
// `ThreadIdentifiers.class`), `ForkJoinPool` pool ids, every static
// `VarHandle` in `--jdk-only` (`VarHandleInts.FieldStaticReadWrite` calls
// `UNSAFE.getAndAddInt(handle.base, handle.fieldOffset, ..)`).
//
// * The table: [`UNSAFE_STATIC_CELL_COUNT`] four-word entries after the
//   sub-word cells of the same per-VM table ([`DirectHelperTable::unsafe_field_cells`]),
//   `[key, mirror, base cell, class id << 32 | cell displacement]`, published by the VM
//   (`JitRealm::unsafe_inline.note_static_target`) from the natives'
//   mirror-base whole-static CAS (`synthetic_cas`, which the mirror-base
//   compareAndExchange loops through), only for a static declared exactly
//   `I` (`key = offset << 1`) or `J` (`key = offset << 1 | 1`), and only while
//   that native CAS is the VM's hardware update of the cell
//   (`vm_exec::hw_static_cell_update`: `CRATONVM_HW_STATIC_CAS`,
//   `CRATONVM_UNSAFE_STATIC_HW_RMW`, the statics index). An entry is written
//   once (mirror, base cell and displacement first, the key last with
//   `Release`); the layout-epoch reset does not touch this half (a static's
//   storage is not an instance layout), so a reader that matches the key
//   reads a complete entry (x86-TSO keeps loads in order).
// * The base proof: word 1 is the class's `Class` mirror itself, the value
//   `class_mirrors[class]` held when the entry was published (under that
//   map's read lock). It is a WEAK word, maintained like the compiled
//   `getClass()` words (`ClassRealm::getclass_mirror_words`, JIT round 21
//   proposal ATOMICS21-5): the collector re-keys it in the same pause step
//   that moves `class_mirrors` (`memory::gc::update_all_roots` step 6, every
//   mutator parked) and clears the entry when the class unloads, so a class
//   whose static atomics ran compiled stays unloadable (an earlier design
//   proved the base through a JNI-global mirror slot, which pinned the
//   class). `CMP RAX, [entry + 8]` accepts exactly the mirror the natives
//   accept (`unsafe_static_field_target_for_base`: `class_id_from_mirror(base)
//   ==` the registered class); any other base declines to the call, which
//   keeps the natives' answer for it (the per-object synthetic side store).
// * The cell: word 2 is the class's statics base POINTER cell
//   (`StaticsIndex::base_cell_addr`, what compiled `getstatic` bakes), word 3
//   `class_id << 32 | field_index * 16` (the screen loads the low half). The
//   static's 16-byte `Value` cell is
//   `[[base cell]] + disp`; its tag must be `Int` (width 4) or `Long` (width
//   8) -- `hw_static_cell_update`'s tag guard -- and the RMW runs at the
//   payload (`FIELD_CELL_PAYLOAD32_OFFSET` / `_64_`), the very word that
//   function CASes. Every other access of the static (`putstatic` /
//   `getstatic` in any tier, `set_static_shared`) is a plain load or store of
//   that word, which a locked RMW is atomic against.
// * Barriers: none owed -- the payload is primitive and statics are roots,
//   never card or remembered-set sources. No GC point, no call.

/// `CRATONVM_JIT_UNSAFE_STATIC_RMW_INLINE=0` (JIT round 21, lane atomics,
/// STATICCAS21-2) -- no static arm in either tier's `Unsafe` RMW prefix, and
/// the VM publishes no static entry: every static atomic keeps its call (the
/// native's hardware CAS), as before. Default ON. Read per site at lowering
/// and by the VM when it bakes the table, so no cache.
pub fn unsafe_static_rmw_inline_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_UNSAFE_STATIC_RMW_INLINE")
}

/// Entries in the static half of the `Unsafe` cell table. A power of two:
/// the screen indexes with `AND`.
pub const UNSAFE_STATIC_CELL_COUNT: usize = 128;

/// Machine words per static entry: key, mirror, base cell, class id and
/// displacement.
pub const UNSAFE_STATIC_CELL_WORDS: usize = 4;

/// Byte offset of the static entries from the cell table's address: past the
/// epoch word, the int/long cells, the reference cells and the sub-word cells
/// ([`UNSAFE_SUBWORD_FIELD_CELL_BASE`]).
pub const UNSAFE_STATIC_CELL_BASE: usize = 8 * (1 + 3 * UNSAFE_FIELD_CELL_COUNT);

/// The first `Unsafe` offset a static can have (the natives' synthetic band,
/// `native-builtins` `SYNTHETIC_OFFSET_BASE`; a scaled static offset under
/// byte field offsets lies inside it too).
pub const UNSAFE_STATIC_OFFSET_FIRST: usize = 0x2000_0000;

/// The last `Unsafe` offset a static can have (`SYNTHETIC_OFFSET_END`).
pub const UNSAFE_STATIC_OFFSET_LAST: usize = 0x3FFF_FFFF;

/// The `Value` discriminant of a static cell holding an `int` (and of every
/// sub-word static, which is why only declared `I` statics are published).
/// Pinned against `Value::Int` by this file's `jr21_atomics_static_cell_tests`.
pub const UNSAFE_STATIC_TAG_INT: u8 = 0;

/// The `Value` discriminant of a static cell holding a `long`.
pub const UNSAFE_STATIC_TAG_LONG: u8 = 1;

/// The key word of a static entry: `offset << 1 | wide`. Never `0` for an
/// offset in the static band.
pub const fn unsafe_static_cell_key(offset: usize, wide: bool) -> u64 {
    ((offset as u64) << 1) | wide as u64 // Cast: an offset below 2^30
}

/// The entry index of `offset`: `(offset ^ (offset >> 3)) & (COUNT - 1)` --
/// minted offsets are consecutive and scaled ones multiples of 8, so both
/// spread. Exactly what [`unsafe_inline_static_cell_bytes`] computes (in 32
/// bits, which an offset of the band fits).
pub const fn unsafe_static_cell_index(offset: usize) -> usize {
    (offset ^ (offset >> 3)) & (UNSAFE_STATIC_CELL_COUNT - 1)
}

/// The static half's address for this compile's RMW prefixes, or `None` (no
/// static arm): no cell table from the VM, `CRATONVM_JIT_UNSAFE_FIELD_RMW_INLINE=0`
/// (the VM bakes no table then), `CRATONVM_JIT_UNSAFE_STATIC_RMW_INLINE=0`, or
/// an armed ZGC read barrier (`x64::zgc_read_barrier_blocks_inline_fields`,
/// the gate of the compiled `getClass()` words, whose re-key discipline the
/// entries' mirror word shares).
pub fn unsafe_static_arm_table(table: &DirectHelperTable) -> Option<usize> {
    if table.unsafe_field_cells == 0
        || !unsafe_field_rmw_inline_enabled()
        || !unsafe_static_rmw_inline_enabled()
        || !cfg!(target_arch = "x86_64")
        || crate::x64::zgc_read_barrier_blocks_inline_fields()
    {
        return None;
    }
    Some(table.unsafe_field_cells & !UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG)
}

/// The static arm's screen, shared byte for byte by both tiers' RMW prefixes.
/// On entry RAX = a non-null NON-array base, R11 = the `Unsafe` offset (a
/// `long`). On a hit RAX = the static's statics block base and R11 = the
/// payload's displacement in it, so [`unsafe_inline_rmw_bytes`] (which
/// addresses `[RAX + R11]`) performs the RMW on the static's payload word.
/// Writes RAX (only after the proof), RCX, RDX, R10, R11 and the flags.
///
/// Returns `(bytes, declines, not_static)`: the rel32 decline fields, and the
/// one rel32 field taken for an offset outside the static band -- with R11
/// still the offset, so the caller may route it to the instance-field screen.
///
/// ```text
///   MOV RCX, R11 ; SUB RCX, FIRST ; CMP RCX, LAST-FIRST ; JA not_static
///   MOV ECX, R11D ; SHR ECX, 3 ; XOR ECX, R11D ; AND ECX, COUNT-1 ; SHL ECX, 5
///   LEA RDX, [R11 + R11] ; (OR RDX, 1)             ; the key
///   MOV R10, table + STATIC_BASE
///   CMP RDX, [R10 + RCX] ; JNE decline              ; key (0 = empty)
///   CMP RAX, [R10 + RCX + 8] ; JNE decline          ; the class's mirror
///   MOV R11D, [R10 + RCX + 24]                      ; field_index * 16
///   MOV RAX, [R10 + RCX + 16] ; MOV RAX, [RAX]      ; the statics block
///   TEST RAX, RAX ; JZ decline
///   CMP DWORD [RAX + R11], tag ; JNE decline        ; Int / Long cell
///   ADD R11, payload
/// ```
pub fn unsafe_inline_static_cell_bytes(table: usize, width: u8) -> (Vec<u8>, Vec<usize>, usize) {
    const _: () = assert!(cratonvm_types::FIELD_CELL_TAG_OFFSET == 0);
    const _: () = assert!(UNSAFE_STATIC_CELL_COUNT.is_power_of_two());
    const _: () = assert!(UNSAFE_STATIC_CELL_COUNT <= 128);
    const _: () = assert!(UNSAFE_STATIC_CELL_WORDS * 8 == 32);
    let wide = width == 8;
    let mut b: Vec<u8> = Vec::with_capacity(112);
    let mut edges: Vec<usize> = Vec::new();
    let jcc = |b: &mut Vec<u8>, cc: u8| -> usize {
        b.extend_from_slice(&[0x0F, cc, 0, 0, 0, 0]);
        b.len() - 4
    };
    // The band: an unsigned `offset - FIRST <= LAST - FIRST`.
    b.extend_from_slice(&[0x4C, 0x89, 0xD9]); // MOV RCX, R11
    b.extend_from_slice(&[0x48, 0x81, 0xE9]); // SUB RCX, imm32
    b.extend_from_slice(&(UNSAFE_STATIC_OFFSET_FIRST as u32).to_le_bytes()); // Cast: 0x2000_0000
    b.extend_from_slice(&[0x48, 0x81, 0xF9]); // CMP RCX, imm32
    b.extend_from_slice(
        &((UNSAFE_STATIC_OFFSET_LAST - UNSAFE_STATIC_OFFSET_FIRST) as u32).to_le_bytes(), // Cast: 0x1FFF_FFFF
    );
    let not_static = jcc(&mut b, 0x87); // JA
                                        // ECX = the entry's byte offset in the static half.
    b.extend_from_slice(&[0x44, 0x89, 0xD9]); // MOV ECX, R11D
    b.extend_from_slice(&[0xC1, 0xE9, 0x03]); // SHR ECX, 3
    b.extend_from_slice(&[0x44, 0x31, 0xD9]); // XOR ECX, R11D
    b.extend_from_slice(&[0x83, 0xE1, (UNSAFE_STATIC_CELL_COUNT - 1) as u8]); // AND ECX, imm8 (Cast: 127)
    b.extend_from_slice(&[0xC1, 0xE1, 0x05]); // SHL ECX, 5 (32-byte entries)
                                              // RDX = the key.
    b.extend_from_slice(&[0x4B, 0x8D, 0x14, 0x1B]); // LEA RDX, [R11 + R11]
    if wide {
        b.extend_from_slice(&[0x48, 0x83, 0xCA, 0x01]); // OR RDX, 1
    }
    b.extend_from_slice(&[0x49, 0xBA]); // MOV R10, imm64
    b.extend_from_slice(&((table + UNSAFE_STATIC_CELL_BASE) as u64).to_le_bytes()); // Cast: an address
    b.extend_from_slice(&[0x49, 0x3B, 0x14, 0x0A]); // CMP RDX, [R10 + RCX]
    edges.push(jcc(&mut b, 0x85)); // JNE
                                   // The base is the class's mirror (the entry's collector-maintained word).
    b.extend_from_slice(&[0x49, 0x3B, 0x44, 0x0A, 0x08]); // CMP RAX, [R10 + RCX + 8]
    edges.push(jcc(&mut b, 0x85)); // JNE
                                   // The low half of word 3 (the high half is the class id).
    b.extend_from_slice(&[0x45, 0x8B, 0x5C, 0x0A, 0x18]); // MOV R11D, [R10 + RCX + 24]
    b.extend_from_slice(&[0x49, 0x8B, 0x44, 0x0A, 0x10]); // MOV RAX, [R10 + RCX + 16]
    b.extend_from_slice(&[0x48, 0x8B, 0x00]); // MOV RAX, [RAX]: the block now
    b.extend_from_slice(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
    edges.push(jcc(&mut b, 0x84)); // JZ
    let tag = if wide {
        UNSAFE_STATIC_TAG_LONG
    } else {
        UNSAFE_STATIC_TAG_INT
    };
    b.extend_from_slice(&[0x42, 0x83, 0x3C, 0x18, tag]); // CMP DWORD [RAX + R11], imm8
    edges.push(jcc(&mut b, 0x85)); // JNE
    let payload = if wide {
        cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET
    } else {
        cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET
    };
    b.extend_from_slice(&[0x49, 0x83, 0xC3, payload as u8]); // ADD R11, imm8 (Cast: 4 or 8)
    (b, edges, not_static)
}

// ── JIT round 21, lane atomics: UNSAFE21-2 ───────────────────────────────
//
// `Unsafe.compareAndExchangeByte` / `compareAndExchangeShort` /
// `compareAndSetShort` / `compareAndSetByte` on a COMPACT `boolean` / `byte` /
// `short` / `char` instance field as ONE `LOCK CMPXCHG` of the field's width
// in both tiers -- HotSpot's `lock cmpxchgb/w`. The JDK's other sub-word CAS
// spellings reach these four: `compareAndSetChar` / `compareAndExchangeChar`
// / `compareAndSetBoolean` are bodies over the `Short` / `Byte` forms, the
// `weak*` / `Acquire` / `Release` forms bodies over the plain ones, and in
// `--jdk-only` `compareAndSetByte` is the JDK's `compareAndExchangeByte(..) ==
// expected`.
//
// * Atomicity domain: the VM publishes a sub-word cell only from
//   `vm_exec::hw_cas_subword`'s compact arm (JIT round 22 wave 4c,
//   `CRATONVM_HW_SUBWORD_CAS`), i.e. exactly where every native sub-word
//   field CAS (the natives' whole-field paths, `FieldLane`'s whole-slot CAS
//   loops, the `VarHandle` natives) is ONE hardware `LOCK CMPXCHG` of the
//   field's width on the compact payload. Plain and volatile stores of the
//   field are plain stores of those bytes. So the compiled `LOCK CMPXCHG` on
//   the same bytes is atomic against every access.
// * The natives' answer: they compare the field's raw low byte / word with
//   `expected`'s (`boolean` and `byte` the raw low 8 bits, `char` / `short`
//   the low 16), as CMPXCHG compares AL / AX, and store `new`'s low bytes.
//   CMPXCHG leaves the witness in AL / AX; `MOVSX` widens it as the
//   compareAndExchange natives answer it (sign-extended).
// * Cells: a third [`UNSAFE_FIELD_CELL_COUNT`]-word half of the instance cell
//   table at [`UNSAFE_SUBWORD_FIELD_CELL_BASE`], keyed like the int/long half
//   with the key's low bit = "2-byte field" (`boolean` / `byte` 0, `short` /
//   `char` 1), so a `short` access never matches a `byte` field. Same epoch
//   word, same reset, two-way like the other halves. No array element (the
//   prefix declines arrays: the natives' byte-lane rules for arrays stay on
//   the call), no legacy cell (the screen's compact test).
// * Barriers: none owed -- primitive bytes.

/// `CRATONVM_JIT_UNSAFE_SUBWORD_RMW_INLINE=0` (JIT round 21, lane atomics,
/// UNSAFE21-2) -- no sub-word CAS prefix in either tier and the VM publishes
/// no sub-word cell: every `compareAndExchange{Byte,Short}` /
/// `compareAndSet{Byte,Short}` site keeps its call, as before. Default ON.
/// Read per site at lowering and by the VM when it bakes the table, so no
/// cache.
pub fn unsafe_subword_rmw_inline_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_UNSAFE_SUBWORD_RMW_INLINE")
}

/// Byte offset of the sub-word cells from the cell table's address: past the
/// epoch word, the int/long cells and the reference cells.
pub const UNSAFE_SUBWORD_FIELD_CELL_BASE: i32 = 8 * (1 + 2 * UNSAFE_FIELD_CELL_COUNT as i32); // Cast: 256

/// One sub-word CAS site the prefixes may answer inline (UNSAFE21-2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnsafeSubwordRmw {
    /// 1 (`byte`, which also serves a `boolean` field) or 2 (`short`, which
    /// also serves a `char` field).
    pub width: u8,
    /// `true` for a compareAndExchange (the witness, sign-extended), `false`
    /// for a compareAndSet (`0` / `1`).
    pub cae: bool,
}

impl UnsafeSubwordRmw {
    /// The admission rows, bits [`Self::FIRST_BIT`].. of
    /// [`DirectHelperTable::unsafe_inline_admitted`] in this order. None has a
    /// thin helper, so the VM asks their policy through per-VM memos
    /// (`JitRealm::unsafe_inline.subword_sites`) and ORs the answer in, as for
    /// the `compareAndExchange{Int,Long}` rows.
    pub const GATE_BITS: [(&'static str, &'static str); 4] = [
        ("compareAndExchangeByte", "(Ljava/lang/Object;JBB)B"),
        ("compareAndExchangeShort", "(Ljava/lang/Object;JSS)S"),
        ("compareAndSetShort", "(Ljava/lang/Object;JSS)Z"),
        ("compareAndSetByte", "(Ljava/lang/Object;JBB)Z"),
    ];

    /// The bit of [`Self::GATE_BITS`]' first row: after every older row (the
    /// reference get-and-set row, bit [`UNSAFE_REF_GET_AND_SET_GATE_BIT`], is
    /// the highest).
    pub const FIRST_BIT: usize = 29;

    /// This site's row in [`Self::GATE_BITS`].
    pub const fn row(self) -> usize {
        match (self.width, self.cae) {
            (1, true) => 0,
            (_, true) => 1,
            (2, false) => 2,
            _ => 3,
        }
    }

    /// Does the VM's compile-time policy word admit this site?
    pub const fn admitted(self, mask: usize) -> bool {
        mask & (1usize << (Self::FIRST_BIT + self.row())) != 0
    }
}

const _: () = assert!(UNSAFE_REF_GET_AND_SET_GATE_BIT < UnsafeSubwordRmw::FIRST_BIT);
const _: () = assert!(UNSAFE_FIELD_ARM_BAKED_BIT < UnsafeSubwordRmw::FIRST_BIT);
const _: () = assert!(
    UnsafeSubwordRmw::FIRST_BIT + UnsafeSubwordRmw::GATE_BITS.len() <= usize::BITS as usize
);

/// Recognise a sub-word CAS site (UNSAFE21-2), by class, name and descriptor.
/// `None` under `CRATONVM_JIT_UNSAFE_SUBWORD_RMW_INLINE=0`.
pub fn unsafe_inline_subword_site(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<UnsafeSubwordRmw> {
    if class_name != "jdk/internal/misc/Unsafe" {
        return None;
    }
    let site = match (method_name, descriptor) {
        ("compareAndExchangeByte", "(Ljava/lang/Object;JBB)B") => UnsafeSubwordRmw {
            width: 1,
            cae: true,
        },
        ("compareAndExchangeShort", "(Ljava/lang/Object;JSS)S") => UnsafeSubwordRmw {
            width: 2,
            cae: true,
        },
        ("compareAndSetShort", "(Ljava/lang/Object;JSS)Z") => UnsafeSubwordRmw {
            width: 2,
            cae: false,
        },
        ("compareAndSetByte", "(Ljava/lang/Object;JBB)Z") => UnsafeSubwordRmw {
            width: 1,
            cae: false,
        },
        _ => return None,
    };
    unsafe_subword_rmw_inline_enabled().then_some(site)
}

/// The machine code of a sub-word CAS, shared by both tiers: on entry RAX =
/// the object, R11 = the field's compact displacement, R10 = `expected`, RDX =
/// `new`. `XCHG RAX, R10` (RAX = expected, R10 = the object), `LOCK CMPXCHG
/// [R10 + R11], DL / DX`, then the result in RAX: the witness sign-extended
/// (`MOVSX RAX, AL / AX`) for a compareAndExchange, `0` / `1` for a
/// compareAndSet. Writes RAX, R10 and the flags.
pub fn unsafe_inline_subword_rmw_bytes(rmw: UnsafeSubwordRmw) -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    b.extend_from_slice(&[0x49, 0x92]); // XCHG RAX, R10
    if rmw.width == 1 {
        b.extend_from_slice(&[0xF0, 0x43, 0x0F, 0xB0, 0x14, 0x1A]); // LOCK CMPXCHG [R10+R11], DL
    } else {
        b.extend_from_slice(&[0xF0, 0x66, 0x43, 0x0F, 0xB1, 0x14, 0x1A]); // LOCK CMPXCHG [R10+R11], DX
    }
    match (rmw.cae, rmw.width) {
        (true, 1) => b.extend_from_slice(&[0x48, 0x0F, 0xBE, 0xC0]), // MOVSX RAX, AL
        (true, _) => b.extend_from_slice(&[0x48, 0x0F, 0xBF, 0xC0]), // MOVSX RAX, AX
        (false, _) => {
            b.extend_from_slice(&[0x0F, 0x94, 0xC0]); // SETE AL
            b.extend_from_slice(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
        }
    }
    b
}

/// The sub-word cells' screen, shared byte for byte by both tiers: the
/// int/long field arm's screen ([`unsafe_inline_field_cell_bytes`]) keyed with
/// the "2-byte field" bit for a `short` access and reading the cell from the
/// sub-word half. On entry RAX = a non-null NON-array base, R11 = the `Unsafe`
/// offset; on a hit R11 = the field's compact displacement. Writes RCX, RDX,
/// R10, R11 and the flags.
pub fn unsafe_inline_subword_field_cell_bytes(
    cells: usize,
    epoch_addr: usize,
    width: u8,
) -> (Vec<u8>, Vec<usize>) {
    // The screen's key bit is "width 8"; here it means "a 2-byte field".
    let key_width = if width == 2 { 8 } else { 4 };
    unsafe_inline_cell_screen_bytes(cells, epoch_addr, key_width, UNSAFE_SUBWORD_FIELD_CELL_BASE)
}

/// The cell-table words both tiers bake into a sub-word CAS site, or `None`
/// (no arm: the site keeps its call): the VM's admission bit for the row
/// clear, `CRATONVM_JIT_UNSAFE_SUBWORD_RMW_INLINE=0`, the accessor family's
/// kill switch, or no instance-field cell table ([`unsafe_field_arm_words`]:
/// also an armed ZGC read barrier).
pub fn unsafe_subword_field_arm_words(
    table: &DirectHelperTable,
    rmw: UnsafeSubwordRmw,
) -> Option<(usize, usize)> {
    if !rmw.admitted(table.unsafe_inline_admitted)
        || !unsafe_subword_rmw_inline_enabled()
        || !unsafe_accessor_inline_enabled()
    {
        return None;
    }
    unsafe_field_arm_words(table)
}

// ── JIT round 21 wave 1, lane updaters: UPDATERS21-1 ─────────────────────
//
// `Unsafe.getReferenceVolatile` / `putReferenceVolatile` (and the
// `getReference{Acquire,Opaque}` / `putReference{Release,Opaque}` Java bodies,
// each one call of those in JDK 25) on a published COMPACT reference field, as
// one `MOV` load / one `XCHG` (or `MOV` for release / opaque) store in both
// tiers: `AtomicReferenceFieldUpdater.get` / `set` / `lazySet`. Book:
// `docs/feature-designs/jit-ir-optimizations.md` UPDATERS21-1.
//
// * The cell: the reference half AT-4 publishes (`hw_atomic_addr`'s compact
//   arm for `HwAtomicKind::Reference`, Generational and G1 only, never ZGC,
//   never under narrow oops): the field's declared descriptor is a reference,
//   the instance is compact and the slot is the 8-byte bare pointer at
//   `object + disp` -- exactly the bytes the natives'
//   `get/set_field_volatile(obj, slot)` read and write.
// * A get needs no barrier on these collectors: the compiled reference
//   `getfield` is the same plain load (`ir_lower::vh_const_core_bytes`'s
//   reference `Get`). `java.lang.ref.Reference.referent` never has a cell
//   (the JDK never CASes it, and the volatile thin helpers' publication
//   refuses a `Reference` subclass), so no keep-alive is skipped.
// * A put takes the gated reference-store screen first
//   (`ir_lower::vh_ref_store_gate_screen_bytes`, as the CAS arm): the SATB
//   pre-gate byte clear and (a null value, a young holder, or no old
//   objects); anything else -- and a collector with no gate plan, G1 -- is
//   the call, whose helper runs both barriers.
// * Admission: bits [`UNSAFE_REF_VOLATILE_GET_GATE_BIT`] /
//   [`UNSAFE_REF_VOLATILE_PUT_GATE_BIT`] of
//   [`DirectHelperTable::unsafe_inline_admitted`], from the thin helpers'
//   own gates (`UNSAFE_{GET,PUT}_REFERENCE_VOLATILE_GATE`).

/// `CRATONVM_JIT_UNSAFE_REF_VOLATILE_FIELD=1` (JIT round 21 wave 1, lane
/// updaters, UPDATERS21-1; opt-in, default OFF): the reference volatile
/// get / put field arm in both tiers. Off, no site is recognised and the VM
/// sets neither admission bit. Read per site at lowering, so no cache.
pub fn unsafe_ref_volatile_field_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_UNSAFE_REF_VOLATILE_FIELD")
}

/// The [`DirectHelperTable::unsafe_inline_admitted`] bit of
/// `getReferenceVolatile(Object,long)Object` (after the `boolean` / `float` /
/// `double` rows, bits 19..25).
pub const UNSAFE_REF_VOLATILE_GET_GATE_BIT: usize = 25;
/// ... and of `putReferenceVolatile(Object,long,Object)V`.
pub const UNSAFE_REF_VOLATILE_PUT_GATE_BIT: usize = 26;

/// One reference volatile accessor site of UPDATERS21-1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnsafeRefVolatileAccess {
    /// The site stores its third operand: a `putReference*`, or (with
    /// [`Self::exchange`]) a `getAndSetReference*`.
    pub is_put: bool,
    /// `putReferenceRelease` / `putReferenceOpaque`: a plain `MOV` (x86-TSO:
    /// a release store), not the `XCHG` a volatile store owes.
    pub ordered_store: bool,
    /// JIT round 23 wave 1, lane L9 (W4-L4-3's reference half,
    /// [`unsafe_ref_get_and_set_field_enabled`]): a `getAndSetReference*` --
    /// the volatile put's `XCHG`, whose previous value is the site's result.
    /// Always with `is_put` and never with `ordered_store`.
    pub exchange: bool,
}

impl UnsafeRefVolatileAccess {
    /// Its admission bit.
    pub const fn gate_bit(self) -> usize {
        if self.exchange {
            UNSAFE_REF_GET_AND_SET_GATE_BIT
        } else if self.is_put {
            UNSAFE_REF_VOLATILE_PUT_GATE_BIT
        } else {
            UNSAFE_REF_VOLATILE_GET_GATE_BIT
        }
    }

    /// Java parameters after the receiver: `(obj, offset)`, plus the value.
    pub const fn num_params(self) -> usize {
        2 + self.is_put as usize
    }

    /// Does the site return a reference (a get, or a getAndSet's previous
    /// value)? Otherwise it returns `void`.
    pub const fn returns_value(self) -> bool {
        !self.is_put || self.exchange
    }
}

/// JIT round 23 wave 1, lane L9: the [`DirectHelperTable::unsafe_inline_admitted`]
/// bit of `getAndSetReference(Object,long,Object)Object` and its `Acquire` /
/// `Release` bodies. The VM sets it when neither native the JDK body calls
/// (`getReferenceVolatile`, `compareAndSetReference`) would be refused by its
/// thin helper's gate. After the veto-only [`UNSAFE_FIELD_ARM_BAKED_BIT`] (27).
pub const UNSAFE_REF_GET_AND_SET_GATE_BIT: usize = 28;

/// `CRATONVM_JIT_UNSAFE_REF_GET_AND_SET_FIELD=0` (JIT round 23 wave 1, lane
/// L9; W4-L4-3 of `jit-r21-nio-proposals-w4-L4.md`, the reference half the
/// `int` / `long` RMW prefix never had): no reference-field arm for
/// `Unsafe.getAndSetReference*` (`AtomicReferenceFieldUpdater.getAndSet`) in
/// either tier, and the VM sets no [`UNSAFE_REF_GET_AND_SET_GATE_BIT`]: every
/// site keeps its call (the JDK's `getReferenceVolatile` +
/// `weakCompareAndSetReference` loop). Default ON (a perf lever with an
/// off-switch). The arm is UPDATERS21-1's volatile put (the reference cell
/// screen, the gated reference-store screen, one `XCHG`) keeping the
/// exchanged-out value as its result; it needs no
/// `CRATONVM_JIT_UNSAFE_REF_VOLATILE_FIELD`. Read per site at lowering.
pub fn unsafe_ref_get_and_set_field_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_UNSAFE_REF_GET_AND_SET_FIELD")
}

/// The `getAndSetReference*` descriptor.
const UNSAFE_REF_GET_AND_SET_DESC: &str =
    "(Ljava/lang/Object;JLjava/lang/Object;)Ljava/lang/Object;";

/// Is `(method_name, descriptor)` one of the `getAndSetReference*` rows of
/// `jdk/internal/misc/Unsafe` (switch aside)?
fn is_unsafe_ref_get_and_set(method_name: &str, descriptor: &str) -> bool {
    descriptor == UNSAFE_REF_GET_AND_SET_DESC
        && matches!(
            method_name,
            "getAndSetReference" | "getAndSetReferenceAcquire" | "getAndSetReferenceRelease"
        )
}

/// The tail of a getAndSet hit, after [`unsafe_ref_volatile_access_bytes`]'
/// `XCHG [RAX + R11], RDX`: `MOV RAX, RDX`, the previous value as the result.
pub const UNSAFE_REF_EXCHANGE_RESULT_BYTES: [u8; 3] = [0x48, 0x89, 0xD0];

/// Recognise an UPDATERS21-1 site; `None` under
/// `CRATONVM_JIT_UNSAFE_REF_VOLATILE_FIELD` unset. Since JIT round 23 wave 1
/// (lane L9) also the `getAndSetReference*` rows (`exchange`), under
/// [`unsafe_ref_get_and_set_field_enabled`] instead.
pub fn unsafe_inline_ref_volatile_site(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<UnsafeRefVolatileAccess> {
    const GET: &str = "(Ljava/lang/Object;J)Ljava/lang/Object;";
    const PUT: &str = "(Ljava/lang/Object;JLjava/lang/Object;)V";
    if class_name != "jdk/internal/misc/Unsafe" {
        return None;
    }
    // JIT round 23 wave 1, lane L9: the getAndSet rows, under their own
    // switch (default ON) and not under UPDATERS21-1's opt-in.
    if is_unsafe_ref_get_and_set(method_name, descriptor) {
        return unsafe_ref_get_and_set_field_enabled().then_some(UnsafeRefVolatileAccess {
            is_put: true,
            ordered_store: false,
            exchange: true,
        });
    }
    if !unsafe_ref_volatile_field_enabled() {
        // j27 wave 6, lane T1DISP20 (DISP20-8): the get rows alone for the
        // reference-array arm (`unsafe_ref_array_arm`), whose cores then
        // emit no field arm (`unsafe_ref_volatile_field_arm_words` asks
        // this switch too).
        return (crate::unsafe_ref_array_arm::unsafe_ref_array_inline_enabled()
            && descriptor == GET
            && matches!(
                method_name,
                "getReferenceVolatile" | "getReferenceAcquire" | "getReferenceOpaque"
            ))
        .then_some(UnsafeRefVolatileAccess {
            is_put: false,
            ordered_store: false,
            exchange: false,
        });
    }
    let access = |is_put: bool, ordered_store: bool| UnsafeRefVolatileAccess {
        is_put,
        ordered_store,
        exchange: false,
    };
    match (method_name, descriptor) {
        ("getReferenceVolatile" | "getReferenceAcquire" | "getReferenceOpaque", GET) => {
            Some(access(false, false))
        }
        ("putReferenceVolatile", PUT) => Some(access(true, false)),
        ("putReferenceRelease" | "putReferenceOpaque", PUT) => Some(access(true, true)),
        _ => None,
    }
}

/// The cell-table words both tiers bake into an UPDATERS21-1 site, or `None`
/// (the site keeps its call): its admission bit clear, the switch off, the
/// accessor family off, narrow oops (a compact reference field is then not
/// the 8-byte pointer the arm moves; the VM publishes no cell either), or no
/// cell table ([`unsafe_field_arm_words`]: also an armed ZGC read barrier).
pub fn unsafe_ref_volatile_field_arm_words(
    table: &DirectHelperTable,
    access: UnsafeRefVolatileAccess,
) -> Option<(usize, usize)> {
    if table.unsafe_inline_admitted & (1usize << access.gate_bit()) == 0
        || !(if access.exchange {
            unsafe_ref_get_and_set_field_enabled()
        } else {
            unsafe_ref_volatile_field_enabled()
        })
        || !unsafe_accessor_inline_enabled()
        || crate::x64::narrow_oops_block_inline_fields()
    {
        return None;
    }
    unsafe_field_arm_words(table)
}

/// The access itself, after the reference cell screen (RAX = the holder, R11
/// = the published compact displacement; for a put RDX = the value, loaded
/// after the screen, which writes RDX). A get leaves the reference in RAX:
/// `MOV RAX, [RAX + R11]`. A put: `XCHG [RAX + R11], RDX` (volatile: the
/// StoreLoad), or `MOV [RAX + R11], RDX` (release / opaque).
pub fn unsafe_ref_volatile_access_bytes(access: UnsafeRefVolatileAccess) -> [u8; 4] {
    match (access.is_put, access.ordered_store) {
        (false, _) => [0x4A, 0x8B, 0x04, 0x18],
        (true, false) => [0x4A, 0x87, 0x14, 0x18],
        (true, true) => [0x4A, 0x89, 0x14, 0x18],
    }
}

// ── JIT round 21 wave 1, lane updaters: UPDATERS21-2 ─────────────────────

/// The bit the VM folds into the word it hands
/// [`unsafe_inline_rmw_body_splice_loses_prefix`] (never into
/// [`DirectHelperTable::unsafe_inline_admitted`]) when this compile will bake
/// an `Unsafe` instance-field cell table, i.e. when the RMW prefix has a field
/// arm at all.
pub const UNSAFE_FIELD_ARM_BAKED_BIT: usize = 27;

/// `CRATONVM_JIT_UNSAFE_RMW_VETO_NEEDS_FIELD_ARM=1` (JIT round 21 wave 1,
/// lane updaters, UPDATERS21-2; opt-in, default OFF): the AT-5 splice veto
/// keeps a `getAndAdd*` / `getAndSet*` / `getAndBitwise*` Java body a call
/// only when the compile also has a field arm ([`UNSAFE_FIELD_ARM_BAKED_BIT`]).
/// Without one the prefix serves primitive-array elements alone, and a
/// field-updater site kept as a call runs the compiled Java body's thin-call
/// loop behind one more call. Trade-off, stated: under this switch with no
/// field arm an ARRAY-base Java-body site splices too and loses its one
/// `LOCK XADD`; decide by `R21UpdaterRows` `ifu-getandadd` and
/// `Nio21CasRace -Dnio21.rows=bench` `xadd lxadd`. Read at inline resolution.
pub fn unsafe_rmw_veto_needs_field_arm_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_UNSAFE_RMW_VETO_NEEDS_FIELD_ARM")
}

#[cfg(test)]
mod nio21_w5_unsafe_inline_rmw_tests {
    use super::*;

    /// JIT round 20 wave 11, lane pAtom (W6-L3-3 / AT-5): the inline
    /// resolver keeps a Java-body RMW row as a call exactly when the prefix
    /// would serve it; a CAS row, a refused mask, another class and a near-miss
    /// descriptor are left to the ordinary screens.
    #[test]
    fn a_java_body_rmw_row_is_kept_as_a_call_only_where_the_prefix_serves_it() {
        if !cfg!(target_arch = "x86_64")
            || !unsafe_rmw_inline_enabled()
            || !unsafe_bitwise_inline_enabled()
            || !unsafe_accessor_inline_enabled()
            || !crate::ir_thin_prefix_enabled()
            || !unsafe_rmw_body_no_splice_enabled()
        {
            return;
        }
        let u = "jdk/internal/misc/Unsafe";
        let all = usize::MAX;
        let veto = |m: &str, d: &str, mask: usize| {
            unsafe_inline_rmw_body_splice_loses_prefix(u, m, d, mask)
        };
        assert!(veto("getAndAddInt", "(Ljava/lang/Object;JI)I", all));
        assert!(veto("getAndAddLong", "(Ljava/lang/Object;JJ)J", all));
        assert!(veto("getAndSetInt", "(Ljava/lang/Object;JI)I", all));
        assert!(veto("getAndSetLong", "(Ljava/lang/Object;JJ)J", all));
        assert!(veto("getAndBitwiseOrInt", "(Ljava/lang/Object;JI)I", all));
        assert!(veto(
            "getAndBitwiseXorLongRelease",
            "(Ljava/lang/Object;JJ)J",
            all
        ));
        // Exactly the row's own admission: `getAndAddInt` needs bits 14 + 10.
        assert!(veto(
            "getAndAddInt",
            "(Ljava/lang/Object;JI)I",
            (1 << 14) | (1 << 10)
        ));
        assert!(!veto("getAndAddInt", "(Ljava/lang/Object;JI)I", 1 << 14));
        assert!(!veto("getAndAddInt", "(Ljava/lang/Object;JI)I", 0));
        // A CAS body is one native call whose own prefix survives the splice.
        assert!(!veto(
            "weakCompareAndSetInt",
            "(Ljava/lang/Object;JII)Z",
            all
        ));
        assert!(!veto(
            "compareAndExchangeIntAcquire",
            "(Ljava/lang/Object;JII)I",
            all
        ));
        assert!(!veto("getAndAddInt", "(Ljava/lang/Object;JJ)J", all));
        assert!(!veto(
            "getAndAddReference",
            "(Ljava/lang/Object;JLjava/lang/Object;)Ljava/lang/Object;",
            all
        ));
        assert!(!unsafe_inline_rmw_body_splice_loses_prefix(
            "sun/misc/Unsafe",
            "getAndAddInt",
            "(Ljava/lang/Object;JI)I",
            all
        ));
    }

    #[test]
    fn nio21_w5_rmw_rows_ask_the_cas_bits() {
        assert_eq!(
            UnsafeInlineAccess::GATE_BITS[14],
            ("compareAndSetInt", "(Ljava/lang/Object;JII)Z")
        );
        assert_eq!(
            UnsafeInlineAccess::GATE_BITS[15],
            ("compareAndSetLong", "(Ljava/lang/Object;JJJ)Z")
        );
        if !unsafe_rmw_inline_enabled() {
            return;
        }
        let u = "jdk/internal/misc/Unsafe";
        let cas =
            unsafe_inline_rmw_site(u, "compareAndSetInt", "(Ljava/lang/Object;JII)Z").unwrap();
        assert_eq!(
            (cas.width, cas.op, cas.num_params(), cas.ret_type()),
            (4, UnsafeRmwOp::Cas, 4, b'Z')
        );
        assert!(cas.admitted(1 << 14) && !cas.admitted(0x3FFF) && !cas.admitted(1 << 15));
        let weak = unsafe_inline_rmw_site(
            u,
            "weakCompareAndSetLongAcquire",
            "(Ljava/lang/Object;JJJ)Z",
        )
        .unwrap();
        assert_eq!((weak.width, weak.op), (8, UnsafeRmwOp::Cas));
        assert!(weak.admitted(1 << 15) && !weak.admitted(1 << 14));
        let add = unsafe_inline_rmw_site(u, "getAndAddInt", "(Ljava/lang/Object;JI)I").unwrap();
        assert_eq!((add.num_params(), add.ret_type()), (3, b'I'));
        assert!(add.admitted((1 << 14) | (1 << 10)));
        assert!(
            !add.admitted(1 << 14),
            "the getIntVolatile row is needed too"
        );
        let set = unsafe_inline_rmw_site(u, "getAndSetLong", "(Ljava/lang/Object;JJ)J").unwrap();
        assert_eq!((set.op, set.ret_type()), (UnsafeRmwOp::GetAndSet, b'J'));
        assert!(set.admitted((1 << 15) | (1 << 11)) && !set.admitted((1 << 14) | (1 << 10)));
        for (c, m, d) in [
            // nio21 wave 7 L3: `compareAndExchangeInt` is served now; its
            // reference and mismatched neighbours are not.
            (
                u,
                "compareAndExchangeReference",
                "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            (u, "compareAndExchangeInt", "(Ljava/lang/Object;JJJ)J"),
            (
                u,
                "compareAndSetReference",
                "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
            ),
            // nio21 wave 6 L3: `getAndBitwiseOrInt` is served now; its
            // narrow, mismatched and `Nand`-shaped neighbours are not.
            (u, "getAndBitwiseOrByte", "(Ljava/lang/Object;JB)B"),
            (u, "getAndBitwiseOrInt", "(Ljava/lang/Object;JJ)J"),
            (u, "getAndBitwiseNandInt", "(Ljava/lang/Object;JI)I"),
            (u, "getAndAddByte", "(Ljava/lang/Object;JB)B"),
            (u, "compareAndSetInt", "(Ljava/lang/Object;JJJ)Z"),
            (
                "sun/misc/Unsafe",
                "compareAndSwapInt",
                "(Ljava/lang/Object;JII)Z",
            ),
            ("sun/misc/Unsafe", "getAndAddInt", "(Ljava/lang/Object;JI)I"),
        ] {
            assert!(unsafe_inline_rmw_site(c, m, d).is_none(), "{c}.{m}{d}");
        }
    }

    #[test]
    fn nio21_w5_rmw_bytes_are_locked_and_widen_like_the_natives() {
        let cas4 = unsafe_inline_rmw_bytes(UnsafeInlineRmw {
            width: 4,
            op: UnsafeRmwOp::Cas,
        });
        assert_eq!(
            cas4,
            [0x49, 0x92, 0xF0, 0x43, 0x0F, 0xB1, 0x14, 0x1A, 0x0F, 0x94, 0xC0, 0x0F, 0xB6, 0xC0]
        );
        let cas8 = unsafe_inline_rmw_bytes(UnsafeInlineRmw {
            width: 8,
            op: UnsafeRmwOp::Cas,
        });
        assert_eq!(&cas8[2..8], &[0xF0, 0x4B, 0x0F, 0xB1, 0x14, 0x1A]);
        let add4 = unsafe_inline_rmw_bytes(UnsafeInlineRmw {
            width: 4,
            op: UnsafeRmwOp::GetAndAdd,
        });
        assert_eq!(add4, [0xF0, 0x42, 0x0F, 0xC1, 0x14, 0x18, 0x48, 0x63, 0xC2]);
        let add8 = unsafe_inline_rmw_bytes(UnsafeInlineRmw {
            width: 8,
            op: UnsafeRmwOp::GetAndAdd,
        });
        assert_eq!(add8, [0xF0, 0x4A, 0x0F, 0xC1, 0x14, 0x18, 0x48, 0x89, 0xD0]);
        let set4 = unsafe_inline_rmw_bytes(UnsafeInlineRmw {
            width: 4,
            op: UnsafeRmwOp::GetAndSet,
        });
        assert_eq!(set4, [0x42, 0x87, 0x14, 0x18, 0x48, 0x63, 0xC2]);
        // nio21 wave 7 L3: the alignment test only; the synthetic-band
        // decline comes back under the natives' arm-order kill switch.
        let (screen, edges) = unsafe_inline_rmw_screen_bytes(8);
        assert_eq!(&screen[..6], &[0x41, 0xF6, 0xC3, 0x07, 0x0F, 0x85]);
        if unsafe_array_before_synthetic_enabled() {
            assert_eq!(edges, [6usize]);
            assert_eq!(screen.len(), 10);
        }
        let (old, old_edges) = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_UNSAFE_ARRAY_BEFORE_SYNTHETIC", Some("0"))],
            || unsafe_inline_rmw_screen_bytes(8),
        );
        assert_eq!(&old[10..17], &[0x49, 0x81, 0xFB, 0x00, 0x00, 0x00, 0x20]);
        assert_eq!(&old[17..19], &[0x0F, 0x83]);
        assert_eq!(old_edges, [6usize, 19]);
        assert_eq!(old.len(), 23);
    }

    /// nio21 wave 7 L3 (W6-L3-2): the `compareAndExchange*` rows ask their
    /// own bit (16 / 17) and the width's CAS bit, and keep the witness.
    #[test]
    fn nio21_w7_cae_rows_keep_the_witness_behind_their_own_bits() {
        assert_eq!(
            UnsafeInlineRmw::CAE_FIRST_BIT,
            UnsafeInlineAccess::GATE_BITS.len()
        );
        let cae4 = unsafe_inline_rmw_bytes(UnsafeInlineRmw {
            width: 4,
            op: UnsafeRmwOp::Cae,
        });
        assert_eq!(
            cae4,
            [0x49, 0x92, 0xF0, 0x43, 0x0F, 0xB1, 0x14, 0x1A, 0x48, 0x63, 0xC0]
        );
        let cae8 = unsafe_inline_rmw_bytes(UnsafeInlineRmw {
            width: 8,
            op: UnsafeRmwOp::Cae,
        });
        assert_eq!(cae8, [0x49, 0x92, 0xF0, 0x4B, 0x0F, 0xB1, 0x14, 0x1A]);
        if !unsafe_rmw_inline_enabled() || !unsafe_cae_inline_enabled() {
            return;
        }
        let u = "jdk/internal/misc/Unsafe";
        for (m, d, w, ret) in [
            (
                "compareAndExchangeInt",
                "(Ljava/lang/Object;JII)I",
                4u8,
                b'I',
            ),
            (
                "compareAndExchangeIntAcquire",
                "(Ljava/lang/Object;JII)I",
                4,
                b'I',
            ),
            (
                "compareAndExchangeLongRelease",
                "(Ljava/lang/Object;JJJ)J",
                8,
                b'J',
            ),
        ] {
            let r = unsafe_inline_rmw_site(u, m, d).unwrap_or_else(|| panic!("{m}{d}"));
            assert_eq!(
                (r.width, r.op, r.num_params(), r.ret_type()),
                (w, UnsafeRmwOp::Cae, 4, ret)
            );
            assert!(r.has_expected());
            let own = 1usize << (16 + (w == 8) as usize);
            let cas = 1usize << (14 + (w == 8) as usize);
            assert!(r.admitted(own | cas), "{m}");
            assert!(!r.admitted(own), "{m}: the CAS row is needed too");
            assert!(
                !r.admitted(0xFFFF),
                "{m}: the sixteen static rows do not admit it"
            );
        }
        assert_eq!(
            UnsafeInlineRmw::CAE_GATE_BITS,
            [
                ("compareAndExchangeInt", "(Ljava/lang/Object;JII)I"),
                ("compareAndExchangeLong", "(Ljava/lang/Object;JJJ)J"),
            ]
        );
    }

    /// JIT r23 wave 9 (lane W9-L2, CF-W9-1): the two-way screen. Its first
    /// probe's three misses jump to the second probe (which reloads the table
    /// and hashes the class id with `HASH2`), its hit jumps past the second
    /// probe, and only the second probe's misses (plus the shared prefix's
    /// three) are decline edges. The documented collision has two distinct
    /// second cells.
    #[test]
    fn cf_w9_two_way_cell_screen() {
        let _two_way = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_JIT_UNSAFE_FIELD_CELL_TWO_WAY",
                None,
            )]),
        );
        assert!(unsafe_field_cell_two_way_enabled());
        // `UniApply` (667) `status` (slot 0) and `ConcurrentHashMap` (145)
        // `sizeCtl` (slot 5, after `AbstractMap`'s two) share cell 155.
        assert_eq!(unsafe_field_cell_index(667, 0, false), 155);
        assert_eq!(unsafe_field_cell_index(145, 5, false), 155);
        assert_ne!(
            unsafe_field_cell_index2(667, 0, false),
            unsafe_field_cell_index2(145, 5, false)
        );
        assert_eq!(
            unsafe_field_cell_index2(667, 0, false),
            ((667u32.wrapping_mul(UNSAFE_FIELD_CELL_HASH2) >> 24) as usize) & 255
        );
        let cells = 0x1122_3344_5566usize;
        for width in [4u8, 8] {
            let (b, e) = unsafe_inline_field_cell_bytes(cells, 0x7788_99AA_BBCC, width);
            assert_eq!(e.len(), 6, "3 prefix + 3 second-probe declines");
            for &at in &e {
                assert_eq!(b[at - 2], 0x0F, "a rel32 Jcc at {at}");
                assert_eq!(&b[at..at + 4], &[0, 0, 0, 0], "decline edges are left for the caller");
            }
            let has = |n: &[u8]| b.windows(n.len()).position(|w| w == n);
            let load = [0x4F, 0x8B, 0x5C, 0xDA, 0x08];
            let first = has(&load).expect("first cell load");
            let second_load = b[first + 1..]
                .windows(5)
                .position(|w| w == load)
                .map(|p| p + first + 1)
                .expect("second cell load");
            let mut imul = vec![0x44, 0x69, 0xD9];
            imul.extend_from_slice(&UNSAFE_FIELD_CELL_HASH2.to_le_bytes());
            let at_imul = has(&imul).expect("IMUL R11D, ECX, HASH2");
            assert!(first < at_imul && at_imul < second_load);
            // The reload of the table sits right before the IMUL.
            let mut reload = vec![0x49, 0xBA];
            reload.extend_from_slice(&(cells as u64).to_le_bytes());
            assert_eq!(&b[at_imul - 10..at_imul], &reload[..]);
            let second = at_imul - 10;
            // Every internal Jcc rel32 between the first load and the second
            // probe targets the second probe; the JMP before it targets the end.
            let jmp = second - 5;
            assert_eq!(b[jmp], 0xE9);
            let rel = |at: usize| i32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as i64;
            assert_eq!(jmp as i64 + 5 + rel(jmp + 1), b.len() as i64, "hit jumps to the end");
            let mut internal = 0;
            let mut i = first;
            while i + 6 <= jmp {
                if b[i] == 0x0F && matches!(b[i + 1], 0x84 | 0x85) {
                    assert_eq!(i as i64 + 6 + rel(i + 2), second as i64, "miss -> second probe");
                    internal += 1;
                    i += 6;
                } else {
                    i += 1;
                }
            }
            assert_eq!(internal, 3, "class, key and empty-cell misses");
            assert!(b.ends_with(&[0x45, 0x0F, 0xB7, 0xDB, 0x45, 0x85, 0xDB, 0x0F, 0x84, 0, 0, 0, 0]));
        }
    }

    /// nio21 wave 7 L3 (W6-L3-1): the cell word, its index and the field
    /// arm's bytes agree with each other.
    #[test]
    fn nio21_w7_field_cell_word_index_and_bytes() {
        // JIT r23 wave 9 (CF-W9-1): this pins the ONE-way screen's bytes; the
        // two-way screen has its own test (`cf_w9_two_way_cell_screen`).
        let _one_way = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_JIT_UNSAFE_FIELD_CELL_TWO_WAY",
                Some("0"),
            )]),
        );
        assert_eq!(
            unsafe_field_cell_word(7, 3, false, 12),
            Some((7u64 << 32) | (6 << 16) | 12)
        );
        assert_eq!(
            unsafe_field_cell_word(7, 3, true, 16),
            Some((7u64 << 32) | (7 << 16) | 16)
        );
        assert_eq!(
            unsafe_field_cell_word(7, 0x8000, false, 12),
            None,
            "slot past the key"
        );
        assert_eq!(
            unsafe_field_cell_word(7, 3, false, 0),
            None,
            "disp 0 is the empty cell"
        );
        assert_eq!(
            unsafe_field_cell_word(7, 3, false, 0x1_0000),
            None,
            "disp past u16"
        );
        assert_eq!(unsafe_field_cell_index(7, 3, false), (7 ^ 6) & 255);
        assert_eq!(
            unsafe_field_cell_index(0x1234, 0x7FFF, true),
            (0x1234 ^ 0xFFFF) & 255
        );
        let (b4, e4) = unsafe_inline_field_cell_bytes(0x1122_3344_5566, 0x7788_99AA_BBCC, 4);
        let (b8, e8) = unsafe_inline_field_cell_bytes(0x1122_3344_5566, 0x7788_99AA_BBCC, 8);
        assert_eq!(e4.len(), 6);
        assert_eq!(
            b8.len(),
            b4.len() + 3,
            "only the long key's OR EDX, 1 differs"
        );
        for (b, e) in [(&b4, &e4), (&b8, &e8)] {
            for &at in e.iter() {
                assert_eq!(b[at - 2], 0x0F, "a rel32 Jcc at {at}");
                assert_eq!(&b[at..at + 4], &[0, 0, 0, 0]);
            }
        }
        let has = |b: &[u8], n: &[u8]| b.windows(n.len()).any(|w| w == n);
        assert!(
            has(&b4, &[0x49, 0xBA, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0, 0]),
            "MOV R10, cells"
        );
        assert!(
            has(&b4, &[0x48, 0xB9, 0xCC, 0xBB, 0xAA, 0x99, 0x88, 0x77, 0, 0]),
            "MOV RCX, epoch"
        );
        assert!(has(&b4, &[0x41, 0x3B, 0x0A]), "CMP ECX, [R10]");
        assert!(has(
            &b4,
            &[
                0xF6,
                0x40,
                cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8,
                cratonvm_types::GC_FLAG_COMPACT
            ]
        ));
        assert!(
            has(&b4, &[0x4F, 0x8B, 0x5C, 0xDA, 0x08]),
            "MOV R11, [R10+R11*8+8]"
        );
        assert!(
            has(&b8, &[0x43, 0x8D, 0x14, 0x1B, 0x83, 0xCA, 0x01]),
            "the long key"
        );
        assert!(!has(&b4, &[0x83, 0xCA, 0x01]));
        assert!(b4.ends_with(&[0x45, 0x0F, 0xB7, 0xDB, 0x45, 0x85, 0xDB, 0x0F, 0x84, 0, 0, 0, 0]));
        let (kind, edge) = unsafe_inline_not_array_bytes();
        assert_eq!(
            &kind[..4],
            &[
                0x0F,
                0xB6,
                0x48,
                cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8
            ]
        );
        assert_eq!(&kind[edge - 2..edge], &[0x0F, 0x85]);
        assert_eq!(edge + 4, kind.len());
        assert_eq!(DirectHelperTable::EMPTY.unsafe_field_cells, 0);
    }

    /// nio21 wave 6 L3 (W5-L3-2): the `getAndBitwise*` rows and their loop.
    #[test]
    fn nio21_w6_bitwise_rows_and_bytes() {
        if !unsafe_rmw_inline_enabled() || !unsafe_bitwise_inline_enabled() {
            return;
        }
        let u = "jdk/internal/misc/Unsafe";
        let or =
            unsafe_inline_rmw_site(u, "getAndBitwiseOrInt", "(Ljava/lang/Object;JI)I").unwrap();
        assert_eq!(
            (or.width, or.op),
            (4, UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::Or))
        );
        assert_eq!((or.num_params(), or.ret_type()), (3, b'I'));
        assert!(or.admitted((1 << 14) | (1 << 10) | (1 << 3)));
        assert!(
            !or.admitted((1 << 14) | (1 << 10)),
            "the Release body's getInt row"
        );
        let xr =
            unsafe_inline_rmw_site(u, "getAndBitwiseXorLongRelease", "(Ljava/lang/Object;JJ)J")
                .unwrap();
        assert_eq!(
            (xr.width, xr.op, xr.ret_type()),
            (8, UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::Xor), b'J')
        );
        let and4 = unsafe_inline_rmw_bytes(UnsafeInlineRmw {
            width: 4,
            op: UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::And),
        });
        assert_eq!(
            and4,
            [
                0x49, 0x89, 0xC2, // MOV R10, RAX
                0x43, 0x8B, 0x04, 0x1A, // MOV EAX, [R10+R11]
                0x89, 0xC1, // MOV ECX, EAX
                0x21, 0xD1, // AND ECX, EDX
                0xF0, 0x43, 0x0F, 0xB1, 0x0C, 0x1A, // LOCK CMPXCHG [R10+R11], ECX
                0x75, 0xF4, // JNE -12
                0x48, 0x63, 0xC0, // MOVSXD RAX, EAX
            ]
        );
        let or8 = unsafe_inline_rmw_bytes(UnsafeInlineRmw {
            width: 8,
            op: UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::Or),
        });
        assert_eq!(&or8[7..13], &[0x48, 0x89, 0xC1, 0x48, 0x09, 0xD1]);
        assert_eq!(&or8[19..], &[0x75, 0xF2], "JNE -14, no widening");
    }
}

#[cfg(test)]
mod unsafe_inline_access_tests {
    use super::*;

    #[test]
    fn plain_rows_name_their_own_gate_bit() {
        // Rows 14 and 15 (nio21 wave 5 L3) are the CAS gates the RMW rows
        // ask; no accessor names them (`nio21_w5_rmw_rows_ask_the_cas_bits`).
        for (bit, (name, desc)) in UnsafeInlineAccess::GATE_BITS.iter().enumerate().take(14) {
            let a = unsafe_inline_access_site("jdk/internal/misc/Unsafe", name, desc)
                .unwrap_or_else(|| panic!("{name}{desc} must be recognised"));
            assert!(!a.unaligned);
            assert_eq!(a.plain_bit(), 1usize << bit, "{name}{desc}");
            assert!(a.admitted(1usize << bit));
            assert!(!a.admitted(!(1usize << bit) & 0x3FF));
        }
    }

    #[test]
    fn unaligned_forms_need_every_accessor_of_their_direction() {
        let g = unsafe_inline_access_site(
            "jdk/internal/misc/Unsafe",
            "getIntUnaligned",
            "(Ljava/lang/Object;JZ)I",
        )
        .unwrap();
        assert!(g.unaligned && g.big_endian_operand && !g.is_put && g.width == 4);
        assert_eq!(g.num_params(), 3);
        assert!(g.admitted(0b11111));
        assert!(!g.admitted(0b11011));
        let p = unsafe_inline_access_site(
            "jdk/internal/misc/Unsafe",
            "putLongUnaligned",
            "(Ljava/lang/Object;JJZ)V",
        )
        .unwrap();
        assert!(p.unaligned && p.big_endian_operand && p.is_put && p.width == 8);
        assert_eq!(p.num_params(), 4);
        assert!(p.admitted(0b11111 << 5));
        assert!(!p.admitted(0b11111));
    }

    /// `ir_lower`'s inline prefix scales the array length by
    /// `element_type & 3` and accepts the tag bytes 0x11..=0x2D as "primitive
    /// array"; both are layout facts, pinned here against the layout's own
    /// table.
    #[test]
    fn element_tag_low_bits_are_log2_of_the_element_size() {
        use cratonvm_types::{element_byte_size, ArrayElementType as E, ObjectKind};
        for et in [
            E::Boolean,
            E::Char,
            E::Float,
            E::Double,
            E::Byte,
            E::Short,
            E::Int,
            E::Long,
        ] {
            let tag = et as u8;
            assert_eq!(1usize << (tag & 3), element_byte_size(et), "{et:?}");
            let byte = (ObjectKind::Array as u8) | (tag << 2);
            assert!((0x11..=0x2D).contains(&byte), "{et:?} tag byte {byte:#x}");
        }
        let reference = (ObjectKind::Array as u8) | ((E::Reference as u8) << 2);
        assert!(reference < 0x11);
    }

    #[test]
    fn adjacent_shapes_are_not_recognised() {
        // JIT round 20 wave 12, lane qUnsafe (L2-2): these four are rows now,
        // unless `CRATONVM_JIT_UNSAFE_QU_EXTRA_ROWS=0`.
        let extra = unsafe_qu_extra_rows_enabled();
        let vol = extra && unsafe_volatile_inline_enabled();
        for (m, d, want) in [
            ("getBoolean", "(Ljava/lang/Object;J)Z", extra),
            ("getFloat", "(Ljava/lang/Object;J)F", extra),
            ("getIntAcquire", "(Ljava/lang/Object;J)I", vol),
            ("putLongRelease", "(Ljava/lang/Object;JJ)V", vol),
        ] {
            assert_eq!(
                unsafe_inline_access_site("jdk/internal/misc/Unsafe", m, d).is_some(),
                want,
                "{m}{d}"
            );
        }
        for (c, m, d) in [
            (
                "jdk/internal/misc/Unsafe",
                "getBooleanVolatile",
                "(Ljava/lang/Object;J)Z",
            ),
            (
                "jdk/internal/misc/Unsafe",
                "getFloatVolatile",
                "(Ljava/lang/Object;J)F",
            ),
            (
                "jdk/internal/misc/Unsafe",
                "putDoubleRelease",
                "(Ljava/lang/Object;JD)V",
            ),
            (
                "jdk/internal/misc/Unsafe",
                "getShortVolatile",
                "(Ljava/lang/Object;J)S",
            ),
            (
                "jdk/internal/misc/Unsafe",
                "getReferenceVolatile",
                "(Ljava/lang/Object;J)Ljava/lang/Object;",
            ),
            (
                "jdk/internal/misc/Unsafe",
                "compareAndSetInt",
                "(Ljava/lang/Object;JII)Z",
            ),
            ("jdk/internal/misc/Unsafe", "getInt", "(J)I"),
            (
                "jdk/internal/misc/Unsafe",
                "getReference",
                "(Ljava/lang/Object;J)Ljava/lang/Object;",
            ),
            ("sun/misc/Unsafe", "getInt", "(Ljava/lang/Object;J)I"),
            (
                "sun/misc/Unsafe",
                "getIntVolatile",
                "(Ljava/lang/Object;J)I",
            ),
            (
                "jdk/internal/misc/ScopedMemoryAccess",
                "getIntUnaligned",
                "(Ljava/lang/Object;JZ)I",
            ),
        ] {
            assert!(unsafe_inline_access_site(c, m, d).is_none(), "{c}.{m}{d}");
        }
        assert_eq!(DirectHelperTable::EMPTY.unsafe_inline_admitted, 0);
    }

    /// nio21 wave 4 L4: the four volatile rows are recognised as aligned-only
    /// volatile accesses of their width and direction, own bits 10..14, and
    /// are not admitted by a mask of the plain accessors' bits.
    #[test]
    fn nio21_w4_volatile_rows_own_bits_ten_to_fourteen() {
        let rows = [
            (
                "getIntVolatile",
                "(Ljava/lang/Object;J)I",
                4u8,
                false,
                10usize,
            ),
            ("getLongVolatile", "(Ljava/lang/Object;J)J", 8, false, 11),
            ("putIntVolatile", "(Ljava/lang/Object;JI)V", 4, true, 12),
            ("putLongVolatile", "(Ljava/lang/Object;JJ)V", 8, true, 13),
        ];
        for (m, d, width, is_put, bit) in rows {
            let a = unsafe_inline_access_site("jdk/internal/misc/Unsafe", m, d)
                .unwrap_or_else(|| panic!("{m}{d} must be recognised"));
            assert!(a.volatile && !a.unaligned && !a.big_endian_operand, "{m}");
            assert_eq!((a.width, a.is_put), (width, is_put), "{m}");
            assert_eq!(a.num_params(), 2 + is_put as usize, "{m}");
            assert_eq!(UnsafeInlineAccess::GATE_BITS[bit], (m, d));
            assert!(a.admitted(1 << bit), "{m}");
            assert!(
                !a.admitted(0x3FF),
                "{m}: the plain rows' bits must not admit it"
            );
        }
        // The plain rows are not volatile.
        let p = unsafe_inline_access_site(
            "jdk/internal/misc/Unsafe",
            "putInt",
            "(Ljava/lang/Object;JI)V",
        )
        .unwrap();
        assert!(!p.volatile);
    }
}

/// JIT round 20 wave 12, lane qUnsafe: the L2-2 rows and the AT-4 reference
/// cell screen.
#[cfg(test)]
mod r20_qunsafe_tests {
    use super::*;

    const U: &str = "jdk/internal/misc/Unsafe";

    #[test]
    fn r20_qu_extra_rows_own_bits_nineteen_to_twenty_five() {
        if !unsafe_qu_extra_rows_enabled() {
            return;
        }
        for (i, (m, d)) in UNSAFE_QU_EXTRA_GATE_BITS.iter().enumerate() {
            let a = unsafe_inline_access_site(U, m, d).unwrap_or_else(|| panic!("{m}{d}"));
            let bit = 1usize << (UNSAFE_QU_EXTRA_FIRST_BIT + i);
            assert_eq!(a.plain_bit(), bit, "{m}");
            assert!(a.admitted(bit) && !a.admitted(0xFFFF), "{m}");
            assert!(a.heap_only() && !a.volatile && !a.ordered_store, "{m}");
            assert_eq!(a.is_put, i % 2 == 1, "{m}");
            assert_eq!(a.num_params(), 2 + (i % 2), "{m}");
        }
        let b = unsafe_inline_access_site(U, "getBoolean", "(Ljava/lang/Object;J)Z").unwrap();
        assert_eq!((b.width, b.value_kind, b.signed), (1, b'Z', true));
        let f = unsafe_inline_access_site(U, "getFloat", "(Ljava/lang/Object;J)F").unwrap();
        assert!(f.is_fp() && f.width == 4);
        let d = unsafe_inline_access_site(U, "putDouble", "(Ljava/lang/Object;JD)V").unwrap();
        assert!(d.is_fp() && d.width == 8 && d.is_put);
        // The extra bits sit past the CAE rows and the reference CAS row.
        assert_eq!(
            UNSAFE_QU_EXTRA_FIRST_BIT,
            UnsafeInlineRmw::CAE_FIRST_BIT + 3
        );
        assert_eq!(UNSAFE_REF_CAS_GATE_BIT, UnsafeInlineRmw::CAE_FIRST_BIT + 2);
    }

    #[test]
    fn r20_qu_ordered_rows_are_the_volatile_rows_with_a_plain_release_store() {
        if !unsafe_qu_extra_rows_enabled() || !unsafe_volatile_inline_enabled() {
            return;
        }
        for (m, d, vm, width, put) in [
            (
                "getIntAcquire",
                "(Ljava/lang/Object;J)I",
                "getIntVolatile",
                4u8,
                false,
            ),
            (
                "getIntOpaque",
                "(Ljava/lang/Object;J)I",
                "getIntVolatile",
                4,
                false,
            ),
            (
                "getLongAcquire",
                "(Ljava/lang/Object;J)J",
                "getLongVolatile",
                8,
                false,
            ),
            (
                "getLongOpaque",
                "(Ljava/lang/Object;J)J",
                "getLongVolatile",
                8,
                false,
            ),
            (
                "putIntRelease",
                "(Ljava/lang/Object;JI)V",
                "putIntVolatile",
                4,
                true,
            ),
            (
                "putIntOpaque",
                "(Ljava/lang/Object;JI)V",
                "putIntVolatile",
                4,
                true,
            ),
            (
                "putLongRelease",
                "(Ljava/lang/Object;JJ)V",
                "putLongVolatile",
                8,
                true,
            ),
            (
                "putLongOpaque",
                "(Ljava/lang/Object;JJ)V",
                "putLongVolatile",
                8,
                true,
            ),
        ] {
            let a = unsafe_inline_access_site(U, m, d).unwrap_or_else(|| panic!("{m}"));
            let v = unsafe_inline_access_site(U, vm, d).unwrap();
            assert_eq!((a.width, a.is_put, a.volatile), (width, put, true), "{m}");
            assert_eq!(a.ordered_store, put, "{m}");
            assert!(!v.ordered_store, "{vm}: a volatile store stays an XCHG");
            assert_eq!(a.plain_bit(), v.plain_bit(), "{m} asks {vm}'s bit");
        }
    }

    #[test]
    fn r20_qu_ref_cas_sites_and_the_reference_cell_load() {
        // JIT r23 wave 9 (CF-W9-1): the byte comparison below is of the
        // ONE-way screens (the two-way screen loads a cell twice).
        let _one_way = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "CRATONVM_JIT_UNSAFE_FIELD_CELL_TWO_WAY",
                Some("0"),
            )]),
        );
        let d = "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z";
        let on = unsafe_qu_ref_cas_field_enabled();
        assert_eq!(
            unsafe_inline_ref_cas_site(U, "compareAndSetReference", d),
            on
        );
        assert_eq!(
            unsafe_inline_ref_cas_site(U, "weakCompareAndSetReferenceAcquire", d),
            on
        );
        assert!(!unsafe_inline_ref_cas_site(
            U,
            "compareAndExchangeReference",
            d
        ));
        assert!(!unsafe_inline_ref_cas_site(
            "sun/misc/Unsafe",
            "compareAndSetReference",
            d
        ));
        assert!(!unsafe_inline_ref_cas_site(
            U,
            "compareAndSetReference",
            "(Ljava/lang/Object;JII)Z"
        ));
        // The int/long screen is unchanged; the reference screen differs only
        // in its cell load: `[R10 + R11*8 + 2056]` instead of `+ 8`.
        let (long_b, long_e) = unsafe_inline_field_cell_bytes(0x1000, 0x2000, 8);
        let (ref_b, ref_e) = unsafe_inline_ref_field_cell_bytes(0x1000, 0x2000);
        let near = [0x4F, 0x8B, 0x5C, 0xDA, 0x08];
        let at = long_b
            .windows(5)
            .position(|w| w == near)
            .expect("the int/long cell load");
        let mut far = vec![0x4F, 0x8B, 0x9C, 0xDA];
        far.extend_from_slice(&UNSAFE_REF_FIELD_CELL_BASE.to_le_bytes());
        assert_eq!(
            UNSAFE_REF_FIELD_CELL_BASE,
            8 + 8 * UNSAFE_FIELD_CELL_COUNT as i32
        );
        assert_eq!(&ref_b[..at], &long_b[..at]);
        assert_eq!(&ref_b[at..at + 8], &far[..]);
        assert_eq!(&ref_b[at + 8..], &long_b[at + 5..]);
        assert_eq!(ref_e.len(), long_e.len());
        // No table, no admission bit: no arm.
        assert!(unsafe_ref_cas_field_arm_words(&DirectHelperTable::EMPTY).is_none());
        let unadmitted = DirectHelperTable {
            unsafe_field_cells: 0x10_0000,
            ..DirectHelperTable::EMPTY
        };
        assert!(unsafe_ref_cas_field_arm_words(&unadmitted).is_none());
    }
}

#[cfg(test)]
mod nio21_volatile_op_tests {
    use super::*;

    const U: &str = "jdk/internal/misc/Unsafe";

    /// Every nio21 wave-3 row (lane L4) is recognised with the shape its
    /// helper's signature row declares.
    #[test]
    fn nio21_rows_are_recognised_with_their_shapes() {
        let rows: [(&str, &str, &str, UnsafeAccessorOp, usize, u8); 9] = [
            (
                U,
                "getIntVolatile",
                "(Ljava/lang/Object;J)I",
                UnsafeAccessorOp::GetIntVolatile,
                2,
                b'I',
            ),
            (
                U,
                "getLongVolatile",
                "(Ljava/lang/Object;J)J",
                UnsafeAccessorOp::GetLongVolatile,
                2,
                b'J',
            ),
            (
                U,
                "putIntVolatile",
                "(Ljava/lang/Object;JI)V",
                UnsafeAccessorOp::PutIntVolatile,
                3,
                b'V',
            ),
            (
                U,
                "putLongVolatile",
                "(Ljava/lang/Object;JJ)V",
                UnsafeAccessorOp::PutLongVolatile,
                3,
                b'V',
            ),
            (
                U,
                "putReferenceVolatile",
                "(Ljava/lang/Object;JLjava/lang/Object;)V",
                UnsafeAccessorOp::PutReferenceVolatile,
                3,
                b'V',
            ),
            (
                U,
                "compareAndSetReference",
                "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
                UnsafeAccessorOp::CasReference,
                4,
                b'Z',
            ),
            (U, "fullFence", "()V", UnsafeAccessorOp::FullFence, 0, b'V'),
            (
                "java/lang/ref/Reference",
                "refersTo0",
                "(Ljava/lang/Object;)Z",
                UnsafeAccessorOp::RefersTo0,
                1,
                b'Z',
            ),
            (
                "java/lang/ref/PhantomReference",
                "refersTo0",
                "(Ljava/lang/Object;)Z",
                UnsafeAccessorOp::PhantomRefersTo0,
                1,
                b'Z',
            ),
        ];
        for (class, name, desc, op, params, ret) in rows {
            assert_eq!(
                is_unsafe_accessor_op(class, name, desc),
                Some(op),
                "{class}.{name}{desc}"
            );
            assert_eq!(op.num_params(), params, "{op:?}");
            assert_eq!(op.return_type(), ret, "{op:?}");
            assert!(op.is_nio21_row(), "{op:?}");
            assert_eq!(op.is_cas(), op == UnsafeAccessorOp::CasReference, "{op:?}");
            assert_eq!(
                op.is_refers_to(),
                matches!(
                    op,
                    UnsafeAccessorOp::RefersTo0 | UnsafeAccessorOp::PhantomRefersTo0
                ),
                "{op:?}"
            );
        }
    }

    /// Each row reads its OWN table slot: a declined `PhantomReference` call
    /// must not dispatch as `Reference`'s, nor a `putLongVolatile` as a get.
    #[test]
    fn every_nio21_row_names_its_own_table_slot() {
        let table = DirectHelperTable {
            unsafe_get_int_volatile: 0x10,
            unsafe_get_long_volatile: 0x18,
            unsafe_put_int_volatile: 0x20,
            unsafe_put_long_volatile: 0x28,
            unsafe_put_reference_volatile: 0x30,
            unsafe_cas_reference: 0x38,
            unsafe_full_fence: 0x40,
            reference_refers_to0: 0x48,
            phantom_reference_refers_to0: 0x50,
            ..DirectHelperTable::EMPTY
        };
        let ops = [
            UnsafeAccessorOp::GetIntVolatile,
            UnsafeAccessorOp::GetLongVolatile,
            UnsafeAccessorOp::PutIntVolatile,
            UnsafeAccessorOp::PutLongVolatile,
            UnsafeAccessorOp::PutReferenceVolatile,
            UnsafeAccessorOp::CasReference,
            UnsafeAccessorOp::FullFence,
            UnsafeAccessorOp::RefersTo0,
            UnsafeAccessorOp::PhantomRefersTo0,
        ];
        let mut seen: Vec<usize> = ops.iter().map(|op| op.helper_entry(&table)).collect();
        assert!(seen.iter().all(|&e| e != 0));
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), ops.len());
        for op in ops {
            assert_eq!(
                op.helper_entry(&DirectHelperTable::EMPTY),
                0,
                "{op:?} is wired in EMPTY"
            );
        }
    }

    /// The Java-bodied neighbours stay unrecognised: binding one would shadow
    /// real class bytes.
    #[test]
    fn java_bodied_neighbours_are_not_recognised() {
        for (class, name, desc) in [
            (
                U,
                "putReferenceRelease",
                "(Ljava/lang/Object;JLjava/lang/Object;)V",
            ),
            (
                U,
                "putReferenceOpaque",
                "(Ljava/lang/Object;JLjava/lang/Object;)V",
            ),
            (U, "getLongAcquire", "(Ljava/lang/Object;J)J"),
            (U, "putLongRelease", "(Ljava/lang/Object;JJ)V"),
            (U, "getAndAddLong", "(Ljava/lang/Object;JJ)J"),
            (U, "weakCompareAndSetLong", "(Ljava/lang/Object;JJJ)Z"),
            (U, "loadFence", "()V"),
            (
                "sun/misc/Unsafe",
                "getLongVolatile",
                "(Ljava/lang/Object;J)J",
            ),
            (
                "java/lang/ref/Reference",
                "refersTo",
                "(Ljava/lang/Object;)Z",
            ),
            (
                "java/lang/ref/WeakReference",
                "refersTo0",
                "(Ljava/lang/Object;)Z",
            ),
            (
                "java/lang/ref/Reference",
                "refersTo0",
                "(Ljava/lang/Object;)I",
            ),
        ] {
            assert_eq!(
                is_unsafe_accessor_op(class, name, desc),
                None,
                "{class}.{name}{desc}"
            );
        }
    }
}

#[cfg(test)]
mod direct_helper_abi_tests {
    use super::*;

    /// Two rows naming the same slot would make [`direct_helper_sig`] answer
    /// with whichever came first, so an assertion at a call site could be
    /// silently checking the wrong helper's arity.
    #[test]
    fn every_signature_row_names_a_distinct_slot() {
        let mut seen: Vec<&str> = Vec::new();
        for sig in DIRECT_HELPER_FN_SIGS {
            assert!(
                !seen.contains(&sig.field),
                "two `direct_helper_fn_slots!` rows name the slot `{}`",
                sig.field,
            );
            seen.push(sig.field);
        }
    }

    /// The six hand-written slots are the ones with no stack-argument path
    /// anywhere, so the set is worth naming rather than only counting: a
    /// seventh member would be a helper the emitter calls with registers it
    /// never loads, and the census assertion alone cannot say which one is new.
    /// (`field_site_cp` joined in interpreter round i4 wave 92, lane L2.)
    #[test]
    fn the_hand_written_call_path_has_exactly_the_six_known_members() {
        let mut hand: Vec<&str> = DIRECT_HELPER_FN_SIGS
            .iter()
            .filter(|s| matches!(s.path, DirectHelperCallPath::HandWritten))
            .map(|s| s.field)
            .collect();
        hand.sort_unstable();
        assert_eq!(
            hand,
            [
                "arm_savebase_watch",
                "disarm_savebase_watch",
                "field_site_cp",
                "monitor_enter",
                "monitor_exit",
                "sb_grow",
            ],
            "the set of direct helpers reached by a hand-written call sequence \
             changed. Every member must fit Win64's four integer argument \
             registers, because that route has no shadow-space stack-argument \
             marshalling at all.",
        );
    }

    /// The nine slots that overflow the Win64 register file must ALL be on the
    /// marshalled path. The count assertion beside the table would be
    /// satisfied by trading a marshalled overflow for a hand-written one; this
    /// is the check that would not be.
    #[test]
    fn every_slot_that_overflows_the_register_file_is_marshalled() {
        for sig in DIRECT_HELPER_FN_SIGS {
            if sig.win64_stack_args() > 0 {
                assert!(
                    matches!(sig.path, DirectHelperCallPath::JavaArgMarshalled),
                    "`{}` takes {} arguments but is not reached through \
                     JitDirectCall's `emit_stack_arg_setup`",
                    sig.field,
                    sig.arity,
                );
            }
        }
    }

    /// Every slot reached through the `JitDirectCall` sequence returns a word.
    ///
    /// That sequence (`x64/op_invoke.rs`, both the invokestatic and the
    /// instance arm) ends in `emit_post_invoke_exception_check(ret_type)`,
    /// which compares RAX against `i64::MIN` for a `V` site too. A `-> ()`
    /// helper leaves RAX undefined there, and one whose cold arm dispatches a
    /// native that throws loses the exception: `varhandle_write` did exactly
    /// that, and a `ClassCastException` from the native escaped the compiled
    /// caller's own `catch`
    /// (r11w7-orch-jit-varhandle-native-exception-escapes-the-local-handler).
    #[test]
    fn every_java_arg_marshalled_slot_returns_a_word() {
        for sig in DIRECT_HELPER_FN_SIGS {
            if matches!(sig.path, DirectHelperCallPath::JavaArgMarshalled) {
                assert!(
                    sig.returns_value,
                    "`{}` is reached through JitDirectCall but is declared `-> ()`; \
                     the post-call sentinel compare would read an undefined RAX",
                    sig.field,
                );
            }
        }
        assert!(direct_helper_sig_of("varhandle_write").returns_value);
        assert!(direct_helper_sig_of("reachability_fence").returns_value);
    }

    /// `varhandle_cas` is the slot the review nominated for the Win64
    /// stack-argument census, so pin what the answer actually is: five words,
    /// one over the register file, and legal precisely because it is NOT on
    /// the hand-written route. Moving it there without adding marshalling
    /// would pass an uninitialised fifth argument on Windows.
    #[test]
    fn varhandle_cas_overflows_win64_and_is_marshalled_not_hand_written() {
        let sig = direct_helper_sig_of("varhandle_cas");
        assert_eq!(sig.arity, 5);
        assert_eq!(sig.win64_stack_args(), 1);
        assert_eq!(sig.sysv_stack_args(), 0);
        assert_eq!(sig.path, DirectHelperCallPath::JavaArgMarshalled);
    }

    /// The `ScopedMemoryAccess` write accessors take seven words and so spill
    /// on SysV too — the only slots in either helper table that do. Nothing
    /// said so anywhere before this.
    #[test]
    fn the_scoped_memory_write_accessors_spill_on_both_abis() {
        for field in [
            "scoped_memory_put_int",
            "scoped_memory_put_short",
            "scoped_memory_put_long",
            "scoped_memory_put_char",
        ] {
            let sig = direct_helper_sig_of(field);
            assert_eq!(sig.arity, 7, "{field}");
            assert_eq!(sig.sysv_stack_args(), 1, "{field}");
            assert_eq!(sig.win64_stack_args(), 3, "{field}");
        }
    }

    /// `monitor_exit` returns `1`, not the object — which is why a shared
    /// enter/exit emitter must not store RAX back. Same declared shape as
    /// `monitor_enter`, which is what makes sharing the emitter legal.
    #[test]
    fn the_two_monitor_helpers_share_one_declared_shape() {
        let enter = direct_helper_sig_of("monitor_enter");
        let exit = direct_helper_sig_of("monitor_exit");
        assert_eq!(
            (enter.arity, enter.returns_value, enter.path),
            (exit.arity, exit.returns_value, exit.path),
            "`runtime_lowering::emit_monitor_stub` emits ONE sequence for both; \
             a divergence here makes one of the two call sites wrong",
        );
    }

    /// An unknown field name must fail the build, not quietly assert nothing.
    /// The `const fn` half of that cannot be tested (it would not compile), so
    /// this pins the `None` arm it is built on.
    #[test]
    fn an_unknown_slot_name_resolves_to_nothing() {
        assert!(direct_helper_sig("monitor_entre").is_none());
        assert!(direct_helper_sig("").is_none());
        assert!(direct_helper_sig("monitor_enter").is_some());
    }

    /// The compiler-only predicates are deliberately absent from the signature
    /// table: their arguments are a `u32` and a `bool`, which the
    /// machine-word rule would (correctly) reject. Their convention is pinned
    /// by the `extern "C"` aliases the transmute sites name instead.
    #[test]
    fn the_compiler_only_predicates_are_not_baked_call_targets() {
        for field in [
            "buffer_session_served_class",
            "nio_byte_element_served_class",
            "static_base_resolver",
            "class_initialized",
            "well_known_class_id",
        ] {
            assert!(
                direct_helper_sig(field).is_none(),
                "{field} must not be described as a baked callable",
            );
        }
    }

    /// `integer_class_id` answers only with BOTH the resolver and the VM
    /// context wired, passes the `Integer` selector, and never answers class
    /// id 0 (`java/lang/Object`) or a value outside `u32`: each of those would
    /// be a receiver guard that some non-`Integer` object passes.
    #[test]
    fn the_integer_class_id_needs_the_resolver_the_vm_and_a_real_id() {
        unsafe extern "C" fn answers_42(ctx: i64, which: i64) -> i64 {
            if ctx == 0x1000 && which == WELL_KNOWN_CLASS_INTEGER {
                42
            } else {
                0
            }
        }
        unsafe extern "C" fn answers_zero(_ctx: i64, _which: i64) -> i64 {
            0
        }
        unsafe extern "C" fn answers_too_wide(_ctx: i64, _which: i64) -> i64 {
            i64::from(u32::MAX) + 1
        }
        let wired = DirectHelperTable {
            well_known_class_id: answers_42 as *const () as usize,
            static_base_resolver_ctx: 0x1000,
            ..DirectHelperTable::EMPTY
        };
        assert_eq!(wired.integer_class_id(), Some(42));
        let no_vm = DirectHelperTable {
            static_base_resolver_ctx: 0,
            ..wired
        };
        assert_eq!(no_vm.integer_class_id(), None);
        let no_resolver = DirectHelperTable {
            well_known_class_id: 0,
            ..wired
        };
        assert_eq!(no_resolver.integer_class_id(), None);
        let zero = DirectHelperTable {
            well_known_class_id: answers_zero as *const () as usize,
            ..wired
        };
        assert_eq!(zero.integer_class_id(), None);
        let too_wide = DirectHelperTable {
            well_known_class_id: answers_too_wide as *const () as usize,
            ..wired
        };
        assert_eq!(too_wide.integer_class_id(), None);
        assert_eq!(DirectHelperTable::EMPTY.integer_class_id(), None);
    }

    /// Round 11 wave 14: `string_hash_field` needs both answers, decodes the
    /// field selector's `index + 1`, and treats `0` (unknown) as no answer —
    /// field 0 is reachable only as `1`.
    #[test]
    fn the_string_hash_field_needs_the_class_and_a_decoded_index() {
        unsafe extern "C" fn answers(ctx: i64, which: i64) -> i64 {
            match (ctx, which) {
                (0x1000, WELL_KNOWN_CLASS_STRING) => 7,
                (0x1000, WELL_KNOWN_STRING_HASH_FIELD) => 3,
                (0x2000, WELL_KNOWN_CLASS_STRING) => 7,
                (0x2000, WELL_KNOWN_STRING_HASH_FIELD) => 0,
                (0x3000, WELL_KNOWN_STRING_HASH_FIELD) => 1,
                _ => 0,
            }
        }
        let wired = DirectHelperTable {
            well_known_class_id: answers as *const () as usize,
            static_base_resolver_ctx: 0x1000,
            ..DirectHelperTable::EMPTY
        };
        assert_eq!(wired.string_hash_field(), Some((7, 2)));
        assert_eq!(wired.integer_class_id(), None, "the selectors stay apart");
        let no_field = DirectHelperTable {
            static_base_resolver_ctx: 0x2000,
            ..wired
        };
        assert_eq!(no_field.string_hash_field(), None);
        let no_class = DirectHelperTable {
            static_base_resolver_ctx: 0x3000,
            ..wired
        };
        assert_eq!(no_class.string_hash_field(), None);
        assert_eq!(DirectHelperTable::EMPTY.string_hash_field(), None);
    }
}

/// JIT round 19 proposal wave 7, lane tail7 (OSRFRAME18-2): the per-helper
/// "cannot reach the VM" classification the sibling tail call asks.
#[cfg(test)]
mod tail7_thin_helper_vm_reach_tests {
    use super::*;

    unsafe extern "C" fn twins(ctx: i64, which: i64) -> i64 {
        match (ctx, which) {
            (0x1000, WELL_KNOWN_LEAF_TWIN_CURRENT_THREAD) => 0x7100,
            (0x1000, WELL_KNOWN_LEAF_TWIN_CHECK_INDEX) => 0x7200,
            _ => 0,
        }
    }

    fn table() -> DirectHelperTable {
        DirectHelperTable {
            reachability_fence: 0x100,
            thread_current_thread: 0x200,
            preconditions_check_index: 0x300,
            integer_value_of: 0x400,
            thread_local_get: 0x500,
            well_known_class_id: twins as *const () as usize,
            static_base_resolver_ctx: 0x1000,
            ..DirectHelperTable::EMPTY
        }
    }

    #[test]
    fn only_the_fence_may_run_without_its_callers_frame() {
        let t = table();
        assert_eq!(t.thin_helper_vm_reach(0x100), ThinHelperVmReach::Never);
        for entry in [0x200, 0x300, 0x400, 0x500, 0x999] {
            assert!(
                !t.thin_helper_vm_reach(entry).never_reaches_vm(),
                "entry {entry:#x} must keep its caller's frame"
            );
        }
    }

    #[test]
    fn the_two_twins_carry_a_decline_their_helper_never_returns() {
        let t = table();
        assert_eq!(
            t.thin_helper_vm_reach(0x200),
            ThinHelperVmReach::LeafTwin {
                twin: 0x7100,
                decline: 0
            }
        );
        assert_eq!(
            t.thin_helper_vm_reach(0x300),
            ThinHelperVmReach::LeafTwin {
                twin: 0x7200,
                decline: -1
            }
        );
    }

    #[test]
    fn an_unpublished_twin_or_an_unwired_slot_may_reach_the_vm() {
        let no_vm = DirectHelperTable {
            static_base_resolver_ctx: 0,
            ..table()
        };
        assert_eq!(
            no_vm.thin_helper_vm_reach(0x200),
            ThinHelperVmReach::MayReachVm
        );
        assert_eq!(
            no_vm.thin_helper_vm_reach(0x300),
            ThinHelperVmReach::MayReachVm
        );
        // The fence needs no twin.
        assert_eq!(no_vm.thin_helper_vm_reach(0x100), ThinHelperVmReach::Never);
        // `0` is "not wired" in every slot: an unwired `reachability_fence`
        // must never make some entry read as `Never`.
        let e = DirectHelperTable::EMPTY;
        assert_eq!(e.thin_helper_vm_reach(0), ThinHelperVmReach::MayReachVm);
        assert_eq!(e.thin_helper_vm_reach(0x100), ThinHelperVmReach::MayReachVm);
    }
}

/// JIT round 19 proposal wave 8, lane tail8 (TAIL7-19-3): the four wider
/// leaf twins answer only under their own switch, and the wave-7 rows only
/// under theirs.
#[cfg(test)]
mod tail8_wide_leaf_twin_tests {
    use super::*;

    unsafe extern "C" fn twins(ctx: i64, which: i64) -> i64 {
        match (ctx, which) {
            (0x1000, WELL_KNOWN_LEAF_TWIN_CURRENT_THREAD) => 0x7100,
            (0x1000, WELL_KNOWN_LEAF_TWIN_CHECK_INDEX) => 0x7200,
            (0x1000, WELL_KNOWN_LEAF_TWIN_THREAD_LOCAL_GET) => 0x7300,
            (0x1000, WELL_KNOWN_LEAF_TWIN_UNSAFE_CAS_INT) => 0x7400,
            (0x1000, WELL_KNOWN_LEAF_TWIN_UNSAFE_CAS_LONG) => 0x7500,
            (0x1000, WELL_KNOWN_LEAF_TWIN_VECTORIZED_HASH_CODE) => 0x7600,
            _ => 0,
        }
    }

    fn table() -> DirectHelperTable {
        DirectHelperTable {
            reachability_fence: 0x100,
            thread_current_thread: 0x200,
            preconditions_check_index: 0x300,
            thread_local_get: 0x500,
            unsafe_cas_int: 0x600,
            unsafe_cas_long: 0x700,
            vectorized_hash_code: 0x800,
            // Reviewed and refused: must stay `MayReachVm` under both switches.
            hashmap_get: 0x900,
            thread_local_set: 0xA00,
            integer_int_value: 0xB00,
            object_get_class: 0xC00,
            unsafe_get_int: 0xD00,
            well_known_class_id: twins as *const () as usize,
            static_base_resolver_ctx: 0x1000,
            ..DirectHelperTable::EMPTY
        }
    }

    #[test]
    fn the_wide_rows_carry_a_decline_their_helper_never_returns() {
        let t = table();
        let twin = |entry| t.thin_helper_vm_reach_with(entry, false, true);
        assert_eq!(
            twin(0x500),
            ThinHelperVmReach::LeafTwin {
                twin: 0x7300,
                decline: -1
            }
        );
        assert_eq!(
            twin(0x600),
            ThinHelperVmReach::LeafTwin {
                twin: 0x7400,
                decline: -1
            }
        );
        assert_eq!(
            twin(0x700),
            ThinHelperVmReach::LeafTwin {
                twin: 0x7500,
                decline: -1
            }
        );
        assert_eq!(
            twin(0x800),
            ThinHelperVmReach::LeafTwin {
                twin: 0x7600,
                decline: LEAF_TWIN_DECLINE_OUTSIDE_INT
            }
        );
        // The hash helper answers every `int` sign-extended: the decline must
        // be outside that range.
        assert!(
            LEAF_TWIN_DECLINE_OUTSIDE_INT < i64::from(i32::MIN)
                || LEAF_TWIN_DECLINE_OUTSIDE_INT > i64::from(i32::MAX)
        );
    }

    #[test]
    fn each_switch_admits_only_its_own_rows() {
        let t = table();
        for entry in [0x500, 0x600, 0x700, 0x800] {
            assert_eq!(
                t.thin_helper_vm_reach_with(entry, true, false),
                ThinHelperVmReach::MayReachVm,
                "{entry:#x} is a wide row"
            );
            // `thin_helper_vm_reach` is the narrow set, unchanged.
            assert_eq!(t.thin_helper_vm_reach(entry), ThinHelperVmReach::MayReachVm);
        }
        for entry in [0x100, 0x200, 0x300] {
            assert_eq!(
                t.thin_helper_vm_reach_with(entry, false, true),
                ThinHelperVmReach::MayReachVm,
                "{entry:#x} is a narrow row"
            );
        }
        // No wide row is ever jumped to.
        for entry in [0x500, 0x600, 0x700, 0x800] {
            assert!(!t
                .thin_helper_vm_reach_with(entry, true, true)
                .never_reaches_vm());
        }
    }

    #[test]
    fn the_reviewed_refusals_stay_may_reach_vm() {
        let t = table();
        for entry in [0x900, 0xA00, 0xB00, 0xC00, 0xD00] {
            assert_eq!(
                t.thin_helper_vm_reach_with(entry, true, true),
                ThinHelperVmReach::MayReachVm,
                "{entry:#x}"
            );
        }
    }

    #[test]
    fn an_unpublished_wide_twin_keeps_the_call() {
        let t = DirectHelperTable {
            static_base_resolver_ctx: 0,
            ..table()
        };
        for entry in [0x500, 0x600, 0x700, 0x800] {
            assert_eq!(
                t.thin_helper_vm_reach_with(entry, true, true),
                ThinHelperVmReach::MayReachVm
            );
        }
    }
}

impl DirectHelperTable {
    /// nio21 wave 19, lane L3: the `ThreadLocal.get()` expansion's layout from
    /// [`Self::thread_local_ir_layout`], or `None` (not wired, no VM, or the
    /// VM declined). Asked once per compile that reaches such a site.
    pub fn thread_local_ir_layout_words(&self) -> Option<[u64; THREAD_LOCAL_IR_LAYOUT_WORDS]> {
        let raw = self.thread_local_ir_layout;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 {
            return None;
        }
        // SAFETY: the VM fills this field from `jit_thread_local_ir_layout`, an
        // `extern "C" fn(i64, i64, i64) -> i64` with process lifetime; a test
        // fills it from a function of the same signature.
        let f: DirectHelperFnThreadLocalIrLayout = unsafe { std::mem::transmute(raw) };
        let mut out = [0u64; THREAD_LOCAL_IR_LAYOUT_WORDS];
        // Casts: the context and the buffer address as the C ABI's i64s.
        //
        // SAFETY: `ctx` is the `SharedVm` published beside `f`, alive for every
        // compilation it runs; `out` is `THREAD_LOCAL_IR_LAYOUT_WORDS` words.
        let got = unsafe {
            f(
                ctx as i64,
                out.as_mut_ptr() as i64,
                THREAD_LOCAL_IR_LAYOUT_WORDS as i64,
            )
        };
        (got == THREAD_LOCAL_IR_LAYOUT_WORDS as i64).then_some(out)
    }

    /// JIT round 21 (lane refs2): `Reference.referent`'s field index from the
    /// same producer, asked with [`REFERENCE_REFERENT_QUERY_WORDS`], or `None`
    /// (not wired, no VM, or the VM declined: not `--jdk-only`). Unlike
    /// [`Self::thread_local_ir_layout_words`] it does not wait for the
    /// `ThreadLocal` classes to be loaded.
    pub fn reference_referent_index(&self) -> Option<u32> {
        let raw = self.thread_local_ir_layout;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 {
            return None;
        }
        // SAFETY: as `thread_local_ir_layout_words`.
        let f: DirectHelperFnThreadLocalIrLayout = unsafe { std::mem::transmute(raw) };
        let mut out = [0u64; REFERENCE_REFERENT_QUERY_WORDS];
        // Casts: the context and the buffer address as the C ABI's i64s.
        //
        // SAFETY: `ctx` is the `SharedVm` published beside `f`; `out` is
        // `REFERENCE_REFERENT_QUERY_WORDS` words.
        let got = unsafe {
            f(
                ctx as i64,
                out.as_mut_ptr() as i64,
                REFERENCE_REFERENT_QUERY_WORDS as i64,
            )
        };
        if got != REFERENCE_REFERENT_QUERY_WORDS as i64 {
            return None;
        }
        u32::try_from(out[0]).ok()
    }

    /// JIT round 21 (lane tlsub, TL321-4): which `Thread` map
    /// `ThreadLocal.get` reads for a receiver of class `class_id`, and whether
    /// that class also leaves `set(Object)` to `ThreadLocal`, from the same
    /// producer asked with [`THREAD_LOCAL_RECEIVER_QUERY_WORDS`]; `None` (not
    /// wired, no VM, the VM declined: not `--jdk-only`, the classes not loaded,
    /// an unknown id). Compile time only.
    pub fn thread_local_receiver_kind(&self, class_id: u32) -> Option<(u8, bool)> {
        let raw = self.thread_local_ir_layout;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 || class_id == 0 {
            return None;
        }
        // SAFETY: as `thread_local_ir_layout_words`.
        let f: DirectHelperFnThreadLocalIrLayout = unsafe { std::mem::transmute(raw) };
        let mut io = [u64::from(class_id), 0u64];
        // Casts: the context and the buffer address as the C ABI's i64s.
        //
        // SAFETY: `ctx` is the `SharedVm` published beside `f`; `io` is
        // `THREAD_LOCAL_RECEIVER_QUERY_WORDS` words.
        let got = unsafe {
            f(
                ctx as i64,
                io.as_mut_ptr() as i64,
                THREAD_LOCAL_RECEIVER_QUERY_WORDS as i64,
            )
        };
        if got != THREAD_LOCAL_RECEIVER_QUERY_WORDS as i64 {
            return None;
        }
        Some((u8::try_from(io[0]).ok()?, io[1] == 1))
    }

    /// TLGET21-7: is the expansion's layout pending -- served by this VM, but
    /// not yet resolvable because a class it bakes (`ThreadLocalMap`, its
    /// `Entry`) has not been loaded? Asked with
    /// [`THREAD_LOCAL_LAYOUT_PENDING_QUERY_WORDS`]; `false` when not wired, no
    /// VM, the VM declined, or the layout is there.
    pub fn thread_local_layout_pending(&self) -> bool {
        let raw = self.thread_local_ir_layout;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 {
            return false;
        }
        // SAFETY: as `thread_local_ir_layout_words`.
        let f: DirectHelperFnThreadLocalIrLayout = unsafe { std::mem::transmute(raw) };
        let mut out = [0u64; THREAD_LOCAL_LAYOUT_PENDING_QUERY_WORDS];
        // Casts: the context and the buffer address as the C ABI's i64s.
        //
        // SAFETY: `ctx` is the `SharedVm` published beside `f`; `out` is
        // `THREAD_LOCAL_LAYOUT_PENDING_QUERY_WORDS` words.
        let got = unsafe {
            f(
                ctx as i64,
                out.as_mut_ptr() as i64,
                THREAD_LOCAL_LAYOUT_PENDING_QUERY_WORDS as i64,
            )
        };
        got == 1 && out[0] == 1
    }

    /// JIT round 21 (lane g1vhsoft, `CRATONVM_JIT_IR_REFERENCE_GET_INLINE`):
    /// the `WeakReference.get()` / `SoftReference.get()` expansion's words
    /// from the same producer, asked with [`REFERENCE_GET_LAYOUT_QUERY_WORDS`]
    /// (see `crate::ir::ReferenceGetIrLayout::from_words` for the meaning),
    /// or `None` (not wired, no VM, or the VM declined). Compile time only.
    pub fn reference_get_layout_words(&self) -> Option<[u64; REFERENCE_GET_LAYOUT_QUERY_WORDS]> {
        let raw = self.thread_local_ir_layout;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 {
            return None;
        }
        // SAFETY: as `thread_local_ir_layout_words`.
        let f: DirectHelperFnThreadLocalIrLayout = unsafe { std::mem::transmute(raw) };
        let mut out = [0u64; REFERENCE_GET_LAYOUT_QUERY_WORDS];
        // Casts: the context and the buffer address as the C ABI's i64s.
        //
        // SAFETY: `ctx` is the `SharedVm` published beside `f`; `out` is
        // `REFERENCE_GET_LAYOUT_QUERY_WORDS` words.
        let got = unsafe {
            f(
                ctx as i64,
                out.as_mut_ptr() as i64,
                REFERENCE_GET_LAYOUT_QUERY_WORDS as i64,
            )
        };
        (got == REFERENCE_GET_LAYOUT_QUERY_WORDS as i64).then_some(out)
    }

    /// t1 wave 12 (lane REFS4, `CRATONVM_JIT_IR_REFERS_TO_COMPACT_ROW`):
    /// `(index, 1 + compact displacement)` of `Reference.referent` from the
    /// same producer, asked with [`REFERENCE_REFERENT_ROW_QUERY_WORDS`]; the
    /// second word is `0` when the VM names no row. `None` when not wired,
    /// without a VM, or where the VM declines.
    pub fn reference_referent_row(&self) -> Option<(u32, u32)> {
        let out = self.reference_referent_row_words()?;
        Some((u32::try_from(out[0]).ok()?, u32::try_from(out[1]).ok()?))
    }

    /// j27 wave 5 (lane REFS, `CRATONVM_JIT_IR_REFERS_TO_ZGC_ROW`): the
    /// referent-row answer's COMPARE-ONLY row (word 3,
    /// [`REFERENCE_REFERENT_ROW_ANSWER_WORDS`]): `1 + compact displacement`
    /// for a read that owes no keep-alive, `0` when the VM names none. `None`
    /// where [`Self::reference_referent_row`] is.
    pub fn reference_referent_compare_row(&self) -> Option<u32> {
        let out = self.reference_referent_row_words()?;
        u32::try_from(out[3]).ok()
    }

    /// The [`REFERENCE_REFERENT_ROW_QUERY_WORDS`] answer, taken only when the
    /// producer fills exactly [`REFERENCE_REFERENT_ROW_ANSWER_WORDS`].
    fn reference_referent_row_words(&self) -> Option<[u64; REFERENCE_REFERENT_ROW_QUERY_WORDS]> {
        let raw = self.thread_local_ir_layout;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 {
            return None;
        }
        // SAFETY: as `thread_local_ir_layout_words`.
        let f: DirectHelperFnThreadLocalIrLayout = unsafe { std::mem::transmute(raw) };
        let mut out = [0u64; REFERENCE_REFERENT_ROW_QUERY_WORDS];
        // SAFETY: `ctx` is the `SharedVm` published beside `f`; `out` is
        // `REFERENCE_REFERENT_ROW_QUERY_WORDS` words.
        let got = unsafe {
            f(
                ctx as i64,
                out.as_mut_ptr() as i64,
                REFERENCE_REFERENT_ROW_QUERY_WORDS as i64,
            )
        };
        (got == REFERENCE_REFERENT_ROW_ANSWER_WORDS as i64).then_some(out)
    }

    /// j27 wave 5 (lane REFS): may this VM's heap ever arm the ZGC referent
    /// read gate (`VmHeap::zgc_referent_gate_may_arm`), asked with
    /// [`REFERENT_GATE_MAY_ARM_QUERY_WORDS`]? Then every compiled referent
    /// read owes the checked helper, which runs the gate. `false` when not
    /// wired, without a VM, or where the VM declines (the pre-j27 answer).
    /// Compile time only.
    pub fn referent_gate_may_arm(&self) -> bool {
        let raw = self.thread_local_ir_layout;
        let ctx = self.static_base_resolver_ctx;
        if raw == 0 || ctx == 0 {
            return false;
        }
        // SAFETY: as `thread_local_ir_layout_words`.
        let f: DirectHelperFnThreadLocalIrLayout = unsafe { std::mem::transmute(raw) };
        let mut out = [0u64; REFERENT_GATE_MAY_ARM_QUERY_WORDS];
        // SAFETY: `ctx` is the `SharedVm` published beside `f`; `out` is
        // `REFERENT_GATE_MAY_ARM_QUERY_WORDS` words.
        let got = unsafe {
            f(
                ctx as i64,
                out.as_mut_ptr() as i64,
                REFERENT_GATE_MAY_ARM_QUERY_WORDS as i64,
            )
        };
        got == 1 && out[0] == 1
    }
}

/// t1 wave 12 (lane REFS4): [`DirectHelperTable::reference_referent_row`]
/// asks with [`REFERENCE_REFERENT_ROW_QUERY_WORDS`] and takes only that exact
/// answer; the one-word index query is not it.
#[cfg(test)]
mod t1w12_refs4_referent_row_query_tests {
    use super::*;

    unsafe extern "C" fn row_answer(ctx: i64, out: i64, n: i64) -> i64 {
        if ctx != 0x1000 || n != REFERENCE_REFERENT_ROW_QUERY_WORDS as i64 {
            return 0;
        }
        let o = out as *mut u64;
        o.write(0);
        o.add(1).write(9);
        o.add(2).write(77);
        o.add(3).write(11);
        REFERENCE_REFERENT_ROW_ANSWER_WORDS as i64
    }

    /// j27 wave 5 (lane REFS): the pre-j27 three-word answer.
    unsafe extern "C" fn three_word_answer(_ctx: i64, out: i64, _n: i64) -> i64 {
        let o = out as *mut u64;
        o.write(0);
        o.add(1).write(9);
        o.add(2).write(77);
        3
    }

    unsafe extern "C" fn short_answer(_ctx: i64, out: i64, _n: i64) -> i64 {
        (out as *mut u64).write(0);
        1
    }

    fn table(f: DirectHelperFnThreadLocalIrLayout, ctx: usize) -> DirectHelperTable {
        DirectHelperTable {
            thread_local_ir_layout: f as *const () as usize,
            static_base_resolver_ctx: ctx,
            ..DirectHelperTable::EMPTY
        }
    }

    #[test]
    fn t1w12_refs4_the_row_query() {
        assert_eq!(
            table(row_answer, 0x1000).reference_referent_row(),
            Some((0, 9))
        );
        // The row producer is not a one-word index answer, and vice versa.
        assert_eq!(table(row_answer, 0x1000).reference_referent_index(), None);
        assert_eq!(table(short_answer, 0x1000).reference_referent_row(), None);
        assert_eq!(table(row_answer, 0).reference_referent_row(), None);
        assert_eq!(DirectHelperTable::EMPTY.reference_referent_row(), None);
        // Every query size the producer tells apart is its own.
        for other in [
            REFERENCE_REFERENT_QUERY_WORDS,
            THREAD_LOCAL_RECEIVER_QUERY_WORDS,
            THREAD_LOCAL_LAYOUT_PENDING_QUERY_WORDS,
            REFERENCE_GET_LAYOUT_QUERY_WORDS,
            THREAD_LOCAL_IR_LAYOUT_WORDS,
        ] {
            assert_ne!(REFERENCE_REFERENT_ROW_QUERY_WORDS, other);
        }
        assert!(REFERENCE_REFERENT_ROW_ANSWER_WORDS <= REFERENCE_REFERENT_ROW_QUERY_WORDS);
        assert!(REFERENCE_REFERENT_ROW_QUERY_WORDS < THREAD_LOCAL_IR_LAYOUT_WORDS);
    }

    /// j27 wave 5 (lane REFS): word 3 is the compare-only row, read only from
    /// a full answer; a short (three-word) answer is no answer at all.
    #[test]
    fn j27_refs_the_compare_row_is_word_three() {
        assert_eq!(
            table(row_answer, 0x1000).reference_referent_compare_row(),
            Some(11)
        );
        assert_eq!(table(row_answer, 0x1000).reference_referent_row(), Some((0, 9)));
        assert_eq!(table(three_word_answer, 0x1000).reference_referent_compare_row(), None);
        assert_eq!(table(three_word_answer, 0x1000).reference_referent_row(), None);
        assert_eq!(DirectHelperTable::EMPTY.reference_referent_compare_row(), None);
    }
}

#[cfg(test)]
mod nio21w19_thread_local_ir_layout_word_tests {
    use super::*;

    unsafe extern "C" fn fill(ctx: i64, out: i64, n: i64) -> i64 {
        if ctx != 0x1000 || n < THREAD_LOCAL_IR_LAYOUT_WORDS as i64 {
            return 0;
        }
        for i in 0..THREAD_LOCAL_IR_LAYOUT_WORDS {
            (out as *mut u64).add(i).write(100 + i as u64);
        }
        THREAD_LOCAL_IR_LAYOUT_WORDS as i64
    }

    unsafe extern "C" fn decline(_ctx: i64, _out: i64, _n: i64) -> i64 {
        0
    }

    /// The reader asks with the published context and takes only a full
    /// answer; no word, no context or a decline answer `None`.
    #[test]
    fn nio21w19_the_layout_reader_takes_only_a_full_answer() {
        let t = |f: DirectHelperFnThreadLocalIrLayout, ctx: usize| DirectHelperTable {
            thread_local_ir_layout: f as *const () as usize,
            static_base_resolver_ctx: ctx,
            ..DirectHelperTable::EMPTY
        };
        let got = t(fill, 0x1000)
            .thread_local_ir_layout_words()
            .expect("full answer");
        assert_eq!(got[0], 100);
        assert_eq!(got[9], 109);
        assert_eq!(got[14], 114);
        assert!(t(fill, 0).thread_local_ir_layout_words().is_none());
        assert!(t(decline, 0x1000).thread_local_ir_layout_words().is_none());
        assert!(DirectHelperTable::EMPTY
            .thread_local_ir_layout_words()
            .is_none());
    }
}

#[cfg(test)]
mod r21_updaters_field_cell_byte_offset_tests {
    use super::*;

    /// JIT round 21 wave 1, lane updaters: a tagged table's screen is the
    /// byte-offset decode followed by exactly the untagged screen (whose
    /// immediates carry the UNTAGGED table address), and its decline edges
    /// are the decode's `JNZ` plus the untagged edges shifted past it. Both
    /// halves (int/long and reference).
    #[test]
    fn r21_updaters_tagged_table_screen_is_the_decode_then_the_plain_screen() {
        let (decode, edge) = unsafe_byte_field_offset_decode_bytes();
        assert_eq!(
            decode,
            vec![
                0x49, 0x83, 0xEB, 0x10, // SUB R11, 16
                0x41, 0xF6, 0xC3, 0x07, // TEST R11B, 7
                0x0F, 0x85, 0, 0, 0, 0, // JNZ rel32
                0x49, 0xC1, 0xEB, 0x03, // SHR R11, 3
            ]
        );
        assert_eq!(edge, 10);
        let tagged = 0x10_0000 | UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG;
        let pairs = [
            (
                unsafe_inline_field_cell_bytes(0x10_0000, 0x20_0000, 4),
                unsafe_inline_field_cell_bytes(tagged, 0x20_0000, 4),
            ),
            (
                unsafe_inline_field_cell_bytes(0x10_0000, 0x20_0000, 8),
                unsafe_inline_field_cell_bytes(tagged, 0x20_0000, 8),
            ),
            (
                unsafe_inline_ref_field_cell_bytes(0x10_0000, 0x20_0000),
                unsafe_inline_ref_field_cell_bytes(tagged, 0x20_0000),
            ),
        ];
        for ((plain, plain_edges), (with_decode, decode_edges)) in pairs {
            assert_eq!(&with_decode[..decode.len()], &decode[..]);
            assert_eq!(&with_decode[decode.len()..], &plain[..]);
            assert_eq!(decode_edges.len(), plain_edges.len() + 1);
            assert_eq!(decode_edges[0], edge);
            for (t, p) in decode_edges[1..].iter().zip(&plain_edges) {
                assert_eq!(*t, p + decode.len());
            }
        }
    }

    /// The decode's arithmetic against the natives' encode
    /// (`16 + slot * 8`): every cell-keyable slot round-trips, and an offset
    /// below the base, inside a field, past the key, in the synthetic band or
    /// the `Buffer.address` sentinel never names a slot.
    #[test]
    fn r21_updaters_byte_offset_decode_model_matches_the_encode() {
        let decode = |off: u64| -> Option<u64> {
            let rel = off.wrapping_sub(16);
            if rel & 7 != 0 {
                return None;
            }
            let slot = rel >> 3;
            (slot <= UNSAFE_FIELD_CELL_MAX_SLOT as u64).then_some(slot)
        };
        for slot in [0u64, 1, 2, 7, 0x7FFE, 0x7FFF] {
            assert_eq!(decode(16 + slot * 8), Some(slot));
        }
        for bad in [
            0u64,
            8,
            15,
            17,
            20,
            16 + 0x8000 * 8,
            0x2000_0000,
            0x3000_0000,
            0x7fff_ffff_ffff_fffe,
            u64::MAX - 7,
        ] {
            assert_eq!(decode(bad), None, "offset {bad:#x}");
        }
    }

    /// JIT round 21 wave 24 (lane flipprep, flip review #14 / #15): an
    /// Unsafe ARRAY offset is a real byte offset (`16 + 8*i` for `long[]` /
    /// `Object[]`), so the decode above maps it to a slot `i`, and an array's
    /// class word is its COMPONENT class id, so the cell key alone would alias
    /// that class's field cell (CHM `tabAt` reading a `Node` field cell). The
    /// only thing between them is the screen's `TEST BYTE [RAX + GC_FLAGS],
    /// GC_FLAG_COMPACT ; JZ`, so pin that no array header ever carries the
    /// bit: the constructor both inline array allocators take their mark word
    /// from (`ObjectHeader::new`, SP `objects.rs` and IR `runtime_lowering.rs`),
    /// the collector's own `alloc_array`, and the mark / old-gen transitions a
    /// live array goes through. A future "compact array header" fails here
    /// instead of turning the byte-offset field cells into an array alias.
    #[test]
    fn r21_flipprep_no_array_header_carries_gc_flag_compact() {
        use cratonvm_types::{
            ArrayElementType, ClassId, ObjectHeader, ObjectKind, GC_FLAGS_BYTE_OFFSET,
            GC_FLAG_COMPACT, GC_FLAG_MARKED, GC_FLAG_OLD_GEN, MARK_WORD_OFFSET,
        };
        // The byte the screen's TEST reads: the mark word's byte at
        // `GC_FLAGS_BYTE_OFFSET` (little-endian, the mark word at
        // `MARK_WORD_OFFSET`).
        let screen_byte = |mark: u32| (mark >> (8 * (GC_FLAGS_BYTE_OFFSET - MARK_WORD_OFFSET))) as u8;
        let all = [
            ArrayElementType::Boolean,
            ArrayElementType::Char,
            ArrayElementType::Float,
            ArrayElementType::Double,
            ArrayElementType::Byte,
            ArrayElementType::Short,
            ArrayElementType::Int,
            ArrayElementType::Long,
            ArrayElementType::Reference,
        ];
        for elem in all {
            for (class_word, len) in [(0u32, 0u32), (0x0F13, 1), (0x0F13, 17), (7, 0x7FFF)] {
                let header = ObjectHeader::new(ClassId::new(class_word), ObjectKind::Array, elem, len, len);
                let mark = header.mark_word.load(std::sync::atomic::Ordering::Relaxed);
                assert_eq!(screen_byte(mark) & GC_FLAG_COMPACT, 0, "{elem:?}[{len}]: the inline arms' mark word");
                assert_eq!(header.gc_flags() & GC_FLAG_COMPACT, 0);
                assert!(!ObjectHeader::is_short_mark(mark));
                header.add_gc_flags(GC_FLAG_MARKED | GC_FLAG_OLD_GEN);
                header.clear_gc_flags(GC_FLAG_MARKED);
                let after = header.mark_word.load(std::sync::atomic::Ordering::Relaxed);
                assert_eq!(screen_byte(after) & GC_FLAG_COMPACT, 0, "{elem:?}: after mark / promote");
            }
        }
        let heap = cratonvm_gc::Heap::new();
        for elem in all {
            for len in [0usize, 1, 9, 64] {
                let arr = heap.alloc_array(ClassId::new(0x0F13), elem, len);
                let header = heap.get_header(arr);
                assert_eq!(header.kind(), ObjectKind::Array);
                assert_eq!(header.gc_flags() & GC_FLAG_COMPACT, 0, "{elem:?}[{len}]: alloc_array");
                assert_eq!(
                    screen_byte(header.mark_word.load(std::sync::atomic::Ordering::Relaxed)) & GC_FLAG_COMPACT,
                    0
                );
            }
        }
    }

    /// UPDATERS21-1: the access bytes, the admission bits and the arity; no
    /// arm without the admission bit (whatever the switch says).
    #[test]
    fn r21_updaters_reference_volatile_rows_bytes_bits_and_admission() {
        let get = UnsafeRefVolatileAccess {
            is_put: false,
            ordered_store: false,
            exchange: false,
        };
        let put = UnsafeRefVolatileAccess {
            is_put: true,
            ordered_store: false,
            exchange: false,
        };
        let release = UnsafeRefVolatileAccess {
            is_put: true,
            ordered_store: true,
            exchange: false,
        };
        // MOV RAX, [RAX+R11] ; XCHG [RAX+R11], RDX ; MOV [RAX+R11], RDX.
        assert_eq!(unsafe_ref_volatile_access_bytes(get), [0x4A, 0x8B, 0x04, 0x18]);
        assert_eq!(unsafe_ref_volatile_access_bytes(put), [0x4A, 0x87, 0x14, 0x18]);
        assert_eq!(unsafe_ref_volatile_access_bytes(release), [0x4A, 0x89, 0x14, 0x18]);
        assert_eq!(get.gate_bit(), UNSAFE_REF_VOLATILE_GET_GATE_BIT);
        assert_eq!(put.gate_bit(), UNSAFE_REF_VOLATILE_PUT_GATE_BIT);
        assert_eq!((get.num_params(), put.num_params()), (2, 3));
        // The new bits collide with no older row: CAE 16/17, ref CAS 18,
        // the boolean / float / double rows 19..25, and the veto-only 27.
        let taken = (0..UNSAFE_QU_EXTRA_FIRST_BIT + UNSAFE_QU_EXTRA_GATE_BITS.len())
            .chain([UNSAFE_FIELD_ARM_BAKED_BIT]);
        for bit in taken {
            assert_ne!(bit, UNSAFE_REF_VOLATILE_GET_GATE_BIT);
            assert_ne!(bit, UNSAFE_REF_VOLATILE_PUT_GATE_BIT);
        }
        let table = DirectHelperTable {
            unsafe_field_cells: 0x10_0000,
            ..DirectHelperTable::EMPTY
        };
        assert!(unsafe_ref_volatile_field_arm_words(&table, get).is_none());
        assert!(unsafe_ref_volatile_field_arm_words(&table, put).is_none());
        // Recognition: only the reference volatile family, only `Unsafe`.
        let u = "jdk/internal/misc/Unsafe";
        let g = "(Ljava/lang/Object;J)Ljava/lang/Object;";
        let p = "(Ljava/lang/Object;JLjava/lang/Object;)V";
        assert!(unsafe_inline_ref_volatile_site("sun/misc/Unsafe", "getReferenceVolatile", g).is_none());
        assert!(unsafe_inline_ref_volatile_site(u, "getReference", g).is_none());
        assert!(unsafe_inline_ref_volatile_site(u, "putReference", p).is_none());
        const KEY: &str = "CRATONVM_JIT_UNSAFE_REF_VOLATILE_FIELD";
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            assert_eq!(unsafe_inline_ref_volatile_site(u, "getReferenceVolatile", g), Some(get));
            assert_eq!(unsafe_inline_ref_volatile_site(u, "getReferenceAcquire", g), Some(get));
            assert_eq!(unsafe_inline_ref_volatile_site(u, "getReferenceOpaque", g), Some(get));
            assert_eq!(unsafe_inline_ref_volatile_site(u, "putReferenceVolatile", p), Some(put));
            assert_eq!(unsafe_inline_ref_volatile_site(u, "putReferenceRelease", p), Some(release));
            assert_eq!(unsafe_inline_ref_volatile_site(u, "putReferenceOpaque", p), Some(release));
            assert!(unsafe_inline_ref_volatile_site(u, "getReferenceVolatile", p).is_none());
            // The admission bit decides even with the switch on.
            assert!(unsafe_ref_volatile_field_arm_words(&table, get).is_none());
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert!(unsafe_inline_ref_volatile_site(u, "getReferenceVolatile", g).is_none());
            let admitted = DirectHelperTable {
                unsafe_inline_admitted: usize::MAX,
                unsafe_field_cells: 0x10_0000,
                ..DirectHelperTable::EMPTY
            };
            assert!(unsafe_ref_volatile_field_arm_words(&admitted, get).is_none());
        });
    }

    /// UPDATERS21-2: the veto's extra condition is the field-arm bit, and
    /// only under its switch.
    #[test]
    fn r21_updaters_rmw_veto_asks_the_field_arm_bit_only_under_its_switch() {
        if !cfg!(target_arch = "x86_64")
            || !unsafe_rmw_inline_enabled()
            || !unsafe_accessor_inline_enabled()
            || !crate::ir_thin_prefix_enabled()
            || !unsafe_rmw_body_no_splice_enabled()
        {
            return;
        }
        let u = "jdk/internal/misc/Unsafe";
        let d = "(Ljava/lang/Object;JI)I";
        let without = usize::MAX & !(1usize << UNSAFE_FIELD_ARM_BAKED_BIT);
        let with = usize::MAX;
        const KEY: &str = "CRATONVM_JIT_UNSAFE_RMW_VETO_NEEDS_FIELD_ARM";
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert!(unsafe_inline_rmw_body_splice_loses_prefix(u, "getAndAddInt", d, with));
            assert!(unsafe_inline_rmw_body_splice_loses_prefix(u, "getAndAddInt", d, without));
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
            assert!(unsafe_inline_rmw_body_splice_loses_prefix(u, "getAndAddInt", d, with));
            assert!(!unsafe_inline_rmw_body_splice_loses_prefix(u, "getAndAddInt", d, without));
        });
    }

    /// JIT round 23 wave 1, lane L9 (W4-L4-3's reference half): the
    /// `getAndSetReference*` rows are recognised under their own switch (not
    /// UPDATERS21-1's opt-in), store like a volatile put (`XCHG`), return the
    /// previous value (`MOV RAX, RDX`), own admission bit 28 (no collision),
    /// and get an arm only with that bit; the splice veto keeps the body a
    /// call only with that bit AND a baked field arm.
    #[test]
    fn r23_l9_reference_get_and_set_rows_bits_bytes_and_veto() {
        let u = "jdk/internal/misc/Unsafe";
        let d = "(Ljava/lang/Object;JLjava/lang/Object;)Ljava/lang/Object;";
        let xchg = UnsafeRefVolatileAccess {
            is_put: true,
            ordered_store: false,
            exchange: true,
        };
        assert!(xchg.returns_value());
        assert_eq!(xchg.num_params(), 3);
        assert_eq!(xchg.gate_bit(), UNSAFE_REF_GET_AND_SET_GATE_BIT);
        assert_eq!(unsafe_ref_volatile_access_bytes(xchg), [0x4A, 0x87, 0x14, 0x18]);
        assert_eq!(UNSAFE_REF_EXCHANGE_RESULT_BYTES, [0x48, 0x89, 0xD0]);
        let taken = (0..UNSAFE_QU_EXTRA_FIRST_BIT + UNSAFE_QU_EXTRA_GATE_BITS.len()).chain([
            UNSAFE_REF_CAS_GATE_BIT,
            UNSAFE_REF_VOLATILE_GET_GATE_BIT,
            UNSAFE_REF_VOLATILE_PUT_GATE_BIT,
            UNSAFE_FIELD_ARM_BAKED_BIT,
        ]);
        for bit in taken {
            assert_ne!(bit, UNSAFE_REF_GET_AND_SET_GATE_BIT);
        }
        assert!(UNSAFE_REF_GET_AND_SET_GATE_BIT < usize::BITS as usize);
        const KEY: &str = "CRATONVM_JIT_UNSAFE_REF_GET_AND_SET_FIELD";
        const RV: &str = "CRATONVM_JIT_UNSAFE_REF_VOLATILE_FIELD";
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None), (RV, None)], || {
            for m in ["getAndSetReference", "getAndSetReferenceAcquire", "getAndSetReferenceRelease"] {
                assert_eq!(unsafe_inline_ref_volatile_site(u, m, d), Some(xchg), "{m}");
            }
            assert!(unsafe_inline_ref_volatile_site("sun/misc/Unsafe", "getAndSetReference", d).is_none());
            assert!(unsafe_inline_ref_volatile_site(u, "getAndSetReference", "(Ljava/lang/Object;JLjava/lang/Object;)V").is_none());
            assert!(unsafe_inline_ref_volatile_site(u, "getAndSetInt", "(Ljava/lang/Object;JI)I").is_none());
            // UPDATERS21-1's rows stay off.
            assert!(unsafe_inline_ref_volatile_site(u, "getReferenceVolatile", "(Ljava/lang/Object;J)Ljava/lang/Object;").is_none());
            // No arm without bit 28.
            let without = DirectHelperTable {
                unsafe_inline_admitted: usize::MAX & !(1usize << UNSAFE_REF_GET_AND_SET_GATE_BIT),
                unsafe_field_cells: 0x10_0000,
                ..DirectHelperTable::EMPTY
            };
            assert!(unsafe_ref_volatile_field_arm_words(&without, xchg).is_none());
            if cfg!(target_arch = "x86_64")
                && unsafe_accessor_inline_enabled()
                && unsafe_field_rmw_inline_enabled()
                && crate::ir_thin_prefix_enabled()
                && unsafe_rmw_body_no_splice_enabled()
                && !crate::x64::narrow_oops_block_inline_fields()
            {
                let all = usize::MAX;
                assert!(unsafe_inline_rmw_body_splice_loses_prefix(u, "getAndSetReference", d, all));
                let no_arm = all & !(1usize << UNSAFE_FIELD_ARM_BAKED_BIT);
                assert!(!unsafe_inline_rmw_body_splice_loses_prefix(u, "getAndSetReference", d, no_arm));
                let no_bit = all & !(1usize << UNSAFE_REF_GET_AND_SET_GATE_BIT);
                assert!(!unsafe_inline_rmw_body_splice_loses_prefix(u, "getAndSetReference", d, no_bit));
            }
        });
        cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            assert!(unsafe_inline_ref_volatile_site(u, "getAndSetReference", d).is_none());
            assert!(!unsafe_inline_rmw_body_splice_loses_prefix(u, "getAndSetReference", d, usize::MAX));
            let all = DirectHelperTable {
                unsafe_inline_admitted: usize::MAX,
                unsafe_field_cells: 0x10_0000,
                ..DirectHelperTable::EMPTY
            };
            assert!(unsafe_ref_volatile_field_arm_words(&all, xchg).is_none());
        });
    }
}

#[cfg(test)]
mod jr21_atomics_static_cell_tests {
    use super::*;

    /// The tags the static screen compares are the `Value` discriminants a
    /// static cell carries (`vm_exec::hw_static_cell_update`'s `0` / `1`),
    /// read where the screen reads them.
    #[test]
    fn static_tags_are_the_value_discriminants() {
        let tag_of = |v: cratonvm_types::Value| -> u32 {
            // SAFETY: a `Value` is 16 bytes whose first word is the
            // discriminant (`FIELD_CELL_TAG_OFFSET == 0`).
            unsafe { *(&v as *const cratonvm_types::Value as *const u32) }
        };
        assert_eq!(
            tag_of(cratonvm_types::Value::Int(-7)),
            u32::from(UNSAFE_STATIC_TAG_INT)
        );
        assert_eq!(
            tag_of(cratonvm_types::Value::Long(-7)),
            u32::from(UNSAFE_STATIC_TAG_LONG)
        );
    }

    /// Keys are never the empty word for a band offset, and the index is the
    /// screen's `(off ^ off >> 3) & (COUNT - 1)`.
    #[test]
    fn static_keys_and_indices() {
        for off in [
            UNSAFE_STATIC_OFFSET_FIRST,
            UNSAFE_STATIC_OFFSET_FIRST + 1,
            0x3000_0000,
            0x3000_0008,
            UNSAFE_STATIC_OFFSET_LAST,
        ] {
            assert_ne!(unsafe_static_cell_key(off, false), 0);
            assert_eq!(
                unsafe_static_cell_key(off, true),
                unsafe_static_cell_key(off, false) | 1
            );
            let i = unsafe_static_cell_index(off);
            assert!(i < UNSAFE_STATIC_CELL_COUNT);
            // Casts: a band offset fits 32 bits, as the screen computes it.
            assert_eq!(i, ((off as u32 ^ (off as u32 >> 3)) & 127) as usize);
        }
        // Consecutive minted offsets and consecutive scaled ones spread.
        assert_ne!(
            unsafe_static_cell_index(0x2000_0000),
            unsafe_static_cell_index(0x2000_0001)
        );
        assert_ne!(
            unsafe_static_cell_index(0x3000_0000),
            unsafe_static_cell_index(0x3000_0008)
        );
        // Casts: small positive constants. The halves follow each other:
        // int/long, reference, sub-word cells, then the static entries.
        assert_eq!(
            UNSAFE_SUBWORD_FIELD_CELL_BASE as usize,
            UNSAFE_REF_FIELD_CELL_BASE as usize + 8 * UNSAFE_FIELD_CELL_COUNT
        );
        assert_eq!(
            UNSAFE_STATIC_CELL_BASE,
            UNSAFE_SUBWORD_FIELD_CELL_BASE as usize + 8 * UNSAFE_FIELD_CELL_COUNT
        );
    }

    /// The screen's shape: every decline is a zeroed rel32 `Jcc`, the band
    /// edge is the `JA`, the width picks the key bit, the tag and the payload.
    #[test]
    fn static_screen_bytes_shape() {
        let table = 0x0000_1122_3344_5000usize;
        let (b4, e4, ns4) = unsafe_inline_static_cell_bytes(table, 4);
        let (b8, e8, ns8) = unsafe_inline_static_cell_bytes(table, 8);
        assert_eq!(e4.len(), 4);
        assert_eq!(e8.len(), 4);
        assert_eq!(
            b8.len(),
            b4.len() + 4,
            "only the long key's OR RDX, 1 differs"
        );
        for (b, e, ns) in [(&b4, &e4, ns4), (&b8, &e8, ns8)] {
            for &at in e.iter().chain(std::iter::once(&ns)) {
                assert_eq!(b[at - 2], 0x0F, "a rel32 Jcc at {at}");
                assert_eq!(&b[at..at + 4], &[0, 0, 0, 0]);
            }
            assert_eq!(b[ns - 1], 0x87, "the band edge is JA");
            let has = |n: &[u8]| b.windows(n.len()).any(|w| w == n);
            let mut mov = vec![0x49, 0xBA];
            // Cast: an address.
            mov.extend_from_slice(&((table + UNSAFE_STATIC_CELL_BASE) as u64).to_le_bytes());
            assert!(has(&mov), "MOV R10, the static half");
            assert!(has(&[0x49, 0x3B, 0x14, 0x0A]), "CMP RDX, [R10+RCX]");
            assert!(
                has(&[0x49, 0x3B, 0x44, 0x0A, 0x08]),
                "CMP RAX, [R10+RCX+8]: the mirror proof"
            );
            assert!(
                has(&[0x45, 0x8B, 0x5C, 0x0A, 0x18]),
                "MOV R11D: the displacement half"
            );
        }
        let has = |b: &[u8], n: &[u8]| b.windows(n.len()).any(|w| w == n);
        assert!(has(&b4, &[0x42, 0x83, 0x3C, 0x18, UNSAFE_STATIC_TAG_INT]));
        assert!(has(&b8, &[0x42, 0x83, 0x3C, 0x18, UNSAFE_STATIC_TAG_LONG]));
        assert!(b4.ends_with(&[0x49, 0x83, 0xC3, 4]), "ADD R11, payload32");
        assert!(b8.ends_with(&[0x49, 0x83, 0xC3, 8]), "ADD R11, payload64");
        assert!(has(&b8, &[0x4B, 0x8D, 0x14, 0x1B, 0x48, 0x83, 0xCA, 0x01]));
    }

    /// No cell table, no static arm; the byte-offset tag bit is stripped.
    #[test]
    fn static_arm_table_follows_the_cell_table() {
        assert_eq!(unsafe_static_arm_table(&DirectHelperTable::EMPTY), None);
        if !cfg!(target_arch = "x86_64")
            || !unsafe_field_rmw_inline_enabled()
            || !unsafe_static_rmw_inline_enabled()
            || crate::x64::zgc_read_barrier_blocks_inline_fields()
        {
            return;
        }
        let t = DirectHelperTable {
            unsafe_field_cells: 0x8000 | UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG,
            ..DirectHelperTable::EMPTY
        };
        assert_eq!(unsafe_static_arm_table(&t), Some(0x8000));
    }

    /// UNSAFE21-2: the four sub-word rows and their admission bits, above
    /// every older row's; near misses are not sites.
    #[test]
    fn subword_sites_and_bits() {
        if !unsafe_subword_rmw_inline_enabled() {
            return;
        }
        let u = "jdk/internal/misc/Unsafe";
        let site = |m: &str, d: &str| unsafe_inline_subword_site(u, m, d);
        let cae_b = site("compareAndExchangeByte", "(Ljava/lang/Object;JBB)B").unwrap();
        let cae_s = site("compareAndExchangeShort", "(Ljava/lang/Object;JSS)S").unwrap();
        let cas_s = site("compareAndSetShort", "(Ljava/lang/Object;JSS)Z").unwrap();
        let cas_b = site("compareAndSetByte", "(Ljava/lang/Object;JBB)Z").unwrap();
        for (i, r) in [cae_b, cae_s, cas_s, cas_b].into_iter().enumerate() {
            assert_eq!(r.row(), i);
            let (m, d) = UnsafeSubwordRmw::GATE_BITS[i];
            assert_eq!(site(m, d), Some(r));
            assert!(r.admitted(1 << (UnsafeSubwordRmw::FIRST_BIT + i)));
            assert!(!r.admitted(!(1 << (UnsafeSubwordRmw::FIRST_BIT + i))));
        }
        for bit in (0..UNSAFE_QU_EXTRA_FIRST_BIT + UNSAFE_QU_EXTRA_GATE_BITS.len()).chain([
            UNSAFE_REF_CAS_GATE_BIT,
            UNSAFE_REF_VOLATILE_GET_GATE_BIT,
            UNSAFE_REF_VOLATILE_PUT_GATE_BIT,
            UNSAFE_FIELD_ARM_BAKED_BIT,
            UNSAFE_REF_GET_AND_SET_GATE_BIT,
        ]) {
            assert!(bit < UnsafeSubwordRmw::FIRST_BIT, "bit {bit}");
        }
        assert!(site("compareAndExchangeChar", "(Ljava/lang/Object;JCC)C").is_none());
        assert!(site("compareAndExchangeByte", "(Ljava/lang/Object;JII)I").is_none());
        assert!(unsafe_inline_subword_site(
            "sun/misc/Unsafe",
            "compareAndExchangeByte",
            "(Ljava/lang/Object;JBB)B"
        )
        .is_none());
    }

    /// UNSAFE21-2: the CMPXCHG of the access width, then the witness
    /// sign-extended or the `0` / `1`.
    #[test]
    fn subword_rmw_bytes() {
        let r = |width, cae| unsafe_inline_subword_rmw_bytes(UnsafeSubwordRmw { width, cae });
        assert_eq!(
            r(1, true),
            vec![0x49, 0x92, 0xF0, 0x43, 0x0F, 0xB0, 0x14, 0x1A, 0x48, 0x0F, 0xBE, 0xC0]
        );
        assert_eq!(
            r(2, true),
            vec![0x49, 0x92, 0xF0, 0x66, 0x43, 0x0F, 0xB1, 0x14, 0x1A, 0x48, 0x0F, 0xBF, 0xC0]
        );
        assert_eq!(
            r(2, false),
            vec![
                0x49, 0x92, 0xF0, 0x66, 0x43, 0x0F, 0xB1, 0x14, 0x1A, 0x0F, 0x94, 0xC0, 0x0F, 0xB6,
                0xC0
            ]
        );
        assert_eq!(
            r(1, false),
            vec![
                0x49, 0x92, 0xF0, 0x43, 0x0F, 0xB0, 0x14, 0x1A, 0x0F, 0x94, 0xC0, 0x0F, 0xB6, 0xC0
            ]
        );
    }

    /// UNSAFE21-2: the sub-word screen reads its own half (a `disp32` cell
    /// load at `UNSAFE_SUBWORD_FIELD_CELL_BASE`) and keys a `short` access
    /// with the low key bit.
    #[test]
    fn subword_screen_reads_its_half() {
        let (b1, e1) = unsafe_inline_subword_field_cell_bytes(0x1000, 0x2000, 1);
        let (b2, e2) = unsafe_inline_subword_field_cell_bytes(0x1000, 0x2000, 2);
        assert_eq!(e1.len(), e2.len());
        let mut load = vec![0x4F, 0x8B, 0x9C, 0xDA];
        load.extend_from_slice(&UNSAFE_SUBWORD_FIELD_CELL_BASE.to_le_bytes());
        for b in [&b1, &b2] {
            assert!(b.windows(load.len()).any(|w| w == &load[..]));
        }
        assert!(b2.windows(3).any(|w| w == [0x83, 0xCA, 0x01]), "OR EDX, 1");
        assert_eq!(
            unsafe_subword_field_arm_words(
                &DirectHelperTable::EMPTY,
                UnsafeSubwordRmw {
                    width: 1,
                    cae: true
                }
            ),
            None
        );
    }
}

#[cfg(test)]
mod jr21_unsafe_byte_offset_cell_tests {
    use super::*;

    /// JIT round 21, lane unsafe: a tagged cell-table word adds the byte
    /// offset decode (SUB 16 / TEST 7 / JNZ / SHR 3) ahead of the slot
    /// screen, strips the tag from the baked table address, and leaves the
    /// untagged screen byte for byte as before.
    #[test]
    fn a_tagged_table_word_decodes_the_byte_offset_first() {
        let cells = 0x1122_3344_5566usize;
        let (plain, plain_edges) = unsafe_inline_field_cell_bytes(cells, 0x7788, 4);
        let (tagged, tagged_edges) =
            unsafe_inline_field_cell_bytes(cells | UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG, 0x7788, 4);
        let prefix: [u8; 18] = [
            0x49, 0x83, 0xEB, 0x10, // SUB R11, 16
            0x41, 0xF6, 0xC3, 0x07, // TEST R11B, 7
            0x0F, 0x85, 0, 0, 0, 0, // JNZ rel32
            0x49, 0xC1, 0xEB, 0x03, // SHR R11, 3
        ];
        assert_eq!(&tagged[..18], &prefix);
        assert_eq!(&tagged[18..], &plain[..], "the slot screen is unchanged");
        assert_eq!(tagged_edges.len(), plain_edges.len() + 1);
        assert_eq!(tagged_edges[0], 10);
        let has = |b: &[u8], n: &[u8]| b.windows(n.len()).any(|w| w == n);
        assert!(
            has(
                &tagged,
                &[0x49, 0xBA, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0, 0]
            ),
            "MOV R10, cells (the tag stripped)"
        );
        let (ref_tagged, _) =
            unsafe_inline_ref_field_cell_bytes(cells | UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG, 0x7788);
        assert_eq!(&ref_tagged[..18], &prefix);
    }
}

/// JIT round 21, lane refs2: [`DirectHelperTable::reference_referent_index`]
/// asks the layout producer with [`REFERENCE_REFERENT_QUERY_WORDS`] and takes
/// only that exact answer.
#[cfg(test)]
mod jr21_refs2_referent_query_tests {
    use super::*;

    unsafe extern "C" fn referent_only(ctx: i64, out: i64, n: i64) -> i64 {
        if ctx != 0x1000 || n != REFERENCE_REFERENT_QUERY_WORDS as i64 {
            return 0;
        }
        (out as *mut u64).write(0);
        1
    }

    unsafe extern "C" fn decline(_ctx: i64, _out: i64, _n: i64) -> i64 {
        0
    }

    fn table(f: DirectHelperFnThreadLocalIrLayout, ctx: usize) -> DirectHelperTable {
        DirectHelperTable {
            thread_local_ir_layout: f as *const () as usize,
            static_base_resolver_ctx: ctx,
            ..DirectHelperTable::default()
        }
    }

    #[test]
    fn jr21_refs2_the_referent_query() {
        assert_eq!(
            table(referent_only, 0x1000).reference_referent_index(),
            Some(0)
        );
        assert_eq!(table(decline, 0x1000).reference_referent_index(), None);
        assert_eq!(table(referent_only, 0).reference_referent_index(), None);
        // The full layout is not the referent query's answer.
        assert_eq!(
            table(referent_only, 0x1000).thread_local_ir_layout_words(),
            None
        );
    }
}

/// JIT round 21, lane tlsub (TL321-4): [`DirectHelperTable::thread_local_receiver_kind`]
/// hands the class id in word 0, asks with [`THREAD_LOCAL_RECEIVER_QUERY_WORDS`]
/// and takes only that exact answer.
#[cfg(test)]
mod jr21_tlsub_receiver_query_tests {
    use super::*;

    unsafe extern "C" fn answer(ctx: i64, out: i64, n: i64) -> i64 {
        if ctx != 0x1000 || n != THREAD_LOCAL_RECEIVER_QUERY_WORDS as i64 {
            return 0;
        }
        let io = out as *mut u64;
        // Class 77 reads the plain map and keeps `set`; 78 reads the plain
        // map but declares its own `set`; anything else: its bytecode.
        let (kind, set_ok) = match io.read() {
            77 => (u64::from(TL_RECEIVER_KIND_PLAIN), 1),
            78 => (u64::from(TL_RECEIVER_KIND_PLAIN), 0),
            _ => (2, 0),
        };
        io.write(kind);
        io.add(1).write(set_ok);
        2
    }

    unsafe extern "C" fn decline(_ctx: i64, _out: i64, _n: i64) -> i64 {
        0
    }

    fn table(f: DirectHelperFnThreadLocalIrLayout, ctx: usize) -> DirectHelperTable {
        DirectHelperTable {
            thread_local_ir_layout: f as *const () as usize,
            static_base_resolver_ctx: ctx,
            ..DirectHelperTable::default()
        }
    }

    #[test]
    fn jr21_tlsub_the_receiver_query() {
        let t = table(answer, 0x1000);
        assert_eq!(
            t.thread_local_receiver_kind(77),
            Some((TL_RECEIVER_KIND_PLAIN, true))
        );
        assert_eq!(
            t.thread_local_receiver_kind(78),
            Some((TL_RECEIVER_KIND_PLAIN, false))
        );
        assert_eq!(t.thread_local_receiver_kind(5), Some((2, false)));
        assert_eq!(
            t.thread_local_receiver_kind(0),
            None,
            "no class, no question"
        );
        assert_eq!(table(decline, 0x1000).thread_local_receiver_kind(77), None);
        assert_eq!(table(answer, 0).thread_local_receiver_kind(77), None);
        assert_eq!(
            DirectHelperTable::default().thread_local_receiver_kind(77),
            None
        );
    }
}
