// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `ldc` / `ldc2_w` and loader-faithful `CONSTANT_Class` resolution.
//!
//! Moved verbatim out of `interpreter.rs`'s `Helper: LDC / LDC_W (load constant from pool)`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.
//!
//! # The seven `ldc` operand kinds, and the twins that shadow them
//!
//! JVMS §6.5 lets `ldc`/`ldc_w` load `CONSTANT_Integer`, `CONSTANT_Float`,
//! `CONSTANT_String`, `CONSTANT_Class`, `CONSTANT_MethodType`,
//! `CONSTANT_MethodHandle` and (Java 11+) a category-1 `CONSTANT_Dynamic`;
//! `ldc2_w` loads `CONSTANT_Long`, `CONSTANT_Double` and a category-2
//! `CONSTANT_Dynamic`. `classloading::verify_insn`'s `verify_ldc` /
//! `verify_ldc2w` have accepted all of those for some time. Until 2026-08-12
//! this module refused `CONSTANT_MethodType`, `CONSTANT_MethodHandle` and the
//! category-2 `CONSTANT_Dynamic` — a verifier/interpreter split where a class
//! file passed verification and then died at execution with
//! `ClassFormatError: ldc: unsupported constant pool entry type`. The real
//! `java.lang.invoke.MethodHandleProxies.asInterfaceInstance` proxy template
//! is the reachable case: the class it spins does
//! `callerBoundTarget.asType(<MethodType>)` off an `ldc`.
//!
//! Three other implementations of the *same* JVMS rules already existed, and
//! the arms below deliberately call into them instead of restating them:
//!
//! * `runtime::invokedynamic::resolve_method_handle_full` — the
//!   `CONSTANT_MethodHandle` decode (JVMS Table 5.4.3.5-A: which reference
//!   kinds read a `Fieldref` and which read a `Methodref`).
//! * `native_builtins::lang_invoke::build_method_type_from_descriptor` — the
//!   descriptor → `MethodType` construction, used by `invokedynamic`'s
//!   bootstrap-argument path.
//! * `native_builtins::phases_late::reflect_invoke` — the
//!   "prefer `MethodType.fromMethodDescriptorString`, fall back to the VM
//!   builder" ordering, reproduced here rather than reinvented.
//!
//! Materialising a `MethodHandle` goes through `MethodHandles.Lookup.find*`
//! for the same reason: the VM has a second, native-only handle representation
//! (`lang_invoke`'s `MH_KIND_*` shim objects) that is refused under
//! `--jdk-only`, and hardwiring either representation into the opcode would
//! create a fourth twin.

use super::*;

use crate::runtime::resolve::MemberResolver;

use super::site_cache::{PrimitiveConstant, PrimitiveConstantSiteCache};

/// B5: convert a malformed-constant-pool `ClassFormatError` (produced by
/// `execute_ldc`/`execute_ldc2w` on a bad CP index or wrong-type entry) into a
/// catchable Java `java/lang/ClassFormatError`. The verifier normally prevents
/// this, so it is defense-in-depth for `skip_verification` + untrusted
/// classfiles: instead of hard-unwinding via an uncatchable `VmError::Internal`,
/// Java `catch (ClassFormatError)` / `catch (LinkageError)` / `catch (Throwable)`
/// can observe it. Non-`ClassFormatError` failures pass through unchanged.
#[cold]
pub(super) fn convert_ldc_class_format_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    err: MethodCallFailed,
) -> MethodCallFailed {
    if let MethodCallFailed::InternalError(VmError::Linkage(LinkageError::ClassFormatError {
        ref message,
        ..
    })) = err
    {
        match crate::runtime::exceptions::create_exception_object(
            shared,
            thread,
            "java/lang/ClassFormatError",
            Some(message),
        ) {
            Ok(obj_ref) => return MethodCallFailed::ExceptionThrown(obj_ref),
            // Heap-exhausted / rt.jar absent during exception construction —
            // fall through to the original error so we never lose the diagnostic.
            Err(_) => return err,
        }
    }
    err
}

/// Canonical instance for a surrogate-bearing string **literal**.
///
/// JVMS §5.1: every string literal is interned, so two `ldc`s of the same
/// `CONSTANT_String` must push the identical reference and `LITERAL ==
/// LITERAL.intern()` must hold. The ordinary literal path gets that from
/// `create_java_string`, which pools on the Rust `String`. A literal carrying
/// a lone surrogate (ANTLR's `_serializedATN` is the reachable case) cannot be
/// keyed that way — a Rust `String` cannot hold one — and the wide path called
/// `create_java_string_from_units`, which pools nothing and allocates a FRESH
/// object per execution. So `"\uD800" == "\uD800"` answered `false` where
/// HotSpot answers `true`, and every execution of such an `ldc` leaked another
/// String onto the heap.
///
/// The table is `String.intern()`'s own surrogate pool, reached through its
/// two published halves. It has to be that table and not a second one, or
/// `LITERAL == LITERAL.intern()` would still answer `false`.
///
/// Root discipline follows `intern_unrepresentable`: the probe comes first (a
/// hit allocates nothing at all), and a loser releases its own root rather
/// than holding it for the life of the VM.
///
/// Under `new`'s heap-exhaustion contract: collect, retry, then a catchable
/// `OutOfMemoryError` instead of aborting the process (see
/// `intern_string_literal_or_oom`). `ldc` and a condy's String static argument
/// use it. gc-common w9-c: the infallible twin (`intern_wide_string_literal`,
/// through `create_java_string_from_units`) lost its last caller, the condy
/// argument, and was removed
/// (`common-w8v-constant-resolution-strings-abort-on-a-full-heap`).
///
/// gci i32/sl: this is the STRONG-row form. `ldc` takes
/// [`intern_wide_string_literal_or_oom_as`] with
/// [`surrogate_literal_class_held`] and records what it pushes; since gci
/// i33/sc a condy static argument does the same at its own entry
/// ([`condy_wide_string_arg`]) and keeps this form only under either kill
/// switch (`CRATONVM_GC_CONDY_STRING_ARG_RECORD=0`,
/// `CRATONVM_GC_SURROGATE_LITERAL_CLASS_HELD=0`).
fn intern_wide_string_literal_or_oom(
    shared: &SharedVm,
    thread: &mut JvmThread,
    units: &[u16],
) -> Result<ObjectRef, MethodCallFailed> {
    intern_wide_string_literal_or_oom_as(shared, thread, units, false)
}

/// [`intern_wide_string_literal_or_oom`] with the row disposition explicit.
///
/// gci i32/sl (`gci-i20mr-merge-review-findings-FIXED-20261016` finding 10): with
/// `class_held` the pool row stays WEAK -- a hit is resolved with the
/// keep-alive a weak read owes and is never upgraded, and a claim files a weak
/// handle -- and the caller (`ldc`, [`record_wide_string_literal_as_of`])
/// records the literal in its class's never-evicted constant record, which is
/// HotSpot's resolved-references array: a loader-conditional root
/// (`roots::collect_roots` step 13) that the class-unload transaction drops
/// (`gc::unload_dead_class_metadata`, `ResolutionCache::forget_classes`). So
/// the literal lives exactly as long as some class that `ldc`'d it (or
/// anything else that references it), and an unloaded class's literal is
/// collected and its weak row cleared, as on HotSpot. Without `class_held`
/// the row is made STRONG (the 2026-10-15 merge's design: immortal for the
/// VM's life).
fn intern_wide_string_literal_or_oom_as(
    shared: &SharedVm,
    thread: &mut JvmThread,
    units: &[u16],
    class_held: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(obj) = pooled_wide_string_literal(shared, units, class_held) {
        return Ok(obj);
    }
    let fresh = create_string_from_units_or_oom(shared, thread, units)?;
    Ok(claim_wide_string_literal(shared, units, fresh, class_held))
}

/// gci i32/sl kill switch, `CRATONVM_GC_SURROGATE_LITERAL_CLASS_HELD` (default
/// ON; `=0` restores the strong literal row). See
/// [`intern_wide_string_literal_or_oom_as`].
fn surrogate_literal_class_held() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GC_SURROGATE_LITERAL_CLASS_HELD")
}

/// gci i39/rw (`gci-i39rw-lone-surrogate-constantvalue-string-is-not-the-literal-FIXED-20261016`):
/// a lone-surrogate `ConstantValue` String (`vm_util::prepare_class_shared`)
/// is the literal -- the pooled instance if one exists, else a fresh one
/// claimed in the surrogate pool -- as HotSpot's `string_initial_value`
/// interns it, so `Field.get`, an `ldc` of the text and `intern()` answer one
/// object. The static field is the class-lifetime holder (HotSpot's mirror
/// field), so under [`surrogate_literal_class_held`] the row stays weak, as
/// `ldc`'s. Collection-free (the `try_` allocation, the probe and the claim):
/// `None` on a full heap, the caller's `OutOfMemoryError` as before.
pub(crate) fn intern_wide_string_constant_value(
    shared: &SharedVm,
    units: &[u16],
) -> Option<ObjectRef> {
    let class_held = surrogate_literal_class_held();
    if let Some(obj) = pooled_wide_string_literal(shared, units, class_held) {
        return Some(obj);
    }
    let fresh = crate::vm::try_create_java_string_from_units(shared, units)?;
    Some(claim_wide_string_literal(shared, units, fresh, class_held))
}

/// gci i39/rw kill switch, `CRATONVM_GC_SURROGATE_CONSTANT_VALUE_INTERNED`
/// (default ON; `=0` restores the unpooled `ConstantValue` String). Read once
/// per lone-surrogate `ConstantValue` field at preparation.
pub(crate) fn surrogate_constant_value_interned() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GC_SURROGATE_CONSTANT_VALUE_INTERNED")
}

/// gci i32/sl: record an `ldc`'d lone-surrogate literal in its class's
/// never-evicted constant record and answer the value to push (the record's,
/// insert-if-absent). With `class_held` off, the ordinary
/// [`record_cp_constant_if_enabled`] rule, and `value`.
///
/// Under `class_held` the record is what keeps the literal alive (its pool
/// row is weak), so it is written whatever the `ldc` cache switch says and
/// whatever the String table's disposition (the FIFO-capped store could evict
/// it while the class lives). With the cache off nothing reads the record
/// (gcg g3/rw's rule): it is written once (a READ probe first, no
/// `LDC_FILL` count) and `value` is pushed.
fn record_wide_string_literal_as_of(
    shared: &SharedVm,
    class_id: ClassId,
    cp_index: u16,
    value: Value,
    as_of: u64,
    class_held: bool,
) -> Value {
    if !class_held {
        record_cp_constant_if_enabled(shared, class_id, cp_index, value, as_of);
        return value;
    }
    if ldc_const_cache_enabled(shared) {
        return record_cp_constant_permanent_as_of(shared, class_id, cp_index, value, as_of);
    }
    if cached_cp_constant(shared, class_id, cp_index).is_none() {
        let resolver = MemberResolver::new(shared);
        let caller = resolver.scope(class_id);
        let scoped = resolver.scope(value);
        let _ = resolver.record_constant_permanent_as_of(caller, cp_index, scoped, as_of);
    }
    value
}

/// The probe half of [`intern_wide_string_literal_or_oom`]: the pooled instance, if
/// one was already claimed. Allocates nothing.
fn pooled_wide_string_literal(
    shared: &SharedVm,
    units: &[u16],
    class_held: bool,
) -> Option<ObjectRef> {
    let winner =
        cratonvm_native_builtins::lang_string::surrogate_intern_probe(shared.vm_identity, units)?;
    if class_held {
        // gci i32/sl: the row keeps its disposition (weak for an `intern()` /
        // class-held row); the caller's record holds the literal.
        return crate::native::jni::resolve_global_handle_keeping_weak_alive(shared, winner);
    }
    strong_wide_string_literal(shared, units, winner)
}

/// The live instance `winner` names, with the pool row made STRONG first.
/// `None` when `winner` is a cleared weak row (its String died).
///
/// gcg g2/w item 2 (GC wave 9, lane gcD): `String.intern()` now pools a
/// lone-surrogate String under a WEAK handle (`lang_string::intern_unrepresentable`),
/// as HotSpot's weak `StringTable` does. A literal must not die while its
/// class lives (HotSpot's resolved references), so the first `ldc` that meets
/// a weak row swaps in a strong handle to the same object
/// (`surrogate_intern_replace`, a compare-and-swap) and releases the weak one.
/// Nothing here allocates on the Java heap.
///
/// Merge of origin/dev into the gci round (2026-10-15): gci i4/wr fixed the
/// same item with a weak LITERAL row under the weak String pool
/// (`surrogate_intern_replace_dead`), kept alive by its `ldc` record. This
/// design (strong literal rows, every collector) was kept as the sounder one:
/// on G1 / ZGC an `intern()` row is weak too and no `ldc` record keeps the
/// literal. The read still goes through i4/wr's
/// `resolve_global_handle_keeping_weak_alive` (the SATB keep-alive a weak
/// read owes, `NativeContext::add_weak_global_root`).
///
/// gci i32/sl: since then only the strong form's (`ldc` under
/// `CRATONVM_GC_SURROGATE_LITERAL_CLASS_HELD=0`, and a condy static argument
/// under that switch or, since gci i33/sc,
/// `CRATONVM_GC_CONDY_STRING_ARG_RECORD=0`); `ldc` keeps the row
/// weak and records the literal in its class instead (the per-class owner
/// the i20/mr finding 10 asked for, [`intern_wide_string_literal_or_oom_as`]).
fn strong_wide_string_literal(
    shared: &SharedVm,
    units: &[u16],
    winner: usize,
) -> Option<ObjectRef> {
    use crate::native::jni::JObject;
    use cratonvm_native_builtins::lang_string::surrogate_intern_replace;

    let obj = crate::native::jni::resolve_global_handle_keeping_weak_alive(shared, winner)?;
    // A handle another thread upgraded and released since the resolve reads
    // as not weak: the row is strong already, and `obj` was kept alive above.
    if !shared.natives.jni_global_refs.lock().is_weak(winner as JObject) {
        return Some(obj);
    }
    let strong = shared.natives.jni_global_refs.lock().add(obj) as usize;
    let now = surrogate_intern_replace(shared.vm_identity, units, winner, strong);
    let refs = &shared.natives.jni_global_refs;
    if now == strong {
        refs.lock().remove(winner as JObject);
        return Some(obj);
    }
    // Another thread changed the row first: answer what it holds now.
    refs.lock().remove(strong as JObject);
    crate::native::jni::resolve_global_handle_keeping_weak_alive(shared, now)
}

/// The claim half of [`intern_wide_string_literal_or_oom`]: root `fresh`, race to
/// claim the pool entry, and answer the winner (a loser releases its root).
///
/// gci i32/sl: under `class_held` the root is a WEAK global and a live row is
/// answered as it is (see [`intern_wide_string_literal_or_oom_as`]); nothing
/// collects between this claim and the caller's record (no allocation, no
/// safepoint), and a String allocated while a concurrent mark runs is live to
/// that mark.
fn claim_wide_string_literal(
    shared: &SharedVm,
    units: &[u16],
    fresh: ObjectRef,
    class_held: bool,
) -> ObjectRef {
    use crate::native::jni::JObject;
    use cratonvm_native_builtins::lang_string::{surrogate_intern_claim, surrogate_intern_replace};

    let live = |row: usize| {
        if class_held {
            crate::native::jni::resolve_global_handle_keeping_weak_alive(shared, row)
        } else {
            strong_wide_string_literal(shared, units, row)
        }
    };
    let refs = &shared.natives.jni_global_refs;
    let handle = if class_held {
        refs.lock().add_weak(fresh) as usize
    } else {
        refs.lock().add(fresh) as usize
    };
    let winner = surrogate_intern_claim(shared.vm_identity, units.to_vec(), handle);
    if winner == handle {
        return fresh;
    }
    if let Some(obj) = live(winner) {
        refs.lock().remove(handle as JObject);
        return obj;
    }
    // gcg g2/w item 2: a cleared weak row an `intern()` left behind. Take it
    // over with this literal's handle (strong, or weak under `class_held`);
    // the insert-if-absent claim above can never replace it.
    let now = surrogate_intern_replace(shared.vm_identity, units, winner, handle);
    if now == handle {
        refs.lock().remove(winner as JObject);
        return fresh;
    }
    refs.lock().remove(handle as JObject);
    live(now).unwrap_or(fresh)
}

/// zs s2/nd2 (`zgc-zr4ab-infallible-residuals-20261012.md` item 3): the
/// primitive mirror for the one-letter descriptor `desc` under
/// [`class_mirror_or_oom`]'s contract. `try_get_or_create_primitive_mirror`
/// answers `None` only on ZGC (elsewhere it is the infallible factory, as
/// before), so Generational and G1 are unchanged; the nine mirrors normally
/// exist from start-up (`pre_init_wrapper_type_fields`) and this is a cache
/// hit. The caller holds every other reference in a pinned slot (each of the
/// four sites sits beside a `class_mirror_or_oom` arm with the same rule).
fn primitive_mirror_or_oom(
    shared: &SharedVm,
    thread: &mut JvmThread,
    desc: &str,
    site: &'static str,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(mirror) = crate::vm::try_get_or_create_primitive_mirror(shared, desc) {
        return Ok(mirror);
    }
    if let Some(mirror) = super::gc_and_alloc::collect_and_retry(shared, thread, site, |sh| {
        crate::vm::try_get_or_create_primitive_mirror(sh, desc)
    }) {
        return Ok(mirror);
    }
    maybe_dump_heap_on_oom(shared, thread);
    Err(MethodCallFailed::InternalError(VmError::Runtime(
        RuntimeError::OutOfMemoryError {
            message: format!("Java heap space (primitive mirror, {site})"),
        },
    )))
}

/// The `java.lang.Class` mirror of `class_id` under `new`'s heap-exhaustion
/// contract: the collection-free attempt, then the shared ladder
/// (`collect_and_retry`: collect, overhead limit, soft references, G1's
/// last-ditch cycle, retry), then a catchable `OutOfMemoryError` -- never the
/// process abort of `get_or_create_class_mirror`'s infallible allocation. A
/// cached mirror is a read-lock hit and allocates nothing. `site` names the
/// resolution in the collection's cause and in the error's (VM-side) detail.
///
/// gc-common w9-c. `ldc` of a `CONSTANT_Class` had this ladder inline since
/// gen r4w5; the `CONSTANT_MethodHandle` resolution (the reference class, a
/// field accessor's type, `findSpecial`'s caller, the defining loader's
/// mirror), a condy's field type and `Class` static arguments, and an
/// `invokedynamic`'s `Class` static arguments still took the infallible
/// mirror, so the FIRST resolution naming a class whose mirror did not yet
/// exist aborted the process on a full heap
/// (`common-w8v-constant-resolution-strings-abort-on-a-full-heap`).
///
/// The caller must hold every other reference it needs in a pinned slot: a
/// collection here moves objects.
pub(crate) fn class_mirror_or_oom(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    site: &'static str,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(mirror) = crate::vm::try_get_or_create_class_mirror(shared, class_id) {
        return Ok(mirror);
    }
    if let Some(mirror) = super::gc_and_alloc::collect_and_retry(shared, thread, site, |sh| {
        crate::vm::try_get_or_create_class_mirror(sh, class_id)
    }) {
        return Ok(mirror);
    }
    maybe_dump_heap_on_oom(shared, thread);
    Err(MethodCallFailed::InternalError(VmError::Runtime(
        RuntimeError::OutOfMemoryError {
            // The parenthesised site detail never reaches Java -- see
            // `RuntimeError::as_java_throwable`.
            message: format!("Java heap space (class mirror, {site})"),
        },
    )))
}

/// The monitor of a `static synchronized` method (JVMS §2.11.10: its class's
/// `Class` mirror) under [`class_mirror_or_oom`]'s contract, for an invoke
/// door that has already popped the call's arguments into `args` -- a Rust
/// slice the collector does not scan.
///
/// A mirror that exists (every call after the first) or that fits without a
/// collection is the plain [`crate::vm::try_get_or_create_class_mirror`]
/// answer and pins nothing. Only when that declines are the arguments pinned
/// across the collecting ladder and read back, so `args` holds current
/// addresses on `Ok` and on `Err` alike.
///
/// gen r5w1/oom5, `gengc-r4w6-review6-class-mirror-creation-aborts-on-a-full-heap-outside-ldc-FIXED-20260927.md`
/// item 3: every `static synchronized` door took the infallible mirror, so
/// the FIRST call of such a method on a full heap aborted the process with
/// `FATAL: OutOfMemoryError: young gen exhausted` where HotSpot (which made
/// the mirror when it loaded the class) runs the call.
pub(crate) fn static_sync_monitor_or_oom(
    shared: &SharedVm,
    thread: &mut JvmThread,
    declaring_class_id: ClassId,
    args: &mut [Value],
) -> Result<ObjectRef, MethodCallFailed> {
    let mirror = match crate::vm::try_get_or_create_class_mirror(shared, declaring_class_id) {
        Some(mirror) => Ok(mirror),
        None => {
            let pins = super::field_access::InvokeArgsRootGuard::new(thread, args);
            let mirror = class_mirror_or_oom(
                shared,
                thread,
                declaring_class_id,
                "static-synchronized-monitor",
            );
            pins.refresh(args);
            drop(pins);
            mirror
        }
    };
    mirror
}

/// Whether `ldc` may answer from the recorded-resolution store, and `ldc` /
/// `ldc2_w` from the per-thread numeric table (`PrimitiveConstantSiteCache`).
///
/// Default-ON; `CRATONVM_JIT_NO_LDC_CONST_CACHE=1` opts out, which is what
/// makes the A/B a one-binary comparison rather than a cross-binary one.
///
/// Switched off under the three diagnostics whose output a hit would silently
/// remove — `CRATONVM_LDC_CLASSREF_TRACE` and `CRATONVM_DBG_TOARRAY` print on
/// the ClassRef arm, and `CRATONVM_DBG_REMAP_TRACE` records provenance there. A cache that
/// blinds the instrument someone turned on to watch it is worse than no cache.
///
/// Read once per VM into `VmConfig::ldc_const_cache` (interpreter round i2
/// wave 58, lane L7; a process `OnceLock` before, an acquire load per `ldc`).
/// The three diagnostics stay process-wide and are folded in at construction.
#[inline(always)]
fn ldc_const_cache_enabled(shared: &SharedVm) -> bool {
    shared.config.ldc_const_cache
}

/// The three DIAGNOSTIC switches that must see every `ldc` resolution happen,
/// and therefore forbid answering one from a record.
///
/// Split out so the COMPILED `ldc` helpers
/// (`vm::jit::helpers::jit_ldc_string_cp` / `jit_ldc_class_cp`) consult the
/// same predicate rather than a copy of it. They have their own on/off switch
/// — the two routes are separately measurable — but a trace that goes quiet on
/// one of them and not the other is not a switch, it is a bug in the
/// instrument.
pub(crate) fn ldc_const_cache_forced_off_by_diagnostics() -> bool {
    ldc_classref_trace() || crate::runtime::env_cache::dbg_toarray() || remap_trace_on()
}

/// `CRATONVM_LDC_CLASSREF_TRACE`, read once. The `ClassRef` arm of
/// [`execute_ldc`] used to read the variable on every resolution it made,
/// under the `class_manager` read lock — which with the constant cache off is
/// every `ldc` of a class literal.
fn ldc_classref_trace() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_LDC_CLASSREF_TRACE").is_some()
    })
}

/// [`record_cp_constant_as_of`], but only when the `ldc` constant cache is enabled.
///
/// The five tags that learned to record in this change go through here rather
/// than the raw recorder. Under `CRATONVM_JIT_NO_LDC_CONST_CACHE=1` the probe
/// at the top of [`execute_ldc`] is skipped, so an ungated record would make
/// every execution resolve AND take the resolution cache's WRITE lock — work
/// the pre-change interpreter never did. The switched-off arm has to reproduce
/// the old behaviour or the A/B measures the instrument instead of the change.
/// This was caught by the fill counter reading `hit=0 miss=0 fill=1329806`
/// under the kill switch, which is the shape of a cache that is not being read
/// and is still being written.
///
/// `MethodType` / `MethodHandle` deliberately do NOT come through here: they
/// recorded before this change, so the switched-off arm must keep doing it.
///
/// `as_of` is the `ResolutionCache::fill_snapshot` `execute_ldc` took before
/// it read the constant pool (interpreter round i1 wave 23, lane L5).
fn record_cp_constant_if_enabled(
    shared: &SharedVm,
    class_id: ClassId,
    cp_index: u16,
    value: Value,
    as_of: u64,
) {
    if ldc_const_cache_enabled(shared) {
        record_cp_constant_as_of(shared, class_id, cp_index, value, as_of);
    } else if weak_table_needs_the_record(shared, class_id, cp_index, &value) {
        // gcg g3/rw (`gcg-g2w-string-table-review-findings` item 3): under the
        // weak String table the record is what keeps a literal alive (HotSpot's
        // resolved-references array), whatever the cache switch says. With the
        // cache off nothing READS it (the probe at the top of `execute_ldc` is
        // skipped), so this changes no answer; it only keeps the literal from
        // being collected and re-created (a `WeakReference` to a literal must
        // not clear while its class lives). Counter-free: `LDC_FILL` counts
        // the cache's fills, and this is not one.
        store_recorded_cp_constant_as_of(shared, class_id, cp_index, value, as_of);
    }
}

/// Does a cache-off `ldc` still need to record `value` (gcg g3/rw)? Only a
/// reference constant, only under the weak String table
/// (`memory::string_table::weak_table_engaged`), and only the first time: an
/// existing record (insert-if-absent, the same object) answers no under the
/// resolution cache's READ lock, so a hot site pays one read probe per
/// execution, never the write lock.
pub(crate) fn weak_table_needs_the_record(
    shared: &SharedVm,
    class_id: ClassId,
    cp_index: u16,
    value: &Value,
) -> bool {
    matches!(value, Value::Object(Some(_)))
        && crate::memory::string_table::weak_table_engaged(shared)
        && cached_cp_constant(shared, class_id, cp_index).is_none()
}
/// `ldc` / `ldc_w`: the per-thread primitive-site hit; everything else is
/// [`execute_ldc_resolve`] (wave 27).
#[inline(never)]
pub(super) fn execute_ldc(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    index: u16,
) -> Result<(), MethodCallFailed> {
    let frame_class_id = thread.frames[frame_idx].class_id;

    // JVMS §5.4.3: a symbolic reference is resolved ONCE per constant-pool
    // entry and the result recorded; §5.1 says the same of a string literal.
    // The recorded value IS what this instruction pushes, so answering from
    // the record here — BEFORE the class_manager read lock — is the whole
    // instruction for every tag that has run once.
    //
    // Hoisted to the top deliberately. `CONSTANT_MethodType` /
    // `CONSTANT_MethodHandle` already probed this store, but they did it in
    // the second phase, after the lock had been taken and an `LdcValue` built
    // — so they skipped the JDK factory and paid the lock anyway. Every tag
    // now skips both.
    //
    // Category safety: a constant-pool index has exactly one tag, so a `Long`
    // or `Double` entry is only ever reachable through `ldc2_w`, which does
    // NOT probe here: it must push through `push_long`/`push_double` to keep
    // the CompactValue tag, and its only costly tag (condy) already caches
    // inside `resolve_condy_constant`. A category-2 condy reached by plain
    // `ldc` is ill-formed and pushes untagged here exactly as it did before.
    //
    // An Integer/Float site is answered one step earlier still, from the
    // per-thread `PrimitiveConstantSiteCache`, which costs no lock at all
    // where the record costs the `resolution_cache` read lock and a hash
    // probe. Reference results stay in the record only (it is a GC root).
    let cache_on = ldc_const_cache_enabled(shared);
    let prim_epochs = if cache_on {
        match thread
            .site_tables
            .prim_const_sites
            .get(frame_class_id, index)
            .copied()
        {
            Some(PrimitiveConstant::Int(v)) => {
                super::site_cache::site_stats::bump_if(
                    shared.config.site_census,
                    super::site_cache::site_stats::LDC_HIT,
                );
                thread.frames[frame_idx].stack.push(Value::Int(v))?;
                return Ok(());
            }
            Some(PrimitiveConstant::Float(v)) => {
                super::site_cache::site_stats::bump_if(
                    shared.config.site_census,
                    super::site_cache::site_stats::LDC_HIT,
                );
                thread.frames[frame_idx].stack.push(Value::Float(v))?;
                return Ok(());
            }
            // A category-2 constant is never filled at an `ldc` index.
            _ => {}
        }
        // Snapshotted BEFORE anything is read; see `SiteCache::put`.
        Some(PrimitiveConstantSiteCache::epochs_for(shared))
    } else {
        None
    };
    execute_ldc_resolve(
        shared,
        thread,
        frame_idx,
        index,
        frame_class_id,
        cache_on,
        prim_epochs,
    )
}

/// [`execute_ldc`] past its per-thread primitive-site probe: the shared
/// record, then the constant pool (every tag), the records it fills, the
/// failures it records. `cache_on` and `prim_epochs` are the probe's, as the
/// original locals held them.
///
/// Split out of `execute_ldc` in interpreter round i1 wave 27 (lane L7), the
/// body unchanged: the dispatch loop's `ldc` arm calls `execute_ldc` on every
/// execution, and a warm `ldc` of an int or float constant (a loop bound past
/// `sipush`'s range, `1_000_000` in every `TypeCheckBench` row) is answered
/// by that probe alone, which used to share one ~470-line function's frame
/// and prologue with this resolution code. Both are `#[inline(never)]`, so
/// the loop's arm is the same call in every build.
#[inline(never)]
fn execute_ldc_resolve(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    index: u16,
    frame_class_id: ClassId,
    cache_on: bool,
    prim_epochs: Option<(u64, u64)>,
) -> Result<(), MethodCallFailed> {
    if cache_on {
        match cached_cp_constant(shared, frame_class_id, index) {
            Some(cached) => {
                super::site_cache::site_stats::bump_if(
                    shared.config.site_census,
                    super::site_cache::site_stats::LDC_HIT,
                );
                // Another thread recorded it: bring a numeric one into this
                // thread's table so the next execution skips the record.
                let prim = match cached {
                    Value::Int(v) => Some(PrimitiveConstant::Int(v)),
                    Value::Float(v) => Some(PrimitiveConstant::Float(v)),
                    _ => None,
                };
                if let (Some(epochs), Some(prim)) = (prim_epochs, prim) {
                    thread
                        .site_tables
                        .prim_const_sites
                        .put(frame_class_id, index, epochs, prim);
                }
                thread.frames[frame_idx].stack.push(cached)?;
                return Ok(());
            }
            None => super::site_cache::site_stats::bump(super::site_cache::site_stats::LDC_MISS),
        }
    }

    // Resolve the constant pool entry while holding the class_manager read lock.
    // For string references, we extract the string value as an owned String
    // before releasing the lock, so we can then allocate on the heap.
    enum LdcValue {
        Int(i32),
        Float(f32),
        Str(String),
        /// String constant whose Utf8 entry contains lone surrogates — carried
        /// as exact UTF-16 units (a Rust `String` cannot hold them).
        WideStr(Vec<u16>),
        ClassRef(String),
        /// `CONSTANT_MethodType` (tag 16). Carried as the raw method
        /// descriptor: materialising the `java.lang.invoke.MethodType` loads
        /// classes and runs Java, so it must happen after the class-manager
        /// read lock is dropped.
        MethodTypeDesc(String),
        /// `CONSTANT_MethodHandle` (tag 15), decoded by the SAME helper
        /// `invokedynamic` uses
        /// ([`crate::runtime::invokedynamic::resolve_method_handle_full`]) so
        /// the two readings of JVMS §5.4.3.5 cannot drift. Owned strings, so
        /// the value outlives the class-manager read lock.
        MethodHandleRef {
            kind: MethodHandleKind,
            class_name: String,
            member_name: String,
            descriptor: String,
        },
        /// `CONSTANT_Dynamic` (tag 17). No payload: the bootstrap decode and
        /// invocation live in [`resolve_condy_constant`], which `ldc2_w`'s
        /// category-2 condy form calls as well.
        Dynamic,
    }

    // Taken BEFORE the pool read below: a record this execution makes is
    // published only while no redefinition of the class may have replaced
    // that pool (`ResolutionCache::fill_snapshot`; interpreter round i1 wave
    // 23, lane L5). The per-thread primitive table has its own snapshot
    // (`prim_epochs`, above).
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let ldc_val = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(frame_class_id)
            .ok_or_else(|| VmError::Internal {
                message: "current class not found".to_string(),
            })?;

        let entry = class
            .constant_pool
            .get(index)
            // Malformed classfile (bad CP index): route to a catchable
            // `ClassFormatError` (B5) rather than an uncatchable
            // `VmError::Internal`, so unverified bytecode under
            // `skip_verification` does not hard-abort the VM.
            .ok_or_else(|| {
                VmError::Linkage(LinkageError::ClassFormatError {
                    class_name: class.name.to_string(),
                    message: format!("ldc: invalid constant pool index {index}"),
                })
            })?;

        match entry {
            ConstantPoolEntry::Integer(v) => LdcValue::Int(*v),
            ConstantPoolEntry::Float(v) => LdcValue::Float(*v),
            ConstantPoolEntry::StringReference { string_index } => {
                // Surrogate-bearing constants (e.g. ANTLR `_serializedATN`)
                // carry exact UTF-16 units in the pool's side table.
                if let Some(units) = class.constant_pool.get_utf8_wide(*string_index) {
                    LdcValue::WideStr(units.to_vec())
                } else {
                    let s = class
                        .constant_pool
                        .get_utf8(*string_index)
                        .ok_or_else(|| {
                            VmError::Linkage(LinkageError::ClassFormatError {
                                class_name: class.name.to_string(),
                                message: format!("ldc: invalid string_index {string_index}"),
                            })
                        })?
                        .to_string();
                    LdcValue::Str(s)
                }
            }
            ConstantPoolEntry::ClassReference { name_index } => {
                let name = class
                    .constant_pool
                    .get_utf8(*name_index)
                    .ok_or_else(|| {
                        VmError::Linkage(LinkageError::ClassFormatError {
                            class_name: class.name.to_string(),
                            message: format!("ldc: invalid class name_index {name_index}"),
                        })
                    })?
                    .to_string();
                if ldc_classref_trace()
                    && (name.contains("ManagementContextAutoConfiguration")
                        || name.contains("ManagementPortType")
                        || name.contains("WebEndpointAutoConfiguration"))
                {
                    eprintln!(
                        "[LDC-CLASSREF-TRACE] name={} frame_class_id={:?} frame_class_name={}",
                        name, frame_class_id, class.name
                    );
                }
                LdcValue::ClassRef(name)
            }
            // JVMS §6.5 `ldc`: `CONSTANT_MethodType` is one of the seven legal
            // operand kinds, and it is the one the real
            // `MethodHandleProxies.asInterfaceInstance` proxy template needs —
            // its generated `<init>` does `callerBoundTarget.asType(<MT>)` off
            // an `ldc` of the SAM's method type. `classloading::verify_insn`'s
            // `verify_ldc` has always accepted the tag (it pushes
            // `java/lang/invoke/MethodType`); only the interpreter refused it,
            // so a verified class file was rejected at execution.
            ConstantPoolEntry::MethodType { descriptor_index } => {
                let desc = class
                    .constant_pool
                    .get_utf8(*descriptor_index)
                    .ok_or_else(|| {
                        VmError::Linkage(LinkageError::ClassFormatError {
                            class_name: class.name.to_string(),
                            message: format!(
                                "ldc: invalid MethodType descriptor_index {descriptor_index}"
                            ),
                        })
                    })?
                    .to_string();
                LdcValue::MethodTypeDesc(desc)
            }
            // JVMS §6.5 `ldc`: `CONSTANT_MethodHandle`. Decoded through
            // `invokedynamic`'s existing reader rather than a second private
            // copy of Table 5.4.3.5-A — see the module header.
            ConstantPoolEntry::MethodHandle { .. } => {
                let mh = crate::runtime::invokedynamic::resolve_method_handle_full(
                    &class.constant_pool,
                    index,
                )?;
                LdcValue::MethodHandleRef {
                    kind: mh.kind,
                    class_name: mh.class_name.to_string(),
                    member_name: mh.member_name.to_string(),
                    descriptor: mh.descriptor.to_string(),
                }
            }
            ConstantPoolEntry::Dynamic { .. } => LdcValue::Dynamic,
            _ => {
                return Err(VmError::Linkage(LinkageError::ClassFormatError {
                    class_name: class.name.to_string(),
                    message: format!("ldc: unsupported constant pool entry type at #{index}"),
                })
                .into());
            }
        }
        // cm dropped here
    };

    match ldc_val {
        // The primitives record too, and the reason is the probe above rather
        // than the cost of the pool read. Left unrecorded they would MISS on
        // every execution and then do the whole resolving path anyway — the
        // probe would be pure added cost for them. Recorded, the probe hits
        // and the class_manager lock and `get_class` are skipped, which is the
        // part a constant-pool slice index cannot avoid on its own.
        LdcValue::Int(v) => {
            record_cp_constant_if_enabled(shared, frame_class_id, index, Value::Int(v), fill_as_of);
            if let Some(epochs) = prim_epochs {
                thread.site_tables.prim_const_sites.put(
                    frame_class_id,
                    index,
                    epochs,
                    PrimitiveConstant::Int(v),
                );
            }
            thread.frames[frame_idx].stack.push(Value::Int(v))?
        }
        LdcValue::Float(v) => {
            record_cp_constant_if_enabled(
                shared,
                frame_class_id,
                index,
                Value::Float(v),
                fill_as_of,
            );
            if let Some(epochs) = prim_epochs {
                thread.site_tables.prim_const_sites.put(
                    frame_class_id,
                    index,
                    epochs,
                    PrimitiveConstant::Float(v),
                );
            }
            thread.frames[frame_idx].stack.push(Value::Float(v))?
        }
        LdcValue::Str(s) => {
            // Collects and retries, then throws `OutOfMemoryError`, on a full
            // heap -- never aborts (see `intern_string_literal_or_oom`). Found
            // twice on 2026-09-24: gen r4w4 (a `catch (OutOfMemoryError e)`
            // whose handler prints a literal met a 100 %-full young
            // generation) and the gc-common w8 verification
            // (`NativeFactoryReclaimProbe` on G1).
            let obj_ref = intern_string_literal_or_oom(shared, thread, &s)?;
            if remap_trace_on() {
                push_prov_record(obj_ref.as_ptr() as usize, "ldc-str");
            }
            // The pool already guarantees identity; recording it only removes
            // the work of getting back here — the lock, the `get_utf8`, the
            // owned `String` this arm allocated, and the pool's content hash.
            record_cp_constant_if_enabled(
                shared,
                frame_class_id,
                index,
                Value::Object(Some(obj_ref)),
                fill_as_of,
            );
            thread.frames[frame_idx]
                .stack
                .push(Value::Object(Some(obj_ref)))?;
        }
        LdcValue::WideStr(units) => {
            // Interned, not freshly allocated — see `intern_wide_string_literal_or_oom`.
            // gci i32/sl: the row is weak and the class's record holds the
            // literal (`intern_wide_string_literal_or_oom_as`).
            let class_held = surrogate_literal_class_held();
            let obj_ref = intern_wide_string_literal_or_oom_as(shared, thread, &units, class_held)?;
            if remap_trace_on() {
                push_prov_record(obj_ref.as_ptr() as usize, "ldc-str");
            }
            let pushed = record_wide_string_literal_as_of(
                shared,
                frame_class_id,
                index,
                Value::Object(Some(obj_ref)),
                fill_as_of,
                class_held,
            );
            thread.frames[frame_idx].stack.push(pushed)?;
        }
        LdcValue::ClassRef(class_name) => {
            if crate::runtime::env_cache::dbg_toarray() {
                eprintln!(
                    "[DBG_TOARRAY] LDC class={:?} in {}",
                    class_name,
                    thread.frames[frame_idx].class_name()
                );
            }
            let referencing_class_id = thread.frames[frame_idx].class_id;
            // JVMS §5.4.3: an entry whose resolution already failed fails the
            // same way again, without asking any loader.
            if let Some(recorded) =
                recorded_resolution_failure(shared, thread, referencing_class_id, index)
            {
                return Err(recorded);
            }
            // `index` IS the `CONSTANT_Class` entry: resolved through its
            // record, so `ldc X.class` and `new X` / casts / member owners
            // naming the entry share one resolution (an isolated URL loader's
            // `loadClass` is asked once per entry). Round 15 wave 5, lane
            // classsite2 (`r15w5-classsite-class-entry-shared-by-every-
            // instruction-patch-20260930.md`, part B).
            let class_id = resolve_class_entry_loader_aware(
                shared,
                thread,
                (referencing_class_id, index),
                &class_name,
                fill_as_of,
            )
            .map_err(|e| convert_class_not_found(shared, thread, &class_name, e))
            .map_err(|e| {
                record_resolution_failure_as_of(shared, referencing_class_id, index, e, fill_as_of)
            })?;
            // The package step HotSpot runs at the class's define, for an
            // application class-path class handed out unlinked (a sealing
            // violation is this `ldc`'s `SecurityException`; never recorded,
            // nor the mirror below). Interpreter round i4 wave 94, lane L5.
            crate::vm::check_class_path_package_unlinked_at_door(shared, thread, class_id)?;
            // JVMS §5.4.3: a failure another thread recorded for the entry
            // meanwhile is its outcome (wave 38, lane L5; `--jdk-only`).
            if let Some(recorded) = recorded_resolution_failure_after_success(
                shared,
                thread,
                referencing_class_id,
                index,
            ) {
                return Err(recorded);
            }
            // JVMS §5.4.4 (i9-L2): before the mirror is recorded below, which
            // is what every later execution (and the JIT's `ldc` slot) serves.
            check_class_constant_access_as_of(
                shared,
                thread,
                (referencing_class_id, index),
                &class_name,
                Some(class_id),
                "ldc",
                fill_as_of,
            )?;
            // gen r4w5: a Class constant resolved on a full heap collects and
            // throws, like the String arm above, instead of aborting in the
            // mirror's panicking allocation (`GenR4W5ThreadsOomProbe`). The
            // ladder is `class_mirror_or_oom` since gc-common w9-c, which the
            // MethodHandle / condy / indy resolutions share.
            let mirror = class_mirror_or_oom(shared, thread, class_id, "ldc-class")?;
            if remap_trace_on() {
                push_prov_record(mirror.as_ptr() as usize, "ldc-classref");
            }
            // Recorded WITHOUT a loader-namespace guard, unlike the cast and
            // `new` site caches. Those key on the referencing class to make a
            // resolution reusable; here the key IS the constant-pool entry, and
            // JVMS §5.4.3 says a resolved entry returns the same result on
            // every later resolution of it — so recording is the specified
            // behaviour rather than an optimization that needs to prove itself
            // loader-safe. The `MethodType`/`MethodHandle` arms below have
            // recorded through this same store on the same key for the same
            // reason. A failed resolution is recorded too, in the separate
            // failure table (`record_resolution_failure` above): JVMS §5.4.3
            // re-raises the SAME error on every later attempt, it does not
            // re-resolve.
            record_cp_constant_if_enabled(
                shared,
                frame_class_id,
                index,
                Value::Object(Some(mirror)),
                fill_as_of,
            );
            thread.frames[frame_idx]
                .stack
                .push(Value::Object(Some(mirror)))?;
        }
        LdcValue::MethodTypeDesc(desc) => {
            // JVMS §5.4.3: a symbolic reference is resolved ONCE per
            // constant-pool entry and the result recorded. The condy map is
            // that record — keyed by (class, cp index), scanned and remapped
            // by the collector (`for_each_condy_root` / `update_condy_refs`),
            // bounded, and evicted on redefinition — so `CONSTANT_MethodType`
            // and `CONSTANT_MethodHandle` share it rather than re-entering the
            // JDK factory on every execution of the instruction. A CP index
            // has exactly one tag, so the keys cannot collide with a condy's.
            if let Some(cached) = cached_cp_constant(shared, frame_class_id, index) {
                thread.frames[frame_idx].stack.push(cached)?;
                return Ok(());
            }
            // JVMS §5.4.3 applies to method-type resolution as well: a
            // recorded `LinkageError` is rethrown, a new one recorded.
            if let Some(recorded) =
                recorded_resolution_failure(shared, thread, frame_class_id, index)
            {
                return Err(recorded);
            }
            let mt = resolve_method_type_constant(shared, thread, frame_class_id, &desc).map_err(
                |e| record_resolution_failure_as_of(shared, frame_class_id, index, e, fill_as_of),
            )?;
            // JVMS §5.4.3: a failure recorded meanwhile is the entry's (wave 38,
            // lane L5; `--jdk-only`). `mt` is dropped on that path.
            if let Some(recorded) =
                recorded_resolution_failure_after_success(shared, thread, frame_class_id, index)
            {
                return Err(recorded);
            }
            if remap_trace_on() {
                push_prov_record(mt.as_ptr() as usize, "ldc-methodtype");
            }
            let mt = record_method_type_constant_as_of(
                shared,
                frame_class_id,
                index,
                Value::Object(Some(mt)),
                fill_as_of,
            );
            thread.frames[frame_idx].stack.push(mt)?;
        }
        LdcValue::MethodHandleRef {
            kind,
            class_name,
            member_name,
            descriptor,
        } => {
            if let Some(cached) = cached_cp_constant(shared, frame_class_id, index) {
                thread.frames[frame_idx].stack.push(cached)?;
                return Ok(());
            }
            if let Some(recorded) =
                recorded_resolution_failure(shared, thread, frame_class_id, index)
            {
                return Err(recorded);
            }
            let mh = resolve_method_handle_constant(
                shared,
                thread,
                frame_class_id,
                kind,
                &class_name,
                &member_name,
                &descriptor,
            )
            .map_err(|e| {
                record_resolution_failure_as_of(shared, frame_class_id, index, e, fill_as_of)
            })?;
            // JVMS §5.4.3: a failure recorded meanwhile is the entry's (wave 38,
            // lane L5; `--jdk-only`). Allocates only on that path, where `mh`
            // is dropped.
            if let Some(recorded) =
                recorded_resolution_failure_after_success(shared, thread, frame_class_id, index)
            {
                return Err(recorded);
            }
            if remap_trace_on() {
                push_prov_record(mh.as_ptr() as usize, "ldc-methodhandle");
            }
            // Permanent and insert-if-absent: a second `Lookup.find*` answers a
            // NEW handle, so neither an eviction nor a lost race may replace
            // the first one recorded.
            let mh = record_cp_constant_permanent_as_of(
                shared,
                frame_class_id,
                index,
                Value::Object(Some(mh)),
                fill_as_of,
            );
            thread.frames[frame_idx].stack.push(mh)?;
        }
        LdcValue::Dynamic => {
            let result = resolve_condy_constant(shared, thread, frame_class_id, index)?;
            thread.frames[frame_idx].stack.push(result)?;
        }
    }
    Ok(())
}

/// The recorded result of a previous resolution of this constant-pool entry,
/// if any. See the `LdcValue::MethodTypeDesc` arm for why the condy map is the
/// right store for `CONSTANT_MethodType` / `CONSTANT_MethodHandle` too.
///
/// This and [`record_cp_constant_as_of`] are the only two places in this file that
/// touch the resolution record, and they reach it through
/// `MemberResolver::probe_constant` / `record_constant` rather than
/// `shared.classes.resolution_cache` directly. That was the point of the
/// migration: each constant tag that learned to cache used to add its own raw
/// reach, so the file went from two sites to five without anyone deciding to.
/// Everything else here calls one of these two.
pub(crate) fn probe_recorded_cp_constant(
    shared: &SharedVm,
    class_id: ClassId,
    cp_index: u16,
) -> Option<Value> {
    cached_cp_constant(shared, class_id, cp_index)
}

/// Counter-free write half of [`probe_recorded_cp_constant`], for the compiled
/// `ldc` helpers.
///
/// The two `pub(crate)` wrappers exist so `vm::jit::helpers`' CP-indexed `ldc`
/// helpers record into the SAME store on the SAME terms as the interpreter
/// rather than reaching `shared.classes.resolution_cache` themselves — the
/// bypass this file's own migration was written to stop. They are
/// counter-free because the compiled route keeps its own hit/miss/fill triple:
/// folding the two populations into one number would make "the interpreter is
/// answering from the record" and "compiled code is" indistinguishable, and
/// they are separately switchable.
///
/// Test-only since interpreter round i1 wave 24 (lane L5): every production
/// record takes a fill snapshot first and goes through
/// [`store_recorded_cp_constant_as_of`].
#[cfg(test)]
pub(crate) fn store_recorded_cp_constant(
    shared: &SharedVm,
    class_id: ClassId,
    cp_index: u16,
    value: Value,
) {
    let resolver = MemberResolver::new(shared);
    let caller = resolver.scope(class_id);
    let value = resolver.scope(value);
    resolver.record_constant(caller, cp_index, value);
}

/// Counter-free [`record_cp_constant_as_of`]: the record the compiled `ldc`
/// helpers and `invokedynamic`'s static arguments make, for a resolution that
/// took `ResolutionCache::fill_snapshot` `as_of` BEFORE it read the constant
/// pool (nothing is recorded when a redefinition of `class_id` may have
/// replaced it since; i22-L5, interpreter round i1 wave 24).
pub(crate) fn store_recorded_cp_constant_as_of(
    shared: &SharedVm,
    class_id: ClassId,
    cp_index: u16,
    value: Value,
    as_of: u64,
) {
    let resolver = MemberResolver::new(shared);
    let caller = resolver.scope(class_id);
    // gcg g2/w: under the weak String table the record is what keeps an `ldc`
    // literal alive (HotSpot's resolved-references array), so a REFERENCE
    // record goes to the never-evicted store instead of the FIFO-capped one:
    // an evicted literal record would leave its String held by the weak pool
    // alone, and a `WeakReference` to a literal could then clear while its
    // class lives. Re-resolution of every tag recorded here (String, Class,
    // MethodType) yields the identical object, so insert-if-absent answers
    // what the FIFO store would have; both stores are rooted and remapped
    // alike (`for_each_condy_root`, `update_condy_refs`).
    if matches!(value, Value::Object(Some(_)))
        && crate::memory::string_table::weak_table_engaged(shared)
    {
        let value = resolver.scope(value);
        let _ = resolver.record_constant_permanent_as_of(caller, cp_index, value, as_of);
        return;
    }
    let value = resolver.scope(value);
    resolver.record_constant_as_of(caller, cp_index, value, as_of);
}

fn cached_cp_constant(shared: &SharedVm, class_id: ClassId, cp_index: u16) -> Option<Value> {
    let resolver = MemberResolver::new(shared);
    let caller = resolver.scope(class_id);
    resolver
        .probe_constant(caller, cp_index)
        .into_hit()
        .and_then(|hit| resolver.adopt(hit).ok())
}

/// Record the result of resolving this constant-pool entry (write half of
/// [`cached_cp_constant`]), for a resolution that took
/// `ResolutionCache::fill_snapshot` `as_of` BEFORE it read the constant pool:
/// nothing is recorded when a redefinition of `class_id` may have replaced the
/// pool since, so the old pool's constant is not served at the new pool's
/// index (`i22-L5-resolution-cache-fills-can-outlive-a-concurrent-redefinition`,
/// interpreter round i1 wave 23, lane L5). The unconditional twin
/// `record_cp_constant` lost its last caller (the condy static arguments) in
/// wave 24 and was removed.
fn record_cp_constant_as_of(
    shared: &SharedVm,
    class_id: ClassId,
    cp_index: u16,
    value: Value,
    as_of: u64,
) {
    super::site_cache::site_stats::bump(super::site_cache::site_stats::LDC_FILL);
    store_recorded_cp_constant_as_of(shared, class_id, cp_index, value, as_of);
}

/// Whether a `CONSTANT_MethodType` record goes to the never-evicted store
/// (`ResolutionCache::linked_constants_permanent`, kill switch
/// `CRATONVM_RESOLUTION_LINKED_PERMANENT=0`; nio21 wave 11 L4). HotSpot keeps
/// a resolved method type in the class's resolved references for the class's
/// lifetime, while the JDK's `MethodType` intern set is WEAK: an evicted FIFO
/// record left the object to the intern set alone, so a `WeakReference` to an
/// `ldc` method type could clear (and a re-resolution hand out a new object
/// with a new identity hash) while its class lived.
pub(crate) fn method_type_record_is_permanent(shared: &SharedVm) -> bool {
    shared
        .classes
        .resolution_cache
        .read()
        .linked_constants_permanent()
}

/// Record a resolved `CONSTANT_MethodType` (`ldc`, a condy static argument)
/// and answer the value to push: the permanent, insert-if-absent record's
/// winner when [`method_type_record_is_permanent`], else `value` after the
/// FIFO record, as before.
fn record_method_type_constant_as_of(
    shared: &SharedVm,
    class_id: ClassId,
    cp_index: u16,
    value: Value,
    as_of: u64,
) -> Value {
    if method_type_record_is_permanent(shared) {
        return record_cp_constant_permanent_as_of(shared, class_id, cp_index, value, as_of);
    }
    record_cp_constant_as_of(shared, class_id, cp_index, value, as_of);
    value
}

/// [`record_cp_constant_as_of`] for a constant whose re-resolution would NOT
/// yield the same object — `CONSTANT_Dynamic` and `CONSTANT_MethodHandle`.
/// Recorded in the never-evicted, insert-if-absent store
/// (`ResolutionCache::put_permanent_constant_as_of`), and answers the value the
/// caller must push: the FIRST one recorded (JVMS §5.4.3.6), which is not
/// `value` when another thread won the resolution race.
///
/// Every other tag stays in the FIFO-capped store: an evicted `String` or
/// `Class` record re-resolves to the identical object (pool, mirror), an
/// evicted condy re-ran its bootstrap. A `MethodType` comes here too since
/// nio21 wave 11 L4 ([`method_type_record_is_permanent`]): its intern set is
/// weak, so only the record held it for the class's lifetime.
///
/// `as_of` is the `ResolutionCache::fill_snapshot` the caller took BEFORE it
/// read the constant pool. When a redefinition of `class_id` may have replaced
/// the pool since, nothing is recorded and `value` (the OLD pool entry's
/// result) is handed to this execution only; the new pool's entry is resolved
/// afresh by the next one (interpreter round i1 wave 23, lane L5).
///
/// `pub(crate)` for `invokedynamic`'s generic bootstrap, whose
/// `CONSTANT_MethodHandle` static arguments must land in the same record `ldc`
/// of that entry uses. Its unconditional twin `record_cp_constant_permanent`
/// lost its last caller there in wave 24 (lane L5) and was removed.
pub(crate) fn record_cp_constant_permanent_as_of(
    shared: &SharedVm,
    class_id: ClassId,
    cp_index: u16,
    value: Value,
    as_of: u64,
) -> Value {
    super::site_cache::site_stats::bump(super::site_cache::site_stats::LDC_FILL);
    let resolver = MemberResolver::new(shared);
    let caller = resolver.scope(class_id);
    let scoped = resolver.scope(value);
    resolver
        .record_constant_permanent_as_of(caller, cp_index, scoped, as_of)
        .and_then(|winner| resolver.adopt(winner).ok())
        .unwrap_or(value)
}

// ---------------------------------------------------------------------------
// JVMS §5.4.3: recorded resolution FAILURES
// ---------------------------------------------------------------------------
//
// "If an attempt by the Java Virtual Machine to resolve a symbolic reference
// fails because an error is thrown that is an instance of LinkageError (or a
// subclass), then subsequent attempts to resolve the reference always fail with
// the same error that was thrown as a result of the initial resolution
// attempt." Until 2026-09-23 only successes were recorded, so a user loader
// whose `loadClass` failed once and later succeeded made one `ldc`/`new` site
// throw and then succeed, and a `try { X.class } catch (NoClassDefFoundError)`
// probe loop paid a full resolution (a Java `loadClass` upcall and an exception
// construction) per iteration. HotSpot keeps the error class and message per
// entry and throws a new instance of that class with that message; so does
// this. The record is keyed by (referencing class, cp index), which is
// loader-aware by construction: the referencing class has exactly one defining
// loader.

/// `CRATONVM_LOADER_NO_RESOLUTION_FAILURE_RECORD=1` — do not record failed
/// `CONSTANT_Class` / `CONSTANT_Dynamic` resolutions; every later execution
/// resolves again (the pre-2026-09-23 behaviour). A lever for bisecting a
/// workload that depended on a transient resolution failure healing itself,
/// which HotSpot does not allow either. Read once per VM into
/// `VmConfig::resolution_failure_record` (interpreter round i2 wave 59, lane
/// L7; a process `OnceLock` before).
#[inline(always)]
fn resolution_failure_record_enabled(shared: &SharedVm) -> bool {
    shared.config.resolution_failure_record
}

/// Whether `shared` has ever recorded a resolution failure: the per-VM
/// pre-filter `ClassRealm::resolution_failure_recorded`, in the style of
/// `jvmti::any_field_watchpoint_active`. The `ResolutionCache` table is the
/// authority; this bit only keeps its read lock off the resolution miss paths
/// of a VM that has never failed a resolution (almost every program). A
/// process static until interpreter round i1 wave 25 (lane L5), when one VM's
/// first failure sent every VM in the process to the table.
///
/// Also for a caller (the compiled-code CP helpers) that must fetch its
/// `JvmThread` before it can call [`recorded_resolution_failure`] and wants
/// to skip that when nothing was ever recorded.
#[inline]
pub(crate) fn any_resolution_failure_recorded(shared: &SharedVm) -> bool {
    shared
        .classes
        .resolution_failure_recorded
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// Raise [`any_resolution_failure_recorded`] for `shared`, after a record was
/// written (never lowered).
#[cold]
fn note_resolution_failure_recorded(shared: &SharedVm) {
    shared
        .classes
        .resolution_failure_recorded
        .store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The error a previous failed resolution of `(class_id, cp_index)` recorded,
/// as a NEW throwable of the same class with the same message — or `None`
/// when nothing is recorded and the caller must resolve.
#[inline]
pub(crate) fn recorded_resolution_failure(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    cp_index: u16,
) -> Option<MethodCallFailed> {
    if !any_resolution_failure_recorded(shared) {
        return None;
    }
    rethrow_recorded_resolution_failure(shared, thread, class_id, cp_index)
}

/// JVMS §5.4.3 after a SUCCESSFUL slow-path resolution of `(class_id,
/// cp_index)`: the failure another thread recorded for the entry while this
/// one resolved, as a new throwable, which this execution must throw instead
/// of its own answer — HotSpot's `klass_at_impl` "success meets an entry
/// already in error" arm, so every thread sees one outcome
/// (`i25-L5-a-racing-resolution-…`, fix 1; interpreter round i1 wave 38, lane
/// L5; probe `L5W37RacingEntryOutcome`, `fail-first`). Call it right after the
/// resolver returned `Ok`, before any site cache or record is filled.
///
/// Both modes since wave 47 (lane L5; `--compatible` kept its outcomes until
/// then). One relaxed load when nothing was ever recorded in this VM.
#[inline]
pub(crate) fn recorded_resolution_failure_after_success(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    cp_index: u16,
) -> Option<MethodCallFailed> {
    if !any_resolution_failure_recorded(shared) {
        return None;
    }
    let recorded = rethrow_recorded_resolution_failure(shared, thread, class_id, cp_index)?;
    crate::runtime::resolve::loader_throw::note_resolution_race(
        shared,
        "success meets a failure recorded meanwhile",
        class_id,
        &format_args!("cp#{cp_index}"),
    );
    Some(recorded)
}

#[cold]
fn rethrow_recorded_resolution_failure(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    cp_index: u16,
) -> Option<MethodCallFailed> {
    let resolver = MemberResolver::new(shared);
    let failure = resolver.probe_resolution_failure(resolver.scope(class_id), cp_index)?;
    let error = match crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        &failure.error_class,
        failure.message.as_deref(),
    ) {
        Ok(obj) => obj,
        Err(e) => return Some(e),
    };
    // The recorded cause, as a NEW throwable of its class and message, like
    // HotSpot's `ConstantPool::throw_resolution_error` (interpreter round i1
    // wave 23, lane L5). The error is pinned across the cause's allocation,
    // which can collect.
    if let Some((cause_class, cause_message)) = &failure.cause {
        let pin_base = thread.native_pin_roots.len();
        thread.native_pin_roots.push(error);
        let cause = crate::runtime::exceptions::create_exception_object(
            shared,
            thread,
            cause_class,
            cause_message.as_deref(),
        );
        let error = thread.native_pin_roots[pin_base];
        thread.native_pin_roots.truncate(pin_base);
        if let Ok(cause) = cause {
            crate::runtime::exceptions::set_cause_by_name(shared, error, cause);
        }
        return Some(MethodCallFailed::ExceptionThrown(error));
    }
    Some(MethodCallFailed::ExceptionThrown(error))
}

/// Record `failure` against `(class_id, cp_index)` when it is a Java
/// `LinkageError` (JVMS §5.4.3 records nothing else: a `StackOverflowError` or
/// `OutOfMemoryError` raised while resolving must not poison the entry), and
/// hand it back unchanged. Call it with the Java-visible failure, i.e. after
/// `convert_class_not_found`.
#[cold]
pub(crate) fn record_resolution_failure(
    shared: &SharedVm,
    class_id: ClassId,
    cp_index: u16,
    failure: MethodCallFailed,
) -> MethodCallFailed {
    if !resolution_failure_record_enabled(shared) {
        return failure;
    }
    let MethodCallFailed::ExceptionThrown(obj) = &failure else {
        return failure;
    };
    if let Some(record) = linkage_error_record(shared, *obj) {
        let resolver = MemberResolver::new(shared);
        resolver.record_resolution_failure(resolver.scope(class_id), cp_index, record);
        note_resolution_failure_recorded(shared);
    }
    failure
}

/// [`record_resolution_failure`] for a resolution that took
/// `ResolutionCache::fill_snapshot` `as_of` BEFORE it read the constant pool:
/// nothing is recorded when a redefinition of `class_id` may have replaced the
/// pool since, so the new pool's entry at `cp_index` is not failed with an
/// error about a class it never named (interpreter round i1 wave 23, lane L5;
/// `i22-L5-resolution-cache-fills-can-outlive-a-concurrent-redefinition`).
#[cold]
pub(crate) fn record_resolution_failure_as_of(
    shared: &SharedVm,
    class_id: ClassId,
    cp_index: u16,
    failure: MethodCallFailed,
    as_of: u64,
) -> MethodCallFailed {
    if !resolution_failure_record_enabled(shared) {
        return failure;
    }
    let MethodCallFailed::ExceptionThrown(obj) = &failure else {
        return failure;
    };
    if let Some(record) = linkage_error_record(shared, *obj) {
        let resolver = MemberResolver::new(shared);
        resolver.record_resolution_failure_as_of(resolver.scope(class_id), cp_index, record, as_of);
        note_resolution_failure_recorded(shared);
    }
    failure
}

/// The `class_index` of the `Fieldref` / `Methodref` / `InterfaceMethodref` at
/// `cp_index` of `class_id`'s pool: the `CONSTANT_Class` entry a failed owner
/// resolution of that member reference is recorded against.
///
/// JVMS §5.4.3.2 / §5.4.3.3 resolve a member reference's class first, through
/// its `CONSTANT_Class` entry, and HotSpot records that failure on the CLASS
/// entry (`LinkResolver::resolve_klass` -> `ConstantPool::klass_ref_at` ->
/// `klass_at_impl` -> `SystemDictionary::add_resolution_error`). So a
/// `new Opt()`, an `ldc Opt.class`, a `getstatic Opt.X` and an `invokestatic
/// Opt.m()` of one class share one record (interpreter round i1 wave 25, lane
/// L5; probes `L5W24OwnerFailureRecord`, `L5W25OwnerFailureRecordInvoke`).
pub(crate) fn member_ref_class_index(
    shared: &SharedVm,
    class_id: ClassId,
    cp_index: u16,
) -> Option<u16> {
    let cm = shared.classes.class_manager.read();
    match cm.get_class(class_id)?.constant_pool.get(cp_index)? {
        ConstantPoolEntry::FieldReference { class_index, .. }
        | ConstantPoolEntry::MethodReference { class_index, .. }
        | ConstantPoolEntry::InterfaceMethodReference { class_index, .. } => Some(*class_index),
        _ => None,
    }
}

/// [`recorded_resolution_failure`] for the CLASS of the member reference at
/// `member_cp_index` ([`member_ref_class_index`]): the error a previous failed
/// resolution of that `CONSTANT_Class` entry recorded — by this member
/// reference, another one naming the same entry, or a class opcode on it — as
/// a new throwable, or `None`. One relaxed load when nothing was ever recorded.
#[inline]
pub(crate) fn recorded_member_owner_failure(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    member_cp_index: u16,
) -> Option<MethodCallFailed> {
    if !any_resolution_failure_recorded(shared) {
        return None;
    }
    let class_index = member_ref_class_index(shared, class_id, member_cp_index)?;
    rethrow_recorded_resolution_failure(shared, thread, class_id, class_index)
}

/// [`recorded_resolution_failure_after_success`] for a MEMBER reference's
/// owner: after the owner at `member_cp_index`'s class entry
/// ([`member_ref_class_index`]) resolved on the slow path, the failure another
/// thread recorded against that entry meanwhile, which this execution must
/// throw instead (JVMS §5.4.3, one outcome per entry; `i25-L5-a-racing-…`,
/// the member-owner remainder; interpreter round i1 wave 39, lane L5).
/// Both modes since wave 47 (lane L5); one relaxed load when nothing was ever
/// recorded in this VM.
#[inline]
pub(crate) fn recorded_member_owner_failure_after_success(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    member_cp_index: u16,
) -> Option<MethodCallFailed> {
    if !any_resolution_failure_recorded(shared) {
        return None;
    }
    let class_index = member_ref_class_index(shared, class_id, member_cp_index)?;
    recorded_resolution_failure_after_success(shared, thread, class_id, class_index)
}

/// [`record_resolution_failure_as_of`] against the CLASS entry of the member
/// reference at `member_cp_index`, for an owner resolution that failed. The
/// member lookup's own errors (`NoSuchFieldError`, `NoSuchMethodError`,
/// `IncompatibleClassChangeError`) must not come here: they are deterministic
/// for a resolved owner, and recording one on the class entry would fail a
/// `new` of a perfectly good class.
#[cold]
pub(crate) fn record_member_owner_failure_as_of(
    shared: &SharedVm,
    class_id: ClassId,
    member_cp_index: u16,
    failure: MethodCallFailed,
    as_of: u64,
) -> MethodCallFailed {
    match member_ref_class_index(shared, class_id, member_cp_index) {
        Some(class_index) => {
            record_resolution_failure_as_of(shared, class_id, class_index, failure, as_of)
        }
        None => failure,
    }
}

/// Whether a resolution failure is recorded against `(class_id, cp_index)`,
/// for a caller with no `JvmThread` to build the throwable on (the compiler's
/// field peeks): it declines the entry, and the interpreter, reaching it,
/// rethrows the record.
#[inline]
pub(crate) fn resolution_failure_is_recorded(
    shared: &SharedVm,
    class_id: ClassId,
    cp_index: u16,
) -> bool {
    if !any_resolution_failure_recorded(shared) {
        return false;
    }
    let resolver = MemberResolver::new(shared);
    resolver
        .probe_resolution_failure(resolver.scope(class_id), cp_index)
        .is_some()
}

/// `(class, message)` of `throwable` when it is a `LinkageError` that
/// [`rethrow_recorded_resolution_failure`] can re-create by name.
///
/// Only `java/`-prefixed classes qualify: `create_exception_object` loads the
/// class by name through the boot chain, and only the bootstrap loader may
/// define `java/*`, so the name identifies the class exactly. A user-defined
/// `LinkageError` subclass thrown out of a `loadClass` is left unrecorded (the
/// pre-change behaviour) rather than re-created as a different class.
fn linkage_error_record(
    shared: &SharedVm,
    throwable: ObjectRef,
) -> Option<crate::classloading::resolution::ResolutionFailure> {
    let error_class = {
        let cm = shared.classes.class_manager.read();
        let cid = shared.mem.heap.class_id_of(throwable);
        let linkage = cm.get_loaded_class_id("java/lang/LinkageError")?;
        if !cm.is_subclass_of(cid, linkage) {
            return None;
        }
        let name = cm.get_class(cid)?.name.clone();
        if !name.starts_with("java/") {
            return None;
        }
        name
    };
    Some(crate::classloading::resolution::ResolutionFailure {
        error_class: Arc::<str>::from(&*error_class),
        message: throwable_detail_message(shared, throwable).map(Arc::<str>::from),
        cause: linkage_error_cause_record(shared, throwable),
    })
}

/// `(class, message)` of `throwable`'s cause, for the resolution-failure
/// record: HotSpot rethrows a recorded resolution error with a new cause of
/// the recorded cause's class and message (interpreter round i1 wave 23, lane
/// L5). `None` for no cause, a cause that is the throwable itself (Java's
/// "not initialized" encoding), or a cause class outside `java/`, which the
/// rethrow could not instantiate by name as faithfully.
fn linkage_error_cause_record(
    shared: &SharedVm,
    throwable: ObjectRef,
) -> Option<(Arc<str>, Option<Arc<str>>)> {
    let (cause, cause_class) = {
        let cm = shared.classes.class_manager.read();
        let cid = shared.mem.heap.class_id_of(throwable);
        let idx = crate::vm::resolve_field_index_in_hierarchy(cid, "cause", &cm.class_store)?;
        let Value::Object(Some(cause)) = shared.mem.heap.get_field(throwable, idx) else {
            return None;
        };
        if cause == throwable {
            return None;
        }
        let name = cm
            .get_class(shared.mem.heap.class_id_of(cause))?
            .name
            .clone();
        if !name.starts_with("java/") {
            return None;
        }
        (cause, name)
    };
    // A helpful NPE CratonVM raised keeps its text in `detailMessage`; HotSpot
    // records none for it (wave 44, see `condy_bootstrap_error_with_helpful_npe`).
    let message = if throwable_is_vm_helpful_npe(shared, cause) {
        None
    } else {
        throwable_detail_message(shared, cause).map(Arc::<str>::from)
    };
    Some((Arc::<str>::from(&*cause_class), message))
}

/// JVMS §5.4.3.5 resolution of a `CONSTANT_MethodType`, shared by `ldc` and
/// `ldc_w`.
///
/// **Twin discipline.** There are two other places in the tree that turn a
/// method descriptor into a `java.lang.invoke.MethodType`:
/// `invokedynamic`'s bootstrap-argument path
/// (`runtime/invokedynamic.rs`, `StaticArg::MType`) and
/// `phases_late::reflect_invoke`'s `StackFrame.getMethodType()`. The latter
/// already established the correct order — **prefer the real JDK factory**
/// (`MethodType.fromMethodDescriptorString`), because it interns, so the
/// result compares `==` against a `MethodType` obtained any other way; fall
/// back to `lang_invoke::build_method_type_from_descriptor` only when the
/// class library does not carry the factory (synthetic-JDK builds). This
/// function follows that order rather than inventing a third one, and the
/// fallback it uses is the *same function* the `invokedynamic` path uses, so
/// the two agree by construction.
///
/// The class loader passed to the factory is the one that defined the class
/// whose constant pool holds the entry — JVMS §5.4.3.5 resolves the
/// descriptor's field types with that loader as the initiating loader. Passing
/// `null` (the bootstrap loader) instead would silently fail for any
/// descriptor naming an application class.
pub(crate) fn resolve_method_type_constant(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_class_id: ClassId,
    descriptor: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    // `CRATONVM_METHOD_TYPE_VM_TYPES` (interpreter round i1 wave 48, lane
    // L5): the types are resolved here, as HotSpot does; `None` falls back to
    // the JDK factory below (no `findMethodHandleType`, a malformed
    // descriptor, an unexpected answer).
    if shared.classes.method_type_vm_types {
        if let Some(mt) = resolve_method_type_in_vm(shared, thread, frame_class_id, descriptor)? {
            return Ok(mt);
        }
    }
    const FMDS: &str = "(Ljava/lang/String;Ljava/lang/ClassLoader;)Ljava/lang/invoke/MethodType;";
    // `method_exists` only inspects LOADED classes, so make sure the factory's
    // own class is present before asking whether it declares the factory.
    let _ = shared.load_class_concurrent("java/lang/invoke/MethodType");
    // `NativeClassAccess` (which carries `method_exists`) is in scope from the
    // parent module's imports.
    let have_factory = {
        let ctx = crate::vm::NativeContextImpl { shared, thread };
        ctx.method_exists(
            "java/lang/invoke/MethodType",
            "fromMethodDescriptorString",
            FMDS,
        )
    };
    if have_factory {
        let loader = defining_loader_object(shared, thread, frame_class_id)?;
        // GC-safety: the descriptor String below can collect (and so move the
        // heap), and the loader is a bare `ObjectRef` held in a Rust local.
        // Pin it across the allocation and re-read the (possibly forwarded)
        // reference. `invoke_shared` pins its own argument slice, so the pin
        // can be released before the call.
        let pin_base = thread.native_pin_roots.len();
        if let Some(l) = loader {
            thread.native_pin_roots.push(l);
        }
        // `new`'s contract: collect, retry, then `OutOfMemoryError` -- the
        // infallible `create_java_string` aborted the process when the first
        // resolution of the entry met a full heap (gc-common w9-c,
        // `common-w8v-constant-resolution-strings-abort-on-a-full-heap`).
        // Pooled, as before.
        let desc_str = match intern_string_literal_or_oom(shared, thread, descriptor) {
            Ok(s) => s,
            Err(e) => {
                thread.native_pin_roots.truncate(pin_base);
                return Err(e);
            }
        };
        let loader = match loader {
            Some(_) => thread.native_pin_roots.get(pin_base).copied(),
            None => None,
        };
        thread.native_pin_roots.truncate(pin_base);
        match invoke_shared(
            shared,
            thread,
            "java/lang/invoke/MethodType",
            "fromMethodDescriptorString",
            FMDS,
            &[Value::Object(Some(desc_str)), Value::Object(loader)],
        ) {
            Ok(Some(Value::Object(Some(mt)))) => return Ok(mt),
            // A Java-visible failure IS the answer here: JVMS §5.4.3.5 says a
            // method-type resolution that cannot resolve one of its field
            // types fails with that error. Swallowing it and fabricating a
            // MethodType would turn a linkage error into a wrong value. The
            // factory words a missing class as `TypeNotPresentException`;
            // the resolution error is `NoClassDefFoundError`.
            Err(MethodCallFailed::ExceptionThrown(thrown)) => {
                return Err(method_type_failure_as_linkage_error(
                    shared,
                    thread,
                    frame_class_id,
                    descriptor,
                    thrown,
                ));
            }
            other => {
                tracing::debug!(
                    "ldc MethodType: fromMethodDescriptorString({descriptor}) did not \
                     produce a MethodType ({other:?}); falling back to the VM builder"
                );
            }
        }
    }
    let owner = shared
        .classes
        .class_manager
        .read()
        .get_class(frame_class_id)
        .map(|c| c.name.to_string())
        .unwrap_or_default();
    // Collecting and fallible, like `new` (gc-common w4-c): this context is
    // outside any native callback, so a bare `NativeContextImpl` would hit the
    // allocators' infallible arm -- a process abort on Generational/ZGC, G1's
    // emergency reserve otherwise. The builder takes only the descriptor text
    // and re-reads everything it pins, so re-running it after a collection is
    // safe; nothing here holds an `ObjectRef` across the call.
    let built =
        super::native_alloc_collecting(shared, thread, "ldc-methodtype", |shared, thread| {
            let mut ctx = crate::vm::NativeContextImpl { shared, thread };
            cratonvm_native_builtins::lang_invoke::build_method_type_from_descriptor(
                &mut ctx, descriptor,
            )
        })?;
    match built? {
        Some(mt) => Ok(mt),
        None => Err(VmError::Linkage(LinkageError::ClassFormatError {
            class_name: owner,
            message: format!("ldc: malformed CONSTANT_MethodType descriptor `{descriptor}`"),
        })
        .into()),
    }
}

/// The field descriptors of a method descriptor, parameters first and the
/// return type last (`(I[Lp/A;)V` -> `I`, `[Lp/A;`, `V`). `None` for a
/// malformed descriptor (the JDK factory then answers, as before).
fn method_descriptor_types(descriptor: &str) -> Option<Vec<&str>> {
    let rest = descriptor.strip_prefix('(')?;
    let close = rest.find(')')?;
    let (params, ret) = (&rest[..close], &rest[close + 1..]);
    let mut out = Vec::new();
    let mut i = 0;
    let bytes = params.as_bytes();
    while i < bytes.len() {
        let start = i;
        while bytes.get(i) == Some(&b'[') {
            i += 1;
        }
        match bytes.get(i)? {
            b'L' => i += params[i..].find(';')? + 1,
            b'Z' | b'C' | b'F' | b'D' | b'B' | b'S' | b'I' | b'J' => i += 1,
            _ => return None,
        }
        out.push(&params[start..i]);
    }
    let ret_ok = ret == "V"
        || (ret.len() == 1 && "ZCFDBSIJ".contains(ret))
        || (ret.trim_start_matches('[').starts_with('L') && ret.ends_with(';'))
        || (ret.starts_with('[') && ret.trim_start_matches('[').len() == 1);
    if !ret_ok {
        return None;
    }
    out.push(ret);
    Some(out)
}

/// HotSpot's `is_always_visible_class` (`systemDictionary.cpp`) for the field
/// descriptor `desc`, answered in the boot namespace: the class a
/// `CONSTANT_MethodType` resolution may take from the bootstrap loader without
/// asking the resolving class's loader -- a PUBLIC class of package
/// `java.lang` or `java.lang.invoke` (exactly those two), an array of one, or
/// an array of a primitive. `None` for every other type, and for a name the
/// bootstrap namespace cannot load without fabricating a stub (HotSpot's
/// null-loader lookup answers null there too, and the walk leaves it).
fn always_visible_boot_class(shared: &SharedVm, desc: &str) -> Option<ClassId> {
    use cratonvm_types::ClassLoaderId;
    let element = desc.trim_start_matches('[');
    let is_array = element.len() != desc.len();
    if let Some(class_name) = element.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
        let package = &class_name[..class_name.rfind('/')?];
        if package != "java/lang" && package != "java/lang/invoke" {
            return None;
        }
        let loaded = {
            let cm = shared.classes.class_manager.read();
            match cm.loaded_class_under_exact_key(class_name, ClassLoaderId::Bootstrap) {
                Some(id) => Some(id),
                None if cm.would_fabricate_synthetic_stub(class_name) => return None,
                None => None,
            }
        };
        let id = match loaded {
            Some(id) => id,
            None => shared.load_class_concurrent(class_name).ok()?,
        };
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(id)?;
        if class.loader_id != ClassLoaderId::Bootstrap || !class.is_public() {
            return None;
        }
        if !is_array {
            return Some(id);
        }
    } else if !is_array || element.len() != 1 {
        // A primitive is not a class here; `[` alone is malformed.
        return None;
    }
    shared
        .classes
        .class_manager_write_settling()
        .load_array_class_for_loader(desc, ClassLoaderId::Bootstrap)
        .ok()
}

/// JVMS §5.4.3.5 resolution of a `CONSTANT_MethodType` in the VM, as HotSpot's
/// `SystemDictionary::find_method_handle_type` does it
/// (`CRATONVM_METHOD_TYPE_VM_TYPES`; interpreter round i1 wave 48, lane L5;
/// `docs/internal/fixed-bugs/interpreter-L5-proposal-method-types-resolve-their-types-in-the-vm-FIXED-20261012.md`).
///
/// The descriptor's types are walked in order, parameters then the return
/// type. While every reference type so far is "always visible"
/// ([`always_visible_boot_class`]) the walk stays in the bootstrap namespace
/// and asks the class's loader for nothing (a primitive keeps it there); from
/// the first other type on, every remaining reference type is resolved
/// through the loader of `frame_class_id` -- by `Class.forName0(name, false,
/// loader, <the class>)`, the native the JDK factory's per-type
/// `Class.forName` reaches, so that part is today's behaviour, records
/// included. The `MethodType` is then built by one
/// `MethodHandleNatives.findMethodHandleType(rtype, ptypes)` upcall, as
/// HotSpot builds it.
///
/// The JDK factory (`MethodType.fromMethodDescriptorString`) asked the loader
/// for every reference type, so a user loader was asked for `String` and the
/// application loader initiated every JDK type a constant named (probe
/// `tools/probes/interp/L5/L5W48MethodTypeConstantLoaderAsks.java`).
///
/// `Ok(None)` when this cannot answer (no `findMethodHandleType`, a
/// malformed descriptor, an unexpected non-throwing answer): the caller's
/// factory path runs. A loader's `ClassNotFoundException` becomes the
/// `NoClassDefFoundError` of [`method_type_failure_as_linkage_error`]; any
/// other throwable propagates as it is (HotSpot's `resolve_or_fail`).
#[inline(never)]
fn resolve_method_type_in_vm(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_class_id: ClassId,
    descriptor: &str,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    const FMHT: &str = "(Ljava/lang/Class;[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;";
    let Some(types) = method_descriptor_types(descriptor) else {
        return Ok(None);
    };
    let _ = shared.load_class_concurrent("java/lang/invoke/MethodHandleNatives");
    let have_builder = {
        let ctx = crate::vm::NativeContextImpl { shared, thread };
        ctx.method_exists(
            "java/lang/invoke/MethodHandleNatives",
            "findMethodHandleType",
            FMHT,
        )
    };
    let class_class = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id("java/lang/Class");
    let Some(class_class) = class_class.filter(|_| have_builder) else {
        return Ok(None);
    };
    // Every mirror, and the loader and the class's mirror once the walk
    // leaves the bootstrap namespace, is pinned from `pin_base` on: the walk
    // allocates and runs Java. Released on every path below.
    let pin_base = thread.native_pin_roots.len();
    let result = method_type_walk(
        shared,
        thread,
        frame_class_id,
        descriptor,
        &types,
        class_class,
    );
    thread.native_pin_roots.truncate(pin_base);
    let (ptypes, rtype) = match result {
        Ok(Some(pair)) => pair,
        Ok(None) => return Ok(None),
        Err(MethodCallFailed::ExceptionThrown(thrown)) => {
            return Err(method_type_failure_as_linkage_error(
                shared,
                thread,
                frame_class_id,
                descriptor,
                thrown,
            ));
        }
        Err(e) => return Err(e),
    };
    // `invoke_shared` pins its own arguments; nothing allocates in between.
    match invoke_shared(
        shared,
        thread,
        "java/lang/invoke/MethodHandleNatives",
        "findMethodHandleType",
        FMHT,
        &[Value::Object(Some(rtype)), Value::Object(Some(ptypes))],
    ) {
        Ok(Some(Value::Object(Some(mt)))) => Ok(Some(mt)),
        Err(e) => Err(e),
        Ok(_) => Ok(None),
    }
}

/// [`resolve_method_type_in_vm`]'s walk: the `Class[]` of the parameter types
/// and the return type's mirror, read back from their pins after the array
/// is allocated. The caller truncates the pins.
fn method_type_walk(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_class_id: ClassId,
    descriptor: &str,
    types: &[&str],
    class_class: ClassId,
) -> Result<Option<(ObjectRef, ObjectRef)>, MethodCallFailed> {
    use crate::native::registry::NativeContext as _;
    const FOR_NAME0: &str =
        "(Ljava/lang/String;ZLjava/lang/ClassLoader;Ljava/lang/Class;)Ljava/lang/Class;";
    let mut bootstrap_walk = true;
    // `(loader slot or None for the null loader, class-mirror slot)`, pinned
    // when the walk first leaves the bootstrap namespace.
    let mut through: Option<(Option<usize>, usize)> = None;
    let mut slots: Vec<usize> = Vec::with_capacity(types.len());
    let mut asked = 0usize;
    for &desc in types {
        let mirror = if desc.len() == 1 {
            primitive_mirror_or_oom(shared, thread, desc, "ldc-methodtype-type")?
        } else {
            let boot = if bootstrap_walk {
                always_visible_boot_class(shared, desc)
            } else {
                None
            };
            match boot {
                Some(id) => class_mirror_or_oom(shared, thread, id, "ldc-methodtype-type")?,
                None => {
                    bootstrap_walk = false;
                    let (loader_slot, caller_slot) = match through {
                        Some(pair) => pair,
                        None => {
                            let loader = defining_loader_object(shared, thread, frame_class_id)?;
                            let loader_slot = loader.map(|l| {
                                thread.native_pin_roots.push(l);
                                thread.native_pin_roots.len() - 1
                            });
                            let caller = class_mirror_or_oom(
                                shared,
                                thread,
                                frame_class_id,
                                "ldc-methodtype-caller",
                            )?;
                            thread.native_pin_roots.push(caller);
                            let pair = (loader_slot, thread.native_pin_roots.len() - 1);
                            through = Some(pair);
                            pair
                        }
                    };
                    // gci i3/jd (`CRATONVM_METHOD_TYPE_NO_LOADER_RESOLVES_IN_VM`,
                    // default OFF; F1 of the concurrent-javac page): a class
                    // that reports no loader has its types resolved the way
                    // its constant pool resolves them -- in the class's own
                    // namespace, as HotSpot's `find_method_handle_type` does
                    // with the accessing class's loader -- instead of asking
                    // the BOOTSTRAP loader by `forName0(name, false, null, ..)`,
                    // which refuses a non-boot image class (`jdk.compiler`)
                    // with `ClassNotFoundException: p/X`. A genuinely
                    // boot-defined class gets the same class either way.
                    if loader_slot.is_none() && method_type_no_loader_resolves_in_vm() {
                        if cratonvm_types::flags().loader.dbg_access {
                            eprintln!(
                                "[ACCESS-DBG] ldc MethodType {descriptor}: {desc} resolved in the \
                                 class's own namespace (class {frame_class_id:?} reports no loader)"
                            );
                        }
                        let mirror =
                            class_mirror_for_field_descriptor(shared, thread, frame_class_id, desc)?;
                        thread.native_pin_roots.push(mirror);
                        slots.push(thread.native_pin_roots.len() - 1);
                        continue;
                    }
                    let name = match desc.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
                        Some(class_name) => class_name.replace('/', "."),
                        None => desc.replace('/', "."),
                    };
                    let name_str = intern_string_literal_or_oom(shared, thread, &name)?;
                    let loader = loader_slot.map(|i| thread.native_pin_roots[i]);
                    // gci i2/jm: the concurrent-javac narrowing's witness. A
                    // class whose `getClassLoader()` answered null asks the
                    // BOOTSTRAP loader here, which refuses a non-boot image
                    // class with `ClassNotFoundException: p/X` (the internal
                    // name) -- the text the failing first round prints.
                    if loader.is_none() && cratonvm_types::flags().loader.dbg_access {
                        eprintln!(
                            "[ACCESS-DBG] ldc MethodType {descriptor}: {name} asked of the \
                             bootstrap loader (class {frame_class_id:?} reports none)"
                        );
                    }
                    let caller = thread.native_pin_roots[caller_slot];
                    asked += 1;
                    match invoke_shared(
                        shared,
                        thread,
                        "java/lang/Class",
                        "forName0",
                        FOR_NAME0,
                        &[
                            Value::Object(Some(name_str)),
                            Value::Int(0),
                            Value::Object(loader),
                            Value::Object(Some(caller)),
                        ],
                    ) {
                        Ok(Some(Value::Object(Some(found)))) => found,
                        Err(e) => return Err(e),
                        Ok(_) => return Ok(None),
                    }
                }
            }
        };
        thread.native_pin_roots.push(mirror);
        slots.push(thread.native_pin_roots.len() - 1);
    }
    if cratonvm_types::flags().loader.dbg_access {
        eprintln!(
            "[ACCESS-DBG] ldc MethodType {descriptor}: resolved in the VM, {asked} of {} \
             types through the class's loader",
            types.len()
        );
    }
    let (param_slots, ret_slot) = match slots.split_last() {
        Some((ret, params)) => (params.to_vec(), *ret),
        None => return Ok(None),
    };
    let ptypes = super::native_alloc_collecting(
        shared,
        thread,
        "ldc-methodtype-ptypes",
        |shared, thread| {
            let mut ctx = crate::vm::NativeContextImpl { shared, thread };
            let array = ctx.new_ref_array(class_class, param_slots.len());
            for (i, &slot) in param_slots.iter().enumerate() {
                let mirror = ctx.thread.native_pin_roots[slot];
                ctx.set_array_element(array, i, Value::Object(Some(mirror)));
            }
            array
        },
    )?;
    let rtype = thread.native_pin_roots[ret_slot];
    Ok(Some((ptypes, rtype)))
}

/// The error a failed `CONSTANT_MethodType` resolution raises, given what
/// `MethodType.fromMethodDescriptorString` threw.
///
/// HotSpot resolves the descriptor's classes itself
/// (`SystemDictionary::find_method_handle_type`, with
/// `SignatureStream::NCDFError`), so a class it cannot find raises
/// `NoClassDefFoundError` naming that class. The JDK factory raises
/// `TypeNotPresentException` for the same miss — an unchecked exception that
/// is not a `LinkageError`, so `catch (LinkageError e)` around the `ldc` (or a
/// bootstrap's static argument, or a call-site type) did not fire and the
/// failure was not recorded for the entry (JVMS §5.4.3). On a
/// `TypeNotPresentException` this re-resolves the descriptor's classes
/// loader-faithfully, from the class that holds the entry, and raises the
/// first failure as the linkage error; anything else the factory threw, and a
/// descriptor whose classes all resolve here, passes through unchanged. The
/// success path never gets here, so it is untouched.
#[cold]
fn method_type_failure_as_linkage_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_class_id: ClassId,
    descriptor: &str,
    thrown: ObjectRef,
) -> MethodCallFailed {
    // `ClassNotFoundException` is what the VM-side walk's `Class.forName0`
    // raises for the same miss (`resolve_method_type_in_vm`, wave 48).
    let type_not_present = {
        let cm = shared.classes.class_manager.read();
        let cid = shared.mem.heap.class_id_of(thrown);
        [
            "java/lang/TypeNotPresentException",
            "java/lang/ClassNotFoundException",
        ]
        .iter()
        .any(|name| {
            cm.get_loaded_class_id(name)
                .is_some_and(|miss| cm.is_subclass_of(cid, miss))
        })
    };
    if !type_not_present {
        return MethodCallFailed::ExceptionThrown(thrown);
    }
    // Resolution can load classes and run Java, so the original throwable is
    // pinned until it is known whether it is still the answer.
    let pin = thread.native_pin_roots.len();
    thread.native_pin_roots.push(thrown);
    let mut linkage = None;
    for name in descriptor_class_names(descriptor) {
        if let Err(e) = resolve_class_loader_aware(shared, thread, frame_class_id, name) {
            linkage = Some(convert_class_not_found(shared, thread, name, e));
            break;
        }
    }
    let original = thread.native_pin_roots[pin];
    thread.native_pin_roots.truncate(pin);
    linkage.unwrap_or(MethodCallFailed::ExceptionThrown(original))
}

/// The class names a method or field descriptor mentions, in order — array
/// element classes included (`[[Lp/A;` names `p/A`). A class name may itself
/// contain `L`, so a name is read to its `;` rather than searched for.
pub(crate) fn descriptor_class_names(descriptor: &str) -> Vec<&str> {
    let bytes = descriptor.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'L' {
            let Some(len) = descriptor[i + 1..].find(';') else {
                break;
            };
            out.push(&descriptor[i + 1..i + 1 + len]);
            i += len + 2;
        } else {
            i += 1;
        }
    }
    out
}

/// `CRATONVM_METHOD_TYPE_NO_LOADER_RESOLVES_IN_VM` (default OFF, gci i3/jd):
/// [`method_type_walk`] resolves the types of a class that reports no loader
/// in the class's own namespace ([`class_mirror_for_field_descriptor`])
/// instead of asking the bootstrap loader through `Class.forName0`. F1 of
/// `docs/internal/gc/gci-i1orch-concurrent-in-process-javac-first-round-differs-FIXED-20261017.md`;
/// rows `docs/internal/gc-round-gci-20261011/vi3-jd.list` decide a flip. Read
/// per call: only the no-loader arm of the walk asks it.
fn method_type_no_loader_resolves_in_vm() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_METHOD_TYPE_NO_LOADER_RESOLVES_IN_VM")
}

/// The `java.lang.ClassLoader` object that defined `class_id`, or `None` for a
/// bootstrap-defined class (which is what the JDK's own descriptor factories
/// take `null` to mean).
///
/// Asked through `Class.getClassLoader()` rather than the `ClassLoaderId`
/// enum, because the callers here need the loader **object** to hand to Java,
/// and `getClassLoader` is the one place that already reconciles the mirror's
/// field, the defining-loader side table and the built-in loader singletons.
///
/// `Err` only when the class's mirror cannot be allocated on a full heap
/// (`OutOfMemoryError`, gc-common w9-c): answering `None` there would name the
/// bootstrap loader and resolve the descriptor in the wrong namespace. A
/// failure of `getClassLoader` itself answers `None`, as before.
fn defining_loader_object(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let mirror = class_mirror_or_oom(shared, thread, class_id, "ldc-methodtype-loader")?;
    Ok(
        match invoke_shared(
            shared,
            thread,
            "java/lang/Class",
            "getClassLoader",
            "()Ljava/lang/ClassLoader;",
            &[Value::Object(Some(mirror))],
        ) {
            Ok(Some(Value::Object(loader))) => loader,
            // gci i2/jm: a failed ask is taken as the bootstrap loader; say so
            // (the concurrent-javac narrowing needs to tell this from a null
            // answer).
            other => {
                if cratonvm_types::flags().loader.dbg_access {
                    eprintln!(
                        "[ACCESS-DBG] defining_loader_object: getClassLoader of class \
                         {class_id:?} failed ({other:?}); taken as the bootstrap loader"
                    );
                }
                None
            }
        },
    )
}

/// The `Class` mirror named by a **field** descriptor (`I`, `Ljava/lang/X;`,
/// `[I`), resolved loader-faithfully from `frame_class_id` — the reference
/// types go through the same [`resolve_class_loader_aware`] every
/// `new`/`checkcast`/`ldc X.class` uses, so a `CONSTANT_MethodHandle` field
/// accessor cannot resolve to a different copy of a class than the rest of the
/// same class file does.
fn class_mirror_for_field_descriptor(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_class_id: ClassId,
    descriptor: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(inner) = descriptor
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
    {
        let class_id = resolve_class_loader_aware(shared, thread, frame_class_id, inner)
            .map_err(|e| convert_class_not_found(shared, thread, inner, e))?;
        return class_mirror_or_oom(shared, thread, class_id, "ldc-methodhandle-field-type");
    }
    if descriptor.starts_with('[') {
        let class_id = resolve_class_loader_aware(shared, thread, frame_class_id, descriptor)
            .map_err(|e| convert_class_not_found(shared, thread, descriptor, e))?;
        return class_mirror_or_oom(shared, thread, class_id, "ldc-methodhandle-field-type");
    }
    // The primitive mirrors are normally made at VM start-up with the
    // wrappers' `TYPE` fields (`pre_init_wrapper_type_fields`), so this is a
    // cache hit after boot; at most nine exist per VM.
    primitive_mirror_or_oom(shared, thread, descriptor, "ldc-methodhandle-field-type")
}

/// JVMS §5.4.3.5 resolution of a `CONSTANT_MethodHandle` reached from `ldc` /
/// `ldc_w`.
///
/// **Twin discipline.** The constant-pool *decode* (Table 5.4.3.5-A: which
/// reference kinds read a `Fieldref` and which read a
/// `Methodref`/`InterfaceMethodref`) is not reimplemented here — it is
/// `invokedynamic`'s `resolve_method_handle_full`, called by the caller. What
/// is added here is the second half JVMS gives this opcode and gives
/// `invokedynamic` no reason to have: turning the resolved member into a real
/// `java.lang.invoke.MethodHandle`.
///
/// That half is deliberately expressed as `MethodHandles.Lookup.find*` calls
/// rather than as a Rust construction of a handle object. The VM has a second
/// method-handle representation (`native-builtins`' `MH_KIND_*` shim objects,
/// minted by `alloc_method_handle`) which exists only in the compatible-mode
/// native surface and is refused under `--jdk-only`. Routing through `Lookup`
/// means this opcode yields whichever representation the running mode's
/// `Lookup` yields — the shim in compatible mode, a genuine JDK handle in
/// strict mode — instead of hardwiring one of them into the opcode and
/// creating a third twin.
///
/// `MethodHandles.lookup()` is caller-sensitive; invoked from here the
/// innermost Java frame is the method executing the `ldc`, so the lookup class
/// is the class whose constant pool holds the entry, which is exactly the
/// access context JVMS §5.4.3.5 specifies. (Same property `invokedynamic`'s
/// `bootstrap_generic` relies on for the bootstrap `Lookup`.)
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_method_handle_constant(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_class_id: ClassId,
    kind: MethodHandleKind,
    class_name: &str,
    member_name: &str,
    descriptor: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    const LOOKUP: &str = "java/lang/invoke/MethodHandles$Lookup";
    const FIND_MEMBER: &str = "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;";
    const FIND_SPECIAL: &str = "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;";
    const FIND_CTOR: &str =
        "(Ljava/lang/Class;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;";
    const FIND_ACCESSOR: &str =
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;";

    let lookup = match invoke_shared(
        shared,
        thread,
        "java/lang/invoke/MethodHandles",
        "lookup",
        "()Ljava/lang/invoke/MethodHandles$Lookup;",
        &[],
    )? {
        Some(Value::Object(Some(l))) => l,
        _ => {
            return Err(VmError::Internal {
                message: "ldc: MethodHandles.lookup() produced no Lookup".to_string(),
            }
            .into())
        }
    };
    // Every reference below has to survive the allocations that follow it, so
    // each is pinned as it is produced and re-read from its slot afterwards.
    // `invoke_shared` pins its own argument slice, so the pins are released
    // immediately before the dispatch.
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(lookup); // + 0

    // Immediately-invoked so an early `?` cannot leave the pins behind: an
    // abandoned pin slot is never popped by anyone else (every other user
    // truncates to its OWN base), so it would keep its object alive for the
    // lifetime of the thread.
    let prepared = (|| -> Result<(), MethodCallFailed> {
        // Spelled out rather than chained through `map_err`: `thread` is a
        // captured upvar here, and a nested closure that re-borrows it inside
        // the same expression is not worth the ambiguity.
        let refc_id = match resolve_class_loader_aware(shared, thread, frame_class_id, class_name) {
            Ok(id) => id,
            Err(e) => return Err(convert_class_not_found(shared, thread, class_name, e)),
        };
        // JVMS §5.4.4 for the member, from THIS class (wave 40, lane L4): the
        // native `Lookup.find*` gate below admits every member for a
        // full-power lookup and never asks the lookup class. `--compatible`
        // too since wave 50 (`ClassRealm::ldc_handle_access_checks`).
        if shared.classes.ldc_handle_access_checks {
            if let Some(refusal) = method_handle_constant_access_refusal(
                shared,
                thread,
                frame_class_id,
                refc_id,
                kind,
                class_name,
                member_name,
                descriptor,
            ) {
                return Err(refusal);
            }
        }
        // Every allocation here follows `new`'s contract (collect, retry,
        // `OutOfMemoryError`); the infallible mirror and String aborted the
        // process when the first resolution met a full heap (gc-common w9-c,
        // `common-w8v-constant-resolution-strings-abort-on-a-full-heap`).
        // Each collection is safe: everything produced so far is pinned.
        let refc = class_mirror_or_oom(shared, thread, refc_id, "ldc-methodhandle")?;
        thread.native_pin_roots.push(refc); // + 1

        let name_obj = intern_string_literal_or_oom(shared, thread, member_name)?;
        thread.native_pin_roots.push(name_obj); // + 2

        // Reference kinds 1..4 name a FIELD, so their "type" argument is the
        // field's `Class`; 5..9 name a method, so it is a `MethodType`.
        let type_obj = match kind {
            MethodHandleKind::GetField
            | MethodHandleKind::GetStatic
            | MethodHandleKind::PutField
            | MethodHandleKind::PutStatic => {
                class_mirror_for_field_descriptor(shared, thread, frame_class_id, descriptor)?
            }
            _ => resolve_method_type_constant(shared, thread, frame_class_id, descriptor)?,
        };
        thread.native_pin_roots.push(type_obj); // + 3

        // `findSpecial`'s trailing `specialCaller` is the class that holds the
        // constant-pool entry.
        let caller = class_mirror_or_oom(shared, thread, frame_class_id, "ldc-methodhandle")?;
        thread.native_pin_roots.push(caller); // + 4
        Ok(())
    })();
    if let Err(e) = prepared {
        thread.native_pin_roots.truncate(pin_base);
        return Err(e);
    }

    let a_lookup = Value::Object(thread.native_pin_roots.get(pin_base).copied());
    let a_refc = Value::Object(thread.native_pin_roots.get(pin_base + 1).copied());
    let a_name = Value::Object(thread.native_pin_roots.get(pin_base + 2).copied());
    let a_type = Value::Object(thread.native_pin_roots.get(pin_base + 3).copied());
    let a_caller = Value::Object(thread.native_pin_roots.get(pin_base + 4).copied());
    thread.native_pin_roots.truncate(pin_base);

    let (method, method_desc, args): (&str, &str, Vec<Value>) = match kind {
        MethodHandleKind::GetField => (
            "findGetter",
            FIND_ACCESSOR,
            vec![a_lookup, a_refc, a_name, a_type],
        ),
        MethodHandleKind::GetStatic => (
            "findStaticGetter",
            FIND_ACCESSOR,
            vec![a_lookup, a_refc, a_name, a_type],
        ),
        MethodHandleKind::PutField => (
            "findSetter",
            FIND_ACCESSOR,
            vec![a_lookup, a_refc, a_name, a_type],
        ),
        MethodHandleKind::PutStatic => (
            "findStaticSetter",
            FIND_ACCESSOR,
            vec![a_lookup, a_refc, a_name, a_type],
        ),
        MethodHandleKind::InvokeStatic => (
            "findStatic",
            FIND_MEMBER,
            vec![a_lookup, a_refc, a_name, a_type],
        ),
        // `findVirtual` is specified to handle the interface case too, so
        // REF_invokeInterface shares it (this is what the JDK's own
        // `MethodHandleNatives.linkMethodHandleConstant` does).
        MethodHandleKind::InvokeVirtual | MethodHandleKind::InvokeInterface => (
            "findVirtual",
            FIND_MEMBER,
            vec![a_lookup, a_refc, a_name, a_type],
        ),
        MethodHandleKind::InvokeSpecial => (
            "findSpecial",
            FIND_SPECIAL,
            vec![a_lookup, a_refc, a_name, a_type, a_caller],
        ),
        // REF_newInvokeSpecial's descriptor is already `(params)V`, which is
        // the shape `findConstructor` wants; the member name is `<init>` and
        // is not passed.
        MethodHandleKind::NewInvokeSpecial => {
            ("findConstructor", FIND_CTOR, vec![a_lookup, a_refc, a_type])
        }
    };

    let found = invoke_shared(shared, thread, LOOKUP, method, method_desc, &args)
        .map_err(|e| map_lookup_exception_to_error(shared, thread, e))?;
    match found {
        Some(Value::Object(Some(mh))) => Ok(mh),
        _ => Err(VmError::Internal {
            message: format!(
                "ldc: Lookup.{method} produced no MethodHandle for \
                 {kind} {class_name}.{member_name}{descriptor}"
            ),
        }
        .into()),
    }
}

/// The `IllegalAccessError` resolving a `CONSTANT_MethodHandle` of
/// `frame_class_id` owes when the member it names is ANOTHER class's PRIVATE
/// member that JVMS §5.4.4 refuses it (a nestmate is admitted), or `None`.
/// Interpreter round i1 wave 40, lane L4
/// (`tools/probes/interp/L4/L4W40LdcMethodHandleAccess.java`).
///
/// HotSpot's texts: a field handle is `MemberName.makeAccessException`'s
/// (`member is private: C.f/java.lang.String/getField, from class X`, the
/// module part not reproduced) as an `IllegalAccessError` caused by the
/// `IllegalAccessException` (`mapLookupExceptionToError`); a method handle is
/// `LinkResolver`'s own `IllegalAccessError` (`class X tried to access private
/// method '...'`), with no cause.
///
/// PRIVATE, (wave 42) PACKAGE-PRIVATE and (wave 44) PROTECTED members. A
/// `protected` member reached through a handle from a SUBCLASS is admitted by
/// HotSpot with the handle's receiver restricted to the caller, which the
/// §5.4.4 receiver clause of `member_access_refusal` would refuse, so only a
/// caller that is not a subclass of the declaring class is refused
/// ([`protected_refusal_is_hotspots`]; `member is protected: ...` for a field,
/// `class X tried to access protected method '...'` for a method, measured by
/// `tools/probes/interp/L4/L4W44LdcProtectedHandle.java`). A package-private member of another run-time package is
/// `member is private to package: ...` for a field and `class X tried to
/// access method '...'` for a method (measured on HotSpot 25, probe
/// `tools/probes/interp/L4/L4W42LdcPackagePrivateHandle.java`); only when the
/// member's declaring class and the named class are public, since
/// `Lookup.accessFailedMessage` names a non-public class first (`class is
/// not public`), a text not reproduced here.
///
/// A private member whose verdict rests on a `NestHost` claim no door has
/// loaded the host of is judged after the host is loaded through the
/// member's loader (`field_access::settle_nest_host_claims`; JIT round 20
/// wave 16, lane nest), as HotSpot's `InstanceKlass::nest_host` loads it.
#[allow(clippy::too_many_arguments)]
fn method_handle_constant_access_refusal(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_class_id: ClassId,
    refc_id: ClassId,
    kind: MethodHandleKind,
    class_name: &str,
    member_name: &str,
    descriptor: &str,
) -> Option<MethodCallFailed> {
    let nest_unsettled = std::cell::Cell::new(false);
    let nest_mark = super::field_access::nest_host_claim_mark(shared);
    let first = method_handle_constant_access_refusal_once(
        shared,
        thread,
        frame_class_id,
        refc_id,
        kind,
        (class_name, member_name, descriptor),
        &nest_unsettled,
    );
    if !nest_unsettled.get() {
        return first;
    }
    if let Err(e) = super::field_access::settle_nest_host_claims(shared, thread, nest_mark) {
        return Some(e);
    }
    method_handle_constant_access_refusal_once(
        shared,
        thread,
        frame_class_id,
        refc_id,
        kind,
        (class_name, member_name, descriptor),
        &std::cell::Cell::new(false),
    )
}

/// [`method_handle_constant_access_refusal`] once: `None` without building
/// anything when `nest_unsettled` was raised (the caller settles and asks
/// again).
fn method_handle_constant_access_refusal_once(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_class_id: ClassId,
    refc_id: ClassId,
    kind: MethodHandleKind,
    (class_name, member_name, descriptor): (&str, &str, &str),
    nest_unsettled: &std::cell::Cell<bool>,
) -> Option<MethodCallFailed> {
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    let field_kind = match kind {
        MethodHandleKind::GetField => Some("getField"),
        MethodHandleKind::GetStatic => Some("getStatic"),
        MethodHandleKind::PutField => Some("putField"),
        MethodHandleKind::PutStatic => Some("putStatic"),
        _ => None,
    };
    if class_name.starts_with('[') {
        return None;
    }
    let (message, with_cause) = {
        let cm = shared.classes.class_manager.read();
        let caller = {
            let class = cm.get_class(frame_class_id)?;
            crate::runtime::exceptions::hotspot_external_name_of_class(&class.name, class.hidden)
        };
        match field_kind {
            Some(field_kind) => {
                let private = super::field_access::field_handle_access_refusal(
                    shared,
                    &cm,
                    frame_class_id,
                    refc_id,
                    member_name,
                    descriptor,
                    nest_unsettled,
                )?;
                let reason = if private {
                    "private"
                } else {
                    let (_, field, declaring) =
                        crate::classloading::find_field_recursive_by_descriptor(
                            refc_id,
                            member_name,
                            descriptor,
                            &cm.class_store,
                        )?;
                    let flags = field.access_flags.bits();
                    if !handle_member_classes_public(&cm, refc_id, declaring) {
                        return None;
                    }
                    if flags & MEMBER_ACCESS_BITS == 0 {
                        "private to package"
                    } else if protected_refusal_is_hotspots(&cm, flags, frame_class_id, declaring) {
                        "protected"
                    } else {
                        return None;
                    }
                };
                (
                    format!(
                        "member is {reason}: {}.{}/{}/{field_kind}, from class {caller}",
                        crate::runtime::exceptions::hotspot_external_name(&class_name),
                        member_name,
                        crate::runtime::invokedynamic::descriptor_class_name(descriptor),
                    ),
                    true,
                )
            }
            None => {
                let store = cm.class_store();
                let holder = crate::runtime::resolve::selection::resolve_declaring(
                    store,
                    refc_id,
                    member_name,
                    descriptor,
                )?;
                let flags = store
                    .get(holder)?
                    .find_method(member_name, descriptor)?
                    .access_flags;
                let private = flags.contains(MethodAccessFlags::PRIVATE);
                let package_private = flags.bits() & MEMBER_ACCESS_BITS == 0
                    && handle_member_classes_public(&cm, refc_id, holder);
                let protected =
                    protected_refusal_is_hotspots(&cm, flags.bits(), frame_class_id, holder);
                if !private && !package_private && !protected {
                    return None;
                }
                let message = super::field_access::method_member_access_refusal_noting(
                    shared,
                    &cm,
                    frame_class_id,
                    refc_id,
                    member_name,
                    descriptor,
                    nest_unsettled,
                )?;
                (message, false)
            }
        }
    };
    if nest_unsettled.get() {
        return None;
    }
    if crate::runtime::invokedynamic::dbg_indy_all_enabled() {
        eprintln!("[indy-all] ldc method handle refused: {message}");
    }
    if !with_cause {
        return Some(
            match crate::runtime::exceptions::create_exception_object(
                shared,
                thread,
                "java/lang/IllegalAccessError",
                Some(message.as_str()),
            ) {
                Ok(error) => MethodCallFailed::ExceptionThrown(error),
                Err(e) => e,
            },
        );
    }
    let cause = match crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        "java/lang/IllegalAccessException",
        Some(message.as_str()),
    ) {
        Ok(cause) => cause,
        Err(e) => return Some(e),
    };
    Some(crate::runtime::invokedynamic::throwable_with_cause(
        shared,
        thread,
        "java/lang/IllegalAccessError",
        &message,
        cause,
    ))
}

/// `ACC_PUBLIC | ACC_PRIVATE | ACC_PROTECTED` of a field or method: none set
/// is package access (JVMS §4.5, §4.6).
const MEMBER_ACCESS_BITS: u16 = 0x0001 | 0x0002 | 0x0004;

/// Is a §5.4.4 refusal of a PROTECTED member (`flags`, declared by
/// `declaring`) one HotSpot makes for a `CONSTANT_MethodHandle` too? Only
/// when the caller is not a subclass of the declaring class: a subclass is
/// admitted by `Lookup.checkAccess` whatever class the reference names, and
/// the handle's receiver is restricted to the caller instead
/// (`restrictProtectedReceiver`), where `member_access_refusal`'s receiver
/// clause would refuse a reference naming a sibling class. Same-package
/// callers never reach here (the §5.4.4 check admits them). Interpreter round
/// i1 wave 44, lane L4 (`tools/probes/interp/L4/L4W44LdcProtectedHandle.java`).
fn protected_refusal_is_hotspots(
    cm: &crate::classloading::ClassManager,
    flags: u16,
    caller: ClassId,
    declaring: ClassId,
) -> bool {
    const ACC_PROTECTED: u16 = 0x0004;
    flags & ACC_PROTECTED != 0 && caller != declaring && !cm.is_subclass_of(caller, declaring)
}

/// `Lookup.accessFailedMessage`'s class check, for a caller in another
/// package: the member's declaring class is public, and so is the class the
/// reference names (or it is the declaring class). Only then is a refused
/// package-private member's message `member is private to package`.
fn handle_member_classes_public(
    cm: &crate::classloading::ClassManager,
    refc_id: ClassId,
    declaring: ClassId,
) -> bool {
    let public = |id: ClassId| {
        cm.get_class(id)
            .is_some_and(|c| c.access_flags.bits() & 0x0001 != 0)
    };
    public(declaring) && (declaring == refc_id || public(refc_id))
}

/// JVMS §5.4.3.5: resolving a method handle constant fails with a LINKAGE
/// error, not with the checked exception a `Lookup.find*` call throws. This is
/// the JDK's own `MethodHandleNatives.mapLookupExceptionToError`, which
/// HotSpot's `linkMethodHandleConstant` applies to the same `find*` calls:
/// `IllegalAccessException` → `IllegalAccessError`, `NoSuchMethodException` →
/// `NoSuchMethodError`, `NoSuchFieldException` → `NoSuchFieldError`, any other
/// `ReflectiveOperationException` → `IncompatibleClassChangeError`, each with
/// the original as its cause. Anything that is not a
/// `ReflectiveOperationException` (an `Error`, a `RuntimeException`, a VM
/// failure) passes through unchanged.
///
/// Before 2026-09-23 the checked exception escaped the `ldc` itself — a
/// `catch (IllegalAccessError e)` around it did not fire, and a
/// `LinkageError` could not be recorded for the entry (JVMS §5.4.3).
#[cold]
fn map_lookup_exception_to_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    failure: MethodCallFailed,
) -> MethodCallFailed {
    // The compatible-mode native `Lookup.find*` reports its checked exception
    // as a not-yet-materialised `VmError::Runtime` (`lang_invoke.rs`'s
    // `no_such_method_error` etc.), which the arm below never saw: a missing
    // method-handle constant surfaced as `NoSuchMethodException` where HotSpot
    // raises `NoSuchMethodError` (probe L6/GenericIndyStaticArgsProbe).
    // Materialise the reflective variants first so both shapes map alike.
    let failure = match failure {
        MethodCallFailed::InternalError(VmError::Runtime(
            re @ (crate::error::RuntimeError::NoSuchMethodException { .. }
            | crate::error::RuntimeError::NoSuchFieldException { .. }
            | crate::error::RuntimeError::IllegalAccessException { .. }
            | crate::error::RuntimeError::ClassNotFoundException { .. }),
        )) => crate::runtime::exceptions::throw_runtime_error(shared, thread, re),
        other => other,
    };
    let MethodCallFailed::ExceptionThrown(ex) = failure else {
        return failure;
    };
    let error_class = {
        let cm = shared.classes.class_manager.read();
        let cid = shared.mem.heap.class_id_of(ex);
        let is = |name: &str| {
            cm.get_loaded_class_id(name)
                .is_some_and(|target| cm.is_subclass_of(cid, target))
        };
        if !is("java/lang/ReflectiveOperationException") {
            return MethodCallFailed::ExceptionThrown(ex);
        }
        if is("java/lang/IllegalAccessException") {
            "java/lang/IllegalAccessError"
        } else if is("java/lang/NoSuchMethodException") {
            "java/lang/NoSuchMethodError"
        } else if is("java/lang/NoSuchFieldException") {
            "java/lang/NoSuchFieldError"
        } else {
            "java/lang/IncompatibleClassChangeError"
        }
    };
    // The JDK gives the ICCE fallback no message; the three others copy it.
    let message = if error_class == "java/lang/IncompatibleClassChangeError" {
        None
    } else {
        throwable_detail_message(shared, ex)
    };
    let pin = thread.native_pin_roots.len();
    thread.native_pin_roots.push(ex);
    let mapped = match crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        error_class,
        message.as_deref(),
    ) {
        Ok(err) => {
            let err_slot = thread.native_pin_roots.len();
            thread.native_pin_roots.push(err);
            let cause = thread.native_pin_roots[pin];
            let _ = invoke_shared(
                shared,
                thread,
                "java/lang/Throwable",
                "initCause",
                "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
                &[Value::Object(Some(err)), Value::Object(Some(cause))],
            );
            MethodCallFailed::ExceptionThrown(thread.native_pin_roots[err_slot])
        }
        // Could not build the error: surface the original rather than lose it.
        Err(_) => MethodCallFailed::ExceptionThrown(thread.native_pin_roots[pin]),
    };
    thread.native_pin_roots.truncate(pin);
    mapped
}

/// The `detailMessage` of a throwable when it is a `java.lang.String`.
pub(super) fn throwable_detail_message(shared: &SharedVm, throwable: ObjectRef) -> Option<String> {
    let message_ref = {
        let cm = shared.classes.class_manager.read();
        let cid = shared.mem.heap.class_id_of(throwable);
        let idx =
            crate::vm::resolve_field_index_in_hierarchy(cid, "detailMessage", &cm.class_store)?;
        match shared.mem.heap.get_field(throwable, idx) {
            Value::Object(Some(s))
                if cm
                    .get_class(shared.mem.heap.class_id_of(s))
                    .is_some_and(|c| &*c.name == "java/lang/String") =>
            {
                s
            }
            _ => return None,
        }
    };
    crate::vm::read_java_string(&shared.mem.heap, message_ref)
}

/// Resolve the `CONSTANT_Dynamic` at `cp_index` in `frame_class_id`, memoised
/// per (class, constant-pool index).
///
/// A condy `ldc` is a **bootstrap call**, not a constant read: JVMS §5.4.3.6
/// says the bootstrap method runs once per constant-pool entry and its result
/// is remembered, which is what the `resolution_cache` condy map is.
///
/// Split out of `execute_ldc` so `ldc2_w` can call it: JVMS §6.5 `ldc2_w`
/// accepts a `CONSTANT_Dynamic` whose field type is `long`/`double` (Java 11+),
/// and `classloading::verify_insn`'s `verify_ldc2w` already verifies exactly
/// that shape. The interpreter used to refuse it.
///
/// # The bootstrap method really runs (2026-09-23)
///
/// Until 2026-09-23 this pattern-matched a list of `ConstantBootstraps` /
/// `ObjectMethods` / `SwitchBootstraps` entry points and answered
/// `default_for_descriptor` — `0`, `0L`, `0.0` or `null` — for everything else,
/// user-defined bootstraps included, and for `ConstantBootstraps.invoke`, whose
/// `MethodHandle` argument it never called: a wrong value instead of either the
/// right value or an error. Now [`condy_fast_answer`] keeps a Rust answer only
/// where it provably equals what the bootstrap would return, and every other
/// condy goes through [`invoke_condy_bootstrap`]: JVMS §5.4.3.6's
/// `(Lookup, String, Class, static args...)` call, the result converted to the
/// field type, an abrupt completion wrapped in `BootstrapMethodError`, and a
/// `LinkageError` outcome recorded so every later execution rethrows it
/// (§5.4.3, [`record_resolution_failure`]).
pub(crate) fn resolve_condy_constant(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_class_id: ClassId,
    cp_index: u16,
) -> Result<Value, MethodCallFailed> {
    if let Some(val) = cached_cp_constant(shared, frame_class_id, cp_index) {
        if remap_trace_on() {
            if let Value::Object(Some(o)) = &val {
                push_prov_record(o.as_ptr() as usize, "ldc-condy-cached");
            }
        }
        return Ok(val);
    }
    if let Some(recorded) = recorded_resolution_failure(shared, thread, frame_class_id, cp_index) {
        return Err(recorded);
    }

    // Before the pool read: the bootstrap below runs Java, so a redefinition
    // of this class can land before either record (interpreter round i1 wave
    // 23, lane L5; `ResolutionCache::fill_snapshot`).
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let condy = decode_condy(shared, frame_class_id, cp_index)?;
    let result = match condy_fast_answer(shared, thread, frame_class_id, &condy) {
        Ok(Some(v)) => Ok(v),
        Ok(None) => invoke_condy_bootstrap(shared, thread, frame_class_id, &condy, fill_as_of),
        Err(e) => Err(e),
    }
    .map_err(|e| {
        record_resolution_failure_as_of(shared, frame_class_id, cp_index, e, fill_as_of)
    })?;
    // JVMS §5.4.3: a failure another thread recorded for the entry while this
    // bootstrap ran is the entry's outcome (wave 38, lane L5; `--jdk-only`).
    // Allocates only on that path, where `result` is dropped.
    if let Some(recorded) =
        recorded_resolution_failure_after_success(shared, thread, frame_class_id, cp_index)
    {
        return Err(recorded);
    }

    // JVMS §5.4.3.6: when several threads resolve one entry at once, the
    // first result to be recorded is the one every thread observes. The
    // permanent store is insert-if-absent under its write lock, so a thread
    // that lost the race gets the winner back and hands that out, not its own
    // object; and it is never evicted, so the bootstrap cannot run again.
    // A redefinition of the class since `fill_as_of` records nothing: this
    // execution (of the old pool's entry) gets its own result, and the new
    // pool's entry bootstraps afresh.
    let result =
        record_cp_constant_permanent_as_of(shared, frame_class_id, cp_index, result, fill_as_of);
    if remap_trace_on() {
        if let Value::Object(Some(o)) = &result {
            push_prov_record(o.as_ptr() as usize, "ldc-condy");
        }
    }
    Ok(result)
}

/// A `CONSTANT_Dynamic` entry decoded under the `class_manager` read lock, in
/// owned form so the lock can be dropped before any Java runs.
struct CondyDecoded {
    name: String,
    descriptor: String,
    bsm_kind: MethodHandleKind,
    bsm_class: String,
    bsm_method: String,
    bsm_desc: String,
    args: Vec<CondyArg>,
}

/// One static bootstrap argument (JVMS §4.7.23: any loadable constant).
enum CondyArg {
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    /// gci i33/sc: carries the argument's own `CONSTANT_String` index, so its
    /// class's constant record can own the String as HotSpot's resolved
    /// references do ([`condy_string_arg`]).
    Str {
        cp_index: u16,
        text: String,
    },
    /// A string constant carrying lone surrogates (see `LdcValue::WideStr`);
    /// recorded at `cp_index` like [`CondyArg::Str`] ([`condy_wide_string_arg`]).
    WideStr {
        cp_index: u16,
        units: Vec<u16>,
    },
    Class(String),
    /// Resolved through the same per-entry record `ldc` of that index uses,
    /// so the two agree on identity.
    MethodType {
        cp_index: u16,
        descriptor: String,
    },
    MethodHandle {
        cp_index: u16,
        kind: MethodHandleKind,
        class_name: String,
        member_name: String,
        descriptor: String,
    },
    /// A nested `CONSTANT_Dynamic`, resolved (and recorded) first.
    Dynamic(u16),
}

fn decode_condy(
    shared: &SharedVm,
    frame_class_id: ClassId,
    cp_index: u16,
) -> Result<CondyDecoded, MethodCallFailed> {
    let cm = shared.classes.class_manager.read();
    let class = cm
        .get_class(frame_class_id)
        .ok_or_else(|| VmError::Internal {
            message: "condy: current class not found".to_string(),
        })?;
    let cp = &class.constant_pool;
    let malformed = |message: String| {
        VmError::Linkage(LinkageError::ClassFormatError {
            class_name: class.name.to_string(),
            message,
        })
    };
    let (bsm_index, nat_index) = match cp.get(cp_index) {
        Some(ConstantPoolEntry::Dynamic {
            bootstrap_method_attr_index,
            name_and_type_index,
        }) => (*bootstrap_method_attr_index, *name_and_type_index),
        _ => {
            return Err(malformed(format!("condy: cp#{cp_index} is not a CONSTANT_Dynamic")).into())
        }
    };
    let (name, descriptor) = cp
        .get_name_and_type(nat_index)
        .ok_or_else(|| malformed(format!("ldc: invalid condy name_and_type at #{nat_index}")))?;
    let bsm = class
        .bootstrap_methods
        .get(bsm_index as usize) // Widening: index conversion
        .ok_or_else(|| VmError::Internal {
            message: format!("condy: bootstrap method index {bsm_index} out of bounds"),
        })?;
    let handle =
        crate::runtime::invokedynamic::resolve_method_handle_full(cp, bsm.bootstrap_method_ref)?;
    let mut args = Vec::with_capacity(bsm.bootstrap_arguments.len());
    for &idx in &bsm.bootstrap_arguments {
        let bad = || malformed(format!("condy: unusable bootstrap argument cp#{idx}"));
        let arg = match cp.get(idx) {
            Some(ConstantPoolEntry::Integer(v)) => CondyArg::Int(*v),
            Some(ConstantPoolEntry::Long(v)) => CondyArg::Long(v.get()),
            Some(ConstantPoolEntry::Float(v)) => CondyArg::Float(*v),
            Some(ConstantPoolEntry::Double(v)) => CondyArg::Double(v.get()),
            Some(ConstantPoolEntry::StringReference { string_index }) => {
                match cp.get_utf8_wide(*string_index) {
                    Some(units) => CondyArg::WideStr {
                        cp_index: idx,
                        units: units.to_vec(),
                    },
                    None => CondyArg::Str {
                        cp_index: idx,
                        text: cp.get_utf8(*string_index).ok_or_else(bad)?.to_string(),
                    },
                }
            }
            Some(ConstantPoolEntry::ClassReference { name_index }) => {
                CondyArg::Class(cp.get_utf8(*name_index).ok_or_else(bad)?.to_string())
            }
            Some(ConstantPoolEntry::MethodType { descriptor_index }) => CondyArg::MethodType {
                cp_index: idx,
                descriptor: cp.get_utf8(*descriptor_index).ok_or_else(bad)?.to_string(),
            },
            Some(ConstantPoolEntry::MethodHandle { .. }) => {
                let mh = crate::runtime::invokedynamic::resolve_method_handle_full(cp, idx)?;
                CondyArg::MethodHandle {
                    cp_index: idx,
                    kind: mh.kind,
                    class_name: mh.class_name.to_string(),
                    member_name: mh.member_name.to_string(),
                    descriptor: mh.descriptor.to_string(),
                }
            }
            Some(ConstantPoolEntry::Dynamic { .. }) => CondyArg::Dynamic(idx),
            _ => return Err(bad().into()),
        };
        args.push(arg);
    }
    Ok(CondyDecoded {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        bsm_kind: handle.kind,
        bsm_class: handle.class_name.to_string(),
        bsm_method: handle.member_name.to_string(),
        bsm_desc: handle.descriptor.to_string(),
        args,
    })
}

/// The `ConstantBootstraps` answers that can be given without running the
/// bootstrap method because they provably EQUAL what it would return. `None`
/// sends the entry to [`invoke_condy_bootstrap`], which is also where every
/// failure of these bootstraps is produced (with the JDK's own exception).
///
/// * `nullConstant` — `null`, for a reference field type only (a primitive
///   type is an `IllegalArgumentException` the real method raises).
/// * `primitiveClass` — the primitive mirror named by a one-character
///   descriptor, for a `Class`-typed entry only.
/// * `enumConstant` / `getStaticFinal` — the value of a `public static` field
///   matched by name AND descriptor after initializing its class, when it is a
///   non-null reference. That is what `Enum.valueOf` /
///   `Lookup.findStaticGetter(..).invoke()` read. A primitive or null value,
///   a missing field or a non-public one goes to the real bootstrap: the
///   statics store does not carry the `long`/`double` tag reliably across the
///   `Value` boundary (see `push_static_field_value`), and access checks and
///   the error for a missing constant are the JDK's to produce.
///
/// The pre-2026-09-23 version answered `getStaticFinal`/`enumConstant` by
/// field NAME, indexed the statics store with the index among ALL fields
/// (instance fields included), did not initialize the class, and turned every
/// failure into a default value.
fn condy_fast_answer(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_class_id: ClassId,
    condy: &CondyDecoded,
) -> Result<Option<Value>, MethodCallFailed> {
    if !matches!(condy.bsm_kind, MethodHandleKind::InvokeStatic)
        || condy.bsm_class != "java/lang/invoke/ConstantBootstraps"
    {
        return Ok(None);
    }
    // The bootstrap's DESCRIPTOR is part of what makes the answer provable
    // (interpreter round i1 wave 29, lane L4): a `getStaticFinal` entry that
    // names the 4-argument form with no static argument fails with
    // `WrongMethodTypeException` on HotSpot, and was answered here.
    const LNC: &str = "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;";
    let bsm_is = |params_tail: &str, ret: &str| {
        condy
            .bsm_desc
            .strip_prefix(LNC)
            .and_then(|rest| rest.strip_prefix(params_tail))
            .and_then(|rest| rest.strip_prefix(')'))
            == Some(ret)
    };
    let reference_type = matches!(condy.descriptor.as_bytes().first(), Some(b'L' | b'['));
    match condy.bsm_method.as_str() {
        "nullConstant"
            if reference_type && condy.args.is_empty() && bsm_is("", "Ljava/lang/Object;") =>
        {
            Ok(Some(Value::Object(None)))
        }
        "primitiveClass"
            if condy.args.is_empty()
                && bsm_is("", "Ljava/lang/Class;")
                && condy.descriptor == "Ljava/lang/Class;"
                && condy.name.len() == 1
                && "ZBCSIJFDV".contains(condy.name.as_str()) =>
        {
            Ok(Some(Value::Object(Some(primitive_mirror_or_oom(
                shared,
                thread,
                &condy.name,
                "condy-primitive-class",
            )?))))
        }
        "enumConstant" if condy.args.is_empty() && bsm_is("", "Ljava/lang/Enum;") => {
            match object_type_name(&condy.descriptor) {
                Some(owner) => condy_public_static_reference(
                    shared,
                    thread,
                    frame_class_id,
                    owner,
                    condy,
                    CondyStaticRead::EnumConstant,
                ),
                None => Ok(None),
            }
        }
        "getStaticFinal" if reference_type => {
            // 4-argument form: the declaring class is the one static argument.
            // 3-argument form: the field type itself (a class type here).
            let owner = match condy.args.as_slice() {
                [CondyArg::Class(owner)] if bsm_is("Ljava/lang/Class;", "Ljava/lang/Object;") => {
                    Some(owner.as_str())
                }
                [] if bsm_is("", "Ljava/lang/Object;") => object_type_name(&condy.descriptor),
                _ => None,
            };
            match owner {
                Some(owner) => condy_public_static_reference(
                    shared,
                    thread,
                    frame_class_id,
                    owner,
                    condy,
                    CondyStaticRead::StaticFinal,
                ),
                None => Ok(None),
            }
        }
        _ => Ok(None),
    }
}

/// Which `ConstantBootstraps` read [`condy_public_static_reference`] answers
/// for, and so which field the real bootstrap would accept.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CondyStaticRead {
    /// `getStaticFinal`: `Lookup.findStaticGetter`, then "not a final field"
    /// (`IncompatibleClassChangeError`) unless the field is `final`.
    StaticFinal,
    /// `enumConstant`: `Enum.valueOf`, which answers only an enum class's own
    /// enum constants ("X is not an enum class" / "No enum constant X.N").
    EnumConstant,
}

/// `MethodType.toString`'s spelling of a method descriptor: simple names,
/// `(Lookup,String,Class,Class)Object` for
/// `(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;Ljava/lang/Class;)Ljava/lang/Object;`.
fn simple_method_type(desc: &str) -> String {
    fn simple(one: &str) -> String {
        let dims = one.bytes().take_while(|&b| b == b'[').count();
        let base = &one[dims..];
        let name = match base.as_bytes().first() {
            Some(b'L') => {
                let n = base
                    .strip_prefix('L')
                    .and_then(|b| b.strip_suffix(';'))
                    .unwrap_or(base);
                let n = n.rsplit('/').next().unwrap_or(n);
                n.rsplit('$').next().unwrap_or(n).to_owned()
            }
            Some(b'Z') => "boolean".to_owned(),
            Some(b'B') => "byte".to_owned(),
            Some(b'C') => "char".to_owned(),
            Some(b'S') => "short".to_owned(),
            Some(b'I') => "int".to_owned(),
            Some(b'J') => "long".to_owned(),
            Some(b'F') => "float".to_owned(),
            Some(b'D') => "double".to_owned(),
            Some(b'V') => "void".to_owned(),
            _ => base.to_owned(),
        };
        name + &"[]".repeat(dims)
    }
    let (params, ret) = desc
        .strip_prefix('(')
        .and_then(|s| s.split_once(')'))
        .unwrap_or(("", desc));
    let mut out = Vec::new();
    let bytes = params.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'L' {
            while i < bytes.len() && bytes[i] != b';' {
                i += 1;
            }
        }
        i += 1;
        out.push(simple(&params[start..i.min(params.len())]));
    }
    format!("({}){}", out.join(","), simple(ret))
}

/// `java/lang/X` for the field descriptor `Ljava/lang/X;`, `None` for any
/// other shape.
fn object_type_name(descriptor: &str) -> Option<&str> {
    descriptor
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .filter(|s| !s.is_empty())
}

/// `owner.<condy.name>` when it is a `public static` field whose descriptor is
/// `condy.descriptor` and whose value is a non-null reference, and `read`'s
/// real bootstrap would provably answer that same value; `None` otherwise,
/// which hands the entry to the real bootstrap. Resolution and initialization
/// failures propagate — they are the same failures the real bootstrap would
/// see first.
///
/// "Provably" (interpreter round i1 wave 29, lane L4): the field must be
/// `final` for `getStaticFinal` (else `IncompatibleClassChangeError: not a
/// final field`), an `ACC_ENUM` constant of an enum class for `enumConstant`
/// (else `IllegalArgumentException` from `Enum.valueOf`), and the owner class
/// must be accessible to the class holding the entry without a module
/// question (the class itself, its own package, a public class of its own
/// loader, or a public `java/` class) -- else the JDK's `IllegalAccessError`.
/// The screen runs BEFORE the owner's initialization: the real bootstraps
/// refuse these shapes without running `<clinit>`.
fn condy_public_static_reference(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_class_id: ClassId,
    owner: &str,
    condy: &CondyDecoded,
    read: CondyStaticRead,
) -> Result<Option<Value>, MethodCallFailed> {
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags};
    let owner_id = resolve_class_loader_aware(shared, thread, frame_class_id, owner)
        .map_err(|e| convert_class_not_found(shared, thread, owner, e))?;
    let static_index = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.get_class(owner_id) else {
            return Ok(None);
        };
        fn package(name: &str) -> &str {
            name.rfind('/').map_or("", |i| &name[..i])
        }
        let accessible = owner_id == frame_class_id
            || cm.get_class(frame_class_id).is_some_and(|frame_class| {
                let same_loader = frame_class.loader_id == class.loader_id;
                (same_loader && package(&*frame_class.name) == package(&*class.name))
                    || (class.access_flags.contains(ClassAccessFlags::PUBLIC)
                        && (same_loader || class.name.starts_with("java/")))
            });
        let owner_admits = match read {
            CondyStaticRead::StaticFinal => true,
            CondyStaticRead::EnumConstant => {
                class.access_flags.contains(ClassAccessFlags::ENUM)
                    && class.superclass.is_some_and(|s| {
                        cm.get_class(s)
                            .is_some_and(|sc| &*sc.name == "java/lang/Enum")
                    })
            }
        };
        if !accessible || !owner_admits {
            return Ok(None);
        }
        // The statics store is indexed among STATIC fields only (see the
        // `Boolean` arm of `op_getstatic`).
        let required = match read {
            CondyStaticRead::StaticFinal => FieldAccessFlags::PUBLIC | FieldAccessFlags::FINAL,
            CondyStaticRead::EnumConstant => {
                FieldAccessFlags::PUBLIC | FieldAccessFlags::FINAL | FieldAccessFlags::ENUM
            }
        };
        let mut index = 0usize;
        let mut found = None;
        let mut not_final = false;
        for f in class.fields.iter().filter(|f| f.is_static()) {
            if *f.name == *condy.name && *f.descriptor == *condy.descriptor {
                if f.access_flags.contains(required) {
                    found = Some(index);
                } else if read == CondyStaticRead::StaticFinal
                    && f.access_flags.contains(FieldAccessFlags::PUBLIC)
                {
                    not_final = true;
                }
                break;
            }
            index += 1;
        }
        // `getStaticFinal` of a public static field that is not `final`: the
        // JDK's `findStaticGetter` succeeds (the owner is accessible, checked
        // above) and `ConstantBootstraps.getStaticFinal` then throws
        // `IncompatibleClassChangeError("not a final field: " + name)` without
        // initializing the owner. Answered here because the real bootstrap
        // reads `mh.internalMemberName()`, which this VM's getter handles do
        // not carry (interpreter round i1 wave 29 host run:
        // `L4/L4W29CondyStaticReads` `nonFinal*` rows).
        if not_final {
            return Err(MethodCallFailed::InternalError(VmError::Linkage(
                crate::error::LinkageError::IncompatibleClassChangeError {
                    message: format!("not a final field: {}", condy.name),
                },
            )));
        }
        found
    };
    let Some(index) = static_index else {
        return Ok(None);
    };
    ensure_class_initialized_shared(shared, thread, owner_id)?;
    Ok(Some(get_static_shared(shared, owner_id, index))
        .filter(|v| matches!(v, Value::Object(Some(_)))))
}

/// Nesting bound for condy resolution on one thread. A `CONSTANT_Dynamic`
/// whose static arguments (transitively) name itself is legal to write and
/// must fail — HotSpot raises `StackOverflowError` — rather than recurse in
/// Rust until the native stack is gone.
const CONDY_MAX_NESTING: u32 = 64;

thread_local! {
    static CONDY_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Decrements [`CONDY_DEPTH`] on every exit path of [`invoke_condy_bootstrap`].
struct CondyDepthGuard;

impl Drop for CondyDepthGuard {
    fn drop(&mut self) {
        CONDY_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Run a `CONSTANT_Dynamic`'s bootstrap method (JVMS §5.4.3.6) and convert its
/// result to the entry's field type.
///
/// The steps, in HotSpot's order: resolve the bootstrap method's owner, then
/// the field type (a failure of either is the plain `NoClassDefFoundError`,
/// which the caller records), then build `(Lookup, String name, Class type,
/// static args...)`, pack a trailing `Object...` parameter, invoke, and check
/// the result against the type (`BootstrapMethodInvoker.invoke`'s
/// `resultType.cast`, or the unboxing funnel for a primitive type).
///
/// Shares `invokedynamic`'s calling convention (`runtime/invokedynamic.rs`,
/// `bootstrap_generic`): the `Lookup` comes from `MethodHandles.lookup()`,
/// which is caller-sensitive and sees the method executing the `ldc` as the
/// innermost Java frame. The wrapping helpers below are a second copy of that
/// file's `wrap_bootstrap_failure` / `bootstrap_method_error` because those
/// are private to a file another lane owns this round; fold them together when
/// they are exposed.
///
/// `fill_as_of` is the caller's `ResolutionCache::fill_snapshot`, taken before
/// `decode_condy` read the pool: the static arguments' own constant records
/// (`MethodType`, `MethodHandle`) are made under it (i22-L5, wave 24).
fn invoke_condy_bootstrap(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_class_id: ClassId,
    condy: &CondyDecoded,
    fill_as_of: u64,
) -> Result<Value, MethodCallFailed> {
    let depth = CONDY_DEPTH.with(|d| {
        let v = d.get().saturating_add(1);
        d.set(v);
        v
    });
    let _guard = CondyDepthGuard;
    if depth > CONDY_MAX_NESTING {
        return Err(
            match crate::runtime::exceptions::create_exception_object(
                shared,
                thread,
                "java/lang/StackOverflowError",
                Some("CONSTANT_Dynamic resolution nested too deeply (cyclic bootstrap arguments?)"),
            ) {
                Ok(obj) => MethodCallFailed::ExceptionThrown(obj),
                Err(e) => e,
            },
        );
    }
    // Every reference kind reaches the bootstrap's own resolution and
    // `makeConstant`'s type check (interpreter round i5 wave 101, lane L4;
    // `i101-L4-condy-bootstrap-of-a-non-static-kind`): a static method and a
    // constructor are invoked, every other kind is refused there.
    let pin_base = thread.native_pin_roots.len();
    let outcome = invoke_condy_bootstrap_pinned(shared, thread, frame_class_id, condy, fill_as_of);
    thread.native_pin_roots.truncate(pin_base);
    outcome
}

/// Pin `v` if it is a reference; the slot to re-read it from afterwards.
fn condy_pin(thread: &mut JvmThread, v: Value) -> Option<usize> {
    match v {
        Value::Object(Some(o)) => {
            let slot = thread.native_pin_roots.len();
            thread.native_pin_roots.push(o);
            Some(slot)
        }
        _ => None,
    }
}

/// Re-read every pinned argument from its (possibly forwarded) slot.
fn condy_refresh(thread: &JvmThread, args: &mut [Value], slots: &[Option<usize>]) {
    for (v, slot) in args.iter_mut().zip(slots) {
        if let Some(o) = slot.and_then(|s| thread.native_pin_roots.get(s).copied()) {
            *v = Value::Object(Some(o));
        }
    }
}

/// Box a primitive the way `MethodHandle.invoke` does for a reference-typed
/// parameter or an `Object...` element; references pass through.
fn condy_box(
    shared: &SharedVm,
    thread: &mut JvmThread,
    v: Value,
) -> Result<Value, MethodCallFailed> {
    let (class, desc) = match v {
        Value::Int(_) => ("java/lang/Integer", "(I)Ljava/lang/Integer;"),
        Value::Long(_) => ("java/lang/Long", "(J)Ljava/lang/Long;"),
        Value::Float(_) => ("java/lang/Float", "(F)Ljava/lang/Float;"),
        Value::Double(_) => ("java/lang/Double", "(D)Ljava/lang/Double;"),
        other => return Ok(other),
    };
    Ok(invoke_shared(shared, thread, class, "valueOf", desc, &[v])?.unwrap_or(Value::Object(None)))
}

/// gci i33/sc kill switch, `CRATONVM_GC_CONDY_STRING_ARG_RECORD` (default ON;
/// `=0` restores the pre-i33 unrecorded condy String arguments: an ordinary
/// String held by the pool alone, a lone-surrogate one by a STRONG pool row).
///
/// `pub(crate)` with [`condy_string_arg`] / [`condy_wide_string_arg`] for
/// `invokedynamic`'s generic bootstrap, whose String static arguments take the
/// same two helpers under the same switch (gci i33/sc, page
/// `gci-i33sc-indy-string-static-args-unrecorded-and-lossy-FIXED-20261016`).
pub(crate) fn condy_string_arg_record_on() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GC_CONDY_STRING_ARG_RECORD")
}

/// A condy's ordinary `CONSTANT_String` static argument at `cp_index`.
///
/// gci i33/sc (`i32-sl-report.md` residual, review item 1): with `record` the
/// argument resolves on `ldc`'s terms for that entry -- the recorded value if
/// there is one, else the interned literal, recorded by `ldc`'s own rule
/// (the cache on: [`store_recorded_cp_constant_as_of`], which under the weak
/// String table is the never-evicted store; the cache off: only when
/// [`weak_table_needs_the_record`]). Before, nothing recorded the argument's
/// entry, so under `CRATONVM_GC_WEAK_STRING_TABLE` a String used only as a
/// condy argument was held by the weak pool alone and could die (and be
/// re-created with a new identity) while its class lived; HotSpot resolves
/// the argument into the class's resolved references. Counter-free (as
/// `invokedynamic`'s static arguments): `LDC_FILL` counts `ldc`'s fills.
/// The record is made before anything allocates again, so the String the
/// pool answered cannot be collected between the two. Without `record`, the
/// pre-i33 interned String alone.
pub(crate) fn condy_string_arg(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    cp_index: u16,
    text: &str,
    as_of: u64,
    record: bool,
) -> Result<Value, MethodCallFailed> {
    if record {
        if let Some(v) = cached_cp_constant(shared, class_id, cp_index) {
            return Ok(v);
        }
    }
    let v = Value::Object(Some(intern_string_literal_or_oom(shared, thread, text)?));
    if record
        && (ldc_const_cache_enabled(shared)
            || weak_table_needs_the_record(shared, class_id, cp_index, &v))
    {
        store_recorded_cp_constant_as_of(shared, class_id, cp_index, v, as_of);
    }
    Ok(v)
}

/// A condy's lone-surrogate `CONSTANT_String` static argument at `cp_index`.
///
/// gci i33/sc (the residual of gci i32/sl, finding 10 of
/// `gci-i20mr-merge-review-findings-FIXED-20261016`): with `record` and
/// [`surrogate_literal_class_held`] the argument takes `ldc`'s class-held
/// form -- the recorded value if there is one, else a WEAK pool row and the
/// literal recorded in its class's never-evicted constant record
/// ([`record_wide_string_literal_as_of`], whose first-recorded value is the
/// answer) -- so it dies with its last class instead of keeping a strong row
/// for the VM's life. Nothing allocates between the weak claim and the
/// record. Otherwise the strong-row form
/// ([`intern_wide_string_literal_or_oom`]), unrecorded, as before.
pub(crate) fn condy_wide_string_arg(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    cp_index: u16,
    units: &[u16],
    as_of: u64,
    record: bool,
) -> Result<Value, MethodCallFailed> {
    if !(record && surrogate_literal_class_held()) {
        return Ok(Value::Object(Some(intern_wide_string_literal_or_oom(
            shared, thread, units,
        )?)));
    }
    if let Some(v) = cached_cp_constant(shared, class_id, cp_index) {
        return Ok(v);
    }
    let obj = intern_wide_string_literal_or_oom_as(shared, thread, units, true)?;
    Ok(record_wide_string_literal_as_of(
        shared,
        class_id,
        cp_index,
        Value::Object(Some(obj)),
        as_of,
        true,
    ))
}

/// The body of [`invoke_condy_bootstrap`]. Every reference it produces is
/// pinned in `native_pin_roots` as it is produced; the caller truncates the
/// pins on every exit.
fn invoke_condy_bootstrap_pinned(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_class_id: ClassId,
    condy: &CondyDecoded,
    fill_as_of: u64,
) -> Result<Value, MethodCallFailed> {
    // The bootstrap method handle's owner, resolved the way the constant
    // pool's own `Methodref` class would be.
    let owner = resolve_class_loader_aware(shared, thread, frame_class_id, &condy.bsm_class)
        .map_err(|e| convert_class_not_found(shared, thread, &condy.bsm_class, e))?;
    // A bootstrap of any other kind than `REF_invokeStatic` is resolved as
    // `ldc` of its `CONSTANT_MethodHandle` is (a missing member is its own
    // error, first, as HotSpot resolves the bootstrap handle before the static
    // arguments), and kept pinned: a constructor bootstrap is invoked through
    // it below. Interpreter round i5 wave 101, lane L4.
    let bsm_handle_slot = if condy.bsm_kind == MethodHandleKind::InvokeStatic {
        None
    } else {
        let mh = resolve_method_handle_constant(
            shared,
            thread,
            frame_class_id,
            condy.bsm_kind,
            &condy.bsm_class,
            &condy.bsm_method,
            &condy.bsm_desc,
        )?;
        condy_pin(thread, Value::Object(Some(mh)))
    };

    // The field type. JVMS §5.4.3.6 resolves it before the bootstrap runs.
    let descriptor = condy.descriptor.as_str();
    let type_tag = descriptor.as_bytes().first().copied().unwrap_or(b'L');
    let (type_id, type_name) = match type_tag {
        b'L' | b'[' => {
            let name = if type_tag == b'L' {
                object_type_name(descriptor).unwrap_or(descriptor)
            } else {
                descriptor
            };
            let id = resolve_class_loader_aware(shared, thread, frame_class_id, name)
                .map_err(|e| convert_class_not_found(shared, thread, name, e))?;
            (Some(id), name)
        }
        _ => (None, descriptor),
    };
    // Every allocation in this function follows `new`'s contract (collect,
    // retry, `OutOfMemoryError`) -- the infallible mirror and Strings aborted
    // the process when the first resolution met a full heap (gc-common w9-c,
    // `common-w8v-constant-resolution-strings-abort-on-a-full-heap`). A
    // collection moves objects, so `args` holds raw copies that go stale:
    // every reference is pinned the moment it exists and `args` is rebuilt
    // from the pins (`condy_refresh`) before anything reads it.
    let type_mirror = match type_id {
        Some(id) => class_mirror_or_oom(shared, thread, id, "condy-type")?,
        // At most nine, normally made at start-up (see
        // `class_mirror_for_field_descriptor`).
        None => primitive_mirror_or_oom(shared, thread, descriptor, "condy-type")?,
    };

    let mut args: Vec<Value> = Vec::with_capacity(3 + condy.args.len());
    let mut slots: Vec<Option<usize>> = Vec::with_capacity(3 + condy.args.len());
    let type_slot = condy_pin(thread, Value::Object(Some(type_mirror)));

    // 1. Lookup — caller-sensitive, see above.
    let lookup = invoke_shared(
        shared,
        thread,
        "java/lang/invoke/MethodHandles",
        "lookup",
        "()Ljava/lang/invoke/MethodHandles$Lookup;",
        &[],
    )?
    .unwrap_or(Value::Object(None));
    slots.push(condy_pin(thread, lookup));
    args.push(lookup);
    // 2. The entry's name (JDK: `nameObj.toString().intern()`; this pool
    //    interns on content).
    let name_ref = Value::Object(Some(intern_string_literal_or_oom(
        shared,
        thread,
        &condy.name,
    )?));
    slots.push(condy_pin(thread, name_ref));
    args.push(name_ref);
    // 3. The field type's mirror, pinned since before the lookup call (the
    //    raw copy pushed here is replaced from `type_slot` by the refresh
    //    after the static arguments).
    args.push(Value::Object(Some(type_mirror)));
    slots.push(type_slot);

    // 4. Static arguments, left to right.
    // gci i33/sc: String arguments are recorded at their own entry (read once
    // per bootstrap; `=0` is the pre-i33 unrecorded form).
    let record_strings = condy_string_arg_record_on();
    for arg in &condy.args {
        let v = match arg {
            CondyArg::Int(v) => Value::Int(*v),
            CondyArg::Long(v) => Value::Long(*v),
            CondyArg::Float(v) => Value::Float(*v),
            CondyArg::Double(v) => Value::Double(*v),
            CondyArg::Str { cp_index, text } => condy_string_arg(
                shared,
                thread,
                frame_class_id,
                *cp_index,
                text,
                fill_as_of,
                record_strings,
            )?,
            CondyArg::WideStr { cp_index, units } => condy_wide_string_arg(
                shared,
                thread,
                frame_class_id,
                *cp_index,
                units,
                fill_as_of,
                record_strings,
            )?,
            CondyArg::Class(name) => {
                let id = resolve_class_loader_aware(shared, thread, frame_class_id, name)
                    .map_err(|e| convert_class_not_found(shared, thread, name, e))?;
                Value::Object(Some(class_mirror_or_oom(
                    shared,
                    thread,
                    id,
                    "condy-arg-class",
                )?))
            }
            CondyArg::MethodType {
                cp_index,
                descriptor,
            } => match cached_cp_constant(shared, frame_class_id, *cp_index) {
                Some(v) => v,
                None => {
                    let mt =
                        resolve_method_type_constant(shared, thread, frame_class_id, descriptor)?;
                    record_method_type_constant_as_of(
                        shared,
                        frame_class_id,
                        *cp_index,
                        Value::Object(Some(mt)),
                        fill_as_of,
                    )
                }
            },
            CondyArg::MethodHandle {
                cp_index,
                kind,
                class_name,
                member_name,
                descriptor,
            } => match cached_cp_constant(shared, frame_class_id, *cp_index) {
                Some(v) => v,
                None => {
                    let mh = resolve_method_handle_constant(
                        shared,
                        thread,
                        frame_class_id,
                        *kind,
                        class_name,
                        member_name,
                        descriptor,
                    )?;
                    record_cp_constant_permanent_as_of(
                        shared,
                        frame_class_id,
                        *cp_index,
                        Value::Object(Some(mh)),
                        fill_as_of,
                    )
                }
            },
            CondyArg::Dynamic(idx) => resolve_condy_constant(shared, thread, frame_class_id, *idx)?,
        };
        slots.push(condy_pin(thread, v));
        args.push(v);
    }
    // Rebuild every reference from its pin: any argument above may have
    // collected, which leaves the earlier raw copies in `args` stale.
    condy_refresh(thread, &mut args, &slots);

    // 5. Fit the argument list to the bootstrap's declared parameters.
    let (params, _) = split_method_descriptor_ref(&condy.bsm_desc);
    // `ConstantBootstraps.makeConstant`, the JDK's entry for every dynamic
    // constant, refuses a bootstrap whose type has fewer than two parameters
    // or whose first is not `MethodHandles.Lookup` (a final class, so only the
    // exact name passes `Lookup.class.isAssignableFrom`), after the static
    // arguments resolved and before anything is invoked or initialized: a
    // `BootstrapMethodError` with no cause, unwrapped. This linked an
    // `(Object,String,Class)Object` bootstrap and collected everything into
    // an `(Object...)Object` one. Interpreter round i5 wave 101, lane L4
    // (probe `tools/probes/interp/L4/L4W101CondyBootstrapNotLookup.java`).
    // The check reads the HANDLE's type (wave 101): an instance kind's
    // receiver leads it, a field kind's is its getter / setter shape, a
    // constructor's returns its class (`condy_bootstrap_handle_type`).
    let handle_type = condy_bootstrap_handle_type(condy);
    let (handle_params, _) = split_method_descriptor_ref(&handle_type);
    if handle_params.len() < 2 || handle_params[0] != "Ljava/lang/invoke/MethodHandles$Lookup;" {
        return Err(condy_bootstrap_error(
            shared,
            thread,
            None,
            &format!(
                "Invalid bootstrap method declared for resolving a dynamic constant: MethodHandle{}",
                simple_method_type(&handle_type)
            ),
        ));
    }
    if !matches!(
        condy.bsm_kind,
        MethodHandleKind::InvokeStatic | MethodHandleKind::NewInvokeSpecial
    ) {
        // An instance method of `Lookup` itself, the one non-static shape the
        // check admits: not modelled (no class file in the probes has one).
        return Err(condy_bootstrap_error(
            shared,
            thread,
            None,
            &format!(
                "CONSTANT_Dynamic bootstrap {}.{}{} has reference kind {}; only a static \
                 or constructor bootstrap method is supported",
                condy.bsm_class, condy.bsm_method, condy.bsm_desc, condy.bsm_kind
            ),
        ));
    }
    if params.last() == Some(&"[Ljava/lang/Object;") {
        // `Object...` collects everything from its position on, unless the
        // arguments already end in exactly one array there (the
        // `invokeWithArguments` rule for a varargs collector).
        let fixed = params.len() - 1;
        condy_refresh(thread, &mut args, &slots);
        let already_array = args.len() == params.len()
            && match args[fixed] {
                Value::Object(Some(o)) => {
                    shared.mem.heap.kind_of(o) == cratonvm_types::ObjectKind::Array
                }
                Value::Object(None) => true,
                _ => false,
            };
        if args.len() >= fixed && !already_array {
            let mut tail = args.split_off(fixed);
            let mut tail_slots = slots.split_off(fixed);
            for i in 0..tail.len() {
                if !matches!(tail[i], Value::Object(_)) {
                    let boxed = condy_box(shared, thread, tail[i])?;
                    tail_slots[i] = condy_pin(thread, boxed);
                    tail[i] = boxed;
                }
            }
            // Collecting and fallible (gc-common w4-c; see
            // `interpreter::native_alloc_collecting`). Every reference in
            // `args` / `tail` is pinned and re-read by `condy_refresh` below
            // and before the invoke, so a collection here is safe.
            let tail_len = tail.len();
            let arr = super::native_alloc_collecting(
                shared,
                thread,
                "condy-varargs",
                |shared, thread| {
                    let mut ctx = crate::vm::NativeContextImpl { shared, thread };
                    ctx.new_array(cratonvm_types::ArrayElementType::Reference, tail_len)
                },
            )?;
            let arr_slot = condy_pin(thread, Value::Object(Some(arr)));
            condy_refresh(thread, &mut tail, &tail_slots);
            let arr = arr_slot
                .and_then(|s| thread.native_pin_roots.get(s).copied())
                .unwrap_or(arr);
            {
                let ctx = crate::vm::NativeContextImpl {
                    shared,
                    thread: &mut *thread,
                };
                for (i, v) in tail.into_iter().enumerate() {
                    ctx.set_array_element(arr, i, v);
                }
            }
            args.push(Value::Object(Some(arr)));
            slots.push(arr_slot);
        }
    }
    if args.len() != params.len() {
        return Err(condy_bootstrap_error_with_new_cause(
            shared,
            thread,
            "java/lang/invoke/WrongMethodTypeException",
            // `MethodHandle.asType`'s wording, as `BootstrapMethodInvoker`
            // reaches it: the handle's type against the call's
            // `(Lookup, String, Class, Object...)Object` (interpreter round
            // i1 wave 29 host run, `L4/L4W29CondyStaticReads`).
            &format!(
                "cannot convert MethodHandle{} to ({}){}",
                simple_method_type(&condy.bsm_desc),
                ["Lookup", "String", "Class"]
                    .into_iter()
                    .chain(std::iter::repeat("Object").take(args.len().saturating_sub(3)))
                    .collect::<Vec<_>>()
                    .join(","),
                "Object"
            ),
        ));
    }
    // Per-parameter conversion: box for a reference parameter, widen a
    // primitive (`MethodHandle.asType` rules for the cases a constant pool
    // can produce).
    for i in 0..args.len() {
        let tag = params[i].as_bytes().first().copied().unwrap_or(b'L');
        let converted = match (tag, args[i]) {
            (
                b'L' | b'[',
                v @ (Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_)),
            ) => {
                let boxed = condy_box(shared, thread, v)?;
                slots[i] = condy_pin(thread, boxed);
                boxed
            }
            (b'J', Value::Int(x)) => Value::Long(i64::from(x)),
            // Cast: JLS §5.1.2 widening int -> float (may round, as Java does)
            (b'F', Value::Int(x)) => Value::Float(x as f32),
            (b'D', Value::Int(x)) => Value::Double(f64::from(x)),
            // Cast: JLS §5.1.2 widening long -> double (may round, as Java does)
            (b'D', Value::Long(x)) => Value::Double(x as f64),
            (b'D', Value::Float(x)) => Value::Double(f64::from(x)),
            (_, v) => v,
        };
        args[i] = converted;
    }

    // 6. Invoke. JVMS §5.4.3.6 / §5.5: invoking the static bootstrap
    //    initializes the class that declares it; an error from that
    //    propagates unwrapped, like any `Error` the bootstrap itself throws.
    // A constructor bootstrap (wave 101): `BootstrapMethodInvoker` invokes its
    // handle, which initializes the class and answers the new instance; here
    // through the resolved handle's `invokeWithArguments` (every argument
    // boxed into one `Object[]`; an exception the constructor throws comes out
    // as itself and is wrapped below as a static bootstrap's is).
    if let Some(mh_slot) = bsm_handle_slot {
        for i in 0..args.len() {
            if !matches!(args[i], Value::Object(_)) {
                let boxed = condy_box(shared, thread, args[i])?;
                slots[i] = condy_pin(thread, boxed);
                args[i] = boxed;
            }
        }
        let len = args.len();
        let arr = super::native_alloc_collecting(
            shared,
            thread,
            "condy-ctor-args",
            |shared, thread| {
                let mut ctx = crate::vm::NativeContextImpl { shared, thread };
                ctx.new_array(cratonvm_types::ArrayElementType::Reference, len)
            },
        )?;
        let arr_slot = condy_pin(thread, Value::Object(Some(arr)));
        condy_refresh(thread, &mut args, &slots);
        let arr = arr_slot
            .and_then(|s| thread.native_pin_roots.get(s).copied())
            .unwrap_or(arr);
        {
            let ctx = crate::vm::NativeContextImpl {
                shared,
                thread: &mut *thread,
            };
            for (i, v) in args.iter().enumerate() {
                ctx.set_array_element(arr, i, *v);
            }
        }
        let Some(mh) = thread.native_pin_roots.get(mh_slot).copied() else {
            return Err(condy_bootstrap_error(
                shared,
                thread,
                None,
                "CONSTANT_Dynamic constructor bootstrap handle lost",
            ));
        };
        let raw = match invoke_shared(
            shared,
            thread,
            "java/lang/invoke/MethodHandle",
            "invokeWithArguments",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(mh)), Value::Object(Some(arr))],
        ) {
            Ok(v) => v.unwrap_or(Value::Object(None)),
            Err(e) => return Err(condy_wrap_bootstrap_failure(shared, thread, e)),
        };
        return condy_convert_result(shared, thread, type_tag, type_id, type_name, raw);
    }
    initialize_bootstrap_declaring_class(
        shared,
        thread,
        owner,
        &condy.bsm_method,
        &condy.bsm_desc,
    )?;
    condy_refresh(thread, &mut args, &slots);
    let raw = match crate::vm::invoke_static_shared_on_class(
        shared,
        thread,
        owner,
        &condy.bsm_class,
        &condy.bsm_method,
        &condy.bsm_desc,
        &args,
    ) {
        Ok(v) => v.unwrap_or(Value::Object(None)),
        Err(e) => return Err(condy_wrap_bootstrap_failure(shared, thread, e)),
    };

    // 7. Convert to the field type. `BootstrapMethodInvoker.invoke` calls the
    //    bootstrap through `invoke` with an `Object` return, so a primitive
    //    answer is boxed BY THE BOOTSTRAP'S DECLARED RETURN TYPE first
    //    (`short` -> `Short`, `boolean` -> `Boolean`) and only then cast or
    //    funnelled to the field type. Converting the raw primitive straight to
    //    the field type pushed an `int` bootstrap's answer as a `short`
    //    constant (HotSpot: `ClassCastException: Cannot cast java.lang.Integer
    //    to java.lang.Short`) and a `long` one as an `int` constant, a
    //    category-2 value in a category-1 slot (interpreter round i4 wave 98,
    //    lane L7; probe `tools/probes/interp/L7/L7W98CondyPrimitiveBootstrap.java`).
    let raw = match raw {
        Value::Object(_) => raw,
        prim => {
            let ret = condy.bsm_desc.rsplit(')').next().unwrap_or("");
            // The conversion below reads the box before it allocates anything
            // (its `ClassCastException` names the box's class first), so the
            // fresh box needs no pin.
            condy_box_declared(shared, thread, prim, ret.as_bytes().first().copied())?
        }
    };
    condy_convert_result(shared, thread, type_tag, type_id, type_name, raw)
}

/// The `type()` of the direct method handle a condy's bootstrap
/// `CONSTANT_MethodHandle` resolves to, as a descriptor (`DirectMethodHandle`'s
/// rules): a static method's own descriptor; an instance method's with the
/// owner leading; a constructor's parameters returning the owner; a field
/// getter `(owner)T` / `()T`, setter `(owner,T)V` / `(T)V`. What
/// `ConstantBootstraps.makeConstant` checks and what its refusal prints.
/// Interpreter round i5 wave 101, lane L4.
fn condy_bootstrap_handle_type(condy: &CondyDecoded) -> String {
    let owner = if condy.bsm_class.starts_with('[') {
        condy.bsm_class.clone()
    } else {
        format!("L{};", condy.bsm_class)
    };
    let desc = condy.bsm_desc.as_str();
    let params_of = |d: &str| {
        d.strip_prefix('(')
            .and_then(|s| s.split_once(')'))
            .map(|(p, r)| (p.to_string(), r.to_string()))
            .unwrap_or_default()
    };
    match condy.bsm_kind {
        MethodHandleKind::InvokeStatic => desc.to_string(),
        MethodHandleKind::InvokeVirtual
        | MethodHandleKind::InvokeInterface
        | MethodHandleKind::InvokeSpecial => {
            let (p, r) = params_of(desc);
            format!("({owner}{p}){r}")
        }
        MethodHandleKind::NewInvokeSpecial => {
            let (p, _) = params_of(desc);
            format!("({p}){owner}")
        }
        MethodHandleKind::GetField => format!("({owner}){desc}"),
        MethodHandleKind::GetStatic => format!("(){desc}"),
        MethodHandleKind::PutField => format!("({owner}{desc})V"),
        MethodHandleKind::PutStatic => format!("({desc})V"),
    }
}

/// Box a bootstrap's primitive answer as `MethodHandle.invoke` does for an
/// `Object` return: by the bootstrap's declared return type `ret` (`I`, `S`,
/// `B`, `C`, `Z`, `J`, `F`, `D`). Anything else falls back to [`condy_box`]'s
/// boxing by the value's own kind.
fn condy_box_declared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    v: Value,
    ret: Option<u8>,
) -> Result<Value, MethodCallFailed> {
    let (class, desc) = match (ret, v) {
        (Some(b'S'), Value::Int(_)) => ("java/lang/Short", "(S)Ljava/lang/Short;"),
        (Some(b'B'), Value::Int(_)) => ("java/lang/Byte", "(B)Ljava/lang/Byte;"),
        (Some(b'C'), Value::Int(_)) => ("java/lang/Character", "(C)Ljava/lang/Character;"),
        (Some(b'Z'), Value::Int(_)) => ("java/lang/Boolean", "(Z)Ljava/lang/Boolean;"),
        _ => return condy_box(shared, thread, v),
    };
    Ok(invoke_shared(shared, thread, class, "valueOf", desc, &[v])?.unwrap_or(Value::Object(None)))
}

/// Initialize the class a `REF_invokeStatic` bootstrap call initializes
/// (JVMS §5.5): the class that DECLARES the resolved method
/// (`invokestatic_init_class`, the rule `invokestatic` applies), never the
/// named class `owner` of an inherited bootstrap. `C.bsm` with `bsm` declared
/// in `P` runs `P.<clinit>` only, as HotSpot's method handle for it does.
/// Until interpreter round i1 wave 15 (lane L4) the condy route initialized
/// `owner`; the indy route now takes `crate::vm::invoke_static_shared`, which
/// applies the same rule.
fn initialize_bootstrap_declaring_class(
    shared: &SharedVm,
    thread: &mut JvmThread,
    owner: ClassId,
    bsm_method: &str,
    bsm_desc: &str,
) -> Result<(), MethodCallFailed> {
    let declaring =
        crate::runtime::interpreter::invokestatic_init_class(shared, owner, bsm_method, bsm_desc);
    ensure_class_initialized_shared(shared, thread, declaring)
}

/// `BootstrapMethodInvoker.invoke`'s result check for a condy: `resultType.cast`
/// for a reference type, the unboxing funnel for a primitive one. A failure is
/// the `ClassCastException` / `NullPointerException` wrapped in
/// `BootstrapMethodError`, as the JDK wraps it.
fn condy_convert_result(
    shared: &SharedVm,
    thread: &mut JvmThread,
    type_tag: u8,
    type_id: Option<ClassId>,
    type_name: &str,
    raw: Value,
) -> Result<Value, MethodCallFailed> {
    match (type_tag, type_id) {
        (b'L' | b'[', Some(target)) => {
            let obj = match raw {
                Value::Object(None) => return Ok(Value::Object(None)),
                Value::Object(Some(o)) => o,
                // A bootstrap declared with a primitive return: `invoke`
                // boxes it before the cast.
                prim => match condy_box(shared, thread, prim)? {
                    Value::Object(Some(o)) => o,
                    _ => return Ok(Value::Object(None)),
                },
            };
            if condy_result_assignable(shared, obj, target, type_name) {
                return Ok(Value::Object(Some(obj)));
            }
            let actual = condy_class_name(shared, obj);
            Err(condy_bootstrap_error_with_new_cause(
                shared,
                thread,
                "java/lang/ClassCastException",
                &format!(
                    "Cannot cast {} to {}",
                    crate::runtime::exceptions::hotspot_external_name(&actual),
                    crate::runtime::exceptions::hotspot_external_name(&type_name)
                ),
            ))
        }
        _ => {
            let wrapper = match type_tag {
                b'J' => "java/lang/Long",
                b'I' => "java/lang/Integer",
                b'B' => "java/lang/Byte",
                b'S' => "java/lang/Short",
                b'C' => "java/lang/Character",
                b'Z' => "java/lang/Boolean",
                b'F' => "java/lang/Float",
                _ => "java/lang/Double",
            };
            match raw {
                // The JDK unboxes through `ValueConversions.primitiveConversion`
                // and a `Number` accessor; a null answer is that call's helpful
                // NPE (measured on HotSpot 25, `L4W43CondyResultConversion`
                // rows `null-as-*`; `char` and `boolean` unbox through
                // `intValue`). Interpreter round i1 wave 43, lane L4: the text
                // was CratonVM's own.
                Value::Object(None) => {
                    let accessor = match type_tag {
                        b'J' => "longValue",
                        b'F' => "floatValue",
                        b'D' => "doubleValue",
                        b'S' => "shortValue",
                        b'B' => "byteValue",
                        _ => "intValue",
                    };
                    Err(condy_bootstrap_error_with_helpful_npe(
                        shared,
                        thread,
                        &format!(
                            "Cannot invoke \"java.lang.Number.{accessor}()\" because the return value \
                             of \"sun.invoke.util.ValueConversions.primitiveConversion(\
                             sun.invoke.util.Wrapper, Object, boolean)\" is null"
                        ),
                    ))
                }
                Value::Object(Some(o)) => {
                    let actual = condy_class_name(shared, o);
                    // A wrapper whose primitive WIDENS to the declared type
                    // (JLS 5.1.2) converts, as `primitiveConversion` does:
                    // `Short` and `Character` to `int`, `Integer` to `long`,
                    // `Float` to `double` (measured, wave 43, lane L4). It was
                    // the `ClassCastException` of any other wrapper.
                    if actual == wrapper || boxed_primitive_widens_to(&actual, type_tag) {
                        Ok(crate::vm::coerce_value_against_ret_char(
                            raw, type_tag, shared,
                        ))
                    } else {
                        Err(condy_bootstrap_error_with_new_cause(
                            shared,
                            thread,
                            "java/lang/ClassCastException",
                            &format!(
                                "Cannot cast {} to {}",
                                crate::runtime::exceptions::hotspot_external_name(&actual),
                                crate::runtime::exceptions::hotspot_external_name(&wrapper)
                            ),
                        ))
                    }
                }
                // A bootstrap declared with a primitive return type.
                prim => Ok(match (type_tag, prim) {
                    (b'J', Value::Int(x)) => Value::Long(i64::from(x)),
                    // Cast: JLS §5.1.2 widening int -> float
                    (b'F', Value::Int(x)) => Value::Float(x as f32),
                    (b'D', Value::Int(x)) => Value::Double(f64::from(x)),
                    // Cast: JLS §5.1.2 widening long -> double
                    (b'D', Value::Long(x)) => Value::Double(x as f64),
                    (b'D', Value::Float(x)) => Value::Double(f64::from(x)),
                    (_, v) => v,
                }),
            }
        }
    }
}

/// Is `obj` an instance of `target` for `Class.cast`? Arrays by descriptor;
/// everything else by hierarchy plus the same fail-open admissions the
/// `checkcast` opcode applies, so a bootstrap result the opcode would accept
/// is not refused here. The display-class admission is left out: it can load a
/// class, and nothing here needs it.
fn condy_result_assignable(
    shared: &SharedVm,
    obj: ObjectRef,
    target: ClassId,
    target_name: &str,
) -> bool {
    if let Some(src_desc) = array_descriptor_of(shared, obj) {
        return array_is_assignable_to(shared, &src_desc, target_name);
    }
    let cid = shared.mem.heap.class_id_of(obj);
    // Bound first so no read guard is held across the fallbacks, some of
    // which take the class-manager lock themselves (lock-free once `cid`'s
    // supers closure is published).
    let by_hierarchy = class_is_subtype(shared, cid, target);
    // The name rule is `--compatible`-only, as in `op_checkcast`.
    by_hierarchy
        || (!shared
            .classes
            .aligned(cratonvm_types::HotSpotAlignment::NamespacedClassTypes)
            && loader_aware_name_assignable(shared, cid, target, target_name))
        || lambda_proxy_satisfies(shared, cid, target)
        || synthetic_implements(shared, cid, target_name)
        || proxy_instance_satisfies_target(shared, obj, target_name)
        || annotation_proxy_satisfies_target(shared, obj, target_name)
}

/// Does the primitive of the wrapper class `wrapper` (internal name) widen to
/// the primitive `tag` (JLS 5.1.2; the identity excluded)? The same table as
/// `vm_exec::widen_boxed_primitive`, which converts the value
/// (`coerce_value_against_ret_char` calls it).
fn boxed_primitive_widens_to(wrapper: &str, tag: u8) -> bool {
    match wrapper {
        "java/lang/Byte" => matches!(tag, b'S' | b'I' | b'J' | b'F' | b'D'),
        "java/lang/Short" | "java/lang/Character" => matches!(tag, b'I' | b'J' | b'F' | b'D'),
        "java/lang/Integer" => matches!(tag, b'J' | b'F' | b'D'),
        "java/lang/Long" => matches!(tag, b'F' | b'D'),
        "java/lang/Float" => tag == b'D',
        _ => false,
    }
}

fn condy_class_name(shared: &SharedVm, obj: ObjectRef) -> String {
    let cid = shared.mem.heap.class_id_of(obj);
    shared
        .classes
        .class_manager
        .read()
        .get_class(cid)
        .map(|c| c.name.to_string())
        .unwrap_or_else(|| format!("?class_id={cid}"))
}

/// JVMS §5.4.3.6 / `BootstrapMethodInvoker.invoke`: an `Error` thrown by the
/// bootstrap (a `BootstrapMethodError` included) passes through; any other
/// throwable becomes the cause of a new `BootstrapMethodError`. A VM-internal
/// failure (no Java throwable — e.g. the bootstrap method could not be found)
/// becomes a `BootstrapMethodError` naming it, so Java code sees a catchable
/// linkage error rather than an uncatchable VM error.
#[cold]
fn condy_wrap_bootstrap_failure(
    shared: &SharedVm,
    thread: &mut JvmThread,
    failure: MethodCallFailed,
) -> MethodCallFailed {
    let cause = match failure {
        MethodCallFailed::ExceptionThrown(cause) => cause,
        MethodCallFailed::InternalError(e) => {
            return condy_bootstrap_error(
                shared,
                thread,
                None,
                &format!("CONSTANT_Dynamic bootstrap method failed: {e}"),
            )
        }
    };
    let is_error = {
        let cm = shared.classes.class_manager.read();
        let cause_cid = shared.mem.heap.class_id_of(cause);
        cm.get_loaded_class_id("java/lang/Error")
            .is_some_and(|error_cid| cm.is_subclass_of(cause_cid, error_cid))
    };
    if is_error {
        return MethodCallFailed::ExceptionThrown(cause);
    }
    condy_bootstrap_error(
        shared,
        thread,
        Some(cause),
        "bootstrap method initialization exception",
    )
}

/// A new `BootstrapMethodError(message)`, with `cause` attached when given.
/// Falls back to throwing `cause` itself (or the construction failure) when the
/// error object cannot be built.
#[cold]
fn condy_bootstrap_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cause: Option<ObjectRef>,
    message: &str,
) -> MethodCallFailed {
    // `cause` is pinned across the two Java calls below, both safepoints.
    let pin = thread.native_pin_roots.len();
    if let Some(c) = cause {
        thread.native_pin_roots.push(c);
    }
    let created = crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        "java/lang/BootstrapMethodError",
        Some(message),
    );
    let outcome = match created {
        Err(e) => match cause {
            Some(_) => MethodCallFailed::ExceptionThrown(thread.native_pin_roots[pin]),
            None => e,
        },
        Ok(bme) if cause.is_none() => MethodCallFailed::ExceptionThrown(bme),
        Ok(bme) => {
            let bme_slot = thread.native_pin_roots.len();
            thread.native_pin_roots.push(bme);
            let cause = thread.native_pin_roots[pin];
            // The `(String)` constructor leaves `cause == this`, which is the
            // state `initCause` accepts; a failure leaves a cause-less error of
            // the right class and message.
            let _ = invoke_shared(
                shared,
                thread,
                "java/lang/Throwable",
                "initCause",
                "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
                &[Value::Object(Some(bme)), Value::Object(Some(cause))],
            );
            MethodCallFailed::ExceptionThrown(thread.native_pin_roots[bme_slot])
        }
    };
    thread.native_pin_roots.truncate(pin);
    outcome
}

/// [`condy_bootstrap_error`] over a freshly created `cause_class(cause_message)`.
#[cold]
fn condy_bootstrap_error_with_new_cause(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cause_class: &str,
    cause_message: &str,
) -> MethodCallFailed {
    match crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        cause_class,
        Some(cause_message),
    ) {
        Ok(cause) => condy_bootstrap_error(
            shared,
            thread,
            Some(cause),
            "bootstrap method initialization exception",
        ),
        Err(e) => e,
    }
}

/// [`condy_bootstrap_error`] over a new HELPFUL `NullPointerException` whose
/// text is `text`. CratonVM keeps a helpful NPE's text in `detailMessage`
/// (every reader of the message sees it, as the interpreter's own null-deref
/// NPEs do); HotSpot keeps it out of `detailMessage` and computes it in
/// `getMessage()`. The difference shows where a DETAIL message is recorded:
/// the rethrow of a recorded resolution failure (JVMS §5.4.3) gives the new
/// cause the recorded cause's detail message, `null` on HotSpot. So the NPE
/// is marked as helpful the JDK's own way, `extendedMessage` holding the same
/// `String` as `detailMessage` and `extendedMessageState` 2 ("message
/// computed"), a state no NPE constructed with a message reaches, and
/// [`linkage_error_cause_record`] records no message for it
/// ([`throwable_is_vm_helpful_npe`]). Interpreter round i1 wave 44, lane L4
/// (`i43-L4-a-recorded-condy-npe-keeps-its-message`;
/// `tools/probes/interp/L4/L4W43CondyResultConversion.java`, rows
/// `null-as-*`). A class library without those fields (a synthetic JDK)
/// gets the unmarked NPE, as before.
///
/// Interpreter round i4 wave 99, lane L9: `create_exception_object` now builds
/// every implicit NPE whose text is a JEP 358 one HotSpot's way itself -- no
/// `detailMessage`, the text in `extendedMessage`, state 2
/// (`exceptions::store_npe_extended_message`) -- so this NPE has no detail
/// message to record and the copy that used to follow the build is gone.
/// [`throwable_is_vm_helpful_npe`] still recognises the wave-44 shape.
#[cold]
fn condy_bootstrap_error_with_helpful_npe(
    shared: &SharedVm,
    thread: &mut JvmThread,
    text: &str,
) -> MethodCallFailed {
    const NPE: &str = "java/lang/NullPointerException";
    let npe = match crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        NPE,
        Some(text),
    ) {
        Ok(npe) => npe,
        Err(e) => return e,
    };
    condy_bootstrap_error(
        shared,
        thread,
        Some(npe),
        "bootstrap method initialization exception",
    )
}

/// The `detailMessage`, `extendedMessage` and `extendedMessageState` slots of
/// `throwable` when it is a `java.lang.NullPointerException` of a class
/// library that has the JDK's helpful-NPE fields, else `None`.
fn helpful_npe_slots(shared: &SharedVm, throwable: ObjectRef) -> Option<(usize, usize, usize)> {
    let class_id = shared.mem.heap.class_id_of(throwable);
    {
        let cm = shared.classes.class_manager.read();
        if &*cm.get_class(class_id)?.name != "java/lang/NullPointerException" {
            return None;
        }
    }
    let slot =
        |name: &str| crate::vm::vm_exec::resolve_field_slot_by_name_cached(shared, class_id, name);
    Some((
        slot("detailMessage")?,
        slot("extendedMessage")?,
        slot("extendedMessageState")?,
    ))
}

/// Is `throwable` an NPE marked by [`condy_bootstrap_error_with_helpful_npe`]
/// (its `extendedMessage` the very `String` its `detailMessage` holds, state
/// 2)? A `NullPointerException(String)` leaves `extendedMessage` null, and a
/// JDK-computed helpful message leaves `detailMessage` null, so neither
/// matches.
fn throwable_is_vm_helpful_npe(shared: &SharedVm, throwable: ObjectRef) -> bool {
    let Some((detail_slot, extended_slot, state_slot)) = helpful_npe_slots(shared, throwable)
    else {
        return false;
    };
    let heap = &shared.mem.heap;
    match (
        heap.get_field(throwable, detail_slot),
        heap.get_field(throwable, extended_slot),
    ) {
        (Value::Object(Some(d)), Value::Object(Some(e))) => {
            d == e && matches!(heap.get_field(throwable, state_slot), Value::Int(2))
        }
        _ => false,
    }
}

/// Return a default value for a field descriptor.
pub fn default_for_descriptor(descriptor: &str) -> Value {
    match descriptor.as_bytes().first() {
        Some(b'I') | Some(b'B') | Some(b'C') | Some(b'S') | Some(b'Z') => Value::Int(0),
        Some(b'J') => Value::Long(0),
        Some(b'F') => Value::Float(0.0),
        Some(b'D') => Value::Double(0.0),
        _ => Value::Object(None),
    }
}

/// The three shapes `ldc2_w` accepts, decoded under the class-manager read
/// lock so the lock can be dropped before a condy bootstrap runs Java.
enum Ldc2wValue {
    Long(i64),
    Double(f64),
    /// A `CONSTANT_Dynamic` whose field type is `long`/`double` — legal under
    /// `ldc2_w` since Java 11 (JVMS §6.5) and already accepted by
    /// `classloading::verify_insn`'s `verify_ldc2w`.
    Dynamic,
}

pub(super) fn execute_ldc2w(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    index: u16,
) -> Result<(), MethodCallFailed> {
    let frame_class_id = thread.frames[frame_idx].class_id;
    // A long/double constant is a pure function of the class's constant pool,
    // so a warm site answers from the per-thread table with no class_manager
    // read lock (see `PrimitiveConstantSiteCache` for the validity argument).
    // Same switch as `ldc`'s record: `CRATONVM_JIT_NO_LDC_CONST_CACHE=1`
    // restores the lock-per-execution path, probe and fill alike.
    let cache_on = ldc_const_cache_enabled(shared);
    if cache_on {
        match thread
            .site_tables
            .prim_const_sites
            .get(frame_class_id, index)
            .copied()
        {
            Some(PrimitiveConstant::Long(v)) => {
                thread.frames[frame_idx].stack.push_long(v)?;
                return Ok(());
            }
            Some(PrimitiveConstant::Double(v)) => {
                thread.frames[frame_idx].stack.push_double(v)?;
                return Ok(());
            }
            // A category-1 constant is never filled at an `ldc2_w` index.
            _ => {}
        }
    }
    // Snapshotted BEFORE the constant pool is read; see `SiteCache::put`.
    let epochs = cache_on.then(|| PrimitiveConstantSiteCache::epochs_for(shared));
    let decoded = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(frame_class_id)
            .ok_or_else(|| VmError::Internal {
                message: "current class not found".to_string(),
            })?;

        // Bounds-check the constant-pool index.  `ConstantPool::get` already
        // returns `None` for out-of-range entries, so mapping the miss to
        // `ClassFormatError` (rather than a panic) satisfies JVMS §4.4.5 for
        // ldc2_w which accepts CONSTANT_Long_info / CONSTANT_Double_info and
        // (Java 11+) a category-2 CONSTANT_Dynamic_info.
        let entry = class.constant_pool.get(index).ok_or_else(|| {
            VmError::Linkage(LinkageError::ClassFormatError {
                class_name: class.name.to_string(),
                message: format!("ldc2_w: constant-pool index {index} out of range"),
            })
        })?;

        match entry {
            ConstantPoolEntry::Long(v) => Ldc2wValue::Long(v.get()),
            ConstantPoolEntry::Double(v) => Ldc2wValue::Double(v.get()),
            ConstantPoolEntry::Dynamic { .. } => Ldc2wValue::Dynamic,
            _ => {
                return Err(VmError::Linkage(LinkageError::ClassFormatError {
                    class_name: class.name.to_string(),
                    message: format!("ldc2_w: expected Long, Double or Dynamic at cp#{index}"),
                })
                .into());
            }
        }
        // cm dropped here
    };

    // The constant pool already carries the Long/Double tag — use it to push
    // directly as a tagged CompactValue slot, avoiding any Value-enum
    // boundary that would collapse Long into the untagged Double bucket.
    match decoded {
        Ldc2wValue::Long(v) => {
            if let Some(epochs) = epochs {
                thread.site_tables.prim_const_sites.put(
                    frame_class_id,
                    index,
                    epochs,
                    PrimitiveConstant::Long(v),
                );
            }
            thread.frames[frame_idx].stack.push_long(v)?
        }
        Ldc2wValue::Double(v) => {
            if let Some(epochs) = epochs {
                thread.site_tables.prim_const_sites.put(
                    frame_class_id,
                    index,
                    epochs,
                    PrimitiveConstant::Double(v),
                );
            }
            thread.frames[frame_idx].stack.push_double(v)?
        }
        Ldc2wValue::Dynamic => {
            // Same bootstrap-once-per-entry path `ldc` uses; only the push
            // differs, because the two slots have to stay tagged.
            match resolve_condy_constant(shared, thread, frame_class_id, index)? {
                Value::Long(v) => thread.frames[frame_idx].stack.push_long(v)?,
                Value::Double(v) => thread.frames[frame_idx].stack.push_double(v)?,
                // A category-1 result under `ldc2_w` is ill-formed. The
                // verifier rejects it outright; under `skip_verification`
                // refuse it here rather than pushing one slot where the rest
                // of the frame's stack map expects two.
                other => {
                    let class_name = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(frame_class_id)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    return Err(VmError::Linkage(LinkageError::ClassFormatError {
                        class_name,
                        message: format!(
                            "ldc2_w: CONSTANT_Dynamic at cp#{index} resolved to a \
                             category-1 value ({other:?}); ldc2_w loads long/double only"
                        ),
                    })
                    .into());
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helper: Loader-faithful CONSTANT_Class resolution
// ---------------------------------------------------------------------------

/// The object component type of an array descriptor, or `None` when `name`
/// is not an array or its element type is primitive. `[[Lcom/x/Y;` yields
/// `com/x/Y` -- the nesting depth is irrelevant, only the element class needs
/// loader-faithful resolution.
#[inline]
pub(super) fn array_component_class_name(name: &str) -> Option<&str> {
    let element = name.strip_prefix('[')?.trim_start_matches('[');
    let inner = element.strip_prefix('L')?.strip_suffix(';')?;
    if inner.is_empty() {
        None
    } else {
        Some(inner)
    }
}

/// Standard JDK namespaces that a user-defined loader never *isolates*: `java/*`
/// is a JVMS-prohibited package for non-bootstrap loaders, and the platform
/// namespaces below are delegated to the parent by ~every real custom loader, so
/// global resolution already yields the loader-correct answer. Short-circuiting
/// them keeps the re-entrant `loadClass` invocation off the common hot path.
///
/// Not short-circuited: a class the referencing user loader has ITSELF defined
/// under such a name (only `java/` is prohibited to it) — see
/// [`is_user_loader_global_name`].
#[inline]
pub(crate) fn is_global_resolution_namespace(name: &str) -> bool {
    name.starts_with("java/")
        || name.starts_with("javax/")
        || name.starts_with("jdk/")
        || (name.starts_with("sun/") && name != "sun/reflect/misc/Trampoline")
        || name.starts_with("com/sun/")
}

/// A global name resolved through a user loader's initiating dictionary.
/// Includes `java/`: the loader cannot define it, but its `loadClass` call,
/// refusal and returned class are observable (i47). Own definitions of the
/// other global namespaces still precede the global route.
#[inline]
pub(super) fn is_user_loader_global_name(name: &str) -> bool {
    is_global_resolution_namespace(name)
}

/// Defining java.* is forbidden; asking loadClass for it is observable.
/// This policy is per VM and independently revertible from GlobalNameAsk.
pub(super) fn loader_global_name_ask_enabled(shared: &SharedVm, name: &str) -> bool {
    // VM-generated bodies call reviewed native-only services. They are not
    // JDK classes and cannot be delegated through an application's loader.
    // Use the same central classification as native dispatch, not a new list.
    if cratonvm_native_api::no_image_receiver::is_reviewed_vm_service_receiver(name) {
        return false;
    }
    if name.starts_with("java/") {
        shared
            .classes
            .aligned(cratonvm_types::HotSpotAlignment::JavaNameAsk)
    } else {
        shared.classes.global_name_ask_on && is_user_loader_global_name(name)
    }
}

/// Read-only loader-faithful lookup: the `ClassId` that `referencing_class_id`'s
/// *user-defined* defining loader resolves `name` to, **without** any re-entrant
/// `loadClass` call — i.e. only what that loader has already defined itself (its
/// own isolated copy) or previously initiated (memoised in
/// [`SharedVm::initiating_resolution_cache`]).
///
/// Returns `None` (so the caller keeps its existing global resolution) when the
/// gate is off, the loader is built-in, the name is a JDK/array name, or the
/// loader has not yet resolved `name`. A `Some` answer is always loader-correct:
/// a class a loader has itself defined, or a prior validated initiating result.
/// This is the half of loader-faithful resolution that needs no `&mut thread`,
/// so it is safe to call from the hot field/method-owner resolvers.
/// Whether `referencing_class_id`'s DEFINING loader is a `groovy.lang.
/// GroovyClassLoader` (or a subtype, e.g. `GroovyClassLoader$InnerLoader` —
/// the per-`parseClass`-call loader Groovy mints internally). Used to widen
/// the loader-initiated resolution trigger (below) to Groovy scripts
/// unconditionally, without touching the global `CRATONVM_LOADER_AWARE_
/// RESOLUTION` gate's default (which stays off pending the full Tomcat /
/// Hibernate / WildFly custom-loader soak it was written for — see
/// `hib-proxyclassreuse-loader-blind-class-resolution-FIXED.md`).
///
/// Rationale (context.groovy bug cluster): `GroovyShell.evaluate` compiles
/// each script through its own fresh `GroovyClassLoader$InnerLoader`
/// instance, and Groovy names every closure literal in a script
/// positionally (`<Script>$_run_closure1`, `$_run_closure2`, …) — so two
/// different scripts loaded in the same process routinely produce two
/// DIFFERENT classes sharing an identical name. With the gate off, `new
/// <closure>(...)` / `checkcast` / similar `CONSTANT_Class` resolution from
/// the SECOND script's own bytecode resolved through the flat global class
/// store (`load_class_concurrent`) and silently collapsed onto whichever
/// same-named closure a DIFFERENT script's `InnerLoader` had registered
/// first — no exception, just the wrong compiled bytecode running (e.g.
/// Spring's `GroovyBeanDefinitionReader` executing an earlier script's
/// closure body and registering none of the beans the current script
/// declared). A real type check here (subtype of `GroovyClassLoader`, not a
/// class-NAME string match) keeps the blast radius to Groovy's own loader
/// hierarchy: Hibernate's ByteBuddy/CGLIB isolating loaders, Tomcat's
/// `WebappClassLoader`, and WildFly's module loaders are never
/// `GroovyClassLoader` instances, so their resolution order is completely
/// unaffected by this check.
pub(super) fn is_groovy_class_loader(
    shared: &SharedVm,
    loader_obj: cratonvm_types::ObjectRef,
) -> bool {
    is_groovy_class_loader_in(shared, &shared.classes.class_manager.read(), loader_obj)
}

/// [`is_groovy_class_loader`] against a class manager the caller already
/// holds (the JIT's compile-time resolvers run under its read lock, and a
/// nested read while a writer queues deadlocks).
fn is_groovy_class_loader_in(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    loader_obj: cratonvm_types::ObjectRef,
) -> bool {
    let loader_class_id = shared.mem.heap.class_id_of(loader_obj);
    match cm.get_loaded_class_id("groovy/lang/GroovyClassLoader") {
        Some(groovy_cl_id) => {
            loader_class_id == groovy_cl_id || cm.is_subclass_of(loader_class_id, groovy_cl_id)
        }
        // `groovy/lang/GroovyClassLoader` not loaded at all in this process
        // (no Groovy on the classpath) => trivially not a Groovy loader.
        None => false,
    }
}

/// Whether `referencing_class_id`'s DEFINING loader is (exactly) Spring's
/// `org.springframework.core.test.tools.CompileWithForkedClassLoaderClassLoader`
/// -- the loader `@CompileWithForkedClassLoader` gives each annotated test
/// method its own fresh instance of. Same rationale and same narrow,
/// type-checked shape as [`is_groovy_class_loader`] just above: this
/// loader's `findClass` deliberately redefines any class (INCLUDING
/// framework classes like `MergedAnnotations$SearchStrategy`, not just the
/// test's own fixtures) it can pull bytes for via `testClassLoader.
/// getResourceAsStream(...)`, so code running inside its context has a
/// genuinely FRESH `Class`/enum-constant identity for those classes, by
/// design -- matches real CGLIB/Spring behavior, works fine on HotSpot.
/// With the global gate off, a `CONSTANT_Class`/field-ref resolution
/// reached from inside that forked context for a name the built-in
/// delegation chain can ALSO serve (e.g. any `org/springframework/*`
/// class -- `is_global_resolution_namespace` only excludes `java`/`javax`/
/// `jdk`/`sun`/`com.sun`) skipped the loader-initiated fast path entirely
/// and went straight to the global, loader-blind `load_class_concurrent`,
/// which can silently create a SECOND, distinct `ClassId` for a class the
/// fork's own loader already has its own copy of -- surfacing as
/// `IllegalStateException`/`ClassCastException`-shaped `==`/`instanceof`
/// failures wherever the two identities meet (see
/// `CRATONVM-SPRING-GENUINE-BUGLIST`'s
/// `searchEnclosingClass` writeup). The class is `final` with no
/// subtypes, so an exact-id match is sufficient -- no `is_subclass_of`
/// walk needed, unlike Groovy's.
pub(super) fn is_compile_with_forked_class_loader(
    shared: &SharedVm,
    loader_obj: cratonvm_types::ObjectRef,
) -> bool {
    is_compile_with_forked_class_loader_in(shared, &shared.classes.class_manager.read(), loader_obj)
}

/// [`is_compile_with_forked_class_loader`] under a held class manager.
fn is_compile_with_forked_class_loader_in(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    loader_obj: cratonvm_types::ObjectRef,
) -> bool {
    let loader_class_id = shared.mem.heap.class_id_of(loader_obj);
    match cm.get_loaded_class_id(
        "org/springframework/core/test/tools/CompileWithForkedClassLoaderClassLoader",
    ) {
        Some(forked_cl_id) => loader_class_id == forked_cl_id,
        // Not loaded at all in this process (spring-core-test not on the
        // classpath, or the annotation never used) => trivially not this loader.
        None => false,
    }
}

/// Whether `referencing_class_id`'s DEFINING loader is (exactly) Spring
/// Boot's `org.springframework.boot.testsupport.classpath.
/// ModifiedClassPathClassLoader` -- the loader `@ForkedClassPath` (and
/// `@ClassPathExclusions`/`@ClassPathOverrides`) gives each annotated test
/// METHOD its own fresh instance of, via `ModifiedClassPathExtension`
/// re-running the test through a new in-process `Launcher` with this
/// loader as the thread context classloader (see that extension's own
/// `interceptMethod` -- there is no real subprocess fork; several sibling
/// `ModifiedClassPathClassLoader` instances, one per test method, coexist
/// in the SAME process, each with its own isolated copy of anything it
/// redefines). Same rationale and same narrow, type-checked, `final`-class
/// exact-id shape as [`is_compile_with_forked_class_loader`] just above --
/// this loader was simply missing from that carve-out, a separate Spring
/// test-isolation mechanism in a separate module
/// (`spring-boot-testsupport` vs. `spring-core-test`).
///
/// Without this, a `Method.getDefaultValue()`/annotation-enum resolution
/// (or any other loader-faithful lookup) reached from a class this loader
/// defined fell through to the global, loader-blind path, which can define
/// a THIRD, `Application`-scoped copy of a name two sibling
/// `ModifiedClassPathClassLoader` instances already each have their own
/// copy of (`resolve_fast_path_class_id`'s "no loaded candidate" ambiguity
/// when more than one `UserDefined` loader has defined the same name) --
/// surfacing as `ConversionNotSupportedException: Cannot convert value of
/// type 'jakarta.servlet.DispatcherType' to required type
/// 'jakarta.servlet.DispatcherType'`
/// (`MockWebEnvironmentServletComponentScanIntegrationTests.
/// indexedComponentsAreRegistered`, see
/// `fixed-suite-bugs/hibernate/nonpassed-classbyclass-census-20260922-FIXED-20260922.md`).
pub(super) fn is_modified_class_path_loader(
    shared: &SharedVm,
    loader_obj: cratonvm_types::ObjectRef,
) -> bool {
    is_modified_class_path_loader_in(shared, &shared.classes.class_manager.read(), loader_obj)
}

/// [`is_modified_class_path_loader`] under a held class manager.
fn is_modified_class_path_loader_in(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    loader_obj: cratonvm_types::ObjectRef,
) -> bool {
    let loader_class_id = shared.mem.heap.class_id_of(loader_obj);
    match cm.get_loaded_class_id(
        "org/springframework/boot/testsupport/classpath/ModifiedClassPathClassLoader",
    ) {
        Some(mcp_cl_id) => loader_class_id == mcp_cl_id,
        // Not loaded at all in this process (spring-boot-test-support not on
        // the classpath, or the annotation never used) => trivially not this
        // loader.
        None => false,
    }
}

/// Whether loader-initiated (JVMS §5.4.3 initiating-loader) `CONSTANT_Class`
/// resolution should run for a reference from `referencing_class_id`: either
/// the global gate is on, or the referencing class was defined by a
/// `GroovyClassLoader` (see [`is_groovy_class_loader`]'s doc comment for the
/// full rationale — this is the narrow, type-checked carve-out for the
/// context.groovy bug cluster that does not touch the gate's default).
#[inline]
pub(super) fn should_use_loader_initiated_resolution(
    shared: &SharedVm,
    referencing_class_id: ClassId,
) -> bool {
    if crate::runtime::env_cache::loader_aware_resolution() {
        return true;
    }
    match cratonvm_native_builtins::classloader::defining_loader_for(
        &shared.loader_identity,
        referencing_class_id.as_u32(),
    ) {
        Some(loader_obj) => {
            is_groovy_class_loader(shared, loader_obj)
                || is_compile_with_forked_class_loader(shared, loader_obj)
                || is_modified_class_path_loader(shared, loader_obj)
        }
        None => false,
    }
}

/// [`should_use_loader_initiated_resolution`] under a held class manager.
/// Same answer, same questions in the same order; only the lock is the
/// caller's.
pub(crate) fn should_use_loader_initiated_resolution_in(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    referencing_class_id: ClassId,
) -> bool {
    if crate::runtime::env_cache::loader_aware_resolution() {
        return true;
    }
    match cratonvm_native_builtins::classloader::defining_loader_for(
        &shared.loader_identity,
        referencing_class_id.as_u32(),
    ) {
        Some(loader_obj) => {
            is_groovy_class_loader_in(shared, cm, loader_obj)
                || is_compile_with_forked_class_loader_in(shared, cm, loader_obj)
                || is_modified_class_path_loader_in(shared, cm, loader_obj)
        }
        None => false,
    }
}

/// The class `referencing_class_id`'s `CONSTANT_Class` reference to `name`
/// resolves to, answered from what is ALREADY known -- no `loadClass`, no
/// class definition, no Java -- or `None` when only running the referencing
/// class's defining loader could say.
///
/// [`resolve_class_loader_aware`] is the interpreter's door, and for a
/// loader-sensitive reference it drives the defining loader's own
/// `loadClass`. Callers with no `JvmThread`, or holding the class-manager
/// read lock, cannot: the JIT's compile-time resolvers (a `new` site, a
/// type-check target, an invoke owner, an inlining candidate) and the fast
/// paths the JIT's run-time helpers take in front of a real resolution. They
/// used `ClassManager::find_class_by_name_for_class`, whose answer for a user
/// loader is "its own namespace, then its recorded parents, then the built-in
/// chain". That is a GUESS at what the loader's `loadClass` will return, and
/// for a child-first loader it is the wrong guess for every name the loader
/// has not asked for yet: Spring's `CompileWithForkedClassLoaderClassLoader`,
/// Groovy's `InnerLoader` and Boot's `ModifiedClassPathClassLoader` define
/// their OWN copy when asked. The JIT baked the application's `ClassId` into
/// fork code compiled before the fork had defined the name -- fork code
/// instantiated the APPLICATION's ByteBuddy
/// `JavaDispatcher$ProxiedInvocationHandler` -- and only the by-name rule in
/// `instanceof`/`checkcast` hid it (`docs/internal/fixed-suite-bugs/spring/`
/// `spring-jdkonly-bytecode-cast-name-rule-jit-class-resolution-FIXED-20260923.md`).
///
/// So for a loader-sensitive reference -- the predicate `resolve_field_ref`
/// already applies to the same question about a field owner: a user-defined
/// referencing loader, loader-initiated resolution in force, a name outside
/// [`is_global_resolution_namespace`] -- the answer is only what the loader
/// has DEFINED itself or has already INITIATED (the memo
/// [`drive_defining_loader_load`] fills). An object array is answered through
/// its element the same way and then read under the element's defining loader,
/// which is where JVMS §5.3.3 files it. Every other reference keeps
/// `find_class_by_name_for_class`, unchanged.
///
/// `None` is a statement about knowledge, not existence: the caller must defer
/// (a `Deferred` `new` site, a site the run-time helper resolves through the
/// loader), never treat the class as absent.
///
/// A hidden class's reference to its own class-file name is deferred too
/// (JIT round 21 wave 14, lane hidden-static, `CRATONVM_JIT_HIDDEN_SELF_BY_ID`):
/// no name lookup can answer it (the class is in no namespace), and the one
/// this function makes found an ordinary class of that name once one was
/// loaded -- a compile then baked that class's id. The run-time resolution
/// the deferral reaches answers the hidden class itself
/// ([`hidden_self_reference`]).
pub(crate) fn class_resolved_without_loading(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    referencing_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    if hidden_class_may_refer_to_itself(shared, referencing_class_id)
        && hidden_self_reference_in(cm, referencing_class_id, name)
        && jit_hidden_self_by_id_enabled()
    {
        return None;
    }
    let loader = match cm.get_loader_id(referencing_class_id) {
        Some(l @ cratonvm_types::ClassLoaderId::UserDefined(_)) => l,
        _ => {
            let found = cm.find_class_by_name_for_class(name, referencing_class_id)?;
            // A class a USER-DEFINED loader defined is filed under the
            // application namespace when its NAME was new to the VM, and then
            // answers a built-in loader's own reference to that name. A class
            // of a BUILT-IN loader cannot see it: the loader is about to define
            // (or find) its own copy, and a compile that bound this one built
            // `org.h2`-style instances of the fork's class into the
            // application's graph (`jit` `new Helper` of the application copy
            // answered the `ModifiedClassPathClassLoader` fork's, once the fork
            // had defined the name first). Not known without loading.
            if !name.starts_with('[')
                && cratonvm_native_builtins::classloader::any_defining_loader_registered(&shared.loader_identity)
                && cratonvm_native_builtins::classloader::defining_loader_for(
                    &shared.loader_identity,
                    referencing_class_id.as_u32(),
                )
                .is_none()
                && cratonvm_native_builtins::classloader::defining_loader_for(
                    &shared.loader_identity,
                    found.as_u32(),
                )
                .is_some()
            {
                return None;
            }
            return Some(found);
        }
    };
    let element = array_component_class_name(name);
    let key = element.unwrap_or(name);
    // A globally loaded java.* class does not mean this user loader has
    // resolved it. Compiling a cold field/call/intrinsic against that class
    // would erase the first observable loadClass call (i47).
    if key.starts_with("java/") && loader_global_name_ask_enabled(shared, key) {
        let known = direct_supertype_named(cm, referencing_class_id, key).or_else(|| {
            crate::runtime::resolve::initiating_records::loader_initiated_answer(
                shared, loader, key,
            )
        })?;
        if element.is_none() {
            return Some(known);
        }
        return cm.loaded_class_under_exact_key(name, cm.get_loader_id(known)?);
    }
    if (name.starts_with('[') && element.is_none())
        || is_global_resolution_namespace(key)
        || !should_use_loader_initiated_resolution_in(shared, cm, referencing_class_id)
    {
        return cm.find_class_by_name_for_class(name, referencing_class_id);
    }
    let known = cm
        .loaded_class_under_exact_key(key, loader)
        .or_else(|| direct_supertype_named(cm, referencing_class_id, key))
        .or_else(|| {
            crate::runtime::resolve::initiating_records::loader_initiated_answer(
                shared, loader, key,
            )
        })?;
    if element.is_none() {
        return Some(known);
    }
    let array_loader = match cm.get_loader_id(known) {
        Some(l @ cratonvm_types::ClassLoaderId::UserDefined(_)) => l,
        _ => cratonvm_types::ClassLoaderId::Bootstrap,
    };
    cm.loaded_class_under_exact_key(name, array_loader)
}

/// Whether `owner`, the class a method reference's resolution found for the
/// non-`java/` JDK-global name `name` in `referencing_class_id`'s namespace
/// (`field_access::owner_in_referencing_namespace`), is only the built-in
/// chain's GUESS at what the class's user-defined loader will answer: the
/// global-name ask is on for the name ([`loader_global_name_ask_enabled`]),
/// the loader is not known to answer runtime-image names as the JDK does (its
/// `loader_global_name_transparency` row is `false`, or not computed yet: the
/// first drive computes it), and `owner` is neither the loader's own
/// definition of the name, nor the class's direct supertype of that name, nor
/// the loader's recorded answer for it. Such a loader is asked
/// (`drive_loader_for_global_name`) before the call binds, and may define its
/// own class (`L5W27JdkNamedOwnClass` row 7: a child-first loader defining
/// `javax.security.auth.x500.X500Principal` on request); a resolution failure
/// judged against java.base's class raised `NoSuchMethodError` before the
/// loader was asked. Under the caller's class-manager guard; reads the
/// transparency map and the initiating memo (their writers hold no
/// class-manager lock).
///
/// `CRATONVM_GLOBAL_NAME_OWNER_UNGUESSED` (`VmConfig::global_name_owner_unguessed`,
/// default ON, HotSpot parity; `=0` restores judging the guess). Interpreter
/// round i6 wave 124, lane L5; page
/// `i124-L5-lazy-jdk-named-own-class-invokestatic-nsme`.
///
/// Wave 125 (lane L5; `docs/internal/fixed-bugs/interpreter-i125-L9-global-name-guess-filter-can-leave-a-site-uncached-FIXED-20261224.md`):
/// the predicate reads the same fact the doors read before they decide not to
/// ask the loader again, so it cannot keep answering "guess" for a name no
/// door will drive. NOT a guess when the loader's recorded answer
/// (`lookup_loader_initiated`'s two reads for a global name: its own
/// exact-key definition, then `loader_initiated_answer`) holds ANY class,
/// even one other than `owner` (a delegation to a user-loader parent the VM
/// has not recorded: the memo says `Y`, the flat lookup `G`, and
/// `resolve_owner_unknown_to_caller` returns early on `Y`); nor when the
/// `Methodref`'s `CONSTANT_Class` entry `class_index` is recorded
/// (`recorded_class_entry`, which `execute_invokestatic` reads first, and
/// which outlives the capped memo's eviction). `class_index`: `None` where
/// the caller does not have it.
///
/// Wave 126 (lane L9 review of wave 125): the answer is the OWNER to judge,
/// not a yes/no. `Some(owner)` where nothing is guessed (switch off, not such
/// a name or loader: byte-identical to wave 124's `false`); `Some(recorded)`
/// when the loader's recorded answer or the class entry's record settles it
/// -- the RECORDED class, which can differ from the flat lookup's `owner`
/// (the memo's `Y` against `G`: judging `G` raised wave 124's
/// `NoSuchMethodError` again, `method_ref_provably_unresolvable` against
/// java.base's class, on `L5W125GuessFilterUnrecordedParent`'s own sites);
/// `None` for a guess (wave 124's `true`).
pub(crate) fn global_name_owner_settled(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    referencing_class_id: ClassId,
    name: &str,
    owner: ClassId,
    class_index: Option<u16>,
) -> Option<ClassId> {
    if !shared.config.global_name_owner_unguessed
        || name.starts_with('[')
        || name.starts_with("java/")
        || !is_global_resolution_namespace(name)
    {
        return Some(owner);
    }
    let loader = match cm.get_loader_id(referencing_class_id) {
        Some(l @ cratonvm_types::ClassLoaderId::UserDefined(_)) => l,
        _ => return Some(owner),
    };
    if !loader_global_name_ask_enabled(shared, name) {
        return Some(owner);
    }
    let transparent = shared
        .classes
        .loader_global_name_transparency
        .read()
        .get(&loader)
        .copied();
    if transparent == Some(true) {
        return Some(owner);
    }
    // Not classified yet: only a class with a defining-loader object is ever
    // asked (`drive_loader_for_global_name` answers `None` without one, and
    // the guess would then never become a recorded answer).
    if transparent.is_none()
        && cratonvm_native_builtins::classloader::defining_loader_for(
            &shared.loader_identity,
            referencing_class_id.as_u32(),
        )
        .is_none()
    {
        return Some(owner);
    }
    // The site's class entry recorded (the static door's first test; JVMS
    // §5.4.3: the entry's first resolution stands). A read of the resolution
    // cache under the caller's class-manager guard, as `jit_bridge.rs`'s
    // recorded-entry probe does.
    if let Some(recorded) =
        class_index.and_then(|index| recorded_class_entry(shared, referencing_class_id, index))
    {
        return Some(recorded);
    }
    // Wave 125: any recorded answer settles it (the receiver door's own
    // test), not only one equal to `owner` -- and that answer, not `owner`,
    // is the class judged (wave 126, L9).
    cm.loaded_class_under_exact_key(name, loader)
        .or_else(|| direct_supertype_named(cm, referencing_class_id, name))
        .or_else(|| {
            crate::runtime::resolve::initiating_records::loader_initiated_answer(
                shared, loader, name,
            )
        })
}

/// `class_id`'s direct superclass or direct superinterface named `name`.
///
/// Linking `class_id` resolved exactly these names through its own defining
/// loader (JVMS §5.3.5 step 3), so that loader has initiated them, and the
/// answer is the one its constant pool will get. The link path does not go
/// through [`drive_defining_loader_load`], so the initiating memo does not
/// hold them. Only DIRECT supertypes: a grandparent was resolved by the
/// parent's loader, which says nothing about what `class_id`'s would answer.
fn direct_supertype_named(
    cm: &crate::classloading::ClassManager,
    class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    let class = cm.get_class(class_id)?;
    class
        .superclass
        .into_iter()
        .chain(class.interfaces.iter().copied())
        .find(|&sup| cm.get_class(sup).is_some_and(|c| &*c.name == name))
}

// ---------------------------------------------------------------------------
// JVMS §5.4.3.1 / §5.4.4: access to the class a `CONSTANT_Class` names
// ---------------------------------------------------------------------------

/// HotSpot's `IllegalAccessError` message when `referencing_class_id` may not
/// access the class a `CONSTANT_Class` named `name` resolves to, or `None`
/// when it may (or when the question cannot be answered without loading).
///
/// HotSpot runs this check once, where every class-constant resolution goes
/// (`ConstantPool::klass_at_impl` -> `LinkResolver::check_klass_accessibility`),
/// so `new`, `anewarray`, `multianewarray`, `checkcast`, `instanceof` and `ldc`
/// all get it. For an array name the class checked is the BOTTOM element class
/// (`ObjArrayKlass::bottom_klass`); a primitive-element array is always
/// accessible.
///
/// `resolved` is what `name` resolved to, when the caller has it. The element
/// class of an array name, and a non-array name the caller did not resolve, are
/// looked up with [`class_resolved_without_loading`]: nothing here loads,
/// allocates or safepoints, so a caller may hold a raw reference across it,
/// and a class that is not loaded through the referencing loader yet answers
/// `None` (the caller's own resolution comes first on every path that can
/// load). Takes the caller's class-manager guard and no lock of its own
/// besides the initiating-loader memo `class_resolved_without_loading` reads.
pub(crate) fn class_constant_access_denial_in(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    referencing_class_id: ClassId,
    name: &str,
    resolved: Option<ClassId>,
) -> Option<String> {
    class_constant_access_verdict_in(shared, cm, referencing_class_id, name, resolved)?.err()
}

/// [`class_constant_access_denial_in`] with "cannot tell" kept apart from
/// "accessible": `None` when the class to check is not known through the
/// referencing loader yet, `Some(Ok(()))` when it is accessible (a
/// primitive-element array always is), `Some(Err(message))` when it is not.
fn class_constant_access_verdict_in(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    referencing_class_id: ClassId,
    name: &str,
    resolved: Option<ClassId>,
) -> Option<Result<(), String>> {
    let element = if name.starts_with('[') {
        let Some(element_name) = array_component_class_name(name) else {
            return Some(Ok(()));
        };
        // A resolved array class names its bottom element by id (i11-L2):
        // exactly the class its resolution loaded, with no second lookup.
        match resolved.and_then(|id| array_bottom_element_class(cm, id, element_name)) {
            Some(id) => id,
            None => class_resolved_without_loading(shared, cm, referencing_class_id, element_name)?,
        }
    } else {
        match resolved {
            Some(id) => id,
            None => class_resolved_without_loading(shared, cm, referencing_class_id, name)?,
        }
    };
    if element == referencing_class_id {
        return Some(Ok(()));
    }
    let accessor = cm.get_class(referencing_class_id)?;
    let target = cm.get_class(element)?;
    let package_denial = MemberResolver::class_access_denial(accessor, target, &cm.module_registry);
    let package_refused = package_denial.is_some();
    Some(
        match package_denial.or_else(|| class_export_refusal(cm, accessor, target)) {
            None => Ok(()),
            // HotSpot's `Reflection::verify_class_access` admits every access
            // from a subclass of `jdk.internal.reflect.SerializationConstructorAccessorImpl`:
            // the serialization constructor accessors the JDK spins as bytecode
            // `new` and cast the (often package-private) class being
            // deserialized. A JVM rule, asked only on a refusal.
            Some(_) if extends_serialization_constructor_accessor(cm, referencing_class_id) => {
                Ok(())
            }
            Some(message) if package_refused => Err(class_access_message_naming_user_loaders(
                shared, cm, accessor, target, message,
            )),
            Some(message) => Err(message),
        },
    )
}

/// The package refusal's `IllegalAccessError` text with a user-defined
/// loader named as HotSpot names it (`(Box is in unnamed module of loader
/// L$ChildFirst @1dbd16a6; Caller is in unnamed module of loader ...)`), when
/// VM messages name user loaders (`messages_name_user_loaders`: `--jdk-only`,
/// or `--compatible` with `CRATONVM_COMPAT_MODULE_MESSAGES`, the rule the
/// `ClassCastException` funnel follows). `message` (the bare text) otherwise,
/// and when a loader's `nameAndId` cannot be read. Runs under the caller's
/// class-manager guard: reads the defining-loader row and the loader's
/// `nameAndId` string, and allocates no Java object (JIT round 17 wave 5,
/// lane mh5).
#[cold]
fn class_access_message_naming_user_loaders(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    accessor: &crate::classloading::Class,
    target: &crate::classloading::Class,
    message: String,
) -> String {
    if !messages_name_a_user_loader(shared, accessor, target) {
        return message;
    }
    let name_and_id =
        |class: &crate::classloading::Class| user_loader_name_and_id_under_guard(shared, cm, class);
    crate::classloading::access_control::class_access_denied_message_naming(
        accessor,
        target,
        Some(&cm.module_registry),
        &name_and_id,
    )
}

/// Does an access message naming `a` and `b` name a user-defined loader, as
/// HotSpot's does: one of them is a user loader's class, and this VM's
/// messages name user loaders (`messages_name_user_loaders`)? Shared by the
/// class text above and the member texts of `field_access`'s
/// `member_access_refusal` (JIT round 20 wave 15, lane huntLink).
pub(super) fn messages_name_a_user_loader(
    shared: &SharedVm,
    a: &crate::classloading::Class,
    b: &crate::classloading::Class,
) -> bool {
    use cratonvm_types::ClassLoaderId;
    let user =
        |c: &crate::classloading::Class| matches!(c.loader_id, ClassLoaderId::UserDefined(_));
    (user(a) || user(b)) && shared.natives.native_methods.messages_name_user_loaders()
}

/// HotSpot's `ClassLoaderData::loader_name_and_id()` for `class`'s defining
/// loader object: its `ClassLoader.nameAndId` string, `None` when it cannot be
/// read. Under the caller's class-manager guard; allocates nothing.
pub(super) fn user_loader_name_and_id_under_guard(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    class: &crate::classloading::Class,
) -> Option<String> {
    let loader = cratonvm_native_builtins::classloader::defining_loader_for(
        &shared.loader_identity,
        class.id.as_u32(),
    )?;
    let heap = &shared.mem.heap;
    let slot = crate::vm::vm_exec::resolve_field_index_in_hierarchy(
        heap.class_id_of(loader),
        "nameAndId",
        &cm.class_store,
    )?;
    match heap.get_field(loader, slot) {
        crate::types::Value::Object(Some(s)) => {
            crate::vm::read_java_string(heap, s).filter(|s| !s.is_empty())
        }
        _ => None,
    }
}

/// Whether `class_id` is a subclass of THE bootstrap
/// `jdk.internal.reflect.SerializationConstructorAccessorImpl`, the class
/// whose subclasses HotSpot exempts from the access checks
/// (`Reflection::verify_class_access` / `verify_member_access`,
/// `vmClasses::reflect_SerializationConstructorAccessorImpl_klass()`).
///
/// By identity, not by name: only `java.*` is a prohibited package, so a user
/// loader may define its own class under that binary name, and a subclass of
/// it is not exempt on HotSpot (probe
/// `tools/probes/interp/L5/L5W26SerializationAccessorSpoof.java`). The
/// loader-blind `is_subclass_of_by_name` this replaced admitted it
/// (interpreter round i1 wave 26, lane L5). Asked only on a refusal.
pub(crate) fn extends_serialization_constructor_accessor(
    cm: &crate::classloading::ClassManager,
    class_id: ClassId,
) -> bool {
    cm.find_bootstrap_class_by_name("jdk/internal/reflect/SerializationConstructorAccessorImpl")
        .is_some_and(|accessor| cm.is_subclass_of(class_id, accessor))
}

/// The JPMS export clause of JVMS §5.4.4, after the package half admitted
/// (i11-L2): `Some(message)` when it refuses, for every accessor in every
/// mode. `--jdk-only` has enforced it since its wave-12 census read zero
/// would-be refusals over the core and jdk-only suites; `--compatible` since
/// wave 14, when the same census over real workloads (a Spring Boot 4.2
/// application with logback / log4j-api / hibernate-validator, Spring Boot
/// fat jars, Netty buffer and pcap tests) read zero as well
/// (`docs/internal/fixed-bugs/interpreter-L2-jpms-export-clause-not-checked-on-class-constants-FIXED-20260925.md`).
///
/// The caller applies `SerializationConstructorAccessorImpl`'s exemption to
/// a refusal, as HotSpot applies it before the module checks.
fn class_export_refusal(
    cm: &crate::classloading::ClassManager,
    accessor: &crate::classloading::Class,
    target: &crate::classloading::Class,
) -> Option<String> {
    crate::classloading::access_control::class_export_denial(accessor, target, &cm.module_registry)
        .map(|denial| denial.message)
}

/// The bottom element class of the reference-array class `array_id`, read
/// through `array_info` (JVMS §5.3.3: the class its component resolution
/// produced), when that chain is recorded and ends at a class named
/// `element_name`; `None` otherwise, and the caller looks the name up.
fn array_bottom_element_class(
    cm: &crate::classloading::ClassManager,
    array_id: ClassId,
    element_name: &str,
) -> Option<ClassId> {
    let mut id = array_id;
    // JVMS §4.4.1 caps an array type at 255 dimensions.
    for _ in 0..=u8::MAX {
        let class = cm.get_class(id)?;
        match class.array_info.as_ref() {
            Some(info) => id = info.component_class_id,
            None => return (&*class.name == element_name).then_some(id),
        }
    }
    None
}

/// Is the class-constant access check enforced for the instructions other
/// than `new`?
///
/// Every mode since interpreter round i1 wave 10 (lane L2). Wave 9 enforced
/// it under `--jdk-only` only and traced `--compatible` admissions
/// (`CRATONVM_DBG_ACCESS=1`, `[ACCESS-DBG] ADMIT (compatible)`) for a census,
/// because CratonVM defines classes HotSpot does not (VM-built proxies,
/// `cratonvm/synthetic/*` carriers, synthetic JDK stand-ins); that census read
/// zero admissions over the core and jdk-only suites and the Spring Boot /
/// log4j drivers, so the check is HotSpot's in both modes
/// (docs/internal/fixed-bugs/interpreter-L2-class-access-checked-only-by-new-FIXED-20260925.md).
/// Kept as a function: the JIT helpers and [`class_constant_fill_admitted`]
/// ask it, and a future mode that must opt out has one place to say so.
#[inline]
pub(crate) fn class_constant_access_enforced(shared: &SharedVm) -> bool {
    let _ = shared;
    true
}

/// The JVMS §5.4.4 check for a `CONSTANT_Class` resolution by `anewarray`,
/// `multianewarray`, `checkcast`, `instanceof` or `ldc` (`insn` names the
/// instruction for the `CRATONVM_DBG_ACCESS` trace), after the resolution
/// itself succeeded.
///
/// A refusal is an `IllegalAccessError` recorded against `(referencing class,
/// cp_index)`, so every later execution of any instruction naming the entry
/// rethrows it without asking again (JVMS §5.4.3). Call it BEFORE filling any
/// site cache or recording the resolution: a fill is what lets later
/// executions skip it.
///
/// Test-only since interpreter round i1 wave 24 (lane L5): every production
/// caller records the refusal through [`check_class_constant_access_as_of`].
#[cfg(test)]
pub(crate) fn check_class_constant_access(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    cp_index: u16,
    name: &str,
    resolved: Option<ClassId>,
    insn: &'static str,
) -> Result<(), MethodCallFailed> {
    check_class_constant_access_in(
        shared,
        thread,
        (referencing_class_id, cp_index),
        name,
        resolved,
        insn,
        None,
    )
}

/// `check_class_constant_access` (now test-only) for a resolution that took
/// `ResolutionCache::fill_snapshot` `as_of` BEFORE it read `name` from the
/// constant pool: the `IllegalAccessError` is thrown either way, but recorded
/// against the entry only while no redefinition of the referencing class may
/// have replaced the pool since (`record_resolution_failure_as_of`;
/// `i22-L5-resolution-cache-fills-can-outlive-a-concurrent-redefinition`,
/// interpreter round i1 wave 24, lane L5).
pub(crate) fn check_class_constant_access_as_of(
    shared: &SharedVm,
    thread: &mut JvmThread,
    (referencing_class_id, cp_index): (ClassId, u16),
    name: &str,
    resolved: Option<ClassId>,
    insn: &'static str,
    as_of: u64,
) -> Result<(), MethodCallFailed> {
    check_class_constant_access_in(
        shared,
        thread,
        (referencing_class_id, cp_index),
        name,
        resolved,
        insn,
        Some(as_of),
    )
}

fn check_class_constant_access_in(
    shared: &SharedVm,
    thread: &mut JvmThread,
    (referencing_class_id, cp_index): (ClassId, u16),
    name: &str,
    resolved: Option<ClassId>,
    insn: &'static str,
    as_of: Option<u64>,
) -> Result<(), MethodCallFailed> {
    if !class_constant_access_enforced(shared) {
        return Ok(());
    }
    let denial = {
        let cm = shared.classes.class_manager.read();
        class_constant_access_denial_in(shared, &cm, referencing_class_id, name, resolved)
    };
    let Some(message) = denial else {
        return Ok(());
    };
    if cratonvm_types::flags().loader.dbg_access {
        eprintln!("[ACCESS-DBG] DENY {insn}: {message}");
    }
    Err(raise_class_access_denial(
        shared,
        thread,
        referencing_class_id,
        cp_index,
        &message,
        as_of,
    ))
}

/// The `IllegalAccessError` for a refused class-constant resolution,
/// recorded against the entry (JVMS §5.4.3); under `as_of` when the caller
/// took a fill snapshot before its pool read.
#[cold]
fn raise_class_access_denial(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    cp_index: u16,
    message: &str,
    as_of: Option<u64>,
) -> MethodCallFailed {
    // Interpreter round i5 wave 100, lane L4 (probe `L4W100UnnamedModuleHashTexts`):
    // an unnamed-module accessor is named `unnamed module @0x<hash>`, as HotSpot
    // does (the class manager's text has no hash). No class-manager guard is held.
    let message = crate::vm::access_denial_with_unnamed_module_hash_on(
        shared,
        thread,
        referencing_class_id,
        message.to_string(),
    );
    let error = match crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        "java/lang/IllegalAccessError",
        Some(message.as_str()),
    ) {
        Ok(obj) => MethodCallFailed::ExceptionThrown(obj),
        Err(e) => e,
    };
    match as_of {
        Some(as_of) => {
            record_resolution_failure_as_of(shared, referencing_class_id, cp_index, error, as_of)
        }
        None => record_resolution_failure(shared, referencing_class_id, cp_index, error),
    }
}

/// May a site cache remember that `referencing_class_id`'s `CONSTANT_Class`
/// `name` resolved to `resolved`? `false` only when the access check is
/// enforced and refuses: a fill is what lets later executions skip
/// [`check_class_constant_access_as_of`], so a path that resolves WITHOUT running it
/// (an exception handler's catch type, `exception_dispatch::catch_row_verdict`)
/// must not fill an entry a `checkcast` of the same index would then hit.
pub(crate) fn class_constant_fill_admitted(
    shared: &SharedVm,
    referencing_class_id: ClassId,
    name: &str,
    resolved: ClassId,
) -> bool {
    if !class_constant_access_enforced(shared) {
        return true;
    }
    let cm = shared.classes.class_manager.read();
    class_constant_access_denial_in(shared, &cm, referencing_class_id, name, Some(resolved))
        .is_none()
}

#[inline]
pub(super) fn lookup_loader_initiated(
    shared: &SharedVm,
    referencing_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    hotpath_counts::bump(&hotpath_counts::LOOKUP_LOADER_INITIATED_CALLS);
    // PERF (h2-bnf-perf 2026-07-23): several callers (resolve_class_loader_aware,
    // called on every new/checkcast/instanceof/anewarray; also
    // resolved_private_invokevirtual_target) call this unconditionally, with
    // no gate check of their own -- confirmed via call-count instrumentation
    // this function alone accounted for ~90% of ALL executed bytecode
    // instructions on an H2 BNF-autocomplete-heavy workload. get_loader_id
    // below can only ever yield UserDefined(_) for a class that was assigned
    // that identity via a path that also calls register_defining_loader for
    // the same ClassId (see that function's invariant doc in
    // native-builtins/src/classloader.rs), so when NO user-defined loader has
    // ever defined ANY class in this process, class_manager.read() below is
    // guaranteed to return None regardless of referencing_class_id -- skip
    // straight to that outcome without taking the lock. Behavior-preserving:
    // identical to the pre-existing check, just short-circuited earlier.
    if !cratonvm_native_builtins::classloader::any_defining_loader_registered(&shared.loader_identity) {
        return None;
    }
    let loader = match shared
        .classes
        .class_manager
        .read()
        .get_loader_id(referencing_class_id)
    {
        Some(l @ cratonvm_types::ClassLoaderId::UserDefined(_)) => l,
        _ => return None,
    };
    if name.starts_with('[') {
        return None;
    }
    if is_global_resolution_namespace(name) {
        return loader_own_record_of_global_name(shared, loader, name);
    }
    // A class the loader defines itself is authoritative. In particular, a
    // forked Spring test loader can load its own copy after an earlier
    // parent-delegated initiating-resolution cache entry exists for the same
    // binary name. Consulting that cache first collapses the fork back onto
    // the application class and lets stale static invoke-cache entries mix
    // the two identities (for example MergedAnnotation$Adapt).
    if let Some(id) = shared
        .classes
        .class_manager
        .read()
        .class_defined_by_loader_exact(name, loader)
    {
        if crate::runtime::env_cache::dbg_loader_trace() && name.contains("RootReference") {
            eprintln!("[LOADER-TRACE] lookup_loader_initiated name={name} loader={loader:?} HIT class_defined_by_loader_exact id={id:?}");
        }
        return Some(id);
    }
    // The memo, or under `CRATONVM_RECORD_REPLACES_MEMO` the initiating
    // record (wave 49, lane L5).
    let cache_hit =
        crate::runtime::resolve::initiating_records::loader_initiated_answer(shared, loader, name);
    if crate::runtime::env_cache::dbg_loader_trace() && name.contains("RootReference") {
        eprintln!("[LOADER-TRACE] lookup_loader_initiated name={name} loader={loader:?} initiating_resolution_cache={cache_hit:?}");
    }
    cache_hit
}

/// Per-loader hard cap for initiating-loader memoization. Entries are an
/// optimization, not VM state: eviction re-drives `loadClass` and therefore
/// preserves JVMS resolution semantics.
pub(super) const INITIATING_RESOLUTION_CACHE_CAP: usize = 4096;

/// Generation of everything a cached *symbolic-reference resolution* depends on
/// other than the class-name → `ClassId` mapping.
///
/// Sibling of `cratonvm_classloading::class_definition_epoch`. Together the two
/// counters cover the complete input set of a resolved field/method reference,
/// so a cache whose validity condition is "this reference still resolves to
/// this answer" can be revalidated with two atomic loads instead of re-running
/// the resolution. That is what
/// [`crate::runtime::interpreter::field_access::FieldSiteCache`] does.
///
/// Bumped by:
///
/// * [`cache_loader_initiated`] below and the unload sweep in `memory::gc` —
///   the two writers of the per-loader initiating-resolution memo. Memoizing a
///   parent-delegated resolution changes what a loader answers **without**
///   touching `loaded_classes`, so `class_definition_epoch` does not see it.
/// * `vm_init::resolution_invalidate_adapter`, the hook classloading fires
///   whenever cached resolutions must be dropped. Two of its four sites —
///   `upgrade_synthetic_class` and `recompute_subclass_layouts` — change a
///   class's **field layout in place**, keeping both its `ClassId` and its name.
///   Neither the definition epoch nor the redefine latch moves for those, so
///   without this bump a resolved-field cache would keep serving a field index
///   from the pre-upgrade layout.
///
/// Every bumper is a per-VM event, so each also advances ITS VM's copy, the
/// resolution counter of the VM's [`cratonvm_classloading::StoreEpochs`]
/// slot ([`bump_resolution_epoch_in`], [`bump_resolution_epoch_for_domain`]).
/// A cache that can name its VM tags with that copy
/// (`site_cache::SiteCache::epochs_for`), so one VM's class loading no longer
/// retires another VM's entries (interpreter round i1 wave 20, lane L2). This
/// process counter moves on every VM's bumps and stays the tag of every
/// reader that cannot name a VM.
static RESOLUTION_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Current [`RESOLUTION_EPOCH`] — the process-wide counter, which moves on
/// every VM's bumps. One `Acquire` load. A reader that has its VM should use
/// [`resolution_epoch_in`].
#[inline]
pub fn resolution_epoch() -> u64 {
    RESOLUTION_EPOCH.load(std::sync::atomic::Ordering::Acquire)
}

/// `shared`'s own resolution epoch: moves only on its bumps (and on a
/// process-wide [`bump_resolution_epoch`]). One `Acquire` load.
#[inline]
pub fn resolution_epoch_in(shared: &SharedVm) -> u64 {
    cratonvm_classloading::store_epochs(shared.jit.class_layout_domain).resolution_epoch()
}

/// Record that cached resolutions may no longer be valid, in a VM this
/// caller cannot name: every VM's copy moves, and the process counter.
/// Tests only: every production bumper names its VM (or its class store).
#[cfg(test)]
pub(crate) fn bump_resolution_epoch() {
    cratonvm_classloading::bump_every_store_resolution_epoch();
    RESOLUTION_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Release);
}

/// Record that `shared`'s cached resolutions may no longer be valid: its
/// copy moves, and the process counter.
#[inline]
pub(crate) fn bump_resolution_epoch_in(shared: &SharedVm) {
    bump_resolution_epoch_for_domain(shared.jit.class_layout_domain);
}

/// [`bump_resolution_epoch_in`] for the VM whose class store has
/// `layout_domain` — for a hook that names the store, not the VM. Exact even
/// when no live VM owns the store: the slot is the store's.
#[inline]
pub(crate) fn bump_resolution_epoch_for_domain(layout_domain: u32) {
    cratonvm_classloading::store_epochs(layout_domain).bump_resolution_epoch();
    RESOLUTION_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Release);
}

pub(super) fn cache_loader_initiated(
    shared: &SharedVm,
    loader: cratonvm_types::ClassLoaderId,
    name: &str,
    class_id: ClassId,
) {
    // A row the memo already holds, with this answer, changes nothing any
    // cached resolution could have read: no epoch move. Bumping on every
    // drive retired every per-thread site entry on each re-ask of a name the
    // loader had answered before, and a site whose resolution drives
    // `loadClass` (an application class whose defining loader is registered)
    // then never kept a fill: it missed, drove, bumped, and its own fill was
    // dropped as stale, on every thread, for every execution.
    if shared
        .classes
        .initiating_resolution_cache
        .read()
        .get(&loader)
        .and_then(|m| m.get(name))
        .is_some_and(|held| *held == class_id)
    {
        return;
    }
    bump_resolution_epoch_in(shared);
    let mut caches = shared.classes.initiating_resolution_cache.write();
    let cache = caches.entry(loader).or_default();
    let key = cratonvm_types::intern_arc(name);
    if !cache.contains_key(key.as_ref()) && cache.len() >= INITIATING_RESOLUTION_CACHE_CAP {
        if let Some(victim) = cache.keys().next().cloned() {
            cache.remove(victim.as_ref());
        }
    }
    cache.insert(key, class_id);
}

/// Read only the exact definitions that belong to `referencing_class_id`'s
/// user loader. Unlike [`lookup_loader_initiated`], this intentionally never
/// consults the initiating-resolution cache.
///
/// A platform-parented isolated URL loader has no application-loader
/// delegation path. A cached result for such a loader can nevertheless point
/// at an earlier application copy (recorded before the loader had defined its
/// own framework class), collapsing a later `CONSTANT_Class` literal back to
/// that copy. Spring's `ModifiedClassPathClassLoader` then compares an
/// application `ConditionalOnMissingBean.class` against child metadata and
/// loses the annotation by Class identity. Exact definitions are safe; cache
/// entries are not for this loader shape.
#[inline]
pub(super) fn lookup_loader_defined_exact(
    shared: &SharedVm,
    referencing_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    if !cratonvm_native_builtins::classloader::any_defining_loader_registered(&shared.loader_identity)
        || name.starts_with('[')
        || name.starts_with("java/")
    {
        return None;
    }
    let cm = shared.classes.class_manager.read();
    let loader = match cm.get_loader_id(referencing_class_id) {
        Some(l @ cratonvm_types::ClassLoaderId::UserDefined(_)) => l,
        _ => return None,
    };
    // A JDK-global name: only a class this loader defined itself (one hash
    // probe; never `class_defined_by_loader_exact`'s cold full-store scan,
    // which every miss of such a name would pay). Interpreter round i1 wave
    // 27, lane L5: see `is_user_loader_global_name`.
    if is_global_resolution_namespace(name) {
        return cm.loaded_class_under_exact_key(name, loader);
    }
    cm.class_defined_by_loader_exact(name, loader)
}

/// What `loader` (a user-defined loader) itself answers for the JDK-global
/// name `name` ([`is_user_loader_global_name`]) without a `loadClass` call:
/// the class it defined under the name, then its initiating answer. Java names
/// use the uncapped dictionary; other global names retain their memo policy.
/// A site cache keeps the answer after that first resolution.
#[inline(never)]
fn loader_own_record_of_global_name(
    shared: &SharedVm,
    loader: cratonvm_types::ClassLoaderId,
    name: &str,
) -> Option<ClassId> {
    let own = shared
        .classes
        .class_manager
        .read()
        .loaded_class_under_exact_key(name, loader);
    own.or_else(|| {
        crate::runtime::resolve::initiating_records::loader_initiated_answer(shared, loader, name)
    })
}

pub(super) fn is_isolated_url_loader_definition(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
) -> bool {
    use cratonvm_native_api::NativeContext as _;
    let Some(loader) = cratonvm_native_builtins::classloader::defining_loader_for(
        &shared.loader_identity,
        referencing_class_id.as_u32(),
    ) else {
        return false;
    };
    let ctx = crate::vm::NativeContextImpl { shared, thread };
    cratonvm_native_builtins::classloader::url_classloader_isolated_from_app(&ctx, loader)
}

pub(super) fn isolated_loader_class_not_found(
    shared: &SharedVm,
    thread: &mut JvmThread,
    name: &str,
) -> Result<MethodCallFailed, MethodCallFailed> {
    use cratonvm_native_api::NativeContext as _;
    // gc-common w6-c (`handoff-w6c-rerunning-contexts-take-the-unwinding-arm`):
    // this runs `NoClassDefFoundError.<init>`, so it cannot be retried after a
    // collection, but it can take the allocators' unwinding arm instead of the
    // infallible one (which aborts on Generational/ZGC and takes G1's reserve).
    match crate::runtime::native_oom::catch_alloc_oom(|| {
        let mut ctx = crate::vm::NativeContextImpl { shared, thread };
        cratonvm_native_builtins::jboss_module_loader::alloc_single_message_exception(
            &mut ctx,
            "java/lang/NoClassDefFoundError",
            1,
            &name.replace('/', "."),
        )
    }) {
        Ok(Ok(exception)) => Ok(MethodCallFailed::ExceptionThrown(exception)),
        Ok(Err(e)) => Err(e),
        Err(oom) => Err(MethodCallFailed::InternalError(
            crate::error::VmError::Runtime(oom.into_runtime_error()),
        )),
    }
}

/// Resolve a `CONSTANT_Class` reference (`ldc X.class`, `new`/`anewarray`,
/// `checkcast`/`instanceof`) in a *loader-faithful* way.
///
/// `referencing_class_id` is the class whose constant pool holds the reference
/// (the executing frame's class). When the loader-aware gate
/// (`CRATONVM_LOADER_AWARE_RESOLUTION`) is on **and** that class was defined by a
/// user-defined loader, the reference is resolved through that loader as the
/// JVMS §5.4.3 *initiating* loader — by invoking its `loadClass` — so that two
/// isolating loaders which each define their own copy of `X` resolve `X` to
/// their *own* copy (HotSpot semantics) instead of collapsing to the first
/// (application) copy in CratonVM's flat global store.
///
/// Every other case — gate off, a built-in defining loader, a JDK/array name, a
/// missing loader object, or *any* failure of the loader path — falls through to
/// the legacy global [`SharedVm::load_class_concurrent`]. The loader path can
/// therefore only ever return a *more* correct answer, never a worse failure
/// than the pre-gate behavior.
///
/// Contract §9 `requested_by`: the global fallbacks below go through
/// [`SharedVm::load_class_concurrent_for`] with [`requesting_frame`], so a
/// class fabricated to satisfy a constant-pool reference records *which method*
/// referenced it. The loader-drive branches deliberately do not — a class
/// resolved by running a user `loadClass` was produced by that loader, not
/// fabricated, so there is no violation to attribute.
/// Does `stored` name a hidden class whose class-FILE name is `referenced`?
///
/// Every hidden-class mint site writes `format!("{original}/0x{id:x}")` off the
/// one `HIDDEN_CLASS_COUNTER` -- `native-builtins/src/classloader.rs`,
/// `lookup_define.rs` (twice), `lang_system.rs`, `unsafe_natives.rs` and
/// `unsafe_natives_ext.rs` -- so that suffix is the ONLY difference between the
/// name a hidden class is stored under and the name its own constant pool
/// carries.
///
/// The suffix has to be `/0x` followed by hex and nothing else. `A/0x1$Inner`
/// is a different class from `A`, and answering `true` for it would resolve an
/// unrelated reference to the hidden class -- a wrong answer, where the bug this
/// predicate fixes is only a missing one.
///
/// One copy of the rule (JIT round 21 wave 21, item 4 of
/// `r21-review-w21-findings-20261019.md`): this delegates to
/// `cratonvm_classloading::hidden_stored_name_is_class_file_name`, the
/// fallback of the class manager's recorded-name rule
/// (`cratonvm_classloading::hidden_self_name_rule`).
#[inline]
pub(crate) fn hidden_stored_name_is_self(stored: &str, referenced: &str) -> bool {
    cratonvm_classloading::hidden_stored_name_is_class_file_name(stored, referenced)
}

/// Is `referenced` -- a constant-pool class name -- the referencing class's own
/// name?
///
/// Two dispatch doors ask this to recognise a SELF-call and answer it from the
/// frame's own `ClassId` instead of a loader-blind name lookup
/// (`dispatch_static`'s `self_class_id`, `invoke`'s `self_match`). Both used
/// exact string equality, which a hidden class can never satisfy: it is stored
/// under `"<class-file name>/0x<counter>"` and its constant pool carries the
/// class-file name. The lookup they fall through to cannot find it either --
/// a hidden class is in no loader's namespace -- so a hidden class calling its
/// own static method raised `NoClassDefFoundError` under a name that named the
/// class doing the calling.
///
/// Takes the two facts rather than the `Class` so it stays a pure function the
/// gate below can drive, and so neither caller has to take a second lock: both
/// already hold the class.
pub(crate) fn is_self_class_reference(stored: &str, hidden: bool, referenced: &str) -> bool {
    stored == referenced || (hidden && hidden_stored_name_is_self(stored, referenced))
}

/// [`is_self_class_reference`] for class `class_id` under a held
/// class-manager guard: the exact name, or the hidden class's RECORDED
/// class-file name (`ClassManager::is_hidden_self_name`, the one rule; JIT
/// round 21 wave 21, items 3-4 of `r21-review-w21-findings-20261019.md`), so
/// a hidden class whose stored name a mint site spelled otherwise still
/// recognises itself. Takes the class the caller already holds, so there
/// is no second fetch, and asks the class store's one answer
/// (`ClassStore::hidden_self_name`; wave 22, lane hidden-resid, item 4 of
/// `r21-review-w22-findings-20261019.md`): no lookup at all for an ordinary
/// class past the string compare.
#[inline]
pub(crate) fn is_self_class_reference_in(
    cm: &crate::classloading::ClassManager,
    class: &crate::classloading::Class,
    referenced: &str,
) -> bool {
    &*class.name == referenced || cm.class_store.hidden_self_name(class, referenced)
}

/// The referencing class IS the class being referenced, named the way its own
/// class file names it.
///
/// JEP 371: a hidden class is deliberately not registered in any loader's
/// namespace -- `set_class_hidden` is what makes `find_class_by_name` and
/// `Class.forName` unable to see it -- but its constant pool still carries its
/// class-FILE name, and `this_class` and every self-naming `Fieldref` /
/// `Methodref` resolve through that name. A name lookup therefore cannot answer
/// a hidden class's reference to itself, and before this arm existed it did not:
/// it raised `NoClassDefFoundError` under the UNMANGLED name.
///
/// `MethodHandleProxies`' generated proxy is the worked example, and is why
/// `java/lang/System$1` could not be retired (lane 2 §4). Its `<clinit>` is
/// `ldc <the interface>; putstatic <ITSELF>.interfaceType`, and its `<init>`
/// does `invokestatic <ITSELF>.ensureOriginalLookup` and compares
/// `ldc <ITSELF>` against `lookup.lookupClass()` -- three self-references, two
/// of them reached before any of the interface's own methods run.
fn hidden_self_reference(
    shared: &SharedVm,
    referencing_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    // This function is on EVERY constant-pool class resolution, so the
    // not-taken path has to cost one load and a branch: this VM's lock-free
    // hidden-class bitmap (`HiddenClassBits`) screens out every ordinary
    // referencing class before anything takes a lock. It replaced the
    // process-wide `HIDDEN_CLASS_COUNTER`, which every VM's first hidden
    // class (a lambda) armed for every class of every VM (JIT round 21 wave
    // 20, lane hidden-self-resolve).
    if !hidden_class_may_refer_to_itself(shared, referencing_class_id) {
        return None;
    }
    // The class manager's rule, which reads the class-file name recorded at
    // define time (wave 21, items 3-4 of `r21-review-w21-findings-20261019.md`).
    let cm = shared.classes.class_manager.read();
    cm.is_hidden_self_name(referencing_class_id, name)
        .then_some(referencing_class_id)
}

/// [`hidden_self_reference`] as a predicate, under a class-manager guard the
/// caller already holds: `referencing_class_id` is a hidden class and `name`
/// is its own class-FILE name (JIT round 21 wave 14, lane hidden-static;
/// `docs/internal/fixed-bugs/r21-hidden-self-call-by-name-FIXED-20261019.md`).
///
/// The compile doors and the compiled code's by-name routes ask it, so that a
/// hidden class's reference to itself is never answered by whatever class
/// the NAME finds. While nothing else carries the name that is nothing (the
/// call went to the by-name tail, which LOADED the class file of that name
/// from the class path); once an ordinary class of that name is loaded, it is
/// a third class with its own statics. `R21Fz3HiddenAccess1` arm `HN`, by the
/// count (unmeasured): the OSR-compiled `loop` called its own `selfCtor` through that tail, which ran
/// the class-path copy's `selfCtor`, and 437 constructions counted on the
/// copy's `sCount` instead of the hidden class's.
///
/// A class lookup under the held guard. Callers without the guard ask the
/// VM's lock-free [`hidden_class_may_refer_to_itself`] first and take the
/// lock only for a hidden class.
pub(crate) fn hidden_self_reference_in(
    cm: &crate::classloading::ClassManager,
    referencing_class_id: ClassId,
    name: &str,
) -> bool {
    cm.is_hidden_self_name(referencing_class_id, name)
}

/// The hidden class `referencing_class_id` itself when `name` -- a name from
/// its own constant pool -- is its class-FILE name and
/// `CRATONVM_HIDDEN_SELF_CP_BY_ID` is on, else `None` (JIT round 21 wave 20,
/// lane hidden-self-resolve;
/// `docs/known-issues/jit/r21-hidden-self-cp-resolves-to-same-named-class-20261019.md`).
///
/// The owner a member reference's checks and selections are judged against
/// (`field_access::owner_in_referencing_namespace`,
/// `resolve_method_metadata`'s owner, the threadless field peek, the
/// invokespecial / private-invokevirtual selection doors, the JIT's site
/// owners): those asked the loader for the NAME, which answers nothing until
/// an ordinary class of that name is loaded and that class from then on.
/// `R21Fz5HiddenOsr1`: after `plainArm()` loaded the class-path
/// `R21Fz5HoPayload`, the hidden copies' own `invokestatic step` and
/// `invokevirtual priv` were judged against the ORDINARY class's private
/// methods: `IllegalAccessError` in `--jdk-only`, `--compatible` and
/// `--nojit`.
///
/// Under the caller's class-manager guard; an ordinary class pays one load
/// of the lock-free bitmap (`ClassManager::hidden_self_cp_answer`).
#[inline]
pub(crate) fn hidden_self_cp_owner_in(
    cm: &crate::classloading::ClassManager,
    referencing_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    cm.hidden_self_cp_answer(name, referencing_class_id)
}

/// [`hidden_self_cp_owner_in`] for a caller holding no guard: the VM's
/// lock-free bitmap first, the class-manager read only for a hidden class.
#[inline]
pub(crate) fn hidden_self_cp_owner(
    shared: &SharedVm,
    referencing_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    if !hidden_class_may_refer_to_itself(shared, referencing_class_id) {
        return None;
    }
    let cm = shared.classes.class_manager.read_recursive();
    hidden_self_cp_owner_in(&cm, referencing_class_id, name)
}

/// The lock-free prefilter of [`hidden_self_reference_in`] (JIT round 21 wave
/// 14, lane hidden-static; review item 6 of
/// `r21-review-w15-native-findings-FIXED-20261020.md`): `false` when
/// `referencing_class_id` is known NOT hidden from this VM's
/// [`crate::vm::realms::ClassRealm::hidden_classes`] bitmap (one load, no
/// lock, no process global), `true` when it is hidden, or past the bitmap's
/// capacity once a hidden class lives there (wave 21; the caller then asks
/// under the lock).
#[inline]
pub(crate) fn hidden_class_may_refer_to_itself(
    shared: &SharedVm,
    referencing_class_id: ClassId,
) -> bool {
    hidden_bit_admits(shared.classes.hidden_classes.contains(referencing_class_id))
}

/// The decision of [`hidden_class_may_refer_to_itself`] from the bitmap's
/// answer: only a known-ordinary class is screened out.
#[inline]
pub(crate) fn hidden_bit_admits(bit: Option<bool>) -> bool {
    bit != Some(false)
}

/// The pure half of [`hidden_self_reference_in`] with no recorded class-file
/// name: the facts of the referencing class (`stored` name, `hidden` flag)
/// against the referenced constant-pool name. Unlike
/// [`is_self_class_reference`] an ORDINARY class's own name is not an answer
/// here: a by-name lookup already finds it. Since wave 21 the production
/// doors ask `ClassManager::is_hidden_self_name`, which reads the recorded
/// name first; this stays as the rule's fallback witness.
#[cfg(test)]
pub(crate) fn hidden_self_name_matches(stored: &str, hidden: bool, referenced: &str) -> bool {
    cratonvm_classloading::hidden_self_name_rule(stored, hidden, None, referenced)
}

/// `CRATONVM_JIT_HIDDEN_SELF_BY_ID` (JIT round 21 wave 14, lane hidden-static;
/// correctness, default ON, `=0` restores the by-name answers): a hidden
/// class's reference to its own class-file name ([`hidden_self_reference_in`])
/// is answered by the class's id on the compiled routes -- the static tail and
/// the bail of `jit_invoke_dispatch`, the compiled-callee probes by name, and
/// [`class_resolved_without_loading`] (which defers it) -- as the interpreter
/// answers it. Read only once a site has been found to be such a reference.
pub(crate) fn jit_hidden_self_by_id_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_HIDDEN_SELF_BY_ID")
}

/// The `CONSTANT_Class` resolution door ([`resolve_class_loader_aware_unrecorded`]),
/// whose success records a JDK class the referencing class's loader initiated
/// for `Instrumentation.getInitiatedClasses` -- one relaxed load while no
/// start-up agent armed the record (interpreter round i1 wave 46, lane L5;
/// `runtime::resolve::initiating_records::note_class_resolution`).
pub(crate) fn resolve_class_loader_aware(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
) -> Result<ClassId, MethodCallFailed> {
    let resolved =
        resolve_class_loader_aware_unrecorded(shared, thread, referencing_class_id, name)?;
    crate::runtime::resolve::initiating_records::note_class_resolution(
        shared,
        referencing_class_id,
        resolved,
    );
    Ok(resolved)
}

/// `CRATONVM_CLASS_SITE_ISOLATED_STICKY` (default on; `0` / `false` / `off` /
/// `no` turns it off). Round 15 wave 5, lane classsite
/// (`r15w5-fieldowner2-isolated-loader-class-sites-reask-the-loader-CLOSED-20260930.md`,
/// items 1-2): an isolated URL loader's `CONSTANT_Class` entry keeps its first
/// resolution ([`resolve_class_entry_loader_aware`]), and the per-thread
/// `new` / cast / `anewarray` site tables admit it
/// (`opcodes::site_fill_admitted`), so the loader's `loadClass` is asked once
/// per entry, not once per execution. The class twin of the field sites'
/// `CRATONVM_FIELD_SITE_ISOLATED_STICKY`. Read on a site-table miss of a class
/// that has a defining loader only.
pub(super) fn isolated_class_site_sticky_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_CLASS_SITE_ISOLATED_STICKY")
}

/// Does `referencing_class_id`'s `CONSTANT_Class` entries' resolution get a
/// durable per-entry record ([`resolve_class_entry_loader_aware`])? Only for
/// an isolated URL loader's class: its resolver ignores the initiating memo
/// (`lookup_loader_defined_exact`), so nothing else keeps a delegated answer.
/// Every other class's answer is re-derived from the loader's own records
/// (its definitions and memo), which already answer after the first drive.
/// The lock-free side-table probe first: a class with no defining loader
/// pays one probe and reads no switch. `pub(crate)` for `multianewarray`'s
/// entry walk, the invoke owners and the JIT's invokespecial bridge (round 15
/// wave 5, lane classsite2).
pub(crate) fn class_entry_record_applies(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
) -> bool {
    let isolated = cratonvm_native_builtins::classloader::defining_loader_for(
        &shared.loader_identity,
        referencing_class_id.as_u32(),
    )
    .is_some()
        && isolated_class_site_sticky_enabled()
        && is_isolated_url_loader_definition(shared, thread, referencing_class_id);
    // `CRATONVM_CLASS_ENTRY_RECORD_ALL` (interpreter round i6 wave 118, lane
    // L7, default off; stage 1 of
    // `i1-L2-proposal-per-class-resolved-constant-pool`): every class with
    // real bytes and a non-bootstrap defining loader keeps the record too
    // ([`class_entry_record_all_applies`]). Off: one per-VM bool load.
    isolated
        || (shared.config.class_entry_record_all
            && isolated_class_site_sticky_enabled()
            && class_entry_record_all_applies(shared, referencing_class_id))
}

/// [`class_entry_record_applies`]'s widened arm
/// (`CRATONVM_CLASS_ENTRY_RECORD_ALL`, interpreter round i6 wave 118, lane
/// L7): `referencing_class_id` is stored, was defined by a loader other than
/// the bootstrap loader (a bootstrap class's answers are the global
/// namespace, which the per-thread sites already keep), and has real bytes. A
/// compatibility stub keeps no record: it can be upgraded in place to real
/// bytes under the same id with a new constant pool, and a row keyed by its
/// old pool's index would then answer the new pool's entry.
fn class_entry_record_all_applies(shared: &SharedVm, referencing_class_id: ClassId) -> bool {
    let cm = shared.classes.class_manager.read_recursive();
    let loader = cm.get_loader_id(referencing_class_id);
    cm.get_class(referencing_class_id)
        .is_some_and(|class| class_entry_record_all_keys(loader, &class.origin))
}

/// [`class_entry_record_all_applies`]'s rule on the two facts it reads
/// (interpreter round i6 wave 119, lane L7; pure, for the unit tests): a
/// class keys rows when a non-bootstrap loader defined it and real bytes back
/// it. Row retirement then rests on the record's own sweeps: a JVMTI
/// redefinition of the class and an in-place upgrade of it both fire
/// `ResolutionInvalidation::ConstantPoolReplaced`, which drops the rows KEYED
/// by it (`ResolutionCache::invalidate_class`); an upgrade does not move the
/// redefinition count a fill is stamped with, so a fill racing it could land
/// after the sweep, and that is why a stub (the only class an upgrade
/// replaces) keys no row at all. Unloading drops a row by key or by answer.
fn class_entry_record_all_keys(
    loader: Option<cratonvm_types::ClassLoaderId>,
    origin: &cratonvm_classloading::ClassOrigin,
) -> bool {
    !matches!(loader, None | Some(cratonvm_types::ClassLoaderId::Bootstrap))
        && origin.has_real_bytes()
}

/// [`class_entry_answer_recordable`]'s rule on the answer's origin
/// (interpreter round i6 wave 119, lane L7; pure): a compatibility stub is
/// never a durable answer. An in-place upgrade keeps the stub's id, so a row
/// naming it would answer the upgraded class (harmless), but a real class of
/// the same name defined under ANOTHER id would not be seen.
fn class_entry_answer_kept(origin: &cratonvm_classloading::ClassOrigin) -> bool {
    !origin.is_compatibility_stub()
}

/// Whether a resolution to `resolved` may be recorded durably under
/// `CRATONVM_CLASS_ENTRY_RECORD_ALL` (interpreter round i6 wave 118, lane
/// L7): not when `resolved` is a compatibility stub (`--compatible` only),
/// which a later definition of the real class can replace in the name's
/// answer; the entry then resolves again, as it does with the switch off.
/// Always `true` with the switch off (the isolated-loader record, unchanged).
fn class_entry_answer_recordable(shared: &SharedVm, resolved: ClassId) -> bool {
    !shared.config.class_entry_record_all
        || shared
            .classes
            .class_manager
            .read_recursive()
            .get_class(resolved)
            .is_some_and(|class| class_entry_answer_kept(&class.origin))
}

/// Interpreter round i6 wave 119, lane L7 (`CRATONVM_CLASS_ENTRY_RECORD_ALL`):
/// which classes key a durable class-entry row, and which answers are kept.
#[cfg(test)]
mod i119_l7_class_entry_record_all_tests {
    use super::{class_entry_answer_kept, class_entry_record_all_keys};
    use cratonvm_classloading::ClassOrigin;
    use cratonvm_types::ClassLoaderId;
    use std::sync::Arc;

    fn app() -> ClassOrigin {
        ClassOrigin::ApplicationClassPath {
            source: Arc::from("file:/app/"),
        }
    }

    fn stub() -> ClassOrigin {
        ClassOrigin::CompatibilityStub {
            reason: Arc::from("test"),
        }
    }

    #[test]
    fn only_a_real_class_of_a_non_bootstrap_loader_keys_rows() {
        assert!(class_entry_record_all_keys(Some(ClassLoaderId::Application), &app()));
        assert!(!class_entry_record_all_keys(Some(ClassLoaderId::Bootstrap), &app()));
        assert!(!class_entry_record_all_keys(None, &app()));
        // A stub can be upgraded in place under its id with a new pool.
        assert!(!class_entry_record_all_keys(Some(ClassLoaderId::Application), &stub()));
        assert!(!class_entry_record_all_keys(
            Some(ClassLoaderId::Application),
            &ClassOrigin::VmArray
        ));
    }

    #[test]
    fn a_stub_answer_is_not_kept() {
        assert!(class_entry_answer_kept(&app()));
        assert!(class_entry_answer_kept(&ClassOrigin::VmArray));
        assert!(!class_entry_answer_kept(&stub()));
    }
}

/// [`resolve_class_loader_aware`] for the `CONSTANT_Class` entry `cp_index`
/// of `referencing_class_id`'s pool, which names `name` (JVMS §5.4.3: an
/// entry is resolved once, and every instruction naming it shares the
/// resolution). For an isolated URL loader's class
/// ([`class_entry_record_applies`]) the entry's recorded resolution answers
/// first, and a fresh one is recorded (first wins, `fill_as_of` =
/// `ResolutionCache::fill_snapshot` taken before the caller read `name` from
/// the pool); every other class resolves exactly as
/// [`resolve_class_loader_aware`] does. The caller keeps its JVMS §5.4.3
/// failure record and §5.4.4 access check around this call, as around
/// `resolve_class_loader_aware`: a recorded failure is asked for first, so a
/// failed entry never reaches the record. Round 15 wave 5, lane classsite.
///
/// GC-safety: as [`resolve_class_loader_aware`] (a miss can run `loadClass`).
pub(crate) fn resolve_class_entry_loader_aware(
    shared: &SharedVm,
    thread: &mut JvmThread,
    (referencing_class_id, cp_index): (ClassId, u16),
    name: &str,
    fill_as_of: u64,
) -> Result<ClassId, MethodCallFailed> {
    if !class_entry_record_applies(shared, thread, referencing_class_id) {
        return resolve_class_loader_aware(shared, thread, referencing_class_id, name);
    }
    resolve_class_entry_recorded(
        shared,
        thread,
        (referencing_class_id, cp_index),
        name,
        fill_as_of,
    )
}

/// [`resolve_class_entry_loader_aware`] past its predicate, for a caller that
/// has itself found [`class_entry_record_applies`] true (round 15 wave 5,
/// lane classsite2: the invoke owners and the JIT's invokespecial bridge ask
/// the predicate first, to skip a pool read for every other class, and must
/// not pay its loader walk twice; review finding C3).
pub(crate) fn resolve_class_entry_recorded(
    shared: &SharedVm,
    thread: &mut JvmThread,
    (referencing_class_id, cp_index): (ClassId, u16),
    name: &str,
    fill_as_of: u64,
) -> Result<ClassId, MethodCallFailed> {
    let resolver = MemberResolver::new(shared);
    if let Some(id) = resolver.probe_class_entry(resolver.scope(referencing_class_id), cp_index) {
        return Ok(id);
    }
    let resolved = resolve_class_loader_aware(shared, thread, referencing_class_id, name)?;
    // Wave 118 (`CRATONVM_CLASS_ENTRY_RECORD_ALL`): a stub answer is not kept.
    if !class_entry_answer_recordable(shared, resolved) {
        return Ok(resolved);
    }
    // A racing resolver's earlier record is the entry's answer.
    Ok(resolver
        .record_class_entry_as_of(
            resolver.scope(referencing_class_id),
            cp_index,
            resolved,
            fill_as_of,
        )
        .unwrap_or(resolved))
}

/// The recorded resolution of `referencing_class_id`'s `CONSTANT_Class`
/// entry `cp_index` ([`resolve_class_entry_loader_aware`]), for the per-thread
/// site tables' admission (`opcodes::site_fill_admitted`). `None` with the
/// switch off. One read guard; asked at a fill of a loader-namespaced
/// class's site only. `pub(crate)` for the compiled class sites' twin
/// (`r15w5-classsite-class-entry-shared-by-every-instruction-patch-FIXED-20260930.md`,
/// part C), which must answer what the interpreter answers.
pub(crate) fn recorded_class_entry(
    shared: &SharedVm,
    referencing_class_id: ClassId,
    cp_index: u16,
) -> Option<ClassId> {
    if !isolated_class_site_sticky_enabled() {
        return None;
    }
    let resolver = MemberResolver::new(shared);
    resolver.probe_class_entry(resolver.scope(referencing_class_id), cp_index)
}

/// `CRATONVM_ISOLATED_LOADER_DICTIONARY` (default OFF; any non-empty value
/// but `0` / `false` / `off` / `no` turns it on). Round 15 wave 5, lane classsite:
/// proposal FO5-2 of `jit-r15w5-fieldowner2-proposals-RETIRED-20261003.md` (item 3 of the
/// class-sites page). See [`isolated_loader_dictionary_answer`]. Off until the
/// Spring Boot `ModifiedClassPathClassLoader` census that motivated the
/// isolated loaders' exact-definitions rule has run with it on.
fn isolated_loader_dictionary_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_ISOLATED_LOADER_DICTIONARY")
}

/// What an isolated URL loader's DICTIONARY holds for `name`: the class it
/// initiated under that name when the VM asked it (`--jdk-only`;
/// `resolve::initiating_records`, written by the checked door's successful
/// drive and by `Class.forName` with the loader, first answer kept, and a
/// later definition of the name by the loader refused, JVMS §5.3.5). HotSpot
/// answers every later resolution of the name through that loader from its
/// dictionary without a `loadClass` call, from any class the loader defined;
/// the initiating MEMO, which such a loader ignores, also holds answers
/// HotSpot never records, but this table does not. `None` with the switch
/// off, under `--compatible` (nothing is recorded there), and for a class
/// whose class-manager loader is not user-defined. Asked after the loader's
/// own definition missed, on the resolution miss path.
pub(super) fn isolated_loader_dictionary_answer(
    shared: &SharedVm,
    referencing_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    if !shared.config.is_jdk_only() || !isolated_loader_dictionary_enabled() {
        return None;
    }
    let loader = match shared
        .classes
        .class_manager
        .read()
        .get_loader_id(referencing_class_id)
    {
        Some(l @ cratonvm_types::ClassLoaderId::UserDefined(_)) => l,
        _ => return None,
    };
    crate::runtime::resolve::initiating_records::initiated_class(shared, loader, name)
}

/// `CRATONVM_ISOLATED_LOADER_SUPERTYPES` (default on; `0` / `false` / `off` /
/// `no` turns it off): an isolated URL loader's class answers its own DIRECT
/// supertype's name with the class it was linked against. Linking it resolved
/// that name through its defining loader (JVMS §5.3.5 step 3), which made the
/// loader an initiating loader of it, so HotSpot answers every later entry
/// naming it from the loader's dictionary, with no second `loadClass`
/// ([`class_resolved_without_loading`] answers the compile doors the same way).
/// Both modes: a supertype is a fact of the class, not a record. Round 15 wave
/// 6, lane loader6 (`r15w5-classsite2-review-findings-FIXED-20261001.md` R1).
pub(super) fn isolated_direct_supertype_answer(
    shared: &SharedVm,
    referencing_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ISOLATED_LOADER_SUPERTYPES") {
        return None;
    }
    let cm = shared.classes.class_manager.read();
    direct_supertype_named(&cm, referencing_class_id, name)
}

/// `CRATONVM_LOADER_DIRECT_SUPERTYPES` (default on; `0` / `false` / `off` /
/// `no` turns it off): [`isolated_direct_supertype_answer`] for a NON-isolated
/// user loader's class, asked after [`lookup_loader_initiated`] (own
/// definitions, then the initiating memo) declined. The define path's
/// supertype preload asks the loader but fills no memo, so without this the
/// first entry naming a delegated direct supertype drove `loadClass` a second
/// time (HotSpot: one ask; the link made the loader an initiating loader).
/// JDK-global names keep their global answer. A separate switch from the
/// isolated one because this population is every webapp / fat-jar loader.
/// Round 15 wave 6, lane patches6 (`r15w6-loader6-review-findings-CLOSED-20260930.md`
/// L1).
pub(super) fn loader_direct_supertype_answer(
    shared: &SharedVm,
    referencing_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    if is_global_resolution_namespace(name)
        || !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_LOADER_DIRECT_SUPERTYPES")
    {
        return None;
    }
    let cm = shared.classes.class_manager.read();
    direct_supertype_named(&cm, referencing_class_id, name)
}

fn resolve_class_loader_aware_unrecorded(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
) -> Result<ClassId, MethodCallFailed> {
    // (0-pre-pre) A hidden class referring to ITSELF. Answered before the
    //     transform hook and before every load below, because there is nothing
    //     to load: the class is already defined, and the only reason a lookup
    //     fails is that it is stored under a name its own bytecode never
    //     mentions. See `hidden_self_reference`.
    if let Some(self_id) = hidden_self_reference(shared, referencing_class_id, name) {
        return Ok(self_id);
    }
    // (0-pre) `java.lang.instrument` transform-on-load. Every branch below ends
    //     in a `load_class_concurrent*` or a user `loadClass`, and neither can
    //     run a Java `ClassFileTransformer` — the first holds the class-manager
    //     write lock while it defines, the second is the loader's own business.
    //     This is the last point on the constant-pool resolution path where a
    //     Java thread is in hand and no class-manager lock is held, so it is
    //     where the chain gets its shot at the bytes. The hook stages the
    //     rewritten class file; whichever branch below actually defines the
    //     class picks it up. A VM with no registered transformer pays one
    //     relaxed atomic load and a not-taken branch.
    if crate::runtime::instrument::load_hook_armed(shared) {
        crate::runtime::instrument::pre_transform_for_load(shared, thread, name, 0);
    }
    // (0) An array reference resolves its COMPONENT type with the same
    //     initiating loader as the array reference itself (JVMS 5.3.3: the
    //     array class is synthesised from the resolved component; no class
    //     file is consulted). Every loader-faithful branch below keys on
    //     `name` itself, and `drive_defining_loader_load` declines `[` names
    //     outright -- so an array name fell straight through to the flat
    //     global `load_class_concurrent`, which fabricated a synthetic stub
    //     for a component that exists only behind a custom loader AND
    //     registered that stub globally under `Application`, permanently
    //     poisoning the component for the real loader (a stub has no `Code`,
    //     so the first real `new` of it fails with
    //     "<init> ... has no Code attribute").
    //     Found on the Keycloak 26.6.1 / Quarkus fast-jar boot: an
    //     `ldc` of `[Lorg/jboss/threads/EnhancedQueueExecutor$TaskNode;`
    //     from `EnhancedQueueExecutor.<clinit>`, whose jar is reachable only
    //     through Quarkus's `RunnerClassLoader`.
    //     Strictly additive: it only pre-resolves a component whose global
    //     answer was going to be fabricated anyway.
    //
    //     Gated on THIS referencing class having a defining loader, not on the
    //     process-wide "some loader exists" latch. The whole pre-pass exists to
    //     feed `drive_defining_loader_load`, which bails immediately without a
    //     `defining_loader_for(referencing_class_id)` — so for an app- or
    //     bootstrap-defined referencing class the `would_fabricate_synthetic_stub`
    //     probe (a class-manager read lock plus a name lookup) could only ever
    //     lead to a no-op. Same predicate, evaluated per class instead of per
    //     process; see `class_may_have_defining_loader` for why the difference
    //     is worth 1.8x on a class-parsing workload.
    //
    //     Not under `--jdk-only`: no stub is fabricated there, and (0b) asks
    //     a user loader for the element itself, through the checked door; a
    //     pre-pass ask would be the resolution's second request, its throw
    //     dropped (interpreter round i1 wave 41, lane L5).
    if let Some(component) =
        array_component_class_name(name).filter(|_| !shared.classes.loader_throws_on)
    {
        if cratonvm_native_builtins::classloader::defining_loader_for(
            &shared.loader_identity,
            referencing_class_id.as_u32(),
        )
        .is_some()
            && shared
                .classes
                .class_manager
                .read()
                .would_fabricate_synthetic_stub(component)
        {
            let _ = drive_defining_loader_load(shared, thread, referencing_class_id, component);
        }
    }
    // (0b) JVMS §5.3.3: an array class is defined by its COMPONENT's defining
    //      loader, not the bootstrap loader. Every loader-faithful branch below
    //      excludes `[` names, so without this an `X[]` resolved from one user
    //      loader was handed to the next loader that asked — type confusion,
    //      since array-class identity is what `checkcast` and the verifier
    //      consult.
    //
    //      Strictly additive: the helper answers `None` for a non-array name
    //      and for a built-in referencing loader.
    //
    //      `_checked`: under `--jdk-only` a user loader's refusal of the
    //      element fails the array (wave 41, lane L5).
    if name.starts_with('[') {
        if let Some(id) = crate::runtime::interpreter::resolve_array_class_loader_aware_checked(
            shared,
            thread,
            referencing_class_id,
            name,
        )? {
            return Ok(id);
        }
    }
    // (1) Gate / built-in / JDK-name fast paths + already-known loader-local
    //     answer — none of which need a re-entrant call.
    let direct_loader = shared
        .classes
        .class_manager
        .read()
        .get_loader_id(referencing_class_id);
    let direct_user_loader = matches!(
        direct_loader,
        Some(cratonvm_types::ClassLoaderId::UserDefined(_))
    );
    // A class-manager loader id can temporarily disagree with the defining
    // loader side table for forked loaders, so retain that authoritative
    // fallback.  Application/bootstrap classes have neither and must not pay
    // for initiating-resolution map probes merely because another loader was
    // registered elsewhere in the process.
    let has_registered_defining_loader =
        cratonvm_native_builtins::classloader::defining_loader_for(
            &shared.loader_identity,
            referencing_class_id.as_u32(),
        )
        .is_some();
    let has_loader_namespace = direct_user_loader || has_registered_defining_loader;
    let isolated_url_definition = if has_registered_defining_loader {
        is_isolated_url_loader_definition(shared, thread, referencing_class_id)
    } else {
        false
    };
    let known = if has_loader_namespace {
        if isolated_url_definition {
            // Then the loader's dictionary (FO5-2, round 15 wave 5, lane
            // classsite; opt-in): a name it already initiated is answered
            // without a second `loadClass`, from any of its classes.
            lookup_loader_defined_exact(shared, referencing_class_id, name)
                .or_else(|| isolated_loader_dictionary_answer(shared, referencing_class_id, name))
                // Its own direct supertypes (round 15 wave 6, lane loader6).
                .or_else(|| isolated_direct_supertype_answer(shared, referencing_class_id, name))
        } else {
            lookup_loader_initiated(shared, referencing_class_id, name)
                // Its own direct supertypes (round 15 wave 6, lane patches6).
                .or_else(|| loader_direct_supertype_answer(shared, referencing_class_id, name))
        }
    } else {
        None
    };
    if let Some(id) = known {
        return Ok(id);
    }
    // `lookup_loader_initiated` returned `None`, so either this is a legacy case
    // (gate off / built-in loader / JDK or array name) or the loader is
    // user-defined but has not yet resolved this name. Only the latter takes the
    // cold loadClass path below; everything else resolves globally.
    let dbg_trace = crate::runtime::env_cache::dbg_loader_trace()
        && (name.contains("EnvironmentPostProcessorsFactory")
            || name.contains("CloudFoundryVcapEnvironmentPostProcessor")
            || name.contains("ManagementContextAutoConfiguration")
            || name.contains("ManagementPortType")
            || name.contains("ChildManagementContextInitializerAotTests")
            || name.contains("SearchStrategy")
            || name.contains("MergedAnnotations")
            || name.contains("ConditionalOnMissingBean")
            || name.contains("ConditionalOnBean")
            || name.contains("OnBeanCondition")
            || name.contains("DataSourceConfiguration")
            || name.contains("RootReference")
            || name.contains("MVMap")
            || name.contains("org/h2/Driver")
            || name.contains("AotTestContextInitializers")
            || name.contains("AotMergedContextConfiguration")
            || name.contains("DefaultCacheAwareContextLoaderDelegate")
            || name.contains("SecurityFilterAutoConfigurationEarlyInitializationTests")
            || name.contains("PathRequestTests")
            || name.contains("ManagementWebSecurityAutoConfigurationTests")
            || name.contains("WebSocketMessaging")
            || name.contains("Jackson2WebSocketMessageConverterConfiguration"));
    if dbg_trace {
        let cm = shared.classes.class_manager.read();
        let ref_name = cm
            .get_class(referencing_class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        let ref_loader = cm.get_loader_id(referencing_class_id);
        eprintln!(
            "[LOADER-TRACE] resolve name={name} referencing_class_id={referencing_class_id:?} referencing_class={ref_name} referencing_loader={ref_loader:?}"
        );
        if name.contains("RootReference")
            && matches!(ref_loader, Some(cratonvm_types::ClassLoaderId::Application))
        {
            drop(cm);
            eprintln!(
                "[RCLA-STACK] full Java stack at resolve_class_loader_aware for name={name}:"
            );
            for (i, f) in thread.frames.iter().enumerate().rev() {
                eprintln!(
                    "[RCLA-STACK]   [{i}] {}.{}{} pc={}",
                    f.class_name(),
                    f.method_name(),
                    f.method_descriptor(),
                    f.pc
                );
            }
        }
    }
    // An isolated URLClassLoader (including Spring Boot's
    // ModifiedClassPathClassLoader) has the same JVMS initiating-loader
    // requirement as the existing Groovy/forked-loader cases: class literals
    // and other CONSTANT_Class consumers inside one of its private framework
    // copies must resolve through that exact loader, never through the flat
    // application store. Otherwise `OnBeanCondition`'s
    // `ConditionalOnMissingBean.class` literal is app-defined while ASM
    // metadata is child-defined, and MergedAnnotations' Class-keyed lookup
    // misses an annotation that its name-keyed lookup just found.
    //
    // `has_loader_namespace` is tested FIRST and the loader-faithfulness
    // predicate last: every conjunct is side-effect-free, and for an app- or
    // bootstrap-defined referencing class — nearly every resolution — the
    // predicate's second `defining_loader_for` side-table probe could only
    // feed a `false`.
    let user_loader = if has_loader_namespace
        && !name.starts_with('[')
        && !is_global_resolution_namespace(name)
        && (isolated_url_definition
            || should_use_loader_initiated_resolution(shared, referencing_class_id))
    {
        let direct_loader_id = shared
            .classes
            .class_manager
            .read()
            .get_loader_id(referencing_class_id);
        match direct_loader_id {
            Some(l @ cratonvm_types::ClassLoaderId::UserDefined(_)) => Some(l),
            // `should_use_loader_initiated_resolution` just confirmed (via the
            // `defining_loader_for` side table) that `referencing_class_id` WAS
            // defined by a recognized user loader (Groovy or
            // CompileWithForkedClassLoader) -- but `class_manager`'s own
            // `loader_id` field for that same ClassId can disagree (return
            // `Application`/`None`), silently discarding the gate's answer and
            // falling all the way through to the loader-BLIND global fallback
            // below. Observed for the `AotTestContextInitializers`/
            // `AotTestContextInitializersFactory`/
            // `DefaultCacheAwareContextLoaderDelegate`/
            // `AotMergedContextConfiguration` family under
            // `@CompileWithForkedClassLoader` (2026-07-22 AOT bean-override
            // double-context-refresh session, see
            // CRATONVM-SPRING-GENUINE-BUGLIST) -- a `new`
            // instruction referencing one of these classes resolved via the
            // global path instead of the fork's own already-loaded copy,
            // busting a `Class`-identity-keyed cache
            // (`AotMergedContextConfiguration.hashCode()`) and causing a
            // second, uncustomized `ApplicationContext` to be created. Trust
            // the side table `should_use_loader_initiated_resolution` already
            // consulted directly instead of silently downgrading to "not a
            // user loader" on a disagreement.
            _ => cratonvm_native_builtins::classloader::defining_loader_for(
                &shared.loader_identity,
                referencing_class_id.as_u32(),
            )
            .map(|loader_obj| {
                let mut ctx = crate::vm::NativeContextImpl { shared, thread };
                cratonvm_types::ClassLoaderId::UserDefined(
                    cratonvm_native_builtins::classloader::loader_namespace_id(
                        &mut ctx, loader_obj,
                    ),
                )
            }),
        }
    } else {
        None
    };
    if dbg_trace {
        eprintln!("[LOADER-TRACE] name={name} user_loader={user_loader:?}");
    }
    if user_loader.is_some() {
        // Gate-on path: drive the user loader FIRST (initiating-loader
        // semantics), then fall back to the global store.
        // `_checked`: a JVMS §5.3.4 `LinkageError` from the load is the
        // resolution's (interpreter round i1 wave 37, lane L5).
        let driven =
            drive_defining_loader_load_checked(shared, thread, referencing_class_id, name)?;
        if dbg_trace {
            eprintln!("[LOADER-TRACE] name={name} drive_defining_loader_load={driven:?}");
        }
        if let Some(id) = driven {
            return Ok(id);
        }
        if isolated_url_definition {
            if crate::runtime::env_cache::dbg_isolated_cnf() {
                let (ref_name, ref_loader) = {
                    let cm = shared.classes.class_manager.read();
                    (
                        cm.get_class(referencing_class_id)
                            .map(|c| c.name.to_string())
                            .unwrap_or_default(),
                        cm.get_loader_id(referencing_class_id),
                    )
                };
                let global = shared
                    .classes
                    .class_manager
                    .read()
                    .resolve_fast_path_class_id(name);
                eprintln!(
                    "[ISOLATED-CNF] name={name} referencing={referencing_class_id:?}/{ref_name} (loader={ref_loader:?}) global_would_be={global:?}"
                );
                for (i, f) in thread.frames.iter().enumerate().rev().take(14) {
                    eprintln!(
                        "[ISOLATED-CNF]   [{i}] {}.{}{} pc={}",
                        f.class_name(),
                        f.method_name(),
                        f.method_descriptor(),
                        f.pc
                    );
                }
            }
            return Err(isolated_loader_class_not_found(shared, thread, name)?);
        }
        let fallback = shared.load_class_concurrent_for(name, requesting_frame(thread));
        if dbg_trace {
            let owner = fallback
                .as_ref()
                .ok()
                .and_then(|id| shared.classes.class_manager.read().get_loader_id(*id));
            eprintln!(
                "[LOADER-TRACE] name={name} GATE-ON global fallback after drive-miss result={fallback:?} owner_loader={owner:?}"
            );
        }
        // The flat fallback's last resort is "the one user loader that has
        // defined this name" (`resolve_fast_path_class_id`). After the
        // referencing class's OWN loader was asked and refused, a class that
        // an unrelated user loader defined is not an answer JVMS §5.3 can give
        // (the initiating loader's refusal is the resolution's failure), and it
        // is the wrong class besides: probe `L5W25OwnerFailureRecordInvoke`,
        // `shared#0`, resolved `ldc Opt.class` to the `Opt` a sibling loader
        // had defined in the previous scenario, so the entry that HotSpot
        // fails succeeded (interpreter round i1 wave 25, lane L5b).
        if let (Ok(id), Some(referencing_loader)) = (&fallback, user_loader) {
            if has_registered_defining_loader
                && answer_is_from_a_foreign_user_loader(shared, referencing_loader, *id)
            {
                return Err(MethodCallFailed::InternalError(VmError::ClassFile(
                    crate::error::ClassFileError::ClassNotFound {
                        class_name: name.to_string(),
                    },
                )));
            }
        }
        return fallback.map_err(MethodCallFailed::from);
    }

    // Gate-off / built-in defining loader: resolve globally FIRST (the legacy
    // fast path), and only if that misses fall back to the referencing class's
    // defining loader. This rescues classes that live ONLY behind a custom
    // loader and are invisible to the global classpath — e.g. a webapp class
    // referencing another class in its own `/WEB-INF/lib` jar (Tomcat's
    // `WebappClassLoader` serves these from its `WebResourceRoot`, not the
    // classpath): JSTL's `JstlCoreTLV.getHandler()` does `new
    // JstlCoreTLV$Handler(...)`, whose inner class would otherwise surface as
    // `NoClassDefFoundError` (`TestScopedAttributeELResolver`). Strictly
    // additive — only fires on what would already be a resolution failure, so
    // it never changes a previously-successful (or differently-failing)
    // resolution.
    // A name the global path can only answer with a *fabricated synthetic
    // stub* must be offered to the referencing class's own loader FIRST.
    // `load_class_concurrent` would otherwise register that stub globally
    // under `Application`, after which the real loader can never define its
    // own copy -- and a stub has no `Code` and implements no interfaces, so
    // the first real use fails (`VerifyError`, or a `ClassCastException` on
    // an interface the real class does implement). Quarkus's fast-jar
    // `RunnerClassLoader` serving `lib/quarkus/generated-bytecode.jar` is the
    // case this was written for: `new ValueRegistry_..._Synthetic_Bean()` from
    // generated Arc bytecode resolved to a stub, which then could not be cast
    // to `io.quarkus.arc.InjectableBean`. Strictly additive -- it only
    // pre-empts an answer that was going to be fake.
    //
    // Reuses `has_registered_defining_loader` (computed once above) rather than
    // the process-wide "any loader exists" latch, for the same reason as the
    // (0) pre-pass: the body is a `drive_defining_loader_load`, which needs a
    // defining loader for THIS referencing class and returns `None` without
    // one. Every `new`/`checkcast`/`instanceof` of an app-loader class reaches
    // this line -- resolution is not cached per call site -- so the probe ran
    // on the hot path of every allocation once any custom loader had ever
    // defined a class.
    // Whether `drive_loader_for_global_name` below asked the referencing
    // loader for `name` in this resolution: one resolution is one `loadClass`
    // request (JVMS §5.3.2), so the two later asks of such a name are skipped.
    let mut loader_asked = false;
    // The user loader whose answer the global route below IS: a transparent
    // loader `drive_loader_for_global_name` did not ask because its chain
    // answers `name` exactly as the global route does (wave 40). Its answer
    // then meets the JVMS §5.3.4 initiating-load check, as a driven answer
    // does (interpreter round i1 wave 42, lane L5; probe
    // `L5W42GlobalNameConstraint`, `transparent`).
    let mut global_answers_for = None;
    let stub_prepass = has_registered_defining_loader
        && shared
            .classes
            .class_manager
            .read()
            .would_fabricate_synthetic_stub(name);
    // `--compatible` too with the `StubPrepassOneAsk` row (interpreter round
    // i1 wave 51, lane L5): the arm below asks once, through the checked
    // door, and the global route that follows may still fabricate the stub
    // (that mode's own behaviour); only the loader's second ask and its
    // dropped throw go.
    if stub_prepass
        && shared
            .classes
            .aligned(cratonvm_types::HotSpotAlignment::StubPrepassOneAsk)
    {
        // `--jdk-only` never fabricates the stub (`admit_compatibility_class`
        // refuses it), so the global route below can only miss: this is the
        // resolution's one request of the loader (JVMS §5.3.2), through the
        // checked door, and it is not asked again after that miss. It was
        // asked twice, its first throw dropped (interpreter round i1 wave 41,
        // lane L5; probe `L5W41StubPrepassAsk`, which needs
        // `CRATONVM_LOADER_AWARE_RESOLUTION=0` to reach this arm).
        match drive_defining_loader_load_after_global_miss_checked(
            shared,
            thread,
            referencing_class_id,
            name,
        ) {
            Ok(Some(id)) => return Ok(id),
            Ok(None) => loader_asked = true,
            Err(e) => {
                // The row-off route dropped this throw and asked again (wave
                // 51, lane L5: the alignment census).
                shared
                    .classes
                    .note_alignment_divergence(cratonvm_types::HotSpotAlignment::StubPrepassOneAsk);
                return Err(e);
            }
        }
    } else if stub_prepass {
        // `_after_global_miss`: a would-be stub is a global miss, and a
        // `javax/`-style name the loader defines itself is not a stub
        // (interpreter round i1 wave 27, lane L5).
        if let Some(id) =
            drive_defining_loader_load_after_global_miss(shared, thread, referencing_class_id, name)
        {
            if dbg_trace {
                eprintln!("[LOADER-TRACE] name={name} resolved via would-stub loader drive {id:?}");
            }
            return Ok(id);
        }
    }
    // `--jdk-only`: a JDK-global name the loader may answer itself is asked of
    // it before the global route (wave 28, lane L5; `L5W27JdkNamedOwnClass`,
    // `lazy jdk class`); `--compatible` too with
    // `CRATONVM_COMPAT_GLOBAL_NAME_ASK` (`ClassRealm::global_name_ask_on`;
    // wave 49, lane L5). Only a class with a defining-loader object gets here.
    // (Local bools first: an application-loader class pays one test here.)
    if has_registered_defining_loader
        && !stub_prepass
        && loader_global_name_ask_enabled(shared, name)
    {
        // `_checked`: the loader's throwable (or `null`) is the resolution's
        // error under `--jdk-only` (wave 39, lane L5).
        match drive_loader_for_global_name(
            shared,
            thread,
            referencing_class_id,
            Some(direct_loader),
            name,
        )? {
            Some(Some(id)) => {
                note_global_name_ask_answer(shared, id);
                return Ok(id);
            }
            Some(None) => loader_asked = true,
            None => {
                global_answers_for = direct_loader
                    .filter(|l| matches!(l, cratonvm_types::ClassLoaderId::UserDefined(_)));
            }
        }
    }
    // A flat-store load AFTER the referencing class's user loader was asked
    // and refused (`loader_asked`: the stub pre-pass's checked ask, the
    // `BaseMiss` arm's `Ok(None)`, or the global-name ask): HotSpot would
    // define nothing here, so the built-in define this load makes is a VM
    // fallback -- never refused by `CRATONVM_DEFINE_BUILTIN_CONSTRAINT_REFUSAL`,
    // counted and admitted as before (interpreter round i6 wave 118, lane L5;
    // `i118-L9-builtin-define-constraint-refusal-has-no-fallback-mark`). Two
    // short write guards on this cold path only.
    // Wave 119 (lane L9 review): only while the refusal is on, so the off
    // arm (`=0`; the default until wave 134) takes no class-manager write guard here, as before
    // wave 118; and cleared only by the load that set it, so a nested
    // fallback load on this thread does not end the outer one's exemption.
    // WARNING(i6-user-flip): CRATONVM_DEFINE_BUILTIN_CONSTRAINT_REFUSAL default ON by user decision 2026-10-19; =0 restores the old path
    let vm_fallback = loader_asked
        && has_registered_defining_loader
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEFINE_BUILTIN_CONSTRAINT_REFUSAL")
        && shared.classes.class_manager_write().set_vm_fallback_defines(true);
    let global = shared.load_class_concurrent_for(name, requesting_frame(thread));
    if vm_fallback {
        shared.classes.class_manager_write().set_vm_fallback_defines(false);
    }
    // A `javax/`-style name the flat store answered with ANOTHER user loader's
    // class (its last resort, the lone user-loader definition): not what this
    // loader maps the name to unless its own `loadClass` says so, which is
    // asked now. Two child-first loaders each defining their own lazily
    // requested `javax/…/L5Lazy`: the second resolved the first's class
    // (interpreter round i1 wave 27, lane L5; `L5W27JdkNamedOwnClass`, `B lazy
    // name`). Only such a foreign answer is questioned, and a loader that
    // gives nothing keeps it, as before. Cold: a user-loader class's first
    // resolution of such a name, once per site.
    if let Ok(foreign) = &global {
        if has_registered_defining_loader
            && !loader_asked
            && is_user_loader_global_name(name)
            && direct_loader.is_some_and(|loader| {
                answer_is_from_a_foreign_user_loader(shared, loader, *foreign)
            })
        {
            if let Some(id) = drive_defining_loader_load_after_global_miss(
                shared,
                thread,
                referencing_class_id,
                name,
            ) {
                return Ok(id);
            }
            // Another user loader's class is not what the transparent chain
            // answers: no initiating-load check on it (wave 42).
            global_answers_for = None;
        }
    }
    match global {
        Ok(id) => {
            if dbg_trace {
                let cm = shared.classes.class_manager.read();
                let owner = cm.get_loader_id(id);
                eprintln!(
                    "[LOADER-TRACE] name={name} resolved via GLOBAL-FIRST fallback cid={id:?} owner_loader={owner:?}"
                );
            }
            if let Some(loader) = global_answers_for {
                // One read guard and an `is_empty` test while no constraint
                // exists (`check_initiating_load`); `--jdk-only` only.
                crate::runtime::resolve::loader_constraints::check_initiating_load(
                    shared, loader, name, id,
                )?;
                if name.starts_with("java/") {
                    crate::runtime::resolve::initiating_records::record_initiating_load(
                        shared, loader, name, id,
                    );
                    cache_loader_initiated(shared, loader, name, id);
                } else if shared.config.global_name_initiating_record {
                    // `CRATONVM_GLOBAL_NAME_INITIATING_RECORD` (default ON,
                    // `=0` off; interpreter round i6 wave 133, lane L5): the
                    // transparent loader's chain answered this JDK-global name
                    // on HotSpot, which files it in the loader's dictionary;
                    // the record (not the memo: the loader is never asked for
                    // it here) is what `findLoadedClass` reads. One read guard
                    // once the row exists. Probe `L5W133TransparentGlobalNameRecord`.
                    crate::runtime::resolve::initiating_records::record_initiating_load(
                        shared, loader, name, id,
                    );
                }
            }
            Ok(id)
        }
        Err(e) => {
            // After a global miss the loader is asked for a `javax/`-style
            // name too: HotSpot asks it for every name (interpreter round i1
            // wave 27, lane L5; `L5W27JdkNamedOwnClass`, `lazy name` rows).
            // Not a second time in one resolution (wave 28).
            // `--jdk-only` asks through the checked door: the outcome is a
            // failure either way, and the loader's own throwable (or `null`)
            // is HotSpot's error for it (i37-L5 remainder; wave 40, lane L5).
            if !loader_asked {
                let asked = if shared.classes.loader_throws_on {
                    drive_defining_loader_load_after_global_miss_checked(
                        shared,
                        thread,
                        referencing_class_id,
                        name,
                    )?
                } else {
                    drive_defining_loader_load_after_global_miss(
                        shared,
                        thread,
                        referencing_class_id,
                        name,
                    )
                };
                if let Some(id) = asked {
                    return Ok(id);
                }
            }
            // An array resolution fails when its COMPONENT cannot be resolved
            // globally, and `drive_defining_loader_load` declines `[` names --
            // so offer the component to the referencing class's own loader and
            // re-synthesise. The (0) pre-pass above only covers a component the
            // global path would answer with a fabricated STUB; a component that
            // is simply absent from the process class path lands here instead.
            // Keycloak 26.6.1 / Quarkus fast-jar:
            // `[Lorg/antlr/v4/runtime/atn/ATNConfig;` from
            // `ATNConfigSet$AbstractConfigHashSet.createBuckets`, whose jar is
            // reachable only through the `RunnerClassLoader`.
            // `--jdk-only`: a user loader was asked for the element in (0b)
            // already, once (wave 41, lane L5).
            if let Some(component) = array_component_class_name(name) {
                if !(direct_user_loader && shared.classes.loader_throws_on)
                    && drive_defining_loader_load(shared, thread, referencing_class_id, component)
                        .is_some()
                {
                    if let Ok(id) = shared.load_class_concurrent_for(name, requesting_frame(thread))
                    {
                        return Ok(id);
                    }
                }
            }
            Err(MethodCallFailed::from(e))
        }
    }
}

/// Whether `answer` — the flat global fallback's class for a name that the
/// referencing class's defining loader (`referencing_loader`) was just asked
/// for and refused — was defined by a user loader that is neither that loader
/// nor one of its recorded delegation ancestors.
///
/// `SharedVm::load_class_concurrent*` answers from the built-in delegation
/// chain first, but its last resort is the lone user loader that has defined
/// the name (`ClassManager::resolve_fast_path_class_id`, kept for in-memory
/// classes with no class file). For a reference whose initiating loader
/// refused the name, such a class is a sibling namespace's: accepting it
/// resolved an entry HotSpot fails, to a class of another loader (interpreter
/// round i1 wave 25, lane L5b). A built-in loader's answer is kept, as before
/// (CratonVM's compatibility fallback for a loader emulation that missed); so
/// is an ancestor's, which that loader's own delegation could have returned.
pub(crate) fn answer_is_from_a_foreign_user_loader(
    shared: &SharedVm,
    referencing_loader: cratonvm_types::ClassLoaderId,
    answer: ClassId,
) -> bool {
    let cratonvm_types::ClassLoaderId::UserDefined(referencing_ns) = referencing_loader else {
        return false;
    };
    let answer_loader = shared.classes.class_manager.read().get_loader_id(answer);
    let Some(cratonvm_types::ClassLoaderId::UserDefined(answer_ns)) = answer_loader else {
        return false;
    };
    if answer_ns == referencing_ns {
        return false;
    }
    let mut ancestors = [0u32; cratonvm_classloading::MAX_USER_LOADER_DEPTH];
    let n = shared
        .loader_identity
        .user_loader_parents()
        .ancestors(referencing_ns, &mut ancestors);
    !ancestors[..n].contains(&answer_ns)
}

/// The `(owner, method, descriptor)` of the frame that is currently executing,
/// for contract §9's `requested_by` attribution.
///
/// Three borrowed `&str`s, so a caller that resolves a class it never
/// fabricates pays three pointer copies and no allocation; the requester is
/// only formatted at the recording site, and only when a violation was actually
/// produced. Call this at the load site rather than binding it early — the
/// result borrows `thread`, and the resolution paths around it need `thread`
/// mutably.
///
/// `None` on an empty frame stack, which is VM bootstrap: a fabrication there
/// has no Java requester and the census says so rather than inventing one.
#[inline]
pub(crate) fn requesting_frame(thread: &JvmThread) -> Option<(&str, &str, &str)> {
    thread
        .frames
        .last()
        .map(|f| (f.class_name(), f.method_name(), f.method_descriptor()))
}

/// Resolve `name` by invoking the `loadClass` of the loader that DEFINED
/// `referencing_class_id` (JVMS §5.4.3 initiating loader). Returns `None` when
/// there is no recorded defining-loader object, the name is array/JDK-global, a
/// re-entrant resolution for the same (class, name) is already in flight, or the
/// loader's `loadClass` does not produce a class — in every such case the caller
/// falls back to global resolution, so this can only ever resolve MORE classes,
/// never fail one that global resolution would have answered.
pub(crate) fn drive_defining_loader_load(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    drive_defining_loader_load_door(
        shared,
        thread,
        referencing_class_id,
        name,
        "drive-defining-loader-load",
    )
}

/// [`drive_defining_loader_load`] with its request door named by the caller
/// (interpreter round i6 wave 123, lane L5, `i39-L5`): the lambda dispatch
/// override (`lambda-impl-owner`) and the natives' caller-loader drives
/// (`caller-loader-before-stub` / `caller-loader-after-miss`) are their own
/// `Lookup` rows of the `LOADER-REQUEST` census instead of the array pre-pass's
/// `drive-defining-loader-load`. Same drive, same answer.
pub(crate) fn drive_defining_loader_load_door(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
    door: &'static str,
) -> Option<ClassId> {
    if name.starts_with('[') || is_global_resolution_namespace(name) {
        return None;
    }
    // The one request prologue (interpreter round i6 wave 122, lane L5): a
    // `Lookup`-kind drive (array pre-pass, receiver fallbacks, the lambda
    // dispatch override, the caller-loader drives), counted, and answered from
    // the loader's dictionary under `CRATONVM_LOADER_REQUEST_DOOR`.
    if let Some(id) = crate::runtime::resolve::initiating_records::loader_request_prologue(
        shared,
        crate::runtime::resolve::initiating_records::LoaderRequestKind::Lookup,
        door,
        referencing_class_id,
        name,
    ) {
        return Some(id);
    }
    drive_defining_loader_load_named(shared, thread, referencing_class_id, name, false)
        .unwrap_or(None)
}

/// [`drive_defining_loader_load`] for the class-resolution doors
/// ([`resolve_class_loader_aware`], the `invokestatic` owner): `Err` is the
/// resolution's error, as HotSpot propagates it, instead of "not answered"
/// and a global fallback that would define the name elsewhere:
///
/// * a JVMS §5.3.4 loader-constraint `LinkageError` the loader's `loadClass`
///   raised (the define-time check), or that its answer raises (the
///   initiating-load check) — interpreter round i1 wave 37, lane L5;
/// * `--jdk-only`, a loader whose `loadClass` is its own bytecode: the
///   throwable it threw, a `ClassNotFoundException` as `NoClassDefFoundError`
///   with it as the cause (JVMS §5.3; wave 38, lane L5,
///   `runtime::resolve::loader_throw`);
/// * `--jdk-only`, such a loader returning `null` or a class of another
///   name: `NoClassDefFoundError: <name>` with no cause (JVMS §5.3.2; wave
///   39, lane L5, `loader_throw::loader_null_as_resolution_error`).
///
/// `--jdk-only`: a refusal (a throw, or no answer) first re-reads what the
/// loader answers now — its own definition or its initiating record
/// (`runtime::resolve::initiating_records`) — and takes an answer another
/// thread published meanwhile for the same loader and name
/// (JVMS §5.4.3, the loser adopts the winner; `i25-L5` fix 2). Sequentially
/// that re-read finds nothing: the caller's own lookup before the drive
/// missed. Every other outcome is [`drive_defining_loader_load`]'s, and
/// `--compatible` is unchanged (neither arm reaches it).
pub(crate) fn drive_defining_loader_load_checked(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
) -> Result<Option<ClassId>, MethodCallFailed> {
    if name.starts_with('[') || is_global_resolution_namespace(name) {
        return Ok(None);
    }
    // The one request prologue (interpreter round i6 wave 122, lane L5): a
    // `Resolution`-kind request, counted only (its dictionary rule is
    // `CRATONVM_LOADER_DICTIONARY_BEFORE_DRIVE`, inside the drive).
    let _ = crate::runtime::resolve::initiating_records::loader_request_prologue(
        shared,
        crate::runtime::resolve::initiating_records::LoaderRequestKind::Resolution,
        "checked",
        referencing_class_id,
        name,
    );
    drive_defining_loader_load_checked_named(shared, thread, referencing_class_id, name)
}

/// The body of [`drive_defining_loader_load_checked`] after its name filter;
/// also the drive of [`drive_loader_for_global_name`], which
/// asks for a JDK-global name the filter excludes.
fn drive_defining_loader_load_checked_named(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
) -> Result<Option<ClassId>, MethodCallFailed> {
    drive_defining_loader_load_checked_as(shared, thread, referencing_class_id, name, name)
}

/// [`drive_defining_loader_load_checked`] for the ELEMENT class `component` of
/// the array class `array_name` a reference names (JVMS §5.3.3: the element
/// is loaded by the same initiating loader, and a failure fails the array's
/// resolution). HotSpot's `NoClassDefFoundError` for it names the array
/// (`NoClassDefFoundError: [Lp/X;`), so that is the error's message; the
/// loader is asked for `component`. `--jdk-only` callers
/// (`resolve_array_class_loader_aware_checked`; interpreter round i1 wave
/// 41, lane L5, probe `L5W41ArrayComponentLoaderThrow`).
pub(crate) fn drive_array_component_load_checked(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    component: &str,
    array_name: &str,
) -> Result<Option<ClassId>, MethodCallFailed> {
    if component.starts_with('[') || is_global_resolution_namespace(component) {
        return Ok(None);
    }
    drive_defining_loader_load_checked_as(
        shared,
        thread,
        referencing_class_id,
        component,
        array_name,
    )
}

/// The checked door's body: the loader is asked for `name`; a
/// `NoClassDefFoundError` it turns into names `error_name` (`name` itself,
/// or the array whose element `name` is).
fn drive_defining_loader_load_checked_as(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
    error_name: &str,
) -> Result<Option<ClassId>, MethodCallFailed> {
    drive_defining_loader_load_checked_reason(
        shared,
        thread,
        referencing_class_id,
        name,
        error_name,
    )
    .map(|answer| answer.ok())
}

/// Why the checked door answered "no class" without an error (its
/// `Ok(None)`), for the one caller that files the two cases differently
/// ([`drive_loader_for_global_name`] under
/// `CRATONVM_GLOBAL_NAME_REFUSAL_RECORD`). Interpreter round i6 wave 125, lane
/// L5 (`i38-L5-proposal-the-initiating-record-replaces-the-memo-under-jdk-only`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckedDriveMiss {
    /// [`DriveRefusal::BaseMiss`] with `global_route_may_answer`: a
    /// base-delegation loader whose chain reaches the application loader,
    /// where the flat store stood in for the parent and missed. HotSpot's
    /// parent chain answers the class the global route answers, and the
    /// loader becomes an initiating loader of it.
    BaseMiss,
    /// The drive degraded without asking, or its answer was not a refusal the
    /// door converts: the in-flight re-entry guard, no loader object or a
    /// mirror with no `ClassId`, the fall-through (a `null` `null_refuses`
    /// does not cover). HotSpot records nothing for these.
    Degrade,
}

/// [`drive_defining_loader_load_checked_as`] with the reason of its
/// `Ok(None)` ([`CheckedDriveMiss`]): `Ok(Ok(id))` answered, `Ok(Err(why))`
/// no class and no error, `Err` the resolution's error. One body, so the two
/// doors cannot drift.
fn drive_defining_loader_load_checked_reason(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
    error_name: &str,
) -> Result<Result<ClassId, CheckedDriveMiss>, MethodCallFailed> {
    let refused =
        match drive_defining_loader_load_named(shared, thread, referencing_class_id, name, true) {
            Ok(Some(id)) => return Ok(Ok(id)),
            Err(DriveRefusal::Failed(e)) => return Err(e),
            Ok(None) => None,
            Err(refusal) => Some(refusal),
        };
    if shared.classes.loader_throws_on {
        // No allocation since the loader returned: `refused`'s throwable is
        // still live across these read-only lookups. Only the two faithful
        // sources are read — the loader's own definition and its initiating
        // RECORD (written by a successful drive of this door, wave 38) — not
        // the memo, which also holds global-fallback answers and which the
        // `invokestatic` owner and isolated URL loaders deliberately ignore.
        // `CRATONVM_LOADER_DICTIONARY_ONE_DOOR` (default off; interpreter
        // round i6 wave 121, lane L5): the loader's dictionary through its one
        // door (`initiating_records::loader_dictionary`), which reads the same
        // two records but answers a `java/` name's own definition too (the
        // exact-key probe), where `lookup_loader_defined_exact` declines it.
        let published = if shared.config.loader_dictionary_one_door {
            // The guard is dropped at the semicolon, before the door reads.
            let loader = shared
                .classes
                .class_manager
                .read()
                .get_loader_id(referencing_class_id);
            loader.and_then(|loader| {
                crate::runtime::resolve::initiating_records::loader_dictionary(shared, loader, name)
            })
        } else {
            lookup_loader_defined_exact(shared, referencing_class_id, name).or_else(|| {
                let loader = shared
                    .classes
                    .class_manager
                    .read()
                    .get_loader_id(referencing_class_id)?;
                crate::runtime::resolve::initiating_records::initiated_class(shared, loader, name)
            })
        };
        if let Some(id) = published {
            crate::runtime::resolve::loader_throw::note_resolution_race(
                shared,
                "refusal adopts the class published meanwhile",
                referencing_class_id,
                &name,
            );
            return Ok(Ok(id));
        }
    }
    match refused {
        Some(DriveRefusal::LoaderThrew(exc)) => Err(
            crate::runtime::resolve::loader_throw::loader_throw_as_resolution_error(
                shared,
                thread,
                referencing_class_id,
                name,
                error_name,
                exc,
            ),
        ),
        Some(DriveRefusal::LoaderReturnedNull { wrong_name }) => Err(
            crate::runtime::resolve::loader_throw::loader_null_as_resolution_error(
                shared,
                thread,
                referencing_class_id,
                name,
                error_name,
                wrong_name,
            ),
        ),
        Some(DriveRefusal::Failed(e)) => Err(e),
        // An application-parented base-delegation loader's CNFE: the flat
        // store stood in for its parent, and that store's misses are not all
        // faithful, so the global route still answers what it can. When it
        // has nothing either, the resolution fails whatever happens next, and
        // HotSpot's error is `NoClassDefFoundError` caused by the loader's
        // exception, not the global route's own (interpreter round i1 wave
        // 41, lane L5; probe `L5W41AppParentMissCause`). The exception is
        // pinned across the read-only class-path search.
        Some(DriveRefusal::BaseMiss(exc)) => {
            let pin_base = thread.native_pin_roots.len();
            thread.native_pin_roots.push(exc);
            let may_answer = shared
                .classes
                .class_manager
                .read()
                .global_route_may_answer(name);
            let exc = thread.native_pin_roots[pin_base];
            thread.native_pin_roots.truncate(pin_base);
            if may_answer {
                Ok(Err(CheckedDriveMiss::BaseMiss))
            } else {
                Err(
                    crate::runtime::resolve::loader_throw::loader_throw_as_resolution_error(
                        shared,
                        thread,
                        referencing_class_id,
                        name,
                        error_name,
                        exc,
                    ),
                )
            }
        }
        None => Ok(Err(CheckedDriveMiss::Degrade)),
    }
}

/// Why [`drive_defining_loader_load_named`] did not answer, when the caller
/// must hear it ([`drive_defining_loader_load_checked`]); every other caller
/// folds both into "not answered".
enum DriveRefusal {
    /// The resolution's error: a JVMS §5.3.4 loader-constraint `LinkageError`
    /// (wave 37), or, `--jdk-only` at the checked door, the
    /// `ClassCircularityError` of a re-entrant load through a loader that is
    /// not parallel-capable (wave 38).
    Failed(MethodCallFailed),
    /// `--jdk-only`, the checked door, a loader whose `loadClass` is its own
    /// bytecode: the throwable it threw, as thrown (converted by
    /// `loader_throw::loader_throw_as_resolution_error`). Since wave 40 also
    /// a base-delegation loader's throwable that is its own (see
    /// `drive_defining_loader_load_named`'s `base_throw`). Live: nothing has
    /// allocated since the loader returned.
    LoaderThrew(ObjectRef),
    /// `--jdk-only`, the checked door, a loader whose `loadClass` is its own
    /// bytecode: it returned `null`, or (`wrong_name`) a class whose name is
    /// not the requested one (JVMS §5.3.2; wave 39, lane L5). Converted by
    /// `loader_throw::loader_null_as_resolution_error`.
    LoaderReturnedNull { wrong_name: bool },
    /// `--jdk-only`, the checked door, a base-delegation loader whose chain
    /// reaches the application loader: the `ClassNotFoundException` it threw
    /// (not `base_load_class_miss_is_the_loaders`, so the native's
    /// parent-first step was the flat store). The checked door keeps the
    /// global fallback when the global route may answer the name, and
    /// otherwise fails with this exception as HotSpot's cause (interpreter
    /// round i1 wave 41, lane L5; probe `L5W41AppParentMissCause`). Live, as
    /// `LoaderThrew`.
    BaseMiss(ObjectRef),
}

/// [`drive_defining_loader_load`] for a caller whose GLOBAL resolution of
/// `name` has just missed (or would only fabricate a compatibility stub): the
/// JDK-global names, including `java/` ([`is_user_loader_global_name`]), are
/// offered to the referencing class's defining loader too.
///
/// The global route answers those names on the premise that every custom
/// loader delegates them to the JDK. When the JDK has no such class, the
/// premise gives nothing, and the loader may well define it: a child-first
/// loader's own `javax.*` class that it has not been asked for yet resolved to
/// `NoClassDefFoundError` where HotSpot asks the loader (interpreter round i1
/// wave 27, lane L5; probe `L5W27JdkNamedOwnClass`, `lazy name` rows). Only a
/// failure becomes a success, so a resolution that worked before is unchanged,
/// in every mode. The answer is memoised for the loader
/// ([`loader_own_record_of_global_name`] reads it).
pub(crate) fn drive_defining_loader_load_after_global_miss(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    if name.starts_with('[') {
        return None;
    }
    // The one request prologue (interpreter round i6 wave 122, lane L5): a
    // `Lookup`-kind drive, counted, and answered from the loader's dictionary
    // under `CRATONVM_LOADER_REQUEST_DOOR`.
    if let Some(id) = crate::runtime::resolve::initiating_records::loader_request_prologue(
        shared,
        crate::runtime::resolve::initiating_records::LoaderRequestKind::Lookup,
        "after-global-miss",
        referencing_class_id,
        name,
    ) {
        return Some(id);
    }
    drive_defining_loader_load_named(shared, thread, referencing_class_id, name, false)
        .unwrap_or(None)
}

/// [`drive_defining_loader_load_after_global_miss`] through the checked door
/// ([`drive_defining_loader_load_checked`]'s body), for
/// [`resolve_class_loader_aware`]'s ask after the global route MISSED
/// (`--jdk-only`): the resolution fails either way, and the loader's own
/// throwable, or its `null`, is the error HotSpot raises for it, instead of
/// the global route's (interpreter round i1 wave 40, lane L5; the i37-L5
/// "after-global-miss" remainder). Also the `invokestatic` owner's ask after
/// its flat route missed (`dispatch_static.rs`; wave 41, lane L5).
pub(crate) fn drive_defining_loader_load_after_global_miss_checked(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
) -> Result<Option<ClassId>, MethodCallFailed> {
    if name.starts_with('[') {
        return Ok(None);
    }
    drive_defining_loader_load_checked_named(shared, thread, referencing_class_id, name)
}

/// `--jdk-only`: ask the referencing class's user-defined loader for a
/// JDK-global name ([`is_user_loader_global_name`]) BEFORE the global route
/// answers it, when that loader may answer its own class
/// (`L5W27JdkNamedOwnClass`, `lazy jdk class`: a child-first loader defining
/// its own `javax.security.auth.x500.X500Principal` on request; HotSpot asks
/// the loader for every name, `SystemDictionary::resolve_instance_class_or_null`).
///
/// A user loader asked for a JDK-global name answered `id`: count it in the
/// alignment census when `id` is a user loader's class, which the global
/// route (the row-off answer) would not have given (interpreter round i1
/// wave 51, lane L5). Cold: once per resolution of such a name.
#[cold]
#[inline(never)]
fn note_global_name_ask_answer(shared: &SharedVm, id: ClassId) {
    let user_defined = matches!(
        shared.classes.class_manager.read().get_loader_id(id),
        Some(cratonvm_types::ClassLoaderId::UserDefined(_))
    );
    if user_defined {
        shared
            .classes
            .note_alignment_divergence(cratonvm_types::HotSpotAlignment::GlobalNameAsk);
    }
}

/// [`drive_loader_for_global_name`]:
///
/// `None` = not asked: the caller's global route proceeds as before.
/// `Some(Some(id))` = the loader's answer, memoised for (loader, name) by the
/// drive, so the next resolution is answered by [`lookup_loader_initiated`].
/// `Some(None)` = asked and refused: the caller's global route answers, and
/// must not ask the loader a second time in the same resolution.
///
/// The common case pays nothing per resolution beyond one probe of a per-loader
/// map: a loader whose delegation chain overrides no `loadClass` (base
/// parent-first delegation to a built-in loader) answers a runtime-image name
/// exactly as the global route does
/// (`classloader_real::loader_answers_jdk_names_as_the_jdk`, computed once per
/// loader and kept in `ClassRealm::loader_global_name_transparency`), so it is
/// never asked. A loader that overrides `loadClass` is asked once per name; a
/// refusal memoises the already-loaded built-in class the global route will
/// answer, so a refusing loader is not asked again for it. `--compatible`
/// returns `None` on its first test (unchanged). Interpreter round i1 wave 28,
/// lane L5.
///
/// `Err`: the loader is asked through the checked door
/// ([`drive_defining_loader_load_checked`]'s body), so the throwable of a
/// loader whose `loadClass` is its own bytecode, or its `null` answer, is the
/// resolution's error, as HotSpot raises it for any name, instead of a
/// refusal the global route overrules (JVMS §5.3; interpreter round i1 wave
/// 39, lane L5; probe `L5W39GlobalNameLoaderThrow`).
pub(crate) fn drive_loader_for_global_name(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    referencing_loader: Option<Option<cratonvm_types::ClassLoaderId>>,
    name: &str,
) -> Result<Option<Option<ClassId>>, MethodCallFailed> {
    if !loader_global_name_ask_enabled(shared, name) {
        return Ok(None);
    }
    // The caller's own `get_loader_id` answer when it has one (no second
    // class-manager read), else read here.
    let referencing_loader = referencing_loader.unwrap_or_else(|| {
        shared
            .classes
            .class_manager
            .read()
            .get_loader_id(referencing_class_id)
    });
    let loader = match referencing_loader {
        Some(l @ cratonvm_types::ClassLoaderId::UserDefined(_)) => l,
        _ => return Ok(None),
    };
    let known = shared
        .classes
        .loader_global_name_transparency
        .read()
        .get(&loader)
        .copied();
    let transparent = match known {
        Some(t) => t,
        None => {
            let Some(loader_obj) = cratonvm_native_builtins::classloader::defining_loader_for(
                &shared.loader_identity,
                referencing_class_id.as_u32(),
            ) else {
                return Ok(None);
            };
            let t = {
                let mut ctx_impl = crate::vm::NativeContextImpl { shared, thread };
                let ctx: &mut dyn cratonvm_native_api::NativeContext = &mut ctx_impl;
                cratonvm_native_builtins::classloader_real::loader_answers_jdk_names_as_the_jdk(
                    ctx, loader_obj,
                )
            };
            shared
                .classes
                .loader_global_name_transparency
                .write()
                .insert(loader, t);
            t
        }
    };
    if transparent {
        // Transparent for the runtime image; for a name on the application
        // class path only when the loader's chain reaches the application
        // loader (a null- or platform-parented loader's `findClass` answers
        // it on HotSpot). Interpreter round i1 wave 40, lane L5; probe
        // `L5W40BootParentGlobalName`.
        let Some(loader_obj) = cratonvm_native_builtins::classloader::defining_loader_for(
            &shared.loader_identity,
            referencing_class_id.as_u32(),
        ) else {
            return Ok(None);
        };
        let as_global = {
            let mut ctx_impl = crate::vm::NativeContextImpl { shared, thread };
            let ctx: &mut dyn cratonvm_native_api::NativeContext = &mut ctx_impl;
            cratonvm_native_builtins::classloader_real::transparent_loader_answers_as_the_global_route(
                ctx, loader_obj, name,
            )
        };
        if as_global {
            return Ok(None);
        }
        if cratonvm_types::flags().loader.dbg_access {
            eprintln!(
                "[ACCESS-DBG] GLOBAL-NAME ASK {}: transparent {loader:?} does not reach the application loader, asked",
                name.replace('/', ".")
            );
        }
    }
    // Asked before (another call site, or a door that does not read the
    // memo): its answer stands, one request per (loader, name).
    if let Some(id) =
        crate::runtime::resolve::initiating_records::loader_initiated_answer(shared, loader, name)
    {
        return Ok(Some(Some(id)));
    }
    let answer = drive_defining_loader_load_checked_reason(
        shared,
        thread,
        referencing_class_id,
        name,
        name,
    )?;
    let driven = answer.ok();
    if crate::runtime::env_cache::dbg_isolated_cnf() {
        eprintln!(
            "[ISOLATED-CNF] global-name drive name={name} loader={loader:?} answer={driven:?}"
        );
    }
    if let Err(why) = answer {
        // Refused (or re-entrant): the global route answers, as before.
        // `CRATONVM_GLOBAL_NAME_REFUSAL_RECORD` (default off; interpreter
        // round i6 wave 125, lane L5,
        // `i38-L5-proposal-the-initiating-record-replaces-the-memo-under-jdk-only`):
        // the refusal is filed by its reason. A `BaseMiss` is the class
        // HotSpot's parent chain answers, and HotSpot makes the loader an
        // initiating loader of it: it goes to the initiating RECORD
        // (`findLoadedClass` reads it), not the capped memo. A degrade case
        // is an answer HotSpot never records: nothing is kept, and the loader
        // is asked again next time, as on HotSpot. The census
        // (`CRATONVM_DBG=access`, `global-name-refusal-memo=`) counts both
        // kinds in either position of the switch.
        let base_miss = why == CheckedDriveMiss::BaseMiss;
        let builtin = {
            let cm = shared.classes.class_manager.read();
            cm.get_loaded_class_id(name)
                .filter(|id| match cm.get_loader_id(*id) {
                    Some(cratonvm_types::ClassLoaderId::UserDefined(_)) | None => false,
                    Some(cratonvm_types::ClassLoaderId::Application) => !transparent,
                    Some(_) => true,
                })
        };
        if shared.config.global_name_refusal_record {
            let filed = match (base_miss, builtin) {
                (true, Some(id)) if shared.classes.initiating_records_on => {
                    crate::runtime::resolve::initiating_records::record_global_name_refusal(
                        shared, loader, name, id,
                    );
                    "record"
                }
                // No records in this mode (`--compatible` without
                // `CRATONVM_COMPAT_INITIATING_RECORDS`): the record writer is
                // a no-op there, so the memo keeps the answer, as with the
                // switch off; without it every site-cache miss asked the
                // loader again (wave 126, L9 review of wave 125).
                (true, Some(id)) => {
                    cache_loader_initiated(shared, loader, name, id);
                    "memo (no records in this mode)"
                }
                (true, None) => "nothing (no built-in class loaded yet)",
                (false, _) => "nothing",
            };
            if cratonvm_types::flags().loader.dbg_access {
                crate::runtime::resolve::initiating_records::note_global_name_refusal(
                    shared, loader, name, base_miss, filed,
                );
            }
            return Ok(Some(None));
        }
        if cratonvm_types::flags().loader.dbg_access {
            crate::runtime::resolve::initiating_records::note_global_name_refusal(
                shared,
                loader,
                name,
                base_miss,
                if builtin.is_some() { "memo" } else { "nothing" },
            );
        }
        // Switch off: keep the built-in class the global route will answer
        // for this loader in the memo, whatever the reason, so the refusal is
        // not re-driven on every site-cache miss (one upcall and one
        // exception each). A transparent loader reaches here only when its
        // chain does not reach the application loader (above): the
        // application's class is not one it can answer, so it is not
        // memoised for it (wave 40; `builtin`'s filter).
        if let Some(id) = builtin {
            cache_loader_initiated(shared, loader, name, id);
        }
    }
    Ok(Some(driven))
}

/// The body of [`drive_defining_loader_load`] after its name filter. `Err`
/// for a JVMS §5.3.4 loader-constraint `LinkageError`, and, when
/// `propagate_throw` (the checked door) under `--jdk-only`, for the throwable
/// of a loader whose `loadClass` is its own bytecode
/// ([`drive_defining_loader_load_checked`]).
fn drive_defining_loader_load_named(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
    propagate_throw: bool,
) -> Result<Option<ClassId>, DriveRefusal> {
    let dbg_trace = crate::runtime::env_cache::dbg_loader_trace()
        && (name.contains("EnvironmentPostProcessorsFactory")
            || name.contains("CloudFoundryVcapEnvironmentPostProcessor"));
    let loader_obj_opt = cratonvm_native_builtins::classloader::defining_loader_for(
        &shared.loader_identity,
        referencing_class_id.as_u32(),
    );
    if dbg_trace {
        eprintln!(
            "[LOADER-TRACE] drive_defining_loader_load name={name} referencing_class_id={referencing_class_id:?} defining_loader_for={:?}",
            loader_obj_opt.map(|o| o.as_ptr())
        );
    }
    let Some(loader_obj) = loader_obj_opt else {
        return Ok(None);
    };
    // A per-thread in-flight guard breaks pathological re-entry for the same
    // (class, name) by degrading to global resolution. Each row also carries
    // the referencing class's loader (`to_native_id`, `u32::MAX` for none), for
    // the circularity check below. The rows live on the `JvmThread`
    // (`loader_drives_in_flight`), so they are this VM's: they were a
    // `thread_local!` keyed by a bare `ClassId`, which a second VM driven on
    // the same OS thread shared (interpreter round i1 wave 40, lane L5).
    let key = referencing_class_id.as_u32();
    let cache_loader = shared
        .classes
        .class_manager
        .read()
        .get_loader_id(referencing_class_id);
    // `CRATONVM_LOADER_DICTIONARY_BEFORE_DRIVE` (interpreter round i6 wave
    // 120, lane L7, default off, parity; page
    // `i120-L7-a-loader-is-asked-again-for-a-resolved-name`): HotSpot's
    // `SystemDictionary` looks a name up in the initiating loader's
    // dictionary before it calls that loader's `loadClass`, and the
    // dictionary holds every class the loader defined and every class it
    // answered for an earlier resolution. A user loader's `loadClass` is then
    // asked once per name, from any door. This is the one function every
    // resolution door's drive goes through, so it asks the two records of
    // those facts (the loader's own definition, and its initiating record,
    // written by a successful drive) first and answers without a call.
    // Off: the loader is asked, as before.
    if shared.config.loader_dictionary_before_drive {
        // The loader's dictionary through its one door (interpreter round i6
        // wave 121, lane L5: `initiating_records::loader_dictionary`, the same
        // two records, read in the same order).
        if let Some(loader @ cratonvm_types::ClassLoaderId::UserDefined(_)) = cache_loader {
            if let Some(id) =
                crate::runtime::resolve::initiating_records::loader_dictionary(shared, loader, name)
            {
                return Ok(Some(id));
            }
        }
    }
    let loader_key = cache_loader.map_or(u32::MAX, |l| l.to_native_id());
    let (same_site, same_loader) = {
        let s = &thread.loader_drives_in_flight;
        (
            s.iter().any(|(c, n, _)| *c == key && n == name),
            s.iter().any(|(_, n, l)| *l == loader_key && n == name),
        )
    };
    // JVMS §5.3.5 / HotSpot `SystemDictionary::resolve_instance_class_or_null`:
    // a VM-initiated load of `name` through a loader that is NOT
    // parallel-capable, on a thread already loading `name` through that loader
    // (the loader's `loadClass` resolved the name again), is a
    // `ClassCircularityError` — recorded against the entry like any
    // `LinkageError`. `--jdk-only`, the checked door, a user loader whose
    // `parallelLockMap` is null (`loader_locks_itself`); every other re-entry is
    // as before (interpreter round i1 wave 38, lane L5; probe
    // `L5W38ReentrantLoadCircularity`).
    //
    // A PARALLEL-CAPABLE loader re-entered from the same site is asked again,
    // as HotSpot asks it (no placeholder lock for such a loader): the inner
    // call typically defines the name and the outer one finds it. The
    // in-flight guard used to decline there, and the global fallback defined
    // the name in the APPLICATION loader, so one entry resolved to two
    // classes (interpreter round i1 wave 39, lane L5; probe
    // `L5W38ReentrantLoadParallel`). A loader that re-enters without ever
    // defining recurses on HotSpot until `StackOverflowError`; here the
    // nesting is cut at `PARALLEL_REENTRY_BOUND` same-site rows with that
    // error, before the native stack can overflow.
    let mut reask_parallel = false;
    if (same_site || same_loader)
        && propagate_throw
        && shared
            .natives
            .native_methods
            .compat_parallel_capable_loaders()
        && matches!(
            cache_loader,
            Some(cratonvm_types::ClassLoaderId::UserDefined(_))
        )
    {
        let locks_itself = {
            let mut ctx_impl = crate::vm::NativeContextImpl { shared, thread };
            let ctx: &mut dyn cratonvm_native_api::NativeContext = &mut ctx_impl;
            cratonvm_native_builtins::classloader_real::loader_locks_itself(ctx, loader_obj)
        };
        if !locks_itself && same_site {
            const PARALLEL_REENTRY_BOUND: usize = 16;
            let nesting = thread
                .loader_drives_in_flight
                .iter()
                .filter(|(c, n, _)| *c == key && n == name)
                .count();
            let n = shared
                .classes
                .loader_reentry_reasks
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .saturating_add(1);
            let cut = nesting >= PARALLEL_REENTRY_BOUND;
            if cratonvm_types::flags().loader.dbg_access {
                eprintln!(
                    "[ACCESS-DBG] LOADER-REENTRY-PARALLEL #{n} {}: {} re-asked through {cache_loader:?} (class {key}, nesting {nesting})",
                    if cut { "StackOverflowError" } else { "re-ask" },
                    name.replace('/', ".")
                );
            }
            if cut {
                let soe = crate::runtime::exceptions::create_exception_object(
                    shared,
                    thread,
                    "java/lang/StackOverflowError",
                    None,
                );
                return Err(DriveRefusal::Failed(match soe {
                    Ok(soe) => MethodCallFailed::ExceptionThrown(soe),
                    Err(e) => e,
                }));
            }
            reask_parallel = true;
        }
        if locks_itself {
            let n = shared
                .classes
                .loader_reentry_circularities
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .saturating_add(1);
            // The row-off route re-asked the loader (wave 51, lane L5: the
            // alignment census).
            shared
                .classes
                .note_alignment_divergence(cratonvm_types::HotSpotAlignment::ParallelLoaders);
            if cratonvm_types::flags().loader.dbg_access {
                eprintln!(
                    "[ACCESS-DBG] LOADER-REENTRY #{n} ClassCircularityError: {} re-entered through {cache_loader:?} (class {key})",
                    name.replace('/', ".")
                );
            }
            return Err(DriveRefusal::Failed(
                crate::runtime::exceptions::throw_linkage_error(
                    shared,
                    thread,
                    LinkageError::ClassCircularityError {
                        class_name: name.to_string(),
                    },
                ),
            ));
        }
    }
    if same_site && !reask_parallel {
        if crate::runtime::env_cache::dbg_isolated_cnf() {
            eprintln!("[ISOLATED-CNF] drive declined: IN_FLIGHT re-entry name={name} cid={key}");
        }
        return Ok(None);
    }
    thread
        .loader_drives_in_flight
        .push((key, name.to_string(), loader_key));
    let depth = thread.frames.len();
    let dotted = name.replace('/', ".");
    // JVMS §5.3.2 / HotSpot `SystemDictionary::resolve_instance_class_or_null`:
    // a VM-initiated load through a loader that is not parallel-capable holds
    // the loader's monitor (`ObjectLocker`) around its `loadClass`, so two
    // resolving threads never run one such loader's `loadClass` at once (the
    // duplicate-definition race of
    // `docs/internal/fixed-bugs/interpreter-L5-a-racing-resolution-of-one-class-entry-can-leave-threads-disagreeing-FIXED-20261005.md`).
    // `loader_locks_itself` is the JDK's own signal, a null `parallelLockMap`
    // (`--jdk-only`, user loaders only): `getClassLoadingLock` then returns the
    // loader too, and the base `loadClass` native takes the same monitor, so
    // the VM and Java lock one object in one order. The monitor is taken
    // BEFORE `catch_alloc_oom` and released after it on every path; the loader
    // is pinned across both (an allocation or the wait can move it).
    // Interpreter round i1 wave 28, lane L5; probe `L5W28LoaderMonitorAtLoad`.
    // `throw_propagates`: the loader's throwable will be the resolution's
    // (`--jdk-only`, the checked door, a loader whose `loadClass` is its own
    // bytecode; wave 38, lane L5). Decided before the call, while `loader_obj`
    // is certainly current; pure (declared methods only).
    //
    // `base_throw` / `base_cnfe` (wave 40, lane L5): the same door and mode,
    // a loader whose `loadClass` is CratonVM's base-delegation native
    // (`classloader_real::cl_real_load_class_base`, a loader overriding only
    // `findClass`). A throwable that is not a `ClassNotFoundException` is
    // never one of that native's emulated misses (those are CNFEs): it is the
    // loader's `findClass`, its parent's `loadClass`, or its define, and
    // propagates. A CNFE propagates only where the native consults no flat
    // store before `findClass` (`base_load_class_miss_is_the_loaders`).
    //
    // `null_refuses` (wave 47, lane L5): the checked door, a loader whose
    // `loadClass` is its own bytecode, in EITHER mode: its `null` or
    // wrong-named answer is the resolution's `NoClassDefFoundError` (JVMS
    // §5.3.2), which `--compatible` used to turn into a global fallback
    // (`interpreter-L5-compatible-mode-accepts-a-loadclass-answer-of-another-name-FIXED-20261011`).
    // A thrown exception propagates under `--jdk-only`, and under
    // `--compatible` with `CRATONVM_COMPAT_LOADER_THROWS`
    // (`ClassRealm::loader_throws_on`, which also gates the racing-winner
    // re-read of the checked door, the after-global-miss and array-element
    // doors; interpreter round i1 wave 49, lane L5, the i47-L5 `--compatible`
    // gates page, item 1).
    let (loader_pin, loader_locked, throw_propagates, null_refuses, base_throw, base_cnfe) = {
        let mut ctx_impl = crate::vm::NativeContextImpl { shared, thread };
        let ctx: &mut dyn cratonvm_native_api::NativeContext = &mut ctx_impl;
        let pin = ctx.pin_native_root(loader_obj);
        let checked = propagate_throw && shared.classes.loader_throws_on;
        let null_refuses = propagate_throw
            && cratonvm_native_builtins::classloader_real::loader_load_class_is_bytecode(
                ctx, loader_obj,
            );
        let throw_propagates = checked && null_refuses;
        let base_throw = checked && !throw_propagates;
        let base_cnfe = base_throw
            && cratonvm_native_builtins::classloader_real::base_load_class_miss_is_the_loaders(
                ctx, loader_obj, name,
            );
        let locked =
            cratonvm_native_builtins::classloader_real::loader_locks_itself(ctx, loader_obj);
        if locked {
            let _ = ctx.monitor_enter_gc_safe(loader_obj);
        }
        (
            pin,
            locked,
            throw_propagates,
            null_refuses,
            base_throw,
            base_cnfe,
        )
    };
    // Recheck the dictionary AFTER acquiring a non-parallel loader's monitor.
    // Two constant pools can race on one java.* name; the losing thread must
    // not make another observable loadClass call (i47).
    //
    // `CRATONVM_LOADER_DRIVE_UNDER_LOADER_LOCK` (default ON, `=0` restores the
    // `java/`-only recheck; interpreter round i6 wave 129, lane L7, page
    // `i120-L7-a-loader-is-asked-again-for-a-resolved-name`, the `threads`
    // row): EVERY name, as HotSpot's `resolve_instance_class_or_null` does --
    // `ObjectLocker` on a non-parallel-capable loader, then
    // `dictionary->find_class` again under it, and `load_instance_class` only
    // on a miss. A thread that waited here while another one's `loadClass`
    // defined the name (or initiated it: published below before the monitor
    // is released) is answered from the loader's dictionary without a call.
    // A parallel-capable loader, the boot, platform and application loaders
    // take no monitor here (`loader_locked` false), as HotSpot takes none for
    // them (`get_loader_lock_or_null`), and are asked by each racing thread.
    // A re-entry on this thread (the loader's `loadClass` resolving another
    // name) re-enters the monitor; one for the SAME name was refused above
    // (`ClassCircularityError`), before the monitor.
    let lock_recheck_all = loader_locked && shared.config.loader_drive_under_loader_lock;
    if loader_locked && (lock_recheck_all || name.starts_with("java/")) {
        if let Some(id) = cache_loader.and_then(|loader| {
            // `CRATONVM_LOADER_DICTIONARY_ONE_DOOR` (default off; wave 121,
            // lane L5): the whole dictionary (the loader's own definition too).
            // A non-`java/` name under the lock switch asks the whole
            // dictionary too: the loader's own definition is the answer the
            // racing thread's `loadClass` usually left.
            if shared.config.loader_dictionary_one_door
                || (lock_recheck_all && !name.starts_with("java/"))
            {
                crate::runtime::resolve::initiating_records::loader_dictionary(shared, loader, name)
            } else {
                crate::runtime::resolve::initiating_records::initiated_class(shared, loader, name)
            }
        }) {
            use cratonvm_native_api::NativeContext as _;
            let mut ctx = crate::vm::NativeContextImpl { shared, thread };
            let ctx: &mut dyn cratonvm_native_api::NativeContext = &mut ctx;
            let loader_obj = ctx.read_native_pin(loader_pin, loader_obj);
            ctx.monitor_exit(loader_obj);
            ctx.unpin_native_roots(loader_pin);
            thread.loader_drives_in_flight.pop();
            return Ok(Some(id));
        }
    }
    // gc-common w6-c (`handoff-w6c-rerunning-contexts-take-the-unwinding-arm`):
    // the body runs the loader's `loadClass`, so an allocation in it cannot be
    // retried; it takes the unwinding arm instead of the infallible one, and an
    // unwind is an `Err` like any other loader failure (stray frames are
    // truncated below, the in-flight row is popped after, the caller falls through to
    // global resolution).
    let result = match crate::runtime::native_oom::catch_alloc_oom(|| {
        // `create_string` / `invoke_virtual` are `NativeContext` trait methods —
        // bring the trait into scope to call them.
        use cratonvm_native_api::NativeContext as _;
        let mut ctx = crate::vm::NativeContextImpl { shared, thread };
        let name_obj = ctx.create_string(&dotted);
        // `create_string` allocates: the loader is read back from its pin.
        let loader_obj = ctx.read_native_pin(loader_pin, loader_obj);
        if name.starts_with("java/") && loader_global_name_ask_enabled(shared, name) {
            shared
                .classes
                .note_alignment_divergence(cratonvm_types::HotSpotAlignment::JavaNameAsk);
        }
        let result = ctx.invoke_virtual(
            loader_obj,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[Value::Object(Some(name_obj))],
        );
        // Elasticsearch's EmbeddedImplClassLoader can report a failed Java
        // `loadClass` while its provider archive still contains the requested
        // implementation class. This fallback belongs at the VM's
        // initiating-loader boundary, where bytecode references have no
        // ServiceLoader context. The archive helper preserves this loader's
        // namespace and defining-loader association.
        if matches!(result, Ok(Some(Value::Object(Some(_))))) {
            return result;
        }
        // `loadClass` ran Java: the loader is read back from its pin.
        let loader_obj = ctx.read_native_pin(loader_pin, loader_obj);
        // Only for that loader (interpreter round i2 wave 54, lane L5): any
        // other loader's miss is the resolution's answer, as on HotSpot, and
        // must not pay a class-path `IMPL-JARS` search or define a class the
        // loader refused (`impl_jars_fallback_applies`).
        if !cratonvm_native_builtins::service_loader::impl_jars_fallback_applies(&ctx, loader_obj) {
            return result;
        }
        // The loader's throwable, when it threw, is pinned across the archive
        // search, which can define a class (a moving collection): the checked
        // door hands it on as the resolution's error (wave 40, lane L5).
        let thrown_pin = match &result {
            Err(MethodCallFailed::ExceptionThrown(exc)) => Some(ctx.pin_native_root(*exc)),
            _ => None,
        };
        let archive = cratonvm_native_builtins::service_loader::impl_jars_load_class(
            &mut ctx,
            Some(loader_obj),
            name,
        );
        let result = match (thrown_pin, result) {
            (Some(pin), Err(MethodCallFailed::ExceptionThrown(exc))) => {
                let exc = ctx.read_native_pin(pin, exc);
                ctx.unpin_native_roots(pin);
                Err(MethodCallFailed::ExceptionThrown(exc))
            }
            (_, result) => result,
        };
        match archive {
            Ok(Some(mirror)) => Ok(Some(Value::Object(Some(mirror)))),
            // A refusal here leaves the original resolution result standing,
            // exactly as "archive had no such class" already did.
            _ => result,
        }
    }) {
        Ok(r) => r,
        Err(oom) => Err(MethodCallFailed::InternalError(
            crate::error::VmError::Runtime(oom.into_runtime_error()),
        )),
    };
    // Publish a validated java.* initiating answer before releasing that
    // same monitor. Loader constraints cannot differ for prohibited packages.
    if loader_locked && name.starts_with("java/") {
        if let (Some(loader), Ok(Some(Value::Object(Some(mirror))))) = (cache_loader, &result) {
            if let Some(id) = crate::vm::class_id_from_mirror(shared, *mirror) {
                if shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(id)
                    .is_some_and(|class| &*class.name == name)
                {
                    crate::runtime::resolve::initiating_records::record_initiating_load(
                        shared, loader, name, id,
                    );
                    cache_loader_initiated(shared, loader, name, id);
                }
            }
        }
    }
    // `CRATONVM_LOADER_DRIVE_UNDER_LOADER_LOCK` (wave 129, lane L7): every
    // other name's correctly-named answer is judged and published under the
    // same monitor, as HotSpot's `load_instance_class` runs
    // `check_constraints` and `update_dictionary` before `ObjectLocker`
    // releases the loader -- otherwise a waiting thread could take the
    // monitor between the release and the record (a loader that DELEGATED the
    // name: its own definition is visible at define already) and ask again.
    // The outcome is carried past the release in `published`: the class id
    // (no mirror is read after a constraint error has allocated) and the
    // constraint verdict, which the success path below returns as is. A
    // refused answer records nothing, as on HotSpot.
    let mut published: Option<(ClassId, Result<(), MethodCallFailed>)> = None;
    if lock_recheck_all && !name.starts_with("java/") {
        if let (Some(loader), Ok(Some(Value::Object(Some(mirror))))) = (cache_loader, &result) {
            if let Some(id) = crate::vm::class_id_from_mirror(shared, *mirror) {
                let named = shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(id)
                    .is_some_and(|class| &*class.name == name);
                if named {
                    let verdict = crate::runtime::resolve::loader_constraints::check_initiating_load(
                        shared, loader, name, id,
                    );
                    if verdict.is_ok() {
                        cache_loader_initiated(shared, loader, name, id);
                        // The checked door only, as below.
                        if propagate_throw {
                            crate::runtime::resolve::initiating_records::record_initiating_load(
                                shared, loader, name, id,
                            );
                        }
                    }
                    published = Some((id, verdict));
                }
            }
        }
    }
    {
        let mut ctx_impl = crate::vm::NativeContextImpl { shared, thread };
        let ctx: &mut dyn cratonvm_native_api::NativeContext = &mut ctx_impl;
        if loader_locked {
            let loader_now = ctx.read_native_pin(loader_pin, loader_obj);
            ctx.monitor_exit(loader_now);
        }
        ctx.unpin_native_roots(loader_pin);
    }
    // Defensive: a re-entrant call that unwound abnormally must not leave stray
    // frames on this thread's stack.
    if thread.frames.len() > depth {
        thread.frames.truncate(depth);
        // Root-snapshot cache correctness (see `pop_and_recycle_frame_with_reason`):
        // bulk-popping stray frames re-exposes `frames[depth-1]` as the resuming
        // top, so bump its `exec_epoch` to invalidate any stale cached roots.
        if let Some(top) = thread.frames.last_mut() {
            top.exec_epoch = top.exec_epoch.wrapping_add(1);
        }
    }
    thread.loader_drives_in_flight.pop();
    // The answer judged and published under the monitor (above): the success
    // path's own checks and writes were made there.
    if let Some((id, verdict)) = published {
        if dbg_trace {
            eprintln!(
                "[LOADER-TRACE] drive_defining_loader_load name={name} published under the loader lock: {id:?} ok={}",
                verdict.is_ok()
            );
        }
        verdict.map_err(DriveRefusal::Failed)?;
        return Ok(Some(id));
    }
    if dbg_trace {
        let mapped = if let Ok(Some(Value::Object(Some(mirror)))) = result {
            crate::vm::class_id_from_mirror(shared, mirror)
        } else {
            None
        };
        eprintln!(
            "[LOADER-TRACE] drive_defining_loader_load name={name} loadClass_result={result:?} mapped_cid={mapped:?}"
        );
    }
    // An answer whose name is not the requested one is no answer (JVMS
    // §5.3.2; HotSpot's `load_instance_class_impl` compares the names and
    // returns null on a mismatch, which `resolve_or_fail` turns into
    // `NoClassDefFoundError`). Interpreter round i1 wave 39, lane L5
    // (`--jdk-only`); both modes since wave 47; probe
    // `L5W39LoaderReturnsNull`, `wrong` rows.
    let mut wrong_name = false;
    if let Ok(Some(Value::Object(Some(mirror)))) = result {
        let Some(id) = crate::vm::class_id_from_mirror(shared, mirror) else {
            if crate::runtime::env_cache::dbg_isolated_cnf() {
                eprintln!("[ISOLATED-CNF] drive declined: mirror had no ClassId name={name}");
            }
            return Ok(None);
        };
        wrong_name = shared
            .classes
            .class_manager
            .read()
            .get_class(id)
            .is_some_and(|c| &*c.name != name);
        if !wrong_name {
            if let Some(l) = cache_loader {
                // JVMS §5.3.4: `l` now initiates `name` as `id`; a constraint
                // pinning it elsewhere refuses the load (HotSpot's
                // `check_constraints` for an initiating loader; interpreter
                // round i1 wave 37, lane L5). `--jdk-only` only.
                crate::runtime::resolve::loader_constraints::check_initiating_load(
                    shared, l, name, id,
                )
                .map_err(DriveRefusal::Failed)?;
                cache_loader_initiated(shared, l, name, id);
                // JVMS §5.3: the resolution door's request makes `l` an
                // initiating loader of `id` (HotSpot's dictionary record,
                // which `findLoadedClass` and a later define of the name by
                // `l` read). The checked door only: the other drives are
                // CratonVM's own probes. `--jdk-only`; wave 38, lane L5.
                if propagate_throw {
                    crate::runtime::resolve::initiating_records::record_initiating_load(
                        shared, l, name, id,
                    );
                }
            }
            return Ok(Some(id));
        }
    }
    // The checked door, a loader whose `loadClass` is its own bytecode: a
    // `null` answer (or one of another name, above) is the resolution's
    // `NoClassDefFoundError`, as on HotSpot, not a global fallback (wave 39,
    // lane L5, `--jdk-only`; both modes since wave 47; probe
    // `L5W39LoaderReturnsNull`).
    if null_refuses && (wrong_name || matches!(result, Ok(Some(Value::Object(None))))) {
        return Err(DriveRefusal::LoaderReturnedNull { wrong_name });
    }
    // A loader-constraint `LinkageError` raised inside the loader's
    // `defineClass` propagates, as on HotSpot, instead of a global fallback
    // that would define the name elsewhere (wave 37, lane L5; `--jdk-only`).
    if let Err(MethodCallFailed::ExceptionThrown(exc)) = &result {
        if crate::runtime::resolve::loader_constraints::is_loader_constraint_error(shared, *exc) {
            return Err(DriveRefusal::Failed(MethodCallFailed::ExceptionThrown(
                *exc,
            )));
        }
        // The loader's own refusal or error is the resolution's (JVMS §5.3;
        // wave 38, lane L5): the checked door converts it after re-reading a
        // racing winner. Nothing has allocated since the loader returned.
        if throw_propagates {
            return Err(DriveRefusal::LoaderThrew(*exc));
        }
        // A base-delegation loader's throw (see `base_throw` above; wave 40,
        // lane L5, probe `L5W40BaseLoaderThrow`). Reads only; still live.
        if base_throw
            && (base_cnfe
                || !crate::runtime::resolve::loader_throw::is_class_not_found(shared, *exc))
        {
            return Err(DriveRefusal::LoaderThrew(*exc));
        }
        // The rest of a base-delegation loader's throws are CNFEs of an
        // application-parented chain: the checked door decides after asking
        // whether the global route may answer (wave 41, lane L5).
        if base_throw {
            return Err(DriveRefusal::BaseMiss(*exc));
        }
    }
    if crate::runtime::env_cache::dbg_isolated_cnf() {
        let shape = match &result {
            Ok(Some(Value::Object(None))) => "Ok(null)".to_string(),
            Ok(None) => "Ok(void)".to_string(),
            Ok(Some(v)) => format!("Ok(non-object {v:?})"),
            Err(MethodCallFailed::ExceptionThrown(exc)) => {
                let cm = shared.classes.class_manager.read();
                let cid = Some(shared.mem.heap.class_id_of(*exc));
                let cname = cid
                    .and_then(|c| cm.get_class(c).map(|k| k.name.to_string()))
                    .unwrap_or_default();
                format!("Err(thrown {cname})")
            }
            Err(e) => format!("Err({e:?})"),
        };
        eprintln!("[ISOLATED-CNF] drive declined: loadClass -> {shape} name={name}");
    }
    Ok(None)
}

#[cfg(test)]
mod i2_l2_constant_resolution_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::ThreadId;

    #[test]
    fn object_type_name_accepts_only_class_descriptors() {
        assert_eq!(
            object_type_name("Ljava/lang/String;"),
            Some("java/lang/String")
        );
        assert_eq!(object_type_name("L;"), None);
        assert_eq!(object_type_name("I"), None);
        assert_eq!(object_type_name("[Ljava/lang/String;"), None);
        // Unterminated: refused rather than sliced (a byte slice at `len - 1`
        // could split a multi-byte character).
        assert_eq!(object_type_name("Ljava/lang/Stri\u{e9}"), None);
    }

    /// A bootstrap declared with a primitive return type hands back a
    /// primitive `Value`; the field type's widening applies, as
    /// `MethodHandle.invoke` would apply it.
    #[test]
    fn condy_primitive_results_widen_to_the_field_type() {
        let shared = SharedVm::new(VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let t = &mut thread;
        assert_eq!(
            condy_convert_result(&shared, &mut *t, b'J', None, "J", Value::Int(5)).ok(),
            Some(Value::Long(5))
        );
        assert_eq!(
            condy_convert_result(&shared, &mut *t, b'D', None, "D", Value::Int(2)).ok(),
            Some(Value::Double(2.0))
        );
        assert_eq!(
            condy_convert_result(&shared, &mut *t, b'I', None, "I", Value::Int(42)).ok(),
            Some(Value::Int(42))
        );
        // A null reference result for a reference type is a legal answer.
        let null_ok = condy_convert_result(
            &shared,
            &mut *t,
            b'L',
            Some(ClassId::new(0)),
            "java/lang/Object",
            Value::Object(None),
        );
        assert_eq!(null_ok.ok(), Some(Value::Object(None)));
    }

    /// JVMS §5.4.3: a recorded `LinkageError` is rethrown as a NEW instance of
    /// the same class with the same message, keyed per (class, cp index); a
    /// throwable that is not a `LinkageError` is never recorded.
    #[test]
    fn a_recorded_linkage_error_is_rethrown_with_its_class_and_message() {
        let shared = SharedVm::new(VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let referencing = ClassId::new(0);
        let Ok(ncdfe) = crate::runtime::exceptions::create_exception_object(
            &shared,
            &mut thread,
            "java/lang/NoClassDefFoundError",
            Some("p/Opt"),
        ) else {
            // Stripped VM (no class library on the test classpath).
            return;
        };
        let Ok(rte) = crate::runtime::exceptions::create_exception_object(
            &shared,
            &mut thread,
            "java/lang/RuntimeException",
            Some("not a linkage error"),
        ) else {
            return;
        };
        let _ = record_resolution_failure(
            &shared,
            referencing,
            7,
            MethodCallFailed::ExceptionThrown(rte),
        );
        assert!(recorded_resolution_failure(&shared, &mut thread, referencing, 7).is_none());

        let _ = record_resolution_failure(
            &shared,
            referencing,
            9,
            MethodCallFailed::ExceptionThrown(ncdfe),
        );
        let Some(MethodCallFailed::ExceptionThrown(again)) =
            recorded_resolution_failure(&shared, &mut thread, referencing, 9)
        else {
            panic!("the NoClassDefFoundError must have been recorded");
        };
        assert_ne!(again, ncdfe, "a new throwable, not the recorded object");
        // Same class and message as the original (compared through the same
        // reader, so the test does not depend on the Throwable field layout).
        let original = linkage_error_record(&shared, ncdfe).expect("a LinkageError");
        let rethrown = linkage_error_record(&shared, again).expect("still a LinkageError");
        assert_eq!(&*rethrown.error_class, "java/lang/NoClassDefFoundError");
        assert_eq!(rethrown, original);
        // Another entry of the same class is unaffected.
        assert!(recorded_resolution_failure(&shared, &mut thread, referencing, 10).is_none());
    }

    /// Interpreter round i1 wave 25, lane L5: the "a failure was recorded"
    /// pre-filter is the VM's own (`ClassRealm::resolution_failure_recorded`),
    /// not a process static: one VM's record leaves another VM's miss paths
    /// off the table, and a record raises the recording VM's bit.
    #[test]
    fn the_failure_prefilter_is_per_vm() {
        let first = SharedVm::new(VmConfig::default());
        let second = SharedVm::new(VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let Ok(ncdfe) = crate::runtime::exceptions::create_exception_object(
            &first,
            &mut thread,
            "java/lang/NoClassDefFoundError",
            Some("p/Opt"),
        ) else {
            // Stripped VM (no class library on the test classpath).
            return;
        };
        assert!(!any_resolution_failure_recorded(&first));
        assert!(!any_resolution_failure_recorded(&second));
        let _ = record_resolution_failure(
            &first,
            ClassId::new(0),
            9,
            MethodCallFailed::ExceptionThrown(ncdfe),
        );
        assert!(
            any_resolution_failure_recorded(&first),
            "the recording VM's pre-filter is raised"
        );
        assert!(
            !any_resolution_failure_recorded(&second),
            "another VM's record must not raise this VM's pre-filter"
        );
        assert!(!resolution_failure_is_recorded(&second, ClassId::new(0), 9));
        assert!(resolution_failure_is_recorded(&first, ClassId::new(0), 9));
    }

    /// The compatible-mode native `Lookup.find*` reports a miss as an
    /// unmaterialised `VmError::Runtime(NoSuchMethodException)`; it must map
    /// to the `NoSuchMethodError` HotSpot raises, exactly as a thrown
    /// `NoSuchMethodException` does (probe L6/GenericIndyStaticArgsProbe).
    #[test]
    fn an_unmaterialised_lookup_miss_maps_to_the_linkage_error() {
        let shared = SharedVm::new(VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(0), "test");
        if crate::runtime::exceptions::create_exception_object(
            &shared,
            &mut thread,
            "java/lang/NoSuchMethodError",
            None,
        )
        .is_err()
        {
            // Stripped VM (no class library on the test classpath).
            return;
        }
        // The mapping recognises the reflective family by its real superclass
        // chain; a VM whose `NoSuchMethodException` is a synthetic stand-in
        // (no `ReflectiveOperationException` above it) cannot exercise it.
        let Ok(probe) = crate::runtime::exceptions::create_exception_object(
            &shared,
            &mut thread,
            "java/lang/NoSuchMethodException",
            None,
        ) else {
            return;
        };
        {
            let cm = shared.classes.class_manager.read();
            let reflective = cm
                .get_loaded_class_id("java/lang/ReflectiveOperationException")
                .is_some_and(|roe| cm.is_subclass_of(shared.mem.heap.class_id_of(probe), roe));
            if !reflective {
                return;
            }
        }
        let failure = MethodCallFailed::InternalError(VmError::Runtime(
            crate::error::RuntimeError::NoSuchMethodException {
                message: "p.C.missing()I".to_string(),
            },
        ));
        let MethodCallFailed::ExceptionThrown(err) =
            map_lookup_exception_to_error(&shared, &mut thread, failure)
        else {
            panic!("a reflective miss must come back as a thrown Java error");
        };
        let cm = shared.classes.class_manager.read();
        let name = cm
            .get_class(shared.mem.heap.class_id_of(err))
            .map(|c| c.name.to_string());
        assert_eq!(name.as_deref(), Some("java/lang/NoSuchMethodError"));
    }

    fn thrown_class_name(shared: &SharedVm, failure: &MethodCallFailed) -> Option<String> {
        let MethodCallFailed::ExceptionThrown(obj) = failure else {
            return None;
        };
        let cm = shared.classes.class_manager.read();
        cm.get_class(shared.mem.heap.class_id_of(*obj))
            .map(|c| c.name.to_string())
    }

    #[test]
    fn descriptor_class_names_reads_each_name_to_its_semicolon() {
        assert_eq!(
            descriptor_class_names("(Lp/A;[[Lp/B;IJ)Lp/C;"),
            vec!["p/A", "p/B", "p/C"]
        );
        // An `L` inside a name is part of the name, not a new type.
        assert_eq!(descriptor_class_names("(ILcom/Lib;[I)V"), vec!["com/Lib"]);
        assert!(descriptor_class_names("(IJ)[D").is_empty());
        // A malformed tail stops rather than slicing past the end.
        assert_eq!(descriptor_class_names("(Lp/A;Lp/Unterminated"), vec!["p/A"]);
    }

    /// The JDK factory's `TypeNotPresentException` for a missing class becomes
    /// the `NoClassDefFoundError` HotSpot's method-type resolution raises; any
    /// other throwable passes through as the same object.
    #[test]
    fn a_method_type_miss_is_a_no_class_def_found_error() {
        let shared = SharedVm::new(VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let Ok(tnpe) = crate::runtime::exceptions::create_exception_object(
            &shared,
            &mut thread,
            "java/lang/TypeNotPresentException",
            None,
        ) else {
            // Stripped VM (no class library on the test classpath).
            return;
        };
        let Ok(other) = crate::runtime::exceptions::create_exception_object(
            &shared,
            &mut thread,
            "java/lang/IllegalStateException",
            None,
        ) else {
            return;
        };
        let desc = "(Lcratonvm/test/l2/NoSuchParamType;)V";
        let holder = ClassId::new(0);
        let mapped = method_type_failure_as_linkage_error(&shared, &mut thread, holder, desc, tnpe);
        assert_eq!(
            thrown_class_name(&shared, &mapped).as_deref(),
            Some("java/lang/NoClassDefFoundError")
        );
        let kept = method_type_failure_as_linkage_error(&shared, &mut thread, holder, desc, other);
        assert!(matches!(kept, MethodCallFailed::ExceptionThrown(o) if o == other));
    }

    /// Numeric `ldc` / `ldc2_w` push a correctly typed (and, for long/double,
    /// tagged) value on the fill and on the hit, and a warm site is served
    /// from the per-thread `PrimitiveConstantSiteCache`.
    #[test]
    fn numeric_ldc_sites_are_served_from_the_thread_table() {
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry as E};

        // The cache on whatever the environment says (wave 58, lane L7: the
        // gate is per-VM).
        let mut config = VmConfig::default();
        config.ldc_const_cache = true;
        let shared = SharedVm::new(config);
        let class_id = {
            let mut cm = shared.classes.class_manager.write();
            let Ok(id) = cm.try_ensure_synthetic_class("cratonvm/test/l2/Ldc2wSite", 0) else {
                return; // --jdk-only policy in the environment: nothing to fabricate
            };
            let Some(class) = cm.class_store.get_mut(id) else {
                return;
            };
            class.constant_pool = ConstantPool::new(vec![
                E::Tombstone,
                E::Long((i64::MIN + 7).into()),
                E::Tombstone,
                E::Double((-2.5).into()),
                E::Tombstone,
                E::Integer(-424_242),
                E::Float(1.5),
            ]);
            id
        };
        let mut thread = JvmThread::new(ThreadId(0), "ldc2w");
        thread.frames.push(Frame::new(
            class_id,
            "cratonvm/test/l2/Ldc2wSite".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        ));
        for round in 0..2 {
            let before = PrimitiveConstantSiteCache::epochs_now();
            assert!(
                execute_ldc2w(&shared, &mut thread, 0, 1).is_ok(),
                "round {round}"
            );
            assert!(
                execute_ldc2w(&shared, &mut thread, 0, 3).is_ok(),
                "round {round}"
            );
            assert!(
                execute_ldc(&shared, &mut thread, 0, 5).is_ok(),
                "round {round}"
            );
            assert!(
                execute_ldc(&shared, &mut thread, 0, 6).is_ok(),
                "round {round}"
            );
            assert_eq!(thread.frames[0].stack.pop_float().ok(), Some(1.5));
            assert_eq!(thread.frames[0].stack.pop_int().ok(), Some(-424_242));
            assert_eq!(thread.frames[0].stack.pop_double().ok(), Some(-2.5));
            assert_eq!(thread.frames[0].stack.pop_long().ok(), Some(i64::MIN + 7));
            // Epochs are process-wide and other tests define classes
            // concurrently, so the table is asserted only over a quiet window.
            let double_hit = thread
                .site_tables
                .prim_const_sites
                .get(class_id, 3)
                .copied();
            let int_hit = thread
                .site_tables
                .prim_const_sites
                .get(class_id, 5)
                .copied();
            if ldc_const_cache_enabled(&shared)
                && !cratonvm_classloading::any_class_redefined()
                && PrimitiveConstantSiteCache::epochs_now() == before
            {
                assert_eq!(
                    double_hit,
                    Some(PrimitiveConstant::Double(-2.5)),
                    "round {round}"
                );
                assert_eq!(
                    int_hit,
                    Some(PrimitiveConstant::Int(-424_242)),
                    "round {round}"
                );
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod w9c_resolution_oom_tests {
    //! gc-common w9-c: the allocations of constant resolution beyond `ldc`'s
    //! String arms -- a `CONSTANT_MethodType`'s descriptor, a
    //! `CONSTANT_MethodHandle`'s member name and mirrors, a condy's and an
    //! indy's bootstrap name and static arguments -- follow `new`'s contract
    //! (`common-w8v-constant-resolution-strings-abort-on-a-full-heap`).
    //!
    //! The unit-test VM has no JDK, so `MethodHandles.lookup()` (which every
    //! bootstrap and `MethodHandle` resolution calls first) cannot run here;
    //! the tests drive the helpers those resolutions call, on a heap held full
    //! by pins, and ratchet the files against the infallible constructors.

    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::threading::jvm_thread::ThreadId;

    pub(crate) fn small_gen_vm() -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            max_heap_size: 8 * 1024 * 1024,
            initial_heap_size: 8 * 1024 * 1024,
            ..VmConfig::default()
        })
    }

    /// Fill `shared`'s heap with objects pinned on `thread` until a forced
    /// collection frees nothing (the `w8v_ldc_literal_oom_tests` recipe: both
    /// the young path and the whole-heap path, collecting between passes,
    /// because a young collection promotes the pins and empties eden). `true`
    /// once neither path can place a one-slot object.
    pub(crate) fn fill_heap_with_pins(shared: &SharedVm, thread: &mut JvmThread) -> bool {
        // JIT r20 lane gcI: a LOWER bound of the live bytes pinned so far
        // (8 bytes per slot, headers ignored). Above the 8 MB `-Xmx` it proves
        // the heap admitted more live data than its maximum: a VM defect, not
        // a slow fill, so it fails loudly instead of reading as "never full".
        let mut pinned_lower_bound = 0usize;
        for _ in 0..64 {
            for full_heap in [false, true] {
                for slots in [1024usize, 64, 1] {
                    for _ in 0..1_000_000 {
                        let heap = &shared.mem.heap;
                        let got = if full_heap {
                            heap.try_alloc_object_full(ClassId::new(0), slots)
                        } else {
                            heap.try_alloc_object(ClassId::new(0), slots)
                        };
                        match got {
                            Some(o) => {
                                thread.native_pin_roots.push(o);
                                pinned_lower_bound += slots * 8;
                                assert!(
                                    pinned_lower_bound <= 8 * 1024 * 1024,
                                    "the 8 MB heap admitted more than 8 MB of pinned live data"
                                );
                            }
                            None => break,
                        }
                    }
                }
            }
            crate::runtime::interpreter::maybe_gc_forced_pub_at(shared, thread, "w9c-test-fill");
            // JIT r20 lane gcI: a probe that SUCCEEDS is pinned too. Left
            // unpinned it is garbage the next forced collection frees, which
            // re-opens exactly one object's room for the next probe: since the
            // old generation may grow past its 2/3 cap on a refused request
            // (`OldGen::grow_after_refusal`), the first post-growth probe
            // succeeds and the recipe oscillated on its own garbage for all
            // 64 passes. Pinned, every pass only ever adds live data, so the
            // fill is monotone and ends once the (bounded) heap is full.
            let heap = &shared.mem.heap;
            match heap.try_alloc_object(ClassId::new(0), 1) {
                Some(o) => thread.native_pin_roots.push(o),
                None => match heap.try_alloc_object_full(ClassId::new(0), 1) {
                    Some(o) => thread.native_pin_roots.push(o),
                    None => return true,
                },
            }
        }
        false
    }

    fn assert_java_heap_space<T: std::fmt::Debug>(got: Result<T, MethodCallFailed>) {
        match got {
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::OutOfMemoryError { message },
            ))) => assert!(message.starts_with("Java heap space"), "{message}"),
            Ok(v) => panic!("a pinned-full heap cannot hold it, got {v:?}"),
            Err(other) => panic!("expected OutOfMemoryError, got {other:?}"),
        }
    }

    /// A class id no mirror exists for. The mirror code does not require the
    /// class to be loaded (it names an unknown id `unknown_<n>`), and the id
    /// sits below the lambda-proxy range.
    const NO_MIRROR_YET: u32 = 0x0765_4321;

    /// A cached mirror is answered again, identical, without allocating.
    #[test]
    fn class_mirror_or_oom_answers_the_cached_mirror() {
        let shared = small_gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "w9c-mirror-cache");
        let id = ClassId::new(NO_MIRROR_YET);
        let a = class_mirror_or_oom(&shared, &mut thread, id, "w9c-test").expect("room");
        let b = class_mirror_or_oom(&shared, &mut thread, id, "w9c-test").expect("hit");
        assert_eq!(a, b);
        assert_eq!(
            shared.classes.class_mirrors.read().get(&id).copied(),
            Some(a)
        );
    }

    /// On a full heap the mirror is refused with `OutOfMemoryError` after the
    /// ladder, the cache is left without a row, and no pin is left behind.
    #[test]
    fn class_mirror_or_oom_on_a_full_heap_is_an_oome() {
        let shared = small_gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "w9c-mirror-full");
        // Prime `java/lang/Class` and the cached mirror width on an empty
        // heap, so the refusal below is the mirror's own allocation.
        class_mirror_or_oom(
            &shared,
            &mut thread,
            ClassId::new(NO_MIRROR_YET + 1),
            "w9c-prime",
        )
        .expect("an empty heap has room for a mirror");
        assert!(
            fill_heap_with_pins(&shared, &mut thread),
            "the pinned objects never filled the 8 MB heap"
        );
        let pins = thread.native_pin_roots.len();
        let id = ClassId::new(NO_MIRROR_YET);
        assert_java_heap_space(class_mirror_or_oom(&shared, &mut thread, id, "w9c-test"));
        assert!(shared.classes.class_mirrors.read().get(&id).is_none());
        assert_eq!(thread.native_pin_roots.len(), pins);
    }

    /// A surrogate-bearing String static argument (condy) on a full heap is
    /// refused with `OutOfMemoryError`; the process-wide surrogate pool is not
    /// touched, because the claim runs only after a successful allocation.
    #[test]
    fn a_wide_literal_on_a_full_heap_is_an_oome() {
        let shared = small_gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "w9c-wide-full");
        assert!(
            fill_heap_with_pins(&shared, &mut thread),
            "the pinned objects never filled the 8 MB heap"
        );
        // A lone high surrogate, then text no other test uses.
        let mut units: Vec<u16> = vec![0xD800];
        units.extend("w9c-wide-literal-on-a-full-heap".encode_utf16());
        assert_java_heap_space(intern_wide_string_literal_or_oom(
            &shared,
            &mut thread,
            &units,
        ));
        assert!(
            cratonvm_native_builtins::lang_string::surrogate_intern_probe(
                shared.vm_identity,
                &units
            )
            .is_none()
        );
    }

    /// The surrogate intern pool is per VM (applied `handoff-w9c-surrogate-intern-pool-per-vm`):
    /// each VM answers its own canonical object for a lone-surrogate literal,
    /// resolvable in its own global-ref table, and a released VM's rows are gone.
    #[test]
    fn a_wide_literal_is_canonical_per_vm_and_forgotten_with_its_vm() {
        let a = small_gen_vm();
        let b = small_gen_vm();
        let mut ta = JvmThread::new(ThreadId(0), "w9c-wide-a");
        let mut tb = JvmThread::new(ThreadId(0), "w9c-wide-b");
        let mut units: Vec<u16> = vec![0xD800];
        units.extend("w9c-wide-literal-per-vm".encode_utf16());
        let a1 = intern_wide_string_literal_or_oom(&a, &mut ta, &units).expect("room");
        let a2 = intern_wide_string_literal_or_oom(&a, &mut ta, &units).expect("pooled");
        let b1 = intern_wide_string_literal_or_oom(&b, &mut tb, &units).expect("room");
        let b2 = intern_wide_string_literal_or_oom(&b, &mut tb, &units).expect("pooled");
        assert_eq!(a1, a2, "canonical in VM A");
        assert_eq!(b1, b2, "canonical in VM B, not a fresh String per ldc");
        let probe = cratonvm_native_builtins::lang_string::surrogate_intern_probe;
        let ha = probe(a.vm_identity, &units).expect("A's row");
        let hb = probe(b.vm_identity, &units).expect("B's row");
        assert_eq!(
            a.natives
                .jni_global_refs
                .lock()
                .resolve(ha as crate::native::jni::JObject),
            Some(a1)
        );
        assert_eq!(
            b.natives
                .jni_global_refs
                .lock()
                .resolve(hb as crate::native::jni::JObject),
            Some(b1)
        );
        cratonvm_native_builtins::lang_string::surrogate_intern_forget_vm(a.vm_identity);
        assert!(probe(a.vm_identity, &units).is_none(), "A's rows go with A");
        assert_eq!(probe(b.vm_identity, &units), Some(hb), "B's rows stay");
        cratonvm_native_builtins::lang_string::surrogate_intern_forget_vm(b.vm_identity);
    }

    /// gcg g2/w item 2 (GC wave 9, lane gcD): `String.intern()` pools a
    /// lone-surrogate String under a WEAK handle. `ldc` upgrades a live weak
    /// row to a strong one on the same object (a literal lives with its
    /// class), and takes over a row whose weak handle no longer resolves
    /// (its String died) instead of answering a fresh String per execution.
    #[test]
    fn gcd_w9_a_wide_literal_upgrades_a_weak_row_and_replaces_a_dead_one() {
        use crate::native::jni::JObject;
        use cratonvm_native_builtins::lang_string::{
            surrogate_intern_claim, surrogate_intern_probe,
        };
        let shared = small_gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "gcd-w9-wide");
        let vm = shared.vm_identity;

        // A live weak row, as `intern()` leaves it.
        let mut units: Vec<u16> = vec![0xD800];
        units.extend("gcd-w9-wide-weak-row".encode_utf16());
        let interned = create_string_from_units_or_oom(&shared, &mut thread, &units).expect("room");
        let weak = shared.natives.jni_global_refs.lock().add_weak(interned) as usize;
        assert_eq!(surrogate_intern_claim(vm, units.clone(), weak), weak);
        let lit = intern_wide_string_literal_or_oom(&shared, &mut thread, &units).expect("pooled");
        assert_eq!(lit, interned, "the literal is the interned instance");
        let row = surrogate_intern_probe(vm, &units).expect("row");
        assert_ne!(row, weak, "the row was upgraded");
        let refs = shared.natives.jni_global_refs.lock();
        assert!(!refs.is_weak(row as JObject), "the literal's row is strong");
        assert_eq!(refs.resolve(row as JObject), Some(interned));
        assert_eq!(
            refs.resolve(weak as JObject),
            None,
            "the weak handle was released"
        );
        drop(refs);

        // A dead row: a handle that does not resolve, as a cleared weak row's
        // does not. A tagged value no allocation can produce, so a later
        // `add` cannot reuse it (releasing a real handle here would let the
        // allocator hand the same box address to the literal's own handle).
        let mut units2: Vec<u16> = vec![0xDC00];
        units2.extend("gcd-w9-wide-dead-row".encode_utf16());
        let dead: usize = 0x1_0001;
        assert_eq!(
            shared
                .natives
                .jni_global_refs
                .lock()
                .resolve(dead as JObject),
            None
        );
        assert_eq!(surrogate_intern_claim(vm, units2.clone(), dead), dead);
        let a = intern_wide_string_literal_or_oom(&shared, &mut thread, &units2).expect("room");
        let b = intern_wide_string_literal_or_oom(&shared, &mut thread, &units2).expect("pooled");
        assert_eq!(a, b, "one instance once the dead row is replaced");
        let row2 = surrogate_intern_probe(vm, &units2).expect("row");
        assert_ne!(row2, dead);
        assert_eq!(
            shared
                .natives
                .jni_global_refs
                .lock()
                .resolve(row2 as JObject),
            Some(a)
        );
        cratonvm_native_builtins::lang_string::surrogate_intern_forget_vm(vm);
    }

    /// The production parts of this file and of `invokedynamic.rs` call no
    /// infallible String or mirror constructor: each resolution allocation
    /// goes through an `_or_oom` helper. This is the page's confirmation grep
    /// as a ratchet. It bans a CALL anywhere above the test modules, so it
    /// survives restructuring; a `try_` form is not a match.
    #[test]
    fn resolution_paths_call_no_infallible_constructor() {
        // Line endings normalised, so a CRLF checkout splits the same way.
        fn production_part(src: &str) -> String {
            let src = src.replace("\r\n", "\n");
            src.split("\n#[cfg(test)]\nmod ")
                .next()
                .unwrap_or_default()
                .to_string()
        }
        fn infallible_calls(src: &str, needle: &str) -> usize {
            src.match_indices(needle)
                .filter(|(at, _)| !src[..*at].ends_with("try_"))
                .count()
        }
        let files = [
            (
                "constants.rs",
                production_part(include_str!("constants.rs")),
            ),
            (
                "invokedynamic.rs",
                production_part(include_str!("../invokedynamic.rs")),
            ),
        ];
        for (file, src) in &files {
            for needle in [
                "create_java_string(",
                "create_java_string_from_units(",
                "create_java_string_uninterned(",
                "get_or_create_class_mirror(",
                "get_or_create_primitive_mirror(",
                ".get_class_mirror(",
            ] {
                assert_eq!(
                    infallible_calls(src.as_str(), needle),
                    0,
                    "{file} calls `{needle}` outside its tests. A constant \
                     resolution runs per entry after start-up, so it must \
                     collect, retry and throw OutOfMemoryError instead \
                     (`intern_string_literal_or_oom`, \
                     `intern_wide_string_literal_or_oom`, \
                     `create_string_from_units_or_oom`, `class_mirror_or_oom`, \
                     `primitive_mirror_or_oom`)"
                );
            }
        }
    }
}

#[cfg(test)]
mod i9_l2_class_access_tests {
    //! Interpreter round i1, wave 9, lane L2: JVMS §5.4.3.1 / §5.4.4 class
    //! access on every `CONSTANT_Class` resolution, not only `new`
    //! (`docs/internal/fixed-bugs/interpreter-L2-class-access-checked-only-by-new-FIXED-20260925.md`).
    use super::{
        check_class_constant_access, class_constant_access_denial_in, class_constant_fill_admitted,
        recorded_resolution_failure,
    };
    use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
    use crate::error::MethodCallFailed;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::SharedVm;
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    fn jdk_only_vm() -> SharedVm {
        let mut config = crate::config::VmConfig::default();
        config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
        // `JdkOnly` with the synthetic library is a rejected pair.
        config.use_synthetic_jdk = false;
        SharedVm::new(config)
    }

    /// Define and register a field-less, method-less class.
    fn define(
        shared: &SharedVm,
        name: &str,
        loader: ClassLoaderId,
        access_flags: ClassAccessFlags,
        hidden: bool,
    ) -> ClassId {
        let mut cm = shared.classes.class_manager.write();
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            loader_id: loader,
            name: cratonvm_types::intern_arc(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Initialized,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden,
            module_name: None,
            origin: cratonvm_classloading::ClassOrigin::default(),
            has_finalizer: false,
            code_source: None,
            array_info: None,
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        });
        if !hidden {
            cm.register_class_name(loader, name, id);
        }
        id
    }

    const PKG_PRIVATE: &str = "cratonvm/i9l2/p/Hidden";
    const PUBLIC: &str = "cratonvm/i9l2/p/Open";

    /// `(accessor in another package, package-private target, public target)`.
    fn fixture(shared: &SharedVm) -> (ClassId, ClassId, ClassId) {
        let app = ClassLoaderId::Application;
        let accessor = define(
            shared,
            "cratonvm/i9l2/q/X",
            app,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            false,
        );
        let hidden = define(shared, PKG_PRIVATE, app, ClassAccessFlags::SUPER, false);
        let open = define(
            shared,
            PUBLIC,
            app,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            false,
        );
        (accessor, hidden, open)
    }

    /// The verdict: a package-private class of another package is refused
    /// with HotSpot's text, for the class itself and as the bottom element of
    /// an array name at any depth; a public class, a primitive array and a
    /// self-reference are admitted.
    #[test]
    fn the_verdict_checks_the_bottom_element_class() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let (accessor, hidden, open) = fixture(&shared);
        let cm = shared.classes.class_manager.read();
        let denial =
            class_constant_access_denial_in(&shared, &cm, accessor, PKG_PRIVATE, Some(hidden));
        assert_eq!(
            denial.as_deref(),
            Some(
                "failed to access class cratonvm.i9l2.p.Hidden from class cratonvm.i9l2.q.X \
                 (cratonvm.i9l2.p.Hidden and cratonvm.i9l2.q.X are in unnamed module of \
                 loader 'app')"
            )
        );
        for array in ["[Lcratonvm/i9l2/p/Hidden;", "[[[Lcratonvm/i9l2/p/Hidden;"] {
            assert!(
                class_constant_access_denial_in(&shared, &cm, accessor, array, None).is_some(),
                "{array}"
            );
        }
        assert_eq!(
            class_constant_access_denial_in(&shared, &cm, accessor, PUBLIC, Some(open)),
            None
        );
        assert_eq!(
            class_constant_access_denial_in(
                &shared,
                &cm,
                accessor,
                "[Lcratonvm/i9l2/p/Open;",
                None
            ),
            None
        );
        assert_eq!(
            class_constant_access_denial_in(&shared, &cm, accessor, "[[I", None),
            None
        );
        assert_eq!(
            class_constant_access_denial_in(&shared, &cm, hidden, PKG_PRIVATE, Some(hidden)),
            None,
            "a class always accesses itself"
        );
        // Not loaded through the referencing loader: no verdict, never a
        // guessed refusal.
        assert_eq!(
            class_constant_access_denial_in(&shared, &cm, accessor, "cratonvm/i9l2/p/Gone", None),
            None
        );
    }

    /// HotSpot admits every class access from a subclass of
    /// `jdk.internal.reflect.SerializationConstructorAccessorImpl` (the JDK's
    /// generated serialization constructor accessors); so does the verdict.
    #[test]
    fn a_serialization_constructor_accessor_reaches_any_class() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let (_, hidden, _) = fixture(&shared);
        let flags = ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER;
        let base = define(
            &shared,
            "jdk/internal/reflect/SerializationConstructorAccessorImpl",
            ClassLoaderId::Bootstrap,
            flags,
            false,
        );
        let generated = define(
            &shared,
            "jdk/internal/reflect/GeneratedSerializationConstructorAccessor1",
            ClassLoaderId::Bootstrap,
            flags,
            false,
        );
        let unrelated = define(
            &shared,
            "jdk/internal/reflect/GeneratedMethodAccessor1",
            ClassLoaderId::Bootstrap,
            flags,
            false,
        );
        shared
            .classes
            .class_manager
            .write()
            .set_superclass(generated, Some(base));
        let cm = shared.classes.class_manager.read();
        assert_eq!(
            class_constant_access_denial_in(&shared, &cm, generated, PKG_PRIVATE, Some(hidden)),
            None
        );
        assert!(
            class_constant_access_denial_in(&shared, &cm, unrelated, PKG_PRIVATE, Some(hidden))
                .is_some(),
            "control: the exemption is the superclass, not the package"
        );
        drop(cm);

        // Wave 26 (lane L5): the exemption is THE bootstrap class, not its
        // name. A user loader's own class under that binary name buys its
        // subclasses nothing (probe `L5W26SerializationAccessorSpoof`).
        let spoof_loader = ClassLoaderId::UserDefined(5);
        let spoof_base = define(
            &shared,
            "jdk/internal/reflect/SerializationConstructorAccessorImpl",
            spoof_loader,
            flags,
            false,
        );
        let evil = define(&shared, "l5sp/Evil", spoof_loader, flags, false);
        shared
            .classes
            .class_manager
            .write()
            .set_superclass(evil, Some(spoof_base));
        let cm = shared.classes.class_manager.read();
        assert!(
            class_constant_access_denial_in(&shared, &cm, evil, PKG_PRIVATE, Some(hidden))
                .is_some(),
            "a same-named class of a user loader is not the exempt class"
        );
        assert_eq!(
            class_constant_access_denial_in(&shared, &cm, generated, PKG_PRIVATE, Some(hidden)),
            None,
            "the bootstrap class's subclasses stay exempt"
        );
    }

    /// Interpreter round i1 wave 27 (lane L5): a user loader's OWN class under
    /// a JDK-looking name that java.base also has is what that loader answers
    /// for the name; another loader with no record of it gets `None` (the
    /// global route), then its memoised `loadClass` answer. `java/` names are
    /// never a user loader's (probe `L5W27JdkNamedOwnClass`).
    #[test]
    fn java_name_initiation_survives_memo_eviction() {
        const NAME: &str = "java/util/concurrent/LinkedTransferQueue";
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let loader = ClassLoaderId::UserDefined(38);
        let id = define(
            &shared,
            NAME,
            ClassLoaderId::Bootstrap,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            false,
        );
        crate::runtime::resolve::initiating_records::record_initiating_load(
            &shared, loader, NAME, id,
        );
        super::cache_loader_initiated(&shared, loader, NAME, id);
        // Force the capped cache to lose precisely this row. The dictionary
        // must still prevent another loadClass call and answer compile peeks.
        shared
            .classes
            .initiating_resolution_cache
            .write()
            .get_mut(&loader)
            .unwrap()
            .remove(NAME);
        assert_eq!(
            super::loader_own_record_of_global_name(&shared, loader, NAME),
            Some(id)
        );
        assert_eq!(
            crate::runtime::resolve::initiating_records::loader_initiated_answer(
                &shared, loader, NAME,
            ),
            Some(id)
        );
    }

    /// Interpreter round i6 wave 133 (lane L5): a `javax/` name a user loader
    /// answered with the JDK's class is in its initiating record
    /// (`CRATONVM_GLOBAL_NAME_INITIATING_RECORD`, default on), which the
    /// `findLoadedClass` natives read; its own definition of such a name is
    /// not a record row (probe `L5W125GlobalNameRefusalRecord`).
    #[test]
    fn a_jdk_global_name_answer_is_an_initiating_record_row() {
        const NAME: &str = "javax/net/SocketFactory";
        const OWN: &str = "javax/net/ssl/L5W133OwnName";
        let shared = SharedVm::new(crate::config::VmConfig::default());
        if !shared.config.global_name_initiating_record || !shared.classes.initiating_records_on {
            return; // `=0` in the test environment: nothing to check.
        }
        let flags = ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER;
        let jdk = define(&shared, NAME, ClassLoaderId::Bootstrap, flags, false);
        let loader = ClassLoaderId::UserDefined(133);
        let own = define(&shared, OWN, loader, flags, false);
        crate::runtime::resolve::initiating_records::record_initiating_load(
            &shared, loader, NAME, jdk,
        );
        crate::runtime::resolve::initiating_records::record_initiating_load(
            &shared, loader, OWN, own,
        );
        assert_eq!(
            crate::runtime::resolve::initiating_records::initiated_class(&shared, loader, NAME),
            Some(jdk)
        );
        assert_eq!(
            crate::runtime::resolve::initiating_records::initiated_class(&shared, loader, OWN),
            None,
            "the loader's own definition is its namespace already"
        );
        assert_eq!(
            crate::runtime::resolve::initiating_records::define_refusal(&shared, loader, NAME),
            None,
            "the define refusal still reads the recordable names only"
        );
    }

    #[test]
    fn a_user_loaders_own_jdk_named_class_is_its_answer_for_the_name() {
        const NAME: &str = "javax/security/auth/x500/X500PrivateCredential";
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let flags = ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER;
        let jdk = define(&shared, NAME, ClassLoaderId::Bootstrap, flags, false);
        let a = ClassLoaderId::UserDefined(31);
        let b = ClassLoaderId::UserDefined(32);
        let own_a = define(&shared, NAME, a, flags, false);
        assert!(super::is_user_loader_global_name(NAME));
        assert!(super::is_user_loader_global_name(
            "jdk/internal/reflect/L5Helper"
        ));
        assert!(super::is_user_loader_global_name("java/lang/String"));
        assert!(!super::is_user_loader_global_name("com/example/Foo"));
        assert_eq!(
            super::loader_own_record_of_global_name(&shared, a, NAME),
            Some(own_a),
            "the loader's own definition, not java.base's"
        );
        assert_eq!(
            super::loader_own_record_of_global_name(&shared, b, NAME),
            None,
            "no record: the caller takes the global route"
        );
        super::cache_loader_initiated(&shared, b, NAME, jdk);
        assert_eq!(
            super::loader_own_record_of_global_name(&shared, b, NAME),
            Some(jdk),
            "a memoised loadClass answer"
        );
    }

    /// Re-recording the answer the memo already holds moves no epoch (a re-ask
    /// of a name the loader answered before must not retire every per-thread
    /// site entry); a new row or a different answer does.
    #[test]
    fn re_recording_a_held_initiating_answer_does_not_move_the_resolution_epoch() {
        const NAME: &str = "com/example/EpochProbe";
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let flags = ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER;
        let a = define(&shared, NAME, ClassLoaderId::Application, flags, false);
        let b = define(
            &shared,
            "com/example/EpochProbe2",
            ClassLoaderId::Application,
            flags,
            false,
        );
        let loader = ClassLoaderId::UserDefined(41);
        let before = super::resolution_epoch_in(&shared);
        super::cache_loader_initiated(&shared, loader, NAME, a);
        let after_new = super::resolution_epoch_in(&shared);
        assert!(after_new > before, "a new row moves the epoch");
        super::cache_loader_initiated(&shared, loader, NAME, a);
        super::cache_loader_initiated(&shared, loader, NAME, a);
        assert_eq!(
            super::resolution_epoch_in(&shared),
            after_new,
            "the same answer again changes nothing a cached resolution read"
        );
        super::cache_loader_initiated(&shared, loader, NAME, b);
        assert!(
            super::resolution_epoch_in(&shared) > after_new,
            "a different answer moves it"
        );
    }

    /// A hidden class (stored as `<name>/0x<hex>`) is in the runtime package
    /// of its class-file name, so a lambda-proxy-shaped class reaches its
    /// host package's package-private classes.
    #[test]
    fn a_hidden_class_reaches_its_hosts_package() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let (_, hidden, _) = fixture(&shared);
        let proxy = define(
            &shared,
            "cratonvm/i9l2/p/Host$$Lambda/0x1f",
            ClassLoaderId::Application,
            ClassAccessFlags::FINAL | ClassAccessFlags::SUPER,
            true,
        );
        let cm = shared.classes.class_manager.read();
        assert_eq!(
            class_constant_access_denial_in(&shared, &cm, proxy, PKG_PRIVATE, Some(hidden)),
            None
        );
    }

    /// i10-L2: `--compatible` refuses too (the wave-9 census read zero
    /// admissions there), and a catch row may no longer fill the entry.
    #[test]
    fn compatible_mode_refuses_too() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        assert!(
            !shared.config.is_jdk_only(),
            "the default config is --compatible"
        );
        let (accessor, hidden, open) = fixture(&shared);
        let mut thread = JvmThread::new(ThreadId(0), "test");
        assert!(check_class_constant_access(
            &shared,
            &mut thread,
            accessor,
            7,
            PKG_PRIVATE,
            Some(hidden),
            "checkcast",
        )
        .is_err());
        assert!(check_class_constant_access(
            &shared,
            &mut thread,
            accessor,
            8,
            "[Lcratonvm/i9l2/p/Hidden;",
            None,
            "instanceof",
        )
        .is_err());
        assert!(!class_constant_fill_admitted(
            &shared,
            accessor,
            PKG_PRIVATE,
            hidden
        ));
        // Control: an accessible class is still admitted and may fill.
        assert!(check_class_constant_access(
            &shared,
            &mut thread,
            accessor,
            9,
            PUBLIC,
            Some(open),
            "ldc",
        )
        .is_ok());
        assert!(class_constant_fill_admitted(
            &shared, accessor, PUBLIC, open
        ));
    }

    /// `--jdk-only` refuses, as a Java throwable (never a silent admission),
    /// and keeps admitting what is accessible, the array-target form included.
    #[test]
    fn jdk_only_refuses_an_inaccessible_class_constant() {
        let shared = jdk_only_vm();
        let (accessor, hidden, open) = fixture(&shared);
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let refused = check_class_constant_access(
            &shared,
            &mut thread,
            accessor,
            7,
            PKG_PRIVATE,
            Some(hidden),
            "anewarray",
        );
        match refused {
            Err(MethodCallFailed::ExceptionThrown(exc)) => {
                let cid = shared.mem.heap.class_id_of(exc);
                // The stripped unit VM may lack the real hierarchy: name the
                // class only when it is there, and expect the JVMS §5.4.3
                // record only when it is a `LinkageError` by the hierarchy.
                let (name, is_linkage) = {
                    let cm = shared.classes.class_manager.read();
                    let name = cm.get_class(cid).map(|c| c.name.to_string());
                    let is_linkage = cm
                        .get_loaded_class_id("java/lang/LinkageError")
                        .is_some_and(|linkage| cm.is_subclass_of(cid, linkage));
                    (name, is_linkage)
                };
                if is_linkage
                    && name.as_deref() == Some("java/lang/IllegalAccessError")
                    && super::resolution_failure_record_enabled(&shared)
                {
                    assert!(
                        recorded_resolution_failure(&shared, &mut thread, accessor, 7).is_some()
                    );
                }
            }
            Err(MethodCallFailed::InternalError(_)) => {
                // The stripped unit VM could not build the throwable; the
                // refusal itself is what this test pins.
            }
            Ok(()) => panic!("--jdk-only must refuse a package-private class of another package"),
        }
        assert!(check_class_constant_access(
            &shared,
            &mut thread,
            accessor,
            9,
            "[[Lcratonvm/i9l2/p/Hidden;",
            None,
            "checkcast",
        )
        .is_err());
        assert!(!class_constant_fill_admitted(
            &shared,
            accessor,
            PKG_PRIVATE,
            hidden
        ));
        for _ in 0..2 {
            assert!(check_class_constant_access(
                &shared,
                &mut thread,
                accessor,
                10,
                PUBLIC,
                Some(open),
                "ldc",
            )
            .is_ok());
            assert!(check_class_constant_access(
                &shared,
                &mut thread,
                accessor,
                11,
                "[Lcratonvm/i9l2/p/Open;",
                None,
                "instanceof",
            )
            .is_ok());
        }
    }

    /// Register `name` as an array class over `component` with `array_info`,
    /// the way `synthesize_array_class_for_loader` records it.
    fn define_array(shared: &SharedVm, name: &str, component: ClassId, dimension: u8) -> ClassId {
        let id = define(
            shared,
            name,
            ClassLoaderId::Application,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::FINAL,
            false,
        );
        let mut cm = shared.classes.class_manager.write();
        if let Some(class) = cm.class_store.get_mut(id) {
            class.array_info = Some(cratonvm_classloading::ArrayInfo {
                component_class_id: component,
                array_dimension: dimension,
                leaf_component_name: cratonvm_types::intern_arc(PKG_PRIVATE),
            });
        }
        id
    }

    /// i11-L2: a RESOLVED array class names its bottom element by id
    /// (`array_info`), so the verdict reads the class the resolution loaded
    /// rather than looking the element name up a second time; a chain that
    /// ends at a class of another name is not trusted.
    #[test]
    fn a_resolved_array_names_its_bottom_element_by_id() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let (accessor, hidden, _) = fixture(&shared);
        let one = define_array(&shared, "[Lcratonvm/i9l2/p/Hidden;", hidden, 1);
        let two = define_array(&shared, "[[Lcratonvm/i9l2/p/Hidden;", one, 2);
        let cm = shared.classes.class_manager.read();
        assert_eq!(
            super::array_bottom_element_class(&cm, two, PKG_PRIVATE),
            Some(hidden)
        );
        assert_eq!(
            super::array_bottom_element_class(&cm, two, PUBLIC),
            None,
            "a chain that ends at another class is not the element"
        );
        assert!(class_constant_access_denial_in(
            &shared,
            &cm,
            accessor,
            "[[Lcratonvm/i9l2/p/Hidden;",
            Some(two)
        )
        .is_some());
    }
}

/// Interpreter round i1 wave 15, lane L4: a bootstrap method inherited by the
/// class its `CONSTANT_MethodHandle` names initializes only the class that
/// declares it (JVMS §5.5), on the condy and the generic indy route
/// (`interpreter-L5-bootstrap-method-calls-initialize-the-symbolic-owner-FIXED-20260925.md`,
/// probe `tools/probes/interp/L7/L7BootstrapInitializesDeclaringClass.java`).
#[cfg(test)]
mod i15_l4_bootstrap_init_tests {
    use super::initialize_bootstrap_declaring_class;
    use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
    use crate::config::VmConfig;
    use crate::error::MethodCallResult;
    use crate::native::registry::{NativeCallback, NativeContext};
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::types::Value;
    use crate::vm::SharedVm;
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    use cratonvm_reader::method::ClassFileMethod;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    const BSM_DESC: &str =
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;";

    static PARENT_CLINIT: AtomicU32 = AtomicU32::new(0);
    static CHILD_CLINIT: AtomicU32 = AtomicU32::new(0);

    fn parent_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        PARENT_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }

    fn child_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        CHILD_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }

    fn method(name: &str, descriptor: &str, flags: MethodAccessFlags) -> ClassFileMethod {
        ClassFileMethod {
            name: Arc::from(name),
            descriptor: Arc::from(descriptor),
            access_flags: flags,
            attributes: vec![],
        }
    }

    /// Define `name` as a LOADED (not initialized) application class.
    fn define(
        shared: &SharedVm,
        name: &str,
        superclass: Option<ClassId>,
        methods: Vec<ClassFileMethod>,
    ) -> ClassId {
        let mut cm = shared.classes.class_manager.write();
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods,
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: cratonvm_classloading::ClassOrigin::ApplicationClassPath {
                source: Arc::from("i15-l4-test"),
            },
            has_finalizer: false,
            code_source: None,
            array_info: None,
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
            init_state: Arc::new(std::sync::atomic::AtomicU8::new(0)),
        });
        cm.register_class_name(ClassLoaderId::Application, name, id);
        if superclass.is_some() {
            cm.set_superclass(id, superclass);
        }
        id
    }

    /// `C.bsm` for a static `bsm` declared in `P` runs `P.<clinit>` only; a
    /// bootstrap `C` declares itself initializes `C`.
    #[test]
    fn an_inherited_bootstrap_initializes_only_its_declaring_class() {
        const PARENT: &str = "cratonvm/i15l4/BsmParent";
        const CHILD: &str = "cratonvm/i15l4/BsmChild";
        let mut vm = SharedVm::new(VmConfig::default());
        let clinits: [(&str, NativeCallback); 2] = [(PARENT, parent_clinit), (CHILD, child_clinit)];
        for (owner, callback) in clinits {
            vm.natives.native_methods.register_with_kind(
                owner,
                "<clinit>",
                "()V",
                callback,
                cratonvm_native_api::NativeKind::Bridge,
            );
        }
        let shared = Arc::new(vm);
        let clinit = || {
            method(
                "<clinit>",
                "()V",
                MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE,
            )
        };
        // Native, so the link step's verifier (which wave 15 runs before the
        // superclass is initialized) accepts a method with no Code attribute;
        // the test never calls them.
        let public_static =
            MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE;
        let parent = define(
            &shared,
            PARENT,
            None,
            vec![clinit(), method("bsm", BSM_DESC, public_static)],
        );
        let child = define(
            &shared,
            CHILD,
            Some(parent),
            vec![clinit(), method("own", BSM_DESC, public_static)],
        );
        let initialized = |id| crate::vm::is_class_initialized_via_manager(&shared, id);
        let mut thread = JvmThread::new(ThreadId(0), "i15-l4-bsm");

        let inherited =
            initialize_bootstrap_declaring_class(&shared, &mut thread, child, "bsm", BSM_DESC);
        assert!(inherited.is_ok(), "{inherited:?}");
        assert_eq!(PARENT_CLINIT.load(Ordering::SeqCst), 1);
        assert_eq!(
            CHILD_CLINIT.load(Ordering::SeqCst),
            0,
            "an inherited bootstrap never initializes the class it is named through"
        );
        assert!(initialized(parent));
        assert!(!initialized(child));

        let own =
            initialize_bootstrap_declaring_class(&shared, &mut thread, child, "own", BSM_DESC);
        assert!(own.is_ok(), "{own:?}");
        assert_eq!(
            CHILD_CLINIT.load(Ordering::SeqCst),
            1,
            "a bootstrap C declares"
        );
        assert!(initialized(child));
    }

    /// Both bootstrap routes use the rule: the condy route through
    /// `initialize_bootstrap_declaring_class`, the generic indy route through
    /// `crate::vm::invoke_static_shared` for a `REF_invokeStatic` bootstrap. A
    /// text pin, because either route needs `MethodHandles.lookup()`, which the
    /// unit-test VM cannot run.
    #[test]
    fn both_bootstrap_routes_initialize_the_declaring_class() {
        fn production_flat(src: &str) -> String {
            let src = src.replace("\r\n", "\n");
            src.split("\n#[cfg(test)]\nmod ")
                .next()
                .unwrap_or_default()
                .chars()
                .filter(|c| !c.is_ascii_whitespace())
                .collect()
        }
        let condy = production_flat(include_str!("constants.rs"));
        assert!(condy.contains("initialize_bootstrap_declaring_class(shared,thread,owner,"));
        assert!(
            !condy.contains("ensure_class_initialized_shared(shared,thread,owner)?;"),
            "the condy bootstrap must not initialize the named class"
        );
        let indy = production_flat(include_str!("../invokedynamic.rs"));
        let start = indy
            .find("fnlink_generic_call_site(")
            .expect("the generic indy linker");
        let linker = &indy[start..];
        assert!(linker.contains("h.kind==MethodHandleKind::InvokeStatic"));
        assert!(linker.contains("ifbsm_is_static{crate::vm::invoke_static_shared("));
    }
}

/// gci i3/jd: F1 of
/// `docs/internal/gc/gci-i1orch-concurrent-in-process-javac-first-round-differs-FIXED-20261017.md`
/// (`method_type_no_loader_resolves_in_vm`, default OFF).
#[cfg(test)]
mod gci_i3jd_method_type_no_loader_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::ThreadId;

    /// The walk's production text, whitespace removed.
    fn walk_text() -> String {
        let src: String = include_str!("constants.rs")
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        let start = src.find("fnmethod_type_walk(").expect("the walk");
        let end = src[start..]
            .find("fnmethod_type_failure_as_linkage_error(")
            .expect("the walk's end");
        src[start..start + end].to_string()
    }

    /// With the switch on, a class that reports no loader never reaches the
    /// bootstrap `forName0` for a type: its arm runs first, resolves through
    /// the class's own namespace and goes on to the next type.
    #[test]
    fn the_no_loader_arm_runs_before_the_bootstrap_for_name0() {
        let walk = walk_text();
        let arm = walk
            .find("ifloader_slot.is_none()&&method_type_no_loader_resolves_in_vm(){")
            .expect("the no-loader arm");
        let for_name0 = walk.find("\"forName0\"").expect("the forName0 ask");
        assert!(arm < for_name0, "the arm must come before the forName0 ask");
        let body = &walk[arm..for_name0];
        assert!(body.contains(
            "class_mirror_for_field_descriptor(shared,thread,frame_class_id,desc)?"
        ));
        assert!(body.contains("slots.push(thread.native_pin_roots.len()-1);continue;"));
    }

    /// The arm's miss is the `NoClassDefFoundError` HotSpot's method-type
    /// resolution raises, never the bootstrap loader's checked
    /// `ClassNotFoundException` (whose escape out of an `invokedynamic` is the
    /// page's crash).
    #[test]
    fn the_no_loader_arms_miss_is_a_no_class_def_found_error() {
        let shared = SharedVm::new(VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(0), "gci-i3jd");
        let missed = class_mirror_for_field_descriptor(
            &shared,
            &mut thread,
            ClassId::new(0),
            "Lcratonvm/test/gci/i3jd/NoSuchType;",
        );
        let Err(MethodCallFailed::ExceptionThrown(obj)) = missed else {
            // Stripped VM (no class library on the test class path): nothing
            // Java-visible to check.
            return;
        };
        let name = {
            let cm = shared.classes.class_manager.read();
            cm.get_class(shared.mem.heap.class_id_of(obj))
                .map(|c| c.name.to_string())
        };
        assert_eq!(name.as_deref(), Some("java/lang/NoClassDefFoundError"));
    }
}

/// gci i4/wr (`gcg-g2w-string-table-review-findings` item 2): a row whose
/// handle no longer resolves is replaced rather than answering a fresh String
/// forever. (Since the 2026-10-15 merge of origin/dev a literal's row is
/// strong under either pool; see `strong_wide_string_literal`.)
#[cfg(test)]
mod gci_i4wr_surrogate_pool_weak_tests {
    use super::w9c_resolution_oom_tests::small_gen_vm;
    use super::*;
    use crate::native::jni::{JObject, JniGlobalKind};
    use crate::threading::jvm_thread::ThreadId;

    #[test]
    fn gci_i4wr_a_wide_literal_row_is_weak_and_a_dead_row_is_replaced() {
        let shared = small_gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "gci-i4wr-wide");
        let mut units: Vec<u16> = vec![0xD800];
        units.extend("gci-i4wr-wide-literal-weak-row".encode_utf16());
        let probe = cratonvm_native_builtins::lang_string::surrogate_intern_probe;

        let first = intern_wide_string_literal_or_oom(&shared, &mut thread, &units).expect("room");
        let h1 = probe(shared.vm_identity, &units).expect("the row");
        let kind = shared.natives.jni_global_refs.lock().kind(h1 as JObject);
        // Merge of origin/dev (2026-10-15): the literal's row is STRONG under
        // either pool (dev's gcg g2/w item 2 design, `strong_wide_string_literal`;
        // only an `intern()` row is weak). The name is kept for the citations.
        assert_eq!(kind, Some(JniGlobalKind::Strong), "a literal's row is strong");
        assert_eq!(
            intern_wide_string_literal_or_oom(&shared, &mut thread, &units).expect("pooled"),
            first,
            "canonical while it lives"
        );

        // A row whose handle no longer resolves (released here; a collected
        // weak referent resolves to `None` the same way).
        assert!(shared.natives.jni_global_refs.lock().remove(h1 as JObject));
        let again = intern_wide_string_literal_or_oom(&shared, &mut thread, &units).expect("room");
        let h2 = probe(shared.vm_identity, &units).expect("the row");
        // The global-ref table may hand the freed slot back, so `h2 == h1` is
        // allowed: what matters is that the row is live again and names the
        // new answer (below).
        assert!(
            shared.natives.jni_global_refs.lock().kind(h2 as JObject).is_some(),
            "the dead row was replaced"
        );
        assert_eq!(
            shared.natives.jni_global_refs.lock().resolve(h2 as JObject),
            Some(again),
            "the new row names the answer"
        );
        assert_eq!(
            intern_wide_string_literal_or_oom(&shared, &mut thread, &units).expect("pooled"),
            again,
            "canonical again after the replacement"
        );
        cratonvm_native_builtins::lang_string::surrogate_intern_forget_vm(shared.vm_identity);
    }

    /// The keep-alive resolver answers what the table answers, for strong and
    /// weak handles alike, and `None` for a released one.
    #[test]
    fn gci_i4wr_the_keep_alive_resolver_answers_like_the_table() {
        let shared = small_gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "gci-i4wr-resolve");
        let mut units: Vec<u16> = vec![0xD801];
        units.extend("gci-i4wr-keep-alive-resolver".encode_utf16());
        let obj = create_string_from_units_or_oom(&shared, &mut thread, &units).expect("room");
        let strong = shared.natives.jni_global_refs.lock().add(obj) as usize;
        let weak = shared.natives.jni_global_refs.lock().add_weak(obj) as usize;
        let resolve = crate::native::jni::resolve_global_handle_keeping_weak_alive;
        assert_eq!(resolve(&shared, strong), Some(obj));
        assert_eq!(resolve(&shared, weak), Some(obj));
        assert!(shared.natives.jni_global_refs.lock().remove(weak as JObject));
        assert_eq!(resolve(&shared, weak), None, "released");
        assert!(shared.natives.jni_global_refs.lock().remove(strong as JObject));
    }
}

#[cfg(test)]
mod gci_i32sl_surrogate_literal_class_held_tests {
    //! gci i32/sl (`gci-i20mr-merge-review-findings-FIXED-20261016` finding 10): a
    //! lone-surrogate literal's pool row stays WEAK and its class's
    //! never-evicted `ldc` record holds the literal, so the literal dies with
    //! its class (HotSpot's resolved references) instead of living for the
    //! VM's life. `CRATONVM_GC_SURROGATE_LITERAL_CLASS_HELD=0` keeps the
    //! strong row.
    use super::w9c_resolution_oom_tests::small_gen_vm;
    use super::*;
    use crate::native::jni::{JObject, JniGlobalKind};
    use crate::threading::jvm_thread::ThreadId;

    #[test]
    fn gci_i32sl_a_class_held_literal_keeps_its_row_weak() {
        use cratonvm_native_builtins::lang_string::{
            surrogate_intern_claim, surrogate_intern_forget_vm, surrogate_intern_probe,
        };
        let shared = small_gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "gci-i32sl-weak-row");
        let vm = shared.vm_identity;
        let kind = |h: usize| shared.natives.jni_global_refs.lock().kind(h as JObject);

        // A fresh literal files a WEAK row, and a second `ldc` answers it
        // without touching the row.
        let mut units: Vec<u16> = vec![0xD800];
        units.extend("gci-i32sl-class-held-claim".encode_utf16());
        let first =
            intern_wide_string_literal_or_oom_as(&shared, &mut thread, &units, true).expect("room");
        let h = surrogate_intern_probe(vm, &units).expect("the row");
        assert_eq!(kind(h), Some(JniGlobalKind::Weak), "a class-held row is weak");
        assert_eq!(
            intern_wide_string_literal_or_oom_as(&shared, &mut thread, &units, true)
                .expect("pooled"),
            first,
            "canonical while it lives"
        );
        assert_eq!(surrogate_intern_probe(vm, &units), Some(h), "a hit changes no row");

        // An `intern()` row (weak) is answered as it is, never upgraded.
        let mut units2: Vec<u16> = vec![0xDC00];
        units2.extend("gci-i32sl-class-held-intern-row".encode_utf16());
        let interned =
            create_string_from_units_or_oom(&shared, &mut thread, &units2).expect("room");
        let weak = shared.natives.jni_global_refs.lock().add_weak(interned) as usize;
        assert_eq!(surrogate_intern_claim(vm, units2.clone(), weak), weak);
        let lit = intern_wide_string_literal_or_oom_as(&shared, &mut thread, &units2, true)
            .expect("pooled");
        assert_eq!(lit, interned, "the literal is the interned instance");
        assert_eq!(surrogate_intern_probe(vm, &units2), Some(weak), "not upgraded");
        assert_eq!(kind(weak), Some(JniGlobalKind::Weak));

        // Positive control: the kill-switch arm (and a condy argument) upgrades.
        let strong = intern_wide_string_literal_or_oom_as(&shared, &mut thread, &units2, false)
            .expect("pooled");
        assert_eq!(strong, interned);
        let row = surrogate_intern_probe(vm, &units2).expect("the row");
        assert_ne!(row, weak, "the strong arm swapped the row");
        assert_eq!(kind(row), Some(JniGlobalKind::Strong));
        surrogate_intern_forget_vm(vm);
    }

    /// The literal's record goes to the never-evicted store whatever the
    /// String table's disposition (the weak table off here: the G1 / ZGC
    /// default), answers the `ldc` probe, and leaves with its class
    /// (`ResolutionCache::forget_classes`, the unload transaction's step).
    /// The kill-switch arm records as before (nothing, with the table off,
    /// beyond the ordinary FIFO record when the cache is on).
    #[test]
    fn gci_i32sl_the_class_record_holds_the_literal_until_its_class_unloads() {
        let shared = small_gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "gci-i32sl-record");
        shared.mem.string_table.set_weak(false);
        let mut units: Vec<u16> = vec![0xD801];
        units.extend("gci-i32sl-class-record".encode_utf16());
        let lit =
            intern_wide_string_literal_or_oom_as(&shared, &mut thread, &units, true).expect("room");
        let value = Value::Object(Some(lit));
        let as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
        let permanent = || MemberResolver::new(&shared).permanent_constant_count();
        let (cls, other, idx) = (ClassId::new(9_321), ClassId::new(9_322), 7u16);

        let before = permanent();
        record_wide_string_literal_as_of(&shared, other, idx, value, as_of, false);
        assert_eq!(permanent(), before, "kill-switch arm: the ordinary rule");

        let pushed = record_wide_string_literal_as_of(&shared, cls, idx, value, as_of, true);
        assert_eq!(pushed, value, "the record's (first) value is pushed");
        assert_eq!(permanent(), before + 1, "class-held: the never-evicted store");
        assert_eq!(cached_cp_constant(&shared, cls, idx), Some(value));
        let again = record_wide_string_literal_as_of(&shared, cls, idx, value, as_of, true);
        assert_eq!(again, value);
        assert_eq!(permanent(), before + 1, "insert-if-absent");

        MemberResolver::new(&shared).forget_class_records(cls);
        assert_eq!(cached_cp_constant(&shared, cls, idx), None, "gone with its class");
        assert_eq!(permanent(), before);
        cratonvm_native_builtins::lang_string::surrogate_intern_forget_vm(shared.vm_identity);
    }

    /// `ldc` takes the class-held form and records through
    /// `record_wide_string_literal_as_of`; the condy static argument keeps the
    /// strong form only under a kill switch (gci i33/sc: it records at its own
    /// entry through `condy_wide_string_arg`).
    #[test]
    fn gci_i32sl_ldc_takes_the_class_held_form() {
        let src = include_str!("constants.rs").replace("\r\n", "\n");
        let prod = src
            .split("\n#[cfg(test)]\nmod ")
            .next()
            .unwrap_or_default()
            .to_string();
        let arm = prod
            .split("LdcValue::WideStr(units) => {")
            .nth(1)
            .and_then(|rest| rest.split("LdcValue::ClassRef(").next())
            .expect("the WideStr arm");
        assert!(arm.contains("let class_held = surrogate_literal_class_held();"));
        assert!(arm.contains(
            "intern_wide_string_literal_or_oom_as(shared, thread, &units, class_held)"
        ));
        assert!(arm.contains("record_wide_string_literal_as_of("));
        assert!(!arm.contains("record_cp_constant_if_enabled("));
        assert!(prod.contains(
            "CondyArg::WideStr \u{7b} cp_index, units \u{7d} => condy_wide_string_arg("
        ));
    }
}

#[cfg(test)]
mod gci_i33sc_condy_string_arg_record_tests {
    //! gci i33/sc (`i32-sl-report.md` residual): a condy's String static
    //! argument is recorded at its own constant-pool entry, as `ldc` of that
    //! entry records it (HotSpot's resolved references), so the class's
    //! record owns it under the weak String table and a lone-surrogate one
    //! keeps a WEAK pool row and dies with its class.
    //! `CRATONVM_GC_CONDY_STRING_ARG_RECORD=0` keeps the unrecorded form.
    use super::w9c_resolution_oom_tests::small_gen_vm;
    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::native::jni::{JObject, JniGlobalKind};
    use crate::threading::jvm_thread::ThreadId;

    fn permanent(shared: &SharedVm) -> usize {
        MemberResolver::new(shared).permanent_constant_count()
    }

    /// Ordinary String argument under the weak String table (the
    /// Generational default): recorded in the never-evicted store, answered
    /// from it, forgotten with its class; the kill-switch arm records nothing.
    #[test]
    fn gci_i33sc_an_ordinary_string_arg_is_recorded_under_the_weak_table() {
        let shared = small_gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "gci-i33sc-str");
        shared.mem.string_table.set_weak(true);
        assert!(crate::memory::string_table::weak_table_engaged(&shared));
        let as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
        let (cls, other, idx) = (ClassId::new(9_331), ClassId::new(9_332), 5u16);
        let text = "gci-i33sc-condy-string-arg";

        let before = permanent(&shared);
        let off = condy_string_arg(&shared, &mut thread, other, idx, text, as_of, false)
            .expect("room");
        assert_eq!(cached_cp_constant(&shared, other, idx), None, "kill switch: no record");
        assert_eq!(permanent(&shared), before);

        let v = condy_string_arg(&shared, &mut thread, cls, idx, text, as_of, true).expect("room");
        assert_eq!(v, off, "the canonical literal");
        assert_eq!(cached_cp_constant(&shared, cls, idx), Some(v), "recorded at its entry");
        assert_eq!(permanent(&shared), before + 1, "the never-evicted store");
        let again =
            condy_string_arg(&shared, &mut thread, cls, idx, text, as_of, true).expect("hit");
        assert_eq!(again, v, "answered from the record");
        assert_eq!(permanent(&shared), before + 1, "insert-if-absent");

        MemberResolver::new(&shared).forget_class_records(cls);
        assert_eq!(cached_cp_constant(&shared, cls, idx), None, "gone with its class");
        assert_eq!(permanent(&shared), before);
    }

    /// The `ldc` cache off: recorded only under the weak table (gcg g3/rw's
    /// rule), as `ldc` of the entry would be.
    #[test]
    fn gci_i33sc_a_cache_off_string_arg_follows_ldc_s_record_rule() {
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            max_heap_size: 8 * 1024 * 1024,
            initial_heap_size: 8 * 1024 * 1024,
            ldc_const_cache: false,
            ..VmConfig::default()
        });
        let mut thread = JvmThread::new(ThreadId(0), "gci-i33sc-cache-off");
        let as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
        let (cls, idx) = (ClassId::new(9_333), 6u16);
        let text = "gci-i33sc-cache-off-arg";

        shared.mem.string_table.set_weak(false);
        let v = condy_string_arg(&shared, &mut thread, cls, idx, text, as_of, true).expect("room");
        assert_eq!(cached_cp_constant(&shared, cls, idx), None, "strong pool, cache off");

        shared.mem.string_table.set_weak(true);
        let w = condy_string_arg(&shared, &mut thread, cls, idx, text, as_of, true).expect("room");
        assert_eq!(cached_cp_constant(&shared, cls, idx), Some(w), "weak table: recorded");
        let _ = v;
    }

    /// Lone-surrogate argument: a WEAK pool row and the never-evicted record
    /// (the weak table off here, the G1 / ZGC default); the kill-switch arm
    /// keeps the strong row and records nothing.
    #[test]
    fn gci_i33sc_a_wide_string_arg_is_class_held() {
        use cratonvm_native_builtins::lang_string::{
            surrogate_intern_forget_vm, surrogate_intern_probe,
        };
        if !surrogate_literal_class_held() {
            return; // `CRATONVM_GC_SURROGATE_LITERAL_CLASS_HELD=0` in the test env
        }
        let shared = small_gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "gci-i33sc-wide");
        shared.mem.string_table.set_weak(false);
        let vm = shared.vm_identity;
        let kind = |h: usize| shared.natives.jni_global_refs.lock().kind(h as JObject);
        let as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
        let (cls, other, idx) = (ClassId::new(9_334), ClassId::new(9_335), 8u16);

        let mut units: Vec<u16> = vec![0xD802];
        units.extend("gci-i33sc-wide-arg".encode_utf16());
        let before = permanent(&shared);
        let v = condy_wide_string_arg(&shared, &mut thread, cls, idx, &units, as_of, true)
            .expect("room");
        let h = surrogate_intern_probe(vm, &units).expect("the row");
        assert_eq!(kind(h), Some(JniGlobalKind::Weak), "a class-held row is weak");
        assert_eq!(cached_cp_constant(&shared, cls, idx), Some(v), "recorded at its entry");
        assert_eq!(permanent(&shared), before + 1, "the never-evicted store");
        assert_eq!(
            condy_wide_string_arg(&shared, &mut thread, cls, idx, &units, as_of, true)
                .expect("hit"),
            v
        );
        assert_eq!(surrogate_intern_probe(vm, &units), Some(h), "a hit changes no row");

        // Kill-switch arm: the strong form, nothing recorded.
        let off = condy_wide_string_arg(&shared, &mut thread, other, idx, &units, as_of, false)
            .expect("pooled");
        assert_eq!(off, v, "canonical");
        let row = surrogate_intern_probe(vm, &units).expect("the row");
        assert_eq!(kind(row), Some(JniGlobalKind::Strong), "upgraded by the strong form");
        assert_eq!(cached_cp_constant(&shared, other, idx), None);
        surrogate_intern_forget_vm(vm);
    }

    /// Source pin: `decode_condy` carries the argument's index and the
    /// bootstrap reads the switch once and routes both String shapes.
    #[test]
    fn gci_i33sc_the_condy_args_route_through_the_recording_helpers() {
        let src = include_str!("constants.rs").replace("\r\n", "\n");
        let prod = src
            .split("\n#[cfg(test)]\nmod ")
            .next()
            .unwrap_or_default()
            .to_string();
        let indent = " ".repeat(24);
        let opens = |head: &str| [head, " \u{7b}\n", indent.as_str(), "cp_index: idx,"].concat();
        assert!(prod.contains(&opens("Some(units) => CondyArg::WideStr")));
        assert!(prod.contains(&opens("None => CondyArg::Str")));
        assert!(prod.contains("let record_strings = condy_string_arg_record_on();"));
        assert!(prod.contains(
            "CondyArg::Str \u{7b} cp_index, text \u{7d} => condy_string_arg("
        ));
        assert!(prod.contains(
            "CondyArg::WideStr \u{7b} cp_index, units \u{7d} => condy_wide_string_arg("
        ));
    }
}

/// gci i39/rw (`gci-i39rw-lone-surrogate-constantvalue-string-is-not-the-literal-FIXED-20261016`):
/// a lone-surrogate `ConstantValue` String is the interned literal, in either
/// order (prepared before or after an `ldc` of the text), and its row keeps
/// the class-held disposition.
#[cfg(test)]
mod gci_i39rw_constant_value_tests {
    use super::w9c_resolution_oom_tests::small_gen_vm;
    use super::*;
    use crate::native::jni::{JObject, JniGlobalKind};
    use crate::threading::jvm_thread::ThreadId;

    #[test]
    fn gci_i39rw_a_wide_constant_value_is_the_ldc_literal() {
        use cratonvm_native_builtins::lang_string::{surrogate_intern_forget_vm, surrogate_intern_probe};
        let shared = small_gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "gci-i39rw-constant-value");
        let vm = shared.vm_identity;

        // `ldc` first, then the field's preparation: the same object.
        let mut units: Vec<u16> = vec![0xD800];
        units.extend("gci-i39rw-ldc-first".encode_utf16());
        let lit = intern_wide_string_literal_or_oom_as(&shared, &mut thread, &units, true)
            .expect("room");
        assert_eq!(intern_wide_string_constant_value(&shared, &units), Some(lit));

        // The field first, then `ldc`: the same object, and one row.
        let mut units2: Vec<u16> = vec![0xDC00];
        units2.extend("gci-i39rw-field-first".encode_utf16());
        let field = intern_wide_string_constant_value(&shared, &units2).expect("room");
        let row = surrogate_intern_probe(vm, &units2).expect("the field claimed the row");
        if surrogate_literal_class_held() {
            assert_eq!(
                shared.natives.jni_global_refs.lock().kind(row as JObject),
                Some(JniGlobalKind::Weak),
                "the static field holds it; the row stays weak"
            );
        }
        let lit2 = intern_wide_string_literal_or_oom_as(&shared, &mut thread, &units2, true)
            .expect("pooled");
        assert_eq!(lit2, field, "the ldc literal is the field's String");
        assert_eq!(surrogate_intern_probe(vm, &units2), Some(row), "a hit changes no row");
        surrogate_intern_forget_vm(vm);
    }

    /// Source pin: `prepare_class_shared`'s wide arm reads the switch and
    /// interns through the helper; `=0` keeps the unpooled String.
    #[test]
    fn gci_i39rw_prepare_class_interns_the_wide_constant_value() {
        let src = include_str!("../../vm/vm_util.rs").replace("\r\n", "\n");
        let arm = &src[src
            .find("Some(CvSeed::StringUtf16(units)) => \u{7b}")
            .expect("the wide arm")..];
        let arm = &arm[..arm.find("None => \u{7b}\u{7d}").expect("the arm's end")];
        assert!(arm.contains("constants::surrogate_constant_value_interned()"));
        assert!(arm.contains("constants::intern_wide_string_constant_value("));
        assert!(arm.contains("vm_object::try_create_java_string_from_units(shared, units)"));
    }
}
