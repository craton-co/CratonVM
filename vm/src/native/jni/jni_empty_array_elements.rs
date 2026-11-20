#[allow(unused_imports)]
use super::*;


// ---- Indices 183-190: Get<Type>ArrayElements ----
// Returns a pointer to a native-endian COPY of the array data.
//
// GC-correctness (vm-jni-roots #2): we ALWAYS hand native code a detached copy
// of the array body and report `is_copy = JNI_TRUE` — never a raw pointer into
// the live, in-heap array. This VM ships *moving* collectors (the generational
// young-gen copy and the G1 evacuator both relocate live objects); a direct
// body pointer handed to C would dangle the instant a GC fired while native
// code held it — a use-after-free / heap-corruption hole. The JNI spec
// explicitly permits returning a copy (`is_copy = JNI_TRUE`), so a copy is the
// only collector-agnostic correct choice for this heap. (HotSpot can return a
// direct pointer because it pins the array's page/region for the window; this
// VM closes the same gap with the copy instead. A prior revision handed out the
// direct `array_data_ptr` for ordinary arrays on a "heap never moves" premise
// the VM does not actually hold — that was the bug this restores.)
//
// The copy is built via the region-safe per-element accessor, so it works
// uniformly for ordinary contiguous arrays AND G1 *humongous* arrays (whose
// payload spans non-contiguous regions and have no flat data pointer). It is
// registered in the VM's `JniGlobalRefs::elem_copies` (per VM since gc-common
// w10-c, so a release from any thread finds it) and the matching
// `Release<Type>ArrayElements` copies any mutations back and frees it.
//
// Keep-alive AND copy-back correctness under a MOVING GC: at Get we mint a JNI
// **global ref** for the SOURCE array and stash its handle in the buffer's
// `elem_copies` record. A global ref is both (a) a GC root — so the
// array cannot be reclaimed before the copy-back at Release — and, crucially,
// (b) *remappable*: `collect_roots` scans it and `update_after_gc` /
// `update_all_roots` rewrite the boxed `ObjectRef` to the object's new address
// after every relocating collection (generational young-copy and G1 evacuation
// alike). So `Release<Type>ArrayElements` resolves the array's CURRENT location
// through that handle, and the (possibly mutated) copy is written back into the
// live array — never a freed CSet region (silent data loss) or a recycled
// object (corruption). This replaces the previous keep-alive object-pin for
// this path, which was address-keyed and therefore went stale under a move; the
// global ref is deleted on the final Release. (The JNI spec permits holding the
// copy arbitrarily long, so region pinning — used by the *critical* path — is
// the wrong tool here: it would starve the collector for the whole window. A
// remappable handle imposes no such no-relocation constraint.)
/// The address `Get<Type>ArrayElements` answers for an empty array, in place
/// of HotSpot's `get_bad_address()` (a reserved, protected page): non-NULL,
/// the same for every call and element type (8, aligned for all of them),
/// never recorded in `elem_copies` and never freed. Like HotSpot's, it faults
/// if a native reads or writes through it (there are no elements). Not a
/// `static` (the per-VM statics ratchet), and no Rust allocation ever has this
/// address registered as a copy buffer: every recorded buffer holds at least
/// one element.
pub(in crate::native::jni) fn jni_empty_array_elements<T>() -> *mut T {
    std::ptr::NonNull::<u64>::dangling().as_ptr().cast::<T>()
}

/// The empty `Get<Type>ArrayElements` copy buffer with room for `len`
/// elements, or `None` when the C heap cannot hold it. It deliberately takes
/// no VM: HotSpot 25 answers a failed copy allocation with NULL and no pending
/// exception (`NEW_C_HEAP_ARRAY_RETURN_NULL` in
/// `DEFINE_GETSCALARARRAYELEMENTS`), so nothing here may raise one.
pub(in crate::native::jni) fn reserve_elements_copy<T>(len: usize) -> Option<Vec<T>> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(len).ok()?;
    Some(buf)
}

get_array_elements!(jni_get_boolean_array_elements, JBoolean, Int, 0); // 183
get_array_elements!(jni_get_byte_array_elements, JByte, Int, 0); // 184
get_array_elements!(jni_get_char_array_elements, JChar, Int, 0); // 185
get_array_elements!(jni_get_short_array_elements, JShort, Int, 0); // 186
get_array_elements!(jni_get_int_array_elements, JInt, Int, 0); // 187
get_array_elements!(jni_get_long_array_elements, JLong, Long, 0); // 188
get_array_elements!(jni_get_float_array_elements, JFloat, Float, 0.0); // 189
get_array_elements!(jni_get_double_array_elements, JDouble, Double, 0.0); // 190

/// `(copy back, free)` for a `Release<Type>ArrayElements` mode, as HotSpot 25's
/// `DEFINE_RELEASESCALARARRAYELEMENTS` decides them: `0` both, `JNI_COMMIT`
/// (1) copy only, `JNI_ABORT` (2) free only, any other value neither (j27
/// wave 1, lane JNINAT).
pub(in crate::native::jni) fn release_elements_effects(mode: JInt) -> (bool, bool) {
    (matches!(mode, 0 | 1), matches!(mode, 0 | 2))
}

release_array_elements!(
    jni_release_boolean_array_elements,
    JBoolean,
    |v: JBoolean| Value::Int(v as i32)
); // 191
release_array_elements!(jni_release_byte_array_elements, JByte, |v: JByte| {
    Value::Int(v as i32)
}); // 192
release_array_elements!(jni_release_char_array_elements, JChar, |v: JChar| {
    Value::Int(v as i32)
}); // 193
release_array_elements!(jni_release_short_array_elements, JShort, |v: JShort| {
    Value::Int(v as i32)
}); // 194
release_array_elements!(jni_release_int_array_elements, JInt, |v: JInt| Value::Int(
    v
)); // 195
release_array_elements!(jni_release_long_array_elements, JLong, |v: JLong| {
    Value::Long(v)
}); // 196
release_array_elements!(jni_release_float_array_elements, JFloat, |v: JFloat| {
    Value::Float(v)
}); // 197
release_array_elements!(jni_release_double_array_elements, JDouble, |v: JDouble| {
    Value::Double(v)
}); // 198

// ---------------------------------------------------------------------------
// Region bounds-checking (JNI Get/Set<Type>ArrayRegion + string regions)
// ---------------------------------------------------------------------------
//
// The JNI spec mandates that `Get/Set<Type>ArrayRegion` and the string-region
// accessors throw `ArrayIndexOutOfBoundsException` (resp. `StringIndexOutOf-
// BoundsException`, which we surface as AIOOBE) when `start`/`len` fall outside
// the array/string. Previously these helpers relied only on the per-element
// accessor's *internal* bounds check, which silently swallowed an out-of-range
// request (reading/writing nothing) instead of signalling the contract error.
// `region_bounds_ok` performs the explicit, overflow-safe check; on failure it
// raises a pending AIOOBE so the interpreter throws it on native return.

/// Set a pending `ArrayIndexOutOfBoundsException` for the current JNI call.
///
/// When a `JvmThread` context is available we materialise a real exception
/// object and store its handle in `JNI_PENDING_EXCEPTION` (the same slot
/// `Throw`/`ThrowNew` use), so `vm_exec` rethrows it as a catchable Java
/// exception on return from the native call. Without a thread context (e.g. a
/// direct unit-test call) we fall back to the `ThrowNew` sentinel so the error
/// is still flagged rather than silently dropped.
///
/// gcf f3/j: the class and message are the caller's (HotSpot's
/// `check_bounds` for an array region, a bare `StringIndexOutOfBoundsException`
/// for a string region; see [`region_bounds_ok`]).
pub(in crate::native::jni) fn raise_jni_region_error(class_name: &str, msg: Option<&str>) {
    let raised =
        with_jni_context(
            |shared, thread| match crate::runtime::exceptions::create_exception_object(
                shared, thread, class_name, msg,
            ) {
                Ok(exc) => {
                    set_jni_pending_exception_object(exc);
                    Ok(())
                }
                Err(e) => Err(is_heap_exhaustion(&e)),
            },
        )
        .unwrap_or(Err(false));
    if let Err(heap_exhausted) = raised {
        // No thread context or allocation failed: the preallocated OOME for a
        // heap failure, else the sentinel, so the condition is not silently
        // swallowed (gcd d4/k, `pend_unbuilt_throwable`).
        pend_unbuilt_throwable(heap_exhausted);
    }
}

/// Validate that the `[start, start + len)` window lies within `[0, length)`.
///
/// `start`/`len` are caller-supplied `JSize` (i32). Returns `true` for an
/// in-bounds (or empty) region and `false` otherwise; on `false` a pending
/// exception has been raised. Overflow-safe (64-bit arithmetic).
///
/// gcf f3/j (`gcf-f2j-jni-region-edges-diverge-from-hotspot`), the answers
/// HotSpot 25 gives (read off its own JNI table through a native method,
/// 2026-10-01): an ARRAY region raises `ArrayIndexOutOfBoundsException` with
/// `Length <len> is negative`, or `Array region <start>..<start+len> out of
/// bounds for length <n>`; a negative window was ignored here, and the
/// message was `Index <start> out of bounds for length <n>`.
pub(in crate::native::jni) fn region_bounds_ok(start: JSize, len: JSize, length: usize) -> bool {
    match region_bounds_error(start, len, length) {
        None => true,
        Some(msg) => {
            raise_jni_region_error("java/lang/ArrayIndexOutOfBoundsException", Some(&msg));
            false
        }
    }
}

/// gcf f3/j: HotSpot's `check_bounds` message for an out-of-range array
/// region, or `None` when `[start, start + len)` fits `[0, length)`.
pub(in crate::native::jni) fn region_bounds_error(start: JSize, len: JSize, length: usize) -> Option<String> {
    if len < 0 {
        return Some(format!("Length {len} is negative"));
    }
    let end = i64::from(start) + i64::from(len);
    if start < 0 || end > i64::try_from(length).unwrap_or(i64::MAX) {
        return Some(format!(
            "Array region {start}..{end} out of bounds for length {length}"
        ));
    }
    None
}

/// gcf f3/j: a STRING region (`GetStringRegion`, `GetStringUTFRegion`) out of
/// range, negative window included, raises a bare
/// `StringIndexOutOfBoundsException` (no message), as HotSpot 25 does; it
/// raised `ArrayIndexOutOfBoundsException` here, and nothing for a negative
/// window.
pub(in crate::native::jni) fn string_region_bounds_ok(start: JSize, len: JSize, length: usize) -> bool {
    if region_bounds_error(start, len, length).is_none() {
        return true;
    }
    raise_jni_region_error("java/lang/StringIndexOutOfBoundsException", None);
    false
}

// gcf f2/j (`common-w9g-proposal-bulk-jni-array-copies`, opt-in
// `CRATONVM_JNI_BULK_ARRAY_COPY=1`): a `Get/Set<Type>ArrayRegion` whose array
// really has the region's element type copies the `len * stride` bytes
// between the array's payload (`VmHeap::array_data_ptr`, contiguous on every
// backend) and the C buffer with one `memcpy`, as HotSpot's
// `ArrayAccess::arraycopy_to/from_native` does, instead of one boxed
// `get_array_element` / `set_array_element` per element. Sound for the same
// reason the element walk is: the caller is a counted mutator or inside its
// leaf window, so no pause can move the array during the copy, and a
// primitive store needs no barrier on any collector. Exceptions keep the
// walk: a mismatched element type (the walk's per-element conversion), a
// `boolean[]` store (the walk's store, not a raw byte), and the `long[]`
// store's per-element smuggled-handle mint (kept, over the copied values).

/// gcf f2/j: is `CRATONVM_JNI_BULK_ARRAY_COPY` on (latched)?
///
/// gcg g5/jn: default ON (`=0` / `false` / `off` / `no` kills it), on vg4's
/// `jn4_*_bulk_*` rows: stdout HotSpot's on all 32 rows, and a 65 536-element
/// `Get`+`Set<Int>ArrayRegion` round trip ~1 ms -> ~5 us on Gen, G1 and ZGC
/// (`GcgG4jnJniBulkRegionProbe`); a 16-element one 345 -> 44 ns.
pub(in crate::native::jni) fn bulk_array_copies() -> bool {
    jni_switches().bulk_array_copies
}

/// gcg g5/jn: the default rule of `CRATONVM_JNI_BULK_ARRAY_COPY` from its
/// explicit value ([`jni_switch_value`]): on unless set off. Pure.
pub(in crate::native::jni) fn bulk_array_copies_from(value: Option<bool>) -> bool {
    value != Some(false)
}

/// gcf f2/j: the address of element `start` of `oref`'s payload when the array
/// holds `want` elements (`None`: not such an array, so the caller walks).
/// `stride` is the element size the caller copies with.
pub(in crate::native::jni) fn primitive_region_ptr(
    shared: &SharedVm,
    oref: ObjectRef,
    want: ArrayElementType,
    start: usize,
    stride: usize,
) -> Option<*mut u8> {
    if shared.mem.heap.array_element_type(oref)? != want {
        return None;
    }
    let base = shared.mem.heap.array_data_ptr(oref)?;
    // SAFETY: the caller bounds-checked `[start, start + len)` against the
    // array's length, so the offset lies inside the payload.
    Some(unsafe { base.add(start * stride) })
}

get_array_region!(
    jni_get_boolean_array_region,
    JBoolean,
    Int,
    0,
    ArrayElementType::Boolean
); // 199
get_array_region!(
    jni_get_byte_array_region,
    JByte,
    Int,
    0,
    ArrayElementType::Byte
); // 200
get_array_region!(
    jni_get_char_array_region,
    JChar,
    Int,
    0,
    ArrayElementType::Char
); // 201
get_array_region!(
    jni_get_short_array_region,
    JShort,
    Int,
    0,
    ArrayElementType::Short
); // 202
get_array_region!(
    jni_get_int_array_region,
    JInt,
    Int,
    0,
    ArrayElementType::Int
); // 203
get_array_region!(
    jni_get_long_array_region,
    JLong,
    Long,
    0,
    ArrayElementType::Long
); // 204
get_array_region!(
    jni_get_float_array_region,
    JFloat,
    Float,
    0.0,
    ArrayElementType::Float
); // 205
get_array_region!(
    jni_get_double_array_region,
    JDouble,
    Double,
    0.0,
    ArrayElementType::Double
); // 206

set_array_region!(
    jni_set_boolean_array_region,
    JBoolean,
    |v: JBoolean| Value::Int(v as i32),
    None
); // 207
set_array_region!(
    jni_set_byte_array_region,
    JByte,
    |v: JByte| Value::Int(v as i32),
    Some(ArrayElementType::Byte)
); // 208
set_array_region!(
    jni_set_char_array_region,
    JChar,
    |v: JChar| Value::Int(v as i32),
    Some(ArrayElementType::Char)
); // 209
set_array_region!(
    jni_set_short_array_region,
    JShort,
    |v: JShort| Value::Int(v as i32),
    Some(ArrayElementType::Short)
); // 210
set_array_region!(
    jni_set_int_array_region,
    JInt,
    |v: JInt| Value::Int(v),
    Some(ArrayElementType::Int)
); // 211

// Index 212: SetLongArrayRegion — hand-unrolled (vs the macro) to add the
// long-smuggle mint chokepoint: native code bulk-storing raw jobject handles
// into a long[] (see smuggled_longs). Strict object-start probe per element,
// so ordinary numeric bulk stores register nothing.
pub(in crate::native::jni) extern "C" fn jni_set_long_array_region(
    _env: JNIEnv,
    array: JArray,
    start: JSize,
    len: JSize,
    buf: *const JLong,
) {
    let _fx = ForeignJniEntry::enter();
    if array == 0 || buf.is_null() {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(array)?;
        // JNI contract: validate the requested window against the array
        // length BEFORE writing; raise AIOOBE on a bad range so no
        // partial / out-of-range store is performed.
        if !region_bounds_ok(start, len, shared.mem.heap.array_length(oref)) {
            return None;
        }
        // gcf f2/j: one copy, then the mint over the stored values (see
        // `primitive_region_ptr`).
        let bulk = if bulk_array_copies() {
            primitive_region_ptr(shared, oref, ArrayElementType::Long, start as usize, 8)
        } else {
            None
        };
        if let Some(dst) = bulk {
            // SAFETY: as in `get_array_region!`, the other way.
            unsafe {
                std::ptr::copy_nonoverlapping(buf as *const u8, dst, len as usize * 8);
            }
        }
        for i in 0..len as usize {
            let val = unsafe { *buf.add(i) };
            let bits = val as u64;
            if bits != 0
                && bits & 0x7 == 0
                && shared.mem.heap.is_object_address(bits as usize).is_some()
            {
                crate::memory::smuggled_longs::record_minted_long(&shared.mem.heap, bits);
            }
            if bulk.is_none() {
                let _ =
                    shared
                        .mem
                        .heap
                        .set_array_element(oref, start as usize + i, Value::Long(val));
            }
        }
        Some(())
    });
}
set_array_region!(
    jni_set_float_array_region,
    JFloat,
    |v: JFloat| Value::Float(v),
    Some(ArrayElementType::Float)
); // 213
set_array_region!(
    jni_set_double_array_region,
    JDouble,
    |v: JDouble| Value::Double(v),
    Some(ArrayElementType::Double)
); // 214

// ---- Index 217: MonitorEnter ----
//
// gc-common w2-c (2026-09-23), `common-a-jni-placeholder-thread-id-at-barrier`:
// this used `ThreadId(0)` — the MAIN thread's id — as both the monitor owner and
// the barrier identity, whatever thread it ran on. Consequences, all fixed here:
//
// * ownership: every JNI caller shared main's identity, so two native threads
//   could both "hold" one monitor (a re-entrant acquire), and a native
//   `MonitorEnter` on an object its own Java caller already locked with
//   `synchronized` contended with ITSELF (unless it was main) and never woke;
// * the barrier: the contended path bumped only the anonymous blocked counter
//   and never raised `in_blocked_region`, so the production identity census
//   still counted the thread while it slept in `block_enter` — a pause that
//   started then waited for an arrival that could not come, and the
//   `BlockedGuard` drop on acquisition then waited for that pause (deadlock;
//   the take-over cannot excuse it, it is not in compiled code). A pre-STW
//   arrival under `ThreadId(0)` was either skipped (main is the initiator) or
//   filled main's quota slot.
//
// Now: a thread with a JNI binding that is a counted mutator takes the
// canonical GC-safe contended acquire every Java `monitorenter` uses
// (`monitor_enter_blocking`: pin, retire TLAB, deposit roots + raise the flag,
// block, remap); one already excluded from the census (an idle foreign-attached
// thread, a `host_thread_enter_native` thread) acquires without joining the
// census, its mark-word operation run with no pause in progress
// (`jni_monitor_enter_excluded`); an unbound caller is resolved by OS tid and
// never arrives under a guessed identity, and one the registry cannot name
// locks under an id of its own (`unnamed_caller_monitor_owner`).
pub(in crate::native::jni) extern "C" fn jni_monitor_enter(_env: JNIEnv, obj: JObject) -> JInt {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 {
        // HotSpot's `jni_MonitorEnter`: `THROW_(NullPointerException, JNI_ERR)`
        // (interpreter round i5 wave 101, lane L6).
        raise_jni_monitor_error("java/lang/NullPointerException", None);
        return JNI_ERR;
    }
    if jni_bound_thread_id().is_some() {
        return with_jni_context(|shared, thread| {
            let oref = jobject_to_obj(obj)?;
            jni_monitor_enter_as(shared, thread, obj, oref)?;
            Some(JNI_OK)
        })
        .flatten()
        .unwrap_or(JNI_ERR);
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        // No JNI binding (a registered thread whose binding a nested native
        // cleared, or a host thread). A registered thread running on this OS
        // thread still has its `JvmThread`: take the bound path with it, so a
        // pause that starts while it sleeps excludes it by identity
        // (gc-common w4-g, `common-w3a-jni-unbound-counted-monitor-enter` (1)).
        if let Some(thread) = unbound_caller_jvm_thread(shared) {
            // gci i11/ja (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
            // item 3): this pointer is the REGISTRY's published address, not
            // one installed from a live `&mut` (there is no binding: that is
            // why this arm runs), while a Rust frame up the stack -- the
            // builtin native whose C code called back here, e.g. an FFM
            // downcall -- may hold this thread's `&mut`. So no `&mut` is made
            // from it when none is needed: a caller already in its blocked
            // region (an idle or host-native excluded thread) acquires as an
            // excluded thread, which reads the flag and the id and writes
            // nothing.
            // SAFETY: `unbound_caller_jvm_thread` answers only the published
            // `JvmThread` of the one alive registry entry whose OS tid is THIS
            // thread's, i.e. the thread executing this call; it cannot be
            // dropped while its own thread runs. Only an atomic and the
            // immutable `Copy` id are read, through a shared reference.
            let (excluded, me) = {
                let view = unsafe { &*(thread as *const JvmThread) };
                (
                    view.gc_block_state
                        .in_blocked_region
                        .load(std::sync::atomic::Ordering::Acquire),
                    view.thread_id,
                )
            };
            if excluded {
                jni_monitor_enter_excluded(shared, obj, oref, me)?;
                return Some(JNI_OK);
            }
            // A COUNTED unbound caller must deposit and block GC-safe, which
            // writes the thread. Inside an FFM downcall (the C code that
            // called here; `ForeignJniEntry` above already left its region)
            // the downcall's own context holds the thread's `&mut`: borrow
            // the thread through the binding that context installs from it
            // (`NativeContext::run_with_jni_binding`), not from the registry.
            if let Some(done) = jni_monitor_enter_through_downcall(shared, me, obj, oref) {
                done?;
                return Some(JNI_OK);
            }
            // No downcall context on this thread (no known caller: every
            // other way C runs on a registered thread installs a binding):
            // the registry's `&mut`, as before.
            // SAFETY: as above; the thread is executing this call.
            jni_monitor_enter_as(shared, unsafe { &mut *thread }, obj, oref)?;
            return Some(JNI_OK);
        }
        // Otherwise: resolve the caller by its OS tid alone.
        let me = shared
            .threads
            .thread_registry
            .thread_id_for_current_os_tid();
        let excluded = me
            .and_then(|t| shared.threads.thread_registry.gc_block_state_of(t))
            .map(|s| {
                s.in_blocked_region
                    .load(std::sync::atomic::Ordering::Acquire)
            })
            .unwrap_or(false);
        // An unnamed caller (`me == None`) locks under an id of its own, never
        // `ThreadId(0)`: that is `main`'s real id, so a `MonitorEnter` on an
        // object `main` held thin was taken as `main`'s re-entry (two threads
        // inside one monitor) -- `r11w7-lock-jni-unresolved-caller-locks-as-main`.
        let owner = match me {
            Some(t) => t,
            None => unnamed_caller_monitor_owner(shared, true)?,
        };
        if excluded {
            jni_monitor_enter_excluded(shared, obj, oref, owner)?;
        } else if let Some(m) = shared.threads.monitors.enter_or_contend(oref, owner) {
            // Counted (or unknown) and without a `JvmThread` to deposit from.
            // A counted thread arrives under its REAL id; an unregistered one
            // (`me == None`) does not arrive at all — see
            // `jni_caller_thread_id` for why a placeholder is worse than a
            // hang. Reached now only by a caller with no published
            // `JvmThread` (see `unbound_caller_jvm_thread`; a mounted virtual
            // thread is resolved there since w6-g).
            let blk = shared.mem.gc_barrier.enter_blocked();
            if blk.pre_stw {
                if let Some(tid) = me {
                    let _ = shared.mem.gc_barrier.arrive_and_wait_auto(tid);
                }
            }
            m.block_enter(owner);
            drop(blk);
        }
        // Wave 95 (lane L6): a registered caller's enter is counted, as in
        // `jni_monitor_enter_as`; an unnamed owner has no registry entry.
        if let Some(me) = me {
            crate::vm::vm_exec::note_unstructured_monitor_enter(shared, me);
        }
        Some(JNI_OK)
    })
    .flatten()
    .unwrap_or(JNI_ERR)
}

/// `MonitorEnter` for a caller whose `JvmThread` is known: an excluded
/// (blocked-region) thread acquires without joining the census, a counted one
/// takes the canonical GC-safe contended acquire. `None`: `obj` (a global ref)
/// no longer resolves.
pub(in crate::native::jni) fn jni_monitor_enter_as(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: JObject,
    oref: ObjectRef,
) -> Option<()> {
    let entered = if thread
        .gc_block_state
        .in_blocked_region
        .load(std::sync::atomic::Ordering::Acquire)
    {
        // Already excluded from every census (and its roots are in its
        // deposited snapshot). Running the blocking protocol here would
        // CLEAR the flag on the way out (`check_post_block_gc`) and turn
        // an idle thread back into a counted mutator that is parked in
        // host code — the next pause would wait for it forever.
        jni_monitor_enter_excluded(shared, obj, oref, thread.thread_id)
    } else {
        let _ = crate::vm::vm_exec::monitor_enter_blocking(shared, thread, oref);
        Some(())
    };
    // Interpreter round i4 wave 95, lane L6
    // (`CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT`): HotSpot's `_jni_monitor_count`.
    if entered.is_some() {
        crate::vm::vm_exec::note_unstructured_monitor_enter(shared, thread.thread_id);
    }
    entered
}

/// gci i11/ja (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
/// item 3): [`jni_monitor_enter_as`] for an unbound caller `me` whose C code
/// an FFM downcall of `shared` runs: through the binding the downcall's own
/// context installs from its `&mut` (`NativeContext::run_with_jni_binding`),
/// so the thread is borrowed as a reborrow of that frame's borrow. `None`
/// (nothing done) when no downcall context of `shared` is active here, or the
/// binding names another thread; `Some(answer)` otherwise.
pub(in crate::native::jni) fn jni_monitor_enter_through_downcall(
    shared: &SharedVm,
    me: ThreadId,
    obj: JObject,
    oref: ObjectRef,
) -> Option<Option<()>> {
    let vm = shared.vm_identity;
    let mut taken: Option<Option<()>> = None;
    let _ = cratonvm_native_builtins::panama_libffi::with_active_context(|ctx| {
        if ctx.vm_identity() != vm {
            return;
        }
        ctx.run_with_jni_binding(&mut || {
            taken = with_jni_context(|shared, bound| {
                (bound.thread_id == me).then(|| jni_monitor_enter_as(shared, bound, obj, oref))
            })
            .flatten();
        });
    });
    taken
}

/// The `JvmThread` of an UNBOUND JNI caller that is a registered thread on
/// this OS thread, if exactly one alive entry claims this OS thread and it has
/// published its `JvmThread` address.
///
/// gc-common w4-g (`common-w3a-jni-unbound-counted-monitor-enter`, residual
/// 1): a registered, COUNTED thread whose JNI binding a nested native cleared
/// used to take the anonymous contended path in `MonitorEnter` -- no deposit,
/// no `in_blocked_region` -- so a pause that STARTED while it slept in
/// `block_enter` still counted it and waited until the monitor was granted (a
/// hang if the holder was parked at that pause). Its `JvmThread` is on this
/// very OS thread, published in the registry (`set_jvm_thread_addr`) for the
/// take-over; with it the caller takes the same GC-safe acquire a bound
/// caller does. `None` for an unregistered host thread and for a thread that
/// has not published its address.
///
/// gc-common w6-g (residual 2): an unbound MOUNTED virtual thread is resolved
/// too. Its carrier's OS tid is also claimed by every parked virtual thread
/// that last ran there; `thread_id_for_current_os_tid` now answers the
/// claimant with the newest publish on this OS thread, which is the mounted
/// one (a mount publishes before it runs Java), and its address is read by id
/// (`own_jvm_thread_addr`), so no parked thread's `JvmThread` is ever touched.
pub(in crate::native::jni) fn unbound_caller_jvm_thread(shared: &SharedVm) -> Option<*mut JvmThread> {
    let registry = &shared.threads.thread_registry;
    let me = registry.thread_id_for_current_os_tid()?;
    let thread = registry.own_jvm_thread_addr(me)? as *mut JvmThread;
    // SAFETY: `me` is the identity running on this OS thread (see above), so
    // its published, non-zero `jvm_thread_addr` points at the live `JvmThread`
    // executing this call; `thread_id` is an immutable `Copy` field.
    // Cross-checked so a stale publication of another id can never be adopted.
    (unsafe { (*thread).thread_id } == me).then_some(thread)
}

/// The calling OS thread's own `JvmThread` in `shared`, for a C JVMTI function
/// that asks about the current thread (interpreter round i1 wave 22, lane L1:
/// `GetCurrentThread`, `GetFrameCount`, `GetStackTrace`, `GetFrameLocation`,
/// `jvmti::native_env`): the one the JNI layer installed on this thread (a
/// native call, an event callback, an attached thread) when `shared`'s
/// registry publishes it under its own id, else the registered thread this OS
/// thread runs ([`unbound_caller_jvm_thread`]). `None` for a thread `shared`
/// does not know. Only the calling thread may dereference the answer, and
/// only for the duration of the call it is in.
pub(crate) fn current_jvm_thread_of(shared: &SharedVm) -> Option<*mut JvmThread> {
    let installed = JNI_THREAD
        .try_with(|c| c.get())
        .unwrap_or(std::ptr::null_mut()) as *mut JvmThread;
    if !installed.is_null() {
        // SAFETY: `JNI_THREAD` holds a pointer to the live `JvmThread` running
        // this call (see `set_jni_thread`); `thread_id` is an immutable `Copy`
        // field.
        let tid = unsafe { (*installed).thread_id };
        // Cast: the pointer's address, as the registry publishes it.
        if shared.threads.thread_registry.own_jvm_thread_addr(tid) == Some(installed as usize) {
            return Some(installed);
        }
    }
    unbound_caller_jvm_thread(shared)
}

/// gci i11/ja: the address this OS thread's JNI binding (`JNI_THREAD`) holds,
/// `None` when nothing is installed. The raw TLS value only: nothing is read
/// through it, so it is safe to ask while a Rust frame up the stack holds the
/// installed thread's `&mut` (a poster that installed its own handle before
/// an event). `try_with`: `None` on a thread whose TLS is being torn down.
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) fn installed_jni_thread_addr() -> Option<usize> {
    let p = JNI_THREAD
        .try_with(|c| c.get())
        .unwrap_or(std::ptr::null_mut());
    // Cast: the pointer's address, as the registry publishes it.
    (!p.is_null()).then_some(p as usize)
}

/// Acquire `obj` (resolved to `oref`) for a caller the STW census already
/// excludes (`in_blocked_region` raised). It never joins a census: no pause is
/// waiting for this thread and its roots are in its deposited snapshot.
///
/// The mark-word half runs inside `GcBarrier::with_no_pause_in_progress`
/// (`r11w4-sync-jni-excluded-monitor-ops-touch-a-movable-header`). Because
/// no pause waits for this thread, one could otherwise relocate the object
/// between the resolution and the CAS (or during an inflation), so the lock
/// would land on the vacated copy. Waiting a pause out is safe exactly
/// because this thread is excluded: no pause can be waiting for it. The
/// header operation does not spin while it holds the barrier lock. The
/// contended wait (`block_enter`, which touches no header) stays outside the
/// scope, as it always did.
///
/// `None`: `obj` is a global ref that no longer resolves.
pub(in crate::native::jni) fn jni_monitor_enter_excluded(
    shared: &SharedVm,
    obj: JObject,
    oref: ObjectRef,
    owner: ThreadId,
) -> Option<()> {
    let contended = shared.mem.gc_barrier.with_no_pause_in_progress(|| {
        let oref = excluded_caller_current_ref(obj, oref)?;
        Some(
            shared
                .threads
                .monitors
                .enter_or_contend_without_spin(oref, owner),
        )
    })?;
    if let Some(m) = contended {
        // Interpreter round i2 wave 54 (lane L6): a suspended thread gives
        // the monitor it just won back until its resume (HotSpot's
        // `ExitOnSuspend`); this caller is already in its blocked region.
        crate::vm::vm_exec::block_enter_suspension_aware(shared, owner, &m, None);
    }
    Some(())
}

/// What `obj` names NOW, for a census-excluded caller inside
/// `GcBarrier::with_no_pause_in_progress`. The caller resolved it to
/// `resolved` before the scope, and a collection may have run since. A
/// global ref is resolved again through its table, which that collection
/// remapped. A local ref is a raw address either way (nothing rewrites the
/// native's copy: `docs/feature-designs/real-jdk-mode.md`),
/// so it keeps its earlier validation. That validation takes heap locks,
/// which must not be taken under the barrier lock.
pub(in crate::native::jni) fn excluded_caller_current_ref(obj: JObject, resolved: ObjectRef) -> Option<ObjectRef> {
    // A `jclass` (an odd `ClassId` has bit 0 set) is NOT re-resolved here: its
    // decode takes the mirror-cache and class-manager locks, and a thread that
    // mints a mirror holds the cache's write lock across an allocation that
    // may wait on this barrier. It keeps its earlier resolution, like a local.
    if obj & 1 == 1 && !is_jclass_handle(obj) {
        jobject_to_obj(obj)
    } else {
        Some(resolved)
    }
}

/// Whether the JNI caller `me` is excluded from the STW census
/// (`in_blocked_region` raised): its bound `JvmThread`'s flag if a binding is
/// installed (what `MonitorEnter` tests), else its registry entry's.
pub(in crate::native::jni) fn jni_caller_is_census_excluded(shared: &SharedVm, me: ThreadId) -> bool {
    use std::sync::atomic::Ordering;
    let bound = JNI_THREAD
        .try_with(|c| c.get())
        .unwrap_or(std::ptr::null_mut());
    if !bound.is_null() {
        // SAFETY: as in `jni_bound_thread_id`: a non-null `JNI_THREAD` points at
        // a live `JvmThread` while installed. Only an atomic is read, through a
        // shared reference.
        let thread = unsafe { &*(bound as *const JvmThread) };
        return thread
            .gc_block_state
            .in_blocked_region
            .load(Ordering::Acquire);
    }
    shared
        .threads
        .thread_registry
        .gc_block_state_of(me)
        .is_some_and(|s| s.in_blocked_region.load(Ordering::Acquire))
}

thread_local! {
    /// This OS thread's monitor-owner id for JNI `MonitorEnter` /
    /// `MonitorExit` when the registry cannot name it, with the address of the
    /// `SharedVm` that issued it. See [`unnamed_caller_monitor_owner`].
    pub(in crate::native::jni) static JNI_UNNAMED_MONITOR_OWNER: Cell<Option<(usize, ThreadId)>> =
        const { Cell::new(None) };
}

/// The monitor owner for a JNI caller with no JNI binding whose OS thread the
/// registry cannot name: an unregistered host thread, a mounted virtual thread
/// sharing its carrier's OS tid, or any unbound caller on a platform with no
/// OS-tid backend.
///
/// Such a caller used `ThreadId(0)`, which is `main`'s real id: its enter on
/// an object `main` held thin became `main`'s re-entry, and its exit could
/// release `main`'s lock (`r11w7-lock-jni-unresolved-caller-locks-as-main`).
/// It now gets an id from the registry's own counter: never a registered
/// thread's id, below `u32::MAX` (so the lock stays in the mark word and
/// excludes Java threads), stable for this OS thread (so its `MonitorExit`
/// matches its `MonitorEnter`), and different for every other thread. The id
/// is NOT registered. It arrives at no barrier (as before) and publishes
/// nothing to JMX.
///
/// `issue == false` (`MonitorExit`): answer only an id this thread already
/// holds. `None` then means this thread never locked anything under one, so
/// there is nothing of its own to release. The cached id is tied to the
/// `SharedVm` that issued it (by `vm_identity`, which is never reused; it was
/// the VM's address until interpreter round i1 wave 23, which a later VM at
/// the same address inherited), and is dropped if that VM has since given it
/// to a live registered thread.
pub(in crate::native::jni) fn unnamed_caller_monitor_owner(shared: &SharedVm, issue: bool) -> Option<ThreadId> {
    let vm_key = shared.vm_identity;
    let registry = &shared.threads.thread_registry;
    JNI_UNNAMED_MONITOR_OWNER
        .try_with(|c| match c.get() {
            Some((key, tid)) if key == vm_key && !registry.is_alive(tid) => Some(tid),
            _ if issue => {
                let tid = registry.next_thread_id();
                c.set(Some((vm_key, tid)));
                Some(tid)
            }
            _ => None,
        })
        .ok()
        .flatten()
}

// ---- Index 218: MonitorExit ----
pub(in crate::native::jni) extern "C" fn jni_monitor_exit(_env: JNIEnv, obj: JObject) -> JInt {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 {
        // HotSpot's `jni_MonitorExit`: `THROW_(NullPointerException, JNI_ERR)`.
        raise_jni_monitor_error("java/lang/NullPointerException", None);
        return JNI_ERR;
    }
    let answer = with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        // The SAME identity `jni_monitor_enter` acquired under (ownership is
        // keyed by it). `monitor_exit_and_retract_jmx` also retracts the JMX
        // ownership publish `monitor_enter_blocking` made; for a caller that
        // never published it the retract is a no-op.
        let exited = match jni_caller_thread_id(shared) {
            // A census-excluded caller releases with no pause in progress, for
            // the reason `jni_monitor_enter_excluded` acquires that way: the
            // release CASes (or, inflated, reads) the mark word.
            Some(me) if jni_caller_is_census_excluded(shared, me) => {
                let exited = shared.mem.gc_barrier.with_no_pause_in_progress(|| {
                    let oref = excluded_caller_current_ref(obj, oref)?;
                    let exited = crate::vm::vm_exec::monitor_exit_and_retract_jmx(shared, oref, me);
                    Some(exited.is_ok())
                })?;
                // Wave 95 (lane L6): the count `MonitorEnter` raised.
                if exited {
                    crate::vm::vm_exec::note_unstructured_monitor_exit(shared, me);
                }
                exited
            }
            Some(me) => {
                let exited =
                    crate::vm::vm_exec::monitor_exit_and_retract_jmx(shared, oref, me).is_ok();
                if exited {
                    crate::vm::vm_exec::note_unstructured_monitor_exit(shared, me);
                }
                exited
            }
            // The id `jni_monitor_enter` locked under, never `main`'s
            // `ThreadId(0)`. No id: this thread holds nothing of its own.
            None => match unnamed_caller_monitor_owner(shared, false) {
                Some(owner) => shared.threads.monitors.exit(oref, owner).is_ok(),
                None => false,
            },
        };
        // Interpreter round i5 wave 101, lane L6
        // (`VmConfig::jni_monitor_exceptions`, `CRATONVM_JNI_MONITOR_EXCEPTIONS`,
        // default on): an exit of a monitor this caller does not own is
        // HotSpot's `ObjectSynchronizer::jni_exit` -> `check_owner`: `JNI_ERR`
        // with `IllegalMonitorStateException("current thread is not owner")`
        // pending. This answered `JNI_OK` with nothing pending.
        if !exited {
            return Some(None);
        }
        Some(Some(JNI_OK))
    })
    .flatten();
    match answer {
        Some(Some(code)) => code,
        Some(None) => {
            raise_jni_monitor_error(
                "java/lang/IllegalMonitorStateException",
                Some("current thread is not owner"),
            );
            JNI_ERR
        }
        None => JNI_ERR,
    }
}

/// Leave HotSpot's `jni_MonitorEnter` / `jni_MonitorExit` throwable pending
/// (interpreter round i5 wave 101, lane L6; `VmConfig::jni_monitor_exceptions`).
/// Only for a caller with a JNI context: an unattached caller gets the bare
/// `JNI_ERR` it got before rather than the unbuilt-throwable sentinel, which
/// would fail its native's return inside the VM.
pub(in crate::native::jni) fn raise_jni_monitor_error(class_name: &str, msg: Option<&str>) {
    let _ = with_jni_context(|shared, thread| {
        if let Ok(exc) =
            crate::runtime::exceptions::create_exception_object(shared, thread, class_name, msg)
        {
            set_jni_pending_exception_object(exc);
        }
    });
}

// ---- Index 219: GetJavaVM ----
pub(in crate::native::jni) extern "C" fn jni_get_java_vm(_env: JNIEnv, vm: *mut JavaVM) -> JInt {
    if vm.is_null() {
        return JNI_ERR;
    }
    let handle = JNI_SHARED_VM
        .with(|c| c.borrow().clone())
        .map(|shared| java_vm_for(&shared))
        .unwrap_or_else(get_java_vm);
    unsafe {
        *vm = handle;
    }
    JNI_OK
}

// ---- Index 228: ExceptionCheck ----
pub(in crate::native::jni) extern "C" fn jni_exception_check(_env: JNIEnv) -> JBoolean {
    // This returned `JNI_FALSE` unconditionally — a constant, not a check.
    //
    // `if ((*env)->ExceptionCheck(env)) { … }` is how essentially every JNI
    // library asks "did that up-call throw?", so a hard `false` did not fail
    // loudly: it made every error-handling branch in every native library
    // unreachable, and each caller went on to use a return value the JNI
    // spec leaves undefined once an exception is pending.
    //
    // The pending slot is the authority here rather than
    // `native_pending_return`, because the slot is what `ExceptionClear`
    // resets — a check that consulted the root instead would keep answering
    // `true` after the native had cleared.
    if JNI_PENDING_EXCEPTION.with(|cell| cell.get()) != 0 {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

// ---- Index 5: DefineClass ----
pub(in crate::native::jni) extern "C" fn jni_define_class(
    _env: JNIEnv,
    name: *const c_char,
    loader: JObject,
    buf: *const u8,
    len: JSize,
) -> JClass {
    let _fx = ForeignJniEntry::enter();
    // Two policies, chosen per VM (round 12 wave 6, lane jni;
    // `r12w5-tier3-jni-defineclass-ignores-the-loader-FIXED-20260926.md`):
    //
    // * `--real-jdk`: the define goes through `ClassLoader.defineClass1`'s
    //   registered native with the caller's `loader`, the backend every Java
    //   define uses ([`jni_define_class_through_loader`]): the loader's
    //   namespace, the defining-loader record, the transform chain, the
    //   metaspace check, and HotSpot's exception types.
    // * `--compatible` (and the kill switch): the legacy define below,
    //   byte-for-byte: every class lands in the application namespace and
    //   every failure is a `NoClassDefFoundError`.
    let through_loader = jni_define_class_through_loader_active();
    // Per JNI, `name` may be NULL (the name is then taken from the class file);
    // when supplied it is the expected binary name. The bytecode buffer is
    // mandatory: a NULL/empty/negative-length buffer is a hard error.
    if buf.is_null() || len <= 0 {
        if through_loader {
            // HotSpot parses the empty stream and fails with this.
            raise_jni_throwable("java/lang/ClassFormatError", "Truncated class file");
        } else {
            raise_jni_no_class_def_found("DefineClass called with a null or empty bytecode buffer");
        }
        return 0;
    }
    // The class name may be NULL (JNI allows deriving it from the class file).
    let class_name = unsafe { cstr_to_str(name) }.map(|s| s.replace('.', "/"));

    // SAFETY: the caller guarantees `buf` points to `len` readable bytes for
    // the duration of the call (standard JNI DefineClass contract). We copy the
    // bytes out immediately so the slice does not outlive this borrow.
    let bytes: Vec<u8> = unsafe { std::slice::from_raw_parts(buf, len as usize) }.to_vec();

    if through_loader {
        // `None`: no attached `JvmThread` to run the define on; the legacy
        // define below is all such a caller could ever get.
        if let Some(defined) =
            jni_define_class_through_loader(class_name.as_deref(), loader, &bytes)
        {
            return defined;
        }
    }

    // Legacy define (`--compatible`): define the class from the caller-supplied
    // `buf[..len]` bytecode via the same `define_class` path the interpreter
    // uses for `ClassLoader.defineClass` / agent retransform, so JNI/agent code
    // that synthesises classes at runtime gets the bytes it actually passed —
    // not a same-named class loaded from the classpath.
    let result = with_shared_vm(|shared| {
        // If the caller did not supply a name, the class manager will derive it
        // from the class file's `this_class` entry; use the empty string as a
        // placeholder that `define_class` overrides from the bytes.
        let define_name = class_name.as_deref().unwrap_or("");
        let mut cm = shared.classes.class_manager_write();
        let next_id_before = cm.class_store.next_id();
        let cid = cm
            .define_class(
                define_name,
                &bytes,
                cratonvm_types::ClassLoaderId::Application,
            )
            .ok()?;
        drop(cm);
        // JIT round 21 wave 17 (lane finalize-reinit, item 3 of
        // `docs/feature-designs/real-jdk-mode.md`): the
        // legacy define runs the finalizable-define listener too, as
        // `define_class_full` does, after the class-manager guard is gone.
        shared.note_finalizable_define(cid);
        // j27 wave 2 (lane REDEFINE): and the class-hierarchy listener.
        shared.note_java_loader_subtype_define(cid);
        // Mirror the interpreter's defineClass path: invalidate any JIT code
        // that may have inlined from a previously-loaded class of this name so
        // a redefinition is honoured rather than served stale. Only a define
        // that can have replaced one does (round 12 wave 5,
        // `define_withdraws_by_name`).
        let withdraws =
            crate::vm::realms::jit_realm::define_withdraws_by_name(cid, next_id_before, false);
        if let Some(n) = class_name.as_deref().filter(|_| withdraws) {
            let _ = shared.jit.jit_cache.write().invalidate_for_class(n);
            // r10 lane `tier2`: the profile store is keyed by (class_id, method,
            // descriptor) and `class_id` is STABLE across a redefine, so without this the
            // new bytecode's execution blends into branch ratios and receiver tables
            // gathered from the OLD bytecode. The unload path in `memory/gc.rs` has always
            // invalidated both; redefine invalidated only the tiering verdicts.
            shared.jit.profile_store.invalidate_class(cid.as_u32());
            shared.jit.tiered_manager.on_class_redefined(cid, n);
        }
        Some(class_id_to_jclass(cid))
    })
    .flatten();

    match result {
        Some(c) => c,
        None => {
            // Defining from the supplied bytes failed (malformed class file,
            // linkage error, or no VM context). Surface it as a real Java
            // exception instead of silently substituting a classpath class.
            let label = class_name.as_deref().unwrap_or("<unnamed>");
            raise_jni_no_class_def_found(&format!(
                "DefineClass failed to define class {label} from the supplied bytecode"
            ));
            0
        }
    }
}

/// Does this VM's `DefineClass` go through the caller's loader
/// ([`jni_define_class_through_loader`])? In both modes: the owner turned it
/// on for `--compatible` too (2026-09-27); the kill switch below gives either
/// mode the legacy define back.
pub(in crate::native::jni) fn jni_define_class_through_loader_active() -> bool {
    jni_define_class_loader_switch()
}

/// `CRATONVM_JNI_DEFINECLASS_LOADER`, latched like every flag. Default ON;
/// `0` / `false` / `off` / `no` gives `--real-jdk` the legacy define too.
pub(in crate::native::jni) fn jni_define_class_loader_switch() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
                .as_deref()
                .map(str::trim),
            Ok("0" | "false" | "off" | "no")
        )
    })
}

/// What [`jni_define_class_through_loader`]'s up-call came back with.
pub(in crate::native::jni) enum JniDefineOutcome {
    Defined(ClassId),
    /// A Java argument could not be allocated; `OutOfMemoryError` is raised
    /// outside the thread borrow (`raise_jni_oom` takes it itself).
    OutOfMemory(&'static str),
    /// The define failed; whatever it threw is already pending.
    Failed,
}

/// JNI `DefineClass` as HotSpot's `jni_DefineClass` does it: the class is
/// defined BY `loader` (null = the bootstrap loader, which the native models
/// as namespace 0 as it does for every Java caller passing null). The define
/// is `ClassLoader.defineClass1(loader, name, bytes, 0, len, null, null)` run
/// through the registered native, so it is exactly the backend
/// `ClassLoader.defineClass` uses: the loader's namespace
/// (`loader_namespace_id`), its supertypes linked through that loader, the
/// defining-loader record and the mirror's `classLoader` field, the
/// transform-on-load chain, the metaspace limit, the JIT withdrawal
/// (`define_withdraws_by_name`), and typed failures -- `ClassFormatError`,
/// `NoClassDefFoundError` for a name mismatch, `LinkageError` for a duplicate
/// in the same loader, `SecurityException` for `java.*`.
///
/// `None` when this OS thread has no attached `JvmThread` to run it on.
pub(in crate::native::jni) fn jni_define_class_through_loader(
    class_name: Option<&str>,
    loader: JObject,
    bytes: &[u8],
) -> Option<JClass> {
    const DEFINE_CLASS1_DESC: &str = "(Ljava/lang/ClassLoader;Ljava/lang/String;[BIILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;";
    // A class file longer than an `int` cannot be a `byte[]`.
    let len = i32::try_from(bytes.len()).ok()?;
    let _fg = ForeignCallGuard::enter();
    let outcome = with_jni_context(|shared, thread| {
        let Ok(class_loader) = shared.load_class_concurrent("java/lang/ClassLoader") else {
            return JniDefineOutcome::Failed;
        };
        // Both arguments are allocated without a collection (JNI's rule: the
        // native holds raw references), and the up-call roots its arguments.
        let name_arg = match class_name {
            Some(n) => match crate::vm::try_create_java_string_uninterned(shared, n) {
                Some(s) => Value::Object(Some(s)),
                None => return JniDefineOutcome::OutOfMemory("a java/lang/String"),
            },
            None => Value::Object(None),
        };
        let Some(array) = shared.mem.heap.try_alloc_array_full(
            ClassId::new(0),
            ArrayElementType::Byte,
            bytes.len(),
        ) else {
            return JniDefineOutcome::OutOfMemory("a byte array");
        };
        for (i, &b) in bytes.iter().enumerate() {
            let _ = shared
                .mem
                .heap
                .set_array_element(array, i, Value::Int(b as i8 as i32));
        }
        // Decoded last, after the allocations, as every JNI function decodes
        // its handles before it touches the heap.
        let loader_arg = Value::Object(if loader == 0 {
            None
        } else {
            jobject_to_obj(loader)
        });
        let args = [
            loader_arg,
            name_arg,
            Value::Object(Some(array)),
            Value::Int(0),
            Value::Int(len),
            Value::Object(None),
            Value::Object(None),
        ];
        // `no_retarget`: `defineClass1` is static; its first argument is the
        // loader, not a receiver to dispatch on.
        //
        // zt t16/pl7: HotSpot's `jni_DefineClass` defines through
        // `SystemDictionary::resolve_from_stream`, not
        // `jvm_define_class_common`, so `sun.cls.defineAppClasses` /
        // `appClassBytes` do not count it: the native's count skips this
        // define (`perf_class_trace::take_define_uncounted`). The mark is
        // cleared after the up-call in case the native never reached its count.
        cratonvm_types::perf_class_trace::set_define_uncounted(true);
        let result = crate::vm::invoke_on_class_shared_no_retarget(
            shared,
            thread,
            class_loader,
            "defineClass1",
            DEFINE_CLASS1_DESC,
            &args,
        );
        cratonvm_types::perf_class_trace::set_define_uncounted(false);
        match result {
            Ok(Some(Value::Object(Some(mirror)))) => {
                match crate::vm::class_id_from_mirror(shared, mirror) {
                    Some(cid) => JniDefineOutcome::Defined(cid),
                    None => JniDefineOutcome::Failed,
                }
            }
            Ok(_) => JniDefineOutcome::Failed,
            Err(err) => {
                // The native answers typed VM errors; JNI hands the native a
                // pending throwable, as the interpreter's invoke arms do
                // (`classify_fastpath_invoke_error`). Converted here whatever
                // the up-call switch says: this path is `--real-jdk`'s own.
                // Wave 99 (lane L6): a VM error, before it becomes a
                // throwable, as `jni_surface_jdk_only` records it.
                if matches!(err, crate::error::MethodCallFailed::InternalError(_)) {
                    thread.vm_error_upcall_seen = true;
                }
                let err = jni_vm_error_as_throwable(shared, thread, err);
                let _ = jni_surface_jdk_only(shared, thread, Err(err));
                JniDefineOutcome::Failed
            }
        }
    })?;
    Some(match outcome {
        JniDefineOutcome::Defined(cid) => class_id_to_jclass(cid),
        JniDefineOutcome::OutOfMemory(what) => {
            let _ = with_shared_vm(|shared| raise_jni_oom(shared, what));
            0
        }
        JniDefineOutcome::Failed => {
            // Never a bare NULL: an error the native could not type (an
            // internal VM failure) still leaves something pending.
            if JNI_PENDING_EXCEPTION.with(|cell| cell.get()) == 0 {
                let label = class_name.unwrap_or("<unnamed>");
                raise_jni_no_class_def_found(&format!(
                    "DefineClass failed to define class {label} from the supplied bytecode"
                ));
            }
            0
        }
    })
}

/// Allocate a JNI-requested object, or raise `OutOfMemoryError` and return
/// `None` -- JNI's contract for an allocating function that fails is `NULL`
/// with a pending `OutOfMemoryError`.
///
/// gc-common w5-g (`handoff-w4c-infallible-callers-residue` §2): these sites
/// used the infallible `alloc_object` / `alloc_array`, which ABORT the process
/// on G1 when the heap is exhausted (`FATAL: G1 infallible alloc_object`), so
/// a native library asking for one array too many killed the VM instead of
/// seeing a catchable error. The fallible twins take the same no-collection
/// path, so a successful allocation is unchanged. By default no collection is
/// attempted here: native code may hold raw local references across the call.
///
/// gcf f1/o (2026-10-01), `CRATONVM_GC_JNI_ALLOC_LADDER` (default ON since gcf 2026-10-01, `=0` kills)
/// (`gce-e2j-zgc-default-jni-cost-probe-oome-flake`): a refused allocation runs
/// the shared native ladder (`native_reclaim_before_alloc_retry`: the forced
/// collection, the overhead-limit rungs, the last-ditch and second majors) and
/// retries ONCE before raising -- what HotSpot does, whose JNI allocation is an
/// ordinary VM allocation that collects. Without it a native that allocates in
/// a loop inside ONE call (`Gcd1JniCostProbe`'s `new-string`: 200 000
/// `NewStringUTF`) depends entirely on some other thread to collect: the
/// funnel's pressure relief runs only at the NEXT native boundary, so on G1 and
/// ZGC, whose young budget runs out long before the heap does, the call dies of
/// `OutOfMemoryError` with a heap of garbage whenever their background cycle
/// falls behind (host load).
///
/// Only when a collection cannot leave a stale address in the native's own
/// variables ([`jni_alloc_may_collect`]); otherwise the historical answer.
/// Every `alloc` closure here captures ids, lengths and Rust-side data only
/// (no `ObjectRef`), so re-running it after a collection is sound.
pub(in crate::native::jni) fn jni_alloc_or_oom(
    shared: &SharedVm,
    what: &str,
    alloc: impl FnMut(&crate::memory::vm_heap::VmHeap) -> Option<ObjectRef>,
) -> Option<ObjectRef> {
    jni_alloc_or_oom_sized(shared, what, 0, alloc)
}

/// gci i13/oa item 4: [`jni_alloc_or_oom`] for a request of `bytes` (header
/// plus data; `0` = unknown), so the collection a G1 humongous
/// `NewObjectArray` / `New<Type>Array` asks for runs under HotSpot's
/// `G1 Humongous Allocation` (`native_reclaim_before_alloc_retry_graced_for`).
pub(in crate::native::jni) fn jni_alloc_or_oom_sized(
    shared: &SharedVm,
    what: &str,
    bytes: usize,
    mut alloc: impl FnMut(&crate::memory::vm_heap::VmHeap) -> Option<ObjectRef>,
) -> Option<ObjectRef> {
    if let Some(obj) = alloc(&shared.mem.heap) {
        return Some(obj);
    }
    if jni_alloc_may_collect() {
        use crate::runtime::interpreter::gc_and_alloc::{
            note_ladder_door_allocated, note_ladder_door_entered, LadderDoor,
        };
        note_ladder_door_entered(shared, LadderDoor::Jni);
        let verdict = with_jni_context(|s, thread| {
            crate::runtime::interpreter::gc_and_alloc::native_reclaim_before_alloc_retry_graced_for(
                s, thread, bytes,
            )
        });
        if let Some(verdict) = verdict.filter(|v| v.retry) {
            if let Some(obj) = alloc(&shared.mem.heap) {
                // gcf f2/o: a graced retry that allocated is counted
                // (`ladder_latched_grace_ok=`).
                if verdict.graced {
                    crate::runtime::interpreter::gc_and_alloc::note_latched_grace_outcome(
                        shared, true,
                    );
                }
                note_ladder_door_allocated(shared, LadderDoor::Jni);
                with_jni_context(|s, thread| {
                    crate::runtime::interpreter::gc_and_alloc::zgc_native_room_skip_served(
                        s, thread,
                    )
                });
                return Some(obj);
            }
            // zr z8/nr: the retry failed after a ZGC ladder that skipped its
            // clear-soft rung (its collection had made room): pay the rung,
            // then one more attempt before the error.
            let repaid = with_jni_context(|s, thread| {
                crate::runtime::interpreter::gc_and_alloc::zgc_native_repay_skipped_soft_rung(
                    s, thread,
                )
            });
            if repaid == Some(true) {
                if let Some(obj) = alloc(&shared.mem.heap) {
                    note_ladder_door_allocated(shared, LadderDoor::Jni);
                    return Some(obj);
                }
            }
        }
    }
    raise_jni_oom(shared, what);
    None
}

/// gcf f1/o: the detail message of a refused JNI allocation's
/// `OutOfMemoryError`, HotSpot's (see `raise_jni_oom`).
pub(in crate::native::jni) const JNI_HEAP_OOME_MESSAGE: &str = "Java heap space";

/// Refuse an impossible JNI array length before entering any collection door.
/// All collectors use HotSpot's VM-limit message and the normal OOME hook.
/// The per-VM `CRATONVM_JNI_ARRAY_VM_LIMIT=0` diagnostic kill retains the
/// historical heap refusal for positive controls.
pub(in crate::native::jni) fn jni_array_refused_for_its_length(shared: &SharedVm, length: JSize, what: &str) -> bool {
    let applies = jni_switches_of(shared).array_vm_limit;
    if !jni_array_length_refusal(length, applies) {
        return false;
    }
    raise_jni_oom_message(
        shared,
        what,
        crate::runtime::interpreter::gc_and_alloc::ARRAY_SIZE_EXCEEDS_VM_LIMIT,
    );
    true
}

/// gcj j1/jn: the rule of [`jni_array_refused_for_its_length`], pure.
pub(in crate::native::jni) fn jni_array_length_refusal(length: JSize, applies: bool) -> bool {
    applies
        && usize::try_from(length).is_ok_and(|n| {
            crate::runtime::interpreter::gc_and_alloc::array_length_exceeds_vm_limit(n)
        })
}



/// gcf f1/o: may a refused JNI allocation collect before its retry? Only with
/// the switch on, and only when no copy the native holds can go stale:
///
/// * the native's locals are indirect handles that never escaped raw
///   (`!local_refs_may_be_held_raw()`, the JNI package), or
/// * every open local frame on this thread holds no live object local at all
///   ([`jni_frames_hold_no_object_local`]) -- with raw locals (the default),
///   a native that has deleted its locals holds no reference it may legally
///   use, so a move cannot hurt it.
///
/// Foreign attachments use [`foreign_attached_alloc_may_collect`]: recorded
/// raw locals are pinned, and indirect locals can be rewritten.
/// A `jclass` is a tagged class id and a global ref an indirect handle, so
/// neither can go stale; `Get*ArrayElements` / `*Critical` hand out copies.
pub(in crate::native::jni) fn jni_alloc_may_collect() -> bool {
    if is_foreign_attached() {
        return foreign_attached_alloc_may_collect();
    }
    !local_refs_may_be_held_raw() || jni_frames_hold_no_object_local()
}

/// gcj j1/jn (`gcj-j1jn-foreign-attached-jni-allocation-never-collects-20261019`):
/// `CRATONVM_JNI_FOREIGN_ALLOC_LADDER`, read once per VM
/// ([`JniSwitches::foreign_alloc_ladder`]). WARNING flip (gcj j2, 2026-10-19):
/// default ON (`=0` kills), on every collector.
/// See [`foreign_attached_alloc_may_collect`].
pub(in crate::native::jni) fn foreign_alloc_ladder_enabled() -> bool {
    true
}

/// A foreign attachment may collect while counted in its own VM, on every
/// collector. Raw handles are recorded in its always-open attachment frame;
/// the owner's root scan publishes them as pins. Generational majors sweep
/// instead of sliding while a raw attachment is alive. An idle attachment
/// with `in_blocked_region` raised cannot initiate a collection.
pub(in crate::native::jni) fn foreign_attached_alloc_may_collect() -> bool {
    let admitted = with_shared_vm(|shared| {
        jni_switches_of(shared).foreign_alloc_ladder
    })
    .unwrap_or(false);
    if !admitted {
        return false;
    }
    FOREIGN_THREAD_BOX
        .try_with(|c| {
            c.try_borrow().ok().and_then(|slot| {
                slot.as_ref().map(|jt| {
                    !jt.gc_block_state
                        .in_blocked_region
                        .load(std::sync::atomic::Ordering::Acquire)
                })
            })
        })
        .ok()
        .flatten()
        .unwrap_or(false)
}

/// gcf f1/o: is there at least one open JNI local frame on this thread, and
/// does no frame hold a live raw object local? The same entry filter as
/// `for_each_live_local_ref`. `false` (refuse) when no frame is open: a
/// handle minted then is handed out raw and never recorded.
pub(in crate::native::jni) fn jni_frames_hold_no_object_local() -> bool {
    JNI_LOCAL_FRAMES
        .try_with(|frames| {
            let Ok(stack) = frames.try_borrow() else {
                return false;
            };
            !stack.is_empty()
                && stack
                    .iter()
                    .all(|frame| frame.iter().all(|&h| jni_local_entry_is_not_an_object(h)))
        })
        .unwrap_or(false)
}

/// gcf f1/o: a local-frame table entry that names no raw heap object (empty,
/// odd-tagged, a `jclass`, or an indirect tag).
pub(in crate::native::jni) fn jni_local_entry_is_not_an_object(h: JObject) -> bool {
    h == 0 || h & 1 == 1 || is_jclass_handle(h) || is_indirect_local_tag(h)
}



/// Set a pending `OutOfMemoryError` for the current JNI call. Built without a
/// collection (the allocation that failed already took the no-GC path, and a
/// native frame holds raw references); if even that cannot be allocated, the
/// VM's preallocated OOME is thrown, and failing that the sentinel is set so
/// the failure is never silently a plain `NULL`.
///
/// gcf f1/o (2026-10-01): the Java-visible message is HotSpot's, `Java heap
/// space` (a JNI allocation is an ordinary heap allocation there); it used to
/// read `Java heap space: failed to allocate <what> in JNI`, which no other
/// tier shows (`hotspot_oome_detail` strips only the ` (<site>)` form). The
/// detail stays on the `tracing` channel. And the OOME chokepoint every other
/// heap-OOME door passes (`maybe_dump_heap_on_oom_for`) runs here too:
/// `-XX:+HeapDumpOnOutOfMemoryError`, `-XX:OnOutOfMemoryError`,
/// `-XX:+ExitOnOutOfMemoryError` / `CrashOnOutOfMemoryError` and the native
/// funnel's post-OOME debt (`note_heap_oome_raised`) were all skipped for a
/// JNI allocation.
pub(in crate::native::jni) fn raise_jni_oom(shared: &SharedVm, what: &str) {
    raise_jni_oom_message(shared, what, JNI_HEAP_OOME_MESSAGE);
}

/// [`raise_jni_oom`] with the detail message `msg` (gcj j1/jn: HotSpot's
/// `Requested array size exceeds VM limit` for a length refusal,
/// [`jni_array_refused_for_its_length`]; `Java heap space` otherwise). The
/// chokepoint (`maybe_dump_heap_on_oom_for`) runs with the same message, as
/// HotSpot's `report_java_out_of_memory` does for both.
pub(in crate::native::jni) fn raise_jni_oom_message(shared: &SharedVm, what: &str, msg: &str) {
    tracing::debug!(target: "cratonvm::jni", "JNI allocation refused: {what} ({msg})");
    let raised = with_jni_context(|ctx_shared, thread| {
        crate::runtime::interpreter::maybe_dump_heap_on_oom_for(ctx_shared, thread, msg);
        // Preserve VM OOME events, immutable shape and debt bookkeeping, but
        // never collect to construct an impossible-length error, on any heap.
        let built = if msg == crate::runtime::interpreter::gc_and_alloc::ARRAY_SIZE_EXCEEDS_VM_LIMIT {
            crate::runtime::exceptions::create_vm_oome_object_no_collect(
                ctx_shared,
                thread,
                "java/lang/OutOfMemoryError",
                Some(msg),
            )
        } else {
            crate::runtime::exceptions::create_vm_oome_object(
                ctx_shared,
                thread,
                "java/lang/OutOfMemoryError",
                Some(msg),
            )
        };
        match built {
            Ok(exc) => {
                set_jni_pending_exception_object(exc);
                true
            }
            Err(_) => false,
        }
    })
    .unwrap_or(false);
    if raised {
        return;
    }
    // Keep the requested message under heap exhaustion on every collector.
    let fallback = crate::runtime::exceptions::preallocated_oome_for(shared, msg);
    match fallback {
        Some(oom) => set_jni_pending_exception_object(oom),
        None => JNI_PENDING_EXCEPTION.with(|cell| cell.set(u64::MAX)),
    }
}

/// Raise a `NoClassDefFoundError` on the current thread so a failed
/// `DefineClass` surfaces as a real Java exception rather than a fabricated
/// null/0 return. Mirrors [`raise_jni_region_error`].
pub(in crate::native::jni) fn raise_jni_no_class_def_found(msg: &str) {
    raise_jni_error("java/lang/NoClassDefFoundError", msg);
}

/// Leave a `class_name(msg)` throwable pending for the calling native (the
/// `FindClass` refusal pattern, shared with `GetMethodID`'s miss).
pub(in crate::native::jni) fn raise_jni_error(class_name: &str, msg: &str) {
    let raised =
        with_jni_context(
            |shared, thread| match crate::runtime::exceptions::create_exception_object(
                shared,
                thread,
                class_name,
                Some(msg),
            ) {
                Ok(exc) => {
                    set_jni_pending_exception_object(exc);
                    Ok(())
                }
                Err(e) => Err(is_heap_exhaustion(&e)),
            },
        )
        .unwrap_or(Err(false));
    if let Err(heap_exhausted) = raised {
        // No thread context or allocation failed: see `pend_unbuilt_throwable`.
        pend_unbuilt_throwable(heap_exhausted);
    }
}

// ---- Index 7: FromReflectedMethod ----
// Convert a java.lang.reflect.Method/Constructor to a JMethodID.
pub(in crate::native::jni) extern "C" fn jni_from_reflected_method(_env: JNIEnv, method: JObject) -> JMethodID {
    let _fx = ForeignJniEntry::enter();
    if method == 0 {
        return 0;
    }
    // gcf f2/j (`gcf-f1j-jni-to-reflected-method-builds-a-hollow-method`):
    // the member a REAL `Method` / `Constructor` names -- its `clazz`, `name`
    // and the descriptor of its `parameterTypes` / `returnType` -- resolved
    // exactly as `GetMethodID` resolves it in the declaring class (HotSpot's
    // `jni_FromReflectedMethod` reads the holder and `slot`). This read
    // absolute slots 0 and 1 of the object as `(ClassId, index)`: inherited
    // fields of another type, so NULL or a wrong method.
    let Some(Some(member)) = with_jni_context(|shared, thread| {
        let obj = jobject_to_obj(method)?;
        reflected_member_of(shared, thread, obj)
    }) else {
        return 0;
    };
    if member.kind == ReflectedKind::Field {
        return 0;
    }
    jni_method_id_lookup(
        class_id_to_jclass(member.declaring),
        &member.name,
        &member.descriptor,
    )
    .map_or(0, |(mid, _, _)| mid)
}

// ---- Index 8: FromReflectedField ----
// Convert a java.lang.reflect.Field to a JFieldID.
pub(in crate::native::jni) extern "C" fn jni_from_reflected_field(_env: JNIEnv, field: JObject) -> JFieldID {
    let _fx = ForeignJniEntry::enter();
    if field == 0 {
        return 0;
    }
    // gcf f2/j: as `jni_from_reflected_method`, through `GetFieldID`'s own
    // resolution (an instance field's absolute slot, a static field's index
    // among its class's statics).
    let Some(Some(member)) = with_jni_context(|shared, thread| {
        let obj = jobject_to_obj(field)?;
        reflected_member_of(shared, thread, obj)
    }) else {
        return 0;
    };
    if member.kind != ReflectedKind::Field {
        return 0;
    }
    let (Ok(name), Ok(sig)) = (
        std::ffi::CString::new(member.name),
        std::ffi::CString::new(member.descriptor),
    ) else {
        return 0;
    };
    jni_field_id(
        class_id_to_jclass(member.declaring),
        name.as_ptr(),
        sig.as_ptr(),
        member.is_static,
    )
}

/// gcf f2/j: which `java.lang.reflect` member class a reflection object is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::native::jni) enum ReflectedKind {
    Method,
    Constructor,
    Field,
}

/// gcf f2/j: the member a `Method` / `Constructor` / `Field` object names:
/// its declaring class, its name (`<init>` for a constructor) and its JVM
/// descriptor, read from the object's JDK fields BY NAME (`clazz`, `name`,
/// `parameterTypes`, `returnType`, `type`), which every builder fills -- the
/// VM's (`native-builtins` `create_method_object`) and the JDK's own copies
/// (`Method.copy`). HotSpot keys on `slot`; this VM leaves `slot` 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::native::jni) struct ReflectedMember {
    pub(in crate::native::jni) kind: ReflectedKind,
    pub(in crate::native::jni) declaring: ClassId,
    pub(in crate::native::jni) name: String,
    pub(in crate::native::jni) descriptor: String,
    /// `modifiers` has `ACC_STATIC` (which field id encoding a `Field` needs).
    pub(in crate::native::jni) is_static: bool,
}

/// gcf f2/j: see [`ReflectedMember`]. `None` for any other object, or when a
/// field the read needs is missing. Allocates nothing on the heap (a caller
/// may hold raw references across it).
pub(in crate::native::jni) fn reflected_member_of(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: ObjectRef,
) -> Option<ReflectedMember> {
    let kind = {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(shared.mem.heap.class_id_of(obj))?;
        match &*class.name {
            "java/lang/reflect/Method" => ReflectedKind::Method,
            "java/lang/reflect/Constructor" => ReflectedKind::Constructor,
            "java/lang/reflect/Field" => ReflectedKind::Field,
            _ => return None,
        }
    };
    let ctx_impl = crate::vm::NativeContextImpl { shared, thread };
    let ctx: &dyn cratonvm_native_api::NativeContext = &ctx_impl;
    let object_field = |name: &str| match ctx.get_field_by_name(obj, name) {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let declaring = ctx.class_id_from_mirror(object_field("clazz")?)?;
    let name = match kind {
        ReflectedKind::Constructor => "<init>".to_string(),
        _ => ctx.read_string(object_field("name")?)?,
    };
    let descriptor = match kind {
        ReflectedKind::Field => mirror_descriptor_token(ctx, object_field("type")?)?,
        _ => {
            let params = object_field("parameterTypes")?;
            let mut d = String::from("(");
            for i in 0..ctx.array_length(params) {
                match ctx.get_array_element(params, i) {
                    Value::Object(Some(m)) => d.push_str(&mirror_descriptor_token(ctx, m)?),
                    _ => return None,
                }
            }
            d.push(')');
            match kind {
                ReflectedKind::Constructor => d.push('V'),
                _ => d.push_str(&mirror_descriptor_token(ctx, object_field("returnType")?)?),
            }
            d
        }
    };
    let is_static = matches!(
        ctx.get_field_by_name(obj, "modifiers"),
        Value::Int(m) if m & 0x0008 != 0
    );
    Some(ReflectedMember {
        kind,
        declaring,
        name,
        descriptor,
        is_static,
    })
}

/// gcf f2/j: the JVM descriptor token of the class a `java.lang.Class`
/// mirror stands for (`I`, `[J`, `Ljava/lang/String;`). A primitive mirror
/// has no `ClassId`; its `name` field names it.
pub(in crate::native::jni) fn mirror_descriptor_token(
    ctx: &dyn cratonvm_native_api::NativeContext,
    mirror: ObjectRef,
) -> Option<String> {
    let name = match ctx.class_id_from_mirror(mirror) {
        Some(cid) => ctx.class_name_of_id(cid)?,
        None => match ctx.get_field_by_name(mirror, "name") {
            Value::Object(Some(s)) => ctx.read_string(s)?,
            _ => return None,
        },
    };
    Some(descriptor_token_of_class_name(&name))
}

/// gcf f2/j: `int` -> `I`, `[I` -> `[I`, `java/lang/String` (or the binary
/// `java.lang.String`) -> `Ljava/lang/String;`.
pub(in crate::native::jni) fn descriptor_token_of_class_name(name: &str) -> String {
    let internal = name.replace('.', "/");
    let prim = match internal.as_str() {
        "void" => "V",
        "boolean" => "Z",
        "byte" => "B",
        "char" => "C",
        "short" => "S",
        "int" => "I",
        "long" => "J",
        "float" => "F",
        "double" => "D",
        _ if internal.starts_with('[') => return internal,
        _ => return format!("L{internal};"),
    };
    prim.to_string()
}

/// gcf f2/j: `ToReflectedMethod` / `ToReflectedField`: the reflection object
/// Java reflection itself answers for the member `(declaring, name,
/// descriptor)` -- HotSpot's `Reflection::new_method` / `new_constructor` /
/// `new_field` build exactly what `getDeclared*` would. Asked of the
/// declaring class's mirror (`getDeclaredMethods` / `getDeclaredConstructors`
/// / `getDeclaredFields`, a fresh copy each), then picked by name and
/// descriptor, so the object is fully built and invokable in both modes. A
/// local reference, or 0 (the member is filtered from reflection, or the
/// up-call threw: its exception stays pending, as HotSpot's would).
pub(in crate::native::jni) fn to_reflected_member(
    declaring: ClassId,
    kind: ReflectedKind,
    name: &str,
    descriptor: &str,
) -> JObject {
    let (getter, getter_desc) = match kind {
        ReflectedKind::Method => ("getDeclaredMethods", "()[Ljava/lang/reflect/Method;"),
        ReflectedKind::Constructor => (
            "getDeclaredConstructors",
            "()[Ljava/lang/reflect/Constructor;",
        ),
        ReflectedKind::Field => ("getDeclaredFields", "()[Ljava/lang/reflect/Field;"),
    };
    let Some(mirror) = jobject_to_obj(class_id_to_jclass(declaring)) else {
        return 0;
    };
    with_jni_context(|shared, thread| {
        let class_class = shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id("java/lang/Class")?;
        // The mirror is the up-call's receiver, rooted by its frame.
        let arr = match invoke_on_class_shared(
            shared,
            thread,
            class_class,
            getter,
            getter_desc,
            &[Value::Object(Some(mirror))],
        ) {
            Ok(Some(Value::Object(Some(arr)))) => arr,
            _ => return None,
        };
        // Nothing below allocates on the heap until the handle is made, so
        // `arr` and its elements stay where they are.
        let element = |thread: &mut JvmThread, i: Option<usize>| {
            let ctx_impl = crate::vm::NativeContextImpl { shared, thread };
            let ctx: &dyn cratonvm_native_api::NativeContext = &ctx_impl;
            match i {
                None => (ctx.array_length(arr), None),
                Some(i) => match ctx.get_array_element(arr, i) {
                    Value::Object(Some(e)) => (0, Some(e)),
                    _ => (0, None),
                },
            }
        };
        let (len, _) = element(&mut *thread, None);
        for i in 0..len {
            let Some(elem) = element(&mut *thread, Some(i)).1 else {
                continue;
            };
            let Some(m) = reflected_member_of(shared, thread, elem) else {
                continue;
            };
            if m.kind == kind && m.name == name && m.descriptor == descriptor {
                return Some(new_local_handle(elem));
            }
        }
        None
    })
    .flatten()
    .unwrap_or(0)
}

/// Resolve a class a JNI entry point needs to allocate against, refusing rather
/// than fabricating under `--real-jdk`.
///
/// The "refuse diagnosably" shape of `vm_init::ensure_bootstrap_compat_class`,
/// adapted to a caller with no Java-side error channel: a JNI function cannot
/// throw from here, and its documented failure value is NULL, so a refusal
/// returns `None` and the caller returns null after this has warned and named
/// the class. The real class is preferred first, exactly as before, so on any
/// complete image nothing about this path changes.
///
/// Added 2026-08-10 with Real-JDK wave 2 step 3, which deleted the infallible
/// `ensure_synthetic_class` these three sites used to reach.
pub(in crate::native::jni) fn jni_class_or_refuse(shared: &SharedVm, name: &str, num_fields: usize) -> Option<ClassId> {
    if let Ok(id) = shared.load_class_concurrent(name) {
        return Some(id);
    }
    match shared
        .classes
        .class_manager_write_settling()
        .try_ensure_synthetic_class(name, num_fields)
    {
        Ok(id) => Some(id),
        Err(err) => {
            tracing::warn!(
                class = name,
                error = %err,
                "--real-jdk: refusing to fabricate this class for a JNI entry point. The \
                 call returns NULL, which is JNI's documented failure value, and the \
                 caller sees it at its own call site."
            );
            None
        }
    }
}

// ---- Index 9: ToReflectedMethod ----
// Convert a JMethodID to a java.lang.reflect.Method / Constructor object.
//
// gcf f2/j (`gcf-f1j-jni-to-reflected-method-builds-a-hollow-method`): built
// by Java reflection itself ([`to_reflected_member`]), a `Constructor` for
// `<init>`, for the method's DECLARING class as HotSpot's
// `jni_ToReflectedMethod` does (`clazz` and `isStatic` only name it). This
// allocated a `Method` whose JDK fields were never written, with ints in
// absolute slots 0 and 1 -- inherited fields of other types.
pub(in crate::native::jni) extern "C" fn jni_to_reflected_method(
    _env: JNIEnv,
    _clazz: JClass,
    method_id: JMethodID,
    _is_static: JBoolean,
) -> JObject {
    let _fx = ForeignJniEntry::enter();
    if method_id == 0 {
        return 0;
    }
    let (decl_class_id, method_index) = decode_method_id(method_id);
    let Some(Some((decl_class_id, name, descriptor))) = with_shared_vm(|shared| {
        let stored = {
            let cm = shared.classes.class_manager.read();
            cm.get_class(decl_class_id)
                .and_then(|class| class.methods.get(usize::from(method_index)))
                .map(|m| (decl_class_id, m.name.to_string(), m.descriptor.to_string()))
        };
        stored.or_else(|| lambda_proxy_reflected_method(shared, decl_class_id, method_index))
    }) else {
        return 0;
    };
    let kind = match name.as_str() {
        "<init>" => ReflectedKind::Constructor,
        // No reflection object names a class initializer.
        "<clinit>" => return 0,
        _ => ReflectedKind::Method,
    };
    to_reflected_member(decl_class_id, kind, &name, &descriptor)
}

/// `ToReflectedMethod` of a lambda proxy's own method id (interpreter round
/// i2 wave 60, lane L1; [`lambda_proxy_declared_method_id`]): the method of
/// the functional interface it implements, which is what `GetMethodID`
/// answered for it before (reflection does not build a `Method` of a proxy
/// yet; HotSpot's is declared by the hidden class). `None` for the
/// constructor and for any id that is not a proxy's. Cold.
#[cold]
#[inline(never)]
pub(in crate::native::jni) fn lambda_proxy_reflected_method(
    shared: &SharedVm,
    proxy: ClassId,
    index: u16,
) -> Option<(ClassId, String, String)> {
    let (name, descriptor) = lambda_proxy_jni_method(shared, proxy, index)?;
    // Interpreter round i6 wave 118, lane L3 (`CRATONVM_REFLECT_LAMBDA_PROXY_FACTS`):
    // HotSpot's `ToReflectedMethod` answers the hidden class's own `Method` /
    // `Constructor`; core reflection lists them from the same record under
    // the switch (`lang_class::declared_methods_with_synthetic`; the
    // constructor from `getDeclaredConstructors`), so the member is looked up
    // on the proxy itself.
    if cratonvm_types::flags().natives.reflect_lambda_proxy_facts {
        return Some((proxy, name.to_string(), descriptor.to_string()));
    }
    if &*name == "<init>" {
        return None;
    }
    let [iface, _] = lambda_proxy_method_classes(shared, proxy)?;
    let cm = shared.classes.class_manager.read();
    let (_, declaring) = find_method_recursive(iface, &name, &descriptor, &cm.class_store)?;
    Some((declaring, name.to_string(), descriptor.to_string()))
}

// ---- Index 12: ToReflectedField ----
// Convert a JFieldID to a java.lang.reflect.Field object.
//
// gcf f2/j: as `jni_to_reflected_method`. A field id's index is an instance
// field's absolute slot or a static field's position among its class's
// statics (`Class::find_own_field`), so `isStatic` says which -- as in
// HotSpot, where it selects the `jfieldID` encoding.
pub(in crate::native::jni) extern "C" fn jni_to_reflected_field(
    _env: JNIEnv,
    _clazz: JClass,
    field_id: JFieldID,
    is_static: JBoolean,
) -> JObject {
    let _fx = ForeignJniEntry::enter();
    if field_id == 0 {
        return 0;
    }
    let (decl_class_id, field_index) = decode_field_id(field_id);
    let Some(Some((name, descriptor))) = with_shared_vm(|shared| {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(decl_class_id)?;
        let f = if is_static != 0 {
            class
                .fields
                .iter()
                .filter(|f| f.is_static())
                .nth(field_index)?
        } else {
            class.field_at_index(field_index)?
        };
        Some((f.name.to_string(), f.descriptor.to_string()))
    }) else {
        return 0;
    };
    to_reflected_member(decl_class_id, ReflectedKind::Field, &name, &descriptor)
}

// ---- Index 220: GetStringRegion ----
pub(in crate::native::jni) extern "C" fn jni_get_string_region(
    _env: JNIEnv,
    str_obj: JString,
    start: JSize,
    len: JSize,
    buf: *mut JChar,
) {
    let _fx = ForeignJniEntry::enter();
    if str_obj == 0 || buf.is_null() {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(str_obj)?;
        // gci i13/jc: the exact units (a lone surrogate is copied as itself,
        // as HotSpot's `jni_GetStringRegion` does, not as U+FFFD).
        let utf16: Vec<u16> = crate::vm::read_java_string_units(&shared.mem.heap, oref)?;
        // JNI contract: a region outside the string raises (String)IndexOutOf-
        // BoundsException — surfaced here as AIOOBE via the shared checker
        // (overflow-safe; no partial copy on a bad range).
        if !string_region_bounds_ok(start, len, utf16.len()) {
            return None;
        }
        let start = start as usize;
        let len = len as usize;
        unsafe {
            std::ptr::copy_nonoverlapping(utf16[start..start + len].as_ptr(), buf, len);
        }
        Some(())
    });
}

// ---- Index 221: GetStringUTFRegion ----
pub(in crate::native::jni) extern "C" fn jni_get_string_utf_region(
    _env: JNIEnv,
    str_obj: JString,
    start: JSize,
    len: JSize,
    buf: *mut c_char,
) {
    let _fx = ForeignJniEntry::enter();
    if str_obj == 0 || buf.is_null() {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(str_obj)?;
        // In JNI, start/len refer to UTF-16 code units. gci i14/jh: the exact
        // units (`read_java_string_units`), not a `str` re-encoded: a lone
        // surrogate and a half of a split pair are their own 3-byte sequence,
        // as on HotSpot, where they were U+FFFD.
        let utf16: Vec<u16> = crate::vm::read_java_string_units(&shared.mem.heap, oref)?;
        // JNI contract: a region outside the string raises (String)IndexOutOf-
        // BoundsException — surfaced here as AIOOBE via the shared checker.
        if !string_region_bounds_ok(start, len, utf16.len()) {
            return None;
        }
        let start = start as usize;
        let len = len as usize;
        let region = &utf16[start..start + len];
        // JNI writes *modified* UTF-8 here, exactly as `GetStringUTFChars`
        // does — same encoder (gci i14/jh: `units_to_modified_utf8`), so the
        // two entry points and `GetStringUTFLength` (which measures with
        // `modified_utf8_len_units`) all agree. This used to take the region's *standard* UTF-8 bytes
        // directly, which disagreed with both siblings in two ways:
        //
        //   * U+0000 was written as a raw `0x00` instead of `0xC0 0x80`, so
        //     every C consumer saw the region truncated at the first interior
        //     NUL — `"a\0b"` read back as `"a"`.
        //   * A supplementary character was written as its 4-byte standard
        //     UTF-8 form instead of the 6-byte surrogate pair (CESU-8) the
        //     spec mandates. The canonical caller sizes its buffer with
        //     `GetStringUTFLength` (which correctly says 6) and then reads
        //     that many bytes, so bytes 4..6 were whatever `malloc` left
        //     there — an uninitialized read, and a string a modified-UTF-8
        //     decoder cannot parse.
        //
        // gci i14/jh: the old residual is gone -- a region that splits a
        // surrogate pair wrote U+FFFD for the orphaned half (a round trip
        // through a Rust `str`); each unit is now its own sequence, as
        // HotSpot's `as_utf8_string` writes it. Same byte count as before.
        let bytes = units_to_modified_utf8(region);
        // gcf f3/j: HotSpot 25 DOES terminate the region (`as_utf8_string`
        // writes a NUL after it, and a zero-length region writes `buf[0] = 0`),
        // read off its own JNI table, 2026-10-01; the comment here said it
        // did not, and a C reader of the buffer found no end.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, buf, bytes.len());
            *buf.add(bytes.len()) = 0;
        }
        Some(())
    });
}

/// Metadata for the detached copy buffer `GetPrimitiveArrayCritical` hands out
/// (every array, not only G1-humongous ones). See `JniGlobalRefs::critical_copies`.
///
/// gc-common w2-c (2026-09-23), `common-g-pinning-semantics-differ-per-backend`:
/// the array is found again at Release through `array_gref`, a REMAPPABLE JNI
/// global ref, exactly as `ArrayElemBuffer` does for `Get<Type>ArrayElements`.
/// It used to keep the raw Get-time handle and re-resolve that, which was sound
/// only while the array could not move: G1 and ZGC pinned its region/page for
/// the whole section (`VmHeap::pin_critical_region`), Generational pinned
/// nothing -- so a Generational young copy inside the window (a foreign-attached
/// thread is GC-blocked between its calls; an up-call inside the section can
/// collect) made the copy-back write into from-space or a recycled object. With
/// the global ref the copy-back follows the array on every backend, the region
/// pin has nothing left to protect, and the three collectors honour one
/// contract: a detached copy, kept alive by a per-VM global ref, written back to
/// wherever the array is at Release. The keep-alive pin in the process-global
/// `cratonvm_gc::pinned` table went with it: the global ref is that root.
/// gci i13/jc: kept per VM in `JniGlobalRefs::critical_copies` (F10).
#[derive(Clone, Copy)]
pub(in crate::native::jni) struct CriticalCopy {
    /// Remappable handle to the source array (a JNI global ref, bit 0 set).
    /// Resolved at Release to the array's current address; deleted on the
    /// final (freeing) release.
    pub(in crate::native::jni) array_gref: JObject,
    /// Element type, so release re-packs each element correctly.
    pub(in crate::native::jni) element_type: ArrayElementType,
    /// Number of elements (== array length at Get time).
    pub(in crate::native::jni) len: usize,
    /// Bytes per element (the contiguous buffer is `len * stride` bytes).
    pub(in crate::native::jni) stride: usize,
    /// zt t18/hunt1: the sections open on this buffer (`>= 1`): a second
    /// critical of the same array shares it (`share_critical_copy`), and only
    /// the last freeing release frees it and deletes `array_gref`.
    pub(in crate::native::jni) opens: usize,
}

/// Size of the copy buffer `GetPrimitiveArrayCritical` hands out for `len`
/// elements of `stride` bytes: never 0, so every handout is a real,
/// uniquely-addressed allocation (gc-common w8-c; see the Get site).
pub(in crate::native::jni) fn critical_buffer_bytes(len: usize, stride: usize) -> usize {
    (len * stride).max(1)
}

/// Bytes per stored element for a primitive array element type.
pub(in crate::native::jni) fn critical_stride(et: ArrayElementType) -> usize {
    match et {
        ArrayElementType::Byte | ArrayElementType::Boolean => 1,
        ArrayElementType::Char | ArrayElementType::Short => 2,
        ArrayElementType::Int | ArrayElementType::Float => 4,
        ArrayElementType::Long | ArrayElementType::Double => 8,
        // Reference arrays are not valid for the primitive-critical API.
        ArrayElementType::Reference => 0,
    }
}

/// Write the host-endian bytes of array element `i` (read via the
/// region-safe accessor) into `dst[i*stride .. (i+1)*stride]`.
pub(in crate::native::jni) fn critical_encode_element(v: Value, et: ArrayElementType, dst: &mut [u8]) {
    match et {
        ArrayElementType::Byte | ArrayElementType::Boolean => {
            let x = v.as_int().unwrap_or(0) as i8;
            dst[0] = x as u8;
        }
        ArrayElementType::Short => {
            let x = v.as_int().unwrap_or(0) as i16;
            dst[..2].copy_from_slice(&x.to_ne_bytes());
        }
        ArrayElementType::Char => {
            let x = v.as_int().unwrap_or(0) as u16;
            dst[..2].copy_from_slice(&x.to_ne_bytes());
        }
        ArrayElementType::Int => {
            let x = v.as_int().unwrap_or(0);
            dst[..4].copy_from_slice(&x.to_ne_bytes());
        }
        ArrayElementType::Float => {
            let x = match v {
                Value::Float(f) => f,
                _ => 0.0,
            };
            dst[..4].copy_from_slice(&x.to_ne_bytes());
        }
        ArrayElementType::Long => {
            let x = match v {
                Value::Long(l) => l,
                _ => 0,
            };
            dst[..8].copy_from_slice(&x.to_ne_bytes());
        }
        ArrayElementType::Double => {
            let x = match v {
                Value::Double(d) => d,
                _ => 0.0,
            };
            dst[..8].copy_from_slice(&x.to_ne_bytes());
        }
        ArrayElementType::Reference => {}
    }
}

/// Decode element `i` from `src[i*stride .. (i+1)*stride]` (host-endian)
/// back into a `Value` for store via the region-safe accessor.
pub(in crate::native::jni) fn critical_decode_element(et: ArrayElementType, src: &[u8]) -> Value {
    match et {
        ArrayElementType::Byte | ArrayElementType::Boolean => Value::Int(src[0] as i8 as i32),
        ArrayElementType::Short => {
            let x = i16::from_ne_bytes([src[0], src[1]]);
            Value::Int(x as i32)
        }
        ArrayElementType::Char => {
            let x = u16::from_ne_bytes([src[0], src[1]]);
            Value::Int(x as i32)
        }
        ArrayElementType::Int => Value::Int(i32::from_ne_bytes([src[0], src[1], src[2], src[3]])),
        ArrayElementType::Float => {
            Value::Float(f32::from_ne_bytes([src[0], src[1], src[2], src[3]]))
        }
        ArrayElementType::Long => Value::Long(i64::from_ne_bytes([
            src[0], src[1], src[2], src[3], src[4], src[5], src[6], src[7],
        ])),
        ArrayElementType::Double => Value::Double(f64::from_ne_bytes([
            src[0], src[1], src[2], src[3], src[4], src[5], src[6], src[7],
        ])),
        ArrayElementType::Reference => Value::Object(None),
    }
}



/// The release mode a critical copy is released with: `JNI_ABORT` (2) becomes
/// a copy-back-and-free (0) under the HotSpot-shaped opt-in; every other mode,
/// and every mode without it, is unchanged. `JNI_COMMIT` keeps its
/// copy-back-and-keep meaning on both arms (the buffer outliving the commit is
/// what a native that keeps using it after a commit needs here).
pub(in crate::native::jni) fn critical_release_mode(mode: JInt, hotspot: bool) -> JInt {
    if hotspot && mode == 2 {
        0
    } else {
        mode
    }
}

/// The `isCopy` `GetPrimitiveArrayCritical` stores: `JNI_TRUE` (it is a copy),
/// or `JNI_FALSE` under the HotSpot-shaped opt-in, where every release writes
/// the copy back so no release mode can tell it from a pinned body.
pub(in crate::native::jni) fn critical_is_copy_answer(hotspot: bool) -> JBoolean {
    if hotspot {
        JNI_FALSE
    } else {
        JNI_TRUE
    }
}

// gci i14/pn (`CRATONVM_JNI_CRITICAL_G1_PIN`, default ON since 2026-10-17,
// `=0` kills): on G1, `GetPrimitiveArrayCritical` and `GetStringCritical` (a
// UTF16-coded String's value array) hand native code the array's OWN body and
// pin its region, as HotSpot 25 G1 does (JEP 423): `isCopy` is `JNI_FALSE`,
// the native's writes are in the array as it makes them, and
// `ReleasePrimitiveArrayCritical` ignores `mode` -- every release (0,
// `JNI_COMMIT`, `JNI_ABORT`) unpins and copies nothing, as
// `jni_ReleasePrimitiveArrayCritical` does (`unlock_gc_or_unpin_object`
// whatever the mode). G1 keeps the region out of every move while the pin is
// held (`G1Collector::pin_region_for_addr`'s doc names each path). The array
// stays reachable through a per-VM global ref for the section (as the copy
// does), and a thread that exits with a section open drops its pins
// ([`release_critical_pins_opened_by`]).
//
// Generational and ZGC keep the detached copy, as does a G1 array whose payload
// `G1Collector::pin_array_payload_for_critical` refuses: a Generational young
// collection copies every survivor and no young-gen pin exists (HotSpot Serial
// holds the GC locker instead -- every collection waits for the last critical
// section to end -- which would be a new pause-protocol state, not a pin).



/// gci i14/pn: one pinned G1 critical section (see `JniGlobalRefs::critical_pins`).
#[derive(Clone, Copy)]
pub(in crate::native::jni) struct CriticalPin {
    /// The body handed to native code (`VmHeap::array_data_ptr`); the key.
    pub(in crate::native::jni) ptr: usize,
    /// Keep-alive global ref to the array, deleted at the release.
    pub(in crate::native::jni) array_gref: JObject,
    /// The pinned G1 region (the array's header region).
    pub(in crate::native::jni) region: usize,
    /// For the `long[]` smuggled-handle mint at release.
    pub(in crate::native::jni) element_type: ArrayElementType,
    /// Elements at Get time.
    pub(in crate::native::jni) len: usize,
    /// The thread whose JNI binding opened it (`jni_bound_thread_id`).
    pub(in crate::native::jni) opener: Option<ThreadId>,
}

/// gci i14/pn: pin `oref`'s region on G1 and record the section, answering
/// the array body native code may use in place; `None` (nothing pinned or
/// recorded) on any other heap or a payload G1 refuses, and the caller copies.
/// `len` elements of `stride` bytes. Runs inside a JNIEnv function's
/// transition (a counted mutator), so no pause moves `oref` between its
/// resolution and the pin.
pub(in crate::native::jni) fn open_critical_g1_pin(
    shared: &SharedVm,
    oref: ObjectRef,
    element_type: ArrayElementType,
    len: usize,
    stride: usize,
) -> Option<*mut u8> {
    if !shared.mem.heap.is_g1() {
        return None;
    }
    let body = shared.mem.heap.array_data_ptr(oref)?;
    let obj_addr = oref.as_ptr() as usize;
    let offset = (body as usize).checked_sub(obj_addr)?;
    let bytes = len.checked_mul(stride)?;
    let region = shared.mem.heap.g1_pin_array_payload_for_critical(obj_addr, offset, bytes)?;
    shared.natives.jni_global_refs.lock().open_critical_pin(
        body as usize,
        oref,
        region,
        element_type,
        len,
        jni_bound_thread_id(),
    );
    Some(body)
}

/// gci i14/pn: the release of a pinned section: unpin its region. A `long[]`
/// body first passes every value through the smuggled-handle mint the copy's
/// write-back runs (`mint_if_smuggled_long_value`), when `scan_longs` (the
/// caller runs as a counted mutator, so the body cannot move or die under the
/// read: the record's global ref was deleted under the same transition).
pub(in crate::native::jni) fn finish_critical_pin(shared: &SharedVm, pin: CriticalPin, scan_longs: bool) {
    if scan_longs && pin.element_type == ArrayElementType::Long {
        for i in 0..pin.len {
            // SAFETY: `pin.ptr` is the pinned, still-reachable body of a
            // `long[]` of at least `pin.len` elements (a Java array's length
            // never changes), 8-aligned (`ARRAY_DATA_OFFSET`).
            let bits = unsafe { std::ptr::read((pin.ptr as *const u64).add(i)) };
            mint_if_smuggled_jlong(&shared.mem.heap, bits);
        }
    }
    shared.mem.heap.g1_unpin_region(pin.region);
}

/// gci i14/pn: release the pinned G1 section whose body is at `ptr`, if one
/// is open (preferring one this thread opened). Answers whether `ptr` named
/// one; `false` sends the caller to its copy record. `scan_longs`: see
/// [`finish_critical_pin`] (`false` where the caller is not a counted mutator).
pub(in crate::native::jni) fn release_critical_g1_pin(ptr: usize, scan_longs: bool) -> bool {
    with_shared_vm(|shared| {
        let pin = shared
            .natives
            .jni_global_refs
            .lock()
            .take_critical_pin(ptr, jni_bound_thread_id())?;
        finish_critical_pin(shared, pin, scan_longs);
        Some(())
    })
    .flatten()
    .is_some()
}

/// gci i14/pn: a thread is exiting (or detaching): release every pinned G1
/// critical section it opened and never released, so its regions can move
/// again and its arrays can die. A native bug (HotSpot would leak the pin);
/// counted in a `warn!`. Returns how many. Safe in any thread state: it takes
/// two locks and reads no heap memory. The callers: `DetachCurrentThread`'s
/// teardown here (`detach_foreign_thread_idle`) and the platform thread's
/// termination (`vm_exec.rs` `thread_start`, cross-lane hunk of gci i14/pn).
pub(crate) fn release_critical_pins_opened_by(shared: &SharedVm, tid: ThreadId) -> usize {
    let pins = shared
        .natives
        .jni_global_refs
        .lock()
        .take_critical_pins_opened_by(tid);
    let n = pins.len();
    for pin in pins {
        finish_critical_pin(shared, pin, false);
    }
    if n > 0 {
        tracing::warn!(
            sections = n,
            "JNI: a thread ended with {n} GetPrimitiveArrayCritical/GetStringCritical section(s) \
             open; their G1 region pins are released"
        );
    }
    n
}

// ---- Index 222: GetPrimitiveArrayCritical ----
// Returns a pointer to a native-endian COPY of the array data (is_copy=JNI_TRUE);
// see the `Get<Type>ArrayElements` module comment for why we never hand out a
// direct heap pointer under this VM's moving collectors -- except on G1 under
// `CRATONVM_JNI_CRITICAL_G1_PIN` (gci i14/pn), where the array's own body is
// handed out with its region pinned (`open_critical_g1_pin`, is_copy=JNI_FALSE).
// (gci i15/ra: this header said "never" after the pinned arm existed.)
pub(in crate::native::jni) extern "C" fn jni_get_primitive_array_critical(
    _env: JNIEnv,
    array: JArray,
    is_copy: *mut JBoolean,
) -> *mut std::ffi::c_void {
    let _fx = ForeignJniEntry::enter();
    if array == 0 {
        return std::ptr::null_mut();
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(array)?;
        // FORCE-COPY (vm-jni-roots #2): never hand out a raw pointer into the
        // live array body. The VM's moving collectors would relocate the array
        // out from under the native critical pointer (UAF). Materialise a
        // detached, native-endian copy via the region-safe per-element accessor,
        // register it for copy-back, and report `is_copy = JNI_TRUE`. The JNI
        // spec permits returning a copy from GetPrimitiveArrayCritical; it is the
        // only collector-agnostic correct choice for this heap. Works uniformly
        // for ordinary AND G1-humongous arrays.
        let element_type = shared.mem.heap.array_element_type(oref)?;
        let stride = critical_stride(element_type);
        if stride == 0 {
            return None; // reference array — not a primitive critical
        }
        let len = shared.mem.heap.array_length(oref);
        // gci i14/pn: on G1 under `CRATONVM_JNI_CRITICAL_G1_PIN`, the array's
        // own body with its region pinned (HotSpot G1's answer, `isCopy` 0);
        // the copy below otherwise. See `open_critical_g1_pin`.
        {
            if let Some(body) = open_critical_g1_pin(shared, oref, element_type, len, stride) {
                if !is_copy.is_null() {
                    unsafe {
                        *is_copy = JNI_FALSE;
                    }
                }
                return Some(body as *mut std::ffi::c_void);
            }
        }
        // zt t18/hunt1: a section already open on this array hands out ITS
        // buffer (see `JniGlobalRefs::share_critical_copy`): on HotSpot the
        // two answers are one memory, the array's body.
        let shared_copy = {
            let mut refs = shared.natives.jni_global_refs.lock();
            refs.share_critical_copy(oref, element_type, len)
        };
        if let Some(ptr) = shared_copy {
            if !is_copy.is_null() {
                unsafe {
                    *is_copy = critical_is_copy_answer(true);
                }
            }
            return Some(ptr as *mut std::ffi::c_void);
        }
        // gc-common w8-c: `.max(1)` (as `Get<Type>ArrayElements` already
        // does). A zero-length `vec!` does not allocate: its pointer is the
        // same dangling sentinel for every empty array, so two open criticals
        // on empty arrays (`in = Get(src); out = Get(dst)`, the compression
        // idiom) collided on one `JNI_CRITICAL_COPIES` key. The second insert
        // dropped the first entry, whose global ref -- the array's root --
        // was then never deleted: that array was immortal. Release rebuilds
        // the buffer with the same `critical_buffer_bytes`.
        //
        // gc-common w10-c: allocated FALLIBLY, and as an exact-length boxed
        // slice (Release rebuilds `(byte_len, byte_len)`). `vec![0u8; n]`
        // aborted the process when the C heap could not hold the copy.
        //
        // j27 wave 3 (lane DISPATCH2): a failed copy answers NULL with NO
        // pending exception. HotSpot 25 never copies here (it pins or locks
        // the GC), so this arm has no HotSpot counterpart; its nearest
        // relatives, `Get<Type>ArrayElements` and the copying arm of
        // `jni_GetStringCritical`, answer a failed C-heap allocation with
        // NULL alone (`NEW_C_HEAP_ARRAY_RETURN_NULL`, "JNI Specification
        // states return null on OOM"), and a native inside a critical region
        // may not call the JNI functions that would observe a pending one.
        let byte_len = critical_buffer_bytes(len, stride);
        let mut buf: Vec<u8> = reserve_elements_copy(byte_len)?;
        buf.resize(byte_len, 0);
        let mut buf: Box<[u8]> = buf.into_boxed_slice();
        // gci i13/jc (F11): one `memcpy` of the payload when the array's
        // elements are contiguous raw values (`primitive_region_ptr`, the
        // `Get<Type>ArrayRegion` bulk path, under the same
        // `CRATONVM_JNI_BULK_ARRAY_COPY`); the element walk otherwise. Both
        // write the native-endian element bytes.
        let bulk_src = if len > 0 && bulk_array_copies() {
            primitive_region_ptr(shared, oref, element_type, 0, stride)
        } else {
            None
        };
        if let Some(src) = bulk_src {
            // SAFETY: `src` spans the array's `len` elements of `stride` bytes
            // (its payload); `buf` holds `len * stride` bytes; heap vs C heap,
            // so no overlap. Nothing between here and the copy can move it.
            unsafe {
                std::ptr::copy_nonoverlapping(src as *const u8, buf.as_mut_ptr(), len * stride);
            }
        } else {
            for i in 0..len {
                let v = match shared.mem.heap.get_array_element(oref, i) {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let off = i * stride;
                critical_encode_element(v, element_type, &mut buf[off..off + stride]);
            }
        }
        let ptr = buf.as_mut_ptr();
        std::mem::forget(buf); // OWNERSHIP: transferred to native caller, reclaimed by jni_release_primitive_array_critical

        // Keep-alive AND find-again in one handle: a remappable global ref,
        // rewritten by every relocating collection, so the copy-back at Release
        // lands in the live array on every backend (see `CriticalCopy`). No
        // region/page pin: native code holds a copy, so the array is free to
        // move (gc-common w2-c). Recorded in this VM's table (gci i13/jc F10).
        shared.natives.jni_global_refs.lock().open_critical_copy(
            ptr as usize,
            oref,
            element_type,
            len,
            stride,
        );
        if !is_copy.is_null() {
            // Always a copy under this VM's moving GC; see
            // `critical_is_copy_answer` for the HotSpot-shaped opt-in.
            unsafe {
                *is_copy = critical_is_copy_answer(true);
            }
        }
        Some(ptr as *mut std::ffi::c_void)
    })
    .flatten()
    .unwrap_or(std::ptr::null_mut())
}

// ---- Index 223: ReleasePrimitiveArrayCritical ----
pub(in crate::native::jni) extern "C" fn jni_release_primitive_array_critical(
    _env: JNIEnv,
    _array: JArray,
    carray: *mut std::ffi::c_void,
    mode: JInt,
) {
    let _fx = ForeignJniEntry::enter();
    if carray.is_null() {
        return;
    }
    // gci i14/pn: a pinned G1 body (`CRATONVM_JNI_CRITICAL_G1_PIN`): every
    // mode unpins and copies nothing, as HotSpot's release ignores `mode`.
    // Records exist only with the switch on, so it gates the lookup.
    if release_critical_g1_pin(carray as usize, true) {
        return;
    }
    // Every GetPrimitiveArrayCritical handout is a registered copy buffer. If
    // `carray` is not one we handed out (foreign pointer / already released),
    // there is nothing to copy back or free.
    // gci i13/jc: `JNI_ABORT` keeps the native's writes under the
    // HotSpot-shaped opt-in (see `critical_release_mode`). The flag is read
    // only for an abort.
    let mode = if mode == 2 {
        critical_release_mode(mode, true)
    } else {
        mode
    };
    // gci i13/jc (F10): the record is in this VM's table (taken unless this
    // is a `JNI_COMMIT`, whose section continues), found from any thread.
    let copy = with_shared_vm(|shared| {
        shared
            .natives
            .jni_global_refs
            .lock()
            .critical_copy(carray as usize, mode != 1)
    })
    .flatten();
    // zt t18/hunt1: `last` is false while another section shares the buffer
    // (`share_critical_copy`); that release copies back but frees nothing.
    let Some((copy, last)) = copy else {
        return;
    };
    // Copy buffer. mode 0 = copy back + free, JNI_COMMIT (1) =
    // copy back, don't free, JNI_ABORT (2) = free without copy back.
    if mode != 2 {
        with_shared_vm(|shared| {
            // The array's CURRENT address (w2-c): the global ref followed any
            // relocation since Get. Bounded by the live length too, as
            // `Release<Type>ArrayElements` is.
            let oref = shared
                .natives
                .jni_global_refs
                .lock()
                .resolve(copy.array_gref)?;
            let copy_len = copy.len.min(shared.mem.heap.array_length(oref));
            // gci i13/jc (F11): one `memcpy` back, as `Set<Type>ArrayRegion`'s
            // bulk path does (never for `boolean[]`, which that path also
            // walks); a `long[]` still passes every value through the mint
            // chokepoint below, without the per-element store.
            let bulk_dst = if copy_len > 0
                && copy.element_type != ArrayElementType::Boolean
                && bulk_array_copies()
            {
                primitive_region_ptr(shared, oref, copy.element_type, 0, copy.stride)
            } else {
                None
            };
            if let Some(dst) = bulk_dst {
                // SAFETY: `dst` spans `copy_len` elements of the live array
                // (bounded by its current length above); the buffer holds
                // `copy.len >= copy_len` elements; C heap vs heap.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        carray as *const u8,
                        dst,
                        copy_len * copy.stride,
                    );
                }
                if copy.element_type != ArrayElementType::Long {
                    return Some(());
                }
            }
            for i in 0..copy_len {
                let off = i * copy.stride;
                // SAFETY: the buffer is `copy.len * copy.stride` bytes and we
                // only read within it.
                let src = unsafe {
                    std::slice::from_raw_parts((carray as *const u8).add(off), copy.stride)
                };
                let v = critical_decode_element(copy.element_type, src);
                // Mint chokepoint for a `long[]` critical write-back.
                mint_if_smuggled_long_value(&shared.mem.heap, &v);
                if bulk_dst.is_none() {
                    let _ = shared.mem.heap.set_array_element(oref, i, v);
                }
            }
            Some(())
        });
    }
    if mode != 1 && last {
        // Final release only (not JNI_COMMIT, whose section continues, nor a
        // release while another section shares the buffer): drop
        // the global ref, i.e. the array's keep-alive root.
        with_shared_vm(|shared| {
            shared
                .natives
                .jni_global_refs
                .lock()
                .remove(copy.array_gref);
            Some(())
        });
        // Reconstruct the `Vec<u8>` with its original layout and drop it.
        let byte_len = critical_buffer_bytes(copy.len, copy.stride);
        // SAFETY: `carray` was produced by `Vec::<u8>::as_mut_ptr` +
        // `mem::forget` in `jni_get_primitive_array_critical` from
        // `vec![0u8; critical_buffer_bytes(len, stride)]`, so capacity ==
        // length == `byte_len`; we reconstruct the exact layout.
        unsafe {
            drop(Vec::from_raw_parts(carray as *mut u8, byte_len, byte_len));
        }
    }
    // JNI_COMMIT: the record was left in the table (`critical_copy(.., false)`)
    // with its global ref, so a later release still finds it. So was a shared
    // buffer's record (`last == false`), counted down by one section.
}

// ---- Index 224: GetStringCritical ----
// HotSpot pins the string for the critical section; by default this VM never
// hands out a pointer into the heap (every collector here may move the string),
// so this is GetStringChars: a detached UTF-16 COPY, which the JNI spec permits
// (`isCopy` HotSpot's per coder since gci i13/jc, see `string_chars_copy`).
// Nothing is pinned and no collection is held off. gci i14/pn: on G1 under
// `CRATONVM_JNI_CRITICAL_G1_PIN`, a UTF16-coded String's value array is pinned
// and its body handed out (`isCopy` 0, not NUL-terminated: HotSpot's pinned
// arm), as `GetPrimitiveArrayCritical` does.
// (gc-common w8-c: the comment said "our heap doesn't move objects between
// GC", which has not been true of any backend since the moving collectors.)
pub(in crate::native::jni) extern "C" fn jni_get_string_critical(
    _env: JNIEnv,
    str_obj: JString,
    is_copy: *mut JBoolean,
) -> *const JChar {
    // gci i13/jc (F14): `isCopy` as HotSpot's `jni_GetStringCritical` answers
    // it (see `string_chars_copy`).
    string_chars_copy(str_obj, is_copy, true)
}

// ---- Index 225: ReleaseStringCritical ----
pub(in crate::native::jni) extern "C" fn jni_release_string_critical(_env: JNIEnv, str_obj: JString, chars: *const JChar) {
    // gci i14/pn: a pinned UTF16 value array (no heap read: locks only, so no
    // transition is needed here either).
    if !chars.is_null()
        && release_critical_g1_pin(chars as usize, false)
    {
        return;
    }
    jni_release_string_chars(_env, str_obj, chars);
}

// ---- Index 226: NewWeakGlobalRef ----
// gc-common w8-c (`common-w7c-jni-weak-global-refs-are-strong`): a real weak
// global. It is kept in `JniGlobalRefs`' weak set, which is not a GC root; every
// collection remaps it or clears it (`sweep_weak_global_refs` and the concurrent
// remark / reclamation sweeps), and resolving it is a keep-alive. It used to be
// a strong global ref, so every object a native cached weakly (and the loader
// of every class it cached weakly) was immortal and never read as collected.
pub(in crate::native::jni) extern "C" fn jni_new_weak_global_ref(_env: JNIEnv, obj: JObject) -> JObject {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 {
        return 0;
    }
    // Same `jclass`-is-a-ClassId round trip as `NewGlobalRef` — and this is the
    // overload JNA's `LOAD_CREF` actually calls, so it is the one that decided
    // whether `com.sun.jna.Native` could initialise at all.
    // gci i14/jh: under `CRATONVM_JNI_CLASS_GLOBAL_REFS=1` a real weak global
    // to the mirror (HotSpot's `JNIWeakGlobalRefType`).
    if jclass_id_handle(obj) {
        return class_handle_global_ref(obj, true, class_global_refs_active()).unwrap_or(obj);
    }
    with_shared_vm(|shared| {
        // `jobject_to_obj` of a cleared weak `obj` is NULL, and so is the
        // result (HotSpot: a weak global to a collected object gives NULL).
        let oref = jobject_to_obj(obj)?;
        let mut refs = shared.natives.jni_global_refs.lock();
        Some(refs.add_weak(oref))
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 227: DeleteWeakGlobalRef ----
pub(in crate::native::jni) extern "C" fn jni_delete_weak_global_ref(_env: JNIEnv, wref: JObject) {
    let _fx = ForeignJniEntry::enter();
    if wref == 0 {
        return;
    }
    // A class handle is permanent; dropping it is a no-op (see DeleteGlobalRef).
    if is_jclass_handle(wref) {
        return;
    }
    with_shared_vm(|shared| {
        let mut refs = shared.natives.jni_global_refs.lock();
        refs.remove(wref);
        Some(())
    });
}

// ---- Index 19: GetObjectRefType ----
// Returns the type of the reference: JNIInvalidRefType=0, JNILocalRefType=1,
// JNIGlobalRefType=2, JNIWeakGlobalRefType=3.
pub(in crate::native::jni) extern "C" fn jni_get_object_ref_type(_env: JNIEnv, obj: JObject) -> JInt {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 {
        return 0; // JNIInvalidRefType
    }
    // A `jclass` handle carries `JCLASS_TAG`, not the global-ref bit-0 tag, and
    // its low bit is just the low bit of the ClassId — so without this it
    // answered global-or-local at random per class. `FindClass` hands back a
    // local ref on HotSpot.
    if is_jclass_handle(obj) {
        return 1; // JNILocalRefType
    }
    if obj & 1 == 1 {
        // gc-common w8-c: a weak global answers JNIWeakGlobalRefType (3), as
        // the spec says.
        //
        // gci i13/jc: a tagged handle the table does not hold (deleted by
        // `DeleteGlobalRef` / `DeleteWeakGlobalRef`, or never one of ours)
        // answers JNIInvalidRefType (0), as HotSpot's `jni_GetObjectRefType`
        // does for a handle in no handle block or storage; it answered 2, so
        // the "is this ref still valid?" idiom read a deleted global as live.
        // With no JNI context there is no table to ask: the old answer, 2.
        match with_shared_vm(|shared| shared.natives.jni_global_refs.lock().kind(obj)) {
            Some(Some(JniGlobalKind::Weak)) => 3, // JNIWeakGlobalRefType
            Some(Some(JniGlobalKind::Strong)) | None => 2, // JNIGlobalRefType
            Some(None) => 0,                      // JNIInvalidRefType
        }
    } else {
        1 // JNILocalRefType
    }
}

/// Stub for unimplemented JNI functions. Logs a warning and returns 0.
pub(in crate::native::jni) extern "C" fn jni_stub() -> usize {
    tracing::warn!("unimplemented JNI function called");
    0
}

/// Stub for the bare C-varargs (`...`) JNI call slots — `CallObjectMethod`,
/// `CallStatic<Type>Method`, `CallNonvirtual<Type>Method`, etc.
///
/// The `...`-taking forms cannot be dispatched in stable Rust: there is no
/// portable way to walk a platform `va_list` that the caller assembled inline
/// (the V/`*MethodV` form receives an explicit `va_list` and the A/`*MethodA`
/// form receives a `jvalue[]`, both of which *are* implemented and wired).
///
/// Rather than silently fabricating a `0`/null result (which a native would
/// mistake for a real return value — an empty string, a null object, a zero
/// count), this raises an `UnsatisfiedLinkError` so the unsupported call fails
/// loudly. Native callers should use the `*MethodV` / `*MethodA` variants.
pub(in crate::native::jni) extern "C" fn jni_varargs_unsupported() -> usize {
    let _fx = ForeignJniEntry::enter();
    jni_throw_unsatisfied_link(
        "bare C-varargs JNI call form (CallXxxMethod(...)) is not supported on this VM; \
         use the CallXxxMethodV (va_list) or CallXxxMethodA (jvalue[]) variant instead",
    );
    0
}

#[cfg(target_arch = "x86_64")]
jni_varargs_trampolines! {
    jni_va_new_object => (jni_new_object_v, 3),

    jni_va_call_object_method  => (jni_call_object_method_v, 3),
    jni_va_call_boolean_method => (jni_call_boolean_method_v, 3),
    jni_va_call_byte_method    => (jni_call_byte_method_v, 3),
    jni_va_call_char_method    => (jni_call_char_method_v, 3),
    jni_va_call_short_method   => (jni_call_short_method_v, 3),
    jni_va_call_int_method     => (jni_call_int_method_v, 3),
    jni_va_call_long_method    => (jni_call_long_method_v, 3),
    jni_va_call_float_method   => (jni_call_float_method_v, 3),
    jni_va_call_double_method  => (jni_call_double_method_v, 3),
    jni_va_call_void_method    => (jni_call_void_method_v, 3),

    jni_va_call_nonvirtual_object_method  => (jni_call_nonvirtual_object_method_v, 4),
    jni_va_call_nonvirtual_boolean_method => (jni_call_nonvirtual_boolean_method_v, 4),
    jni_va_call_nonvirtual_byte_method    => (jni_call_nonvirtual_byte_method_v, 4),
    jni_va_call_nonvirtual_char_method    => (jni_call_nonvirtual_char_method_v, 4),
    jni_va_call_nonvirtual_short_method   => (jni_call_nonvirtual_short_method_v, 4),
    jni_va_call_nonvirtual_int_method     => (jni_call_nonvirtual_int_method_v, 4),
    jni_va_call_nonvirtual_long_method    => (jni_call_nonvirtual_long_method_v, 4),
    jni_va_call_nonvirtual_float_method   => (jni_call_nonvirtual_float_method_v, 4),
    jni_va_call_nonvirtual_double_method  => (jni_call_nonvirtual_double_method_v, 4),
    jni_va_call_nonvirtual_void_method    => (jni_call_nonvirtual_void_method_v, 4),

    jni_va_call_static_object_method  => (jni_call_static_object_method_v, 3),
    jni_va_call_static_boolean_method => (jni_call_static_boolean_method_v, 3),
    jni_va_call_static_byte_method    => (jni_call_static_byte_method_v, 3),
    jni_va_call_static_char_method    => (jni_call_static_char_method_v, 3),
    jni_va_call_static_short_method   => (jni_call_static_short_method_v, 3),
    jni_va_call_static_int_method     => (jni_call_static_int_method_v, 3),
    jni_va_call_static_long_method    => (jni_call_static_long_method_v, 3),
    jni_va_call_static_float_method   => (jni_call_static_float_method_v, 3),
    jni_va_call_static_double_method  => (jni_call_static_double_method_v, 3),
    jni_va_call_static_void_method    => (jni_call_static_void_method_v, 3),
}

/// The 31 bare-varargs slots, paired with what serves each: the trampoline on
/// x86-64, `jni_varargs_unsupported` everywhere else.
#[cfg(target_arch = "x86_64")]
pub(in crate::native::jni) fn jni_varargs_slots() -> [(usize, usize); 31] {
    macro_rules! slot {
        ($idx:expr, $sym:ident) => {
            ($idx, $sym as *const () as usize)
        };
    }
    [
        slot!(28, jni_va_new_object),
        slot!(34, jni_va_call_object_method),
        slot!(37, jni_va_call_boolean_method),
        slot!(40, jni_va_call_byte_method),
        slot!(43, jni_va_call_char_method),
        slot!(46, jni_va_call_short_method),
        slot!(49, jni_va_call_int_method),
        slot!(52, jni_va_call_long_method),
        slot!(55, jni_va_call_float_method),
        slot!(58, jni_va_call_double_method),
        slot!(61, jni_va_call_void_method),
        slot!(64, jni_va_call_nonvirtual_object_method),
        slot!(67, jni_va_call_nonvirtual_boolean_method),
        slot!(70, jni_va_call_nonvirtual_byte_method),
        slot!(73, jni_va_call_nonvirtual_char_method),
        slot!(76, jni_va_call_nonvirtual_short_method),
        slot!(79, jni_va_call_nonvirtual_int_method),
        slot!(82, jni_va_call_nonvirtual_long_method),
        slot!(85, jni_va_call_nonvirtual_float_method),
        slot!(88, jni_va_call_nonvirtual_double_method),
        slot!(91, jni_va_call_nonvirtual_void_method),
        slot!(114, jni_va_call_static_object_method),
        slot!(117, jni_va_call_static_boolean_method),
        slot!(120, jni_va_call_static_byte_method),
        slot!(123, jni_va_call_static_char_method),
        slot!(126, jni_va_call_static_short_method),
        slot!(129, jni_va_call_static_int_method),
        slot!(132, jni_va_call_static_long_method),
        slot!(135, jni_va_call_static_float_method),
        slot!(138, jni_va_call_static_double_method),
        slot!(141, jni_va_call_static_void_method),
    ]
}

/// See the x86-64 arm: same slot indices, all refusing loudly.
#[cfg(not(target_arch = "x86_64"))]
pub(in crate::native::jni) fn jni_varargs_slots() -> [(usize, usize); 31] {
    let refuse = jni_varargs_unsupported as *const () as usize;
    let mut out = [(0usize, refuse); 31];
    let idx = [
        28, 34, 37, 40, 43, 46, 49, 52, 55, 58, 61, 64, 67, 70, 73, 76, 79, 82, 85, 88, 91, 114,
        117, 120, 123, 126, 129, 132, 135, 138, 141,
    ];
    for (o, i) in out.iter_mut().zip(idx) {
        o.0 = i;
    }
    out
}

// ---------------------------------------------------------------------------
// Internal helpers for field access
// ---------------------------------------------------------------------------

pub(in crate::native::jni) fn get_int_field_raw(obj: JObject, field_id: JFieldID) -> JInt {
    if obj == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        match shared.mem.heap.get_field(oref, field_index) {
            Value::Int(i) => Some(i),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

pub(in crate::native::jni) fn set_int_field_raw(obj: JObject, field_id: JFieldID, val: i32) {
    if obj == 0 || field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        crate::runtime::frozen_pages::note_slot_write(shared, oref, field_index, true);
        shared
            .mem
            .heap
            .set_field(oref, field_index, Value::Int(val));
        Some(())
    });
}

pub(in crate::native::jni) fn get_static_int_raw(clazz: JClass, field_id: JFieldID) -> JInt {
    if clazz == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let statics = shared.classes.statics.read();
        let fields = statics.get(&decl_class_id)?;
        match fields.load_cell(field_index).as_ref() {
            Some(Value::Int(i)) => Some(*i),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

pub(in crate::native::jni) fn set_static_int_raw(field_id: JFieldID, val: JInt) {
    if field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let mut statics = shared.classes.statics.write();
        if let Some(fields) = statics.get_mut(&decl_class_id) {
            if field_index < fields.len() {
                fields.store_cell(field_index, Value::Int(val));
            }
        }
    });
}

// ---------------------------------------------------------------------------
// JNI Native Method Registry (RegisterNatives)
// ---------------------------------------------------------------------------

/// C-compatible struct matching `JNINativeMethod` in jni.h.
/// Used by `RegisterNatives` to pass (name, signature, function pointer) triples.
#[repr(C)]
pub(in crate::native::jni) struct JNINativeMethod {
    pub(in crate::native::jni) name: *const c_char,
    pub(in crate::native::jni) signature: *const c_char,
    pub(in crate::native::jni) fn_ptr: *mut (),
}

// Safety: the pointers in JNINativeMethod are only read while holding the
// caller's guarantee that they're valid (they're passed in from C). The struct
// itself is never stored.

/// The table type behind `SharedVm::natives::jni_native_methods`.
///
/// Key = FNV-1a hash of `"class_name.method_nameDescriptor"`, value = the raw
/// `fn` address inside the host library, called through `dispatch_jni_native`.
///
/// This was the process global `static JNI_NATIVE_METHODS` here until
/// 2026-08-06 — `JDK-ONLY-WAVE2` §6 (retired record:
/// additional-wave2-markers-not-in-the-original-inventory.md).
/// Contract §2 forbids process globals for this feature's state, and the
/// concrete hazard was that two VMs in one process saw each other's
/// `RegisterNatives`: a library loaded by VM A bound its pointers for VM B too.
///
/// Real-JDK mode: this remains a **second, parallel native registry** and is
/// deliberately left as one. Everything in it is a real function pointer inside
/// a real `.so`/`.dll` that a real JNI library published — exactly the "native
/// code may cross VM boundaries" case §11 sanctions. There is no `NativeKind`
/// to attach because there is no CratonVM-authored implementation to classify:
/// a `NativeKind::SyntheticStub` cannot get in here, so this table cannot be
/// the §1.3 bypass the single-resolver rule targets. Its dispatch sites in
/// `vm/src/vm/vm_exec.rs` consult `resolve_dispatch` before falling through to
/// [`find_jni_native`].
///
/// The census gap the move left, CLOSED 2026-08-10 without minting a fake id.
/// `record_invocation` keys on a `NativeMethodId` and only
/// `NativeMethodRegistry` issues one, so there is still no honest way to give a
/// `dlsym` result an id — and none is invented. Instead both dispatch sites in
/// `vm/src/vm/vm_exec.rs` increment `NativeRealm::jni_bridge_invocations`, and
/// `--real-jdk-report` adds that to `bridge_invocations`, which is where a real
/// function in a real library belongs. Until then that key under-counted
/// genuine JNI bridges by exactly the number of dispatches through this table.
/// `synthetic_stub_invocations` was and stays exact: a stub can never be here.
pub type JniNativeMethodTable = parking_lot::RwLock<HashMap<u64, usize>>;

/// Compute a hash key for a (class, method, descriptor) triple.
/// Identical algorithm to `NativeMethodRegistry::native_method_hash`.
pub(in crate::native::jni) fn jni_native_key(class_name: &str, method_name: &str, descriptor: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in class_name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^= b'.' as u64;
    h = h.wrapping_mul(0x100000001b3);
    for b in method_name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^= b'.' as u64;
    h = h.wrapping_mul(0x100000001b3);
    for b in descriptor.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Store a JNI function pointer in a specific table.
///
/// The `_in` pair is where the logic lives so a test can hand in a table
/// without standing up a whole `NativeRealm` — and, more usefully, so the
/// per-VM isolation this move exists for is directly testable with two of them.
///
/// Returns whether the key already had an entry (a re-registration, or a
/// hash collision), i.e. whether an answer some memo may hold was REPLACED
/// rather than a missing one added (gci i10/cr, [`register_jni_native`]).
pub fn register_jni_native_in(
    table: &JniNativeMethodTable,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    fn_ptr: usize,
) -> bool {
    let key = jni_native_key(class_name, method_name, descriptor);
    let mut table = table.write();
    if let Some(&existing) = table.get(&key) {
        if existing != fn_ptr {
            tracing::warn!(
                "JNI native method hash collision or re-registration: {}.{}{} (replacing 0x{:x} with 0x{:x})",
                class_name, method_name, descriptor, existing, fn_ptr
            );
        }
    }
    table.insert(key, fn_ptr).is_some()
}

/// gci i10/cr (`i9-cs-report.md` section 6 item 5): whether a write to the
/// JNI table must bump [`NativeRealm::jni_native_generation`]. Every memo
/// validated by that generation -- `find_jni_native`'s per-thread memo, the
/// i8/dm door memo and the i9/cs call-site memo -- stores only a key the
/// table ANSWERED (a miss is never memoised), so a write that adds a key the
/// table did not have changes no memoised answer. Only a replacement (a
/// re-registration, a hash collision) or a removal of a bound key
/// (`UnregisterNatives`) does -- `changed` -- and `rebind_jni_native` always
/// bumps. Bumping on a fresh key retired every
/// row of every thread on each lazy `dlsym` bind of any native.
///
/// gci i19/jt: the rule no longer depends on the memo opt-in. With
/// `CRATONVM_JNI_DISPATCH_MEMO` off (`=0`; default ON since 2026-10-15,
/// WARNING gci-user-flip-20261013) the one reader of the
/// generation is `find_jni_native`'s per-thread [`JniFnMemo`], a POSITIVE
/// one-entry memo whose key is compared by value: a fresh key is a triple no
/// thread can have memoised (a miss is never stored), so leaving the
/// generation alone keeps every memoised answer exact, exactly the argument
/// above. Before, every lazy bind of any native voided every thread's memo
/// on the default path, and the flip of the memo family would have had to
/// move this reader in step with `vm_exec`'s latch; now the flip is that
/// latch alone. The negative stamps that DO read a fresh bind
/// (`vm_exec::jni_site_probe_missed`, the two-miss rule) live inside the
/// opt-in, which already ran without the bump.
///
/// [`NativeRealm::jni_native_generation`]: crate::vm::realms::NativeRealm::jni_native_generation
#[inline]
pub(in crate::native::jni) fn jni_write_bumps_generation(changed: bool) -> bool {
    changed
}

/// Look up a JNI function pointer in a specific table.
pub fn find_jni_native_in(
    table: &JniNativeMethodTable,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<usize> {
    let key = jni_native_key(class_name, method_name, descriptor);
    table.read().get(&key).copied()
}

/// Store a JNI function pointer registered via `RegisterNatives` or symbol
/// lookup, in **this VM's** table.
pub fn register_jni_native(
    natives: &crate::vm::realms::NativeRealm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    fn_ptr: usize,
) {
    let replaced = register_jni_native_in(
        &natives.jni_native_methods,
        class_name,
        method_name,
        descriptor,
        fn_ptr,
    );
    // gce e1/j: after the write, so a memo validated against the new
    // generation can only have been filled from the new table.
    // gci i10/cr: a fresh key replaces no memoised answer
    // (`jni_write_bumps_generation`; gci i19/jt: with the memo on or off).
    if jni_write_bumps_generation(replaced) {
        natives
            .jni_native_generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }
}

/// Remove one triple's binding from `table`; whether it had one (j27 wave 8,
/// lane W8JNI, the `_in` half of [`clear_jni_native`]).
pub(in crate::native::jni) fn clear_jni_native_in(
    table: &JniNativeMethodTable,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let key = jni_native_key(class_name, method_name, descriptor);
    table.write().remove(&key).is_some()
}

/// HotSpot 25's `Method::clear_native_function` for a `RegisterNatives`
/// entry whose `fnPtr` is NULL (`Method::register_native`'s `else` arm): the
/// triple's binding goes away, so the next call links by name again, as
/// `UnregisterNatives` does per method (same generation rule).
pub(in crate::native::jni) fn clear_jni_native(
    natives: &crate::vm::realms::NativeRealm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) {
    let removed =
        clear_jni_native_in(&natives.jni_native_methods, class_name, method_name, descriptor);
    if jni_write_bumps_generation(removed) {
        natives
            .jni_native_generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }
}

/// [`register_jni_native`] of an address a JVMTI agent handed back from
/// `NativeMethodBind` in place of the one just bound (interpreter round i2
/// wave 63, lane L1): a deliberate replacement, so without the
/// re-registration warning, which HotSpot has no counterpart of.
#[cfg(feature = "experimental-debug")]
pub(crate) fn rebind_jni_native(
    natives: &crate::vm::realms::NativeRealm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    fn_ptr: usize,
) {
    let key = jni_native_key(class_name, method_name, descriptor);
    natives.jni_native_methods.write().insert(key, fn_ptr);
    natives
        .jni_native_generation
        .fetch_add(1, std::sync::atomic::Ordering::Release);
}

/// Look up a JNI function pointer for the given method in **this VM's** table.
/// Returns `None` if no pointer was registered.
///
/// gce e1/j (`gcd-d5f-jni-default-dispatch-costs-500ns`, item 1): a hit is
/// memoised per thread ([`JniFnMemo`], one entry) and served while the table's
/// registration generation ([`NativeRealm::jni_native_generation`]) is
/// unchanged -- three string compares and one atomic load instead of the
/// FNV hash of the triple, the table's read lock and the map probe, on every
/// dispatch of the same native from a loop. A miss is never memoised.
///
/// [`NativeRealm::jni_native_generation`]: crate::vm::realms::NativeRealm::jni_native_generation
pub fn find_jni_native(
    natives: &crate::vm::realms::NativeRealm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<usize> {
    let realm = natives as *const crate::vm::realms::NativeRealm as usize;
    // Read BEFORE the table: a registration racing with the lookup below then
    // leaves the memo on the old generation, which the next read refuses.
    let generation = natives
        .jni_native_generation
        .load(std::sync::atomic::Ordering::Acquire);
    let memo_hit = JNI_NATIVE_CALL
        .try_with(|t| {
            let memo = t.fn_memo.try_borrow().ok()?;
            memo.as_ref()
                .filter(|m| m.matches(realm, generation, class_name, method_name, descriptor))
                .map(|m| m.fn_ptr)
        })
        .ok()
        .flatten();
    if memo_hit.is_some() {
        return memo_hit;
    }
    let found = find_jni_native_in(
        &natives.jni_native_methods,
        class_name,
        method_name,
        descriptor,
    )?;
    let _ = JNI_NATIVE_CALL.try_with(|t| {
        if let Ok(mut memo) = t.fn_memo.try_borrow_mut() {
            let m = memo.get_or_insert_with(JniFnMemo::default);
            m.fill(
                realm,
                generation,
                class_name,
                method_name,
                descriptor,
                found,
            );
        }
    });
    Some(found)
}

/// gce e1/j: this thread's last [`find_jni_native`] hit. The strings are
/// compared by value (a `Class.method` name pair and a descriptor), the realm
/// by address and the generation, which is unique per VM, by value.
#[derive(Default)]
pub(in crate::native::jni) struct JniFnMemo {
    pub(in crate::native::jni) realm: usize,
    pub(in crate::native::jni) generation: u64,
    pub(in crate::native::jni) class_name: String,
    pub(in crate::native::jni) method_name: String,
    pub(in crate::native::jni) descriptor: String,
    pub(in crate::native::jni) fn_ptr: usize,
}

impl JniFnMemo {
    pub(in crate::native::jni) fn matches(
        &self,
        realm: usize,
        generation: u64,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> bool {
        self.fn_ptr != 0
            && self.realm == realm
            && self.generation == generation
            && self.method_name == method_name
            && self.descriptor == descriptor
            && self.class_name == class_name
    }

    /// Refill in place (the three `String` buffers are reused).
    pub(in crate::native::jni) fn fill(
        &mut self,
        realm: usize,
        generation: u64,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        fn_ptr: usize,
    ) {
        self.realm = realm;
        self.generation = generation;
        self.class_name.clear();
        self.class_name.push_str(class_name);
        self.method_name.clear();
        self.method_name.push_str(method_name);
        self.descriptor.clear();
        self.descriptor.push_str(descriptor);
        self.fn_ptr = fn_ptr;
    }
}

// ---------------------------------------------------------------------------
// JNI name mangling (JNI spec §11.3)
// ---------------------------------------------------------------------------

/// Encode a single component (class name or method name) for JNI symbol lookup.
/// JNI encoding rules:
///   `/` → `_`   (package separator)
///   `_` → `_1`  (literal underscore)
///   `;` → `_2`  (descriptor separator)
///   `[` → `_3`  (array prefix)
///   Unicode `\uXXXX` → `_0XXXX`
pub(in crate::native::jni) fn jni_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for ch in s.chars() {
        match ch {
            '/' => out.push('_'),
            '_' => out.push_str("_1"),
            ';' => out.push_str("_2"),
            '[' => out.push_str("_3"),
            // JNI spec 11.3: only alphanumerics survive as themselves. EVERY
            // other character takes the `_0XXXX` escape — it is not a
            // non-ASCII escape.
            //
            // This arm used to be `c if c.is_ascii() => out.push(c)`, which
            // passed `$` through verbatim. `$` is the separator in every
            // nested class's binary name, so for
            // `JdkOnlyPlatformProbe$JniProbe.add` the VM looked up
            // `Java_JdkOnlyPlatformProbe$JniProbe_add` while the compiler had
            // emitted `Java_JdkOnlyPlatformProbe_00024JniProbe_add` — verified
            // against `nm -D` on a library HotSpot binds from the same file.
            // dlsym never matched, and the method raised UnsatisfiedLinkError
            // with the library loaded and the symbol present. NO native on a
            // nested or inner class could bind by name, which is most of them:
            // the conventional shape is a package-private nested holder.
            c if c.is_ascii_alphanumeric() => out.push(c),
            c => {
                // Unicode escape: `_0xxxx`, ONE PER UTF-16 CODE UNIT -- HotSpot's
                // `NativeLookup::mangle_name_on` walks `jchar`s, so a character
                // outside the BMP is its two surrogates, `_0d83d_0de00`
                // (mcs21 review: `c as u32` printed `_1f600`'s five digits,
                // a symbol no compiler emits, so a native on such a class
                // could not bind by name).
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units).iter() {
                    out.push_str(&format!("_0{unit:04x}"));
                }
            }
        }
    }
    out
}

/// Build the JNI short name: `Java_<class>_<method>`.
///
/// Example: `("java/lang/System", "arraycopy")` → `"Java_java_lang_System_arraycopy"`.
pub fn jni_short_name(class_name: &str, method_name: &str) -> String {
    format!(
        "Java_{}_{}",
        jni_encode(class_name),
        jni_encode(method_name)
    )
}

/// Build the JNI long name: `Java_<class>_<method>__<encoded_params>`.
///
/// The parameter portion is the substring between `(` and `)` in the descriptor,
/// encoded with the same rules plus `;` → `_2` and `[` → `_3`.
///
/// Example: `("java/lang/System", "arraycopy", "(Ljava/lang/Object;ILjava/lang/Object;II)V")`
///   → `"Java_java_lang_System_arraycopy__Ljava_lang_Object_2ILjava_lang_Object_2II"`.
pub fn jni_long_name(class_name: &str, method_name: &str, descriptor: &str) -> String {
    let params = descriptor
        .strip_prefix('(')
        .and_then(|s| s.split(')').next())
        .unwrap_or("");
    format!(
        "Java_{}_{}__{}",
        jni_encode(class_name),
        jni_encode(method_name),
        jni_encode(params)
    )
}

/// Try to resolve a native method by JNI naming convention in loaded libraries
/// belonging to `loader_id` (`None` = unscoped/legacy).
///
/// Tries the short name first, then the long name (JNI spec §11.3 resolution order).
/// Raw libraries (`native_library_loaders[index] == None`) and libraries loaded by
/// other loaders are skipped (HotSpot `NativeLookup::lookup_style` parity).
/// If found, the function pointer is cached in `jni_lazy_linked_methods` keyed by
/// `(loader_id, triple)` so one loader's resolution never leaks to another.
pub fn resolve_jni_native_in_libraries(
    natives: &crate::vm::realms::NativeRealm,
    loader_id: u32,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<usize> {
    let cache_key = (loader_id, jni_native_key(class_name, method_name, descriptor));
    if let Some(&fn_ptr) = natives.jni_lazy_linked_methods.read().get(&cache_key) {
        return Some(fn_ptr);
    }

    let short = jni_short_name(class_name, method_name);
    let long = jni_long_name(class_name, method_name, descriptor);

    // mcs21 review: a library `RawNativeLibraries.unload0` retired is no
    // longer reachable (`find_native_symbol` refuses it too). Snapshotted
    // before the library lock, so the tombstone mutex is never nested in it.
    let unloaded = crate::vm::vm_exec::unloaded_native_library_indices(natives);
    let lib_loaders = natives.native_library_loaders.lock().clone();
    let libs = natives.native_libraries.lock();
    for symbol_name in [&short, &long] {
        let c_name = match std::ffi::CString::new(symbol_name.as_bytes()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for (index, lib) in libs.iter().enumerate() {
            if unloaded.contains(&index) {
                continue;
            }
            if lib_loaders.get(index).copied().flatten() != Some(loader_id) {
                continue;
            }
            // Safety: we are looking up a symbol name that follows JNI conventions.
            if let Ok(sym) = unsafe { lib.get::<*const ()>(c_name.as_bytes_with_nul()) } {
                let fn_ptr = *sym as usize;
                if fn_ptr != 0 {
                    drop(libs);
                    natives.jni_lazy_linked_methods.write().insert(cache_key, fn_ptr);
                    tracing::info!(
                        symbol = %symbol_name,
                        loader_id,
                        method = %format!("{}.{}{}", class_name, method_name, descriptor),
                        "Auto-resolved native method via JNI naming convention"
                    );
                    return Some(fn_ptr);
                }
            }
        }
    }
    None
}

/// The `jlong` bits for one Java `long` argument of a JNI call — with an
/// `Unsafe`-arena handle translated to the real address it names.
///
/// This is the second door of the problem
/// [`direct_buffer_native_address`](fn@direct_buffer_native_address) documents
/// for the first. `Unsafe.allocateMemory` on this VM returns a tagged handle,
/// not an OS pointer; `GetDirectBufferAddress` already translates one before
/// handing it to C, but a library that never calls that entry point and instead
/// takes the address as a plain `long` parameter got the raw handle. Netty's
/// `netty-tcnative` is written exactly that way — `SSL.bioWrite(long bio, long
/// address, int len)` — so the first BIO write of the first TLS handshake
/// reached `BUF_MEM_append` with `0x4000_0010_0000_0010` in RSI and took a
/// SIGSEGV inside BoringSSL, with no CratonVM frame at the fault to say why.
///
/// Only a value that is BOTH tagged (bit 62 — never set on a real pointer on
/// any target this VM builds for) AND inside a live arena block is rewritten,
/// so a genuine `long` payload is untouched unless it lands in the few-GiB
/// window of currently-live arena addresses, which no real datum does.
///
/// A tagged handle that no live block covers — a use-after-free, or an offset
/// past the end — is passed through unchanged rather than silently redirected
/// to some other block: the native then faults on the handle, which is the
/// same visible failure it has today, instead of quietly reading the wrong
/// object's bytes.
#[inline]
pub(in crate::native::jni) fn jni_long_arg_bits(raw: i64) -> u64 {
    if !cratonvm_native_builtins::unsafe_arena_addr_is_tagged(raw) {
        return raw as u64;
    }
    match cratonvm_native_builtins::unsafe_arena_real_ptr(raw) {
        Some((ptr, _remaining)) => ptr as usize as u64,
        None => {
            tracing::warn!(
                handle = format!("{raw:#x}"),
                "JNI long argument is a dead Unsafe-arena handle — passing it through untranslated"
            );
            raw as u64
        }
    }
}

/// Dispatch a JNI native function pointer call.
///
/// Converts `Value` args to 64-bit C values (correct for all integer/reference
/// types on x86-64). Float/double args are passed as their bit representation
/// in integer registers — this is ABI-correct on Windows x64, but not on
/// Linux x86-64 System V for the float/double parameter positions.
///
/// Java `long` arguments pass through [`jni_long_arg_bits`], which turns an
/// `Unsafe`-arena handle into the real address it names — see that function for
/// why the `GetDirectBufferAddress` translation alone was not enough.
///
/// # Safety
/// `fn_ptr` must be a valid JNI native function whose signature matches
/// `descriptor`, and the TLS JNI context must already be set.
pub unsafe fn dispatch_jni_native(
    fn_ptr: usize,
    env: JNIEnv,
    receiver: JObject,
    args: &[Value],
    descriptor: &str,
) -> Value {
    dispatch_jni_native_census(fn_ptr, env, receiver, args, descriptor, None)
}

/// [`dispatch_jni_native`] with the dispatching VM's JNI phase census
/// ([`JniPhaseCensus`], gce e1/j): `vm_exec`'s JNI door passes it, so the
/// phases inside the dispatch are timed without a thread-local lookup. `None`
/// or a census that is off times nothing.
///
/// # Safety
/// As [`dispatch_jni_native`].
pub(crate) unsafe fn dispatch_jni_native_census(
    fn_ptr: usize,
    env: JNIEnv,
    receiver: JObject,
    args: &[Value],
    descriptor: &str,
    census: Option<&JniPhaseCensus>,
) -> Value {
    let census = census.filter(|c| c.on);
    let mut clock = census.and_then(JniPhaseCensus::start);
    let lap = |phase: JniPhase, clock: &mut Option<std::time::Instant>| {
        if let Some(c) = census {
            c.lap(phase, clock);
        }
    };
    // gcd d10/j: the tags in place (`param_type_tags`), not a cached copy.
    let param_types = param_type_tags(descriptor);
    // gcj j2/jn2 (cut 1, `CRATONVM_JNI_SWITCHES_ONCE_PER_CALL`, default ON
    // since WARNING t1-user-flip-20261026, `=0` kills): the
    // VM's switches, resolved here. Off, this is the one resolution the band
    // check below always made (moved up); on, it is the only one of the
    // dispatch, passed to the local recording and the in-native entry.
    let switches = jni_switches();
    let once = switches.switches_once_per_call;
    // `(indirect, encode)`: the local recording rule, resolved once (cut 1).
    let once_rule = once.then(|| {
        (
            indirect_locals_active_with(&switches),
            locals_are_indirect_here_with(&switches),
        )
    });
    // gcd d3/k (`CRATONVM_JNI_NATIVE_TRANSITIONS`): read before the receiver
    // and the arguments are recorded, so a raw one among them keeps this call
    // out of native (`JniNativeCall`).
    let escapes_before = raw_local_escapes();

    // Build a type-tagged argument list. `env` and `receiver` are always the
    // first two integer-class arguments; the Java args follow, each tagged
    // integer (GP register class) or floating-point (SSE register class) so the
    // platform calling convention can route them to the correct registers.
    // gcd d10/j: on the stack up to 14 Java arguments (a heap `Vec` per call
    // before); the trampoline reads a slice either way.
    let mut jargs: smallvec::SmallVec<[JniArg; 16]> =
        smallvec::SmallVec::with_capacity(args.len() + 2);
    // `env` is a raw pointer (`*const *const usize`); flatten it to a 64-bit
    // integer-class word. `receiver` is already a `JObject` (u64 handle).
    jargs.push(JniArg::int(env as usize as u64));
    // The receiver is a local ref for the duration of the call, like the
    // object arguments below (gc-common w3-g; `new_local_handle`). A static
    // native's `jclass` is skipped by `record_local_handle`. With
    // `CRATONVM_JNI_INDIRECT_LOCALS` the native gets an indirect handle to it
    // (gc-common w6-g); a `jclass` comes back unchanged.
    let receiver = if let Some((indirect, encode)) = once_rule {
        record_local_handle_with(receiver, indirect, encode)
    } else if indirect_locals_active() {
        record_local_handle_indirect(receiver)
    } else {
        record_local_handle(receiver);
        receiver
    };
    jargs.push(JniArg::int(receiver));
    for (v, tag) in args.iter().zip(param_types) {
        let a = match v {
            Value::Int(i) => JniArg::int(*i as i64 as u64),
            Value::Long(l) => JniArg::int(jni_long_arg_bits(*l)),
            // Float/double are SSE-class: their bit pattern must go in an XMM
            // register on SysV (and in the positionally-shared XMM slot on
            // Win64), not in a GP register.
            Value::Float(f) => JniArg::float(f.to_bits() as u64),
            Value::Double(d) => JniArg::float(d.to_bits()),
            Value::Object(Some(r)) => JniArg::int(new_local_handle_ruled(*r, once_rule)),
            Value::Object(None) => JniArg::int(0u64),
            // Defensive: an operand of an unexpected variant. Fall back to the
            // declared descriptor tag to keep the register class correct.
            _ => {
                if tag == b'F' || tag == b'D' {
                    JniArg::float(0)
                } else {
                    JniArg::int(0)
                }
            }
        };
        jargs.push(a);
    }

    // Whether the return value comes back in XMM0 (F/D) rather than RAX.
    let ret_char = descriptor
        .rfind(')')
        .and_then(|i| descriptor.as_bytes().get(i + 1).copied())
        .unwrap_or(b'V');
    let fp_return = ret_char == b'F' || ret_char == b'D';

    // gcd d3/k: the C call runs in native (GC-safe) when the flag is on and
    // every local it can hold is indirect; the guard leaves native BEFORE the
    // result below is decoded, so an indirect result resolves through the
    // remapped table. Inert (one latched load) with the flag off.
    // gcd d4/k2 (lane m's request 1): count this call in the VM's pause
    // ledger while the native may hold RAW locals -- decided here, after the
    // receiver and arguments were recorded, so a raw one among them counts.
    // Held to the end of the dispatch, i.e. past the in-native leave below.
    // One cached flag load unless the pinned young copy or option B is on.
    lap(JniPhase::ArgMarshal, &mut clock);
    let _raw_locals = RawLocalsDispatch::enter();
    lap(JniPhase::RawLocals, &mut clock);
    // gch h4/bp (`CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE`): the compiled caller's
    // callee-saved registers, captured in THIS frame (the one that makes the C
    // call) BEFORE the entry below raises anything, and kept live until the
    // call has left native. A value the caller keeps only in such a register
    // is then inside the band the pause scans (`in_native_band_at_pause`),
    // whatever the C code does with the register. An `if`, not
    // `bool::then(capture)`: the capture must run in this frame.
    // gcj j2/jn2: the switches resolved above (this was a separate
    // resolution of the same switch). JIT round 21 wave 19 (lane jnilazy,
    // FF3-3): the lazy in-native state needs the same spill, for the same
    // reason (`in_native_lazy_band`).
    let band_spill = if switches.in_native_band_at_pause || switches.in_native_lazy {
        Some(cratonvm_native_builtins::panama_libffi::CalleeSavedSpill::capture())
    } else {
        None
    };
    let in_native = if once {
        JniNativeCall::enter_banded_with(&switches, escapes_before, band_spill.as_ref())
    } else {
        JniNativeCall::enter_banded(escapes_before, band_spill.as_ref())
    };
    lap(JniPhase::EnterNative, &mut clock);
    let raw_result = call_jni_marshalled(fn_ptr, &jargs[..], fp_return);
    lap(JniPhase::NativeBody, &mut clock);
    drop(in_native);
    if let Some(spill) = &band_spill {
        spill.keep_alive();
    }
    lap(JniPhase::LeaveNative, &mut clock);
    // Charged at every return below (a `Drop` guard: the match returns).
    struct DecodeClock<'a>(Option<&'a JniPhaseCensus>, Option<std::time::Instant>);
    impl Drop for DecodeClock<'_> {
        fn drop(&mut self) {
            if let Some(c) = self.0 {
                c.stop(JniPhase::ResultDecode, self.1);
            }
        }
    }
    let _decode_clock = DecodeClock(census, clock);

    // `ret_char` was already computed above to decide `fp_return`.
    match ret_char {
        b'V' => Value::Object(None),
        // `jboolean` is a byte, and Java's `boolean` is 0 or 1, so the return
        // has to be normalised — but by VALUE, the way HotSpot's native
        // wrapper does it (`movzbl` then `setne`), not by the low BIT. A
        // native that returns a flag word rather than a literal `JNI_TRUE`
        // — `return flags & MASK;` is ordinary C — hands back something like
        // `0x80`, which `& 1` reads as FALSE and HotSpot reads as true.
        b'Z' => Value::Int((raw_result as u8 != 0) as i32),
        b'B' => Value::Int(raw_result as i8 as i32),
        b'C' => Value::Int(raw_result as u16 as i32),
        b'S' => Value::Int(raw_result as i16 as i32),
        b'I' => Value::Int(raw_result as i32),
        b'J' => {
            // Long-smuggle mint chokepoint: a native that received a raw
            // jobject handle (obj_to_jobject = the object's address) may echo
            // it back as its jlong return — the classic long-as-jobject
            // smuggle. Register the exact value so the GC's value-stack
            // rewrite arm can distinguish this genuine handle from a
            // primitive long that merely collides with a heap address (which
            // must NOT be rewritten). Strict object-start probe: only real
            // handles register; ordinary numeric returns don't look like
            // aligned live object bases.
            let bits = raw_result;
            if bits != 0 && bits & 0x7 == 0 {
                with_shared_vm(|shared| {
                    if shared.mem.heap.is_object_address(bits as usize).is_some() {
                        crate::memory::smuggled_longs::record_minted_long(&shared.mem.heap, bits);
                    }
                    Some(())
                });
            }
            Value::Long(raw_result as i64)
        }
        b'F' => Value::Float(f32::from_bits(raw_result as u32)),
        b'D' => Value::Double(f64::from_bits(raw_result)),
        _ => match jobject_to_obj(raw_result) {
            Some(r) => Value::Object(Some(r)),
            None => Value::Object(None),
        },
    }
}

/// A single C-ABI argument together with its register class.
///
/// The register class decides whether the 64-bit `bits` value is passed in a
/// general-purpose register (integers, pointers, JNI handles) or an SSE/XMM
/// register (float/double). This distinction is invisible once a value is
/// flattened to a bare `u64`, which is why the previous implementation passed
/// floats in integer registers — correct only on the Windows x64 ABI, wrong on
/// System V (Linux/macOS) where FP args use XMM0–XMM7.
#[derive(Clone, Copy)]
pub(in crate::native::jni) struct JniArg {
    pub(in crate::native::jni) bits: u64,
    /// `true` if this argument is floating-point (SSE register class).
    pub(in crate::native::jni) is_fp: bool,
}

impl JniArg {
    #[inline]
    pub(in crate::native::jni) fn int(bits: u64) -> Self {
        JniArg { bits, is_fp: false }
    }
    #[inline]
    pub(in crate::native::jni) fn float(bits: u64) -> Self {
        JniArg { bits, is_fp: true }
    }
}

/// Validate a JNI native function pointer before calling through it.
/// Returns `false` (and logs) for a null pointer, and for an entry address the
/// TARGET ISA cannot execute.
///
/// "Cannot execute" is an architecture fact, not a style preference, and the
/// two are not interchangeable. x86-64 instructions have NO alignment
/// requirement at all: a function may legally start on an odd byte, and gcc
/// does exactly that for small leaf functions — `cc -shared -fPIC -O1` on the
/// strict corpus’s own JNI fixture put `add` at `0x11f7`, `mulLong` at
/// `0x11ff` and `scale` at `0x120b`. The `fn_ptr % 2 != 0` check this function
/// used to apply therefore REFUSED seven of the twelve natives in that library
/// and returned 0 for each, which the Java side read as `add(40,2) == 0` —
/// silent data corruption produced by a guard, from a correctly resolved
/// pointer into correctly compiled code. Measured 2026-08-06 against the
/// HotSpot 25 arm binding the same `.so`.
///
/// aarch64 is different and keeps its check: A64 instructions are fixed-width
/// and must be 4-byte aligned, so an unaligned entry there really would fault.
#[inline]
pub(in crate::native::jni) fn jni_fn_ptr_ok(fn_ptr: usize) -> bool {
    if fn_ptr == 0 {
        tracing::error!("JNI call with null function pointer — returning 0");
        return false;
    }
    #[cfg(target_arch = "aarch64")]
    if fn_ptr % 4 != 0 {
        tracing::error!(
            "JNI call with misaligned function pointer {:#x} — returning 0",
            fn_ptr
        );
        return false;
    }
    true
}

/// Call a JNI native function pointer, marshalling each argument into the
/// correct register class (GP vs XMM) and spilling any overflow to the stack,
/// per the platform calling convention. `args` already includes `env` and the
/// receiver/class as its first two (integer) entries.
///
/// On `x86_64` this routes through a small assembly trampoline that honours the
/// active ABI (Windows x64 or System V), so float/double arguments land in XMM
/// registers and calls with more than the register-resident argument count
/// correctly spill the remainder to the stack. When `fp_return` is set, the
/// XMM0 result is returned in place of RAX so the caller can reinterpret it as
/// a float/double.
///
/// On non-`x86_64` targets we keep the previous fixed-arity integer fast path
/// (correct for integer/reference args up to the register limit) and fail loud
/// for the float/double or large-arity cases we cannot honour without an
/// architecture-specific trampoline — never fabricating a 0/null result.
///
/// # Safety
/// `fn_ptr` must be a valid JNI native function whose C signature matches the
/// argument register classes described by `args` and the requested return kind.
pub(in crate::native::jni) unsafe fn call_jni_marshalled(fn_ptr: usize, args: &[JniArg], fp_return: bool) -> u64 {
    if !jni_fn_ptr_ok(fn_ptr) {
        return 0;
    }

    // Fast path: integer/reference-only calls (no FP arg, no FP return) are
    // dispatched with the proven fixed-arity `extern "C"` transmutes. The
    // compiler emits the correct ABI sequence for these, INCLUDING spilling any
    // integer overflow past the register file to the stack, so this is valid for
    // every all-integer arity the helper enumerates (up to MAX_INT_FAST total
    // args). This keeps the overwhelmingly common JNI call shape on the
    // well-tested path and confines the assembly trampoline strictly to the
    // cases that path cannot express: float/double arguments or return values.
    const MAX_INT_FAST: usize = 8; // highest fixed-arity arm in call_jni_fn_ptr_int
    if !fp_return && args.len() <= MAX_INT_FAST && !args.iter().any(|a| a.is_fp) {
        return call_jni_fn_ptr_int(fn_ptr, args);
    }

    #[cfg(target_arch = "x86_64")]
    {
        // Partition the arguments into GP-register, XMM-register and stack-spill
        // slots according to the active x86-64 ABI.
        //
        //  * Windows x64: the first FOUR arguments go in registers and integer
        //    vs FP shares one positional counter — i.e. argument N (0-based) uses
        //    GP slot N if integer or XMM slot N if FP, and any argument with
        //    N >= 4 spills to the stack. Each register arg still reserves its
        //    32-byte "shadow space" slot, which the trampoline allocates.
        //  * System V: integers and FP have INDEPENDENT counters — up to 6 GP
        //    registers (RDI,RSI,RDX,RCX,R8,R9) and up to 8 XMM registers
        //    (XMM0–7); anything beyond a class's register budget spills.
        let mut gp: [u64; 6] = [0; 6];
        let mut xmm: [u64; 8] = [0; 8];
        let mut stack: Vec<u64> = Vec::new();

        #[cfg(target_os = "windows")]
        {
            const MAX_REG: usize = 4; // RCX/RDX/R8/R9 share positions with XMM0–3
            let mut n_gp = 0usize;
            let mut n_xmm = 0usize;
            for (pos, a) in args.iter().enumerate() {
                if pos < MAX_REG {
                    if a.is_fp {
                        xmm[pos] = a.bits;
                        n_xmm = n_xmm.max(pos + 1);
                    } else {
                        gp[pos] = a.bits;
                        n_gp = n_gp.max(pos + 1);
                    }
                } else {
                    stack.push(a.bits);
                }
            }
            jni_trampoline_call(fn_ptr, &gp, n_gp, &xmm, n_xmm, &stack, fp_return)
        }

        #[cfg(not(target_os = "windows"))]
        {
            let mut n_gp = 0usize;
            let mut n_xmm = 0usize;
            for a in args.iter() {
                if a.is_fp {
                    if n_xmm < xmm.len() {
                        xmm[n_xmm] = a.bits;
                        n_xmm += 1;
                    } else {
                        stack.push(a.bits);
                    }
                } else if n_gp < gp.len() {
                    gp[n_gp] = a.bits;
                    n_gp += 1;
                } else {
                    stack.push(a.bits);
                }
            }
            jni_trampoline_call(fn_ptr, &gp, n_gp, &xmm, n_xmm, &stack, fp_return)
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        // No architecture-specific trampoline. We can still correctly dispatch
        // integer/reference-only calls that fit the GP register file via fixed
        // -arity transmutes; FP args or oversized arg lists cannot be honoured
        // safely here, so we fail loud rather than fabricate a result.
        if args.iter().any(|a| a.is_fp) || fp_return {
            tracing::error!(
                "JNI native dispatch with float/double argument or return type is \
                 unsupported on this architecture (no FP-aware trampoline) — \
                 refusing to call to avoid an ABI mismatch"
            );
            jni_throw_unsatisfied_link(
                "float/double JNI signatures require an FP-aware trampoline on this architecture",
            );
            return 0;
        }
        call_jni_fn_ptr_int(fn_ptr, args)
    }
}

/// x86-64 assembly trampoline driver.
///
/// Lays out the GP registers, XMM registers and any stack-spill words into a
/// single contiguous control block and tail-calls the naked trampoline, which
/// performs the actual register loads and the `call`. The naked trampoline
/// stores XMM0 back into the control block on return so we can surface an
/// FP result.
#[cfg(target_arch = "x86_64")]
#[inline]
pub(in crate::native::jni) unsafe fn jni_trampoline_call(
    fn_ptr: usize,
    gp: &[u64; 6],
    n_gp: usize,
    xmm: &[u64; 8],
    n_xmm: usize,
    stack: &[u64],
    fp_return: bool,
) -> u64 {
    // Control block consumed by the naked trampoline. Field order/offsets are
    // mirrored exactly by the assembly below — DO NOT reorder without updating
    // the offsets there.
    #[repr(C)]
    struct CallBlock {
        fn_ptr: u64,    // +0
        gp: [u64; 6],   // +8
        xmm: [u64; 8],  // +56
        stack_ptr: u64, // +120  (pointer to first stack word, or null)
        n_stack: u64,   // +128  (count of stack words)
        n_xmm: u64,     // +136  (used for AL: # of vector regs, SysV varargs)
        xmm0_ret: u64,  // +144  (out: XMM0 result)
    }

    let mut block = CallBlock {
        fn_ptr: fn_ptr as u64,
        gp: *gp,
        xmm: *xmm,
        stack_ptr: if stack.is_empty() {
            0
        } else {
            stack.as_ptr() as u64
        },
        n_stack: stack.len() as u64,
        n_xmm: n_xmm as u64,
        xmm0_ret: 0,
    };
    let _ = n_gp; // GP regs are always loaded (unused entries are 0); kept for clarity.

    let rax = jni_naked_trampoline(&mut block as *mut CallBlock as *mut u8);
    if fp_return {
        block.xmm0_ret
    } else {
        rax
    }
}

// Naked x86-64 trampoline. Receives a single pointer to the `CallBlock` in the
// first integer-argument register of the active ABI (RCX on Windows, RDI on
// System V). It:
//   1. computes the stack space for spilled args (+ 32 bytes shadow space on
//      Windows), keeping 16-byte alignment at the `call`,
//   2. copies the stack-spill words into place,
//   3. loads the GP and XMM argument registers from the block,
//   4. sets AL = n_xmm (required by the System V variadic convention; ignored
//      by the Windows ABI),
//   5. `call`s the target, then stores XMM0 into the block and returns RAX.
//
// Field offsets MUST match `CallBlock` above. Two fully separate, item-level
// `cfg`-gated `global_asm!` blocks are used (rather than per-line `cfg`
// attributes inside one `global_asm!`, which are not supported) so the Windows
// x64 and System V variants are each unambiguous.
//
// Common prologue/epilogue note: after `push rbp; mov rbp,rsp; push rbx; push
// r12`, the saved-register window is [rbp]=rbp, [rbp-8]=rbx, [rbp-16]=r12, so
// the epilogue restores RSP with `lea rsp,[rbp-16]` before popping r12/rbx/rbp.

#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
core::arch::global_asm!(
    ".p2align 4",
    ".globl jni_naked_trampoline_asm",
    "jni_naked_trampoline_asm:",
    "push rbp",
    "mov rbp, rsp",
    "push rbx",
    "push r12",
    "mov rbx, rcx",         // block ptr (Win64 arg0 = RCX)
    "mov r12, [rbx + 128]", // r12 = n_stack
    "mov rax, r12",
    "shl rax, 3",  // bytes for stack args
    "add rax, 32", // + 32-byte shadow space (Win64)
    "sub rsp, rax",
    "and rsp, -16",        // 16-byte align at the call
    "mov r8, [rbx + 120]", // r8 = stack_ptr (source, may be 0)
    "test r12, r12",
    "jz 2f",
    "test r8, r8",
    "jz 2f",
    "mov r9, rsp",
    "add r9, 32", // dest = above the shadow space
    "xor r10, r10",
    "1:",
    "mov rax, [r8 + r10*8]",
    "mov [r9 + r10*8], rax",
    "inc r10",
    "cmp r10, r12",
    "jb 1b",
    "2:",
    "movq xmm0, [rbx + 56]",
    "movq xmm1, [rbx + 64]",
    "movq xmm2, [rbx + 72]",
    "movq xmm3, [rbx + 80]",
    // XMM4–7 are loaded too (harmless on Win64; only XMM0–3 are arg regs).
    "movq xmm4, [rbx + 88]",
    "movq xmm5, [rbx + 96]",
    "movq xmm6, [rbx + 104]",
    "movq xmm7, [rbx + 112]",
    "mov rax, [rbx + 136]", // AL = n_xmm (ignored by Win64; set for uniformity)
    "mov r11, [rbx + 0]",   // target fn ptr
    "mov rcx, [rbx + 8]",   // gp[0]
    "mov rdx, [rbx + 16]",  // gp[1]
    "mov r8,  [rbx + 24]",  // gp[2]
    "mov r9,  [rbx + 32]",  // gp[3]
    "call r11",
    "movq [rbx + 144], xmm0", // capture FP return
    "lea rsp, [rbp - 16]",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
);

#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
core::arch::global_asm!(
    ".p2align 4",
    ".globl jni_naked_trampoline_asm",
    "jni_naked_trampoline_asm:",
    "push rbp",
    "mov rbp, rsp",
    "push rbx",
    "push r12",
    "mov rbx, rdi",         // block ptr (SysV arg0 = RDI)
    "mov r12, [rbx + 128]", // r12 = n_stack
    "mov rax, r12",
    "shl rax, 3", // bytes for stack args (no shadow space on SysV)
    "sub rsp, rax",
    "and rsp, -16",        // 16-byte align at the call
    "mov r8, [rbx + 120]", // r8 = stack_ptr (source, may be 0)
    "test r12, r12",
    "jz 2f",
    "test r8, r8",
    "jz 2f",
    "mov r9, rsp", // dest = [rsp] (no shadow space)
    "xor r10, r10",
    "1:",
    "mov rax, [r8 + r10*8]",
    "mov [r9 + r10*8], rax",
    "inc r10",
    "cmp r10, r12",
    "jb 1b",
    "2:",
    "movq xmm0, [rbx + 56]",
    "movq xmm1, [rbx + 64]",
    "movq xmm2, [rbx + 72]",
    "movq xmm3, [rbx + 80]",
    "movq xmm4, [rbx + 88]",
    "movq xmm5, [rbx + 96]",
    "movq xmm6, [rbx + 104]",
    "movq xmm7, [rbx + 112]",
    "mov rax, [rbx + 136]", // AL = n_xmm (SysV variadic FP-reg count)
    "mov r11, [rbx + 0]",   // target fn ptr
    "mov rdi, [rbx + 8]",   // gp[0]
    "mov rsi, [rbx + 16]",  // gp[1]
    "mov rdx, [rbx + 24]",  // gp[2]
    "mov rcx, [rbx + 32]",  // gp[3]
    "mov r8,  [rbx + 40]",  // gp[4]
    "mov r9,  [rbx + 48]",  // gp[5]
    "call r11",
    "movq [rbx + 144], xmm0", // capture FP return
    "lea rsp, [rbp - 16]",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
);

#[cfg(target_arch = "x86_64")]
extern "C" {
    // The naked trampoline defined in `global_asm!` above. Takes the control
    // block pointer, returns the integer (RAX) result.
    fn jni_naked_trampoline_asm(block: *mut u8) -> u64;
}

#[cfg(target_arch = "x86_64")]
#[inline]
pub(in crate::native::jni) unsafe fn jni_naked_trampoline(block: *mut u8) -> u64 {
    jni_naked_trampoline_asm(block)
}

/// Integer/reference-only dispatch via fixed-arity `extern "C"` transmutes, so
/// the compiler emits the correct platform register layout. This is the fast
/// path on `x86_64` for all-integer calls that fit in GP registers, and the
/// only dispatch mechanism on non-`x86_64` targets (which lack the assembly
/// trampoline). Calls with more arguments than the register file can hold are
/// not supported here and fail loud (UnsatisfiedLinkError) rather than silently
/// returning 0.
///
/// # Safety
/// Every entry of `args` must be an integer/reference-class value; `fn_ptr`
/// must match a signature receiving them all as 64-bit integers.
pub(in crate::native::jni) unsafe fn call_jni_fn_ptr_int(fn_ptr: usize, args: &[JniArg]) -> u64 {
    // args[0]=env, args[1]=recv, args[2..]=the Java params.
    let g = |i: usize| args[i].bits;
    match args.len() {
        0 => {
            let f: extern "C" fn() -> u64 = std::mem::transmute(fn_ptr);
            f()
        }
        1 => {
            let f: extern "C" fn(u64) -> u64 = std::mem::transmute(fn_ptr);
            f(g(0))
        }
        2 => {
            let f: extern "C" fn(u64, u64) -> u64 = std::mem::transmute(fn_ptr);
            f(g(0), g(1))
        }
        3 => {
            let f: extern "C" fn(u64, u64, u64) -> u64 = std::mem::transmute(fn_ptr);
            f(g(0), g(1), g(2))
        }
        4 => {
            let f: extern "C" fn(u64, u64, u64, u64) -> u64 = std::mem::transmute(fn_ptr);
            f(g(0), g(1), g(2), g(3))
        }
        5 => {
            let f: extern "C" fn(u64, u64, u64, u64, u64) -> u64 = std::mem::transmute(fn_ptr);
            f(g(0), g(1), g(2), g(3), g(4))
        }
        6 => {
            let f: extern "C" fn(u64, u64, u64, u64, u64, u64) -> u64 = std::mem::transmute(fn_ptr);
            f(g(0), g(1), g(2), g(3), g(4), g(5))
        }
        7 => {
            let f: extern "C" fn(u64, u64, u64, u64, u64, u64, u64) -> u64 =
                std::mem::transmute(fn_ptr);
            f(g(0), g(1), g(2), g(3), g(4), g(5), g(6))
        }
        8 => {
            let f: extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> u64 =
                std::mem::transmute(fn_ptr);
            f(g(0), g(1), g(2), g(3), g(4), g(5), g(6), g(7))
        }
        n => {
            tracing::error!(
                "JNI call with {} total args exceeds the supported register count on this \
                 architecture (no stack-spill trampoline) — raising UnsatisfiedLinkError",
                n
            );
            jni_throw_unsatisfied_link(
                "JNI signatures with more register-resident integer arguments than this \
                 architecture supports require a stack-spilling trampoline",
            );
            0
        }
    }
}

/// Raise an `UnsatisfiedLinkError` on the current thread so an unsupported
/// native dispatch surfaces as a real Java exception rather than a fabricated
/// 0/null return.
///
/// Mirrors [`raise_jni_region_error`]: when a `JvmThread` context is available we
/// materialise a real exception object and store its handle in
/// `JNI_PENDING_EXCEPTION`, so `vm_exec` rethrows it on return from the native
/// call. Without a thread context (e.g. a direct unit-test call) we fall back
/// to the `ThrowNew` sentinel so the error is flagged rather than swallowed.
pub(in crate::native::jni) fn jni_throw_unsatisfied_link(msg: &str) {
    let full = format!("Unsupported native method ABI: {msg}");
    let raised =
        with_jni_context(
            |shared, thread| match crate::runtime::exceptions::create_exception_object(
                shared,
                thread,
                "java/lang/UnsatisfiedLinkError",
                Some(&full),
            ) {
                Ok(exc) => {
                    set_jni_pending_exception_object(exc);
                    Ok(())
                }
                Err(e) => Err(is_heap_exhaustion(&e)),
            },
        )
        .unwrap_or(Err(false));
    if let Err(heap_exhausted) = raised {
        pend_unbuilt_throwable(heap_exhausted);
    }
}


pub(in crate::native::jni) extern "C" fn jni_register_natives(
    _env: JNIEnv,
    clazz: JClass,
    methods: *const JNINativeMethod,
    n_methods: JInt,
) -> JInt {
    let _fx = ForeignJniEntry::enter();
    if methods.is_null() || n_methods <= 0 || clazz == 0 {
        return JNI_OK;
    }

    // A `JClass` in this table IS a `ClassId` — that is what `FindClass`
    // returns (`class_id_to_jclass`, a `ClassId` in the low 32 bits under
    // `JCLASS_TAG`) and what GetSuperclass, IsAssignableFrom, GetFieldID,
    // CallStaticXxxMethod and a dozen others decode, all through
    // `jclass_class_id` (which also decodes a mirror handle, local, global or
    // weak; gci i14/jh: the `ClassId::new(clazz as u32)` truncation this
    // comment used to name survives only for a tagged handle).
    //
    // This function decoded it as an OBJECT HANDLE instead, which is the one
    // convention `FindClass` never produces. Every odd-numbered ClassId took
    // the global-ref branch of `jobject_to_obj` and logged "handle 0x2cb not
    // found in global ref table"; every even one failed the heap-address
    // check. Either way `class_name` was None and RegisterNatives returned
    // JNI_ERR — so the ENTIRE RegisterNatives path was dead for the canonical
    // `FindClass` + `RegisterNatives` idiom that every JNI_OnLoad uses. It was
    // invisible because the library still loads, the symbol-bound natives
    // still resolve by name, and only the methods a library binds
    // exclusively through RegisterNatives raise UnsatisfiedLinkError.
    // Resolved BEFORE the class-manager read: `jclass_class_id` may take the
    // mirror-reverse lock, and nesting that inside a `class_manager` reader is
    // a lock order this file does not otherwise establish.
    let class_id = jclass_class_id(clazz);
    let class_name = with_shared_vm(|shared| {
        shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.clone())
    })
    .flatten();

    let class_name = match class_name {
        Some(n) => n,
        None => return JNI_ERR,
    };

    // j27 wave 1 (lane JNINAT, `jr21-jni-registernatives-unknown-method-
    // answers-ok-20261014.md`): each entry is checked against the class as
    // HotSpot's `Method::register_native` checks it, IN ORDER; the entries
    // before the first failing one stay bound (HotSpot binds as it goes), the
    // failing one leaves `NoSuchMethodError` pending and the call answers
    // `JNI_ERR`. `CRATONVM_JNI_REGISTERNATIVES_EXACT=0` restores the old
    // "bind every triple under the named class, answer JNI_OK".
    let exact = true;
    let mut bindings = Vec::with_capacity(n_methods as usize); // Cast: positive, checked above
    let mut refusal: Option<String> = None;
    // j27 wave 8 (lane W8DSP): HotSpot's `do_warning`, decided once per call.
    let rebind_class = with_jni_context(|shared, thread| {
        jni_platform_rebind::platform_rebind_class(shared, thread, class_id)
    })
    .flatten();
    let warn_rebind = |name: &str, sig: &str| {
        if let Some(external) = rebind_class.as_deref() {
            jni_platform_rebind::warn_platform_rebind(external, name, sig);
        }
    };
    for i in 0..n_methods as usize {
        // Safety: caller guarantees `methods` points to `n_methods` valid elements
        let m = unsafe { &*methods.add(i) };
        if m.name.is_null() || m.signature.is_null() {
            // HotSpot dereferences both (`strlen`); skipping is the lenient
            // answer for a contract violation, unchanged.
            continue;
        }
        // Safety: name and signature are null-terminated C strings from the JNI caller
        let name = unsafe { CStr::from_ptr(m.name).to_string_lossy().into_owned() };
        let sig = unsafe { CStr::from_ptr(m.signature).to_string_lossy().into_owned() };
        let bind_class = if exact {
            let target =
                with_shared_vm(|shared| register_natives_target(shared, class_id, &name, &sig));
            match target.unwrap_or(RegisterNativesTarget::Lenient) {
                RegisterNativesTarget::Bind(declaring) => declaring,
                RegisterNativesTarget::Lenient => class_name.to_string(),
                RegisterNativesTarget::Refuse(msg) => {
                    // HotSpot warns before `Method::register_native` refuses.
                    warn_rebind(&name, &sig);
                    refusal = Some(msg);
                    break;
                }
                RegisterNativesTarget::RefuseUnknownSymbol(msg) => {
                    // The symbol-table probe throws before the warning.
                    refusal = Some(msg);
                    break;
                }
            }
        } else {
            class_name.to_string()
        };
        warn_rebind(&name, &sig);
        if m.fn_ptr.is_null() && !exact {
            // Switch off: a NULL `fnPtr` keeps the previous binding (the old
            // answer).
            continue;
        }
        // j27 wave 8 (lane W8JNI): under the switch a NULL `fnPtr` is kept as
        // `0`, which the bind loop below turns into HotSpot's
        // `clear_native_function` (`clear_jni_native`), in entry order.
        bindings.push((bind_class, name, sig, m.fn_ptr as usize));
    }
    // Interpreter round i2 wave 63, lane L1: a C JVMTI env's
    // `NativeMethodBind` of each, the address it hands back bound instead;
    // posted from native, as the agent's callback runs.
    #[cfg(feature = "experimental-debug")]
    {
        drop(_fx);
        if let Some(shared) = calling_thread_vm() {
            if shared
                .debug
                .debugger_gates
                .native_event_armed(crate::debug::NativeEvent::NativeBind)
            {
                if let Some(current) = current_jvm_thread_of(&shared) {
                    // SAFETY: the calling thread's own `JvmThread`; `thread_id`
                    // is `Copy`.
                    let tid = unsafe { (*current).thread_id.0 };
                    for (_, name, sig, fn_ptr) in &mut bindings {
                        if *fn_ptr == 0 {
                            // HotSpot's `clear_native_function` posts no
                            // `NativeMethodBind`.
                            continue;
                        }
                        *fn_ptr = crate::jvmti::native_env::post_native_method_bind(
                            &shared, tid, class_id, name, sig, *fn_ptr,
                        );
                    }
                }
            }
        }
    }
    for (bind_class, name, sig, fn_ptr) in bindings {
        // Bind into the CALLING VM's table. `with_shared_vm` is the same handle
        // the class name above was read through, so a `RegisterNatives` driven
        // by a library another VM in this process loaded cannot land here.
        with_shared_vm(|shared| {
            if fn_ptr == 0 {
                clear_jni_native(&shared.natives, &bind_class, &name, &sig);
            } else {
                register_jni_native(&shared.natives, &bind_class, &name, &sig, fn_ptr);
            }
        });
    }
    if let Some(msg) = refusal {
        raise_jni_error("java/lang/NoSuchMethodError", &msg);
        return JNI_ERR;
    }
    JNI_OK
}



/// What `RegisterNatives` does with one `(name, signature)` entry.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::native::jni) enum RegisterNativesTarget {
    /// Bind it under this class (internal name): the class on the superclass
    /// chain that declares the native method (HotSpot's `lookup_method`).
    Bind(String),
    /// Not decidable here (no real class bytes behind the class, the class is
    /// gone, or a JVMTI native-method prefix is set): bind under the named
    /// class as before.
    Lenient,
    /// HotSpot refuses it: the `NoSuchMethodError` message.
    Refuse(String),
    /// HotSpot refuses it at the symbol-table probe, before its platform
    /// re-registration warning (`jni_platform_rebind`): the message.
    RefuseUnknownSymbol(String),
}

/// HotSpot 25's `jni_RegisterNatives` + `Method::register_native` for one
/// entry of `class_id`:
///
/// * `k->lookup_method(name, sig)` walks the SUPERCLASS chain (not the
///   interfaces), the first declaration wins, `<init>` is not inherited;
/// * a miss is `NoSuchMethodError("Method '<ret> <class>.<name>(<params>)'
///   name or signature does not match")`, with the NAMED class in the text;
/// * a hit that is not `native` is `"... is not declared as native"` (after
///   the JVMTI prefix search, which this VM models at link time instead:
///   `vm_exec::prefixed_native_link` reads the binding of the unprefixed
///   name, so with any prefix set the entry is bound as before);
/// * before either, a name or signature that is not in the symbol table at
///   all is `"Method <class>.<name><sig> not found"`. This VM has no symbol
///   table; a name counts as present when any loaded class declares a method
///   or field of that name, a signature when any loaded class declares a
///   method with that descriptor (an approximation: HotSpot's table also
///   holds every constant-pool `Utf8`).
///
/// A class without real class bytes (a compatibility stub, a VM-internal or
/// array class) declares nothing a library could be checked against, so it
/// stays [`RegisterNativesTarget::Lenient`] (the old behaviour, both modes).
pub(in crate::native::jni) fn register_natives_target(
    shared: &SharedVm,
    class_id: ClassId,
    name: &str,
    sig: &str,
) -> RegisterNativesTarget {
    let lookup = {
        let cm = shared.classes.class_manager.read();
        register_natives_lookup(cm.class_store(), class_id, name, sig)
    };
    match lookup {
        RegisterNativesLookup::Undecidable => RegisterNativesTarget::Lenient,
        RegisterNativesLookup::Native(declaring) => RegisterNativesTarget::Bind(declaring),
        RegisterNativesLookup::NotNative { named } => {
            if register_natives_prefix_active(shared) {
                return RegisterNativesTarget::Lenient;
            }
            RegisterNativesTarget::Refuse(format!(
                "Method '{}' is not declared as native",
                crate::runtime::resolve::selection::external_method_name(&named, name, sig)
            ))
        }
        RegisterNativesLookup::Missing { named, symbols_known } => {
            if symbols_known {
                RegisterNativesTarget::Refuse(format!(
                    "Method '{}' name or signature does not match",
                    crate::runtime::resolve::selection::external_method_name(&named, name, sig)
                ))
            } else {
                RegisterNativesTarget::RefuseUnknownSymbol(format!(
                    "Method {}.{name}{sig} not found",
                    crate::runtime::exceptions::hotspot_external_name(&named)
                ))
            }
        }
    }
}

/// The class-store half of [`register_natives_target`] (no VM state, so a
/// unit test can drive it with a hand-built store).
#[derive(Debug, PartialEq, Eq)]
pub(in crate::native::jni) enum RegisterNativesLookup {
    Undecidable,
    /// Declared `native` by this class (internal name) on the chain.
    Native(String),
    /// Declared, not `native`; `named` is the class `RegisterNatives` named.
    NotNative { named: String },
    /// Not declared on the chain; `symbols_known` is the symbol-table
    /// approximation of [`register_natives_target`].
    Missing { named: String, symbols_known: bool },
}

pub(in crate::native::jni) fn register_natives_lookup(
    store: &crate::classloading::ClassStore,
    class_id: ClassId,
    name: &str,
    sig: &str,
) -> RegisterNativesLookup {
    let Some(named) = store.get(class_id) else {
        return RegisterNativesLookup::Undecidable;
    };
    if !named.origin.has_real_bytes() {
        return RegisterNativesLookup::Undecidable;
    }
    let named_name = named.name.to_string();
    let mut current = Some(class_id);
    while let Some(id) = current {
        let Some(class) = store.get(id) else {
            // A superclass missing from the store: not decidable.
            return RegisterNativesLookup::Undecidable;
        };
        if let Some(method) = class.find_method(name, sig) {
            return if method.is_native() {
                RegisterNativesLookup::Native(class.name.to_string())
            } else {
                RegisterNativesLookup::NotNative { named: named_name }
            };
        }
        if name == "<init>" {
            break;
        }
        current = class.superclass;
    }
    let name_known = store.iter().any(|c| {
        c.methods.iter().any(|m| &*m.name == name) || c.fields.iter().any(|f| &*f.name == name)
    });
    let sig_known = name_known
        && store
            .iter()
            .any(|c| c.methods.iter().any(|m| &*m.descriptor == sig));
    RegisterNativesLookup::Missing {
        named: named_name,
        symbols_known: name_known && sig_known,
    }
}

/// Whether any JVMTI / `java.lang.instrument` native-method prefix is set in
/// this VM (the sources `vm_exec::prefixed_native_link` reads).
pub(in crate::native::jni) fn register_natives_prefix_active(shared: &SharedVm) -> bool {
    let active = !crate::runtime::instrument::native_method_prefixes(shared.vm_identity).is_empty();
    #[cfg(feature = "experimental-debug")]
    {
        active || !crate::jvmti::native_env::all_native_method_prefixes(shared).is_empty()
    }
    #[cfg(not(feature = "experimental-debug"))]
    {
        active
    }
}

// ---- Index 216: UnregisterNatives ----

pub(in crate::native::jni) extern "C" fn jni_unregister_natives(_env: JNIEnv, clazz: JClass) -> JInt {
    let _fx = ForeignJniEntry::enter();
    if clazz == 0 {
        return JNI_OK;
    }
    // Resolve the class name and remove all registered natives for it.
    // Since JNI_NATIVE_METHODS is keyed by hash, we need the class name
    // to reconstruct the keys. If we can't resolve the class, best-effort no-op.
    //
    // `JClass` is a `ClassId` here, matching `jni_register_natives` above and
    // the rest of the table. Decoding it as an object handle made this a
    // permanent no-op for anything `FindClass` returned, which paired with the
    // same defect in RegisterNatives: neither half of the pair worked.
    let class_info = with_shared_vm(|shared| {
        let class_id = jclass_class_id(clazz);
        shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| (class_id, c.name.clone()))
    })
    .flatten();

    if let Some((class_id, class_name)) = class_info {
        // Get all methods for this class and remove their native registrations
        let methods_to_remove: Vec<u64> = with_shared_vm(|shared| {
            let cm = shared.classes.class_manager.read();
            let mut keys = Vec::new();
            // `clazz` already supplied the exact loader-qualified ClassId.
            if let Some(class) = cm.get_class(class_id) {
                for method in &class.methods {
                    if method.is_native() {
                        let key = jni_native_key(&class_name, &method.name, &method.descriptor);
                        keys.push(key);
                    }
                }
            }
            keys
        })
        .unwrap_or_default();

        if !methods_to_remove.is_empty() {
            // Unbind from the CALLING VM's table only — the mirror image of
            // `RegisterNatives` above. While this was a process global, an
            // `UnregisterNatives` in one VM tore down another VM's bindings for
            // the same class.
            with_shared_vm(|shared| {
                let mut table = shared.natives.jni_native_methods.write();
                let mut removed_any = false;
                for key in &methods_to_remove {
                    removed_any |= table.remove(key).is_some();
                }
                drop(table);
                // gce e1/j: void every thread's `find_jni_native` memo.
                // gci i10/cr: only when a bound key went away
                // (`jni_write_bumps_generation`; gci i19/jt: with the memo on
                // or off); an unbound native's removal changes no memoised
                // answer.
                if jni_write_bumps_generation(removed_any) {
                    shared
                        .natives
                        .jni_native_generation
                        .fetch_add(1, std::sync::atomic::Ordering::Release);
                }
            });
            tracing::debug!(
                "UnregisterNatives: removed {} native methods for {}",
                methods_to_remove.len(),
                class_name
            );
        }
    }
    JNI_OK
}

