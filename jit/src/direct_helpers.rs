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
    };

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
        // same signature.
        let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = unsafe { std::mem::transmute(raw) };
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
        // SAFETY: the VM fills this field from a `fn(u32) -> bool` item with
        // process lifetime (`direct_helper_table`), and nothing else writes it.
        let f: fn(u32) -> bool = unsafe { std::mem::transmute(raw) };
        f(class_id)
    }

    /// Would the thin `ByteBuffer` element helper SERVE a receiver of
    /// `class_id`? `false` whenever it cannot prove `true`: an unwired
    /// predicate, class id 0, or a class the funnel has not served.
    pub fn nio_byte_element_class_is_served(&self, class_id: u32, write: bool) -> bool {
        let raw = self.nio_byte_element_served_class;
        if raw == 0 || class_id == 0 {
            return false;
        }
        // SAFETY: the VM fills this field from a `fn(u32, bool) -> bool` item
        // with process lifetime (`direct_helper_table`), and nothing else
        // writes it.
        let f: fn(u32, bool) -> bool = unsafe { std::mem::transmute(raw) };
        f(class_id, write)
    }
}

/// A classified `ScopedMemoryAccess.*Unaligned` operation.
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
}

impl ScopedMemoryOp {
    /// Number of parameters excluding receiver.
    pub const fn num_params(self) -> usize {
        match self {
            Self::GetIntUnaligned
            | Self::GetShortUnaligned
            | Self::GetLongUnaligned
            | Self::GetCharUnaligned => 4,
            Self::PutIntUnaligned
            | Self::PutShortUnaligned
            | Self::PutLongUnaligned
            | Self::PutCharUnaligned => 5,
        }
    }

    /// Return type byte.
    pub const fn return_type(self) -> u8 {
        match self {
            Self::GetIntUnaligned => b'I',
            Self::GetShortUnaligned => b'S',
            Self::GetLongUnaligned => b'J',
            Self::GetCharUnaligned => b'C',
            Self::PutIntUnaligned
            | Self::PutShortUnaligned
            | Self::PutLongUnaligned
            | Self::PutCharUnaligned => b'V',
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
            "getIntUnaligned" | "getIntUnalignedInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)I",
        ) => Some(ScopedMemoryOp::GetIntUnaligned),
        (
            "getShortUnaligned" | "getShortUnalignedInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)S",
        ) => Some(ScopedMemoryOp::GetShortUnaligned),
        (
            "getLongUnaligned" | "getLongUnalignedInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)J",
        ) => Some(ScopedMemoryOp::GetLongUnaligned),
        (
            "getCharUnaligned" | "getCharUnalignedInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)C",
        ) => Some(ScopedMemoryOp::GetCharUnaligned),
        (
            "putIntUnaligned" | "putIntUnalignedInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JIZ)V",
        ) => Some(ScopedMemoryOp::PutIntUnaligned),
        (
            "putShortUnaligned" | "putShortUnalignedInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSZ)V",
        ) => Some(ScopedMemoryOp::PutShortUnaligned),
        (
            "putLongUnaligned" | "putLongUnalignedInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJZ)V",
        ) => Some(ScopedMemoryOp::PutLongUnaligned),
        (
            "putCharUnaligned" | "putCharUnalignedInternal",
            "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCZ)V",
        ) => Some(ScopedMemoryOp::PutCharUnaligned),
        _ => None,
    }
}
