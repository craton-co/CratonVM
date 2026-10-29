// ---------------------------------------------------------------------------

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to a byte/boolean array object. Null triggers a pending NPE + `i64::MIN`
// deopt sentinel; out-of-bounds is handled gracefully by the bounds check below.
pub unsafe extern "C" fn jit_baload(array_ptr: i64, index: i64) -> i64 {
    let f = || jit_baload_body(array_ptr, index);
    contain("jit_baload", OnPanic::Deopt, i64::MIN, f)
}

/// Body of [`jit_baload`], run under its panic guard.
unsafe fn jit_baload_body(array_ptr: i64, index: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) array
    // reference identically: take the NPE path instead of dereferencing it
    // (the length/element read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // arrays always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(array_ptr as u64) {
        // JVMS §baload: throw NullPointerException on null array reference.
        // Previously returned 0, which silently fabricated a zero byte and
        // masked real null-deref bugs in user code. Match the iaload/aaload
        // protocol: flag the pending NPE and return the deopt sentinel so the
        // post-JIT interpreter path throws on resume.
        set_jit_pending_npe_action(crate::runtime::exceptions::helpful_npe::jit_action::ALOAD_BYTE);
        return i64::MIN;
    }
    // SAFETY: array_ptr is non-null and points to a live array object on the GC heap.
    // ARRAY_LENGTH_OFFSET is the fixed offset to the length field in the array header.
    let ptr = array_ptr as *const u8;
    let length = *(ptr.add(ARRAY_LENGTH_OFFSET) as *const u32) as i64;
    if index < 0 || index >= length {
        // JVMS §baload: throw ArrayIndexOutOfBoundsException on an out-of-bounds
        // index. Previously returned 0, which silently fabricated a zero byte and
        // masked real OOB bugs in user code — the same silent-fabrication class the
        // null arm above was fixed for. Mirror `jit_throw_aioobe`: flag the pending
        // AIOOBE and return the `i64::MIN` deopt sentinel so the post-JIT interpreter
        // path constructs the real exception and routes it through the method's
        // exception table.
        JIT_SIGNALS.with(|s| s.aioobe.set(Some((index, length))));
        // The frames this bounds check fired in are about to leave: the helper
        // returns and the compiled body runs its epilogue, so the
        // `ArrayIndexOutOfBoundsException` is built later, from the interpreter,
        // on a stack that no longer has them. Snapshot here, the one moment the
        // signal and the frames both exist -- the same rung the NPE and
        // div-by-zero setters stand on. `0` for the trap key: an array opcode's
        // bci is one the frame walk already has, unlike an inline null check's.
        snapshot_trap_frames(0);
        return i64::MIN;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize);
    *elem_ptr as i8 as i64
}

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to a byte/boolean array object. Null aborts the process — see comment.
// Out-of-bounds is handled gracefully.
//
// JEP 358 inline-NPE-path update (2026-06): the inline null-check failure stub
// in `jit/src/x64.rs::emit_null_check_store_stubs` NO LONGER calls this helper —
// it now calls the dedicated `jit_npe_with_action(code)` so it can attach the
// correct per-element-type JEP-358 action (the old shared stub called
// `bastore(0)` and therefore fabricated `ASTORE_BYTE` for every array opcode).
// The `array_ptr == 0` null guard below is retained as honest defense-in-depth
// for any *direct* compiled call to this helper (it stays registered in
// `JitRuntimeHelpers` for ABI stability), and still must not read `index`/`val`
// on the null path. The fragile "zeroed-by-happenstance argument register" ABI
// coupling the previous comment described is gone with the shared-stub call.
pub unsafe extern "C" fn jit_bastore(array_ptr: i64, index: i64, val: i64) {
    let f = || jit_bastore_body(array_ptr, index, val);
    contain("jit_bastore", OnPanic::Deopt, (), f)
}

/// Body of [`jit_bastore`], run under its panic guard.
// SAFETY: Called under `contain` panic guard with JIT caller arguments. Checks
// pointer plausibility and bounds before performing the raw memory store.
unsafe fn jit_bastore_body(array_ptr: i64, index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) array
    // reference identically: take the NPE path instead of dereferencing it
    // (the length/element read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // arrays always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(array_ptr as u64) {
        // JVMS §bastore: throw NullPointerException on null array reference.
        //
        // Round-8 CRIT fix (audit `round8-jit.md`, "false promise" item):
        // previously this called `std::process::abort()` with a comment
        // claiming the helper was unreachable from inlined codegen, but
        // (a) the helper is still registered in `JitRuntimeHelpers` and
        // therefore reachable from any future codegen path that uses it,
        // and (b) the hardware-page-fault NPE path through
        // `emit_bounds_check` is *also* a false promise — the signal
        // handler dumps an hs_err and re-raises, killing the VM. We now
        // set the pending-NPE flag and return; the void return cannot
        // carry a sentinel, but the interpreter's post-JIT path drains
        // `JIT_PENDING_NPE` on EVERY return (not just the i64::MIN
        // sentinel arm — fixed in the same round) so the NPE surfaces
        // at the right method instead of leaking across calls. The
        // inline-store codegen also emits an explicit `TEST receiver,
        // receiver; JZ deopt_npe` guard before the bounds check, so
        // this helper is the second line of defense.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::ASTORE_BYTE,
        );
        // Out-of-band deopt signal: the x64 `emit_null_check_store_stubs` stub
        // calls this helper with `array_ptr == 0` and then loads `i64::MIN` as
        // the method's return value, so flag the deopt to keep the interpreter
        // from mistaking a legitimate `Long.MIN_VALUE` return for one. (The
        // interpreter's NPE drain runs first and clears it; this preserves the
        // "every `i64::MIN` method-return sets the flag" invariant regardless.)
        set_jit_deopt_pending();
        return;
    }
    // SAFETY: array_ptr is non-null and points to a live array object on the GC heap.
    let ptr = array_ptr as *mut u8;
    let length = *(ptr.add(ARRAY_LENGTH_OFFSET) as *const u32) as i64;
    if index < 0 || index >= length {
        // JVMS §bastore: throw ArrayIndexOutOfBoundsException on an out-of-bounds
        // index. Previously the store was silently dropped, masking real OOB bugs.
        // The void return cannot carry the `i64::MIN` deopt sentinel, so — exactly
        // like the void-return null arm above — we set the pending-AIOOBE flag and
        // return; the interpreter's post-JIT drain surfaces the exception at this
        // method (see `take_jit_pending_aioobe` in runtime/interpreter.rs).
        JIT_SIGNALS.with(|s| s.aioobe.set(Some((index, length))));
        // The frames this bounds check fired in are about to leave: the helper
        // returns and the compiled body runs its epilogue, so the
        // `ArrayIndexOutOfBoundsException` is built later, from the interpreter,
        // on a stack that no longer has them. Snapshot here, the one moment the
        // signal and the frames both exist -- the same rung the NPE and
        // div-by-zero setters stand on. `0` for the trap key: an array opcode's
        // bci is one the frame walk already has, unlike an inline null check's.
        snapshot_trap_frames(0);
        return;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize);
    *elem_ptr = val as u8;
    // The host just wrote this array, so a GPU input-cache entry
    // mirroring it is stale. `jit_iastore` has carried this since Phase
    // 10 #2; this helper did not, because `byte[]` could not be
    // marshalled and so was never cached. It became cacheable on
    // 2026-09-02 and this line landed with the same change.
    #[cfg(feature = "gpu-offload")]
    crate::runtime::offload::input_cache::invalidate(cratonvm_types::ObjectRef::from_raw(ptr));
}

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to an int array object. Null triggers a pending NPE + `i64::MIN` deopt
// sentinel; out-of-bounds is handled gracefully by the bounds check below.
pub unsafe extern "C" fn jit_iaload(array_ptr: i64, index: i64) -> i64 {
    contain("jit_iaload", OnPanic::Deopt, i64::MIN, || {
        jit_iaload_body(array_ptr, index)
    })
}

/// Body of [`jit_iaload`]. Panic-guarded via [`contain`] with [`OnPanic::Deopt`].
unsafe fn jit_iaload_body(array_ptr: i64, index: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) array
    // reference identically: take the NPE path instead of dereferencing it
    // (the length/element read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // arrays always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(array_ptr as u64) {
        // JVMS §iaload: throw NullPointerException on null array reference.
        // Signal the interpreter via the pending-NPE flag + `i64::MIN` deopt
        // sentinel (same protocol as `jit_throw_aioobe`).
        set_jit_pending_npe_action(crate::runtime::exceptions::helpful_npe::jit_action::ALOAD_INT);
        return i64::MIN;
    }
    // SAFETY: array_ptr is non-null and points to a live int[] on the GC heap.
    // The element at HEADER_SIZE + index*4 is within bounds (checked below).
    let ptr = array_ptr as *const u8;
    let length = *(ptr.add(ARRAY_LENGTH_OFFSET) as *const u32) as i64;
    if index < 0 || index >= length {
        // JVMS §iaload: throw ArrayIndexOutOfBoundsException on an out-of-bounds
        // index (same protocol as `jit_throw_aioobe`). Previously returned 0,
        // silently fabricating a zero element and masking real OOB bugs.
        JIT_SIGNALS.with(|s| s.aioobe.set(Some((index, length))));
        // The frames this bounds check fired in are about to leave: the helper
        // returns and the compiled body runs its epilogue, so the
        // `ArrayIndexOutOfBoundsException` is built later, from the interpreter,
        // on a stack that no longer has them. Snapshot here, the one moment the
        // signal and the frames both exist -- the same rung the NPE and
        // div-by-zero setters stand on. `0` for the trap key: an array opcode's
        // bci is one the frame walk already has, unlike an inline null check's.
        snapshot_trap_frames(0);
        return i64::MIN;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize * 4) as *const i32;
    *elem_ptr as i64
}

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to an int array object. Null aborts the process — see `jit_bastore` for
// the rationale. Out-of-bounds is handled gracefully.
pub unsafe extern "C" fn jit_iastore(array_ptr: i64, index: i64, val: i64) {
    contain("jit_iastore", OnPanic::Deopt, (), || {
        jit_iastore_body(array_ptr, index, val)
    });
}

/// Body of [`jit_iastore`]. Panic-guarded via [`contain`] with [`OnPanic::Deopt`].
unsafe fn jit_iastore_body(array_ptr: i64, index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) array
    // reference identically: take the NPE path instead of dereferencing it
    // (the length/element read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // arrays always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(array_ptr as u64) {
        // JVMS §iastore: throw NullPointerException on null array reference.
        // Round-8 CRIT fix: see `jit_bastore` for full rationale. Set the
        // pending-NPE flag; the interpreter's post-JIT path now drains it
        // on every return, so the void-return sentinel-less channel is
        // no longer a correctness blocker.
        set_jit_pending_npe_action(crate::runtime::exceptions::helpful_npe::jit_action::ASTORE_INT);
        return;
    }
    // SAFETY: array_ptr is non-null and points to a live int[] on the GC heap.
    let ptr = array_ptr as *mut u8;
    let length = *(ptr.add(ARRAY_LENGTH_OFFSET) as *const u32) as i64;
    if index < 0 || index >= length {
        // JVMS §iastore: throw ArrayIndexOutOfBoundsException on an out-of-bounds
        // index. Previously the store was silently dropped. Set the pending-AIOOBE
        // flag and return — the void return cannot carry the deopt sentinel, so the
        // interpreter's post-JIT drain surfaces the exception (same void-arm protocol
        // as the null case above).
        JIT_SIGNALS.with(|s| s.aioobe.set(Some((index, length))));
        // The frames this bounds check fired in are about to leave: the helper
        // returns and the compiled body runs its epilogue, so the
        // `ArrayIndexOutOfBoundsException` is built later, from the interpreter,
        // on a stack that no longer has them. Snapshot here, the one moment the
        // signal and the frames both exist -- the same rung the NPE and
        // div-by-zero setters stand on. `0` for the trap key: an array opcode's
        // bci is one the frame walk already has, unlike an inline null check's.
        snapshot_trap_frames(0);
        return;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize * 4) as *mut i32;
    *elem_ptr = val as i32;
    // Phase 10 #2: the host just wrote this array, so a GPU input-cache
    // entry mirroring it is stale. Costs one relaxed load when nothing is
    // cached, which is every run that never submits a kernel.
    #[cfg(feature = "gpu-offload")]
    crate::runtime::offload::input_cache::invalidate(cratonvm_types::ObjectRef::from_raw(ptr));
}

// ─── Reference-word decode chokepoint for the JIT read helpers ───────────────
//
// Six reference-returning read arms in this file (`jit_aaload`'s element read,
// `jit_getfield`'s three arms, `jit_getstatic`'s two) ended commit 6a04b0e3c1
// (2026-06-29, "degrade stale references to null at every decode boundary")
// carrying the same three lines verbatim:
//
//     if plausible_heap_pointer(raw) { raw as i64 } else { 0 }
//
// That commit's own message records what they are: *defense-in-depth for an
// unfixed GC defect* — "the underlying GC root-coverage gap (live blocked-thread
// frame objects swept by the non-moving young sweep) remains and is the only
// complete fix". They are a heuristic compensating for a known-live bug, not an
// integrity guard on trusted data, and that distinction is why the shape below
// is not just a de-duplication.
//
// WHY THIS FUNCTION EXISTS — two failure modes the copy-pasted idiom hides:
//
// (1) The JIT-side degrade is SILENT, and its interpreter twin is not. The same
//     commit put the same filter on the interpreter's decode path, where it
//     feeds `cratonvm_types::compact_value`'s process-wide degradation counter
//     and a one-shot stderr line (`note_object_degradation`, types/src/
//     compact_value.rs:330). These six JIT arms feed nothing. So
//     `object_degradation_count()` reads 0 while compiled code is quietly
//     handing Java `null` for live objects — the counter is not merely
//     incomplete, it actively reports "clean" for the configuration where the
//     root-coverage gap is *most* likely to bite (JIT'd frames are exactly the
//     ones the deposited root snapshot misses). A root-coverage regression that
//     only manifests under JIT is invisible to the instrument built to catch it.
//     `jit_decode_ref_word` closes that, at the cost of one relaxed increment on
//     a path that is already a broken state. This is a bug today, independent of
//     ZGC.
//
//     DISCHARGED: this used to carry a `TODO(zgc)/TODO(gc)` saying
//     `note_object_degradation` was `pub(crate)` and so the JIT had to keep a
//     private counter, giving two numbers where a triager needed one. The sink
//     is now `pub` and source-tagged (`DegradationSource::{Interpreter, Jit,
//     ArrayElement}`), and this file's counter has been folded into its `Jit`
//     slot — see `jit_ref_degradation_count`. `object_degradation_count()` now
//     totals every degrading path in the tree, and
//     `object_degradation_breakdown()` still names which one fired.
//
// (2) A ZGC colored word is NOT corruption, and must never take the degrade.
//     `gc/src/zgc/vaddr.rs` sets bit 63 (`Z_COLORED_TAG`) on every non-null
//     colored word *precisely so* it fails `plausible_heap_pointer`'s 47-bit
//     test and is caught loudly rather than dereferenced as a wild pointer. The
//     idiom above does the exact opposite of loud: it converts "the load barrier
//     has not run on this word" into a null reference handed to Java, i.e. a
//     NullPointerException or a silently dropped store at an arbitrary point far
//     from the cause. Under a relocating collector that is silent heap
//     corruption, which the JIT-load-barrier study (docs/feature-designs/
//     zgc-jit-load-barrier.md, risk J1) rates worse than a clean SIGSEGV.
//
//     The fix is NOT to weaken `plausible_heap_pointer` — the study says so
//     explicitly — it is to make the caller barrier the word first. Until that
//     lands (stage (a)), the colored-word case is an invariant violation and
//     fails loudly instead of fabricating a null.

/// Read the running count of stale references the JIT read helpers degraded to
/// `null` because they failed [`cratonvm_types::plausible_heap_pointer`].
///
/// A non-zero value means compiled code has been handed `null` for a slot that
/// held bits — i.e. the GC root-coverage gap recorded in commit `6a04b0e3c1` is
/// live in this run. Zero is the only good value.
///
/// # This is one slot of the shared counter, not a private one
///
/// It used to be a `static JIT_REF_DEGRADATIONS: AtomicU64` in this file,
/// because `cratonvm_types::compact_value::note_object_degradation` was
/// `pub(crate)` and this crate could not reach it — see the `TODO` this
/// discharges in the block comment above. Two counters where there should be
/// one is not merely untidy here: it made
/// `cratonvm_types::compact_value::object_degradation_count()` read a
/// reassuring `0` for **precisely** the configuration in which the underlying
/// root-coverage gap is most likely to fire, because a JIT'd frame is the frame
/// a deposited root snapshot misses. A triager had to know to read two numbers,
/// and zero on one was not zero overall.
///
/// The sink is now `pub` and source-tagged, so this reads
/// [`cratonvm_types::compact_value::DegradationSource::Jit`]'s slot of the one
/// process-wide table. `object_degradation_count()` genuinely totals, and
/// `object_degradation_breakdown()` still separates this source out — which
/// matters, because a `Jit` count carries no long↔object ambiguity (the word
/// came from a slot the JVM type system says is a reference) while an
/// `Interpreter` count can be a benign collision.
/// `cratonvm_gc::heap::ref_element_degradation_count` (the `ArrayElement` slot)
/// was merged the same way.
///
/// Advisory and `Relaxed`: it carries no happens-before relationship with the
/// slot it counts.
pub fn jit_ref_degradation_count() -> u64 {
    cratonvm_types::compact_value::object_degradation_count_from(
        cratonvm_types::compact_value::DegradationSource::Jit,
    )
}

/// Decode a raw reference word read out of a heap slot into the `i64` the JIT
/// expects, degrading a provably-impossible pointer to `0` (null).
///
/// The fast path is byte-for-byte the predicate the six call sites used before:
/// `plausible_heap_pointer(raw)` and nothing else. Everything new lives in the
/// `#[cold]`, `#[inline(never)]` callee, which is only reached once that
/// predicate has *already* failed — so this costs nothing per field access on
/// any build or any collector. See the block comment above for why.
///
/// `site` is only materialised inside the cold callee.
#[inline(always)]
fn jit_decode_ref_word(raw: u64, site: &'static str) -> i64 {
    if cratonvm_types::plausible_heap_pointer(raw) {
        raw as i64
    } else {
        jit_ref_word_implausible(raw, site)
    }
}

/// Cold arm of [`jit_decode_ref_word`]: the word cannot be a live heap pointer.
///
/// Distinguishes the two reasons a word can land here, which the previous
/// `else { 0 }` conflated:
///
/// * A **ZGC colored word** — legitimate data that simply has not been through
///   the load barrier. Returning `0` for it is the silent-null corruption of
///   §2.5 / J1 in `docs/feature-designs/zgc-jit-load-barrier.md`; returning the
///   colored word itself is a wild-pointer deref in compiled code. Neither is
///   acceptable, so this is a hard failure that names the missing barrier.
/// * **Stale/garbage bits** (the `0x8D8D..` class from commit `6a04b0e3c1`) —
///   genuinely not a pointer. Keeps the existing degrade-to-null contract, now
///   counted.
///
/// The ZGC arm is `#[cfg(feature = "zgc")]`, so a default build is unaffected —
/// not merely equivalent, but not compiled. It is additionally guarded by
/// `is_well_formed`, which requires bits 62-46 clear and exactly one metadata
/// bit set: a stale `0x8D8D8D8D8D8D8D8D` has bit 63 set but fails that, so a
/// `--features zgc` build running Generational or G1 keeps the old degrade for
/// real garbage. This bit-pattern test is used instead of "is ZGC the selected
/// backend?" deliberately: `cratonvm_gc::vm_heap::VmHeap` exposes no backend
/// accessor, and three of the five sites that reach this arm (`jit_getfield`'s
/// legacy 16-byte `Value` cell and `jit_getstatic`'s two) no longer hold the
/// SLOT by the time they get here, so they cannot dispatch through
/// [`jit_load_ref_slot`] at all -- see the report accompanying this change.
///
/// The clause that used to lead that sentence -- "`jit_aaload` receives no
/// `vm_ptr` at all" -- stopped being true on 2026-09-01, when that helper
/// gained a leading `vm_ptr` and site A became barriered. It is named here
/// rather than silently deleted, because a rationale that has quietly stopped
/// holding misdirects the next reader more than no rationale would.
#[cold]
#[inline(never)]
fn jit_ref_word_implausible(raw: u64, site: &'static str) -> i64 {
    // `plausible_heap_pointer(0)` is FALSE (it requires `raw >= 0x1000`), so an
    // ordinary null reference lands here on the way to returning 0. Counting it
    // would put the counter in the millions on a perfectly clean run, falsifying
    // its own contract that "zero is the only good value". A null is not a
    // degradation: no reference was lost, because there was none to lose.
    //
    // That early-out used to be written here. It now lives inside
    // `note_ref_word_degradation` below — this arm and
    // `cratonvm_gc::heap::ref_element_word_implausible` each had to discover the
    // rule independently, so it is stated once in the shared sink where the next
    // raw-word path cannot miss it. Nothing below fires on a null: bit 63 of `0`
    // is clear, so `is_colored_word(0)` is false.
    #[cfg(feature = "zgc")]
    {
        // TODO(zgc) -- RESTATED 2026-09-01 after following the plumbing the
        // previous wording named. Two of its premises did not survive reading
        // the source, so they are corrected here rather than left to be
        // rediscovered:
        //
        //  * "through the existing `VmHeap::load_and_forward` chokepoint" does
        //    not work. That function (gc/src/vm_heap.rs, `pub fn
        //    load_and_forward`) genuinely does dispatch on the backend, and its
        //    `Zgc` arm genuinely does work -- `forwarded_after_slide`, the
        //    relocation-table lookup that repairs a reference the compacting
        //    slide moved. But its parameter is an `ObjectRef`: a MACHINE
        //    POINTER the caller has already fabricated, and a colored word is
        //    not one. Handed a colored word it would build an `ObjectRef` out
        //    of bit-63 bits, fail `is_object_address`, and hand the SAME word
        //    back unchanged -- a silent no-op that additionally violates
        //    `cratonvm_gc::zgc::vaddr::debug_assert_plain_word`. It is the
        //    chokepoint for a DIFFERENT hazard (post-slide forwarding of a
        //    plain address), not for colored-word decoding.
        //  * A barrier cannot be taken at THIS point under any design.
        //    `cratonvm_gc::zgc::barrier::z_load` takes `&AtomicU64` -- the
        //    SLOT, not the loaded word -- because its slow path CAS-heals the
        //    slot in place; and it returns a bare 42-bit heap OFFSET, which the
        //    caller must add the heap base to. By the time control reaches this
        //    cold arm the slot address is gone and only the word survives.
        //
        // The barrier therefore belongs one frame up, in [`jit_load_ref_slot`],
        // which is now the single seam this file funnels its raw reference-slot
        // reads through -- one seam dispatching on the backend, rather than the
        // per-site ZGC special cases the original TODO rightly forbade. The
        // backend-dispatching call that seam must make does not exist yet and
        // has to be ADDED to `gc/src/vm_heap.rs`; its exact signature, and what
        // its `Zgc` arm must do, are written down in
        // `.agent-requests/A9-gc-barrier.txt`. This branch becomes unreachable
        // the moment that lands, because the word arriving here will already be
        // a plain address.
        //
        // Keep the panic as the tripwire for a site that was missed. It has
        // never fired -- and today it CANNOT, which is a different fact and the
        // more important one: `vm/src/vm/vm_init.rs` pins
        // `const RELOCATION_REQUESTED: bool = false`, `ZgcRealHeap::
        // set_barrier_color` (the sole writer of the colored state) has no
        // non-test caller, and `vaddr::color` has no production caller at all.
        // No colored word is stored in a heap slot in any shipping
        // configuration, so this tripwire is armed where nothing can reach it.
        // That is exactly why the two counters below exist: without them, "the
        // tripwire is armed and zero words reached it" and "the tripwire is
        // armed in a place a word cannot reach" produce identical silence.
        if cratonvm_gc::zgc::vaddr::is_colored_word(raw)
            && cratonvm_gc::zgc::vaddr::is_well_formed(raw)
        {
            // Unconditional, unlike the per-site counters: this arm is `#[cold]`
            // and a clean run never reaches it, so the increment costs nothing
            // and this is the one number that must be readable without anyone
            // having remembered to set a diagnostic flag first.
            ref_load_census::COLORED_WORDS_SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if !zgc_jit_load_barrier_suppressed() {
                panic!(
                    "ZGC colored word {raw:#018x} reached the JIT read helper `{site}` \
                     with no load barrier: bit 63 (Z_COLORED_TAG) is set by \
                     gc/src/zgc/vaddr.rs so this word is a legitimate reference that \
                     has not been unmasked, not corruption. Barrier it via \
                     cratonvm_gc::zgc::barrier::z_load and plausibility-check the \
                     UNMASKED address; do not degrade it to null (that is the silent \
                     heap corruption of zgc-jit-load-barrier.md J1) and do not weaken \
                     plausible_heap_pointer to admit it."
                );
            }
            // `CRATONVM_ZGC_NO_JIT_LOAD_BARRIER` is set, so the operator has
            // asked to get PAST the tripwire. That re-arms exactly the
            // silent-null corruption of `zgc-jit-load-barrier.md` J1 for the
            // rest of the run, which is why it is counted separately -- a
            // bring-up run can then say how many live references it turned into
            // null -- and announced once on stderr, for the reason the
            // compressed-oops gate in `vm_init.rs` states about itself: a silent
            // fallback looks identical to a successful run, and the operator has
            // to be able to see which one they got.
            ref_load_census::COLORED_WORDS_DEGRADED
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            warn_once_jit_load_barrier_suppressed(raw, site);
        }
    }
    // Counts the event under the `Jit` source and emits the one-shot diagnostic
    // naming `site`, or returns `false` and does neither for `raw == 0`. See
    // `jit_ref_degradation_count` for why this is the shared table's `Jit` slot
    // rather than a static in this file.
    cratonvm_types::compact_value::note_ref_word_degradation(
        raw,
        cratonvm_types::compact_value::DegradationSource::Jit,
        site,
    );
    0
}

/// Per-site census of the raw heap reference-slot reads this file performs, and
/// of the ZGC colored words that reached them.
///
/// # Why a census, when there is already a tripwire
///
/// [`jit_ref_word_implausible`] panics on a well-formed colored word and does
/// nothing whatsoever otherwise. So "the seam is wired and four billion
/// reference loads went through it" and "the seam is wired into an arm that
/// never executes" produce byte-identical output: silence. This tree has been
/// bitten by that shape repeatedly -- an instrument armed where it cannot fire
/// reads as a clean result -- and this particular tripwire is a live instance
/// of it, because no colored word is stored in a heap slot in any shipping
/// configuration today (the argument is on the tripwire itself). These counters
/// supply the denominator, so a future `[JIT_REF_LOADS] getfield_compact_ref=0`
/// reads as "the seam is in the wrong arm" rather than as "nothing went wrong".
///
/// # Why the per-site counts are gated, and gated on a cached flag read
///
/// `jit_getfield`'s compact-reference arm is the hottest read in this VM. This
/// file has already measured ONE unconditional relaxed increment on that path
/// at ~2-3 ns of a 9 ns reference-field read (see
/// [`getfield_census_counting_enabled`]), and an *uncached* `runtime_var_os` on
/// the same path at a 3.4x regression (see [`compact_inline_dbg`]). So the
/// per-site increments sit behind the same cached-`OnceLock` gate those two
/// use, and the default configuration pays one relaxed load.
///
/// [`COLORED_WORDS_SEEN`] and [`COLORED_WORDS_DEGRADED`] are deliberately NOT
/// gated. They are incremented only from the `#[cold]`
/// [`jit_ref_word_implausible`] arm, which a clean run never reaches, so they
/// cost nothing -- and they are the two numbers that must be readable without
/// anyone having remembered to set a diagnostic flag first.
pub mod ref_load_census {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// `jit_aaload`'s element read. Category A of
    /// `docs/feature-designs/zgc-jit-load-barrier.md` 2.3: a raw
    /// reference-array element load, barriered where it stands.
    pub const AALOAD_ELEMENT: usize = 0;
    /// `jit_getfield`'s compact-reference field read. Site B of that document's
    /// 2.5.1 table, and the arm every Category-A inline lowering falls back to
    /// once `zgc_read_barrier_blocks_inline_fields` routes it here.
    pub const GETFIELD_COMPACT_REF: usize = 1;
    /// `jit_getfield`'s legacy 16-byte `Value` cell. Site D: the word has
    /// already been laundered through an `ObjectRef` by `read_value_atomic`, so
    /// this counts an arm whose barrier belongs UPSTREAM, not a barriered load.
    pub const GETFIELD_LEGACY_VALUE: usize = 2;
    /// `jit_getstatic`'s `System.in` intercept. Site E; same upstream caveat as
    /// [`GETFIELD_LEGACY_VALUE`], with `crate::vm::get_static_shared` as the
    /// upstream.
    pub const GETSTATIC_SYSTEM_IN: usize = 3;
    /// `jit_getstatic`'s general object arm. Site F; same upstream caveat.
    pub const GETSTATIC_OBJECT: usize = 4;

    const N: usize = 5;
    const NAMES: [&str; N] = [
        "aaload_element",
        "getfield_compact_ref",
        "getfield_legacy_value",
        "getstatic_system_in",
        "getstatic_object",
    ];

    static COUNTS: [AtomicU64; N] = [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ];

    /// Well-formed ZGC colored words that reached a JIT read helper with no
    /// load barrier. **Zero is the only good value, and today zero is also the
    /// only reachable value** -- see the tripwire's own comment for why. A
    /// non-zero count is proof that a Category-A site was missed.
    ///
    /// Ungated and unconditional: incremented only from a `#[cold]` arm.
    pub static COLORED_WORDS_SEEN: AtomicU64 = AtomicU64::new(0);

    /// The subset of [`COLORED_WORDS_SEEN`] that `CRATONVM_ZGC_NO_JIT_LOAD_BARRIER`
    /// let past the tripwire, each of which was then handed to compiled code as
    /// `null` -- i.e. the exact silent-heap-corruption count for a run that
    /// deliberately disarmed the tripwire.
    ///
    /// This is the kill switch's engagement counter. A run with the switch set
    /// and this reading `0` did not exercise the suppressed path at all, which
    /// is a different result from "the suppressed path behaved".
    pub static COLORED_WORDS_DEGRADED: AtomicU64 = AtomicU64::new(0);

    /// Reference-slot loads that went through `VmHeap::load_ref_slot_barriered`
    /// -- the ones where [`super::jit_load_ref_slot`] had a `&VmHeap` to
    /// dispatch on.
    ///
    /// **This is the wiring's engagement counter, and it is the number the
    /// tripwire cannot supply.** [`COLORED_WORDS_SEEN`] reads `0` whether every
    /// load was barriered or the barrier call sits in an arm nothing executes;
    /// this tree has been bitten by an instrument armed where it cannot fire
    /// often enough that the denominator gets written down rather than
    /// inferred. Non-zero means the barriered route RAN.
    ///
    /// Gated on [`enabled`], because `GETFIELD_COMPACT_REF` -- the only site
    /// that increments it today -- is the hottest reference read in the VM and
    /// one UNGATED relaxed increment there has already been measured at a
    /// quarter of the read. It shares the single `enabled()` test with the
    /// per-site count inside [`note_route`], so the default configuration pays
    /// exactly what it paid before this counter existed.
    pub static BARRIERED_LOADS: AtomicU64 = AtomicU64::new(0);

    /// Reference-slot loads that took the RAW read because the seam had no
    /// `&VmHeap` to reach the barrier through.
    ///
    /// **Expected `0`, and ungated so a non-zero value cannot hide behind an
    /// unset diagnostic flag.** Since 2026-09-01 there is NO caller that passes
    /// `None`: `jit_getfield`'s compact-reference arm always had a `vm_ptr`,
    /// and `jit_aaload` gained one (`.agent-requests/B8-abi.txt`), so both of
    /// this file's slot-holding sites pass `Some(..)`.
    ///
    /// That changed what a zero MEANS here, and the difference is the whole
    /// value of the number. It used to read "the only `None` caller is a helper
    /// no emitter calls, so nothing reached the raw route". It now reads "no
    /// `None` route exists in this tree at all". A non-zero count is therefore
    /// no longer merely surprising: it says a NEW call site of the seam was
    /// added without a heap to dispatch on, and that those reference loads were
    /// not barriered -- harmless while nothing arms the barrier, and exactly
    /// risk J1 of `docs/feature-designs/zgc-jit-load-barrier.md` the moment
    /// something does.
    ///
    /// Ungated is affordable precisely because it is not on a hot path: the
    /// seam is `#[inline(always)]` and `heap` is a compile-time `Some` at both
    /// call sites, so in a release build the `!barriered` branch folds out of
    /// both and this increment is emitted nowhere at all. Keeping the `None`
    /// arm (rather than narrowing the seam to a bare `&VmHeap`) is deliberate:
    /// it is what makes a future third call site declare, in its own argument
    /// list, whether it can reach the barrier -- and fail loudly here if not.
    pub static UNBARRIERED_LOADS: AtomicU64 = AtomicU64::new(0);

    /// Is anything going to READ the per-site counters this run?
    ///
    /// Cached for the reason [`super::compact_inline_dbg`] documents: a
    /// per-call `runtime_var_os` on the `jit_getfield` path is itself the
    /// regression. `CRATONVM_DBG_JIT_REF_LOADS` is declared in
    /// `.agent-requests/A9-flags.txt`.
    #[inline]
    pub fn enabled() -> bool {
        static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
        cratonvm_jit::memo_flag::memo_bool(&ON, || cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_REF_LOADS"))
    }

    /// Count one reference-slot read at `slot`, one of the `*_ELEMENT` /
    /// `GETFIELD_*` / `GETSTATIC_*` constants above.
    #[inline(always)]
    pub fn note(slot: usize) {
        if enabled() {
            if let Some(c) = COUNTS.get(slot) {
                c.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// [`note`], plus which ROUTE the load took: `barriered = true` when
    /// [`super::jit_load_ref_slot`] had a `&VmHeap` and called
    /// `VmHeap::load_ref_slot_barriered`, `false` when it fell back to the raw
    /// `read_ref_slot`.
    ///
    /// # Why the route needs a number of its own
    ///
    /// A per-site count says a load happened; it does not say whether that load
    /// went through the barrier. With the barrier unarmed the two routes return
    /// bit-identical values, so nothing observable separates "the barrier is
    /// wired and ran four billion times" from "the barrier call sits in an arm
    /// that never executes" -- the failure this module was created to make
    /// impossible, reappearing one level down. `COLORED_WORDS_SEEN` cannot
    /// answer it either: it is `0` in both cases, and stays `0` for a third
    /// reason (nothing colors a slot today).
    ///
    /// # Why the two counters are gated differently
    ///
    /// [`BARRIERED_LOADS`] rides the SAME `enabled()` test as the per-site
    /// count, so the hot `getfield` arm pays one cached-flag read in total,
    /// exactly what it paid before this existed. [`UNBARRIERED_LOADS`] is
    /// ungated, which is affordable because `barriered` is a compile-time
    /// constant at every call site of the `#[inline(always)]` seam: since both
    /// callers pass `Some(..)` (2026-09-01) the increment folds out of a
    /// release build entirely, and it would be emitted only into a future
    /// `None` caller. An expected-zero tripwire that only a diagnostic flag can
    /// reveal is not much of a tripwire.
    #[inline(always)]
    pub(crate) fn note_route(slot: usize, barriered: bool) {
        if !barriered {
            UNBARRIERED_LOADS.fetch_add(1, Ordering::Relaxed);
        }
        if enabled() {
            if let Some(c) = COUNTS.get(slot) {
                c.fetch_add(1, Ordering::Relaxed);
            }
            if barriered {
                BARRIERED_LOADS.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Per-site counts, or `None` when the census is gated off.
    ///
    /// **Not `Some(vec![0; N])` when off.** A gated counter reported as a
    /// number is indistinguishable from a path that never ran, which is the
    /// precise misreading this module exists to prevent -- the same rule
    /// [`super::jit_getfield_helper_calls`] states for its own counter.
    pub fn snapshot() -> Option<Vec<(&'static str, u64)>> {
        if !enabled() {
            return None;
        }
        Some(
            NAMES
                .iter()
                .zip(COUNTS.iter())
                .map(|(n, c)| (*n, c.load(Ordering::Relaxed)))
                .collect(),
        )
    }

    /// The two ungated tripwire counters, `(seen, degraded_by_kill_switch)`.
    /// Always real numbers; see the statics' own docs.
    pub(crate) fn colored_word_counts() -> (u64, u64) {
        (
            COLORED_WORDS_SEEN.load(Ordering::Relaxed),
            COLORED_WORDS_DEGRADED.load(Ordering::Relaxed),
        )
    }

    /// `(barriered, unbarriered)` -- which ROUTE the seam's loads took.
    ///
    /// `barriered` is `None` when the per-site census is gated off, for the
    /// same reason [`snapshot`] returns `None` rather than a vector of zeros:
    /// a gated counter reported as a number is indistinguishable from a path
    /// that never ran, and that is the exact misreading these counters exist
    /// to prevent. `unbarriered` is ungated and always real.
    pub fn route_counts() -> (Option<u64>, u64) {
        let barriered = if enabled() {
            Some(BARRIERED_LOADS.load(Ordering::Relaxed))
        } else {
            None
        };
        (barriered, UNBARRIERED_LOADS.load(Ordering::Relaxed))
    }

    /// One `[JIT_REF_LOADS]` line. Prints the tripwire counters unconditionally
    /// and says so explicitly when the per-site half is switched off, so the
    /// reader is never left inferring which of the two zeros they are holding.
    ///
    /// `barriered=` is the wiring's engagement number and appears only with the
    /// census on, because it rides the hot path. `unbarriered=` is always
    /// printed, is expected to be `0`, and a non-zero value means a reference
    /// load skipped the barrier.
    pub fn report() {
        let (seen, degraded) = colored_word_counts();
        let (barriered, unbarriered) = route_counts();
        match snapshot() {
            None => eprintln!(
                "[JIT_REF_LOADS] per-site counts OFF (set CRATONVM_DBG_JIT_REF_LOADS=1) \
                 unbarriered={unbarriered} colored_words_seen={seen} \
                 colored_words_degraded={degraded}"
            ),
            Some(rows) => {
                let mut line = String::from("[JIT_REF_LOADS]");
                for (name, n) in rows {
                    line.push_str(&format!(" {name}={n}"));
                }
                if let Some(b) = barriered {
                    line.push_str(&format!(" barriered={b}"));
                }
                line.push_str(&format!(
                    " unbarriered={unbarriered} colored_words_seen={seen} \
                     colored_words_degraded={degraded}"
                ));
                eprintln!("{line}");
            }
        }
    }

    /// [`report`], but silent on a run that has nothing to say: the per-site
    /// census is off AND no colored word ever reached a helper.
    ///
    /// This is the shape an exit-path caller wants (`Once`-guarded, silent when
    /// there is no news), matching `cratonvm_types::compact_value::
    /// degradation_exit_summary`. Wiring request: `.agent-requests/A9-wiring.txt`.
    pub fn exit_summary() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let (seen, _) = colored_word_counts();
            let (_, unbarriered) = route_counts();
            // `unbarriered != 0` is news even with the per-site census off: it
            // names reference loads that did not go through the load barrier
            // at all, which is the one thing this file cannot afford to report
            // as silence.
            if !enabled() && seen == 0 && unbarriered == 0 {
                return;
            }
            report();
        });
    }
}

/// Per-site JIT reference-load counts, or `None` when the census is gated off.
/// See [`ref_load_census::snapshot`] for why `None` and not a vector of zeros.
pub fn jit_ref_load_census() -> Option<Vec<(&'static str, u64)>> {
    ref_load_census::snapshot()
}

/// `(colored_words_seen, colored_words_degraded)` -- the two ungated tripwire
/// counters. See [`ref_load_census::COLORED_WORDS_SEEN`].
pub fn jit_colored_word_counts() -> (u64, u64) {
    ref_load_census::colored_word_counts()
}

/// `(barriered, unbarriered)` reference-slot loads -- the wiring's engagement
/// numbers.
///
/// `barriered` is `None` when the per-site census is gated off, for the reason
/// [`ref_load_census::snapshot`] gives; `unbarriered` is always a real number
/// and is expected to be `0`. See [`ref_load_census::note_route`].
pub fn jit_ref_load_routes() -> (Option<u64>, u64) {
    ref_load_census::route_counts()
}

/// Has the operator asked the JIT read helpers NOT to enforce the ZGC
/// load-barrier invariant?
///
/// `CRATONVM_ZGC_NO_JIT_LOAD_BARRIER` is the kill switch for the one behaviour
/// this file's ZGC arm has: the [`jit_ref_word_implausible`] tripwire. Set, a
/// well-formed colored word is counted and degraded to null instead of
/// panicking -- which is J1's silent heap corruption, deliberately re-armed so
/// that a barrier bring-up run can get past a first colored word and collect a
/// census instead of dying on it. Unset (the default) the tripwire panics,
/// which is today's behaviour byte for byte.
///
/// It is A/B-able in one binary on purpose: the alternative -- two builds --
/// is the cross-binary A/B this repo has repeatedly found is not an A/B.
///
/// Cached for the same reason [`compact_inline_dbg`] is: the read would
/// otherwise sit on a path a bring-up run takes per reference load. Declared in
/// `.agent-requests/A9-flags.txt`.
#[cfg(feature = "zgc")]
fn zgc_jit_load_barrier_suppressed() -> bool {
    static CACHE: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&CACHE, || cratonvm_types::flags::runtime_flag_on("CRATONVM_ZGC_NO_JIT_LOAD_BARRIER"))
}

/// Announce, once, that the kill switch above turned a live reference into
/// `null`.
///
/// On stderr rather than through `tracing`, for the reason the compressed-oops
/// refusal in `vm/src/vm/vm_init.rs` gives for its own stderr line: the
/// degraded run looks identical to a clean one from the outside, and the
/// operator has to be able to see which one they got. Once, because the
/// per-event count is [`ref_load_census::COLORED_WORDS_DEGRADED`]'s job and a
/// per-load print would bury it.
#[cfg(feature = "zgc")]
fn warn_once_jit_load_barrier_suppressed(raw: u64, site: &'static str) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        eprintln!(
            "[cratonvm] CRATONVM_ZGC_NO_JIT_LOAD_BARRIER is set: the ZGC colored word \
             {raw:#018x} that reached the JIT read helper `{site}` was DEGRADED TO NULL \
             instead of tripping the missing-load-barrier panic. Compiled code has just \
             been handed null for a live object (zgc-jit-load-barrier.md J1). Every \
             further occurrence is counted silently in [JIT_REF_LOADS] \
             colored_words_degraded. Unset the variable to restore the tripwire."
        );
    });
}

/// The single seam through which this file turns a raw heap reference SLOT into
/// the `i64` compiled code expects -- and the one place a ZGC load barrier has
/// to be installed.
///
/// # Why one seam and not a barrier per site
///
/// The `TODO(zgc)` this discharges was explicit that the JIT read helpers must
/// "acquire the barrier by dispatching on the backend rather than by growing a
/// ZGC special case per site", and it is right: seven hand-written colored-word
/// tests are seven chances for the eighth reader to be missed, which is risk J1
/// of `docs/feature-designs/zgc-jit-load-barrier.md` verbatim
/// (`emit_load_string_value_ptr` was already missed once by exactly that class
/// of gate). Sites A and B of that document's 2.5.1 table are the only two
/// arms in this file that hold a SLOT when the plausibility filter runs, and
/// both now come here.
///
/// # What is wired now, and what is not
///
/// The barrier call itself now EXISTS: `VmHeap::load_ref_slot_barriered`
/// (`gc/src/vm_heap.rs`) takes the SLOT, dispatches on the backend inside
/// `gc/`, and so keeps every ZGC detail there instead of growing the per-site
/// ZGC special case the design forbids. Its `Generational` / `G1` arms and its
/// unarmed `Zgc` arm are literally `read_ref_slot(slot)`; only an ARMED ZGC
/// barrier delegates to `ZgcRealHeap::load_barrier_slot`, which owns the
/// fast/slow path, the offset-0/null disambiguation and -- the part a fresh
/// implementation gets wrong -- the 42-bit-offset-to-address conversion that
/// `gc/src/zgc/relocate.rs` and `gc/src/zgc/mark.rs` both carry warnings about.
///
/// Both call sites are wired. Site B (`jit_getfield`'s compact-reference arm)
/// always had a `&VmHeap`: that helper takes `vm_ptr` and binds
/// `vm = &*(vm_ptr as *const SharedVm)`, so it passes `Some(&vm.mem.heap)`.
/// Site A (`jit_aaload`) was the last gap -- declared `(array_ptr, index)` with
/// no `vm_ptr`, so it had nothing to reach a heap through and could only pass
/// `None`. On 2026-09-01 it gained a leading `vm_ptr`
/// (`.agent-requests/B8-abi.txt`) and passes `Some(heap_from_vm(vm_ptr))`. The
/// ABI change was affordable because no emitter calls `helpers.aaload`; the
/// row's own comment in `jit-api/src/helpers_abi.rs` carries that argument.
///
/// So `heap` is `Some(..)` at every call site in this file, and
/// [`ref_load_census::UNBARRIERED_LOADS`] is expected `0` for a STRONGER reason
/// than before: there is no `None` route left to take, rather than one that
/// nothing happens to reach. The `None` arm is kept anyway -- see that
/// counter's own doc for why a future caller has to say so in its argument
/// list rather than be given a global to reach for.
///
/// # Why `None` and not a cached heap handle
///
/// A thread-local (or process-global) `*const VmHeap`, installed from
/// [`heap_from_vm`], would avoid the ABI change and cost one cached read on a
/// path that already pays a census increment. It was rejected as a
/// USE-AFTER-FREE, not on taste:
///
///  * This process creates and drops several `SharedVm`s on one thread --
///    `native::jni::set_process_vm`'s own comment names "a test that builds a
///    second `Vm` in-process" as an expected case -- and nothing inside this
///    file can clear a cached handle at VM teardown. `jit_aaload` can be the
///    first helper a thread runs after a new VM starts, so even a
///    poison-on-mismatch rule never gets to observe the change in time.
///    Reading a freed `VmHeap`'s enum discriminant on the GC read path is UB,
///    and it would be UB introduced by a change whose entire claim is that it
///    alters nothing.
///  * Caching an `Arc<SharedVm>` would fix the lifetime and buy two new
///    problems: it keeps a VM alive past its owner, which is exactly why
///    `native::jni::PROCESS_VM` deliberately holds a `Weak`; and a
///    `thread_local!` holding `Option<Arc<SharedVm>>` is one of the shapes
///    `vm/src/lib.rs`'s teardown shim blames for the Windows
///    `STATUS_ACCESS_VIOLATION` at test exit. Caching the `Weak` instead puts
///    an `upgrade`/drop refcount round trip -- a contended atomic pair -- on
///    the hottest reference read in the VM.
///  * The safe resolvers that already exist are not affordable per load:
///    `native::jni::process_vm_strict` takes a `parking_lot::Mutex`, runs a
///    `Vec::retain` and upgrades a `Weak`, and answers `None` whenever more
///    than one VM is live.
///
/// The `None` fallback is the raw read, and that is the point: it is not
/// silently unbarriered. A colored word arriving through it still reaches the
/// [`jit_decode_ref_word`] tripwire and panics naming the site, rather than
/// being degraded to a null (risk J1). Missing the barrier fails LOUDLY.
/// [`ref_load_census::UNBARRIERED_LOADS`] counts every load that took it --
/// ungated, and expected `0`.
///
/// # ARENA MUTEX -- read this before arming the barrier
///
/// `ZgcRealHeap::load_barrier_slot` reaches the heap base through
/// `ZMarkContext::heap_base()`, which takes `self.arena.lock()`. So an ARMED
/// barrier takes the ZGC arena mutex ONCE PER REFERENCE LOAD. That is
/// pre-existing inside `load_barrier_slot` and inert today, because nothing
/// arms the barrier -- but wiring this seam is what puts it on the hot path,
/// and it is a deadlock hazard for any future caller that is already holding
/// that lock when a reference load happens. Whoever flips
/// `RELOCATION_REQUESTED` should meet this before their first run rather than
/// after: the base is a per-cycle constant and wants to be hoisted out of the
/// lock, not taken 10^9 times.
///
/// Site A raises the stakes rather than adding a new hazard. `jit_aaload` is a
/// reference-ARRAY element read -- the body of `for (Object o : arr)` -- so an
/// armed barrier takes and releases that one mutex per loop iteration, on every
/// mutator thread at once; and the deadlock arm is reachable from anything that
/// walks references while already holding the arena (a relocation or marking
/// helper that re-enters compiled code). Neither is an argument for leaving
/// site A unbarriered: an unbarriered read under an ARMED barrier is a wrong
/// ANSWER, while a slow correct one is recoverable. They are the argument for
/// hoisting the base out of `heap_base()` BEFORE the first armed run.
///
/// # The precondition a caller must already satisfy for that to be legal
///
/// `z_load` takes `&AtomicU64`, which is a promise about the SLOT, not about
/// the word: it must be an 8-byte-aligned, exactly-8-byte location that is
/// valid for WRITES as well as reads, because the barrier's slow path
/// self-heals it with a `compare_exchange`. Both current callers satisfy this
/// only while compressed oops are off -- with `narrow_oops_enabled()` the slot
/// is FOUR bytes ([`cratonvm_types::narrow_oop::read_ref_slot`] branches on
/// exactly that) and viewing it as an `AtomicU64` would be unsound. ZGC plus
/// compressed oops is already refused at VM init
/// (`vm/src/vm/vm_init.rs`, the `narrow_oops` gate) -- and
/// `load_ref_slot_barriered` now re-checks the width ITSELF, before the armed
/// test, so a narrow unarmed run still takes the ordinary plain read and a
/// narrow armed one refuses rather than truncating. It does not inherit the
/// guarantee from this comment, which is what the `gc/` side was asked for.
///
/// # HOW WE KNOW A DEFAULT RUN IS UNCHANGED
///
/// Not "measured equivalent" -- the value returned is the same EXPRESSION.
/// `load_ref_slot_barriered` is written as an early return on the one arm that
/// differs, and that arm is unreachable: `vm/src/vm/vm_init.rs` pins `const
/// RELOCATION_REQUESTED: bool = false`; `ZgcRealHeap::set_barrier_color`, the
/// sole writer of the colored state, has no non-test caller; and
/// `barrier_good_mask` never leaves `vaddr::Z_REMAPPED`, so
/// `load_barrier_armed()` is false for the process lifetime. Every reachable
/// configuration -- `Generational`, `G1`, `--no-default-features`, and ZGC as
/// shipped -- therefore evaluates `read_ref_slot(slot)`, exactly as this
/// function did before. What is added is two relaxed `AtomicBool` loads and
/// two not-taken branches on site B, both behind `#[inline]`, which is the
/// price `load_ref_slot_barriered`'s own doc comment quotes for itself.
///
/// # Safety
///
/// `slot` must point at a live reference slot of the current width
/// ([`cratonvm_types::narrow_oop::ref_field_size`]) inside a live object -- the
/// same contract as `read_ref_slot`, which the `None` arm is a thin wrapper
/// over. When `heap` is `Some`, it must be the heap that OWNS that slot:
/// passing another VM's heap would, once the barrier is armed, forward the
/// reference through the wrong relocation table and return a pointer into the
/// wrong arena. Both call sites take it from the `SharedVm` they were handed,
/// which is the only way to satisfy that without a global.
#[inline(always)]
unsafe fn jit_load_ref_slot(
    slot: *const u8,
    heap: Option<&VmHeap>,
    census_slot: usize,
    site: &'static str,
) -> i64 {
    ref_load_census::note_route(census_slot, heap.is_some());
    let raw = match heap {
        // THE BARRIER. Backend dispatch lives inside `gc/`; see this
        // function's doc comment for why it cannot live here.
        Some(h) => h.load_ref_slot_barriered(slot),
        // No `&VmHeap` reachable at this call site (site A only). Raw read --
        // and the tripwire below is what stops that being silent.
        None => read_ref_slot(slot),
    };
    // The plausibility test runs on the UNMASKED address the barrier returned,
    // never on a colored word. That ordering is the whole point: filtering the
    // colored word first is what silently nulls a live reference (risk J1), and
    // `plausible_heap_pointer` is deliberately NOT weakened to admit one --
    // doing so would leave the tripwire unable to fire, which is worse than
    // the state this replaces.
    jit_decode_ref_word(raw, site)
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm
// pointer -- the universal JIT-helper caller contract, the same one jit_aastore
// and jit_getfield state. It is read only on the successful element-read path:
// the null/implausible-array arm and the out-of-bounds arm both return before
// `heap_from_vm` is reached, which is what lets the null-array unit test below
// call this helper with a vm_ptr of 0. array_ptr must be 0 (null) or a
// valid heap pointer to a reference array object. Null triggers a pending NPE +
// `i64::MIN` deopt sentinel; out-of-bounds is handled gracefully by the bounds
// check below.
//
// vm_ptr is FIRST, matching `jit_aastore(vm_ptr, array_ptr, index, val)` and
// `jit_getfield(vm_ptr, obj_ptr, field_index)`: every VM-touching helper in the
// table puts the VM in arg0, and a helper that put it last would be exactly the
// asymmetry `accessor_name_matches_field` exists to catch the consequences of.
// The C declaration lives in `jit-api/src/helpers_abi.rs`; the two halves are
// pinned together by `let _: HelperFnAaload = jit_aaload;` further down this
// file, which is why this change could not land half done.
pub unsafe extern "C" fn jit_aaload(vm_ptr: i64, array_ptr: i64, index: i64) -> i64 {
    contain("jit_aaload", OnPanic::Deopt, i64::MIN, || {
        jit_aaload_body(vm_ptr, array_ptr, index)
    })
}

/// Body of [`jit_aaload`]. Panic-guarded via [`contain`] with [`OnPanic::Deopt`].
// SAFETY: Called under `contain` panic guard with JIT caller arguments. Checks
// pointer plausibility and bounds before performing the reference load.
unsafe fn jit_aaload_body(vm_ptr: i64, array_ptr: i64, index: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) array
    // reference identically: take the NPE path instead of dereferencing it
    // (the length/element read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // arrays always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(array_ptr as u64) {
        // JVMS §aaload: throw NullPointerException on null array reference.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::ALOAD_OBJECT,
        );
        return i64::MIN;
    }
    // SAFETY: array_ptr is non-null and points to a live Object[] on the GC heap.
    // ptr::read is used because Value::Object may contain non-Copy ObjectRef.
    let ptr = array_ptr as *const u8;
    let length = *(ptr.add(ARRAY_LENGTH_OFFSET) as *const u32) as i64;
    if index < 0 || index >= length {
        // JVMS §aaload: throw ArrayIndexOutOfBoundsException on an out-of-bounds
        // index (same protocol as `jit_throw_aioobe`). Previously returned 0 (null),
        // silently fabricating a null element and masking real OOB bugs.
        JIT_SIGNALS.with(|s| s.aioobe.set(Some((index, length))));
        // The frames this bounds check fired in are about to leave: the helper
        // returns and the compiled body runs its epilogue, so the
        // `ArrayIndexOutOfBoundsException` is built later, from the interpreter,
        // on a stack that no longer has them. Snapshot here, the one moment the
        // signal and the frames both exist -- the same rung the NPE and
        // div-by-zero setters stand on. `0` for the trap key: an array opcode's
        // bci is one the frame walk already has, unlike an inline null check's.
        snapshot_trap_frames(0);
        return i64::MIN;
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize * ref_element_size());
    // Degrade an implausible element reference to null instead of returning bits
    // the JIT will deref -> SIGSEGV (the `0x8D8D..`-class stale ref). Mirrors
    // `read_prim_element`'s Reference arm; valid refs (or 0=null) pass through.
    //
    // Site A ("raw reference-array element load", Category A of
    // `docs/feature-designs/zgc-jit-load-barrier.md` 2.3, and site A of its
    // 2.5.1 table). This is one of only TWO arms in this file that still hold
    // the SLOT when the plausibility filter runs, so it is one of the two that
    // can take the barrier where it stands. It goes through
    // [`jit_load_ref_slot`], the file's single seam, rather than through a
    // ZGC test of its own.
    //
    // BARRIERED as of 2026-09-01. This was the LAST unbarriered slot-holding
    // reference read in the file. The seam's barrier call needs a `&VmHeap`;
    // this helper used to receive no `vm_ptr`, so it had nothing to reach one
    // through and passed `None`. A cached thread-local or process-global heap
    // handle was considered and rejected as a use-after-free -- the argument is
    // on [`jit_load_ref_slot`] -- so the gap was closed the way
    // `.agent-requests/B8-abi.txt` wrote out: a leading `vm_ptr`, exactly as
    // `jit_aastore` already has one, plus ONE line in
    // `jit-api/src/helpers_abi.rs`. It cost nothing at any emitter, because no
    // emitter calls `helpers.aaload` -- `aaload` is lowered inline by
    // `x64::arrays::emit_ref_aload_regs`, and `grep -rn "helpers\.aaload"
    // jit/src` is empty -- so there was no emitted `call` whose argument
    // registers had to be rearranged.
    //
    // `heap_from_vm(vm_ptr)` rather than an open-coded
    // `&(*(vm_ptr as *const SharedVm)).mem.heap`: it is the same deref plus the
    // `debug_assert!(vm_ptr != 0)` every other VM-touching helper in this file
    // already relies on, so the caller contract stays stated in ONE place. It
    // is evaluated HERE, after both guard arms, which is why the `vm_ptr == 0`
    // the null-array unit test passes is never dereferenced. And the heap it yields is by
    // construction the one that OWNS `elem_ptr` -- `array_ptr` was handed to us
    // by compiled code running against this same VM -- which is the seam's
    // `Some(..)` contract.
    //
    // The tripwire behind this call is NOT retired by the barrier; it is what
    // proves no site was missed. A colored word reaching `jit_decode_ref_word`
    // still panics naming the site rather than being degraded to null (risk
    // J1), and `plausible_heap_pointer` is still not weakened to admit one. The
    // panic simply became UNREACHABLE from here, because the word the seam now
    // hands it is an unmasked address -- and the plausibility test runs on that
    // unmasked address, never on the colored word.
    jit_load_ref_slot(
        elem_ptr,
        Some(heap_from_vm(vm_ptr)),
        ref_load_census::AALOAD_ELEMENT,
        "jit_aaload/element",
    )
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// array_ptr must be 0 (null) or a valid heap pointer to a reference array.
// val is 0 (null) or a raw pointer to a live heap object. Write barrier is issued
// for non-null stores to maintain generational GC card table invariants.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
/// JVMS §aastore covariance: does storing `val` into `array_ptr` have to throw
/// `ArrayStoreException`?
///
/// Returns `true` **and stashes the throwable** when the store is refused;
/// `false` (allow the store) otherwise. Call only with a non-null `val`: a
/// null element is always storable.
///
/// # One body, two callers, on purpose
///
/// [`jit_aastore`] (the full helper) and [`jit_aastore_type_check`] (what the
/// x64 inline lowering calls) both come here. They used to be one function
/// with one caller, and then the x64 emitter stopped calling it: R20/HIGH-5
/// replaced the helper call with an inline store, justified by a comment
/// saying "the current `jit_aastore` helper does NOT enforce the ASE check …
/// no regression". That was true when written and was falsified — silently —
/// when the check landed in the helper, because a premise in a comment is not
/// a compile-time link. The result was a JIT-compiled `aastore` that performed
/// the store and raised nothing (`RExceptions`: `cold=[java.lang.Integer]
/// hot=[no-throw]`).
///
/// Keeping the rule in one function is the part that is mechanically
/// enforceable; the comment above is not.
///
/// # Conservative posture
///
/// `aastore_element_assignable` fails OPEN on imprecise type information, so
/// this is additive correctness and never invents an `ArrayStoreException`. If
/// no JIT thread is available or the funnel cannot build the throwable, this
/// returns `false` and the store proceeds — the pre-fix behaviour, in the rare
/// construction-failure case only.
///
/// **IMPRECISE, not merely UNPROVEN — and the difference is a measured
/// defect.** `RArrayStoreInterfaces` s12 (`Runnable[] <- Proxy(Marker)`)
/// answered `no-throw` in `cold` AND `hot` where HotSpot 25.0.3+9-LTS throws
/// `ArrayStoreException`: the predicate held a `$Proxy`-named blanket that
/// admitted every dynamic proxy into every interface-typed array. A generated
/// `$ProxyN`'s interface set is not imprecise information — it is recorded on
/// `ClassOrigin::GeneratedProxy` by the code that generated the class — so the
/// blanket was scoped to proxies whose set the VM does NOT hold, and the store
/// above is now refused. **Nothing in this file changed to achieve that**, and
/// that is the load-bearing observation: the identical `cold`/`hot` divergence
/// is what proved the defect was in the shared predicate rather than in
/// lowering, and the identical fix in both tiers follows from this call being
/// the compiled tier's ONLY type question. See
/// `docs/feature-designs/concurrent-gc-maturation.md`.
///
// SAFETY: `vm_ptr` must be a live `SharedVm`; `array_ptr` and `val` must be live heap
// pointers (the callers null/plausibility-check both first).
unsafe fn aastore_store_is_refused(vm_ptr: i64, array_ptr: i64, val: i64) -> bool {
    let vm = &*(vm_ptr as *const SharedVm);
    let array_ref = ObjectRef::from_raw(array_ptr as usize as *mut u8);
    let value_ref = ObjectRef::from_raw(val as usize as *mut u8);
    if vm.mem.heap.element_type_of(array_ref) != ArrayElementType::Reference {
        return false;
    }
    // r9w12: the `Object[]` RULE, asked HERE rather than four heap reads later.
    //
    // `aastore_element_assignable` opens with exactly this test, but behind a
    // `kind_of` + a SECOND `element_type_of` that only exist because its other
    // callers have not established the shape. This one has: the line above is
    // that establishment. So the common store -- `ArrayList.elementData`, every
    // varargs pack, every `toArray()`, and `BoxProbe box`'s `Object[] sink` --
    // now costs ONE `class_id_of` plus the memo probe instead of four checked
    // heap accesses and a call.
    //
    // It is a rule, not a heuristic: every reference value IS a
    // `java.lang.Object`, so no component information and no hierarchy walk can
    // make the store illegal. `reference_array_component_is_object` answers
    // `true` for an unnameable component too, which is the same direction
    // `aastore_element_assignable`'s own arm takes for it.
    //
    // MEASURED on the 2026-09-20 dev build: `BoxProbe box` (10 M escaping
    // `Integer.valueOf` into an `Object[1024]`) spent 2.24 % in
    // `aastore_store_is_refused`, 1.64 % in `aastore_element_assignable`, 1.64 %
    // in `VmHeap::element_type_of`, 1.14 % in `VmHeap::kind_of` and 1.30 % in
    // `jit_aastore_type_check` -- the 9.6 % the collection-natives page's
    // wave-11 "still open" item 2 named.
    if crate::runtime::interpreter::reference_array_component_is_object(vm, array_ref) {
        return false;
    }
    if crate::runtime::interpreter::aastore_element_assignable(vm, array_ref, value_ref) {
        return false;
    }
    // W8-E11-1: the raw `get_class(class_id_of(value_ref)).name` lookup that
    // used to stand here was the THIRD twin of a defect the interpreter's two
    // `aastore` arms had already had fixed (W8-E6-1). On a reference array the
    // header class id holds the COMPONENT's class BY DESIGN — stated in
    // `typecheck::array_descriptor_of` and again on `cce_display_class_name`,
    // where the same trap once produced `java.lang.String cannot be cast to
    // java.lang.String` and cost a session as a supposed class-identity split.
    // So for an array-valued element the message was off by exactly one array
    // dimension, and right for everything else.
    //
    // HotSpot's rule is `Klass::external_name()` of the VALUE'S OWN class.
    // MEASURED, JDK 25.0.3 (`scratchpad/e11/AseName.java`, one execution per
    // shape so nothing is in the fast-throw regime):
    //
    //   String[]   <- Integer      java.lang.Integer
    //   String[]   <- AseName$Inner  AseName$Inner      (binary-with-dots, `$` kept)
    //   String[]   <- Plain        Plain                (default package)
    //   String[][] <- Integer[]    [Ljava.lang.Integer;
    //   String[][] <- String[][]   [[Ljava.lang.String;
    //   String[]   <- Plain[]      [LPlain;
    //   String[][] <- int[]        [I
    //   String[][] <- int[][]      [[I
    //   String[][] <- byte[]       [B
    //
    // Reuse `cce_display_class_name` rather than writing a fourth spelling of
    // this parse: it is the same "Java-visible class name for a VM-minted type
    // error" question `checkcast` and the two interpreter `aastore` arms ask,
    // it reconstructs the descriptor via `array_descriptor_of`, and it carries
    // the `UnmodifiableMap` storage-stamp translation that keeps a VM-internal
    // class out of an app-visible message.
    //
    // It returns the INTERNAL (slashed) name and that is what we want here.
    // W7-37: raise through `throw_runtime_error`, the single funnel every
    // VM-minted `RuntimeError` passes through, which is where the message gains
    // HotSpot's EXTERNAL class name — minting the throwable here printed
    // `java/lang/Integer` where HotSpot and the interpreter's own `aastore`
    // print `java.lang.Integer`, and callers regex that message and feed the
    // capture to `Class.forName`. The funnel's `rewritable` predicate
    // (`contains('/') && no whitespace`) correctly leaves `[I` / `[[I` / `[B`
    // and a default-package name such as `[LPlain;` alone.
    //
    // Two statements, not one: `cce_display_class_name` takes the class-manager
    // read lock itself, so the guard from the name lookup must be dropped
    // first.
    //
    // r9w9 exc9: built inside the `OmitStackTraceInFastThrow` door's closure,
    // so a hot site (opt-in, default off) skips the name lookup along with the
    // rest of the full construction.
    let elem_message = || {
        let raw_elem_name = vm
            .classes
            .class_manager
            .read()
            .get_class(vm.mem.heap.class_id_of(value_ref))
            .map(|c| c.name.to_string())
            .unwrap_or_else(|| "?".to_string());
        let elem_cls =
            crate::runtime::interpreter::cce_display_class_name(vm, value_ref, &raw_elem_name);
        // JIT round 21 wave 24, lane names: by the value's bottom class's
        // hidden flag (`CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`).
        let elem_cls = crate::runtime::exceptions::array_store_element_text(vm, value_ref, elem_cls);
        crate::error::RuntimeError::ArrayStoreException { message: elem_cls }
    };
    if let Some((thread, _guard)) = jit_thread_mut() {
        use crate::error::MethodCallFailed;
        if let MethodCallFailed::ExceptionThrown(exc) =
            crate::runtime::exceptions::throw_implicit_runtime_error(
                vm,
                thread,
                crate::runtime::exceptions::FastThrowKind::ArrayStore,
                crate::runtime::exceptions::fast_throw_site_of_live_frames(vm).as_ref(),
                elem_message,
            )
        {
            set_jit_pending_exception(thread, exc);
            return true;
        }
    }
    false
}

/// `aastore` element-type check for the x64 INLINE lowering.
///
/// The emitter lowers `aastore` itself — null check, bounds check, SATB
/// pre-write barrier, `MOV [array + index*8 + HEADER_SIZE], val`, card mark —
/// and the only part it cannot do inline is this one, because answering it
/// needs the class manager. So the arm calls this first and lets the store
/// proceed only on a `0`.
///
/// Returns `0` when the store is legal, and the `i64::MIN` deopt sentinel when
/// it is not, having stashed the `ArrayStoreException`. The codegen routes the
/// sentinel through `emit_post_invoke_exception_check`, the same drain
/// `jit_checkcast`'s ClassCastException uses.
///
/// A null `val` never reaches here: the emitter branches over the call, so the
/// overwhelmingly common `arr[i] = null` costs a test-and-jump, not a call.
///
/// # SAFETY
///
/// Called from JIT-compiled code. `vm_ptr` must be a valid `SharedVm` pointer.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_aastore_type_check(vm_ptr: i64, array_ptr: i64, val: i64) -> i64 {
    let f = || jit_aastore_type_check_body(vm_ptr, array_ptr, val);
    contain(
        "jit_aastore_type_check",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_aastore_type_check`], run under its panic guard.
// SAFETY: Called under `contain` panic guard with JIT caller arguments. Validates
// array and element heap pointers before invoking type checking.
unsafe fn jit_aastore_type_check_body(vm_ptr: i64, array_ptr: i64, val: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Same posture as `jit_checkcast`: a null or implausible (stale/garbage,
    // unaligned/>47-bit) pointer is not dereferenced. Allow the store rather
    // than inventing an exception from a value whose type is unknowable.
    if val == 0
        || !cratonvm_types::plausible_heap_pointer(val as u64)
        || !cratonvm_types::plausible_heap_pointer(array_ptr as u64)
    {
        return 0;
    }
    if aastore_store_is_refused(vm_ptr, array_ptr, val) {
        i64::MIN
    } else {
        0
    }
}

pub unsafe extern "C" fn jit_aastore(vm_ptr: i64, array_ptr: i64, index: i64, val: i64) {
    let f = || jit_aastore_body(vm_ptr, array_ptr, index, val);
    contain("jit_aastore", OnPanic::Throw { vm_ptr }, (), f)
}

/// Body of [`jit_aastore`], run under its panic guard.
// SAFETY: Called under `contain` panic guard with JIT caller arguments. Checks
// array pointer plausibility, bounds, and type assignability before storing.
unsafe fn jit_aastore_body(vm_ptr: i64, array_ptr: i64, index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) array
    // reference identically: take the NPE path instead of dereferencing it
    // (the length/element read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // arrays always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(array_ptr as u64) {
        // JVMS §aastore: throw NullPointerException on null array reference.
        // Round-8 CRIT fix: see `jit_bastore` for full rationale. Set the
        // pending-NPE flag; the interpreter's post-JIT path now drains it
        // on every return, so the void-return sentinel-less channel is
        // no longer a correctness blocker.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::ASTORE_OBJECT,
        );
        return;
    }
    // SAFETY: array_ptr is non-null and points to a live Object[] on the GC heap.
    let ptr = array_ptr as *mut u8;
    let length = *(ptr.add(ARRAY_LENGTH_OFFSET) as *const u32) as i64;
    if index < 0 || index >= length {
        // JVMS §aastore: throw ArrayIndexOutOfBoundsException on an out-of-bounds
        // index. Previously the store was silently dropped. Set the pending-AIOOBE
        // flag and return BEFORE the SATB barrier / write below (no element is read
        // or written on the OOB path). The void return cannot carry the deopt
        // sentinel, so the interpreter's post-JIT drain surfaces the exception.
        JIT_SIGNALS.with(|s| s.aioobe.set(Some((index, length))));
        // The frames this bounds check fired in are about to leave: the helper
        // returns and the compiled body runs its epilogue, so the
        // `ArrayIndexOutOfBoundsException` is built later, from the interpreter,
        // on a stack that no longer has them. Snapshot here, the one moment the
        // signal and the frames both exist -- the same rung the NPE and
        // div-by-zero setters stand on. `0` for the trap key: an array opcode's
        // bci is one the frame walk already has, unlike an inline null check's.
        snapshot_trap_frames(0);
        return;
    }
    // JVMS §aastore covariance check: a non-null element whose runtime type is
    // NOT assignment-compatible with the array's component type must throw
    // ArrayStoreException. It runs AFTER the null and bounds checks above and
    // BEFORE the barrier and store below — that order is the JVMS §6.5
    // precedence (NPE → AIOOBE → ASE), and getting it wrong is not theoretical:
    // `RArrayStoreTiers` s15 caught the interpreter fast path reporting ASE for
    // a past-the-end index.
    //
    // The rule itself lives in `aastore_store_is_refused`, shared with
    // `jit_aastore_type_check` — what the x64 inline lowering calls — so the
    // full helper and the inline path can never again enforce different rules.
    // It was EXTRACTED rather than copied, because the way this whole defect
    // arose was one JVMS rule implemented twice, each copy citing a property of
    // the other that the build could not check. There must stay exactly ONE
    // ArrayStoreException check and ONE message builder in this crate.
    //
    // A comment that used to stand here said "on x64 this arm does not run at
    // all, because nothing calls this function". That is still true of THIS
    // function: `jit/src/x64/bytecode_walk.rs`'s `0x53` arm lowers `aastore`
    // inline and calls `jit_aastore_type_check`, never `self.helpers.aastore`.
    // What is no longer true is the conclusion once drawn from it — that the
    // funnel routing below is not what the JIT prints. It is: the emitter's
    // check and this one are the same body. If that ever stops being so,
    // correct the prose in the same commit; a premise stated in a comment is
    // not a compile-time link, and this pair of comments spent months asserting
    // incompatible things about each other. See
    // docs/feature-designs/jdk-only-mode.md
    // §"Part 4" for the codegen change, and
    // docs/feature-designs/jit-ir-optimizations.md for the
    // defect itself.
    if val != 0 && aastore_store_is_refused(vm_ptr, array_ptr, val) {
        // Refused; an ArrayStoreException is pending on this thread. No
        // element is written and no barrier runs on the exception path.
        return;
    }
    // JIT round 23 wave 2, lane L4: while a ZGC load barrier is ARMED an
    // element slot holds a coloured word; the raw read and the plain store
    // below would mis-read it and write an uncoloured pointer. The heap's
    // own element store reads through the barrier (SATB), stores the coloured
    // word (`ZgcRealHeap::armed_store_ref_element`) and cards.
    if cratonvm_types::zgc_read_barrier_armed() {
        let heap = heap_from_vm(vm_ptr);
        if !(heap.is_generational() || heap.is_g1()) {
            let v = (val != 0).then(|| ObjectRef::from_raw(val as usize as *mut u8));
            let _ = heap.set_array_element(ObjectRef::from_raw(ptr), index as usize, Value::Object(v));
            return;
        }
    }
    let elem_ptr = ptr.add(HEADER_SIZE + index as usize * ref_element_size()) as *mut u8;
    // Task #43 (HIGH soundness, deferred from #25/#26): SATB pre-write
    // barrier — the JIT helper equivalent of the interpreter's
    // `shared.mem.heap.satb_barrier(old_value)` at runtime/interpreter.rs:4228
    // (aastore) and :5349 (aastore via set_array_element). Read the OLD
    // reference *before* the store so concurrent marking still sees a
    // path to the about-to-be-overwritten target (snapshot-at-the-
    // beginning). Without this the marker loses the only path to a
    // still-live object on every JIT-overwritten aastore, and the next
    // mixed evacuation turns the missed live into a use-after-free.
    //
    // `satb_barrier` is the inherent name for the pre-write barrier on
    // `VmHeap` in this codebase (the `GarbageCollector::write_barrier_pre`
    // trait alias is planned but not yet landed here — when it does, this
    // call should migrate to it for triad-pairing under the
    // `vm_heap.rs` debug-build assertion).
    let old_raw = read_ref_slot(elem_ptr);
    if old_raw != 0 {
        let heap = heap_from_vm(vm_ptr);
        let old_obj = ObjectRef::from_raw(old_raw as usize as *mut u8);
        // zt t32 (2f): the slot-aware form (plain words only under
        // load-barrier marking).
        heap.satb_barrier_slot(elem_ptr as *const u8, Value::Object(Some(old_obj)));
    }
    write_ref_slot(elem_ptr, val as u64);

    if val != 0 {
        let heap = heap_from_vm(vm_ptr);
        let obj_ref = ObjectRef::from_raw(ptr);
        let value = Value::Object(Some(ObjectRef::from_raw(val as usize as *mut u8)));
        // gen r4w4/cards4 (precise-array residual, item 1): the generational
        // barrier dirties the ELEMENT's card, as `set_array_element` has since
        // gen r4w3/cards3, so one compiled store into a wide old array no longer
        // re-scans every element on each young pause. It falls back to the
        // header card itself when the table is not precise
        // (`CRATONVM_GC_PRECISE_ARRAY_CARDS=0`). G1 and ZGC keep their own
        // barriers. `index` was bounds-checked above.
        match heap {
            VmHeap::Generational(generational) => {
                generational.write_barrier_ref_array_element(obj_ref, index as usize, value)
            }
            // zt t7/tg (GN-8): ZGC's generational card is per element granule
            // for a large old array under `CRATONVM_ZGC_GEN_ELEMENT_CARDS=1`
            // (`note_ref_store_element`, the interpreter `aastore`'s producer
            // since GN-3); every other case is the object card
            // `write_barrier` makes. The optimizing tier's gated `aastore`
            // sends every old-array store with a non-null value here.
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(zgc) => {
                zgc.note_ref_store_element(ptr as usize, elem_ptr as usize, length as usize)
            }
            _ => heap.write_barrier(obj_ref, value),
        }
    }
}

/// `multianewarray` with `dimensions == 2`.
///
/// SUPERSEDED by [`jit_multianewarray_n`], which the x64 emitter calls for
/// every arity including two. This slot stays wired because the helper table
/// is append-only (`jit-api/src/helpers_abi.rs`, ABI v14) and because a
/// two-dimensional call is still the shape most of the differential corpus
/// exercises; it delegates, so there is exactly one allocation path.
///
/// `site` packs the compile-time-constant description of the site:
/// `holder_class_id` in the low 32 bits, the constant-pool index of the array
/// class in the next 16. The emitter has both as immediates; passing them
/// instead of a pre-digested element type is what lets this helper reach the
/// SAME body the interpreter runs (`interpreter::multianewarray_alloc`), which
/// resolves the per-level component classes loader-faithfully.
///
/// It used to take a bare `leaf_et` element-type code and allocate every level
/// with `ClassId::new(0)`. That is a real miscompile, not a cosmetic one: the
/// outer array of a JIT-compiled `new String[a][b]` carried no class at all, so
/// `getClass()` read back `[Ljava.lang.Object;` and any `checkcast` to the
/// declared array type threw `ClassCastException`. Commons Math's
/// `DSCompiler.getCompiler` publishes such an array through an
/// `AtomicReference` and casts it back on the next call, which turned 118 of
/// `DerivativeStructureTest`'s 124 methods red under the JIT and none under
/// `--nojit`.
///
/// # Safety
/// Called from JIT-compiled code. `vm_ptr` must be a valid `SharedVm` pointer.
/// `dim1`/`dim2` are the two dimension sizes (outer, inner) as JIT stack slots.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_multianewarray_2d(
    vm_ptr: i64,
    site: i64,
    dim1: i64,
    dim2: i64,
) -> i64 {
    let f = || jit_multianewarray_2d_body(vm_ptr, site, dim1, dim2);
    contain("jit_multianewarray_2d", OnPanic::Throw { vm_ptr }, 0, f)
}

/// Body of [`jit_multianewarray_2d`], run under its panic guard.
///
/// Two dimensions read from registers, then the shared N-ary body. It carries
/// no allocation logic of its own: when this helper and the interpreter were
/// two transcriptions of JVMS §multianewarray only one of them resolved
/// component classes, and a compiled `new String[a][b]` came back as
/// `[Ljava.lang.Object;`. One body is the answer to that, and a second
/// arity-specific body here would be the same mistake one level down.
// SAFETY: Delegates to `jit_multianewarray_n_body` with stack-allocated dims array.
unsafe fn jit_multianewarray_2d_body(vm_ptr: i64, site: i64, dim1: i64, dim2: i64) -> i64 {
    let dims = [dim1, dim2];
    jit_multianewarray_n_body(vm_ptr, site, 2, dims.as_ptr())
}

/// `multianewarray` of ANY arity — the lowering the x64 emitter uses.
///
/// `dims_ptr` addresses `ndims` consecutive `i64` dimension counts, OUTERMOST
/// FIRST, in the caller's own frame. That indirection is the whole point of
/// this helper: [`jit_multianewarray_2d`] takes its two dimensions by value and
/// with `vm_ptr` and `site` that is all four Win64 integer argument registers,
/// so a third dimension had nowhere to go. The x64 scanner refused every
/// `multianewarray` with `ndims != 2` for exactly that reason — and a `jit_scan`
/// refusal bail-lists the WHOLE method permanently, at every door, so a single
/// `new byte[3][4][4]` made `RomulusEngine.skinny_128_384_plus_enc` (138x
/// HotSpot, 63% of `RomulusTest`'s samples) uncompilable for the life of the
/// process.
///
/// `site` packs the compile-time-constant description of the site exactly as
/// for the 2-D helper: `holder_class_id` in the low 32 bits, the array class's
/// constant-pool index in the next 16. It carries no arity, and does not need
/// to — `multianewarray_alloc` keys its plan cache on `(class, cp, ndims)`.
///
/// # Safety
/// Called from JIT-compiled code. `vm_ptr` must be a valid `SharedVm` pointer.
/// `dims_ptr` must address `ndims` readable, aligned `i64`s that stay valid for
/// the duration of the call — the emitter pins them into frame scratch slots
/// immediately before the call and reads nothing back afterwards.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_multianewarray_n(
    vm_ptr: i64,
    site: i64,
    ndims: i64,
    dims_ptr: *const i64,
) -> i64 {
    let f = || jit_multianewarray_n_body(vm_ptr, site, ndims, dims_ptr);
    contain("jit_multianewarray_n", OnPanic::Throw { vm_ptr }, 0, f)
}

/// The dimension count [`jit_multianewarray_n_body`] can read without spilling
/// to the heap. JVMS §4.4.1 caps an array type at 255 dimensions, so this is a
/// fast path and not a limit; anything above it takes a `Vec`. Three is the
/// deepest shape the JIT actually admits today (see
/// `MAX_JIT_MULTIANEWARRAY_DIMS` in `jit/src/x64/bytecode_compat.rs`), so the
/// `Vec` arm is defensive.
const MULTIANEWARRAY_INLINE_DIMS: usize = 8;

/// Body of [`jit_multianewarray_n`], run under its panic guard, and the single
/// allocation path for BOTH JIT helpers.
// SAFETY: Called under `contain` panic guard with valid JIT caller arguments.
// `dims_ptr` points to readable dimension counts; `vm_ptr` is validated.
unsafe fn jit_multianewarray_n_body(
    vm_ptr: i64,
    site: i64,
    ndims: i64,
    dims_ptr: *const i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Round-7 fix (CRIT, audit §3): SATB safepoint flush.
    jit_safepoint_flush_satb(vm_ptr);
    if vm_ptr == 0 {
        return 0;
    }
    // SAFETY: vm_ptr is a valid SharedVm pointer per the caller contract.
    let vm = &*(vm_ptr as *const SharedVm);

    // `ndims` is a compile-time constant the emitter takes from the bytecode
    // operand, so neither arm is reachable from a well-formed site. Both are
    // checked anyway: a wrong `ndims` here would read (or write nothing and
    // then index) arbitrary frame words, which is not a failure mode worth
    // trusting a `debug_assert!` with in a release build.
    if ndims < 1 || dims_ptr.is_null() {
        return jit_cp_alloc_internal_error(
            vm,
            "JIT multianewarray: dimension count out of range or no dimension buffer",
        );
    }
    // Widening: `ndims` is positive here and a bytecode operand upstream.
    let n = ndims as usize;

    let mut inline = [0usize; MULTIANEWARRAY_INLINE_DIMS];
    let mut spilled: Vec<usize> = Vec::new();
    if n > MULTIANEWARRAY_INLINE_DIMS {
        spilled = vec![0usize; n];
    }
    for i in 0..n {
        // BUGFIX (mirrors jit_newarray / jit_anewarray_object): narrow each
        // dimension to its int payload and sign-extend, defending against
        // NaN-boxed CompactValue raw bits leaking from JIT operand-stack slots.
        //
        // SAFETY: the caller contract is `ndims` readable aligned `i64`s.
        let d = (*dims_ptr.add(i)) as i32 as i64;
        if d < 0 {
            // JLS: NegativeArraySizeException, routed through the pending-
            // exception channel + the 0/null sentinel so the `multianewarray`
            // codegen's `emit_post_alloc_oom_check` bail hands it to the
            // method's exception table. Returning a bare 0 (what the 2-D
            // helper did before round 9 wave 6) pushed a null the compiled code
            // then dereferenced.
            //
            // OUTERMOST FIRST, which is the order the levels are allocated in:
            // `new int[-1][-2]` reports -1. The 2-D helper already reported the
            // outer dimension, so this preserves its answer.
            return jit_negative_array_size(vm, d);
        }
        // Widening: `d` is non-negative here.
        if n > MULTIANEWARRAY_INLINE_DIMS {
            spilled[i] = d as usize;
        } else {
            inline[i] = d as usize;
        }
    }
    let dims: &[usize] = if n > MULTIANEWARRAY_INLINE_DIMS {
        &spilled
    } else {
        &inline[..n]
    };

    // The index lock-free, for the plan-cache probe only; the pool is read at
    // the index judged under the guard it is read under, and every record is
    // keyed by that one (interpreter round i1 wave 44, lane L2;
    // `interpreter-L2-multianewarray-and-type-check-sites-judge-their-index-before-the-pool-read-FIXED-20261008`).
    let (cp_site, cp_index, judged_at) = multianewarray_site(vm, site);
    let holder_cid = cp_site.holder;
    let judge = |cm: &crate::classloading::ClassManager| cp_site.index_in(cm);
    let compiled = crate::runtime::interpreter::CompiledMultiANewArraySite {
        judged_at,
        index_in: &judge,
    };

    // Resolution can define array classes and run a user `ClassLoader`, i.e.
    // arbitrary Java on this thread, so it needs the real thread the way
    // `jit_resolve_cp_class` does. `emit_post_alloc_oom_check` forces
    // `has_dispatch` on every site that reaches here, so `JIT_THREAD` is set;
    // the `None` arm is purely defensive.
    let Some((thread, _guard)) = jit_thread_mut() else {
        return jit_cp_alloc_internal_error(
            vm,
            "JIT multianewarray: no live JIT thread to resolve the array class",
        );
    };

    match crate::runtime::interpreter::multianewarray_alloc_at_site(
        vm,
        thread,
        holder_cid,
        cp_index,
        Some(&compiled),
        dims,
    ) {
        Ok(arr) => arr.as_ptr() as i64,
        // Round 9 wave 6 (`vmhelp6`): `multianewarray_alloc` does NOT leave a
        // pending exception -- it RETURNS it: the heap-exhaustion arms of
        // `alloc_multi_array` return `InternalError(Runtime(OutOfMemoryError))`
        // and the dimension/descriptor guard returns the `VerifyError` object
        // as `ExceptionThrown`. This arm used to discard both (`Err(_) => 0`),
        // so the compiled code bailed on the 0 sentinel with NOTHING pending
        // and a `catch (OutOfMemoryError e)` around `new long[a][b]` never ran.
        // Stash it the way the interpreter throws it.
        Err(err) => jit_cp_alloc_stash_failure(vm, thread, err, "JIT multianewarray"),
    }
}

// SAFETY: Called from JIT-compiled code. array_ptr must be 0 (null) or a valid heap
// pointer to any array object. Null triggers a pending NPE + `i64::MIN` deopt
// sentinel (JVMS §arraylength requires NullPointerException on null).
pub unsafe extern "C" fn jit_arraylength(array_ptr: i64) -> i64 {
    contain("jit_arraylength", OnPanic::Deopt, i64::MIN, || {
        jit_arraylength_body(array_ptr)
    })
}

/// Body of [`jit_arraylength`]. Panic-guarded via [`contain`] with [`OnPanic::Deopt`].
unsafe fn jit_arraylength_body(array_ptr: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) array
    // reference identically: take the NPE path instead of dereferencing it
    // (the length/element read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // arrays always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(array_ptr as u64) {
        // JVMS §arraylength: throw NullPointerException on null array reference.
        // Previously returned -1, which JIT'd Java would happily compare against
        // and use as an array bound — masking real null-deref bugs in user code.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::ARRAY_LENGTH,
        );
        return i64::MIN;
    }
    // SAFETY: array_ptr is non-null and points to a live array on the GC heap.
    // ARRAY_LENGTH_OFFSET is the fixed offset to the u32 length field.
    let ptr = array_ptr as *const u8;
    let length_ptr = ptr.add(ARRAY_LENGTH_OFFSET) as *const u32;
    (*length_ptr) as i64
}

// ---------------------------------------------------------------------------
// Field access helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct CompactFieldProof {
    generation: u64,
    class_id: u32,
    index: usize,
    offset: usize,
    storage: cratonvm_types::FieldStorageKind,
}

thread_local! {
    // Layout metadata only. Generation changes retire every proof; object
    // flags are checked on every lookup so a legacy instance never uses one.
    static COMPACT_FIELD_PROOFS: [std::cell::Cell<Option<CompactFieldProof>>; 64] =
        const { [const { std::cell::Cell::new(None) }; 64] };
}

/// `(byte_offset, is_ref)` for a field of a **compact** object (one allocated
/// under `CRATONVM_COMPACT_REF_FIELDS`, marked `GC_FLAG_COMPACT` @ header byte
/// 21), else `None` for a legacy object. The JIT field helpers only get the raw
/// object pointer + resolved field index, so they read `class_id` (header
/// offset 0) and consult the per-class layout registry directly.
///
/// # Safety
/// `obj_ptr` must be non-null and point at a valid object header.
#[inline]
// SAFETY: callers validate that `obj_ptr` names a live object before using
// the compact-layout metadata derived from its header.
unsafe fn jit_compact_field_slot(
    obj_ptr: i64,
    field_index: i64,
) -> Option<(usize, cratonvm_types::FieldStorageKind)> {
    if field_index < 0 {
        return None;
    }
    // GC_FLAG_COMPACT is in the exported gc_flags byte.
    let gc_flags = std::ptr::read((obj_ptr as *const u8).add(cratonvm_types::GC_FLAGS_BYTE_OFFSET));
    if gc_flags & cratonvm_types::GC_FLAG_COMPACT == 0 {
        return None;
    }
    let class_id = std::ptr::read(obj_ptr as *const u32);
    let idx = field_index as usize;
    let generation = cratonvm_types::layout_generation();
    let bucket = (class_id as usize).wrapping_mul(31).wrapping_add(idx) & 63;
    if let Some(answer) = COMPACT_FIELD_PROOFS.with(|proofs| {
        let proof = proofs[bucket].get()?;
        (proof.generation == generation && proof.class_id == class_id && proof.index == idx)
            .then_some((proof.offset, proof.storage))
    }) {
        return Some(answer);
    }
    // A compact header names its current layout; classes with mutable shapes
    // are never compact. This canonical lookup checks the field index and
    // storage together, without resolving the same layout again for its count.
    // GC_FLAG_COMPACT also proves the registry's monotone domain ownership.
    let (offset, storage) = cratonvm_types::compact_field_disp_storage(class_id, idx)?;
    if generation == cratonvm_types::layout_generation() {
        COMPACT_FIELD_PROOFS.with(|proofs| {
            proofs[bucket].set(Some(CompactFieldProof {
                generation,
                class_id,
                index: idx,
                offset,
                storage,
            }))
        });
    }
    Some((offset, storage))
}

#[cfg(test)]
mod compact_field_proof_tests {
    use super::jit_compact_field_slot;
    use cratonvm_types::{
        register_class_layout, unregister_class_layout, ArrayElementType, ClassId, CompactLayout,
        FieldStorageKind, ObjectHeader, ObjectKind, FIRST_LAYOUT_DOMAIN,
    };
    use std::sync::Arc;

    #[test]
    fn field_proof_obeys_object_flags_bounds_replacement_and_unload() {
        let cid = 60_001;
        let register = |offset, storage| {
            register_class_layout(
                FIRST_LAYOUT_DOMAIN,
                cid,
                Arc::new(CompactLayout {
                    field_disps: vec![offset],
                    is_ref: vec![false],
                    field_kinds: vec![storage],
                    ref_disps: vec![],
                    total_size: 32,
                }),
            );
        };
        register(8, FieldStorageKind::Int);
        let mut header = ObjectHeader::new(
            ClassId::new(cid),
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            1,
        );
        let raw = (&header as *const ObjectHeader) as i64;
        unsafe {
            assert_eq!(jit_compact_field_slot(raw, 0), None);
        }
        header.set_compact_shape(1, 32);
        for _ in 0..3 {
            unsafe {
                assert_eq!(
                    jit_compact_field_slot(raw, 0),
                    Some((8, FieldStorageKind::Int))
                );
                assert_eq!(jit_compact_field_slot(raw, -1), None);
                assert_eq!(jit_compact_field_slot(raw, 1), None);
            }
        }
        register(16, FieldStorageKind::Long);
        unsafe {
            assert_eq!(
                jit_compact_field_slot(raw, 0),
                Some((16, FieldStorageKind::Long))
            );
        }
        unregister_class_layout(cid);
        unsafe {
            assert_eq!(jit_compact_field_slot(raw, 0), None);
        }
    }
}

/// Byte pointer to a **primitive** field's 16-byte cell, honouring the compact
/// layout (primitive cells stay 16 bytes, only at a packed offset). Legacy
/// objects use the uniform `index * SLOT_SIZE`.
///
/// # Safety
/// `obj_ptr` must be non-null, point at a valid object header, and `field_index`
/// must be within the object's slot count (callers bounds-check first).
#[inline]
// SAFETY: callers bounds-check `field_index`, so the returned address remains
// within the live object's allocated field storage.
unsafe fn jit_field_cell_ptr(
    obj_ptr: i64,
    field_index: i64,
) -> (*mut u8, Option<cratonvm_types::FieldStorageKind>) {
    let (off, storage) = match jit_compact_field_slot(obj_ptr, field_index) {
        Some((o, storage)) => (o, Some(storage)),
        None => (HEADER_SIZE + field_index as usize * SLOT_SIZE, None),
    };
    ((obj_ptr as *mut u8).add(off), storage)
}

/// `CRATONVM_DBG_WATCH_PUN=<class-name-substring>:<slot>` — the parsed watch,
/// for the **compiled store side**.
///
/// # Why this exists separately from the collector's copy of the same watch
///
/// `ZgcRealHeap::set_field_no_card` already carries a writer-side trap under
/// this exact variable, and the punned-`SQLChar.rawData` investigation used it
/// to conclude that nothing writes slot 1 at all: a run that demonstrably
/// produced the punned cell fired that watch **6879 times for slot 2** (the
/// positive control) and **zero times for slot 1**.
///
/// That conclusion followed from a false premise. The page recording it states
/// that "the `jit_putfield_*` helpers call `heap.set_field`" and are therefore
/// covered. They do not, and they are not: every one of [`jit_putfield_int`],
/// [`jit_putfield_long`], [`jit_putfield_float`] and [`jit_putfield_double`]
/// resolves the cell itself with [`jit_field_cell_ptr`] and writes it with
/// `write_value_atomic` / `write_compact_field` — no accessor, no lock, and no
/// watch. The collector-side trap is blind to the entire compiled putfield
/// family, which is exactly the population a JIT-written punned cell comes
/// from.
///
/// So the measured `0` was never evidence of absence; it was an instrument
/// pointed where the writer cannot be. This is the same watch, on the path the
/// other one cannot see.
fn jit_putfield_pun_watch() -> Option<&'static (String, usize)> {
    static W: std::sync::OnceLock<Option<(String, usize)>> = std::sync::OnceLock::new();
    W.get_or_init(|| {
        let spec = cratonvm_types::flags::runtime_var("CRATONVM_DBG_WATCH_PUN").ok()?;
        let (class, slot) = spec.rsplit_once(':')?;
        Some((class.to_string(), slot.parse().ok()?))
    })
    .as_ref()
}

/// How many primitive stores compiled code made into a slot its class declares
/// a **reference**. Counted only while [`jit_putfield_pun_watch`] is armed —
/// the reference-ness lookup re-reads the class-layout registry, which is not
/// something to put on the hot putfield path unmeasured.
pub static JIT_PUTFIELD_PRIMITIVE_INTO_REF_SLOT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Snapshot of [`JIT_PUTFIELD_PRIMITIVE_INTO_REF_SLOT`], or `None` when the
/// watch was never armed — a `0` from an unarmed counter and a `0` from an
/// armed one are different facts, and only one of them is evidence.
pub fn jit_putfield_primitive_into_ref_slot() -> Option<u64> {
    jit_putfield_pun_watch()
        .map(|_| JIT_PUTFIELD_PRIMITIVE_INTO_REF_SLOT.load(std::sync::atomic::Ordering::Relaxed))
}

/// Report a watched compiled field store, naming the compiled method that made
/// it and the frames underneath.
///
/// `Backtrace::force_capture` gives ~40 symbolized frames on Linux, which is
/// what turns "a punned cell exists" into "this method wrote it".
///
/// # Safety
/// `obj_ptr` must name a live object whose header is readable, and
/// `field_index` must already be bounds-checked against its slot count.
#[cold]
unsafe fn report_jit_punned_putfield(obj_ptr: i64, field_index: i64, value: Value) {
    let Some((want_class, want_slot)) = jit_putfield_pun_watch() else {
        return;
    };
    if field_index < 0 || field_index as usize != *want_slot {
        return;
    }
    let class_id = std::ptr::read(obj_ptr as *const u32);
    let name = cratonvm_gc::collector::class_name_for_diagnostics(class_id);
    if !name.contains(want_class.as_str()) {
        return;
    }
    // Whether the CLASS declares this slot a reference. `compact_field_slot`
    // answers from the registered layout, which exists for a class whether or
    // not this particular instance was allocated compact — the distinction that
    // made the original report ambiguous (`compact_flag=false` on an instance
    // of a class that does have a layout). `None` = no registered layout.
    let declared_ref =
        cratonvm_types::compact_field_disp(class_id, field_index as usize).map(|(_, r)| r);
    if declared_ref == Some(true) {
        JIT_PUTFIELD_PRIMITIVE_INTO_REF_SLOT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let gc_flags = std::ptr::read((obj_ptr as *const u8).add(cratonvm_types::GC_FLAGS_BYTE_OFFSET));
    let num_slots = (*(obj_ptr as *const cratonvm_types::ObjectHeader)).num_slots();
    eprintln!(
        "[punned-store-jit] class={name} class_id={class_id} num_slots={num_slots} \
         field_index={field_index} value={value:?} declared_ref={declared_ref:?} \
         compact_flag={} gc_flags={gc_flags:#x} jit_callee={} compiled_frames=[{}]\n{}",
        gc_flags & cratonvm_types::GC_FLAG_COMPACT != 0,
        current_jit_callee(),
        compiled_frames_above(),
        std::backtrace::Backtrace::force_capture(),
    );
}

/// The compiled methods whose bodies are on the stack above this helper, named.
///
/// `current_jit_callee()` is populated by `JitCalleeGuard`, which only
/// `jit_invoke_dispatch` constructs — so a callee reached by a RAW JIT-to-JIT
/// direct call has no name there at all, and the punned-store report came back
/// with `jit_callee=` empty every time. That is not a gap in the report, it is
/// the report telling you which call form you are looking at; but it leaves the
/// writer unnamed and the only way to the method was a deny bisect.
///
/// This walks the machine stack the same way the conservative root scanner
/// does — every word from here up, tested against the JIT code-range registry
/// — and names each compiled body it finds. Diagnostic-only, on a `#[cold]`
/// path behind `CRATONVM_DBG_JIT_PUTFIELD`, so its cost is irrelevant; and it
/// cannot fabricate an answer, because `pin_jit_code_range_owner` returns a
/// body only for an address a LIVE compiled artifact actually covers.
fn compiled_frames_above() -> String {
    // EXACT, via the frame-pointer chain -- not a conservative sweep.
    //
    // The scan this replaced tested every stack word in a window against the
    // JIT code-range table. It found frames sometimes and nothing at other
    // times on the same build, which is the worst property a witness can have:
    // its silence meant nothing. The chain answers the actual question -- which
    // return addresses are on this stack -- and answers it the same way every
    // time.
    //
    // Requires frame pointers in the Rust frames between here and
    // `jit_putfield_int`; build the diagnostic binary with
    // `RUSTFLAGS="-C force-frame-pointers=yes"`. Compiled JIT frames maintain
    // RBP already (`emit_post_call_rbp_republish` exists to keep it correct
    // across a raw JIT-to-JIT return), so the chain continues into them.
    let ranges = cratonvm_jit::jit_code_ranges_snapshot();
    if ranges.is_empty() {
        return "none: code-range table empty".to_string();
    }
    let mut rbp: usize;
    // The frame-pointer register, by name. x86-64 calls it RBP; AAPCS64 calls
    // it X29 and the chain has the same shape (saved FP at `[FP]`, return
    // address at `[FP+8]`), so the walk below is unchanged. Any other
    // architecture has no name for it here and the diagnostic says so rather
    // than walking a garbage value.
    #[cfg(target_arch = "x86_64")]
    // SAFETY: reads a register. No memory is accessed by the asm itself.
    unsafe {
        std::arch::asm!("mov {}, rbp", out(reg) rbp, options(nomem, nostack, preserves_flags));
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: reads a register. No memory is accessed by the asm itself.
    unsafe {
        std::arch::asm!("mov {}, x29", out(reg) rbp, options(nomem, nostack, preserves_flags));
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        return "none: no frame-pointer register known for this architecture".to_string();
    }
    let mut out: Vec<String> = Vec::new();
    let mut depth = 0usize;
    // 64 links is far past the handful of frames between this helper and the
    // compiled body that called it, and bounds a chain a clobbered RBP could
    // otherwise make cyclic.
    while depth < 64 && rbp > 0xffff && rbp % 8 == 0 {
        // SAFETY: `rbp` is a frame-pointer value read from the chain; each link
        // is `[saved_rbp, return_address]`. The bounds tests above reject the
        // obviously-invalid values a clobbered register would produce, and the
        // loop is depth-bounded.
        let (next, ret) = unsafe {
            (
                std::ptr::read_volatile(rbp as *const usize),
                std::ptr::read_volatile((rbp + 8) as *const usize),
            )
        };
        if let Some(&(entry, _)) = ranges.iter().find(|(e, end)| ret >= *e && ret < *end) {
            let name = cratonvm_jit::jit_code_range_method_key(ret).unwrap_or_default();
            out.push(format!(
                "{entry:#x}+{:#x}{}",
                ret - entry,
                if name.is_empty() {
                    String::new()
                } else {
                    format!("({name})")
                }
            ));
            if out.len() >= 6 {
                break;
            }
        }
        if next <= rbp {
            break; // the chain must grow upward; anything else is not a frame
        }
        rbp = next;
        depth += 1;
    }
    if out.is_empty() {
        format!("none: walked {depth} frames, {} ranges live", ranges.len())
    } else {
        out.join(", ")
    }
}

/// Armed-check for [`report_jit_punned_putfield`], inlined into the four
/// compiled putfield helpers so the default configuration pays one cached
/// `Option` test and nothing else.
///
/// # Safety
/// Same contract as [`report_jit_punned_putfield`].
#[inline]
unsafe fn note_jit_putfield_watch(obj_ptr: i64, field_index: i64, value: Value) {
    if jit_putfield_pun_watch().is_some() {
        report_jit_punned_putfield(obj_ptr, field_index, value);
    }
}

/// How many compiled `getfield` reads fell through the inline guard into
/// [`jit_getfield`]. See the counter's own comment there, and
/// `dispatch_counters`' neighbours for the house style.
pub static GETFIELD_HELPER_CALLS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// obj_ptr may be 0 (null), a valid heap pointer, or stale/corrupt raw bits from
// a miscompiled JIT frame; this helper validates it against the live heap before
// reading any object header. field_index is the resolved field slot index within
// the object layout. ptr::read is used because Value may contain non-Copy
// variants (ObjectRef).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
/// How many times compiled code fell through to this helper.
/// `CRATONVM_DBG_GETFIELD_RECEIVERS=1` — classify every helper receiver into
/// [`JIT_GETFIELD_RECEIVER_SHAPE`]. Off by default: the classification re-reads
/// the bounds table and the object header on a path taken tens of millions of
/// times, so it must not be in the measured configuration.
/// Is anything going to READ [`GETFIELD_HELPER_CALLS`] this run?
///
/// The increment is one relaxed atomic on the hottest helper in the VM, and
/// its original comment claimed "this is not visible in it". That claim was
/// never tested and was wrong: an ablation build measured it at ~2-3 ns of a
/// 9 ns reference-field read, i.e. a quarter to a third of the post-fix cost.
/// The counter is only ever printed under `CRATONVM_DBG=mic-prof` or
/// `CRATONVM_DBG=jit-method-stats` (and it is the denominator the receiver
/// census needs), so counting outside those buys nothing and costs the default
/// configuration.
///
/// Cached, because a per-call `runtime_var_os` on this exact path is the 3.4x
/// regression [`compact_inline_dbg`] documents.
fn getfield_census_counting_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_MIC_PROF")
            || cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_METHOD_STATS").is_some()
            || cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_GETFIELD_RECEIVERS")
    })
}

/// Cached `CRATONVM_DBG_COMPACT_INLINE` gate for the `jit_getfield` guard-failure
/// dump.
///
/// PERF (2026-08-18, the `BigDecimalBench` flag-read census): this gate was read
/// through `runtime_var_os` on EVERY `jit_getfield` helper call, and that is not
/// a cheap read — it hashes the name against the declared-flag set and then
/// falls through to `std::env::var_os`. A per-key census of a 50k-iteration
/// `BigDecimal` run counted **4,560,891 of 4,600,000 flag reads (99.1%) for this
/// one name**, ~91 per benchmark iteration.
///
/// Worth noting where it sat: the comment on `GETFIELD_HELPER_CALLS` directly
/// above the call site argues that one relaxed atomic increment is too cheap to
/// show up in the measured 8.2 ns `receiverFieldTax` — and it is right. The
/// uncached environment lookup on the very next line was the expensive one.
///
/// Same `OnceLock` idiom as [`getfield_receiver_census_enabled`] immediately
/// below, which is the sibling gate on the same path and was always cached.
fn compact_inline_dbg() -> bool {
    static CACHE: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&CACHE, || cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_COMPACT_INLINE"))
}

/// `CRATONVM_DBG_NULL_FIELD_PROVENANCE=<internal/class/Name>` — when a
/// reference field of that class reads back NULL through the getfield helper,
/// say what the RECEIVER actually is.
///
/// Written for the DoHead Generational residual, where
/// `java/util/concurrent/locks/ReentrantLock.sync` is null at `unlock()` having
/// been non-null at `lock()` — the slot is live entering the critical section
/// and zero leaving it, while the owning thread is blocked inside. "A final
/// field reads null" has several very different causes and the crash site
/// cannot tell them apart:
///
///   * the receiver is a STALE copy the collector already evacuated and the
///     sweep then zeroed (every slot reads zero, and the header may still
///     resolve) — a missed frame update, not a missed mark;
///   * the receiver is memory the collector RECLAIMED and handed to a new
///     allocation that has not run its constructor yet;
///   * the receiver is genuinely live and its slot was overwritten by
///     something writing through a stale address.
///
/// Those are distinguishable at the read, and nowhere else: `reclaimed_hole_at`
/// answers free-list membership outright, an all-zero body separates "swept
/// corpse" from "live object with one null field", and `live_holders_of` names
/// who still points at it.
///
/// Reached only with `CRATONVM_JIT_GETFIELD_HELPER=1`, since the inline
/// getfield never calls this helper.
fn null_field_provenance_watch() -> Option<&'static str> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            cratonvm_types::flags::runtime_var("CRATONVM_DBG_NULL_FIELD_PROVENANCE").ok()
        })
        .as_deref()
}

/// Bounded: a reclaimed span is read by many call sites and the first few
/// carry all of it.
static NULL_FIELD_PROVENANCE_REPORTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[cold]
fn note_null_field_provenance(vm_ptr: i64, obj_ptr: i64, field_index: usize) {
    let Some(want) = null_field_provenance_watch() else {
        return;
    };
    if obj_ptr == 0 {
        return;
    }
    // SAFETY: the helper's own contract — `vm_ptr` is the live `SharedVm` the
    // JIT was compiled against, and this runs on the same thread.
    let shared: &crate::vm::SharedVm = unsafe { &*(vm_ptr as *const crate::vm::SharedVm) };
    let addr = obj_ptr as usize;
    let obj = unsafe { cratonvm_types::ObjectRef::from_raw(obj_ptr as *mut u8) };
    if shared.mem.heap.is_object_address(addr).is_none() {
        return;
    }
    let cid = shared.mem.heap.class_id_of(obj);
    let name = {
        let Some(cm) = shared.classes.class_manager.try_read() else {
            return;
        };
        match cm.get_class(cid) {
            Some(c) => c.name.to_string(),
            None => return,
        }
    };
    // EXACT, not `contains`. The first run of this probe watched
    // "java/util/concurrent/locks/ReentrantLock" and matched
    // `ReentrantLock$NonfairSync` too, whose `head`/`tail`/`exclusiveOwnerThread`
    // are LEGITIMATELY null on an idle lock — 64 reports in a round with zero
    // failures, and they exhausted the report cap before anything real could be
    // seen. A watch that fires on healthy code is worse than no watch.
    if name != want {
        return;
    }
    if !matches!(
        shared.mem.heap.get_field(obj, field_index),
        cratonvm_types::Value::Object(None)
    ) {
        return;
    }
    let n = NULL_FIELD_PROVENANCE_REPORTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n >= 32 {
        return;
    }
    let nf = shared.mem.heap.num_fields(obj);
    let all_zero = (0..nf).all(|i| {
        matches!(
            shared.mem.heap.get_field(obj, i),
            cratonvm_types::Value::Object(None) | cratonvm_types::Value::Int(0)
        )
    });
    let hole = shared.mem.heap.reclaimed_hole_at(addr);
    let holders = shared.mem.heap.live_holders_of(addr, 6);
    eprintln!(
        "[null-field] {name}.slot{field_index} read NULL — receiver=0x{addr:x} \
         class_id={} num_fields={nf} whole_body_zero={all_zero}(degenerate when num_fields==1) reclaimed_hole={hole:?} \
         live_holders={holders:x?} (report {}/8)",
        cid.as_u32(),
        n + 1
    );
}

fn getfield_receiver_census_enabled() -> bool {
    static CACHE: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&CACHE, || cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_GETFIELD_RECEIVERS"))
}

/// Snapshot of [`GETFIELD_HELPER_CALLS`] for the shutdown diagnostic.
///
/// There is ONE counter, deliberately. This accessor briefly had its own
/// `JIT_GETFIELD_HELPER_CALLS` static, added in parallel with the identical
/// `GETFIELD_HELPER_CALLS` above; keeping both would have left whichever one
/// the helper stopped incrementing reading a confident `0` at shutdown, which
/// on a page about instruments that measure the wrong thing would have been a
/// poor way to go.
pub fn jit_getfield_helper_calls() -> Option<u64> {
    if !getfield_census_counting_enabled() {
        // NOT `Some(0)`. The counter is gated (see
        // `getfield_census_counting_enabled`), and a gated counter reported as
        // a number is indistinguishable from a fast path that never fell
        // through — which is the precise misreading this whole counter exists
        // to prevent.
        return None;
    }
    Some(GETFIELD_HELPER_CALLS.load(std::sync::atomic::Ordering::Relaxed))
}

/// Why each helper call arrived: the receiver's own shape, counted at
/// EXECUTION.
///
/// Every previous attempt on this page counted EMISSIONS — which arms emitted a
/// `CALL`, how many sites declined to inline — and emission counts cannot
/// answer "which branch runs". Four hypotheses died that way. These four
/// counters are on the one path every fall-through must cross, so their sum is
/// exactly [`GETFIELD_HELPER_CALLS`] and no site can hide from them.
///
/// The clauses are the inline guards, in the order both backends emit them:
///
///   0. `implausible` — null, unaligned, or not a plausible heap pointer.
///      Both backends' clause 1/2.
///   1. `out-of-bounds` — plausible, but outside every published
///      `JIT_REGION_BOUNDS` region. Both backends' clause 3, and the clause the
///      page's title blamed. On a backend that publishes nothing this is every
///      receiver; the page already showed it is NOT what separates the arms.
///   2. `legacy-layout` — in bounds, but `GC_FLAG_COMPACT` is CLEAR, so the
///      object is a uniform 16-byte-cell instance. **The single-pass arm reads
///      this inline; the IR arm sends it to the helper.** That asymmetry is
///      documented in `ir_lower::emit_inline_compact_getfield` as "the one
///      simplification against the single-pass version" and is invisible to
///      every collector A/B, because per-object compactness is not a collector
///      property.
///   3. `compact-eligible` — passed all three, i.e. a receiver the inline path
///      *should* have read without calling anyone. A non-zero count here means
///      a guard is genuinely misbehaving; a zero means every call has a reason.
pub static JIT_GETFIELD_RECEIVER_SHAPE: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Names for [`JIT_GETFIELD_RECEIVER_SHAPE`], index-parallel.
pub const JIT_GETFIELD_RECEIVER_SHAPE_NAMES: [&str; 4] = [
    "implausible-or-null",
    "outside-published-bounds",
    "legacy-layout-receiver",
    "compact-eligible",
];

/// `(name, count)` for every receiver shape seen at least once.
pub fn jit_getfield_receiver_shapes() -> Vec<(&'static str, u64)> {
    JIT_GETFIELD_RECEIVER_SHAPE_NAMES
        .iter()
        .zip(JIT_GETFIELD_RECEIVER_SHAPE.iter())
        .map(|(n, c)| (*n, c.load(std::sync::atomic::Ordering::Relaxed)))
        .collect()
}

/// Among the calls that failed the CONTAINMENT clause, how many read a
/// REFERENCE field and how many a primitive. Index 0 = primitive, 1 = reference.
///
/// This is the fork that decides whether anything further is reachable on ZGC
/// and G1. Those two collectors publish no region bounds by design — the empty
/// `JIT_REGION_BOUNDS` is the interlock that keeps inline reference STORES
/// unreachable there (`g1-audit.md` §8.1) — so containment can only be
/// bypassed by a receiver check that does not need it. The IR tier now has one
/// (`emit_trusted_oop_receiver_check`, primitives only), and the single-pass
/// arm has always had one.
///
///   * a REFERENCE-heavy remainder means the rest is blocked on the ZGC JIT
///     load barrier (`feature-designs/zgc-jit-load-barrier.md`), because a
///     compact reference slot there may hold `Z_COLORED_TAG | colour | offset`
///     and inlining its load is the use-after-free that design exists to stop.
///     Nothing to fix in the getfield arms.
///   * a PRIMITIVE-heavy remainder means some arm is failing to take a shortcut
///     it is already entitled to, and that is an ordinary bug.
pub static JIT_GETFIELD_OOB_FIELD_KIND: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// `(name, count)` for [`JIT_GETFIELD_OOB_FIELD_KIND`].
pub fn jit_getfield_oob_field_kinds() -> Vec<(&'static str, u64)> {
    use std::sync::atomic::Ordering;
    vec![
        (
            "primitive",
            JIT_GETFIELD_OOB_FIELD_KIND[0].load(Ordering::Relaxed),
        ),
        (
            "reference",
            JIT_GETFIELD_OOB_FIELD_KIND[1].load(Ordering::Relaxed),
        ),
    ]
}

/// Is the field at `field_index` of the object at `addr` a reference?
///
/// Asks the object's OWN layout, the same way the helper's read does: a compact
/// object through the registered layout's storage kind, a legacy one through
/// the 16-byte `Value` cell's tag byte. Anything it cannot resolve counts as a
/// primitive, which is the conservative direction here — it under-reports the
/// reference bucket rather than inventing one.
///
/// SAFETY: `addr` must have passed `plausible_heap_pointer` and point at a live
/// object header whose body covers `field_index`.
unsafe fn getfield_field_is_reference(addr: usize, field_index: i64) -> bool {
    let Ok(idx) = usize::try_from(field_index) else {
        return false;
    };
    let header = &*(addr as *const cratonvm_types::ObjectHeader);
    if idx >= header.num_slots() as usize {
        return false;
    }
    if let Some((_, storage)) = jit_compact_field_slot(addr as i64, field_index) {
        return storage == cratonvm_types::FieldStorageKind::Reference;
    }
    let tag = *((addr
        + cratonvm_types::HEADER_SIZE
        + idx * cratonvm_types::SLOT_SIZE
        + cratonvm_types::FIELD_CELL_TAG_OFFSET) as *const u8);
    tag == cratonvm_types::VTAG_OBJECT || tag == cratonvm_types::VTAG_NULL
}

/// Classify the receiver of one helper call into
/// [`JIT_GETFIELD_RECEIVER_SHAPE`].
///
/// Deliberately re-derives each clause from the same words the emitted guards
/// compare against, rather than from anything the helper has already computed:
/// the point is to answer what the INLINE code decided, and the helper's own
/// validation is a different (stricter) test.
///
/// SAFETY: `vm_ptr` must be the live `SharedVm` the helper was called with.
/// Only dereferences `obj_ptr` after `plausible_heap_pointer` AND the helper's
/// own validator (`VmHeap::is_object_address`, which consults the commit
/// bitmaps) have both passed.
///
/// gen r5w1/young5 (2026-09-26), item 3 of
/// `docs/feature-designs/generative-gc-architecture.md`:
/// the precondition used to be "plausible and inside a published read
/// region", and neither half made the header readable: an address outside
/// every region was dereferenced on `plausible_heap_pointer` alone (the
/// field-kind split), and the young read region is the whole semi-space
/// CAPACITY, including granules the young uncommit released. With the census
/// on, a stale receiver the helper would have refused faulted here instead.
/// Slot 0 now also counts "not an object the helper would accept".
#[inline(never)]
#[cold]
unsafe fn note_getfield_receiver_shape(vm_ptr: i64, obj_ptr: i64, field_index: i64) {
    use std::sync::atomic::Ordering;
    let bump = |i: usize| {
        JIT_GETFIELD_RECEIVER_SHAPE[i].fetch_add(1, Ordering::Relaxed);
    };
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
        bump(0);
        return;
    }
    // SAFETY: the caller's contract: `vm_ptr` is the live `SharedVm`.
    let vm = &*(vm_ptr as *const SharedVm);
    if vm.mem.heap.is_object_address(obj_ptr as usize).is_none() {
        bump(0);
        return;
    }
    // `JIT_READ_BOUNDS`, not `JIT_REGION_BOUNDS`: the getfield receiver guard
    // (`emit_guarded_getfield_receiver_check` and `ir_lower`'s copy) reads the
    // READ table, and this census exists to say what that guard decided. The
    // store table is empty under ZGC and G1 by design, so reading it labelled
    // every ZGC receiver `outside-published-bounds` (r9w3 vmhelpers3, per
    // `NOTES-w3-x64obj3.md` cross-lane request 4).
    let base = cratonvm_gc::jit_read_bounds_addr();
    let word = |i: usize| -> usize {
        // SAFETY: `jit_read_bounds_addr` returns the address of the
        // process-global `#[repr(C)]` six-`AtomicUsize` `JIT_READ_BOUNDS`
        // static, which lives for the program's lifetime.
        let w = &*((base + i * 8) as *const std::sync::atomic::AtomicUsize);
        w.load(Ordering::Acquire)
    };
    let addr = obj_ptr as usize;
    let contained = (0..3).any(|r| {
        let (b, e) = (word(r * 2), word(r * 2 + 1));
        // An all-zero pair matches nothing, which is the documented state for a
        // backend that never publishes.
        b != 0 && addr >= b && addr < e
    });
    if !contained {
        bump(1);
        // Split the containment failures by field kind — see
        // `JIT_GETFIELD_OOB_FIELD_KIND`. The receiver passed the helper's own
        // object validation above, so its header and body are readable, which
        // is all this needs; it is NOT inside a
        // published read region (a collector that publishes none — or ZGC with
        // its barrier armed — leaves the table empty, so this alone says
        // nothing about whether the memory is mapped).
        let is_ref = getfield_field_is_reference(addr, field_index);
        JIT_GETFIELD_OOB_FIELD_KIND[usize::from(is_ref)].fetch_add(1, Ordering::Relaxed);
        return;
    }
    let flags = *((addr + cratonvm_types::GC_FLAGS_BYTE_OFFSET) as *const u8);
    if flags & cratonvm_types::GC_FLAG_COMPACT == 0 {
        bump(2);
        note_legacy_receiver_class(addr);
    } else {
        bump(3);
        note_compact_eligible_receiver(addr, field_index);
    }
}

/// Per-`(class, field index)` tally of the `compact-eligible` receivers: the
/// ones every inline guard clause should have passed. The shape census can
/// only say such calls exist; this names the field, which names the site.
/// A fixed 1024-slot open-addressed table (no allocation, no lock on a path
/// taken tens of millions of times), names captured on first sight as
/// [`LEGACY_RECEIVER_CLASSES`] does; keys past a full table are dropped, and
/// the printout is the top twenty by count.
const COMPACT_ELIGIBLE_SLOTS: usize = 1024;
static COMPACT_ELIGIBLE_RECEIVERS: [(
    std::sync::atomic::AtomicU64,
    std::sync::atomic::AtomicU64,
    std::sync::OnceLock<String>,
); COMPACT_ELIGIBLE_SLOTS] = [const {
    (
        std::sync::atomic::AtomicU64::new(u64::MAX),
        std::sync::atomic::AtomicU64::new(0),
        std::sync::OnceLock::new(),
    )
}; COMPACT_ELIGIBLE_SLOTS];

/// Record one compact-eligible receiver. `addr` must already have passed the
/// plausibility, object and containment tests.
unsafe fn note_compact_eligible_receiver(addr: usize, field_index: i64) {
    use std::sync::atomic::Ordering;
    let header = &*(addr as *const cratonvm_types::ObjectHeader);
    let cid = header.class_id.as_u32();
    let key = (u64::from(cid) << 32) | (field_index as u64 & 0xFFFF_FFFF);
    let start = (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 54) as usize;
    for probe in 0..COMPACT_ELIGIBLE_SLOTS {
        let (slot_key, count, name) =
            &COMPACT_ELIGIBLE_RECEIVERS[(start + probe) % COMPACT_ELIGIBLE_SLOTS];
        let cur = slot_key.load(Ordering::Relaxed);
        if cur == key {
            count.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if cur == u64::MAX
            && slot_key
                .compare_exchange(u64::MAX, key, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let disp = jit_compact_field_slot(addr as i64, field_index);
            let _ = name.set(format!(
                "{}.field#{field_index} compact_slot={disp:?}",
                cratonvm_gc::gc::resolve_class_info(cid)
                    .map(|(n, _)| n)
                    .unwrap_or_else(|| "<unresolved>".to_string())
            ));
            count.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if slot_key.load(Ordering::Relaxed) == key {
            count.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
}

/// `(description, count)` for the twenty compact-eligible receiver keys seen
/// most often.
pub fn jit_getfield_compact_eligible_receivers() -> Vec<(String, u64)> {
    use std::sync::atomic::Ordering;
    let mut rows: Vec<(String, u64)> = COMPACT_ELIGIBLE_RECEIVERS
        .iter()
        .filter(|(key, _, _)| key.load(Ordering::Relaxed) != u64::MAX)
        .map(|(_, count, name)| {
            (
                name.get()
                    .cloned()
                    .unwrap_or_else(|| "<unnamed>".to_string()),
                count.load(Ordering::Relaxed),
            )
        })
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    rows.truncate(20);
    rows
}

/// Per-class tally of the receivers that reached the helper with
/// `GC_FLAG_COMPACT` clear.
///
/// "The receivers are legacy" is a shape, not a diagnosis — the actionable
/// question is WHICH class allocates legacy on a path this hot, because
/// `CRATONVM_DBG_COMPACT_LEGACY`'s allocation-side census answers a different
/// one (it names classes with no matching layout, and a class can be absent
/// there and still be the receiver here).
///
/// Sixteen slots, linear scan, first-come. Small and fixed on purpose: this
/// runs under the same off-by-default flag as its caller and must not allocate
/// or lock on a path taken tens of millions of times.
///
/// The NAME is captured on first sight, not at print time. `resolve_class_info`
/// answers through `live_hook_vms()` and returns `None` "after every registered
/// VM has been dropped" — which is exactly the state a shutdown diagnostic runs
/// in, and it is why the first version of this tally printed
/// `<unresolved>(id=440)` for a class the allocation-side census had no trouble
/// naming.
static LEGACY_RECEIVER_CLASSES: [(
    std::sync::atomic::AtomicU32,
    std::sync::atomic::AtomicU64,
    std::sync::OnceLock<String>,
); 16] = [const {
    (
        std::sync::atomic::AtomicU32::new(u32::MAX),
        std::sync::atomic::AtomicU64::new(0),
        std::sync::OnceLock::new(),
    )
}; 16];

/// Record one legacy receiver against its class id. `addr` must already have
/// passed the plausibility and containment tests.
unsafe fn note_legacy_receiver_class(addr: usize) {
    use std::sync::atomic::Ordering;
    let header = &*(addr as *const cratonvm_types::ObjectHeader);
    let cid = header.class_id.as_u32();
    let slots = header.num_slots();
    for (slot_cid, count, name) in LEGACY_RECEIVER_CLASSES.iter() {
        let cur = slot_cid.load(Ordering::Relaxed);
        if cur == cid {
            count.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if cur == u32::MAX
            && slot_cid
                .compare_exchange(u32::MAX, cid, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            // Does a compact layout EXIST for this (class, field-count)? That
            // separates the two stories a legacy receiver can tell: "no layout
            // was ever registered for this shape" (an allocation-side gap) from
            // "a matching layout exists and this object was still allocated
            // legacy" (an allocation that bypassed or predated it). Neither
            // legacy census fires for `SHA256Digest`, so this is the question
            // left standing.
            let registered = cratonvm_types::class_layout(cid).map(|l| l.field_count());
            // `array_length` and the raw flags nibble separate the last two
            // stories. `plan_object_alloc` writes `(body_size, GC_FLAG_COMPACT)`
            // for a compact object and `(0, 0)` for a legacy one, so:
            //   array_length != 0 with the COMPACT bit clear  => allocated
            //     compact and the bit was CLEARED afterwards;
            //   array_length == 0                             => allocated
            //     legacy, and the allocation-side census should have said so.
            // The whole nibble is printed because a promoted or marked object
            // carries other bits, and an all-zero nibble on a live object is
            // itself a signal.
            let arr_len = header.array_length();
            let raw_flags = header.gc_flags();
            let _ = name.set(format!(
                "{} num_slots={slots} registered_layout_fields={registered:?} \
                 array_length={arr_len} gc_flags={raw_flags:#04x}",
                cratonvm_gc::gc::resolve_class_info(cid)
                    .map(|(n, declared)| format!("{n} declared_fields={declared}"))
                    .unwrap_or_else(|| "<unresolved>".to_string())
            ));
            count.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
}

/// `(class name, class id, count)` for every class seen as a legacy receiver.
pub fn jit_getfield_legacy_receiver_classes() -> Vec<(String, u32, u64)> {
    use std::sync::atomic::Ordering;
    LEGACY_RECEIVER_CLASSES
        .iter()
        .filter_map(|(cid, count, name)| {
            let cid = cid.load(Ordering::Relaxed);
            if cid == u32::MAX {
                return None;
            }
            let n = count.load(Ordering::Relaxed);
            let name = name
                .get()
                .cloned()
                .unwrap_or_else(|| "<unnamed>".to_string());
            Some((name, cid, n))
        })
        .collect()
}

/// First few helper calls, dumped with the receiver AND the live bounds table.
///
/// The counter says the inline guard fell through; this says WHY. The guard is
/// null-check, 8-alignment, then containment in one of three `[base, end)`
/// pairs — so printing the receiver beside all six words names the failing
/// clause directly instead of leaving it to be inferred from collector A/Bs,
/// which came back identical on ZGC, Generational and G1 and therefore ruled
/// out the "ZGC never publishes" story on their own.
#[inline(never)]
#[cold]
fn dump_getfield_guard_failure(obj_ptr: i64) {
    use std::sync::atomic::Ordering;
    static DUMPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if DUMPED.fetch_add(1, Ordering::Relaxed) >= 8 {
        return;
    }
    // The READ table — the one the getfield guard compares against (see
    // `note_getfield_receiver_shape`).
    let base = cratonvm_gc::jit_read_bounds_addr();
    // SAFETY: `jit_read_bounds_addr` returns the address of the process-global
    // `#[repr(C)]` `JIT_READ_BOUNDS` static, which is six `AtomicUsize` and
    // lives for the program's lifetime.
    let words: Vec<String> = (0..6)
        .map(|i| unsafe {
            let w = &*((base + i * 8) as *const std::sync::atomic::AtomicUsize);
            format!("{:#x}", w.load(Ordering::Acquire))
        })
        .collect();
    eprintln!(
        "[getfield-guard] receiver={obj_ptr:#x} aligned8={} bounds=[{}]",
        obj_ptr & 7 == 0,
        words.join(", ")
    );
}

/// Census of which JIT helper still performs an `is_object_address` heap
/// membership walk, and how often.
///
/// The getfield arm stopped asking (see `GETFIELD_RECEIVER_PROVEN_OOP`), and
/// roughly half the walk traffic survived. `perf` could not attribute the
/// remainder: DWARF unwinding on this optimized build returns self-recursive
/// frames, and LBR is unavailable on the virtualised PMU. So count it.
///
/// Index: 0 = `jit_getfield_impl`, 1 = `try_jit_site_cached_native_dispatch`,
/// 2 = `decode_dispatch_values_into`, 3 = `jit_invoke_dispatch`,
/// 4 = `jit_checkcast`, 5 = `jit_instanceof`, 6 = everything else.
///
/// The counters themselves live per-thread in [`JitCounterBlock`] — this is
/// only the naming. They used to be one process-global `AtomicU64` array and a
/// `fetch_add` per walk, which `jit_native_dispatch_profile` priced at 4-5 ns
/// on a path that also does the ~3 ns walk being counted.
///
/// Index-parallel with [`JitCounterBlock::membership_walk`].
pub const MEMBERSHIP_WALK_SITE_NAMES: [&str; 7] = [
    "getfield",
    "native-dispatch-cached",
    "native-dispatch-decode-args",
    "invoke-dispatch",
    "checkcast",
    "instanceof",
    "other",
];

/// One thread's tallies for the always-on JIT dispatch counters.
///
/// # Why these are not `fetch_add` on a process-global any more
///
/// Three of them fire on every site-cached native dispatch from compiled code
/// — the membership-walk site counter, the leaf/non-leaf hit counter, and the
/// registry's per-slot `record_invocation`. Each was a `lock xadd`, which on
/// x86 is a full barrier rather than a store, and
/// `jit_native_dispatch_profile` prices them at 4-5 ns EACH and 7-10 ns for the
/// pair-plus-one a dispatch actually pays — against a ~50 ns dispatch. That is
/// a diagnostic costing more than several of the things it is used to measure.
///
/// The fix keeps every count EXACT rather than gating or sampling. Each thread
/// owns its own block and is the only writer; it increments with a relaxed
/// load-add-store, which is an ordinary store with no lock prefix. Readers walk
/// [`JIT_COUNTER_BLOCKS`] and sum. The fields stay atomic only so that a reader
/// on another thread is not a data race — no reader needs a consistent instant
/// across counters, and none of them ever did.
///
/// Blocks are never removed: a thread that exits leaves its counts behind,
/// which is exactly what a whole-run census wants.
#[derive(Default)]
struct JitCounterBlock {
    membership_walk: [std::sync::atomic::AtomicU64; MEMBERSHIP_WALK_SITE_NAMES.len()],
    leaf_native_hits: std::sync::atomic::AtomicU64,
    site_cached_native_hits: std::sync::atomic::AtomicU64,
    /// `VarHandle` direct-helper `(served, declined)` pairs, indexed by
    /// [`VH_DIRECT_READ`] / [`VH_DIRECT_WRITE`] / [`VH_DIRECT_CAS`].
    ///
    /// r9-vmside: these were six process-global `AtomicU64`s, and a served
    /// call paid TWO `lock xadd`s (its own hit counter plus
    /// `JIT_FUNNEL_BYPASS_HITS`) on the thin path whose whole point is to cost
    /// less than the funnel. `VarHandle` CAS/read/write is the substrate of
    /// every `j.u.c` lock and atomic, i.e. the one call that N threads issue
    /// against each other at once — so the two shared lines bounced between
    /// every core doing it, a cost that grows with the thread count the
    /// counters exist to observe. Same cure, and same exactness, as the fields
    /// above.
    vh_direct_served: [std::sync::atomic::AtomicU64; 3],
    vh_direct_declined: [std::sync::atomic::AtomicU64; 3],
    /// Compiled-code native-funnel bypasses. See [`jit_funnel_bypass_count`].
    funnel_bypass_hits: std::sync::atomic::AtomicU64,
    /// In-funnel `VarHandle` field fast paths: instance-field READs served,
    /// and CAS `(served, declined)`. See [`varhandle_field_cas_counts`].
    vh_field_read_hits: std::sync::atomic::AtomicU64,
    vh_field_cas_served: std::sync::atomic::AtomicU64,
    vh_field_cas_declined: std::sync::atomic::AtomicU64,
    /// In-funnel `VarHandle` ARRAY-ELEMENT accesses on a real `int[]` / `long[]`
    /// handle served as one hardware access. See
    /// [`varhandle_array_element_served_count`].
    vh_array_element_served: std::sync::atomic::AtomicU64,
    /// Site-cached `ArrayList.get`/`size` hits served by the no-wrapper
    /// collection probes. See [`jit_collection_leaf_hit_count`].
    collection_leaf_hits: std::sync::atomic::AtomicU64,
    /// Guarded thin direct-helper calls refused / admitted under `JdkOnly`.
    /// See [`jit_direct_helper_jdk_only_refusals`].
    ///
    /// Round 11 (lane `rt`): these were two process-global `AtomicU64`s bumped
    /// on EVERY call into ~25 guarded helpers (`HashMap.get`/`put`,
    /// `MessageDigest.update(byte)`, the `ByteBuffer` and `ScopedMemoryAccess`
    /// accessors, `Thread.currentThread`) whenever the VM is `--jdk-only` —
    /// the launcher default — and on every `Unsafe` accessor call in every
    /// mode. One contended `lock xadd` per call on a helper whose whole point
    /// is to be cheaper than the funnel; the same cure as the fields above.
    ///
    /// t1 wave 6, lane Y3: the two tallies now live in the calling VM's
    /// `JitCache::jdk_only_sinks` (a single-writer
    /// `cratonvm_jit::jdk_only_sink::DirectHelperGateTally` this thread
    /// registered there), because summing them over every thread of the
    /// process put every other VM's calls in a VM's `--jdk-only-report`. The
    /// block keeps this thread's CURRENT tally and the VM it belongs to, so the
    /// per-call cost is one compare more than before (see
    /// [`JitCounterBlock::bump_direct_helper`]).
    ///
    /// `direct_helper_owner` is the `vm_identity` (never reused) of the VM
    /// `direct_helper_tally` belongs to; both are written and read only by the
    /// owning thread.
    direct_helper_owner: std::sync::atomic::AtomicUsize,
    direct_helper_tally:
        std::sync::atomic::AtomicPtr<cratonvm_jit::jdk_only_sink::DirectHelperGateTally>,
    /// Every tally this thread ever registered, with the `vm_identity` it was
    /// registered for: at most ONE per VM (t1 wave 7, lane V6: a thread
    /// alternating between two VMs used to register a fresh tally on every
    /// switch, growing this list and the VM's sink without bound; a switch
    /// back now finds its old tally). Holding them here keeps
    /// `direct_helper_tally` valid for the thread's whole life whatever
    /// happens to the VM, and lets the process census
    /// ([`jit_direct_helper_jdk_only_refusals_in_process`]) sum them. Locked
    /// only on a VM switch and by readers.
    direct_helper_tallies: parking_lot::Mutex<
        Vec<(usize, std::sync::Arc<cratonvm_jit::jdk_only_sink::DirectHelperGateTally>)>,
    >,
    /// Dispatches the exact-receiver `Matcher` leaf served. See
    /// [`matcher_leaf_hit_count`].
    matcher_leaf_hits: std::sync::atomic::AtomicU64,
    /// JIT round 21 lane reflect4 (REFLECT4-21-3): the compiled
    /// signature-polymorphic link's outcomes, one slot per
    /// [`MH_LINK_CENSUS_REASONS`] row. Bumped only under
    /// `CRATONVM_DBG_MH_LINK_CENSUS`. See [`mh_link_census_lines`].
    mh_link_census: [std::sync::atomic::AtomicU64; MH_LINK_CENSUS_REASONS.len()],
    /// JIT round 21 lane reflect4 (REFLECT4-21-1): the bytecode-only door's
    /// outcomes, one slot per [`BYTECODE_ONLY_CENSUS_REASONS`] row. Bumped
    /// only under `CRATONVM_DBG_BYTECODE_ONLY_BODY`. See
    /// [`bytecode_only_census_lines`].
    bytecode_only_census: [std::sync::atomic::AtomicU64; BYTECODE_ONLY_CENSUS_REASONS.len()],
}

/// [`JitCounterBlock::vh_direct_served`] / `vh_direct_declined` index for the
/// `VarHandle` READ direct helpers.
const VH_DIRECT_READ: usize = 0;
/// As [`VH_DIRECT_READ`], for the WRITE direct helpers.
const VH_DIRECT_WRITE: usize = 1;
/// As [`VH_DIRECT_READ`], for the CAS direct helpers.
const VH_DIRECT_CAS: usize = 2;

/// This thread's +1 for a `VarHandle` direct helper that served (`served`) or
/// declined to the generic dispatcher. See [`JitCounterBlock`].
#[inline]
fn note_vh_direct(kind: usize, served: bool) {
    JIT_COUNTERS.with(|b| {
        let counters = if served {
            &b.vh_direct_served
        } else {
            &b.vh_direct_declined
        };
        if let Some(c) = counters.get(kind) {
            JitCounterBlock::bump(c);
        }
    });
}

/// `(served, declined)` summed over every thread, for [`note_vh_direct`]'s
/// `kind`.
fn vh_direct_counts(kind: usize) -> (u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        jit_counter_total(|b| b.vh_direct_served.get(kind).map_or(0, |c| c.load(Relaxed))),
        jit_counter_total(|b| {
            b.vh_direct_declined
                .get(kind)
                .map_or(0, |c| c.load(Relaxed))
        }),
    )
}

/// This thread's +1 for a compiled-code native-funnel bypass. See
/// [`jit_funnel_bypass_count`].
#[inline]
fn note_funnel_bypass_hit() {
    JIT_COUNTERS.with(|b| JitCounterBlock::bump(&b.funnel_bypass_hits));
}

impl JitCounterBlock {
    /// This thread's own +1. Sound only from the owning thread — see the type
    /// doc for why that is the whole point.
    #[inline]
    fn bump(counter: &std::sync::atomic::AtomicU64) {
        use std::sync::atomic::Ordering::Relaxed;
        counter.store(counter.load(Relaxed).wrapping_add(1), Relaxed);
    }

    /// This thread's +1 on `vm`'s guarded direct-helper tally (refused or
    /// admitted). Owning thread only. The hot path is two relaxed loads, a
    /// compare and the tally's load-add-store; a thread that starts serving
    /// another VM registers a fresh tally with that VM's sinks once.
    #[inline]
    fn bump_direct_helper(&self, vm: &SharedVm, refused: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        let p = self.direct_helper_tally.load(Relaxed);
        let tally = if !p.is_null() && self.direct_helper_owner.load(Relaxed) == vm.vm_identity {
            // SAFETY: `p` came from an `Arc` this block still holds in
            // `direct_helper_tallies` (never removed), so it is alive.
            unsafe { &*p }
        } else {
            self.adopt_direct_helper_tally(vm)
        };
        tally.bump_from_owner(refused);
    }

    /// Make this thread's tally for `vm` its current one: the one it already
    /// registered for `vm` (a switch back), else a fresh one registered now.
    #[cold]
    #[inline(never)]
    fn adopt_direct_helper_tally(
        &self,
        vm: &SharedVm,
    ) -> &cratonvm_jit::jdk_only_sink::DirectHelperGateTally {
        use std::sync::atomic::Ordering::Relaxed;
        let known = self
            .direct_helper_tallies
            .lock()
            .iter()
            .find(|(id, _)| *id == vm.vm_identity)
            .map(|(_, t)| std::sync::Arc::as_ptr(t));
        let p = match known {
            Some(p) => p,
            None => {
                // Registered with the VM outside this block's lock (only this
                // thread pushes here, so nothing can slip in between).
                let tally = vm
                    .jit
                    .jit_cache
                    .jdk_only_sinks()
                    .register_direct_helper_tally();
                let p = std::sync::Arc::as_ptr(&tally);
                self.direct_helper_tallies
                    .lock()
                    .push((vm.vm_identity, tally));
                p
            }
        };
        self.direct_helper_tally.store(p.cast_mut(), Relaxed);
        self.direct_helper_owner.store(vm.vm_identity, Relaxed);
        // SAFETY: as in `bump_direct_helper`: the `Arc` is held in
        // `direct_helper_tallies`, which never drops an entry.
        unsafe { &*p }
    }
}

/// Every thread's block, for the readers. Touched once per thread, on the
/// thread's first counted event.
static JIT_COUNTER_BLOCKS: std::sync::Mutex<Vec<std::sync::Arc<JitCounterBlock>>> =
    std::sync::Mutex::new(Vec::new());

thread_local! {
    /// This thread's block. The `Arc` is cloned into [`JIT_COUNTER_BLOCKS`]
    /// once, at creation, so the global mutex is never touched again.
    static JIT_COUNTERS: std::sync::Arc<JitCounterBlock> = {
        let block = std::sync::Arc::new(JitCounterBlock::default());
        if let Ok(mut blocks) = JIT_COUNTER_BLOCKS.lock() {
            blocks.push(std::sync::Arc::clone(&block));
        }
        block
    };
}

/// t1 wave 6, lane Y3: the guarded direct-helper gate tallies are per VM.
#[cfg(test)]
mod t1y3_direct_helper_tally_tests {
    use super::*;

    /// Two VMs served by one thread (and a second thread) each read only the
    /// calls made for them; switching back to a VM keeps adding to its count,
    /// and the process census still sees every call.
    #[test]
    fn t1y3_direct_helper_tallies_land_in_the_calling_vm_only() {
        let vm_a = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let vm_b = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        assert_ne!(vm_a.vm_identity, vm_b.vm_identity);
        let process_before = jit_direct_helper_jdk_only_refusals_in_process();
        std::thread::scope(|s| {
            s.spawn(|| {
                JIT_COUNTERS.with(|b| {
                    b.bump_direct_helper(&vm_a, true);
                    b.bump_direct_helper(&vm_a, false);
                    b.bump_direct_helper(&vm_b, true);
                    b.bump_direct_helper(&vm_b, true);
                    b.bump_direct_helper(&vm_a, true);
                });
            });
            s.spawn(|| {
                JIT_COUNTERS.with(|b| b.bump_direct_helper(&vm_b, false));
            });
        });
        assert_eq!(jit_direct_helper_jdk_only_refusals(&vm_a), 2);
        assert_eq!(jit_direct_helper_jdk_only_admissions(&vm_a), 1);
        assert_eq!(jit_direct_helper_jdk_only_refusals(&vm_b), 2);
        assert_eq!(jit_direct_helper_jdk_only_admissions(&vm_b), 1);
        // Other tests may add concurrently, so only `>=`.
        assert!(jit_direct_helper_jdk_only_refusals_in_process() >= process_before + 4);
    }

    /// t1 wave 7, lane V6: a thread that keeps alternating between two VMs
    /// holds ONE tally per VM (it used to register a fresh one on every
    /// switch, growing its list and each VM's sink without bound), and every
    /// call still lands in its own VM's count.
    #[test]
    fn t1v6_alternating_vms_reuse_their_tally() {
        let vm_a = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let vm_b = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let held = std::thread::scope(|s| {
            s.spawn(|| {
                JIT_COUNTERS.with(|b| {
                    for _ in 0..50 {
                        b.bump_direct_helper(&vm_a, true);
                        b.bump_direct_helper(&vm_b, false);
                    }
                    b.direct_helper_tallies.lock().len()
                })
            })
            .join()
            .unwrap()
        });
        assert_eq!(held, 2, "one tally per VM, not one per switch");
        assert_eq!(jit_direct_helper_jdk_only_refusals(&vm_a), 50);
        assert_eq!(jit_direct_helper_jdk_only_admissions(&vm_a), 0);
        assert_eq!(jit_direct_helper_jdk_only_refusals(&vm_b), 0);
        assert_eq!(jit_direct_helper_jdk_only_admissions(&vm_b), 50);
    }
}

#[cfg(test)]
mod jit_counter_block_tests {
    use super::*;

    /// The tallies moved off process-global `fetch_add` and into per-thread
    /// blocks to stop paying a `lock xadd` per dispatch. The whole claim is
    /// that they stayed EXACT while doing it, so assert exactly that: every
    /// increment from every thread, including threads that have since exited,
    /// must be visible to the reader.
    ///
    /// Asserted on a DELTA rather than an absolute, because these are
    /// process-global counters and the rest of this binary's tests dispatch
    /// natives of their own.
    #[test]
    fn every_thread_s_increments_survive_and_are_summed() {
        const THREADS: usize = 4;
        const PER_THREAD: u64 = 10_000;

        let before_walks = membership_walks_by_site()
            .into_iter()
            .find(|(n, _)| *n == MEMBERSHIP_WALK_SITE_NAMES[1])
            .map(|(_, v)| v)
            .unwrap_or(0);
        let before_leaf = leaf_native_hit_count();

        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..PER_THREAD {
                        note_membership_walk(1);
                        note_leaf_native_hit();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("worker panicked");
        }
        // Every worker has EXITED by now. Its block is still registered, which
        // is what a whole-run census needs.
        let after_walks = membership_walks_by_site()
            .into_iter()
            .find(|(n, _)| *n == MEMBERSHIP_WALK_SITE_NAMES[1])
            .map(|(_, v)| v)
            .unwrap_or(0);
        let after_leaf = leaf_native_hit_count();

        assert_eq!(
            after_walks - before_walks,
            PER_THREAD * THREADS as u64,
            "membership-walk tally lost increments across threads"
        );
        assert_eq!(
            after_leaf - before_leaf,
            PER_THREAD * THREADS as u64,
            "leaf-hit tally lost increments across threads"
        );
    }

    /// A site index past the end of the block is ignored, exactly as the
    /// `MEMBERSHIP_WALK_BY_SITE.get(site)` it replaced ignored it — this is
    /// the arm that kept an out-of-range site from panicking a release build.
    ///
    /// Read through THIS THREAD'S OWN block, not through
    /// `leaf_native_hit_count()`, which sums every registered block in the
    /// process. `cargo test` runs tests concurrently and several of them reach
    /// compiled-dispatch code, so the global sum moves under this test for
    /// reasons that have nothing to do with the out-of-range site — it failed
    /// that way at 8121 against an expected 8072. A thread's own counter is the
    /// only reading no other test can perturb, and it is the reading this test
    /// always meant: the question is whether a bad index lands on a
    /// NEIGHBOURING FIELD of the same block.
    #[test]
    fn an_out_of_range_site_is_ignored_rather_than_panicking() {
        fn my_leaf_hits() -> u64 {
            JIT_COUNTERS.with(|block| {
                block
                    .leaf_native_hits
                    .load(std::sync::atomic::Ordering::Relaxed)
            })
        }
        let before = my_leaf_hits();
        note_membership_walk(MEMBERSHIP_WALK_SITE_NAMES.len());
        note_membership_walk(usize::MAX);
        assert_eq!(
            my_leaf_hits(),
            before,
            "an out-of-range site must not land on a neighbouring counter"
        );
    }

    /// Every name has a counter behind it. The two arrays are index-parallel
    /// by contract and nothing else checks it now that the counters live in a
    /// struct rather than beside the names.
    #[test]
    fn the_block_has_one_counter_per_site_name() {
        let block = JitCounterBlock::default();
        assert_eq!(
            block.membership_walk.len(),
            MEMBERSHIP_WALK_SITE_NAMES.len(),
            "site names and per-thread counters must stay index-parallel"
        );
    }

    /// The `VarHandle` direct-helper pairs moved off six process-global
    /// `lock xadd` counters (r9-vmside). Same claim as above: still EXACT,
    /// summed across threads that have exited, and one kind never lands on
    /// another's counter. Read on THIS thread's block for the cross-kind check,
    /// because other tests in the binary may drive real VarHandle helpers.
    #[test]
    fn varhandle_direct_counters_are_per_thread_and_exact() {
        const THREADS: usize = 3;
        const PER_THREAD: u64 = 5_000;
        let before = vh_direct_counts(VH_DIRECT_CAS);
        let before_bypass = jit_funnel_bypass_count();
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..PER_THREAD {
                        note_vh_direct(VH_DIRECT_CAS, true);
                        note_funnel_bypass_hit();
                    }
                    note_vh_direct(VH_DIRECT_CAS, false);
                })
            })
            .collect();
        for h in handles {
            h.join().expect("worker panicked");
        }
        // `>=`, not `==`: these are process-wide sums and a concurrently
        // running test may drive a real VarHandle helper. A LOST increment —
        // the failure this guards — still reads short.
        let after = vh_direct_counts(VH_DIRECT_CAS);
        assert!(after.0 - before.0 >= PER_THREAD * THREADS as u64);
        assert!(after.1 - before.1 >= THREADS as u64);
        assert!(jit_funnel_bypass_count() - before_bypass >= PER_THREAD * THREADS as u64);

        // Kinds are separate, and an out-of-range kind is ignored.
        let mine = |kind: usize| {
            JIT_COUNTERS
                .with(|b| b.vh_direct_served[kind].load(std::sync::atomic::Ordering::Relaxed))
        };
        let (r0, w0, c0) = (
            mine(VH_DIRECT_READ),
            mine(VH_DIRECT_WRITE),
            mine(VH_DIRECT_CAS),
        );
        note_vh_direct(VH_DIRECT_WRITE, true);
        note_vh_direct(usize::MAX, true);
        assert_eq!(mine(VH_DIRECT_READ), r0);
        assert_eq!(mine(VH_DIRECT_WRITE), w0 + 1);
        assert_eq!(mine(VH_DIRECT_CAS), c0);
    }
}

/// Sum one field across every thread's block.
fn jit_counter_total(pick: impl Fn(&JitCounterBlock) -> u64) -> u64 {
    match JIT_COUNTER_BLOCKS.lock() {
        Ok(blocks) => blocks.iter().map(|b| pick(b)).sum(),
        Err(poisoned) => poisoned.into_inner().iter().map(|b| pick(b)).sum(),
    }
}

/// Record one membership walk at `site`.
#[inline]
pub fn note_membership_walk(site: usize) {
    JIT_COUNTERS.with(|block| {
        if let Some(c) = block.membership_walk.get(site) {
            JitCounterBlock::bump(c);
        }
    });
}

/// `(name, count)` for every site that walked at least once.
pub fn membership_walks_by_site() -> Vec<(&'static str, u64)> {
    MEMBERSHIP_WALK_SITE_NAMES
        .iter()
        .enumerate()
        .map(|(i, n)| {
            (
                *n,
                jit_counter_total(|b| {
                    b.membership_walk[i].load(std::sync::atomic::Ordering::Relaxed)
                }),
            )
        })
        .filter(|(_, v)| *v > 0)
        .collect()
}

pub unsafe extern "C" fn jit_getfield(vm_ptr: i64, obj_ptr: i64, field_index: i64) -> i64 {
    contain("jit_getfield", OnPanic::Deopt, i64::MIN, || {
        jit_getfield_body(vm_ptr, obj_ptr, field_index)
    })
}

/// Body of [`jit_getfield`]. Panic-guarded via [`contain`] with [`OnPanic::Deopt`].
unsafe fn jit_getfield_body(vm_ptr: i64, obj_ptr: i64, field_index: i64) -> i64 {
    // The JIT may set `GETFIELD_RECEIVER_PROVEN_OOP` to say it has already
    // proven this receiver is an oop; see that constant for why that is sound
    // and why it rides in the argument rather than in a second helper slot.
    let raw = field_index as u64;
    let proven_oop = raw & cratonvm_jit_api::GETFIELD_RECEIVER_PROVEN_OOP != 0;
    // Gated exactly like its denominator `GETFIELD_HELPER_CALLS` (r9-vmside).
    // Its only reader is `vm-cli`'s `jit.method_stats` line, which prints it
    // "of" that total — and `getfield_census_counting_enabled` covers that
    // switch — yet it was the one ungated `lock xadd` left on the helper whose
    // sibling increment was measured at 2-3 ns of a 9 ns read and gated for it.
    if proven_oop && getfield_census_counting_enabled() {
        JIT_GETFIELD_TRUSTED_REF_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    // `GETFIELD_EXPECT_REFERENCE` says the caller will DEREFERENCE what it gets
    // back. See that constant: without it the helper hands compiled code the
    // payload of whatever `Value` variant the slot holds, and a reference field
    // punned to a primitive becomes a wild pointer.
    let expect_ref = raw & cratonvm_jit_api::GETFIELD_EXPECT_REFERENCE != 0;
    // The NPE trap-SITE key, so a null receiver here can carry JEP 358's
    // message out. This helper has the receiver (null) and the slot index and
    // neither names the field or the bci; the emitter knows both and recorded
    // them under this key. `0` = the site was not described (the IR tier, the
    // no-metadata arm, or the feature switched off) and yields the historical
    // unmessaged NPE.
    let npe_site = cratonvm_jit_api::getfield_npe_site_of(field_index);
    let field_index = cratonvm_jit_api::getfield_index_of(field_index);
    let loaded = jit_getfield_impl(
        vm_ptr,
        obj_ptr,
        field_index,
        !proven_oop,
        expect_ref,
        npe_site,
    );
    // JIT round 20 wave 12, lane qNull (L1W19-2): a null receiver at a site
    // the single-pass tier marked as an implicit null check's slow path came
    // here through a translated hardware fault. The mark is only emitted under
    // `CRATONVM_JIT_NULL_TRAP_RECOMPILE`, so the off arm never reaches this.
    if obj_ptr == 0 && raw & cratonvm_jit_api::GETFIELD_IMPLICIT_NULL_SITE != 0 {
        note_implicit_null_fault(vm_ptr, npe_site);
    }
    // SBR4 satb: slot 0 of a soft / weak / phantom reference IS
    // `Reference.referent`, and HotSpot loads it through an SATB keep-alive
    // (`Reference.get()` is the `G1ReferenceGet` intrinsic). The VM's marker
    // hides the slot, so a referent handed to compiled code here must be
    // logged or a concurrent cycle can clear and free it while the program
    // holds it. The sites that read it are given no compact row on purpose
    // (`jit_referent_load`), so they all land in this helper. The shared
    // helper checks the precise receiver and field. Sites without resolved
    // field metadata cannot set `expect_ref`; the raw helper validates the
    // receiver, slot, and returned heap address before treating that word as
    // an oop, so an unrelated primitive slot remains inert.
    if loaded > 0 {
        let vm = &*(vm_ptr as *const SharedVm);
        // zt t32 (2b): a referent read is resurrection-blocked until the
        // after-release referent clear has run. The word is handed over
        // UNVALIDATED: the gate answers only for a `Reference.referent` slot
        // (whose word is a reference), and the referent it blocks is exactly
        // one the pause already pruned from the registry, which
        // `is_object_address` would refuse.
        if vm.mem.heap.zgc_referent_gate_state() != cratonvm_gc::zgc::ZGC_REFERENT_GATE_NEVER {
            let receiver = ObjectRef::from_raw(obj_ptr as *mut u8);
            let gated = crate::memory::ref_discovery::gate_referent_read(
                vm,
                receiver,
                field_index as usize,
                Value::Object(Some(ObjectRef::from_raw(loaded as *mut u8))),
                || {
                    let again = jit_getfield_impl(
                        vm_ptr,
                        obj_ptr,
                        field_index,
                        !proven_oop,
                        expect_ref,
                        npe_site,
                    );
                    Value::Object((again > 0).then(|| ObjectRef::from_raw(again as *mut u8)))
                },
            );
            let word = match gated {
                Value::Object(Some(r)) => r.as_ptr() as i64,
                _ => 0,
            };
            if word != loaded {
                return word;
            }
        }
        // t1 wave 10 (lane REFS3, `CRATONVM_JIT_IR_REFERS_TO_NO_KEEPALIVE`):
        // a compare-only referent read (the IR `refersTo` expansions) owes no
        // keep-alive -- HotSpot's `refersTo0` load is `AS_NO_KEEPALIVE`, as the
        // interpreter's natives and `jit_refers_to0` read it. The gate above
        // still ran: the compare must see the blocked answer.
        if raw & cratonvm_jit_api::GETFIELD_NO_KEEPALIVE != 0 {
            return loaded;
        }
        // zt t23/zb7: under the ZGC color flip the keep-alive is the load
        // barrier's mark step, otherwise this same SATB log; that choice is
        // `VmHeap::referent_keep_alive`, which the shared helper's log must
        // call (`ref_discovery::keep_alive_referent_read_raw`).
        crate::memory::ref_discovery::keep_alive_referent_read_raw(
            vm,
            ObjectRef::from_raw(obj_ptr as *mut u8),
            field_index as usize,
            loaded as usize,
        );
    }
    loaded
}

/// JIT round 20 wave 12, lane qNull (L1W19-2, `CRATONVM_JIT_NULL_TRAP_RECOMPILE`):
/// count one implicit-null fault at the trapping `getfield` and, on the
/// per-bci trap limit (`interpreter::PER_BCI_DESPEC_LIMIT`, HotSpot's
/// `PerBytecodeTrapLimit`), have the method recompiled with an
/// explicit null check there -- HotSpot's `too_many_traps(Reason_null_check)`.
///
/// Called by [`jit_getfield_body`] after the null arm has raised the pending
/// NPE, so the frame snapshot `set_jit_pending_npe_action_at` took is in
/// `JIT_SIGNALS`: its innermost frame is the trapping artifact, and the site
/// key names the bci in that artifact's own code (`apply_npe_trap_site`'s
/// lookup). The count and the verdict live in THIS VM's de-spec registry
/// (`DespecRegistry::note_implicit_null_trap`), under the artifact's label --
/// the single-pass `method_key` its next compile asks
/// (`x64::Compiler::null_check_explicit_after_traps`). On the limit the method
/// is charged one `NullCheck` deopt at the bci, which evicts the body (made not
/// entrant) so the next call recompiles; the running activation is untouched
/// and throws its NPE exactly as before. Nothing here changes the exception,
/// its message or its trace.
///
/// Declines quietly (the site keeps faulting, as today) when the site was not
/// described (`npe_site == 0`), the snapshot is off or empty, the site sits in
/// a splice (an implicit site never does), or the label does not parse.
#[cold]
#[inline(never)]
fn note_implicit_null_fault(vm_ptr: i64, npe_site: u32) {
    if npe_site == 0 || vm_ptr == 0 {
        return;
    }
    let cm_ptr = JIT_SIGNALS.with(|s| {
        s.trap_frames
            .borrow()
            .as_ref()
            .and_then(|frames| frames.last())
            .map(|top| top.cm_ptr)
    });
    let Some(cm_ptr) = cm_ptr.filter(|&p| p != 0) else {
        return;
    };
    // SAFETY: the same window and contract as `apply_npe_trap_site`, which
    // read this pointer moments ago inside this same helper: the owning
    // thread, the artifact still registered and its activation still live.
    let cm = unsafe { &*(cm_ptr as *const cratonvm_jit::CompiledMethod) };
    let Some(site) = cm.npe_trap_map.get(npe_site) else {
        return;
    };
    if !site.chain.is_empty() {
        return;
    }
    let bci = site.bci;
    // SAFETY: `vm_ptr` is the live VM context compiled code passed this helper.
    let vm = unsafe { &*(vm_ptr as *const SharedVm) };
    let limit = crate::runtime::interpreter::PER_BCI_DESPEC_LIMIT as u32;
    if !vm
        .jit
        .despec_registry
        .note_implicit_null_trap(cm.method_label.as_str(), bci, limit)
    {
        return;
    }
    // Owned, and `cm` is not read again below: the eviction retires this very
    // artifact, so nothing past it borrows from it.
    let method_key = cm.method_label.clone();
    // JIT r20 lane rev12: evict by the key the body was PUBLISHED under. An
    // inherited body is published under the receiver's class (name and id),
    // while `method_label` names the declaring class; the label's name with
    // the key's id matched no cache entry, so the one request this site ever
    // makes evicted nothing and the body kept faulting. The registry entry
    // above stays under `method_label`, the key the recompile asks with.
    let published = cm.published_key_parts();
    let (owner_class_id, class_name, method_name, descriptor): (ClassId, &str, &str, &str) =
        match &published {
            Some((class, method, descriptor, id)) => {
                (ClassId::new(*id), &**class, &**method, &**descriptor)
            }
            None => {
                // `"<class>.<name>:<descriptor>"`; a method name has no '.', so
                // the last one before the descriptor separates it from the
                // (internal) class name.
                let Some(desc_at) = method_key.find(":(") else {
                    return;
                };
                let Some((class_name, method_name)) = method_key[..desc_at].rsplit_once('.') else {
                    return;
                };
                (
                    ClassId::new(cm.owner_class_id),
                    class_name,
                    method_name,
                    &method_key[desc_at + 1..],
                )
            }
        };
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] implicit-null trap limit: {method_key} bci={bci} \
             ({limit} faults) -> recompile with an explicit null check"
        );
    }
    let _ = DeoptimizationController::deoptimize_or_by_name(
        vm,
        owner_class_id,
        class_name,
        method_name,
        descriptor,
        cratonvm_jit::deopt::DeoptReason::NullCheck,
        bci,
    );
}

/// Reference loads whose slot did not hold a reference, degraded to null by
/// [`GETFIELD_EXPECT_REFERENCE`](cratonvm_jit_api::GETFIELD_EXPECT_REFERENCE)
/// instead of being handed to compiled code as a pointer.
///
/// An engagement counter, printed beside the fix rather than trusted: a zero
/// here on a workload that used to crash means the crash came from somewhere
/// else.
///
/// **It is NOT a defect count, and the first version of this comment said it
/// was.** Measured 2026-08-24 on the two Tomcat classes it was written for:
/// 20 190 hits across two solo runs, of which **zero** carried a non-zero
/// payload word. The total is dominated by reference fields of freshly
/// allocated objects, whose cells are still zero-filled and therefore decode as
/// `Int(0)` — the inline read this defers from returned the correct null from
/// them by accident. Quote [`JIT_GETFIELD_PUNNED_REF_NONZERO`] instead: that is
/// the subset that would actually have been dereferenced.
pub static JIT_GETFIELD_PRIMITIVE_IN_REF_SLOT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Of [`JIT_GETFIELD_PRIMITIVE_IN_REF_SLOT`], the subset whose cell payload
/// word was NON-ZERO — i.e. the ones the inline arm would have handed to
/// compiled code as a pointer to follow.
///
/// The total is dominated by zero-filled cells of freshly allocated objects,
/// where the old inline read produced the correct null by accident. This
/// counter is the one that measures danger, and it is the number to quote.
pub static JIT_GETFIELD_PUNNED_REF_NONZERO: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// How many punned reference loads have been REPORTED, bounding the per-run
/// dump independently of the dangerous-payload count.
pub static JIT_GETFIELD_PUNNED_REF_REPORTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Snapshot of [`JIT_GETFIELD_PUNNED_REF_NONZERO`].
pub fn jit_getfield_punned_ref_nonzero() -> u64 {
    JIT_GETFIELD_PUNNED_REF_NONZERO.load(std::sync::atomic::Ordering::Relaxed)
}

/// Snapshot of [`JIT_GETFIELD_PRIMITIVE_IN_REF_SLOT`].
pub fn jit_getfield_primitive_in_ref_slot() -> u64 {
    JIT_GETFIELD_PRIMITIVE_IN_REF_SLOT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Corrupt legacy `Value` cells this crate has decoded at a JIT helper door.
///
/// Also increments `cratonvm_types::cell_census`, so the process-wide total
/// stays right; kept separately because "a JIT helper read one" and "a
/// collector read one" want different searches.
pub static JIT_CORRUPT_VALUE_CELLS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Snapshot of [`JIT_CORRUPT_VALUE_CELLS`].
pub fn jit_corrupt_value_cells() -> u64 {
    JIT_CORRUPT_VALUE_CELLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Read one legacy 16-byte `Value` cell with its discriminant SCREENED — the
/// VM-crate counterpart of `gc::heap::read_value_cell_checked`.
///
/// # Why a raw `read_value_atomic` is not acceptable here
///
/// `read_value_atomic` loads two words and `transmute`s them into a `Value`
/// with no validation. If those 16 bytes were not a `Value` — array payload
/// read through the flat-object path, or a punned/torn write — the result is an
/// enum holding an out-of-range discriminant, which is UB *the instant it
/// exists*, before anything looks at it.
///
/// What that UB costs is neither hypothetical nor a fault at the read. A
/// `match` on a Rust enum needs no default arm and therefore gets **no bounds
/// check**: LLVM indexes its jump table directly — `movsxd rax, [r10 + rax*4]`
/// — because a valid discriminant is in range by construction. The garbage tag
/// becomes the index. That is the decoded faulting instruction of the eight
/// hibernate-orm JSON/XML `hs_err` files, and it sits in
/// `gc::heap::coerce_field_value_for_slot` — one frame ABOVE the reader that
/// built the bad `Value`, which is why auditing read sites for jump tables
/// found nothing at the read sites themselves. See
/// `internal/fixed-bugs/hib-orm-json-xml-function-tests-segfault-g1-zgc-FIXED-20260901.md` §3.
///
/// Screening at the read turns that into `Value::Object(None)` plus a counted,
/// rate-limited report naming the slot and both raw words.
///
/// # Safety
/// `ptr` must name 16 readable, 8-byte-aligned bytes inside a live allocation.
#[inline]
unsafe fn jit_read_value_cell_checked(ptr: *const Value, site: &'static str) -> Value {
    match cratonvm_types::read_value_checked_atomic(ptr) {
        Some(v) => v,
        None => {
            JIT_CORRUPT_VALUE_CELLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let n = cratonvm_types::cell_census::note_decoded();
            if n < 32 {
                cratonvm_types::cell_census::note_reported();
                // SAFETY: caller contract — 16 readable, 8-byte-aligned bytes.
                // Per-word atomic so the diagnostic cannot tear against a
                // concurrent plain writer.
                let raw0 = (*(ptr as *const std::sync::atomic::AtomicU64))
                    .load(std::sync::atomic::Ordering::Relaxed);
                let raw1 = (*((ptr as *const u8).add(8) as *const std::sync::atomic::AtomicU64))
                    .load(std::sync::atomic::Ordering::Relaxed);
                tracing::error!(
                    target: "cratonvm::jit::guard",
                    slot = ?ptr,
                    raw0 = format!("{raw0:#018x}"),
                    raw1 = format!("{raw1:#018x}"),
                    "{site}: corrupt Value cell (out-of-range discriminant) — \
                     returning null instead of a UB-on-match Value. Heap \
                     reference-integrity defect (see HIB-CV-32).",
                );
            }
            Value::Object(None)
        }
    }
}

/// Calls that arrived with `GETFIELD_RECEIVER_PROVEN_OOP` set — the engagement
/// counter for the trusted-receiver arm, printed beside the total so "adopted"
/// and "helped" stay separable.
pub static JIT_GETFIELD_TRUSTED_REF_CALLS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Snapshot of [`JIT_GETFIELD_TRUSTED_REF_CALLS`].
pub fn jit_getfield_trusted_ref_calls() -> u64 {
    JIT_GETFIELD_TRUSTED_REF_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Shared body of [`jit_getfield`] and [`jit_getfield_trusted_ref`].
///
/// `validate_membership` and `expect_ref` are the only parameters that differ
/// between them. Kept as one function so the two entry points cannot drift in
/// the read, the layout split or the NPE contract — the drift this repository
/// has been bitten by every time a "fast" copy of a helper was maintained
/// separately.
///
/// `expect_ref` is a SAFETY input, not an optimisation one: it says the caller
/// will dereference the returned word. Every path that would otherwise return a
/// primitive payload has to refuse under it, because compiled code has already
/// emitted the dereference and checks only for the `i64::MIN` sentinel.
#[inline]
unsafe fn jit_getfield_impl(
    vm_ptr: i64,
    obj_ptr: i64,
    field_index: i64,
    validate_membership: bool,
    expect_ref: bool,
    npe_site: u32,
) -> i64 {
    // ENGAGEMENT COUNTER for the guarded inline `getfield` fast path.
    //
    // Reaching this function AT ALL means the inline guard
    // (`emit_guarded_getfield_receiver_check`, and the compact-layout tag test
    // that follows it) fell through: every compiled `getfield` either takes its
    // inline branch or lands here, so this count IS the fast path's miss count.
    //
    // every-jit-getfield-takes-the-helper-FIXED-20260820.md
    // asks for exactly this as its step 1 — "a fix priced on anything but that
    // counter is a guess" — because three separate signals (the gates are
    // default-on, 35 sites were emitted, the codegen arm has unit tests) all
    // report EMISSION, and none of them asks whether the inline branch ever
    // runs. `probes/AccessorDispatchProbe.java` makes the denominator exact: an
    // arm does `rounds * per` field reads and nothing else, so
    // `getfield_helper_calls / (rounds * per)` is the miss RATE rather than a
    // number needing interpretation.
    //
    // Relaxed, and unconditional so it cannot be on in one build and off in the
    // measured one. It costs one uncontended increment on a path that already
    // pays `note_jit_boundary`, `is_object_address` and a layout lookup — the
    // measured `receiverFieldTax` is 8.2 ns, and this is not visible in it.
    if getfield_census_counting_enabled() {
        GETFIELD_HELPER_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    if getfield_receiver_census_enabled() {
        note_getfield_receiver_shape(vm_ptr, obj_ptr, field_index);
    }
    if null_field_provenance_watch().is_some() {
        note_null_field_provenance(vm_ptr, obj_ptr, field_index as usize);
    }
    if compact_inline_dbg() {
        dump_getfield_guard_failure(obj_ptr);
    }
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) receiver
    // identically: take the existing null path instead of dereferencing it (the
    // field/class/header read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // objects always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
        // JVMS §getfield: throw NullPointerException on a null receiver.
        // Previously returned 0, which silently fabricated a zero/null field
        // value and masked real null-deref bugs in user code — the same
        // silent-fabrication class the array-load helpers were fixed for.
        // Flag the pending NPE (drained on every JIT method return — see
        // `take_jit_pending_npe` in runtime/interpreter.rs) and return the
        // `i64::MIN` deopt sentinel, mirroring `jit_arraylength`.
        //
        // `npe_site` names the trapping `getfield` so the drain can rebuild
        // JEP 358's message from the bytecode there. Without it this NPE
        // reaches the caller with `getMessage() == null` — the hot half of
        // the-helpful-npe-message-is-lost-in-compiled-code-FIXED-20260911.
        set_jit_pending_npe_action_at(
            crate::runtime::exceptions::helpful_npe::jit_action::NONE,
            npe_site,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    if validate_membership && {
        note_membership_walk(0);
        vm.mem.heap.is_object_address(obj_ptr as usize).is_none()
    } {
        // A receiver that is plausible but not a live object is a MISCOMPILE,
        // not a Java-level null, so it deliberately does NOT claim the trap
        // site: the bci is right but the message ("because <expr> is null")
        // would assert something about the program that is not true.
        set_jit_pending_npe();
        return i64::MIN;
    }
    let _ = &vm;
    // B2 fix (audit `vm-runtime.md`): bounds-check the field slot against the
    // receiver's declared `num_slots` BEFORE the raw read, mirroring the
    // symmetric `jit_putfield_slot_in_bounds` guard on every `jit_putfield_*`
    // helper. Without this, a stale `field_index` (synthetic/real-JDK layout
    // drift) or an operand-stack miscompile reads `obj + HEADER + index*SLOT`
    // out of the object and into the *neighbouring* heap object, leaking its
    // bytes back to JIT'd Java as an i64/object pointer (info leak + potential
    // follow-on UAF if interpreted as a ref). The interpreter (`get_field`)
    // returns a default for an out-of-range slot rather than reading past the
    // object; match that by returning 0 without dereferencing.
    // The compact slot proof below validates this same bound and records
    // its offset/storage at the current layout generation. Asking the header
    // for the count first repeats a registry lookup on every proven read.
    // Legacy cells still use the header bound before any payload access.
    // Compact layout: a reference field is a bare 8-byte pointer (0 = null,
    // matching the legacy `Object(Some(r)) => r.as_ptr()` / `Object(None) => 0`
    // result); a primitive field stays a 16-byte cell at its packed offset.
    if let Some((off, storage)) = jit_compact_field_slot(obj_ptr, field_index) {
        // SAFETY: the canonical compact slot lookup checked field_index
        // against this object's registered layout before publishing its proof.
        let ptr = (obj_ptr as *const u8).add(off);
        if storage.is_reference() {
            // Degrade an implausible reference (stale/garbage from a GC
            // root-coverage gap) to null instead of handing the JIT bits it
            // will later deref -> SIGSEGV. Mirrors `read_prim_element`'s
            // Reference arm so interpreter and JIT decode a stale ref slot
            // identically. Valid refs (or 0=null) always pass through.
            //
            // TODO(zgc) -- site B, THE compact-reference field load: the
            // helper-arm site `docs/feature-designs/zgc-jit-load-barrier.md`
            // 2.5 names, and the fallback every Category-A inline arm takes
            // once `zgc_read_barrier_blocks_inline_fields` is on. Like site A
            // it still holds the SLOT here, so the barrier belongs between the
            // slot read and the plausibility test, with the test then asking
            // about the UNMASKED address.
            //
            // DISCHARGED for this site. It goes through [`jit_load_ref_slot`],
            // the file's single seam, WITH the heap: this helper takes
            // `vm_ptr` and `vm` is already bound above, so the seam calls
            // `VmHeap::load_ref_slot_barriered` and the plausibility test then
            // runs on the address that call returns rather than on the slot
            // word. `&vm.mem.heap` reuses the reference this function has
            // already dereferenced instead of adding a second raw deref of
            // `vm_ptr`, and it is by construction the heap that OWNS `ptr`,
            // which is the seam's `Some(..)` contract. Site A does the same
            // since 2026-09-01, via `heap_from_vm(vm_ptr)`; it had no `vm_ptr`
            // at all until then, which is what made this arm the only barriered
            // slot-holding read in the file for a day.
            //
            // This is the hottest reference read in the VM, which is why the
            // seam is `#[inline(always)]` and why its census increment is
            // behind a cached gate -- one UNGATED relaxed increment on this
            // path has already been measured at a quarter of the read (see
            // `getfield_census_counting_enabled`). The new route counter
            // shares that one cached test rather than adding a second, and the
            // barrier itself costs two relaxed `AtomicBool` loads and two
            // not-taken branches while nothing arms it.
            return jit_load_ref_slot(
                ptr,
                Some(&vm.mem.heap),
                ref_load_census::GETFIELD_COMPACT_REF,
                "jit_getfield/compact-ref",
            );
        }
        // PLAIN-SLOT TEARING FIX (2026-07-06): was `std::ptr::read(ptr as
        // *const Value)` -- a non-atomic 16-byte copy that can tear against a
        // concurrent plain putfield on the SAME slot from another mutator
        // thread (interpreted OR JIT-compiled -- `jit_putfield_*` already
        // uses `write_value_atomic`, see commit 4e6b560f, but this read side
        // was never updated to match). Real JDK library code legally relies
        // on a plain field read/write being tear-free (e.g.
        // `ReentrantReadWriteLock$Sync`'s plain `firstReader`/
        // `firstReaderHoldCount`) -- see
        // elasticsearch-lucene-binary-docvalues-range-hangs.md
        // #3 for the interpreter-side counterpart of this same gap.
        let val: Value =
            cratonvm_types::read_compact_field(ptr, storage, std::sync::atomic::Ordering::Relaxed);
        // The caller will dereference this, and the compact layout says the
        // slot is NOT a reference — the two disagree about the field. Refuse
        // rather than hand over a primitive payload; see the legacy arm below
        // for the full argument and the crash it comes from.
        if expect_ref {
            JIT_GETFIELD_PRIMITIVE_IN_REF_SLOT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return 0;
        }
        return match val {
            Value::Int(i) => i as i64,
            Value::Long(l) => l,
            Value::Float(f) => f.to_bits() as i64,
            Value::Double(d) => d.to_bits() as i64,
            Value::Object(Some(r)) => {
                // Degrade an implausible (stale/garbage) object pointer to null
                // rather than handing the JIT bits it will deref → SIGSEGV.
                // Valid refs always pass the plausibility gate.
                //
                // UNREACHABLE, and kept only as a total `match`: control only
                // gets here when `storage.is_reference()` was FALSE (the
                // reference arm `return`s above), and `is_reference()` is
                // `matches!(self, Self::Reference)` (types/src/field_layout.rs:82).
                // `read_compact_field` (types/src/field_layout.rs:972) produces
                // `Value::Object` from its `Reference` arm and from nowhere
                // else — every other `FieldStorageKind` yields Int/Long/Float/
                // Double. So this filter has never fired and cannot fire, and it
                // is NOT a ZGC barrier site despite being counted as one in
                // `zgc-jit-load-barrier.md` §2.2/§9.
                let raw = r.as_ptr() as u64;
                jit_decode_ref_word(raw, "jit_getfield/compact-nonref-unreachable")
            }
            Value::Object(None) => 0,
            _ => 0,
        };
    }
    // zt t25/hunt8: an ARRAY receiver is never a plain-object getfield, and
    // the bound below cannot see it (an array mirrors its length into the
    // shape word `num_slots` reads). h3_P2's corrupt cell was slot 2 of the
    // `byte[41]` behind "java/lang/instrument/ClassFileTransformer"; the old
    // answer was null, and the same compiled body then crashed a few
    // instructions later. Raise the miscompile NPE instead (no trap-site claim,
    // like the membership refusal above): the body unwinds through its
    // exception path rather than running on with a fabricated value.
    if jit_field_receiver_is_array(
        Some(vm),
        obj_ptr,
        field_index,
        "jit_getfield/legacy-slot",
        npe_site,
    ) {
        set_jit_pending_npe();
        return i64::MIN;
    }
    if !jit_putfield_slot_in_bounds(obj_ptr, field_index) {
        return 0;
    }
    // SAFETY: obj_ptr is non-null and points to a live object, and field_index is now
    // verified < num_slots, so HEADER_SIZE + field_index * SLOT_SIZE is within the
    // object's allocated region.
    let ptr = (obj_ptr as *const u8).add(HEADER_SIZE + field_index as usize * SLOT_SIZE);
    // PLAIN-SLOT TEARING FIX (2026-07-06): see the matching note on the
    // compact-layout branch above -- was `std::ptr::read(ptr as *const
    // Value)`, non-atomic, tearable against a concurrent plain putfield.
    // DISCRIMINANT-SCREENED, and the `match` below is why. `read_value_atomic`
    // would `transmute` these 16 bytes into a `Value` unconditionally; the
    // multi-arm `match val` a hundred lines down then indexes a jump table by
    // the tag with no bounds check. See `jit_read_value_cell_checked`, and
    // `coerce_field_value_for_slot` for the crash that shape produced.
    //
    // zt t18/zb1 (zr1br A2a): with the ZGC read barrier ARMED the cell is read
    // through the heap's own accessor, whose legacy arm sends an Object-tagged
    // cell's payload word through the barrier and answers every other tag with
    // this same checked decode. Unarmed (every default run) the raw read
    // below is unchanged.
    let val: Value = if vm.mem.heap.zgc_load_barrier_armed() {
        vm.mem.heap.get_field(
            ObjectRef::from_raw(obj_ptr as *mut u8),
            field_index as usize,
        )
    } else {
        jit_read_value_cell_checked(ptr as *const Value, "jit_getfield/legacy-slot")
    };
    // A REFERENCE load whose slot does not hold a reference.
    //
    // The legacy 16-byte slot carries its own discriminant, so unlike the
    // compact layout there is no `FieldStorageKind` to consult and this
    // function has, until now, simply returned the payload of whatever variant
    // it found. For a caller that is going to dereference the answer that turns
    // a type-punned slot straight into a wild pointer:
    // `org.apache.derby.iapi.types.SQLChar.rawData` is declared `[C`, held
    // `Int(1)`, and the compiled `arraylength` after this call faulted at
    // `addr=0x5` (= 1 + ARRAY_LENGTH_OFFSET). Compiled code checks the returned
    // word against `i64::MIN` and nothing else, so every other primitive
    // payload was a pointer it would follow.
    //
    // Degrading to null is the policy `jit_decode_ref_word` already applies
    // twice in this function to an IMPLAUSIBLE pointer; this is the same
    // policy for a slot that holds no pointer at all. It is containment, not a
    // cure — whatever wrote a primitive into a reference slot (the G30-1
    // species, `known-issues/.../G30-1-the-silent-reference-slot-coercion`)
    // is still doing it, and the counter says how often.
    if expect_ref && !matches!(val, Value::Object(_)) {
        JIT_GETFIELD_PRIMITIVE_IN_REF_SLOT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // SPLIT THE COUNT, because the total does not mean what it looks like.
        //
        // The inline arm this defers from read the cell's 8-byte payload word
        // and used it as a pointer. For the overwhelmingly common case — a
        // reference field of a freshly allocated object, whose cell is still
        // zero-filled and so decodes as `Int(0)` — that word is 0, i.e. the
        // correct null, by accident. Those are not corruption, and counting
        // them together made a benign number read as a defect number.
        //
        // What is dangerous is a NON-ZERO payload word under a non-`Object`
        // tag: that is what the inline arm would have handed to compiled code
        // as a pointer, and it is the shape that killed
        // `SQLChar.readExternalFromArray` with `addr=0x5`. Counted separately,
        // and under `CRATONVM_DBG_PUNNED_REF` it names the class and field so
        // the WRITER can be found rather than inferred.
        //
        // SAFETY: `ptr` is the cell base of an in-bounds slot of a live object
        // (both checked above), so the 8 bytes at FIELD_CELL_PAYLOAD64_OFFSET
        // inside that cell are within the allocation.
        let payload64 = std::ptr::read_unaligned(
            ptr.add(cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET) as *const u64,
        );
        if payload64 != 0 {
            JIT_GETFIELD_PUNNED_REF_NONZERO.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        // WHAT TO REPORT, and why the payload word can no longer decide it.
        //
        // The report used to fire only when `payload64 != 0`, on the argument
        // that a zero payload is the benign case — a never-assigned reference
        // cell of a freshly allocated object, still zero-filled, decoding as
        // `Int(0)` and reading back as the correct null. That argument held only
        // while `write_value_atomic` could leave the payload word as stack
        // padding, which made "non-zero" a decent proxy for "somebody WROTE this
        // cell". `value_words` now writes the unused half as a hard zero, so a
        // genuinely punned `Int(1)` cell has `payload64 == 0` as well, and the
        // old gate would hide precisely the writes that fix was made to expose.
        //
        // But dropping the gate outright is not the answer either: the benign
        // zero-fill case is the overwhelming majority — this counter reaches the
        // millions on a PASSING workload — so an unfiltered dump spends its
        // 32-line budget on noise and never reaches the one event worth seeing.
        //
        // So the filter moved from the payload to the CLASS.
        // `CRATONVM_DBG_PUNNED_REF=1` keeps the historical behaviour (report the
        // dangerous-payload subset); `CRATONVM_DBG_PUNNED_REF=<substring>`
        // reports every pun on a class whose name contains that substring,
        // whatever its payload. One is for "is anything about to be
        // dereferenced", the other for "is THIS field the one reading null".
        let want = cratonvm_types::flags::runtime_var("CRATONVM_DBG_PUNNED_REF").ok();
        let class_filter = want.as_deref().filter(|w| {
            let w = w.trim();
            !w.is_empty() && w != "1" && !w.eq_ignore_ascii_case("true")
        });
        let hdr = &*(obj_ptr as *const cratonvm_types::ObjectHeader);
        let interesting = match class_filter {
            Some(sub) => cratonvm_gc::collector::class_name_for_diagnostics(hdr.class_id.as_u32())
                .contains(sub.trim()),
            None => want.is_some() && payload64 != 0,
        };
        if interesting {
            let n =
                JIT_GETFIELD_PUNNED_REF_REPORTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 32 {
                // The whole cell, not just the word that would have been
                // dereferenced. `Value::Int` keeps its payload at
                // FIELD_CELL_PAYLOAD32_OFFSET, so a non-zero payload64 under an
                // Int tag is either a writer that used the WRONG offset or
                // padding left by the cell's previous occupant — and the two
                // want completely different searches. Printing tag+p32+p64
                // separates them in one run instead of by argument.
                let tag = std::ptr::read_unaligned(
                    ptr.add(cratonvm_types::FIELD_CELL_TAG_OFFSET) as *const u32
                );
                let payload32 = std::ptr::read_unaligned(
                    ptr.add(cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET) as *const u32,
                );
                // THE DECIDING BIT: is this object actually COMPACT?
                //
                // Reaching here means `jit_compact_field_slot` returned None,
                // so the VM read the body as legacy 16-byte cells. If the
                // object's own `GC_FLAG_COMPACT` header bit is nevertheless
                // SET, the body is packed and those "cells" are packed field
                // bytes reinterpreted — the tag/payload triple is then garbage
                // by construction and there is no punned WRITE to find, only a
                // layout disagreement to fix.
                //
                // This distinction is why the writer hunt kept coming up empty:
                // measured 2026-08-24, a store-side watch on this class fired
                // 6879 times for slot 2 and ZERO times for slot 1, and the
                // single-pass backend does not even inline primitive putfields
                // (they all go through `putfield_int` -> `heap.set_field`, i.e.
                // through that watch). "Nothing ever writes slot 1" and "slot 1
                // holds Int(1)" cannot both be true of the same bytes unless
                // the bytes are not the cell anyone thinks they are.
                let gc_flags = std::ptr::read_unaligned(
                    (obj_ptr as *const u8).add(cratonvm_types::GC_FLAGS_BYTE_OFFSET),
                );
                let compact_bit = gc_flags & cratonvm_types::GC_FLAG_COMPACT != 0;
                eprintln!(
                    "[punned-ref] class={} class_id={} num_slots={} \
                     field_index={field_index} tag={tag} payload32={payload32:#x} \
                     payload64={payload64:#x} decoded={val:?} \
                     compact_flag={compact_bit} gc_flags={gc_flags:#x} \
                     jit_callee={} (payload64 is the word that would have been \
                     dereferenced; compact_flag=true means this was read at the \
                     WRONG offsets)",
                    cratonvm_gc::collector::class_name_for_diagnostics(hdr.class_id.as_u32()),
                    hdr.class_id,
                    hdr.num_slots(),
                    current_jit_callee(),
                );
                // EVERY cell of the object, not just the punned one.
                //
                // The punned cell alone cannot tell an isolated scribble from a
                // SHIFT. `org.apache.derby.iapi.types.SQLChar` declares
                // `value`(0), `rawData`(1), `rawLength`(2), and its
                // `readExternalFromArray` writes `rawLength` and `rawData` on
                // adjacent lines -- so `Int(1)` in `rawData` is exactly what a
                // `rawLength = 1` landing one slot low would leave. If that is
                // what happened, slot 2 holds the `char[]` and slot 0 holds the
                // old `rawData`; if it is an isolated scribble, the neighbours
                // are untouched. One line of output decides between two
                // completely different searches.
                for i in 0..hdr.num_slots() as usize {
                    // SAFETY: `i < num_slots` and the object is legacy-laid-out
                    // (established above), so each 16-byte cell is inside the
                    // allocation.
                    let cell = (obj_ptr as *const u8)
                        .add(cratonvm_types::HEADER_SIZE + i * cratonvm_types::SLOT_SIZE);
                    let t = std::ptr::read_unaligned(
                        cell.add(cratonvm_types::FIELD_CELL_TAG_OFFSET) as *const u32,
                    );
                    let p32 = std::ptr::read_unaligned(
                        cell.add(cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET) as *const u32,
                    );
                    let p64 = std::ptr::read_unaligned(
                        cell.add(cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET) as *const u64,
                    );
                    eprintln!(
                        "[punned-ref]   slot {i}: tag={t} payload32={p32:#x} payload64={p64:#x}"
                    );
                }
                // TRANSIENT or PERMANENT? The two have completely different
                // writers. A cell that is whole again a millisecond later was
                // never written wrong -- it was READ mid-write, and the search
                // is for the missing publication order. A cell that is still
                // punned is a store that put the wrong bytes at the right
                // address, and the search is for that store.
                std::thread::sleep(std::time::Duration::from_millis(2));
                for i in 0..hdr.num_slots() as usize {
                    // SAFETY: as above.
                    let cell = (obj_ptr as *const u8)
                        .add(cratonvm_types::HEADER_SIZE + i * cratonvm_types::SLOT_SIZE);
                    let t = std::ptr::read_unaligned(
                        cell.add(cratonvm_types::FIELD_CELL_TAG_OFFSET) as *const u32,
                    );
                    let p32 = std::ptr::read_unaligned(
                        cell.add(cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET) as *const u32,
                    );
                    let p64 = std::ptr::read_unaligned(
                        cell.add(cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET) as *const u64,
                    );
                    eprintln!(
                        "[punned-ref] +2ms slot {i}: tag={t} payload32={p32:#x} payload64={p64:#x}"
                    );
                }
            }
        }
        return 0;
    }
    let result = match val {
        Value::Int(i) => i as i64,
        Value::Long(l) => l,
        Value::Float(f) => f.to_bits() as i64,
        Value::Double(d) => d.to_bits() as i64,
        Value::Object(Some(r)) => {
            // Live filter, unlike the compact-layout twin above. The read is
            // now discriminant-screened (`jit_read_value_cell_checked`), which
            // rules out an invalid TAG — it says nothing about the payload, so
            // this `ObjectRef` can still hold arbitrary bits straight off the
            // slot and the plausibility gate below is still load-bearing.
            //
            // TODO(zgc) -- site D of `docs/feature-designs/zgc-jit-load-barrier.md`
            // 2.5.1, and NOT a barrier site. Written out precisely because the
            // previous wording ("the barrier belongs on the slot read") named
            // the destination without naming what stops it being taken here.
            //
            // The missing fact is a SLOT. The word below is `r.as_ptr()` off an
            // `ObjectRef` that `read_value_atomic` (types/src/value.rs) already
            // fabricated, several lines upstream, by transmuting two relaxed
            // word loads. `cratonvm_gc::zgc::barrier::z_load` needs
            // `&AtomicU64` -- the slot itself, because its slow path CAS-heals
            // it -- and there is nothing here to hand it. Worse, building an
            // `ObjectRef` out of a colored word is itself the violation
            // `cratonvm_gc::zgc::vaddr::debug_assert_plain_word` exists to
            // catch, so on a colored word the damage is done before control
            // arrives.
            //
            // What would have to change, and where: `read_value_atomic`'s
            // reference arm must take the barrier at ITS slot read, before the
            // `Value` is reconstituted. That is `types/` (or a `gc/`-side
            // accessor it delegates to), not this file -- and it is shared with
            // the interpreter, which is the point: one barriered path instead of
            // two that can disagree about when a slot was healed.
            //
            // Counted anyway, so "this arm ran N times" stays separable from
            // "this arm never runs". The count is of an UNBARRIERED read;
            // `jit_decode_ref_word` still fails loudly on a colored word.
            ref_load_census::note(ref_load_census::GETFIELD_LEGACY_VALUE);
            let raw = r.as_ptr() as u64;
            jit_decode_ref_word(raw, "jit_getfield/legacy-value-slot")
        }
        Value::Object(None) => 0,
        _ => 0,
    };
    result
}

/// Compiled field stores DROPPED because the receiver was not a plausible
/// heap pointer.
///
/// The helper returns without writing and without raising, so the field keeps
/// whatever it held — for a freshly allocated object, the zero-filled cell that
/// a reference `getfield` reads back as **null**.
pub static JIT_PUTFIELD_DROPPED_BAD_RECEIVER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Compiled field stores DROPPED because `field_index` fell outside the
/// receiver's declared `num_slots`. Same consequence as the counter above.
pub static JIT_PUTFIELD_DROPPED_OUT_OF_BOUNDS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Snapshot of the two dropped-store counters, `(bad_receiver, out_of_bounds)`.
pub fn jit_putfield_dropped_stores() -> (u64, u64) {
    (
        JIT_PUTFIELD_DROPPED_BAD_RECEIVER.load(std::sync::atomic::Ordering::Relaxed),
        JIT_PUTFIELD_DROPPED_OUT_OF_BOUNDS.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// Count — and under `CRATONVM_DBG_DROPPED_PUTFIELD`, report — a compiled field
/// store this helper family is about to discard.
///
/// # Why a dropped store needs a counter at all
///
/// Both drops are deliberate and both are the right call in isolation: writing
/// through an implausible receiver, or past the end of the object, corrupts the
/// neighbouring allocation instead (that is how a JIT-compiled
/// `Catalina.setParentClassLoader` produced a delayed SIGSEGV). Dropping is the
/// containment.
///
/// What was missing is that the containment is INVISIBLE. A dropped store
/// leaves the field exactly as it was, and for a field assigned once in a
/// constructor that means the zero-filled cell — which a reference `getfield`
/// reads back as a perfectly ordinary `null`. The defect then surfaces
/// arbitrarily far away as "this reference is null and cannot be", with nothing
/// anywhere connecting it to a store that did not happen.
///
/// That is the shape of `TCPSSLOptions.getTransportOptions()` returning null
/// under `CRATONVM_BG_COMPILE=0`, which reaches
/// `TcpConfig.<init>` as a null `other` and throws roughly nine seconds into
/// `TechEmpowerTest` (`known-issues/jit/bg-compile-off-nulls-a-reference-
/// argument-…`). A zero here rules the whole mechanism out in one run; a
/// non-zero names the receiver, the slot and the compiled method.
///
/// # Safety
/// `obj_ptr` is only dereferenced on the reporting path, and only after
/// `plausible_heap_pointer` has accepted it.
#[cold]
unsafe fn note_dropped_putfield(obj_ptr: i64, field_index: i64, why: &str, plausible: bool) {
    let n = if plausible {
        JIT_PUTFIELD_DROPPED_OUT_OF_BOUNDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    } else {
        JIT_PUTFIELD_DROPPED_BAD_RECEIVER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    };
    if n >= 32 || !cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_DROPPED_PUTFIELD") {
        return;
    }
    let (class, num_slots) = if plausible {
        let class_id = std::ptr::read(obj_ptr as *const u32);
        let num_slots = (*(obj_ptr as *const cratonvm_types::ObjectHeader)).num_slots();
        (
            cratonvm_gc::collector::class_name_for_diagnostics(class_id),
            num_slots as i64,
        )
    } else {
        ("<implausible receiver>".to_string(), -1)
    };
    eprintln!(
        "[dropped-putfield] why={why} class={class} num_slots={num_slots} \
         field_index={field_index} obj=0x{:x} jit_callee={}",
        obj_ptr as u64,
        current_jit_callee(),
    );
}

/// Compiled field accesses refused because the receiver was an ARRAY
/// (zt t25/hunt8, [`jit_field_receiver_is_array`]).
pub static JIT_FIELD_ARRAY_RECEIVER_REFUSALS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Snapshot of [`JIT_FIELD_ARRAY_RECEIVER_REFUSALS`].
pub fn jit_field_array_receiver_refusals() -> u64 {
    JIT_FIELD_ARRAY_RECEIVER_REFUSALS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Is this compiled field access's receiver an ARRAY? `true` means the caller
/// must refuse the access (a `getfield` raises the miscompile NPE, a `putfield`
/// drops the store); the refusal has already been counted and reported.
///
/// zt t25/hunt8 (`CRATONVM_JIT_FIELD_ARRAY_RECEIVER_SCREEN`, default ON, `0`
/// restores the striding read/write). This is the JIT's door onto the species
/// `corrupt-value-cell-array-receiver-species-CLOSED-20260823` closed at the
/// four heap accessors (`heap::refuse_array_receiver_field_access`): the
/// compiled helpers read and write the legacy 16-byte cell themselves, never
/// through those accessors, and their only guard was the `num_slots` bound --
/// which an array passes for every index below its LENGTH, since `alloc_array`
/// mirrors the length into the same shape word.
///
/// h3_P2 (t22, Windows) is the case: `jit_getfield/legacy-slot` decoded
/// `raw0=0x656d726f66736e61 raw1=0x72`, i.e. `"ansforme"` + `"r"` + padding.
/// `"java/lang/instrument/ClassFileTransformer"` is 41 bytes; bytes 32..40 are
/// `ansformer`, and a 16-byte cell at slot index 2 sits at body offset
/// `HEADER_SIZE + 2 * SLOT_SIZE = 48`, i.e. data offset 32. So the receiver was
/// the BASE of that `byte[41]`, and the index check admitted slot 2 because 2 <
/// 41. No instance read can be right on that receiver, whichever way it got
/// there (a stale reference that relocation or a free left pointing where the
/// array now starts, or a wrong value in a reference slot).
///
/// Compact instances never reach this question with an array: an array's header
/// has no `GC_FLAG_COMPACT`, so every caller is on its legacy arm already, and
/// the cost is one header-byte test on that arm.
///
/// # Safety
/// `obj_ptr` must be a plausible, readable object base (every caller has
/// already tested it with `plausible_heap_pointer`).
///
/// `npe_site` is the access's NPE trap-site key (`getfield_npe_site_of`; `0`
/// where the caller has none, every putfield): with it the report names the
/// exact bci of the refused access (zt t26/hunt9).
#[inline]
unsafe fn jit_field_receiver_is_array(
    vm: Option<&SharedVm>,
    obj_ptr: i64,
    field_index: i64,
    site: &'static str,
    npe_site: u32,
) -> bool {
    let header = &*(obj_ptr as *const cratonvm_types::ObjectHeader);
    if header.kind() != cratonvm_types::ObjectKind::Array
        || !crate::runtime::env_cache::jit_field_array_receiver_screen()
    {
        return false;
    }
    note_jit_field_array_receiver(vm, obj_ptr, header, field_index, site, npe_site);
    true
}

/// zt t26/hunt9: one compiled activation as [`format_array_receiver_site`]
/// prints it (a view of `conservative_roots::ActiveCompiledFrame`).
struct RefusalFrame<'a> {
    /// `"class/Name.method:descriptor"`.
    label: &'a str,
    /// The activation's bci, `-1` when the frame published none.
    bci: i32,
    /// `bci` came from the access's own NPE trap site (exact), not from the
    /// frame's last published safepoint id.
    exact: bool,
    /// The callees inlined at that point, innermost first.
    chain: &'a [crate::jit::conservative_roots::InlinedLevel],
}

/// zt t26/hunt9: the text of an array-receiver refusal's `compiled_site`
/// field. `frames` is innermost first. The access runs in `method` at `bci`,
/// or, when `inlined` is non-empty, in `inlined`'s FIRST level (the innermost
/// spliced callee) at that level's bci, inside `method`. `bci_source` says
/// how much the bci can be trusted: `npe-trap-site` is the access itself;
/// `safepoint-id` is the frame's last published call, which may precede the
/// access; `none` is no bci at all. Pure.
fn format_array_receiver_site(frames: &[RefusalFrame<'_>]) -> String {
    use std::fmt::Write as _;
    let Some(top) = frames.first() else {
        return "none (no compiled activation on this thread: a runtime caller, not compiled \
                code)"
            .to_string();
    };
    let source = if top.bci < 0 {
        "none"
    } else if top.exact {
        "npe-trap-site"
    } else {
        "safepoint-id"
    };
    let mut text = format!(
        "method={} bci={} bci_source={source} inlined=[",
        top.label, top.bci
    );
    for (i, level) in top.chain.iter().enumerate() {
        let sep = if i == 0 { "" } else { " <- " };
        let _ = write!(text, "{sep}{}@{}", level.label, level.bci);
    }
    text.push_str("] callers=[");
    for (i, frame) in frames.iter().skip(1).enumerate() {
        let sep = if i == 0 { "" } else { " <- " };
        let _ = write!(text, "{sep}{}@{}", frame.label, frame.bci);
    }
    text.push(']');
    text
}

/// zt t26/hunt9: the compiled program point of an array-receiver refusal,
/// read the way a JIT-signalled NPE's trap snapshot reads it
/// ([`snapshot_trap_frames`]): this thread's innermost compiled activations,
/// the trapping one given the access's own bci and inline chain when the
/// emitter described the site (`npe_site != 0`). Cold: the first 32 refusals
/// only.
#[cold]
#[inline(never)]
fn array_receiver_compiled_site(npe_site: u32) -> String {
    let mut frames = crate::jit::conservative_roots::active_compiled_frames_bounded(0, 4);
    crate::jit::conservative_roots::apply_npe_trap_site(&mut frames, npe_site);
    let view: Vec<RefusalFrame<'_>> = frames
        .iter()
        .rev()
        .take(4)
        .map(|f| RefusalFrame {
            label: &f.label,
            bci: f.bci,
            exact: f.trap_site_exact,
            chain: f.chain(),
        })
        .collect();
    format_array_receiver_site(&view)
}

/// Count and (first 32 per process) report one refusal of
/// [`jit_field_receiver_is_array`]. Through the corrupt-cell census
/// (`note_decoded` + `note_array_receiver`) so `CRATONVM_DBG_CORRUPT_CELL`'s
/// exit line counts it beside the heap accessors' refusals.
///
/// The report names what the next h3_P2-shaped crash needs: the array's
/// element type and length (its species), the cell bytes when the cell lies
/// inside the array's own body (so the string can be read back), the number of
/// collections so far (`0` rules out every relocation/free reading), and the
/// compiled callee.
///
/// zt t26/hunt9: and `compiled_site` ([`array_receiver_compiled_site`]), the
/// method and bci of the refused access with its inline chain and two callers.
/// `jit_callee` is empty unless `CRATONVM_DBG_JIT_PUTFIELD` is set (only the
/// dispatch doors record it, and only then), so the t25 line could not name
/// the code that was handed the array, which is what page item 3 needs.
///
/// # Safety
/// `obj_ptr` is a live allocation base whose header is `header`. The two cell
/// words are read only when the 16-byte cell lies inside the array's body.
#[cold]
#[inline(never)]
unsafe fn note_jit_field_array_receiver(
    vm: Option<&SharedVm>,
    obj_ptr: i64,
    header: &cratonvm_types::ObjectHeader,
    field_index: i64,
    site: &'static str,
    npe_site: u32,
) {
    JIT_FIELD_ARRAY_RECEIVER_REFUSALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let n = cratonvm_types::cell_census::note_decoded();
    cratonvm_types::cell_census::note_array_receiver();
    if n >= 32 {
        return;
    }
    cratonvm_types::cell_census::note_reported();
    let length = header.array_length() as usize;
    let element_type = header.element_type();
    let cell_off =
        HEADER_SIZE.saturating_add((field_index.max(0) as usize).saturating_mul(SLOT_SIZE));
    let body_end = cratonvm_types::array_data_size_checked(length, element_type)
        .and_then(|d| cratonvm_types::ARRAY_DATA_OFFSET.checked_add(d));
    let in_body = matches!(body_end, Some(end) if cell_off.saturating_add(16) <= end);
    let (raw0, raw1) = if in_body {
        // SAFETY: the whole cell lies inside this array's body (checked just
        // above). Per-word atomic: an element store may race the report.
        let p = (obj_ptr as *const u8).add(cell_off);
        (
            (*(p as *const std::sync::atomic::AtomicU64))
                .load(std::sync::atomic::Ordering::Relaxed),
            (*(p.add(8) as *const std::sync::atomic::AtomicU64))
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    } else {
        (0, 0)
    };
    let collections = vm.map_or(-1, |vm| vm.mem.heap.collection_count() as i64);
    let compiled_site = array_receiver_compiled_site(npe_site);
    tracing::error!(
        target: "cratonvm::jit::guard",
        obj = ?(obj_ptr as *const u8),
        field_index,
        length,
        element_type = ?element_type,
        class_id = ?header.class_id,
        raw0 = format!("{raw0:#018x}"),
        raw1 = format!("{raw1:#018x}"),
        in_body,
        collections,
        jit_callee = %current_jit_callee(),
        compiled_site = %compiled_site,
        "{site}: compiled field access on an ARRAY receiver -- refused (getfield: \
         NullPointerException; putfield: store dropped) instead of striding packed \
         element data as 16-byte Value cells. compiled_site names the method and bci \
         that were handed the array. collections=0 rules out a stale reference left \
         by a relocation or a free; otherwise the receiver may be one (see \
         zgc-zt24hunt6-freed-code-buffer-line-was-range-reuse). \
         CRATONVM_JIT_FIELD_ARRAY_RECEIVER_SCREEN=0 restores the old read.",
    );
}

/// Bounds-check a JIT putfield slot against the receiver's declared
/// `num_slots` (read directly from the object header at offset 16 — the same
/// `num_slots` field `VmHeap::num_fields` returns; see the header layout in
/// `types/src/heap_types.rs` and the existing offset-16 read at the array
/// diagnostics above). Mirrors the guard in `jit_putfield_object`: under
/// synthetic/real-JDK layout drift a stale `field_index` would otherwise
/// overflow the raw `obj + HEADER + index*SLOT` write into the *neighbouring*
/// heap object — silent corruption surfacing as a delayed SIGSEGV far from the
/// offending putfield. The interpreter (`set_field`) silently drops such
/// writes; match it. `obj_ptr` must be non-null and canonical (the caller's
/// null check + the JIT's receiver discipline guarantee this — a non-canonical
/// receiver would fault on the header read exactly as the raw write would).
// SAFETY: `obj_ptr` must be non-null and canonical (caller's null check + the
// JIT's receiver discipline guarantee this); only the object's `num_slots`
// header word at offset 16 is read to bounds-check `field_index`.
#[inline]
unsafe fn jit_putfield_slot_in_bounds(obj_ptr: i64, field_index: i64) -> bool {
    if field_index < 0 {
        return false;
    }
    // zt t25/hunt8: an ARRAY receiver passes the count below for every index
    // under its length, and the store would stride 16-byte cells over (and
    // past) its packed elements. Dropped like an out-of-bounds store; see
    // `jit_field_receiver_is_array`. `getfield` screens before calling this,
    // so a refusal it reports is never reported twice.
    if jit_field_receiver_is_array(None, obj_ptr, field_index, "jit_putfield/legacy-slot", 0) {
        return false;
    }
    // Through the header accessor: a LEGACY instance's count is its shape word
    // (`NUM_SLOTS_OFFSET`), while a compact instance's 8-byte header has none
    // -- offset 8 is its first field -- and its count is its class layout's.
    let num_slots = (*(obj_ptr as *const cratonvm_types::ObjectHeader)).num_slots();
    (field_index as u64) < num_slots as u64
}

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index was resolved at JIT compile time to a valid slot.
pub unsafe extern "C" fn jit_putfield_int(obj_ptr: i64, field_index: i64, val: i64) {
    contain("jit_putfield_int", OnPanic::Deopt, (), || {
        jit_putfield_int_body(obj_ptr, field_index, val)
    });
}

/// Body of [`jit_putfield_int`]. Panic-guarded via [`contain`] with [`OnPanic::Deopt`].
unsafe fn jit_putfield_int_body(obj_ptr: i64, field_index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) receiver
    // identically: take the existing null path instead of dereferencing it (the
    // field/class/header read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // objects always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
        note_dropped_putfield(obj_ptr, field_index, "implausible-receiver", false);
        return;
    }
    // DIAGNOSTIC (gated by CRATONVM_DBG_JIT_PUTFIELD, one-shot, zero release
    // cost when off): a JIT operand-stack miscompile can hand this helper a
    // non-canonical receiver (e.g. the int/boolean `1` instead of an object
    // pointer), and the unchecked write below then faults through ~null. When
    // enabled, report obj_ptr/field_index + the containing JIT method BEFORE
    // dereferencing, so the miscompiled method/bytecode can be pinned. (Pinned
    // case: avrora real-RAF -> LegacyInstrVisitor.visit(CPI), obj_ptr=0x1.)
    if crate::runtime::env_cache::jit_putfield_diag() {
        let bits = obj_ptr as u64;
        if (bits & 0x7) != 0 || bits >= (1u64 << 48) {
            use std::sync::atomic::{AtomicBool, Ordering};
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "[JIT-PFI-BAD] non-canonical receiver obj_ptr=0x{:x} field_index={} val=0x{:x} (write would target 0x{:x}) current_jit_callee={}",
                    bits, field_index, val as u64,
                    bits.wrapping_add((HEADER_SIZE + field_index as usize * SLOT_SIZE) as u64),
                    current_jit_callee(),
                );
            }
        }
    }
    if !jit_putfield_slot_in_bounds(obj_ptr, field_index) {
        note_dropped_putfield(obj_ptr, field_index, "slot-out-of-bounds", true);
        return;
    }
    // SAFETY: obj_ptr is non-null, field slot is within the object's allocated
    // region; `jit_field_cell_ptr` packs the offset for a compact object.
    let (ptr, storage) = jit_field_cell_ptr(obj_ptr, field_index);
    if crate::runtime::env_cache::jit_pfi_trace() {
        // Read existing value to see if we're overwriting a ref with an int
        let existing = if let Some(storage) = storage {
            cratonvm_types::read_compact_field(ptr, storage, std::sync::atomic::Ordering::Relaxed)
        } else {
            // Screened even though this is diagnostic-only: `{:?}` on a `Value`
            // is itself a `match` over the discriminant, so an unscreened read
            // here would crash the very run that armed the trace.
            jit_read_value_cell_checked(ptr as *const Value, "jit_putfield_int/pfi-trace")
        };
        let cid_off = obj_ptr as *const u8;
        let cid: u32 = std::ptr::read(cid_off as *const u32);
        eprintln!("[JIT-PFI] obj=0x{:x} class_id={} field_index={} val=0x{:x} (val_as_i32={}) prev_value={:?}",
            obj_ptr as usize, cid, field_index, val as u64, val as i32, existing);
    }
    // Atomic per-word store: the concurrent GC marker may read this 16-byte
    // slot at the same time (it scans object fields concurrently). See
    // `cratonvm_types::write_value_atomic`.
    note_jit_putfield_watch(obj_ptr, field_index, Value::Int(val as i32));
    if let Some(storage) = storage {
        cratonvm_types::write_compact_field(
            ptr,
            storage,
            Value::Int(val as i32),
            std::sync::atomic::Ordering::Relaxed,
        );
    } else {
        cratonvm_types::write_value_atomic(ptr as *mut Value, Value::Int(val as i32));
    }
}

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index was resolved at JIT compile time to a valid slot.
pub unsafe extern "C" fn jit_putfield_long(obj_ptr: i64, field_index: i64, val: i64) {
    contain("jit_putfield_long", OnPanic::Deopt, (), || {
        jit_putfield_long_body(obj_ptr, field_index, val)
    });
}

/// Body of [`jit_putfield_long`]. Panic-guarded via [`contain`] with [`OnPanic::Deopt`].
unsafe fn jit_putfield_long_body(obj_ptr: i64, field_index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) receiver
    // identically: take the existing null path instead of dereferencing it (the
    // field/class/header read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // objects always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
        note_dropped_putfield(obj_ptr, field_index, "implausible-receiver", false);
        return;
    }
    if !jit_putfield_slot_in_bounds(obj_ptr, field_index) {
        note_dropped_putfield(obj_ptr, field_index, "slot-out-of-bounds", true);
        return;
    }
    // SAFETY: obj_ptr is non-null, field slot is within the object's allocated
    // region; `jit_field_cell_ptr` packs the offset for a compact object.
    let (ptr, storage) = jit_field_cell_ptr(obj_ptr, field_index);
    // Atomic per-word store (concurrent-GC torn-read safety; see
    // `write_value_atomic`).
    note_jit_putfield_watch(obj_ptr, field_index, Value::Long(val));
    if let Some(storage) = storage {
        cratonvm_types::write_compact_field(
            ptr,
            storage,
            Value::Long(val),
            std::sync::atomic::Ordering::Relaxed,
        );
    } else {
        cratonvm_types::write_value_atomic(ptr as *mut Value, Value::Long(val));
    }
}

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index was resolved at JIT compile time to a valid slot.
pub unsafe extern "C" fn jit_putfield_float(obj_ptr: i64, field_index: i64, val: i64) {
    contain("jit_putfield_float", OnPanic::Deopt, (), || {
        jit_putfield_float_body(obj_ptr, field_index, val)
    });
}

/// Body of [`jit_putfield_float`]. Panic-guarded via [`contain`] with [`OnPanic::Deopt`].
unsafe fn jit_putfield_float_body(obj_ptr: i64, field_index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) receiver
    // identically: take the existing null path instead of dereferencing it (the
    // field/class/header read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // objects always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
        note_dropped_putfield(obj_ptr, field_index, "implausible-receiver", false);
        return;
    }
    if !jit_putfield_slot_in_bounds(obj_ptr, field_index) {
        note_dropped_putfield(obj_ptr, field_index, "slot-out-of-bounds", true);
        return;
    }
    // SAFETY: obj_ptr is non-null, field slot is within the object's allocated
    // region; `jit_field_cell_ptr` packs the offset for a compact object.
    let (ptr, storage) = jit_field_cell_ptr(obj_ptr, field_index);
    // Atomic per-word store (concurrent-GC torn-read safety; see
    // `write_value_atomic`).
    let value = Value::Float(f32::from_bits(val as u32));
    note_jit_putfield_watch(obj_ptr, field_index, value);
    if let Some(storage) = storage {
        cratonvm_types::write_compact_field(
            ptr,
            storage,
            value,
            std::sync::atomic::Ordering::Relaxed,
        );
    } else {
        cratonvm_types::write_value_atomic(ptr as *mut Value, value);
    }
}

// SAFETY: Called from JIT-compiled code. obj_ptr must be 0 (null) or a valid heap pointer
// to a live object. field_index was resolved at JIT compile time to a valid slot.
pub unsafe extern "C" fn jit_putfield_double(obj_ptr: i64, field_index: i64, val: i64) {
    contain("jit_putfield_double", OnPanic::Deopt, (), || {
        jit_putfield_double_body(obj_ptr, field_index, val)
    });
}

/// Body of [`jit_putfield_double`]. Panic-guarded via [`contain`] with [`OnPanic::Deopt`].
unsafe fn jit_putfield_double_body(obj_ptr: i64, field_index: i64, val: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) receiver
    // identically: take the existing null path instead of dereferencing it (the
    // field/class/header read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // objects always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
        note_dropped_putfield(obj_ptr, field_index, "implausible-receiver", false);
        return;
    }
    if !jit_putfield_slot_in_bounds(obj_ptr, field_index) {
        note_dropped_putfield(obj_ptr, field_index, "slot-out-of-bounds", true);
        return;
    }
    // SAFETY: obj_ptr is non-null, field slot is within the object's allocated
    // region; `jit_field_cell_ptr` packs the offset for a compact object.
    let (ptr, storage) = jit_field_cell_ptr(obj_ptr, field_index);
    // Atomic per-word store (concurrent-GC torn-read safety; see
    // `write_value_atomic`).
    let value = Value::Double(f64::from_bits(val as u64));
    note_jit_putfield_watch(obj_ptr, field_index, value);
    if let Some(storage) = storage {
        cratonvm_types::write_compact_field(
            ptr,
            storage,
            value,
            std::sync::atomic::Ordering::Relaxed,
        );
    } else {
        cratonvm_types::write_value_atomic(ptr as *mut Value, value);
    }
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// obj_ptr must be 0 (null) or a valid heap pointer to a live object.
// val is 0 (null) or a raw pointer to a live heap object. Write barrier is issued
// for non-null stores to maintain generational GC invariants.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putfield_object(
    vm_ptr: i64,
    obj_ptr: i64,
    field_index: i64,
    val: i64,
) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) receiver
    // identically: take the existing null path instead of dereferencing it (the
    // field/class/header read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // objects always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
        note_dropped_putfield(obj_ptr, field_index, "implausible-receiver", false);
        return;
    }
    let obj_ref = ObjectRef::from_raw(obj_ptr as usize as *mut u8);
    let value = if val == 0 {
        Value::Object(None)
    } else {
        Value::Object(Some(ObjectRef::from_raw(val as usize as *mut u8)))
    };
    // Bounds check against the object's declared slot count. The
    // interpreter's `GenHeap::set_field` silently drops a write whose
    // index falls past the object's layout (synthetic/real-JDK layout
    // drift); the JIT helper previously did a raw unchecked
    // `obj + HEADER + index*SLOT` write, so an out-of-range `field_index`
    // overflowed into the *neighbouring* heap object — silent corruption
    // that surfaced as a delayed SIGSEGV far from the offending putfield
    // (observed in Tomcat: JIT-compiled `Catalina.setParentClassLoader`).
    // Match the interpreter: drop the write instead of corrupting the heap.
    // Reads num_slots straight from the object header (same check
    // `jit_putfield_int` uses) — the `heap.num_fields` virtual-dispatch
    // route lands on the same header word at several times the cost, and
    // this helper runs once per reference field store.
    //
    // gcf f2/p (`gcf-f1p-jit-putfield-object-resolves-the-compact-layout-up-to-four-times`):
    // a COMPACT receiver's slot is resolved first, once. A `Some` IS the
    // bounds test -- the layout is looked up by the class's field count and
    // `field_disp(field_index)?` is `None` past it -- so the `num_slots()`
    // read (a second layout-count lookup for a compact header) runs only for
    // a legacy receiver or a compact one that did not resolve, which is
    // exactly the set it used to drop.
    let compact_slot = jit_compact_field_slot(obj_ptr, field_index);
    if compact_slot.is_none() && !jit_putfield_slot_in_bounds(obj_ptr, field_index) {
        note_dropped_putfield(obj_ptr, field_index, "slot-out-of-bounds", true);
        return;
    }
    // zt t19/zb2 (zr1br A2b): on a ZGC heap armed with COLORED stores (tests
    // only; production refuses the mode) the heap accessor makes the store:
    // SATB of the decoded old value while marking, the good-colored word, the
    // card. Both raw arms below read the old word as a pointer and store a
    // plain one. The process flag first: an unarmed run pays one relaxed load.
    if cratonvm_types::zgc_read_barrier_armed() {
        let heap = heap_from_vm(vm_ptr);
        if heap.zgc_load_barrier_colored_stores() {
            heap.set_field(obj_ref, field_index as usize, value);
            return;
        }
    }
    if crate::runtime::env_cache::jit_pfo_trace() {
        let cid_off = obj_ptr as *const u8;
        let obj_cid: u32 = std::ptr::read(cid_off as *const u32);
        // Surface only the suspect bit-patterns: invalid kind byte or
        // implausibly-large array_length. Real refs to live objects pass
        // through silently.
        let val_class_id = if val != 0 {
            let v_cid_ptr = val as *const u8;
            std::ptr::read(v_cid_ptr as *const u32)
        } else {
            0
        };
        let val_kind = if val != 0 {
            cratonvm_types::kind_tag_at(val as *const u8)
        } else {
            0
        };
        let val_arrlen = if val != 0 {
            let len_ptr = (val as *const u8).add(cratonvm_types::ARRAY_LENGTH_OFFSET);
            std::ptr::read(len_ptr as *const u32)
        } else {
            0
        };
        if val != 0 && (val_kind > 1 || val_arrlen > 1_000_000) {
            eprintln!("[JIT-PFO] obj=0x{:x} obj_cid={} field_index={} val=0x{:x} val_cid={} val_kind={} val_arrlen=0x{:x}",
                obj_ptr as usize, obj_cid, field_index, val as u64, val_class_id, val_kind, val_arrlen);
        }
    }
    // Compact layout: reference fields are bare 8-byte pointers. Preserve the
    // SATB pre-barrier on the old 8-byte ref, then route through the heap's
    // compact-aware `set_field` (8-byte store + generational card barrier).
    if let Some((off, storage)) = compact_slot {
        if !storage.is_reference() {
            heap_from_vm(vm_ptr).set_field(obj_ref, field_index as usize, value);
            return;
        }
        let heap = heap_from_vm(vm_ptr);
        // SAFETY: `off` is within the object body (slot bounds-checked above).
        let old_raw: u64 = read_ref_slot((obj_ptr as *const u8).add(off));
        // gci i12/wb (`gci-i12wb-jit-barrier-review-findings-FIXED-20261015` item 4):
        // not on G1, whose `set_field` below publishes the overwritten value
        // itself (logging it here too logged it twice).
        if old_raw != 0 && !heap.is_g1() {
            // zt t32 (2f): the slot-aware form.
            heap.satb_barrier_slot(
                (obj_ptr as *const u8).add(off),
                Value::Object(Some(ObjectRef::from_raw(old_raw as usize as *mut u8))),
            );
        }
        // gcf f2/p: on the generational heap the resolved slot goes straight
        // to the store + card barrier (`set_resolved_compact_ref_field`, which
        // is `set_field`'s own compact arm and falls back to `set_field` for
        // every input that arm does not cover), instead of `set_field`
        // resolving the layout twice more. G1 and ZGC keep `set_field`: their
        // stores are not the generational one.
        match heap {
            VmHeap::Generational(generational) => generational.set_resolved_compact_ref_field(
                obj_ref,
                field_index as usize,
                off,
                value,
            ),
            _ => heap.set_field(obj_ref, field_index as usize, value),
        }
        return;
    }
    let ptr = obj_ref
        .as_ptr()
        .add(HEADER_SIZE + field_index as usize * SLOT_SIZE);
    // Task #43 (HIGH soundness, deferred from #25/#26): SATB pre-write
    // barrier — the JIT helper equivalent of the interpreter putfield's
    // `shared.mem.heap.satb_barrier(old_value)` at runtime/interpreter.rs:6391.
    // Read the OLD reference before overwriting it so concurrent marking
    // preserves the snapshot-at-the-beginning invariant. The post-store
    // `write_barrier` (card-table dirty) below is necessary but not
    // sufficient on its own — without this pre-barrier the marker can
    // lose any still-live ref reachable only through this slot.
    //
    // `satb_barrier` is the inherent name for the pre-write barrier on
    // `VmHeap` in this codebase. When/if the planned
    // `GarbageCollector::write_barrier_pre` trait alias lands, this call
    // should migrate to it so the debug-build (pre, store, post) triad
    // assertion in `gc/src/vm_heap.rs` can validate slot-identity pairing.
    // Atomic per-word read/write: the slot is read concurrently by the GC
    // marker and (possibly) written by another mutator thread; pair both ends
    // through the atomic helpers so the access is well-defined and tear-free.
    // Screened: a corrupt cell decodes to `Object(None)` and is skipped by the
    // `if let` below, rather than becoming an invalid `Value` and then an
    // `ObjectRef` pushed onto the mark queue.
    let old_value: Value =
        jit_read_value_cell_checked(ptr as *const Value, "jit_putfield_ref/satb-pre");
    if let Value::Object(Some(_)) = old_value {
        let heap = heap_from_vm(vm_ptr);
        heap.satb_barrier(old_value);
    }
    cratonvm_types::write_value_atomic(ptr as *mut Value, value);
    if val != 0 {
        let heap = heap_from_vm(vm_ptr);
        heap.write_barrier(obj_ref, value);
    }
}

/// F-08 — G1's post-write barrier, called from the slow arm of the JIT's
/// INLINE G1 barrier.
///
/// # What the caller has already proved, and what it has not
///
/// The inline sequence reaches this call only when both of its filters have
/// failed to prove there is nothing to remember: `val_ptr` is non-null, and
/// `obj_ptr` and `val_ptr` do not lie in the same G1 region. Those are exactly
/// the two conditions `G1Collector::post_write_barrier_rset` itself tests
/// first, so the inline arm is eliding calls the callee would have returned
/// from — not calls it would have acted on. Everything else, including an
/// address outside G1's arena and a destination region that is Free, is left
/// to the callee, which already handles all of it.
///
/// # Why this is not [`jit_write_barrier`]
///
/// That helper routes through `VmHeap::write_barrier`, which carries a
/// `debug_assert!` requiring an SATB pre-barrier on the same thread whenever a
/// mark cycle is active. The assertion is correct for a general store and
/// wrong for this caller: the inline arm stores only when no pre-barrier is
/// owed -- the field's OLD value is NULL, or (g1put, the optimizing tier's
/// `CRATONVM_JIT_IR_G1_GATED_REF_STORE`) the SATB pre-gate byte is clear, so no
/// queue is active -- and bails to `jit_putfield_object` otherwise. Both are
/// cases `satb_pre_barrier` returns from without logging. No pre-barrier
/// fires, none is owed, and the debug assertion would fire on a correct
/// program.
/// Weakening it would remove the check from every other caller; a separate
/// entry point says the thing once, here.
///
/// It also skips the generational card-marking dispatch entirely, going
/// straight to G1's remembered-set barrier, which is the only collector this
/// helper is ever wired for by the emitter (`g1_inline_barrier_available`
/// requires a published `JIT_G1_BARRIER` table, and only `G1Collector`
/// publishes one). A non-G1 heap reaching here is a no-op rather than a
/// misfiled card: the match below has one arm.
///
/// # Safety
///
/// Called from JIT-compiled code. `vm_ptr` must be a valid `SharedVm` pointer;
/// `obj_ptr` and `val_ptr` are raw heap words and are screened here exactly as
/// [`jit_write_barrier`] screens them, because a stale or garbage receiver must
/// take the null path rather than be dereferenced.
pub unsafe extern "C" fn jit_g1_post_write_barrier(vm_ptr: i64, obj_ptr: i64, val_ptr: i64) {
    // r22 review area barriers review point: the lean post call carries no GC
    // map, which is correct because post_write_barrier_rset does not allocate,
    // does not safepoint, and does not poll for safepoints.
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache,
    // exactly as `jit_write_barrier` does. Omitting it would leave a stale
    // frame census behind a call that can reach the collector.
    crate::jit::conservative_roots::note_jit_boundary();
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
        return;
    }
    if val_ptr == 0 {
        return;
    }
    let heap = heap_from_vm(vm_ptr);
    if let cratonvm_gc::vm_heap::VmHeap::G1(g1) = heap {
        // JIT round 23 wave 3, lane L4 (`CRATONVM_G1_YOUNG_SOURCE_FILTER`,
        // default ON under `CRATONVM_G1_YOUNG_SOURCE_PREREQ`): HotSpot's
        // young-card filter for compiled stores -- a holder in an Eden /
        // Survivor region owes no edge and no card, because G1 re-walks every
        // young region a pause leaves out of its set before reading a
        // remembered set (`G1Collector::young_holder_skips_post_barrier`).
        if g1.young_holder_skips_post_barrier(ObjectRef::from_raw(obj_ptr as usize as *mut u8)) {
            return;
        }
        g1.post_write_barrier_rset(
            ObjectRef::from_raw(obj_ptr as usize as *mut u8),
            ObjectRef::from_raw(val_ptr as usize as *mut u8),
        );
    }
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// obj_ptr and val_ptr must be 0 (null) or valid heap pointers to live objects.
// Records a generational write barrier so the GC tracks old-to-young references.
pub unsafe extern "C" fn jit_write_barrier(vm_ptr: i64, obj_ptr: i64, val_ptr: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) receiver
    // identically: take the existing null path instead of dereferencing it (the
    // field/class/header read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // objects always pass (8-aligned, ≤47-bit); zero false positives.
    //
    // Not counted as a dropped STORE: this helper records a card, it does not
    // write the field. The store itself was made (or dropped, and counted) by
    // whichever `jit_putfield_*` ran before this.
    //
    // gen r4w4/cards4: under the GENERATIONAL heap `obj_ptr` names the CARD to
    // dirty, and may be an 8-aligned INTERIOR address of an old reference
    // array — the element's slot — which the x64 `aastore` arms pass when the
    // card table is element-precise (`JitCardView::FLAG_PRECISE_REF_ARRAYS`).
    // `GenerationalHeap::write_barrier` uses its first argument as an address
    // only (a range test and a card index; it never reads the header), so the
    // slot's card is dirtied exactly as `write_barrier_ref_array_element` would.
    // The emitter passes an interior address ONLY when the helper table carries
    // a generational card view, so G1's and ZGC's barriers, which do read the
    // object, never see one.
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
        return;
    }
    if val_ptr == 0 {
        return;
    }
    // zt t7/tg (GN-8): the single-pass `aastore` arm of ZGC's age-floor plan
    // under `CRATONVM_JIT_ZGC_ELEMENT_CARD_BARRIER=1` passes the ELEMENT's
    // slot address with bit 0 set in place of the value (it has already
    // tested the value non-null, and a reference is never odd), so the card
    // can be the element's granule (`jit_write_barrier_element_slot`).
    if val_ptr & 1 != 0 {
        jit_write_barrier_element_slot(vm_ptr, obj_ptr, (val_ptr & !1) as usize);
        return;
    }
    let heap = heap_from_vm(vm_ptr);
    let obj_ref = ObjectRef::from_raw(obj_ptr as usize as *mut u8);
    let val_ref = ObjectRef::from_raw(val_ptr as usize as *mut u8);
    let value = Value::Object(Some(val_ref));
    heap.write_barrier(obj_ref, value);
}

/// zt t7/tg (`zgc-zt3gn-generational-mode-review-residuals` GN-8, zr z2/g2's
/// X2): the post barrier of a compiled `aastore` that names the element.
/// `array_ptr` is the (plausible) array, `slot` the element's address; the
/// store has been made. On ZGC this is the interpreter `aastore`'s producer,
/// `ZgcRealHeap::note_ref_store_element`: a large old
/// array is carded per 8-byte granule under `CRATONVM_ZGC_GEN_ELEMENT_CARDS=1`
/// (else the array's object card, as `write_barrier` makes it). Any other
/// collector (the emitter passes the tagged form only on ZGC's age-floor
/// plan, so this arm is a fallback) gets its ordinary barrier with the value
/// the slot now holds.
///
/// # Safety
///
/// `vm_ptr` is a valid `SharedVm`; `array_ptr` a live reference array and
/// `slot` the address of one of its elements (the emitter computed it from
/// the bounds-checked index it stored through).
unsafe fn jit_write_barrier_element_slot(vm_ptr: i64, array_ptr: i64, slot: usize) {
    let heap = heap_from_vm(vm_ptr);
    match heap {
        #[cfg(feature = "zgc")]
        VmHeap::Zgc(zgc) => {
            let ptr = array_ptr as usize as *const u8;
            let length = *(ptr.add(ARRAY_LENGTH_OFFSET) as *const u32) as usize;
            zgc.note_ref_store_element(array_ptr as usize, slot, length);
        }
        _ => {
            let raw = read_ref_slot(slot as *const u8);
            if raw != 0 {
                let obj_ref = ObjectRef::from_raw(array_ptr as usize as *mut u8);
                let value = Value::Object(Some(ObjectRef::from_raw(raw as usize as *mut u8)));
                heap.write_barrier(obj_ref, value);
            }
        }
    }
}

// Round-7 fix (CRIT, UAF in JIT): SATB pre-write barrier helper.
//
// Logs the OLD reference value to the per-thread SATB buffer before the
// JIT-compiled aastore / putfield / putstatic actually overwrites the
// reference slot. This preserves the snapshot-at-the-beginning invariant
// the concurrent marker relies on; without it, JIT-overwritten still-live
// references silently disappear from the mark closure and become UAF on
// the next mixed evacuation.
//
// The interpreter calls `shared.mem.heap.satb_barrier(old_value)` at every
// ref-store site (interpreter.rs lines 4093, 5164, 5961, 6206). This
// helper is the JIT-callable equivalent.
//
// Fast path: when concurrent marking is idle (`SatbQueue::is_active() ==
// false`), the helper performs a single Acquire load and returns — no
// lock taken, no buffer touched. In steady state the cost is two
// register operations and a not-taken branch.
//
// SAFETY: Called from JIT-compiled code. `vm_ptr` must be a valid
// SharedVm pointer. `old_ref` is 0 (null) or the raw address of the
// reference value that was about to be overwritten; null is filtered out
// inside `satb_barrier` and `satb_thread_local_log`, so passing it is
// safe but wasteful.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_satb_pre_write_barrier(vm_ptr: i64, old_ref: i64) {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if vm_ptr == 0 || old_ref == 0 {
        return;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let old_obj = ObjectRef::from_raw(old_ref as usize as *mut u8);
    vm.mem.heap.satb_barrier(Value::Object(Some(old_obj)));
}

// ---------------------------------------------------------------------------
// Static field helpers
// ---------------------------------------------------------------------------

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
/// Lock-free "this class is already initialized" memo for the JIT static-field
/// helpers.
///
/// `ensure_class_initialized_shared` already has an atomic fast path, but
/// reaching that atomic still costs a `class_manager.read()` acquisition — per
/// `getstatic`, in compiled code. Class initialization is **monotonic** (JVMS
/// §5.5: a class that has completed initialization never returns to an
/// uninitialized state, and redefinition does not re-run `<clinit>`), so once
/// the answer is "initialized" it can be memoized forever without invalidation.
///
/// A dense bitmap indexed by `ClassId` makes the steady-state check one relaxed
/// load plus a bit test. Ids outside the bitmap simply take the slow path, so
/// the capacity bound is a performance choice, not a correctness one.
///
/// # VM scoping (2026-08-01; per VM since interpreter round i1 wave 22)
///
/// `ClassId` is only unique *within* a VM. The bitmap used to be an unqualified
/// process global, so VM A marking id 5000 initialized made
/// [`jit_getstatic`]/[`jit_putstatic_object`] in VM B skip the JVMS §5.5
/// initialization check for *its* class 5000 — a compiled `getstatic` that is
/// the first-ever access to that class then reads the zero-initialized
/// placeholder (`Value::Int(0)`, decoded as a null reference), which is exactly
/// the `LineWrapper$FlushType` NPE this memo's own doc describes, resurrected
/// by a second VM in the same process.
///
/// The 2026-08-01 fix gave the one table to the first VM that touched it and
/// sent every other VM to the authoritative `ensure_class_initialized_shared`
/// path forever: correct, but a second live VM, and every VM created after the
/// first was dropped, paid a class-manager read lock on each compiled static
/// access for the life of the process. Since interpreter round i1 wave 22
/// (lane L5) each VM owns its table (`ClassRealm::class_init_bits`), so every
/// VM has the fast path and none can read another's bits. See
/// `vm-jit-cache-keying.md`.
mod class_init_memo {
    use crate::vm::SharedVm;

    #[inline]
    pub fn is_initialized(vm: &SharedVm, class_id: u32) -> bool {
        vm.classes.class_init_bits.is_set(class_id)
    }

    /// The init-reset generation to load BEFORE the read that finds a class
    /// initialized (see `cratonvm_classloading::ClassInitBits`).
    #[inline]
    pub fn generation(vm: &SharedVm) -> u64 {
        vm.classes.class_init_bits.reset_generation()
    }

    /// `generation_before`: [`generation`] loaded before the read that found
    /// the class initialized. A stub upgrade since then (interpreter round i2
    /// wave 61, lane L5) leaves the bit clear: the one exception to the
    /// monotonicity this memo relies on.
    #[inline]
    pub fn mark_initialized(vm: &SharedVm, class_id: u32, generation_before: u64) {
        vm.classes
            .class_init_bits
            .set_as_of(class_id, generation_before);
    }
}

// `system_class_memo` — a per-`ClassId` "is this `java/lang/System`?" bitmap
// pair, owned by the first VM to query it (every other VM took a
// `class_manager` read lock and a name compare on EVERY compiled `getstatic`)
// — was deleted in interpreter round i1 wave 5 (lane L4). `jit_getstatic` now
// asks `interpreter::static_may_be_system`, the interpreter's screen: one
// relaxed load of this VM's `ClassRealm::system_class_id` and a compare, for
// every VM alike.

// `system_class_id` — a process-global `AtomicU32` holding `java/lang/System`'s
// `ClassId`, "resolved once" — was DELETED on 2026-08-01. It had no callers
// anywhere in the repository (`system_class_memo::is_system` superseded it),
// and it was wrong for the same reason that memo was: the first VM in the
// process to resolve System latched its id forever, so in a second VM the
// `System.out`/`err` intercept would fire on whatever class happened to hold
// that id and never on the real `java/lang/System`. Do not reintroduce it
// without a `vm_identity` in the key.

/// Record a CONFIRMED class initialization in the lock-free memo.
///
/// Called from `finalize_class_init` (the single authoritative end of a
/// successful `<clinit>`), so the memo answers for every initialized class
/// rather than only for those a compiled `getstatic` has already missed on.
/// Idempotent, lock-free, and a no-op for a VM that does not own the table.
///
/// Also bumps the optimizing tier's inputs generation: a newly initialized
/// class can turn an IR-gate "no evidence" refusal (an unresolvable
/// `new`/`checkcast`/inline plan) into an admissible build, so every
/// input-stamped refusal recorded before this point must stop being consulted
/// (`cratonvm_jit::ir_evidence`, "Refusal memo"). One atomic add.
///
/// `init_generation`: the init-reset generation read under the class-manager
/// write lock that marked the class initialized (`ClassInitBits::set_as_of`).
pub fn note_class_initialized(vm: &SharedVm, class_id: ClassId, init_generation: u64) {
    class_init_memo::mark_initialized(vm, class_id.as_u32(), init_generation);
    cratonvm_jit::ir_evidence::bump_inputs_generation();
}

/// `CRATONVM_JIT_IR_VH_FIELD_FOLD` (default ON), VM half: whether
/// [`note_class_initialized_varhandles`] files anything at all. The compiler
/// half reads the same name; with it off, neither side does any work.
fn varhandle_field_fold_registered() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_VH_FIELD_FOLD")
    })
}

/// File the `static final VarHandle` fields of a class whose `<clinit>` just
/// completed, for the optimizing tier's constant fold of `VarHandle.get*`
/// (`UnboxOp::FieldInt` / `FieldLong`).
///
/// A field is filed when it is `static final`, declared `VarHandle`, holds a
/// handle the natives registered as an INSTANCE-field handle with a plan
/// ([`cratonvm_native_builtins::lang_invoke::varhandle_instance_field_plan_for_handle`]
/// refuses invoke-exact handles, static, array and view handles), reads as an
/// `int` or `long`, and its coordinate class is `final`: the folded load guards
/// the receiver's EXACT class, and for a non-final class that guard would
/// spuriously fail on every subclass instance.
///
/// Runs on the mutator that finished the initialisation, so reading the static
/// and the handle's header is as safe as any interpreter `getstatic`. The
/// filed record is pure data (no address), so a later relocation of the handle
/// cannot make it stale: the handle IS this field's value forever.
pub fn note_class_initialized_varhandles(vm: &SharedVm, class_id: ClassId) {
    // nio21 wave 11 L5 (W9L2-2): the same walk also files the constant-handle
    // plans both tiers' `VarHandle` prefixes read (`vh_const_file_plan`),
    // under their own switch.
    let fold = varhandle_field_fold_registered();
    let consts = cratonvm_jit::ir_lower::vh_const_inline_enabled();
    if !fold && !consts {
        return;
    }
    // nio21 wave 12 L2: this class's supers display, for the prefixes'
    // subclass test. Every class reaches here once, after its superclasses.
    if consts && cratonvm_jit::ir_lower::vh_const_subtype_enabled() {
        vh_const_publish_display(vm, class_id, true);
    }
    // Static field indices are the STATIC-field enumeration order of the
    // class's own fields (`typed_default_static_slots`).
    let candidates: Vec<u32> = {
        let cm = vm.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return;
        };
        class
            .fields
            .iter()
            .filter(|f| f.is_static())
            .enumerate()
            .filter(|(_, f)| f.is_final() && &*f.descriptor == "Ljava/lang/invoke/VarHandle;")
            .map(|(index, _)| index as u32)
            .collect()
    };
    if !candidates.is_empty() {
        vm.classes.static_varhandle_holders.write().insert(class_id);
    }
    // j27 wave 2 (lane VHCOMP, `CRATONVM_JIT_VH_CONST_STATIC`): the keys a
    // static constant-handle cell may be registered for (below the loop).
    let static_keys: Vec<(u32, u32)> = candidates
        .iter()
        .map(|&index| (class_id.as_u32(), index))
        .collect();
    // JIT r23 wave 6, lane L2: which candidate filed a constant-handle plan,
    // and where each other one stopped (`CRATONVM_DBG_JITC=1`). Added for
    // `CompletableFuture.RESULT`, whose `setRelease` neither tier serves
    // inline while a byte-for-byte user-class clone's is
    // (`r21-completablefuture-stage-cost-is-allocation-and-calls`).
    let trace = |index: u32, what: &str| {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] vh-const-file holder={} static={index}: {what}",
                class_id.as_u32()
            );
        }
    };
    for index in candidates {
        let Value::Object(Some(handle)) =
            crate::vm::get_static_shared(vm, class_id, index as usize)
        else {
            trace(index, "static is not a handle (yet)");
            continue;
        };
        let raw = handle.as_ptr() as usize;
        // SAFETY: `raw` was read from a static this thread just observed; the
        // helper validates it as a live object before any dereference.
        let Some((_, key)) = (unsafe { direct_receiver_facts(vm, raw) }) else {
            trace(index, "no receiver facts");
            continue;
        };
        // JIT round 20 wave 3, lane atomics: a handle the JDK's own
        // `findVarHandle` built (`--jdk-only`) has no side-table row until a
        // `VarHandle` door first adopts it -- after this `<clinit>` -- so the
        // natives answer no plan here and the handle used to be filed never.
        // Read its plan off the real object instead (`vh_const_real_handle_plan`).
        let Some(plan) =
            cratonvm_native_builtins::lang_invoke::varhandle_instance_field_plan_for_handle(
                vm.vm_identity,
                key,
                raw,
            )
            .or_else(|| vh_const_real_handle_plan(vm, handle))
        else {
            // JIT r20 wave 18 (lane xVhArr, AT-2): an `int[]` / `long[]`
            // element handle files its own plan word for the prefixes' array arm.
            if consts {
                if let Some(kind) = vh_const_array_handle_kind(vm, handle) {
                    vh_const_file_array_plan(vm, class_id, index, kind);
                }
            }
            trace(index, "no field plan (natives row or real-handle read)");
            continue;
        };
        // A genuine JDK handle can be adopted with a plain metadata row even
        // when its live final exact flag is set. An exact call at a broader
        // coordinate type must keep WrongMethodTypeException before all
        // coordinate faults; a bare field load cannot judge that descriptor.
        let Some(exact_slot) = crate::runtime::interpreter::var_handle_exact_gate(vm) else {
            trace(index, "no VarHandle.exact slot");
            continue;
        };
        if crate::runtime::interpreter::var_handle_receiver_is_exact(vm, handle, exact_slot) {
            trace(index, "an exact handle");
            continue;
        }
        if consts {
            vh_const_file_plan(vm, class_id, index, &plan);
            trace(index, "filed");
        }
        if !fold || !matches!(plan.value_desc, b'I' | b'J') || plan.class_id == 0 {
            continue;
        }
        let Ok(slot) = u16::try_from(plan.field_index) else {
            continue;
        };
        let receiver_is_final = vm
            .classes
            .class_manager
            .read()
            .get_class(ClassId::new(plan.class_id))
            .is_some_and(|c| c.is_final());
        if !receiver_is_final {
            continue;
        }
        let row = cratonvm_jit::StaticVarHandleField {
            receiver_class_id: plan.class_id,
            slot,
            value_desc: plan.value_desc,
        };
        let previous = vm
            .classes
            .static_varhandle_fields
            .write()
            .insert((class_id.as_u32(), index), row);
        if previous.is_none_or(|old| {
            (old.receiver_class_id, old.slot, old.value_desc)
                != (row.receiver_class_id, row.slot, row.value_desc)
        }) {
            // Reopen an input-stamped refusal that saw the unresolved handle.
            cratonvm_jit::ir_evidence::bump_inputs_generation();
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] static-varhandle-field filed holder={} index={} receiver={} slot={} type={}",
                    class_id.as_u32(), index, row.receiver_class_id, row.slot, row.value_desc as char
                );
            }
        }
    }
    vh_const_register_static_cells(vm, &static_keys);
}

/// j27 wave 2 (lane VHCOMP, `CRATONVM_JIT_VH_CONST_STATIC`): an unfilled
/// static constant-handle cell for each `static final VarHandle` of `keys`
/// that filed no instance or array plan above (a static-field handle, or one
/// not yet known); the natives' static arms fill it
/// (`crate::jit::vh_static_cells`).
fn vh_const_register_static_cells(vm: &SharedVm, keys: &[(u32, u32)]) {
    if !cratonvm_jit::ir_lower::vh_static_const::vh_const_static_enabled() {
        return;
    }
    let open: Vec<(u32, u32)> = keys
        .iter()
        .copied()
        .filter(|&key| vm.jit.vh_const_plans.plan_word(key) == 0)
        .collect();
    if !open.is_empty() {
        vm.jit.vh_static_cells.register(&open);
    }
}

/// Refresh late-resolved constant handles on the mutator before compilation.
/// A lookup-created handle can still have field_index == -1 at its holder's
/// clinit completion; its first checked access resolves the slot later. A
/// one-shot clinit feed then leaves every optimizing read on the helper.
/// Background compilers keep consuming pure filed data, never heap objects.
pub(crate) fn refresh_static_varhandles_before_compile(vm: &SharedVm, class_id: ClassId) {
    // Native excusal is restricted to NonMovingPause handshakes, not a GC.
    // This mutator walk allocates no Java objects and reaches no safepoint.
    if class_init_memo::is_initialized(vm, class_id.as_u32()) {
        note_class_initialized_varhandles(vm, class_id);
    }
}

/// A first checked access resolved a handle after the holder's clinit feed.
/// This is a cold native-to-VM notification, outside the metadata table lock.
/// It also reaches a holder whose getter was eagerly compiled before its first
/// execution, so no later interpreter tier-up offer visits that getter.
pub(crate) fn refresh_resolved_static_varhandles(vm: &SharedVm) {
    let holders: Vec<_> = vm
        .classes
        .static_varhandle_holders
        .read()
        .iter()
        .copied()
        .collect();
    for holder in holders {
        refresh_static_varhandles_before_compile(vm, holder);
    }
}

/// JIT round 20 wave 11 (lane pIR; books `jit-r20-nio-proposals-w6-L3.md`
/// L3W6-2, `-w6-L4.md` W6L4-5): file the length of every trusted `static
/// final` array field of a class whose `<clinit>` just completed, in this VM's
/// `ClassRealm::static_final_array_lengths`, for the optimizing tier's
/// `arraylength` fold (`CRATONVM_JIT_IR_STATIC_FINAL_ARRAY_LENGTH`; the
/// compiler half reads the same switch, so with it off neither side works).
///
/// Trusted exactly as `CompileRequest::cp_static_field_final_resolver` trusts
/// a field (`static_field_is_trusted_final` in `jit_bridge`): `ACC_STATIC |
/// ACC_FINAL`, an array descriptor, declared by a class the application loader
/// or a user-defined loader defined from a class file of version 53+ (where a
/// `putstatic` of a final outside `<clinit>` is an `IllegalAccessError`).
/// Bootstrap and platform classes are never filed (the JDK rewrites some of
/// its static finals at boot), and neither is `java/lang/System`. HotSpot's C2
/// trusts a `static final` of an initialised class the same way (JLS 17.5.3
/// lets a compiler ignore reflective and `Unsafe` writes to it).
///
/// Runs on the mutator that finished the initialisation, so reading the static
/// and the array's header is as safe as an interpreter `getstatic` +
/// `arraylength`. A `null` field, or a value that is not an array, is not
/// filed.
pub fn note_class_initialized_final_array_lengths(vm: &SharedVm, class_id: ClassId) {
    if !cratonvm_jit::ir_optimize::ir_static_final_array_length_enabled() {
        return;
    }
    let candidates: Vec<u32> = {
        use cratonvm_types::ClassLoaderId;
        let cm = vm.classes.class_manager.read();
        if !matches!(
            cm.get_loader_id(class_id),
            Some(ClassLoaderId::Application | ClassLoaderId::UserDefined(_))
        ) {
            return;
        }
        let Some(class) = cm.get_class(class_id) else {
            return;
        };
        if class.version.major < 53 || &*class.name == "java/lang/System" {
            return;
        }
        // Static field indices are the STATIC-field enumeration order of the
        // class's own fields, as `note_class_initialized_varhandles` reads them.
        class
            .fields
            .iter()
            .filter(|f| f.is_static())
            .enumerate()
            .filter(|(_, f)| f.is_final() && f.descriptor.starts_with('['))
            .map(|(index, _)| index as u32)
            .collect()
    };
    if candidates.is_empty() {
        return;
    }
    let mut filed: Vec<((u32, u32), i32)> = Vec::new();
    for index in candidates {
        let Value::Object(Some(array)) = crate::vm::get_static_shared(vm, class_id, index as usize)
        else {
            continue;
        };
        if vm.mem.heap.kind_of(array) != cratonvm_types::ObjectKind::Array {
            continue;
        }
        let Ok(length) = i32::try_from(vm.mem.heap.array_length(array)) else {
            continue;
        };
        filed.push(((class_id.as_u32(), index), length));
    }
    if filed.is_empty() {
        return;
    }
    if crate::runtime::env_cache::dbg_jitc() {
        for &((holder, index), length) in &filed {
            eprintln!(
                "[cratonvm-jitc] static final array length filed class_id={holder} field={index} length={length}"
            );
        }
    }
    vm.classes.static_final_array_lengths.write().extend(filed);
}

/// File the class of the object each trusted `static final` reference field
/// of a class whose `<clinit>` just completed holds, in this VM's
/// `ClassRealm::static_final_ref_classes`, for the optimizing tier's exact
/// receiver proof (`CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`; the compiler
/// half reads the same switch).
///
/// Trusted as [`note_class_initialized_final_array_lengths`] trusts a field,
/// except that a class file older than 53 is admitted too when no method but
/// `<clinit>` writes the field (JVMS 6.5 `putstatic` lets only the declaring
/// class write a final, so its own methods are the whole search; HotSpot's
/// `has_initialized_final_update` rule). Reflective and `Unsafe` writes are
/// ignored, as JLS 17.5.3 lets a compiler do and C2 does.
///
/// Runs on the mutator that finished the initialisation, so reading the
/// static and its object's header is as safe as an interpreter `getstatic`.
/// A `null` field or an array is not filed.
pub fn note_class_initialized_final_ref_classes(vm: &SharedVm, class_id: ClassId) {
    if !cratonvm_jit::ir::ir_static_final_exact_class_enabled() {
        return;
    }
    let candidates: Vec<u32> = {
        use cratonvm_reader::constant_pool::ConstantPoolEntry;
        use cratonvm_types::ClassLoaderId;
        let cm = vm.classes.class_manager.read();
        if !matches!(
            cm.get_loader_id(class_id),
            Some(ClassLoaderId::Application | ClassLoaderId::UserDefined(_))
        ) {
            return;
        }
        let Some(class) = cm.get_class(class_id) else {
            return;
        };
        if &*class.name == "java/lang/System" {
            return;
        }
        let finals: Vec<(u32, &str)> = class
            .fields
            .iter()
            .filter(|f| f.is_static())
            .enumerate()
            .filter(|(_, f)| f.is_final() && f.descriptor.starts_with('L'))
            .map(|(index, f)| (index as u32, &*f.name))
            .collect();
        if finals.is_empty() {
            return;
        }
        // Before 53, the names some method other than `<clinit>` writes.
        let mut written: Vec<&str> = Vec::new();
        if class.version.major < 53 {
            for method in class.methods.iter().filter(|m| &*m.name != "<clinit>") {
                let Some(code) = method.code() else {
                    continue;
                };
                let bytes = code.code.as_bytes();
                let Some(cps) = cratonvm_jit::ir::putstatic_cp_indices(bytes, bytes.len()) else {
                    // Undecodable: trust none of this class's finals.
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] static final filing skipped for {}: {}{} does not decode",
                            class.name, method.name, method.descriptor
                        );
                    }
                    return;
                };
                for cp in cps {
                    let Some(ConstantPoolEntry::FieldReference {
                        class_index,
                        name_and_type_index,
                    }) = class.constant_pool.get(cp)
                    else {
                        continue;
                    };
                    if class.constant_pool.get_class_name(*class_index) != Some(&*class.name) {
                        continue;
                    }
                    if let Some((name, _)) =
                        class.constant_pool.get_name_and_type(*name_and_type_index)
                    {
                        written.push(name);
                    }
                }
            }
        }
        finals
            .into_iter()
            .filter(|(_, name)| !written.contains(name))
            .map(|(index, _)| index)
            .collect()
    };
    let mut filed: Vec<((u32, u32), u32)> = Vec::new();
    for index in candidates {
        // A trusted `static final` that `<clinit>` left null is filed as class
        // `0`: the optimizing tier reads it as the constant null
        // (`PlatformDependent.UNSAFE_UNAVAILABILITY_CAUSE`, behind every
        // `hasUnsafe()`), as HotSpot folds a static final.
        let obj = match crate::vm::get_static_shared(vm, class_id, index as usize) {
            Value::Object(Some(obj)) => obj,
            Value::Object(None) => {
                filed.push(((class_id.as_u32(), index), 0));
                continue;
            }
            _ => continue,
        };
        if vm.mem.heap.kind_of(obj) != cratonvm_types::ObjectKind::Object {
            continue;
        }
        let value_class = vm.mem.heap.class_id_of(obj);
        if value_class.as_u32() == 0 {
            continue;
        }
        filed.push(((class_id.as_u32(), index), value_class.as_u32()));
    }
    if filed.is_empty() {
        return;
    }
    if crate::runtime::env_cache::dbg_jitc() {
        for &((holder, index), class) in &filed {
            eprintln!(
                "[cratonvm-jitc] static final exact class filed class_id={holder} field={index} value_class={class}"
            );
        }
    }
    vm.classes.static_final_ref_classes.write().extend(filed);
}

/// JIT r23 wave 4 (lane L8, L8-8 stage 4; `CRATONVM_JIT_IR_FFM_ELEMENT_INLINE`,
/// opt-in): file, in this VM's `ClassRealm::ffm_layout_facts`, the access
/// facts (carrier, width, alignment, byte order) of every `static final`
/// field of a class whose `<clinit>` just completed that holds a primitive
/// `ValueLayout` (`ValueLayouts$Of*Impl`), so the optimizing tier's inline
/// `MemorySegment` access can take the layout argument of
/// `seg.get(JAVA_INT, off)` as a constant
/// (`cratonvm_jit::ffm_element_inline::ffm_layout_facts`).
///
/// Trusted fields: every `static final` of a bootstrap class (the JDK's own,
/// `ValueLayout.JAVA_INT` and friends: HotSpot trusts these too); of any other
/// class only a field [`note_class_initialized_final_ref_classes`] already
/// filed (its written-elsewhere screen; that filing runs first and has its own
/// switch). The layout object must be of a bootstrap class with real bytes,
/// and its facts are read from the declaring classes' fields
/// (`AbstractLayout.byteSize` / `byteAlignment`,
/// `ValueLayouts$AbstractValueLayout.order` compared by identity with
/// `ByteOrder.BIG_ENDIAN` / `LITTLE_ENDIAN`). Layouts are immutable, so the
/// facts never go stale. Runs on the mutator that finished the
/// initialisation, as the sibling filings do.
pub fn note_class_initialized_ffm_layout_facts(vm: &SharedVm, class_id: ClassId) {
    use cratonvm_jit::ffm_element_inline as fei;
    use cratonvm_types::ClassLoaderId;
    if !cratonvm_jit::ir::ir_ffm_element_inline_enabled() {
        return;
    }
    // Phase 1, under the class manager: the candidate statics and the slots
    // and constants the reads below need.
    struct Plan {
        candidates: Vec<u32>,
        bootstrap: bool,
        byte_size: usize,
        byte_alignment: usize,
        order: usize,
        byte_order: ClassId,
        big_index: usize,
        little_index: usize,
    }
    let plan = {
        let cm = vm.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return;
        };
        let bootstrap = matches!(cm.get_loader_id(class_id), Some(ClassLoaderId::Bootstrap));
        let candidates: Vec<u32> = class
            .fields
            .iter()
            .filter(|f| f.is_static())
            .enumerate()
            .filter(|(_, f)| f.is_final() && f.descriptor.starts_with("Ljava/lang/foreign/"))
            .map(|(index, _)| index as u32) // Cast: a class's static count fits u32
            .collect();
        if candidates.is_empty() {
            return;
        }
        let slot = |owner: &str, name: &str, desc: &str| {
            let owner = cm.find_bootstrap_class_by_name(owner)?;
            crate::vm::resolve_field_index_in_hierarchy_desc(owner, name, Some(desc), &cm.class_store)
        };
        let (Some(byte_size), Some(byte_alignment), Some(order)) = (
            slot("jdk/internal/foreign/layout/AbstractLayout", "byteSize", "J"),
            slot("jdk/internal/foreign/layout/AbstractLayout", "byteAlignment", "J"),
            slot(
                "jdk/internal/foreign/layout/ValueLayouts$AbstractValueLayout",
                "order",
                "Ljava/nio/ByteOrder;",
            ),
        ) else {
            return;
        };
        let Some(byte_order) = cm.find_bootstrap_class_by_name("java/nio/ByteOrder") else {
            return;
        };
        let Some(order_class) = cm.get_class(byte_order) else {
            return;
        };
        let static_index = |name: &str| {
            order_class
                .fields
                .iter()
                .filter(|f| f.is_static())
                .position(|f| &*f.name == name)
        };
        let (Some(big_index), Some(little_index)) =
            (static_index("BIG_ENDIAN"), static_index("LITTLE_ENDIAN"))
        else {
            return;
        };
        Plan {
            candidates,
            bootstrap,
            byte_size,
            byte_alignment,
            order,
            byte_order,
            big_index,
            little_index,
        }
    };
    let (Value::Object(Some(big)), Value::Object(Some(little))) = (
        crate::vm::get_static_shared(vm, plan.byte_order, plan.big_index),
        crate::vm::get_static_shared(vm, plan.byte_order, plan.little_index),
    ) else {
        return;
    };
    let mut candidates = plan.candidates;
    if !plan.bootstrap {
        // Read after the class manager is released: no lock nests in it.
        let trusted = vm.classes.static_final_ref_classes.read();
        candidates.retain(|&i| trusted.contains_key(&(class_id.as_u32(), i)));
    }
    let mut filed: Vec<((u32, u32), i64)> = Vec::new();
    for index in candidates {
        let Value::Object(Some(layout)) = crate::vm::get_static_shared(vm, class_id, index as usize)
        else {
            continue;
        };
        if vm.mem.heap.kind_of(layout) != cratonvm_types::ObjectKind::Object {
            continue;
        }
        let layout_class = vm.mem.heap.class_id_of(layout);
        let name_ok = {
            let cm = vm.classes.class_manager.read();
            cm.get_class(layout_class).and_then(|c| {
                (matches!(cm.get_loader_id(layout_class), Some(ClassLoaderId::Bootstrap))
                    && c.origin.has_real_bytes())
                .then(|| c.name.to_string())
            })
        };
        let Some(layout_name) = name_ok else {
            continue;
        };
        if fei::ffm_value_layout_carrier(&layout_name).is_none() {
            continue;
        }
        let (Value::Long(size), Value::Long(align), Value::Object(Some(order))) = (
            vm.mem.heap.get_field(layout, plan.byte_size),
            vm.mem.heap.get_field(layout, plan.byte_alignment),
            vm.mem.heap.get_field(layout, plan.order),
        ) else {
            continue;
        };
        let big_endian = if order == big {
            true
        } else if order == little {
            false
        } else {
            continue;
        };
        if let Some(facts) = fei::ffm_layout_facts_from_fields(&layout_name, size, align, big_endian)
        {
            filed.push(((class_id.as_u32(), index), facts.pack()));
        }
    }
    if filed.is_empty() {
        return;
    }
    if crate::runtime::env_cache::dbg_jitc() {
        for &((holder, index), word) in &filed {
            eprintln!(
                "[cratonvm-jitc] ffm layout facts filed class_id={holder} field={index} word={word:#x}"
            );
        }
    }
    vm.classes.ffm_layout_facts.write().extend(filed);
}

/// JIT r23 wave 4 (lane L8, L8-8 stages 2 and 4): the answer to one
/// `cratonvm_jit::ffm_element_inline` query word (`tag`, `class_id`, `index`
/// from `ffm_inline_query_decode`), `0` = unknown.
///
/// * Layout facts: the word [`note_class_initialized_ffm_layout_facts`] filed
///   for the `static final` `(class_id, index)`.
/// * Segment fields: only for a bootstrap class with real bytes whose name is
///   one of the JDK's segment leaves (`ffm_segment_kind_answer`); the slot is
///   resolved from the field's DECLARING class (`FFM_SEGMENT_FIELD_ROWS`), as
///   `byte_buffer_view_read_layout` does. Never waits on the class manager.
fn ffm_inline_well_known_answer(vm: &SharedVm, tag: i64, class_id: u32, index: u16) -> i64 {
    use cratonvm_jit::ffm_element_inline as fei;
    if tag == fei::WELL_KNOWN_FFM_LAYOUT_FACTS_TAG {
        // r23 w4 review: `try_read`, as the class-manager read below and
        // `vh_const_well_known_answer` -- a compile never waits on a filing;
        // contention answers 0 (no fact), which keeps the call.
        return vm
            .classes
            .ffm_layout_facts
            .try_read()
            .and_then(|facts| facts.get(&(class_id, u32::from(index))).copied())
            .unwrap_or(0);
    }
    let Some(cm) = vm.classes.class_manager.try_read_untracked() else {
        return 0;
    };
    let cid = ClassId::new(class_id);
    let Some(class) = cm.get_class(cid) else {
        return 0;
    };
    if !matches!(
        cm.get_loader_id(cid),
        Some(cratonvm_types::ClassLoaderId::Bootstrap)
    ) || !class.origin.has_real_bytes()
    {
        return 0;
    }
    let kind = fei::ffm_segment_kind_answer(&class.name);
    if index == fei::FFM_SEGMENT_SEL_KIND {
        return i64::from(kind);
    }
    if !fei::ffm_segment_selector_applies(kind, index) {
        return 0;
    }
    let Some(&(_, owner, name, desc)) = fei::FFM_SEGMENT_FIELD_ROWS
        .iter()
        .find(|row| row.0 == index)
    else {
        return 0;
    };
    let Some(owner) = cm.find_bootstrap_class_by_name(owner) else {
        return 0;
    };
    crate::vm::resolve_field_index_in_hierarchy_desc(owner, name, Some(desc), &cm.class_store)
        .and_then(|idx| i64::try_from(idx).ok())
        .and_then(|idx| idx.checked_add(1))
        .unwrap_or(0)
}

/// nio21 wave 11 L5 (W9L2-2, page
/// `r21-varhandle-constant-handle-field-access-inline-FIXED-20261012.md`): file the
/// constant-`VarHandle` plan of the `static final` field `(holder, index)`,
/// whose handle the natives described as `plan`, in this VM's
/// [`crate::vm::realms::jit_realm::VhConstPlans`] -- the table both tiers'
/// `VarHandle` prefixes ask through `jit_well_known_class_id`
/// (`cratonvm_jit::ir_lower::vh_const_plan`).
///
/// Unlike the `UnboxOp` fold above it, any coordinate class is filed: the
/// prefixes test the receiver's EXACT class and decline (to the call, never a
/// deopt) on a subclass instance, so a non-`final` class costs a subclass
/// receiver nothing but the call it already made. Kinds `I`, `J` and `L` only
/// (the prefixes emit 4- and 8-byte accesses and the bare 8-byte reference).
///
/// For a reference field the row also carries the value-admission facts
/// (`cratonvm_jit::ir_lower::VhConstRefRow`): `Object` (no stored value
/// judged), the declared class, and whether a call-site descriptor naming that
/// class proves a value is one -- only for a class that is not an interface
/// (the verifier does not type interface operands) and that the coordinate
/// class's loader defines, or a `java/` class (no other loader may define one).
fn vh_const_file_plan(
    vm: &SharedVm,
    holder: ClassId,
    index: u32,
    plan: &cratonvm_native_builtins::lang_invoke::VarHandleInstanceFieldPlan,
) {
    use cratonvm_jit::ir_lower::{VhConstPlan, VH_CONST_REF_NAME_PROVES, VH_CONST_REF_UNCHECKED};
    if !matches!(plan.value_desc, b'I' | b'J' | b'L') || plan.class_id == 0 {
        return;
    }
    let (Ok(slot), Ok(_)) = (u16::try_from(plan.field_index), u16::try_from(index)) else {
        return;
    };
    let word = VhConstPlan {
        receiver_class_id: plan.class_id,
        slot,
        kind: plan.value_desc,
        read_only: plan.read_only,
    }
    .to_word();
    let ref_facts = (plan.value_desc == b'L').then(|| {
        let mut flags = 0u32;
        let mut name: Option<Box<str>> = None;
        if plan.ref_value_unchecked {
            flags |= VH_CONST_REF_UNCHECKED;
        } else if plan.ref_type_class_id != 0 {
            let cm = vm.classes.class_manager.read();
            if let (Some(field_type), Some(coordinate)) = (
                cm.get_class(ClassId::new(plan.ref_type_class_id)),
                cm.get_class(ClassId::new(plan.class_id)),
            ) {
                if !field_type.is_interface()
                    && (field_type.loader_id == coordinate.loader_id
                        || field_type.name.starts_with("java/"))
                {
                    flags |= VH_CONST_REF_NAME_PROVES;
                    name = Some(Box::from(&*field_type.name));
                }
            }
        }
        (flags, plan.ref_type_class_id, name)
    });
    vm.jit
        .vh_const_plans
        .file((holder.as_u32(), index), word, ref_facts);
}

/// `CRATONVM_JIT_VH_CONST_REAL_HANDLES` (JIT round 20 wave 3, lane atomics;
/// **default ON**, `=0` files natives-minted handles only, as before).
fn vh_const_real_handles_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_VH_CONST_REAL_HANDLES")
}

/// JIT round 20 wave 3, lane atomics (page
/// `r21-unsafe-volatile-accessors-are-thin-calls-not-inline-and-getandadd-is-not-one-xadd-FIXED-20261012.md`):
/// the instance-field plan of a REAL `java/lang/invoke/VarHandle{Ints,Longs,References}$FieldInstance{ReadOnly,ReadWrite}`
/// -- the object the JDK's own `Lookup.findVarHandle` returns under
/// `--jdk-only` -- read from its own fields, for
/// [`note_class_initialized_varhandles`].
///
/// Such a handle has no natives side-table row until a `VarHandle` door first
/// adopts it (`lang_invoke::vh_adopt_real_field_handle`), which is always after
/// the holder's `<clinit>` filed (or, before this, failed to file) its
/// `static final`s. So under `--jdk-only` no constant-handle plan was ever
/// filed and neither tier's `vh-const-inline` prefix engaged
/// (`Nio21CasRace` printed no `vh-const-inline` line).
///
/// The plan is the row adoption would file: `fieldOffset` is the VM's slot
/// index (`Unsafe.objectFieldOffset` answers one), `receiverType` the
/// coordinate class, the field found at that slot up the superclass chain
/// gives the descriptor, the class name gives read-only. Conservative where
/// it cannot match adoption exactly: `int` / `long` / reference fields only;
/// an invoke-exact handle (`exact`) and a compatibility-stub hierarchy give
/// `None`; a reference field's declared type is resolved through the
/// coordinate class's loader (`0`, i.e. only `null` admitted inline, when not
/// loaded); array-typed fields admit only `null` inline. Every value the
/// prefixes cannot admit declines to the call, whose door then adopts the
/// handle and answers as before.
fn vh_const_real_handle_plan(
    vm: &SharedVm,
    handle: ObjectRef,
) -> Option<cratonvm_native_builtins::lang_invoke::VarHandleInstanceFieldPlan> {
    if !vh_const_real_handles_enabled() {
        return None;
    }
    let heap = &vm.mem.heap;
    let (handle, validated) = heap.load_and_forward_checked(handle);
    if !validated {
        return None;
    }
    let vh_class = heap.class_id_of(handle);
    let (read_only, family_desc) = {
        let cm = vm.classes.class_manager.read();
        let name = cm.get_class(vh_class)?.name.clone();
        let rest = name.strip_prefix("java/lang/invoke/VarHandle")?;
        let (family, shape) = rest.split_once('$')?;
        let family_desc = match family {
            "Ints" => b'I',
            "Longs" => b'J',
            "References" => b'L',
            _ => return None,
        };
        let read_only = match shape {
            "FieldInstanceReadOnly" => true,
            "FieldInstanceReadWrite" => false,
            _ => return None,
        };
        (read_only, family_desc)
    };
    let slot_of =
        |name: &str| crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, vh_class, name);
    // An invoke-exact handle has no plan (as `varhandle_instance_field_plan_uncached`).
    if !matches!(
        heap.get_field_as(handle, slot_of("exact")?, b'Z'),
        Value::Int(0)
    ) {
        return None;
    }
    let offset = match heap.get_field_as(handle, slot_of("fieldOffset")?, b'J') {
        Value::Long(v) if v >= 0 => usize::try_from(v).ok()?,
        _ => return None,
    };
    // JIT round 20 wave 8c (lane huntDisp): under
    // `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS` the handle's offset is `16 + slot * 8`
    // (`native-builtins` `unsafe_encode_field_offset`); the plan keys the slot.
    let offset = if vm.classes.unsafe_byte_field_offsets {
        if offset < 16 || (offset - 16) & 7 != 0 {
            return None;
        }
        (offset - 16) >> 3
    } else {
        offset
    };
    let Value::Object(Some(mirror)) = heap.get_field_as(handle, slot_of("receiverType")?, b'L')
    else {
        return None;
    };
    let coordinate = crate::vm::class_id_from_mirror(vm, mirror)?;
    // JIT round 20 review: a reference handle's own `fieldType` is the class
    // its access modes cast a stored value to (`fieldType.cast(value)`); a
    // by-name lookup through the coordinate class's loader can name another
    // loader's same-named class when the field is declared by a superclass
    // from a parent loader, and would then admit a foreign value inline.
    // Read before the class manager's lock is taken.
    let field_type_id = if family_desc == b'L' {
        match slot_of("fieldType").map(|s| heap.get_field_as(handle, s, b'L')) {
            Some(Value::Object(Some(m))) => crate::vm::class_id_from_mirror(vm, m),
            _ => None,
        }
    } else {
        None
    };
    let cm = vm.classes.class_manager.read();
    cm.get_class(coordinate)?;
    // The field at absolute instance slot `offset`, up the superclass chain
    // (the walk `resolve_field_descriptor_byte_cached` makes).
    let mut search = Some(coordinate);
    let mut descriptor: Option<std::sync::Arc<str>> = None;
    for _ in 0..64 {
        let cls = cm.get_class(search?)?;
        if cls.origin.is_compatibility_stub() {
            return None;
        }
        if offset >= cls.first_field_index {
            let local = offset - cls.first_field_index;
            descriptor = cls
                .fields
                .iter()
                .filter(|f| !f.is_static())
                .nth(local)
                .map(|f| f.descriptor.clone());
            break;
        }
        search = cls.superclass;
    }
    let descriptor = descriptor?;
    let (value_desc, ref_value_unchecked, ref_type_class_id) = match &*descriptor {
        "I" => (b'I', false, 0),
        "J" => (b'J', false, 0),
        "Ljava/lang/Object;" => (b'L', true, 0),
        d if d.starts_with('[') => (b'L', false, 0),
        d => {
            let name = d.strip_prefix('L')?.strip_suffix(';')?;
            // The handle's `fieldType`, when it is the class this descriptor
            // names; otherwise `0` (only `null` admitted inline).
            let id = field_type_id
                .filter(|&c| cm.get_class(c).is_some_and(|k| &*k.name == name))
                .map_or(0, |c| c.as_u32());
            (b'L', false, id)
        }
    };
    // The handle's family must agree with the slot's field type; a mismatch
    // means the slot arithmetic named another field: no plan.
    if value_desc != family_desc {
        return None;
    }
    Some(
        cratonvm_native_builtins::lang_invoke::VarHandleInstanceFieldPlan {
            field_index: u32::try_from(offset).ok()?,
            value_desc,
            class_id: coordinate.as_u32(),
            read_only,
            ref_value_unchecked,
            ref_array_element: None,
            ref_type_class_id,
            ref_array_component_class_id: 0,
        },
    )
}

/// JIT r20 wave 18, lane xVhArr (AT-2 of
/// `docs/feature-designs/jit-ir-optimizations.md`): the element
/// kind (`I` / `J`) of a REAL `java/lang/invoke/VarHandleInts$Array` /
/// `VarHandleLongs$Array` -- what the JDK's own
/// `MethodHandles.arrayElementVarHandle(int[].class / long[].class)` returns in
/// both `--jdk-only` and `--compatible` (no native replaces that factory
/// there; `lang_invoke.rs`, "NOT REGISTERED HERE") -- for
/// [`note_class_initialized_varhandles`]. `None` for every other handle, for
/// an invoke-exact one (`withInvokeExactBehavior()`: its `exact` field), and
/// under `CRATONVM_JIT_VH_CONST_ARRAY_ELEMENT=0`.
///
/// The class name is proof enough of the element type: both classes are
/// `final`, live in `java/lang/invoke` (no other loader may define one) and
/// each serves exactly one array type (`real_array_var_handle_descriptors`).
/// Reference-array handles (`VarHandleReferences$Array`) are not filed: their
/// stores need the barrier gates and the covariant store check.
fn vh_const_array_handle_kind(vm: &SharedVm, handle: ObjectRef) -> Option<u8> {
    if !cratonvm_jit::ir_lower::vh_const_array_element_enabled() {
        return None;
    }
    let heap = &vm.mem.heap;
    let (handle, validated) = heap.load_and_forward_checked(handle);
    if !validated {
        return None;
    }
    let vh_class = heap.class_id_of(handle);
    let kind = {
        let cm = vm.classes.class_manager.read();
        match &*cm.get_class(vh_class)?.name {
            "java/lang/invoke/VarHandleInts$Array" => b'I',
            "java/lang/invoke/VarHandleLongs$Array" => b'J',
            // JIT round 21 wave 1, lane updaters (UPDATERS21-5,
            // `CRATONVM_JIT_VH_CONST_REF_ARRAY_GET`, opt-in): checked below.
            // JIT round 21 wave 22, lane vhconst: or its store arm's switch
            // (`CRATONVM_JIT_VH_CONST_REF_ARRAY_STORE`, opt-in), on the same plan.
            "java/lang/invoke/VarHandleReferences$Array"
                if cratonvm_jit::ir_lower::vh_const_ref_array_get_enabled()
                    || cratonvm_jit::ir_lower::vh_const_ref_array_store_enabled() =>
            {
                b'L'
            }
            _ => return None,
        }
    };
    let exact = crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, vh_class, "exact")?;
    if !matches!(heap.get_field_as(handle, exact, b'Z'), Value::Int(0)) {
        return None;
    }
    // UPDATERS21-5: only a handle whose `arrayType` is exactly `Object[]`.
    // Its JDK body casts the coordinate to `arrayType`, which every reference
    // array passes, so the compiled screen (any reference array) answers the
    // same; a narrower `arrayType` (`String[]`) would need that cast inline
    // and is not filed.
    if kind == b'L' {
        let slot = crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, vh_class, "arrayType")?;
        let Value::Object(Some(mirror)) = heap.get_field_as(handle, slot, b'L') else {
            return None;
        };
        let array_class = crate::vm::class_id_from_mirror(vm, mirror)?;
        let cm = vm.classes.class_manager.read();
        if &*cm.get_class(array_class)?.name != "[Ljava/lang/Object;" {
            return None;
        }
    }
    Some(kind)
}

/// JIT r20 wave 18, lane xVhArr (AT-2): file the array-element plan of the
/// `static final` field `(holder, index)` (element `kind`) in this VM's
/// [`crate::vm::realms::jit_realm::VhConstPlans`], under the field arm's key:
/// a `static final` holds one handle, so one row per key.
fn vh_const_file_array_plan(vm: &SharedVm, holder: ClassId, index: u32, kind: u8) {
    let word = cratonvm_jit::ir_lower::vh_const_array_plan_word(kind);
    if word == 0 || u16::try_from(index).is_err() {
        return;
    }
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] vh-const array plan filed class_id={} field={index} kind={}",
            holder.as_u32(),
            char::from(kind),
        );
    }
    vm.jit
        .vh_const_plans
        .file((holder.as_u32(), index), word, None);
}

/// JIT round 21 wave 1 (lane indy, INDY21-2, `CRATONVM_JIT_IR_CLINIT_BARRIER`):
/// the `jit_well_known_class_id` arm of the optimizing tier's class-init
/// barrier (`ir_lower::Lowerer::emit_clinit_barrier_getstatic`): the
/// `JvmThread::thread_id` offset, the init-owner cell of a class that is not
/// initialized (`jit_bridge::clinit_owner_cell_for`), and that class's statics
/// base cell without `jit_resolve_static_base`'s initialized rule. `Some(0)`
/// for an initialized class or any refusal; `None` for every other `which`.
/// Never waits (the cell's class-manager read is a try-read).
fn clinit_barrier_well_known_answer(vm: &SharedVm, which: i64) -> Option<i64> {
    use cratonvm_jit::ir_lower as jl;
    if which == jl::WELL_KNOWN_JVM_THREAD_ID_OFFSET {
        const _: () =
            assert!(std::mem::size_of::<crate::threading::jvm_thread::ThreadId>() == 8);
        // Cast: a small struct offset as the C ABI's i64 answer, plus one.
        return Some(std::mem::offset_of!(JvmThread, thread_id) as i64 + 1);
    }
    let tag = which & !0xFFFF_FFFF_FFFF_i64;
    if tag != jl::WELL_KNOWN_CLINIT_OWNER_CELL_TAG && tag != jl::WELL_KNOWN_CLINIT_BASE_CELL_TAG {
        return None;
    }
    // Cast: the masked fields of a `vh_const_query` word fit their widths.
    let raw = ((which >> 16) & 0xFFFF_FFFF) as u32;
    let field_index = (which & 0xFFFF) as usize; // Cast: as above
    if raw == 0 || class_init_memo::is_initialized(vm, raw) {
        return Some(0);
    }
    let class_id = ClassId::new(raw);
    if tag == jl::WELL_KNOWN_CLINIT_OWNER_CELL_TAG {
        // Cast: a VM-lived cell address as the C ABI's i64 answer.
        return Some(
            crate::runtime::interpreter::jit_bridge::clinit_owner_cell_for(vm, class_id) as i64,
        );
    }
    // The statics base cell, refused where `jit_resolve_static_base` refuses
    // for a reason other than initialization.
    if vm
        .classes
        .system_class_id
        .load(std::sync::atomic::Ordering::Relaxed)
        == raw
        || vm
            .classes
            .boolean_class_id
            .load(std::sync::atomic::Ordering::Relaxed)
            == raw
        || crate::vm::statics_index_disabled()
    {
        return Some(0);
    }
    Some(
        vm.classes
            .statics_index
            .base_cell_addr(class_id, field_index)
            // Cast: a user-space address fits the positive i64 range.
            .map_or(0, |addr| addr as i64),
    )
}

/// nio21 wave 11 L5: the `jit_well_known_class_id` arm of the two
/// constant-`VarHandle` queries (`cratonvm_jit::ir_lower::vh_const_query`):
/// the filed plan word, or the address of the filed reference row, `0` when
/// nothing is filed (or the table is momentarily write-locked: the site then
/// keeps its call). `None` for every other `which`.
fn vh_const_well_known_answer(vm: &SharedVm, which: i64) -> Option<i64> {
    use cratonvm_jit::ir_lower as jl;
    let (tag, class_id, field_index) = jl::vh_const_query_decode(which)?;
    let key = (class_id, field_index);
    Some(if tag == jl::WELL_KNOWN_VH_CONST_PLAN_TAG {
        vm.jit.vh_const_plans.plan_word(key)
    } else if tag == jl::vh_static_const::WELL_KNOWN_VH_CONST_STATIC_CELL_TAG {
        // j27 wave 2 (lane VHCOMP): the static constant-handle cell's address.
        // Cast: a VM-lived cell address handed across the C ABI as i64.
        vm.jit.vh_static_cells.cell_addr(key) as i64
    } else if tag == jl::WELL_KNOWN_VH_CONST_REF_ROW_TAG {
        vm.jit.vh_const_plans.ref_row_addr(key)
    } else if tag == jl::WELL_KNOWN_VH_CONST_DISPLAY_CELL_TAG {
        // Cast: a heap address handed across the C ABI as i64.
        vm.jit.supers_displays.cell_addr() as i64
    } else if tag == jl::WELL_KNOWN_VH_CONST_TYPECHECK_ROWS_TAG {
        // JIT round 21, lane dispatch (`CRATONVM_JIT_TYPECHECK_ROWS`): the
        // type-check rows, allocated (and backfilled) on the first ask.
        // Cast: a heap address handed across the C ABI as i64.
        vm.jit.supers_displays.typecheck_rows_addr() as i64
    } else if tag == jl::WELL_KNOWN_VH_CONST_DEPTH_TAG {
        // nio21 wave 14 L4: a class asked about before its `<clinit>`
        // completed (its display is published at the end of it) gets one
        // now, if the class manager is free.
        if vm.jit.supers_displays.enabled() && vm.jit.supers_displays.depth_of(class_id).is_none() {
            vh_const_publish_display(vm, ClassId::new(class_id), false);
        }
        vm.jit
            .supers_displays
            .depth_of(class_id)
            .map_or(0, |d| i64::from(d) + 1)
    } else if tag == jl::WELL_KNOWN_VH_CONST_IFACE_TAG {
        // nio21 wave 14 L4: `1` when `class_id` is an interface, which the
        // value screen then answers from the displays' interface sets.
        let is_interface = vm.jit.supers_displays.enabled()
            && vm
                .classes
                .class_manager
                .try_read_untracked()
                .and_then(|cm| {
                    cm.get_class(ClassId::new(class_id))
                        .map(|c| c.is_interface())
                })
                .unwrap_or(false);
        i64::from(is_interface)
    } else if tag == jl::WELL_KNOWN_FFM_CONST_ROW_TAG {
        // JIT round 22 (lane memory, FFM22-1): the compiled-call row of the
        // bound FFM downcall handle a `static final` field holds.
        crate::runtime::interpreter::jit_bridge::ffm_const_row_addr(vm, class_id, field_index)
    } else if tag == jl::WELL_KNOWN_TYPECHECK_MISS_MEMO_TAG {
        // JIT r25 wave 8 (W8-2): this VM's `instanceof` memo of `class_id`.
        // Cast: a heap address handed across the C ABI as i64.
        vm.jit.supers_displays.miss_memo_addr(class_id) as i64
    } else if tag == jl::WELL_KNOWN_TYPECHECK_ARRAY_COMPONENT_TAG {
        // JIT r21 wave 1 (lane typecheck, TYPECHECK21-3): `1 + id` of the
        // immediate component of the array class `class_id` when that
        // component is a class or interface (`ArrayInfo::component_class_id`,
        // the array's identity witness), `0` for anything else -- a non-array,
        // a primitive-component array, a nested array, or a held lock.
        vm.classes
            .class_manager
            .try_read_untracked()
            .and_then(|cm| {
                let array = cm.get_class(ClassId::new(class_id))?;
                if !array.name.starts_with("[L") {
                    return None;
                }
                let component = array.array_info.as_ref()?.component_class_id;
                let component_class = cm.get_class(component)?;
                (!component_class.name.starts_with('[')).then_some(component)
            })
            .map_or(0, |c| i64::from(c.as_u32()) + 1)
    } else if tag == jl::WELL_KNOWN_VH_CONST_LOADER_TAG {
        // JIT round 21, lane vhcae (`CRATONVM_JIT_VH_CONST_RET_SAME_LOADER`):
        // `1 +` the class's defining loader's native id, `0` when unknown.
        vm.classes
            .class_manager
            .try_read_untracked()
            .and_then(|cm| {
                cm.get_class(ClassId::new(class_id))
                    .map(|c| i64::from(c.loader_id.to_native_id()) + 1)
            })
            .unwrap_or(0)
    } else if tag == jl::WELL_KNOWN_VH_CONST_ARRAY_COMPONENT_TAG {
        // JIT round 21, lane tcarray (`CRATONVM_JIT_TYPECHECK_ARRAY_ARM`):
        // `1 +` the component of a reference-array class, the id the run-time
        // verdict compares against (`typecheck::array_cast_resolved_verdict`
        // reads the same `array_info`), `0` otherwise.
        vm.classes
            .class_manager
            .try_read_untracked()
            .and_then(|cm| {
                let class = cm.get_class(ClassId::new(class_id))?;
                if !class.name.starts_with('[') {
                    return None;
                }
                let component = class.array_info.as_ref()?.component_class_id;
                cm.get_class(component)?;
                Some(i64::from(component.as_u32()) + 1)
            })
            .unwrap_or(0)
    } else if tag == jl::WELL_KNOWN_VH_CONST_IDENTITY_HASH_TAG {
        // JIT round 21, lane hash (NATIVE21-1): the profiled class of the
        // `Object.hashCode()` identity arm.
        let verdict = class_hash_code_identity_verdict(vm, ClassId::new(class_id));
        // JIT round 21, lane hash2: the answer is the class table's verdict too.
        if let Some(proven) = verdict {
            record_identity_hash_class(vm, class_id, proven);
        }
        // JIT round 21, lane hashlock (HASH2-21-1): a class whose hashes are
        // answered inline from here on no longer reaches the leaf that files
        // its equals verdict, so file that one now as well.
        open_identity_equals_classes(vm);
        note_identity_equals_class(vm, class_id);
        i64::from(verdict.unwrap_or(false))
    } else if tag == jl::WELL_KNOWN_VH_CONST_IDENTITY_HASH_TABLE_TAG {
        // JIT round 21, lane hash2 (`CRATONVM_JIT_OBJECT_HASH_CLASS_TABLE`):
        // the identity arm's per-class verdict table, allocated on the first
        // ask.
        // JIT round 21, lane hashlock (HASH2-21-1): the equals verdict table
        // opens with it, so every class the hash leaf files from now on gets
        // its equals verdict filed too (its later hashes may be inline).
        open_identity_equals_classes(vm);
        // Cast: a heap address handed across the C ABI as i64.
        vm.jit.identity_hash_classes.base_addr() as i64
    } else if tag == jl::WELL_KNOWN_VH_CONST_IDENTITY_EQUALS_TAG {
        // JIT round 21, lane hashlock (HASH2-21-1): the profiled class of the
        // `Object.equals(Object)` identity arm; the answer is filed too.
        let verdict = class_equals_identity_verdict(vm, ClassId::new(class_id));
        if let Some(proven) = verdict {
            record_identity_equals_class(vm, class_id, proven);
        }
        i64::from(verdict.unwrap_or(false))
    } else if tag == jl::WELL_KNOWN_VH_CONST_IDENTITY_EQUALS_TABLE_TAG {
        // JIT round 21, lane hashlock (HASH2-21-1,
        // `CRATONVM_JIT_OBJECT_EQUALS_IDENTITY_ARM`): the equals arm's
        // per-class verdict table, allocated on the first ask. Cast: a heap
        // address handed across the C ABI as i64.
        vm.jit.identity_equals_classes.base_addr() as i64
    } else if tag == jl::WELL_KNOWN_VH_CONST_G1_EDGE_FILTER_TAG {
        // JIT round 21, lane vhcae (VHCAE21-1, `CRATONVM_JIT_G1_EDGE_FILTER`):
        // this VM's G1 collector's compiled-barrier edge filter, `0` under any
        // other collector. Cast: a heap address handed across the C ABI as i64.
        match &vm.mem.heap {
            cratonvm_gc::vm_heap::VmHeap::G1(g1) => g1.jit_edge_filter_addr() as i64,
            _ => 0,
        }
    } else {
        0
    })
}

/// JIT round 21, lane hash (NATIVE21-1; page
/// `docs/known-issues/jit/jr21-hash-object-hashcode-identity-arm-20261014.md`):
/// is `class_id`'s virtual `hashCode()I` selection provably
/// `java/lang/Object.hashCode`, so a compiled `Object.hashCode()` site may
/// answer an exact instance of it from the header
/// (`cratonvm_jit::ir_lower::identity_hash_profiled_class`)?
///
/// `true` only for a concrete (not interface, not abstract), non-array,
/// non-lambda-proxy class whose whole superclass chain up to
/// `java/lang/Object` has real class bytes and a fixed hierarchy
/// ([`typecheck_miss_is_hierarchy_only`]: no stub, no VM-internal or
/// re-parentable class, no proxy / annotation proxy / `Unmodifiable*` stamp),
/// and in which no class DECLARES `hashCode()I` (an override, or an abstract
/// re-declaration) or has a native REGISTERED under that triple. That is the
/// JVMS §5.4.6 selection: nothing below `Object` declares the method, so
/// `Object.hashCode` is selected, and the receiver dispatches on its own class
/// (`virtual_dispatch_target_for_receiver`'s plain arm). The three reflection
/// type stubs whose `Object.hashCode` is structural are interfaces, refused by
/// the first test. A class manager the compile thread cannot read without
/// waiting answers `false`. The answer is a property of the class: class ids
/// are never reused within a VM and a redefinition may not add a method.
///
/// JIT round 21, lane hash2: `None` (was `false`) when the class manager's
/// read lock could not be taken without waiting or a class on the chain is
/// not in the store, so a caller that RECORDS the verdict
/// ([`record_identity_hash_class`]) asks again later instead of filing a
/// transient refusal for good; `Some(answer)` otherwise.
fn class_hash_code_identity_verdict(vm: &SharedVm, class_id: ClassId) -> Option<bool> {
    if class_id.as_u32() == 0 || class_id.as_u32() >= 0x8000_0000 {
        return Some(false);
    }
    let cm = vm.classes.class_manager.try_read_untracked()?;
    let registry = &vm.natives.native_methods;
    let mut cur = Some(class_id);
    let mut depth = 0usize;
    while let Some(cid) = cur {
        // A class the store does not hold (yet): no verdict to file.
        let class = cm.get_class(cid)?;
        let name: &str = &class.name;
        if name == "java/lang/Object" {
            return Some(depth > 0);
        }
        if depth > 4096
            || class.is_interface()
            || (depth == 0 && class.is_abstract())
            || name.starts_with('[')
            || !typecheck_miss_is_hierarchy_only(class)
            || class.find_method("hashCode", "()I").is_some()
            || registry.find(name, "hashCode", "()I").is_some()
        {
            return Some(false);
        }
        cur = class.superclass;
        depth += 1;
    }
    Some(false)
}

/// JIT round 21, lane hash2: file `class_id`'s verdict in this VM's
/// identity-hash class table (`JitRealm::identity_hash_classes`), once.
fn record_identity_hash_class(vm: &SharedVm, class_id: u32, proven: bool) {
    use cratonvm_jit::ir_lower as jl;
    vm.jit.identity_hash_classes.record(
        class_id,
        if proven {
            jl::IDENTITY_HASH_CLASS_PROVEN
        } else {
            jl::IDENTITY_HASH_CLASS_REFUSED
        },
    );
}

/// JIT round 21, lane hash2 (page
/// `docs/known-issues/jit/jr21-hash2-object-hashcode-class-table-20261014.md`):
/// an identity-hash leaf (`Object.hashCode` / `System.identityHashCode`,
/// `NativeContext::identity_hash_code_installed`) is serving `obj`; once the
/// compiled identity arm's class table exists, file the verdict of `obj`'s
/// class there if it is not decided yet, so the next compiled
/// `Object.hashCode()` on an instance of it answers from the header. A plain
/// object only (an array header carries its component's id). Costs a header
/// read and one byte load once the class is decided; nothing before a compile
/// asked for the table, or under `CRATONVM_JIT_OBJECT_HASH_CLASS_TABLE=0`
/// (no compile asks).
pub(crate) fn note_identity_hash_receiver(vm: &SharedVm, obj: ObjectRef) {
    let table = &vm.jit.identity_hash_classes;
    // JIT round 21, lane hashlock (HASH2-21-1): the equals verdict table is
    // filed from the same leaf -- a `HashMap` key is hashed before it is
    // compared, so this is where its class is first seen.
    let equals = &vm.jit.identity_equals_classes;
    let (hash_open, equals_open) = (table.is_allocated(), equals.is_allocated());
    if !hash_open && !equals_open {
        return;
    }
    let heap = &vm.mem.heap;
    if heap.kind_of(obj) != cratonvm_types::ObjectKind::Object {
        return;
    }
    let class_id = heap.class_id_of(obj);
    let raw = class_id.as_u32();
    let unknown = cratonvm_jit::ir_lower::IDENTITY_HASH_CLASS_UNKNOWN;
    let hash_todo = hash_open && table.verdict(raw) == unknown;
    let equals_todo = equals_open && equals.verdict(raw) == unknown;
    if raw == 0 || !(hash_todo || equals_todo) {
        return;
    }
    if hash_todo {
        if let Some(proven) = class_hash_code_identity_verdict(vm, class_id) {
            record_identity_hash_class(vm, raw, proven);
        }
    }
    if equals_todo {
        if let Some(proven) = class_equals_identity_verdict(vm, class_id) {
            record_identity_equals_class(vm, raw, proven);
        }
    }
}

/// JIT round 21, lane hashlock (HASH2-21-1): allocate this VM's equals
/// verdict table now when the compiled equals identity arm is on
/// (`CRATONVM_JIT_OBJECT_EQUALS_IDENTITY_ARM`), so the classes the identity-hash
/// paths file from here on get their equals verdict filed in the same step.
/// Called when a compile opens the identity-hash class table or asks a
/// profiled class's hash verdict: after that, the class's hashes may be
/// answered inline and never reach [`note_identity_hash_receiver`] again.
fn open_identity_equals_classes(vm: &SharedVm) {
    if cratonvm_jit::ir_lower::object_equals_identity_arm_enabled() {
        let _ = vm.jit.identity_equals_classes.base_addr();
    }
}

/// JIT round 21, lane hashlock (HASH2-21-1): file `class_id`'s equals verdict
/// if the table exists and the class is not decided yet.
fn note_identity_equals_class(vm: &SharedVm, class_id: u32) {
    let table = &vm.jit.identity_equals_classes;
    if class_id == 0
        || !table.is_allocated()
        || table.verdict(class_id) != cratonvm_jit::ir_lower::IDENTITY_HASH_CLASS_UNKNOWN
    {
        return;
    }
    if let Some(proven) = class_equals_identity_verdict(vm, ClassId::new(class_id)) {
        record_identity_equals_class(vm, class_id, proven);
    }
}

/// JIT round 21, lane hashlock (HASH2-21-1): file `class_id`'s equals verdict
/// in this VM's table (`JitRealm::identity_equals_classes`), once.
fn record_identity_equals_class(vm: &SharedVm, class_id: u32, proven: bool) {
    use cratonvm_jit::ir_lower as jl;
    vm.jit.identity_equals_classes.record(
        class_id,
        if proven {
            jl::IDENTITY_HASH_CLASS_PROVEN
        } else {
            jl::IDENTITY_HASH_CLASS_REFUSED
        },
    );
}

/// JIT round 21, lane hashlock (HASH2-21-1; page
/// `docs/known-issues/jit/jr21-native-identity-hash-crossings-pay-the-full-funnel-20261014.md`):
/// is `class_id`'s virtual `equals(Ljava/lang/Object;)Z` selection provably
/// `java/lang/Object.equals`, so a compiled `Object.equals(Object)` site may
/// answer an exact instance of it as `this == arg`?
///
/// The rule of [`class_hash_code_identity_verdict`] with the `equals` triple:
/// a concrete, non-array, non-lambda class whose whole superclass chain up to
/// `java/lang/Object` has real bytes and a fixed hierarchy
/// ([`typecheck_miss_is_hierarchy_only`]), none of which DECLARES
/// `equals(Ljava/lang/Object;)Z` or has a native REGISTERED under that triple.
/// JVMS 5.4.6 then selects `Object.equals` (a class method is found on the
/// superclass chain before any interface is searched), whose bytecode is
/// `this == obj` and whose registered natives answer the same for every such
/// receiver (their structural arm is for the reflection type stubs, which are
/// interfaces and refused here). One more, conservative screen: every
/// transitive superinterface must be readable and hierarchy-only, carry no
/// native registered under the triple, and declare no NON-abstract `equals`
/// (no Java source compiles one, but a class file could), so this VM's
/// by-name dispatch cannot pick an interface's answer either.
///
/// `None` when the class manager's read lock could not be taken without
/// waiting or a class in the closure is not in the store (asked again later);
/// `Some(answer)` otherwise. The answer is a property of the class: class ids
/// are never reused within a VM and a redefinition may not add a method.
fn class_equals_identity_verdict(vm: &SharedVm, class_id: ClassId) -> Option<bool> {
    const NAME: &str = "equals";
    const DESC: &str = "(Ljava/lang/Object;)Z";
    if class_id.as_u32() == 0 || class_id.as_u32() >= 0x8000_0000 {
        return Some(false);
    }
    let cm = vm.classes.class_manager.try_read_untracked()?;
    let registry = &vm.natives.native_methods;
    let mut pending: Vec<ClassId> = Vec::new();
    let mut cur = Some(class_id);
    let mut depth = 0usize;
    let mut reached_object = false;
    while let Some(cid) = cur {
        // A class the store does not hold (yet): no verdict to file.
        let class = cm.get_class(cid)?;
        let name: &str = &class.name;
        if name == "java/lang/Object" {
            reached_object = depth > 0;
            break;
        }
        if depth > 4096
            || class.is_interface()
            || (depth == 0 && class.is_abstract())
            || name.starts_with('[')
            || !typecheck_miss_is_hierarchy_only(class)
            || class.find_method(NAME, DESC).is_some()
            || registry.find(name, NAME, DESC).is_some()
        {
            return Some(false);
        }
        pending.extend(class.interfaces.iter().copied());
        cur = class.superclass;
        depth += 1;
    }
    if !reached_object {
        return Some(false);
    }
    let mut seen: Vec<u32> = Vec::new();
    while let Some(iid) = pending.pop() {
        if seen.contains(&iid.as_u32()) {
            continue;
        }
        if seen.len() > 4096 {
            return Some(false);
        }
        let iface = cm.get_class(iid)?;
        let name: &str = &iface.name;
        if !typecheck_miss_is_hierarchy_only(iface)
            || registry.find(name, NAME, DESC).is_some()
            || iface
                .find_method(NAME, DESC)
                .is_some_and(|m| !m.is_abstract())
        {
            return Some(false);
        }
        seen.push(iid.as_u32());
        pending.extend(iface.interfaces.iter().copied());
    }
    Some(true)
}

/// nio21 wave 12 L2 (`r21-varhandle-inline-declines-subclass-receivers-and-subtype-values-FIXED-20261012.md`):
/// publish `class_id`'s superclass display
/// ([`crate::vm::realms::jit_realm::SupersDisplays`]) when its `<clinit>`
/// completes. Interfaces get none (an object's class is never one, and the
/// verifier does not type interface operands, so no screen asks). The chain
/// is read under the class manager's read lock; a class whose chain cannot be
/// read whole is skipped (the prefixes then decline its instances).
///
/// nio21 wave 14 L4: the display also carries the class's transitive
/// interface set (its own, its superclasses', and every superinterface of
/// those), the secondary-supers half the prefixes' interface-typed value
/// screen reads. `blocking == false` (the compile thread) only TRIES the class
/// manager's read lock and publishes nothing when it is held.
fn vh_const_publish_display(vm: &SharedVm, class_id: ClassId, blocking: bool) {
    let facts = if blocking {
        let cm = vm.classes.class_manager.read();
        vh_const_display_facts(&cm, class_id)
    } else {
        let Some(cm) = vm.classes.class_manager.try_read_untracked() else {
            return;
        };
        vh_const_display_facts(&cm, class_id)
    };
    if let Some((chain, interfaces, hierarchy_only)) = facts {
        vm.jit.supers_displays.publish_with_verdict(
            class_id.as_u32(),
            &chain,
            &interfaces,
            hierarchy_only,
        );
    }
}

/// JIT r20 wave 17 (lane pAlloc2): may a compiled `instanceof` answer a
/// display MISS for an instance whose class chain or interface set holds
/// `class` -- i.e. is `class` neither an instance-admitted shape nor one
/// whose edges the VM may still rewrite? A class with real bytes behind it
/// (its edges are its class file's, fixed once defined; a generated proxy
/// is excluded by origin and by name), and none of the receivers
/// `jit_typecheck_resolve` admits from the INSTANCE
/// (`receiver_class_determines_cast_verdict`'s list: a
/// `java/lang/reflect/Proxy` subclass or shim, an `AnnotationProxy`, an
/// `Unmodifiable*` stamp). A VM-spun lambda class keeps marker interfaces
/// in side tables, and a stub or VM-internal class can be re-parented
/// (`ClassStore::set_superclass`), so both decline.
pub(crate) fn typecheck_miss_is_hierarchy_only(class: &crate::classloading::Class) -> bool {
    use cratonvm_classloading::ClassOrigin;
    let name: &str = &class.name;
    matches!(
        class.origin,
        ClassOrigin::BootImage { .. }
            | ClassOrigin::ApplicationClassPath { .. }
            | ClassOrigin::UserDefined { .. }
            | ClassOrigin::HiddenClass { .. }
            | ClassOrigin::ReflectionAccessor { .. }
    ) && name != "java/lang/reflect/Proxy"
        && name != "java/lang/reflect/Proxy$Instance"
        && name != "java/lang/annotation/AnnotationProxy"
        && !name.starts_with("cratonvm/internal/Unmodifiable")
}

/// `(superclass chain root first, transitive interface set, hierarchy only)`
/// of the CLASS `class_id`, or `None` for an interface, an unknown class or a
/// chain that cannot be read whole. The flag is the display's verdict word
/// (`SupersDisplays::publish_with_verdict`): every class on the chain and
/// every interface in the set passes [`typecheck_miss_is_hierarchy_only`].
/// When this answers `Some`, its flag equals
/// [`supertype_closure_is_hierarchy_only`] for the same class (the same
/// predicate over the same distinct set), which the interpreter's by-name
/// fallback reads.
pub(crate) fn vh_const_display_facts(
    cm: &crate::classloading::ClassManager,
    class_id: ClassId,
) -> Option<(Vec<u32>, Vec<u32>, bool)> {
    let mut chain: Vec<u32> = Vec::new();
    let mut pending: Vec<ClassId> = Vec::new();
    let mut hierarchy_only = class_id.as_u32() < 0x8000_0000;
    let mut cur = Some(class_id);
    while let Some(cid) = cur {
        let class = cm.get_class(cid)?;
        if class.is_interface() || chain.len() > 4096 {
            return None;
        }
        hierarchy_only &= typecheck_miss_is_hierarchy_only(class);
        chain.push(cid.as_u32());
        pending.extend(class.interfaces.iter().copied());
        cur = class.superclass;
    }
    chain.reverse();
    // Every superinterface, each once. An interface that cannot be read makes
    // the set incomplete, and an incomplete set could only DECLINE a value it
    // should admit -- but a missing class here is a broken hierarchy, so the
    // display is withheld instead.
    let mut interfaces: Vec<u32> = Vec::new();
    while let Some(iid) = pending.pop() {
        if interfaces.contains(&iid.as_u32()) {
            continue;
        }
        if interfaces.len() > 4096 {
            return None;
        }
        let iface = cm.get_class(iid)?;
        hierarchy_only &= typecheck_miss_is_hierarchy_only(iface);
        interfaces.push(iid.as_u32());
        pending.extend(iface.interfaces.iter().copied());
    }
    Some((chain, interfaces, hierarchy_only))
}

/// JIT round 21 (lane tcbudget; correctness, unconditional; page
/// `docs/known-issues/jit/jr21-revjit2-synthetic-implements-closure-budget-diverges-from-the-display-verdict-20261014.md`):
/// THE rule for "is `class_id`'s whole supertype closure hierarchy-only" --
/// every DISTINCT class on its superclass chain and every distinct transitive
/// superinterface passes [`typecheck_miss_is_hierarchy_only`]. The supers
/// display's verdict word ([`vh_const_display_facts`]) is this predicate over
/// this set, and the interpreter's `synthetic_implements` (the interpreter,
/// the `jit_instanceof` / `checkcast` helpers and every compiled call into
/// them) asks it before applying any by-name rule, so a compiled display miss
/// and an interpreted `instanceof` give one answer.
///
/// A visited set, not a tree walk: a diamond-heavy closure (20 interfaces in a
/// two-wide lattice is 2^10 paths) is walked once per distinct class. There is
/// no budget -- the walk is bounded by the distinct closure, which is finite
/// and acyclic for a loaded class, and the visited set ends it even if it were
/// not -- so the answer is always the real hierarchy's, never a give-up that
/// lets a by-name rule over-admit. (The display caps its sets at 4096 and then
/// publishes NO display, so a compiled site sends such a receiver to the
/// helper, i.e. here.) Interfaces' `superclass` is not followed, exactly as
/// in the display. `false` for an id past the class band, or a class in the
/// closure the manager cannot read.
pub(crate) fn supertype_closure_is_hierarchy_only(
    cm: &crate::classloading::ClassManager,
    class_id: ClassId,
) -> bool {
    if class_id.as_u32() >= 0x8000_0000 {
        return false;
    }
    // Small closures (nearly all) stay inline with a linear membership test;
    // past 64 distinct classes the set moves to a hash set.
    let mut seen: smallvec::SmallVec<[u32; 64]> = smallvec::SmallVec::new();
    let mut seen_big: Option<rustc_hash::FxHashSet<u32>> = None;
    let mut pending: smallvec::SmallVec<[ClassId; 32]> = smallvec::SmallVec::new();
    pending.push(class_id);
    while let Some(id) = pending.pop() {
        let raw = id.as_u32();
        let fresh = match seen_big.as_mut() {
            Some(set) => set.insert(raw),
            None if seen.contains(&raw) => false,
            None if seen.len() < 64 => {
                seen.push(raw);
                true
            }
            None => {
                let mut set: rustc_hash::FxHashSet<u32> = seen.iter().copied().collect();
                set.insert(raw);
                seen_big = Some(set);
                true
            }
        };
        if !fresh {
            continue;
        }
        let Some(class) = cm.get_class(id) else {
            return false;
        };
        if !typecheck_miss_is_hierarchy_only(class) {
            return false;
        }
        pending.extend(class.interfaces.iter().copied());
        if !class.is_interface() {
            if let Some(sup) = class.superclass {
                pending.push(sup);
            }
        }
    }
    true
}

/// nio21 wave 14 L4: publish `class_id`'s display now if it has none --
/// the lazy half of "publish before the first instance is used". Called from
/// the thin `VarHandle` helpers' receiver / value admission, i.e. on the path
/// a prefix DECLINED to, so a class whose instances are used before its
/// `<clinit>` completes (an instance its own `<clinit>` creates and uses) is
/// served from the next execution on. Lock-free when the display exists.
fn vh_const_ensure_display(vm: &SharedVm, class_id: u32) {
    if class_id != 0
        && vm.jit.supers_displays.enabled()
        && !vm.jit.supers_displays.is_published(class_id)
    {
        vh_const_publish_display(vm, ClassId::new(class_id), true);
    }
}

/// `CRATONVM_JIT_VH_CONST_DISPLAY_AT_INIT_START=0` (JIT round 23 wave 1, lane
/// L9; W14L4-1 of `docs/feature-designs/jit-ir-optimizations.md`):
/// no supers display published when a class's `<clinit>` STARTS
/// ([`vh_const_publish_display_at_init_start`]); the display then comes, as
/// before, when `<clinit>` completes or from the lazy publishers. Default ON
/// (a perf lever with an off-switch). Read once per class initialisation,
/// which is not a hot path.
fn vh_const_display_at_init_start_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_VH_CONST_DISPLAY_AT_INIT_START")
}

/// JIT round 23 wave 1, lane L9 (W14L4-1): publish `class_id`'s supers display
/// at the `Initializing` transition of its init claim, i.e. after its
/// superclasses and superinterfaces are initialized and before its
/// `<clinit>` runs -- before any instance of it can exist. An instance its
/// own `<clinit>` creates (enum constants, singletons, a `static final`
/// `CompletableFuture` subclass) then meets a published display at its first
/// constant-`VarHandle` access or display-backed type check, instead of a
/// decline until `<clinit>` completes ([`note_class_initialized_varhandles`])
/// or a lazy publisher runs.
///
/// Conservative where the completion publish is not: only a display whose
/// verdict is HIERARCHY-ONLY (every class on the chain and every interface
/// in the set has real bytes and fixed edges, [`typecheck_miss_is_hierarchy_only`])
/// is published this early, so a class whose edges the VM may still rewrite
/// (a stub, a VM-internal or spun class) keeps its old timing. Never under a
/// held class-manager lock: the caller has dropped its write guard.
pub(crate) fn vh_const_publish_display_at_init_start(vm: &SharedVm, class_id: ClassId) {
    if !vm.jit.supers_displays.enabled()
        || vm.jit.supers_displays.is_published(class_id.as_u32())
        || !vh_const_display_at_init_start_enabled()
    {
        return;
    }
    let facts = {
        let cm = vm.classes.class_manager.read();
        vh_const_display_facts(&cm, class_id)
    };
    if let Some((chain, interfaces, true)) = facts {
        vm.jit
            .supers_displays
            .publish_with_verdict(class_id.as_u32(), &chain, &interfaces, true);
    }
}

#[cfg(test)]
mod r23_l9_display_at_init_start_tests {
    use super::*;
    use crate::config::VmConfig;

    /// W14L4-1: a class with real-bytes edges gets its display at the start of
    /// its initialisation; a `--compatible` stub (re-parentable) does not, and
    /// neither does anything under `=0`.
    #[test]
    fn a_hierarchy_only_class_is_published_at_init_start_and_a_stub_is_not() {
        let shared = SharedVm::new(VmConfig::default());
        if !shared.jit.supers_displays.enabled() {
            return;
        }
        let ids = {
            let mut cm = shared.classes.class_manager.write();
            let (Ok(real), Ok(stub), Ok(off)) = (
                cm.try_ensure_synthetic_class("cratonvm/test/r23l9/DisplayReal", 0),
                cm.try_ensure_synthetic_class("cratonvm/test/r23l9/DisplayStub", 0),
                cm.try_ensure_synthetic_class("cratonvm/test/r23l9/DisplayOff", 0),
            ) else {
                return; // --jdk-only policy in the environment
            };
            for id in [real, stub, off] {
                if cm.class_store.get(id).is_none() {
                    return;
                }
                cm.set_superclass(id, None);
                cm.set_interfaces(id, Vec::new());
            }
            for id in [real, off] {
                if let Some(class) = cm.class_store.get_mut(id) {
                    class.origin = cratonvm_classloading::ClassOrigin::ApplicationClassPath {
                        source: std::sync::Arc::from("r23l9-test"),
                    };
                }
            }
            if let Some(class) = cm.class_store.get_mut(stub) {
                class.origin = cratonvm_classloading::ClassOrigin::CompatibilityStub {
                    reason: std::sync::Arc::from("r23l9-test"),
                };
            }
            (real, stub, off)
        };
        let (real, stub, off) = ids;
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_VH_CONST_DISPLAY_AT_INIT_START", None)],
            || {
                vh_const_publish_display_at_init_start(&shared, real);
                vh_const_publish_display_at_init_start(&shared, stub);
            },
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_VH_CONST_DISPLAY_AT_INIT_START", Some("0"))],
            || vh_const_publish_display_at_init_start(&shared, off),
        );
        let d = &shared.jit.supers_displays;
        assert!(d.is_published(real.as_u32()), "a real-bytes class: published");
        assert_eq!(d.depth_of(real.as_u32()), Some(0));
        assert!(!d.is_published(stub.as_u32()), "a stub keeps the old timing");
        assert!(!d.is_published(off.as_u32()), "=0: nothing published");
    }
}

/// Compile-time resolver: where does this static field's storage live?
///
/// Returns the address of the `AtomicPtr` cell holding the declaring class's
/// statics base (see `StaticsIndex::base_cell_addr`), or `0` for "not
/// inlineable — keep the helper". The JIT bakes the returned address as an
/// immediate and emits two dependent loads instead of a `CALL jit_getstatic`.
///
/// # Why every rejection below is required
///
/// * **`java/lang/System`** — its `out`/`err`/`in` statics are serviced by the
///   bootstrap intercept inside [`jit_getstatic`], which returns a synthetic
///   stream rather than the stored value. A direct load would read the raw
///   slot and `println` would silently no-op on a null stream.
/// * **Not yet initialized** — an inline load runs no `<clinit>` (JVMS §5.5).
///   The helper's init check is the only thing standing between a compiled
///   first-touch `getstatic` and the zero-initialized placeholder; a site is
///   therefore only inlined when the class is ALREADY initialized at compile
///   time, which is also exactly when HotSpot omits its init barrier.
/// * **Nothing published / index off** — no address to bake.
///
/// # Lock discipline
///
/// This runs on whichever thread is compiling, including the background
/// compiler, and takes **no VM lock at all**: three relaxed/acquire atomic
/// loads. That is deliberate. The compiler is called from inside the
/// interpreter's tier-up path and from a background thread, and a resolver that
/// reached for `class_manager.read()` would be one queued writer away from
/// deadlocking a compile against a class load.
///
/// # Safety
///
/// `vm_ptr` must be the `SharedVm` pointer the JIT registered alongside this
/// function (see `direct_helper_table_for`), or 0.
pub unsafe extern "C" fn jit_resolve_static_base(
    vm_ptr: i64,
    class_id_raw: i64,
    field_index: i64,
) -> i64 {
    if vm_ptr == 0 || class_id_raw < 0 || field_index < 0 {
        return 0;
    }
    // SAFETY: `vm_ptr` is the `&SharedVm` the VM registered with the JIT; the
    // `SharedVm` lives inside an `Arc` for the life of the process.
    let vm = &*(vm_ptr as *const SharedVm);
    let raw = class_id_raw as u32;
    if vm
        .classes
        .system_class_id
        .load(std::sync::atomic::Ordering::Relaxed)
        == raw
    {
        // SBR5 lane ncall: only `out`/`err`/`in` need the bootstrap intercept
        // (`jit_getstatic`). Every other `java/lang/System` static -- `props`,
        // `lineSeparator`, ... -- is an ordinary field; declining the whole
        // class sent `System.getProperty`/`lineSeparator` through the helper
        // call (~60 ns each, 17% + 12% of a `getProperty` loop).
        if crate::runtime::interpreter::system_stream_field(
            vm,
            ClassId::new(raw),
            field_index as usize,
        )
        .is_some()
        {
            return 0;
        }
    }
    if !class_init_memo::is_initialized(vm, raw) {
        return 0;
    }
    if crate::vm::statics_index_disabled() {
        return 0;
    }
    // A `Boolean` static that still reads back as no object may be the
    // synthetic `TRUE`/`FALSE` the helper repairs (`jit_getstatic`, i1 wave
    // 5); a direct load would bake in the null. Once repaired it is stored,
    // so a later compile inlines it. Lock-free: the index read only.
    if vm
        .classes
        .boolean_class_id
        .load(std::sync::atomic::Ordering::Relaxed)
        == raw
        && !matches!(
            vm.classes
                .statics_index
                .get(ClassId::new(raw), field_index as usize),
            Some(Value::Object(Some(_)))
        )
    {
        return 0;
    }
    match vm
        .classes
        .statics_index
        .base_cell_addr(ClassId::new(raw), field_index as usize)
    {
        // Cast: a user-space address always fits the positive i64 range; `0` is
        // the sentinel and `base_cell_addr` never returns a null cell address.
        Some(addr) => addr as i64,
        None => 0,
    }
}

/// Compile-time predicate: is `class_id` initialised in this VM? `1` / `0`.
///
/// Asked by the optimizing tier's `Op::New` lowering before it takes the
/// inline TLAB bump, which runs no `<clinit>`. Conservative: `0` for any class
/// whose `<clinit>` has not completed in this VM (`class_init_memo` is filled
/// at the end of every successful `<clinit>`). Monotone, so a `1` stays true
/// for the life of the code it justifies. The same `Relaxed` memo read the
/// static-base resolver makes; the compiled code that depends on the answer is
/// published through the code-install release/acquire.
///
/// # Safety
///
/// `vm_ptr` must be the `SharedVm` pointer published beside this function
/// (`direct_helper_table_for`), or 0.
pub unsafe extern "C" fn jit_class_initialized(vm_ptr: i64, class_id_raw: i64) -> i64 {
    if vm_ptr == 0 || class_id_raw <= 0 || class_id_raw > i64::from(u32::MAX) {
        return 0;
    }
    // SAFETY: as `jit_resolve_static_base`: `vm_ptr` is the `&SharedVm`
    // published with this predicate, alive for the process.
    let vm = &*(vm_ptr as *const SharedVm);
    // Cast: range-checked above.
    i64::from(class_init_memo::is_initialized(vm, class_id_raw as u32))
}

/// The compile-time reader of a concat bridge site's inline plan (JIT round 19
/// proposal wave 4, lane strgap4, STRGAP19-3): the plan's address, or `0` for
/// a null or non-concat site or one the plan does not admit. Called by the
/// single-pass emitter through `DirectHelperTable::concat_inline_plan`, never
/// from generated code.
///
/// SAFETY: `site_ptr` is 0 or an `indy_bridge` site pointer handed to the
/// compile by this VM (process-lived).
pub unsafe extern "C" fn jit_concat_inline_plan(site_ptr: i64) -> i64 {
    // Cast: the site pointer's bits back to an address.
    match crate::runtime::invokedynamic::jit_concat_site_inline_plan(site_ptr as usize) {
        // Cast: the plan's address as the C ABI's i64 answer.
        Some(plan) => {
            plan as *const cratonvm_jit::concat_inline_plan::JitConcatInlinePlan as usize as i64
        }
        None => 0,
    }
}

/// Compile-time resolver: the `ClassId` of a well-known bootstrap class in
/// this VM, or `0`. `which` is a `cratonvm_jit::direct_helpers::WELL_KNOWN_*`
/// selector: `java/lang/Integer`, `java/lang/String`, and (not a class id)
/// `String.hash`'s slot index plus one. Since round 11 wave 17 also
/// `cratonvm_jit::ir_lower::WELL_KNOWN_INTEGER_CACHE_HIGH` (not a class id
/// either): the resolved `IntegerCache.high`, read through
/// `lang_math::integer_cache_high_for`, which is lock-free once this VM's box
/// table is filed and otherwise takes only that table's own leaf mutex, never
/// `class_manager`.
///
/// Asked by the single-pass emitter for the receiver guard of its inline
/// `Integer.hashCode()` / `equals(Object)` prefix (round 11 wave 12, lane
/// `hashmap`), whose call sites name `java/lang/Object` in their constant pool.
///
/// # Lock discipline
///
/// Never blocks. The name index lives under `class_manager`, and a resolver
/// that WAITED for it on the compiler thread would be one queued writer away
/// from deadlocking a compile against a class load (see
/// `jit_resolve_static_base`). `try_read_untracked` takes the read side only
/// if it is free right now, and skips the lock-order graph because a
/// non-blocking acquisition adds no wait-for edge. A busy lock answers `0`,
/// which only costs this compile its prefix.
///
/// `find_unique_class_by_name` fails closed: an ambiguous name (two
/// definitions) or a hidden class answers `0` as well, so a guard can never be
/// built on the wrong one of two same-named classes.
///
/// # Safety
///
/// `vm_ptr` must be the `SharedVm` pointer published beside this function
/// (`direct_helper_table_for`), or 0.
pub unsafe extern "C" fn jit_well_known_class_id(vm_ptr: i64, which: i64) -> i64 {
    if vm_ptr == 0 {
        return 0;
    }
    // JIT round 19 proposal wave 4, lane strgap4 (STRGAP19-3): NOT a class id,
    // but the address of this VM's `compact_strings` flag, which the inline
    // concat tests per evaluation (a LATIN1 result only while it is set). The
    // `SharedVm` lives for the process, so the address is stable.
    if which == cratonvm_jit::direct_helpers::WELL_KNOWN_COMPACT_STRINGS_FLAG {
        // SAFETY: as below -- `vm_ptr` is the `&SharedVm` published with this
        // resolver, alive for the process.
        let vm = &*(vm_ptr as *const SharedVm);
        // Cast: the flag's address as the C ABI's i64 answer.
        // j27 wave 9 (W9VER): `COMPACT_STRINGS` as the JDK reads it, false
        // under `-XX:-CompactStrings` (`ClassRealm::jit_compact_strings_flag`).
        return vm.classes.jit_compact_strings_flag() as *const std::sync::atomic::AtomicBool as usize as i64;
    }
    // JIT round 19 proposal wave 7, lane tail7 (OSRFRAME18-2): NOT class ids,
    // but the addresses of the two thin helpers' leaf twins, which the
    // single-pass tail site calls under `CRATONVM_JIT_SP_TAIL_LEAF_HELPERS`
    // (the compiler asks only with that switch on). Process-lived functions.
    if which == cratonvm_jit::direct_helpers::WELL_KNOWN_LEAF_TWIN_CURRENT_THREAD {
        let _: cratonvm_jit::direct_helpers::DirectHelperFnThreadCurrentThread =
            jit_thread_current_thread_leaf;
        // Cast: the twin's address as the C ABI's i64 answer.
        return jit_thread_current_thread_leaf as *const () as usize as i64;
    }
    if which == cratonvm_jit::direct_helpers::WELL_KNOWN_LEAF_TWIN_CHECK_INDEX {
        let _: cratonvm_jit::direct_helpers::DirectHelperFnPreconditionsCheckIndex =
            jit_preconditions_check_index_leaf;
        // Cast: as above.
        return jit_preconditions_check_index_leaf as *const () as usize as i64;
    }
    // JIT round 19 proposal wave 8, lane tail8 (TAIL7-19-3): four more leaf
    // twins, asked only under `CRATONVM_JIT_SP_TAIL_LEAF_HELPERS_WIDE`. Each is
    // coerced to its helper's `DirectHelperFn*` alias, so a signature drift is
    // a compile error here.
    if which == cratonvm_jit::direct_helpers::WELL_KNOWN_LEAF_TWIN_THREAD_LOCAL_GET {
        let _: cratonvm_jit::direct_helpers::DirectHelperFnThreadLocalGet =
            jit_thread_local_get_leaf;
        // SAFETY: as above.
        let vm = &*(vm_ptr as *const SharedVm);
        // Only where the helper's fast arm can serve at all: elsewhere the twin
        // would decline every call, and the site keeps its ordinary call.
        if !thread_local_real_fast_serves(vm) {
            return 0;
        }
        // Cast: the twin's address as the C ABI's i64 answer.
        return jit_thread_local_get_leaf as *const () as usize as i64;
    }
    if which == cratonvm_jit::direct_helpers::WELL_KNOWN_LEAF_TWIN_UNSAFE_CAS_INT {
        let _: cratonvm_jit::direct_helpers::DirectHelperFnUnsafeCasInt = jit_unsafe_cas_int_leaf;
        // Cast: as above.
        return jit_unsafe_cas_int_leaf as *const () as usize as i64;
    }
    if which == cratonvm_jit::direct_helpers::WELL_KNOWN_LEAF_TWIN_UNSAFE_CAS_LONG {
        let _: cratonvm_jit::direct_helpers::DirectHelperFnUnsafeCasLong = jit_unsafe_cas_long_leaf;
        // Cast: as above.
        return jit_unsafe_cas_long_leaf as *const () as usize as i64;
    }
    if which == cratonvm_jit::direct_helpers::WELL_KNOWN_LEAF_TWIN_VECTORIZED_HASH_CODE {
        let _: cratonvm_jit::direct_helpers::DirectHelperFnVectorizedHashCode =
            jit_vectorized_hash_code_leaf;
        // Cast: as above.
        return jit_vectorized_hash_code_leaf as *const () as usize as i64;
    }
    // JIT round 19 proposal wave 5, lane concat5 (STRGAP4-2): NOT class ids
    // either, but the addresses of this VM's rooted `"null"` / `""` literal
    // slots (`string_literal_slot_for`), or `0` while the literal is not
    // pooled yet (the site then keeps its call).
    if which == cratonvm_jit::direct_helpers::WELL_KNOWN_NULL_LITERAL_SLOT
        || which == cratonvm_jit::direct_helpers::WELL_KNOWN_EMPTY_LITERAL_SLOT
    {
        // SAFETY: as above.
        let vm = &*(vm_ptr as *const SharedVm);
        let literal =
            u32::from(which == cratonvm_jit::direct_helpers::WELL_KNOWN_EMPTY_LITERAL_SLOT);
        return i64::try_from(string_literal_slot_for(vm, literal)).unwrap_or(0);
    }
    use cratonvm_jit::direct_helpers::{
        WELL_KNOWN_CLASS_INTEGER, WELL_KNOWN_CLASS_STRING, WELL_KNOWN_STRING_HASH_FIELD,
    };
    // JIT round 21 wave 1, lane updaters (`CRATONVM_JIT_VH_CONST_CAST_RETURN`):
    // the class id a constant-`VarHandle` get site's narrower return class
    // names for its caller (`ir_lower::vh_const_class_by_name`), only when it
    // is the ONE loaded class of that name (so the inline cast can never
    // admit a same-named class of another loader); `0` otherwise. Never loads
    // a class; a `try_read`.
    if which & !0xFFFF_FFFF_FFFF_i64
        == cratonvm_jit::ir_lower::WELL_KNOWN_VH_CONST_CLASS_BY_NAME_TAG
    {
        // SAFETY: as below -- `vm_ptr` is the `&SharedVm` published with this
        // resolver, alive for the process.
        let vm = &*(vm_ptr as *const SharedVm);
        // SAFETY: the compiler passes, under this tag, the address of a live
        // `VhConstClassByName` it holds for the duration of this call
        // (`vh_const_class_by_name`); nothing else builds the tag.
        let query = &*((which & 0xFFFF_FFFF_FFFF) as usize
            as *const cratonvm_jit::ir_lower::VhConstClassByName);
        if query.name_ptr.is_null() || query.name_len == 0 || query.name_len > 4096 {
            return 0;
        }
        // SAFETY: `name_ptr` / `name_len` are a `&str`'s, alive as above.
        let bytes = std::slice::from_raw_parts(query.name_ptr, query.name_len);
        let Ok(name) = std::str::from_utf8(bytes) else {
            return 0;
        };
        let Some(cm) = vm.classes.class_manager.try_read_untracked() else {
            return 0;
        };
        let for_caller = cm.find_class_by_name_for_class(name, ClassId::new(query.caller_class_id));
        return match (for_caller, cm.find_unique_class_by_name(name)) {
            (Some(a), Some(b)) if a == b => i64::from(a.as_u32()),
            _ => 0,
        };
    }
    // JIT round 21 wave 16 (lane perf-cf-lean,
    // `CRATONVM_JIT_IR_CALL_ENTRY_NULL_GUARD`): NOT a class id -- whether a
    // `void` call's target returns at once while a field of `this` is null
    // (`ir_lower::call_entry_null_guard_ask`), answered by
    // `jit_bridge::call_entry_null_guard_answer`. Never loads a class.
    if which & !0xFFFF_FFFF_FFFF_i64 == cratonvm_jit::ir_lower::WELL_KNOWN_CALL_ENTRY_NULL_GUARD_TAG
    {
        // SAFETY: as below -- `vm_ptr` is the `&SharedVm` published with this
        // resolver, alive for the process.
        let vm = &*(vm_ptr as *const SharedVm);
        // SAFETY: the compiler passes, under this tag, the address of a live
        // `CallEntryNullGuardQuery` it holds for the duration of this call
        // (`call_entry_null_guard_ask`); nothing else builds the tag.
        let query = &*((which & 0xFFFF_FFFF_FFFF) as usize
            as *const cratonvm_jit::ir_lower::CallEntryNullGuardQuery);
        /// The query's UTF-8 text at `ptr` / `len`, or `None`.
        ///
        /// # Safety
        /// `ptr` / `len` must describe live bytes for `'a`.
        unsafe fn text<'a>(ptr: *const u8, len: usize) -> Option<&'a str> {
            if ptr.is_null() || len == 0 || len > 4096 {
                return None;
            }
            // SAFETY: the caller's contract.
            std::str::from_utf8(unsafe { std::slice::from_raw_parts(ptr, len) }).ok()
        }
        // The three texts are the call's `JitInvokeInfo` `&'static str`s
        // (`call_entry_null_guard_ask`), alive for this call.
        let (Some(class), Some(method), Some(desc)) = (
            text(query.class_ptr, query.class_len),
            text(query.method_ptr, query.method_len),
            text(query.desc_ptr, query.desc_len),
        ) else {
            return 0;
        };
        return crate::runtime::interpreter::jit_bridge::call_entry_null_guard_answer(
            vm,
            ClassId::new(query.caller_class_id),
            query.invoke_kind,
            class,
            method,
            desc,
        );
    }
    // JIT r23 wave 4 (lane L8, L8-8 stages 2 and 4): NOT a class id -- a
    // segment field's slot or kind, or a filed layout's facts word
    // (`ffm_inline_well_known_answer`). Asked only under
    // `CRATONVM_JIT_IR_FFM_ELEMENT_INLINE`.
    if let Some((tag, class_id, index)) =
        cratonvm_jit::ffm_element_inline::ffm_inline_query_decode(which)
    {
        // SAFETY: as below -- `vm_ptr` is the `&SharedVm` published with this
        // resolver, alive for the process.
        let vm = &*(vm_ptr as *const SharedVm);
        return ffm_inline_well_known_answer(vm, tag, class_id, index);
    }
    // nio21 wave 11 L5 (W9L2-2): NOT a class id -- the constant-`VarHandle`
    // plan word or reference row of one `static final` field
    // (`vh_const_well_known_answer`). A `try_read`: never waits on a filing.
    {
        // SAFETY: as below -- `vm_ptr` is the `&SharedVm` published with this
        // resolver, alive for the process.
        let vm = &*(vm_ptr as *const SharedVm);
        if let Some(answer) = vh_const_well_known_answer(vm, which) {
            return answer;
        }
        // JIT round 21 wave 1 (lane indy, INDY21-2): the class-init barrier's
        // three words (`clinit_barrier_well_known_answer`).
        if let Some(answer) = clinit_barrier_well_known_answer(vm, which) {
            return answer;
        }
    }
    // Round 11 wave 17 (lane `hashmap`): NOT a class id, but the
    // `IntegerCache.high` this VM's `Integer.valueOf` native resolved, for the
    // optimizing tier's inline out-of-cache box
    // (`ir_lower::try_emit_ir_integer_value_of_prefix`). The same bound
    // `jit_integer_value_of_direct` tests per call, which is what makes the
    // inline answer the helper's own. `0` while no `valueOf` has resolved it:
    // the site then keeps the call. It needs no class-manager read.
    if which == cratonvm_jit::ir_lower::WELL_KNOWN_INTEGER_CACHE_HIGH {
        // SAFETY: as below -- `vm_ptr` is the `&SharedVm` published with this
        // resolver, alive for the process.
        let vm = &*(vm_ptr as *const SharedVm);
        return cratonvm_native_builtins::lang_math::integer_cache_high_for(vm.vm_identity)
            .map_or(0, i64::from);
    }
    // JIT round 16 (lane callbind): NOT class ids either, but the address and
    // the slot count of this VM's `Integer` box table
    // (`lang_math::integer_cache_slots_raw`), for the optimizing tier's inline
    // cached-range `Integer.valueOf` read. The same table
    // `jit_integer_value_of_direct`'s `integer_cache_peek` reads, leaked and
    // never resized, so both answers are fixed once non-zero. `0` while no
    // `valueOf` has filed it: the site then keeps the call. Lock-free once
    // filed; otherwise only the table directory's leaf mutex.
    if which == cratonvm_jit::ir_lower::WELL_KNOWN_INTEGER_CACHE_SLOTS
        || which == cratonvm_jit::ir_lower::WELL_KNOWN_INTEGER_CACHE_SLOTS_LEN
    {
        // SAFETY: as above.
        let vm = &*(vm_ptr as *const SharedVm);
        let Some((base, len)) =
            cratonvm_native_builtins::lang_math::integer_cache_slots_raw(vm.vm_identity)
        else {
            return 0;
        };
        let answer = if which == cratonvm_jit::ir_lower::WELL_KNOWN_INTEGER_CACHE_SLOTS {
            base
        } else {
            len
        };
        return i64::try_from(answer).unwrap_or(0);
    }
    // JIT round 16 wave 7 (lane callbind, CB16-3): the `Long` box table's slot
    // 0 address (`lang_math::long_cache_slots_raw`), for the optimizing tier's
    // inline cached-range `Long.valueOf` read. Answered only for the fixed
    // 256-slot table `jit_long_value_of_direct`'s `long_cache_peek` reads; `0`
    // while no `Long.valueOf` has filed it (the site keeps the call).
    if which == cratonvm_jit::ir_lower::WELL_KNOWN_LONG_CACHE_SLOTS {
        // SAFETY: as above.
        let vm = &*(vm_ptr as *const SharedVm);
        return match cratonvm_native_builtins::lang_math::long_cache_slots_raw(vm.vm_identity) {
            Some((base, 256)) => i64::try_from(base).unwrap_or(0),
            _ => 0,
        };
    }
    // nio20 wave 4, lane L2: the three never-closing JDK session classes the
    // optimizing tier's scoped fold admits as a non-null session
    // (`ir_lower::scoped_session_never_closing_class_ids` says why each one's
    // `checkValidStateRaw()` cannot fail). The BOOTSTRAP definition only --
    // the one `java.base`'s own `MemorySessionImpl.create*` instantiates -- so
    // a same-named class of another loader is never admitted.
    // nio20 wave 5, lane L2: `java/lang/Class` (bootstrap), the receiver guard
    // of the optimizing tier's inline identity hash at a `Class.hashCode()` /
    // `Object.hashCode()` site (`ir_lower::try_emit_ir_identity_hash_prefix`).
    // nio20 wave 5, lane L2: NOT a class id -- `offset_of!(JvmThread,
    // java_thread_obj)` plus one, for the confined arm of the scoped fold
    // (`ir_lower::scoped_session_confined_rows`), which COMPARES that word with
    // a session's `owner` the way `p67_owner_is_current_thread` does. It never
    // stands in for `Thread.currentThread()`, so unlike
    // `thread_java_obj_offset_for` no execution-policy gate applies.
    if which == cratonvm_jit::ir_lower::WELL_KNOWN_THREAD_JAVA_OBJ_OFFSET {
        const _: () = assert!(
            std::mem::size_of::<Option<cratonvm_types::ObjectRef>>()
                == std::mem::size_of::<usize>()
        );
        if !cfg!(target_arch = "x86_64") {
            return 0;
        }
        return i64::try_from(std::mem::offset_of!(JvmThread, java_thread_obj))
            .ok()
            .and_then(|off| off.checked_add(1))
            .unwrap_or(0);
    }
    // nio21 wave 1 L3: NOT a class id -- the address of the process's
    // JIT-readable `Unsafe` arena table
    // (`cratonvm_native_builtins::unsafe_arena_jit_table_addr`), for the
    // off-heap arm of the optimizing tier's scoped fold
    // (`ir_lower::scoped_arena_jit_table`). `0` when the table is off. The
    // lowering bakes the table's layout; these asserts are where the two
    // crates' copies of it are held equal.
    if which == cratonvm_jit::ir_lower::WELL_KNOWN_UNSAFE_ARENA_JIT_TABLE {
        use cratonvm_jit::ir_lower as jl;
        use cratonvm_native_builtins as nb;
        const _: () = assert!(nb::UNSAFE_ARENA_JIT_SLOT_SHIFT == jl::SCOPED_ARENA_SLOT_SHIFT);
        const _: () = assert!(nb::UNSAFE_ARENA_JIT_INDEX_BITS == jl::SCOPED_ARENA_INDEX_BITS);
        const _: () = assert!(nb::UNSAFE_ARENA_JIT_ENTRY_SHIFT == jl::SCOPED_ARENA_ENTRY_SHIFT);
        const _: () = assert!(nb::UNSAFE_ARENA_JIT_KEY_OFFSET == jl::SCOPED_ARENA_KEY_OFFSET);
        const _: () = assert!(nb::UNSAFE_ARENA_JIT_LEN_OFFSET == jl::SCOPED_ARENA_LEN_OFFSET);
        const _: () = assert!(nb::UNSAFE_ARENA_JIT_PTR_OFFSET == jl::SCOPED_ARENA_PTR_OFFSET);
        if !cfg!(target_arch = "x86_64") {
            return 0;
        }
        return i64::try_from(nb::unsafe_arena_jit_table_addr()).unwrap_or(0);
    }
    // JIT r20 wave 19 (lane yFfmShared, MEM20-2): NOT a class id -- the TLS
    // displacement (plus one) of the FFM access-word mirror
    // (`cratonvm_native_builtins::ffm_fast::access_word_tls_disp`), which the
    // optimizing tier's scoped fold reads to open this thread's access window
    // inline for a shared session (`ir_lower::scoped_access_word_tls_disp`).
    // Process-wide and thread-invariant; `0` when unavailable.
    if which == cratonvm_jit::ir_lower::WELL_KNOWN_FFM_ACCESS_WORD_TLS_DISP {
        if !cfg!(target_arch = "x86_64") {
            return 0;
        }
        return cratonvm_native_builtins::ffm_fast::access_word_tls_disp()
            .map_or(0, |disp| i64::from(disp) + 1);
    }
    // nio20 wave 5, lane L2: the two confined-session classes of the scoped
    // fold (the JDK's `ConfinedSession`, and `MemorySessionImpl` itself --
    // the class of CratonVM's session model) and their `state` / `owner`
    // slots (plus one), resolved by name AND descriptor through the hierarchy
    // on the BOOTSTRAP definition, as `p67_session_slots` resolves them by
    // name. An absent class or field answers 0 and the fold keeps its deopt.
    let confined_slot = if which == cratonvm_jit::ir_lower::WELL_KNOWN_SESSION_CONFINED_STATE_SLOT {
        Some(("jdk/internal/foreign/ConfinedSession", "state", "I"))
    } else if which == cratonvm_jit::ir_lower::WELL_KNOWN_SESSION_CONFINED_OWNER_SLOT {
        Some((
            "jdk/internal/foreign/ConfinedSession",
            "owner",
            "Ljava/lang/Thread;",
        ))
    } else if which == cratonvm_jit::ir_lower::WELL_KNOWN_SESSION_SHARED_STATE_SLOT {
        // JIT round 22 (lane memory, MEM20-2 residual): the JDK's
        // `SharedSession`, for the scoped fold's shared window.
        Some(("jdk/internal/foreign/SharedSession", "state", "I"))
    } else if which == cratonvm_jit::ir_lower::WELL_KNOWN_SESSION_SHARED_OWNER_SLOT {
        Some((
            "jdk/internal/foreign/SharedSession",
            "owner",
            "Ljava/lang/Thread;",
        ))
    } else if which == cratonvm_jit::ir_lower::WELL_KNOWN_SESSION_CONFINED_ACQUIRE_SLOT {
        // JIT round 22 (lane memory, FFM22-3): the count the compiled
        // segment call's helper acquires (`jit_ffm_const_seg_call`).
        Some(("jdk/internal/foreign/ConfinedSession", "acquireCount", "I"))
    } else if which == cratonvm_jit::ir_lower::WELL_KNOWN_SEGMENT_NATIVE_MIN_SLOT {
        // Same (FFM22-3): the JDK's native segment, as the direct segment
        // row's `segment_row_real_slots` resolves it by name.
        Some(("jdk/internal/foreign/NativeMemorySegmentImpl", "min", "J"))
    } else if which == cratonvm_jit::ir_lower::WELL_KNOWN_SEGMENT_NATIVE_LENGTH_SLOT {
        Some((
            "jdk/internal/foreign/NativeMemorySegmentImpl",
            "length",
            "J",
        ))
    } else if which == cratonvm_jit::ir_lower::WELL_KNOWN_SEGMENT_NATIVE_SCOPE_SLOT {
        Some((
            "jdk/internal/foreign/NativeMemorySegmentImpl",
            "scope",
            "Ljdk/internal/foreign/MemorySessionImpl;",
        ))
    } else if which == cratonvm_jit::ir_lower::WELL_KNOWN_SESSION_MODEL_STATE_SLOT {
        // nio21 wave 2 (lane L3, W1L3-2): the model's never-closing state
        // word, baked by the lowering; the two crates' copies held equal here.
        const _: () = assert!(
            cratonvm_native_builtins::phases_late::foreign_ffm::P67_SESSION_STATE_NONCLOSEABLE
                == cratonvm_jit::ir_lower::SCOPED_MODEL_STATE_NONCLOSEABLE as i32
        );
        Some(("jdk/internal/foreign/MemorySessionImpl", "state", "I"))
    } else if which == cratonvm_jit::ir_lower::WELL_KNOWN_SESSION_MODEL_OWNER_SLOT {
        Some((
            "jdk/internal/foreign/MemorySessionImpl",
            "owner",
            "Ljava/lang/Thread;",
        ))
    } else {
        None
    };
    if let Some((class_name, field, descriptor)) = confined_slot {
        // SAFETY: as below.
        let vm = &*(vm_ptr as *const SharedVm);
        let Some(cm) = vm.classes.class_manager.try_read_untracked() else {
            return 0;
        };
        let Some(cid) = cm.find_bootstrap_class_by_name(class_name) else {
            return 0;
        };
        return crate::vm::resolve_field_index_in_hierarchy_desc(
            cid,
            field,
            Some(descriptor),
            &cm.class_store,
        )
        .and_then(|idx| i64::try_from(idx).ok())
        .and_then(|idx| idx.checked_add(1))
        .unwrap_or(0);
    }
    let session_class = if which == cratonvm_jit::ir_lower::WELL_KNOWN_CLASS_CLASS {
        Some("java/lang/Class")
    } else if which == cratonvm_jit::ir_lower::WELL_KNOWN_SEGMENT_NATIVE {
        Some("jdk/internal/foreign/NativeMemorySegmentImpl")
    } else if which == cratonvm_jit::ir_lower::WELL_KNOWN_SESSION_CONFINED {
        Some("jdk/internal/foreign/ConfinedSession")
    } else if which == cratonvm_jit::ir_lower::WELL_KNOWN_SESSION_MODEL {
        Some("jdk/internal/foreign/MemorySessionImpl")
    } else if which == cratonvm_jit::ir_lower::WELL_KNOWN_SESSION_SHARED {
        Some("jdk/internal/foreign/SharedSession")
    } else if which == cratonvm_jit::ir_lower::WELL_KNOWN_SESSION_GLOBAL {
        Some("jdk/internal/foreign/GlobalSession")
    } else if which == cratonvm_jit::ir_lower::WELL_KNOWN_SESSION_HEAP {
        Some("jdk/internal/foreign/GlobalSession$HeapSession")
    } else if which == cratonvm_jit::ir_lower::WELL_KNOWN_SESSION_IMPLICIT {
        Some("jdk/internal/foreign/ImplicitSession")
    } else {
        None
    };
    if let Some(name) = session_class {
        // SAFETY: as below.
        let vm = &*(vm_ptr as *const SharedVm);
        let Some(cm) = vm.classes.class_manager.try_read_untracked() else {
            return 0;
        };
        return cm
            .find_bootstrap_class_by_name(name)
            .map_or(0, |id| i64::from(id.as_u32()));
    }
    if !matches!(
        which,
        WELL_KNOWN_CLASS_INTEGER | WELL_KNOWN_CLASS_STRING | WELL_KNOWN_STRING_HASH_FIELD
    ) && which != cratonvm_jit::ir_lower::WELL_KNOWN_CLASS_LONG
    {
        return 0;
    }
    // SAFETY: as `jit_resolve_static_base`: `vm_ptr` is the `&SharedVm`
    // published with this resolver, alive for the process.
    let vm = &*(vm_ptr as *const SharedVm);
    // JIT r23 wave 6, lane L2 (`CRATONVM_JIT_WELL_KNOWN_BOX_CLASS_MEMO`,
    // default ON): the `Integer` / `Long` answer, once given, is memoised per
    // VM. The lookup below is a `try_read`: a compile that asked while any
    // thread held the class-manager write lock (a class being defined -- the
    // timing loop's own `println` spins string-concat classes) got `0`, so
    // that body baked no inline out-of-cache box and called the helper for
    // every box for its whole life. `BoxLam` ran 20 or 70-90 ns/op from run
    // to run on that alone (n23-w5). `java/lang/Integer` / `Long` are boot
    // classes no other loader may define, so the first answer is the answer.
    let box_memo = (which == WELL_KNOWN_CLASS_INTEGER
        || which == cratonvm_jit::ir_lower::WELL_KNOWN_CLASS_LONG)
        .then(|| {
            crate::runtime::interpreter::jit_bridge::well_known_box_class_memo(
                vm,
                which != WELL_KNOWN_CLASS_INTEGER,
            )
        })
        .flatten();
    if let Some(memo) = box_memo {
        let id = memo.load(std::sync::atomic::Ordering::Acquire);
        if id != 0 {
            return i64::from(id);
        }
    }
    let Some(cm) = vm.classes.class_manager.try_read_untracked() else {
        return 0;
    };
    let remember = |id: i64| {
        if let (Some(memo), Ok(raw)) = (box_memo, u32::try_from(id)) {
            if raw != 0 {
                memo.store(raw, std::sync::atomic::Ordering::Release);
            }
        }
        id
    };
    if which == WELL_KNOWN_CLASS_INTEGER {
        return remember(
            cm.find_unique_class_by_name("java/lang/Integer")
                .map_or(0, |id| i64::from(id.as_u32())),
        );
    }
    // Round 12 wave 2 (lane `calls`): `java/lang/Long`, for both tiers' inline
    // out-of-cache `Long.valueOf` box, answered exactly as `Integer` is.
    if which == cratonvm_jit::ir_lower::WELL_KNOWN_CLASS_LONG {
        return remember(
            cm.find_unique_class_by_name("java/lang/Long")
                .map_or(0, |id| i64::from(id.as_u32())),
        );
    }
    // Round 11 wave 14 (lane `hashmap`): `java/lang/String` and its `hash`
    // slot, for the optimizing tier's `Object.hashCode()` prefix. Resolved
    // the way `resolve_string_field_layout` resolves the single-pass
    // prefix's layout -- the BOOTSTRAP definition, its own fields -- and
    // only for the shape that prefix accepts (`has_coder`): a `byte[] value`,
    // a `byte coder` and an instance `int hash`. Anything else answers 0 for
    // BOTH selectors, so the prefix keeps the plain call.
    let Some(string_id) = cm.find_bootstrap_class_by_name("java/lang/String") else {
        return 0;
    };
    let Some(class) = cm.get_class(string_id) else {
        return 0;
    };
    let instance = |name: &str, descriptor: &str| {
        class
            .find_own_field_by_descriptor(name, descriptor)
            .filter(|(_, f)| !f.is_static())
            .map(|(idx, _)| idx)
    };
    let (Some(_), Some(_), Some(hash_idx)) = (
        instance("value", "[B"),
        instance("coder", "B"),
        instance("hash", "I"),
    ) else {
        return 0;
    };
    if which == WELL_KNOWN_CLASS_STRING {
        i64::from(string_id.as_u32())
    } else {
        // `index + 1`, so field 0 stays distinguishable from "unknown".
        i64::try_from(hash_idx)
            .ok()
            .and_then(|i| i.checked_add(1))
            .unwrap_or(0)
    }
}

pub unsafe extern "C" fn jit_getstatic(vm_ptr: i64, class_id_raw: i64, field_index: i64) -> i64 {
    let f = || jit_getstatic_body(vm_ptr, class_id_raw, field_index);
    contain("jit_getstatic", OnPanic::Throw { vm_ptr }, i64::MIN, f)
}

/// Body of [`jit_getstatic`], run under its panic guard.
unsafe fn jit_getstatic_body(vm_ptr: i64, class_id_raw: i64, field_index: i64) -> i64 {
    // Gated like the cycle guards below: only `gs_prof::dump` reads it, and
    // only when armed. Unconditional, it was a process-wide atomic RMW on
    // every helper `getstatic`, one cache line bounced between all threads.
    if gs_prof::enabled() {
        gs_prof::CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let _gs_total = gs_prof::CycGuard::new(&gs_prof::CYC_TOTAL);
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    {
        let _g = gs_prof::CycGuard::new(&gs_prof::CYC_BOUNDARY);
        crate::jit::conservative_roots::note_jit_boundary();
    }
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let class_id = ClassId::new(class_id_raw as u32);

    // JVMS §5.5 / §getstatic: the declaring class must be initialized
    // before its static storage is read. The interpreter's `getstatic`
    // handler (`runtime/interpreter.rs`) already calls
    // `ensure_class_initialized_shared` first; this JIT helper never did.
    // A JIT-compiled `getstatic` that happens to be the FIRST-EVER access
    // to that class's statics (e.g. a rarely-taken branch, such as
    // `LineWrapper.append`'s `shouldWrap ? FlushType.WRAP : nextFlush`
    // ternary, whose WRAP arm is only exercised once a line actually needs
    // wrapping -- well after the surrounding method has already tiered up
    // to JIT) silently read the zero-initialized placeholder
    // (`get_static_shared` returns `Value::Int(0)` for a class with no
    // `statics` entry yet) instead of running `<clinit>` first. Decoded as
    // a reference by the caller, that `Int(0)` becomes a null pointer --
    // `LineWrapper$FlushType.ordinal()` NPE, JIT-only (the interpreter
    // path always initializes the class on its own earlier `getstatic`,
    // masking this gap; see CRATONVM-SPRING-GENUINE-BUGLIST,
    // "JavaPoet LineWrapper$FlushType NPE" JIT-only residual).
    //
    // Mirror the interpreter: ensure init before reading. On failure
    // (`<clinit>` threw -- JVMS wraps this as ExceptionInInitializerError),
    // stash the exception and return the `i64::MIN` deopt sentinel so the
    // caller's `emit_post_invoke_exception_check` (added alongside this
    // fix in `jit/src/x64.rs`'s `0xb2` getstatic codegen) routes it through
    // the method's exception table instead of pushing a bogus value.
    //
    // Steady state costs one relaxed load and a bit test, no lock at all:
    // `ensure_class_initialized_shared`'s own fast path still needs a
    // `class_manager.read()` to reach the per-class atomic, and paying that on
    // every static read from compiled code was a measurable part of
    // `getstatic`'s ~35 ns. Initialization is monotonic, so the memo never
    // needs invalidating. See `class_init_memo`.
    let _gs_init = gs_prof::CycGuard::new(&gs_prof::CYC_INIT);
    if !class_init_memo::is_initialized(vm, class_id_raw as u32) {
        if let Some((thread, _guard)) = jit_thread_mut() {
            // JIT round 21 wave 1 (lane indy, `CRATONVM_JIT_GETSTATIC_OWN_CLINIT_FAST`,
            // opt-in): a read from inside the class's own `<clinit>` (this
            // thread holds its init claim) is the JVMS 5.5 step 3 recursive
            // request, which the walk below answers `Ok`, and the re-check
            // after it answers `false` (the class is not `Initialized` yet).
            // One untracked class-manager read answers both.
            use crate::runtime::interpreter::jit_bridge as bridge;
            let own_clinit = bridge::getstatic_own_clinit_fast_enabled(vm)
                && bridge::class_initializing_on(vm, class_id, thread.thread_id.0);
            if own_clinit {
                // Nothing to memoize: the class is still being initialized.
            } else if let Err(err) =
                crate::vm::ensure_class_initialized_shared(vm, thread, class_id)
            {
                let convert = helper_failure_conversion_on();
                stash_helper_failure(
                    vm,
                    thread,
                    err,
                    &format_args!(
                        "JIT getstatic class_id {class_id_raw} field_index {field_index} failed to initialize"
                    ),
                    convert,
                    convert,
                );
                return i64::MIN;
            }
            // Only memoize a CONFIRMED success. A failed `<clinit>` must be
            // re-attempted per JVMS (the class enters the erroneous state and
            // subsequent attempts throw NoClassDefFoundError from the real
            // check, not from a stale memo).
            //
            // `Ok` from `ensure_class_initialized_shared` is NOT that
            // confirmation: it also answers `Ok` for a class THIS thread is
            // still initializing (a compiled `getstatic` reached from the
            // class's own `<clinit>`). The memo is shared by every thread of
            // the VM, so marking it then let another thread's compiled
            // `getstatic` -- and the inline load `jit_resolve_static_base` admits --
            // read the statics without waiting for `<clinit>` to finish, and
            // kept doing so after a failed `<clinit>`. Mark only a class that
            // is `Initialized` (the one-time slow path pays the check);
            // `note_class_initialized` marks it anyway when `<clinit>` ends.
            let generation = class_init_memo::generation(vm);
            if !own_clinit && crate::vm::is_class_initialized_via_manager(vm, class_id) {
                class_init_memo::mark_initialized(vm, class_id_raw as u32, generation);
            }
        }
    }

    // Bootstrap intercept: mirror the interpreter's System.out/err/in intercept.
    // The real JDK System.<clinit> isn't fully bootable; the interpreter returns
    // pre-built synthetic streams for these three fields. The JIT must do the same,
    // or jit_getstatic falls through to get_static_shared → Object(None) → null
    // receiver → println silently no-ops (arg0=0x0 in jit_invoke_dispatch).
    // The intercept concerns exactly ONE class, but the probe for it used to
    // run on EVERY `getstatic`: a `class_manager.read()` acquisition, a
    // `get_class`, and a string compare of the class name — per static field
    // read, in compiled code. Measured on `probes/StaticFieldProbe.java`, a
    // `getstatic` cost ~35 ns against HotSpot's ~1, and this was part of it.
    //
    // Resolve `java/lang/System`'s ClassId once and compare integers instead.
    // Every other class now answers with a relaxed atomic load and an `==`,
    // touching no lock at all. `java/lang/System` is loaded during bootstrap
    // and its id is stable for the life of the VM, so a one-shot cache is
    // sound; until it resolves we fall back to the id-less answer (`None`),
    // which is what a not-yet-loaded System would have produced anyway.
    drop(_gs_init);
    // Scoped so the guard is DROPPED here rather than at end of function — an
    // unscoped guard silently folded the read segment into this one and made
    // `syscheck` look like the dominant cost.
    // `system_stream_field` counts STATIC fields: `field_index` is a static
    // index, which used to be looked up in `Class::fields` (every field).
    use crate::runtime::interpreter::SystemStreamField;
    let stream_field = {
        let _gs_sys = gs_prof::CycGuard::new(&gs_prof::CYC_SYSCHECK);
        if crate::runtime::interpreter::static_may_be_system(vm, class_id) {
            crate::runtime::interpreter::system_stream_field(vm, class_id, field_index as usize)
        } else {
            None
        }
    };
    if let Some(stream_field) = stream_field {
        if stream_field != SystemStreamField::In {
            // Honor System.setOut/setErr: if the static field was explicitly set
            // (via setOut0/setErr0), use that value; otherwise fall back to the
            // canonical synthetic stream (same logic as the interpreter intercept).
            let overridden = match crate::vm::get_static_shared(vm, class_id, field_index as usize)
            {
                Value::Object(Some(s)) => Some(s),
                _ => None,
            };
            let stream = match overridden {
                Some(s) => s,
                // `System.setOut(null)`: null, as `op_getstatic` answers
                // (`SystemStreamField::null_reads_as_null`).
                None if stream_field.null_reads_as_null(vm) => return 0,
                None => {
                    let (out, err) = match vm.try_ensure_system_streams() {
                        Ok(pair) => pair,
                        Err(error) => {
                            if let Some((thread, _guard)) = jit_thread_mut() {
                                let failure = crate::runtime::exceptions::throw_runtime_error(
                                    vm, thread, error,
                                );
                                if let crate::error::MethodCallFailed::ExceptionThrown(exc) =
                                    failure
                                {
                                    set_jit_pending_exception(thread, exc);
                                }
                            }
                            return i64::MIN;
                        }
                    };
                    if stream_field == SystemStreamField::Out {
                        out
                    } else {
                        err
                    }
                }
            };
            return stream.as_ptr() as i64;
        } else {
            // System.in: the static field first (`System.setIn`), then the
            // canonical stdin -- `op_getstatic`'s rule. Until i1 wave 6 this
            // arm answered `null` while the field was still empty, where the
            // interpreter builds the stdin object (`jit_thread_mut` gives the
            // thread `ensure_system_stdin_object` needs, as the `Boolean`
            // repair below uses it).
            let stdin = match crate::vm::get_static_shared(vm, class_id, field_index as usize) {
                Value::Object(Some(r)) => Some(r),
                // `System.setIn(null)` (see the out/err arm above).
                _ if stream_field.null_reads_as_null(vm) => None,
                _ => match jit_thread_mut() {
                    Some((thread, _guard)) => {
                        match crate::vm::ensure_system_stdin_object(vm, thread) {
                            Ok(r) => Some(r),
                            Err(crate::error::MethodCallFailed::ExceptionThrown(exc)) => {
                                set_jit_pending_exception(thread, exc);
                                return i64::MIN;
                            }
                            // Nothing to throw: the old answer, `null`.
                            Err(crate::error::MethodCallFailed::InternalError(_)) => None,
                        }
                    }
                    None => None,
                },
            };
            return match stdin {
                Some(r) => {
                    // TODO(zgc) -- site E of
                    // `docs/feature-designs/zgc-jit-load-barrier.md` 2.5.1, and
                    // NOT a barrier site, for the same structural reason as
                    // site D: `crate::vm::get_static_shared` has already turned
                    // the static slot into a `Value`, so this arm holds an
                    // `ObjectRef` and not an `&AtomicU64`.
                    //
                    // Where it belongs: inside `get_static_shared`
                    // (`vm/src/vm/vm_object.rs`), which is the read the
                    // interpreter takes too -- so barriering there merges the
                    // two paths instead of duplicating one.
                    //
                    // One extra hazard this arm carries that D does not: 8 Q5
                    // of that document flags statics as the one slot shape that
                    // is not atomic today, so a static may not be CAS-healable
                    // and may need the non-healing barrier variant. That is a
                    // question for the `get_static_shared` change, not for this
                    // call site.
                    ref_load_census::note(ref_load_census::GETSTATIC_SYSTEM_IN);
                    let raw = r.as_ptr() as u64;
                    jit_decode_ref_word(raw, "jit_getstatic/System.in")
                }
                _ => 0,
            };
        }
    }

    let mut val = {
        let _g = gs_prof::CycGuard::new(&gs_prof::CYC_READ);
        crate::vm::get_static_shared(vm, class_id, field_index as usize)
    };
    // Synthetic-`Boolean` repair, shared with `op_getstatic` (i1 wave 5): a
    // `Boolean` without a `<clinit>` reads `TRUE`/`FALSE` back null, and this
    // helper used to hand compiled code that null where the interpreter
    // materialises the constant. Screened by class id inside the callee, so a
    // legitimately null static costs one relaxed load and a compare.
    if matches!(val, Value::Object(None)) {
        if let Some((thread, _guard)) = jit_thread_mut() {
            match crate::runtime::interpreter::repair_boolean_constant_static(
                vm,
                thread,
                class_id,
                field_index as usize,
                val,
            ) {
                Ok(v) => val = v,
                Err(crate::error::MethodCallFailed::ExceptionThrown(exc)) => {
                    set_jit_pending_exception(thread, exc);
                    return i64::MIN;
                }
                // Nothing to throw: keep the null, as before.
                Err(crate::error::MethodCallFailed::InternalError(_)) => {}
            }
        }
    }
    match val {
        Value::Int(i) => i as i64,
        Value::Long(l) => l,
        Value::Float(f) => f.to_bits() as i64,
        Value::Double(d) => d.to_bits() as i64,
        Value::Object(Some(r)) => {
            // TODO(zgc) -- site F of
            // `docs/feature-designs/zgc-jit-load-barrier.md` 2.5.1, and NOT a
            // barrier site. Identical in shape to site E above: the
            // `ObjectRef` was built by `crate::vm::get_static_shared`, so this
            // arm has a word and no slot, and the barrier belongs inside that
            // function (`vm/src/vm/vm_object.rs`) where the interpreter reads
            // the same statics. 8 Q5's non-atomic-static caveat applies here
            // too.
            //
            // Counted so that "the general getstatic object arm ran" is
            // distinguishable from "only the System.in intercept ran" -- the
            // two arms have different remedies and the same silence.
            ref_load_census::note(ref_load_census::GETSTATIC_OBJECT);
            let raw = r.as_ptr() as u64;
            jit_decode_ref_word(raw, "jit_getstatic/object")
        }
        Value::Object(None) => 0,
        _ => 0,
    }
}

/// Where does a `getstatic` actually spend its time?
///
/// The helper has four distinguishable segments and, measured from outside, they
/// were indistinguishable — which is how a lock-free read path that turned out
/// to change nothing got built on an assumption. Each segment is now timed with
/// `rdtsc` under `CRATONVM_DBG_GETSTATIC_PROF=1`; the dump also reports whether
/// the lock-free index is actually being HIT, so an inert lever cannot pass for
/// an ineffective one.
pub mod gs_prof {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub static CALLS: AtomicU64 = AtomicU64::new(0);
    pub static CYC_TOTAL: AtomicU64 = AtomicU64::new(0);
    pub static CYC_BOUNDARY: AtomicU64 = AtomicU64::new(0);
    pub static CYC_INIT: AtomicU64 = AtomicU64::new(0);
    pub static CYC_SYSCHECK: AtomicU64 = AtomicU64::new(0);
    pub static CYC_READ: AtomicU64 = AtomicU64::new(0);

    pub fn enabled() -> bool {
        static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
        cratonvm_jit::memo_flag::memo_bool(&ON, || cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_GETSTATIC_PROF"))
    }

    #[inline]
    #[cfg(target_arch = "x86_64")]
    pub fn now() -> u64 {
        // SAFETY: rdtsc is unprivileged on x86-64.
        unsafe { core::arch::x86_64::_rdtsc() }
    }

    /// The same counter off x86-64, where `_rdtsc` does not exist.
    ///
    /// AArch64's equivalent is the virtual counter `CNTVCT_EL0`, unprivileged
    /// under Linux by default. It ticks at a fixed frequency rather than the
    /// core clock, so these numbers are NOT comparable across architectures --
    /// which is fine, because every consumer uses them as deltas within one
    /// process. Anything else falls back to the monotonic clock so the counters
    /// keep working rather than the crate refusing to build.
    #[inline]
    #[cfg(not(target_arch = "x86_64"))]
    pub fn now() -> u64 {
        #[cfg(target_arch = "aarch64")]
        {
            let cnt: u64;
            // SAFETY: CNTVCT_EL0 is a read-only counter, unprivileged on Linux.
            unsafe { std::arch::asm!("mrs {}, cntvct_el0", out(reg) cnt) };
            cnt
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
        }
    }

    pub struct CycGuard<'a> {
        start: u64,
        sink: &'a AtomicU64,
        armed: bool,
    }

    impl<'a> CycGuard<'a> {
        #[inline]
        pub fn new(sink: &'a AtomicU64) -> Self {
            let armed = enabled();
            Self {
                start: if armed { now() } else { 0 },
                sink,
                armed,
            }
        }
    }

    impl Drop for CycGuard<'_> {
        #[inline]
        fn drop(&mut self) {
            if self.armed {
                self.sink
                    .fetch_add(now().wrapping_sub(self.start), Ordering::Relaxed);
            }
        }
    }

    pub fn dump() {
        if !enabled() {
            return;
        }
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let calls = g(&CALLS).max(1);
        let [site_get, site_put, site_fill] = crate::vm::interp_static_site_counts();
        eprintln!(
            "[GS_PROF] calls={} cyc/call total={:.1} boundary={:.1} init={:.1} \
             syscheck={:.1} read={:.1} | index_hit={} index_miss={} \
             | interp_site get_hit={} put_hit={} fill={}",
            g(&CALLS),
            g(&CYC_TOTAL) as f64 / calls as f64,
            g(&CYC_BOUNDARY) as f64 / calls as f64,
            g(&CYC_INIT) as f64 / calls as f64,
            g(&CYC_SYSCHECK) as f64 / calls as f64,
            g(&CYC_READ) as f64 / calls as f64,
            crate::vm::statics_index_hits(),
            crate::vm::statics_index_misses(),
            site_get,
            site_put,
            site_fill,
        );
    }
}

// JVMS §5.5 / §putstatic: the declaring class must be initialized before
// its static storage is written, exactly like §getstatic (see the long
// comment on `jit_getstatic`'s fix for the full hazard writeup — this is
// the sibling bytecode, same missing check, same shape of bug). Beyond the
// generic "read/write before <clinit> ran" hazard, `putstatic` has its own
// sharper failure mode: `set_static_shared` (`vm/src/vm/vm_object.rs`)
// lazily creates the class's static-field `Vec` on FIRST write, sized from
// `class.fields.len()`, with no awareness of whether `<clinit>` has run.
// If a JIT `putstatic` writes first, a *later* `<clinit>` run (triggered by
// some other, unrelated access to the class) reaches the very same
// `set_static_shared` and unconditionally overwrites that slot with its own
// initializer value — silently discarding the JIT's write. Ordering
// `<clinit>` before the write (mirroring the interpreter's own `putstatic`
// handler, `runtime/interpreter.rs`, which calls
// `ensure_class_initialized_shared` before ever touching the operand
// stack/value) is the only sound fix.
//
// `putstatic` is `void`-returning, so unlike `jit_getstatic` there is no
// legitimate return value that could collide with the `i64::MIN` deopt
// sentinel — but the JIT call site still needs *some* signal to route a
// `<clinit>` failure through the method's exception table instead of
// silently proceeding to the (now-skipped) write. Mirrors the existing
// void-helper convention used by the `invokestatic` arraycopy dispatch call
// (`jit/src/x64.rs`, `emit_post_invoke_exception_check(b'V')` after
// `self.helpers.invoke_dispatch`): the helper always returns `i64`, `0` on
// the ordinary/no-exception path and `i64::MIN` when it stashed a pending
// exception via `set_jit_pending_exception`. The matching codegen change
// (`emit_post_invoke_exception_check(b'V')` after each `putstatic_*` call)
// lives in `jit/src/x64.rs`'s two `0xb3` arms.
//
// Shared by all five `jit_putstatic_*` helpers. Returns `Some(i64::MIN)`
// (the deopt sentinel, after stashing a Java exception) if `<clinit>`
// failed; the caller must return that value immediately without writing
// the field. Returns `None` when initialization already succeeded (or the
// per-thread JIT context isn't available — mirrors `jit_getstatic`'s same
// defensive fallback), meaning the caller should proceed with the write.
//
// `unsafe` purely to match the calling-convention contract of its `pub
// unsafe extern "C"` callers (the five `jit_putstatic_*` helpers, e.g.
// `jit_putstatic_int` just below) — every argument here is a safe
// reference/value already validated at that boundary.
#[inline]
// SAFETY: must run on the JIT-execution thread, which every
// `jit_putstatic_*` caller guarantees — that's the only precondition
// `jit_thread_mut()`'s per-thread context lookup and
// `set_jit_pending_exception` depend on here.
unsafe fn jit_putstatic_class_init_guard(vm: &SharedVm, class_id_raw: i64) -> Option<i64> {
    // Steady state: one relaxed load, the same memo `jit_getstatic` consults
    // first. `ensure_class_initialized_shared` below answers an initialized
    // class too, but only after the `jit_thread_mut` lookup and its own
    // thread-local set probe, on every compiled `putstatic`.
    if class_init_memo::is_initialized(vm, class_id_raw as u32) {
        return None;
    }
    let class_id = ClassId::new(class_id_raw as u32);
    if let Some((thread, _guard)) = jit_thread_mut() {
        if let Err(err) = crate::vm::ensure_class_initialized_shared(vm, thread, class_id) {
            let convert = helper_failure_conversion_on();
            stash_helper_failure(
                vm,
                thread,
                err,
                &format_args!("JIT putstatic class_id {class_id_raw} failed to initialize"),
                convert,
                convert,
            );
            return Some(i64::MIN);
        }
        // Memoize only a CONFIRMED initialization, exactly as `jit_getstatic`
        // does: `Ok` above also answers a class this thread is still
        // initializing.
        let generation = class_init_memo::generation(vm);
        if crate::vm::is_class_initialized_via_manager(vm, class_id) {
            class_init_memo::mark_initialized(vm, class_id_raw as u32, generation);
        }
    }
    None
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_int(
    vm_ptr: i64,
    class_id_raw: i64,
    field_index: i64,
    val: i64,
) -> i64 {
    let f = || jit_putstatic_int_body(vm_ptr, class_id_raw, field_index, val);
    contain("jit_putstatic_int", OnPanic::Throw { vm_ptr }, i64::MIN, f)
}

/// Body of [`jit_putstatic_int`], run under its panic guard.
unsafe fn jit_putstatic_int_body(
    vm_ptr: i64,
    class_id_raw: i64,
    field_index: i64,
    val: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    if vm_ptr == 0 {
        return 0;
    }
    // SAFETY: vm_ptr is non-null and points to a valid SharedVm.
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(sentinel) = jit_putstatic_class_init_guard(vm, class_id_raw) {
        return sentinel;
    }
    let class_id = ClassId::new(class_id_raw as u32);
    crate::vm::set_static_shared(vm, class_id, field_index as usize, Value::Int(val as i32));
    0
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_long(
    vm_ptr: i64,
    class_id_raw: i64,
    field_index: i64,
    val: i64,
) -> i64 {
    let f = || jit_putstatic_long_body(vm_ptr, class_id_raw, field_index, val);
    contain("jit_putstatic_long", OnPanic::Throw { vm_ptr }, i64::MIN, f)
}

/// Body of [`jit_putstatic_long`], run under its panic guard.
unsafe fn jit_putstatic_long_body(
    vm_ptr: i64,
    class_id_raw: i64,
    field_index: i64,
    val: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(sentinel) = jit_putstatic_class_init_guard(vm, class_id_raw) {
        return sentinel;
    }
    let class_id = ClassId::new(class_id_raw as u32);
    crate::vm::set_static_shared(vm, class_id, field_index as usize, Value::Long(val));
    0
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_float(
    vm_ptr: i64,
    class_id_raw: i64,
    field_index: i64,
    val: i64,
) -> i64 {
    let f = || jit_putstatic_float_body(vm_ptr, class_id_raw, field_index, val);
    contain(
        "jit_putstatic_float",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_putstatic_float`], run under its panic guard.
unsafe fn jit_putstatic_float_body(
    vm_ptr: i64,
    class_id_raw: i64,
    field_index: i64,
    val: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(sentinel) = jit_putstatic_class_init_guard(vm, class_id_raw) {
        return sentinel;
    }
    let class_id = ClassId::new(class_id_raw as u32);
    crate::vm::set_static_shared(
        vm,
        class_id,
        field_index as usize,
        Value::Float(f32::from_bits(val as u32)),
    );
    0
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time and refer to a valid static field.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_double(
    vm_ptr: i64,
    class_id_raw: i64,
    field_index: i64,
    val: i64,
) -> i64 {
    let f = || jit_putstatic_double_body(vm_ptr, class_id_raw, field_index, val);
    contain(
        "jit_putstatic_double",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_putstatic_double`], run under its panic guard.
unsafe fn jit_putstatic_double_body(
    vm_ptr: i64,
    class_id_raw: i64,
    field_index: i64,
    val: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(sentinel) = jit_putstatic_class_init_guard(vm, class_id_raw) {
        return sentinel;
    }
    let class_id = ClassId::new(class_id_raw as u32);
    crate::vm::set_static_shared(
        vm,
        class_id,
        field_index as usize,
        Value::Double(f64::from_bits(val as u64)),
    );
    0
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// class_id_raw and field_index were resolved at JIT compile time. val is 0 (null) or a raw
// pointer to a live heap object, converted to Value::Object for storage in the static field table.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_putstatic_object(
    vm_ptr: i64,
    class_id_raw: i64,
    field_index: i64,
    val: i64,
) -> i64 {
    let f = || jit_putstatic_object_body(vm_ptr, class_id_raw, field_index, val);
    contain(
        "jit_putstatic_object",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_putstatic_object`], run under its panic guard.
unsafe fn jit_putstatic_object_body(
    vm_ptr: i64,
    class_id_raw: i64,
    field_index: i64,
    val: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    // The class-init guard may run `<clinit>` — Java code, allocation, a
    // moving collection — while the reference to store exists only as this
    // raw argument. The compiled caller popped it off its operand stack to
    // pass it, so no oop map names it either. Root it in the thread's pin
    // list across the guard and re-read it afterwards, or the static would be
    // written with a from-space address. The thread borrow is released before
    // the guard, which takes its own.
    let pin_idx = if val != 0 {
        jit_thread_mut().map(|(thread, _guard)| {
            let idx = thread.native_pin_roots.len();
            thread
                .native_pin_roots
                .push(ObjectRef::from_raw(val as usize as *mut u8));
            idx
        })
    } else {
        None
    };
    let init_failed = jit_putstatic_class_init_guard(vm, class_id_raw);
    let val = match pin_idx {
        Some(idx) => match jit_thread_mut() {
            Some((thread, _guard)) => {
                let current = thread.native_pin_roots[idx].as_ptr() as usize as i64;
                thread.native_pin_roots.truncate(idx);
                current
            }
            None => val,
        },
        None => val,
    };
    if let Some(sentinel) = init_failed {
        return sentinel;
    }
    let class_id = ClassId::new(class_id_raw as u32);
    // Round-7 fix (CRIT, UAF in JIT): the SATB pre-write barrier on the OLD
    // static value. It used to be taken here, by a second read of the slot;
    // `set_static_shared` has logged the overwritten reference itself, on
    // both its lock-free and its locked path, since the barrier was
    // centralized there, so this helper no longer reads the slot twice.
    let value = if val == 0 {
        Value::Object(None)
    } else {
        Value::Object(Some(ObjectRef::from_raw(val as usize as *mut u8)))
    };
    crate::vm::set_static_shared(vm, class_id, field_index as usize, value);
    0
}

// ---------------------------------------------------------------------------
// Type check helpers
// ---------------------------------------------------------------------------

#[cfg(test)]
mod r21_hash_identity_class_tests {
    //! JIT round 21, lane hash (NATIVE21-1): the VM's answer to the
    //! identity-hash class query (`class_hash_code_identity_verdict`).
    use super::*;
    use crate::config::VmConfig;

    /// Class id 0 (the arm's own exact compare), an id the store does not
    /// hold, and the lambda-proxy / sentinel band all answer `0`, through the
    /// query word the compiler sends.
    #[test]
    fn unprovable_classes_answer_zero() {
        let shared = SharedVm::new(VmConfig::default());
        let ask = |cid: u32| {
            vh_const_well_known_answer(
                &shared,
                cratonvm_jit::ir_lower::vh_const_query(
                    cratonvm_jit::ir_lower::WELL_KNOWN_VH_CONST_IDENTITY_HASH_TAG,
                    cid,
                    0,
                ),
            )
        };
        assert_eq!(ask(0), Some(0));
        assert_eq!(ask(61_999), Some(0));
        assert_eq!(ask(0x8000_0001), Some(0));
        assert_eq!(ask(u32::MAX - 2), Some(0));
    }
}

#[cfg(test)]
mod jr21_hash2_identity_class_table_tests {
    //! JIT round 21, lane hash2 (`CRATONVM_JIT_OBJECT_HASH_CLASS_TABLE`): the
    //! per-VM identity-hash class verdict table (`JitRealm::identity_hash_classes`)
    //! and the well-known query that hands its address to the compiler.
    use super::*;
    use crate::config::VmConfig;
    use cratonvm_jit::ir_lower as jl;

    fn ask(shared: &SharedVm, tag: i64, cid: u32) -> Option<i64> {
        vh_const_well_known_answer(shared, jl::vh_const_query(tag, cid, 0))
    }

    /// The first table query allocates it; the address is stable, per VM,
    /// and starts with the capacity word the compiled test compares against.
    #[test]
    fn the_table_query_allocates_one_stable_table_per_vm() {
        let shared = SharedVm::new(VmConfig::default());
        let table = &shared.jit.identity_hash_classes;
        assert!(!table.is_allocated());
        // Before the table exists, recording is a no-op.
        table.record(5, jl::IDENTITY_HASH_CLASS_PROVEN);
        assert_eq!(table.verdict(5), jl::IDENTITY_HASH_CLASS_UNKNOWN);
        let addr = ask(&shared, jl::WELL_KNOWN_VH_CONST_IDENTITY_HASH_TABLE_TAG, 0).unwrap();
        assert_ne!(addr, 0);
        assert!(table.is_allocated());
        assert_eq!(
            ask(&shared, jl::WELL_KNOWN_VH_CONST_IDENTITY_HASH_TABLE_TAG, 0),
            Some(addr)
        );
        // Cast: the address handed back as the compiler reads it.
        assert_eq!(usize::try_from(addr).unwrap(), table.base_addr());
        // SAFETY: `addr` is the live table's base; its first 8 bytes are the
        // capacity word, written before the address was handed out.
        let cap = unsafe { std::ptr::read_unaligned(table.base_addr() as *const u64) };
        assert_eq!(
            usize::try_from(cap).unwrap(),
            jl::IDENTITY_HASH_CLASS_TABLE_CAPACITY
        );
        let other = SharedVm::new(VmConfig::default());
        let other_addr = ask(&other, jl::WELL_KNOWN_VH_CONST_IDENTITY_HASH_TABLE_TAG, 0).unwrap();
        assert_ne!(other_addr, addr, "one table per VM");
    }

    /// A verdict is filed once and never rewritten; ids past the capacity
    /// have no byte.
    #[test]
    fn a_verdict_is_written_once() {
        let shared = SharedVm::new(VmConfig::default());
        let table = &shared.jit.identity_hash_classes;
        let _ = table.base_addr();
        table.record(42, jl::IDENTITY_HASH_CLASS_PROVEN);
        table.record(42, jl::IDENTITY_HASH_CLASS_REFUSED);
        assert_eq!(table.verdict(42), jl::IDENTITY_HASH_CLASS_PROVEN);
        table.record(43, jl::IDENTITY_HASH_CLASS_REFUSED);
        table.record(43, jl::IDENTITY_HASH_CLASS_PROVEN);
        assert_eq!(table.verdict(43), jl::IDENTITY_HASH_CLASS_REFUSED);
        let past = u32::try_from(jl::IDENTITY_HASH_CLASS_TABLE_CAPACITY).unwrap();
        table.record(past, jl::IDENTITY_HASH_CLASS_PROVEN);
        assert_eq!(table.verdict(past), jl::IDENTITY_HASH_CLASS_UNKNOWN);
        // SAFETY: the byte of class 42 lies inside the live table.
        let byte = unsafe {
            *((table.base_addr() + jl::IDENTITY_HASH_CLASS_TABLE_HEADER + 42) as *const u8)
        };
        assert_eq!(
            byte,
            jl::IDENTITY_HASH_CLASS_PROVEN,
            "the byte compiled code reads"
        );
    }

    /// The identity-class query files nothing for a class the store does not
    /// hold (it may be defined later) and still answers `0`.
    #[test]
    fn an_absent_class_is_not_filed_as_refused() {
        let shared = SharedVm::new(VmConfig::default());
        let _ = shared.jit.identity_hash_classes.base_addr();
        assert_eq!(
            ask(&shared, jl::WELL_KNOWN_VH_CONST_IDENTITY_HASH_TAG, 61_999),
            Some(0)
        );
        assert_eq!(
            shared.jit.identity_hash_classes.verdict(61_999),
            jl::IDENTITY_HASH_CLASS_UNKNOWN
        );
    }
}

#[cfg(test)]
mod jr21_tcarray_array_typecheck_tests {
    //! JIT round 21, lane tcarray: the reference-array component query the
    //! optimizing tier's array arm compiles against
    //! (`WELL_KNOWN_VH_CONST_ARRAY_COMPONENT_TAG`) and the type-check helpers'
    //! array memo (`CRATONVM_JIT_TYPECHECK_ARRAY_MEMO`).
    use super::*;
    use crate::config::VmConfig;
    use cratonvm_jit::ir_lower as jl;
    use std::sync::Arc;

    const ELEM: &str = "cratonvm/test/jr21tcarray/Elem";
    const ELEM_ARRAY: &str = "[Lcratonvm/test/jr21tcarray/Elem;";

    fn ask(shared: &SharedVm, cid: u32) -> Option<i64> {
        vh_const_well_known_answer(
            shared,
            jl::vh_const_query(jl::WELL_KNOWN_VH_CONST_ARRAY_COMPONENT_TAG, cid, 0),
        )
    }

    /// `(component, its array class)`, or `None` where the environment's
    /// policy refuses a synthetic class (`--jdk-only`).
    fn classes(shared: &SharedVm) -> Option<(ClassId, ClassId)> {
        let mut cm = shared.classes.class_manager.write();
        let component = cm.try_ensure_synthetic_class(ELEM, 0).ok()?;
        let array = cm
            .load_array_class_for_loader(ELEM_ARRAY, cratonvm_types::ClassLoaderId::Bootstrap)
            .ok()?;
        Some((component, array))
    }

    #[test]
    fn the_component_query_answers_reference_array_classes_only() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        assert_eq!(ask(&shared, 61_999), Some(0), "an unknown class");
        let Some((component, array)) = classes(&shared) else {
            return;
        };
        assert_eq!(
            ask(&shared, array.as_u32()),
            Some(i64::from(component.as_u32()) + 1)
        );
        assert_eq!(ask(&shared, component.as_u32()), Some(0), "not an array");
    }

    /// A reference array's `true` is memoized under its own key (the length
    /// word's array bit), never under the plain-receiver key of its component
    /// class; a primitive array's verdict is never memoized.
    #[test]
    fn a_reference_array_true_is_memoized_apart_from_its_component() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let Some((component, array)) = classes(&shared) else {
            return;
        };
        let (ptr, len) = cratonvm_jit::intern_typecheck_target(ELEM_ARRAY, Some(array.as_u32()));
        let mut receiver =
            shared
                .mem
                .heap
                .alloc_array(component, cratonvm_types::ArrayElementType::Reference, 1);
        let mut raise = None;
        // SAFETY: `receiver` is a live array of `shared`'s own heap; no
        // compiled code or JIT thread is involved.
        let verdict = unsafe {
            jit_typecheck_memoized(
                &shared,
                component,
                &mut receiver,
                ptr,
                len,
                true,
                &mut raise,
            )
        };
        assert_eq!(verdict, Some(true), "an Elem[] is an Elem[]");
        assert!(raise.is_none());
        let epoch = typecheck_memo_epoch(&shared);
        let vm_key = shared.vm_identity;
        let array_key = (
            vm_key,
            ptr as usize,
            len | JIT_TYPECHECK_ARRAY_MEMO_BIT,
            component.as_u32(),
            true,
        );
        assert_eq!(
            jit_typecheck_answer_cached(array_key, epoch),
            shared.jit.supers_displays.array_cast_memo()
        );
        let plain_key = (vm_key, ptr as usize, len, component.as_u32(), true);
        assert!(
            !jit_typecheck_answer_cached(plain_key, epoch),
            "a plain Elem is no Elem[]"
        );
        // An int[] carries header id 0: refused, and nothing is recorded.
        let mut ints =
            shared
                .mem
                .heap
                .alloc_array(ClassId::new(0), cratonvm_types::ArrayElementType::Int, 1);
        // SAFETY: as above.
        let verdict = unsafe {
            jit_typecheck_memoized(
                &shared,
                ClassId::new(0),
                &mut ints,
                ptr,
                len,
                true,
                &mut raise,
            )
        };
        assert_eq!(verdict, Some(false), "an int[] is no Elem[]");
        let int_key = (
            vm_key,
            ptr as usize,
            len | JIT_TYPECHECK_ARRAY_MEMO_BIT,
            0,
            true,
        );
        assert!(!jit_typecheck_answer_cached(int_key, epoch));
    }
}

/// JIT round 21, lane refs2 (`CRATONVM_JIT_IR_REFERS_TO_OWN_LAYOUT`): the
/// layout producer's referent-only query answers `--jdk-only` VMs without the
/// `ThreadLocal` classes, and `--compatible` VMs not at all.
#[cfg(test)]
mod jr21_refs2_referent_query_tests {
    use super::*;

    fn ask(vm: Option<&SharedVm>) -> (i64, u64) {
        let mut word = [7u64; cratonvm_jit::direct_helpers::REFERENCE_REFERENT_QUERY_WORDS];
        let vm_ptr = vm.map_or(0, |vm| vm as *const SharedVm as i64);
        let got = unsafe {
            jit_thread_local_ir_layout(
                vm_ptr,
                word.as_mut_ptr() as i64,
                cratonvm_jit::direct_helpers::REFERENCE_REFERENT_QUERY_WORDS as i64,
            )
        };
        (got, word[0])
    }

    #[test]
    fn jr21_refs2_jdk_only_answers_the_referent_slot_without_thread_locals() {
        let mut config = crate::config::VmConfig::default();
        config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
        // `JdkOnly` with the synthetic library is a rejected pair.
        config.use_synthetic_jdk = false;
        let vm = SharedVm::new(config);
        // Answered whether or not the `ThreadLocal` classes are loaded (a fresh
        // VM has none): the referent query does not ask `tl_get_layout`.
        assert_eq!(ask(Some(&vm)), (1, 0), "REF_FIELD_REFERENT");
    }

    #[test]
    fn jr21_refs2_compatible_and_no_vm_decline_without_a_write() {
        let vm = SharedVm::new(crate::config::VmConfig::default());
        assert_eq!(ask(Some(&vm)), (0, 7));
        assert_eq!(ask(None), (0, 7));
    }
}

#[cfg(test)]
mod jr21_hashlock_identity_equals_table_tests {
    //! JIT round 21, lane hashlock (HASH2-21-1,
    //! `CRATONVM_JIT_OBJECT_EQUALS_IDENTITY_ARM`): the per-VM equals verdict
    //! table (`JitRealm::identity_equals_classes`), its queries, and its
    //! pairing with the identity-hash class table.
    use super::*;
    use crate::config::VmConfig;
    use cratonvm_jit::ir_lower as jl;

    fn ask(shared: &SharedVm, tag: i64, cid: u32) -> Option<i64> {
        vh_const_well_known_answer(shared, jl::vh_const_query(tag, cid, 0))
    }

    /// The equals table query allocates one stable table per VM, distinct
    /// from the identity-hash one, with the capacity word the shared compiled
    /// test reads.
    #[test]
    fn the_equals_table_query_allocates_one_stable_table_per_vm() {
        let shared = SharedVm::new(VmConfig::default());
        let table = &shared.jit.identity_equals_classes;
        assert!(!table.is_allocated());
        let addr = ask(
            &shared,
            jl::WELL_KNOWN_VH_CONST_IDENTITY_EQUALS_TABLE_TAG,
            0,
        )
        .unwrap();
        assert_ne!(addr, 0);
        assert_eq!(
            ask(
                &shared,
                jl::WELL_KNOWN_VH_CONST_IDENTITY_EQUALS_TABLE_TAG,
                0
            ),
            Some(addr)
        );
        // Cast: the address handed back as the compiler reads it.
        assert_eq!(usize::try_from(addr).unwrap(), table.base_addr());
        // SAFETY: the live table's first 8 bytes are its capacity word.
        let cap = unsafe { std::ptr::read_unaligned(table.base_addr() as *const u64) };
        assert_eq!(
            usize::try_from(cap).unwrap(),
            jl::IDENTITY_HASH_CLASS_TABLE_CAPACITY
        );
        let hash_addr = ask(&shared, jl::WELL_KNOWN_VH_CONST_IDENTITY_HASH_TABLE_TAG, 0).unwrap();
        assert_ne!(hash_addr, addr, "two tables");
    }

    /// Opening the identity-hash table opens the equals table too (switch
    /// on), so a class whose hash is answered inline from then on still had
    /// its equals verdict filed by the leaf; `=0` leaves it closed.
    #[test]
    fn the_hash_table_query_opens_the_equals_table_under_the_switch() {
        let shared = SharedVm::new(VmConfig::default());
        let _ = ask(&shared, jl::WELL_KNOWN_VH_CONST_IDENTITY_HASH_TABLE_TAG, 0);
        assert_eq!(
            shared.jit.identity_equals_classes.is_allocated(),
            jl::object_equals_identity_arm_enabled()
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_OBJECT_EQUALS_IDENTITY_ARM", Some("0"))],
            || {
                let other = SharedVm::new(VmConfig::default());
                let _ = ask(&other, jl::WELL_KNOWN_VH_CONST_IDENTITY_HASH_TABLE_TAG, 0);
                assert!(!other.jit.identity_equals_classes.is_allocated());
            },
        );
    }

    /// `java/lang/Object` itself (id 0, the arm's own exact compare) and a
    /// lambda / sentinel id are refused outright; a class the store does not
    /// hold answers `0` and stays unknown, so a later ask can still prove it.
    #[test]
    fn unprovable_classes_answer_zero_and_absent_ones_stay_unknown() {
        let shared = SharedVm::new(VmConfig::default());
        let table = &shared.jit.identity_equals_classes;
        let _ = table.base_addr();
        assert_eq!(
            class_equals_identity_verdict(&shared, ClassId::new(0)),
            Some(false)
        );
        assert_eq!(
            class_equals_identity_verdict(&shared, ClassId::new(0x8000_0001)),
            Some(false)
        );
        assert_eq!(
            ask(&shared, jl::WELL_KNOWN_VH_CONST_IDENTITY_EQUALS_TAG, 61_999),
            Some(0)
        );
        assert_eq!(table.verdict(61_999), jl::IDENTITY_HASH_CLASS_UNKNOWN);
    }

    /// JIT round 21 (lane tlsub, wave 21; correctness): a bridge `get()Object`
    /// / `set(Object)V` is an override (javac's covariant-override bridge), and
    /// the `set` helper screens `set` overrides before its store and its
    /// private miss arm.
    #[test]
    fn jr21_tlsub_bridges_are_overrides_and_set_screens_its_overrides() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let slice = |from: &str, to: &str| -> String {
            let a = src.find(from).expect(from);
            let b = a + src[a..].find(to).expect(to);
            src[a..b].to_string()
        };
        let kind = slice("\nfn tl_get_compute_kind(", "\nfn tl_class_declares_set(");
        let set = slice("\nfn tl_class_declares_set(", "\nfn tl_get_receiver_kind(");
        for body in [&kind, &set] {
            assert!(
                !body.contains("is_bridge"),
                "a bridge must count as an override"
            );
        }
        let helper = slice(
            "\nunsafe fn jit_thread_local_set_direct_body(",
            "\npub(in crate::jit) unsafe extern \"C\" fn jit_thread_local_get_direct(",
        );
        let screen = helper.find("tl_set_receiver_ok(").expect("the set screen");
        let find = helper.find("tl_find_entry(").expect("the probe");
        let private = helper.find("tl_miss_private_ok(").expect("the miss arm");
        assert!(screen < find && find < private);
        assert_eq!(TL_SET_MEMO_TAG, 1 << 31);
    }
}
