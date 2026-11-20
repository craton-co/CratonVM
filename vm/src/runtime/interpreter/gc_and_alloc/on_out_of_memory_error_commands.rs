#[allow(unused_imports)]
use super::*;

/// Step 2 of [`maybe_dump_heap_on_oom_for`], `VM_ReportJavaOutOfMemory::doit`'s
/// shape: a `#` header naming the error and the option, then each command run
/// to completion, with HotSpot's `#   Executing ...` line before it (Linux
/// names the `/bin/sh -c` it uses; the other platforms print the bare command,
/// as HotSpot does). The commands inherit the VM's standard streams.
/// A command that cannot be started is reported and skipped, never fatal.
pub(in crate::runtime::interpreter::gc_and_alloc) fn run_on_out_of_memory_error_commands(raw: &str, message: &str, pid: u32) {
    use std::io::Write as _;
    {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "#");
        let _ = writeln!(out, "# java.lang.OutOfMemoryError: {message}");
        let _ = writeln!(out, "# -XX:OnOutOfMemoryError=\"{raw}\"");
        let _ = out.flush();
    }
    for cmd in on_out_of_memory_error_commands(raw, pid) {
        {
            let mut out = std::io::stdout().lock();
            if cfg!(target_os = "linux") {
                let _ = writeln!(out, "#   Executing /bin/sh -c \"{cmd}\"...");
            } else {
                let _ = writeln!(out, "#   Executing \"{cmd}\"...");
            }
            let _ = out.flush();
        }
        if let Err(e) = on_error_shell_command(&cmd).status() {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "os::fork_and_exec failed: {e}");
            let _ = out.flush();
        }
    }
}

/// The process HotSpot's `os::fork_and_exec` starts for one `OnError`-style
/// command: `/bin/sh -c <cmd>` on Unix; `cmd /C <cmd>` on Windows, the command
/// passed verbatim (not re-quoted) with newlines turned into `&`, as HotSpot's
/// Windows `fork_and_exec` does.
pub(in crate::runtime::interpreter::gc_and_alloc) fn on_error_shell_command(cmd: &str) -> std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").raw_arg(cmd.replace('\n', "&"));
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = std::process::Command::new("/bin/sh");
        c.arg("-c").arg(cmd);
        c
    }
}


/// TLAB hit-only fast path for array allocation — the array twin of
/// [`tlab_alloc_object`], and the general-element-type twin of
/// [`tlab_alloc_byte_array`].
///
/// Why this exists: object allocation has had a TLAB fast path for a long
/// time, but *array* allocation never did — `gc_alloc_array` went straight to
/// `heap.try_alloc_array`, and every young allocation there takes the single
/// global `young_from` mutex and holds it across the bump, the `write_bytes`
/// zeroing of the whole object, and the header `init`. Because the zeroing is
/// inside the lock, the hold time grows with the allocation, so arrays
/// serialise harder than objects do.
///
/// Measured on this box (`apps/hib-suite-runner/AllocScaleProbe.java`,
/// aggregate throughput, 1 -> 4 threads):
///
/// | shape        | 1 thread | 4 threads | scaling |
/// |--------------|---------:|----------:|--------:|
/// | `new Object()` (TLAB) | 19.8 Mops/s | 41.4 Mops/s | 2.10x |
/// | `new long[16]` (no TLAB) | 13.1 Mops/s | 11.9 Mops/s | **0.91x** |
///
/// HotSpot scales the same array shape ~27x over the same range. Array
/// allocation was therefore capped at roughly one thread's worth of
/// throughput no matter how many cores were available — which is why a
/// 5-thread allocation-heavy workload (an ORM opening sessions and building
/// `ArrayList`/`HashMap`/`StringBuilder` backing arrays) could not use them.
///
/// It shares [`tlab_alloc_shaped_inner`] with the object path, so it inherits
/// that path's already-hardened refill gate, wedge breakers and
/// retire-before-replace protocol verbatim rather than carrying a second copy
/// of them. A hit-only first attempt is not enough on its own: a workload that
/// allocates mostly arrays drains the TLAB and then misses back to the global
/// lock on every subsequent allocation (measured — hit-only bought ~23% on one
/// thread and moved multi-thread scaling not at all).
///
/// Zeroing: [`GenerationalHeap::refill_tlab`] zeroes the whole TLAB region
/// when it is handed out, so the array body already reads back as Java's
/// mandated default values and the initializer only has to write the header —
/// the same contract [`tlab_alloc_byte_array`] relies on.
pub(in crate::runtime::interpreter) fn tlab_alloc_array(
    thread: &mut JvmThread,
    shared: &SharedVm,
    class_id: ClassId,
    element_type: ArrayElementType,
    length: usize,
) -> Option<ObjectRef> {
    use cratonvm_gc::heap::{ObjectKind, ARRAY_DATA_OFFSET, HEADER_SIZE};
    let length_u32 = u32::try_from(length).ok()?;
    let data_size = cratonvm_gc::heap::array_data_size_checked(length, element_type)?;
    let total_size = ARRAY_DATA_OFFSET.checked_add(data_size)?;
    // Keep well clear of the humongous threshold: anything at or above the
    // TLAB's per-allocation cap goes down the ordinary path, which owns the
    // young-vs-old-gen routing decision.
    if total_size > cratonvm_gc::tlab::tlab_max_alloc() {
        return None;
    }
    let obj = tlab_alloc_shaped_inner(
        thread,
        shared,
        class_id,
        TlabShape::Array {
            element_type,
            length_u32,
        },
        total_size,
        // Same value the interpreter's object path passes: the young-room
        // pre-check is the JIT slow path's gate, not this one's.
        false,
    )?;
    cratonvm_gc::a2dbg::record(
        obj.as_ptr() as usize,
        class_id.as_u32(),
        ObjectKind::Array as u8,
        element_type as u8,
        length_u32,
        length_u32,
        total_size,
    );
    Some(obj)
}

/// The TLAB arm of the NATIVE array doors (`NativeContextImpl::{new_array,
/// try_new_array, new_ref_array, try_new_ref_array}`; round 16 wave 12, lane
/// nativearr, proposal BN2-1 of `jit-r16-bignum2-proposals-RETIRED-20261006.md`), asked only
/// under `CRATONVM_NATIVE_ARRAY_TLAB` (default OFF, see
/// `env_cache::native_array_tlab_mode`). `None` sends the caller down its
/// historical road (`try_alloc_array_full`, the young arena's lock), which
/// still owns every unusual case: an array above `tlab_max_alloc` (32 KiB,
/// far below every collector's humongous threshold), a length past `u32`, a
/// size that overflows, and -- with `refill` false -- a thread with no buffer
/// or a tail too short for the array.
///
/// `refill` false is a pure bump of the CURRENT buffer: no refill, no
/// retire, no heap call at all apart from the backend's announce. `refill`
/// true is [`tlab_alloc_array`], the interpreter's `newarray` arm, whose
/// refill passes `refill_needs_young_room = false`: it refills without ever
/// collecting, the contract the native object door (`native_alloc_no_gc_arms`
/// -> [`tlab_alloc_object`]) already relies on inside a native callback.
///
/// Every collector's TLAB contract is the one the interpreter and the
/// compiled `newarray` already honour, through the same code: the chunk is
/// zeroed by its carve (`GenerationalHeap::refill_tlab_at_least`,
/// `G1Collector::refill_tlab` -> `carve_tlab_from_eden_slot`, ZGC
/// `vm_tlab::refill_tlab`), so only the header is written; the header is
/// announced the moment it is complete through `VmHeap::note_tlab_object`
/// (ZGC's start registry, allocate-black and young grain; a no-op on
/// Generational and G1, whose sweeps parse the chunk linearly); and the bytes
/// are charged once, as the cursor's advance (`note_tlab_allocation`), so the
/// caller must NOT also charge them as an external allocation.
pub(crate) fn tlab_alloc_array_native(
    thread: &mut JvmThread,
    shared: &SharedVm,
    class_id: ClassId,
    element_type: ArrayElementType,
    length: usize,
    refill: bool,
) -> Option<ObjectRef> {
    use cratonvm_gc::heap::{ObjectKind, ARRAY_DATA_OFFSET};
    if refill {
        return tlab_alloc_array(thread, shared, class_id, element_type, length);
    }
    let length_u32 = u32::try_from(length).ok()?;
    let data_size = cratonvm_gc::heap::array_data_size_checked(length, element_type)?;
    let total_size = ARRAY_DATA_OFFSET.checked_add(data_size)?;
    if total_size > cratonvm_gc::tlab::tlab_max_alloc() {
        return None;
    }
    let shape = TlabShape::Array {
        element_type,
        length_u32,
    };
    let ptr = thread.tlab.alloc_initialized(total_size, 8, |ptr| {
        // SAFETY: `alloc_initialized` reserved `total_size` (>= HEADER_SIZE)
        // bytes at `ptr`, 8-byte aligned and privately owned until commit.
        unsafe { shape.init_header(ptr, class_id) };
        // ZGC registers every TLAB object the moment its header is complete;
        // a no-op on the linear-sweep backends.
        shared.mem.heap.note_tlab_object(ptr, total_size);
    })?;
    note_tlab_allocation(thread, shared, total_size);
    cratonvm_gc::a2dbg::record(
        ptr as usize,
        class_id.as_u32(),
        ObjectKind::Array as u8,
        element_type as u8,
        length_u32,
        length_u32,
        total_size,
    );
    // SAFETY: `ptr` is the just-initialised object base from the TLAB bump
    // above: non-null, header-aligned, its header written before this point.
    Some(unsafe { ObjectRef::from_raw(ptr) })
}

/// Try to allocate an array, running GC and retrying on failure.
///
/// `try_alloc_array_full` is deliberately used rather than the young-only
/// `try_alloc_array`: it has the same young-first policy, but once a
/// non-moving JIT-safe sweep has left the young generation fragmented it can
/// spill the request into old space.  This matches `alloc_object_shared`'s
/// object path.  Retrying young-only here used to report OOM for a tiny array
/// while most of the heap was available as old-generation headroom.
pub(crate) fn gc_alloc_array(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    element_type: ArrayElementType,
    length: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let arr = gc_alloc_array_unsampled(shared, thread, class_id, element_type, length)?;
    // Interpreter round i2 wave 63, lane L1: as in [`gc_alloc_object`].
    #[cfg(feature = "experimental-debug")]
    if shared
        .debug
        .debugger_gates
        .native_event_armed(crate::debug::NativeEvent::SampledAlloc)
    {
        return Ok(crate::jvmti::native_env::sample_allocation(
            shared, thread, arr,
        ));
    }
    Ok(arr)
}

/// [`gc_alloc_array`] without the JVMTI allocation sample.
///
/// `pub(crate)` since gci i25/vt (rv25 item 5): the carrier's post-yield
/// heap probe (`jdk_continuation::refuse_yield_without_headroom`) allocates a
/// `byte[0]` the program never asked for, which no agent may be handed.
pub(crate) fn gc_alloc_array_unsampled(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    element_type: ArrayElementType,
    length: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(arr) = tlab_alloc_array(thread, shared, class_id, element_type, length) {
        return Ok(arr);
    }
    // HotSpot's size check comes BEFORE any collection. A length past the VM's
    // array limit is refused outright with its own message. It is never run
    // through the full GC ladder and then reported as "Java heap space". See
    // `array_length_exceeds_vm_limit`. The check sits after the TLAB attempt
    // so the fast path pays nothing; the TLAB already refuses any such length
    // through its own `tlab_max_alloc` cap.
    if array_length_exceeds_vm_limit(length) {
        maybe_dump_heap_on_oom_for(shared, thread, ARRAY_SIZE_EXCEEDS_VM_LIMIT);
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError {
                message: ARRAY_SIZE_EXCEEDS_VM_LIMIT.to_string(),
            },
        )));
    }
    // Everything below this line bypasses the TLAB, so the thread's allocation
    // counter (`Tlab::thread_allocated_bytes`, read by
    // `com.sun.management.ThreadMXBean.getThreadAllocatedBytes`) sees none of
    // it from the cursor. Record it explicitly — arrays are precisely the
    // shape that skips the TLAB, so an unrecorded array path would make the
    // counter report a small fraction of a buffer-allocating workload.
    let external_bytes = external_array_bytes(element_type, length);
    // gci i20/ys (`gci-i20ys-g1-humongous-allocation-never-asks-for-a-concurrent-start-FIXED-20261016`,
    // default-ON `CRATONVM_G1_HUMONGOUS_IHOP_START`, read inside the predicate):
    // HotSpot's `attempt_allocation_humongous` asks for a concurrent-start
    // collection (`G1 Humongous Allocation`) BEFORE a humongous request that
    // would take the old occupancy over IHOP. Nothing is allocated yet, so the
    // pause holds no new object; the request then allocates as below.
    if shared.mem.heap.is_g1() {
        if shared.mem.heap.g1_humongous_alloc_wants_concurrent_start(external_bytes) {
            flush_tlab_allocation_batch(thread, shared);
            thread.tlab.retire();
            maybe_gc_forced_at(shared, thread, G1_HUMONGOUS_ALLOC_SITE);
        }
    }
    if let Some(arr) = shared
        .mem
        .heap
        .try_alloc_array_full(class_id, element_type, length)
    {
        note_shared_array_allocation(thread, shared, external_bytes);
        return Ok(arr);
    }
    // Forced GC, the GC-overhead limit (bail to OOM if the heap is
    // GC-thrashing rather than spinning on slivers), retry, G1's last-ditch
    // complete mark cycle (the young pause cannot reclaim dead Old/humongous
    // spans — only a completed cycle's cleanup can), retry. See
    // `collect_and_retry`.
    // gci i11/go: a G1 humongous request's collection is `G1 Humongous
    // Allocation` (`gc_alloc_array_site`; `external_bytes` includes the
    // array header).
    let site = gc_alloc_array_site(shared, external_bytes);
    // JIT round 22 wave 11: the request, for `CRATONVM_DBG_OOME_LADDER`'s line.
    oome_ladder_step(
        shared,
        thread,
        &format!("request array bytes={external_bytes} humongous={}", site == G1_HUMONGOUS_ALLOC_SITE),
    );
    let arr = collect_and_retry(shared, thread, site, |s| {
        s.mem
            .heap
            .try_alloc_array_full(class_id, element_type, length)
    })
    .ok_or_else(|| {
        maybe_dump_heap_on_oom(shared, thread);
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
            message: format!("Java heap space (alloc_array length {})", length),
        }))
    })?;
    note_shared_array_allocation(thread, shared, external_bytes);
    Ok(arr)
}

/// Charge an array [`gc_alloc_array`] placed outside the TLAB to BOTH
/// counters [`alloc_object_shared`] charges for an object on the same path:
/// the thread's own total (`getThreadAllocatedBytes`) and the VM's
/// `bytes_allocated_total` (the `--verbose:gc` allocation rate and the wedge
/// breaker's re-arm progress).
///
/// gc-common w7-b: this path fed only the thread counter, so every array too
/// big for a TLAB (anything past `tlab_max_alloc`, 32 KiB: every large buffer,
/// every collection resize past a few thousand elements) was invisible to
/// `bytes_allocated_total`, whose doc promises "all TLAB and slow-path heap
/// allocations". A buffer-heavy workload therefore reported a fraction of its
/// allocation rate and re-armed the wedge breaker late. Direct `fetch_add`,
/// as `alloc_object_shared` does: this is the slow path and it already took
/// the backend's allocation lock.
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn note_shared_array_allocation(thread: &mut JvmThread, shared: &SharedVm, bytes: usize) {
    // Widening: usize -> u64 is loss-free on all supported targets.
    shared
        .mem
        .bytes_allocated_total
        .fetch_add(bytes as u64, std::sync::atomic::Ordering::Relaxed);
    note_thread_external_allocation(thread, shared, bytes);
}

/// Footprint, in bytes, of an array that is about to be allocated outside the
/// TLAB — header plus payload, computed the same way [`tlab_alloc_array`]
/// computes `total_size`.
///
/// Saturating rather than checked: this feeds a monitoring counter, and an
/// array whose data size overflows `usize` is about to fail its allocation
/// anyway. Reporting the clamped figure keeps this off the error path.
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn external_array_bytes(element_type: ArrayElementType, length: usize) -> usize {
    use cratonvm_gc::heap::ARRAY_DATA_OFFSET;
    let data = cratonvm_gc::heap::array_data_size_checked(length, element_type).unwrap_or(0);
    ARRAY_DATA_OFFSET.saturating_add(data)
}

/// The largest array length HotSpot will attempt to allocate, for EVERY
/// element type. On 64-bit, `arrayOopDesc::max_array_length` is
/// `align_down(max_jint - header_words, MinObjAlignment)`. With a 16-byte
/// array header (two words, with compressed or compact class pointers alike)
/// and 8-byte object alignment, that is `Integer.MAX_VALUE - 2`.
///
/// So on a small heap `new byte[Integer.MAX_VALUE - 2]` fails with
/// `OutOfMemoryError: Java heap space`, and on any heap
/// `new byte[Integer.MAX_VALUE - 1]` fails with
/// `OutOfMemoryError: Requested array size exceeds VM limit`.
pub(crate) const MAX_JAVA_ARRAY_LENGTH: usize = cratonvm_gc::vm_heap::VM_MAX_ARRAY_LENGTH;

/// HotSpot's message for [`array_length_exceeds_vm_limit`].
pub(crate) const ARRAY_SIZE_EXCEEDS_VM_LIMIT: &str = "Requested array size exceeds VM limit";

/// Would HotSpot refuse an array of `length` elements as exceeding the VM
/// limit, rather than as heap exhaustion? See [`MAX_JAVA_ARRAY_LENGTH`].
///
/// gc-common w1-f: before this check, the interpreter ran such a request down
/// the whole collection ladder (a forced GC, the overhead-limit check, a G1
/// last-ditch concurrent cycle) and then reported
/// `Java heap space (alloc_array length 2147483647)`. `multianewarray` checks
/// it too since w2-d (`alloc_multi_array`), and so do the JIT's `newarray`
/// helpers and the native allocators (`vm_exec.rs` `new_array` /
/// `new_ref_array`) -- see
/// `docs/feature-designs/jit-ir-optimizations.md`.
#[inline]
pub(crate) fn array_length_exceeds_vm_limit(length: usize) -> bool {
    length > MAX_JAVA_ARRAY_LENGTH
}


/// Update the thread's root snapshot with current frame ObjectRefs.
/// Called at safepoints and before blocking operations.
///
/// **Spring Boot SEGV fix (2026-05-16), as it stands today:** the
/// freshly-scanned operand-stack roots are screened before they join the
/// snapshot, because `ValueStack::scan_object_refs` used to report every
/// `CompactTag::Long` slot whose bits looked like an aligned pointer as a
/// heap root without consulting the heap — a primitive `long` carrying a file
/// size or a jboss-modules token then poisoned the root set and SEGV'd the GC.
///
/// The screen is `heap.is_heap_addr` (alignment + arena containment), which is
/// what kills that population. It was `is_object_address` until 2026-08-04;
/// that is a strictly stronger probe and it also dropped GENUINE roots — see
/// `scan_frame_roots` for why, and for the two in-tree fixes the strict
/// version was silently reverting. Locals (screened the same way inside
/// `Frame::scan_local_objects`), `native_pin_roots`, and
/// `native_pending_return` come from validated paths and are appended after
/// the filter.
/// DBG (CRATONVM_DBG_ROOTSNAP): instrumentation for the per-native-call root
/// snapshot cost. Confirms/quantifies whether `update_root_snapshot` is the
/// embedded-server deployment hotspot (O(stack-depth) full-frame scan + the
/// per-operand-stack-object `is_object_address` triple-lock validation, run on
/// every object-returning native call). Prints a cumulative line every 200k
/// calls. Default-off; zero cost when the gate is unset (cached OnceLock).
pub(in crate::runtime::interpreter) fn rootsnap_dbg_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| None::<std::ffi::OsString>.is_some())}
/// DBG (CRATONVM_DBG_ROOTSNAP_VERIFY): after the frozen-frame cache builds a
/// snapshot, re-scan every frame the default (uncached) way and diff the two.
/// The cached path claims to be byte-identical to the default path; any root
/// the fresh scan reports that the cached snapshot lacks is a LOST ROOT —
/// precisely the failure the retired real-ForkJoinPool bypass claimed to
/// prevent, and the check that retired it (see `update_root_snapshot`). Prints
/// a line per miss plus a periodic tally; run it under `CRATONVM_DBG_ROOTSNAP`
/// so the tally shows how much of the snapshot came from the cache. Costs a
/// full extra frame scan per snapshot, so it is a diagnostic, not a default:
/// unset, this is one cached `OnceLock` read.
pub(in crate::runtime::interpreter) fn rootsnap_verify_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        None::<std::ffi::OsString>.is_some()    })}

/// Print period for the two DBG tallies (`CRATONVM_DBG_ROOTSNAP_EVERY`,
/// default 200k). Short workloads publish far fewer snapshots than that, so a
/// smaller period is what makes "the cache was actually exercised" visible
/// rather than assumed.
pub(in crate::runtime::interpreter) fn rootsnap_dbg_every() -> u64 {
    use std::sync::OnceLock;
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(200_000)
    })
}

// Cache engagement (`CRATONVM_DBG_ROOTSNAP`): snapshots that took the cached
// path, and how many frames / roots those reused instead of re-scanning.
// Cache soundness (`CRATONVM_DBG_ROOTSNAP_VERIFY`): snapshots diffed against a
// fresh full scan, and the roots the cached snapshot was missing.

/// Scan ONE frame's GC roots (locals + operand stack, with the operand-stack
/// pointer-shaped-Long validation) onto the end of `out`. This is exactly the
/// per-frame body of `update_root_snapshot`'s loop, factored out so the opt-in
/// root-snapshot cache can scan a single frame into a cache entry as well as
/// into the live snapshot. Behaviour is byte-identical to the inline loop.
///
/// The operand-stack post-filter is `is_heap_addr` (alignment + arena
/// containment), NOT the strict `is_object_address` header probe, and the two
/// are not interchangeable here:
///
/// * `is_object_address` rejects a young / mid-init object whose header it
///   cannot yet vouch for. `Frame::scan_local_objects` has always used the
///   loose screen for exactly that reason (see its comment naming the
///   BouncyCastle `EC5Util.getCurve` local), and `ValueStack::scan_object_refs`
///   was changed to root a genuine object slot unconditionally with the same
///   argument — "the prior code dropped such roots (use-after-free risk for
///   newly allocated objects) — the regression this fixes". Re-applying the
///   strict probe over its output put that regression straight back, on the
///   operand-stack half only.
/// * It also discarded the JNI long-as-jobject smuggle roots that
///   `scan_object_refs` validates with `is_heap_addr` precisely because the
///   strict probe rejects them (interior offsets, mid-init) — the WildFly
///   `jboss-modules` SEGV that comment records.
///
/// The filter's original justification — that `scan_object_refs` "treats
/// pointer-shaped `Long` bits as roots without heap validation" — is no longer
/// true: that scan is kind-gated (`KIND_LONG`/`KIND_DOUBLE` slots are never
/// rooted through the object branch) and heap-validates the smuggle branch.
/// What remains is a loose root reaching the collector, which BOTH generations
/// already screen with an exact object-base oracle before writing anything
/// through it — `young_object_starts` membership in `forward_object`, and
/// `walk_objects`-derived `walked_bases` in `old_gen_gc`. Over-retention for
/// one cycle is the whole cost; a dropped root is a use-after-free.
#[inline]
pub(in crate::runtime::interpreter) fn scan_frame_roots(
    frame: &Frame,
    out: &mut Vec<ObjectRef>,
    heap: &crate::memory::VmHeap,
    minted_only_primitives: bool,
    debugger_observes_locals: bool,
    // Interpreter round i6 wave 121, lane L7: the VM's type-map store, so
    // this scan leaves out the slots the mapped scans leave out under
    // `CRATONVM_INTERP_PRECISE_OOP_MAPS` (`None`: the unit tests).
    maps: Option<&cratonvm_classloading::TypeMapStore>,
) {
    // gch h1/jp (f1f parity): every local while a debugger can read them
    // (`jvmti::debugger_observes_locals`, read once by the caller).
    frame.scan_local_objects_observed_mapped(out, heap, debugger_observes_locals, maps);
    let before = out.len();
    // Wave 122, lane L7: the stack half of `CRATONVM_INTERP_PRECISE_OOP_MAPS`.
    frame.scan_stack_objects_mapped(out, heap, minted_only_primitives, maps);
    let len = out.len();
    if len > before {
        let mut write = before;
        for read in before..len {
            let o = out[read];
            // Cast: object/code pointer to integer address
            if heap.is_heap_addr(o.as_ptr() as usize).is_some() {
                out[write] = o;
                write += 1;
            }
        }
        out.truncate(write);
    }
}

/// `CRATONVM_GC_G1_ONLY_JIT_PINS=1` -- restore the pre-2026-09-06 gate on the
/// two deposit-side publications of a parked thread's CONSERVATIVE JIT-frame
/// roots into `gc_quiescence`'s process-global pin registry.
///
/// Those two sites were written when G1 was the only backend that moved, and
/// their own comments said so: *"they can only over-retain (the young sweep
/// runs non-moving while any thread is in JIT, so nothing is relocated)"*.
/// That premise expired twice and neither expiry reached the gate:
///
///  * ZGC compaction shipped 2026-08-13, and ZGC withholds the PAGE of every
///    address in `pinned_jit_roots_snapshot()` from its relocation set
///    (`zgc.rs`, "CONSERVATIVE JIT ROOTS PIN THEIR PAGE TOO"). With the
///    publication gated on G1, that consumer only ever saw the INITIATOR's
///    pins -- every parked peer's compiled frames were unprotected.
///  * The cross-thread JIT coverage handshake (2026-08-23) let the
///    generational moving-young cycle run while peers are in JIT, so its
///    Cheney copy started relocating exactly the objects the comment promised
///    it would not.
///
/// Measured on the `qdox-parser-static-array-race` reproducer (4 threads x
/// 400 fresh-builder QDox parses, Azure 20.80.105.49):
/// ZGC lost 1-11 parses per run to `ArrayIndexOutOfBoundsException: Index N
/// out of bounds for length 0` -- ZGC's own documented signature for a
/// conservative root left pointing at a vacated, zeroed span -- and
/// Generational SIGSEGV'd 3/3 inside compiled `java/lang/StringUTF16.compress`
/// on an array whose base had been evacuated. G1, the one backend the gate
/// admitted, was clean 3/3.
///
/// Publishing unconditionally costs one `Vec<usize>` collect and one map
/// insert per deposit, on a path that has just finished a conservative stack
/// scan. Nothing consumes the registry on a backend that does not relocate.
pub(crate) fn g1_only_jit_pins() -> bool {
    // gcd d1/d: the typed field (`GcFlags::g1_only_jit_pins`), the one the
    // collector's pinned-compaction plan reads too.
    cratonvm_types::flags().gc.g1_only_jit_pins
}

/// `CRATONVM_GC_NO_REFPROC_PREPASS=1` — put the generational post-GC path back
/// on the pre-2026-09-21 order, where `remove_collected` runs only AFTER
/// reference processing.
///
/// A kill switch for the reordering described at its call site in
/// `process_references_after_gc`, not a tuning knob: with it set, a
/// `SoftReference` whose own instance died in the cycle again roots the
/// weak-clearing closure, which is the defect
/// `gengc-mark-dead-softref-object-still-roots-weak-closure-FIXED-20260923.md`
/// describes. It exists so a bisect can attribute a reference-processing
/// change to this reordering in one run rather than by rebuilding.
///
/// `=0` / `=false` / `=off` mean OFF (gc-common w2-a, 2026-09-23). It was
/// presence-parsed, so the legacy spelling `CRATONVM_GC_NO_REFPROC_PREPASS=0`
/// — the natural way to say "leave it off" — switched the kill switch ON and
/// silently restored the weak-clearing defect above
/// (`docs/feature-designs/real-jdk-mode.md` #3). The grouped `CRATONVM_GC=` spelling
/// inserts or removes the key and is unaffected; presence with any other
/// value (including empty) is still ON.
pub(crate) fn no_refproc_prepass() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        match None::<std::ffi::OsString> {
            None => false,
            Some(v) => {
                let v = v.to_string_lossy();
                let v = v.trim();
                !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
            }
        }
    })
}

/// `CRATONVM_DBG_BLOCKGC`, read once (override-aware `MemoSlot`), for the
/// safepoint deposit and the safepoint-resume remap.
///
/// gc-common 2026-09-23: the SAFEPOINT-STALE hunter in `update_root_snapshot`
/// read this flag with an UNCACHED `runtime_var_os` on every call, and that
/// function runs on every safepoint park as well as at every GC entry. The
/// same flag's per-call probe on the native funnel was measured at ~7% of a
/// HashMapOnly run before `vm_exec::blockgc_dbg` cached it. Hoisted out of
/// `update_root_snapshot` in w2-g so `apply_pointer_map_to_thread` -- which
/// ran the same uncached probe once per parked thread per relocating pause --
/// shares it.
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn peer_blockgc_dbg() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = false;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// `CRATONVM_DBG_BUG03`, read once (override-aware `MemoSlot`). The
/// safepoint-resume remap probed it uncached for every pause on thread 0.
#[inline]
pub(in crate::runtime::interpreter::gc_and_alloc) fn peer_bug03_dbg() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = false;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

pub(crate) fn update_root_snapshot(shared: &SharedVm, thread: &mut JvmThread) {
    // gcj j1pn ledger item 4: this publish's band scans fill no thread-local
    // movable / unrewritable set (no reader ever sees them; see the scope).
    let _publish = crate::native::jni::SafepointPublishScope::enter();
    remap_trace_push(shared, thread, "publish", "");
    // CRATONVM_DBG_CORRUPT_CELL backstop. Every instrumented door above names
    // its own receiver; this catches a corrupt cell decoded by a reader that
    // has NO door -- reflection, `Unsafe`, a class-mirror populator, a
    // serialization walk -- and reports it with this thread's frames rather
    // than letting it pass as silence. A one-door instrument cannot tell "no
    // defect" from "not my door".
    crate::memory::reclaim_guard::corrupt_cell_backstop(shared, thread);
    // Stamp when this thread last published, so a stale-address report can say
    // how many collections completed since. See `note_root_publish`.
    crate::memory::reclaim_guard::note_root_publish(shared, thread);
    // cceres3 FIX: self-heal a leaked blocked-region exit. If a blocking
    // native returned without `check_post_block_gc` (unpaired exit), this
    // thread is running with an unconsumed fixup chain / slot-origin set —
    // its frames still hold from-space addresses from every GC it slept
    // through. Apply them here, at the first safepoint publish, before this
    // thread's stale refs can leak into reachable object graphs.
    //
    // ORDER (gc-common w2-g, 2026-09-23): clear a raised flag FIRST, take and
    // apply the fixup SECOND -- the order `check_post_block_gc_refs` documents
    // for the ordinary wake. The heal used to run first. While the flag is up,
    // every pause that completes FOLDS its map into this thread's fixup, and
    // `leave_blocked_region_flagged` waits such a pause out before clearing
    // the flag; so a pause that completed between the heal's take and the
    // clear left its fold in `fixup` with nothing to apply it. The thread then
    // rebuilt its snapshot from the un-remapped frames (publishing vacated
    // addresses as roots) and ran on them until its next GC-time publish.
    // After the clear, no newer pause can complete (or fold) without this
    // thread arriving, so the fixup taken below is final.
    {
        // A raised flag at an interpreter safepoint means some raise site's
        // exit skipped `check_post_block_gc` (the monitor_wait early-return
        // bug class): this thread is RUNNING, yet every census still excludes
        // it, so moving collections keep completing under its feet. Restore
        // the invariant: wait out any in-flight pause and clear the flag
        // (idempotent with the eventual legitimate wake, whose fixup-take
        // then finds an empty map).
        if thread
            .gc_block_state
            .in_blocked_region
            .load(std::sync::atomic::Ordering::Acquire)
        {
            shared.mem.gc_barrier.leave_blocked_region_flagged(
                thread.thread_id,
                &thread.gc_block_state.in_blocked_region,
            );
            if peer_blockgc_dbg() {
                eprintln!(
                    "[blockgc] SAFEPOINT-FLAG-CLEAR tid={} - in_blocked_region was raised on a running thread",
                    thread.thread_id.0,
                );
            }
        }
        let pending = !thread.gc_block_state.fixup.lock().is_empty()
            || thread
                .gc_block_state
                .slot_origins
                .lock()
                .iter()
                .any(|so| so.cur != so.orig)
            // The native-stack write-back is a third channel with the same
            // leaked-exit exposure, and testing only the first two let a
            // thread whose ONLY pending repair was a raw stack word run on
            // with it. See `apply_native_slot_fixups`.
            || thread
                .gc_block_state
                .native_slots
                .lock()
                .iter()
                .any(|ns| ns.cur != ns.orig);
        if pending {
            let n = crate::vm::vm_exec::apply_pending_blocked_fixups(shared, thread);
            if n > 0 && peer_blockgc_dbg() {
                eprintln!(
                    "[blockgc] SAFEPOINT-HEAL tid={} applied {} pending fixups (leaked blocked-region exit upstream)",
                    thread.thread_id.0, n,
                );
            }
        }
    }
    // H2-CID0: once per collection per thread, ask whether any LIVE frame slot
    // now points into memory the collector reclaimed. The blocked-region
    // deposit/wake pair covers a PARKED thread; this covers a RUNNING one, and
    // it bounds the loss window to "since the previous safepoint" — which no
    // reader-side reporter can do, because by the time a `checkcast` or an
    // `invoke` trips over the address, any number of collections have passed.
    //
    // One relaxed load per publish on the common path; the frame walk runs only
    // when the collection counter actually moved. Unconditional, and that is
    // the point: this family has been chased across four sessions on runs that
    // were never armed.
    //
    // gc-common w7-g: the stamp is keyed by VM. It is an OS-thread local, and
    // one OS thread can publish for two VMs (a host thread attached to both);
    // comparing VM A's collection count with VM B's ran the audit for no
    // collection, or skipped it for a real one whenever the two counts
    // happened to agree. A VM switch re-seeds the stamp, like the first
    // publish does.
    {
        thread_local! {
            static AUDIT_CC: std::cell::Cell<(usize, u64)> =
                const { std::cell::Cell::new((0, u64::MAX)) };
        }
        let cc = shared.mem.heap.collection_count();
        let (prev_vm, prev) = AUDIT_CC.with(|c| c.replace((shared.vm_identity, cc)));
        if prev_vm == shared.vm_identity && cc != prev && prev != u64::MAX {
            crate::memory::reclaim_guard::audit_thread_frames(
                shared,
                thread,
                "running frame slot (safepoint)",
            );
        }
    }
    // DIAGNOSTIC-ONLY (cceres3): first-miss hunter. Once per GC epoch per
    // thread, verify no frame slot holds an already-forwarded (quarantined)
    // address at the safepoint publish. A hit here bounds the miss window to
    // "since the previous safepoint" on a RUNNING thread, which none of the
    // DEPOSIT/WAKE/ARRIVE verifiers can see.
    if peer_blockgc_dbg() {
        // Keyed by VM for the same reason as `AUDIT_CC` above (gc-common w7-g).
        thread_local! {
            static LAST_CC: std::cell::Cell<(usize, u64)> =
                const { std::cell::Cell::new((0, u64::MAX)) };
        }
        let cc = shared.mem.heap.collection_count();
        let (prev_vm, prev) = LAST_CC.with(|c| c.replace((shared.vm_identity, cc)));
        if prev_vm == shared.vm_identity && cc != prev && prev != u64::MAX {
            for (fi, fr) in thread.frames.iter().enumerate() {
                for li in 0..fr.locals_len() {
                    if let Value::Object(Some(o)) = fr.get_local(li as u16) {
                        let a = o.as_ptr() as usize;
                        if let Some(new) = shared.mem.heap.debug_forwarded_target(a) {
                            eprintln!(
                                "[blockgc] SAFEPOINT-STALE e{cc} tid={} frame#{fi} {}.{} pc={} local[{li}] 0x{a:x}->0x{new:x}",
                                thread.thread_id.0, fr.class_name(), fr.method_name(), fr.pc,
                            );
                        }
                    }
                }
                for si in 0..fr.stack.len() {
                    if let Value::Object(Some(o)) = fr.stack.peek_at(si) {
                        let a = o.as_ptr() as usize;
                        if let Some(new) = shared.mem.heap.debug_forwarded_target(a) {
                            eprintln!(
                                "[blockgc] SAFEPOINT-STALE e{cc} tid={} frame#{fi} {}.{} pc={} stack[{si}] 0x{a:x}->0x{new:x}",
                                thread.thread_id.0, fr.class_name(), fr.method_name(), fr.pc,
                            );
                        }
                    }
                }
            }
        }
    }

    // The per-thread JIT memo caches are published below as roots for as long
    // as they are present; a pause completed since this thread last probed
    // them means they can never be served again (the probe would clear them),
    // so drop them now instead of rooting a dropped `HashMap` for another
    // cycle. `docs/feature-designs/jit-ir-optimizations.md`.
    crate::memory::roots::drop_stale_jit_memo_caches(shared, thread);

    let _rs_t0 = if rootsnap_dbg_enabled() {
        Some((std::time::Instant::now(), thread.frames.len()))
    } else {
        None
    };
    // Each frame's class owner (defining user loader / non-strong hidden
    // mirror), gathered BEFORE the snapshot lock: see `roots::frame_class_owners`
    // (gc-common w19-a). Appended after the frame scan, outside the frozen-frame
    // cache's per-frame ranges.
    let class_owners = crate::memory::roots::frame_class_owners(&shared.loader_identity, thread);
    // Lock via an Arc clone so the guard does not borrow `thread`, leaving the
    // disjoint `frames` / `rs_cache` fields freely (mutably) borrowable below.
    let snap_arc = thread.root_snapshot.clone();
    let mut snapshot = snap_arc.lock();
    snapshot.clear();

    // The frozen-frame cache yields to the default path while the
    // conservative-locals hardening is engaged: the cached `scan_frame_roots`
    // does not run `scan_locals_conservative`, so caching under it could drop a
    // lost-tag local from the snapshot. `conservative_locals_enabled()` reads
    // the RESOLVED real-ForkJoinPool flag and is additionally gated on GC
    // quiescence, i.e. it answers "is the hardening running right now" rather
    // than "was the lane explicitly requested".
    //
    // There is no separate real-ForkJoinPool bypass any more (2026-07-31): the
    // old one keyed off the PRESENCE of `CRATONVM_REAL_FORKJOINPOOL`, which
    // stopped being set when the real pool became the default, so it had not
    // fired on a default run since that flip. Before removing it the cache was
    // measured against the loss it claimed to prevent — see
    // `rootsnap_verify_enabled` (`CRATONVM_DBG_ROOTSNAP_VERIFY`), which
    // re-scans every frame the uncached way after each cached snapshot and
    // reports any root the cached snapshot lacks; the real-lane
    // Fork6/Fork6Hard GC-stress repros reported none.
    //
    // The mechanism behind that result: the real-FJP lane keeps a Bridge native
    // surface for `submit`/`invoke`/`fork`/`join`, which runs every task INLINE
    // on the submitting thread (measured: every `compute()` in this lane runs
    // on `main`, vs 15 pool workers on HotSpot). The hazard the bypass
    // described — a pool worker holding a forked subtask in its own frames
    // across a peer-triggered collection — has no thread to happen on. If real
    // ForkJoinPool ever gains workers that actually execute tasks, re-run the
    // verifier against that lane before trusting this.
    let conservative_locals = crate::memory::roots::conservative_locals_enabled();
    // gch h1/jp (f1f parity): every local is a root while a debugger can read
    // locals, exactly as the initiator's `collect_roots` (a config fact, read
    // once per deposit).
    let observes_locals = crate::runtime::jvmti::debugger_observes_locals(shared);
    // gcg g6/jn: see `GcBlockState::deposit_conservative`.
    thread
        .gc_block_state
        .deposit_conservative
        .store(conservative_locals, std::sync::atomic::Ordering::Release);
    if crate::runtime::env_cache::rootsnap_cache() && !conservative_locals {
        // ── Frozen-frame cached path ────────────────────────────────────────
        // Reuse the cached roots of the deep, continuously-frozen frames and
        // re-scan only the churning top. Correctness rests on the LIFO stack
        // discipline: if `frames[k]` is still the SAME instance (`seq`
        // unchanged) it has never been popped, so by the stack property every
        // frame *below* it (`0..k`) has been continuously present AND frozen
        // (you cannot pop the middle of a stack) — their cached roots are still
        // exact. A GC may have *moved/promoted* objects, changing addresses, so
        // the whole cache is only valid while `collection_count()` is unchanged.
        let gen = shared.mem.heap.collection_count();
        let len = thread.frames.len();
        // Longest prefix whose frame instances are unchanged since the cache
        // was built (prefix-closed: a matching `frames[p]` implies every frame
        // below it matches too).
        let mut p = 0usize;
        if gen == thread.rs_cache_gen {
            let maxk = thread.rs_cache.len().min(len);
            while p < maxk
                && thread.frames[p].seq != 0
                // Key on (seq, exec_epoch): seq proves the frame was never
                // popped; exec_epoch proves it has not RE-EXECUTED (and thus
                // possibly reassigned a local) since it was cached. A mismatch in
                // either ends the reusable prefix so this frame and all above it
                // are re-scanned — the fix for the stale-reassigned-local AME.
                && (thread.frames[p].seq, thread.frames[p].exec_epoch) == thread.rs_cache[p].0
            {
                p += 1;
            }
        }
        // The deepest matching frame (`p-1`) VOUCHES for everything strictly
        // below it, but its own above-neighbour is the divergence point, so its
        // own roots may be stale — exclude it. Never reuse the current top
        // (index `len-1`), which churns its operand stack between snapshots.
        let reuse = p.saturating_sub(1).min(len.saturating_sub(1));

        // Build the next cache as we go (frozen frames `0..len-1`); the top is
        // never cached.
        let mut new_cache: Vec<((u64, u64), Vec<ObjectRef>)> =
            Vec::with_capacity(len.saturating_sub(1));
        if _rs_t0.is_some() {
            use std::sync::atomic::Ordering::Relaxed;
            shared.mem.diagnostics.rootsnap_cached_calls.fetch_add(1, Relaxed);
            shared.mem.diagnostics.rootsnap_reused_frames.fetch_add(reuse as u64, Relaxed);
            shared.mem.diagnostics.rootsnap_reused_roots.fetch_add(
                thread.rs_cache[..reuse]
                    .iter()
                    .map(|e| e.1.len() as u64)
                    .sum::<u64>(),
                Relaxed,
            );
        }
        // (a) reused frozen frames — copy their cached roots into the snapshot
        //     and carry the entry forward (move, no re-alloc).
        for i in 0..reuse {
            snapshot.extend_from_slice(&thread.rs_cache[i].1);
            new_cache.push(std::mem::take(&mut thread.rs_cache[i]));
        }
        // (b) re-scan `reuse..len`. Frozen ones (`< len-1`) are scanned into the
        //     snapshot and cached; the top is scanned into the snapshot only.
        for i in reuse..len {
            let start = snapshot.len();
            scan_frame_roots(
                &thread.frames[i],
                &mut snapshot,
                &shared.mem.heap,
                crate::runtime::value_stack::MINTED_ONLY_PRIMITIVE_STACK_ROOTS,
                observes_locals,
                Some(&*shared.classes.type_maps),
            );
            // The frame's implicit `monitorexit` target -- see the default
            // path below. Cached with the frame: it is fixed for the frame's
            // life and remapped with the rest of the cache.
            if let Some(m) = thread.frames[i].monitor_on_exit {
                snapshot.push(m);
            }
            // Block monitors (`Frame::held_monitors`, wave 23 lane L7). Unlike
            // `monitor_on_exit` they change during the frame's life, but only
            // while it is the top frame, which is never cached, and a frame
            // becomes the top again only through a return into it, which
            // bumps the `exec_epoch` the cache key includes.
            snapshot.extend_from_slice(thread.frames[i].held_monitors.as_slice());
            if i + 1 < len {
                new_cache.push((
                    (thread.frames[i].seq, thread.frames[i].exec_epoch),
                    snapshot[start..].to_vec(),
                ));
            }
        }
        thread.rs_cache = new_cache;
        thread.rs_cache_gen = gen;
        if rootsnap_verify_enabled() {
            use std::sync::atomic::Ordering::Relaxed;
            let mut fresh: Vec<ObjectRef> = Vec::with_capacity(snapshot.len());
            for frame in &thread.frames {
                scan_frame_roots(
                    frame,
                    &mut fresh,
                    &shared.mem.heap,
                    crate::runtime::value_stack::MINTED_ONLY_PRIMITIVE_STACK_ROOTS,
                    observes_locals,
                    Some(&*shared.classes.type_maps),
                );
            }
            shared.mem.diagnostics.rootsnap_verified.fetch_add(1, Relaxed);
            let have: std::collections::HashSet<usize> =
                snapshot.iter().map(|o| o.as_ptr() as usize).collect();
            let mut missed = 0u64;
            for (i, o) in fresh.iter().enumerate() {
                let a = o.as_ptr() as usize;
                if !have.contains(&a) {
                    missed += 1;
                    if shared.mem.diagnostics.rootsnap_misses.load(Relaxed) < 40 {
                        eprintln!(
                            "[ROOTSNAP-MISS] tid={} depth={} reuse={} fresh_idx={} addr=0x{a:x} top={}.{}",
                            thread.thread_id.0,
                            len,
                            reuse,
                            i,
                            thread.frames[len - 1].class_name(),
                            thread.frames[len - 1].method_name(),
                        );
                    }
                }
            }
            if missed > 0 {
                shared.mem.diagnostics.rootsnap_misses.fetch_add(missed, Relaxed);
                shared.mem.diagnostics.rootsnap_miss_calls.fetch_add(1, Relaxed);
            }
            let n = shared.mem.diagnostics.rootsnap_verified.load(Relaxed);
            if n == 1 || n % rootsnap_dbg_every() == 0 {
                eprintln!(
                    "[ROOTSNAP-VERIFY] verified_snapshots={} miss_snapshots={} missed_roots={}",
                    n,
                    shared.mem.diagnostics.rootsnap_miss_calls.load(Relaxed),
                    shared.mem.diagnostics.rootsnap_misses.load(Relaxed),
                );
            }
        }
    } else {
        // ── Default path (unchanged) ────────────────────────────────────────
        // Multi-thread non-moving-sweep root hardening (Fork6): see
        // `roots::conservative_locals_enabled`. Capture lost-tag object refs in
        // THIS thread's frame locals so a parked/running worker (or main)
        // publishes them, pinning them against selective-promotion evacuation.
        // (`conservative_locals` was computed above, where it also gates the
        // cached path off so this hardening is never skipped.)
        // gch h3/yc (`gch-h2yg-parked-conservative-snapshot-...`): the PRECISE
        // words of a conservative deposit -- the liveness-mapped locals, the
        // ruled operand stack, the monitors -- for the unrecorded-young-root
        // guard (`GcBlockState::precise_deposit_words`). Empty otherwise.
        let mut precise_words: Vec<usize> = Vec::new();
        for frame in &thread.frames {
            let mapped_start = snapshot.len();
            // Apply the same bytecode liveness proof as the initiator. A
            // conservative tag probe must not revive a completed worker from
            // a dead local in a parked peer's snapshot. (gch h1/jp: and the
            // same debugger answer -- every local while one can read them.)
            if observes_locals {
                frame.scan_local_objects_all_live(&mut snapshot, &shared.mem.heap);
            } else {
                frame.scan_local_objects_mapped(
                    &mut snapshot,
                    &shared.mem.heap,
                    &shared.classes.type_maps,
                );
            }
            if conservative_locals {
                precise_words.extend(snapshot[mapped_start..].iter().map(|o| o.as_ptr() as usize));
                frame.scan_locals_conservative(&mut snapshot, &shared.mem.heap);
            }
            let before = snapshot.len();
            // Wave 122, lane L7: the frame's depth-matched map row under
            // `CRATONVM_INTERP_PRECISE_OOP_MAPS`; unchanged with it off.
            frame.scan_stack_objects_mapped(
                &mut snapshot,
                &shared.mem.heap,
                crate::runtime::value_stack::MINTED_ONLY_PRIMITIVE_STACK_ROOTS,
                Some(&*shared.classes.type_maps),
            );
            // Validate every operand-stack-sourced root against the heap.
            // `Frame::scan_local_objects` was already cleaned to drop the
            // pointer-shaped-Long heuristic; the operand-stack scanner is
            // restricted from edits, so filter at the boundary instead.
            //
            // Done IN PLACE (compact valid entries down over the invalid ones,
            // then truncate) rather than `split_off` — `update_root_snapshot` runs
            // on every object-returning native call (tens of millions during an
            // embedded-server deploy), and the old `split_off` allocated a fresh
            // Vec for every frame that had operand-stack objects. The in-place
            // retain is allocation-free and keeps identical semantics.
            let len = snapshot.len();
            if len > before {
                let mut write = before;
                for read in before..len {
                    let o = snapshot[read];
                    // `is_heap_addr`, not `is_object_address` — see
                    // `scan_frame_roots`, whose body this loop duplicates.
                    // Cast: object/code pointer to integer address
                    if shared.mem.heap.is_heap_addr(o.as_ptr() as usize).is_some() {
                        snapshot[write] = o;
                        write += 1;
                    }
                }
                snapshot.truncate(write);
            }
            if conservative_locals {
                precise_words.extend(snapshot[before..].iter().map(|o| o.as_ptr() as usize));
                frame
                    .stack
                    .scan_object_refs_conservative(&mut snapshot, &shared.mem.heap);
            }
            // A synchronized method's implicit `monitorexit` target
            // (gc-common w2-g, 2026-09-23). The initiator (`collect_roots` §1)
            // and the blocked deposit (`deposit_root_snapshot_inner`) publish
            // it; this safepoint deposit did not, while its resume half
            // (`apply_pointer_map_to_thread`) has always REWRITTEN it. The
            // receiver usually still sits in local 0, but the local scan above
            // is liveness-filtered: once local 0 is dead at the parked pc (or
            // the bytecode reused the slot) the lock object had no root in a
            // peer-initiated collection, and the frame's pop then ran
            // `monitorexit` on a reclaimed address.
            if let Some(m) = frame.monitor_on_exit {
                snapshot.push(m);
                if conservative_locals {
                    precise_words.push(m.as_ptr() as usize);
                }
            }
            // Block monitors (`Frame::held_monitors`, wave 23 lane L7);
            // remapped by `apply_pointer_map_to_thread`.
            snapshot.extend_from_slice(frame.held_monitors.as_slice());
            if conservative_locals {
                precise_words.extend(
                    frame
                        .held_monitors
                        .as_slice()
                        .iter()
                        .map(|o| o.as_ptr() as usize),
                );
            }
        }
        // Written whole under the snapshot lock (order snapshot -> this, as
        // `ThreadRegistry::collect_all_root_snapshots` reads them).
        *thread.gc_block_state.precise_deposit_words.lock() = precise_words;
    }
    // A live activation keeps its class (gc-common w19-a,
    // `common-w18d-peer-interpreter-activations-do-not-keep-their-class`): the
    // initiator's `collect_roots` §1 has always pushed these for its own
    // frames; a parked peer's frames reach a collection only through here.
    snapshot.extend_from_slice(&class_owners);
    // Every reference held in a `JvmThread` FIELD rather than a frame, through
    // the ONE list the initiator's scan and the blocked deposit also use
    // (gc-common w4-b; `handoff-w3b-peer-deposits-use-push-thread-field-roots.md`):
    // native invoke pins, rooted handle slots (a peer-initiated collection
    // sees this parked thread only through `root_snapshot`), the native
    // old-gen allocation pool, the native object in flight, the print buffer,
    // scoped values, both parked throwables, both JIT memo caches (stale ones
    // dropped above -- TOMCAT-JNDIREALM-JIT.3 was a case-cache entry missing from
    // this deposit), this thread's own `java.lang.Thread` mirror (Tomcat
    // TestDigestAuthenticator: a worker-thread GC orphaned the parked JUnit
    // main thread's mirror) and any pending async exception. The resume-side
    // remap of all of them is `memory::gc::remap_thread_off_frame_refs`, called
    // from `apply_pointer_map_to_thread`.
    crate::memory::roots::push_off_frame_thread_roots(thread, &mut snapshot);
    // The JIT's stashed deopt / exceptional frames (gc-common w2-g,
    // 2026-09-23). They live in `jit/` thread-locals no peer collector can
    // reach, so this deposit is the only view a cross-thread collection has of
    // them while this thread is parked at the safepoint -- and a callee-handler
    // path that can reach a blocking point with a frame stashed reaches an
    // interpreter safepoint poll in the same window
    // (`docs/jit/deopt-thread-local-roots.md`). The blocked deposit
    // (`deposit_root_snapshot_inner`) has always done this; the initiator does
    // it in `collect_roots` §10. The remap half is in
    // `apply_pointer_map_to_thread`. Two thread-local probes and a `RefCell`
    // borrow when both stashes are empty, which is the normal case.
    cratonvm_jit::deopt::for_each_stashed_deopt_object(|addr| {
        if let Some(obj) = shared.mem.heap.is_object_address(addr as usize) {
            snapshot.push(obj);
        }
    });
    // JNI local references (INT-5, safepoint half): a JNI native that
    // obtained local refs and re-entered Java parks HERE — and a
    // cross-thread collector marks this thread only from this snapshot, so
    // an object reachable solely through this thread's `JNI_LOCAL_FRAMES`
    // was reclaimed. Thread-local storage; this deposit always runs on the
    // owning thread. The resume-side remap is `update_local_refs_after_gc`
    // in `apply_pointer_map_to_thread`.
    crate::native::jni::collect_local_ref_roots(&mut snapshot);

    // COLLECTOR FIRST. `moving_young_enabled()` ANDs a JIT-side gate with
    // `flags().gc.moving_young` and consults the collector in neither, so
    // under G1 and ZGC this whole question is inert — and answering it is
    // not free: `refresh_moving_young_coverage_for_current_thread` ends in
    // an UNMEMOISED `native_stack_has_jit_frame` over the full band, on
    // every blocked-region entry.
    //
    // Measured on `DefaultCatalogAndSchemaTest` under the default (ZGC)
    // collector, 240 s: the probe ran 1,139,842 times reading 35.2 BILLION
    // stack words. With the moving-young term forced off
    // (`CRATONVM_NO_MOVING_YOUNG=1`) it ran 494,968 times reading 15.3
    // billion — exactly one probe per blocked deposit instead of two, and
    // 20 billion fewer words, for a decision no non-moving collector reads.
    // gc-common w21-e (`handoff-w21e-bind-the-safepoint-root-deposit`): the
    // coverage proof, the conservative JIT scan and the pin publication below
    // write this VM's moving-young coverage rows (`gc_quiescence::CoverageCycle`).
    // Bind THIS VM's pause ledger for them, as `deposit_root_snapshot_inner`
    // does. A parking peer has no binding of its own, so without this scope its
    // writes land in the process's orphan rows, which every VM reads. The
    // initiator's call (after its request won) is already bound to the same
    // ledger, and the scope restores that binding when it drops.
    let ledger_scope =
        cratonvm_gc::gc_quiescence::scoped_pause_ledger(shared.mem.gc_barrier.pause_ledger());
    // (The JIT scans below read this VM's type maps when the verifier oracle
    // is armed; `bind` is a cached-bool no-op otherwise.)
    let _oracle_maps =
        crate::jit::conservative_roots::OracleTypeMapsScope::bind(&shared.classes.type_maps);
    let moving_young_precise_only = shared.mem.heap.is_generational()
        && crate::jit::conservative_roots::moving_young_enabled()
        && crate::jit::conservative_roots::refresh_moving_young_coverage_for_current_thread()
        && !cratonvm_gc::gc_quiescence::moving_young_coverage_incomplete();

    // Cross-thread JIT-root hardening: also publish the conservative roots of
    // every active JIT frame on THIS thread into the snapshot.
    //
    // The cross-thread STW collector reads each thread's `root_snapshot` (via
    // `collect_all_root_snapshots`) — it does NOT call `collect_roots` for a
    // non-current thread (that path's `scan_active_jit_frames` is thread-local
    // and would scan the *collector's* empty JIT chain, not the parked
    // worker's). So an object whose only live reference lives in a *parked*
    // worker thread's JIT spill slot was absent from the snapshot the collector
    // marks from — a latent reclamation risk for the non-moving young-gen sweep
    // (relocation is already prevented: the collector runs non-moving while any
    // thread is in JIT). `update_root_snapshot` always runs on the thread it is
    // snapshotting, so the thread-local scan captures exactly that worker's live
    // JIT spill region; empty (no-op) when not in JIT; false positives filtered
    // by `is_object_address`.
    //
    // NOTE: this closes a real latent gap but does NOT fix the avrora real-RAF
    // SEGV — that crash was experimentally shown to be NOT a GC reclamation /
    // relocation bug (it reproduces with the young sweep capturing these JIT
    // roots and with the concurrent old-gen collector disabled). See
    // real-raf-segv-root-cause.md.
    if !moving_young_precise_only {
        // `update_root_snapshot` is also called at ordinary native-call
        // boundaries, not only immediately before a safepoint.  Its JIT-root
        // contribution is therefore a future cross-thread collector's only
        // view of this thread while it is parked.  Do not let the per-thread
        // scan cache republish a scan taken before the interpreted/native
        // callee below the JIT frame allocated a new live object: none of the
        // cache's keys change for that mutation.  The next collector could
        // otherwise reclaim the omitted object and hand a zero-header slot
        // back to compiled code.  This mirrors the safepoint and blocked
        // snapshot paths, both of which already invalidate before publishing.
        crate::jit::conservative_roots::invalidate_scan_cache_for_gc();
        let jit_scan_start = snapshot.len();
        crate::memory::native_roots::rootprof::note_scan_caller(1); // safepoint
                                                                    // r12w1 g1store: the pins owed by band words the G1 root screen rejects.
        let reject_capture =
            crate::jit::conservative_roots::g1_band_reject_pins::Capture::arm_if_wanted(
                &shared.mem.heap,
                false,
            );
        crate::jit::conservative_roots::scan_active_jit_frames(&shared.mem.heap, &mut snapshot);
        let reject_pins = reject_capture.map(|c| c.finish()).unwrap_or_default();
        // G1 pin-in-place, cross-thread half: the snapshot keeps these
        // conservatively-discovered objects ALIVE, but under G1 (a moving
        // collector) their regions must also be EXCLUDED from the collection
        // set — the JIT register/spill slots holding them cannot be
        // rewritten when the object moves. The initiator only publishes its
        // OWN JIT roots (roots.rs); every parked/blocked mutator must
        // publish here, into the process-global per-thread pin registry
        // consumed by `G1Collector::jit_pinned_region_set`. Replace
        // semantics: a deposit with no live JIT frames clears this thread's
        // stale pins.
        // 2026-09-06: NOT `is_g1()` any more. See `g1_only_jit_pins` for the
        // two collectors this gate had silently stopped covering and for the
        // measurement.
        if !g1_only_jit_pins() || shared.mem.heap.is_g1() {
            // gci i17/dk (i16/dp D4): on the stack up to 32 addresses (the
            // republish-compare bound), not a heap `Vec` per deposit, as the
            // blocked deposit does since i16/dp. Same addresses, same order:
            // the reject pins, when any, after the scan's (`extend_published`
            // appends nothing for an empty list).
            let mut addrs: smallvec::SmallVec<[usize; 32]> = snapshot[jit_scan_start..]
                .iter()
                .map(|r| r.as_ptr() as usize)
                .collect();
            if !reject_pins.is_empty() {
                let mut extra: Vec<usize> = Vec::new();
                crate::jit::conservative_roots::g1_band_reject_pins::extend_published(
                    &reject_pins,
                    &mut extra,
                );
                addrs.extend(extra);
            }
            cratonvm_gc::gc_quiescence::publish_pinned_jit_roots(&addrs);
        }
    } else if !g1_only_jit_pins() || shared.mem.heap.is_g1() {
        // Precise-relocation mode covers every JIT oop with rewritable
        // shadow-stack slots — no conservative pins needed; drop stale ones.
        cratonvm_gc::gc_quiescence::publish_pinned_jit_roots(&[]);
    }
    crate::native::jni::publish_raw_local_pins();
    drop(ledger_scope);

    // §4 (multi-thread shadow scan, marking half). Also publish THIS thread's
    // shadow-stack precise roots into the snapshot. With `CRATONVM_SHADOW_STACK`
    // the moving collector is allowed to run while threads are in JIT, so a
    // cross-thread STW cycle (which marks from each parked thread's
    // `root_snapshot`, never `collect_roots`) must see a parked worker's
    // shadow-held oops or they are reclaimed. Mirrors the current-thread fold-in
    // in `roots.rs`; every slot is an oop by construction (re-validated via
    // `is_object_address`). The matching remap is in `apply_pointer_map_to_thread`.
    // No-op when the gate is off or the shadow stack is empty/unallocated.
    if crate::jit::conservative_roots::shadow_stack_enabled() {
        thread.shadow_stack.for_each_value(|v| {
            if let Some(obj_ref) = shared.mem.heap.is_object_address(v) {
                snapshot.push(obj_ref);
            }
        });
    }
    if let Some((t0, nframes)) = _rs_t0 {
        use std::sync::atomic::Ordering::Relaxed;
        drop(snapshot); // release the lock before the (rare) print
        let calls = shared.mem.diagnostics.rootsnap_calls.fetch_add(1, Relaxed) + 1;
        // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
        shared.mem.diagnostics.rootsnap_nanos.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
        // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
        shared.mem.diagnostics.rootsnap_frames.fetch_add(nframes as u64, Relaxed);
        if calls == 1 || calls % rootsnap_dbg_every() == 0 {
            let nanos = shared.mem.diagnostics.rootsnap_nanos.load(Relaxed);
            let frames = shared.mem.diagnostics.rootsnap_frames.load(Relaxed);
            eprintln!(
                "[ROOTSNAP] calls={} total_ms={} avg_us={:.2} avg_frames={:.1} cached_calls={} reused_frames={} reused_roots={}",
                calls,
                nanos / 1_000_000,
                // Cast: numeric/representation conversion
                (nanos as f64 / calls as f64) / 1000.0,
                // Cast: numeric/representation conversion
                frames as f64 / calls as f64,
                shared.mem.diagnostics.rootsnap_cached_calls.load(Relaxed),
                shared.mem.diagnostics.rootsnap_reused_frames.load(Relaxed),
                shared.mem.diagnostics.rootsnap_reused_roots.load(Relaxed),
            );
        }
    }
}


/// Publish `thread`'s LIVE call stack into its `frame_trace`, with the frame
/// that locked each held monitor and the lock stack of the same instant, run
/// by the thread itself at a poll: what a frame-trace pause's arrival
/// publishes ([`safepoint_check`]), and since gci i23/hs what a frame-trace
/// handshake publishes ([`frame_trace_handshake_op`]).
pub(crate) fn publish_live_frame_trace(shared: &SharedVm, thread: &JvmThread) {
    // With the compiled activations (wave 38, lane L7).
    // With the reflective calls' JDK frames (wave 45, lane L3).
    // With the served `Thread.run` rows (round 15 wave 6, lane trace6).
    let trace = crate::runtime::stackwalker::capture_published_trace(
        shared,
        &thread.frames,
        &thread.reflective_calls,
        &thread.reflective_frames_named,
        &thread.served_thread_run,
    );
    let trace_len = trace.entries.len();
    *thread.frame_trace.lock() = trace.entries;
    // With the trace, the frame that locked each held monitor
    // (`ThreadInfo.getLockedMonitors()`, wave 24, lane L7).
    shared.threads.thread_registry.publish_jmx_frame_monitors(
        thread.thread_id,
        &thread.frames,
        &thread.jmx_frame_monitors_published,
        trace.frame_positions.as_deref().map(|p| (p, trace_len)),
        &trace.compiled_holds,
    );
    // JIT round 19 wave 12, lane gc12: and the lock stack of this
    // same instant (`CRATONVM_JMX_RUNNING_LOCKS_AT_TRACE`).
    record_pause_trace_locks(shared, thread.thread_id, trace_len);
}

/// gci i23/hs (step 2 of `common-a-proposal-per-thread-handshakes`, opt-in
/// `CRATONVM_GC_HANDSHAKE_FRAME_TRACE`): the handshake a cross-thread
/// `getStackTrace()` queues for a running peer
/// (`vm_exec::thread_stack_trace_served`) instead of stopping the world. The
/// peer runs it at its next [`safepoint_check`]: [`publish_live_frame_trace`],
/// then one `root_publish_seq` bump, so the JNI in-native repeat
/// (`native::jni::InNativeRepeat`, keyed on that sequence) does not keep this
/// trace -- taken at the poll, not at the native's call site -- as the one
/// its previous deposit published.
pub(crate) fn frame_trace_handshake_op() -> crate::threading::jvm_thread::HandshakeOp {
    Box::new(|shared: &SharedVm, thread: &JvmThread| {
        publish_live_frame_trace(shared, thread);
        thread
            .root_publish_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    })
}


/// Check if a stop-the-world pause is requested and participate if so.
///
/// Called at safepoints: allocation sites and backward branches (loop iterations).
/// If STW is active, this thread deposits its roots and waits for GC to complete,
/// then applies the pointer map to update its own frame references.
pub(crate) fn safepoint_check(shared: &SharedVm, thread: &mut JvmThread) {
    use std::sync::atomic::Ordering;
    // Execution profiler, off unless `CRATONVM_PROFILE_SAMPLE_MS` is set: one
    // relaxed load of a latched `Option<u64>` on the default path. Placed at
    // the TOP so a sample reflects the frame that was running, not whatever
    // the safepoint machinery below leaves on the stack. See
    // `runtime::exec_sampler` for the two biases this sampling point carries.
    crate::runtime::exec_sampler::maybe_sample(thread);
    // Every thread passes here at a GC pause, including one that has stopped
    // reaching the tier-up strides that also call this. One load and a compare
    // unless something was withdrawn since this thread last looked.
    super::jit_bridge::release_withdrawn_code_owners(thread);
    // Same shape for a class redefinition: this thread's frames that run a
    // body it replaced move onto their translated copies (interpreter round
    // i1 wave 19, lane L3; `obsolete_frames::after_redefinition` takes the
    // pause that brings every running thread here).
    super::obsolete_frames::convert_obsolete_frames_if_redefined(shared, thread);
    // CRATONVM_DBG_BLOCKED_ACCESS: reaching an interpreter safepoint with the
    // thread's own `in_blocked_region` flag still raised means every STW
    // census is excluding a RUNNING mutator — a moving GC can complete under
    // its feet, and nothing ever applies its accumulated blocked-fixup. This
    // catches stuck flags from any raise site whose wake/error path skipped
    // `check_post_block_gc` (the monitor_wait early-return bug class). No-op
    // when the gate is off.
    if cratonvm_gc::blocked_access_debug::enabled()
        && thread
            .gc_block_state
            .in_blocked_region
            .load(Ordering::Acquire)
    {
        cratonvm_gc::blocked_access_debug::report_blocked_violation(
            "interpreter safepoint reached with in_blocked_region raised",
            0,
        );
    }
    // bc math-ec 0x4 (CRATONVM_DBG_MEMWATCH): O(1) poll of one absolute
    // watched address at full safepoint frequency — catches the corrupting
    // write within one safepoint window, with the live Java stack. Off ⇒
    // a single predicted branch. On a HIT the dump includes the TOP frame's
    // locals (raw bits + array header/extent for in-heap-shaped values) so
    // the watched address can be placed inside/outside the frame's array
    // receivers (legit-write-to-reused-slot vs stale/OOB receiver).
    crate::runtime::memwatch::poll("safepoint", || {
        let mut s = thread
            .frames
            .iter()
            .rev()
            .take(28)
            .map(|f| {
                format!(
                    "  {}.{}{} pc={}",
                    f.class_name(),
                    f.method_name(),
                    f.method_descriptor(),
                    f.pc
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(top) = thread.frames.last() {
            s.push_str("\n  -- top-frame locals --");
            let n = top.locals_len().min(10);
            for i in 0..n {
                let raw = top.get_local_raw(i);
                // Widening: small integer index -> usize (non-negative, fits in pointer width)
                let p = (raw & 0x0000_7fff_ffff_ffff) as usize;
                let mut extra = String::new();
                if p != 0 && p % 8 == 0 && shared.mem.heap.is_heap_addr(p).is_some() {
                    // SAFETY: in-heap address; first 16 header bytes of managed
                    // memory are always readable (raw bytes, not enum fields).
                    let (kind_b, elem_b, alen) = unsafe {
                        let q = p as *const u8;
                        (
                            cratonvm_types::kind_tag_at(q),
                            cratonvm_types::element_type_tag_at(q),
                            // Cast: reinterpret pointer/address to typed pointer
                            (q.add(cratonvm_types::ARRAY_LENGTH_OFFSET) as *const u32)
                                .read_unaligned(),
                        )
                    };
                    extra = format!(
                        " [heap obj kind={kind_b} elem={elem_b} len={alen} data=0x{:x}..0x{:x}]",
                        p + 40,
                        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                        p + 40 + (alen as usize) * 8,
                    );
                }
                s.push_str(&format!("\n  local[{i}] = 0x{raw:x}{extra}"));
            }
        }
        s
    });
    // gci i23/hs (step 1 of `common-a-proposal-per-thread-handshakes`): the
    // per-thread handshake poll. Run what other threads queued for this
    // thread (`ThreadRegistry::request_handshake`; today only the opt-in
    // frame-trace op, `CRATONVM_GC_HANDSHAKE_FRAME_TRACE`), on this thread,
    // BEFORE it parks for a pause: a requester stops waiting once a pause is
    // requested and withdraws what this thread has not taken, so an op is
    // never left for after the pause. One `Acquire` load of this thread's
    // own queue word when nothing is queued (always, with the switch off).
    // The poll's reach is this function's: every allocation slow path
    // (`maybe_gc`), each pause arrival, a JIT poll's slow path; an
    // interpreted loop that does not allocate reaches it only at a pause
    // (its `LoopPollWord` is not bumped for a handshake: P4/ph), so its
    // requester falls back to the frame-trace pause after its budget.
    thread.run_pending_handshakes(shared);
    // Frozen-fact snapshots run on class init and JIT compile offers, not safepoint polls.
    if shared.mem.gc_barrier.stw_requested.load(Ordering::Acquire) {
        // gcj j3/rt3 residual 4: a handshake op must not wait for a pause
        // (`HandshakeOp`'s contract); debug builds fail it here.
        thread.gc_block_state.handshakes.debug_assert_no_op_running();
        // CRIT (TLAB UAF) — retire this thread's TLAB before parking for GC,
        // exactly as the GC initiator does in `maybe_gc`. The moving collector
        // run by the initiator can `grow()` (realloc) the young arena while we
        // are parked, freeing the old backing buffer; a TLAB that still held
        // `[cursor,end)` into that buffer would then be dangling, so the first
        // post-GC fast-path bump on this thread writes the object header into
        // freed/unmapped memory → EXCEPTION_ACCESS_VIOLATION in
        // `init_object_header` (deterministic once a grow frees the old buffer;
        // reproduces with JIT off because that path always uses the moving
        // collector). Retiring empties the TLAB so the next allocation refills
        // from the current arena, and installs a walkable filler over
        // `[cursor,end)` so the collector's from-space walk doesn't desync on
        // the unfilled tail (the same reason the initiator retires).
        flush_tlab_allocation_batch(thread, shared);
        thread.tlab.retire();
        // Round-5 fix (CRIT — UAF): drain this thread's per-thread SATB
        // buffer into the global queue BEFORE we park at the barrier.
        // The thread-local buffer holds up to 256 overwritten references;
        // without an explicit flush they sit invisible to the marker and
        // the next evacuation cycle turns the SATB lost-object scenario
        // into a use-after-free. Must run on every mutator at every
        // safepoint arrival — `flush_thread_satb` short-circuits cheaply
        // on `is_active() == false` when concurrent marking is idle.
        shared.mem.heap.flush_thread_satb();

        // Update root snapshot before pausing. This snapshot is the ONLY view a
        // cross-thread STW collector has of this (parked) thread's roots, so the
        // JIT-frame portion must be a fresh scan, not the possibly-stale cached
        // one (see `invalidate_scan_cache_for_gc`): the conservative scan covers
        // the native stack below this thread's JIT frames, which mutated since
        // the last boundary bump, and a dropped live root would be reclaimed by
        // the collector this thread is about to park for.
        crate::jit::conservative_roots::invalidate_scan_cache_for_gc();
        update_root_snapshot(shared, thread);
        if remap_trace_on() {
            let snap = thread.root_snapshot.lock().clone();
            deposit_gap_diff(thread, &snap, "publish");
        }
        // Publish this thread's LIVE call stack, when — and only when — some
        // other thread is at this moment taking a cross-thread
        // `Thread.getStackTrace()` / `dumpThreads()` (see
        // [`stw_publish_frame_traces`]).
        //
        // `frame_trace` is otherwise written at the BLOCKING deposit points
        // only, which is the right place for a parked thread (it shows the
        // blocking call site) and useless for a running one: a thread that has
        // never blocked publishes nothing, and one that has publishes where it
        // blocked LAST. That is what made cross-thread `getStackTrace()` return
        // an empty array for any thread actually executing Java code.
        //
        // Gated on the request flag rather than unconditional: this runs inside
        // every safepoint park, i.e. on every mutator on every GC pause, and the
        // capture allocates a `Vec` per thread. A thread dump is rare; a GC
        // pause is not. Unset, this is one relaxed load.
        if frame_trace_wanted(shared) {
            publish_live_frame_trace(shared, thread);
            // gci i23/hs finding 1 (i23-hs-report.md): this trace was taken
            // at the POLL, not at a native's call site. Bump the sequence the
            // JNI in-native repeat (`native::jni::InNativeRepeat`) is keyed
            // on, so a repeated in-native deposit from the same compiled call
            // site re-publishes its own trace instead of keeping this one --
            // the same bump `frame_trace_handshake_op` makes.
            thread
                .root_publish_seq
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        // Cross-thread JIT coverage handshake, PEER half. This thread is about
        // to park; the initiator is about to ask whether every live compiled
        // frame in the process is rewritable, and this thread's chain is the
        // part of that question only this thread can answer (`JIT_ENTRY_CHAIN`,
        // the cached top RBP and the shadow window are all thread-local).
        //
        // It belongs HERE and not in `update_root_snapshot` above, even though
        // that function already computes the same proof for the generational
        // collector: `update_root_snapshot` is also the blocked-region deposit
        // path, which runs on every blocking native call, and its own comment
        // records the measurement that made the proof generational-only there
        // (1.1 M probes reading 35 billion stack words on one 240 s run). A
        // safepoint park happens once per thread per pause, so the same proof
        // costs nothing here — and this is the only site where the deposit's
        // promise holds, because a thread that reaches this line resumes
        // through `apply_pointer_map_to_thread` and remaps its own frames.
        //
        // Into THIS VM's ledger (gc-common w8-d): the deposit is bound to the
        // barrier this thread is about to arrive at, so it can only ever be
        // credited to this VM's pause -- never to another VM's initiator
        // (`cratonvm_gc::gc_quiescence::PauseLedger`).
        let oracle_maps =
            crate::jit::conservative_roots::OracleTypeMapsScope::bind(&shared.classes.type_maps);
        cratonvm_gc::gc_quiescence::with_pause_ledger(shared.mem.gc_barrier.pause_ledger(), || {
            crate::jit::conservative_roots::publish_peer_jit_coverage_for_stw();
        });
        drop(oracle_maps);
        // Arrive at barrier and wait for GC to complete. Census-aware (auto):
        // a genuine safepoint arrival is normally counted, but if this pause's
        // census excluded us as blocked (a finding-1(a) window), participating
        // would fill a counted mutator's quota slot.
        //
        // The map comes back as a shared `Arc` (gc-common 2026-09-23): one
        // allocation per pause, reference-counted to every waiter, instead of a
        // deep copy per thread taken under the barrier lock.
        let pointer_map = shared.mem.gc_barrier.arrive_and_wait_auto(thread.thread_id);

        // `remap_trace_push` returns at once when the trace is off, but its
        // `extra` argument was formatted unconditionally — a `String`
        // allocation (and a 16-shard `len()` walk) on every mutator at every
        // pause. Only build it when the trace will read it.
        if remap_trace_on() {
            remap_trace_push(
                shared,
                thread,
                if pointer_map.is_empty() {
                    "arrive-nomap"
                } else {
                    "arrive"
                },
                &format!("map={}", pointer_map.len()),
            );
        }
        // Apply pointer map to this thread's frames
        if !pointer_map.is_empty() {
            apply_pointer_map_to_thread(
                thread,
                &pointer_map,
                &shared.mem.heap,
                &shared.classes.type_maps,
            );
        } else {
            // Nothing moved: keep (re-tag) the frozen-frame root cache, exactly
            // as the initiator's `update_all_roots` does before its empty-map
            // early return (gc-common w2-g, 2026-09-23). Without this every
            // parked peer rebuilt its cache from scratch at its next deposit
            // after EVERY non-moving pause -- ZGC's sweep fallback, the
            // Generational non-moving young sweep, every concurrent-mark STW --
            // for nothing: the cache was built by the deposit just above, every
            // cached root was in the snapshot the collector marked from, and
            // no frame was rewritten, so the cache still mirrors the frames.
            // With an empty map `remap_rs_cache_after_gc` skips the walk and
            // only re-tags; it still clears the cache under
            // `CRATONVM_ROOTSNAP_CACHE(_SURVIVE_GC)=0`.
            remap_rs_cache_after_gc(thread, &pointer_map, &shared.mem.heap);
        }
        mtroots_selfcheck(thread, &shared.mem.heap, "safepoint-resume");
        // Interpreter round i2 wave 56, lane L6: this thread's frames are
        // current again; a native frames window may be read again.
        #[cfg(feature = "experimental-debug")]
        crate::runtime::interpreter::note_native_frames_current(shared, thread);
    }
    // (The T1.5.1 async-exception drain that ended this function, and
    // `check_pending_async_exception`, were removed in interpreter round i1
    // wave 4 with the rest of that channel: nothing posted to it and nothing
    // raised what it drained. See
    // docs/feature-designs/profile-guided-inlining.md.)
}

/// Apply a GC pointer map to a thread's frame locals and operand stacks.
///
/// `maps` is the VM's verifier type-map store (`shared.classes.type_maps`); it
/// only feeds the `CRATONVM_DBG_VERIFY_OOP_MAPS` shadow comparison of the frame
/// remap (`Frame::update_frame_refs_mapped`) and changes no rewrite.
/// `CRATONVM_GC_VERIFY_STALE`'s "zeroed header" test for a parked thread's
/// Object slot: class id, slots and array length all zero.
///
/// gcd d1/e: the mark is proven not FORWARDED first. The slot this tests is
/// suspected stale, so its bytes may read as a forwarded mark, and
/// `num_slots()` / `array_length()` would follow that mark through the second
/// word (the pin9 fault shape,
/// `docs/feature-designs/generative-gc-architecture.md`).
/// A forwarded header is not zeroed memory, so every other answer is the old
/// one.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zero_header_on_a_stale_ref(h: &cratonvm_types::ObjectHeader) -> bool {
    let mark = h.mark_word.load(std::sync::atomic::Ordering::Relaxed);
    !cratonvm_types::ObjectHeader::is_forwarded_mark(mark)
        && h.class_id.as_u32() == 0
        && h.num_slots() == 0
        && h.array_length() == 0
}

pub(crate) fn apply_pointer_map_to_thread(
    thread: &mut JvmThread,
    pointer_map: &cratonvm_types::PointerMap,
    heap: &crate::memory::VmHeap,
    maps: &cratonvm_classloading::TypeMapStore,
) {
    // Attribution for a later stale-reference report: this thread applied a
    // relocation map through the STOP-THE-WORLD RESUME path. See
    // `gc_quiescence::note_pointer_map_applied`.
    if !pointer_map.is_empty() {
        cratonvm_gc::gc_quiescence::note_pointer_map_applied(1);
    }
    // JNI local references (INT-2, safepoint-resume half): rewrite THIS
    // thread's `JNI_LOCAL_FRAMES` handles through the pointer map — a JNI
    // native that re-entered Java and parked at the safepoint poll must not
    // resume with dangling local jobjects after a moving collection. The
    // storage is thread-local and this function always runs on the resuming
    // thread, so this is the only place that can reach these handles.
    crate::native::jni::update_local_refs_after_gc(pointer_map);
    // BUG-03 trace (gated): record that the safepoint-peer remap ran for main.
    if thread.thread_id.0 == 0 && peer_bug03_dbg() {
        let jto = thread
            .java_thread_obj
            .map(|o| o.as_ptr() as usize)
            .unwrap_or(0);
        eprintln!(
            "[BUG03-fm] e{} path=peer(apply_pointer_map) tid0 jto=0x{:x} jto_in_map={} pm.len={}",
            heap.collection_count(),
            jto,
            jto != 0 && pointer_map.contains_key(&jto),
            pointer_map.len()
        );
    }
    // See `JvmThread::last_heal_collection`.
    thread.last_heal_collection = heap.collection_count();
    for frame in &mut thread.frames {
        frame.update_frame_refs_mapped(pointer_map, heap, maps);
        // Forward the synchronized-method monitor object too. A `synchronized`
        // method records the object it locked on entry in `monitor_on_exit` and
        // releases it on frame-pop. If a *cross-thread* moving GC relocated that
        // object while this thread was parked at the STW safepoint barrier
        // (`arrive_and_wait` in the safepoint-poll path), a stale
        // `monitor_on_exit` makes the implicit `monitorexit` target the old
        // address — surfacing as "thread does not own the monitor" (observed as
        // an intermittent IllegalMonitorStateException in the ES RestClient
        // `org/elasticsearch/client/Cancellable$RequestCancellable.
        // runIfNotCancelled`, whose `synchronized` body allocates heavily under
        // `-Xmx1g` GC pressure). The GC-initiator (`update_all_roots` in
        // memory/gc.rs) and the native-blocked-thread wake path
        // (`check_post_block_gc` in vm/vm_exec.rs) already forward it; this
        // non-initiator safepoint-resume path was the missing third site. Keep
        // it consistent with the relocated object, identically to those two.
        if let Some(ref mut obj_ref) = frame.monitor_on_exit {
            // Cast: object/code pointer to integer address
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                // SAFETY: new_addr was produced by pointer_map and points at the relocated, valid object header within the heap arena.
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
        // Block monitors (`Frame::held_monitors`, wave 23 lane L7), for the
        // same reason: `monitorexit` finds its entry by address.
        frame
            .held_monitors
            .remap_with(|a| pointer_map.get(&a).copied());
    }
    // DIAGNOSTIC-ONLY (cceres3): mirror of the wake-time WAKE-STALE verifier;
    // catches a frame slot left stale right after a safepoint-arrival remap.
    //
    // THE PREDICATE IS THE POINTER MAP, NOT THE FORWARDING WORD (2026-08-17).
    // `debug_forwarded_target` reads a forwarding word at the old address, and
    // ZGC's slide leaves none — `compact_low_to` zeroes what it vacated and the
    // memmove overwrites the rest — so on the DEFAULT collector this verifier
    // reported zero whatever the truth was, which is how it stayed silent while
    // the H2 MVStore-writer residual reproduced under it. `pointer_map` is the
    // authoritative record of this collection's moves and is right here in
    // hand.
    //
    // This is also the only EXACT place to ask the question. Every address-keyed
    // instrument outside the pause cannot tell an old reference to the moved
    // object from a new reference to whatever the allocator has since put at
    // that address; here, the remap has just run and no mutator on this thread
    // has resumed, so a frame slot holding a map KEY is unambiguously a slot the
    // remap did not reach.
    if peer_blockgc_dbg() {
        // A source address this slide also wrote a SURVIVOR to is not evidence:
        // survivors slide down into the space vacated objects left, so a slot
        // legitimately holding that survivor names an address that is also a
        // map key. Excluding destinations is what separates "the remap missed
        // this slot" from "this slot holds the object that moved INTO the
        // address" — the same distinction that made the first vacated-frames
        // instrument report eight findings a run that were all correct code.
        let destinations: rustc_hash::FxHashSet<usize> = pointer_map.values().copied().collect();
        for (fi, fr) in thread.frames.iter().enumerate() {
            for li in 0..fr.locals_len() {
                if let Value::Object(Some(o)) = fr.get_local(li as u16) {
                    let a = o.as_ptr() as usize;
                    if destinations.contains(&a) {
                        continue;
                    }
                    if let Some(new) = pointer_map.get(&a).copied() {
                        eprintln!(
                            "[blockgc] ARRIVE-STALE tid={} frame#{fi} {}.{} pc={} local[{li}] 0x{a:x}->0x{new:x} in_map={}",
                            thread.thread_id.0, fr.class_name(), fr.method_name(), fr.pc,
                            pointer_map.contains_key(&a),
                        );
                    }
                }
            }
            for si in 0..fr.stack.len() {
                if let Value::Object(Some(o)) = fr.stack.peek_at(si) {
                    let a = o.as_ptr() as usize;
                    if destinations.contains(&a) {
                        continue;
                    }
                    if let Some(new) = pointer_map.get(&a).copied() {
                        eprintln!(
                            "[blockgc] ARRIVE-STALE tid={} frame#{fi} {}.{} pc={} stack[{si}] 0x{a:x}->0x{new:x} in_map={}",
                            thread.thread_id.0, fr.class_name(), fr.method_name(), fr.pc,
                            pointer_map.contains_key(&a),
                        );
                    }
                }
            }
        }
    }
    // Step 5 GAP B (precise-JIT remap, non-initiator half). The GC initiator's
    // `update_all_roots` (memory/gc.rs:73) remaps this collection's precise JIT
    // oop-map slots via `remap_active_jit_frames`, but a thread that was PARKED at
    // the STW barrier reaches HERE instead and was missing that call. Under a
    // moving collector this stranded a non-initiator's JIT-frame oops at their old
    // addresses after a relocation — a use-after-free with CRATONVM_PRECISE_JIT_MAPS
    // on. It is specifically a G1 hazard: G1 young/mixed move unconditionally,
    // whereas the generational collector falls back to a non-moving sweep whenever
    // any thread is in JIT (`gc_quiescence`), so its non-initiator JIT frames never
    // see relocation. Mirror the initiator: remap THIS resuming thread's precise
    // JIT oop slots before the shadow stack. Thread-local (walks this thread's JIT
    // entry chain — sound because we run on the resuming thread itself) and inert
    // unless a precise-map frame is live, so it is a no-op on the default path.
    crate::jit::conservative_roots::remap_active_jit_frames(
        pointer_map,
        &crate::jit::conservative_roots::shadow_owned_slots(&thread.shadow_stack),
    );
    crate::jit::conservative_roots::remap_register_image_words(pointer_map, None);
    // §4 (multi-thread shadow scan, remap half). Remap THIS thread's shadow-stack
    // precise roots in place, so a worker resuming from the STW barrier sees the
    // relocated addresses in the JIT registers/slots it reloads from its shadow
    // stack. (The GC initiator's own shadow stack is remapped by `update_all_roots`
    // in `gc.rs`; a non-initiator reaches here instead.) Every shadow slot is a
    // known oop, so the rewrite is unconditionally safe. No-op when the gate is
    // off or the shadow stack is empty. `remap` (like `shadow_owned_slots`
    // above) bounds indirect entries by THIS thread's live stack, so an odd
    // primitive a compiled frame published in a reference home is skipped,
    // never written through (gc-common w5-g, `ShadowOddLongProbe`) -- which is
    // why this must stay on the resuming thread itself.
    if crate::jit::conservative_roots::shadow_stack_enabled() {
        thread.shadow_stack.remap(pointer_map);
    }
    // gcg g5/og: after the shadow-stack remap (see vm/src/memory/gc.rs).
    crate::jit::conservative_roots::report_stale_after_remap(pointer_map, None);
    // Every off-frame per-thread reference, through the ONE list the four
    // per-thread remap paths share (`memory::gc::remap_thread_off_frame_refs`):
    // printed values, native pins, handle slots, the native allocation pool,
    // the in-flight native return, the JIT HashMap node cache, the ASCII case
    // cache, scoped-value keys and values, and the three single-slot references
    // (`java_thread_obj`, `jit_pending_exception`, `uncaught_exception_pending`).
    //
    // This path used to carry its own copy of that list and it had drifted:
    // `jit_pending_exception` and `uncaught_exception_pending` were forwarded
    // by the initiator only, so a peer-initiated relocation left the JIT's
    // pending throwable (live across the unwind to the interpreter's drain,
    // which crosses safepoint polls) naming a vacated address.
    crate::memory::gc::remap_thread_off_frame_refs(thread, pointer_map);
    // The JIT deopt / exceptional stash, remap half (gc-common w2-g,
    // 2026-09-23). Paired with the scan half `update_root_snapshot` now runs
    // on every safepoint deposit, which is what `remap_stashed_deopt_objects`'
    // debug assertion checks. Thread-local, and this function always runs on
    // the thread whose stash it is. Before the pairing a stashed frame parked
    // at a safepoint was neither rooted nor forwarded by a peer's collection.
    cratonvm_jit::deopt::remap_stashed_deopt_objects(|addr| {
        pointer_map.get(&(addr as usize)).map(|&to| to as u64)
    });
    // This thread's deposited root snapshot, in lockstep with the frames it
    // describes. Without it the snapshot keeps the PRE-move addresses until the
    // next deposit, and the one reader that can run before that deposit -- the
    // cross-thread takeover freezing this thread in compiled code at the NEXT
    // pause (`collect_all_root_snapshots`, `root_snapshots_for_os_tids`) --
    // hands the collector addresses this cycle vacated. The initiator and
    // blocked threads already had their snapshots forwarded (`update_all_roots`
    // section 11, `fold_pointer_map_into_blocked`); a parked peer was the gap.
    crate::memory::gc::remap_root_snapshot(thread, pointer_map);
    // Lever #3 (bug 04): keep this thread's rootsnap frozen-frame cache valid
    // across the relocation we just applied, in lockstep with the frames above.
    remap_rs_cache_after_gc(thread, pointer_map, heap);

    // DIAG (CRATONVM_GC_VERIFY_STALE=1): after a NON-INITIATOR thread resumes
    // from the STW barrier and applies the pointer map, walk its own frames and
    // flag any Object slot whose header is ZEROED (class_id=0 && num_slots=0).
    // A zeroed header means the object was NOT copied by the collector (a missed
    // marking root — its ref was absent from this thread's deposited snapshot,
    // e.g. a lost operand-stack tag), then young-from was reset over it. This is
    // the per-PARKED-THREAD analogue of `verify_no_stale_refs` (which only
    // checks the GC initiator) — it localizes the missed-root that surfaces as
    // the teardown "all-zero header" corruption / reactor-thread leak.
    // Cache the gate so the OFF path (the default) is a single relaxed load, not
    // a per-parked-thread-per-GC environment lookup.
    fn gc_verify_stale_enabled() -> bool {
        use std::sync::OnceLock;
        static E: OnceLock<bool> = OnceLock::new();
        *E.get_or_init(|| {
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
                .ok()
                .as_deref()
                == Some("1")
        })
    }
    if gc_verify_stale_enabled() {
        use cratonvm_types::ObjectHeader;
        let tname = thread.thread_id.0;
        for (fi, frame) in thread.frames.iter().enumerate() {
            let cn = frame.class_name();
            let mn = frame.method_name();
            for li in 0..frame.locals_len() {
                // Cast: numeric/representation conversion
                if let crate::types::Value::Object(Some(o)) = frame.get_local(li as u16) {
                    // Cast: object/code pointer to integer address
                    let a = o.as_ptr() as usize;
                    if a != 0 {
                        // SAFETY: `a` is a non-null heap address from a live Object local; reading its ObjectHeader is valid for the lifetime of the borrow.
                        let h = unsafe { &*(a as *const ObjectHeader) };
                        if zero_header_on_a_stale_ref(h) {
                            eprintln!(
                                "POST-GC ZERO-HEADER PARKED tid={} frame[{}] {}.{} local[{}] pc={} addr=0x{:x}",
                                tname, fi, cn, mn, li, frame.pc, a
                            );
                        }
                    }
                }
            }
            let mut stk = Vec::new();
            frame.stack.scan_object_refs(&mut stk, heap);
            for o in stk {
                // Cast: object/code pointer to integer address
                let a = o.as_ptr() as usize;
                if a != 0 {
                    // SAFETY: `a` is a non-null heap address from a live Object stack slot; reading its ObjectHeader is valid for the lifetime of the borrow.
                    let h = unsafe { &*(a as *const ObjectHeader) };
                    if zero_header_on_a_stale_ref(h) {
                        eprintln!(
                            "POST-GC ZERO-HEADER PARKED-STACK tid={} frame[{}] {}.{} pc={} addr=0x{:x}",
                            tname, fi, cn, mn, frame.pc, a
                        );
                    }
                }
            }
        }
    }
}

/// Keep the `update_root_snapshot` frozen-frame cache (`rs_cache`) valid across
/// a GC instead of letting the next snapshot discard it on the
/// `collection_count` bump. The cache stores object ADDRESSES, which a
/// collection only invalidates by RELOCATING the object (the default non-moving
/// young sweep still relocates via selective promotion). So remap the cached
/// roots through the same `pointer_map` that relocated the frames, then tag the
/// cache with the post-collection count so `update_root_snapshot`'s gen gate
/// accepts it.
///
/// FAIL-SAFE: `rs_cache_gen` is advanced ONLY here. Any GC path that relocates
/// this thread's objects WITHOUT calling this leaves `rs_cache_gen` stale, so
/// the gen gate rebuilds the cache from scratch; a stale cached address is
/// never trusted. Default-on with opt-outs (`CRATONVM_ROOTSNAP_CACHE=0` or
/// `CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC=0`). Must be called at every site that
/// applies a `pointer_map` to a thread's frames (`apply_pointer_map_to_thread`
/// here, `update_all_roots` in memory/gc.rs); missing one only costs a rebuild,
/// never correctness. (Until 2026-07-31 this also cleared the cache outright in
/// the "real ForkJoinPool lane", keyed off a presence test on
/// `CRATONVM_REAL_FORKJOINPOOL` that stopped being set when that lane became
/// the default — see `env_cache.rs` for why that bypass was retired instead of
/// repointed.)
pub(crate) fn remap_rs_cache_after_gc(
    thread: &mut JvmThread,
    pointer_map: &cratonvm_types::PointerMap,
    heap: &crate::memory::VmHeap,
) {
    if !crate::runtime::env_cache::rootsnap_cache()
    {
        thread.rs_cache.clear();
        return;
    }
    // An empty map means nothing moved → cached addresses are already valid;
    // skip the walk but still re-tag the gen below so the cache is kept.
    if !pointer_map.is_empty() {
        for (_key, roots) in thread.rs_cache.iter_mut() {
            for r in roots.iter_mut() {
                // Cast: object/code pointer to integer address
                if let Some(&new_addr) = pointer_map.get(&(r.as_ptr() as usize)) {
                    // SAFETY: new_addr came from the GC's pointer_map and points
                    // at the relocated object's header within the heap arena —
                    // the same invariant the frame/snapshot remaps above rely on.
                    *r = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }
    }
    thread.rs_cache_gen = heap.collection_count();
}

/// Check if the old generation needs a concurrent GC cycle.
///
/// If the old gen is above its capacity threshold, this starts a concurrent
/// mark-sweep cycle using brief STW pauses for initial mark and remark,
/// with the marking phase running concurrently with application threads.
/// LANE W7-D — the ONE implementation of "advance G1's mark-cycle lifecycle",
/// with the door that asked.
///
/// # Why this is one function and was three
///
/// `maybe_concurrent_gc`'s G1 arm, `g1_drive_concurrent_mark_pub` and (in a
/// loop, with a deadline) `g1_force_full_cycle` each carried their own copy of
/// the finish-then-start pair. The copies had already drifted:
/// `maybe_concurrent_gc` tested `g1_should_start_marking() &&
/// !g1_is_marking_active()` — a redundant second read of the flag it had
/// already branched on — while `g1_drive_concurrent_mark_pub` did not. Nothing
/// depended on the difference, which is exactly the shape that eventually
/// does.
///
/// More to the point: three copies is three places a FOURTH caller has to be
/// discovered, and the defect this lane closed was a missing caller, not a
/// wrong one. With one implementation, a new pause path needs one line, and
/// the census below cannot disagree with what the code did because it is
/// written from the same branch.
///
/// # The outcome taxonomy is the diagnosis
///
/// Four terminal states, and `waiting` is the one no counter in the tree could
/// report before:
///
/// * `Idle` — no cycle, and the IHOP policy declined to open one.
/// * `Started` — no cycle, and this visit opened one.
/// * `Waiting` — a cycle IS open and the background marker has not drained.
/// * `Finished` / `LostStw` — a cycle is open, the marker has drained, and
///   the STW remark+cleanup either ran or was beaten to the barrier.
///
/// `remark_pauses=1, cleanup_pauses=0` on a default build is compatible with
/// `waiting` in the millions (a driver asking constantly and always refused)
/// and with `visits=0` (no driver on this pause path at all). They are
/// different defects. See
/// `docs/feature-designs/concurrent-gc-maturation.md` §2.
///
/// Not a GC pause path in its own right: the caller has already completed its
/// collection and reopened the world, so the two `g1_*` predicate loads and at
/// most one census `fetch_add` are charged per PAUSE, never per allocation.
pub(in crate::runtime::interpreter) fn g1_drive_mark_cycle(shared: &SharedVm, thread: &mut JvmThread, door: MarkDoor) {
    // Finish first, start second: if an active cycle's background marker
    // has drained to a fixed point, run the STW final remark + cleanup
    // NOW, on this thread (it has the STW-barrier context). The remark
    // is NOT optional — the SATB log and a fresh root scan must reach
    // the bitmap before cleanup acts on it (see
    // `g1_final_remark_and_cleanup`); the old completion path (a watcher
    // thread calling straight into cleanup) discarded both.
    if shared.mem.heap.g1_is_marking_active() {
        // JIT round 23 wave 8, lane L9 (`CRATONVM_G1_FUTILE_YOUNG_ESCALATION`,
        // OPT-IN `=1` since the n23-w8g A/B, where it was neutral): a cycle whose marker has NOT drained is
        // finished now anyway when the last young pauses have all left the
        // Free pool starved (`G1Collector::take_futile_young_escalation`: 8 in
        // a row under 3% of the heap's regions). `g1_final_remark_cleanup`
        // joins the marker and drains the gray set to a fixed point inside the
        // pause -- the force-full door's step when its quiescence wait runs
        // out -- so the cleanup is as complete as a quiesced one, only
        // earlier. HotSpot answers the same heap with an evacuation failure
        // and a Full collection; `KeepLong -Xmx1g` took ~100 2 MB young pauses
        // per round here waiting for a 4.2 s mark instead.
        let finished = shared.mem.heap.g1_concurrent_mark_finished();
        if !finished && shared.mem.heap.g1_take_starved_full() {
            // r23 w6 (`CRATONVM_G1_STARVED_FULL`, default ON): two young
            // pauses in a row under 10% Free with this cycle open (doubled per
            // unproductive starved full) get HotSpot's answer -- abort the
            // cycle and compact (`g1_starved_full`).
            // The young pause that brought us here reports first.
            super::gc_events::g1_emit_deferred_young_report(shared, thread.thread_id, false);
            let ran = g1_starved_full(shared, thread);
            note_mark_door(
                door,
                if ran {
                    MarkDoorOutcome::Finished
                } else {
                    MarkDoorOutcome::LostStw
                },
            );
            return;
        }
        if finished || shared.mem.heap.g1_take_futile_young_escalation() {
            let ran = g1_final_remark_cleanup(shared, thread);
            note_mark_door(
                door,
                if ran {
                    MarkDoorOutcome::Finished
                } else {
                    MarkDoorOutcome::LostStw
                },
            );
        } else {
            note_mark_door(door, MarkDoorOutcome::Waiting);        }
        // gci i27/ob: a cycle was already open, so this requester's young
        // pause opened none: its parked report is `Pause Young (Normal)`.
        super::gc_events::g1_emit_deferred_young_report(shared, thread.thread_id, false);
        return;
    }
    // gci cs48 (`gci-i19ob-g1-concurrent-start-label-is-a-prediction-FIXED-20261017`):
    // after a `Mixed` pause no cycle opens; the next young-only pause's drive
    // opens it, and that pause is the `Concurrent Start` (HotSpot 25's
    // `decide_on_concurrent_start_pause`). Asked first, so a declined drive
    // reaches no IHOP census either.
    if !g1_start_waits_for_young_only_pause(shared) && shared.mem.heap.g1_should_start_marking() {
        g1_concurrent_mark_cycle(shared, thread);
        // Asked-and-got, not asked. `g1_concurrent_mark_cycle` returns
        // without activating when its initial-mark STW loses the race, and a
        // census that recorded the ATTEMPT would report cycles that never
        // opened — the same "the instrument cannot see the failure" shape
        // README §5 rule 8 is about. One extra relaxed load, once per pause.
        note_mark_door(
            door,
            if shared.mem.heap.g1_is_marking_active() {
                MarkDoorOutcome::Started            } else {
                MarkDoorOutcome::LostStw            },
        );
    } else {
        note_mark_door(door, MarkDoorOutcome::Idle);    }
    // gci i27/ob: a cycle this drive opened already printed the requester's
    // parked young report as `Concurrent Start` (`g1_concurrent_mark_cycle`,
    // before its `Concurrent Mark Cycle` line); one still parked means none
    // opened (IHOP declined, or the initial mark lost its STW race): `Normal`.
    super::gc_events::g1_emit_deferred_young_report(shared, thread.thread_id, false);
}

/// gci cs48 (`gci-i19ob-g1-concurrent-start-label-is-a-prediction-FIXED-20261017`):
/// did the last G1 collection pause run as `Mixed`? Then no mark-cycle drive
/// ([`g1_drive_mark_cycle`], the JIT mark driver's gate) opens a cycle until
/// the next young-only pause: HotSpot 25's `G1Policy` only records the request
/// at the pause that ends a mixed phase (`set_initiate_conc_mark_if_possible`)
/// and honours it at the next young-only pause, which it labels
/// `Pause Young (Concurrent Start)`. The flag is written inside every G1
/// collection pause (`gc_events::gc_event_seal`; never set under
/// `CRATONVM_G1_CONCURRENT_START_AT_YOUNG_PAUSE=0`). One relaxed load.
pub fn g1_start_waits_for_young_only_pause(shared: &SharedVm) -> bool {
    shared
        .mem
        .alloc_ladder
        .g1_conc_start_waits_for_young_pause
        .load(std::sync::atomic::Ordering::Relaxed)
}

pub(in crate::runtime::interpreter) fn maybe_concurrent_gc(shared: &SharedVm, thread: &mut JvmThread) {
    maybe_concurrent_gc_at(shared, thread, MarkDoor::MaybeGc)
}

/// [`maybe_concurrent_gc`] with the door's identity, for the census.
///
/// `System.gc()` reaches this function through `force_gc_from_native`, and it
/// is the one flag-free route `w6m-a-workload-that-mixes.md` §4 measured
/// COMPLETING cycles on a default build (6 cycles, 9 mixed pauses). A census
/// in which it is indistinguishable from `maybe_gc`'s epilogue cannot be used
/// to check that account, which is the whole reason the door parameter exists.
pub(in crate::runtime::interpreter) fn maybe_concurrent_gc_at(shared: &SharedVm, thread: &mut JvmThread, door: MarkDoor) {
    // G1 backend: trigger concurrent marking when IHOP threshold crossed
    if shared.mem.heap.is_g1() {
        g1_drive_mark_cycle(shared, thread, door);
        return;
    }

    // gen r4w2/concmark (2026-09-23): a cycle is already open (another
    // driver's, sliced and between two of its slices). The CAS below is the
    // authority; this read only saves building a marker — whose bitmap is
    // 1/64th of the old generation — that could not open anyway.
    if shared.mem.concurrent_gc_state.phase() != cratonvm_gc::ConcurrentGcPhase::Idle {
        return;
    }

    // Only proceed if old gen needs collection and we have a concurrent marker
    //
    // gen r4w4/concmark4 (2026-09-24): the concurrent cycle's OWN trigger,
    // `GenerationalHeap::concurrent_cycle_due` — the adaptive initiating
    // occupancy below the STW floor under the default concurrent-first
    // policy, and exactly the old `old_gen_needs_gc()` under
    // `CRATONVM_GC_NO_CONCURRENT_FIRST`. G1 returned above; ZGC never answered
    // yes here (`old_gen_needs_gc` is a hard `false` off Generational).
    //
    // gen r5w3/unload7 — the per-door start census
    // (`gengc-r5w2-conc6-proposal-generational-start-census-by-door`): which
    // door asked, which found the cycle due, which opened it (below, at the
    // initial mark). Generational only; two relaxed adds per collection that
    // asks.
    let census_door = gen_conc_door(shared, door);
    let due = match &shared.mem.heap {
        _ if shared.mem.heap.is_generational() => {
            let h = &shared.mem.heap;
            let state = &shared.mem.concurrent_gc_state;
            state.note_door(
                census_door,
                cratonvm_gc::concurrent_mark::GenConcDoorStep::Asked,
            );
            let due = h.concurrent_cycle_due();
            if due {
                state.note_door(
                    census_door,
                    cratonvm_gc::concurrent_mark::GenConcDoorStep::Due,
                );
            }
            due
        }
        #[allow(unreachable_patterns)]
        _ => false,
    };
    if !due {
        return;
    }

    let (old_gen_base, old_gen_size) = shared.mem.heap.old_gen_info();
    // gcj j2/og2 (C2): the marker's BITMAP spans the generation's growth
    // limit, so a growth between this read and the initial-mark pause no
    // longer makes the remark refuse the traced cycle
    // (`GenerationalHeap::old_gen_mark_span`). Only the bitmap: the
    // reference-skip screen and the class-unload capture below keep the
    // capacity span, since they read headers in it.
    let (_, old_gen_mark_span) = shared.mem.heap.old_gen_mark_span();

    // Create a temporary concurrent marker for this cycle.
    //
    // fork6 GC_STRESS fix — build it on the heap's SHARED SATB queue + phase
    // state (attached via `enable_concurrent_gc` at SharedVm construction).
    // The previous `ConcurrentMarker::new` created a private queue + state per
    // cycle while the heap's `satb_barrier` gated on the HEAP's (formerly
    // never-attached) instances: the write barrier was a hard no-op, nothing
    // ever reached this cycle's remark, and the concurrent mark effectively
    // ran against live mutators with no write barrier — the sweep then freed
    // old objects whose only reference moved during the concurrent phase.
    let marker = cratonvm_gc::ConcurrentMarker::with_shared(
        old_gen_base,
        old_gen_mark_span,
        shared.mem.concurrent_satb.clone(),
        shared.mem.concurrent_gc_state.clone(),
    );

    // gen r4w2/concmark (2026-09-23) — own the cycle before touching the
    // shared SATB queue or phase. Two drivers used to be able to overlap on
    // them (the second opened between the first one's Phase 2 and its remark),
    // and whichever remarked first deactivated the queue under the other's
    // mark; see `ConcurrentGcState::try_open_cycle`. Every `return` below
    // drops `cycle`, which abandons the cycle (phase back to Idle, barrier
    // off); only the post-sweep `cycle.complete()` releases it without that.
    let Some(cycle) = marker.try_open_cycle() else {
        return;
    };

    // Phase 1: Initial Mark — brief STW pause.
    //
    // INT-3 residual fix: open-coded (request → takeover-wait → work →
    // complete) instead of a request plus a plain `wait_for_all` (the deleted
    // `brief_stw*` helpers), which stalls forever on a peer spinning in
    // compiled code — and whose root set covered such a peer only by its
    // STALE deposit snapshot. Mark-only pause: the frozen peers' fresh
    // conservative roots are extra MARK roots; nothing moves, so no
    // pin/pointer-map concerns. The request passes the identity census
    // (gc-common w4-a, `request_non_collection_pause`).
    let Some(counted_os_tids) = request_non_collection_pause(shared, thread.thread_id) else {
        return; // Another STW was in progress
    };
    // gen r5w1/refs5 — remark-time reference processing for this cycle, BOTH
    // halves or neither (`gengc-r4w5-concmark5-remark-reference-processing-design`):
    // the referent-slot skip set published in this initial-mark pause, and the
    // processing at remark (`gen_concurrent_remark_pause`). Decided ONCE per
    // cycle, here, so the two halves cannot disagree. `Some` is the skip set's
    // candidate list, which the remark's safety net re-reads.
    let refproc_on = gen_conc_remark_refproc_enabled();
    // gen r5w3/unload7 — concurrent class unloading, decided once per cycle
    // like the reference processing it runs inside (and only with it): the
    // initial mark and the remark take their roots under the class-unload
    // licence, the marker follows the side tables, and the remark unloads.
    // `false` (the default) is every line below byte for byte as before.
    let class_unload_on = refproc_on && gen_conc_class_unload_enabled();
    // gen r5w6/conc10 — `CRATONVM_GEN_Y2O_LIVE_SEED` (opt-in): both pauses of
    // this cycle take their young→old seeds from the LIVE young set, and, when
    // the cycle processes references at its remark, not through the referent
    // slot of a young `Reference` (`gen_conc_y2o_seeds`). Decided once per
    // cycle, like the two above. `false` is every line below as before.
    let y2o_live_seed = cratonvm_gc::gen_heap::y2o_live_seed_enabled();
    let mut hidden_refs: Option<Vec<usize>> = None;
    let mut initial_marked = false;
    {
        // Forcibly stop + conservatively scan in-JIT peers, then wait for
        // the cooperative mutators (byte-identical to wait_for_all() when
        // no thread is in JIT).
        let mut xt_roots: Vec<ObjectRef> = Vec::new();
        // gen r5w5/conc9: armed BEFORE the take-over, so an unwind anywhere in
        // this pause resumes the frozen peers and reopens the world (see
        // `GenConcPauseGuard`); the normal path below releases it.
        let mut pause_guard = GenConcPauseGuard::arm(shared);
        pause_guard.hold(stw_take_over_and_wait(
            shared,
            &mut xt_roots,
            &counted_os_tids,
        ));
        // gc-common w5-a: `--verbose:gc` pause line (`door=gen-initial-mark`).
        let mut pause_timer =
            non_collection_pause_start(shared, NonCollectionPause::GenInitialMark);
        // gcj j3/x2 (og2's growth span, C2 follow-up): with the bitmap over the
        // growth limit, a growth between the pre-pause read above and this
        // pause is no longer refused at remark, so the class-unload capture
        // and the reference-skip screen below must see the generation THIS
        // pause marks: a loader promoted into the grown span had no `loaders`
        // row in a capture clipped at the stale capacity, no instance's class
        // edge reached it, and the authorised sweep freed it live. The world
        // is stopped, so the capacity read here is the initial mark's (both
        // users only filter by range or screen allocated objects inside it).
        // Under `CRATONVM_GC_CONC_BITMAP_GROWTH_SPAN=0` (span == capacity)
        // the pre-pause pair, as before: the remark refuses such a cycle.
        let (old_gen_base, old_gen_size) = if old_gen_mark_span > old_gen_size {
            shared.mem.heap.old_gen_info()
        } else {
            (old_gen_base, old_gen_size)
        };
        // Collect root pointers for old-gen marking. gen r5w3/unload7: under
        // `with_class_unload_marking` when this cycle unloads, which lifts the
        // concurrent-mark veto on side-table deferrals for THIS scan only
        // (`roots::generational_metadata_is_conditional`).
        let roots = if class_unload_on {
            cratonvm_gc::gc_quiescence::with_class_unload_marking(|| {
                cratonvm_gc::gen_heap::with_no_young_sweep_following(|| collect_roots(shared, thread))
            })
        } else {
            cratonvm_gc::gen_heap::with_no_young_sweep_following(|| collect_roots(shared, thread))
        };
        // gci i13/cm item 6: no young sweep follows a concurrent pause; drop the
        // precise-root table this scan published so no later sweep on this
        // thread can take it (`take_sealed_precise_root_values`).
        cratonvm_gc::gen_heap::clear_precise_root_values();
        let mut snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
        // gch h4/bp: as in `run_collection_pause`.
        append_in_native_band_roots(shared, &mut snapshot_roots);
        let mut root_ptrs: Vec<*mut u8> = roots
            .iter()
            .chain(snapshot_roots.iter())
            // INT-3 — frozen in-JIT peers' conservative register/stack roots.
            .chain(xt_roots.iter())
            .map(|r| r.as_ptr())
            .collect();
        // JIT r23 w1b L7 (R23W1L7-4; `CRATONVM_DBG=root-kind-census`, off by default).
        root_kind_census_for_parts(
            shared,
            "gen-initial-mark",
            [&roots[..], &snapshot_roots[..], &xt_roots[..]],
        );
        // gen r4w4/concmark4 — the FROZEN peers' objects again, separately:
        // `initial_mark_with_frozen` scans each old-gen one inside this pause,
        // which closes the JIT SATB gate takeover window
        // (`gengc-r4w2-concmark-jit-gate-takeover-window`; the argument is on
        // `ConcurrentMarker::initial_mark_with_frozen`). Empty unless the
        // takeover froze someone.
        let frozen_ptrs: Vec<*mut u8> = xt_roots.iter().map(|r| r.as_ptr()).collect();
        // fork6 GC_STRESS fix — young→old references are mandatory
        // old-marking roots. `initial_mark` filters this list with
        // `old_gen.contains`, so an old object whose only path from a
        // root goes THROUGH a young object (root → young holder → old
        // target) was invisible and the sweep freed it live. Selective
        // promotion mass-produces exactly that shape (it tenures a
        // pinned young holder's children), which is why the Fork6Hard
        // GC_STRESS lane corrupted even single-threaded during clinit.
        // Safe here: brief STW, mutators quiesced, TLABs retired.
        //
        // gen r5w6/conc10: under `CRATONVM_GEN_Y2O_LIVE_SEED`, from the live
        // young set (the young collector's own, for THIS pause's roots) and
        // with the referent slot of every active young `Reference` hidden when
        // this cycle processes references at remark — `refproc_on`, the same
        // one-commit rule as the old-gen skip set below. Any doubt falls back
        // to the all-young enumeration inside the call.
        let y2o = if y2o_live_seed {
            if class_unload_on {
                cratonvm_gc::gc_quiescence::with_class_unload_marking(|| {
                    gen_conc_y2o_seeds(shared, &root_ptrs, refproc_on)
                })
            } else {
                gen_conc_y2o_seeds(shared, &root_ptrs, refproc_on)
            }
        } else {
            None
        };
        match &y2o {
            Some(seeds) => root_ptrs.extend(seeds.old_refs.iter().map(|&a| a as *mut u8)),
            None => root_ptrs.extend(
                shared
                    .mem
                    .heap
                    .collect_young_to_old_roots()
                    .into_iter()
                    .map(|a| a as *mut u8),
            ),
        }
        // gen r5w5/conc9: where the young→old roots end (the young objects'
        // loaders follow), for the `CRATONVM_DBG_MIRRORPIN_WHY` report below.
        let young_to_old_end = root_ptrs.len();
        // gen r5w1/refs5 — half 1: hide the referent slot of every active
        // old-gen Weak/Soft/Phantom `Reference`, BEFORE `initial_mark_with_frozen`
        // scans anything. Screened (shape, identity) with no old-gen guard held;
        // `set_reference_skip` itself touches only the marker.
        if refproc_on {
            let candidates = gen_conc_reference_skip_candidates(shared, old_gen_base, old_gen_size);
            marker.set_reference_skip(&candidates);
            hidden_refs = Some(candidates);
            // gen r5w5/conc9: the remark's dead-finalizer retention records
            // what it marks, so the reference pass clears soft and weak
            // references to those objects before `finalize()` can resurrect
            // them (`gen_remark_process_references`).
            marker.record_finalizer_retention_closure();
        }
        // gen r5w3/unload7 — the side tables AS THIS PAUSE'S ROOT SCAN LEFT
        // THEM (the scan rebuilt this VM's `metadata_pin` rows), and the young
        // objects' loaders as roots, both before the marker scans anything.
        // See `cratonvm_gc::concurrent_mark::ClassUnloadTables`.
        // gcg g1/c (unload7 item 4): one young class-id walk per initial-mark
        // pause, shared by the young instances' loaders and the layout census
        // (nothing allocates in young between the two: the world is stopped).
        let mut young_ids: Option<(rustc_hash::FxHashSet<u32>, bool)> = None;
        if class_unload_on {
            let tables = cratonvm_gc::concurrent_mark::ClassUnloadTables::capture(
                old_gen_base,
                old_gen_size,
            );
            // gen r5w6/conc10: the loaders of the LIVE young objects' classes
            // when the seeds came from the live set (a dead young instance
            // keeps no loader), else every young object's, as before.
            match &y2o {
                Some(seeds) => root_ptrs.extend(gen_conc_y2o_instance_loaders(seeds, &tables)),
                None => root_ptrs.extend(gen_conc_young_instance_loaders(
                    shared,
                    &tables,
                    &mut young_ids,
                )),
            }
            if gen_conc_unload_why_enabled() {
                gen_conc_young_instances_why(shared, &tables, y2o.as_ref(), "initial-mark");
            }
            marker.set_class_unload_tables(tables);
        }
        // gen r5w4/conc8 — the retained-layout census: layouts an earlier
        // remark kept for its sweep are released once a complete sweep finds no
        // instance of their class (`ConcurrentGcState::note_layouts_retained`).
        // Its young half runs here, where the world is stopped. Nothing (not
        // even the young walk) unless a layout is pending, which needs
        // `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`.
        if shared.mem.concurrent_gc_state.has_retained_layouts() {
            marker.set_layout_census(gen_conc_layout_census_candidates(shared, &mut young_ids));
        }
        if let Some(guard) = shared.mem.heap.old_gen_lock() {
            marker.initial_mark_with_frozen(&root_ptrs, &frozen_ptrs, &*guard);
            initial_marked = true;
            // gen r5w3/unload7: the door that opened this cycle.
            shared.mem.concurrent_gc_state.note_door(
                census_door,
                cratonvm_gc::concurrent_mark::GenConcDoorStep::Opened,
            );
            // gen r4w3/oldgen3: the measurement
            // `docs/feature-designs/jit-ir-optimizations.md` asks
            // for first — how often the pause that ARMS the JIT SATB gate has
            // frozen a thread mid-JIT (possibly between a gate test that read
            // "clear" and its store). Per-VM census on the shared phase state.
            shared
                .mem
                .concurrent_gc_state
                .note_initial_mark_takeover(pause_guard.frozen_count());
        }
        // gen r5w5/conc9 — which root source names each user loader and its
        // mirrors at the initial mark (diagnostic; see `gen_conc_unload_why`).
        if class_unload_on && initial_marked && gen_conc_unload_why_enabled() {
            let a = roots.len();
            let b = a + snapshot_roots.len();
            let c = b + xt_roots.len();
            gen_conc_unload_why(
                shared,
                &marker,
                "initial-mark",
                &[
                    ("collect_roots", &root_ptrs[..a]),
                    ("thread_snapshots", &root_ptrs[a..b]),
                    ("frozen_peers", &root_ptrs[b..c]),
                    ("young_to_old", &root_ptrs[c..young_to_old_end]),
                    ("young_instance_loaders", &root_ptrs[young_to_old_end..]),
                ],
            );
        }
        // Clear TLAB skip regions + resume frozen peers BEFORE reopening
        // the world (same race rationale as maybe_gc's epilogue).
        non_collection_pause_seal(&mut pause_timer);
        let taken = pause_guard.release();
        retire_skip_spans_and_resume(shared, taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());
        non_collection_pause_finish(pause_timer);
    }
    // gen r4w2/concmark: unreachable on Generational (`old_gen_lock()` is
    // always `Some` there), but a cycle whose initial mark did not run has no
    // snapshot to finish; say so structurally instead of remarking nothing.
    if !initial_marked {
        return;
    }

    // gen r4w2/concmark (2026-09-23) — objects per Phase-2 slice; `0` is the
    // pre-2026-09-23 driver (see `cratonvm_gc::concurrent_mark::gen_conc_mark_slice`).
    let slice = cratonvm_gc::concurrent_mark::gen_conc_mark_slice();

    // Phase 2: Concurrent Mark — runs while app threads continue
    if slice == 0 {
        // Legacy arm: the whole transitive closure under ONE old-gen lock
        // hold, with no safepoint poll. Every promotion waits for it, and so
        // does every other thread's pause (this thread is a counted mutator
        // that does not arrive), which is why the remark below then usually
        // loses its race. Kept as the A/B arm.
        if let Some(guard) = shared.mem.heap.old_gen_lock() {
            marker.concurrent_mark(&*guard);
        }
    }

    // gen r4w2/concmark — Phase 2 (sliced) and Phase 3, retried together.
    //
    // A slice holds the old-gen lock for at most `slice` objects, and the
    // thread polls a safepoint between slices, so a promotion waits for one
    // slice and another thread's pause gets this thread's arrival within one
    // slice, instead of both waiting for the whole closure
    // (`gengc-mark-concurrent-mark-holds-the-old-gen-lock-FIXED-20260923`). Between
    // slices another thread's young GC may promote into the old generation
    // (the next slice's object-start set covers that: `free_list_seq` moved)
    // or reclaim old-gen storage (the epoch moved; the slice reports the
    // closure done and the stale check below abandons the cycle without a
    // remark pause).
    //
    // A remark that loses its STW race is no longer the end of the cycle:
    // join the pause that won (it is waiting for this thread), trace whatever
    // it left, and ask again, up to `REMARK_ATTEMPTS` times. Nothing a
    // young pause does can make the eventual remark unsound — its snapshot,
    // SATB log, epoch and eligibility checks are exactly the ones a remark
    // that won the first time relies on.
    const REMARK_ATTEMPTS: u32 = 4;
    let mut remark_attempts = 0u32;
    let remark_outcome = loop {
        if slice > 0 {
            loop {
                let done = match shared.mem.heap.old_gen_lock() {
                    Some(guard) => marker.concurrent_mark_budget(&*guard, slice).1,
                    None => true,
                };
                cratonvm_gc::concurrent_mark::CONC_MARK_SLICES
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if done {
                    break;
                }
                // gcd d1/c — the service thread gives its cycle up at the next
                // Phase-2 slice once VM teardown has stopped it, so the
                // teardown's bounded wait for it to detach is short
                // (`gengc-r5w5-conc9-the-service-thread-can-outlive-vm-teardown`).
                // Dropping `cycle` abandons it, as the stale check below does.
                // Inert unless a service is attached AND stopped: one atomic
                // load per slice otherwise.
                if gen_conc_service_stopped_here(shared) {
                    tracing::debug!(
                        target: "cratonvm::gc::concurrent",
                        "concurrent cycle abandoned in Phase 2: the service thread was stopped"
                    );
                    return;
                }
                safepoint_check(shared, thread);
            }
            let stale = shared
                .mem
                .heap
                .old_gen_lock()
                .is_some_and(|guard| marker.cycle_is_stale(&*guard));
            if stale {
                // Another collector freed or slid old-gen storage since the
                // initial mark: the remark would refuse the sweep anyway.
                cratonvm_gc::concurrent_mark::CONC_CYCLE_STALE_ABANDONS
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        }
        match gen_concurrent_remark_pause(
            shared,
            thread,
            &marker,
            hidden_refs.as_deref(),
            class_unload_on,
            y2o_live_seed,
        ) {
            Some(remarked) => break Some(remarked),
            None => {
                // Lost the race. The legacy arm gives up here, as it always
                // did; the sliced arm joins the winning pause and retries.
                remark_attempts += 1;
                if slice == 0 || remark_attempts >= REMARK_ATTEMPTS {
                    cratonvm_gc::concurrent_mark::CONC_REMARK_ABANDONS
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    break None;
                }
                cratonvm_gc::concurrent_mark::CONC_REMARK_RETRIES
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                safepoint_check(shared, thread);
            }
        }
    };

    // fork6 GC_STRESS fix — the remark STW is NOT optional. If another
    // thread's STW won the race (the request returned false — a
    // near-certainty under allocation storms, where a young-GC request is
    // always pending), the closure never ran: the SATB queue is undrained
    // and the roots were never rescanned, so the mark bitmap is NOT final.
    // The old code fell through to the sweep anyway and freed live objects.
    // Abort the cycle instead (deactivate the barrier, discard the bitmap);
    // the next `old_gen_needs_gc` trigger starts over.
    //
    // gen r4w2/concmark: `None` is every attempt lost, `Some(false)` a pause
    // whose remark did not run (see `gen_concurrent_remark_pause`). "Abort" is
    // now dropping `cycle`, which runs `abort_cycle` and releases the shared
    // phase for the next driver.
    //
    // gen r5w3/unload7 — the `Concurrent Sweep` `jdk.GCPhaseConcurrent` row
    // (obs6's cross-lane request): the cycle's JFR id, which the remark pause
    // left for this, taken NOW — while the phase is still `ConcurrentSweep`,
    // so no other cycle can have opened and replaced it — with the sweep's
    // start. Taken on the abort path too, so an abandoned cycle's id cannot
    // outlive it. `None` (and one uncontended lock) when no recording runs.
    let sweep_jfr = super::gc_events::gen_concurrent_sweep_jfr_open(shared);
    if remark_outcome != Some(true) {
        cycle.abort();
        return;
    }

    // Phase 4: Concurrent Sweep
    //
    // gen r4w3/oldgen3 (2026-09-23): in slices, like Phase 2
    // (`gengc-r4w2-concmark-concurrent-sweep-holds-the-old-gen-lock-FIXED-20260923.md`).
    // A slice walks at most `slice` objects under the old-gen lock, and the
    // thread polls a safepoint between slices, so a promotion or another
    // thread's pause waits for one slice instead of the whole sweep. What must
    // hold between slices (the epoch, the resume offset, TAMS) is argued at
    // `ConcurrentMarker::concurrent_sweep_budget`; a foreign free or compaction
    // in between stops the sweep there, reclaiming nothing further. `slice == 0`
    // is the pre-2026-09-23 single hold.
    //
    // THE ADDRESS-KEYED SIDE TABLES (gc-common w3-g; `common-b-address-keyed-
    // sweeps-miss-concurrent-reclamation`). The minted smuggled-`long` registry
    // and the Throwable backtrace table are swept by `update_all_roots`, which
    // this cycle never reaches. `OldGen::free` does not zero, so a freed block
    // still parses as its dead object, and the first object placed on it --
    // a young pause PROMOTING into the free list is the common way -- would
    // adopt the dead row: `update_all_roots`' own sweep after that pause sees
    // a live object at the address and keeps it. So the tables are swept here,
    // with the verdict `survived_in_place` (a free-list hole is dead), always
    // AFTER the old-gen guard is dropped (the predicate takes the same,
    // non-reentrant lock) and BEFORE anything can reuse what was freed:
    // * once when the sweep ends, and
    // * between slices, before a `safepoint_check` that is about to join a
    //   pending pause -- that pause may promote into the blocks the slices so
    //   far have freed. A pause requested after the check cannot complete
    //   without this (counted) thread's arrival, which is at the NEXT
    //   between-slice check, so this covers every promotion. Sweeping only
    //   then keeps the table walks to one per interleaved pause, not one per
    //   slice.
    // * The DIRECT old-gen allocation route (a humongous array placed on a
    //   block a slice freed, between the guard drop and a table sweep) needs
    //   no pause, so neither sweep above covers it, and after the allocation
    //   `survived_in_place` sees a live object and keeps the dead row. Closed
    //   in gc-common w5-g (`common-w3g-concurrent-sweep-direct-old-alloc-
    //   window`): each slice reports the spans it freed
    //   (`concurrent_sweep_budget_into`), and the rows keyed inside them are
    //   dropped by a pure range test UNDER the same guard, before anything can
    //   be allocated there (`addr_keyed::drop_address_keyed_rows_in`, whose
    //   doc argues the lock order). The `survived_in_place` sweeps stay as the
    //   cycle-level backstop.
    let mut swept = 0usize;
    let mut unswept_frees = false;
    let mut freed_spans: Vec<(usize, usize)> = Vec::new();
    // gen r4w4/concmark4: for the end-of-cycle log line below.
    let completed_before = shared.mem.concurrent_gc_state.census().cycles_completed;
    // gcd d1/c — THE REFERENCE PROCESSOR'S ROWS
    // (`gengc-r5w5-conc9-concurrent-sweep-leaves-reference-rows-of-freed-references`).
    // The address-keyed tables above do not include it: on a cycle that did
    // not process references at its remark (the default) nothing pruned the
    // row of a dead old `Reference` this sweep frees, and a block re-issued
    // before the next collection (a promotion, a direct old-gen allocation)
    // could adopt the row's verdict. Each slice's freed spans now drop the
    // soft / weak / phantom rows keyed inside them
    // (`gen_conc_sweep_drop_reference_rows`), by a pure range test, AFTER the
    // old-gen guard is dropped (the processor is never taken under it; see
    // `gen_concurrent_remark_pause`) and before the between-slice
    // `safepoint_check`, i.e. before any pause can promote onto those blocks.
    // A direct old-gen allocation can land on one in between; its object is
    // new, so its row (if it is a `Reference`) carries a registration number
    // at or above this snapshot and is kept. Taken here, after the remark:
    // everything registered from now on belongs to an object that is marked
    // or not sweep-eligible, i.e. never to one this sweep frees.
    // gcg g2/r (`gcd-d3n-proposal-sweep-reference-row-prune-at-pause-boundaries`):
    // opt-in, hold the slices' freed spans in the processor and prune them in
    // one walk per pause boundary / cap / sweep end instead of one per slice.
    // A pause the sweep does not see coming flushes them itself
    // (`ReferenceProcessor::flush_deferred_sweep`).
    let refrow_batch =
        false;
    let (rows_registered_before, (batch_walks_before, batch_rows_before)) = {
        let rp = shared.mem.ref_processor.lock();
        (rp.registration_seq(), rp.deferred_sweep_totals())
    };
    let mut reference_rows_dropped = 0usize;
    let mut reference_row_prunes = 0u64;
    if slice == 0 {
        if let Some(mut guard) = shared.mem.heap.old_gen_lock() {
            loop {
                let (n, done) = marker.concurrent_sweep_budget_into(
                    &mut *guard,
                    usize::MAX,
                    Some(&mut freed_spans),
                );
                swept += n;
                if done {
                    break;
                }
            }
            freed_spans.sort_unstable_by_key(|&(start, _)| start);
            crate::memory::addr_keyed::drop_address_keyed_rows_in(shared, &freed_spans);
        }
        // gcd d1/c: outside the guard (see above).
        reference_rows_dropped +=
            gen_conc_sweep_drop_reference_rows(shared, rows_registered_before, &freed_spans);
        reference_row_prunes += u64::from(!freed_spans.is_empty());
        unswept_frees = swept > 0;
    } else {
        loop {
            freed_spans.clear();
            let (n, done) = match shared.mem.heap.old_gen_lock() {
                Some(mut guard) => {
                    let r = marker.concurrent_sweep_budget_into(
                        &mut *guard,
                        slice,
                        Some(&mut freed_spans),
                    );
                    // Under the guard: nothing can have been allocated on
                    // these spans yet.
                    freed_spans.sort_unstable_by_key(|&(start, _)| start);
                    crate::memory::addr_keyed::drop_address_keyed_rows_in(shared, &freed_spans);
                    r
                }
                None => (0, true),
            };
            // gcd d1/c: the guard is dropped; before the safepoint below.
            if refrow_batch {
                // gcg g2/r: hold the spans (no row walk); flush before a pause
                // this thread is about to join, or at the cap.
                let pause_requested = shared
                    .mem
                    .gc_barrier
                    .stw_requested
                    .load(std::sync::atomic::Ordering::Acquire);
                if !freed_spans.is_empty() || pause_requested {
                    let mut rp = shared.mem.ref_processor.lock();
                    rp.defer_swept_spans(rows_registered_before, &freed_spans);
                    if !done && rp.deferred_sweep_wants_flush(pause_requested) {
                        rp.flush_deferred_sweep();
                    }
                }
            } else {
                reference_row_prunes += u64::from(!freed_spans.is_empty());
                reference_rows_dropped += gen_conc_sweep_drop_reference_rows(
                    shared,
                    rows_registered_before,
                    &freed_spans,
                );
            }
            swept += n;
            unswept_frees |= n > 0;
            if done {
                break;
            }
            if unswept_frees
                && shared
                    .mem
                    .gc_barrier
                    .stw_requested
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                crate::memory::addr_keyed::sweep_address_keyed_tables(shared);
                unswept_frees = false;
            }
            safepoint_check(shared, thread);
        }
    }
    if unswept_frees {
        crate::memory::addr_keyed::sweep_address_keyed_tables(shared);
    }
    if refrow_batch {
        // gcg g2/r: the sweep's end flushes what is still held; the
        // processor's totals across this sweep (pause-time flushes included)
        // are the batch arm's walks and rows.
        let mut rp = shared.mem.ref_processor.lock();
        rp.flush_deferred_sweep();
        let (walks, rows) = rp.deferred_sweep_totals();
        reference_row_prunes += walks.saturating_sub(batch_walks_before);
        reference_rows_dropped += rows.saturating_sub(batch_rows_before) as usize;
    }
    shared
        .mem
        .concurrent_gc_state
        .note_sweep_reference_rows_dropped(reference_rows_dropped);
    shared
        .mem
        .concurrent_gc_state
        .note_sweep_reference_row_prunes(reference_row_prunes);
    if swept > 0 {
        tracing::debug!("Concurrent GC: swept {} old-gen objects", swept,);
    }
    // Cycle complete — phase back to Idle (the write barrier's
    // `is_marking_active()` gate is already false after remark, but leaving
    // the shared state at `ConcurrentSweep` would misreport the VM as
    // mid-cycle to any observer).
    //
    // gen r4w2/concmark: `concurrent_sweep` already stored `Idle` on every
    // path, and that store handed the shared phase to whichever driver opens
    // next; `complete` releases ownership WITHOUT writing the phase again
    // (`finish_cycle` is now a CAS from `ConcurrentSweep`).
    cycle.complete();
    // gen r5w4/conc8 — unregister the retained layouts this sweep's census
    // released (empty unless one ran and was complete). Outside every pause
    // and every heap lock: no instance of these classes is left to size.
    let releasable = shared.mem.concurrent_gc_state.take_releasable_layouts();
    if !releasable.is_empty() {
        let released = crate::memory::gc::release_retained_layouts(shared, &releasable);
        shared
            .mem
            .concurrent_gc_state
            .note_layouts_released(released);
    }
    // gen r5w3/unload7: the sweep's JFR row, outside every pause.
    super::gc_events::gen_concurrent_sweep_jfr_close(shared, sweep_jfr);
    // gen r4w4/concmark4 — the cycle on the logs. The sweep's end recorded
    // this cycle's report (`ConcurrentGcState::note_cycle_end`); `cycle >
    // completed_before` makes sure it is THIS cycle's and not an older one
    // (a sweep that authorised nothing records no report). `-Xlog:gc` gets
    // the HotSpot-shaped `GC(c<n>) Concurrent Mark Cycle <old>M-><old>M(<cap>M)
    // <t>ms`; `--verbose:gc` the `[GC] concurrent-cycle:` key=value line.
    if let Some(report) = shared
        .mem
        .concurrent_gc_state
        .last_cycle_report()
        .filter(|r| r.cycle > completed_before)
    {
        // gen r5w3/obs7: under `gc+marking`, so a plain `-Xlog:gc` stays one
        // `GC(<int>)` sequence, as on HotSpot.
        if crate::runtime::unified_logging::is_unified_logging_enabled(
            &[crate::runtime::unified_logging::LogTag::GcMarking],
            crate::runtime::unified_logging::LogLevel::Info,
        ) {
            crate::runtime::unified_logging::log_unified(
                &[crate::runtime::unified_logging::LogTag::GcMarking],
                crate::runtime::unified_logging::LogLevel::Info,
                &report.xlog_line(),
            );
        }
        if shared
            .mem
            .verbose_gc
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            eprintln!("{}", report.verbose_line());
        }
    }
}

/// JIT round 19 residual (`r19-an-unwind-out-of-a-pause-leaks-its-lazy-native-claims-FIXED-20261007`):
/// an unwind out of a MOVING collection pause ([`run_collection_pause`])
/// aborts the process. Between the take-over and the release the heap may be
/// half-relocated, so reopening the world with an empty pointer map (what
/// [`GenConcPauseGuard`] does for the non-moving pauses) would let every
/// mutator run on stale addresses; leaving it closed left `stw_requested`
/// raised and every counted mutator parked for good -- a silent hang where
/// HotSpot reports a fatal error. The panic's own message is already printed
/// by the panic hook when this runs. The frozen peers are resumed first by
/// `TakenOver`'s `Drop`; that no longer matters once the process aborts.
pub(in crate::runtime::interpreter::gc_and_alloc) struct MovingPauseUnwindAbort {
    pub(in crate::runtime::interpreter::gc_and_alloc) armed: bool,
}

impl MovingPauseUnwindAbort {
    pub(in crate::runtime::interpreter::gc_and_alloc) fn arm() -> Self {
        Self { armed: true }
    }

    /// The normal end of the pause: the world was released.
    pub(in crate::runtime::interpreter::gc_and_alloc) fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for MovingPauseUnwindAbort {
    fn drop(&mut self) {
        if self.armed && std::thread::panicking() {
            eprintln!(
                "[gc] fatal: a panic unwound out of a stop-the-world collection; the heap \
                 may be half-relocated and every parked thread would wait for good -- aborting"
            );
            std::process::abort();
        }
    }
}

/// gen r5w5/conc9 — reopens the world if one of the generational concurrent
/// cycle's two pauses (the initial mark in [`maybe_concurrent_gc_at`], the
/// remark in [`gen_concurrent_remark_pause`]) UNWINDS.
///
/// Both pauses are open-coded (request, take-over, work, resume, release)
/// because they need the frozen peers' roots, which [`NonMovingPause`] does not
/// gather. So a panic anywhere between the take-over and the release — the
/// root scan, the marker, the remark's reference pass or class unload — left
/// `stw_requested` raised, every counted mutator parked and every frozen peer
/// suspended, for good: a hang instead of a crash report (the cycle guard
/// `ConcurrentCycle` aborted the cycle, but nothing reopened the world). On the
/// service thread (`CRATONVM_GEN_CONC_SERVICE_THREAD`) it also hid the panic.
/// This does on an unwind what [`NonMovingPause`]'s `Drop` does on every path:
/// resume the peers (retiring the skip spans the take-over published, or
/// clearing them if the take-over itself unwound) and complete the pause with
/// an empty pointer map (nothing moves in either pause). The normal path hands
/// the peers back with [`Self::release`] and runs its own resume and release,
/// byte for byte as before.
pub(in crate::runtime::interpreter::gc_and_alloc) struct GenConcPauseGuard<'a> {
    pub(in crate::runtime::interpreter::gc_and_alloc) shared: &'a SharedVm,
    pub(in crate::runtime::interpreter::gc_and_alloc) taken: Option<crate::jit::xt_root_scan::TakenOver>,
    pub(in crate::runtime::interpreter::gc_and_alloc) open: bool,
}

impl<'a> GenConcPauseGuard<'a> {
    /// Arm before the take-over (the pause is already requested).
    pub(in crate::runtime::interpreter::gc_and_alloc) fn arm(shared: &'a SharedVm) -> Self {
        Self {
            shared,
            taken: None,
            open: true,
        }
    }

    /// Keep the take-over's frozen peers until the pause ends.
    pub(in crate::runtime::interpreter::gc_and_alloc) fn hold(&mut self, taken: crate::jit::xt_root_scan::TakenOver) {
        self.taken = Some(taken);
    }

    /// How many peers the take-over froze for this pause.
    pub(in crate::runtime::interpreter::gc_and_alloc) fn frozen_count(&self) -> usize {
        self.taken.as_ref().map_or(0, |t| t.unrewritable_count())
    }

    /// The normal end of the pause: disarm and hand the frozen peers back to
    /// the caller, which resumes them and releases the world itself.
    pub(in crate::runtime::interpreter::gc_and_alloc) fn release(mut self) -> crate::jit::xt_root_scan::TakenOver {
        self.open = false;
        self.taken.take().unwrap_or_default()
    }
}

impl Drop for GenConcPauseGuard<'_> {
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        match self.taken.take() {
            Some(taken) => retire_skip_spans_and_resume(self.shared, taken),
            // Unwound out of the take-over: it may have published spans.
            None => self.shared.mem.heap.clear_jit_tlab_skip_regions(),
        }
        self.shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());
    }
}

/// Phase 3 of the generational concurrent cycle: ONE attempt at the remark
/// pause.
///
/// `None`: another thread's stop-the-world won the race and nothing ran.
/// `Some(false)`: the pause ran but the remark inside it did not (the
/// unreachable `old_gen_lock() == None` arm; see below). `Some(true)`: the
/// bitmap is final and the sweep is authorised or refused by the remark
/// itself.
///
/// gen r4w2/concmark (2026-09-23): split out of `maybe_concurrent_gc_at`
/// unchanged, so the driver can retry a lost pause.
///
/// gen r5w1/refs5: `hidden` is `Some(the skip-set candidates)` when this cycle
/// published a referent-slot skip set at its initial mark, and then the remark
/// MUST run the reference processing that clears or resurrects every hidden
/// referent (see [`gen_remark_process_references`]); `None` is the remark as
/// before.
///
/// gen r5w3/unload7: `class_unload` — this cycle unloads classes (its initial
/// mark armed the marker's side tables). The remark then takes its roots under
/// the class-unload licence too, re-captures the tables and the young
/// objects' loaders, and unloads after `remark_finish`
/// ([`gen_remark_unload_classes`]). Honoured only together with `hidden`: the
/// unload runs inside the reference-processing arm, and a remark that deferred
/// roots without reconciling afterwards would free what the deferred rows
/// name, so without `hidden` this remark roots everything, as before.
///
/// gen r5w6/conc10: `y2o_live_seed` — `CRATONVM_GEN_Y2O_LIVE_SEED`, decided
/// at the initial mark: the remark's young→old seeds come from the live young
/// set too, and with `hidden` the referent slot of every active young
/// `Reference` is hidden (listed afresh here: young objects move between the
/// pauses), with the safety net extended over that list
/// ([`gen_remark_process_references`]).
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_concurrent_remark_pause(
    shared: &SharedVm,
    thread: &mut JvmThread,
    marker: &cratonvm_gc::ConcurrentMarker,
    hidden: Option<&[usize]>,
    class_unload: bool,
    y2o_live_seed: bool,
) -> Option<bool> {
    let class_unload = class_unload && hidden.is_some();
    // Phase 3: Remark — brief STW pause.
    // INT-3 residual fix: open-coded for the same takeover-wait reason as
    // Phase 1 above (a never-polling in-JIT peer must not stall the remark
    // nor be covered only by its stale deposit snapshot).
    //
    // Finding 1(a): remark pauses use the identity census too, so blocked
    // threads are excluded BY IDENTITY and their wake-time arrivals cannot
    // satisfy this pause's quota (`arrive_and_wait_auto`). The anonymous
    // `threads_blocked` subtraction this replaced excluded the same population
    // without recording who it excluded. gc-common w4-a: and the census says
    // whether the initiator owns a counted slot (`request_non_collection_pause`).
    let counted_os_tids = request_non_collection_pause(shared, thread.thread_id)?;
    {
        let mut xt_roots: Vec<ObjectRef> = Vec::new();
        // gen r5w5/conc9: as at the initial mark (`GenConcPauseGuard`).
        let mut pause_guard = GenConcPauseGuard::arm(shared);
        pause_guard.hold(stw_take_over_and_wait(
            shared,
            &mut xt_roots,
            &counted_os_tids,
        ));
        // gc-common w5-a: `--verbose:gc` pause line (`door=gen-remark`).
        let mut pause_timer = non_collection_pause_start(shared, NonCollectionPause::GenRemark);
        // Round-5 fix (CRIT — UAF): drain the initiator's per-thread
        // SATB buffer before remark drains the global queue. Other
        // mutators flushed when they arrived at the STW barrier;
        // the initiator must drain its own.
        shared.mem.heap.flush_thread_satb();
        // gen r5w3/unload7: under the class-unload licence when this cycle
        // unloads (see `maybe_concurrent_gc_at`'s initial mark).
        let roots = if class_unload {
            cratonvm_gc::gc_quiescence::with_class_unload_marking(|| {
                cratonvm_gc::gen_heap::with_no_young_sweep_following(|| collect_roots(shared, thread))
            })
        } else {
            cratonvm_gc::gen_heap::with_no_young_sweep_following(|| collect_roots(shared, thread))
        };
        // gci i13/cm item 6: no young sweep follows a concurrent pause; drop the
        // precise-root table this scan published so no later sweep on this
        // thread can take it (`take_sealed_precise_root_values`).
        cratonvm_gc::gen_heap::clear_precise_root_values();
        let mut snapshot_roots = shared.threads.thread_registry.collect_all_root_snapshots();
        // gch h4/bp: as in `run_collection_pause`.
        append_in_native_band_roots(shared, &mut snapshot_roots);
        let mut root_ptrs: Vec<*mut u8> = roots
            .iter()
            .chain(snapshot_roots.iter())
            // INT-3 — frozen in-JIT peers' conservative register/stack roots.
            .chain(xt_roots.iter())
            .map(|r| r.as_ptr())
            .collect();
        // JIT r23 w1b L7 (R23W1L7-4; `CRATONVM_DBG=root-kind-census`, off by default).
        root_kind_census_for_parts(
            shared,
            "gen-remark",
            [&roots[..], &snapshot_roots[..], &xt_roots[..]],
        );
        // fork6 GC_STRESS fix — refresh the young→old roots at remark
        // too: a young→old edge created during the concurrent phase
        // (e.g. a promoted child stored into a fresh young holder) must
        // be in the final bitmap before the sweep.
        //
        // gen r5w6/conc10: from the live young set under
        // `CRATONVM_GEN_Y2O_LIVE_SEED` (see the initial mark). The young
        // `Reference`s are listed again: this pause's addresses.
        let y2o = if y2o_live_seed {
            if class_unload {
                cratonvm_gc::gc_quiescence::with_class_unload_marking(|| {
                    gen_conc_y2o_seeds(shared, &root_ptrs, hidden.is_some())
                })
            } else {
                gen_conc_y2o_seeds(shared, &root_ptrs, hidden.is_some())
            }
        } else {
            None
        };
        match &y2o {
            Some(seeds) => root_ptrs.extend(seeds.old_refs.iter().map(|&a| a as *mut u8)),
            None => root_ptrs.extend(
                shared
                    .mem
                    .heap
                    .collect_young_to_old_roots()
                    .into_iter()
                    .map(|a| a as *mut u8),
            ),
        }
        let hidden_young: &[usize] = y2o.as_ref().map_or(&[][..], |s| &s.hidden_young_refs[..]);
        // gen r5w5/conc9: for the `CRATONVM_DBG_MIRRORPIN_WHY` report below.
        let young_to_old_end = root_ptrs.len();
        // gen r5w3/unload7: the rows THIS root scan deferred (merged into the
        // initial mark's; `remark_begin` marks those of every owner already
        // live) and the loaders of the young objects, which include every
        // instance allocated during the cycle.
        if class_unload {
            let (old_gen_base, old_gen_size) = shared.mem.heap.old_gen_info();
            let tables = cratonvm_gc::concurrent_mark::ClassUnloadTables::capture(
                old_gen_base,
                old_gen_size,
            );
            // gen r5w6/conc10: as at the initial mark.
            match &y2o {
                Some(seeds) => root_ptrs.extend(gen_conc_y2o_instance_loaders(seeds, &tables)),
                None => {
                    root_ptrs.extend(gen_conc_young_instance_loaders(shared, &tables, &mut None))
                }
            }
            if gen_conc_unload_why_enabled() {
                gen_conc_young_instances_why(shared, &tables, y2o.as_ref(), "remark");
            }
            marker.add_class_unload_tables(tables);
        }
        // The remark is NOT optional (see the abort in `maybe_concurrent_gc_at`), but
        // this `if let` is the one place where that invariant rests on two
        // `match`es in `VmHeap` agreeing rather than on anything structural:
        // the cycle only gets here when the Generational arm's
        // `concurrent_cycle_due()` said yes (gen r4w4/concmark4; it was
        // `old_gen_needs_gc()`, a hard `false` on G1 and ZGC — the driver now
        // answers `false` itself off Generational), and `old_gen_lock()` is unconditionally
        // `Some` on Generational. So today the `None` arm is unreachable. It is
        // recorded rather than assumed because a fallible `old_gen_lock()` arm
        // would otherwise skip the remark SILENTLY and fall through to the
        // Phase 4 sweep with a non-final bitmap — the exact live-object loss
        // the fork6 fix below was written for, reintroduced by an edit two
        // files away.
        let remarked = if let Some(hidden) = hidden {
            // gen r5w1/refs5 — half 2: remark-time reference processing, with
            // the old-gen guard RELEASED around it (the design page's lock-order
            // option (b)). The processing reaches heap accessors that take the
            // old-gen lock themselves (`soft_ref_policy_free_mb`,
            // `is_heap_addr` in the closure tracer), which would self-deadlock
            // under a held guard, and it would add an `old-gen → ref_processor`
            // edge the lock ranking never had. `ConcurrentMarker::remark_begin`
            // argues why releasing the guard inside the pause is sound.
            //
            // SAFETY: as for the arm below — inside the pause this function
            // requested, every other mutator parked by `stw_take_over_and_wait`.
            let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
            let progress = shared
                .mem
                .heap
                .old_gen_lock()
                .map(|guard| marker.remark_begin(&stw, &root_ptrs, &*guard));
            match progress {
                Some(progress) => {
                    let authorised = progress.sweep_authorised();
                    let keep = if authorised {
                        let is_marked = marker.remark_liveness();
                        // gen r5w5/conc9: what `remark_begin`'s dead-finalizer
                        // retention marked (recorded because this cycle
                        // processes references at remark).
                        let mut retained = marker.take_finalizer_retention_closure();
                        // gcd d1/c — and the YOUNG objects only those reach,
                        // under `CRATONVM_GEN_Y2O_LIVE_SEED` (the live young
                        // set it needs is that flag's; see
                        // `gen_conc_young_behind_retained`).
                        if y2o_live_seed && !retained.is_empty() {
                            let young = gen_conc_young_behind_retained(
                                shared, &root_ptrs, marker, &retained,
                            );
                            retained.extend(young);
                        }
                        let (keep, retired) = gen_remark_process_references(
                            shared,
                            &is_marked,
                            hidden,
                            hidden_young,
                            class_unload,
                            &retained,
                        );
                        // gc-common w8-c: the JNI weak-global clear runs after
                        // `remark_finish` below (gci i13/jc F15).
                        shared
                            .mem
                            .concurrent_gc_state
                            .note_remark_refproc_retired(retired);
                        Some(keep)
                    } else {
                        None
                    };
                    let finished = match shared.mem.heap.old_gen_lock() {
                        Some(guard) => {
                            marker.remark_finish(&stw, progress, keep.as_deref(), &*guard);
                            true
                        }
                        // Unreachable (see above); the unfinished remark is
                        // abandoned with the cycle by the driver.
                        None => false,
                    };
                    // gc-common w8-c: JNI weak globals whose referent this
                    // cycle judged dead read NULL from here on, while every
                    // mutator is still stopped and before the concurrent sweep
                    // frees anything; a referent the reference pass is
                    // resurrecting (`keep`) keeps its weak global.
                    //
                    // gci i13/jc (F15): AFTER `remark_finish`, which marks the
                    // keep closure and drains it. Before it (where this ran),
                    // an object reachable only from a resurrected finalizable
                    // read unmarked, and its weak global was cleared one cycle
                    // earlier than HotSpot clears it (HotSpot's weak JNI
                    // processing follows the final-reference keep-alive). G1
                    // already clears on the final bitmap
                    // (`g1_remark_mark_kept_closure` runs first).
                    if let Some(keep) = keep.as_deref() {
                        let is_marked = marker.remark_liveness();
                        let _ = crate::native::jni::clear_weak_global_refs_unmarked(
                            shared, &is_marked, keep,
                        );
                    }
                    // gen r5w3/unload7: the class unload, on the FINAL bitmap
                    // (after the keep closure and the last SATB drain), with
                    // the old-gen guard released and the world still stopped.
                    // A refused remark sweeps nothing and judges nothing dead.
                    if finished && authorised && class_unload {
                        // gen r5w5/conc9 — the final bitmap's verdict on each
                        // user loader, and which remark source named it
                        // (diagnostic; see `gen_conc_unload_why`).
                        if gen_conc_unload_why_enabled() {
                            let a = roots.len();
                            let b = a + snapshot_roots.len();
                            let c = b + xt_roots.len();
                            let keep_ptrs: Vec<*mut u8> =
                                keep.iter().flatten().map(|&addr| addr as *mut u8).collect();
                            gen_conc_unload_why(
                                shared,
                                marker,
                                "remark",
                                &[
                                    ("collect_roots", &root_ptrs[..a]),
                                    ("thread_snapshots", &root_ptrs[a..b]),
                                    ("frozen_peers", &root_ptrs[b..c]),
                                    ("young_to_old", &root_ptrs[c..young_to_old_end]),
                                    ("young_instance_loaders", &root_ptrs[young_to_old_end..]),
                                    ("refproc_keep", &keep_ptrs[..]),
                                ],
                            );
                        }
                        gen_remark_unload_classes(shared, marker);
                    }
                    finished
                }
                None => false,
            }
        } else if let Some(guard) = shared.mem.heap.old_gen_lock() {
            // SAFETY: this runs inside the pause this function requested
            // (`request_non_collection_pause`), after
            // `stw_take_over_and_wait` has parked every other mutator -- the
            // canonical construction site named in `StopTheWorldToken::new`'s
            // own safety note. `ConcurrentMarker::remark` requires the witness
            // because its SATB final drain does (2026-09-21): the drain has no
            // happens-before edge against a writer parked on a shard lock, and
            // only a real pause closes that window.
            let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
            // gc-common w8-c: clear the JNI weak globals whose referent this
            // cycle judged dead, at the remark, while every mutator is stopped
            // and before `concurrent_sweep` can free the referent. After the
            // remark the SATB barrier is off, so a weak global resolved
            // between the remark and the free would hand out a referent the
            // sweep then frees. Returns nothing to resurrect.
            let mut clear_jni_weak = |is_marked: &dyn Fn(usize) -> bool| -> Vec<usize> {
                let _ = crate::native::jni::clear_weak_global_refs_unmarked(shared, is_marked, &[]);
                Vec::new()
            };
            marker.remark_with_reference_processing(
                &stw,
                &root_ptrs,
                &*guard,
                Some(&mut clear_jni_weak),
            );
            true
        } else {
            false
        };
        // Clear TLAB skip regions + resume frozen peers BEFORE reopening
        // the world (same race rationale as maybe_gc's epilogue).
        non_collection_pause_seal(&mut pause_timer);
        let taken = pause_guard.release();
        retire_skip_spans_and_resume(shared, taken);
        shared
            .mem
            .gc_barrier
            .complete_gc(cratonvm_types::PointerMap::default());
        non_collection_pause_finish(pause_timer);
        // gci i11/go (R3): the remark's deferred `gc+ref` block.
        flush_remark_reference_lines(shared);
        Some(remarked)
    }
}

/// gen r5w1/refs5 — does this generational concurrent cycle run remark-time
/// reference processing? Opt-in `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK` (the
/// wave-5 hook flag, now the real processing's switch), and only while the
/// reference protocol itself is on: `CRATONVM_WEAKREF_CLEAR=0` (never clear)
/// and `CRATONVM_DBG_NO_REFPROC` (process nothing) would leave hidden
/// referents unprocessed — freed by the sweep while their `Reference` still
/// points at them — so under either the cycle hides nothing.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_conc_remark_refproc_enabled() -> bool {
    cratonvm_types::flags().gc.gen_conc_remark_refproc_hook
        && weakref_clear_enabled()
        && !no_refproc()
}

/// gcd d1/c (2026-09-27) — is this thread the generational concurrent-cycle
/// service, and has VM teardown stopped it (`ConcurrentGcState::shutdown_service`)?
/// Only then does [`maybe_concurrent_gc_at`] give up a cycle between two
/// Phase-2 slices. The first test is one atomic load; the second (a leaf lock)
/// runs only after a stop.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_conc_service_stopped_here(shared: &SharedVm) -> bool {
    let state = &shared.mem.concurrent_gc_state;
    state.service_shutting_down() && state.on_service_thread()
}

/// gcd d1/c (2026-09-27) — drop the reference processor's soft / weak /
/// phantom rows whose `Reference` object lies in `freed_spans` (what one
/// concurrent sweep slice just freed; sorted by start, non-overlapping) and
/// that were registered before `registered_before`. Returns the ACTIVE rows
/// dropped. Nothing (not even the lock) for an empty span list.
///
/// Called by [`maybe_concurrent_gc_at`]'s sweep with NO heap lock held: the
/// processor (L7) is never taken under the old-gen guard (the remark's lock
/// order), and the test is a pure range test, so an object allocated on a
/// freed span since cannot make a dead row look live. See
/// `ReferenceProcessor::remove_reference_objects_registered_before` and
/// `gengc-r5w5-conc9-concurrent-sweep-leaves-reference-rows-of-freed-references`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_conc_sweep_drop_reference_rows(
    shared: &SharedVm,
    registered_before: u64,
    freed_spans: &[(usize, usize)],
) -> usize {
    if freed_spans.is_empty() {
        return 0;
    }
    drop_swept_reference_rows(
        &mut shared.mem.ref_processor.lock(),
        registered_before,
        freed_spans,
    )
}

/// The processor half of [`gen_conc_sweep_drop_reference_rows`] (a unit-test
/// seam: no VM needed).
pub(in crate::runtime::interpreter::gc_and_alloc) fn drop_swept_reference_rows(
    ref_proc: &mut cratonvm_gc::ReferenceProcessor,
    registered_before: u64,
    freed_spans: &[(usize, usize)],
) -> usize {
    ref_proc.remove_reference_objects_registered_before(registered_before, &|addr| {
        crate::memory::addr_keyed::in_spans(addr, freed_spans)
    })
}


/// gen r5w1/refs5 — the referent-slot skip set's candidates for a generational
/// concurrent cycle: every ACTIVE Weak/Soft/Phantom `Reference` the VM's
/// processor holds that lies in the old generation and still IS that
/// `Reference` (not an array, a `java.lang.ref.Reference` subclass with the
/// referent/queue fields, the identity it was discovered with). Called inside
/// the initial-mark pause with NO old-gen guard held.
///
/// Every screen declines, because hiding is the dangerous direction: a slot 0
/// hidden on an object that is not the recorded `Reference` would hide an
/// ordinary STRONG field, and the object behind it could be swept live. A
/// declined candidate is simply traced (over-retention, as today). No
/// `java/lang/ref/Reference` class resolved yet hides nothing.
///
/// Lock ranks: the processor (L7) is read and RELEASED before the class
/// manager (L10) is taken, the order `process_references_after_gc` keeps.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_conc_reference_skip_candidates(
    shared: &SharedVm,
    old_gen_base: usize,
    old_gen_size: usize,
) -> Vec<usize> {
    let rows = shared
        .mem
        .ref_processor
        .lock()
        .active_reference_objects_with_stamps();
    if rows.is_empty() {
        return Vec::new();
    }
    let class_manager = shared.classes.class_manager.read();
    let Some(reference_cid) = class_manager.find_bootstrap_class_by_name("java/lang/ref/Reference")
    else {
        return Vec::new();
    };
    let old_gen_end = old_gen_base.saturating_add(old_gen_size);
    let heap = &shared.mem.heap;
    rows.into_iter()
        .filter(|&(addr, stamp)| {
            if addr < old_gen_base || addr >= old_gen_end || addr & 7 != 0 {
                // Young (its referent stays traced through
                // `collect_young_to_old_roots`) or not an object start.
                return false;
            }
            // SAFETY: an 8-aligned address inside the old generation's
            // storage, held by the processor; only its header is read below.
            let obj = unsafe { ObjectRef::from_raw(addr as *mut u8) };
            heap.kind_of(obj) != crate::memory::heap::ObjectKind::Array
                && class_manager.is_subclass_of(heap.class_id_of(obj), reference_cid)
                && heap.num_fields(obj) >= 2
                && (stamp == 0 || {
                    let now = heap.identity_hash_code(obj);
                    now == 0 || now == stamp
                })
        })
        .map(|(addr, _)| addr)
        .collect()
}

/// `CRATONVM_ZGC_CONC_REF_SKIP` (default ON): does a ZGC
/// concurrent mark cycle hide the VM's active Weak / Phantom referent slots
/// ([`zgc_conc_reference_skip_candidates`])? Read at each mark-start pause
/// (one flag-snapshot lookup per cycle, never per allocation).
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_conc_ref_skip_enabled() -> bool {
    true
}


/// zr z1/rf — the ZGC concurrent cycle's referent-slot skip set: every ACTIVE
/// Weak / Phantom row of the VM processor
/// (`ReferenceProcessor::active_weak_phantom_objects_with_stamps`) whose
/// address still names a `java.lang.ref.Reference` with its registered
/// identity, the screens [`gen_conc_reference_skip_candidates`] applies (over
/// the whole heap: ZGC has no old-generation range). Hiding a slot of an
/// object that is NOT that `Reference` would leave a live field untraced, so a
/// declined row is simply traced (over-retention, as before). Soft rows are
/// handed in separately ([`zgc_conc_soft_reference_skip_candidates`], zr
/// z2/cc). Lock ranks: processor (L7) read and released before the class
/// manager (L10).
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_conc_reference_skip_candidates(shared: &SharedVm) -> Vec<usize> {
    if shared.mem.heap.zgc_persistent_skip_bits_enabled() {
        return Vec::new();
    }
    let rows = shared
        .mem
        .ref_processor
        .lock()
        .active_weak_phantom_objects_with_stamps();
    zgc_screen_reference_rows(shared, rows)
}

/// zr z2/cc — `CRATONVM_ZGC_CONC_REF_SKIP_SOFT` (default ON, meaningful only
/// with concurrent reference processing enabled): does the concurrent cycle hide the
/// active SOFT rows too (for its concurrent phase; the heap re-roots the kept
/// referents at the mark end)? `=0` restores weak / phantom only. Read once
/// per mark-start pause.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_conc_ref_skip_soft_enabled() -> bool {
    true
}

/// zr z2/cc — the soft half of [`zgc_conc_reference_skip_candidates`]: every
/// ACTIVE Soft row (`ReferenceProcessor::active_soft_objects_with_stamps`)
/// through the same screens. Hidden for the concurrent phase only; see
/// `ZgcRealHeap::set_concurrent_vm_soft_reference_skip` for why the mark end
/// needs no VM hand-in (the pre-collection pass nulls the condemned slots
/// before the collection that finishes the cycle).
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_conc_soft_reference_skip_candidates(shared: &SharedVm) -> Vec<usize> {
    let rows = shared
        .mem
        .ref_processor
        .lock()
        .active_soft_objects_with_stamps();
    zgc_screen_reference_rows(shared, rows)
}

/// The screens shared by the weak and soft skip candidates: an 8-aligned
/// registered object start, not an array, a `java.lang.ref.Reference`
/// subclass, two or more fields, and the row's identity stamp. Lock ranks:
/// the caller has released the processor (L7) before the class manager (L10)
/// is taken here.
pub(in crate::runtime::interpreter::gc_and_alloc) fn zgc_screen_reference_rows(shared: &SharedVm, rows: Vec<(usize, i32)>) -> Vec<usize> {
    if rows.is_empty() {
        return Vec::new();
    }
    let class_manager = shared.classes.class_manager.read();
    let Some(reference_cid) = class_manager.find_bootstrap_class_by_name("java/lang/ref/Reference")
    else {
        return Vec::new();
    };
    let heap = &shared.mem.heap;
    rows.into_iter()
        .filter(|&(addr, stamp)| {
            if addr & 7 != 0 || heap.is_object_address(addr).is_none() {
                return false;
            }
            // SAFETY: an 8-aligned registered object start held by the
            // processor, inside the mark-start pause; only its header is read.
            let obj = unsafe { ObjectRef::from_raw(addr as *mut u8) };
            heap.kind_of(obj) != crate::memory::heap::ObjectKind::Array
                && class_manager.is_subclass_of(heap.class_id_of(obj), reference_cid)
                && heap.num_fields(obj) >= 2
                && (stamp == 0 || {
                    let now = heap.identity_hash_code(obj);
                    now == 0 || now == stamp
                })
        })
        .map(|(addr, _)| addr)
        .collect()
}

/// gen r5w1/refs5 — remark-time reference processing for the GENERATIONAL
/// concurrent cycle: the twin of [`g1_remark_process_references`] that
/// `docs/feature-designs/generative-gc-architecture.md`
/// specifies. Runs inside the remark pause, between
/// `ConcurrentMarker::remark_begin` and `remark_finish`, with the old-gen
/// guard RELEASED, and `is_marked` the seam's verdict ("marked in this cycle's
/// bitmap, or not sweep-eligible" — young, promoted after the initial mark,
/// or outside the old generation all read as live, so only an old-gen object
/// that existed at the initial mark can be judged dead).
///
/// Returns `(keep, retired)`: the addresses the remark must mark before the
/// sweep, and how many weak/soft/phantom rows left the processor's active set
/// this round (cleared, enqueued, or pruned because their own `Reference` was
/// dead by mark).
///
/// # What it reuses, and why that is the same code
///
/// The consumer protocol is G1's, and it is backend-agnostic once the heap
/// lock is not held: class-mirror / defining-loader reconcile and class
/// unloading, the dead-`Reference` pre-pass, the four phases with the
/// transitive tracer (`g1_remark_reference_closure`, which descends only into
/// UNMARKED objects — here exactly the sweep-eligible old ones — and treats
/// every `Reference`'s slot 0 as no edge, as the hiding marker does), the
/// SATB-suppressed referent clears, the dead-by-mark enqueue guards, the queue
/// wake-up hand-off (RECORDED for the delivery thread, never run here — the
/// initiator may be the service thread, which must not run Java), and the
/// keep list: dead finalizables, cleaner actions and chains, soft survivors.
/// So this calls it rather than copying ~150 lines of shape guards.
///
/// # What the generational cycle adds
///
/// 1. **Phantom slots.** A phantom enqueued here must have its slot 0 NULLED
///    (JDK 9+ clears a phantom as it is enqueued; the post-collection path
///    gets that from its pre-null pass, which never restores an enqueued
///    phantom). Otherwise the referent is swept and the live
///    `PhantomReference` keeps a dangling slot 0 that the next card scan or
///    cycle would trace. Every row this round RETIRED whose `Reference` is
///    live gets its slot nulled. Found here first; the nulling now lives in
///    [`remark_reference_rows`], so G1's remark, which had the same hole, does
///    it too.
/// 2. **A safety net over the hidden set.** After processing, a live hidden
///    `Reference` whose slot 0 still names an UNMARKED referent — a row the
///    phases did not retire (softly or finalizer-reachable, policy-kept, or
///    one that went inactive between the two pauses without its slot being
///    cleared) — has that referent added to `keep`. The one-commit rule is
///    then a local invariant of this function: no hidden referent reaches the
///    sweep unmarked while a live `Reference` points at it.
///
/// # gen r5w3/unload7 — class unloading, and the retired rows
///
/// * `class_unload`: this cycle unloads classes
///   (`CRATONVM_GEN_CONC_CLASS_UNLOAD`). The class-metadata reconcile is then
///   NOT run here but after `remark_finish` ([`gen_remark_unload_classes`]):
///   the keep list this returns (finalizables about to be resurrected, soft
///   survivors, cleaner actions) is marked by that closure, and a kept object's
///   scan follows its class's `loader_pin` edge, so its loader is live by the
///   time the verdict is taken. Reconciling first (G1's order) would unload
///   the class of an object the same remark then resurrects. Without
///   `class_unload` the reconcile runs first, as before (under the veto every
///   user-loader mirror is a root, so it unloads nothing a scan could miss) —
///   with the dead classes' layouts retained for the sweep either way
///   (`memory::gc::with_retained_unloaded_layouts`).
/// * The retired rows come from the processor
///   (`ReferenceProcessingResult::retired`, plus the pre-pass's dropped active
///   rows) instead of two `active_reference_object_set()` builds and their
///   difference (`gengc-r5w1-refs5-proposal-processor-reports-what-it-retired`).
///
/// # gen r5w5/conc9 — references to what the finalizer retention kept
///
/// `retained` is what `ConcurrentMarker::remark_begin`'s dead-finalizer
/// retention marked (`take_finalizer_retention_closure`): every dead old
/// finalizable and everything reachable only through one. `is_marked` calls
/// them live, so without this a `WeakReference` to such an object survived
/// the remark, and `finalize()` could hand the object back to the program
/// with the weak reference still naming it. HotSpot clears soft and weak
/// references first. They are passed to [`remark_reference_rows`], which tells
/// the processor they are marked only for `finalize()` — the post-collection
/// path's `note_resurrected_finalizables` contract. Empty (no dead finalizable)
/// is the remark as before.
///
/// # gen r5w6/conc10 — the hidden YOUNG references
///
/// `hidden_young` lists the young `Reference`s whose slot 0 this remark's
/// young→old seeding hid (`CRATONVM_GEN_Y2O_LIVE_SEED`, `gen_conc_y2o_seeds`;
/// empty otherwise). The phases process their rows like any other (a young
/// `Reference` is live by `is_marked`, so a retired one gets its slot nulled
/// in step 1), and the safety net of step 2 covers them too: a live young
/// hidden `Reference` whose slot 0 still names an UNMARKED old-gen referent
/// has it kept.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_remark_process_references(
    shared: &SharedVm,
    is_marked: &dyn Fn(usize) -> bool,
    hidden: &[usize],
    hidden_young: &[usize],
    class_unload: bool,
    retained: &[usize],
) -> (Vec<usize>, usize) {
    if !class_unload {
        let unloaded = remark_unload_dead_classes(shared, is_marked);
        // gen r5w4/conc8: this path retains layouts too (and the census
        // releases them), so it counts them, or `layouts_retained -
        // layouts_released` would stop being what is still held. Its loaders
        // and classes stay out of the class-unload counters, which describe
        // remarks that ran with the side tables armed.
        shared
            .mem
            .concurrent_gc_state
            .note_class_unload(0, 0, unloaded.layouts_retained);
    }
    // gci i48/zl2: `CRATONVM_G1_REMARK_SOFT_CLOCK_NOW` covers this remark too.
    gen_remark_note_soft_clock(shared);
    let rows = remark_reference_rows(shared, is_marked, retained);
    // gcj j4/rf (`gcj-j1rf-gen-reference-and-finalizer-review-residuals` item
    // 2): soft before final.
    if !retained.is_empty()
        && !rows.soft_kept.is_empty()
    {
        gen_remark_retract_softly_reachable_finalizers(
            shared,
            is_marked,
            retained,
            &rows.soft_kept,
        );
    }
    let mut keep = rows.keep;
    let mut retired_rows = rows.pruned;

    // L10 alone: the processor lock was released above.
    let class_manager = shared.classes.class_manager.read();
    let reference_cid = class_manager.find_bootstrap_class_by_name("java/lang/ref/Reference");
    let heap = &shared.mem.heap;
    // Same guard as the other referent writers: a registry address that is
    // live by mark and still a `Reference` (not an array; the class check is
    // skipped only when the class is not resolved, as theirs is).
    let is_reference_shaped = |obj: ObjectRef| -> bool {
        heap.kind_of(obj) != crate::memory::heap::ObjectKind::Array
            && heap.num_fields(obj) >= 2
            && match reference_cid {
                Some(cid) => class_manager.is_subclass_of(heap.class_id_of(obj), cid),
                None => true,
            }
    };

    // 1. Every row this round retired (cleared or enqueued) had its referent
    //    slot nulled inside `remark_reference_rows`, which G1's remark shares
    //    since the G1 phantom fix; here they are only counted. The rows the
    //    pre-pass pruned as dead by mark were counted above and need nothing:
    //    the sweep frees their `Reference`.
    retired_rows += rows.retired.len();

    // 2. The safety net over the hidden set.
    let (old_gen_base, old_gen_size) = heap.old_gen_info();
    let old_gen_end = old_gen_base.saturating_add(old_gen_size);
    for &ref_addr in hidden {
        if ref_addr < old_gen_base || ref_addr >= old_gen_end || !is_marked(ref_addr) {
            // Not hidden (young), or dead: nothing points at its referent.
            continue;
        }
        // SAFETY: as above.
        let obj = unsafe { ObjectRef::from_raw(ref_addr as *mut u8) };
        if !is_reference_shaped(obj) {
            continue;
        }
        if let Value::Object(Some(referent)) = heap.get_field(obj, 0) {
            let addr = referent.as_ptr() as usize;
            if !is_marked(addr) {
                keep.push(addr);
            }
        }
    }
    // 2b. gen r5w6/conc10 — the same net over the hidden YOUNG references
    //     (listed in this pause, so their addresses are current). Only an OLD
    //     referent can be unmarked; a young one is live by `is_marked`.
    for &ref_addr in hidden_young {
        if !is_marked(ref_addr) {
            continue;
        }
        // SAFETY: a current young-object address listed by this pause's
        // shape- and identity-screened scan (`gen_conc_young_reference_skip_candidates`);
        // the world is stopped and nothing has moved since.
        let obj = unsafe { ObjectRef::from_raw(ref_addr as *mut u8) };
        if !is_reference_shaped(obj) {
            continue;
        }
        if let Value::Object(Some(referent)) = heap.get_field(obj, 0) {
            let addr = referent.as_ptr() as usize;
            if addr >= old_gen_base && addr < old_gen_end && !is_marked(addr) {
                keep.push(addr);
            }
        }
    }
    drop(class_manager);
    (keep, retired_rows)
}



/// gcj j4/rf (`gcj-j1rf-gen-reference-and-finalizer-review-residuals` item 2):
/// the Generational remark retains its dead old finalizables
/// (`ConcurrentMarker::remark_begin`, on the strong-only bitmap) BEFORE the
/// reference round's soft phase, and the round then keeps a recently read
/// `SoftReference`'s referent and everything behind it -- but a retained
/// candidate in that subgraph stayed on the retained list, so the next
/// stop-the-world collection reported it dead and `finalize()` ran while
/// `get()` still returned it. HotSpot processes soft references before final
/// ones: a softly reachable finalizable is not finalized at that cycle.
///
/// Traces from the kept soft referents through the retention closure
/// (`retained`, marked only for `finalize()`, so treated as unmarked here, as
/// the round's own tracer does) and takes every retained candidate it reaches
/// back off the list (`ConcurrentGcState::retract_remark_retained_finalizers`).
/// The candidate stays marked (it is alive) and registered; a later cycle
/// that finds it dead finalizes it. Returns how many were retracted.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_remark_retract_softly_reachable_finalizers(
    shared: &SharedVm,
    is_marked: &dyn Fn(usize) -> bool,
    retained: &[usize],
    soft_kept: &[usize],
) -> usize {
    // addr-keyed: local to this remark pause; dropped before the world resumes.
    let retained_set: rustc_hash::FxHashSet<usize> = retained.iter().copied().collect();
    let tracer_is_marked =
        |addr: usize| -> bool { is_marked(addr) && !retained_set.contains(&addr) };
    let class_manager = shared.classes.class_manager.read();
    let reference_cid = class_manager.find_bootstrap_class_by_name("java/lang/ref/Reference");
    let heap = &shared.mem.heap;
    let is_reference_shaped = |obj: ObjectRef| -> bool {
        heap.kind_of(obj) != crate::memory::heap::ObjectKind::Array
            && match reference_cid {
                Some(cid) => class_manager.is_subclass_of(heap.class_id_of(obj), cid),
                None => true,
            }
    };
    let reached =
        g1_remark_reference_closure(shared, &tracer_is_marked, &is_reference_shaped, soft_kept);
    drop(class_manager);
    // addr-keyed: local to this remark pause; dropped before the world resumes.
    let softly: rustc_hash::FxHashSet<usize> = reached
        .into_iter()
        .filter(|a| retained_set.contains(a))
        .collect();
    let n = shared
        .mem
        .concurrent_gc_state
        .retract_remark_retained_finalizers(&softly);
    if n != 0 && None::<std::ffi::OsString>.is_some() {
        eprintln!("[refproc-remark] soft-before-final: {n} retained finalizable(s) softly reachable, not reported");    }
    n
}

/// gen r5w3/unload7 — does this generational concurrent cycle UNLOAD classes?
/// Opt-in `CRATONVM_GEN_CONC_CLASS_UNLOAD`, and only while loader unloading and
/// instance→loader pinning are on (the side tables it follows exist only
/// then). The caller also requires remark-time reference processing
/// ([`gen_conc_remark_refproc_enabled`]): the unload runs in that remark.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_conc_class_unload_enabled() -> bool {
    cratonvm_types::flags().gc.gen_conc_class_unload
        && cratonvm_native_builtins::classloader::loader_unload_enabled()
        && cratonvm_types::loader_pin::loader_pinning_enabled()
}

/// gen r5w3/unload7 — the defining loader of every YOUNG object's class, as
/// extra old-gen mark roots for a class-unloading concurrent cycle (hazard 2
/// on `gengc-r5w1-refs5-concurrent-cycle-cannot-unload-classes`, its instance
/// half).
///
/// The concurrent trace never scans a young object, and the young objects'
/// FIELD edges into the old generation are roots already
/// (`collect_young_to_old_roots`) — but an instance's edge to its class's
/// loader is not a field, it is the `loader_pin` row. A class whose mirror the
/// root scan deferred and whose only live instances are young would otherwise
/// have an unmarked loader, and the remark would unload the class under its
/// live instances. Every young object is live by assumption here (the cycle
/// does not collect young), so every young object's loader is a root:
/// over-approximate in the safe direction, one young walk per pause. The
/// loaders that are themselves young need nothing (the trace does not collect
/// young); their OWN rows are marked by the marker's pre-pass
/// (`ConcurrentMarker::mark_rows_of_live_owners`).
///
/// Inside the initial-mark or remark pause, no heap lock held.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_conc_young_instance_loaders(
    shared: &SharedVm,
    tables: &cratonvm_gc::concurrent_mark::ClassUnloadTables,
    young_ids: &mut Option<(rustc_hash::FxHashSet<u32>, bool)>,
) -> Vec<*mut u8> {
    if !shared.mem.heap.is_generational() {
        return Vec::new();
    }
    let h = &shared.mem.heap;
    // gen r5w6/conc10: the walk's class ids, and whether it parsed ALL of
    // from-space. It re-anchors past an unparseable stretch, dropping the
    // objects inside it, so their classes' loaders would be judged by the
    // bitmap alone — a class unloaded under a live instance nobody saw. Then
    // every loader in the snapshot is a root (no unload this pause).
    let (class_ids, complete) = young_ids.get_or_insert_with(|| shared.mem.heap.gen_young_object_class_ids().unwrap());
    let mut loaders = gen_conc_loaders_of_class_ids(class_ids, tables);
    if !*complete {
        loaders.extend(tables.all_loaders().into_iter().map(|l| l as *mut u8));
    }
    loaders
}

/// gen r5w6/conc10 — the defining loaders (in this snapshot) of `class_ids`,
/// as mark roots: the tail of [`gen_conc_young_instance_loaders`], and the
/// whole of it under `CRATONVM_GEN_Y2O_LIVE_SEED`, where the ids are the LIVE
/// young objects' classes (`YoungToOldSeeds::class_ids`).
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_conc_loaders_of_class_ids(
    class_ids: &rustc_hash::FxHashSet<u32>,
    tables: &cratonvm_gc::concurrent_mark::ClassUnloadTables,
) -> Vec<*mut u8> {
    class_ids
        .iter()
        .filter_map(|&class_id| tables.loader_of(class_id))
        .map(|loader| loader as *mut u8)
        .collect()
}

/// gen r5w6/conc10 — one concurrent pause's young→old seeds under
/// `CRATONVM_GEN_Y2O_LIVE_SEED`, and the young `Reference`s whose slot 0 they
/// hid (the remark's safety net needs the list).
pub(in crate::runtime::interpreter::gc_and_alloc) struct GenConcY2o {
    pub(in crate::runtime::interpreter::gc_and_alloc) old_refs: Vec<usize>,
    pub(in crate::runtime::interpreter::gc_and_alloc) class_ids: rustc_hash::FxHashSet<u32>,
    /// `YoungToOldSeeds::young_walk_complete`: `class_ids` names every young
    /// object's class it should.
    pub(in crate::runtime::interpreter::gc_and_alloc) young_walk_complete: bool,
    pub(in crate::runtime::interpreter::gc_and_alloc) hidden_young_refs: Vec<usize>,
}

/// gen r5w6/conc10 — [`gen_conc_young_instance_loaders`] under
/// `CRATONVM_GEN_Y2O_LIVE_SEED`: the loaders of the seeding young objects'
/// classes (the LIVE ones on the live path), plus every loader in the snapshot
/// when the young walk could not enumerate all classes.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_conc_y2o_instance_loaders(
    seeds: &GenConcY2o,
    tables: &cratonvm_gc::concurrent_mark::ClassUnloadTables,
) -> Vec<*mut u8> {
    let mut loaders = gen_conc_loaders_of_class_ids(&seeds.class_ids, tables);
    if !seeds.young_walk_complete {
        loaders.extend(tables.all_loaders().into_iter().map(|l| l as *mut u8));
    }
    loaders
}

/// gen r5w6/conc10 — `CRATONVM_GEN_Y2O_LIVE_SEED`: the young→old seeds of a
/// generational concurrent pause from the LIVE young set
/// (`GenerationalHeap::collect_young_to_old_seeds`, which falls back to the
/// all-young enumeration on any doubt; `[GC] conc_y2o:` counts both).
///
/// `root_ptrs` is this pause's root list so far (the collected roots, the
/// thread snapshots, the frozen peers' conservative words). The two extra live
/// inputs are what the young collector would keep although no root names it:
/// every finalizable object ([`finalizable_roots`]: the next young collection
/// RESURRECTS a dead one and its `finalize()` reads its fields) and every JNI
/// weak-global referent (the one non-root table that can hand a young object
/// back to a mutator before the next young collection decides it).
///
/// `hide_young_referents`: the cycle processes references at its remark
/// (`refproc_on`); the active young weak / soft / phantom `Reference`s then do
/// not seed their OLD referents (the remark's phases decide them, and its
/// safety net keeps any it leaves set). Without it nothing is hidden: the
/// one-commit rule of `ConcurrentMarker::set_reference_skip`.
///
/// Inside a concurrent pause, no heap lock held. `None` off Generational.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_conc_y2o_seeds(
    shared: &SharedVm,
    root_ptrs: &[*mut u8],
    hide_young_referents: bool,
) -> Option<GenConcY2o> {
    if !shared.mem.heap.is_generational() {
        return None;
    }
    let h = &shared.mem.heap;
    let hidden_young_refs = if hide_young_referents {
        gen_conc_young_reference_skip_candidates(shared)
    } else {
        Vec::new()
    };
    let roots: Vec<usize> = root_ptrs.iter().map(|&p| p as usize).collect();
    let mut extra_live = finalizable_roots(shared);
    extra_live.extend(gen_conc_jni_weak_referents(shared));
    let seeds = h.gen_collect_young_to_old_seeds(&cratonvm_gc::gen_heap::YoungToOldSeedRequest {
        roots: &roots,
        extra_live: &extra_live,
        hidden_young_refs: &hidden_young_refs,
        live_only: true,
    }).unwrap();
    shared
        .mem
        .concurrent_gc_state
        .note_y2o_seeds(&seeds, hidden_young_refs.len());
    Some(GenConcY2o {
        old_refs: seeds.old_refs,
        class_ids: seeds.class_ids,
        young_walk_complete: seeds.young_walk_complete,
        hidden_young_refs,
    })
}

/// gcd d1/c (2026-09-27) — the YOUNG objects a concurrent remark's
/// dead-finalizer retention keeps alive and nothing strong reaches: the young
/// half of the "marked only for `finalize()`" set that
/// [`gen_remark_process_references`] hands the processor
/// (`docs/feature-designs/generative-gc-architecture.md`).
///
/// The retention closure (`retained`) holds only OLD objects: to the
/// concurrent cycle every young object is live, so a `WeakReference` to a young
/// object reachable only through a dead finalizable was kept, and `finalize()`
/// could hand the object back with the weak reference still naming it, where
/// HotSpot clears it first. This computes that young set with
/// `GenerationalHeap::young_reached_only_through`: the strong young set from
/// this pause's roots (`root_ptrs`) and the same extra live objects as the
/// seeds ([`gen_conc_y2o_seeds`]), with the old→young slot seed restricted to
/// old objects the remark marked that are NOT in `retained`; then what the
/// retained objects' young referents reach beyond it.
///
/// Every doubt keeps weak references (the behaviour before this pass): a
/// fallback of the live marking returns nothing, the strong set is an
/// over-approximation, and the answer never includes a strongly reachable
/// young object. Residual: an old object marked through a young object that
/// only a retained one reaches reads as strong, so what IT reaches is not
/// reported.
///
/// Inside the remark pause, after `remark_begin`, with no heap lock held.
/// Only under `CRATONVM_GEN_Y2O_LIVE_SEED` on a cycle that processes
/// references at its remark, and only when the retention found something.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_conc_young_behind_retained(
    shared: &SharedVm,
    root_ptrs: &[*mut u8],
    marker: &cratonvm_gc::ConcurrentMarker,
    retained: &[usize],
) -> Vec<usize> {
    if !shared.mem.heap.is_generational() {
        return Vec::new();
    }
    let h = &shared.mem.heap;
    let roots: Vec<usize> = root_ptrs.iter().map(|&p| p as usize).collect();
    let mut extra_live = finalizable_roots(shared);
    extra_live.extend(gen_conc_jni_weak_referents(shared));
    let retained_set: rustc_hash::FxHashSet<usize> = retained.iter().copied().collect();
    // Snapshot the immutable eligibility set once for this batch. Calling
    // remark_is_marked for every candidate would reacquire its mutex per
    // object during the stop-the-world young reachability walk.
    let is_marked = marker.remark_liveness();
    let strong_holder = |addr: usize| -> bool { is_marked(addr) && !retained_set.contains(&addr) };
    let found = h.gen_young_reached_only_through(&roots, &extra_live, &strong_holder, retained).unwrap_or(Ok(Vec::new()));
    shared
        .mem
        .concurrent_gc_state
        .note_y2o_finalizer_only_young(found.as_ref().ok().map(Vec::len));
    found.unwrap_or_default()
}

/// gen r5w6/conc10 — the YOUNG twin of [`gen_conc_reference_skip_candidates`]:
/// the processor's active weak / soft / phantom rows whose `Reference` object
/// is in the young generation, under the same screens (a `Reference`-shaped
/// non-array with at least two fields, and an identity stamp that still
/// matches), so a row whose object died and whose address was re-issued never
/// hides slot 0 of an unrelated object. Young membership is asked before the
/// class manager is read (it takes the young-from lock per row).
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_conc_young_reference_skip_candidates(shared: &SharedVm) -> Vec<usize> {
    let rows = shared
        .mem
        .ref_processor
        .lock()
        .active_reference_objects_with_stamps();
    if rows.is_empty() {
        return Vec::new();
    }
    let heap = &shared.mem.heap;
    let young: Vec<(usize, i32)> = rows
        .into_iter()
        .filter(|&(addr, _)| addr != 0 && addr & 7 == 0 && heap.is_in_young_addr(addr))
        .collect();
    if young.is_empty() {
        return Vec::new();
    }
    let class_manager = shared.classes.class_manager.read();
    let Some(reference_cid) = class_manager.find_bootstrap_class_by_name("java/lang/ref/Reference")
    else {
        return Vec::new();
    };
    young
        .into_iter()
        .filter(|&(addr, stamp)| {
            // SAFETY: an 8-aligned address inside young from- or to-space,
            // held by the processor; only its header is read below (a to-space
            // address reads a wiped header and fails the class screen).
            let obj = unsafe { ObjectRef::from_raw(addr as *mut u8) };
            heap.kind_of(obj) != crate::memory::heap::ObjectKind::Array
                && class_manager.is_subclass_of(heap.class_id_of(obj), reference_cid)
                && heap.num_fields(obj) >= 2
                && (stamp == 0 || {
                    let now = heap.identity_hash_code(obj);
                    now == 0 || now == stamp
                })
        })
        .map(|(addr, _)| addr)
        .collect()
}

/// gen r5w6/conc10 — the current referent address of every uncleared JNI
/// weak global ref. Read through [`crate::native::jni::clear_weak_global_refs_unmarked`]
/// with a predicate that records each address it is asked about and answers
/// "marked", so NOTHING is cleared: the JNI table offers this file no
/// read-only accessor (a cross-lane request on
/// `gengc-r5w6-conc10-young-to-old-seeding-keeps-dead-old-objects-FIXED-20260929.md`
/// asks for one). One table lock, no heap lock.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_conc_jni_weak_referents(shared: &SharedVm) -> Vec<usize> {
    let seen: std::cell::RefCell<Vec<usize>> = std::cell::RefCell::new(Vec::new());
    let record = |addr: usize| -> bool {
        seen.borrow_mut().push(addr);
        true
    };
    let cleared = crate::native::jni::clear_weak_global_refs_unmarked(shared, &record, &[]);
    debug_assert_eq!(
        cleared, 0,
        "a predicate that answers `marked` clears nothing"
    );
    seen.into_inner()
}

/// gen r5w6/conc10 — `CRATONVM_DBG_MIRRORPIN_WHY` on a class-unloading
/// concurrent pause: for every user loader some YOUNG object's class names,
/// how many young objects of its classes there are and (under
/// `CRATONVM_GEN_Y2O_LIVE_SEED`) whether their classes are in the live set —
/// the `young_instance_loaders` source of `[conc-unload-why]`, broken down.
/// One `[conc-unload-why] young-instances` line per loader. Diagnostic only:
/// one young walk, inside the pause, no heap lock held.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_conc_young_instances_why(
    shared: &SharedVm,
    tables: &cratonvm_gc::concurrent_mark::ClassUnloadTables,
    y2o: Option<&GenConcY2o>,
    pause: &str,
) {
    if !shared.mem.heap.is_generational() {
        return;
    }
    let h = &shared.mem.heap;
    let mut per_class: std::collections::BTreeMap<u32, usize> = std::collections::BTreeMap::new();
    for (ptr, _size) in h.gen_walk_young_objects().unwrap() {
        // SAFETY: as in `gen_conc_young_instance_loaders`.
        let header = unsafe { &*(ptr as *const cratonvm_types::ObjectHeader) };
        *per_class.entry(header.class_id.as_u32()).or_default() += 1;
    }
    let mut per_loader: std::collections::BTreeMap<usize, Vec<(u32, usize)>> =
        std::collections::BTreeMap::new();
    for (class_id, count) in per_class {
        if let Some(loader) = tables.loader_of(class_id) {
            per_loader
                .entry(loader)
                .or_default()
                .push((class_id, count));
        }
    }
    for (loader, classes) in per_loader {
        let listed: Vec<String> = classes
            .iter()
            .map(|&(class_id, count)| {
                let live = match y2o {
                    Some(seeds) => {
                        if seeds.class_ids.contains(&class_id) {
                            "live"
                        } else {
                            "dead"
                        }
                    }
                    None => "all-young",
                };
                format!("{class_id}x{count}:{live}")
            })
            .collect();
        eprintln!(
            "[conc-unload-why] pause={pause} young-instances loader={loader:#x} classes=[{}]",
            listed.join(", ")
        );
    }
}

/// gen r5w4/conc8 — the retained-layout census candidates of this cycle: the
/// pending retained class ids (`ConcurrentGcState::retained_layouts_pending`)
/// of which the YOUNG generation holds no object, live or dead. Inside the
/// initial-mark pause, no heap lock held. A class id stays pending while any
/// young object of it exists, so its layout outlives every young walker that
/// could still size one; none can appear later (the class is out of the
/// store). The old half of the census is the sweep's
/// (`ConcurrentMarker::set_layout_census`).
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_conc_layout_census_candidates(
    shared: &SharedVm,
    young_ids: &mut Option<(rustc_hash::FxHashSet<u32>, bool)>,
) -> Vec<u32> {
    let pending = shared.mem.concurrent_gc_state.retained_layouts_pending();
    if pending.is_empty() {
        return pending;
    }
    if !shared.mem.heap.is_generational() {
        return Vec::new();
    }
    let h = &shared.mem.heap;
    // gen r5w6/conc10: a walk that stepped over an unparseable stretch did not
    // see every young object, so "no young instance of this class" is not
    // proved for any pending id: no candidate this cycle (release deferred).
    let (young_class_ids, complete) = young_ids.get_or_insert_with(|| shared.mem.heap.gen_young_object_class_ids().unwrap());
    if !*complete {
        return Vec::new();
    }
    pending
        .into_iter()
        .filter(|id| !young_class_ids.contains(id))
        .collect()
}

/// gcd d2/g (`gengc-r5w4-conc8-proposal-stw-majors-take-the-retained-layout-census`)
/// — the retained-layout census after a STOP-THE-WORLD collection. Inside the
/// pause (`process_references_after_gc`), no heap lock held.
///
/// The concurrent census releases a layout a remark retained only after a
/// LATER concurrent cycle proves no instance is left; a program whose later old
/// collections are all stop-the-world majors never ran one, so the layouts
/// stayed for the life of the VM. Here, after a collection that reclaimed
/// old-generation storage (`old_gen_reclaimed_last_cycle`), both generations
/// are walked with the world stopped and a pending class id of which neither
/// holds any object, live or dead, is released. The census is of what the
/// collection LEFT, so it needs no reachability argument: a dead instance a
/// conservative root kept is still an object, and keeps its layout. Either
/// walk incomplete (an unparseable stretch or region) takes no census.
///
/// The release takes the class manager's write lock inside the pause, as the
/// unload transaction just above it does on the same path.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_stw_layout_census(shared: &SharedVm) {
    let state = &shared.mem.concurrent_gc_state;
    if !cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle() || !state.has_retained_layouts()
    {
        return;
    }
    if !shared.mem.heap.is_generational() {
        return;
    }
    let h = &shared.mem.heap;
    let (mut survivors, young_complete) = shared.mem.heap.gen_young_object_class_ids().unwrap();
    if !young_complete {
        return;
    }
    let (old_ids, old_complete) = shared.mem.heap.gen_old_object_class_ids().unwrap();
    if !old_complete {
        return;
    }
    survivors.extend(old_ids);
    if state.complete_stw_layout_census(&survivors) == 0 {
        return;
    }
    let releasable = state.take_releasable_layouts();
    let released = crate::memory::gc::release_retained_layouts(shared, &releasable);
    state.note_layouts_released(released);
}

/// The G1 half of the retained-layout census: the layouts
/// [`g1_remark_process_references`] retained stay registered while any
/// instance of the class, live or dead, is still in a region. After a G1 pause
/// that could have freed one ([`G1Collector::layout_census_due`]: the first
/// pause after a retention, then each pause that handed an old region back),
/// every non-free region is walked and each pending id no object has is
/// released, under the class manager's write lock as the generational census
/// does. A walk that could not size some object takes no census (the layout
/// stays: a leak, never an unsizable object). Inside the pause, no heap lock
/// held. One leaf-mutex read per pause otherwise.
///
/// `docs/feature-designs/concurrent-gc-maturation.md`.
pub(in crate::runtime::interpreter::gc_and_alloc) fn g1_stw_layout_census(shared: &SharedVm) {
    if !shared.mem.heap.is_g1() {
        return;
    }
    let g1 = &shared.mem.heap;
    let state = &shared.mem.concurrent_gc_state;
    if !state.has_retained_layouts() {
        return;
    }
    if !g1.g1_layout_census_due(state.census().class_unload_layouts_retained) {
        return;
    }
    let (survivors, complete) = g1.g1_object_class_ids().unwrap();
    if !complete || state.complete_stw_layout_census(&survivors) == 0 {
        return;
    }
    let releasable = state.take_releasable_layouts();
    let released = crate::memory::gc::release_retained_layouts(shared, &releasable);
    state.note_layouts_released(released);
}

/// `[GC] unload_layouts:` -- the retained-layout ledger of the class-unload
/// transactions whose dead instances outlive them (a G1 final remark, a
/// generational concurrent remark), every collector, on the GC-stats summary.
/// `pending` is what is still registered for a walker to size a dead
/// instance; `g1_old_regions_reclaimed` / `g1_census_incomplete` say how
/// often the G1 census could run and how often its walk could not finish.
pub fn unload_layouts_line(shared: &SharedVm) -> String {
    let state = &shared.mem.concurrent_gc_state;
    let c = state.census();
    let (reclaimed, incomplete) = match &shared.mem.heap {
        _ => shared.mem.heap.g1_layout_census_counts(),
    };
    format!(
        "[GC] unload_layouts: classes_unloaded={} layouts_retained={} layouts_released={} pending={} \
         stw_censuses={} g1_old_regions_reclaimed={reclaimed} g1_census_incomplete={incomplete}",
        c.class_unload_classes,
        c.class_unload_layouts_retained,
        c.class_unload_layouts_released,
        state.retained_layouts_pending().len(),
        c.class_unload_stw_layout_censuses,
    )
}

/// gen r5w3/unload7 — the class-metadata reconcile and unload transaction of a
/// generational concurrent remark: the same four calls
/// [`g1_remark_process_references`] makes, with the unloaded classes' compact
/// layouts RETAINED (`memory::gc::with_retained_unloaded_layouts`), because
/// this cycle's dead instances are freed by the concurrent sweep AFTER the
/// transaction, and the sweep's walk sizes them from that layout.
pub(in crate::runtime::interpreter::gc_and_alloc) fn remark_unload_dead_classes(
    shared: &SharedVm,
    is_marked: &dyn Fn(usize) -> bool,
) -> crate::memory::gc::ClassMetadataUnloadResult {
    crate::memory::gc::reconcile_class_mirrors(shared, is_marked, None);
    let no_moves = cratonvm_types::PointerMap::default();
    let dead_class_hints = cratonvm_native_builtins::classloader::gc_reconcile_defining_loaders(
        &shared.loader_identity,
        is_marked,
        &no_moves,
        None,
    );
    crate::runtime::invokedynamic::repin_lambda_proxy_loaders(shared.vm_identity);
    let unloaded = crate::memory::gc::with_retained_unloaded_layouts(|| {
        crate::memory::gc::unload_dead_class_metadata(shared, &dead_class_hints)
    });
    // gcd d10/r: the `mirror_pin` registry follows the pruned tables here too
    // (see `g1_remark_process_references`): the concurrent sweep is about to
    // free the dead loaders and mirrors this remark judged, and their rows
    // must not outlive them until the next stop-the-world epilogue.
    crate::memory::gc::rebuild_mirror_pins(shared, &no_moves);
    unloaded
}

/// gen r5w3/unload7 — the class unload of a generational concurrent remark
/// that ran with the side tables armed: AFTER `remark_finish` (the bitmap is
/// final: the keep closure and the last SATB drain are in it), still inside
/// the pause, with the old-gen guard released. The verdict is the remark's
/// (`ConcurrentMarker::remark_is_marked`: marked, or not sweep-eligible), so
/// only a loader that existed at the initial mark and that nothing — root,
/// field, young instance, side-table edge — reached is judged dead, and the
/// sweep then frees what the transaction dropped.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_remark_unload_classes(shared: &SharedVm, marker: &cratonvm_gc::ConcurrentMarker) {
    let is_marked = marker.remark_liveness();
    let unloaded = remark_unload_dead_classes(shared, &is_marked);
    let state = &shared.mem.concurrent_gc_state;
    state.note_class_unload_remark();
    state.note_class_unload(
        unloaded.loaders_unloaded,
        unloaded.classes_unloaded,
        unloaded.layouts_retained,
    );
    if unloaded.classes_unloaded != 0 {
        tracing::debug!(
            loaders = unloaded.loaders_unloaded,
            classes = unloaded.classes_unloaded,
            jit_entries = unloaded.jit_entries_retired,
            layouts_retained = unloaded.layouts_retained,
            "generational concurrent remark unloaded class-loader metadata"
        );
    }
}

/// gen r5w5/conc9 — `CRATONVM_DBG_MIRRORPIN_WHY` (`CRATONVM_DBG=mirrorpin-why`)
/// on a class-unloading generational concurrent cycle. Read per call, not
/// cached in a `static` (the per-VM statics ratchet): it is asked at most
/// twice per class-unloading cycle, as `collect_roots` asks it per scan.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_conc_unload_why_enabled() -> bool {
    false
}

/// gen r5w5/conc9 — why a user loader is still live at a class-unloading
/// concurrent pause (`gengc-r5w1-refs5-concurrent-cycle-cannot-unload-classes`:
/// the orchestrator's run showed `concunload_remarks=1 concunload_classes=0`,
/// i.e. the remark ran and judged every loader live, and nothing in the
/// census says which of five root sources, or which side-table row, did it).
///
/// One stderr line per `mirror_pin` owner (the user loaders that minted a
/// mirror; at most 64), naming for the loader and each of its mirrors:
/// whether it is in the old generation, whether this cycle's bitmap marks it,
/// `live_or_ineligible` (the remark's verdict, `remark_is_marked`: an address
/// that is not an old-gen object start of the initial-mark snapshot — a YOUNG
/// object, one promoted since — is live whatever the bitmap says), and which of
/// this pause's root `sources` named it DIRECTLY (`-` for none: then it was
/// reached by the trace, or it is ineligible). The mirror's class name is
/// printed so a probe's class can be found. Diagnostic only: inside the pause,
/// no heap lock held, the class-mirror and class-manager read locks as
/// `collect_roots` takes them.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_conc_unload_why(
    shared: &SharedVm,
    marker: &cratonvm_gc::ConcurrentMarker,
    pause: &str,
    sources: &[(&str, &[*mut u8])],
) {
    let Some(rows) = cratonvm_types::mirror_pin::snapshot() else {
        eprintln!("[conc-unload-why] pause={pause} no mirror_pin rows");
        return;
    };
    let rows: Vec<(usize, Vec<usize>)> = rows.into_iter().take(64).collect();
    let mut targets: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    for (loader, mirrors) in &rows {
        targets.insert(*loader);
        targets.extend(mirrors.iter().copied());
    }
    let mut hits: rustc_hash::FxHashMap<usize, Vec<&str>> = rustc_hash::FxHashMap::default();
    for &(name, ptrs) in sources {
        for &p in ptrs {
            let addr = p as usize;
            if targets.contains(&addr) {
                let named = hits.entry(addr).or_default();
                if !named.contains(&name) {
                    named.push(name);
                }
            }
        }
    }
    let mirror_classes: rustc_hash::FxHashMap<usize, ClassId> = shared
        .classes
        .class_mirrors
        .read()
        .iter()
        .filter(|(_, m)| targets.contains(&(m.as_ptr() as usize)))
        .map(|(id, m)| (m.as_ptr() as usize, *id))
        .collect();
    let (old_base, old_size) = shared.mem.heap.old_gen_info();
    let old_end = old_base.saturating_add(old_size);
    let class_manager = shared.classes.class_manager.read();
    let describe = |addr: usize| -> String {
        let class = mirror_classes
            .get(&addr)
            .and_then(|id| class_manager.get_class(*id))
            .map(|c| format!(" class={}", c.name))
            .unwrap_or_default();
        format!(
            "{addr:#x}(old={} marked={} live_or_ineligible={} roots={}{class})",
            addr >= old_base && addr < old_end,
            marker.bitmap.is_marked(addr),
            marker.remark_is_marked(addr),
            hits.get(&addr)
                .map_or_else(|| "-".to_string(), |v| v.join("+")),
        )
    };
    for (loader, mirrors) in &rows {
        let mirrors: Vec<String> = mirrors.iter().map(|&m| describe(m)).collect();
        eprintln!(
            "[conc-unload-why] pause={pause} loader={} mirrors=[{}]",
            describe(*loader),
            mirrors.join(", ")
        );
    }
}

/// gen r5w3/unload7 — the door census's name for the door that called the
/// generational driver. The service thread calls it with `MaybeGc` (the G1
/// token set has no service), so it is told apart by thread identity.
pub(in crate::runtime::interpreter::gc_and_alloc) fn gen_conc_door(shared: &SharedVm, door: MarkDoor) -> cratonvm_gc::concurrent_mark::GenConcDoor {
    use cratonvm_gc::concurrent_mark::GenConcDoor;
    if shared.mem.concurrent_gc_state.on_service_thread() {
        return GenConcDoor::Service;
    }
    match door {
        MarkDoor::MaybeGc => GenConcDoor::MaybeGc,
        MarkDoor::AllocFail => GenConcDoor::AllocFail,
        MarkDoor::SystemGc => GenConcDoor::SystemGc,
        MarkDoor::JitDriver | MarkDoor::ForceFull => GenConcDoor::Other,
    }
}

