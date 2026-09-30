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
    /// write-up (`docs/internal/jdk-only/heap-bytebuffer-and-chm-run-interpreted-
    /// under-jdk-only-20260918.md`), "What landed" section 4. `0` = not wired.
    pub unsafe_get_byte: usize,
    pub unsafe_get_short: usize,
    pub unsafe_get_int: usize,
    pub unsafe_get_long: usize,
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

    /// `jdk/internal/util/Preconditions.checkIndex(II[BiFunction])I` thin
    /// direct-call helper. Top of the `--dump-native-registry` invocation census on
    /// `probes/NioAccessorRate.java`: 4 000 000 calls for 800 000 `ByteBuffer`
    /// accessor operations, ahead of the store itself. `0` = not wired.
    pub preconditions_check_index: usize,

    /// `java/lang/ref/Reference.reachabilityFence(Object)V` thin direct-call
    /// helper. Second on the same census (3 200 000 calls), and its registered
    /// native does nothing but be opaque about its argument. `0` = not wired.
    pub reachability_fence: usize,

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
}

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
        unsafe_put_byte: 0,
        unsafe_put_short: 0,
        unsafe_put_int: 0,
        unsafe_put_long: 0,
        unsafe_cas_int: 0,
        unsafe_cas_long: 0,
        buffer_session_served_class: 0,
        nio_byte_element_served_class: 0,
        md_update_byte: 0,
        thread_current_thread: 0,
        monitor_enter: 0,
        monitor_exit: 0,
        preconditions_check_index: 0,
        reachability_fence: 0,
        indy_bridge: 0,
        varhandle_read: [0; crate::VARHANDLE_READ_SLOTS],
        varhandle_write: [0; crate::VARHANDLE_WRITE_SLOTS],
        varhandle_cas: [0; crate::VARHANDLE_CAS_SLOTS],
        static_base_resolver: 0,
        static_base_resolver_ctx: 0,
        arm_savebase_watch: 0,
        disarm_savebase_watch: 0,
        class_initialized: 0,
        well_known_class_id: 0,
    };

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
    /// four). Today: the two monitor helpers and the two savebase watchpoints.
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

    // (vm_ptr, index, length, formatter).
    DirectHelperFnPreconditionsCheckIndex, preconditions_check_index, JavaArgMarshalled,
        (i64, i64, i64, i64) -> i64;
    // (vm_ptr, referent) -> 0. Java-`void`, but NOT `-> ()`: every
    // `JavaArgMarshalled` slot is reached through the `JitDirectCall` sequence,
    // which compares RAX against the `i64::MIN` sentinel after the CALL even
    // for a `V` site, so a `()` return made that compare read whatever the
    // callee last left in RAX. Pinned by
    // `every_java_arg_marshalled_slot_returns_a_word` below.
    DirectHelperFnReachabilityFence, reachability_fence, JavaArgMarshalled, (i64, i64) -> i64;

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
        DIRECT_HELPERS_NEEDING_WIN64_STACK_ARGS == 23,
        "the number of direct helpers spilling arguments on Win64 changed. The \
         twenty-three are the eight ScopedMemoryAccess accessors, their eight \
         *Internal twins, varhandle_cas, the four Unsafe \
         put{{Byte,Short,Int,Long}} accessors and the two Unsafe \
         compareAndSet{{Int,Long}} forms, all reached through \
         JitDirectCall's `emit_stack_arg_setup`. A new one is fine on that \
         route and is NOT fine on the hand-written route — say which by \
         giving the new row its DirectHelperCallPath, then update this count.",
    );
    assert!(
        DIRECT_HELPERS_NEEDING_SYSV_STACK_ARGS == 8,
        "the number of direct helpers spilling arguments on SysV changed. The \
         eight are the ScopedMemoryAccess `put` accessors and their `*Internal` \
         twins, at seven words each.",
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
// 142 words = 34 scalar slots + 40 read + 40 write + 10 CAS + 8 Unsafe scalar
// slots (get{Byte,Short,Int,Long} + put{Byte,Short,Int,Long}) + 8
// ScopedMemoryAccess *Internal twins + 2 Unsafe compareAndSet{Int,Long}.
// The 33rd scalar slot is `class_initialized` (round 11 wave 4) and the 34th
// `well_known_class_id` (round 11 wave 12): CompilerOnly predicates called by
// the compiler through a transmute, never baked into a CALL, so neither has a
// signature row (the latter's convention is pinned by
// `DirectHelperFnWellKnownClassId` and the producer-side coercion in
// `vm/src/jit/helpers.rs`).
// 49 signature rows = 39 + 8 (one per *Internal slot) + 2 (one per
// compareAndSet slot; six words each, so no SysV spill and a two-word Win64
// one). Rows are per FIELD, not per array element: the `R` kind round 11
// wave 18 added to the VarHandle write (+4) and CAS (+1) arrays grew the word
// count and left the row count alone.
const _: () = {
    assert!(
        core::mem::size_of::<DirectHelperTable>() == 142 * core::mem::size_of::<usize>(),
        "`DirectHelperTable` changed shape. Every slot the emitter can bake \
         into a CALL needs a `direct_helper_fn_slots!` row carrying its real C \
         signature, or it is back to being a raw `usize` that type-checks \
         against nothing — which is the entire reason this table's ABI \
         description exists. Add the row (and the producer-side coercion check \
         described in docs/jit/helper-abi.md §10), then update this word count \
         and the row count below.",
    );
    assert!(
        DIRECT_HELPER_FN_SIGS.len() == 49,
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
pub const DIRECT_HELPER_ABI_VERSION: u32 = 4;

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
        (
            "getIntUnaligned",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)I",
        ) => Some(ScopedMemoryOp::GetIntUnaligned),
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
        (
            "getLongUnaligned",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)J",
        ) => Some(ScopedMemoryOp::GetLongUnaligned),
        (
            "getLongUnalignedInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)J",
        ) => Some(ScopedMemoryOp::GetLongUnalignedInternal),
        (
            "getCharUnaligned",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)C",
        ) => Some(ScopedMemoryOp::GetCharUnaligned),
        (
            "getCharUnalignedInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)C",
        ) => Some(ScopedMemoryOp::GetCharUnalignedInternal),
        (
            "putIntUnaligned",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JIZ)V",
        ) => Some(ScopedMemoryOp::PutIntUnaligned),
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
}

impl UnsafeAccessorOp {
    /// Number of parameters excluding receiver: `(obj, offset)` for a get,
    /// `(obj, offset, value)` for a put, `(obj, offset, expected, new)` for a
    /// compare-and-set.
    pub const fn num_params(self) -> usize {
        match self {
            Self::GetByte | Self::GetShort | Self::GetInt | Self::GetLong => 2,
            Self::PutByte | Self::PutShort | Self::PutInt | Self::PutLong => 3,
            Self::CasInt | Self::CasLong => 4,
        }
    }

    /// Return type byte.
    pub const fn return_type(self) -> u8 {
        match self {
            Self::GetByte => b'B',
            Self::GetShort => b'S',
            Self::GetInt => b'I',
            Self::GetLong => b'J',
            Self::PutByte | Self::PutShort | Self::PutInt | Self::PutLong => b'V',
            Self::CasInt | Self::CasLong => b'Z',
        }
    }

    /// The compare-and-set pair, which is counted and switched apart from the
    /// eight accessors: `CRATONVM_JIT_UNSAFE_CAS_DIRECT` and the
    /// `UNSAFE_CAS_*` census rather than the accessors' own.
    pub const fn is_cas(self) -> bool {
        matches!(self, Self::CasInt | Self::CasLong)
    }

    /// Whether this op's own switch is on. The doors ask this rather than
    /// `unsafe_accessor_direct_enabled` directly, so switching the accessors
    /// off does not switch off the CAS and vice versa.
    pub fn direct_enabled(self) -> bool {
        if self.is_cas() {
            crate::unsafe_cas_direct_enabled()
        } else {
            crate::unsafe_accessor_direct_enabled()
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
            Self::PutByte => table.unsafe_put_byte,
            Self::PutShort => table.unsafe_put_short,
            Self::PutInt => table.unsafe_put_int,
            Self::PutLong => table.unsafe_put_long,
            Self::CasInt => table.unsafe_cas_int,
            Self::CasLong => table.unsafe_cas_long,
        }
    }
}

/// Recognise a plain `jdk/internal/misc/Unsafe` scalar accessor call site by
/// class, method name and descriptor. Only the aligned, no-`bigEndian`,
/// `(Object,long[,V])` shapes -- the `*Unaligned` family is
/// [`is_scoped_memory_op`]'s sibling on `ScopedMemoryAccess` -- plus the two
/// `compareAndSet{Int,Long}` forms. The `(J)X` off-heap-only overloads,
/// `compareAndSetReference` (a reference CAS owes SATB and card barriers this
/// bind does not model), and the weak/exchange/volatile/opaque/acquire/release
/// family are out of scope.
pub fn is_unsafe_accessor_op(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<UnsafeAccessorOp> {
    if class_name != "jdk/internal/misc/Unsafe" {
        return None;
    }
    match (method_name, descriptor) {
        ("getByte", "(Ljava/lang/Object;J)B") => Some(UnsafeAccessorOp::GetByte),
        ("getShort", "(Ljava/lang/Object;J)S") => Some(UnsafeAccessorOp::GetShort),
        ("getInt", "(Ljava/lang/Object;J)I") => Some(UnsafeAccessorOp::GetInt),
        ("getLong", "(Ljava/lang/Object;J)J") => Some(UnsafeAccessorOp::GetLong),
        ("putByte", "(Ljava/lang/Object;JB)V") => Some(UnsafeAccessorOp::PutByte),
        ("putShort", "(Ljava/lang/Object;JS)V") => Some(UnsafeAccessorOp::PutShort),
        ("putInt", "(Ljava/lang/Object;JI)V") => Some(UnsafeAccessorOp::PutInt),
        ("putLong", "(Ljava/lang/Object;JJ)V") => Some(UnsafeAccessorOp::PutLong),
        ("compareAndSetInt", "(Ljava/lang/Object;JII)Z") => Some(UnsafeAccessorOp::CasInt),
        ("compareAndSetLong", "(Ljava/lang/Object;JJJ)Z") => Some(UnsafeAccessorOp::CasLong),
        _ => None,
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

    /// The four hand-written slots are the ones with no stack-argument path
    /// anywhere, so the set is worth naming rather than only counting: a fifth
    /// member would be a helper the emitter calls with registers it never
    /// loads, and the census assertion alone cannot say which one is new.
    #[test]
    fn the_hand_written_call_path_has_exactly_the_four_known_members() {
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
                "monitor_enter",
                "monitor_exit",
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
