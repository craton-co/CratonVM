#[allow(unused_imports)]
use super::*;

/// `MemorySegment.ofArray(...)` for all seven primitive array types — an
/// ALIAS carrier.
///
/// Mints the H2 shape: `[0]=0` (no machine address), `[1]=byteSize`,
/// `[2]=this segment's session`, `[6]=the array`, `[7]=0`. The element width is
/// read off the array itself through [`heap_element_width`], so the factory
/// and the accessors cannot disagree about the stride. See the registration
/// site for why int[]/long[]/float[]/double[] used to mint a different,
/// copying carrier here and no longer do.
pub(crate) fn pe_of_array_alias(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let array = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        // The row this comment used to defer. MEASURED: `ofArray(null)` is a
        // message-less `NullPointerException` on HotSpot 25.0.4+7, and all
        // FIVE arms of the family now raise it rather than answering a null
        // segment. "One row for the whole family" was right about the scope and
        // wrong about the cost.
        Some(Value::Object(None)) => {
            return Err(RuntimeError::NullPointerException { message: None }.into())
        }
        _ => return Ok(Some(Value::Object(None))),
    };
    let width = match heap_element_width(ctx.heap_element_type_of(array)) {
        Some(width) if ctx.object_is_array(array) => width as i64,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "MemorySegment.ofArray requires a byte[], short[], char[], int[], \
                          long[], float[] or double[]"
                    .into(),
            }
            .into())
        }
    };
    let byte_size = (ctx.array_length(array) as i64).saturating_mul(width);
    // G19-1: THE SCOPE IS MINTED HERE, ONCE, NOT ON EVERY `scope()` CALL.
    //
    // MEASURED on 25.0.3+9-LTS (`G19Probe` §SC): a heap segment's scope is a
    // per-segment session that can never close —
    //
    //     ofArray(new byte[16]).scope().getClass()
    //         = jdk.internal.foreign.GlobalSession$HeapSession
    //     heap.scope() == heap.scope()                  true
    //     heap.asSlice(4,4).scope() == heap.scope()     true
    //     heap.asReadOnly().scope() == heap.scope()     true
    //     ofArray(a).scope() == ofArray(a).scope()      FALSE  (same array!)
    //     heap.scope() == Arena.global().scope()        FALSE
    //
    // so it is neither a fresh object per CALL nor a process-wide singleton:
    // it is one object per SEGMENT, shared by everything derived from it. With
    // slot 2 left empty, `p67_receiver_session` found nothing on this carrier
    // and minted a fresh always-open session on every `scope()` — measured
    // `heap.scope() == heap.scope()` = **false**, which is the assertion
    // `RForeignLayoutJdkInterfaces` dies on ("a heap segment's scope is
    // stable").
    //
    // Slot 2 is the right home and not a new convention: `pe_segment_session`
    // above already documents "tolerate a segment stamped with the session
    // directly", which is exactly this shape.
    let array_pin = ctx.pin_native_root(array);
    let session = crate::phases_late::foreign_ffm::p67_memory_session(ctx)?;
    let array = ctx.read_native_pin(array_pin, array);
    ctx.unpin_native_roots(array_pin);
    // Both the array and the session must survive the segment's allocation, so
    // both are pinned across it; `unpin_native_roots` truncates the pin stack
    // to its argument, so the FIRST handle releases both.
    let array_pin = ctx.pin_native_root(array);
    let session_obj = match session {
        Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
        _ => None,
    };
    let seg = match alloc_segment_carrier(ctx, SEG_HEAP_FIELDS) {
        Ok(seg) => seg,
        Err(err) => {
            // Round 14 wave 3 (lane ffm): not with both pins taken.
            ctx.unpin_native_roots(array_pin);
            return Err(err);
        }
    };
    let array = ctx.read_native_pin(array_pin, array);
    let session = match session_obj {
        Some((pin, obj)) => Value::Object(Some(ctx.read_native_pin(pin, obj))),
        None => Value::Object(None),
    };
    ctx.unpin_native_roots(array_pin);
    ctx.set_field(seg, 0, Value::Long(0));
    ctx.set_field(seg, 1, Value::Long(byte_size));
    ctx.set_field(seg, 2, session);
    ctx.set_field(seg, 3, Value::Int(0));
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    ctx.set_field(seg, SEG_HEAP_BASE_FIELD, Value::Object(Some(array)));
    ctx.set_field(seg, SEG_HEAP_START_FIELD, Value::Long(0));
    Ok(Some(Value::Object(Some(seg))))
}

/// Is `kind` one of the nine VALUE layouts (`LAYOUT_BYTE` … `LAYOUT_CHAR`)?
///
/// **Written as a range CONTAINMENT, not as `kind < 10`.** The kinds are
/// `0..=8` for the value layouts and `10..=13` for the group family, so
/// `kind < 10` looks like the same test and is not: it is also true for every
/// NEGATIVE kind, and `panama_libffi::LAYOUT_UNKNOWN` is **-2**. Three sites in
/// this file spelled it that way, so an unclassifiable carrier — the answer
/// F27 introduced precisely so that an undecodable layout would stop being a
/// plausible eight-byte integer — was admitted by all three and then fell to
/// each one's default arm: `Value::Int(0)` for a read, a silent no-op for a
/// write, and `"Ljava/lang/foreign/MemorySegment;"` for a carrier descriptor.
///
/// None of the three is reachable today, because `alloc_return_slot` and
/// `layout_to_ffi_type` both refuse an unknown carrier earlier on every path.
/// That is a property of TODAY'S CALLERS, not of this code, and it is one
/// reorder away from being false. Every consumer in this file now either uses
/// this predicate or names `LAYOUT_UNKNOWN` explicitly.
pub(in crate::panama) fn layout_kind_is_value(kind: i32) -> bool {
    (0..10).contains(&kind)
}

/// The refusal a `MemorySegment` accessor raises for a layout it cannot
/// classify.
///
/// Names the class, because a refusal that cannot say WHICH carrier was
/// undecodable is not actionable.
pub(in crate::panama) fn unclassifiable_access_layout(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    op: &str,
) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: format!(
            "MemorySegment.{op}: cannot classify layout carrier {} — refusing rather than \
             defaulting to an eight-byte integer",
            ctx.class_name_of_id(ctx.class_id_of_object(layout))
                .unwrap_or_else(|| "<unknown>".to_string())
        ),
    }
    .into()
}

/// Validate a single-element access (get/set) against the segment's declared
/// size and compute the target address with checked arithmetic.
///
/// Mirrors the bounds/overflow checks the `copy`/`fill` paths perform, and
/// throws the same `IllegalStateException` on violation. Rejects:
///   - access through a scope that has been closed (under `--synthetic-jdk`
///     `arena.close()` really frees the block, so this is a use-after-FREE
///     guard, not a cosmetic one; the default model frees nothing on close,
///     see `r12w3-ffm-arena-close-frees-nothing-in-the-default-model`) — see
///     [`pe_segment_check_scope`],
///   - zero-size segments (a 0-size segment — as produced by `ofAddress`
///     before `reinterpret` — is not accessible, matching JDK semantics),
///   - negative `offset`,
///
/// **This is the RAW-ADDRESS path only.** A heap segment never reaches it:
/// `pe_segment_get_impl`/`pe_segment_set_impl` resolve a [`HeapSegmentView`]
/// first. The "Null segment address" arm below is therefore no longer the
/// answer a heap segment gets — it used to be, and its wording named the
/// symptom rather than the reason (F27 residual 4).
///   - `offset + width` overflowing `i64`,
///   - `offset + width` exceeding the segment size,
///   - `(ptr + base_off + offset)` overflowing the address space, or a null
///     resulting address.
///
/// `width` is the access width in bytes derived from the layout kind
/// (`ffi::layout_byte_size`). Returns the validated raw address.
pub(in crate::panama) fn pe_segment_access_addr(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    offset: i64,
    width: i64,
) -> Result<usize, MethodCallFailed> {
    // Liveness before bounds: the order before round 13 (see
    // [`ffm_bounds_before_scope`], whose callers locate first).
    pe_segment_check_scope(ctx, seg)?;
    pe_segment_locate_addr(ctx, seg, offset, width)
}

/// `CRATONVM_FFM_BOUNDS_BEFORE_SCOPE` (default on; round 13 wave 1, lane
/// mhffm, both modes): an element access checks read-only, bounds and
/// alignment BEFORE the session, as JDK 25 does -- `MemorySegment.get` is
/// `layout.varHandle().get(..)`, whose `checkSegment` /
/// `checkEnclosingLayout` run before `ScopedMemoryAccess` checks the session
/// -- so a closed (or another thread's) segment accessed out of bounds is
/// `IndexOutOfBoundsException`, not `IllegalStateException` /
/// `WrongThreadException`. Only a combined fault changes. `0` restores the
/// session check first. Shared with `lang_invoke.rs`'s layout VarHandles.
pub(crate) fn ffm_bounds_before_scope() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// [`pe_segment_access_addr`] without the session check: bounds, then the
/// address arithmetic. For callers that check the session themselves.
pub(in crate::panama) fn pe_segment_locate_addr(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    offset: i64,
    width: i64,
) -> Result<usize, MethodCallFailed> {
    let ptr = crate::panama_libffi::segment_address(ctx, seg);
    let size = crate::panama_libffi::segment_byte_size(ctx, seg);

    // Bounds check: 0 <= offset and offset + width <= size, with overflow guard.
    //
    // Bounds, not state. The final FFM API specifies `IllegalStateException`
    // for a SCOPE violation — a closed arena, the wrong thread — and
    // `IndexOutOfBoundsException` for an access outside the segment. Measured
    // on HotSpot 25: `seg.get(JAVA_INT, 62)` on a 64-byte segment raises
    // `IndexOutOfBoundsException`. A caller writing
    // `catch (IndexOutOfBoundsException e)` — the idiom for a bounds check —
    // did not catch ours.
    //
    // THE ZERO-SIZE CASE IS THE SAME CHECK, not a separate `IllegalStateException`.
    // It used to be raised above this block, which contradicted the paragraph
    // directly above it. Measured, 25.0.3+9-LTS, and the exception class is the
    // same for a native and a heap carrier:
    //
    //     MemorySegment.NULL.get(JAVA_BYTE, 0)
    //       -> IndexOutOfBoundsException: Out of bound access on segment
    //          MemorySegment{ kind: native, address: 0x0, byteSize: 0 };
    //          new offset = 0; new length = 1
    //     MemorySegment.ofAddress(0x1000).get(JAVA_BYTE, 0)   -> IndexOutOfBoundsException
    //     MemorySegment.ofArray(new byte[0]).get(JAVA_BYTE, 0) -> IndexOutOfBoundsException
    //
    // `size == 0` cannot pass `offset + width <= size` for any `width >= 1`, so
    // deleting the special case does not admit anything: it only re-labels the
    // refusal with the class the oracle throws and a caller can catch. A closed
    // scope is still `IllegalStateException: Already closed`, raised above.
    let end = offset.checked_add(width);
    if offset < 0 || size <= 0 || end.map_or(true, |e| e > size) {
        // j27 wave 4 (lane FFMMSG): the segment's own `toString()`.
        return Err(segment_out_of_bounds(ctx, seg, offset, width));
    }

    // Validate address arithmetic doesn't overflow. The base is the HOST
    // address: a segment over a direct buffer carries an `Unsafe` arena handle
    // (see `panama_libffi::host_base_address`), which the load below cannot
    // dereference. The caller's alignment check then runs on this address (the
    // block's own storage: the allocator's alignment, at least 8 for a block of
    // 8 bytes or more).
    // The block must cover THIS access (`offset + width`), not the segment's
    // declared size: a C-pointer view of a block (`ADDRESS.withTargetLayout(
    // sequenceLayout(JAVA_BYTE))`, `byteSize == Long.MAX_VALUE`) returned by a
    // downcall into an `Arena` block reads within it, as on HotSpot
    // (`R18FfmretShapes` `ptr-return-cptr`, `strchr-cptr-read`).
    let host_ptr = if crate::panama_libffi::is_arena_handle(ptr) {
        let host = crate::panama_libffi::host_base_address(ptr, end.unwrap_or(size));
        if host == 0 {
            // A freed handle, or a block shorter than the segment: refuse by
            // name instead of dereferencing `0 + offset`.
            return Err(RuntimeError::IllegalStateException {
                message: "Null segment address".into(),
            }
            .into());
        }
        host
    } else {
        ptr
    };
    let total = (host_ptr as u64).checked_add(offset as u64);
    match total {
        Some(addr) if addr != 0 => Ok(addr as usize),
        Some(_) => Err(RuntimeError::IllegalStateException {
            message: "Null segment address".into(),
        }
        .into()),
        None => Err(RuntimeError::IllegalStateException {
            message: "address arithmetic overflow in MemorySegment access".into(),
        }
        .into()),
    }
}

/// Publish an FFM fast-path verdict for `seg` — see [`crate::ffm_fast`].
///
/// Called only where [`pe_segment_access_addr`] has already returned a
/// validated address, i.e. the scope was live, the shape resolved and the
/// access was in bounds. This records THAT, and never the address or the size:
/// the JIT re-reads those slots itself, so nothing here can go stale under a
/// slot rewrite.
///
/// Gated to the plain SIX-slot CratonVM-minted native carrier, because that is
/// the only shape whose `[0]=ptr, [1]=size, [5]=offset` decode the JIT can
/// reproduce. The 8-slot heap-aliasing carrier keeps `ptr` at 0 and reuses
/// slots 6/7, and the 2-/3-slot shapes have no slot 5 at all — publishing
/// either would hand the JIT a decode that does not describe it.
///
/// `observed` is the FFM epoch the caller read before `pe_segment_access_addr`;
/// a verdict whose checks straddled a free is not published.
pub(in crate::panama) fn ffm_publish_verdict(ctx: &dyn NativeContext, seg: ObjectRef, write: bool, observed: u64) {
    if ctx.object_num_fields(seg) != 6 {
        return;
    }
    if crate::panama_libffi::is_real_heap_segment(ctx, seg) {
        return;
    }
    crate::ffm_fast::note_validated_at(seg.as_ptr() as u64, write, observed);
    crate::ffm_fast::note_publish();
}

// ---------------------------------------------------------------------------
// Byte order and alignment of an element access (round 12 wave 5, lane ffm2)
// ---------------------------------------------------------------------------
//
// `r12w4-hunter2-ffm-element-access-ignores-layout-order-and-alignment`:
// `get`/`set`/`getAtIndex`/`setAtIndex` read and wrote every layout in host
// order (`JAVA_INT.withOrder(BIG_ENDIAN)` answered the little-endian value)
// and never raised the `IllegalArgumentException` HotSpot raises for a
// misaligned native access (`seg.asSlice(1).get(JAVA_INT, 0)`). Both are the
// JDK's contract in every mode, so the fixes apply in `--compatible` too; each
// has a kill switch.



/// `CRATONVM_FFM_NATIVE_ALIGN_CHECK` (default on): a native-segment element
/// access whose absolute address is not a multiple of the layout's byte
/// alignment raises `IllegalArgumentException` (HotSpot's `Target offset ..
/// is incompatible with alignment constraint ..`, [`native_misaligned_refusal`]),
/// and `getAtIndex`/`setAtIndex` refuse a layout whose alignment exceeds its
/// size. Off restores the old unchecked access.
pub(crate) fn ffm_native_align_check() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `CRATONVM_FFM_BULK_SCOPE_CHECK` (default on): the bulk and pass-through
/// segment paths that used to touch a block with no liveness check at all --
/// `fill`, `MemorySegment.copy(segment, .., segment, ..)`, the segment-to-array
/// `copy`, and the segment arguments of a downcall -- raise the scope's
/// `IllegalStateException` / `WrongThreadException` like every element access.
/// `R12Hunt2FfmLayouts`' `closed-ise` 12000 against HotSpot's 16000 was `fill`
/// and `copy` on a closed arena answering. Off restores the unchecked paths,
/// and then (see `foreign_ffm::p67_arena_frees_on_close`) no arena frees its
/// blocks on close either, since those paths would read freed memory.
pub(crate) fn ffm_bulk_scope_check() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}





/// `CRATONVM_FFM_DOWNCALL_GC_SAFE` (default OFF; round 13 wave 5, lane ffm3):
/// every downcall runs its C call inside a GC blocking region, so a
/// stop-the-world pause on another thread does not wait for a thread blocked
/// in C (HotSpot's `_thread_in_native`). OFF by default because entering the
/// region deposits a fresh root snapshot per call, which a `strlen`-sized
/// downcall cannot afford; see
/// `r13w5-ffm3-a-blocked-downcall-stalls-every-stop-the-world` for the cheap
/// design that should replace it.
///
/// gcd d5/f: a `Linker.Option.critical` handle stays counted under the flag
/// ([`DOWNCALL_CRITICAL`]), and a JNIEnv function the C code calls leaves the
/// region like an upcall does (`panama_libffi::leave_active_downcall_region`).
/// The flip gate is on the page named above (d5/f STATUS).
///
/// JIT round 16 wave 9 (lane ffm): the region is the quiet in-native
/// transition by default ([`ffm_downcall_in_native_quiet`]). The flip, once
/// the cost A/B on that page passes, is this reader becoming
/// `runtime_flag_default_on` and the `ffm-downcall-gc-safe` flag row taking
/// `off_word: Some("0")`.
pub(in crate::panama) fn ffm_downcall_gc_safe() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| false)
}

/// `CRATONVM_FFM_DOWNCALL_IN_NATIVE_QUIET` (default on, inert unless
/// [`ffm_downcall_gc_safe`]; JIT round 16 wave 9, lane ffm): a GC-safe
/// downcall enters native through `NativeContext::begin_native_region_quiet`
/// -- one root deposit (reusing the frozen-band verdict of the stack above
/// the compiled caller), no code-reclamation summary -- and leaves through
/// `end_native_region_quiet`, which skips the full wake (fixups and a second
/// deposit) when no pause completed and nothing was relocated into the
/// thread during the call: the JNI in-native bracket's transition. `0` takes
/// round 14's full `begin_native_region` / `end_blocking_region` pair (one
/// deposit and a summary walk in, a full wake and a second deposit out).
pub(in crate::panama) fn ffm_downcall_in_native_quiet() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY` (default ON since JIT round 17
/// wave 5, lane ffm4, `=0` off; JIT round 16 wave 14, lane ffm3,
/// `r13w5-ffm3-a-blocked-downcall-stalls-every-stop-the-world`). Default ON
/// because the counted state is a liveness defect, not a speed choice: a
/// stop-the-world pause waits for every counted thread, so a thread blocked
/// in C on something only a parked thread can provide (a pipe `read` whose
/// writer is waking from `Thread.sleep` behind the pause) is a deadlock --
/// `R17EintrNative` hung on every run. `=0` restores the counted state. A
/// non-critical downcall goes lazily into C: a
/// non-critical downcall goes into C through HotSpot's `_thread_in_native`
/// transition, `NativeContext::begin_native_region_lazy` -- one release store
/// of the thread's state in, one compare-exchange out, no root deposit, no
/// TLAB retire, no barrier lock -- instead of staying counted (the default) or
/// paying the deposit-based region of [`ffm_downcall_gc_safe`]. A collection
/// that finds the thread in C claims it and scans it like a peer frozen in
/// compiled code (the vm's `gc_and_alloc::claim_lazy_native_peers`, which reads
/// the same switch), so the pause no longer waits for a C function that
/// blocks; the thread's leave waits while such a pause is still running.
/// Where the context declines (a target or heap the take-over cannot claim
/// on), [`ffm_downcall_gc_safe`]'s region is taken if that switch is on, and
/// the thread stays counted otherwise. `Linker.Option.critical` handles are
/// never affected. The vm's pause half reads the same switch with the same
/// default (`gc_and_alloc::ffm_in_native_lazy_claims_enabled`); were the two to
/// disagree, a thread in the lazy state that no pause claims is waited for,
/// the counted behaviour, never unsound.
pub(in crate::panama) fn ffm_downcall_in_native_lazy() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// JIT round 16 wave 14 (lane ffm3): open the downcall's GC-safe state right
/// before the C call: the lazy in-native state under
/// [`ffm_downcall_in_native_lazy`] (falling back to the deposit-based region
/// when [`ffm_downcall_gc_safe`] is on), the deposit-based region otherwise.
/// Only called when one of the two switches is on and the handle is not
/// critical. The caller must have captured its `CalleeSavedSpill` first.
#[inline(always)]
pub(in crate::panama) fn downcall_enter_gc_safe(
    ctx: &mut dyn NativeContext,
    guard: &crate::panama_libffi::ActiveContextGuard,
) {
    if ffm_downcall_in_native_lazy() {
        let fallback = ffm_downcall_gc_safe().then(ffm_downcall_in_native_quiet);
        guard.enter_region_lazy(ctx, fallback);
    } else {
        guard.enter_region(ctx, ffm_downcall_in_native_quiet());
    }
}

/// `CRATONVM_FFM_DOWNCALL_PLAIN_MEMO` (default OFF, a performance switch for
/// the orchestrator's A/B; JIT round 16 wave 11, lane ffmcost,
/// `r16-ffm-plain-downcall-costs-microseconds-FIXED-20261007`): a downcall handle
/// whose parameters and return are all primitive VALUE layouts (no
/// `ADDRESS`, no group), non-variadic, without `captureCallState`, an
/// address argument or a return allocator, is classified once
/// ([`DOWNCALL_PLAIN`]) and later calls take [`pe_downcall_invoke_plain`]: no
/// descriptor or layout reads, no `Vec`, no `String`, no pins, arguments
/// written straight into the cached CIF's cells. Answers and exceptions are
/// the general path's -- anything the plain path cannot vouch for falls
/// through to it before any effect.
pub(in crate::panama) fn ffm_downcall_plain_memo() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| false)
}



/// `CRATONVM_JIT_FFM_DOWNCALL_BIND_SEGMENTS` (default ON since wave 14:
/// strlen 4290 -> 1930, llabs-errno 4300 -> 2080 ns/call, loaded 20/20; `=0`
/// off; JIT round 16 wave 13, lane ffmseg, FFD16-2 / FFD16-3): the memo paths and the JIT's
/// bound downcall row also serve a handle whose parameters include `ADDRESS`
/// (a `MemorySegment` argument) and a `captureCallState` handle (its leading
/// state segment), through [`pe_downcall_invoke_memo`]: per call, each
/// segment's address is read and screened as `marshal_arg` screens it, its
/// scope is checked (confinement, then liveness) and its session acquired for
/// the call and released after it, and the capture state is written straight
/// after the call. A heap segment, a `null`, a closed or foreign-thread scope,
/// or a screen refusal declines to the general path before any effect, and the
/// general path raises what it always raised. Read once.
pub fn jit_downcall_bind_segments_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}



/// `CRATONVM_COMPAT_FFM_OFARRAY_HEAP_DOWNCALL_CARVEOUT` (default OFF, `=1`
/// restores the pre-j27 behaviour; j27 wave 4, lane FFMFLIP): a plain
/// downcall (no `critical(true)`) takes a `MemorySegment.ofArray(int[]|long[]|
/// float[]|double[])` segment through the call-scoped scratch buffer, the
/// carve-out once kept for Elasticsearch's bulk-vector downcalls. HotSpot 25
/// refuses it (`IllegalArgumentException: Heap segment not allowed:
/// <segment>`), and so does this VM unless the switch is set. Read per
/// numeric heap argument of a plain downcall, never cached (j27 wave 8, lane
/// W8FFM: it was a process-wide `OnceLock`, compatibility state that
/// AGENTS.md keeps per VM, and one a test override could not reach).
pub(in crate::panama) fn ffm_ofarray_heap_downcall_carveout() -> bool {
    false
}



/// JIT round 16 wave 16 (lane ffmlean): may an encoding ([`plain_memo_encode`]
/// / [`segment_memo_encode`]) be called through a direct stub? At most
/// [`crate::panama_libffi::DIRECT_STUB_MAX_ARGS`] parameters, each and the
/// return (or void) integer class, on a target with direct stubs. The
/// capture-state segment is not a C parameter and does not count. Pure.
pub(in crate::panama) fn memo_direct_shape(memo: i64) -> bool {
    use crate::panama_libffi as plf;
    if !plf::direct_stubs_supported() || !plain_memo_is_encoded(memo) {
        return false;
    }
    // JIT round 18 wave 11 (lane ffmret): a by-value group return goes
    // direct only when it comes back in general registers.
    if memo & PLAIN_MEMO_RET_GROUP != 0 && memo & PLAIN_MEMO_GROUP_IN_REGS == 0 {
        return false;
    }
    // Round 19: FLOAT-class parameters and returns too, on SysV x86-64
    // (`plf::direct_stub_call_fp`); a register-returned group keeps the
    // integer pair stub.
    let fp = plf::direct_stub_fp_supported() && memo & PLAIN_MEMO_RET_GROUP == 0;
    let kind_ok =
        |kind: i32| plf::direct_stub_kind_ok(kind) || (fp && plf::direct_stub_fp_kind(kind));
    let arity = plain_memo_arity(memo);
    arity <= plf::DIRECT_STUB_MAX_ARGS
        && (0..arity).all(|i| kind_ok(plain_memo_param(memo, i)))
        && plain_memo_return(memo).is_none_or(kind_ok)
}

/// Round 19: does a [`memo_direct_shape`] encoding have a FLOAT-class
/// parameter or return (so its call is `plf::direct_stub_call_fp`)? Pure.
pub(in crate::panama) fn memo_direct_has_fp(memo: i64) -> bool {
    use crate::panama_libffi as plf;
    (0..plain_memo_arity(memo)).any(|i| plf::direct_stub_fp_kind(plain_memo_param(memo, i)))
        || plain_memo_return(memo).is_some_and(plf::direct_stub_fp_kind)
}

/// The direct-stub call of a [`memo_direct_shape`] encoding whose `regs` are
/// [`memo_direct_regs`]: [`crate::panama_libffi::direct_stub_call`] for an
/// all-integer one, else (round 19) the integer and the float-class registers
/// split, each in parameter order, for
/// [`crate::panama_libffi::direct_stub_call_fp`]. Answers rax, or `xmm0`'s
/// bits for a float-class return; `None` -- nothing called -- as those.
///
/// # Safety
///
/// [`crate::panama_libffi::direct_stub_call`]'s: `fn_addr` a validated target
/// of this encoding's own non-variadic shape.
#[inline(always)]
pub(in crate::panama) unsafe fn memo_direct_stub_call(
    memo: i64,
    fn_addr: usize,
    regs: &[u64; crate::panama_libffi::DIRECT_STUB_MAX_ARGS],
) -> Option<u64> {
    use crate::panama_libffi as plf;
    let arity = plain_memo_arity(memo);
    if !memo_direct_has_fp(memo) {
        // SAFETY: the caller's contract, with every register integer class.
        return unsafe { plf::direct_stub_call(fn_addr, arity, regs) };
    }
    let mut ints = [0u64; plf::DIRECT_STUB_MAX_ARGS];
    let mut fps = [0u64; plf::DIRECT_STUB_MAX_FP_ARGS];
    let (mut n_int, mut n_fp) = (0, 0);
    for (i, reg) in regs.iter().enumerate().take(arity) {
        if plf::direct_stub_fp_kind(plain_memo_param(memo, i)) {
            *fps.get_mut(n_fp)? = *reg;
            n_fp += 1;
        } else {
            *ints.get_mut(n_int)? = *reg;
            n_int += 1;
        }
    }
    let ret_fp = plain_memo_return(memo).is_some_and(plf::direct_stub_fp_kind);
    // SAFETY: the caller's contract; the split is the SysV assignment.
    unsafe { plf::direct_stub_call_fp(fn_addr, &ints, &fps, ret_fp) }
}

/// The register images of a direct-stub call: each of the encoding's
/// parameters' cells widened as libffi widens it.
pub(in crate::panama) fn memo_direct_regs(memo: i64, cells: &[u64]) -> [u64; crate::panama_libffi::DIRECT_STUB_MAX_ARGS] {
    let mut regs = [0u64; crate::panama_libffi::DIRECT_STUB_MAX_ARGS];
    for (i, (reg, cell)) in regs.iter_mut().zip(cells).enumerate() {
        *reg = crate::panama_libffi::direct_stub_widen(plain_memo_param(memo, i), *cell);
    }
    regs
}

/// The Java value of a memo-path call's raw return slot: `null` for void
/// (the general path's answer), else the return kind's own low bytes.
pub(in crate::panama) fn memo_return_value(memo: i64, ret: [u64; 2]) -> Value {
    match plain_memo_return(memo) {
        None => Value::Object(None),
        Some(kind) => {
            let mut bytes = [0u8; 16];
            bytes[..8].copy_from_slice(&ret[0].to_ne_bytes());
            bytes[8..].copy_from_slice(&ret[1].to_ne_bytes());
            crate::panama_libffi::unmarshal_return_primitive(kind, &bytes)
        }
    }
}

/// `CRATONVM_DBG_FFM`, read once for the plain-memo diagnostics below
/// (`[ffm-plain] memo ..` when a handle is classified, `[ffm-plain] hit ..`
/// on its first plain call): the engagement proof for a probe.
pub(in crate::panama) fn ffm_dbg_plain() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| false)}

/// [`DOWNCALL_PLAIN`]: classified, and not a plain handle.
pub(in crate::panama) const PLAIN_MEMO_NOT_PLAIN: i64 = -1;
/// [`DOWNCALL_PLAIN`]: set on every plain encoding, so one is never `0`.
pub(in crate::panama) const PLAIN_MEMO_TAG: i64 = 1 << 62;
/// [`DOWNCALL_PLAIN`]: the plain path has reported its first hit
/// (`CRATONVM_DBG_FFM` only).
pub(in crate::panama) const PLAIN_MEMO_HIT_SEEN: i64 = 1 << 61;
/// [`DOWNCALL_PLAIN`] (JIT round 16 wave 13, lane ffmseg): a SEGMENT encoding
/// -- at least one parameter is `ADDRESS` (a `MemorySegment` argument). Served
/// by [`pe_downcall_invoke_memo`] only with
/// `CRATONVM_JIT_FFM_DOWNCALL_BIND_SEGMENTS`; never a plain encoding.
pub(in crate::panama) const PLAIN_MEMO_SEGMENTS: i64 = 1 << 56;
/// [`DOWNCALL_PLAIN`] (wave 13, lane ffmseg): the handle has
/// `captureCallState`: its first Java argument is the state segment, which is
/// not among the encoded (C) parameters. Never a plain encoding.
pub(in crate::panama) const PLAIN_MEMO_CAPTURE: i64 = 1 << 57;
/// Parameters an encoding holds: four bits each from bit 8, below
/// [`PLAIN_MEMO_SEGMENTS`] (thirteen until wave 13, which took bits 56-57).
pub(in crate::panama) const PLAIN_MEMO_MAX_ARGS: usize = 12;
/// [`DOWNCALL_PLAIN`] (JIT round 18 wave 11, lane ffmret; FFMCOST18-2,
/// `CRATONVM_FFM_DOWNCALL_MEMO_ADDRESS_RETURN`): the return is `ADDRESS`. The
/// return nibble then holds `LAYOUT_ADDRESS + 1`, and every consumer mints the
/// result segment ([`memo_address_return_segment`]) instead of answering the
/// raw pointer. Never a plain encoding: the mint allocates.
pub(in crate::panama) const PLAIN_MEMO_RET_ADDRESS: i64 = 1 << 58;
/// [`DOWNCALL_PLAIN`] (wave 11, lane ffmret; FFMCOST18-3,
/// `CRATONVM_FFM_DOWNCALL_MEMO_STRUCT_RETURN`): the return is a group BY VALUE
/// and the handle takes a leading `SegmentAllocator` (before the capture-state
/// segment, the JDK's order). The return nibble is `0`; the group's size and
/// alignment are the cached CIF's return type's (checked equal to the Java
/// layout's at the classification). Never a plain encoding.
pub(in crate::panama) const PLAIN_MEMO_RET_GROUP: i64 = 1 << 59;
/// With [`PLAIN_MEMO_RET_ADDRESS`] only: the `AddressLayout`'s target is an
/// unbounded byte sequence (jextract's `C_POINTER`), so the result segment is
/// `Long.MAX_VALUE` bytes long; without it, zero bytes.
pub(in crate::panama) const PLAIN_MEMO_ADDRESS_UNBOUNDED: i64 = 1 << 60;
/// With [`PLAIN_MEMO_RET_GROUP`] only (the same bit as
/// [`PLAIN_MEMO_ADDRESS_UNBOUNDED`], read only under its own kind bit): the
/// group comes back in general registers (all-integer, at most 16 bytes on
/// SysV in `rax:rdx`; 1, 2, 4 or 8 bytes on Win64 in `rax`), so a direct stub
/// can make the call ([`memo_direct_shape`]).
pub(in crate::panama) const PLAIN_MEMO_GROUP_IN_REGS: i64 = 1 << 60;
/// The bits that make an encoding a SEGMENT (memo-path) one rather than a
/// plain one.
pub(in crate::panama) const PLAIN_MEMO_NOT_PLAIN_BITS: i64 =
    PLAIN_MEMO_SEGMENTS | PLAIN_MEMO_CAPTURE | PLAIN_MEMO_RET_ADDRESS | PLAIN_MEMO_RET_GROUP;





/// The leading Java arguments of an encoding before its C parameters: the
/// return allocator ([`PLAIN_MEMO_RET_GROUP`]) and then the capture-state
/// segment ([`PLAIN_MEMO_CAPTURE`]).
pub(in crate::panama) fn plain_memo_lead(memo: i64) -> usize {
    usize::from(memo & PLAIN_MEMO_RET_GROUP != 0) + usize::from(memo & PLAIN_MEMO_CAPTURE != 0)
}

/// Does the encoding return an object the call mints or obtains after the C
/// call (an `ADDRESS` segment, or the allocator's segment for a group)?
pub(in crate::panama) fn plain_memo_returns_object(memo: i64) -> bool {
    memo & (PLAIN_MEMO_RET_ADDRESS | PLAIN_MEMO_RET_GROUP) != 0
}

/// The `ADDRESS`-return bits for the return layout `rl`, or `None` when its
/// target is one the encoding does not hold. The general path's answer for a
/// pointer `raw` is [`pe_address_read_segment`]'s: a segment of
/// `address_target_size` bytes after an `address_target_align` check (both
/// `(0, 1)` without `CRATONVM_FFM_ADDRESS_READ_TARGET`). Two targets are kept:
/// none (`(0, 1)`) and an unbounded byte sequence (`(Long.MAX_VALUE, 1)`).
pub(in crate::panama) fn memo_address_return_bits(ctx: &dyn NativeContext, rl: ObjectRef) -> Option<i64> {
    let (size, align) = if ffm_address_read_target() {
        (
            crate::panama_upcall::address_target_size(ctx, rl),
            crate::panama_upcall::address_target_align(ctx, rl),
        )
    } else {
        (0, 1)
    };
    match (size, align) {
        (0, 1) => Some(PLAIN_MEMO_RET_ADDRESS),
        (i64::MAX, 1) => Some(PLAIN_MEMO_RET_ADDRESS | PLAIN_MEMO_ADDRESS_UNBOUNDED),
        _ => None,
    }
}

/// [`segment_memo_encode`] of the parameters with an `ADDRESS` return:
/// `bits` from [`memo_address_return_bits`]. Pure.
pub(in crate::panama) fn address_return_memo_encode(params: &[i32], capture: bool, bits: i64) -> i64 {
    let base = segment_memo_encode(None, params, capture);
    if !plain_memo_is_encoded(base) {
        return PLAIN_MEMO_NOT_PLAIN;
    }
    (base & !0xF) | i64::from(LAYOUT_ADDRESS + 1) | bits
}

/// [`segment_memo_encode`] of the parameters with a by-value group return
/// through a leading allocator; `in_regs` is [`PLAIN_MEMO_GROUP_IN_REGS`].
/// Pure.
pub(in crate::panama) fn group_return_memo_encode(params: &[i32], capture: bool, in_regs: bool) -> i64 {
    let base = segment_memo_encode(None, params, capture);
    if !plain_memo_is_encoded(base) {
        return PLAIN_MEMO_NOT_PLAIN;
    }
    let mut memo = base | PLAIN_MEMO_RET_GROUP;
    if in_regs {
        memo |= PLAIN_MEMO_GROUP_IN_REGS;
    }
    memo
}

/// The size and alignment of a cached CIF's return type, as libffi prepared
/// it. `None` for a zero pointer.
///
/// # Safety
///
/// `cif_u64` is `0` or a [`crate::panama_libffi::box_cif_to_u64`] pointer of a
/// live handle.
pub(in crate::panama) unsafe fn cif_return_size_align(
    cif_u64: u64,
) -> Option<(usize, usize, *const libffi::raw::ffi_type)> {
    // SAFETY: the caller's contract.
    let cif = unsafe { crate::panama_libffi::cached_cif_ref(cif_u64) }?;
    let raw = cif.as_raw_ptr();
    // SAFETY: a prepared CIF's `rtype` is a prepared `ffi_type`.
    let rtype: *const libffi::raw::ffi_type = unsafe { (*raw).rtype };
    if rtype.is_null() {
        return None;
    }
    // SAFETY: as above.
    let (size, align) = unsafe { ((*rtype).size, usize::from((*rtype).alignment)) };
    Some((size, align, rtype))
}

/// Is every leaf of the prepared `ffi_type` an integer or a pointer (no
/// float, double, long double or complex)? A struct must have at least one
/// element. This is libffi's own view of the type, the one `ffi_call` and the
/// general path use.
///
/// # Safety
///
/// `ty` is null or a prepared `ffi_type` whose `elements` (for a struct) is a
/// null-terminated array of prepared types.
pub(in crate::panama) unsafe fn ffi_type_all_integer(ty: *const libffi::raw::ffi_type, depth: usize) -> bool {
    if ty.is_null() || depth > crate::panama_libffi::MAX_LAYOUT_DEPTH {
        return false;
    }
    // SAFETY: the caller's contract.
    let t = unsafe { &*ty };
    match t.type_ {
        // FFI_TYPE_INT, then UINT8 ..= SINT64, and POINTER (libffi-sys's
        // `FFI_TYPE_*` numbering).
        1 | 5..=12 | 14 => true,
        // FFI_TYPE_STRUCT.
        13 => {
            let mut cursor = t.elements as *const *mut libffi::raw::ffi_type;
            if cursor.is_null() {
                return false;
            }
            let mut any = false;
            loop {
                // SAFETY: a null-terminated element array.
                let element = unsafe { *cursor };
                if element.is_null() {
                    return any;
                }
                // SAFETY: a prepared element type.
                if !unsafe { ffi_type_all_integer(element, depth + 1) } {
                    return false;
                }
                any = true;
                // SAFETY: still inside the null-terminated array.
                cursor = unsafe { cursor.add(1) };
            }
        }
        _ => false,
    }
}

/// Does a group of `size` bytes whose libffi type is all-integer
/// ([`ffi_type_all_integer`]) come back in general registers on this target:
/// SysV x86-64 `rax` (and `rdx` past 8 bytes) up to 16 bytes, Win64 `rax` for
/// exactly 1, 2, 4 or 8 bytes (anything else is returned through a hidden
/// pointer)? Pure apart from the target.
pub(in crate::panama) fn group_size_in_regs(size: usize, all_integer: bool) -> bool {
    if !all_integer || !crate::panama_libffi::direct_stubs_supported() {
        return false;
    }
    if cfg!(windows) {
        matches!(size, 1 | 2 | 4 | 8)
    } else {
        (1..=16).contains(&size)
    }
}

/// The group-return bits for the return layout `rl` of `handle`, or `None`:
/// a group of 1 to 16 bytes whose Java size and alignment
/// (`p67_layout_size_of` / `p67_layout_align_of`, what the general path asks
/// the allocator for) equal the cached CIF's return type's, so the memo paths
/// may read them from the CIF. `Some(in_regs)`.
pub(in crate::panama) fn memo_group_return_in_regs(
    ctx: &dyn NativeContext,
    handle: ObjectRef,
    rl: ObjectRef,
) -> Option<bool> {
    let Value::Long(cif) = ctx.get_field(handle, DOWNCALL_CIF) else {
        return None;
    };
    // SAFETY: the handle's own cached CIF, just stashed by its general call.
    let (size, align, rtype) = unsafe { cif_return_size_align(cif as u64) }?;
    let java_size = crate::phases_late::foreign_ffm::p67_layout_size_of(ctx, rl).max(0) as usize;
    let java_align = crate::phases_late::foreign_ffm::p67_layout_align_of(ctx, rl).max(1) as usize;
    if size == 0 || size > 16 || java_size != size || java_align != align.max(1) {
        return None;
    }
    // SAFETY: the prepared return type of that CIF.
    let all_integer = unsafe { ffi_type_all_integer(rtype, 0) };
    Some(group_size_in_regs(size, all_integer))
}

/// A layout kind the plain path marshals and unmarshals itself: the eight
/// primitive value kinds, exactly the set
/// [`crate::panama_libffi::marshal_primitive_into`] covers.
pub(in crate::panama) fn plain_memo_kind_ok(kind: i32) -> bool {
    matches!(
        kind,
        LAYOUT_BYTE
            | LAYOUT_SHORT
            | LAYOUT_INT
            | LAYOUT_LONG
            | LAYOUT_FLOAT
            | LAYOUT_DOUBLE
            | LAYOUT_BOOLEAN
            | LAYOUT_CHAR
    )
}

/// Encode a plain handle's shape: bits 0-3 the return kind + 1 (`0` = void),
/// bits 4-7 the parameter count, then four bits per parameter kind, and
/// [`PLAIN_MEMO_TAG`]. [`PLAIN_MEMO_NOT_PLAIN`] when any kind is not
/// [`plain_memo_kind_ok`] or there are more than [`PLAIN_MEMO_MAX_ARGS`].
pub(in crate::panama) fn plain_memo_encode(ret: Option<i32>, params: &[i32]) -> i64 {
    if params.len() > PLAIN_MEMO_MAX_ARGS
        || !params.iter().all(|k| plain_memo_kind_ok(*k))
        || !ret.is_none_or(plain_memo_kind_ok)
    {
        return PLAIN_MEMO_NOT_PLAIN;
    }
    let mut memo =
        PLAIN_MEMO_TAG | ret.map_or(0, |k| i64::from(k) + 1) | ((params.len() as i64) << 4);
    for (i, kind) in params.iter().enumerate() {
        memo |= i64::from(*kind) << (8 + 4 * i);
    }
    memo
}

/// Is `memo` an encoding at all (not undecided, not
/// [`PLAIN_MEMO_NOT_PLAIN`]): a plain or a segment one?
pub(in crate::panama) fn plain_memo_is_encoded(memo: i64) -> bool {
    memo > 0 && memo & PLAIN_MEMO_TAG != 0
}

/// Is `memo` a plain encoding: primitive parameters and return only, no
/// capture state?
pub(in crate::panama) fn plain_memo_is_plain(memo: i64) -> bool {
    plain_memo_is_encoded(memo) && memo & PLAIN_MEMO_NOT_PLAIN_BITS == 0
}

/// Is `memo` a segment encoding ([`segment_memo_encode`]): an `ADDRESS`
/// parameter or a capture-state segment, or (JIT round 18 wave 11, lane
/// ffmret) an `ADDRESS` or by-value group return?
pub(in crate::panama) fn plain_memo_is_segment(memo: i64) -> bool {
    plain_memo_is_encoded(memo) && memo & PLAIN_MEMO_NOT_PLAIN_BITS != 0
}

/// Encode a handle whose parameters may include `ADDRESS` and that may capture
/// call state (JIT round 16 wave 13, lane ffmseg): [`plain_memo_encode`]'s
/// layout plus [`PLAIN_MEMO_SEGMENTS`] / [`PLAIN_MEMO_CAPTURE`]. Without an
/// `ADDRESS` parameter or a capture it IS [`plain_memo_encode`]. The return
/// stays primitive or void: an `ADDRESS` return mints a segment (a Java
/// allocation after the call), which stays with the general path.
pub(in crate::panama) fn segment_memo_encode(ret: Option<i32>, params: &[i32], capture: bool) -> i64 {
    let addresses = params.contains(&LAYOUT_ADDRESS);
    if !addresses && !capture {
        return plain_memo_encode(ret, params);
    }
    if params.len() > PLAIN_MEMO_MAX_ARGS
        || !params
            .iter()
            .all(|k| plain_memo_kind_ok(*k) || *k == LAYOUT_ADDRESS)
        || !ret.is_none_or(plain_memo_kind_ok)
    {
        return PLAIN_MEMO_NOT_PLAIN;
    }
    let mut memo =
        PLAIN_MEMO_TAG | ret.map_or(0, |k| i64::from(k) + 1) | ((params.len() as i64) << 4);
    if addresses {
        memo |= PLAIN_MEMO_SEGMENTS;
    }
    if capture {
        memo |= PLAIN_MEMO_CAPTURE;
    }
    for (i, kind) in params.iter().enumerate() {
        memo |= i64::from(*kind) << (8 + 4 * i);
    }
    memo
}

/// The parameter count of a plain encoding.
pub(in crate::panama) fn plain_memo_arity(memo: i64) -> usize {
    ((memo >> 4) & 0xF) as usize
}

/// Parameter `i`'s layout kind in a plain encoding.
pub(in crate::panama) fn plain_memo_param(memo: i64, i: usize) -> i32 {
    ((memo >> (8 + 4 * i)) & 0xF) as i32
}

/// The return layout kind of a plain encoding; `None` for void.
pub(in crate::panama) fn plain_memo_return(memo: i64) -> Option<i32> {
    match (memo & 0xF) as i32 {
        0 => None,
        code => Some(code - 1),
    }
}

/// Classify `handle` for the plain path, once, from what its general call
/// just read ([`pe_downcall_invoke`], after the CIF is cached). `shape_ok` is
/// the general call's verdict on everything that is not a layout: not
/// address-less, not variadic, no return allocator. `capture`: the handle
/// captures call state (wave 13: a segment encoding; until then not plain).
pub(in crate::panama) fn downcall_record_plain_memo(
    ctx: &dyn NativeContext,
    handle: ObjectRef,
    shape_ok: bool,
    capture: bool,
    param_layout_objs: &[Option<ObjectRef>],
    return_layout: Option<ObjectRef>,
) {
    if matches!(ctx.get_field(handle, DOWNCALL_PLAIN), Value::Long(m) if m != 0) {
        return;
    }
    let memo = if shape_ok && param_layout_objs.len() <= PLAIN_MEMO_MAX_ARGS {
        let mut kinds = [0i32; PLAIN_MEMO_MAX_ARGS];
        let mut all = true;
        for (i, layout) in param_layout_objs.iter().enumerate() {
            match layout {
                Some(layout) => kinds[i] = crate::panama_libffi::read_layout_kind(ctx, *layout),
                None => all = false,
            }
        }
        let ret = return_layout.map(|rl| crate::panama_libffi::read_layout_kind(ctx, rl));
        let params = &kinds[..param_layout_objs.len()];
        match (ret, return_layout) {
            _ if !all => PLAIN_MEMO_NOT_PLAIN,
            // JIT round 18 wave 11 (lane ffmret; FFMCOST18-2): an `ADDRESS`
            // return of a target the encoding holds.
            (Some(LAYOUT_ADDRESS), Some(rl)) if true => {
                match memo_address_return_bits(ctx, rl) {
                    Some(bits) => address_return_memo_encode(params, capture, bits),
                    None => PLAIN_MEMO_NOT_PLAIN,
                }
            }
            // FFMCOST18-3: a by-value group through the leading allocator
            // (the general path took one exactly when the return is a group
            // and `CRATONVM_FFM_DOWNCALL_RETURN_ALLOCATOR` is on).
            (Some(_), Some(rl))
                if downcall_returns_group(ctx, Some(rl)) =>
            {
                match memo_group_return_in_regs(ctx, handle, rl) {
                    Some(in_regs) => group_return_memo_encode(params, capture, in_regs),
                    None => PLAIN_MEMO_NOT_PLAIN,
                }
            }
            _ => segment_memo_encode(ret, params, capture),
        }
    } else {
        PLAIN_MEMO_NOT_PLAIN
    };
    ctx.set_field(handle, DOWNCALL_PLAIN, Value::Long(memo));
    if ffm_dbg_plain() {
        eprintln!(
            "[ffm-plain] memo fn=0x{:x} plain={} segments={} memo=0x{memo:x}",
            downcall_fn_addr(ctx, handle),
            plain_memo_is_plain(memo),
            plain_memo_is_segment(memo)
        );
        // JIT round 18 wave 11 (lane ffmret): the engagement line of
        // `CRATONVM_FFM_DOWNCALL_MEMO_ADDRESS_RETURN` / `_STRUCT_RETURN`.
        if plain_memo_is_encoded(memo) && plain_memo_returns_object(memo) {
            eprintln!(
                "[ffm-ret] memo fn=0x{:x} ret={} unbounded={} in-regs={} direct-shape={}",
                downcall_fn_addr(ctx, handle),
                if memo & PLAIN_MEMO_RET_GROUP != 0 {
                    "group"
                } else {
                    "address"
                },
                memo & PLAIN_MEMO_RET_ADDRESS != 0 && memo & PLAIN_MEMO_ADDRESS_UNBOUNDED != 0,
                memo & PLAIN_MEMO_RET_GROUP != 0 && memo & PLAIN_MEMO_GROUP_IN_REGS != 0,
                memo_direct_shape(memo)
            );
        }
    }
}

/// Do the accepted handle's host, raw-memory and downcall checks answer `Ok`
/// without side effects? False under `CRATONVM_UNTRUSTED_CODE`, the refusal
/// `check_host_native_access_or_throw` makes), no `SecurityManager` is
/// installed or any VM has a capability policy. With neither, every
/// capability check is `Ok` and records nothing.
pub(in crate::panama) fn downcall_access_is_trivial(ctx: &dyn NativeContext) -> bool {
    !cratonvm_native_api::capability::any_capabilities_installed()
        && crate::security_manager::get_security_manager(ctx).is_none()
        && !cratonvm_types::flags::flags().io.untrusted_code
}

thread_local! {
    /// [`downcall_policy_settled`]'s memo: `(vm_identity,
    /// capabilities_generation, settled)`. Keyed on the index generation, so an
    /// install, replacement or removal of any policy re-resolves it; a `Cell`
    /// of a `Copy` payload, the shape of `capability_gate`'s raw-memory memo.
    pub(in crate::panama) static DOWNCALL_POLICY_MEMO: std::cell::Cell<Option<(usize, u64, bool)>> =
        const { std::cell::Cell::new(None) };
}

/// JIT round 16 wave 13 (lane ffmseg): may a memo-path call skip the
/// capability checks ([`require_native_access`]'s `RawMemory` and
/// `gate_foreign_downcall`'s `ForeignDowncall`)? True with no policy in the
/// process, and -- with `CRATONVM_FFM_DOWNCALL_PERMISSIVE_SETTLED`, see there
/// for the audit trade -- under this VM's `Permissive` policy. One atomic
/// load with no policy; otherwise one more load and a thread-local compare
/// once this thread has resolved the policy at the current index generation.
pub(in crate::panama) fn downcall_policy_settled(ctx: &dyn NativeContext) -> bool {
    use cratonvm_native_api::capability::{self as cap, CapabilityCheck};
    if !cap::any_capabilities_installed() {
        return true;
    }
    let vm = ctx.vm_identity();
    // Read before the policy, so a change after this load re-resolves.
    let generation = cap::capabilities_generation();
    if let Some((memo_vm, memo_gen, settled)) = DOWNCALL_POLICY_MEMO.with(|m| m.get()) {
        if memo_vm == vm && memo_gen == generation {
            return settled;
        }
    }
    let settled = ctx
        .vm_capabilities()
        .is_none_or(|set| set.mode() == cap::CapabilityMode::Permissive);
    DOWNCALL_POLICY_MEMO.with(|m| m.set(Some((vm, generation, settled))));
    settled
}

/// JIT round 16 wave 13 (lane ffmseg): the memo paths' access verdict. True
/// exactly when `require_native_access(ctx, "downcall")` and
/// `gate_foreign_downcall` would answer `Ok` and the only effect they would
/// have is a `Permissive` audit count ([`downcall_policy_settled`]): native
/// access enabled (false under `CRATONVM_UNTRUSTED_CODE` too), no
/// `SecurityManager`. Wave 11's [`downcall_access_is_trivial`] stays the
/// general path's (with `CRATONVM_FFM_DOWNCALL_PLAIN_MEMO`), so a handle's
/// first, classifying call still records its capabilities.
pub(in crate::panama) fn downcall_access_settled(ctx: &dyn NativeContext) -> bool {
    downcall_policy_settled(ctx)
        && downcall_no_security_manager(ctx)
        && !cratonvm_types::flags::flags().io.untrusted_code
}

/// `CRATONVM_FFM_SECURITY_MANAGER_MIRROR` (default ON since j27 wave 4, lane
/// FFMFLIP; `=0` restores the mutex-only test; a performance switch,
/// JIT round 16 wave 14, lane ffm3, proposal FFS16-1): the memo paths' "no
/// `SecurityManager`" test reads the process's mirror
/// (`security_manager::any_security_manager_installed`, one atomic load)
/// before it takes the process-wide security-state mutex: while no VM holds a
/// manager -- always on JDK 25, where `System.setSecurityManager` throws --
/// no call locks it. The answer is the same: a zero mirror means every VM's
/// slot is empty.
pub(in crate::panama) fn ffm_security_manager_mirror() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    *ON.get_or_init(|| {
        true
    })
}

/// JIT round 16 wave 14 (lane ffm3): the calling VM has no `SecurityManager`;
/// through the mirror first under [`ffm_security_manager_mirror`].
pub(in crate::panama) fn downcall_no_security_manager(ctx: &dyn NativeContext) -> bool {
    if ffm_security_manager_mirror() && !crate::security_manager::any_security_manager_installed() {
        return true;
    }
    crate::security_manager::get_security_manager(ctx).is_none()
}

/// JIT round 16 wave 11 (lane ffmcost): a memoised plain handle's call
/// (`CRATONVM_FFM_DOWNCALL_PLAIN_MEMO`): [`pe_downcall_invoke_memo`] without
/// segment encodings.
pub(in crate::panama) fn pe_downcall_invoke_plain(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Option<MethodCallResult> {
    pe_downcall_invoke_memo(ctx, args, false)
}

/// JIT round 16 wave 13 (lane ffmseg): the address `marshal_arg` would pass
/// for a NATIVE segment argument `seg`, or `None` -- decline to the general
/// path -- for anything it would refuse or treat specially: a heap segment
/// (refused, or the scratch-buffer carve-outs), a tagged arena handle or a
/// base in the low window (its screens). Field reads only.
pub(in crate::panama) fn downcall_native_segment_address(ctx: &dyn NativeContext, seg: ObjectRef) -> Option<usize> {
    use crate::panama_libffi as plf;
    if is_alias_heap_segment(ctx, seg) {
        return None;
    }
    let addr = plf::segment_address(ctx, seg);
    let size = plf::segment_byte_size(ctx, seg);
    let opaque = size == 0;
    // Round 19: an `Unsafe.allocateMemory` arena block passes its real
    // storage, as `marshal_arg` passes it.
    let addr = segment_row_arena_address(addr, size);
    let checked = if opaque && plf::opaque_low_pointer_args_allowed() {
        plf::checked_foreign_handle_tag(addr, "downcall pointer argument")
    } else {
        plf::checked_foreign_addr(addr, "downcall pointer argument")
    };
    checked.ok().map(|v| v as usize)
}

/// Round 19 (`r16-ffm-plain-downcall-costs-microseconds`): the address a
/// downcall row passes for a segment whose base is an arena HANDLE (every
/// `--real-jdk` `Arena` segment): the block's real storage for the call
/// (`panama_libffi::arena_call_address`, the general path's own translation).
/// Anything else, or a handle that does not resolve, is unchanged and meets
/// the screens as before.
pub(in crate::panama) fn segment_row_arena_address(addr: i64, size: i64) -> i64 {
    if crate::panama_libffi::is_arena_handle(addr) {
        crate::panama_libffi::arena_call_address(addr, size).unwrap_or(addr)
    } else {
        addr
    }
}

/// JIT round 16 wave 13 (lane ffmseg): `pe_segment_check_scope`'s verdict on
/// `seg` without its exceptions. `Some(Some(session))`: the segment's modelled
/// session, open and (if confined) owned by this thread; `Some(None)`: no
/// session to check, and not a closed synthetic arena's; `None`: the check
/// would raise (`WrongThreadException` first, then `IllegalStateException:
/// Already closed`, the JDK's order), so the caller declines and the general
/// path raises it. Reads fields only: the owner check reads the thread's
/// existing mirror and never mints one (`p67_owner_is_current_thread`, round 17
/// wave 2), so no GC point lies under the caller's raw references.
pub(in crate::panama) fn downcall_segment_scope_settled(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
) -> Option<Option<ObjectRef>> {
    let Some(session) = pe_segment_session(ctx, seg) else {
        if pe_segment_in_closed_synthetic_arena(ctx, seg) {
            return None;
        }
        // Round 19: a REAL session, checked as the acquire will check it; the
        // memo path then acquires a confined or shared one through
        // `p67_session_acquire`. A `NONCLOSEABLE` one (`Arena.global()`,
        // `ofAuto()`) has nothing to count.
        if let Some((real, _)) = pe_segment_real_session(ctx, seg) {
            use crate::phases_late::foreign_ffm as ff;
            let slots = ff::p67_session_slots(ctx, real);
            return match ff::p67_real_session_observe(ctx, real, slots, true) {
                Some((ff::P67RealVerdict::Open, seen)) => Some(seen.count.map(|_| real)),
                _ => None,
            };
        }
        return Some(None);
    };
    let slots = crate::phases_late::foreign_ffm::p67_session_slots(ctx, session);
    if let Value::Object(Some(owner)) = ctx.get_field(session, slots.owner) {
        if !crate::phases_late::foreign_ffm::p67_owner_is_current_thread(ctx, owner) {
            return None;
        }
    }
    if matches!(ctx.get_field(session, slots.state), Value::Int(0)) {
        return None;
    }
    Some(Some(session))
}

// ---------------------------------------------------------------------------
// The segment row (JIT round 17 wave 1, lane ffmseg; proposal FFL16-2,
// `CRATONVM_FFM_DOWNCALL_SEGMENT_ROW`)
// ---------------------------------------------------------------------------
//
// Per segment argument the memo path paid, through `NativeContext`:
// `is_alias_heap_segment` (`class_name_of_id`: a lock and a `String`, then a
// `base` lookup by NAME), `segment_address` (`min` by name, then
// `is_real_heap_segment` again), `segment_byte_size` (`length` by name),
// `pe_segment_session` (the arena and session class memos), and per session
// `p67_session_slots` -- four field-NAME resolutions under the class-manager
// lock -- about five times (the scope check, `p67_session_modelled`,
// `p67_session_is_real`, the acquire's count and the release's). For the one
// carrier shape every native segment of this VM has (`alloc_segment_carrier`'s
// six slots: base, size, scope, read-only, alive, offset) and the one session
// class it stamps into the scope slot (`MemorySessionImpl`, `foreign_ffm`'s
// model), every one of those answers is a function of the two class ids. The
// row remembers them per thread, keyed on the VM, and reads the slots by
// index. Any other shape (a real JDK `NativeMemorySegmentImpl`, an arena in
// the scope slot, a session class it has not verified) takes the readers above
// for that segment, unchanged.



/// `CRATONVM_FFM_ACQUIRE_HANDSHAKE`, read here as `foreign_ffm` reads it
/// (default on): the row's acquire and release are the handshake's (an access
/// window from the state check to the count, a compare-and-swap count). With
/// it off the row takes `foreign_ffm`'s own acquire and release.
pub(in crate::panama) fn segment_row_acquire_handshake() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// What the row has verified on this thread for one VM: the class ids of the
/// segment carrier and of the modelled session, with the session's slot map.
/// Ids only, no reference; a pure cache (every answer is re-derivable from
/// the two classes, neither of which is ever unloaded).
#[derive(Clone, Copy)]
pub(in crate::panama) struct SegmentRowShape {
    pub(in crate::panama) vm: usize,
    pub(in crate::panama) carrier: Option<u32>,
    pub(in crate::panama) session: Option<(u32, crate::phases_late::foreign_ffm::P67SessionSlots)>,
    /// Round 19: up to four REAL session classes (`ConfinedSession`,
    /// `SharedSession`, `ImplicitSession`, `GlobalSession`) with their slot
    /// maps (`P67SessionSlots::real`).
    pub(in crate::panama) real_sessions: [Option<(u32, crate::phases_late::foreign_ffm::P67SessionSlots)>; 4],
    /// JIT round 17 wave 2 (lane ffm2; `CRATONVM_FFM_SEGMENT_ROW_REAL_SEGMENTS`):
    /// up to two real JDK native segment classes (`NativeMemorySegmentImpl`,
    /// `MappedMemorySegmentImpl`) with their resolved slots.
    pub(in crate::panama) real: [Option<(u32, RealSegmentSlots)>; 2],
    /// The last class the real arm refused (a heap segment class, say), so a
    /// refused shape costs one compare, not the name and field resolutions.
    pub(in crate::panama) not_real: Option<u32>,
}

/// The slots of a real JDK native segment class
/// (`AbstractMemorySegmentImpl{length, readOnly, scope}` then
/// `NativeMemorySegmentImpl{min}`), resolved once by name per class id.
#[derive(Clone, Copy)]
pub(in crate::panama) struct RealSegmentSlots {
    pub(in crate::panama) min: usize,
    pub(in crate::panama) length: usize,
    /// `AbstractMemorySegmentImpl.readOnly` (JIT round 22, lane memory: the
    /// struct-return store's fast path, [`downcall_store_register_group_real`]).
    pub(in crate::panama) read_only: usize,
}



thread_local! {
    /// [`SegmentRowShape`] of the last VM this thread made a segment-row call
    /// in. A `Cell` of a `Copy` payload, like [`DOWNCALL_POLICY_MEMO`].
    pub(in crate::panama) static SEGMENT_ROW_SHAPE: std::cell::Cell<Option<SegmentRowShape>> =
        const { std::cell::Cell::new(None) };
}

/// The thread's shape memo for `vm` (a fresh, empty one for another VM).
/// Under `cfg(test)` always empty: mock class ids are per context, the reason
/// [`PeClassMemo`] is bypassed there too.
pub(in crate::panama) fn segment_row_shape(vm: usize) -> SegmentRowShape {
    let empty = SegmentRowShape {
        vm,
        carrier: None,
        session: None,
        real_sessions: [None; 4],
        real: [None; 2],
        not_real: None,
    };
    if cfg!(test) {
        return empty;
    }
    match SEGMENT_ROW_SHAPE.with(std::cell::Cell::get) {
        Some(shape) if shape.vm == vm => shape,
        _ => empty,
    }
}

pub(in crate::panama) fn segment_row_remember(shape: SegmentRowShape) {
    if !cfg!(test) {
        SEGMENT_ROW_SHAPE.with(|memo| memo.set(Some(shape)));
    }
}

/// Is `seg`'s class the native segment carrier: [`CRATON_SEGMENT_CLASS`], or
/// the interface stamp [`alloc_segment_carrier`] falls back to where that
/// class is refused? Both declare no instance field, so every by-name read
/// the general readers make of them (`min`, `length`, `base`) misses, and
/// neither name contains `HeapMemorySegmentImpl`.
pub(in crate::panama) fn segment_row_is_carrier(
    ctx: &dyn NativeContext,
    shape: &mut SegmentRowShape,
    seg: ObjectRef,
) -> bool {
    let class_id = ctx.class_id_of_object(seg);
    let raw = class_id.as_u32();
    if shape.carrier == Some(raw) {
        return true;
    }
    let carrier = matches!(
        ctx.class_name_arc_of_id(class_id).as_deref(),
        Some(CRATON_SEGMENT_CLASS | PE_SEGMENT_INTERFACE)
    );
    if carrier && raw != 0 {
        shape.carrier = Some(raw);
        segment_row_remember(*shape);
    }
    carrier
}

/// `session`'s slot map, if it is a session [`pe_session_modelled`] accepts:
/// the exact `MemorySessionImpl` class (verified once per VM and thread), wide
/// enough for the map, its state word an `Int`. `None` otherwise -- the
/// general readers then decide.
pub(in crate::panama) fn segment_row_session_slots(
    ctx: &dyn NativeContext,
    shape: &mut SegmentRowShape,
    session: ObjectRef,
) -> Option<crate::phases_late::foreign_ffm::P67SessionSlots> {
    let class_id = ctx.class_id_of_object(session);
    let raw = class_id.as_u32();
    if let Some((_, slots)) = shape
        .real_sessions
        .iter()
        .flatten()
        .find(|(id, _)| *id == raw)
    {
        return Some(*slots);
    }
    let slots = match shape.session {
        Some((id, slots)) if id == raw => slots,
        _ => {
            if ctx.class_name_arc_of_id(class_id).as_deref() != Some(PE_SESSION_CLASS) {
                // Round 19: a REAL session class, remembered by id.
                let slots = crate::phases_late::foreign_ffm::p67_session_slots(ctx, session);
                if !slots.real || raw == 0 {
                    return None;
                }
                shape.real_sessions = [
                    Some((raw, slots)),
                    shape.real_sessions[0],
                    shape.real_sessions[1],
                    shape.real_sessions[2],
                ];
                segment_row_remember(*shape);
                return Some(slots);
            }
            let slots = crate::phases_late::foreign_ffm::p67_session_slots(ctx, session);
            if raw != 0 {
                shape.session = Some((raw, slots));
                segment_row_remember(*shape);
            }
            slots
        }
    };
    (ctx.object_num_fields(session) >= slots.required_width()
        && matches!(ctx.get_field(session, slots.state), Value::Int(_)))
    .then_some(slots)
}

/// [`segment_row_read`]'s answer for one segment argument.
#[derive(Clone, Copy)]
pub(in crate::panama) enum SegmentRowArg {
    /// The carrier shape: the address `marshal_arg` would pass, and the scope
    /// slot read as [`pe_segment_session`] reads it -- `None` (no session),
    /// a modelled session with its slot map, or `Unknown` (something else is
    /// there; [`downcall_segment_scope_settled`] decides).
    Carrier {
        addr: usize,
        size: i64,
        scope: SegmentRowScope,
    },
    /// Not the carrier shape: the general readers decide this segment.
    Other,
}

#[derive(Clone, Copy)]
pub(in crate::panama) enum SegmentRowScope {
    None,
    Session(ObjectRef, crate::phases_late::foreign_ffm::P67SessionSlots),
    Unknown,
}

/// One segment argument through the row. `None`: the address screen refuses
/// (decline, as [`downcall_native_segment_address`] declines). The carrier
/// shape is EXACTLY six slots: eight is the heap alias carrier
/// ([`is_alias_heap_segment`]), and every other width keeps the general
/// readers. For six slots the general readers answer `slot0 + slot5` for the
/// address (each a `Long`, else 0), `slot1` for the size, and the scope from
/// slot 2: `null` is no session, a modelled session is the session (no
/// session class is an arena class, which they ask about first), anything
/// else is theirs to resolve.
pub(in crate::panama) fn segment_row_read(
    ctx: &dyn NativeContext,
    shape: &mut SegmentRowShape,
    seg: ObjectRef,
) -> Option<SegmentRowArg> {
    use crate::panama_libffi as plf;
    // JIT round 22 (lane memory, MEM20-6 direction 1): a real native segment
    // class already on file (`shape.real`) is answered by its class id alone.
    // Its width was screened when it was filed and is a bootstrap class's,
    // fixed; and no real class is six slots wide (`NativeMemorySegmentImpl`
    // has four, `MappedMemorySegmentImpl` five), so the carrier test below
    // could not have claimed it. Two receiver validations per segment
    // argument fewer.
    {
        let raw = ctx.class_id_of_object(seg).as_u32();
        if raw != 0 {
            if let Some((_, slots)) = shape.real.iter().flatten().find(|(id, _)| *id == raw) {
                let slots = *slots;
                return segment_row_read_real_with(ctx, shape, seg, slots);
            }
        }
    }
    if ctx.object_num_fields(seg) != 6 || !segment_row_is_carrier(ctx, shape, seg) {
        // JIT round 17 wave 2 (lane ffm2): a real JDK native segment.
        return segment_row_read_real(ctx, shape, seg);
    }
    const RAW: u8 = cratonvm_native_api::RAW_SLOT_DESCRIPTOR;
    let mut words = [Value::Object(None); 4];
    ctx.get_fields_typed(seg, &[(0, RAW), (1, RAW), (2, RAW), (5, RAW)], &mut words);
    let long = |v: Value| match v {
        Value::Long(n) => n,
        _ => 0,
    };
    let addr = long(words[0]).wrapping_add(long(words[3]));
    let size = long(words[1]);
    let addr = segment_row_arena_address(addr, size);
    let checked = if size == 0 && plf::opaque_low_pointer_args_allowed() {
        plf::checked_foreign_handle_tag(addr, "downcall pointer argument")
    } else {
        plf::checked_foreign_addr(addr, "downcall pointer argument")
    };
    let addr = checked.ok()? as usize;
    let scope = match words[2] {
        Value::Object(None) => SegmentRowScope::None,
        Value::Object(Some(owner)) => match segment_row_session_slots(ctx, shape, owner) {
            Some(slots) => SegmentRowScope::Session(owner, slots),
            None => SegmentRowScope::Unknown,
        },
        _ => SegmentRowScope::Unknown,
    };
    Some(SegmentRowArg::Carrier { addr, size, scope })
}

/// The real JDK native segment classes the row reads by slot
/// ([`segment_row_read_real`]). Both extend `NativeMemorySegmentImpl`; neither
/// is a heap segment (`HeapMemorySegmentImpl` is their sibling), and both are
/// bootstrap classes, so a remembered class id is never reused.
pub(in crate::panama) const REAL_NATIVE_SEGMENT_CLASSES: [&str; 2] = [
    "jdk/internal/foreign/NativeMemorySegmentImpl",
    "jdk/internal/foreign/MappedMemorySegmentImpl",
];

/// `seg`'s class's [`RealSegmentSlots`], if it is one of
/// [`REAL_NATIVE_SEGMENT_CLASSES`] whose layout the general readers would read
/// exactly as the row does: `min` and `length` resolve, no `base` resolves
/// (`is_real_heap_segment`'s field test), `scope` resolves to slot
/// [`PE_SEGMENT_ARENA_FIELD`] (where `pe_segment_session` reads it first), and
/// the object is no wider than the heap alias carrier's threshold
/// (`is_alias_heap_segment`'s width test).
pub(in crate::panama) fn segment_row_real_slots(
    ctx: &dyn NativeContext,
    shape: &mut SegmentRowShape,
    seg: ObjectRef,
) -> Option<RealSegmentSlots> {
    if ctx.object_num_fields(seg) > SEG_HEAP_START_FIELD {
        return None;
    }
    let class_id = ctx.class_id_of_object(seg);
    let raw = class_id.as_u32();
    if raw == 0 || shape.not_real == Some(raw) {
        return None;
    }
    if let Some((_, slots)) = shape.real.iter().flatten().find(|(id, _)| *id == raw) {
        return Some(*slots);
    }
    let resolved = (|| {
        let name = ctx.class_name_arc_of_id(class_id)?;
        if !REAL_NATIVE_SEGMENT_CLASSES.contains(&&*name) {
            return None;
        }
        if ctx
            .resolve_field_index_by_class_id(class_id, "base")
            .is_some()
            || ctx.resolve_field_index_by_class_id(class_id, "scope")
                != Some(PE_SEGMENT_ARENA_FIELD)
        {
            return None;
        }
        Some(RealSegmentSlots {
            min: ctx.resolve_field_index_by_class_id(class_id, "min")?,
            length: ctx.resolve_field_index_by_class_id(class_id, "length")?,
            read_only: ctx.resolve_field_index_by_class_id(class_id, "readOnly")?,
        })
    })();
    match resolved {
        Some(slots) => {
            // Two entries: the newer replaces the older.
            shape.real = [Some((raw, slots)), shape.real[0]];
        }
        None => shape.not_real = Some(raw),
    }
    segment_row_remember(*shape);
    resolved
}

/// [`segment_row_read`] for a segment that is not the carrier: a real JDK
/// native segment ([`segment_row_real_slots`]) is read by slot -- the address
/// `min` (`segment_address`'s by-name read), the size `length`
/// (`segment_byte_size`'s), screened as the carrier's -- and its scope from
/// slot 2 as the carrier's (`null`: none; a verified modelled session; else
/// `Unknown`, the general readers'). Anything else, or a slot that does not
/// hold a `long`, is [`SegmentRowArg::Other`].
pub(in crate::panama) fn segment_row_read_real(
    ctx: &dyn NativeContext,
    shape: &mut SegmentRowShape,
    seg: ObjectRef,
) -> Option<SegmentRowArg> {
    let Some(slots) = segment_row_real_slots(ctx, shape, seg) else {
        return Some(SegmentRowArg::Other);
    };
    segment_row_read_real_with(ctx, shape, seg, slots)
}

/// [`segment_row_read_real`] once `seg`'s class is known to be a real native
/// segment class with `slots`.
pub(in crate::panama) fn segment_row_read_real_with(
    ctx: &dyn NativeContext,
    shape: &mut SegmentRowShape,
    seg: ObjectRef,
    slots: RealSegmentSlots,
) -> Option<SegmentRowArg> {
    use crate::panama_libffi as plf;
    // One batch: `NativeMemorySegmentImpl.min`, `AbstractMemorySegmentImpl
    // .length` and `.scope` (round 19: three single reads were three receiver
    // validations and descriptor resolutions per segment per call).
    let mut words = [Value::Object(None); 3];
    ctx.get_fields_typed(
        seg,
        &[
            (slots.min, b'J'),
            (slots.length, b'J'),
            (PE_SEGMENT_ARENA_FIELD, b'L'),
        ],
        &mut words,
    );
    let (Value::Long(addr), Value::Long(size)) = (words[0], words[1]) else {
        return Some(SegmentRowArg::Other);
    };
    let addr = segment_row_arena_address(addr, size);
    let checked = if size == 0 && plf::opaque_low_pointer_args_allowed() {
        plf::checked_foreign_handle_tag(addr, "downcall pointer argument")
    } else {
        plf::checked_foreign_addr(addr, "downcall pointer argument")
    };
    let addr = checked.ok()? as usize;
    let scope = match words[2] {
        Value::Object(None) => SegmentRowScope::None,
        Value::Object(Some(owner)) => match segment_row_session_slots(ctx, shape, owner) {
            Some(slots) => SegmentRowScope::Session(owner, slots),
            None => SegmentRowScope::Unknown,
        },
        _ => SegmentRowScope::Unknown,
    };
    Some(SegmentRowArg::Carrier { addr, size, scope })
}

/// One memo-path segment argument: through [`segment_row_read`] when the row
/// is on (`shape` is `Some`) and the segment has the carrier shape, else
/// through [`downcall_native_segment_address`]. The address, and with the row
/// the scope and the byte size it read. `None`: decline.
pub(in crate::panama) fn memo_segment_address(
    ctx: &dyn NativeContext,
    shape: &mut Option<SegmentRowShape>,
    seg: ObjectRef,
) -> Option<(usize, Option<(SegmentRowScope, i64)>)> {
    if let Some(shape) = shape {
        if let SegmentRowArg::Carrier { addr, size, scope } = segment_row_read(ctx, shape, seg)? {
            return Some((addr, Some((scope, size))));
        }
    }
    downcall_native_segment_address(ctx, seg).map(|addr| (addr, None))
}

/// [`downcall_segment_scope_settled`] for a session the row verified:
/// confinement first, then liveness, with the remembered slots. `false`: the
/// check would raise, so the caller declines and the general path raises it.
pub(in crate::panama) fn segment_row_scope_open(
    ctx: &mut dyn NativeContext,
    session: ObjectRef,
    slots: crate::phases_late::foreign_ffm::P67SessionSlots,
) -> bool {
    if slots.real {
        use crate::phases_late::foreign_ffm as ff;
        return ff::p67_real_session_verdict(ctx, session, slots, true)
            == Some(ff::P67RealVerdict::Open);
    }
    if let Value::Object(Some(owner)) = ctx.get_field(session, slots.owner) {
        if !crate::phases_late::foreign_ffm::p67_owner_is_current_thread(ctx, owner) {
            return false;
        }
    }
    !matches!(ctx.get_field(session, slots.state), Value::Int(0))}

/// `foreign_ffm::p67_session_add_acquires` with the remembered slots: move the
/// count by `delta` by compare-and-swap, never below zero; a slot that does not
/// hold an `int` gets the plain write that function makes.
pub(in crate::panama) fn segment_row_add_acquires(
    ctx: &mut dyn NativeContext,
    session: ObjectRef,
    slots: crate::phases_late::foreign_ffm::P67SessionSlots,
    delta: i32,
) {
    // Round 19: a REAL session's count moves with the JDK's semantics (a
    // confined session the row checked can only be counted up; a shared one
    // that closed meanwhile is refused here and its release finds nothing).
    if slots.real {
        use crate::phases_late::foreign_ffm as ff;
        if delta > 0 {
            ff::p67_real_session_acquire_count(ctx, session, slots);
        } else {
            ff::p67_real_session_release_count(ctx, session, slots);
        }
        return;
    }
    loop {
        let current = ctx.get_field(session, slots.acquires);
        let Value::Int(count) = current else {
            ctx.set_field(session, slots.acquires, Value::Int(delta.max(0)));
            return;
        };
        let next = count.saturating_add(delta).max(0);
        if next == count
            || ctx.compare_and_swap_field(session, slots.acquires, current, Value::Int(next))
        {
            return;
        }
    }
}

/// `foreign_ffm::p67_session_acquire` for a session the row verified, under
/// the acquire handshake: the access window open from the confinement and
/// state check to the count, so a shared close either sees the count or is
/// seen. `false` -- nothing counted -- when the check fails; the caller then
/// runs `p67_session_acquire` itself, which raises what applies (or counts,
/// should the state have moved back meanwhile: a close refused for this very
/// reason reopens the session).
pub(in crate::panama) fn segment_row_acquire(
    ctx: &mut dyn NativeContext,
    session: ObjectRef,
    slots: crate::phases_late::foreign_ffm::P67SessionSlots,
) -> bool {
    // Round 19: a REAL session's protocol is its count's compare-and-swap (a
    // shared close swaps 0 -> -1 first, and the count refuses a negative
    // one), so no access window is needed and the row's own check stands: the
    // callers checked the session (`segment_row_scope_open`) with no GC point
    // and no Java since, and a confined session closes only on its owner,
    // this thread.
    if slots.real {
        return crate::phases_late::foreign_ffm::p67_real_session_acquire_count(
            ctx, session, slots,
        );
    }
    // JIT round 17 wave 2 (lane ffm3; FFMSEG17-3,
    // `CRATONVM_FFM_CONFINED_ACQUIRE_NO_WINDOW`): a confined session owned by
    // this thread is acquired without the access window.
    if ffm_confined_acquire_no_window()
        && matches!(ctx.get_field(session, slots.owner), Value::Object(Some(_)))
    {
        if !segment_row_scope_open(ctx, session, slots) {
            return false;
        }
        segment_row_add_acquires(ctx, session, slots, 1);
        return true;
    }
    let _window = crate::ffm_fast::AccessWindow::open();
    // The state word was an `Int` when the row verified the session (and a
    // slot's kind never changes), so this is `p67_session_check_valid` with
    // `p67_session_modelled` holding.
    if !segment_row_scope_open(ctx, session, slots) {
        return false;
    }
    segment_row_add_acquires(ctx, session, slots, 1);
    true
}

/// `CRATONVM_FFM_CONFINED_ACQUIRE_NO_WINDOW` (default OFF, a performance
/// switch for the orchestrator's A/B; JIT round 17 wave 2, lane ffm3,
/// FFMSEG17-3): [`segment_row_acquire`] (the segment row and the direct
/// segment row) acquires a CONFINED session -- `owner` set, and
/// [`segment_row_scope_open`] has checked it is this thread -- without
/// `ffm_fast::AccessWindow::open`.
///
/// Why the window is not needed there: it exists so a SHARED close, running
/// on another thread, either sees the count or is seen by the state check
/// (`foreign_ffm::p67_session_acquire`'s doc). A confined session is closed
/// only by its owner (`p67_session_just_close` runs `p67_session_check_valid`
/// first, which refuses any other thread with `WrongThreadException`), and the
/// owner is this thread, which is running this acquire, so no close can run
/// between the check and the count; a close from an up-call inside the call
/// comes after the count and refuses (`Session is acquired by 1 clients`), as
/// HotSpot's `ConfinedSession` does. The count still moves by
/// compare-and-swap, so a release from another thread (the JDK's
/// `asyncReleaseCount` case) cannot be lost. Saves the window's thread-local
/// lookups and its two sequence-word writes per confined session per call;
/// HotSpot's `ConfinedSession.acquire0` is a plain increment. Read once.
pub(in crate::panama) fn ffm_confined_acquire_no_window() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        false
    })
}

/// The segments one memo-path call may hold: the capture-state segment and
/// one per parameter.
pub(in crate::panama) const MEMO_CALL_SEGMENTS: usize = PLAIN_MEMO_MAX_ARGS + 1;

/// JIT round 16 waves 11 and 13 (lanes ffmcost, ffmseg): a memoised handle's
/// call. `None` -- before any effect -- for anything it cannot vouch for, and
/// the caller then runs the general path, which raises whatever exception
/// applies:
///
/// * the handle is not (yet) classified, its CIF is not cached, or it is a
///   segment encoding and `segments` is off (or the scope checks or the
///   session acquire are switched off, `CRATONVM_FFM_BULK_SCOPE_CHECK` /
///   `CRATONVM_FFM_DOWNCALL_ACQUIRE`, whose general-path behaviour this does
///   not reproduce);
/// * the arguments are not one `Value` per memoised parameter (plus the state
///   segment of a capture handle) of the parameter's own shape: a primitive
///   for a value kind, a non-null native segment for `ADDRESS` (the legacy
///   `invoke(Object[])` bridge, an arity mismatch, `null`, a heap segment);
/// * an address fails `marshal_arg`'s screens
///   ([`downcall_native_segment_address`]), or a segment's scope check would
///   raise ([`downcall_segment_scope_settled`]);
/// * the access verdict is not settled ([`downcall_access_settled`]);
/// * the target fails `validated_fn_ptr`.
///
/// A segment call then acquires each distinct session for the call (pinned,
/// as `pe_downcall_acquire_sessions` pins them) and releases it after; an
/// acquire refused here can only be a close that raced the check (a shared
/// arena closed on another thread), and its `Already closed` is returned as
/// the general path would raise it, with what was acquired released. The
/// capture state is read straight after the C call and written after the
/// releases, like the general path. The `ActiveContextGuard` (an upcall from C
/// still needs it) and the GC-safe region are the general path's.
pub(in crate::panama) fn pe_downcall_invoke_memo(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    segments: bool,
) -> Option<MethodCallResult> {
    pe_downcall_invoke_memo_with(ctx, args, segments, true)
}

/// The handle words the segment row reads in one batch: the memo, the CIF,
/// the target and the critical mark. All four sit past the `MethodHandle`
/// class's declared fields ([`DOWNCALL_BASE`]), where `get_field` resolves
/// no descriptor and reads the slot as stored, which is what the raw
/// descriptor reads too.
pub(in crate::panama) const MEMO_HANDLE_WORDS: [(usize, u8); 4] = [
    (DOWNCALL_PLAIN, cratonvm_native_api::RAW_SLOT_DESCRIPTOR),
    (DOWNCALL_CIF, cratonvm_native_api::RAW_SLOT_DESCRIPTOR),
    (DOWNCALL_FN_ADDR, cratonvm_native_api::RAW_SLOT_DESCRIPTOR),
    (DOWNCALL_CRITICAL, cratonvm_native_api::RAW_SLOT_DESCRIPTOR),
];

/// [`pe_downcall_invoke_memo`] with the segment row
/// (`CRATONVM_FFM_DOWNCALL_SEGMENT_ROW`) passed in, so a unit test runs both
/// arms in one process.
pub(in crate::panama) fn pe_downcall_invoke_memo_with(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    segments: bool,
    row: bool,
) -> Option<MethodCallResult> {
    use crate::panama_libffi as plf;
    use libffi::middle::CodePtr;

    let _arena_call = plf::ArenaCallScope::open();
    let Some(Value::Object(Some(handle))) = args.first() else {
        return None;
    };
    let handle = *handle;
    if ctx.object_num_fields(handle) <= DOWNCALL_PLAIN {
        return None;
    }
    // JIT round 17 wave 1 (lane ffmseg): the row reads the four words in one
    // forwarding; the reads below take them from here.
    let mut words = [Value::Object(None); 4];
    if row {
        ctx.get_fields_typed(handle, &MEMO_HANDLE_WORDS, &mut words);
    }
    let memo_word = if row {
        words[0]
    } else {
        ctx.get_field(handle, DOWNCALL_PLAIN)
    };
    let Value::Long(memo) = memo_word else {
        return None;
    };
    if !plain_memo_is_plain(memo)
        && !(segments
            && plain_memo_is_segment(memo)
            && ffm_bulk_scope_check())
    {
        return None;
    }
    // JIT round 18 wave 11 (lane ffmret): a group return's allocator leads,
    // then the capture-state segment.
    let alloc_lead = usize::from(memo & PLAIN_MEMO_RET_GROUP != 0);
    let lead = plain_memo_lead(memo);
    let call_args = &args[1..];
    if call_args.len() != plain_memo_arity(memo) + lead {
        return None;
    }
    let cif_word = if row {
        words[1]
    } else {
        ctx.get_field(handle, DOWNCALL_CIF)
    };
    let cached_cif_u64: u64 = match cif_word {
        Value::Long(n) if n != 0 => n as u64,
        _ => return None,
    };
    let fn_addr = if row {
        match words[2] {
            Value::Long(n) => n,
            _ => 0,
        }
    } else {
        downcall_fn_addr(ctx, handle)
    };
    if fn_addr == 0 || fn_addr == DOWNCALL_ADDRESS_FROM_ARG0 {
        return None;
    }
    // The row asks the `SecurityManager` mirror before the mutex: the same
    // answer, the direct row's verdict.
    let settled = if row {
        downcall_direct_access_settled(&*ctx)
    } else {
        downcall_access_settled(&*ctx)
    };
    if !settled || validated_fn_ptr::<unsafe extern "C" fn()>(fn_addr).is_err() {
        return None;
    }

    // The arguments, and the segments among them, with no effect yet.
    // JIT round 17 wave 1 (lane ffmseg): with the row, a segment of the
    // carrier shape is read by slot index and its scope comes with it
    // (`seg_scopes`); any other segment keeps the general readers.
    let mut shape = row.then(|| segment_row_shape(ctx.vm_identity()));
    let mut cells = [0u64; PLAIN_MEMO_MAX_ARGS];
    let mut segs = [None::<ObjectRef>; MEMO_CALL_SEGMENTS];
    let mut seg_scopes = [None::<SegmentRowScope>; MEMO_CALL_SEGMENTS];
    let mut seg_count = 0usize;
    // The state segment's address and size, when the row read them.
    let mut state_known: Option<(usize, i64)> = None;
    // JIT round 18 wave 11 (lane ffmret; FFMCOST18-3): the return allocator
    // (`null` declines: the general path raises the NPE before the call) and
    // the group's size and alignment from the cached CIF (equal to the Java
    // layout's, checked at the classification).
    let allocator: Option<ObjectRef> = if alloc_lead == 1 {
        let Value::Object(Some(allocator)) = call_args[0] else {
            return None;
        };
        Some(allocator)
    } else {
        None
    };
    let group: (usize, usize, bool, Option<usize>) = if alloc_lead == 1 {
        // SAFETY: the handle's cached CIF (`cached_cif_u64`, non-zero above).
        let (size, align, rtype) = unsafe { cif_return_size_align(cached_cif_u64) }?;
        if size == 0 || size > 16 {
            return None;
        }
        // SAFETY: that CIF's prepared return type.
        let in_registers = unsafe { group_returned_in_registers(size, rtype) };
        // JIT round 18 wave 11 (lane ffmret;
        // `CRATONVM_FFM_GROUP_RETURN_ALLOCATE_BEFORE_CALL`): an in-memory
        // group calls its allocator BEFORE the C call (HotSpot's
        // `adaptDowncallForIMR`), which only the general path does.
        if !in_registers {
            return None;
        }
        // SAFETY: as above.
        let hfa_width = unsafe { group_hfa_width(rtype) };
        (size, align.max(1), in_registers, hfa_width)
    } else {
        (0, 1, false, None)
    };
    let state: Option<ObjectRef> = if memo & PLAIN_MEMO_CAPTURE != 0 {
        let Value::Object(Some(state)) = call_args[alloc_lead] else {
            return None;
        };
        let (addr, read) = memo_segment_address(ctx, &mut shape, state)?;
        // JIT round 17 wave 2 (lane ffm2): a state segment smaller than the
        // capture layout is the general path's `IndexOutOfBoundsException`.
        let size = match read {
            Some((_, size)) => size,
            None if ffm_capture_segment_size_check() => plf::segment_byte_size(ctx, state),
            None => i64::MAX,
        };
        // JIT round 17 wave 2 (lane ffm3): and `MemorySegment.NULL` or a
        // misaligned one is the general path's `IllegalArgumentException`.
        if capture_segment_declined(addr, size) {
            return None;
        }
        if let Some((scope, size)) = read {
            seg_scopes[0] = Some(scope);
            state_known = Some((addr, size));
        }
        segs[0] = Some(state);
        seg_count = 1;
        Some(state)
    } else {
        None
    };
    for (i, value) in call_args[lead..].iter().enumerate() {
        let kind = plain_memo_param(memo, i);
        if kind == LAYOUT_ADDRESS {
            let Value::Object(Some(seg)) = *value else {
                return None;
            };
            // The pointer at offset 0 of the cell, where libffi reads a
            // pointer-typed argument (as `marshal_arg`'s `usize` slot).
            let (addr, read) = memo_segment_address(ctx, &mut shape, seg)?;
            cells[i] = addr as u64;
            segs[seg_count] = Some(seg);
            seg_scopes[seg_count] = read.map(|(scope, _)| scope);
            seg_count += 1;
            continue;
        }
        if !matches!(
            value,
            Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_)
        ) {
            return None;
        }
        if !plf::marshal_primitive_into(kind, value, &mut cells[i]) {
            return None;
        }
    }
    let mut sessions = [None::<ObjectRef>; MEMO_CALL_SEGMENTS];
    // The row's slot map of each session it verified (JIT round 17 wave 1).
    let mut session_slots =
        [None::<crate::phases_late::foreign_ffm::P67SessionSlots>; MEMO_CALL_SEGMENTS];
    let mut session_count = 0usize;
    for (seg, scope) in segs[..seg_count].iter().zip(seg_scopes) {
        let Some(seg) = *seg else {
            continue;
        };
        let resolved = match scope {
            Some(SegmentRowScope::None) => None,
            Some(SegmentRowScope::Session(session, slots)) => {
                if !segment_row_scope_open(ctx, session, slots) {
                    return None;
                }
                Some((session, Some(slots)))
            }
            Some(SegmentRowScope::Unknown) | None => {
                downcall_segment_scope_settled(ctx, seg)?.map(|session| (session, None))
            }
        };
        if let Some((session, slots)) = resolved {
            if !sessions[..session_count].contains(&Some(session)) {
                sessions[session_count] = Some(session);
                session_slots[session_count] = slots;
                session_count += 1;
            }
        }
    }
    let mut arg_ptrs = [std::ptr::null_mut::<std::ffi::c_void>(); PLAIN_MEMO_MAX_ARGS];
    for (ptr, cell) in arg_ptrs.iter_mut().zip(cells.iter_mut()) {
        *ptr = cell as *mut u64 as *mut std::ffi::c_void;
    }

    if ffm_dbg_plain() && memo & PLAIN_MEMO_HIT_SEEN == 0 {
        ctx.set_field(
            handle,
            DOWNCALL_PLAIN,
            Value::Long(memo | PLAIN_MEMO_HIT_SEEN),
        );
        eprintln!(
            "[ffm-plain] hit fn=0x{fn_addr:x} segments={} direct={} row={row}",
            plain_memo_is_segment(memo),
            memo_direct_shape(memo)
        );
    }

    // The general path's decision, taken while `handle` is fresh.
    // JIT round 16 wave 14 (lane ffm3): or the lazy in-native state.
    let critical = if row {
        matches!(words[3], Value::Int(1 | DOWNCALL_CRITICAL_ALLOW_HEAP))
    } else {
        downcall_is_critical(ctx, handle)
    };
    let gc_safe = (ffm_downcall_gc_safe() || ffm_downcall_in_native_lazy()) && !critical;
    // SAFETY: `cached_cif_u64` was produced by `box_cif_to_u64` on this
    // handle's first call (`pe_downcall_prepare`) and lives as long as the
    // handle (see `pe_downcall_invoke`'s T5.6.3 note).
    let raw_cif_ptr = unsafe { plf::cached_cif_ref(cached_cif_u64) }?.as_raw_ptr();
    // JIT round 16 wave 16 (lane ffmlean; FFS16-4): the direct stub's
    // register images, computed before any effect.
    // JIT round 18 wave 11 (lane ffmret): a group return keeps `ffi_call`
    // here, which writes the whole group into `ret` on every ABI.
    let direct_regs =
        (memo_direct_shape(memo) && memo & PLAIN_MEMO_RET_GROUP == 0)
            .then(|| memo_direct_regs(memo, &cells[..plain_memo_arity(memo)]));

    // Acquire each distinct session for the call, every one pinned before the
    // first acquire (`pe_downcall_acquire_sessions`' order), then pin the
    // state segment, which is written after the call: an upcall from C can
    // collect and move any of them.
    let sessions = &sessions[..session_count];
    let mut session_pins = [0usize; MEMO_CALL_SEGMENTS];
    for (pin, session) in session_pins.iter_mut().zip(sessions) {
        if let Some(session) = session {
            *pin = ctx.pin_native_root(*session);
        }
    }
    // JIT round 17 wave 1 (lane ffmseg): a session the row verified is
    // acquired and released through its remembered slots, under the same
    // handshake; a refused fast acquire falls to `p67_session_acquire`, which
    // raises what applies.
    let row_handshake = row && segment_row_acquire_handshake();
    for (i, (session, pin)) in sessions.iter().zip(session_pins).enumerate() {
        let Some(session) = *session else {
            continue;
        };
        let mut current = ctx.read_native_pin(pin, session);
        let fast = match session_slots[i] {
            Some(slots) if row_handshake => segment_row_acquire(ctx, current, slots),
            _ => false,
        };
        if fast {
            continue;
        }
        if let Err(err) = crate::phases_late::foreign_ffm::p67_session_acquire(ctx, &mut current) {
            let mut acquired = [None::<ObjectRef>; MEMO_CALL_SEGMENTS];
            for ((slot, session), pin) in acquired.iter_mut().zip(&sessions[..i]).zip(session_pins)
            {
                *slot = session.map(|s| ctx.read_native_pin(pin, s));
            }
            ctx.unpin_native_roots(session_pins[0]);
            for session in acquired.iter().flatten() {
                crate::phases_late::foreign_ffm::p67_session_release(ctx, *session);
            }
            return Some(Err(err));
        }
    }
    let state_pin: Option<usize> = state.map(|state| ctx.pin_native_root(state));
    // JIT round 18 wave 11 (lane ffmret): the return allocator is called
    // after the C call, which an upcall can collect across.
    let alloc_pin: Option<usize> = allocator.map(|a| ctx.pin_native_root(a));
    let pin_base: Option<usize> = if sessions.is_empty() {
        state_pin.or(alloc_pin)
    } else {
        Some(session_pins[0])
    };
    let mut allocator_now = allocator;

    let ctx_guard = plf::ActiveContextGuard::install(ctx);
    // JIT round 16 wave 14 (lane ffm3): in THIS frame, before the state is
    // published, so a pause that claims the thread finds the caller chain's
    // callee-saved registers in the band it scans.
    let spill = if gc_safe {
        Some(plf::CalleeSavedSpill::capture())
    } else {
        None
    };
    if gc_safe {
        downcall_enter_gc_safe(ctx, &ctx_guard);
    }
    // libffi widens a small integer return to `ffi_arg` and needs a writable
    // result even for void; sixteen bytes cover every primitive return.
    let mut ret = [0u64; 2];
    // JIT round 17 wave 2 (lane ffm3; EINTR17-1,
    // `CRATONVM_FFM_DOWNCALL_MASK_TAKEOVER_SIGNAL`): a counted call holds the
    // take-over signal pending instead of letting it interrupt C. FFM417-5
    // (j27): decided from the region entered, not from `gc_safe`.
    let masked = plf::downcall_takeover_mask_now();
    let mask = plf::TakeoverSignalMask::block(masked);
    if let Some(regs) = &direct_regs {
        // JIT round 16 wave 16 (lane ffmlean; FFS16-4,
        // `CRATONVM_FFM_DOWNCALL_DIRECT_STUB`): the fixed-arity stub.
        // SAFETY: as for `ffi_call` below; `memo_direct_shape` held, so the
        // function is non-variadic with at most six parameters, each and the
        // return (or void) integer or (round 19, SysV) float class, the CIF's
        // own shape.
        ret[0] = unsafe { memo_direct_stub_call(memo, fn_addr as usize, regs) }.unwrap_or(0);
    } else {
        // SAFETY: `fn_addr` passed `validated_fn_ptr`; the CIF was built from
        // this handle's descriptor, whose parameter kinds are the memo's, so each
        // cell holds its argument at offset 0 in the CIF's type (a pointer for
        // `ADDRESS`); every segment whose address is passed has its session
        // acquired for the call; `cells`, `arg_ptrs` and `ret` outlive the call.
        unsafe {
            libffi::raw::ffi_call(
                raw_cif_ptr,
                Some(*CodePtr::from_ptr(fn_addr as *const std::ffi::c_void).as_safe_fun()),
                ret.as_mut_ptr() as *mut std::ffi::c_void,
                arg_ptrs.as_mut_ptr(),
            );
        }
    }
    // gcd d5/f: the call's state, read before anything can overwrite it.
    let captured = state.map(|_| CapturedCallState::now());
    drop(mask);
    if gc_safe {
        #[cfg(target_os = "linux")]
        {
            // SAFETY: `__errno_location` answers this thread's errno slot.
            let saved_errno = unsafe { *libc::__errno_location() };
            ctx_guard.leave_region(ctx);
            // SAFETY: as above.
            unsafe { *libc::__errno_location() = saved_errno };
        }
        #[cfg(not(target_os = "linux"))]
        ctx_guard.leave_region(ctx);
    }
    if let Some(spill) = &spill {
        spill.keep_alive();
    }
    drop(ctx_guard);

    if let Some(base) = pin_base {
        let state = match (state, state_pin) {
            (Some(state), Some(pin)) => Some(ctx.read_native_pin(pin, state)),
            (state, _) => state,
        };
        let mut held = [None::<ObjectRef>; MEMO_CALL_SEGMENTS];
        for ((slot, session), pin) in held.iter_mut().zip(sessions).zip(session_pins) {
            *slot = session.map(|s| ctx.read_native_pin(pin, s));
        }
        if let (Some(a), Some(pin)) = (allocator, alloc_pin) {
            allocator_now = Some(ctx.read_native_pin(pin, a));
        }
        ctx.unpin_native_roots(base);
        // JIT round 17 wave 1 (lane ffmseg): the state segment is written
        // while its session is still held (`CRATONVM_FFM_CAPTURE_STATE_BEFORE_RELEASE`).
        let before_release = true;
        if before_release {
            if let (Some(state), Some(captured)) = (state, captured) {
                write_memo_capture_state(ctx, state, state_known, captured);
            }
        }
        for (session, slots) in held.iter().zip(session_slots) {
            let Some(session) = *session else {
                continue;
            };
            match slots {
                Some(slots) if row_handshake => segment_row_add_acquires(ctx, session, slots, -1),
                _ => crate::phases_late::foreign_ffm::p67_session_release(ctx, session),
            }
        }
        if !before_release {
            if let (Some(state), Some(captured)) = (state, captured) {
                write_memo_capture_state(ctx, state, state_known, captured);
            }
        }
    }

    // JIT round 18 wave 11 (lane ffmret): an `ADDRESS` return mints its
    // segment and a group return calls the allocator, both here, after every
    // release and with nothing raw held; a primitive is read as before.
    if plain_memo_returns_object(memo) {
        return Some(memo_finish_return(ctx, memo, ret, allocator_now, group));
    }
    let result = match plain_memo_return(memo) {
        None => Value::Object(None),
        Some(kind) => {
            let mut bytes = [0u8; 16];
            bytes[..8].copy_from_slice(&ret[0].to_ne_bytes());
            bytes[8..].copy_from_slice(&ret[1].to_ne_bytes());
            plf::unmarshal_return_primitive(kind, &bytes)
        }
    };
    Some(Ok(Some(result)))
}

// ---------------------------------------------------------------------------
// The bound downcall row of compiled `invokeExact` sites (JIT round 16 wave
// 12, lane ffmdoor; FFC16-1, `r16-ffm-plain-downcall-costs-microseconds`)
// ---------------------------------------------------------------------------
//
// A compiled `MethodHandle.invokeExact` on a downcall carrier reaches the
// JIT's native site cache (`vm/src/jit/helpers.rs`,
// `try_jit_site_cached_native_dispatch`), which runs `MethodHandle`'s erased
// `invokeExact` native: the whole door (`mh_real_*`, the direct and
// converting lanes' screens, the field-link screens, the declared type
// rendered, `exact_call_site_refusal`, `null_receiver_refusal`,
// `invoke_exact_inner_cast_refusal`, `mh_dispatch`'s downcall arm, a `Vec`,
// `pe_downcall_invoke`, then `return_cast_refusal` and a primitive result
// BOXED by `box_return_against_target` for the VM to unbox again). The
// constant bind (`mh_compiled_bind_target`) refuses every downcall carrier.
// Wave 11's plain memo cut `pe_downcall_invoke` and measured no gain, so the
// microseconds are the door's.
//
// With `CRATONVM_JIT_FFM_DOWNCALL_BIND` a site whose handle is a plain
// downcall carrier called at its own exact type is bound once
// ([`jit_downcall_bind_target`]), and the call goes through the VM's native
// funnel to [`jit_downcall_bound_native`] -- the plain path -- instead of the
// door. For such a call every step of the door is the identity: the site is
// the handle's type (no `WrongMethodTypeException`), the arguments are raw
// primitives of the parameters' own kinds (no receiver, no cast, no
// conversion), the carrier has no marking bit, cast chain or recorded leaf
// descriptor (not an `asType` copy), and the primitive result the door boxes
// is unboxed by the caller to the same value. What remains is
// `pe_downcall_invoke`, whose plain path answers it.

/// `CRATONVM_JIT_FFM_DOWNCALL_BIND` (default ON since wave 12: llabs 2419 ->
/// 1529, getpid 2261 -> 1599 ns/call, 20/20 loaded runs; `=0` turns it off;
/// JIT round 16 wave 12, lane ffmdoor, FFC16-1): a
/// compiled `MethodHandle.invokeExact` site whose handle is a plain downcall
/// carrier ([`DOWNCALL_PLAIN`]) at its own exact type calls the plain path
/// without the `invokeExact` door. Also makes the general path record the
/// plain memo (as `CRATONVM_FFM_DOWNCALL_PLAIN_MEMO` does) so a handle can be
/// bound. Read once.
pub fn jit_downcall_bind_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `CRATONVM_DBG_FFM` (read once), for the JIT bind's engagement line.
pub fn ffm_dbg_enabled() -> bool {
    ffm_dbg_plain()
}

/// [`jit_downcall_bind_target`]'s answer for one handle at one site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JitDowncallBind {
    /// Bound. The handle's slot-0 `type` object: the bound site re-reads that
    /// slot per call, so a handle whose `type` is restamped re-asks.
    Bound(ObjectRef),
    /// A plain carrier whose memo or CIF is not there yet (its first general
    /// call has not run): the door serves this call; ask again.
    NotYet,
    /// Not a handle this row serves; the door serves the site.
    Refused,
}

/// The layout kind a site descriptor character names for a plain handle
/// (`downcall_layout_carrier_descriptor`'s map, inverted), or `None` for a
/// reference or `V`.
pub(in crate::panama) fn plain_memo_kind_of_site_char(c: u8) -> Option<i32> {
    Some(match c {
        b'Z' => LAYOUT_BOOLEAN,
        b'B' => LAYOUT_BYTE,
        b'S' => LAYOUT_SHORT,
        b'C' => LAYOUT_CHAR,
        b'I' => LAYOUT_INT,
        b'J' => LAYOUT_LONG,
        b'F' => LAYOUT_FLOAT,
        b'D' => LAYOUT_DOUBLE,
        _ => return None,
    })
}

/// The site descriptor of a `MemorySegment` parameter: what `ADDRESS` and a
/// capture-state segment carry (`downcall_layout_carrier_descriptor`).
pub(in crate::panama) const MEMORY_SEGMENT_SITE_DESC: &[u8] = b"Ljava/lang/foreign/MemorySegment;";

/// The site descriptor of a group-returning handle's leading allocator
/// (JIT round 18 wave 11, lane ffmret).
pub(in crate::panama) const SEGMENT_ALLOCATOR_SITE_DESC: &[u8] = b"Ljava/lang/foreign/SegmentAllocator;";

/// Does the call-site descriptor `site` name exactly the parameter and return
/// kinds of the encoding `memo`, position for position (`V` for a void
/// return)? A plain encoding's parameters are primitive characters; a segment
/// encoding's `ADDRESS` parameters and its capture-state segment (first) are
/// `Ljava/lang/foreign/MemorySegment;` (JIT round 16 wave 13, lane ffmseg).
/// Pure.
pub(in crate::panama) fn jit_downcall_site_matches_memo(site: &str, memo: i64) -> bool {
    if !plain_memo_is_encoded(memo) {
        return false;
    }
    let bytes = site.as_bytes();
    let Some(close) = bytes.iter().position(|b| *b == b')') else {
        return false;
    };
    // JIT round 18 wave 11 (lane ffmret): an `ADDRESS` or group return is
    // `MemorySegment`; every other return is one character.
    let ret_desc = &bytes[close + 1..];
    let object_return = plain_memo_returns_object(memo);
    if bytes.first() != Some(&b'(')
        || (object_return && ret_desc != MEMORY_SEGMENT_SITE_DESC)
        || (!object_return && ret_desc.len() != 1)
    {
        return false;
    }
    let mut params = &bytes[1..close];
    if memo & PLAIN_MEMO_RET_GROUP != 0 {
        let Some(rest) = params.strip_prefix(SEGMENT_ALLOCATOR_SITE_DESC) else {
            return false;
        };
        params = rest;
    }
    if memo & PLAIN_MEMO_CAPTURE != 0 {
        let Some(rest) = params.strip_prefix(MEMORY_SEGMENT_SITE_DESC) else {
            return false;
        };
        params = rest;
    }
    for i in 0..plain_memo_arity(memo) {
        let kind = plain_memo_param(memo, i);
        let rest = if kind == LAYOUT_ADDRESS {
            params.strip_prefix(MEMORY_SEGMENT_SITE_DESC)
        } else {
            match params.split_first() {
                Some((c, rest)) if plain_memo_kind_of_site_char(*c) == Some(kind) => Some(rest),
                _ => None,
            }
        };
        let Some(rest) = rest else {
            return false;
        };
        params = rest;
    }
    if !params.is_empty() {
        return false;
    }
    if object_return {
        return true;
    }
    match (bytes[close + 1], plain_memo_return(memo)) {
        (b'V', None) => true,
        (c, Some(kind)) => plain_memo_kind_of_site_char(c) == Some(kind),
        _ => false,
    }
}

/// JIT round 16 wave 12 (lane ffmdoor; FFC16-1): may a compiled
/// `MethodHandle.invokeExact` site of descriptor `site` call `handle` through
/// [`jit_downcall_bound_native`] instead of the door? `Bound` only when the
/// door, for any arguments a verified caller can pass at `site`, does nothing
/// but `pe_downcall_invoke(handle, args..)` and an identity box/unbox:
///
/// * `handle` is a downcall carrier ([`is_downcall_handle`]), not an `asType`
///   copy (no recorded leaf descriptor: such a copy converts its arguments),
///   with no `MethodHandle` marking bit;
/// * its plain memo is a plain encoding ([`DOWNCALL_PLAIN`]): primitive value
///   parameters and return only, not variadic, no capture state, not
///   address-less, no return allocator; and its CIF is cached. With
///   `CRATONVM_JIT_FFM_DOWNCALL_BIND_SEGMENTS` (wave 13, lane ffmseg) a
///   segment encoding too -- `ADDRESS` parameters, a capture state -- while
///   the scope checks and the session acquire are on (the bound call does
///   both, [`pe_downcall_invoke_memo`]);
/// * its declared `type` (slot 0, which must be the `type` field) renders to
///   exactly `site`, and `site`'s kinds are the memo's.
///
/// `CRATONVM_DBG_MH_DISPATCH` refuses (the door's downcall diagnostics stay
/// whole). Pure reads apart from the `type` render; no GC point.
pub fn jit_downcall_bind_target(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
    site: &str,
) -> JitDowncallBind {
    if crate::nbflags().dbg_mh_dispatch
        || ctx.object_num_fields(handle) <= DOWNCALL_PLAIN
        || !is_downcall_handle(&*ctx, handle)
    {
        return JitDowncallBind::Refused;
    }
    let memo = match ctx.get_field(handle, DOWNCALL_PLAIN) {
        Value::Long(0) => return JitDowncallBind::NotYet,
        Value::Long(m) if plain_memo_is_plain(m) => m,
        Value::Long(m)
            if plain_memo_is_segment(m)
                && jit_downcall_bind_segments_enabled()
                && ffm_bulk_scope_check() =>
        {
            m
        }
        _ => return JitDowncallBind::Refused,
    };
    if !jit_downcall_site_matches_memo(site, memo)
        || crate::lang_invoke::mh_downcall_bind_screens_refuse(&*ctx, handle)
    {
        return JitDowncallBind::Refused;
    }
    let fn_addr = downcall_fn_addr(ctx, handle);
    if fn_addr == 0 || fn_addr == DOWNCALL_ADDRESS_FROM_ARG0 {
        return JitDowncallBind::Refused;
    }
    if !matches!(ctx.get_field(handle, DOWNCALL_CIF), Value::Long(n) if n != 0) {
        return JitDowncallBind::NotYet;
    }
    let Value::Object(Some(mt)) = ctx.get_field(handle, 0) else {
        return JitDowncallBind::Refused;
    };
    if !matches!(ctx.get_field_by_name(handle, "type"), Value::Object(Some(t)) if t == mt) {
        return JitDowncallBind::Refused;
    }
    match crate::lang_invoke::mh_downcall_bind_declared_type(ctx, handle) {
        Some(declared) if declared == site => JitDowncallBind::Bound(mt),
        _ => JitDowncallBind::Refused,
    }
}

/// The native a bound site (JIT round 16 wave 12, lane ffmdoor; FFC16-1,
/// [`jit_downcall_bind_target`]) calls through the VM's native funnel with
/// `[handle, primitive args..]`, in place of `MethodHandle.invokeExact`'s door.
///
/// The memo path first ([`pe_downcall_invoke_memo`]: the plain path, and with
/// `CRATONVM_JIT_FFM_DOWNCALL_BIND_SEGMENTS` the segment path); anything it
/// declines -- an access verdict that is no longer settled (an `Audit` or
/// `Enforce` policy or a `SecurityManager` installed since), a target that
/// fails validation, a heap, `null`, closed or foreign-thread segment -- goes
/// to the general path, which is what the door's downcall arm calls for this
/// handle with these arguments and which raises whatever applies. An entry
/// call site some door armed and nothing consumed is the door's
/// (`mh_dispatch` takes it and adapts), so that call takes `mh_dispatch`.
/// A primitive result is returned raw; the caller unboxes the erased result
/// against the site (`unbox_poly_return_checked`), which keeps it as is.
pub fn jit_downcall_bound_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if crate::lang_invoke::mh_entry_call_site_armed() {
        let handle = obj_arg(args, 0)?;
        return crate::lang_invoke::mh_dispatch(ctx, handle, &args[1..]);
    }
    if let Some(result) = pe_downcall_invoke_memo(ctx, args, jit_downcall_bind_segments_enabled()) {
        return result;
    }
    pe_downcall_invoke(ctx, args)
}

// ---------------------------------------------------------------------------
// The interpreter's downcall lane (round 19; `CRATONVM_FFM_INTERP_DOWNCALL_LANE`)
// ---------------------------------------------------------------------------
//
// An interpreted `invokeExact` of a downcall carrier went through the whole
// `MethodHandle.invokeExact` door: the declared `type` rendered to a
// descriptor string and compared with the call site, the door's refusals, the
// `MH_DESC` decode, `mh_dispatch`, and the general downcall path. About 7.5 us
// a call against HotSpot's 1.1 us under `-Xint`; and every activation of a
// loop runs its first ~1,000 iterations interpreted before its OSR body, so
// the probe shape `R16FfmDowncallCost` (one activation per 20,000 calls) paid
// ~350 ns a call for it. The lane asks the JIT bound row's own question
// ([`jit_downcall_bind_target`]: the door does nothing for this handle at
// this site but `pe_downcall_invoke` and an identity box/unbox) once per
// handle, type and site, and then makes the bound row's call
// ([`jit_downcall_bound_native`]).



/// One line of [`INTERP_DOWNCALL_LANE_MEMO`]: a `Bound` verdict for the
/// handle and `type` at these addresses, in this VM and collection epoch, at
/// this call site.
pub(in crate::panama) struct InterpDowncallLaneLine {
    pub(in crate::panama) vm: usize,
    pub(in crate::panama) epoch: u64,
    pub(in crate::panama) handle: usize,
    pub(in crate::panama) mt: usize,
    pub(in crate::panama) site: String,
}

/// Lines in [`INTERP_DOWNCALL_LANE_MEMO`]; a power of two (the index is a mask).
pub(in crate::panama) const INTERP_DOWNCALL_LANE_SLOTS: usize = 8;

thread_local! {
    /// Per-thread memo of the lane's `Bound` verdicts, indexed by the handle's
    /// address. The soundness rule is `lang_invoke`'s `DIRECT_LANE_MEMO`'s:
    /// nothing moves or is freed within one collection epoch, so an equal
    /// address in an equal epoch is the same object. The verdict reads the
    /// handle's class, tag, plain memo (set once, from `0`), marking bits and
    /// recorded leaf (both set when a handle is minted) and its `type` (slot 0,
    /// compared here, and a `MethodType` is immutable); the access verdict and
    /// the target's validity are the memo path's own per-call checks.
    pub(in crate::panama) static INTERP_DOWNCALL_LANE_MEMO: std::cell::RefCell<[Option<InterpDowncallLaneLine>; INTERP_DOWNCALL_LANE_SLOTS]> =
        const { std::cell::RefCell::new([const { None }; INTERP_DOWNCALL_LANE_SLOTS]) };
}

/// The interpreter's `invokeExact` of `args[0]` at call site `site`, when the
/// handle is a downcall carrier the JIT would bind at that site; `None` --
/// nothing done, nothing read that has an effect -- otherwise, and the door
/// runs as before. `args` is the native's own `[handle, arguments..]`.
pub(crate) fn interp_downcall_lane(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    site: Option<&str>,
) -> Option<MethodCallResult> {
    let site = site?;
    let Some(Value::Object(Some(handle))) = args.first().copied() else {
        return None;
    };
    if ctx.object_num_fields(handle) <= DOWNCALL_PLAIN {
        return None;
    }
    let vm = ctx.vm_identity();
    if vm == 0 {
        return None;
    }
    let Value::Object(Some(mt)) = ctx.get_field(handle, 0) else {
        return None;
    };
    let epoch = ctx.gc_address_epoch();
    let key = (handle.as_ptr() as usize, mt.as_ptr() as usize);
    let slot = (key.0 >> 3) & (INTERP_DOWNCALL_LANE_SLOTS - 1);
    let hit = INTERP_DOWNCALL_LANE_MEMO.with(|memo| {
        memo.try_borrow().ok().is_some_and(|memo| {
            memo[slot].as_ref().is_some_and(|line| {
                line.vm == vm
                    && line.epoch == epoch
                    && (line.handle, line.mt) == key
                    && line.site == site
            })
        })
    });
    if !hit {
        if !matches!(
            jit_downcall_bind_target(ctx, handle, site),
            JitDowncallBind::Bound(_)
        ) {
            return None;
        }
        // The verdict renders `type` (reads, no GC point); should a collection
        // have run anyway, the addresses above no longer name these objects
        // and the door takes the call.
        if ctx.gc_address_epoch() != epoch {
            return None;
        }
        INTERP_DOWNCALL_LANE_MEMO.with(|memo| {
            if let Ok(mut memo) = memo.try_borrow_mut() {
                memo[slot] = Some(InterpDowncallLaneLine {
                    vm,
                    epoch,
                    handle: key.0,
                    mt: key.1,
                    site: site.to_owned(),
                });
            }
        });
    }
    Some(jit_downcall_bound_native(ctx, args))
}

/// JIT round 16 wave 13 (lane ffmseg; FFD16-1, `CRATONVM_JIT_FFM_DOWNCALL_LEAN`):
/// the plain path alone, for the JIT's lean bound call. `None` -- nothing was
/// done -- whenever [`pe_downcall_invoke_plain`] declines, or an entry call
/// site is armed (the door's); the caller then makes the ordinary bound call
/// through the funnel. A plain call holds no reference across C and allocates
/// nothing, which is what lets the caller skip the funnel's pins and
/// collection hooks.
pub fn jit_downcall_plain_only(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Option<MethodCallResult> {
    if crate::lang_invoke::mh_entry_call_site_armed() {
        return None;
    }
    pe_downcall_invoke_plain(ctx, args)
}

// ---------------------------------------------------------------------------
// The direct bound downcall (JIT round 16 wave 16, lane ffmlean; proposals
// FFD16-1 / FFS16-4, `CRATONVM_JIT_FFM_DOWNCALL_DIRECT`)
// ---------------------------------------------------------------------------
//
// A bound plain site (wave 12) still decoded its arguments into `Value`s, ran
// the native funnel, and in `pe_downcall_invoke_memo` re-read five handle
// fields through `NativeContext::get_field` (each a heap validation and a
// descriptor-cache lookup), took the security-state mutex, read the
// native-access lock twice, installed the context guard and ran libffi's
// generic `ffi_call`, then boxed nothing but still went through the
// poly-return checks. Everything a plain handle's call depends on is fixed at
// link time (`pe_downcall_link`: the target, the descriptor, `critical`) or at
// its first call (the memo), and the bind re-validates the handle's identity
// per call (its address, the collection epoch, its `type` slot). So the bind's
// fill now also keeps a [`JitDowncallDirect`]: the validated target, the memo
// and the critical mark. A call then needs only the per-call verdicts that can
// change while the handle does not: the entry call site, the GC-safe
// switches, and the access verdict -- and makes the call through the
// fixed-arity stub with the compiled caller's raw argument slots.

/// `CRATONVM_JIT_FFM_DOWNCALL_DIRECT` (default ON since wave 17, `=0` turns it
/// off; w16a A/B llabs 525 -> 331 ns, 270 with the direct stub; JIT round 16
/// wave 16, lane ffmlean, FFD16-1 / FFS16-4): a compiled `invokeExact` site bound to a PLAIN downcall handle
/// ([`jit_downcall_bind_target`]) whose shape is [`memo_direct_shape`] calls
/// the C function straight from the JIT's site row through a direct stub
/// ([`jit_downcall_direct_invoke`]): no `Value` decode, no native funnel, no
/// handle field reads, no `ffi_call`. Read once.
pub fn jit_downcall_direct_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// One bound plain handle's precompiled call ([`jit_downcall_direct_stub`]).
/// Holds no reference: the JIT's bind re-validates the handle each call, and
/// everything here was read from it while it was the bound handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JitDowncallDirect {
    /// The target, screened by `validated_fn_ptr` at the fill.
    pub(in crate::panama) fn_addr: usize,
    /// The handle's plain encoding ([`DOWNCALL_PLAIN`]).
    pub(in crate::panama) memo: i64,
    /// Linked `Linker.Option.critical` (either mark): no thread-state
    /// transition, no GC-safe region.
    pub(in crate::panama) critical: bool,
    /// JIT round 17 wave 2 (lane ffm2; FFMSEG17-1,
    /// `CRATONVM_JIT_FFM_DOWNCALL_DIRECT_SEGMENTS`): a SEGMENT encoding, whose
    /// call is [`jit_downcall_direct_segment_invoke`] (reference slots for the
    /// `ADDRESS` parameters and the leading capture-state segment).
    pub(in crate::panama) segments: bool,
    /// JIT round 18 wave 11 (lane ffmret; FFMCOST18-3): a by-value group
    /// return's size and alignment, from the cached CIF at the fill (`(0, 1)`
    /// for any other return).
    pub(in crate::panama) group_size: u32,
    pub(in crate::panama) group_align: u32,
}

impl JitDowncallDirect {
    /// JIT round 18 wave 11 (lane ffmret): does the call answer an object it
    /// obtains after the C call (an `ADDRESS` return's minted segment, or a
    /// group return's allocator segment)? Such a call allocates or runs Java
    /// after the C call, so the caller brackets it in the native span and
    /// hands the result over with its handoff root.
    pub fn returns_object(&self) -> bool {
        plain_memo_returns_object(self.memo)
    }

    /// The C parameter count (the compiled site passes one slot each after
    /// the handle, plus the capture-state segment first for a segment stub;
    /// see [`Self::slot_count`]).
    pub fn arity(&self) -> usize {
        plain_memo_arity(self.memo)
    }

    /// The argument slots the compiled site passes after the handle: the C
    /// parameters, plus the leading capture-state segment of a
    /// `captureCallState` handle, and before it (JIT round 18 wave 11, lane
    /// ffmret) a group return's `SegmentAllocator`.
    pub fn slot_count(&self) -> usize {
        self.arity() + plain_memo_lead(self.memo)
    }

    /// A segment stub ([`jit_downcall_direct_segment_invoke`]), not a plain
    /// one ([`jit_downcall_direct_invoke`]).
    pub fn segments(&self) -> bool {
        self.segments
    }

    /// Was the handle linked `critical`? The caller then makes no thread-state
    /// transition (the call is a leaf: no upcall, no blocking, by the option's
    /// contract).
    pub fn critical(&self) -> bool {
        self.critical
    }

    /// The target's address (`CRATONVM_DBG_FFM` lines).
    pub fn fn_addr(&self) -> usize {
        self.fn_addr
    }

    /// JIT round 22 (lane memory, FFM22-1): the shape of the call the
    /// optimizing tier may make itself for this stub, or `None`: a PLAIN stub
    /// (no lead slot, no segment) of at most [`COMPILED_CALL_MAX_ARGS`]
    /// integer-class parameters (`byte`, `boolean`, `short`, `char`, `int`,
    /// `long`) answering one of them or nothing, on SysV x86-64. Each
    /// parameter's code is the widening [`jit_downcall_direct_invoke`] applies
    /// (`crate::panama_libffi::direct_stub_widen`); the return's is the read
    /// its `memo_return_value` makes of `rax`, as the compiled caller's
    /// 64-bit slot holds it (an `int`-class answer sign-extended, a `char`
    /// zero-extended unless `CRATONVM_FFM_CHAR_RETURN_UNSIGNED=0`).
    pub fn compiled_call_shape(&self) -> Option<CompiledCallShape> {
        if !cfg!(all(target_os = "linux", target_arch = "x86_64"))
            || self.segments
            || plain_memo_lead(self.memo) != 0
            || memo_direct_has_fp(self.memo)
            || self.returns_object()
            || !plain_memo_is_plain(self.memo)
        {
            return None;
        }
        let arity = self.arity();
        if arity > COMPILED_CALL_MAX_ARGS {
            return None;
        }
        let mut params = [0u8; COMPILED_CALL_MAX_ARGS];
        for (i, code) in params.iter_mut().enumerate().take(arity) {
            *code = match plain_memo_param(self.memo, i) {
                LAYOUT_BYTE | LAYOUT_BOOLEAN => COMPILED_CALL_S8,
                LAYOUT_CHAR if crate::panama_libffi::char_arg_unsigned() => COMPILED_CALL_U16,
                LAYOUT_SHORT | LAYOUT_CHAR => COMPILED_CALL_S16,
                LAYOUT_INT => COMPILED_CALL_S32,
                LAYOUT_LONG => COMPILED_CALL_W64,
                _ => return None,
            };
        }
        let ret = match plain_memo_return(self.memo) {
            None => COMPILED_CALL_VOID,
            Some(LAYOUT_BYTE | LAYOUT_BOOLEAN) => COMPILED_CALL_S8,
            Some(LAYOUT_CHAR) if crate::panama_libffi::char_return_unsigned() => COMPILED_CALL_U16,
            Some(LAYOUT_SHORT | LAYOUT_CHAR) => COMPILED_CALL_S16,
            Some(LAYOUT_INT) => COMPILED_CALL_S32,
            Some(LAYOUT_LONG) => COMPILED_CALL_W64,
            Some(_) => return None,
        };
        // `arity` is at most six here.
        Some(CompiledCallShape {
            arity: arity as u8,
            params,
            ret,
        })
    }
}

/// JIT round 22 (lane memory, FFM22-1): the per-call access verdict of the
/// direct rows ([`jit_downcall_direct_admits`]'s `downcall_direct_access_settled`)
/// as words compiled code can test: `[(address, value); 2]`, each word to
/// equal its value, or `None` when the verdict is not settled for `vm` now.
/// The policy half is the capability index's generation as it was read before
/// the policy was found settled (no policy, or this VM's `Permissive` one: a
/// set's mode never changes, any install or uninstall moves the generation);
/// the `SecurityManager` half is the process mirror, zero. The native-access
/// half is not a word: it only ever widens (`Module.enableNativeAccess`), so
/// the row's admission settled it.
pub fn compiled_call_access_guard(vm_identity: usize) -> Option<[(u64, u64); 2]> {
    use cratonvm_native_api::capability as cap;
    let generation = cap::capabilities_generation();
    let settled = !cap::any_capabilities_installed()
        || (cap::capabilities_for(cratonvm_native_api::VmId::from_raw(vm_identity))
                .is_none_or(|set| set.mode() == cap::CapabilityMode::Permissive));
    if !settled || crate::security_manager::any_security_manager_installed() {
        return None;
    }
    Some([
        (cap::capabilities_generation_word() as u64, generation),
        (
            crate::security_manager::security_managers_installed_word() as u64,
            0,
        ),
    ])
}

/// [`JitDowncallDirect::compiled_call_shape`]: the C parameters the SysV
/// integer registers carry.
pub const COMPILED_CALL_MAX_ARGS: usize = 6;
/// [`CompiledCallShape`] code: no return value.
pub const COMPILED_CALL_VOID: u8 = 0;
/// [`CompiledCallShape`] code: the low 8 bits, sign-extended.
pub const COMPILED_CALL_S8: u8 = 1;
/// [`CompiledCallShape`] code: the low 16 bits, sign-extended.
pub const COMPILED_CALL_S16: u8 = 2;
/// [`CompiledCallShape`] code: the low 32 bits, sign-extended.
pub const COMPILED_CALL_S32: u8 = 3;
/// [`CompiledCallShape`] code: all 64 bits.
pub const COMPILED_CALL_W64: u8 = 4;
/// [`CompiledCallShape`] code: the low 16 bits, zero-extended.
pub const COMPILED_CALL_U16: u8 = 5;

/// The call the optimizing tier makes for a plain direct stub
/// ([`JitDowncallDirect::compiled_call_shape`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompiledCallShape {
    /// The C parameter count.
    pub arity: u8,
    /// Each parameter's widening code, in order (unused entries `0`).
    pub params: [u8; COMPILED_CALL_MAX_ARGS],
    /// The return's code.
    pub ret: u8,
}

/// [`SegmentCallShape`] return code (JIT round 22, FFM22-3): an `ADDRESS`
/// return, whose segment [`jit_downcall_direct_finish`] mints.
pub const COMPILED_CALL_REF: u8 = 6;

/// JIT round 22 (lane memory, FFM22-3): the call the optimizing tier checks
/// the segment of itself ([`JitDowncallDirect::segment_call_shape`]): one
/// `ADDRESS` parameter, every other one integer-class, no lead slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentCallShape {
    /// The C parameter count.
    pub arity: u8,
    /// Each parameter's widening code, in order; the segment's is
    /// [`COMPILED_CALL_W64`] (its address).
    pub params: [u8; COMPILED_CALL_MAX_ARGS],
    /// The index of the `ADDRESS` parameter.
    pub segment: u8,
    /// The return's code ([`COMPILED_CALL_REF`] for an `ADDRESS` return).
    pub ret: u8,
}

impl JitDowncallDirect {
    /// JIT round 22 (lane memory, FFM22-3): the shape of a SEGMENT stub whose
    /// one segment argument the compiled caller reads and checks itself, or
    /// `None`: on SysV x86-64, no capture-state segment and no group return
    /// (no lead slot), no float-class parameter, at most
    /// [`COMPILED_CALL_MAX_ARGS`] parameters of which exactly one is `ADDRESS`
    /// and the others integer-class, answering an integer-class value,
    /// nothing, or an `ADDRESS`.
    pub fn segment_call_shape(&self) -> Option<SegmentCallShape> {
        if !cfg!(all(target_os = "linux", target_arch = "x86_64"))
            || !self.segments
            || plain_memo_lead(self.memo) != 0
            || self.memo & PLAIN_MEMO_RET_GROUP != 0
            || memo_direct_has_fp(self.memo)
        {
            return None;
        }
        let arity = self.arity();
        if arity > COMPILED_CALL_MAX_ARGS {
            return None;
        }
        let mut params = [0u8; COMPILED_CALL_MAX_ARGS];
        let mut segment = None;
        for (i, code) in params.iter_mut().enumerate().take(arity) {
            *code = match plain_memo_param(self.memo, i) {
                LAYOUT_ADDRESS if segment.is_none() => {
                    segment = Some(i);
                    COMPILED_CALL_W64
                }
                LAYOUT_BYTE | LAYOUT_BOOLEAN => COMPILED_CALL_S8,
                LAYOUT_CHAR if crate::panama_libffi::char_arg_unsigned() => COMPILED_CALL_U16,
                LAYOUT_SHORT | LAYOUT_CHAR => COMPILED_CALL_S16,
                LAYOUT_INT => COMPILED_CALL_S32,
                LAYOUT_LONG => COMPILED_CALL_W64,
                _ => return None,
            };
        }
        let ret = if self.memo & PLAIN_MEMO_RET_ADDRESS != 0 {
            COMPILED_CALL_REF
        } else {
            match plain_memo_return(self.memo) {
                None => COMPILED_CALL_VOID,
                Some(LAYOUT_BYTE | LAYOUT_BOOLEAN) => COMPILED_CALL_S8,
                Some(LAYOUT_CHAR) if crate::panama_libffi::char_return_unsigned() => {
                    COMPILED_CALL_U16
                }
                Some(LAYOUT_SHORT | LAYOUT_CHAR) => COMPILED_CALL_S16,
                Some(LAYOUT_INT) => COMPILED_CALL_S32,
                Some(LAYOUT_LONG) => COMPILED_CALL_W64,
                Some(_) => return None,
            }
        };
        // Cast: both at most six here.
        Some(SegmentCallShape {
            arity: arity as u8,
            params,
            segment: segment? as u8,
            ret,
        })
    }
}

/// JIT round 22 (lane memory, FFM22-3): the address a downcall passes C for a
/// native segment argument whose `min` and `length` the compiled caller read,
/// exactly as the direct segment row derives it ([`segment_row_read`]): an
/// arena handle's real storage, recorded for the call in the enclosing
/// [`JitArenaCallScope`] (found through the arena's JIT-readable table
/// first); then the row's screens. `None` -- nothing to undo --
/// when a screen refuses (the row then declines and the general path raises).
pub fn jit_segment_call_address(min: i64, length: i64) -> Option<u64> {
    use crate::panama_libffi as plf;
    let addr = if plf::is_arena_handle(min) {
        plf::arena_call_address_by(min, length, true).unwrap_or(min)
    } else {
        min
    };
    let checked = if length == 0 && plf::opaque_low_pointer_args_allowed() {
        plf::checked_foreign_handle_tag(addr, "downcall pointer argument")
    } else {
        plf::checked_foreign_addr(addr, "downcall pointer argument")
    };
    // Cast: a screened address's bits, as the row passes them.
    checked.ok().map(|a| a as u64)
}

/// JIT round 22 (lane memory, FFM22-3): the direct segment row's arena-call
/// record ([`crate::panama_libffi::ArenaCallScope`]) for a caller outside
/// this crate: open from before [`jit_segment_call_address`] until the result
/// is finished ([`jit_downcall_direct_finish`] maps a returned pointer into
/// an argument back to its handle through it).
pub struct JitArenaCallScope {
    pub(in crate::panama) _scope: crate::panama_libffi::ArenaCallScope,
}

impl JitArenaCallScope {
    /// Open the record (what the C call's arguments add is dropped with it).
    pub fn open() -> Self {
        Self {
            _scope: crate::panama_libffi::ArenaCallScope::open(),
        }
    }
}

/// JIT round 22 (lane memory, FFM22-3): a segment stub's Java result from the
/// C function's `rax`, as [`jit_downcall_direct_segment_invoke`] finishes it
/// after every release: the minted segment of an `ADDRESS` return (an
/// allocation: a GC point), else the primitive or `null` for void.
pub fn jit_downcall_direct_finish(
    ctx: &mut dyn NativeContext,
    stub: &JitDowncallDirect,
    rax: u64,
) -> MethodCallResult {
    if plain_memo_returns_object(stub.memo) {
        let group = (
            stub.group_size as usize,
            stub.group_align as usize,
            true,
            None,
        );
        return memo_finish_return(ctx, stub.memo, [rax, 0], None, group);
    }
    Ok(Some(memo_return_value(stub.memo, [rax, 0])))
}

/// The fill half: `handle`'s [`JitDowncallDirect`], or `None` (the bound call
/// keeps its funnel) unless [`jit_downcall_direct_enabled`]. Ask only for a
/// handle [`jit_downcall_bind_target`] has just answered `Bound`. Field reads
/// only.
pub fn jit_downcall_direct_stub(
    ctx: &mut dyn NativeContext,
    handle: ObjectRef,
) -> Option<JitDowncallDirect> {
    if !jit_downcall_direct_enabled() {
        return None;
    }
    downcall_direct_stub_of(&*ctx, handle).or_else(|| {
        jit_downcall_direct_segments_enabled()
            .then(|| downcall_direct_segment_stub_of(&*ctx, handle))
            .flatten()
    })
}

/// [`jit_downcall_direct_stub`] without the switch: a plain (not segment)
/// encoding of [`memo_direct_shape`], a cached CIF (the general path has
/// classified the handle) and a target `validated_fn_ptr` accepts.
pub(in crate::panama) fn downcall_direct_stub_of(
    ctx: &dyn NativeContext,
    handle: ObjectRef,
) -> Option<JitDowncallDirect> {
    if ctx.object_num_fields(handle) <= DOWNCALL_PLAIN {
        return None;
    }
    let Value::Long(memo) = ctx.get_field(handle, DOWNCALL_PLAIN) else {
        return None;
    };
    if !plain_memo_is_plain(memo) || !memo_direct_shape(memo) {
        return None;
    }
    if !matches!(ctx.get_field(handle, DOWNCALL_CIF), Value::Long(n) if n != 0) {
        return None;
    }
    let fn_addr = downcall_fn_addr(ctx, handle);
    if fn_addr == 0
        || fn_addr == DOWNCALL_ADDRESS_FROM_ARG0
        || validated_fn_ptr::<unsafe extern "C" fn()>(fn_addr).is_err()
    {
        return None;
    }
    Some(JitDowncallDirect {
        fn_addr: fn_addr as usize,
        memo: memo & !PLAIN_MEMO_HIT_SEEN,
        critical: downcall_is_critical(ctx, handle),
        segments: false,
        group_size: 0,
        group_align: 1,
    })
}

/// The per-call half: may this call of `stub` with `arity` argument slots go
/// direct? `false` -- nothing done -- hands it to the bound call, which then
/// answers or raises as before. Exactly the memo path's per-call decisions
/// that can change while the handle stays the same:
///
/// * no entry call site armed (the door's, [`jit_downcall_bound_native`]);
/// * a non-critical handle under `CRATONVM_FFM_DOWNCALL_GC_SAFE` or
///   `CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY` declines: its GC-safe region is
///   the memo path's, kept as it is (a critical handle never enters one).
///   Since JIT round 17 wave 5 (lane ffm4) the lazy switch is default ON, so
///   this declines every non-critical handle by default, unless
///   [`jit_downcall_direct_lazy`] lets the direct row make the lazy
///   transition itself ([`direct_row_goes_lazy`]);
/// * the access verdict ([`downcall_access_settled`]'s answer, with the
///   `SecurityManager` slot read through the exact mirror first).
pub fn jit_downcall_direct_admits(
    ctx: &dyn NativeContext,
    stub: &JitDowncallDirect,
    arity: usize,
) -> bool {
    arity == stub.slot_count()
        && !crate::lang_invoke::mh_entry_call_site_armed()
        && (stub.critical || direct_row_admits_non_critical())
        && downcall_direct_access_settled(ctx)
}

/// `CRATONVM_JIT_FFM_DOWNCALL_DIRECT_LAZY` (default ON since the round-17
/// close, `=0` off; JIT round 17 wave 5, lane ffm4, FFM417-1). The
/// orchestrator's cost A/B (`R16FfmDowncallCost`, 5 interleaved reps) put the
/// lazy flip alone at 1.8x the eager state's cost on `llabs` (995 vs 553 ns)
/// and 1.6x on `pid` (1161 vs 728); with this switch both came back (563,
/// 743), and its liveness run (three GCs, both modes, `R16Ffm3Stw` 20/20 per
/// GC, loaded `par4` under `CRATONVM_XT_ROOT_SCAN_AUDIT=1`) was clean. With
/// the lazy in-native state on (its default since this wave) and
/// `CRATONVM_FFM_DOWNCALL_GC_SAFE` off, a non-critical handle keeps its direct
/// stub (plain and segment rows): the row captures the callee-saved spill in
/// its own frame and makes the memo path's lazy transition around the C call
/// (`ActiveContextGuard::enter_region_lazy` with no fallback: a declining
/// context keeps the thread counted, as the direct row always did). Without
/// it the flip of the lazy state sends every non-critical bound site back to
/// the bound call and the memo path. Read once.
pub fn jit_downcall_direct_lazy() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// Does a non-critical direct row go into C lazily
/// ([`jit_downcall_direct_lazy`])? The deposit-based region of
/// `CRATONVM_FFM_DOWNCALL_GC_SAFE` stays the memo path's own.
pub(in crate::panama) fn direct_row_goes_lazy() -> bool {
    let verdict = direct_row_lazy_verdict();
    if verdict & DIRECT_LAZY_VERDICT_LEAN != 0 {
        return verdict & DIRECT_LAZY_VERDICT_GOES_LAZY != 0;
    }
    direct_row_goes_lazy_by_switches()
}

/// [`direct_row_goes_lazy`] from its three switches (three `OnceLock` reads).
pub(in crate::panama) fn direct_row_goes_lazy_by_switches() -> bool {
    jit_downcall_direct_lazy() && ffm_downcall_in_native_lazy() && !ffm_downcall_gc_safe()
}

/// May a NON-critical stub go direct ([`jit_downcall_direct_admits`]'s switch
/// half)? Only where no GC-safe state is on, or where the direct row makes the
/// lazy transition itself.
pub(in crate::panama) fn direct_row_admits_non_critical() -> bool {
    let verdict = direct_row_lazy_verdict();
    if verdict & DIRECT_LAZY_VERDICT_LEAN != 0 {
        return verdict & DIRECT_LAZY_VERDICT_ADMITS != 0;
    }
    !(ffm_downcall_gc_safe() || ffm_downcall_in_native_lazy()) || direct_row_goes_lazy_by_switches()
}

/// `CRATONVM_FFM_LAZY_DIRECT_LEAN` (default ON, `=0` off; JIT round 20 wave
/// 14, lane ffm3c, `r13w5-ffm3-a-blocked-downcall-stalls-every-stop-the-world`):
/// the direct rows' own bookkeeping around the lazy in-native transition, cut
/// to what the transition needs. Two things, both answer-neutral:
///
/// * the switch half of [`jit_downcall_direct_admits`] and the row's "go
///   lazy?" test read ONE latched verdict ([`direct_row_lazy_verdict`])
///   instead of up to three `OnceLock`s each (eight switch reads per lazy
///   call become three, the leave's own included);
/// * the leave from the lazy state no longer saves and restores `errno`
///   around itself ([`direct_row_leave_lazy`]): no direct row lets Java see
///   `errno` after the leave. A plain stub has no `captureCallState` (the
///   memo's plain classification excludes it), and the segment row reads the
///   call state (`CapturedCallState::now`) straight after the C function,
///   before the leave. HotSpot does not keep `errno` across its own
///   native-to-Java transition either (its safepoint wait may change it).
///
/// The 270-run cost gate (`tools/probes/jit/ffm-stw/run-cost-gate.py`) put the
/// lazy transition at +27 ns over a 52 ns bound plain downcall (jdk-only ZGC,
/// paired ratio 1.54 against a 1.5 bound); the transition's floor is its two
/// locked compare-exchanges (`GcBlockState::enter_lazy_native` /
/// `leave_lazy_native`), which this switch does not touch. Read once.
pub(in crate::panama) fn ffm_lazy_direct_lean() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// [`direct_row_lazy_verdict`] bit: [`ffm_lazy_direct_lean`] is on (the other
/// bits are valid); clear, the callers read the switches themselves.
pub(in crate::panama) const DIRECT_LAZY_VERDICT_LEAN: u8 = 1;
/// [`direct_row_lazy_verdict`] bit: [`direct_row_admits_non_critical`].
pub(in crate::panama) const DIRECT_LAZY_VERDICT_ADMITS: u8 = 2;
/// [`direct_row_lazy_verdict`] bit: [`direct_row_goes_lazy`].
pub(in crate::panama) const DIRECT_LAZY_VERDICT_GOES_LAZY: u8 = 4;

/// The direct rows' switch verdicts, latched once per process from the same
/// latched switches the unlatched readers use (so the two always agree):
/// `0` when [`ffm_lazy_direct_lean`] is off.
pub(in crate::panama) fn direct_row_lazy_verdict() -> u8 {
    static VERDICT: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *VERDICT.get_or_init(|| {
        if !ffm_lazy_direct_lean() {
            return 0;
        }
        let lazy = direct_row_goes_lazy_by_switches();
        let admits = !(ffm_downcall_gc_safe() || ffm_downcall_in_native_lazy()) || lazy;
        let mut verdict = DIRECT_LAZY_VERDICT_LEAN;
        if admits {
            verdict |= DIRECT_LAZY_VERDICT_ADMITS;
        }
        if lazy {
            verdict |= DIRECT_LAZY_VERDICT_GOES_LAZY;
        }
        verdict
    })
}

/// Leave the lazy state a direct row entered, keeping the C call's `errno`
/// (the leave's wait may make system calls), as the memo path does -- unless
/// [`ffm_lazy_direct_lean`]: no direct row reads `errno` after the leave (see
/// there), so the save and restore are skipped.
#[inline(always)]
pub(in crate::panama) fn direct_row_leave_lazy(
    ctx: &mut dyn NativeContext,
    guard: &crate::panama_libffi::ActiveContextGuard,
) {
    #[cfg(target_os = "linux")]
    {
        if direct_row_lazy_verdict() & DIRECT_LAZY_VERDICT_LEAN != 0 {
            guard.leave_region(ctx);
            return;
        }
        // SAFETY: `__errno_location` answers this thread's errno slot.
        let saved_errno = unsafe { *libc::__errno_location() };
        guard.leave_region(ctx);
        // SAFETY: as above.
        unsafe { *libc::__errno_location() = saved_errno };
    }
    #[cfg(not(target_os = "linux"))]
    guard.leave_region(ctx);
}

/// [`downcall_access_settled`], with the "no `SecurityManager`" half answered
/// by `security_manager::any_security_manager_installed` (an exact mirror of
/// every VM's slot, maintained whatever the switches) before the mutex.
pub(in crate::panama) fn downcall_direct_access_settled(ctx: &dyn NativeContext) -> bool {
    downcall_policy_settled(ctx)
        && (!crate::security_manager::any_security_manager_installed()
            || crate::security_manager::get_security_manager(ctx).is_none())
        && native_access_enabled(ctx)
}

/// The direct call ([`jit_downcall_direct_admits`] said yes): each raw slot
/// widened as libffi widens its kind, the context guard installed (an upcall
/// from a non-critical function needs it), the stub called, and the return
/// read as the memo path reads it (`null` for void). No thread-state change
/// here: the caller brackets a non-critical call in its `NativeRunning` span
/// (and, under `CRATONVM_JIT_FFM_DOWNCALL_DIRECT_LAZY`, this row makes the lazy
/// in-native transition around the C call itself).
///
/// # Safety
///
/// `raw` holds exactly `stub.arity()` slots, one per parameter of the site the
/// bind matched against the memo (so each holds its kind's value in its low
/// bits), and `stub` came from [`jit_downcall_direct_stub`] for the handle
/// that is bound this call.
pub unsafe fn jit_downcall_direct_invoke(
    ctx: &mut dyn NativeContext,
    stub: &JitDowncallDirect,
    raw: &[i64],
) -> Value {
    // JIT round 18 wave 10 (lane ffmcost): `CRATONVM_DBG_FFM_DIRECT_SPLIT`.
    let split = ffm_dbg_direct_split();
    let mut split_t = [0u64; 5];
    if split {
        split_t[0] = direct_split_now();
    }
    let mut cells = [0u64; crate::panama_libffi::DIRECT_STUB_MAX_ARGS];
    for (cell, slot) in cells.iter_mut().zip(raw) {
        *cell = *slot as u64;
    }
    let regs = memo_direct_regs(stub.memo, &cells[..raw.len().min(cells.len())]);
    if split {
        split_t[1] = direct_split_now();
        split_t[2] = split_t[1];
    }
    let guard = crate::panama_libffi::ActiveContextGuard::install(ctx);
    // JIT round 17 wave 5 (lane ffm4; FFM417-1,
    // `CRATONVM_JIT_FFM_DOWNCALL_DIRECT_LAZY`): the memo path's lazy
    // transition, the spill captured in THIS frame first.
    let lazy = !stub.critical && direct_row_goes_lazy();
    let spill = if lazy {
        Some(crate::panama_libffi::CalleeSavedSpill::capture())
    } else {
        None
    };
    if lazy {
        guard.enter_region_lazy(ctx, None);
    }
    // JIT round 17 wave 2 (lane ffm3; EINTR17-1): a direct stub is in no
    // deposit-based region, so the switch decides (a lazy one, as the memo
    // path's GC-safe call, is not masked). FFM417-5 (j27): a lazy entry the
    // context declined is counted, so it is masked too.
    let mask = crate::panama_libffi::TakeoverSignalMask::block(
        crate::panama_libffi::downcall_takeover_mask_now(),
    );
    // SAFETY: `fn_addr` passed `validated_fn_ptr` and `memo_direct_shape`
    // held at the fill: a non-variadic function of at most six integer-class
    // parameters whose CIF this handle's first call built; the caller's
    // contract gives the arity.
    let rax = unsafe { memo_direct_stub_call(stub.memo, stub.fn_addr, &regs) };
    drop(mask);
    if lazy {
        direct_row_leave_lazy(ctx, &guard);
    }
    if let Some(spill) = &spill {
        spill.keep_alive();
    }
    drop(guard);
    if split {
        split_t[3] = direct_split_now();
    }
    let value = memo_return_value(stub.memo, [rax.unwrap_or(0), 0]);
    if split {
        split_t[4] = direct_split_now();
        direct_split_note(false, true, split_t, (0, 0));
    }
    value
}

/// JIT round 22 (lane memory, FFM22-1 step (b)): [`jit_downcall_direct_invoke`]
/// for a stub with a [`JitDowncallDirect::compiled_call_shape`], from register
/// images the caller already widened by that shape's codes, answering `rax`
/// as the C function left it (the caller reads it by the shape's return
/// code). The same context guard, callee-saved spill, lazy in-native
/// transition and take-over mask, in the same order, around the same call;
/// what it skips is the per-call memo decode of the widenings and the return.
///
/// # Safety
///
/// `regs` holds the first `stub.arity()` C arguments widened as
/// `compiled_call_shape` says, and `stub` has that shape.
pub unsafe fn jit_downcall_direct_invoke_regs(
    ctx: &mut dyn NativeContext,
    stub: &JitDowncallDirect,
    regs: &[u64; crate::panama_libffi::DIRECT_STUB_MAX_ARGS],
) -> u64 {
    let guard = crate::panama_libffi::ActiveContextGuard::install(ctx);
    let lazy = !stub.critical && direct_row_goes_lazy();
    let spill = if lazy {
        Some(crate::panama_libffi::CalleeSavedSpill::capture())
    } else {
        None
    };
    if lazy {
        guard.enter_region_lazy(ctx, None);
    }
    let mask = crate::panama_libffi::TakeoverSignalMask::block(
        crate::panama_libffi::downcall_takeover_mask_now(),
    );
    // SAFETY: as `jit_downcall_direct_invoke`'s call: a validated
    // non-variadic target of at most six integer-class parameters (the
    // shape refuses a float-class one), the caller's contract for `regs`.
    let rax = unsafe { crate::panama_libffi::direct_stub_call(stub.fn_addr, stub.arity(), regs) };
    drop(mask);
    if lazy {
        direct_row_leave_lazy(ctx, &guard);
    }
    if let Some(spill) = &spill {
        spill.keep_alive();
    }
    drop(guard);
    rax.unwrap_or(0)
}

// ---------------------------------------------------------------------------
// The direct SEGMENT row (JIT round 17 wave 2, lane ffm2; proposal FFMSEG17-1,
// `CRATONVM_JIT_FFM_DOWNCALL_DIRECT_SEGMENTS`)
// ---------------------------------------------------------------------------
//
// A bound segment site (`strlen(MemorySegment)`, `llabs` with
// `captureCallState`) had no direct stub: it paid the bound row's funnel (the
// `Value` decode, forwarding and pins of every argument, the stop-the-world
// probe, the allocation hooks: the ~270 ns floor the plain direct row
// removed) and then `pe_downcall_invoke_memo`. The direct segment row takes
// the compiled caller's slots as the plain direct row does; each segment slot
// is a reference, validated by the caller's `resolve`, and read through the
// segment row ([`segment_row_read`]: the carrier, or with
// `CRATONVM_FFM_SEGMENT_ROW_REAL_SEGMENTS` a real JDK native segment). Only a
// segment whose scope the row resolves itself is served: no session, or a
// modelled session it verified (owner, then state). Anything else declines
// BEFORE any effect, and the bound call (the funnel and the memo path, then
// the general path) serves and raises as before.

/// `CRATONVM_JIT_FFM_DOWNCALL_DIRECT_SEGMENTS` (default ON since the round-17
/// close, `=0` off; A/B in jit-optin-switch-triage-r17-CLOSED-20261006.md; JIT round 17 wave 2, lane ffm2,
/// FFMSEG17-1): with `CRATONVM_JIT_FFM_DOWNCALL_DIRECT` (default on), a bound
/// site whose handle is a SEGMENT encoding of [`memo_direct_shape`] also gets a
/// direct stub ([`jit_downcall_direct_segment_invoke`]). Read once.
pub fn jit_downcall_direct_segments_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// [`downcall_direct_stub_of`] for a SEGMENT encoding: the memo path's own
/// preconditions for serving one (the bind's segment switch, the scope checks
/// and the session acquire on), [`memo_direct_shape`] (at most six integer
/// class parameters, `ADDRESS` among them; the capture-state segment is not a
/// C parameter), a cached CIF and a validated target.
pub(in crate::panama) fn downcall_direct_segment_stub_of(
    ctx: &dyn NativeContext,
    handle: ObjectRef,
) -> Option<JitDowncallDirect> {
    if ctx.object_num_fields(handle) <= DOWNCALL_PLAIN {
        return None;
    }
    let Value::Long(memo) = ctx.get_field(handle, DOWNCALL_PLAIN) else {
        return None;
    };
    if !plain_memo_is_segment(memo)
        || !memo_direct_shape(memo)
        || !jit_downcall_bind_segments_enabled()
        || !ffm_bulk_scope_check()
    {
        return None;
    }
    let cif = match ctx.get_field(handle, DOWNCALL_CIF) {
        Value::Long(n) if n != 0 => n as u64,
        _ => return None,
    };
    let fn_addr = downcall_fn_addr(ctx, handle);
    if fn_addr == 0
        || fn_addr == DOWNCALL_ADDRESS_FROM_ARG0
        || validated_fn_ptr::<unsafe extern "C" fn()>(fn_addr).is_err()
    {
        return None;
    }
    // JIT round 18 wave 11 (lane ffmret; FFMCOST18-3): a register-returned
    // group (`memo_direct_shape` held, so `PLAIN_MEMO_GROUP_IN_REGS`), sized
    // from the cached CIF as the memo path sizes it.
    let (group_size, group_align) = if memo & PLAIN_MEMO_RET_GROUP != 0 {
        // SAFETY: the handle's own cached CIF.
        let (size, align, _) = unsafe { cif_return_size_align(cif) }?;
        if size == 0 || size > 16 {
            return None;
        }
        (size as u32, align.max(1) as u32)
    } else {
        (0, 1)
    };
    Some(JitDowncallDirect {
        fn_addr: fn_addr as usize,
        memo: memo & !PLAIN_MEMO_HIT_SEEN,
        critical: downcall_is_critical(ctx, handle),
        segments: true,
        group_size,
        group_align,
    })
}

/// Release one session the direct segment row acquired: the row's slot-map
/// release under the acquire handshake, `foreign_ffm`'s otherwise (the pair
/// [`jit_downcall_direct_segment_invoke`]'s acquire took).
pub(in crate::panama) fn direct_segment_release(
    ctx: &mut dyn NativeContext,
    session: ObjectRef,
    slots: crate::phases_late::foreign_ffm::P67SessionSlots,
    handshake: bool,
) {
    if handshake {
        segment_row_add_acquires(ctx, session, slots, -1);
    } else {
        crate::phases_late::foreign_ffm::p67_session_release(ctx, session);
    }
}

/// The direct segment call ([`jit_downcall_direct_admits`] said yes for a
/// [`JitDowncallDirect::segments`] stub). `None` -- nothing done -- when it
/// declines; `Some(Err(..))` only for an acquire that a close racing the
/// check refused (`Already closed`, the memo path's answer, with what was
/// acquired released); else the call's value (`null` for void).
///
/// Per call, before any effect: each segment slot (the capture-state segment
/// first, then the `ADDRESS` parameters) is resolved to a reference by
/// `resolve` (`None` -- `null` or not an object -- declines: the general path
/// raises the NPE), read through [`segment_row_read`] (a heap segment, an
/// unknown shape, an address the screens refuse, or a scope the row does not
/// resolve itself declines), and each distinct session's confinement and state
/// are checked (a failure declines; the general path raises
/// `WrongThreadException` / `Already closed`). No GC point lies in that part
/// (the owner check never mints a mirror), so the raw references stay valid.
///
/// Then, as `pe_downcall_invoke_memo_with`: every session is pinned and
/// acquired for the call (an upcall from a non-critical function can
/// collect), the context guard is installed, the C function is called through
/// the direct stub, the call state is read straight after it and written
/// (at the address the row read) before the sessions are released
/// (`CRATONVM_FFM_CAPTURE_STATE_BEFORE_RELEASE`), and the return read as the
/// plain direct row reads it. No thread-state change here: the caller
/// brackets a non-critical call in its `NativeRunning` span (the lazy
/// in-native transition under `CRATONVM_JIT_FFM_DOWNCALL_DIRECT_LAZY`, as the
/// plain direct row).
///
/// # Safety
///
/// `raw` holds the compiled caller's argument slots after the handle, one per
/// parameter of the site the bind matched against the memo (the state segment
/// first for a capture handle), and `stub` came from
/// [`jit_downcall_direct_stub`] for the handle that is bound this call.
pub unsafe fn jit_downcall_direct_segment_invoke(
    ctx: &mut dyn NativeContext,
    stub: &JitDowncallDirect,
    raw: &[i64],
    resolve: &mut dyn FnMut(i64) -> Option<ObjectRef>,
) -> Option<MethodCallResult> {
    use crate::panama_libffi as plf;
    use crate::phases_late::foreign_ffm as ff;
    use crate::phases_late::foreign_ffm::P67SessionSlots;
    let memo = stub.memo;
    if !stub.segments || raw.len() != stub.slot_count() {
        return None;
    }
    let _arena_call = plf::ArenaCallScope::open();
    // JIT round 18 wave 10 (lane ffmcost): `CRATONVM_DBG_FFM_DIRECT_SPLIT`'s
    // sample (a decline before the served return is counted by the guard's
    // drop) and `CRATONVM_JIT_FFM_DIRECT_SEGMENT_LEAN`.
    let split = ffm_dbg_direct_split();
    let mut split_t = [0u64; 5];
    if split {
        split_t[0] = direct_split_now();
    }
    let mut decline_note = DirectSplitDecline { armed: split };
    let lean = ffm_jit_direct_segment_lean();
    // Confined sessions of a critical call the lean handling leaves alone,
    // and which of `sessions` it acquires by the count alone.
    let mut skipped = [None::<ObjectRef>; MEMO_CALL_SEGMENTS];
    let mut lean_skipped = 0usize;
    let mut owned = [false; MEMO_CALL_SEGMENTS];
    let mut lean_owned = 0usize;
    let arity = stub.arity();
    let lead = stub.slot_count() - arity;
    let mut shape = segment_row_shape(ctx.vm_identity());
    let mut cells = [0u64; plf::DIRECT_STUB_MAX_ARGS];
    let mut sessions = [None::<(ObjectRef, P67SessionSlots)>; MEMO_CALL_SEGMENTS];
    let mut session_count = 0usize;
    // Round 19: a REAL session's count as its check read it (`sessions`'
    // indices), so the acquire and the release are one compare-and-swap each.
    let mut observed = [None::<ff::P67RealObserved>; MEMO_CALL_SEGMENTS];
    // The state segment's address and size, as the row read them.
    let mut state: Option<(usize, i64)> = None;
    // JIT round 18 wave 11 (lane ffmret; FFMCOST18-3): a group return's
    // allocator is the first slot (before the capture-state segment). It is
    // any object; `null` declines (the general path raises the NPE).
    let alloc_lead = usize::from(memo & PLAIN_MEMO_RET_GROUP != 0);
    let allocator: Option<ObjectRef> = if alloc_lead == 1 {
        Some(resolve(*raw.first()?)?)
    } else {
        None
    };
    for (j, slot) in raw.iter().enumerate().skip(alloc_lead) {
        // `None`: the capture-state segment; `Some(i)`: C parameter `i`.
        let param = j.checked_sub(lead);
        if let Some(i) = param {
            if plain_memo_param(memo, i) != LAYOUT_ADDRESS {
                // A primitive: the slot as the plain direct row takes it
                // (`memo_direct_regs` widens it by kind).
                *cells.get_mut(i)? = *slot as u64;
                continue;
            }
        }
        let seg = resolve(*slot)?;
        let SegmentRowArg::Carrier { addr, size, scope } =
            segment_row_read(&*ctx, &mut shape, seg)?
        else {
            return None;
        };
        let (session, slots) = match scope {
            SegmentRowScope::None => (None, None),
            SegmentRowScope::Session(session, slots) => (Some(session), Some(slots)),
            SegmentRowScope::Unknown => return None,
        };
        if let (Some(session), Some(slots)) = (session, slots) {
            let seen = sessions[..session_count]
                .iter()
                .flatten()
                .any(|(held, _)| *held == session)
                || skipped[..lean_skipped]
                    .iter()
                    .flatten()
                    .any(|held| *held == session);
            if !seen {
                if slots.real {
                    // Round 19: the check, keeping the count it read. A
                    // `NONCLOSEABLE` session (global, automatic) is neither
                    // acquired nor released, as the JDK's `acquire0` /
                    // `release0` do nothing there: no pin, no count. Under
                    // the lean switch a session confined to this thread is
                    // left alone by a critical call (only this thread could
                    // close it, and a critical call runs no Java).
                    let Some((ff::P67RealVerdict::Open, seen)) =
                        ff::p67_real_session_observe(ctx, session, slots, true)
                    else {
                        return None;
                    };
                    if lean && seen.confined && stub.critical {
                        *skipped.get_mut(lean_skipped)? = Some(session);
                        lean_skipped += 1;
                    } else if seen.count.is_some() {
                        *observed.get_mut(session_count)? = Some(seen);
                        *sessions.get_mut(session_count)? = Some((session, slots));
                        session_count += 1;
                    }
                } else if lean {
                    // The same check; it also says whether the session is
                    // confined to this thread.
                    let confined = segment_row_scope_open_owned(ctx, session, slots)?;
                    if confined && stub.critical {
                        *skipped.get_mut(lean_skipped)? = Some(session);
                        lean_skipped += 1;
                    } else {
                        *owned.get_mut(session_count)? = confined;
                        *sessions.get_mut(session_count)? = Some((session, slots));
                        session_count += 1;
                    }
                } else {
                    if !segment_row_scope_open(ctx, session, slots) {
                        return None;
                    }
                    *sessions.get_mut(session_count)? = Some((session, slots));
                    session_count += 1;
                }
            }
        }
        match param {
            None if capture_segment_declined(addr, size) => return None,
            None => state = Some((addr, size)),
            Some(i) => *cells.get_mut(i)? = addr as u64,
        }
    }
    let sessions = &sessions[..session_count];
    if split {
        split_t[1] = direct_split_now();
    }

    // Every session pinned before the first acquire (a refused acquire
    // allocates its exception; an upcall can collect), then acquired.
    let mut pins = [0usize; MEMO_CALL_SEGMENTS];
    for (pin, (session, _)) in pins.iter_mut().zip(sessions.iter().flatten()) {
        *pin = ctx.pin_native_root(*session);
    }
    let handshake = segment_row_acquire_handshake();
    for (i, entry) in sessions.iter().enumerate() {
        let Some((session, slots)) = *entry else {
            continue;
        };
        let mut current = ctx.read_native_pin(pins[i], session);
        // Round 19: from the count the check read (a refused one -- a shared
        // close that began since -- falls through to the raising acquire).
        if let Some(seen) = observed[i] {
            if ff::p67_real_session_acquire_from(ctx, current, slots, seen) {
                continue;
            }
        }
        // JIT round 18 wave 10 (lane ffmcost;
        // `CRATONVM_JIT_FFM_DIRECT_SEGMENT_LEAN`): a session confined to this
        // thread, checked open above with no GC point and no Java since,
        // takes the count alone (only this thread can close it).
        if lean && handshake && owned[i] {
            segment_row_add_acquires(ctx, current, slots, 1);
            lean_owned += 1;
            continue;
        }
        if handshake && segment_row_acquire(ctx, current, slots) {
            continue;
        }
        if let Err(err) = crate::phases_late::foreign_ffm::p67_session_acquire(ctx, &mut current) {
            let mut acquired = [None::<(ObjectRef, P67SessionSlots)>; MEMO_CALL_SEGMENTS];
            for ((slot, entry), pin) in acquired.iter_mut().zip(&sessions[..i]).zip(pins) {
                *slot = entry.map(|(s, slots)| (ctx.read_native_pin(pin, s), slots));
            }
            ctx.unpin_native_roots(pins[0]);
            for (session, slots) in acquired.iter().flatten() {
                direct_segment_release(ctx, *session, *slots, handshake);
            }
            // Not a served call: the census counts it as a decline.
            return Some(Err(err));
        }
    }
    if lean {
        direct_lean_report(stub, lean_skipped, lean_owned);
    }
    // JIT round 18 wave 11 (lane ffmret): the allocator is called after the
    // C call (an upcall can collect across it), so it is pinned after the
    // sessions and re-read below.
    let alloc_pin: Option<usize> = allocator.map(|a| ctx.pin_native_root(a));
    if split {
        split_t[2] = direct_split_now();
    }
    decline_note.armed = false;

    let regs = memo_direct_regs(memo, &cells[..arity.min(cells.len())]);
    let guard = plf::ActiveContextGuard::install(ctx);
    // JIT round 17 wave 5 (lane ffm4; FFM417-1): as the plain direct row. The
    // sessions are pinned above and re-read below, as for an up-call.
    let lazy = !stub.critical && direct_row_goes_lazy();
    let spill = if lazy {
        Some(plf::CalleeSavedSpill::capture())
    } else {
        None
    };
    if lazy {
        guard.enter_region_lazy(ctx, None);
    }
    // JIT round 17 wave 2 (lane ffm3; EINTR17-1): as the plain direct row.
    let mask = plf::TakeoverSignalMask::block(plf::downcall_takeover_mask_now());
    // SAFETY: `fn_addr` passed `validated_fn_ptr` and `memo_direct_shape`
    // held at the fill: a non-variadic function of at most six integer-class
    // parameters whose CIF this handle's first call built; each `ADDRESS`
    // cell is a screened native address whose session (if any) is acquired
    // for the call. JIT round 18 wave 11 (lane ffmret): a group return is
    // `PLAIN_MEMO_GROUP_IN_REGS` (`memo_direct_shape` held at the fill), so
    // the pair stub's `rax:rdx` hold it (`rax` alone on Win64).
    let ret_regs: Option<[u64; 2]> = if alloc_lead == 1 {
        // SAFETY: as above, with a register-returned all-integer group.
        unsafe { plf::direct_stub_call_pair(stub.fn_addr, arity, &regs) }
    } else {
        // SAFETY: as above.
        unsafe { memo_direct_stub_call(stub.memo, stub.fn_addr, &regs) }.map(|rax| [rax, 0])
    };
    // The call's state, read before anything can overwrite it.
    let captured = state.map(|_| CapturedCallState::now());
    drop(mask);
    if lazy {
        direct_row_leave_lazy(ctx, &guard);
    }
    if let Some(spill) = &spill {
        spill.keep_alive();
    }
    drop(guard);
    if split {
        split_t[3] = direct_split_now();
    }

    let mut allocator_now = allocator;
    if !sessions.is_empty() || captured.is_some() || alloc_pin.is_some() {
        let mut held = [None::<(ObjectRef, P67SessionSlots)>; MEMO_CALL_SEGMENTS];
        for ((slot, entry), pin) in held.iter_mut().zip(sessions).zip(pins) {
            *slot = entry.map(|(s, slots)| (ctx.read_native_pin(pin, s), slots));
        }
        if let (Some(a), Some(pin)) = (allocator, alloc_pin) {
            allocator_now = Some(ctx.read_native_pin(pin, a));
        }
        if !sessions.is_empty() {
            ctx.unpin_native_roots(pins[0]);
        } else if let Some(pin) = alloc_pin {
            ctx.unpin_native_roots(pin);
        }
        let before_release = true;
        if before_release {
            if let (Some((addr, size)), Some(captured)) = (state, captured) {
                write_capture_state_at(addr as i64, size, captured);
            }
        }
        for (i, entry) in held.iter().enumerate() {
            let Some((session, slots)) = *entry else {
                continue;
            };
            match observed[i] {
                Some(seen) => ff::p67_real_session_release_from(ctx, session, slots, seen),
                None => direct_segment_release(ctx, session, slots, handshake),
            }
        }
        if !before_release {
            if let (Some((addr, size)), Some(captured)) = (state, captured) {
                write_capture_state_at(addr as i64, size, captured);
            }
        }
    }
    let ret = ret_regs.unwrap_or([0, 0]);
    // JIT round 18 wave 11 (lane ffmret; FFMCOST18-2/-3): an `ADDRESS` return
    // mints its segment and a group return calls the allocator, after every
    // release, as the memo path does; nothing raw is held across either.
    if plain_memo_returns_object(memo) {
        // A direct group is `PLAIN_MEMO_GROUP_IN_REGS`: register-returned.
        let group = (
            stub.group_size as usize,
            stub.group_align as usize,
            true,
            None,
        );
        let result = memo_finish_return(ctx, memo, ret, allocator_now, group);
        if split {
            split_t[4] = direct_split_now();
            direct_split_note(
                true,
                true,
                split_t,
                (lean_skipped as u64, lean_owned as u64),
            );
        }
        return Some(result);
    }
    let value = memo_return_value(memo, ret);
    if split {
        split_t[4] = direct_split_now();
        direct_split_note(
            true,
            true,
            split_t,
            (lean_skipped as u64, lean_owned as u64),
        );
    }
    Some(Ok(Some(value)))
}

// ---------------------------------------------------------------------------
// JIT round 18 wave 10 (lane ffmcost): the direct rows' cycle census
// (`CRATONVM_DBG_FFM_DIRECT_SPLIT`) and the lean session handling of the
// direct segment row (`CRATONVM_JIT_FFM_DIRECT_SEGMENT_LEAN`).
// ---------------------------------------------------------------------------
//
// Why the census: round 17's medians put a bound plain direct call at
// ~270-560 ns and a bound direct SEGMENT call at ~1.3-1.7 us (HotSpot 13 /
// ~30 ns), and by reading neither row's own body holds a microsecond: the
// segment row is ~20 `NativeContext` calls, a few pins and two compare-and-
// swaps. The rows' timings came from a host at load 25-60 and from different
// builds, so the split is the first thing to measure. The census cuts a direct
// call into the part OUTSIDE the row (`gap`: from the end of this thread's
// previous served call of the same shape to the start of this one -- in a
// tight compiled loop that is the loop body, the MIC prologue, the native
// site cache probe, the bind's warm check, the admission, the thread-state
// span, `catch_unwind` and the return to compiled code) and the row's own
// phases: `front` (slot reads, segment and scope reads), `acquire` (pins and
// session acquires), `call` (context guard, lazy in-native transition, the C
// function) and `tail` (re-reads, capture state, releases, the return value).

/// `CRATONVM_DBG_FFM_DIRECT_SPLIT` (diagnostic, default OFF; JIT round 18 wave
/// 10, lane ffmcost): the direct rows ([`jit_downcall_direct_invoke`],
/// [`jit_downcall_direct_segment_invoke`]) count their phases in timestamp
/// cycles per thread and print `[ffm-direct-split] shape=plain|segment ..`
/// to stderr every [`DIRECT_SPLIT_REPORT_EVERY`] served calls of a shape.
/// Read once.
pub(in crate::panama) fn ffm_dbg_direct_split() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| false)}

/// `CRATONVM_JIT_FFM_DIRECT_SEGMENT_LEAN` (default ON since round 19, `=0`
/// off; JIT round 18 wave 10, lane ffmcost; FFMSEG17-4 extended, FFMCOST18-1;
/// round 19 A/B, `--compatible` `R18FfmcostShapes`: `seg-confined-crit` 1005 ->
/// 549 ns, `seg-confined` 1079 -> 913, shared and global flat; a REAL session
/// takes the same rule through `p67_real_session_observe`): the direct segment row
/// ([`jit_downcall_direct_segment_invoke`]) handles a session CONFINED to this
/// thread (owner set and checked to be this thread by the row's scope check)
/// with less work:
///
/// * a `critical` handle neither pins, acquires nor releases it. By the
///   option's contract the C function does not call back into Java, so
///   nothing can run on this thread between the scope check and the return,
///   and no other thread can close a confined session (its close refuses a
///   non-owner first, `p67_session_just_close`). The acquire count is
///   therefore unobservable for the call's whole span, exactly as HotSpot's
///   `ConfinedSession.acquire0` (a plain increment) is to every other thread.
///   A misbehaving critical function that upcalls and closes its own arena is
///   undefined behaviour on HotSpot too;
/// * a non-critical handle pins it (an upcall can collect) and acquires it with
///   the count's compare-and-swap alone: no access window and no second scope
///   check. The first check ran on this thread with no GC point and no Java
///   since (pins are a `Vec` push), and only this thread may close it, so the
///   re-check under the window (which exists for a SHARED close on another
///   thread) re-answers a settled question. An upcall's close then still meets
///   the count (`Session is acquired by 1 clients`). Only under the acquire
///   handshake (`CRATONVM_FFM_ACQUIRE_HANDSHAKE`, default on), whose release
///   is the same slot-map compare-and-swap.
///
/// Shared, global and automatic sessions keep the row's handling. Every
/// refusal is unchanged and happens before any effect. Read once.
pub(in crate::panama) fn ffm_jit_direct_segment_lean() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// [`segment_row_scope_open`] that also says whether the session is confined
/// (its `owner` is set, and is this thread): `None` -- the check would raise,
/// decline -- else `Some(confined)`. The same two reads, in the same order
/// (confinement, then liveness: the JDK's).
pub(in crate::panama) fn segment_row_scope_open_owned(
    ctx: &mut dyn NativeContext,
    session: ObjectRef,
    slots: crate::phases_late::foreign_ffm::P67SessionSlots,
) -> Option<bool> {
    if slots.real {
        use crate::phases_late::foreign_ffm as ff;
        let open = ff::p67_real_session_verdict(ctx, session, slots, true)
            == Some(ff::P67RealVerdict::Open);
        let confined = matches!(ctx.get_field(session, slots.owner), Value::Object(Some(_)));
        return open.then_some(confined);
    }
    let confined = match ctx.get_field(session, slots.owner) {
        Value::Object(Some(owner)) => {
            if !crate::phases_late::foreign_ffm::p67_owner_is_current_thread(ctx, owner) {
                return None;
            }
            true
        }
        _ => false,
    };
    (!matches!(ctx.get_field(session, slots.state), Value::Int(0))).then_some(confined)
}

/// The timestamp counter (`rdtsc`), or `0` off x86-64 (the census then
/// reports zeros).
#[inline(always)]
#[cfg(target_arch = "x86_64")]
pub(in crate::panama) fn direct_split_now() -> u64 {
    // SAFETY: rdtsc is unprivileged on x86-64 and touches no memory.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// [`direct_split_now`] off x86-64: no counter, the census reports zeros.
#[inline(always)]
#[cfg(not(target_arch = "x86_64"))]
pub(in crate::panama) fn direct_split_now() -> u64 {
    0
}

/// Served calls per shape between two `[ffm-direct-split]` lines.
pub(in crate::panama) const DIRECT_SPLIT_REPORT_EVERY: u64 = 1 << 16;
/// A `gap` longer than this (cycles, ~5 ms at 3 GHz) is a pause between two
/// rows of a probe, not a call's overhead: not counted.
pub(in crate::panama) const DIRECT_SPLIT_MAX_GAP: u64 = 1 << 24;

/// One shape's running sums ([`ffm_dbg_direct_split`]).
#[derive(Clone, Copy)]
pub(in crate::panama) struct DirectSplitSums {
    pub(in crate::panama) calls: u64,
    pub(in crate::panama) declined: u64,
    pub(in crate::panama) gaps: u64,
    pub(in crate::panama) gap: u64,
    pub(in crate::panama) front: u64,
    pub(in crate::panama) acquire: u64,
    pub(in crate::panama) call: u64,
    pub(in crate::panama) tail: u64,
    /// Sessions the lean handling skipped / fast-acquired.
    pub(in crate::panama) lean_skipped: u64,
    pub(in crate::panama) lean_owned: u64,
    /// The end of this thread's last served call of the shape; `0` after a
    /// decline (the bound call that served it is not this row's).
    pub(in crate::panama) last_end: u64,
}

impl DirectSplitSums {
    pub(in crate::panama) const ZERO: Self = Self {
        calls: 0,
        declined: 0,
        gaps: 0,
        gap: 0,
        front: 0,
        acquire: 0,
        call: 0,
        tail: 0,
        lean_skipped: 0,
        lean_owned: 0,
        last_end: 0,
    };
}

thread_local! {
    /// [`DirectSplitSums`] for the plain (`[0]`) and segment (`[1]`) direct
    /// rows of this thread, and whether this thread has printed its
    /// `[ffm-direct-lean]` engagement line. Diagnostic state only: a `Cell`
    /// of a `Copy` payload, touched only when a switch reading it is on.
    pub(in crate::panama) static DIRECT_ROW_DBG: std::cell::Cell<([DirectSplitSums; 2], bool)> =
        const { std::cell::Cell::new(([DirectSplitSums::ZERO; 2], false)) };
}

/// One direct-row call's census sample: `t` = entry, after `front`, after
/// `acquire`, after `call`, at return. `served == false` counts a decline.
pub(in crate::panama) fn direct_split_note(segments: bool, served: bool, t: [u64; 5], lean: (u64, u64)) {
    let shape = usize::from(segments);
    DIRECT_ROW_DBG.with(|cell| {
        let (mut all, reported) = cell.get();
        let sums = &mut all[shape];
        if !served {
            sums.declined += 1;
            sums.last_end = 0;
            cell.set((all, reported));
            return;
        }
        if sums.last_end != 0 {
            let gap = t[0].wrapping_sub(sums.last_end);
            if gap < DIRECT_SPLIT_MAX_GAP {
                sums.gaps += 1;
                sums.gap += gap;
            }
        }
        sums.calls += 1;
        sums.front += t[1].wrapping_sub(t[0]);
        sums.acquire += t[2].wrapping_sub(t[1]);
        sums.call += t[3].wrapping_sub(t[2]);
        sums.tail += t[4].wrapping_sub(t[3]);
        sums.lean_skipped += lean.0;
        sums.lean_owned += lean.1;
        if sums.calls >= DIRECT_SPLIT_REPORT_EVERY {
            let n = sums.calls;
            eprintln!(
                "[ffm-direct-split] shape={} calls={} declined={} lean={} lean-skipped={} \
                 lean-owned={} cycles/call: gap={} front={} acquire={} call={} tail={}",
                if segments { "segment" } else { "plain" },
                n,
                sums.declined,
                ffm_jit_direct_segment_lean(),
                sums.lean_skipped,
                sums.lean_owned,
                sums.gap / sums.gaps.max(1),
                sums.front / n,
                sums.acquire / n,
                sums.call / n,
                sums.tail / n,
            );
            *sums = DirectSplitSums {
                last_end: direct_split_now(),
                ..DirectSplitSums::ZERO
            };
        } else {
            sums.last_end = direct_split_now();
        }
        cell.set((all, reported));
    });
}

/// Counts a direct segment call that leaves before its served return (a
/// decline, or an acquire a racing close refused) in the census, wherever it
/// returns. Disarmed once the call is committed.
pub(in crate::panama) struct DirectSplitDecline {
    pub(in crate::panama) armed: bool,
}

impl Drop for DirectSplitDecline {
    fn drop(&mut self) {
        if self.armed {
            direct_split_note(true, false, [0; 5], (0, 0));
        }
    }
}

/// `CRATONVM_DBG_FFM`: the first direct segment call on this thread that the
/// lean handling ([`ffm_jit_direct_segment_lean`]) changed prints one
/// `[ffm-direct-lean]` line -- the engagement proof for the A/B.
pub(in crate::panama) fn direct_lean_report(stub: &JitDowncallDirect, skipped: usize, owned: usize) {
    if (skipped == 0 && owned == 0) || !ffm_dbg_enabled() {
        return;
    }
    let first = DIRECT_ROW_DBG.with(|cell| {
        let (all, reported) = cell.get();
        cell.set((all, true));
        !reported
    });
    if first {
        eprintln!(
            "[ffm-direct-lean] fn=0x{:x} critical={} skipped-sessions={} owned-acquires={}",
            stub.fn_addr, stub.critical, skipped, owned
        );
    }
}

/// Whether a downcall returning `return_layout` returns a group BY VALUE: the
/// same classification `pe_downcall_invoke`'s return unmarshalling makes (not
/// a value carrier, not an address, not unclassifiable), so the handle takes
/// the leading `SegmentAllocator` exactly when the call copies a struct out.
pub(in crate::panama) fn downcall_returns_group(ctx: &dyn NativeContext, return_layout: Option<ObjectRef>) -> bool {
    let Some(layout) = return_layout else {
        return false;
    };
    let kind = crate::panama_libffi::read_layout_kind(ctx, layout);
    kind != LAYOUT_ADDRESS
        && kind != crate::panama_libffi::LAYOUT_UNKNOWN
        && !layout_kind_is_value(kind)
}

/// A downcall's by-value group result, copied into
/// `allocator.allocate(size, align)` (round 13 wave 5, lane ffm3): HotSpot's
/// `SharedUtils.allocateBuffer` plus the in-memory-return copy, and the
/// segment the allocator answered is the call's result. `ret` is libffi's
/// return slot (native bytes, nothing to move). Runs Java (the allocator), so
/// no raw reference but `allocator` may be held by the caller across it.
///
/// JIT round 18 wave 11 (lane ffmret): the in-memory rule (every heap segment
/// refused); the callers now say where the ABI returned the group
/// ([`downcall_copy_group_return_in`]).
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::panama) fn downcall_copy_group_return(
    ctx: &mut dyn NativeContext,
    allocator: ObjectRef,
    size: usize,
    align: usize,
    ret: &[u8],
) -> Result<ObjectRef, MethodCallFailed> {
    downcall_copy_group_return_in(ctx, allocator, size, align, ret, false, None)
}



/// Does the target's C ABI return a group of `size` bytes, whose prepared
/// libffi type is `rtype`, in registers (not through a hidden pointer)? The
/// classification HotSpot's `CallArranger` makes (`STRUCT_REGISTER` /
/// `STRUCT_HFA` against in-memory return), for the layouts the JDK linker
/// admits (naturally aligned, padded). An unknown target answers `false`
/// (the in-memory rule, the old refusal).
///
/// # Safety
///
/// `rtype` is null or a prepared `ffi_type`.
pub(in crate::panama) unsafe fn group_returned_in_registers(size: usize, rtype: *const libffi::raw::ffi_type) -> bool {
    if size == 0 {
        return false;
    }
    if cfg!(all(target_arch = "x86_64", windows)) {
        return matches!(size, 1 | 2 | 4 | 8);
    }
    if cfg!(target_arch = "x86_64") {
        return size <= 16;
    }
    if cfg!(target_arch = "aarch64") {
        if size <= 16 {
            return true;
        }
        let mut kind = None;
        let mut count = 0usize;
        // SAFETY: the caller's contract.
        return unsafe { ffi_type_float_leaves(rtype, &mut kind, &mut count, 0) } && count <= 4;
    }
    false
}

/// Are all leaves of `ty` one floating kind (all `float` or all `double`)?
/// Counts them into `count` (an AArch64 homogeneous float aggregate has at
/// most four).
///
/// # Safety
///
/// As [`ffi_type_all_integer`].
pub(in crate::panama) unsafe fn ffi_type_float_leaves(
    ty: *const libffi::raw::ffi_type,
    kind: &mut Option<u16>,
    count: &mut usize,
    depth: usize,
) -> bool {
    if ty.is_null() || depth > crate::panama_libffi::MAX_LAYOUT_DEPTH {
        return false;
    }
    // SAFETY: the caller's contract.
    let t = unsafe { &*ty };
    match t.type_ {
        // FFI_TYPE_FLOAT, FFI_TYPE_DOUBLE.
        2 | 3 => {
            if kind.is_some_and(|k| k != t.type_) {
                return false;
            }
            *kind = Some(t.type_);
            *count += 1;
            true
        }
        // FFI_TYPE_STRUCT.
        13 => {
            let mut cursor = t.elements as *const *mut libffi::raw::ffi_type;
            if cursor.is_null() {
                return false;
            }
            loop {
                // SAFETY: a null-terminated element array.
                let element = unsafe { *cursor };
                if element.is_null() {
                    return *count > 0;
                }
                // SAFETY: a prepared element type.
                if !unsafe { ffi_type_float_leaves(element, kind, count, depth + 1) } {
                    return false;
                }
                // SAFETY: still inside the null-terminated array.
                cursor = unsafe { cursor.add(1) };
            }
        }
        _ => false,
    }
}

/// [`group_returned_in_registers`] for the return type of a prepared CIF.
///
/// # Safety
///
/// `cif` is null or a prepared, live CIF.
pub(in crate::panama) unsafe fn cif_group_returned_in_registers(cif: *const libffi::raw::ffi_cif) -> bool {
    if cif.is_null() {
        return false;
    }
    // SAFETY: the caller's contract.
    let rtype: *const libffi::raw::ffi_type = unsafe { (*cif).rtype };
    if rtype.is_null() {
        return false;
    }
    // SAFETY: a prepared CIF's return type.
    unsafe { group_returned_in_registers((*rtype).size, rtype) }
}

/// [`downcall_copy_group_return`] knowing where the ABI returned the group:
/// the allocator is called (AFTER the C call: HotSpot's order for a
/// register-returned group, whose `Allocate` binding is one of the return
/// bindings) and the bytes are stored into its segment
/// ([`downcall_store_group`]). `hfa_width` is an AArch64 homogeneous float
/// aggregate's member width ([`group_hfa_width`]), `None` elsewhere.
pub(in crate::panama) fn downcall_copy_group_return_in(
    ctx: &mut dyn NativeContext,
    allocator: ObjectRef,
    size: usize,
    align: usize,
    ret: &[u8],
    in_registers: bool,
    hfa_width: Option<usize>,
) -> Result<ObjectRef, MethodCallFailed> {
    let allocated = ctx.invoke_virtual(
        allocator,
        "allocate",
        "(JJ)Ljava/lang/foreign/MemorySegment;",
        &[Value::Long(size as i64), Value::Long(align as i64)],
    )?;
    let Some(Value::Object(Some(seg))) = allocated else {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    };
    downcall_store_group(ctx, seg, size, ret, in_registers, hfa_width)?;
    Ok(seg)
}

/// Store a returned group's bytes `ret` into `seg`, the segment the allocator
/// answered.
///
/// * A register-returned group under `CRATONVM_FFM_GROUP_RETURN_STORE_CHECKS`
///   (JIT round 18 wave 11, lane ffmret) takes HotSpot's `BufferStore`
///   sequence ([`downcall_store_register_group`]): a read-only segment is
///   `IllegalArgumentException: Attempt to write a read-only segment`, a store
///   past the end is `IndexOutOfBoundsException` after the stores before it,
///   no alignment check (the stores are unaligned). A heap segment takes it
///   under `CRATONVM_FFM_GROUP_RETURN_HEAP_ALLOCATOR`.
/// * Otherwise (an in-memory group, or the switches off): a heap segment is
///   refused (`unboxSegment`) unless the round-18 heap rule admits it, and the
///   bytes are copied clamped to the segment, as before. HotSpot hands an
///   in-memory buffer to C, so an undersized one is the C function's overrun
///   there; nothing is raised, and the copy here stays in bounds.
pub(in crate::panama) fn downcall_store_group(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    size: usize,
    ret: &[u8],
    in_registers: bool,
    hfa_width: Option<usize>,
) -> Result<(), MethodCallFailed> {
    if in_registers {
        let plan = group_register_store_plan(size.min(ret.len()), hfa_width);
        return downcall_store_register_group(ctx, seg, ret, &plan);
    }
    // JIT round 18 wave 11 (lane ffmret): a register-returned group is
    // written through the segment, heap or native.
    if in_registers {
        if let Some(view) = heap_segment_view(ctx, seg).filter(|view| !view.read_only) {
            let _window = crate::ffm_fast::AccessWindow::open();
            pe_segment_check_scope(ctx, seg)?;
            let len = size.min(ret.len()).min(view.size.max(0) as usize);
            if len > 0 {
                heap_write_bytes(ctx, &view, 0, &ret[..len]);
            }
            return Ok(());
        }
    }
    if let Some(refusal) = heap_segment_refusal(&*ctx, seg) {
        return Err(refusal);
    }
    // From the scope check to the last byte written, like every other write
    // into a segment: a shared close on another thread waits for it.
    let _window = crate::ffm_fast::AccessWindow::open();
    pe_segment_check_scope(ctx, seg)?;
    let addr = crate::panama_libffi::segment_address(ctx, seg);
    let capacity = crate::panama_libffi::segment_byte_size(ctx, seg).max(0) as usize;
    // Clamped to both sides: an allocator may answer a smaller segment than
    // asked, and `ret` is `alloc_return_slot`'s size, not the layout's.
    let len = size.min(ret.len()).min(capacity);
    if addr != 0 && len > 0 {
        // SBR5 ffmlink: an allocator over `Unsafe.allocateMemory` (every `Arena`) answers a
        // segment whose address is an arena HANDLE; writing through it faulted (a `div`
        // call returning a struct crashed the VM). Resolve it, and give the pointer back
        // once the bytes are copied.
        let real = crate::panama_libffi::host_base_address(addr, len as i64);
        if real != 0 {
            // SAFETY: `real` is the start of a live native segment of at least `len`
            // bytes (scope checked above, under the access window), `ret` holds at
            // least `len` bytes, and a fresh allocation cannot overlap the return slot.
            unsafe { std::ptr::copy_nonoverlapping(ret.as_ptr(), real as *mut u8, len) };
        }
        if crate::panama_libffi::is_arena_handle(addr) {
            crate::unsafe_natives_ext::unsafe_arena_release_translation(addr);
        }
    }
    Ok(())
}



/// The stores HotSpot's return bindings make for a register-returned group
/// of `size` bytes, as `(offset, width)` in order: one per member of an
/// AArch64 homogeneous float aggregate (`hfa_width`); one store of the whole
/// group on Win64 (1, 2, 4 or 8 bytes in `rax`); otherwise one per eightbyte,
/// the last one `size - offset` wide (`CallArranger`'s `Math.min(byteSize -
/// offset, 8)`). Pure.
pub(in crate::panama) fn group_register_store_plan(size: usize, hfa_width: Option<usize>) -> Vec<(usize, usize)> {
    if let Some(width) = hfa_width.filter(|w| *w > 0) {
        return (0..size / width).map(|i| (i * width, width)).collect();
    }
    if cfg!(windows) {
        return vec![(0, size)];
    }
    let mut plan = Vec::with_capacity(2);
    let mut offset = 0;
    while offset < size {
        let width = (size - offset).min(8);
        plan.push((offset, width));
        offset += width;
    }
    plan
}

/// HotSpot's `BufferStore` sequence into `seg`: `checkAccess` per store (the
/// read-only check, then the bounds), the session checked at the first store,
/// and each store's bytes written before the next store is checked -- so a
/// segment too small for the group raises `IndexOutOfBoundsException` with
/// the earlier stores already written. Heap or native.
pub(in crate::panama) fn downcall_store_register_group(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    ret: &[u8],
    plan: &[(usize, usize)],
) -> Result<(), MethodCallFailed> {
    if plan.is_empty() {
        return Ok(());
    }
    if downcall_store_register_group_real(ctx, seg, ret, plan) {
        return Ok(());
    }
    craton_segment_check_read_only(ctx, seg, false)?;
    let view = heap_segment_view(&*ctx, seg);
    let _window = crate::ffm_fast::AccessWindow::open();
    for (k, &(offset, width)) in plan.iter().enumerate() {
        pe_slice_bounds_check(&*ctx, seg, offset as i64, width as i64)?;
        if k == 0 {
            pe_segment_check_scope(ctx, seg)?;
        }
        let Some(bytes) = ret.get(offset..offset + width) else {
            return Ok(());
        };
        match &view {
            Some(view) => {
                heap_write_bytes(ctx, view, offset as i64, bytes);
            }
            None => {
                let addr = crate::panama_libffi::segment_address(&*ctx, seg);
                if addr != 0 {
                    // An `Arena`'s segment has an arena HANDLE for an address
                    // (the SBR5 ffmlink fix of the clamped copy below, which
                    // this default path missed: `R13Ffm3StructReturn`'s
                    // `div` returning a struct into an arena segment wrote to
                    // the handle and faulted). Resolve each store's bytes and
                    // give the pointer back once they are written.
                    let at = addr.wrapping_add(offset as i64);
                    let real = crate::panama_libffi::host_base_address(at, width as i64);
                    if real != 0 {
                        // SAFETY: the bounds check above put `offset + width`
                        // inside this live native segment (scope checked, under
                        // the access window), and `real` is that range's host
                        // address; `bytes` is `width` bytes of `ret`.
                        unsafe {
                            std::ptr::copy_nonoverlapping(bytes.as_ptr(), real as *mut u8, width)
                        };
                    }
                    if crate::panama_libffi::is_arena_handle(at) {
                        crate::unsafe_natives_ext::unsafe_arena_release_translation(at);
                    }
                }
            }
        }
    }
    Ok(())
}

/// JIT round 22 (lane memory, MEM20-6 direction 4): [`downcall_store_register_group`]
/// for the common answer of an allocator -- a real JDK native segment
/// ([`segment_row_real_slots`]) in a real session -- when every check that
/// function makes passes: not read-only, every store of `plan` inside
/// `length`, the session open and (if confined) this thread's. Then the four
/// fields come in one batch, the address resolves once for the whole group
/// (a borrow under the access window, [`crate::panama_libffi::borrowed_host_base_address`])
/// and the stores are copies. `true`: every store was made. `false`: nothing
/// was written -- some check would raise, or the segment is another shape --
/// and the per-store path runs and raises in HotSpot's order. Reads only
/// before the copies: the owner test reads the thread's existing mirror, so
/// no GC point lies under `seg`.
pub(in crate::panama) fn downcall_store_register_group_real(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    ret: &[u8],
    plan: &[(usize, usize)],
) -> bool {
    use crate::phases_late::foreign_ffm as ff;
    let mut shape = segment_row_shape(ctx.vm_identity());
    let Some(slots) = segment_row_real_slots(&*ctx, &mut shape, seg) else {
        return false;
    };
    let mut words = [Value::Object(None); 4];
    ctx.get_fields_typed(
        seg,
        &[
            (slots.min, b'J'),
            (slots.length, b'J'),
            (slots.read_only, b'Z'),
            (PE_SEGMENT_ARENA_FIELD, b'L'),
        ],
        &mut words,
    );
    let (Value::Long(min), Value::Long(length), Value::Int(0)) = (words[0], words[1], words[2])
    else {
        return false;
    };
    let mut end = 0i64;
    for &(offset, width) in plan {
        let (Ok(at), Ok(w)) = (i64::try_from(offset), i64::try_from(width)) else {
            return false;
        };
        match at.checked_add(w) {
            Some(e) if e <= length && ret.len() >= offset + width => end = end.max(e),
            _ => return false,
        }
    }
    let Value::Object(Some(session)) = words[3] else {
        return false;
    };
    let Some(session_slots) =
        segment_row_session_slots(&*ctx, &mut shape, session).filter(|s| s.real)
    else {
        return false;
    };
    if min == 0 {
        return false;
    }
    // From the scope check to the last byte written, as the per-store path.
    let _window = crate::ffm_fast::AccessWindow::open();
    if ff::p67_real_session_verdict(ctx, session, session_slots, false)
        != Some(ff::P67RealVerdict::Open)
    {
        return false;
    }
    let real = crate::panama_libffi::borrowed_host_base_address(min, end);
    if real == 0 {
        return false;
    }
    for &(offset, width) in plan {
        // SAFETY: `real` is the host address of the live block behind `min`,
        // covering `end` bytes (`borrowed_host_base_address` refuses a block
        // shorter than that), the session was checked open under this access
        // window (a shared close waits for it), every store ends at or below
        // `end`, and `ret` holds `offset + width` bytes (checked above).
        unsafe {
            std::ptr::copy_nonoverlapping(
                ret.as_ptr().add(offset),
                (real as *mut u8).add(offset),
                width,
            );
        }
    }
    true
}



/// The member width of an AArch64 homogeneous float aggregate whose prepared
/// libffi type is `rtype` (4 for `float` members, 8 for `double`, at most four
/// members), or `None` (any other target or type).
///
/// # Safety
///
/// As [`ffi_type_all_integer`].
pub(in crate::panama) unsafe fn group_hfa_width(rtype: *const libffi::raw::ffi_type) -> Option<usize> {
    if !cfg!(target_arch = "aarch64") {
        return None;
    }
    let mut kind = None;
    let mut count = 0usize;
    // SAFETY: the caller's contract.
    if !unsafe { ffi_type_float_leaves(rtype, &mut kind, &mut count, 0) } || count > 4 {
        return None;
    }
    match kind {
        Some(2) => Some(4),
        Some(3) => Some(8),
        _ => None,
    }
}



/// Acquire, for one downcall, each distinct modelled session among
/// `segments`' scopes. Answers every acquired session with the native pin
/// that keeps it current: the caller re-reads each through its pin after the
/// call, releases the pins (the first one's index releases them all), and
/// then [`pe_downcall_release_sessions`] them.
///
/// Every session is pinned BEFORE the first acquire, because a refused
/// acquire builds its exception (`WrongThreadException` runs a constructor)
/// and the ones already acquired must then be released through current
/// references. On a refusal nothing stays acquired and nothing stays pinned.
pub(in crate::panama) fn pe_downcall_acquire_sessions(
    ctx: &mut dyn NativeContext,
    segments: &[ObjectRef],
) -> Result<Vec<(ObjectRef, usize)>, MethodCallFailed> {
    if !ffm_bulk_scope_check() {
        return Ok(Vec::new());
    }
    let mut sessions: Vec<ObjectRef> = Vec::new();
    for &segment in segments {
        // A modelled session, else a real JDK one (`pe_segment_real_session`).
        let session = pe_segment_session(ctx, segment)
            .or_else(|| pe_segment_real_session(ctx, segment).map(|(session, _)| session));
        if let Some(session) = session {
            if !sessions.contains(&session) {
                sessions.push(session);
            }
        }
    }
    let pinned: Vec<(ObjectRef, usize)> = sessions
        .iter()
        .map(|&session| (session, ctx.pin_native_root(session)))
        .collect();
    for (done, &(session, pin)) in pinned.iter().enumerate() {
        let mut current = ctx.read_native_pin(pin, session);
        let acquired = match pe_real_session_kind(ctx, current) {
            Some(kind) => pe_real_session_acquire(ctx, current, kind),
            None => crate::phases_late::foreign_ffm::p67_session_acquire(ctx, &mut current),
        };
        if let Err(err) = acquired {
            let acquired: Vec<ObjectRef> = pinned[..done]
                .iter()
                .map(|&(session, pin)| ctx.read_native_pin(pin, session))
                .collect();
            if let Some(&(_, base)) = pinned.first() {
                ctx.unpin_native_roots(base);
            }
            pe_downcall_release_sessions(ctx, &acquired);
            return Err(err);
        }
    }
    Ok(pinned)
}

/// The release half of [`pe_downcall_acquire_sessions`]. Total, like
/// `release0`: it runs after the native call whatever the call did.
pub(in crate::panama) fn pe_downcall_release_sessions(ctx: &mut dyn NativeContext, sessions: &[ObjectRef]) {
    for &session in sessions {
        match pe_real_session_kind(ctx, session) {
            Some(kind) => pe_real_session_release(ctx, session, kind),
            None => crate::phases_late::foreign_ffm::p67_session_release(ctx, session),
        }
    }
}

/// `(little_endian, byte_alignment)` an element access through `layout` must
/// honour.
///
/// A live layout verdict (`ffm_fast::layout_align_mask`) answers without
/// touching the layout: it is only ever published for a host-order layout,
/// and it carries the alignment. Otherwise the order is read the one way this
/// crate reads it (`foreign_ffm::p67_layout_is_little`, which knows both the
/// real `ValueLayouts$Of*Impl` and the fabricated carrier) and the alignment
/// through `panama_libffi::layout_align`, the reader the heap path already
/// uses.
pub(in crate::panama) fn pe_layout_access_traits(ctx: &dyn NativeContext, layout: ObjectRef) -> (bool, u64) {
    if let Some(mask) = crate::ffm_fast::layout_align_mask(layout.as_ptr() as u64) {
        return (cfg!(target_endian = "little"), mask.saturating_add(1));
    }
    let little = {
        crate::phases_late::foreign_ffm::p67_layout_is_little(ctx, layout)
    };
    let align = (crate::panama_libffi::layout_align(ctx, layout) as u64).max(1);
    (little, align)
}

/// Publish a layout verdict for the JIT element fast path: only for a layout
/// in host byte order whose alignment is no stricter than its size (the
/// fast path serves `getAtIndex`/`setAtIndex`, which refuse a stricter one).
/// `observed` is the epoch read before the layout's traits were.
pub(in crate::panama) fn pe_publish_layout_verdict(
    layout: ObjectRef,
    little: bool,
    align: u64,
    width: i64,
    observed: u64,
) {
    if little != cfg!(target_endian = "little") || width <= 0 || align > width as u64 {
        return;
    }
    crate::ffm_fast::note_layout_validated_at(layout.as_ptr() as u64, align, observed);
}

