#[allow(unused_imports)]
use super::*;

/// Close every local frame at or above `depth`: the frame a VM-side scope
/// opened at that depth, and any frame native code pushed inside it and never
/// popped.
///
/// gc-common w7-c. JNI frees a native method's local frames when it returns,
/// the ones it pushed and leaked included (HotSpot's native wrapper restores
/// the thread's handle block). Popping ONE frame at scope exit, as the scopes
/// did, popped the leaked frame and left the scope's own frame open: a root
/// for everything in it for the life of the thread, and one more frame depth
/// every later native saw.
pub fn truncate_local_frames(depth: usize) {
    // `try_with`: a scope can end during thread teardown, after this
    // thread's TLS is gone; there is nothing left to close then.
    let closed = JNI_LOCAL_FRAMES
        .try_with(|f| {
            let Ok(mut stack) = f.try_borrow_mut() else {
                return None;
            };
            if stack.len().checked_sub(1) == Some(depth) {
                // The common case -- a dispatch closing its one implicit
                // frame: its storage may be reused (gcd d10/j).
                return stack.pop();
            }
            stack.truncate(depth);
            None
        })
        .ok()
        .flatten();
    if let Some(frame) = closed {
        keep_spare_local_frame(frame);
    }
    forget_explicit_frames_from(depth);
}

/// Drop the explicit-frame records at or above `depth` (their frames are gone).
pub(in crate::native::jni) fn forget_explicit_frames_from(depth: usize) {
    let _ = JNI_EXPLICIT_FRAME_DEPTHS.try_with(|d| {
        if let Ok(mut depths) = d.try_borrow_mut() {
            while depths.last().is_some_and(|&x| x >= depth) {
                depths.pop();
            }
        }
    });
}

/// Is the innermost open local frame one native code pushed with
/// `PushLocalFrame` (as opposed to a frame the VM opened around a call)?
pub(in crate::native::jni) fn top_local_frame_is_explicit() -> bool {
    let depth = local_frame_depth();
    depth > 0
        && JNI_EXPLICIT_FRAME_DEPTHS
            .try_with(|d| d.try_borrow().is_ok_and(|v| v.last() == Some(&(depth - 1))))
            .unwrap_or(false)
}

/// Pop the topmost local frame, returning `result` promoted to the parent
/// frame.
///
/// gc-common w4-g: "promoted" used to be a comment only -- the result was
/// handed back and recorded nowhere, so an object the native built inside a
/// `PushLocalFrame` scope and returned through `PopLocalFrame` (the documented
/// idiom for keeping one result while dropping the scope's temporaries) was
/// named by no root from the pop on: collectable under the native, and not
/// rewritten by a moving collection. It is now recorded in the new innermost
/// frame (`record_local_handle`: a no-op for 0, a global ref, a `jclass`, or
/// when no parent frame is open).
pub fn pop_local_frame(result: JObject) -> JObject {
    // gc-common w6-g (`CRATONVM_JNI_INDIRECT_LOCALS`): an indirect `result`
    // names a slot, most often of the frame being popped, so it is read
    // BEFORE the pop and handed back as a NEW handle in the parent frame.
    if let Some(slot) = decode_indirect_local(result) {
        let raw = read_local_slot(slot);
        let depth = JNI_LOCAL_FRAMES.with(|f| {
            let mut stack = f.borrow_mut();
            let _ = stack.pop();
            stack.len()
        });
        forget_explicit_frames_from(depth);
        return raw.map_or(0, record_local_handle_indirect);
    }
    let depth = JNI_LOCAL_FRAMES.with(|f| {
        let mut stack = f.borrow_mut();
        let _ = stack.pop(); // drop all refs in the top frame
        stack.len()
    });
    forget_explicit_frames_from(depth);
    if indirect_locals_active() {
        return record_local_handle_indirect(result);
    }
    record_local_handle(result);
    result
}

/// Record a local ref in the current top frame.
///
/// GC-correctness (vm-jni-roots #2): previously, if there was no active frame
/// the ref was silently DROPPED ("untracked") and therefore was NOT a GC root —
/// a local ref a native obtained (NewObject, GetObjectField, …) outside any
/// explicit `PushLocalFrame` was invisible to `collect_local_ref_roots` and
/// could be reclaimed (or left dangling under a moving GC) mid-native-call.
/// The native dispatch path now pushes an IMPLICIT top-level local frame for
/// the duration of every JNI native call (see `vm_exec`), but we additionally
/// synthesize a frame here so a `track_local_ref` that races ahead of (or runs
/// without) an enclosing frame still roots the handle rather than leaking it.
pub fn track_local_ref(jobj: JObject) {
    if jobj == 0 {
        return;
    }
    JNI_LOCAL_FRAMES.with(|f| {
        let mut stack = f.borrow_mut();
        if stack.last().is_none() {
            // No active frame: synthesize an implicit top-level frame so the
            // handle is tracked (and thus a GC root) instead of being dropped.
            stack.push(Vec::with_capacity(16));
        }
        // Safe: we just ensured a top frame exists.
        if let Some(top) = stack.last_mut() {
            top.push(jobj);
        }
    });
}

/// Remove ONE local ref from the current top frame (for DeleteLocalRef).
///
/// gc-common w4-g: this removed EVERY entry equal to `jobj`. A local ref is the
/// object's raw address, so two local refs to one object -- a native that read
/// the same field twice, or got the same object back from two up-calls -- are
/// two equal entries, and deleting one unrooted the other while the native
/// still held it. Only the most recent matching entry goes now.
pub fn delete_local_ref(jobj: JObject) {
    if jobj == 0 {
        return;
    }
    // gc-common w6-g: an indirect handle clears exactly its own slot. With
    // indirect handles live, no entry may be REMOVED from a frame (that would
    // shift every later slot's index under the handles naming them), so the
    // raw arm below clears too.
    if let Some((depth, idx)) = decode_indirect_local(jobj) {
        JNI_LOCAL_FRAMES.with(|f| {
            if let Some(frame) = f.borrow_mut().get_mut(depth) {
                clear_local_slot(frame, idx);
            }
        });
        return;
    }
    let indirect = indirect_locals_active();
    JNI_LOCAL_FRAMES.with(|f| {
        let mut stack = f.borrow_mut();
        if let Some(top) = stack.last_mut() {
            if let Some(i) = top.iter().rposition(|&r| r == jobj) {
                if indirect {
                    clear_local_slot(top, i);
                } else {
                    top.remove(i);
                }
            }
        }
    });
}

/// Zero `frame[idx]` (a deleted local ref), then drop every trailing zeroed
/// slot, which makes the common `New*` + `DeleteLocalRef` loop reuse its slot
/// instead of growing the frame by one word per iteration. Only TRAILING
/// slots go: removing an interior one would move the slots after it. A
/// non-LIFO delete leaves a zero hole until the frame is popped (the roots
/// and remap walks already skip 0).
pub(in crate::native::jni) fn clear_local_slot(frame: &mut Vec<JObject>, idx: usize) {
    if let Some(v) = frame.get_mut(idx) {
        *v = 0;
    }
    while frame.last() == Some(&0) {
        frame.pop();
    }
}

/// A borrowed local table declines the precise lazy-claim proof.
pub(crate) fn local_ref_frames_are_empty() -> bool {
    JNI_LOCAL_FRAMES
        .try_with(|frames| {
            frames
                .try_borrow()
                .is_ok_and(|stack| stack.iter().all(|frame| frame.is_empty()))
        })
        .unwrap_or(false)
}

/// Collect every active JNI **local** reference held by THIS thread into `out`
/// for GC root scanning.
///
/// BUG FIX (vm-jni-roots #1): the per-thread `JNI_LOCAL_FRAMES` stack holds
/// local-ref `JObject` handles, which for local refs are raw heap pointers
/// (bit 0 = 0). Previously only `JniGlobalRefs::collect_roots` was folded into
/// the root set (see `roots.rs`), so a heap object reachable ONLY through a JNI
/// local ref could be reclaimed mid-native-call (or, under a moving GC, left as
/// a stale from-space pointer). This mirrors `JniGlobalRefs::collect_roots`,
/// but for the thread-local local-frame stack. Must be called on each thread
/// that may hold local refs (the per-thread `roots::collect_roots`).
///
/// Global refs (bit 0 = 1) never appear in `JNI_LOCAL_FRAMES`, but we defend
/// against a tagged value sneaking in by skipping any handle with bit 0 set
/// (those are rooted separately via `JniGlobalRefs`).
pub fn collect_local_ref_roots(out: &mut Vec<ObjectRef>) {
    JNI_LOCAL_FRAMES.with(|f| {
        for frame in f.borrow().iter() {
            for &handle in frame.iter() {
                // gci i9/ds: the same filters as `collect_local_ref_roots_since`,
                // `for_each_live_local_ref` and `push_frozen_peer_local_refs`: a
                // `jclass` tag or an indirect-local tag is not a heap address
                // (`record_local_handle` stores neither today, so this changes
                // no root set; it keeps a tagged word from ever becoming an
                // `ObjectRef` here).
                if handle == 0
                    || handle & 1 == 1
                    || is_jclass_handle(handle)
                    || is_indirect_local_tag(handle)
                {
                    continue;
                }
                // Safety: a local ref is a raw, non-null heap pointer; it was a
                // live ObjectRef when pushed and is kept live precisely by being
                // reported here.
                out.push(unsafe { ObjectRef::from_raw(handle as *mut u8) });
            }
        }
    });
}

/// gcd d10/j: the extent of this thread's local frames at one instant -- how
/// many frames were open and how many slots the innermost one had -- so that
/// [`collect_local_ref_roots_since`] can name exactly the locals minted after
/// it (a JNIEnv function only pushes onto the innermost frame, or opens one).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(in crate::native::jni) struct LocalFramesMark {
    pub(in crate::native::jni) depth: usize,
    pub(in crate::native::jni) top_len: usize,
}

/// This thread's [`LocalFramesMark`] now. `Default` (everything counts as
/// new) when the table is unreadable.
pub(in crate::native::jni) fn local_frames_mark() -> LocalFramesMark {
    JNI_LOCAL_FRAMES
        .try_with(|f| {
            f.try_borrow()
                .map_or(LocalFramesMark::default(), |stack| LocalFramesMark {
                    depth: stack.len(),
                    top_len: stack.last().map_or(0, Vec::len),
                })
        })
        .unwrap_or_default()
}

/// [`collect_local_ref_roots`] for the locals minted since `mark`: every entry
/// of a frame opened after it, and the entries of the frame that was innermost
/// at `mark` from its then-length on. Frames below are skipped (their entries
/// were published before). Same filters as `collect_local_ref_roots`.
pub(in crate::native::jni) fn collect_local_ref_roots_since(mark: LocalFramesMark, out: &mut Vec<ObjectRef>) {
    let _ = JNI_LOCAL_FRAMES.try_with(|f| {
        let Ok(stack) = f.try_borrow() else {
            return;
        };
        for (index, frame) in stack.iter().enumerate() {
            let from = match (index + 1).cmp(&mark.depth) {
                std::cmp::Ordering::Less => continue,
                std::cmp::Ordering::Equal => mark.top_len,
                std::cmp::Ordering::Greater => 0,
            };
            for &handle in frame.iter().skip(from) {
                if handle == 0
                    || handle & 1 == 1
                    || is_jclass_handle(handle)
                    || is_indirect_local_tag(handle)
                {
                    continue;
                }
                // Safety: as in `collect_local_ref_roots`.
                out.push(unsafe { ObjectRef::from_raw(handle as *mut u8) });
            }
        }
    });
}

/// gcd d4/k (for lane m's JNI-local-aware pin in the pinned young copy): call
/// `f` with every live JNI local reference held in THIS thread's local frames,
/// innermost frame last, without allocating. Read-only; the same entries
/// [`collect_local_ref_roots`] roots (deleted slots, global refs, `jclass`
/// tags and indirect tags skipped). Thread-local storage, so it answers only
/// for the calling thread: a peer's locals reach a collection through its
/// deposited root snapshot. A no-op during TLS teardown or while the frame
/// table is borrowed (never, outside a local-frame walk).
///
/// Used by the raw-local pin publication as well as root-walk tests.
pub(crate) fn for_each_live_local_ref(mut f: impl FnMut(ObjectRef)) {
    let _ = JNI_LOCAL_FRAMES.try_with(|frames| {
        let Ok(stack) = frames.try_borrow() else {
            return;
        };
        for frame in stack.iter() {
            for &handle in frame.iter() {
                if handle == 0
                    || handle & 1 == 1
                    || is_jclass_handle(handle)
                    || is_indirect_local_tag(handle)
                {
                    continue;
                }
                // Safety: a stored local is a raw, non-null, 8-aligned heap
                // address, kept live by this very table (see
                // `collect_local_ref_roots`).
                f(unsafe { ObjectRef::from_raw(handle as *mut u8) });
            }
        }
    });
}

/// Raw local handles are addresses copied into C variables. Publish them after
/// the JIT pin replacement so every collector keeps their storage in place.
/// The owning thread walks all open frames, including the attachment frame.
pub(crate) fn publish_raw_local_pins() {
    // Frames store object addresses even for indirect handles. Use the
    // attachment-frame marker instead of borrowing ForeignThreadBox: an
    // owner's snapshot deposit may already borrow that box mutably.
    let switches = jni_switches();
    let attached = FOREIGN_ATTACH_FRAME
        .try_with(|c| c.get().is_some())
        .unwrap_or(false);
    let foreign_transitions = foreign_transitions_test_override()
        .unwrap_or(switches.foreign_transitions);
    let indirect = indirect_locals_active_with(&switches)
        && (!attached || foreign_transitions);
    if indirect && raw_local_escapes() == 0 {
        return;
    }
    for_each_live_local_ref(|obj| {
        cratonvm_gc::gc_quiescence::add_pinned_jit_root(obj.as_ptr() as usize);
    });
}

/// gcd d4/k (lane m's companion question): may native code on THIS thread
/// hold a RAW copy of one of its locals, i.e. an address a move would leave
/// stale in the native's own variables? `true` unless every local handed out
/// here was an indirect handle ([`locals_are_indirect_here`]) and none escaped
/// raw ([`raw_local_escapes`], monotonic per thread, so the answer is
/// conservative). With the default flags it is `true`: pin rather than move.
pub(crate) fn local_refs_may_be_held_raw() -> bool {
    !locals_are_indirect_here() || raw_local_escapes() != 0
}

/// Apply a GC pointer map to every active JNI local reference on THIS thread,
/// rewriting moved-object handles in place.
///
/// BUG FIX (vm-jni-roots #1): after a moving GC relocates objects, the raw
/// pointers stored in `JNI_LOCAL_FRAMES` would dangle (point at from-space).
/// This mirrors `JniGlobalRefs::update_after_gc` for the local-frame stack:
/// for each stored local-ref handle whose address appears in `pointer_map`, we
/// overwrite it with the relocated address so the native code's local jobject
/// continues to resolve to the live object. Must be called on each thread that
/// may hold local refs, after the heap has been compacted.
pub fn update_local_refs_after_gc(pointer_map: &cratonvm_types::PointerMap) {
    JNI_LOCAL_FRAMES.with(|f| {
        for frame in f.borrow_mut().iter_mut() {
            for handle in frame.iter_mut() {
                if *handle == 0 || *handle & 1 == 1 {
                    continue;
                }
                let old_addr = *handle as usize;
                if let Some(&new_addr) = pointer_map.get(&old_addr) {
                    *handle = new_addr as JObject;
                }
            }
        }
    });
}

/// gci i9/jw (`gci-i8orch-takeover-promotes-jna-upcall-wrong-result-FIXED-20261017`):
/// publish THIS OS thread's [`JNI_LOCAL_FRAMES`] table address, with its OS
/// tid, into `state` (the `GcBlockState` of the `JvmThread` this thread is
/// installing as its JNI thread), so a take-over pause that freezes this
/// thread in compiled code can root its JNI locals
/// ([`push_frozen_peer_local_refs`]). Two relaxed loads when the pair is
/// already published; the tid is read from the OS once per thread.
pub(crate) fn publish_local_frames_owner(state: &crate::threading::jvm_thread::GcBlockState) {
    let Ok(addr) = JNI_LOCAL_FRAMES.try_with(|f| {
        f as *const std::cell::RefCell<Vec<Vec<JObject>>> as usize
    }) else {
        return;
    };
    let os_tid = JNI_NATIVE_CALL
        .try_with(|t| {
            if t.own_os_tid.get() == 0 {
                t.own_os_tid
                    .set(crate::threading::thread_registry::current_os_tid());
            }
            t.own_os_tid.get()
        })
        .unwrap_or(0);
    state.publish_jni_local_frames(os_tid, addr);
}

/// gci i9/jw: add to `out` every live JNI local reference of a peer the
/// calling pause holds FROZEN, read from the table `state` published
/// ([`publish_local_frames_owner`]); the number added.
///
/// # The hole this closes
///
/// A thread frozen by the take-over (`Rip` in compiled code) reaches the
/// collection through its interpreter frames, its thread fields, its shadow
/// stack and its machine stack (`stw_take_over_and_wait`), and through the
/// root snapshot of its LAST deposit. Its `JNI_LOCAL_FRAMES` table is
/// thread-local, so no other thread's `collect_local_ref_roots` reads it. With
/// indirect locals (`CRATONVM_JNI_INDIRECT_LOCALS`, default ON since
/// 2026-10-10) the native's own stack holds a tagged `(depth, slot)` handle,
/// not the address, so the machine-stack scan no longer finds it either. A
/// local minted after the thread's last deposit -- JNA's callback dispatch
/// builds the up-call's `Object[]` with `NewObjectArray`, then runs Java
/// (`NewObject` -> `Pointer.<init>`) before `CallObjectMethod` hands it to
/// Java -- was therefore in no root set while the thread sat frozen in that
/// Java: the non-moving sweep zeroed it (the comparator saw an `Object[0]`:
/// `IllegalArgumentException: wrong number of arguments: 0 expected: 2`), and
/// a take-over that promotes (`CRATONVM_GEN_TAKEOVER_PROMOTES`) could move it
/// under the frozen table.
///
/// Only ADDS roots: they join `xt_roots`, which pins them for the cycle (the
/// non-moving sweep on Generational, `pin_frozen_peer_roots_for_g1` on G1),
/// so the frozen table needs no remap.
///
/// Sound because the table is read only when its publisher's OS tid is in
/// `frozen_os_tids` -- that thread cannot run until the pause resumes it, and
/// it is alive, so its thread-local is too -- and through
/// `try_borrow_unguarded`, which writes nothing to the owner's `RefCell` and
/// refuses a table the owner was mutating when it was frozen (compiled code
/// never touches it, so that does not happen; refusing only keeps the old
/// root set).
pub(crate) fn push_frozen_peer_local_refs(
    state: &crate::threading::jvm_thread::GcBlockState,
    frozen_os_tids: &[u32],
    heap: &crate::memory::VmHeap,
    out: &mut Vec<ObjectRef>,
) -> usize {
    let Some((os_tid, addr)) = state.published_jni_local_frames() else {
        return 0;
    };
    if !frozen_os_tids.contains(&os_tid) {
        return 0;
    }
    let before = out.len();
    // SAFETY: `addr` is the `JNI_LOCAL_FRAMES` cell of the live OS thread
    // `os_tid` (it published both from its own TLS), which the caller holds
    // frozen for the whole call: see the doc above.
    let cell = unsafe { &*(addr as *const std::cell::RefCell<Vec<Vec<JObject>>>) };
    // SAFETY: the owner cannot run, so no borrow of it can begin or end while
    // this shared view lives; a borrow it held when frozen is refused.
    let Ok(stack) = (unsafe { cell.try_borrow_unguarded() }) else {
        return 0;
    };
    for frame in stack.iter() {
        for &handle in frame.iter() {
            if handle == 0
                || handle & 1 == 1
                || is_jclass_handle(handle)
                || is_indirect_local_tag(handle)
            {
                continue;
            }
            // Screened like a frozen peer's operand-stack word: only a heap
            // address becomes a root.
            if heap.is_heap_addr(handle as usize).is_some() {
                // SAFETY: a stored local is a raw heap address (screened).
                out.push(unsafe { ObjectRef::from_raw(handle as *mut u8) });
            }
        }
    }
    out.len() - before
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

/// The raw handle of `obj`: a pure conversion, recorded nowhere. Use it to
/// COMPARE or to key a handle; a local ref HANDED TO native code must come from
/// [`new_local_handle`], or the native holds an object no root names.
pub fn obj_to_jobject(obj: ObjectRef) -> JObject {
    obj.as_ptr() as u64
}

/// A LOCAL ref handed to native code (a JNI function's result, a native
/// method's receiver or argument): the raw handle, recorded in this thread's
/// innermost open local frame.
///
/// gc-common w3-g. `JniImplicitFrameGuard`'s doc (vm_exec) says every local
/// ref a native obtains "is tracked via `track_local_ref`" and is therefore a
/// GC root (`collect_local_ref_roots`) and remapped
/// (`update_local_refs_after_gc`) for the duration of the call. Only
/// `NewLocalRef` actually called it: `NewStringUTF`, `NewObject`/`AllocObject`,
/// `New<Type>Array`, `Get{Object,StaticObject}Field`,
/// `GetObjectArrayElement` and every `Call*ObjectMethod*` returned a bare
/// address that no root named. A native that created a string and then made an
/// allocating up-call before using it could have that string collected under
/// it, on every backend. Recording the handle here closes the LIVENESS half;
/// the native's own copy of the address is still not rewritten by a moving
/// collection (`docs/feature-designs/real-jdk-mode.md`).
///
/// Recorded only when a frame is OPEN (a native dispatch's implicit frame or
/// an explicit `PushLocalFrame`). A foreign-attached thread calling JNI
/// functions outside any native method has no frame to scope the ref to; the
/// handle is returned unrecorded there, exactly as before, rather than
/// synthesizing a frame nothing would ever pop (a root leak for the life of
/// the thread -- `track_local_ref` does synthesize one, for `NewLocalRef`).
/// With `CRATONVM_JNI_FOREIGN_TRANSITIONS` on (gc-common w10-c) such a thread
/// has its attach-level frame, which detach pops, and the handle is recorded
/// there.
///
/// Cost: one TLS access and a `Vec` push per local ref handed out; the frame
/// is dropped wholesale when the native returns.
pub fn new_local_handle(obj: ObjectRef) -> JObject {
    let h = obj_to_jobject(obj);
    if indirect_locals_active() {
        return record_local_handle_indirect(h);
    }
    record_local_handle(h);
    h
}

// ---------------------------------------------------------------------------
// Indirect local handles (`CRATONVM_JNI_INDIRECT_LOCALS`, default ON since 2026-10-10)
// ---------------------------------------------------------------------------
//
// gc-common w6-g, `docs/feature-designs/real-jdk-mode.md`.
// With the flag on, a local ref handed to native code inside an open frame is
// not the object's address but the address of nothing: a tagged `(frame
// depth, slot)` pair naming the entry of `JNI_LOCAL_FRAMES` that holds the
// address. The table is already a root and already rewritten by every moving
// collection (`collect_local_ref_roots`, `update_local_refs_after_gc`), so a
// native that keeps its `jobject` across an up-call that lets a collection
// move the object now reads the object's CURRENT address at its next use
// (`jobject_to_obj`), like HotSpot's handle-block locals.
//
// What stays raw, exactly as with the flag off: a handle created with no open
// frame (nothing would pop a synthesized one), every handle on a
// foreign-attached thread (its per-call frame is popped at the end of EACH
// JNI call, while its natives keep locals across calls) unless
// `CRATONVM_JNI_FOREIGN_TRANSITIONS` gives it an attach-level frame (gc-common
// w10-c), and a slot past the encodable range. Decoding accepts both forms.
//
// Behaviour a JNI library can see (the flag defaulted on, 2026-10-10, after
// netty-tcnative, JNA, lz4-java and zstd-jni ran clean with it:
// `GcJniLibrariesProbe.java`; `=0` restores raw addresses):
// * a local ref is valid only on its own thread and only until its frame is
//   popped, as the JNI spec says. A library that passes a local to another
//   thread, or keeps one past its native's return (both undefined behaviour,
//   and both "work" with a raw address until a collection moves the object),
//   now resolves NULL or another slot's object;
// * a local smuggled through a `jlong` return is no longer an object address,
//   so the `J`-return mint chokepoint (`smuggled_longs`) does not register it.
// Frame reuse (a stale `(depth, slot)` naming a later frame's entry) is the
// same undefined behaviour HotSpot has.

/// The tag an indirect local handle carries in bits 48..=63. Neither a
/// user-space address (canonical user addresses are below `1 << 47`) nor a
/// `jclass` (`JCLASS_TAG`, `0x7F51` in the same bits) can carry it.
pub(in crate::native::jni) const INDIRECT_LOCAL_TAG: JObject = 0x7F52 << 48;
/// Selects [`INDIRECT_LOCAL_TAG`]'s bits.
pub(in crate::native::jni) const INDIRECT_LOCAL_TAG_MASK: JObject = 0xFFFF << 48;
/// The largest frame depth an indirect handle can name (bits 32..=47).
pub(in crate::native::jni) const INDIRECT_LOCAL_MAX_DEPTH: usize = 0xFFFF;
/// The largest slot index an indirect handle can name (bits 1..=31; bit 0 is
/// clear so the handle never reads as a global ref).
pub(in crate::native::jni) const INDIRECT_LOCAL_MAX_SLOT: usize = (1 << 31) - 1;

/// `CRATONVM_JNI_INDIRECT_LOCALS`, latched like every flag. Default ON since
/// the gc JNI retirement (2026-10-10); `=0` (or any off word) is the kill
/// switch, and takes the foreign transitions with it ([`jni_switches`]).
pub(in crate::native::jni) fn indirect_locals_active() -> bool {
    if let Some(on) = indirect_locals_test_override() {
        return on;
    }
    jni_switches().indirect_locals
}

/// The value rule of [`indirect_locals_active`]: unset, empty, or an off word
/// (`0` / `false` / `off` / `no`) is OFF; anything else (the grouped
/// `CRATONVM_GC=jni-indirect-locals` spelling included) is ON.
///
/// gci i1/jn: the off words are matched case-insensitively
/// (`cratonvm_types::flags::value_is_off`, the rule of
/// `runtime_flag_default_on`), so `=OFF` / `=False` kill every package
/// switch read through [`jni_switch_value`] as they kill any other default-on
/// switch; they used to read as ON here.
pub(in crate::native::jni) fn indirect_locals_from(value: Option<&str>) -> bool {
    match value.map(str::trim) {
        None | Some("") => false,
        Some(v) => !cratonvm_types::flags::value_is_off(v),
    }
}

/// Encode `(depth, slot)` as an indirect local handle, if both fit.
pub(in crate::native::jni) fn encode_indirect_local(depth: usize, slot: usize) -> Option<JObject> {
    (depth <= INDIRECT_LOCAL_MAX_DEPTH && slot <= INDIRECT_LOCAL_MAX_SLOT)
        .then(|| INDIRECT_LOCAL_TAG | ((depth as JObject) << 32) | ((slot as JObject) << 1))
}

/// Does `h` carry the indirect-local encoding (whether or not the flag is on)?
pub(in crate::native::jni) fn is_indirect_local_tag(h: JObject) -> bool {
    h & INDIRECT_LOCAL_TAG_MASK == INDIRECT_LOCAL_TAG && h & 1 == 0
}

/// The `(depth, slot)` an indirect local handle names. `None` for every other
/// handle, and for every handle while the flag is off (a forged tagged value
/// then takes the raw path and fails its heap check, as before).
pub(in crate::native::jni) fn decode_indirect_local(h: JObject) -> Option<(usize, usize)> {
    if !is_indirect_local_tag(h) || !indirect_locals_active() {
        return None;
    }
    Some((
        ((h >> 32) & 0xFFFF) as usize,
        ((h & 0xFFFF_FFFF) >> 1) as usize,
    ))
}

/// The raw address currently in local slot `(depth, slot)`, if the slot
/// exists on this thread and was not deleted.
pub(in crate::native::jni) fn read_local_slot((depth, slot): (usize, usize)) -> Option<JObject> {
    JNI_LOCAL_FRAMES.with(|f| {
        let stack = f.try_borrow().ok()?;
        stack.get(depth)?.get(slot).copied().filter(|&v| v != 0)
    })
}

/// Record the raw local `h` in the innermost open frame and return the handle
/// to give native code: an indirect handle naming that slot when the flag is
/// on, the thread is not foreign-attached and the slot is encodable;
/// otherwise `h` itself (recorded when a frame is open, exactly as
/// [`record_local_handle`] does). A global ref, a `jclass`, an already
/// indirect handle and 0 are returned unchanged and not recorded.
pub(in crate::native::jni) fn record_local_handle_indirect(h: JObject) -> JObject {
    if h == 0 || h & 1 == 1 || is_jclass_handle(h) || is_indirect_local_tag(h) {
        return h;
    }
    // A foreign-attached thread stays raw unless its attach-level locals live
    // in a frame nothing pops before detach (gc-common w10-c,
    // `CRATONVM_JNI_FOREIGN_TRANSITIONS`): then a slot names them for as long
    // as the native may hold them, as on any other thread.
    record_local_handle_indirect_encoded(h, locals_are_indirect_here())
}

/// gcj j2/jn2 (cut 1, `CRATONVM_JNI_SWITCHES_ONCE_PER_CALL`):
/// [`record_local_handle_indirect`] with [`locals_are_indirect_here`]'s answer
/// `encode` already in hand (the dispatch resolves it once per call).
pub(in crate::native::jni) fn record_local_handle_indirect_encoded(h: JObject, encode: bool) -> JObject {
    if h == 0 || h & 1 == 1 || is_jclass_handle(h) || is_indirect_local_tag(h) {
        return h;
    }
    let handed = JNI_LOCAL_FRAMES.with(|f| {
        let Ok(mut stack) = f.try_borrow_mut() else {
            return h;
        };
        let Some(depth) = stack.len().checked_sub(1) else {
            return h;
        };
        let top = &mut stack[depth];
        let slot = top.len();
        top.push(h);
        if encode {
            encode_indirect_local(depth, slot).unwrap_or(h)
        } else {
            h
        }
    });
    // gcd d3/k: a raw handle where an indirect one was due -- no open frame,
    // or a slot past the encodable range -- is one no moving collection may
    // outlive while the native holds it (`JniNativeCall`).
    if encode && handed == h {
        note_raw_local_escape();
    }
    handed
}

/// Record an existing raw local handle (bit 0 clear, not a `jclass` tag) in the
/// innermost open local frame; see [`new_local_handle`]. A no-op with no open
/// frame, for a global ref / `jclass` handle, and if the frame stack is already
/// borrowed on this thread (never expected: no JNI function runs inside a
/// local-frame walk).
pub(in crate::native::jni) fn record_local_handle(h: JObject) {
    if h == 0 || h & 1 == 1 || is_jclass_handle(h) {
        return;
    }
    JNI_LOCAL_FRAMES.with(|f| {
        if let Ok(mut stack) = f.try_borrow_mut() {
            if let Some(top) = stack.last_mut() {
                top.push(h);
            }
        }
    });
}

/// gcj j2/jn2 (cut 1, `CRATONVM_JNI_SWITCHES_ONCE_PER_CALL`):
/// [`new_local_handle`] under the dispatch's resolved rule `(indirect,
/// encode)` ([`record_local_handle_with`]), or as before with `None`.
#[inline]
pub(in crate::native::jni) fn new_local_handle_ruled(obj: ObjectRef, rule: Option<(bool, bool)>) -> JObject {
    match rule {
        Some((indirect, encode)) => record_local_handle_with(obj_to_jobject(obj), indirect, encode),
        None => new_local_handle(obj),
    }
}

/// gcj j2/jn2 (cut 1, `CRATONVM_JNI_SWITCHES_ONCE_PER_CALL`): what
/// [`new_local_handle`] does with the handle `h` of an object, given
/// [`indirect_locals_active`]'s answer `indirect` and
/// [`locals_are_indirect_here`]'s answer `encode`, both resolved once by the
/// dispatch.
pub(in crate::native::jni) fn record_local_handle_with(h: JObject, indirect: bool, encode: bool) -> JObject {
    if indirect {
        record_local_handle_indirect_encoded(h, encode)
    } else {
        record_local_handle(h);
        h
    }
}

/// Where a handle [`new_local_handle`] JUST recorded lives: `(frame depth,
/// index)` of the innermost frame's last entry, when that entry is `h`.
/// `None` when nothing was recorded (no open frame).
pub(in crate::native::jni) fn recorded_local_slot_of(h: JObject) -> Option<(usize, usize)> {
    JNI_LOCAL_FRAMES.with(|f| {
        let stack = f.try_borrow().ok()?;
        let depth = stack.len().checked_sub(1)?;
        let top = &stack[depth];
        let idx = top.len().checked_sub(1)?;
        (top[idx] == h).then_some((depth, idx))
    })
}

/// The CURRENT handle in a slot from [`recorded_local_slot_of`] -- the
/// original address rewritten by every `update_local_refs_after_gc` since.
/// `None` (the caller keeps its own copy) unless the frame stack is back at
/// exactly that depth with the slot still present and non-null: a nested
/// native that leaked or over-popped a frame must not make this read someone
/// else's entry.
pub(in crate::native::jni) fn recorded_local_at(slot: (usize, usize)) -> Option<JObject> {
    JNI_LOCAL_FRAMES.with(|f| {
        let stack = f.try_borrow().ok()?;
        if stack.len() != slot.0 + 1 {
            return None;
        }
        stack[slot.0].get(slot.1).copied().filter(|&v| v != 0)
    })
}

/// Resolve a `JObject` to the underlying `ObjectRef`.
///
/// Two kinds of `JObject`:
///  - **Local ref** (bit 0 = 0): direct raw pointer to a heap object.
///  - **Global ref** (bit 0 = 1): tagged pointer to a `Box<ObjectRef>` in
///    `JniGlobalRefs`; dereference to get the actual ObjectRef.
///
/// For global refs, this function acquires the `jni_global_refs` Mutex via the
/// thread-local `JNI_SHARED_VM` to validate the handle is still alive before
/// dereferencing. This prevents a use-after-free race where a concurrent
/// `remove()` could deallocate the `Box` while we read it.
pub fn jobject_to_obj(jobj: JObject) -> Option<ObjectRef> {
    if jobj == 0 {
        None
    } else if is_jclass_handle(jobj) {
        // Tested before the bit-0 split: an odd `ClassId` would otherwise take
        // the global-ref branch and miss, an even one fail the heap check.
        jclass_handle_mirror(jobj)
    } else if jobj & 1 == 1 {
        // Global ref: resolve through the locked table to prevent
        // use-after-free if another thread concurrently calls remove().
        JNI_SHARED_VM.with(|c| {
            let borrow = c.borrow();
            match borrow.as_ref() {
                Some(shared) => {
                    let (obj, weak) = {
                        let refs = shared.natives.jni_global_refs.lock();
                        let obj = refs.resolve(jobj);
                        (obj, obj.is_some() && refs.is_weak(jobj))
                    };
                    // gc-common w8-c: resolving a WEAK global is a keep-alive,
                    // exactly as `Reference.get()` is
                    // (`NativeContextImpl::gc_reference_keep_alive`): a weak
                    // global is not a root, so a concurrent mark may not have
                    // reached its referent, and a native that stores what it
                    // resolved into an already-marked object would otherwise
                    // have it freed by that cycle. The keep-alive is inert
                    // (one load) when no cycle is marking. Outside the table
                    // lock: the barrier may flush into the collector's queue.
                    // zt t23/zb8: `referent_keep_alive` (SATB, or under the
                    // ZGC color flip the load barrier's mark step).
                    if weak {
                        if let Some(o) = obj {
                            shared.mem.heap.referent_keep_alive(Value::Object(Some(o)));
                        }
                    }
                    obj
                }
                None => {
                    tracing::warn!(
                        "jobject_to_obj: global ref {jobj:#x} resolved outside JNI context"
                    );
                    None
                }
            }
        })
    } else {
        // gc-common w6-g: an INDIRECT local handle (`CRATONVM_JNI_INDIRECT_LOCALS`)
        // names a slot of this thread's local frames; the address there is the
        // object's current one (the slot is a root and is remapped by every
        // moving collection). It then takes the same validation as a raw
        // handle. A deleted or popped slot is NULL.
        let jobj = match decode_indirect_local(jobj) {
            Some(slot) => read_local_slot(slot)?,
            None => jobj,
        };
        // Local ref: raw heap pointer.
        //
        // SECURITY FIX (V3): Previously this branch blindly reconstructed an
        // ObjectRef from the caller-supplied pointer with no validation, while
        // the global-ref branch above is protected by a locked table lookup. A
        // forged native pointer would become a wild read/write, and a local ref
        // held across a GC safepoint could be a stale from-space pointer under
        // the moving/generational collector. Mirror the global-ref path's
        // defensive posture: validate the address against the live heap (the
        // same `heap.is_heap_addr` check the GC root scanners use) and return
        // `None` on failure. `is_heap_addr` returns a freshly reconstructed
        // ObjectRef for a confirmed-live, aligned heap address, so we use its
        // result directly instead of an unchecked `from_raw`.
        //
        // The heap is reached through the same thread-local `JNI_SHARED_VM`
        // context the global-ref path uses (`SharedVm::heap`).
        //
        // FOLLOW-UP: the long-term fix is a full per-thread JNI local-handle
        // table (indirection handles validated on every access, like HotSpot's
        // JNIHandleBlock) so a local jobject can never be a raw heap pointer at
        // all. This address-validity gate is the minimum viable mitigation; it
        // does not catch a forged pointer that happens to land on a live
        // object, which only the handle table would fully prevent.
        JNI_SHARED_VM.with(|c| {
            let borrow = c.borrow();
            match borrow.as_ref() {
                Some(shared) => {
                    note_suspect_local_ref(shared, jobj as usize);
                    shared.mem.heap.is_heap_addr(jobj as usize)
                }
                None => {
                    tracing::warn!(
                        "jobject_to_obj: local ref {jobj:#x} resolved outside JNI context"
                    );
                    None
                }
            }
        })
    }
}

/// `CRATONVM_DBG=jni-localref` — report a LOCAL `jobject` naming an address a
/// recent collection moved an object away from.
///
/// # What this decides
///
/// A local ref is a raw heap pointer with no remap table (see the branch
/// above), so a handle a foreign `.so` holds across a moving collection comes
/// back naming from-space. When the vacated span has been re-issued, its
/// header reads back all-zero, `ClassId(0)` is `java/lang/Object`, and the
/// dispatch that follows raises `NoSuchMethodError java/lang/Object.<method>`.
///
/// The netty `ParameterizedSslHandlerTest` stall produces exactly that line
/// from at least three different producers, and one of them —
/// `checkClientTrusted` on the OPENSSL provider, which reaches the trust
/// manager through tcnative's JNI — has never had its holder named, because no
/// guard fires on its receiver. This one does, at the boundary, with the
/// backtrace that names the JNI entry point.
///
/// # What it cannot become
///
/// A stronger validity check here would NOT fix it: `is_heap_addr` is
/// alignment plus live-region containment, `is_object_address` adds header-tag
/// validation, and an all-zero header satisfies both (`kind` tag 0 is
/// `ObjectKind::Object`, `element_type` 0 is `Reference`). Only the per-thread
/// handle table this branch's FOLLOW-UP note describes removes the hazard.
///
/// Off by default and free when off: one relaxed load of the flag. Armed, it
/// costs a probe of the relocation ring, which itself only holds anything when
/// `CRATONVM_DBG=gcpart` is also set — so the two are meant to be armed
/// together, and the report says so when they are not.
#[inline]
pub(in crate::native::jni) fn note_suspect_local_ref(shared: &SharedVm, addr: usize) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let on = *ON.get_or_init(|| {
        None::<std::ffi::OsString>.is_some()
    });
    if !on {
        return;
    }
    static REPORTED: AtomicU64 = AtomicU64::new(0);
    const MAX_REPORTS: u64 = 24;
    let forwards = crate::memory::gc::gcpart_probe(addr);
    let moved: Vec<String> = forwards
        .iter()
        .filter_map(|(epoch, moved_to, _len, _dest)| {
            moved_to.map(|to| format!("epoch {epoch} -> {to:#x}"))
        })
        .collect();
    if moved.is_empty() {
        return;
    }
    if REPORTED.fetch_add(1, Ordering::Relaxed) >= MAX_REPORTS {
        return;
    }
    let live_base = shared.mem.heap.is_object_address(addr).is_some();
    tracing::error!(
        target: "cratonvm::gc::guard",
        obj = format!("{addr:#x}"),
        forwards = moved.join(", "),
        passes_is_object_address = live_base,
        backtrace = %std::backtrace::Backtrace::force_capture(),
        "a JNI LOCAL ref names an address a recent collection moved an object \
         away from. A local ref is a raw heap pointer with no remap table, so \
         the foreign caller in the backtrace is holding a stale handle; the \
         next dispatch on it resolves ClassId(0) == java/lang/Object. Note \
         `passes_is_object_address`: an all-zero header satisfies that check, \
         so no validity gate here can catch this.",
    );
}

/// Old-style sentinel for use in JniLocalFrame (kept for compatibility).
pub struct JniLocalFrame {
    pub refs: Vec<ObjectRef>,
}

impl JniLocalFrame {
    pub fn new() -> Self {
        Self {
            refs: Vec::with_capacity(16),
        }
    }
    pub fn add(&mut self, obj: ObjectRef) -> JObject {
        self.refs.push(obj);
        obj_to_jobject(obj)
    }
    pub fn remove(&mut self, obj: JObject) {
        // SECURITY FIX (V3) follow-up: match the stored ref by its raw jobject
        // handle directly. The frame's own refs are already trusted, live
        // ObjectRefs, so removal must NOT route through the heap-validating
        // `jobject_to_obj` (which returns `None` outside an active JNI context
        // — e.g. in unit tests or any non-JNI caller — and would silently leak
        // the ref). `obj_to_jobject` is a pure ref→handle conversion.
        self.refs.retain(|r| obj_to_jobject(*r) != obj);
    }
}

impl Default for JniLocalFrame {
    fn default() -> Self {
        Self::new()
    }
}

/// Maximum length for C strings received from native code.
pub(in crate::native::jni) const MAX_JNI_CSTR_LEN: usize = 65536;

/// Convert a C string pointer to a Rust `&str`, with length bounds checking.
///
/// # Safety
/// `ptr` must point to a valid null-terminated C string (or be null).
pub(in crate::native::jni) unsafe fn cstr_to_str<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    // Scan up to MAX_JNI_CSTR_LEN bytes to avoid unbounded reads on malformed input.
    let mut len = 0;
    while len < MAX_JNI_CSTR_LEN {
        if *ptr.add(len) == 0 {
            let slice = std::slice::from_raw_parts(ptr as *const u8, len);
            return std::str::from_utf8(slice).ok();
        }
        len += 1;
    }
    tracing::warn!("JNI cstr_to_str: string exceeds {MAX_JNI_CSTR_LEN} byte limit");
    None
}

/// gcf f1/j: the bytes of the NUL-terminated C string at `ptr`, without the
/// terminator and WITHOUT [`MAX_JNI_CSTR_LEN`]'s cap -- `strlen`, which is how
/// HotSpot reads a `NewStringUTF` argument (a native building a large JSON or
/// XML document through it is ordinary). `None` for a null pointer.
///
/// # Safety
/// `ptr` must point to a valid NUL-terminated C string (or be null).
pub(in crate::native::jni) unsafe fn cstr_bytes<'a>(ptr: *const c_char) -> Option<&'a [u8]> {
    if ptr.is_null() {
        return None;
    }
    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    Some(std::slice::from_raw_parts(ptr as *const u8, len))
}

/// gcf f1/j: decode JNI "modified UTF-8" (the inverse of [`to_modified_utf8`])
/// to UTF-16 units, as HotSpot's `NewStringUTF` does: `C0 80` is U+0000, a
/// three-byte sequence may encode a surrogate (a CESU-8 pair becomes the
/// pair; a lone one stays lone, as a Java `String` may hold it), and a
/// two-byte sequence may be overlong. Standard UTF-8's four-byte form, which
/// modified UTF-8 never produces, is accepted as the supplementary character
/// it encodes (what the strict-UTF-8 path answers for it). `None` for a byte
/// that starts no sequence, a missing continuation byte, or a four-byte form
/// that names no character.
pub(in crate::native::jni) fn decode_modified_utf8(bytes: &[u8]) -> Option<Vec<u16>> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let b0 = bytes[i];
        // The `k`-th continuation byte of the sequence at `i`.
        let cont = |k: usize| -> Option<u16> {
            bytes
                .get(i + k)
                .copied()
                .filter(|b| b & 0xC0 == 0x80)
                .map(|b| u16::from(b & 0x3F))
        };
        if b0 < 0x80 {
            out.push(u16::from(b0));
            i += 1;
        } else if b0 & 0xE0 == 0xC0 {
            out.push((u16::from(b0 & 0x1F) << 6) | cont(1)?);
            i += 2;
        } else if b0 & 0xF0 == 0xE0 {
            out.push((u16::from(b0 & 0x0F) << 12) | (cont(1)? << 6) | cont(2)?);
            i += 3;
        } else if b0 & 0xF8 == 0xF0 {
            let cp = (u32::from(b0 & 0x07) << 18)
                | (u32::from(cont(1)?) << 12)
                | (u32::from(cont(2)?) << 6)
                | u32::from(cont(3)?);
            let ch = char::from_u32(cp)?;
            let mut pair = [0u16; 2];
            out.extend_from_slice(ch.encode_utf16(&mut pair));
            i += 4;
        } else {
            return None;
        }
    }
    Some(out)
}

/// Encode a Rust `&str` into JNI "modified UTF-8" (JNI spec, "Modified UTF-8
/// Strings"). This differs from standard UTF-8 in two ways:
///
///   * `U+0000` is encoded as the two bytes `0xC0 0x80` (never a single `0x00`),
///     so the byte stream contains no interior NUL and can be terminated by a
///     single trailing `0x00` like a C string. Returning null for strings that
///     contain an embedded NUL — as `CString::new` would force — violates the
///     JNI contract for `GetStringUTFChars`.
///   * Supplementary characters (code points `> U+FFFF`) are encoded as a UTF-16
///     surrogate pair, each surrogate then emitted as its own three-byte
///     sequence (CESU-8). A modified-UTF-8 sequence therefore never exceeds
///     three bytes per unit, matching HotSpot's behaviour.
///
/// The returned `Vec<u8>` contains no `0x00` bytes, so it can be wrapped in a
/// `CString` without error.
///
/// gci i14/jh: the JNI entry points encode the exact units with
/// [`units_to_modified_utf8`] now; this `str` encoder stays as the reference
/// the unit tests check it against.
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::native::jni) fn to_modified_utf8(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() + 1);
    for ch in s.chars() {
        let cp = ch as u32;
        match cp {
            // U+0000 — encode as the overlong two-byte form, not a NUL byte.
            0x0000 => out.extend_from_slice(&[0xC0, 0x80]),
            // U+0001..U+007F — single byte (ASCII), unchanged.
            0x0001..=0x007F => out.push(cp as u8),
            // U+0080..U+07FF — two bytes.
            0x0080..=0x07FF => {
                out.push(0xC0 | (cp >> 6) as u8);
                out.push(0x80 | (cp & 0x3F) as u8);
            }
            // U+0800..U+FFFF — three bytes (BMP, including the non-surrogate
            // range; `char` never holds a lone surrogate, so this is safe).
            0x0800..=0xFFFF => {
                out.push(0xE0 | (cp >> 12) as u8);
                out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
                out.push(0x80 | (cp & 0x3F) as u8);
            }
            // Supplementary plane — emit a UTF-16 surrogate pair, each surrogate
            // as a three-byte sequence (CESU-8), per the JNI spec.
            _ => {
                let v = cp - 0x1_0000;
                let hi = 0xD800 | (v >> 10);
                let lo = 0xDC00 | (v & 0x3FF);
                for unit in [hi, lo] {
                    out.push(0xE0 | (unit >> 12) as u8);
                    out.push(0x80 | ((unit >> 6) & 0x3F) as u8);
                    out.push(0x80 | (unit & 0x3F) as u8);
                }
            }
        }
    }
    out
}

/// gci i14/jh: modified UTF-8 of a string's exact UTF-16 UNITS, each unit on
/// its own (HotSpot's `UNICODE::convert_to_utf8`): U+0000 as `C0 80`, a unit
/// below U+0080 as one byte, below U+0800 as two, every other unit -- each
/// half of a surrogate pair, and a LONE surrogate -- as three. For a string
/// with no lone surrogate this is byte for byte [`to_modified_utf8`] of its
/// `str` (a pair's two 3-byte sequences are that function's CESU-8 arm); a
/// lone surrogate is `ED A0 80`-shaped as on HotSpot, where the `str` route
/// wrote U+FFFD (`EF BF BD`), so `NewStringUTF(GetStringUTFChars(s))` lost it.
pub(in crate::native::jni) fn units_to_modified_utf8(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(units.len() + 1);
    push_modified_utf8_units(units, &mut out);
    out
}

/// [`units_to_modified_utf8`], appended to `out` (j27 wave 8, lane W8JNI: so
/// `GetStringUTFChars` can encode into a buffer it reserved fallibly).
pub(in crate::native::jni) fn push_modified_utf8_units(units: &[u16], out: &mut Vec<u8>) {
    for &u in units {
        let cp = u32::from(u);
        match cp {
            0x0000 => out.extend_from_slice(&[0xC0, 0x80]),
            0x0001..=0x007F => out.push(cp as u8),
            0x0080..=0x07FF => {
                out.push(0xC0 | (cp >> 6) as u8);
                out.push(0x80 | (cp & 0x3F) as u8);
            }
            _ => {
                out.push(0xE0 | (cp >> 12) as u8);
                out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
                out.push(0x80 | (cp & 0x3F) as u8);
            }
        }
    }
}

/// gci i14/jh: the byte count [`units_to_modified_utf8`] produces, without the
/// buffer (`GetStringUTFLength`, `GetStringUTFLengthAsLong`).
pub(in crate::native::jni) fn modified_utf8_len_units(units: &[u16]) -> usize {
    units
        .iter()
        .map(|&u| match u {
            0x0000 => 2,
            0x0001..=0x007F => 1,
            0x0080..=0x07FF => 2,
            _ => 3,
        })
        .sum()
}

/// Number of bytes [`to_modified_utf8`] would produce for `s`, without
/// allocating the encoded buffer. Used by `GetStringUTFLength` so the reported
/// length agrees with the bytes `GetStringUTFChars` returns. (gci i14/jh: the
/// entry points measure units with [`modified_utf8_len_units`] now; this stays
/// as the tests' reference.)
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::native::jni) fn modified_utf8_len(s: &str) -> usize {
    let mut n = 0usize;
    for ch in s.chars() {
        let cp = ch as u32;
        n += match cp {
            0x0000 => 2,          // overlong NUL
            0x0001..=0x007F => 1, // ASCII
            0x0080..=0x07FF => 2, // two-byte
            0x0800..=0xFFFF => 3, // three-byte BMP
            _ => 6,               // surrogate pair: two 3-byte sequences
        };
    }
    n
}

/// Encode a method identity as a JMethodID.
/// We pack (class_id, method_index) into a u64.
pub(in crate::native::jni) fn encode_method_id(class_id: ClassId, method_index: u16) -> JMethodID {
    ((class_id.as_u32() as u64) << 32) | (method_index as u64)
}

/// Decode a JMethodID back to (class_id, method_index).
pub(in crate::native::jni) fn decode_method_id(mid: JMethodID) -> (ClassId, u16) {
    let class_id = ClassId::new((mid >> 32) as u32);
    let method_index = (mid & 0xFFFF) as u16;
    (class_id, method_index)
}

/// Encode a field identity as a JFieldID.
/// We pack (class_id, field_index) into a u64.
pub(in crate::native::jni) fn encode_field_id(class_id: ClassId, field_index: usize) -> JFieldID {
    ((class_id.as_u32() as u64) << 32) | (field_index as u64)
}

/// The top bit of a static field's `jfieldID` (interpreter round i1 wave 52,
/// lane L1): a static field's index among its class's statics and an
/// instance field's slot collide, so the C JVMTI table (`GetClassFields`,
/// `GetFieldName`) needs the kind; `GetStaticFieldID` and the JVMTI table
/// set it, and [`decode_field_id`] drops it.
pub(crate) const STATIC_FIELD_ID_MARK: JFieldID = 1 << 63;

/// Decode a JFieldID back to (class_id, field_index).
///
/// The top bit is not part of the class id ([`STATIC_FIELD_ID_MARK`]).
pub(in crate::native::jni) fn decode_field_id(fid: JFieldID) -> (ClassId, usize) {
    let class_id = ClassId::new(((fid >> 32) as u32) & 0x7FFF_FFFF);
    let field_index = (fid & 0xFFFF_FFFF) as usize;
    (class_id, field_index)
}

// ---------------------------------------------------------------------------
// JNI function implementations (extern "C")
// ---------------------------------------------------------------------------

// ---- Index 4: GetVersion ----
/// gcf f1/j: `JNI_VERSION_24`, as HotSpot 25 answers; this answered
/// `JNI_VERSION_1_8` (a native that reads the version to decide whether
/// `GetModule`, `IsVirtualThread` or `GetStringUTFLengthAsLong` exist saw an
/// older VM than the table it was handed). Both modes.
///
/// `inline(never)`: a body this small is otherwise auto-inlinable across
/// codegen units, and each unit's copy has its own address, so the table's
/// slot and `jni_function_table_matches_the_jni_h_layout`'s `fn` item could
/// name two different copies of the same function.
#[inline(never)]
pub(in crate::native::jni) extern "C" fn jni_get_version(_env: JNIEnv) -> JInt {
    JNI_VERSION_24
}

// ---- Index 6: FindClass ----
//
// Round 12 wave 7 (lane compat), both modes
// (`r12w6-jni-findclass-ignores-the-native-loader-and-raises-nothing-20260927.md`):
//
// * the class is resolved as the calling Java code would resolve it
//   ([`jni_find_class_in_caller_context`]), so a native of a class a
//   `URLClassLoader` defined finds its siblings; a name in dotted form is not a
//   class name and is refused, as HotSpot refuses it. Kill switch
//   `CRATONVM_JNI_FINDCLASS_LOADER=0` (the application-namespace lookup below,
//   for every caller, dotted names included);
// * a failure leaves `NoClassDefFoundError: <name>` (or the typed linkage
//   error) pending, as HotSpot's `resolve_or_fail` does, instead of a bare
//   NULL. Kill switch `CRATONVM_JNI_FINDCLASS_RAISES=0`.
pub(in crate::native::jni) extern "C" fn jni_find_class(_env: JNIEnv, name: *const c_char) -> JClass {
    let _fx = ForeignJniEntry::enter();
    let raises = true;
    let name_str = match unsafe { cstr_to_str(name) } {
        Some(s) => s,
        None => {
            if raises {
                // HotSpot's `SystemDictionary::class_name_symbol` text for a
                // NULL name; an undecodable one names nothing it could find.
                raise_jni_no_class_def_found(if name.is_null() {
                    "No class name given"
                } else {
                    "<class name is not valid UTF-8>"
                });
            }
            return 0;
        }
    };
    // HotSpot's `SystemDictionary::resolve_instance_class_or_null_helper`
    // strips one outer `L...;` field-descriptor envelope when resolving a
    // non-array class (`!Signature::is_array(class_name)` and
    // `Signature::has_envelope(class_name)`), resolving the inner name as an
    // instance class while keeping `name_str` for any `NoClassDefFoundError`
    // message. Native libraries such as Apache Arrow's `arrow_cdata_jni` pass
    // `"Ljava/lang/Object;"` to `FindClass`.
    let lookup_name = name_str
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .unwrap_or(name_str);
    let invalid_array_envelope = name_str.starts_with('L') && lookup_name.starts_with('[');
    if jni_find_class_loader_aware() {
        // Internal form only: no class name contains `.` (JVMS 4.2.1), so
        // `java.lang.String` names no class -- HotSpot's loader answers
        // `java/lang/String`, the name check refuses it, and FindClass fails.
        if name_str.contains('.') {
            if raises {
                raise_jni_no_class_def_found(name_str);
            }
            return 0;
        }
        match jni_find_class_in_caller_context(lookup_name, name_str, raises) {
            JniFindClassOutcome::Found(cid) => {
                if jni_find_class_initialize(cid, lookup_name, raises) {
                    return class_id_to_jclass(cid);
                }
                if raises && JNI_PENDING_EXCEPTION.with(|cell| cell.get()) == 0 {
                    raise_jni_no_class_def_found(name_str);
                }
                return 0;
            }
            JniFindClassOutcome::Failed => {
                // Never a bare NULL: whatever the resolution could not type
                // still leaves `NoClassDefFoundError` pending.
                if raises && JNI_PENDING_EXCEPTION.with(|cell| cell.get()) == 0 {
                    raise_jni_no_class_def_found(name_str);
                }
                return 0;
            }
            JniFindClassOutcome::NoCallerContext => {}
        }
    }
    // Without the loader-aware lookup this arm answers every caller; with it,
    // only a thread with no Java frame reaches here, and that lookup is the
    // system loader's, which initiates the class on HotSpot.
    let no_caller_context = jni_find_class_loader_aware();
    let found = (!invalid_array_envelope)
        .then(|| {
            with_shared_vm(|shared| {
                let found = shared.load_class_concurrent(lookup_name).ok();
                if let (Some(class_id), true) = (found, no_caller_context) {
                    crate::runtime::resolve::initiating_records::note_jni_find_class_without_caller(
                        shared, class_id,
                    );
                }
                found
            })
            .flatten()
        })
        .flatten();
    match found {
        Some(class_id) => {
            if jni_find_class_initialize(class_id, lookup_name, raises) {
                return class_id_to_jclass(class_id);
            }
            if raises && JNI_PENDING_EXCEPTION.with(|cell| cell.get()) == 0 {
                raise_jni_no_class_def_found(name_str);
            }
            0
        }
        None => {
            if raises {
                raise_jni_no_class_def_found(name_str);
            }
            0
        }
    }
}

/// JNI `FindClass` hands back an INITIALIZED class, as HotSpot's
/// `jni_FindClass` does (`find_class_from_class_loader(.., init = true, ..)`):
/// a static read through `GetStaticFieldID` / `GetStatic<T>Field` -- which read
/// the statics table directly and initialize nothing -- then sees `<clinit>`'s
/// values, and a failing `<clinit>` surfaces from `FindClass` as its
/// `ExceptionInInitializerError` / `NoClassDefFoundError`. `true` when the
/// class is (or needs nothing to be) initialized; `false` when initialization
/// threw, which is left pending when `raises`. An array class has no
/// `<clinit>`; a thread with no JNI context cannot run Java, and keeps the
/// uninitialized answer it always had. Round 12 wave 8 (lane compat2), both
/// modes; kill switch `CRATONVM_JNI_FINDCLASS_INITIALIZES=0`.
pub(in crate::native::jni) fn jni_find_class_initialize(class_id: ClassId, name: &str, raises: bool) -> bool {
    if name.starts_with('[') {
        return true;
    }
    // `<clinit>` is Java: a foreign-attached thread must be a counted mutator
    // while it runs, as for the loader call in the caller-context lookup.
    let _fg = ForeignCallGuard::enter();
    with_jni_context(|shared, thread| {
        match crate::vm::ensure_class_initialized_shared(shared, thread, class_id) {
            Ok(()) => true,
            Err(err) => {
                if raises {
                    let _ = jni_surface_jdk_only(shared, thread, Err(err));
                }
                false
            }
        }
    })
    .unwrap_or(true)
}



/// What [`jni_find_class_in_caller_context`] came back with.
pub(in crate::native::jni) enum JniFindClassOutcome {
    Found(ClassId),
    /// Resolution failed; what it threw is pending when `raises` asked for it.
    Failed,
    /// No attached `JvmThread`, or no Java frame on it (a host thread that
    /// attached and called FindClass directly): the application-namespace
    /// lookup answers, which is HotSpot's system-loader answer.
    NoCallerContext,
}

/// JNI `FindClass`'s loader, as HotSpot's `jni_FindClass` picks it: the
/// loader of the Java code that called the native -- CratonVM pushes no frame
/// for the native itself, so the innermost Java frame (compiled frames
/// included) is its caller, which is the native's own class or a class of the
/// same loader in the ordinary case -- except from `JNI_OnLoad` /
/// `JNI_OnUnload`, where that frame is the JDK's `NativeLibraries` machinery
/// and the context is `NativeLibraries.getFromClass()`, the class that called
/// `System.loadLibrary`.
///
/// `name` is resolved exactly as a constant-pool class reference of that
/// class resolves ([`resolve_class_loader_aware`]): a built-in context takes
/// the same global lookup the legacy path takes, a user-defined loader is
/// asked first. A failure is converted as an opcode's is
/// (`convert_class_not_found_for`: `NoClassDefFoundError: <name>` caused by
/// the `ClassNotFoundException`) and published pending when `raises`.
///
/// [`resolve_class_loader_aware`]: crate::runtime::interpreter::resolve_class_loader_aware
pub(in crate::native::jni) fn jni_find_class_in_caller_context(
    lookup_name: &str,
    error_name: &str,
    raises: bool,
) -> JniFindClassOutcome {
    // The loader's `loadClass` is Java: a foreign-attached thread must be a
    // counted mutator while it runs.
    let _fg = ForeignCallGuard::enter();
    with_jni_context(|shared, thread| {
        let Some(context) = jni_find_class_context(shared, thread) else {
            return JniFindClassOutcome::NoCallerContext;
        };
        let resolved = if error_name.starts_with('L') && lookup_name.starts_with('[') {
            Err(crate::error::MethodCallFailed::InternalError(
                crate::error::VmError::ClassFile(
                    crate::error::ClassFileError::ClassNotFound {
                        class_name: lookup_name.to_string(),
                    },
                ),
            ))
        } else {
            crate::runtime::interpreter::resolve_class_loader_aware(
                shared,
                thread,
                context,
                lookup_name,
            )
        };
        match resolved {
            Ok(cid) => JniFindClassOutcome::Found(cid),
            Err(err) => {
                if raises {
                    let failed = crate::runtime::exceptions::convert_class_not_found_for(
                        shared,
                        thread,
                        Some(context),
                        error_name,
                        err,
                    );
                    let _ = jni_surface_jdk_only(shared, thread, Err(failed));
                }
                JniFindClassOutcome::Failed
            }
        }
    })
    .unwrap_or(JniFindClassOutcome::NoCallerContext)
}

/// The class whose loader [`jni_find_class_in_caller_context`] resolves
/// through, or `None` when the thread has no Java frame.
pub(in crate::native::jni) fn jni_find_class_context(shared: &SharedVm, thread: &mut JvmThread) -> Option<ClassId> {
    // The native being dispatched, not its caller: HotSpot's
    // `security_get_caller_class(0)` sees the native frame, CratonVM pushes
    // none, so `vm_exec.rs`'s `JniContextGuard::install_for_native` records
    // the native's declaring class on the thread
    // (`r12w8-compat2-jni-findclass-native-declaring-class-patch`).
    let native_holder = {
        thread.jni_native_class
    };
    let innermost = crate::runtime::stackwalker::frame_class_ids_with_compiled(&thread.frames)
        .first()
        .copied()
        .or(native_holder)?;
    const NATIVE_LIBRARIES: &str = "jdk/internal/loader/NativeLibraries";
    let native_libraries = {
        let cm = shared.classes.class_manager.read();
        let from_library_loader = cm.get_class(innermost).is_some_and(|c| {
            matches!(c.loader_id, cratonvm_types::ClassLoaderId::Bootstrap)
                && (&*c.name == NATIVE_LIBRARIES
                    || c.name
                        .strip_prefix(NATIVE_LIBRARIES)
                        .is_some_and(|rest| rest.starts_with('$')))
        });
        if from_library_loader {
            cm.get_loaded_class_id(NATIVE_LIBRARIES)
        } else {
            None
        }
    };
    let Some(native_libraries) = native_libraries else {
        return Some(native_holder.unwrap_or(innermost));
    };
    // `JNI_OnLoad` / `JNI_OnUnload`: HotSpot asks the library machinery which
    // class is loading (`getFromClass`, "invoked in the VM to determine the
    // context class"); it answers `Object.class` outside a load.
    match crate::vm::invoke_on_class_shared(
        shared,
        thread,
        native_libraries,
        "getFromClass",
        "()Ljava/lang/Class;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(mirror)))) => {
            Some(crate::vm::class_id_from_mirror(shared, mirror).unwrap_or(innermost))
        }
        _ => Some(innermost),
    }
}

/// `CRATONVM_JNI_FINDCLASS_LOADER` -- default ON (round 12 wave 7, lane
/// compat, both modes). `0` / `false` / `off` / `no` restores the
/// application-namespace lookup for every caller. Read per call, not cached:
/// a declared flag is read from the immutable flag snapshot, and a cached
/// switch would be one more `static` under
/// `vm/tests/per_vm_state_statics_ratchet.rs`.
pub(in crate::native::jni) fn jni_find_class_loader_aware() -> bool {
    true
}





// ---- Index 10: GetSuperclass ----
pub(in crate::native::jni) extern "C" fn jni_get_superclass(_env: JNIEnv, clazz: JClass) -> JClass {
    let _fx = ForeignJniEntry::enter();
    if clazz == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let class_id = jclass_class_id(clazz);
        // gcf f3/j: a lambda's class (no `ClassStore` entry: this answered
        // NULL) extends `Object`, as HotSpot's hidden lambda class does.
        if let Some([_, object]) = lambda_proxy_method_classes(shared, class_id) {
            return Some(class_id_to_jclass(object));
        }
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        // JNI: "If clazz specifies ... an interface, returns NULL". The class
        // file of an interface names `java/lang/Object` as its super_class,
        // and the class store keeps it, so this answered `Object`
        // (`Class.getSuperclass` answers null, `native_class_get_superclass`;
        // round 12 wave 6, lane jni). Both modes: the owner turned it on for
        // `--compatible` too (2026-09-27).
        if class.is_interface() {
            return None;
        }
        class.superclass.map(class_id_to_jclass)
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 11: IsAssignableFrom ----
pub(in crate::native::jni) extern "C" fn jni_is_assignable_from(_env: JNIEnv, sub: JClass, sup: JClass) -> JBoolean {
    let _fx = ForeignJniEntry::enter();
    if sub == 0 || sup == 0 {
        return JNI_FALSE;
    }
    with_shared_vm(|shared| {
        let sub_id = jclass_class_id(sub);
        let sup_id = jclass_class_id(sup);
        // gci i15/jk (F13's residue, i14/jh's R3): a handle that names no
        // class (`JNI_NO_CLASS`) is a primitive mirror -- with indirect locals
        // (the default) a `Class` parameter such as `int.class` decodes here --
        // or a dead handle. HotSpot's `jni_IsAssignableFrom` answers a
        // primitive by mirror identity; two such handles both decoded to
        // `JNI_NO_CLASS` and the id test below answered `int.class` assignable
        // to `long.class` (and two deleted globals assignable to each other).
        if sub_id == JNI_NO_CLASS || sup_id == JNI_NO_CLASS {
            let mirrors = (jobject_to_obj_no_keepalive(sub), jobject_to_obj_no_keepalive(sup));
            let same = matches!(mirrors, (Some(a), Some(b)) if a == b);
            return if same { JNI_TRUE } else { JNI_FALSE };
        }
        if sub_id == sup_id {
            return JNI_TRUE;
        }
        // gcf f3/j: a lambda's class has no `ClassStore` entry, so the walk
        // below answered `JNI_FALSE` at once (see `lambda_proxy_assignable_to`).
        if let Some(is) = lambda_proxy_assignable_to(shared, sub_id, sup_id) {
            return if is { JNI_TRUE } else { JNI_FALSE };
        }
        let walked = {
            let cm = shared.classes.class_manager.read();
            let mut current = sub_id;
            loop {
                let Some(class) = cm.get_class(current) else {
                    return JNI_FALSE;
                };
                // Check interfaces
                if class.interfaces.contains(&sup_id) {
                    break true;
                }
                match class.superclass {
                    Some(sc) => {
                        if sc == sup_id {
                            break true;
                        }
                        current = sc;
                    }
                    None => break false,
                }
            }
        };
        // The walk sees only DIRECT interfaces; see `jni_is_instance_of`.
        if walked || crate::runtime::interpreter::class_is_subtype(shared, sub_id, sup_id) {
            return JNI_TRUE;
        }
        // Round 11 wave 15 (lane rt, `r11w13-rt-array-alias-audit-jni-refs-gc`
        // open item 2): an ARRAY class's own entries name only `Object`,
        // `Cloneable` and `Serializable`, so covariance -- `String[]` to
        // `Object[]`, `Integer[][]` to `Number[][]` -- is invisible to both
        // relations above, and this answered `JNI_FALSE` where HotSpot answers
        // `JNI_TRUE`. Ask the `instanceof` opcode's strict array rule, exactly
        // as `jni_is_instance_of` does for an array object; an array class's
        // name IS its descriptor.
        let names = {
            let cm = shared.classes.class_manager.read();
            match (cm.get_class(sub_id), cm.get_class(sup_id)) {
                (Some(sub), Some(sup)) => Some((sub.name.to_string(), sup.name.to_string())),
                _ => None,
            }
        };
        match names {
            Some((sub_name, sup_name))
                if sub_name.starts_with('[')
                    && crate::runtime::interpreter::array_is_instance_of(
                        shared, &sub_name, &sup_name,
                    ) =>
            {
                JNI_TRUE
            }
            _ => JNI_FALSE,
        }
    })
    .unwrap_or(JNI_FALSE)
}

// ---- Index 13: Throw ----
pub(in crate::native::jni) extern "C" fn jni_throw(_env: JNIEnv, obj: JThrowable) -> JInt {
    let _fx = ForeignJniEntry::enter();
    // `Throw(env, cls)` (meant: `ThrowNew`) is a common native bug. A `jclass`
    // now resolves to its mirror, which is not a `Throwable`; keep refusing it
    // as before rather than making a `Class` the pending exception.
    if is_jclass_handle(obj) {
        return JNI_ERR;
    }
    let Some(exception) = jobject_to_obj(obj) else {
        return JNI_ERR;
    };
    set_jni_pending_exception_object(exception);
    JNI_OK
}

// ---- Index 14: ThrowNew ----
pub(in crate::native::jni) extern "C" fn jni_throw_new(_env: JNIEnv, clazz: JClass, msg: *const c_char) -> JInt {
    let _fx = ForeignJniEntry::enter();
    if clazz == 0 {
        return JNI_ERR;
    }
    let message = if msg.is_null() {
        None
    } else {
        match unsafe { cstr_to_str(msg) } {
            Some(message) => Some(message),
            None => return JNI_ERR,
        }
    };

    let result = with_jni_context(|shared, thread| {
        let class_id = jclass_class_id(clazz);
        let class_name = {
            let cm = shared.classes.class_manager.read();
            cm.get_class(class_id).map(|class| class.name.to_string())
        };
        let Some(class_name) = class_name else {
            return None;
        };
        crate::runtime::exceptions::create_exception_object_for_jni_throw_new(
            shared,
            thread,
            class_id,
            &class_name,
            message,
        )
        .ok()
    });

    match result.flatten() {
        Some(exception) => {
            set_jni_pending_exception_object(exception);
            JNI_OK
        }
        None => JNI_ERR,
    }
}

// ---- Index 15: ExceptionOccurred ----
pub(in crate::native::jni) extern "C" fn jni_exception_occurred(_env: JNIEnv) -> JThrowable {
    let _fx = ForeignJniEntry::enter();
    let handle = JNI_PENDING_EXCEPTION.with(|cell| cell.get());
    if handle == 0 {
        return 0;
    }
    // A collection may have relocated the exception after its original raw
    // handle was recorded.  Prefer the VM's remapped native-return root.
    //
    // Handed out as a RECORDED local ref (gc-common w4-g): the idiom is
    // `t = ExceptionOccurred(); ExceptionClear(); ... use t`, and
    // `ExceptionClear` drops `native_pending_return` -- the only root that
    // named the throwable -- so the native's `t` was held by nothing across
    // whatever up-call came next. HotSpot returns a local ref here.
    //
    // gcd d4/k (`docs/feature-designs/real-jdk-mode.md`):
    // the `u64::MAX` sentinel ("no throwable could be built") is not a handle
    // and is never handed to native code; NULL instead (ExceptionCheck still
    // answers true, and the native's return still fails). A heap failure now
    // pends the preallocated OOME (`pend_unbuilt_throwable`), so this only
    // remains for a class that could not load or no thread context.
    let fallback = if handle == u64::MAX { 0 } else { handle };
    with_jni_context(|_shared, thread| match thread.native_pending_return {
        Some(exc) => new_local_handle(exc),
        None => {
            // The raw pending handle, not a recorded local: counted as an
            // escape where locals are indirect, so an in-native call stays
            // counted from here on (`JniNativeCall`).
            if fallback != 0 && locals_are_indirect_here() {
                note_raw_local_escape();
            }
            fallback
        }
    })
    .unwrap_or(fallback)
}

// ---- Index 16: ExceptionDescribe ----
///
/// Prints the pending exception and **clears it** — the clear is the part that
/// matters for control flow, and the empty no-op this used to be did neither.
/// `if (ExceptionCheck(env)) { ExceptionDescribe(env); return -1; }` is the
/// canonical JNI error path, and it is written on the understanding that
/// nothing is pending afterwards; leaving the exception in place means it is
/// delivered at the native's return to a caller that believes it was handled.
pub(in crate::native::jni) extern "C" fn jni_exception_describe(_env: JNIEnv) {
    let _fx = ForeignJniEntry::enter();
    let handle = JNI_PENDING_EXCEPTION.with(|cell| cell.get());
    if handle == 0 {
        return;
    }
    // Prefer the GC-remapped root over the raw handle, as `ExceptionOccurred`
    // does — a collection may have moved the throwable since it was recorded.
    // gcd d4/k: the `u64::MAX` sentinel names no object; not decoded (it used
    // to take the global-ref path, miss and log).
    let exc = with_jni_context(|_shared, thread| thread.native_pending_return)
        .flatten()
        .or_else(|| {
            (handle != u64::MAX)
                .then(|| jobject_to_obj(handle))
                .flatten()
        });
    // Clear FIRST: the print below is a Java up-call, and it must not run with
    // this exception still pending.
    JNI_PENDING_EXCEPTION.with(|cell| cell.set(0));
    let _ = with_jni_context(|_shared, thread| {
        thread.native_pending_return = None;
    });
    let Some(exc) = exc else {
        return;
    };
    let _ = with_jni_context(|shared, thread| {
        let class_id = shared.mem.heap.class_id_of(exc);
        let printed = invoke_on_class_shared(
            shared,
            thread,
            class_id,
            "printStackTrace",
            "()V",
            &[Value::Object(Some(exc))],
        );
        // A failure while REPORTING must not become a second pending
        // exception the caller never asked about.
        if printed.is_err() {
            JNI_PENDING_EXCEPTION.with(|cell| cell.set(0));
            thread.native_pending_return = None;
        }
    });
}

// ---- Index 17: ExceptionClear ----
pub(in crate::native::jni) extern "C" fn jni_exception_clear(_env: JNIEnv) {
    let _fx = ForeignJniEntry::enter();
    JNI_PENDING_EXCEPTION.with(|cell| cell.set(0));
    let _ = with_jni_context(|_shared, thread| {
        thread.native_pending_return = None;
    });
}

// ---- Index 18: FatalError ----
///
/// gcf f2/j: HotSpot's line, on HotSpot's stream: `jni_FatalError` prints
/// `FATAL ERROR in native method: <msg>` through `tty` (standard output by
/// default) and then aborts. This printed `JNI FatalError: <msg>` on standard
/// error, and a message that was not UTF-8 as `unknown`. HotSpot then lists
/// the native's Java stack (`print_jni_stack`); that part is not reproduced.
pub(in crate::native::jni) extern "C" fn jni_fatal_error(_env: JNIEnv, msg: *const c_char) {
    use std::io::Write as _;
    let s = if msg.is_null() {
        std::borrow::Cow::Borrowed("")
    } else {
        // SAFETY: a non-null `msg` is the caller's NUL-terminated C string.
        unsafe { CStr::from_ptr(msg) }.to_string_lossy()
    };
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "FATAL ERROR in native method: {s}");
    let _ = out.flush();
    std::process::abort();
}

// ---- Index 20: PushLocalFrame ----
///
/// gc-common w10-c: a negative `capacity` is refused with `OutOfMemoryError`
/// pending and `JNI_ERR`, and pushes nothing; it used to push a frame and
/// answer `JNI_OK`. A large one is only a reservation hint
/// ([`push_local_frame`]). gci i13/jc: "as HotSpot does" was the JNI
/// specification's text, not HotSpot's `jni.cpp`, which refuses `< 0` and
/// `> MaxJNILocalCapacity` with nothing pending; that answer is
/// `CRATONVM_JNI_LOCAL_CAPACITY_HOTSPOT` ([`local_capacity_verdict_with`]),
/// default ON since 2026-10-17 (`=0` kills).
pub(in crate::native::jni) extern "C" fn jni_push_local_frame(_env: JNIEnv, capacity: JInt) -> JInt {
    let _fx = ForeignJniEntry::enter();
    // gci i13/jc: the rule (and its HotSpot-shaped opt-in) is
    // `local_capacity_verdict_with`'s, shared with `EnsureLocalCapacity`.
    if let LocalCapacityVerdict::Refuse { raise } = local_capacity_verdict(capacity) {
        if raise {
            refuse_local_capacity("PushLocalFrame", capacity);
        }
        return JNI_ERR;
    }
    let depth = local_frame_depth();
    push_local_frame(capacity as usize);
    JNI_EXPLICIT_FRAME_DEPTHS.with(|d| d.borrow_mut().push(depth));
    JNI_OK
}

// ---- Index 21: PopLocalFrame ----
///
/// gc-common w7-c: pops only a frame native code PUSHED. An unbalanced
/// `PopLocalFrame` (more pops than pushes, which the spec forbids) used to pop
/// whatever was on top: the native's own implicit dispatch frame, and on the
/// next extra pop the CALLER's -- an outer native's frame, whose locals that
/// native still holds, unrooted under it and no longer remapped by a moving
/// collection. The scope's own exit then popped one frame more. HotSpot
/// ignores such a pop and returns `result` unchanged
/// (`jni_PopLocalFrame`: "code will still work if PopLocalFrame is called
/// without a corresponding PushLocalFrame"); so does this now.
pub(in crate::native::jni) extern "C" fn jni_pop_local_frame(_env: JNIEnv, result: JObject) -> JObject {
    let _fx = ForeignJniEntry::enter();
    if !top_local_frame_is_explicit() {
        return result;
    }
    pop_local_frame(pop_local_frame_result_as_local(result))
}

/// gci i13/jc: the object a balanced `PopLocalFrame` hands back, as a LOCAL
/// handle. HotSpot's `jni_PopLocalFrame` resolves `result` before the pop and
/// answers `JNIHandles::make_local` of it, whatever kind of reference came
/// in; this passed a GLOBAL or WEAK global `result` back unchanged, so
/// `GetObjectRefType` of the answer said 2 / 3 where HotSpot says 1, and the
/// native's `DeleteGlobalRef(g)` after `r = PopLocalFrame(env, g)` (legal on
/// HotSpot: `r` is a separate local) left `r` naming a deleted handle. A
/// cleared weak comes back NULL, as on HotSpot. A local, NULL or `jclass`
/// result is unchanged (`pop_local_frame` re-records a local in the parent).
pub(in crate::native::jni) fn pop_local_frame_result_as_local(result: JObject) -> JObject {
    if result & 1 == 1 && !is_jclass_handle(result) {
        // Resolved with the weak-global keep-alive: the object is handed on.
        return jobject_to_obj(result).map_or(0, obj_to_jobject);
    }
    result
}

/// Tag bit that lifts a `ClassId` out of the range where `0` means JNI NULL.
///
/// **The defect this closes.** A `jclass` in this table IS a `ClassId` — that is
/// what `FindClass` returns and what `GetSuperclass`, `GetFieldID`,
/// `RegisterNatives` and eighteen others decode with `ClassId::new(clazz as
/// u32)`. But `jclass` is a `jobject`, and in JNI a NULL `jobject` means
/// *failure*. So whichever class happened to be `ClassId(0)` was unreachable
/// through `FindClass`: the call returned 0 and every caller read it as "no such
/// class".
///
/// `java.lang.Object` is the first class this VM loads, so `ClassId(0)` is
/// `java.lang.Object` — the one class essentially every JNI library asks for
/// first. Measured with a C probe calling `FindClass` from both `JNI_OnLoad` and
/// a registered native: `FindClass(java/lang/Object) = (nil)` on both, against
/// a live handle on HotSpot.
///
/// Downstream that surfaced as nothing resembling a JNI defect. JNA's
/// `libjnidispatch` aborts its id cache with `JNA: Problems loading core IDs:
/// java.lang.Object`, then never caches `classString`/`MID_String_init`, so
/// every `jstring` it later builds is NULL — and `com.sun.jna.Native.<clinit>`
/// dies on `NullPointerException: Cannot invoke "String.split(String)" because
/// "nativeVersion" is null`, which reads like a JNA/version problem.
///
/// **Why a HIGH tag and not a +1 bias.** The tag lives entirely above bit 31, so
/// the low 32 bits stay exactly the `ClassId` and all twenty-one existing
/// `ClassId::new(clazz as u32)` decode sites keep working untouched — the
/// truncation discards the tag. A bias would have needed every one of them
/// changed in lockstep, and a single missed site would silently decode the WRONG
/// class rather than fail. (gci i14/jh: those sites have since all moved to
/// [`jclass_class_id`]; the truncation is left only inside the decoders, behind
/// an [`is_jclass_handle`] test.)
///
/// **Why these particular bits.** Bits 48-62 are set, which no x86-64 or AArch64
/// user-space pointer can have (canonical user addresses are below
/// `0x0000_8000_0000_0000`). So a tagged `jclass` cannot be mistaken for either
/// of the two object-handle encodings — a raw heap pointer or a tagged
/// `Box<ObjectRef>` — and [`is_jclass_handle`] is an exact test rather than a
/// guess that has to be tried second.
pub(in crate::native::jni) const JCLASS_TAG: JObject = 0x7F51_0000_0000_0000;

/// Mask selecting the tag half of a `jclass` handle.
pub(in crate::native::jni) const JCLASS_TAG_MASK: JObject = 0xFFFF_FFFF_0000_0000;

/// Encode a `ClassId` as the `jclass` handle native code receives.
pub fn class_id_to_jclass(id: ClassId) -> JClass {
    (id.as_u32() as JObject) | JCLASS_TAG
}

/// Does this handle carry the `jclass` encoding at all? Cheap, allocation-free
/// and lock-free — no class-manager lookup, so it is safe to consult FIRST, in
/// paths that would otherwise log a spurious "not found in global ref table".
pub(in crate::native::jni) fn is_jclass_handle(h: JObject) -> bool {
    h & JCLASS_TAG_MASK == JCLASS_TAG
}

/// The `java.lang.Class` mirror a `jclass` handle names, for every place a
/// `jclass` crosses back into Java as an OBJECT: a native's `Class` return
/// (`dispatch_jni_native`), `SetObjectField` / `SetStaticObjectField` /
/// `SetObjectArrayElement` / `NewObjectArray`'s initial element, an up-call
/// argument, `MonitorEnter(cls)`, `GetObjectClass(cls)`. On HotSpot a `jclass`
/// IS a reference to the mirror; here it is a tagged `ClassId`
/// ([`class_id_to_jclass`]), and [`jobject_to_obj`] had no arm for it, so all of
/// these read Java `null` (round 12 wave 1, lane rt;
/// `w36-jni-native-returning-a-jclass-hands-java-null`).
///
/// `None` for a class this VM does not have (a forged or stale tag), and when
/// the mirror does not exist yet and the heap cannot hold it without a
/// collection: the mint is [`crate::vm::try_get_or_create_class_mirror`], which
/// never collects, because every caller holds raw `ObjectRef`s resolved before
/// it. No lock is held across the two lookups, so the lock order is the one
/// `class_mirror_impl` itself uses. Kill switch:
/// `CRATONVM_JNI_JCLASS_MIRROR=0` (the old `null`).
pub(in crate::native::jni) fn jclass_handle_mirror(h: JObject) -> Option<ObjectRef> {
    if !jclass_mirror_decode_active() {
        return None;
    }
    let class_id = ClassId::new(h as u32);
    with_shared_vm(|shared| {
        let cached = shared.classes.class_mirrors.read().get(&class_id).copied();
        if cached.is_some() {
            return cached;
        }
        // gcf f3/j: a lambda's class (`GetObjectClass(lambda)`) has a mirror
        // too (`lambda.getClass()`), but no `ClassStore` entry: it read as an
        // unknown class, so `GetObjectClass(GetObjectClass(lambda))` was NULL.
        let known = shared.classes.is_lambda_proxy_class(class_id)
            || shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .is_some();
        if !known {
            return None;
        }
        crate::vm::try_get_or_create_class_mirror(shared, class_id)
    })
    .flatten()
}

/// `CRATONVM_JNI_JCLASS_MIRROR`, latched like every flag. Default ON; `0` /
/// `false` / `off` / `no` restores the pre-round-12 decode (a `jclass` read as
/// an object is `null`).
pub(in crate::native::jni) fn jclass_mirror_decode_active() -> bool {
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

/// Is this handle a `jclass` (as produced by [`class_id_to_jclass`]) naming a
/// live class?
///
/// A confirmed class is its own permanent reference: classes are never unloaded
/// here, so "a global ref to a class" is the same handle again. That identity is
/// what makes the round trip work, because native code stores the RESULT of
/// `NewGlobalRef`/`NewWeakGlobalRef` and later passes it back as a `jclass` to
/// `GetMethodID`/`NewObject`.
pub(in crate::native::jni) fn jclass_id_handle(h: JObject) -> bool {
    if !is_jclass_handle(h) {
        return false;
    }
    with_shared_vm(|shared| {
        shared
            .classes
            .class_manager
            .read()
            .get_class(ClassId::new(h as u32))
            .is_some()
    })
    .unwrap_or(false)
}

/// Normalise ANY handle that names a class into its `ClassId`.
///
/// A class reaches native code through two encodings, and until this helper
/// existed only one of them was decodable:
///
/// * `FindClass`, `GetObjectClass`, `GetSuperclass` and the `jclass` argument
///   of a static native all hand back [`class_id_to_jclass`] — a `ClassId` under
///   [`JCLASS_TAG`]. Every `jclass`-consuming entry point decodes that with
///   `ClassId::new(h as u32)`, which works because the truncation drops the tag;
/// * a `java.lang.Class` that arrives as an ordinary **parameter** — the native
///   is declared `foo(int, Class<?>, Object)` — is marshalled like any other
///   reference, as `obj_to_jobject` of its mirror. Truncating THAT to a `u32`
///   yields a fragment of a heap address, i.e. an arbitrary unrelated class or
///   no class at all.
///
/// Nothing reconciled the two, so a `Class` parameter was unusable: measured on
/// JDK 25 against a purpose-built JNI fixture, `IsSameObject(param, FindClass)`,
/// `GetMethodID`, `GetStaticFieldID`, `IsAssignableFrom` and `IsInstanceOf` all
/// answered "no"/NULL where HotSpot answered yes. The library that surfaced it
/// is barchart-udt, whose `SocketUDT.setOption0(int code, Class<?> klaz, Object
/// value)` dispatches on `IsSameObject(klaz, <cached Boolean.class>)` and
/// therefore rejected EVERY option — netty's `NioUdtProvider` could not open a
/// channel at all ("unsupported option class in OptionUDT").
///
/// The fix is at consumption rather than at marshalling on purpose: re-encoding
/// `Class` arguments as `jclass` handles would fix the reads and break the
/// writes, because a native that hands the same reference back to Java (or to
/// `GetObjectClass`) needs the mirror `ObjectRef`, which a `jclass` handle does
/// not carry. Reconciling here leaves both encodings intact and makes both
/// decodable.
///
/// Falls back to the old truncation for a raw handle that is not a class, so
/// that decode is unchanged. (The C JVMTI table decodes through
/// [`jclass_class_id_exact`], which has no fallback.)
///
/// gci i14/jh (F13 of
/// `gci-i13jc-jni-critical-weak-globals-local-frames-review-findings-FIXED-20261016`):
/// every `jclass`-consuming JNI function decodes through this function (the
/// `ClassId::new(clazz as u32)` sites the older comments in this file name are
/// gone), so a real GLOBAL or WEAK handle to a mirror -- `NewGlobalRef` of a
/// `Class` parameter today, of any `jclass` under
/// [`class_global_refs_active`] -- decodes through the mirror like a local
/// one. What did not hold was the fallback: a table handle (bit 0) or an
/// indirect local whose referent is gone (a deleted global, a cleared weak, a
/// popped local) had its low 32 bits read as a class id -- a heap address
/// fragment or a small slot number, i.e. an arbitrary LOADED class. Those now
/// decode as [`JNI_NO_CLASS`], which names no class, so the caller answers
/// "no such class" ([`jclass_fallback_class_id`]).
pub(crate) fn jclass_class_id(h: JObject) -> ClassId {
    if is_jclass_handle(h) {
        return ClassId::new(h as u32);
    }
    if let Some(id) = jclass_mirror_class_id(h) {
        return id;
    }
    jclass_fallback_class_id(h)
}

/// gci i14/jh: the id no class has (`u32::MAX`: the field-id encoding caps a
/// class id at `2^31 - 1`, and the rest of the VM uses the same value as its
/// "no class" mark). [`jclass_class_id`]'s answer for a table or indirect
/// handle that resolves to no mirror.
pub(in crate::native::jni) const JNI_NO_CLASS: ClassId = ClassId::new(u32::MAX);

/// gci i14/jh: [`jclass_class_id`]'s answer for a handle that is neither a
/// `jclass` nor a live mirror. A global / weak table handle (bit 0, not a
/// `jclass` tag) or an indirect local carries no class id in its low bits:
/// [`JNI_NO_CLASS`]. Any other handle keeps the old truncation.
///
/// gci i15/jk (F13's residue): so does a RAW handle above `u32::MAX` -- a
/// direct local (indirect locals off) to an object that is no class mirror (a
/// primitive mirror, a non-`Class` object handed in as a `jclass`, a stale
/// address). No `ClassId` is wider than 32 bits and every `jclass` this table
/// mints carries [`JCLASS_TAG`], so such a value is an address and its low 32
/// bits named an arbitrary loaded class. A small untagged value keeps the
/// truncation (a bare id, as before).
pub(in crate::native::jni) fn jclass_fallback_class_id(h: JObject) -> ClassId {
    if is_jclass_handle(h) {
        return ClassId::new(h as u32);
    }
    if h & 1 == 1 || is_indirect_local_tag(h) || h > JObject::from(u32::MAX) {
        return JNI_NO_CLASS;
    }
    ClassId::new(h as u32)
}

/// [`jclass_class_id`] without its truncating fallback: `None` for a handle
/// that is neither a `jclass` nor a `java.lang.Class` mirror (a primitive
/// mirror included). The C JVMTI table's `jclass` decode
/// (`jvmti::native_env`, `JVMTI_ERROR_INVALID_CLASS`; interpreter round i1
/// wave 19): through the fallback, any other handle whose low 32 bits equal a
/// loaded class's id — an indirect local's are a small slot number — named
/// that class.
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) fn jclass_class_id_exact(h: JObject) -> Option<ClassId> {
    if is_jclass_handle(h) {
        return Some(ClassId::new(h as u32));
    }
    jclass_mirror_class_id(h)
}

/// The `ClassId` a handle names IF it is a `java.lang.Class` mirror reference.
/// `None` for a `jclass` handle (already decodable), for a non-class object,
/// and for a primitive/void mirror, which has no `ClassId`.
pub(in crate::native::jni) fn jclass_mirror_class_id(h: JObject) -> Option<ClassId> {
    if h == 0 || is_jclass_handle(h) {
        return None;
    }
    let oref = jobject_to_obj(h)?;
    with_shared_vm(|shared| crate::vm::class_id_from_mirror(shared, oref)).flatten()
}

/// The identity a handle has for `IsSameObject` purposes: a class compares by
/// `ClassId` regardless of which of the two encodings it arrived in.
pub(in crate::native::jni) enum JniIdentity {
    Class(ClassId),
    Object(ObjectRef),
    Null,
}

pub(in crate::native::jni) fn jni_identity(h: JObject) -> JniIdentity {
    if h == 0 {
        return JniIdentity::Null;
    }
    if is_jclass_handle(h) {
        return JniIdentity::Class(ClassId::new(h as u32));
    }
    // gci i13/jc: resolved ONCE and with NO keep-alive. This went through
    // `jclass_mirror_class_id` and then `jobject_to_obj`, both of which
    // resolve a WEAK global with the SATB enqueue `jobject_to_obj` owes a
    // caller that hands the object on; `IsSameObject` only compares, and
    // HotSpot's `jni_IsSameObject` resolves with `resolve_no_keepalive`. With
    // the enqueue, the "was it collected?" idiom `IsSameObject(w, NULL)`, run
    // while a concurrent cycle marks (G1, the Generational old cycle), marked
    // the referent live for that cycle: a native polling its weak cache once
    // per cycle kept the referent forever.
    let Some(r) = jobject_to_obj_no_keepalive(h) else {
        return JniIdentity::Null;
    };
    if let Some(id) = with_shared_vm(|shared| crate::vm::class_id_from_mirror(shared, r)).flatten() {
        return JniIdentity::Class(id);
    }
    JniIdentity::Object(r)
}

/// [`jobject_to_obj`] WITHOUT the weak-global keep-alive (gci i13/jc), for a
/// caller that only compares the object and never hands it on or stores it
/// (`IsSameObject`). Every other handle kind resolves exactly as
/// [`jobject_to_obj`] does.
pub(in crate::native::jni) fn jobject_to_obj_no_keepalive(jobj: JObject) -> Option<ObjectRef> {
    if jobj & 1 == 1 && !is_jclass_handle(jobj) {
        return with_shared_vm(|shared| shared.natives.jni_global_refs.lock().resolve(jobj))
            .flatten();
    }
    jobject_to_obj(jobj)
}

/// gci i14/jh (F13 of
/// `gci-i13jc-jni-critical-weak-globals-local-frames-review-findings-FIXED-20261016`):
/// `CRATONVM_JNI_CLASS_GLOBAL_REFS`, default ON since 2026-10-17 (`=0`
/// kills). Off, `NewGlobalRef` /
/// `NewWeakGlobalRef` of a `jclass` hand the same tagged `jclass` back (a
/// class is its own permanent reference here), so `GetObjectRefType` answers
/// `JNILocalRefType` (1) where HotSpot answers 2 / 3, and the reference keeps
/// neither the mirror nor its loader alive. On, they mint a real global /
/// weak handle to the class's `java.lang.Class` mirror
/// ([`class_handle_global_ref`]), which every `jclass` consumer decodes
/// through the mirror ([`jclass_class_id`]). Every later use of such a handle
/// as a `jclass` pays the table lock and the mirror reverse lookup that the
/// tagged id skips; `vi14-jh.list` passed and the user flipped it default ON
/// on 2026-10-17. Read per call (two JNI functions natives call once per
/// cached class): no static.
pub(in crate::native::jni) fn class_global_refs_active() -> bool {
    true
}

/// gci i14/jh: a real global (`weak == false`) or weak global handle to the
/// mirror of the `jclass` handle `obj`, when `enabled`
/// ([`class_global_refs_active`]). `None` -- the caller hands `obj` back, the
/// pre-i14 answer -- when not enabled, when the mirror cannot be had without a
/// collection ([`jclass_handle_mirror`] never collects) or with no JNI context.
pub(in crate::native::jni) fn class_handle_global_ref(obj: JObject, weak: bool, enabled: bool) -> Option<JObject> {
    if !enabled {
        return None;
    }
    let mirror = jclass_handle_mirror(obj)?;
    with_shared_vm(|shared| {
        let mut refs = shared.natives.jni_global_refs.lock();
        if weak {
            refs.add_weak(mirror)
        } else {
            refs.add(mirror)
        }
    })
}

// ---- Index 22: NewGlobalRef ----
pub(in crate::native::jni) extern "C" fn jni_new_global_ref(_env: JNIEnv, obj: JObject) -> JObject {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 {
        return 0;
    }
    // A `jclass` is checked FIRST: it is an exact, lock-free bit test
    // (`is_jclass_handle`), and routing it through `jobject_to_obj` would log a
    // spurious "handle … not found in global ref table" for every odd ClassId.
    // gci i14/jh: under `CRATONVM_JNI_CLASS_GLOBAL_REFS=1` a real global
    // handle to the mirror (HotSpot's `JNIGlobalRefType`).
    if jclass_id_handle(obj) {
        return class_handle_global_ref(obj, false, class_global_refs_active()).unwrap_or(obj);
    }
    // Resolve the object whether it's a local ref or another global ref.
    let oref = match jobject_to_obj(obj) {
        Some(r) => r,
        None => {
            // `NewGlobalRef(FindClass(env, "..."))` is the first thing almost
            // every `JNI_OnLoad` does, and a `jclass` here is a raw `ClassId`,
            // not an object handle — so this returned 0 and the library
            // concluded the JVM had no `java.lang.Object`. Measured on JNA
            // 5.13.0: `libjnidispatch`'s init prints `JNA: Problems loading
            // core IDs: java.lang.Object`, gives up caching `classString` /
            // `MID_String_init`, and every later `jstring` it builds is NULL —
            // surfacing as `Native.<clinit>` throwing
            // `NullPointerException: Cannot invoke "String.split(String)"
            // because "nativeVersion" is null`, which reads like a missing JNA
            // feature and is in fact a dead JNI primitive.
            return 0;
        }
    };
    with_shared_vm(|shared| {
        let handle = shared.natives.jni_global_refs.lock().add(oref);
        Some(handle)
    })
    .flatten()
    .unwrap_or_else(|| {
        tracing::warn!("JNI NewGlobalRef: failed to create global reference for handle {obj:#x}");
        0
    })
}

// ---- Index 23: DeleteGlobalRef ----
pub(in crate::native::jni) extern "C" fn jni_delete_global_ref(_env: JNIEnv, gref: JObject) {
    let _fx = ForeignJniEntry::enter();
    if gref == 0 || gref & 1 == 0 {
        return; // not a global ref handle
    }
    // A `jclass` handed back from `NewGlobalRef` above can have bit 0 set (it is
    // the low bit of the ClassId). Deleting it must be a no-op — the class
    // outlives every ref to it — and must not disturb the ref table.
    if is_jclass_handle(gref) {
        return;
    }
    with_shared_vm(|shared| {
        shared.natives.jni_global_refs.lock().remove(gref);
    });
}

// ---- Index 24: DeleteLocalRef ----
pub(in crate::native::jni) extern "C" fn jni_delete_local_ref(_env: JNIEnv, lref: JObject) {
    // gcd d5/f: a leaf -- it touches only this thread's local-handle table
    // (the deposited snapshot keeps the object until the next deposit, an
    // over-retention only); see `ForeignJniEntry::enter_leaf`.
    // gci i47w lane oz47: table-only -- no relocation check
    // (`ForeignJniEntry::enter_leaf_table_only`).
    let _fx = ForeignJniEntry::enter_leaf_table_only(lref);
    delete_local_ref(lref);
}

// ---- Index 25: IsSameObject ----
pub(in crate::native::jni) extern "C" fn jni_is_same_object(_env: JNIEnv, a: JObject, b: JObject) -> JBoolean {
    let _fx = ForeignJniEntry::enter();
    // A `jclass` does not resolve through `jobject_to_obj` (it is a tagged
    // `ClassId`, see `JCLASS_TAG`), so both sides fell into the
    // `(None, None) => JNI_TRUE` arm and EVERY pair of distinct classes
    // compared equal. Class handles are canonical — the same class always
    // yields the same tagged id — so compare them by id.
    //
    // Both sides go through `jni_identity` so that the two encodings a class
    // can arrive in compare equal: a `jclass` from `FindClass` and the same
    // class arriving as a `Class` PARAMETER are one object on HotSpot, and
    // `IsSameObject` between them is the idiom a native uses to dispatch on a
    // caller-supplied type. See `jclass_class_id` for what depended on it.
    match (jni_identity(a), jni_identity(b)) {
        (JniIdentity::Null, JniIdentity::Null) => JNI_TRUE,
        (JniIdentity::Class(ca), JniIdentity::Class(cb)) if ca == cb => JNI_TRUE,
        // Resolved through the global-ref layer, so a local ref and a global
        // ref to the same object compare equal.
        (JniIdentity::Object(ra), JniIdentity::Object(rb)) if ra == rb => JNI_TRUE,
        _ => JNI_FALSE,
    }
}

// ---- Index 26: NewLocalRef ----
pub(in crate::native::jni) extern "C" fn jni_new_local_ref(_env: JNIEnv, obj: JObject) -> JObject {
    // gcd d10/j: runs no Java (see `ForeignJniEntry::enter_vm_only`).
    let _fx = ForeignJniEntry::enter_vm_only();
    // A `jclass` is permanent and canonical; handing back a "local ref" to it
    // means handing back the same tagged id. Resolving it as an object would
    // yield 0 and lose the class.
    if is_jclass_handle(obj) {
        return obj;
    }
    // Resolve to a raw local-ref pointer (unwrap global-ref tag if present).
    match jobject_to_obj(obj) {
        Some(oref) => {
            let lref = obj_to_jobject(oref);
            if indirect_locals_active() {
                // gc-common w6-g: the same frame `track_local_ref` would use
                // (synthesized when none is open), handed out indirectly.
                JNI_LOCAL_FRAMES.with(|f| {
                    if let Ok(mut stack) = f.try_borrow_mut() {
                        if stack.is_empty() {
                            stack.push(Vec::with_capacity(16));
                        }
                    }
                });
                return record_local_handle_indirect(lref);
            }
            track_local_ref(lref);
            lref
        }
        None => 0,
    }
}

// ---- Index 27: EnsureLocalCapacity ----
///
/// Frames grow on demand, so any non-negative capacity is `JNI_OK`. A negative
/// one is refused with `OutOfMemoryError` pending and `JNI_ERR` (gc-common
/// w10-c; it answered `JNI_OK`). gci i13/jc: HotSpot's own answer (no
/// pending exception, and a cap at `MaxJNILocalCapacity`) is
/// `CRATONVM_JNI_LOCAL_CAPACITY_HOTSPOT` (default ON since 2026-10-17, `=0`
/// kills), see [`local_capacity_verdict_with`].
pub(in crate::native::jni) extern "C" fn jni_ensure_local_capacity(_env: JNIEnv, capacity: JInt) -> JInt {
    let _fx = ForeignJniEntry::enter();
    match local_capacity_verdict(capacity) {
        LocalCapacityVerdict::Ok => JNI_OK,
        LocalCapacityVerdict::Refuse { raise } => {
            if raise {
                refuse_local_capacity("EnsureLocalCapacity", capacity);
            }
            JNI_ERR
        }
    }
}

/// HotSpot 25's `MaxJNILocalCapacity` default: the largest capacity
/// `EnsureLocalCapacity` / `PushLocalFrame` accept (gci i13/jc).
pub(in crate::native::jni) const JNI_MAX_LOCAL_CAPACITY: JInt = 65536;

/// What `EnsureLocalCapacity` / `PushLocalFrame` answer for a capacity (gci
/// i13/jc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::native::jni) enum LocalCapacityVerdict {
    /// `JNI_OK` (and `PushLocalFrame` pushes).
    Ok,
    /// `JNI_ERR`, pushing nothing; `raise`: with `OutOfMemoryError` pending.
    Refuse { raise: bool },
}

/// The verdict for `capacity`. The in-range answer is decided without a flag
/// read (both functions are on native hot paths); only an out-of-range one
/// consults `CRATONVM_JNI_LOCAL_CAPACITY_HOTSPOT`.
pub(in crate::native::jni) fn local_capacity_verdict(capacity: JInt) -> LocalCapacityVerdict {
    if (0..=JNI_MAX_LOCAL_CAPACITY).contains(&capacity) {
        return LocalCapacityVerdict::Ok;
    }
    local_capacity_verdict_with(
        capacity,
        true,
    )
}

/// [`local_capacity_verdict`] with the switch's value given.
///
/// * Switch off (`CRATONVM_JNI_LOCAL_CAPACITY_HOTSPOT=0`): gc-common w10-c's
///   rule -- a negative capacity is refused with `OutOfMemoryError` pending
///   (the JNI specification's text); any other is accepted (frames grow on
///   demand).
/// * `CRATONVM_JNI_LOCAL_CAPACITY_HOTSPOT` on (gci i13/jc, default ON since
///   2026-10-17): HotSpot 25's `jni_EnsureLocalCapacity` /
///   `jni_PushLocalFrame` as read from `jni.cpp` -- `JNI_ERR` with NOTHING
///   pending for a capacity `< 0` or `> MaxJNILocalCapacity` (65536), and
///   `JNI_OK` otherwise. The pending OOME of the off arm is Java-visible: it
///   is thrown into the caller when the native returns.
pub(in crate::native::jni) fn local_capacity_verdict_with(capacity: JInt, hotspot: bool) -> LocalCapacityVerdict {
    if hotspot {
        if (0..=JNI_MAX_LOCAL_CAPACITY).contains(&capacity) {
            LocalCapacityVerdict::Ok
        } else {
            LocalCapacityVerdict::Refuse { raise: false }
        }
    } else if capacity < 0 {
        LocalCapacityVerdict::Refuse { raise: true }
    } else {
        LocalCapacityVerdict::Ok
    }
}

/// The `OutOfMemoryError` HotSpot leaves pending for a negative local
/// capacity (`PushLocalFrame` / `EnsureLocalCapacity`). No context: the
/// `ThrowNew` sentinel, so the refusal is still flagged.
pub(in crate::native::jni) fn refuse_local_capacity(what: &str, capacity: JInt) {
    let raised = with_shared_vm(|shared| {
        raise_jni_oom(shared, &format!("{what} capacity {capacity}"));
    });
    if raised.is_none() {
        JNI_PENDING_EXCEPTION.with(|cell| cell.set(u64::MAX));
    }
}

// ---- Index 31: GetObjectClass ----
pub(in crate::native::jni) extern "C" fn jni_get_object_class(_env: JNIEnv, obj: JObject) -> JClass {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let class_id = match jni_array_class_id(shared, oref) {
            Some(array_class) => array_class,
            None => shared.mem.heap.class_id_of(oref),
        };
        Some(class_id_to_jclass(class_id))
    })
    .flatten()
    .unwrap_or(0)
}

/// The class `GetObjectClass` answers for an ARRAY object, or `None` when `oref`
/// is not an array.
///
/// An array's header carries its COMPONENT's class id (a `String[]` reads as
/// `java/lang/String`, and a primitive array as class 0), so the header id is
/// never the array's own class. Resolve the array class from its descriptor,
/// as `FindClass("[Ljava/lang/String;")` would. If that cannot be loaded, answer
/// `java/lang/Object`, a true supertype, never the component: with the
/// component, `IsInstanceOf(a, GetObjectClass(a))` is false.
pub(in crate::native::jni) fn jni_array_class_id(shared: &SharedVm, oref: ObjectRef) -> Option<ClassId> {
    let desc = crate::runtime::interpreter::array_descriptor_of(shared, oref)?;
    if let Ok(array_class) = shared.load_class_concurrent(&desc) {
        return Some(array_class);
    }
    shared
        .classes
        .class_manager
        .read()
        .find_bootstrap_class_by_name("java/lang/Object")
}

// ---- Index 32: IsInstanceOf ----
pub(in crate::native::jni) extern "C" fn jni_is_instance_of(_env: JNIEnv, obj: JObject, clazz: JClass) -> JBoolean {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 {
        return JNI_TRUE; // null is instanceof any type per JNI spec
    }
    if clazz == 0 {
        return JNI_FALSE;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let target_id = jclass_class_id(clazz);
        // An array's header carries its COMPONENT's class id, so the walk below
        // answered `String[] instanceof String` (and `Integer[] instanceof
        // Number`) true. Ask the `instanceof` opcode's own array rule.
        if let Some(desc) = crate::runtime::interpreter::array_descriptor_of(shared, oref) {
            let target_name = shared
                .classes
                .class_manager
                .read()
                .get_class(target_id)
                .map(|c| c.name.to_string())?;
            let is = crate::runtime::interpreter::array_is_instance_of(shared, &desc, &target_name);
            return Some(if is { JNI_TRUE } else { JNI_FALSE });
        }
        let obj_class_id = shared.mem.heap.class_id_of(oref);
        if obj_class_id == target_id {
            return Some(JNI_TRUE);
        }
        // gcf f3/j: a lambda (a proxy class with no `ClassStore` entry, where
        // the walk below stopped at once and answered false) is an instance
        // of its functional interface, that interface's supertypes and
        // `Object`.
        if let Some(is) = lambda_proxy_assignable_to(shared, obj_class_id, target_id) {
            return Some(if is { JNI_TRUE } else { JNI_FALSE });
        }
        // Walk superclass chain
        let walked = {
            let cm = shared.classes.class_manager.read();
            let mut current = obj_class_id;
            loop {
                let class = cm.get_class(current)?;
                if class.interfaces.contains(&target_id) {
                    break true;
                }
                match class.superclass {
                    Some(sc) => {
                        if sc == target_id {
                            break true;
                        }
                        current = sc;
                    }
                    None => break false,
                }
            }
        };
        // The walk sees only the interfaces each class names DIRECTLY, so a
        // `Foo implements List` object was not an instance of `Collection`.
        // The subtype relation the opcodes use closes over super-interfaces.
        let is = walked
            || crate::runtime::interpreter::class_is_subtype(shared, obj_class_id, target_id);
        Some(if is { JNI_TRUE } else { JNI_FALSE })
    })
    .flatten()
    .unwrap_or(JNI_FALSE)
}

// ---- Index 33: GetMethodID ----
pub(in crate::native::jni) extern "C" fn jni_get_method_id(
    _env: JNIEnv,
    clazz: JClass,
    name: *const c_char,
    sig: *const c_char,
) -> JMethodID {
    let _fx = ForeignJniEntry::enter();
    jni_method_id(clazz, name, sig, false)
}

/// `GetMethodID` / `GetStaticMethodID` (`is_static`). Round 15 wave 7 (lane
/// drainA; N3 of `r15w6-jni6-review-findings`): HotSpot's `get_method_id`
/// looks `<init>` / `<clinit>` up in the class itself only ("Never search
/// superclasses for constructors"), treats a static/instance mismatch as a
/// miss, and on a miss leaves `NoSuchMethodError("[static ]<signature
/// name>.<name><sig>")` pending. This answered an inherited constructor, any
/// static-ness, and a bare 0 with nothing pending (a native's `if (mid ==
/// NULL) return NULL;` then returned to Java with nothing thrown).
/// `CRATONVM_JNI_GET_METHOD_ID_EXACT=0` restores that (read only on a miss or
/// a refusal).
pub(in crate::native::jni) fn jni_method_id(
    clazz: JClass,
    name: *const c_char,
    sig: *const c_char,
    is_static: bool,
) -> JMethodID {
    let name_str = match unsafe { cstr_to_str(name) } {
        Some(s) => s,
        None => return 0,
    };
    let sig_str = match unsafe { cstr_to_str(sig) } {
        Some(s) => s,
        None => return 0,
    };
    if clazz == 0 {
        return 0;
    }
    let found = jni_method_id_lookup(clazz, name_str, sig_str);
    let refused = match found {
        Some((_, false, static_flag)) => static_flag != is_static,
        Some((_, true, _)) => true,
        None => true,
    };
    if !refused {
        return found.map_or(0, |(mid, _, _)| mid);
    }
    // gcf f3/j: a lambda proxy's name too (it was empty: `.run()V`).
    let signature_name =
        with_shared_vm(|shared| jni_class_signature_name(shared, jclass_class_id(clazz)))
            .unwrap_or_default();
    // The error's constructor is Java: a foreign-attached thread must be a
    // counted mutator while it runs (as `jni_find_class_initialize`).
    let _fg = ForeignCallGuard::enter();
    raise_jni_error(
        "java/lang/NoSuchMethodError",
        &format!(
            "{}{signature_name}.{name_str}{sig_str}",
            if is_static { "static " } else { "" }
        ),
    );
    0
}

/// The `methodID` the hierarchy walk resolves for `(clazz, name, sig)`, with
/// whether it is a constructor / initializer inherited from another class
/// (which `GetMethodID` must not answer) and the method's static flag.
/// gcf f3/j: for a lambda proxy class (`SharedVm::alloc_lambda_proxy_id`, a
/// class with no `ClassStore` entry), the classes whose methods it has: its
/// functional interface (the loader-resolved id the bootstrap recorded, else
/// the loaded class of that name) and `java/lang/Object`. `None` for any other
/// class, and for a proxy whose interface is not loaded.
pub(in crate::native::jni) fn lambda_proxy_method_classes(shared: &SharedVm, class_id: ClassId) -> Option<[ClassId; 2]> {
    if !shared.classes.is_lambda_proxy_class(class_id) {
        return None;
    }
    let site = shared.classes.lambda_call_site_for(class_id)?;
    let cm = shared.classes.class_manager.read();
    let iface = site
        .functional_interface_id
        .or_else(|| cm.get_loaded_class_id(&site.functional_interface))?;
    let object = cm.find_bootstrap_class_by_name("java/lang/Object")?;
    Some([iface, object])
}

/// The `jmethodID` of the method named `name` with descriptor `sig` that
/// lambda proxy `class_id` declares itself
/// (`runtime::lambda_proxy::lambda_proxy_methods`: `(proxy id << 32) |
/// index`, the C JVMTI table's id of it); `None` for any other class or
/// method (interpreter round i2 wave 60, lane L1). Cold: `GetMethodID` of a
/// lambda's class while a C JVMTI env is bound, or (interpreter round i6
/// wave 119, lane L3) under `CRATONVM_REFLECT_LAMBDA_PROXY_FACTS` in any
/// build, for every method but the constructor.
#[cold]
#[inline(never)]
pub(in crate::native::jni) fn lambda_proxy_declared_method_id(
    shared: &SharedVm,
    class_id: ClassId,
    name: &str,
    sig: &str,
) -> Option<JMethodID> {
    if !shared.classes.is_lambda_proxy_class(class_id) {
        return None;
    }
    let facts = crate::runtime::lambda_proxy::lambda_proxy_facts(shared, class_id)?;
    let index = facts
        .methods
        .iter()
        .position(|m| *m.name == *name && *m.descriptor == *sig)?;
    Some(encode_method_id(class_id, u16::try_from(index).ok()?))
}

/// The name and descriptor of the method `mid` names: a class store method
/// (`(declaring class id << 32) | index`), or (interpreter round i2 wave 60,
/// lane L1) a method a lambda proxy class declares, which
/// `GetMethodID` answers with the proxy's own id
/// ([`lambda_proxy_declared_method_id`]; the class store has no class for
/// it). `None` for an id that names neither.
pub(in crate::native::jni) fn jni_method_name_and_descriptor(
    shared: &SharedVm,
    mid: JMethodID,
) -> Option<(Arc<str>, Arc<str>)> {
    let (decl_class_id, method_index) = decode_method_id(mid);
    let stored = {
        let cm = shared.classes.class_manager.read();
        cm.class_store
            .get(decl_class_id)
            .and_then(|class| class.methods.get(usize::from(method_index)))
            .map(|method| (method.name.clone(), method.descriptor.clone()))
    };
    stored.or_else(|| lambda_proxy_jni_method(shared, decl_class_id, method_index))
}

/// [`jni_method_name_and_descriptor`]'s lambda proxy arm: method `index` of
/// proxy `class_id`, from the one record of its facts. Cold.
#[cold]
#[inline(never)]
pub(in crate::native::jni) fn lambda_proxy_jni_method(
    shared: &SharedVm,
    class_id: ClassId,
    index: u16,
) -> Option<(Arc<str>, Arc<str>)> {
    if !shared.classes.is_lambda_proxy_class(class_id) {
        return None;
    }
    let facts = crate::runtime::lambda_proxy::lambda_proxy_facts(shared, class_id)?;
    let method = facts.methods.get(usize::from(index))?;
    Some((method.name.clone(), method.descriptor.clone()))
}

/// gcf f3/j: is the lambda proxy class `proxy` assignable to `target`, as
/// HotSpot's hidden lambda class is: to itself, `Object`, its functional
/// interface and that interface's supertypes, and `java.io.Serializable` when
/// the call site passed `FLAG_SERIALIZABLE` (the metafactory then adds that
/// interface; `SharedVm::lambda_proxy_serializability` owns the rule). `None`
/// when `proxy` is no lambda proxy.
pub(in crate::native::jni) fn lambda_proxy_assignable_to(shared: &SharedVm, proxy: ClassId, target: ClassId) -> Option<bool> {
    let [iface, object] = lambda_proxy_method_classes(shared, proxy)?;
    if target == proxy
        || target == object
        || target == iface
        || crate::runtime::interpreter::class_is_subtype(shared, iface, target)
    {
        return Some(true);
    }
    let serializable = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id("java/io/Serializable");
    Some(
        serializable == Some(target)
            && matches!(
                shared.lambda_proxy_serializability(proxy),
                cratonvm_native_api::LambdaSerializability::ByFlag
            ),
    )
}

/// gcf f3/j: the `L<name>;` HotSpot's `NoSuchMethodError` from `GetMethodID`
/// names a class by (`Klass::signature_name`). A lambda proxy is named as its
/// hidden class is: `L<host>$$Lambda+0x<id>;` (the internal form of the
/// `<host>$$Lambda/0x<id>` its `Class.getName()` answers); it was empty.
pub(in crate::native::jni) fn jni_class_signature_name(shared: &SharedVm, class_id: ClassId) -> String {
    let name = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|c| c.name.to_string());
    match name {
        Some(n) if n.starts_with('[') => n,
        Some(n) => format!("L{n};"),
        None => crate::runtime::interpreter::lambda_proxy_display_name(shared, class_id)
            .map(|d| {
                let (host, id) = d.rsplit_once("$$Lambda/").unwrap_or((d.as_str(), ""));
                format!("L{}$$Lambda+{id};", host.replace('.', "/"))
            })
            .unwrap_or_default(),
    }
}

pub(in crate::native::jni) fn jni_method_id_lookup(
    clazz: JClass,
    name_str: &str,
    sig_str: &str,
) -> Option<(JMethodID, bool, bool)> {
    // Round 8 audit fix (CRIT #2): probe the per-VM `LinkResolver` to
    // dedupe the `(class, name, descriptor)` hierarchy walk. JNI
    // `GetMethodID` is hot on any C-extension entry path (Hibernate
    // ByteBuddy proxies, JNI-heavy libraries like SQLite-JDBC, native
    // image bridges) and the same triple is queried thousands of times
    // per cold start. Cache hits avoid the full `find_method_recursive`
    // walk + the per-class linear `methods` position scan.
    with_shared_vm(|shared| {
        use cratonvm_classloading::resolution::ResolvedMember;
        let requested = jclass_class_id(clazz);
        // Interpreter round i2 wave 60, lane L1 (`CRATONVM_JNI_LAMBDA_METHOD_IDS`,
        // stage 2 of
        // `docs/feature-designs/real-jdk-mode.md`):
        // a method the lambda's class itself declares (its constructor, its
        // functional method, a bridge) is answered with the proxy's own
        // `jmethodID`, `(proxy id << 32) | index` -- the id the C JVMTI table
        // lists it by (`GetClassMethods`, a stack frame) -- as HotSpot answers
        // the hidden class's method. A call through it dispatches on the
        // receiver ([`jni_method_name_and_descriptor`]). The inherited
        // methods (`Object`'s, the interface's defaults) keep the search below.
        #[cfg(feature = "experimental-debug")]
        if shared.debug.debugger_gates.jni_lambda_method_ids_armed() {
            if let Some(mid) = lambda_proxy_declared_method_id(shared, requested, name_str, sig_str)
            {
                return Some((mid, false, false));
            }
        }
        // Interpreter round i6 wave 119, lane L3 (`CRATONVM_REFLECT_LAMBDA_PROXY_FACTS`,
        // default off; proposal i55-L1): the same answer without a C JVMTI env
        // and in a build without `experimental-debug`, so `GetMethodID`,
        // `ToReflectedMethod` and `FromReflectedMethod` agree with core
        // reflection's record-built listing of the class (HotSpot answers the
        // hidden class's own method). Not the constructor: `NewObject` with a
        // proxy's `<init>` id had no allocation path until the wave-120
        // follow-up; it has now.
        // Wave 120 follow-up: the constructor's id too, now that `AllocObject`
        // and `NewObject*` of a lambda class build one
        // ([`lambda_proxy_jni_construct`]).
        if cratonvm_types::flags().natives.reflect_lambda_proxy_facts {
            if let Some(mid) = lambda_proxy_declared_method_id(shared, requested, name_str, sig_str)
            {
                return Some((mid, false, false));
            }
        }
        // gcf f3/j: a lambda's class (`GetObjectClass` of a lambda) is a
        // VM-internal proxy id with no `ClassStore` entry, so the walk below
        // found nothing and `GetMethodID(GetObjectClass(lambda), "run",
        // "()V")` raised `NoSuchMethodError`. Its methods are its functional
        // interface's and `Object`'s (HotSpot's hidden class implements the
        // one and extends the other); the id names the method there, and a
        // call through it dispatches on the receiver
        // (`invoke_on_class_shared`'s lambda arm).
        let proxy_search = lambda_proxy_method_classes(shared, requested);
        let search: &[ClassId] = match &proxy_search {
            Some(classes) => classes,
            None => std::slice::from_ref(&requested),
        };
        let mut resolved = ResolvedMember::NotFound;
        for &class_id in search {
            resolved = {
                let cm = shared.classes.class_manager.read();
                shared
                    .classes
                    .link_resolver
                    .resolve_or_compute(class_id, name_str, sig_str, || {
                        let result =
                            find_method_recursive(class_id, name_str, sig_str, &cm.class_store)
                                .and_then(|(_, declaring)| {
                                    let decl = cm.class_store.get(declaring)?;
                                    let idx = decl.methods.iter().position(|m| {
                                        &*m.name == name_str && &*m.descriptor == sig_str
                                    })?;
                                    Some((declaring, idx as u32))
                                });
                        let resolved = match result {
                            Some((d, i)) => ResolvedMember::Method {
                                declaring_class_id: d,
                                index: i,
                            },
                            None => ResolvedMember::NotFound,
                        };
                        (
                            cratonvm_types::intern_arc(name_str),
                            cratonvm_types::intern_arc(sig_str),
                            resolved,
                        )
                    })
            };
            if matches!(resolved, ResolvedMember::Method { .. }) {
                break;
            }
        }
        match resolved {
            ResolvedMember::Method {
                declaring_class_id,
                index,
            } => {
                let inherited_initializer = declaring_class_id != requested
                    && (name_str == "<init>" || name_str == "<clinit>");
                let static_flag = shared
                    .classes
                    .class_manager
                    .read()
                    .class_store
                    .get(declaring_class_id)
                    .and_then(|c| c.methods.get(index as usize))
                    .map_or(false, |m| m.is_static());
                // gcf f3/j: a static method of the functional interface is
                // not a method of the lambda's class (static interface methods
                // are not inherited; HotSpot's lookup skips them), so
                // `GetStaticMethodID(lambda class, ..)` misses it.
                if static_flag && proxy_search.is_some() {
                    return None;
                }
                Some((
                    encode_method_id(declaring_class_id, index as u16),
                    inherited_initializer,
                    static_flag,
                ))
            }
            _ => None,
        }
    })
    .flatten()
}

// ---------------------------------------------------------------------------
// Indices 34-63: Call<Type>MethodA (virtual instance dispatch)
// ---------------------------------------------------------------------------
//
// The varargs (`CallObjectMethod`) and va_list (`CallObjectMethodV`) forms
// cannot be implemented portably in safe Rust.  We provide the *A variants
// (array form) for all return types, plus stub no-ops at the varargs slots
// so that the function table is correctly sized.

pub(in crate::native::jni) extern "C" fn jni_call_object_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JObject {
    let _fx = ForeignJniEntry::enter();
    match jni_call_instance(obj, mid, args) {
        Some(Value::Object(Some(r))) => new_local_handle(r),
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_boolean_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JBoolean {
    let _fx = ForeignJniEntry::enter();
    match jni_call_instance(obj, mid, args) {
        Some(Value::Int(v)) => v as JBoolean,
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_byte_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JByte {
    let _fx = ForeignJniEntry::enter();
    match jni_call_instance(obj, mid, args) {
        Some(Value::Int(v)) => v as JByte,
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_char_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JChar {
    let _fx = ForeignJniEntry::enter();
    match jni_call_instance(obj, mid, args) {
        Some(Value::Int(v)) => v as JChar,
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_short_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JShort {
    let _fx = ForeignJniEntry::enter();
    match jni_call_instance(obj, mid, args) {
        Some(Value::Int(v)) => v as JShort,
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_int_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JInt {
    let _fx = ForeignJniEntry::enter();
    match jni_call_instance(obj, mid, args) {
        Some(Value::Int(v)) => v,
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_long_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JLong {
    let _fx = ForeignJniEntry::enter();
    match jni_call_instance(obj, mid, args) {
        Some(Value::Long(v)) => v,
        Some(Value::Int(v)) => v as JLong,
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_float_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JFloat {
    let _fx = ForeignJniEntry::enter();
    match jni_call_instance(obj, mid, args) {
        Some(Value::Float(v)) => v,
        _ => 0.0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_double_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) -> JDouble {
    let _fx = ForeignJniEntry::enter();
    match jni_call_instance(obj, mid, args) {
        Some(Value::Double(v)) => v,
        Some(Value::Float(v)) => v as JDouble,
        _ => 0.0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_void_method_a(
    _env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    args: *const JValue,
) {
    let _fx = ForeignJniEntry::enter();
    jni_call_instance(obj, mid, args);
}

// ---------------------------------------------------------------------------
// Indices 64-93: CallNonvirtual<Type>MethodA
// ---------------------------------------------------------------------------

pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_object_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JObject {
    let _fx = ForeignJniEntry::enter();
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Object(Some(r))) => new_local_handle(r),
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_boolean_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JBoolean {
    let _fx = ForeignJniEntry::enter();
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Int(v)) => v as JBoolean,
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_byte_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JByte {
    let _fx = ForeignJniEntry::enter();
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Int(v)) => v as JByte,
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_char_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JChar {
    let _fx = ForeignJniEntry::enter();
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Int(v)) => v as JChar,
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_short_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JShort {
    let _fx = ForeignJniEntry::enter();
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Int(v)) => v as JShort,
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_int_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JInt {
    let _fx = ForeignJniEntry::enter();
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Int(v)) => v,
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_long_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JLong {
    let _fx = ForeignJniEntry::enter();
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Long(v)) => v,
        Some(Value::Int(v)) => v as JLong,
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_float_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JFloat {
    let _fx = ForeignJniEntry::enter();
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Float(v)) => v,
        _ => 0.0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_double_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JDouble {
    let _fx = ForeignJniEntry::enter();
    match jni_call_nonvirtual(obj, clazz, mid, args) {
        Some(Value::Double(v)) => v,
        Some(Value::Float(v)) => v as JDouble,
        _ => 0.0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_void_method_a(
    _env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) {
    let _fx = ForeignJniEntry::enter();
    jni_call_nonvirtual(obj, clazz, mid, args);
}

// ---------------------------------------------------------------------------
// Indices 114-143: CallStatic<Type>MethodA
// ---------------------------------------------------------------------------

pub(in crate::native::jni) extern "C" fn jni_call_static_object_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JObject {
    let _fx = ForeignJniEntry::enter();
    match jni_call_static(clazz, mid, args) {
        Some(Value::Object(Some(r))) => new_local_handle(r),
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_static_boolean_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JBoolean {
    let _fx = ForeignJniEntry::enter();
    match jni_call_static(clazz, mid, args) {
        Some(Value::Int(v)) => v as JBoolean,
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_static_byte_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JByte {
    let _fx = ForeignJniEntry::enter();
    match jni_call_static(clazz, mid, args) {
        Some(Value::Int(v)) => v as JByte,
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_static_char_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JChar {
    let _fx = ForeignJniEntry::enter();
    match jni_call_static(clazz, mid, args) {
        Some(Value::Int(v)) => v as JChar,
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_static_short_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JShort {
    let _fx = ForeignJniEntry::enter();
    match jni_call_static(clazz, mid, args) {
        Some(Value::Int(v)) => v as JShort,
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_static_int_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JInt {
    let _fx = ForeignJniEntry::enter();
    match jni_call_static(clazz, mid, args) {
        Some(Value::Int(v)) => v,
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_static_long_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JLong {
    let _fx = ForeignJniEntry::enter();
    match jni_call_static(clazz, mid, args) {
        Some(Value::Long(v)) => v,
        Some(Value::Int(v)) => v as JLong,
        _ => 0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_static_float_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JFloat {
    let _fx = ForeignJniEntry::enter();
    match jni_call_static(clazz, mid, args) {
        Some(Value::Float(v)) => v,
        _ => 0.0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_static_double_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JDouble {
    let _fx = ForeignJniEntry::enter();
    match jni_call_static(clazz, mid, args) {
        Some(Value::Double(v)) => v,
        Some(Value::Float(v)) => v as JDouble,
        _ => 0.0,
    }
}

pub(in crate::native::jni) extern "C" fn jni_call_static_void_method_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) {
    let _fx = ForeignJniEntry::enter();
    jni_call_static(clazz, mid, args);
}

// ---- Index 94: GetFieldID ----
pub(in crate::native::jni) extern "C" fn jni_get_field_id(
    _env: JNIEnv,
    clazz: JClass,
    name: *const c_char,
    sig: *const c_char,
) -> JFieldID {
    let _fx = ForeignJniEntry::enter();
    jni_field_id(clazz, name, sig, false)
}

/// `GetFieldID` / `GetStaticFieldID` (`is_static`).
///
/// gcf f2/j: HotSpot's `jni_GetFieldID` / `jni_GetStaticFieldID` look the
/// field up with its static-ness (`InstanceKlass::find_field(.., is_static,
/// ..)`), so an instance field asked as static (or the reverse) is a miss,
/// and a miss leaves `NoSuchFieldError(<name>)` pending. This answered the
/// field whatever its static-ness, and a bare 0 with nothing pending on a
/// miss (a native's `if (fid == NULL) return;` then returned to Java with
/// nothing thrown), the shape round 15 fixed for `GetMethodID`
/// (`jni_method_id`). `CRATONVM_JNI_GET_FIELD_ID_EXACT=0` restores the old
/// answers (read only on a miss or a refusal). Decided by
/// `GcfF2jJniRegionProbe`'s `field-id` lines.
pub(in crate::native::jni) fn jni_field_id(
    clazz: JClass,
    name: *const c_char,
    sig: *const c_char,
    is_static: bool,
) -> JFieldID {
    let name_str = match unsafe { cstr_to_str(name) } {
        Some(s) => s,
        None => return 0,
    };
    // Round 9 audit fix (HIGH #5): include the JNI-supplied signature in
    // the LinkResolver cache key. The previous version dropped the
    // signature and keyed only on `(class_id, name)`; for a class that
    // shadows an inherited field with a different *type* (legal in JVMS
    // §5.4.3.2 — fields are uniquely identified by `(name, descriptor)`),
    // the first JNI `GetFieldID` query would populate the cache with one
    // resolution and every subsequent call — even one passing a
    // different signature — would short-circuit to that wrong entry.
    //
    // Tolerate a null signature by mapping it to "" — pre-fix callers
    // could legitimately pass NULL since the parameter was ignored; we
    // keep that behaviour by treating NULL as a distinct ("no signature
    // assertion") cache key.
    let sig_str = unsafe { cstr_to_str(sig) }.unwrap_or("");
    if clazz == 0 {
        return 0;
    }
    // Round 8 audit fix (CRIT #2): probe the per-VM `LinkResolver`.
    let found = with_shared_vm(|shared| {
        use cratonvm_classloading::resolution::ResolvedMember;
        let requested = jclass_class_id(clazz);
        // gcf f4/j: a lambda's captured values are the instance fields
        // `arg$1`, `arg$2`, ... of HotSpot's hidden class; here they are the
        // proxy object's slots 0, 1, ... (`allocate_lambda_proxy_from_values`).
        if !is_static {
            if let Some(fid) = lambda_capture_field_id(shared, requested, name_str, sig_str) {
                return Some((fid, false));
            }
        }
        // gcf f3/j: a lambda's class (no `ClassStore` entry: every lookup
        // missed) sees its functional interface's fields (constants, static)
        // as HotSpot's hidden class does; `Object` declares none.
        let class_id =
            lambda_proxy_method_classes(shared, requested).map_or(requested, |[iface, _]| iface);
        let resolved = {
            let cm = shared.classes.class_manager.read();
            shared
                .classes
                .link_resolver
                .resolve_or_compute(class_id, name_str, sig_str, || {
                    // Round 9 fixed the cache KEY to include the signature; the
                    // resolution underneath it still discarded the signature and
                    // matched on the name alone, so two `GetFieldID` calls with
                    // different signatures got two cache entries holding the
                    // same (possibly wrong) field. JVMS §5.4.3.2 — and the
                    // comment above — say the key is `(name, descriptor)`; use
                    // it. An empty `sig_str` is the documented NULL-signature
                    // case and keeps the name-only search.
                    //
                    // Falls back to name-only, counted, when no field of that
                    // exact pair exists, for the reason `locate_field` does.
                    let result = if sig_str.is_empty() {
                        find_field_recursive(class_id, name_str, &cm.class_store)
                    } else {
                        cratonvm_classloading::find_field_recursive_by_descriptor(
                            class_id,
                            name_str,
                            sig_str,
                            &cm.class_store,
                        )
                        .or_else(|| {
                            let name_only =
                                find_field_recursive(class_id, name_str, &cm.class_store);
                            if name_only.is_some() {
                                crate::runtime::resolve::FIELD_RESOLUTION_DESCRIPTOR_FALLBACKS
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            name_only
                        })
                    };
                    let resolved = match result {
                        Some((field_index, field, declaring)) => ResolvedMember::Field {
                            declaring_class_id: declaring,
                            absolute_index: field_index as u32,
                            is_static: field.access_flags.contains(
                                cratonvm_reader::class_access_flags::FieldAccessFlags::STATIC,
                            ),
                        },
                        None => ResolvedMember::NotFound,
                    };
                    (
                        cratonvm_types::intern_arc(name_str),
                        cratonvm_types::intern_arc(sig_str),
                        resolved,
                    )
                })
        };
        match resolved {
            // Interpreter round i1 wave 52, lane L1: a static field's id
            // carries [`STATIC_FIELD_ID_MARK`], as the C JVMTI table's do, so
            // `GetFieldName` of a `GetStaticFieldID` result names the static
            // field and not the instance field of the same slot number.
            ResolvedMember::Field {
                declaring_class_id,
                absolute_index,
                is_static: field_is_static,
            } => Some((
                encode_field_id(declaring_class_id, absolute_index as usize)
                    | if field_is_static {
                        STATIC_FIELD_ID_MARK
                    } else {
                        0
                    },
                field_is_static,
            )),
            _ => None,
        }
    })
    .flatten();
    if let Some((fid, field_is_static)) = found {
        if field_is_static == is_static {
            return fid;
        }
    }
    // The error's constructor is Java: a foreign-attached thread must be a
    // counted mutator while it runs (as in `jni_method_id`).
    // gcf f3/j: HotSpot 25's text, read off its own JNI table (2026-10-01):
    // `GetStaticFieldID` names the field alone (`nope`), `GetFieldID` the
    // class's external name, the field and its signature
    // (`java.lang.Integer.nope I`).
    let msg = if is_static {
        name_str.to_string()
    } else {
        let class_id = jclass_class_id(clazz);
        let class =
            with_shared_vm(|shared| jni_class_external_name(shared, class_id)).unwrap_or_default();
        format!("{class}.{name_str} {sig_str}")
    };
    let _fg = ForeignCallGuard::enter();
    raise_jni_error("java/lang/NoSuchFieldError", &msg);
    0
}

/// gcf f4/j: the field id of a lambda's captured value `arg$<n>` (1-based,
/// HotSpot's name for it), or `None` when `class_id` is no lambda proxy, the
/// name is not `arg$<n>` (no leading zero), the proxy has no `n`th capture, or
/// `sig` is not of the capture's kind. The proxy records each capture's kind
/// only (`LambdaCallSite::capture_types`: a primitive's letter, `L` for any
/// reference), so a reference capture accepts any `L...;` / `[...` signature,
/// where HotSpot also checks the exact type. The id names slot `n - 1` of the
/// proxy object, which `Get<Type>Field` reads as any instance field.
pub(in crate::native::jni) fn lambda_capture_field_id(
    shared: &SharedVm,
    class_id: ClassId,
    name: &str,
    sig: &str,
) -> Option<JFieldID> {
    if !shared.classes.is_lambda_proxy_class(class_id) {
        return None;
    }
    let digits = name.strip_prefix("arg$")?;
    let n: usize = digits.parse().ok()?;
    if n == 0 || n.to_string() != digits {
        return None;
    }
    let site = shared.classes.lambda_call_site_for(class_id)?;
    let kind = *site.capture_types.get(n - 1)?;
    let sig_ok = match kind {
        'L' | '[' => sig.starts_with('L') || sig.starts_with('['),
        prim => sig.len() == 1 && sig.starts_with(prim),
    };
    sig_ok.then(|| encode_field_id(class_id, n - 1))
}

/// gcf f3/j: a class's external (binary, dotted) name as HotSpot's
/// `Klass::external_name` gives it: `java.lang.Integer`, `[I`; a lambda
/// proxy's `<host>$$Lambda/0x<id>`.
pub(in crate::native::jni) fn jni_class_external_name(shared: &SharedVm, class_id: ClassId) -> String {
    let name = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|c| c.name.to_string());
    match name {
        Some(n) => crate::runtime::exceptions::hotspot_external_name(&n),
        None => crate::runtime::interpreter::lambda_proxy_display_name(shared, class_id)
            .unwrap_or_default(),
    }
}

// ---- Index 95: GetObjectField ----
pub(in crate::native::jni) extern "C" fn jni_get_object_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JObject {
    // gcd d10/j: runs no Java (see `ForeignJniEntry::enter_vm_only`).
    let _fx = ForeignJniEntry::enter_vm_only();
    if obj == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        // zt t32 (2b): a referent read is resurrection-blocked until the
        // after-release referent clear has run.
        let read = shared.mem.heap.get_field(oref, field_index);
        let read = crate::memory::ref_discovery::gate_referent_read(
            shared,
            oref,
            field_index,
            read,
            || shared.mem.heap.get_field(oref, field_index),
        );
        match read {
            Value::Object(Some(r)) => {
                // zt t1/mk: HotSpot's jni_GetObjectField keep-alive for the
                // referent offset (slot 0 of a Reference subclass; the class
                // named at GetFieldID may be a subclass, so the slot decides).
                // ZGC: the zt arm (zt t23/zb7: the load barrier's mark under
                // the color flip, SATB otherwise). G1 / Generational: dev's
                // referent-read keep-alive (`ref_discovery`, slot resolved).
                if shared.mem.heap.is_zgc() {
                    if field_index == 0 {
                        shared.mem.heap.referent_keep_alive(Value::Object(Some(r)));
                    }
                } else {
                    crate::memory::ref_discovery::keep_alive_referent_read(
                        shared,
                        oref,
                        field_index,
                        Value::Object(Some(r)),
                    );
                }
                Some(new_local_handle(r))
            }
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 96: GetBooleanField ----
pub(in crate::native::jni) extern "C" fn jni_get_boolean_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JBoolean {
    let _fx = ForeignJniEntry::enter();
    get_int_field_raw(obj, field_id) as JBoolean
}

// ---- Index 97: GetByteField ----
pub(in crate::native::jni) extern "C" fn jni_get_byte_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JByte {
    let _fx = ForeignJniEntry::enter();
    get_int_field_raw(obj, field_id) as JByte
}

// ---- Index 98: GetCharField ----
pub(in crate::native::jni) extern "C" fn jni_get_char_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JChar {
    let _fx = ForeignJniEntry::enter();
    get_int_field_raw(obj, field_id) as JChar
}

// ---- Index 99: GetShortField ----
pub(in crate::native::jni) extern "C" fn jni_get_short_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JShort {
    let _fx = ForeignJniEntry::enter();
    get_int_field_raw(obj, field_id) as JShort
}

// ---- Index 100: GetIntField ----
pub(in crate::native::jni) extern "C" fn jni_get_int_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JInt {
    let _fx = ForeignJniEntry::enter();
    get_int_field_raw(obj, field_id)
}

// ---- Index 101: GetLongField ----
pub(in crate::native::jni) extern "C" fn jni_get_long_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JLong {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        match shared.mem.heap.get_field(oref, field_index) {
            Value::Long(l) => Some(l),
            Value::Int(i) => Some(i as JLong),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 102: GetFloatField ----
pub(in crate::native::jni) extern "C" fn jni_get_float_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JFloat {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 || field_id == 0 {
        return 0.0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        match shared.mem.heap.get_field(oref, field_index) {
            Value::Float(f) => Some(f),
            _ => Some(0.0),
        }
    })
    .flatten()
    .unwrap_or(0.0)
}

// ---- Index 103: GetDoubleField ----
pub(in crate::native::jni) extern "C" fn jni_get_double_field(_env: JNIEnv, obj: JObject, field_id: JFieldID) -> JDouble {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 || field_id == 0 {
        return 0.0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        match shared.mem.heap.get_field(oref, field_index) {
            Value::Double(d) => Some(d),
            _ => Some(0.0),
        }
    })
    .flatten()
    .unwrap_or(0.0)
}

// ---- Index 104: SetObjectField ----
pub(in crate::native::jni) extern "C" fn jni_set_object_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JObject) {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 || field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        // The value first: a `jclass` value may mint its mirror
        // (`jclass_handle_mirror`), and `oref` must not be held across that.
        let value = match jobject_to_obj(val) {
            Some(r) => Value::Object(Some(r)),
            None => Value::Object(None),
        };
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        crate::runtime::frozen_pages::note_slot_write(shared, oref, field_index, true);
        shared.mem.heap.set_field(oref, field_index, value);
        // write_barrier fires automatically inside set_field
        Some(())
    });
}

// ---- Index 105: SetBooleanField ----
pub(in crate::native::jni) extern "C" fn jni_set_boolean_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JBoolean) {
    let _fx = ForeignJniEntry::enter();
    set_int_field_raw(obj, field_id, val as i32);
}

// ---- Index 106: SetByteField ----
pub(in crate::native::jni) extern "C" fn jni_set_byte_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JByte) {
    let _fx = ForeignJniEntry::enter();
    set_int_field_raw(obj, field_id, val as i32);
}

// ---- Index 107: SetCharField ----
pub(in crate::native::jni) extern "C" fn jni_set_char_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JChar) {
    let _fx = ForeignJniEntry::enter();
    set_int_field_raw(obj, field_id, val as i32);
}

// ---- Index 108: SetShortField ----
pub(in crate::native::jni) extern "C" fn jni_set_short_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JShort) {
    let _fx = ForeignJniEntry::enter();
    set_int_field_raw(obj, field_id, val as i32);
}

// ---- Index 109: SetIntField ----
pub(in crate::native::jni) extern "C" fn jni_set_int_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JInt) {
    let _fx = ForeignJniEntry::enter();
    set_int_field_raw(obj, field_id, val);
}

// ---- Index 110: SetLongField ----
pub(in crate::native::jni) extern "C" fn jni_set_long_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JLong) {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 || field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(obj)?;
        let (_, field_index) = decode_field_id(field_id);
        // Long-smuggle mint chokepoint: native code storing a raw jobject
        // handle into a Java long field (see smuggled_longs). Strict probe so
        // ordinary numeric stores don't register.
        let bits = val as u64;
        if bits != 0
            && bits & 0x7 == 0
            && shared.mem.heap.is_object_address(bits as usize).is_some()
        {
            crate::memory::smuggled_longs::record_minted_long(&shared.mem.heap, bits);
        }
        crate::runtime::frozen_pages::note_slot_write(shared, oref, field_index, true);
        shared
            .mem
            .heap
            .set_field(oref, field_index, Value::Long(val));
        Some(())
    });
}

// ---- Index 111: SetFloatField ----
pub(in crate::native::jni) extern "C" fn jni_set_float_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JFloat) {
    let _fx = ForeignJniEntry::enter();
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
            .set_field(oref, field_index, Value::Float(val));
        Some(())
    });
}

// ---- Index 112: SetDoubleField ----
pub(in crate::native::jni) extern "C" fn jni_set_double_field(_env: JNIEnv, obj: JObject, field_id: JFieldID, val: JDouble) {
    let _fx = ForeignJniEntry::enter();
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
            .set_field(oref, field_index, Value::Double(val));
        Some(())
    });
}

// ---- Index 113: GetStaticMethodID ----
pub(in crate::native::jni) extern "C" fn jni_get_static_method_id(
    _env: JNIEnv,
    clazz: JClass,
    name: *const c_char,
    sig: *const c_char,
) -> JMethodID {
    let _fx = ForeignJniEntry::enter();
    // Same resolution as instance methods — static dispatch is by method index
    // -- with the static-ness check and the `static ` message prefix.
    jni_method_id(clazz, name, sig, true)
}

// ---- Index 144: GetStaticFieldID ----
pub(in crate::native::jni) extern "C" fn jni_get_static_field_id(
    _env: JNIEnv,
    clazz: JClass,
    name: *const c_char,
    sig: *const c_char,
) -> JFieldID {
    let _fx = ForeignJniEntry::enter();
    jni_field_id(clazz, name, sig, true)
}

// ---- Indices 145-150: GetStatic*Field ----
pub(in crate::native::jni) extern "C" fn jni_get_static_object_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JObject {
    let _fx = ForeignJniEntry::enter();
    if clazz == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let statics = shared.classes.statics.read();
        let fields = statics.get(&decl_class_id)?;
        match fields.load_cell(field_index).as_ref() {
            Some(Value::Object(Some(r))) => Some(new_local_handle(*r)),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

pub(in crate::native::jni) extern "C" fn jni_get_static_boolean_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JBoolean {
    let _fx = ForeignJniEntry::enter();
    get_static_int_raw(clazz, field_id) as JBoolean
}

pub(in crate::native::jni) extern "C" fn jni_get_static_byte_field(_env: JNIEnv, clazz: JClass, field_id: JFieldID) -> JByte {
    let _fx = ForeignJniEntry::enter();
    get_static_int_raw(clazz, field_id) as JByte
}

pub(in crate::native::jni) extern "C" fn jni_get_static_char_field(_env: JNIEnv, clazz: JClass, field_id: JFieldID) -> JChar {
    let _fx = ForeignJniEntry::enter();
    get_static_int_raw(clazz, field_id) as JChar
}

pub(in crate::native::jni) extern "C" fn jni_get_static_short_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JShort {
    let _fx = ForeignJniEntry::enter();
    get_static_int_raw(clazz, field_id) as JShort
}

pub(in crate::native::jni) extern "C" fn jni_get_static_int_field(_env: JNIEnv, clazz: JClass, field_id: JFieldID) -> JInt {
    let _fx = ForeignJniEntry::enter();
    get_static_int_raw(clazz, field_id)
}

pub(in crate::native::jni) extern "C" fn jni_get_static_long_field(_env: JNIEnv, clazz: JClass, field_id: JFieldID) -> JLong {
    let _fx = ForeignJniEntry::enter();
    if clazz == 0 || field_id == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let statics = shared.classes.statics.read();
        let fields = statics.get(&decl_class_id)?;
        match fields.load_cell(field_index).as_ref() {
            Some(Value::Long(l)) => Some(*l),
            Some(Value::Int(i)) => Some(*i as JLong),
            _ => Some(0),
        }
    })
    .flatten()
    .unwrap_or(0)
}

pub(in crate::native::jni) extern "C" fn jni_get_static_float_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JFloat {
    let _fx = ForeignJniEntry::enter();
    if clazz == 0 || field_id == 0 {
        return 0.0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let statics = shared.classes.statics.read();
        let fields = statics.get(&decl_class_id)?;
        match fields.load_cell(field_index).as_ref() {
            Some(Value::Float(f)) => Some(*f),
            _ => Some(0.0),
        }
    })
    .flatten()
    .unwrap_or(0.0)
}

pub(in crate::native::jni) extern "C" fn jni_get_static_double_field(
    _env: JNIEnv,
    clazz: JClass,
    field_id: JFieldID,
) -> JDouble {
    let _fx = ForeignJniEntry::enter();
    if clazz == 0 || field_id == 0 {
        return 0.0;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let statics = shared.classes.statics.read();
        let fields = statics.get(&decl_class_id)?;
        match fields.load_cell(field_index).as_ref() {
            Some(Value::Double(d)) => Some(*d),
            _ => Some(0.0),
        }
    })
    .flatten()
    .unwrap_or(0.0)
}

// ---- Indices 154-159: SetStatic*Field ----
pub(in crate::native::jni) extern "C" fn jni_set_static_object_field(
    _env: JNIEnv,
    _clazz: JClass,
    field_id: JFieldID,
    val: JObject,
) {
    let _fx = ForeignJniEntry::enter();
    if field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        if shared
            .classes
            .frozen
            .has_snapshots
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            crate::runtime::frozen_pages::invalidate_class(shared, decl_class_id.as_u32());
        }
        let value = match jobject_to_obj(val) {
            Some(r) => Value::Object(Some(r)),
            None => Value::Object(None),
        };
        let mut statics = shared.classes.statics.write();
        if let Some(fields) = statics.get_mut(&decl_class_id) {
            if field_index < fields.len() {
                // SATB pre-barrier for the overwritten reference, as
                // `set_static_shared` does: statics live outside the heap, so
                // no collector-side barrier sees this store (2026-09-23).
                if let Some(old) = fields.load_cell(field_index) {
                    shared.mem.heap.satb_barrier(old);
                }
                fields.store_cell(field_index, value);
            }
        }
    });
}

pub(in crate::native::jni) extern "C" fn jni_set_static_int_field(
    _env: JNIEnv,
    _clazz: JClass,
    field_id: JFieldID,
    val: JInt,
) {
    let _fx = ForeignJniEntry::enter();
    set_static_int_raw(field_id, val);
}

pub(in crate::native::jni) extern "C" fn jni_set_static_long_field(
    _env: JNIEnv,
    _clazz: JClass,
    field_id: JFieldID,
    val: JLong,
) {
    let _fx = ForeignJniEntry::enter();
    if field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        // Mint chokepoint, the static twin of `SetLongField`'s.
        mint_if_smuggled_jlong(&shared.mem.heap, val as u64);
        let mut statics = shared.classes.statics.write();
        if let Some(fields) = statics.get_mut(&decl_class_id) {
            if field_index < fields.len() {
                fields.store_cell(field_index, Value::Long(val));
            }
        }
    });
}

pub(in crate::native::jni) extern "C" fn jni_set_static_float_field(
    _env: JNIEnv,
    _clazz: JClass,
    field_id: JFieldID,
    val: JFloat,
) {
    let _fx = ForeignJniEntry::enter();
    if field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let mut statics = shared.classes.statics.write();
        if let Some(fields) = statics.get_mut(&decl_class_id) {
            if field_index < fields.len() {
                fields.store_cell(field_index, Value::Float(val));
            }
        }
    });
}

pub(in crate::native::jni) extern "C" fn jni_set_static_double_field(
    _env: JNIEnv,
    _clazz: JClass,
    field_id: JFieldID,
    val: JDouble,
) {
    let _fx = ForeignJniEntry::enter();
    if field_id == 0 {
        return;
    }
    with_shared_vm(|shared| {
        let (decl_class_id, field_index) = decode_field_id(field_id);
        let mut statics = shared.classes.statics.write();
        if let Some(fields) = statics.get_mut(&decl_class_id) {
            if field_index < fields.len() {
                fields.store_cell(field_index, Value::Double(val));
            }
        }
    });
}

// ---- Index 167: NewStringUTF ----
pub(in crate::native::jni) extern "C" fn jni_new_string_utf(_env: JNIEnv, chars: *const c_char) -> JString {
    // gcd d10/j: allocates without a collection and runs no Java; its
    // `OutOfMemoryError` goes through `with_jni_context`, which escalates the
    // entry (see `ForeignJniEntry::enter_vm_only`).
    let _fx = ForeignJniEntry::enter_vm_only();
    // gcf f1/j: the argument is MODIFIED UTF-8, read to its NUL with no length
    // cap, as HotSpot reads it. Standard UTF-8 (every input this accepted
    // before) takes the old path unchanged; what only modified UTF-8 can say
    // -- `C0 80` for U+0000, a supplementary character as a CESU-8 surrogate
    // pair, a lone surrogate -- is decoded to its UTF-16 units. Both used to
    // answer NULL with nothing pending, so `NewStringUTF(GetStringUTFChars(s))`
    // lost every string holding a NUL or an emoji, and any string over 64 KiB.
    let Some(bytes) = (unsafe { cstr_bytes(chars) }) else {
        return 0;
    };
    // Refuse impossible storage before decoding into a host Vec.
    if bytes.len() >= (i32::MAX as usize / 2) {
        let (units, latin1) = jni_utf8_storage_shape(bytes);
        if with_shared_vm(|shared| jni_string_storage_refused(shared, units, latin1)).unwrap_or(false) {
            return 0;
        }
    }
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => {
            let Some(units) = decode_modified_utf8(bytes) else {
                return 0;
            };
            return with_shared_vm(|shared| {
                match jni_alloc_or_oom(shared, "a java/lang/String", |_| {
                    crate::vm::try_create_java_string_from_units(shared, &units)
                }) {
                    Some(obj) => new_local_handle(obj),
                    None => 0,
                }
            })
            .unwrap_or(0);
        }
    };
    // gc-common w7-c: a NEW, un-interned String, allocated fallibly. This used
    // to be `create_java_string`, which INTERNS: every string a native built
    // went into `string_pool`, which `roots.rs` section 5 roots strongly, so
    // it could never be collected (a native formatting a fresh message per
    // call leaked one String per call for the life of the VM), two
    // `NewStringUTF("x")` calls returned the SAME object where HotSpot returns
    // two, and a full heap aborted the process instead of answering NULL
    // with `OutOfMemoryError` pending (`jni_alloc_or_oom`, as every other
    // allocating JNI function does since w5-g).
    with_shared_vm(|shared| {
        match jni_alloc_or_oom(shared, "a java/lang/String", |_| {
            crate::vm::try_create_java_string_uninterned(shared, s)
        }) {
            Some(obj) => new_local_handle(obj),
            None => 0,
        }
    })
    .unwrap_or(0)
}

// ---- Index 168: GetStringUTFLength ----
pub(in crate::native::jni) extern "C" fn jni_get_string_utf_length(_env: JNIEnv, str_obj: JString) -> JSize {
    // gcd d5/f: a leaf (reads the String and its value array into Rust
    // memory); see `ForeignJniEntry::enter_leaf`.
    let _fx = ForeignJniEntry::enter_leaf(str_obj);
    if str_obj == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(str_obj)?;
        // gci i14/jh: measured over the exact units (`modified_utf8_len_units`),
        // as `GetStringUTFChars` now encodes them; no lossy `str`.
        let units = crate::vm::read_java_string_units(&shared.mem.heap, oref)?;
        // The JNI contract specifies the *modified* UTF-8 length, which must
        // agree with what GetStringUTFChars produces (a caller commonly does
        // `malloc(GetStringUTFLength()+1)` then copies the chars in). Standard
        // `s.len()` undercounts interior NULs (1→2 bytes) and supplementary
        // chars (4→6 bytes), so compute the modified-UTF-8 length explicitly.
        Some(modified_utf8_len_units(&units) as JSize)
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 169: GetStringUTFChars ----
pub(in crate::native::jni) extern "C" fn jni_get_string_utf_chars(
    _env: JNIEnv,
    str_obj: JString,
    is_copy: *mut JBoolean,
) -> *const c_char {
    // gcd d10/j: runs no Java (see `ForeignJniEntry::enter_vm_only`).
    let _fx = ForeignJniEntry::enter_vm_only();
    if str_obj == 0 {
        return std::ptr::null();
    }
    let result = with_shared_vm(|shared| match jobject_to_obj(str_obj) {
        Some(oref) => string_utf_chars_copy(&shared.mem.heap, oref),
        None => UtfCharsCopy::NotAString,
    });
    match result {
        Some(UtfCharsCopy::Copied(c_string)) => {
            if !is_copy.is_null() {
                unsafe {
                    *is_copy = JNI_TRUE;
                }
            }
            // Caller frees via ReleaseStringUTFChars (CString::from_raw).
            c_string.into_raw() as *const c_char
        }
        Some(UtfCharsCopy::OutOfCHeap) => {
            // HotSpot 25 `jni_GetStringUTFChars`: `AllocateHeap(...,
            // RETURN_NULL)`, and on NULL `THROW_OOP_(Universe::
            // out_of_memory_error_c_heap(), nullptr)` -- a plain throw of the
            // preallocated "C heap space" error, NOT
            // `report_java_out_of_memory` (no heap dump / OnOutOfMemoryError
            // hooks), so this is not the JNI heap-OOME raise.
            raise_jni_error("java/lang/OutOfMemoryError", JNI_C_HEAP_OOME_MESSAGE);
            std::ptr::null()
        }
        Some(UtfCharsCopy::NotAString) | None => std::ptr::null(),
    }
}

/// The detail message of HotSpot 25's `Universe::out_of_memory_error_c_heap()`
/// (set in `universe.cpp` next to "Java heap space"), which
/// `jni_GetStringUTFChars` throws when its copy cannot be allocated.
pub(in crate::native::jni) const JNI_C_HEAP_OOME_MESSAGE: &str = "C heap space";

/// What `GetStringUTFChars` copies out of a String (j27 wave 8, lane W8JNI).
pub(in crate::native::jni) enum UtfCharsCopy {
    /// Not a String this reader understands: NULL with nothing pending.
    NotAString,
    /// The C heap cannot hold the copy: NULL with HotSpot's C-heap OOME.
    OutOfCHeap,
    /// The NUL-terminated modified UTF-8 copy.
    Copied(std::ffi::CString),
}

/// `GetStringUTFChars`' copy of `oref`: its exact UTF-16 units (gci i14/jh: a
/// lone surrogate is its own 3-byte sequence, as on HotSpot), encoded as JNI
/// modified UTF-8 (`C0 80` for U+0000, so the only NUL is the terminator).
/// j27 wave 8 (lane W8JNI): both buffers are reserved FALLIBLY; the units
/// `Vec` and the encoded bytes used to be infallible allocations that aborted
/// the process on C-heap exhaustion, where HotSpot throws.
pub(in crate::native::jni) fn string_utf_chars_copy(
    heap: &crate::memory::vm_heap::VmHeap,
    oref: ObjectRef,
) -> UtfCharsCopy {
    let Some(count) = crate::vm::java_string_unit_count(heap, oref) else {
        return UtfCharsCopy::NotAString;
    };
    let Some(mut units) = reserve_elements_copy::<u16>(count) else {
        return UtfCharsCopy::OutOfCHeap;
    };
    if !crate::vm::append_java_string_units(heap, oref, &mut units) {
        return UtfCharsCopy::NotAString;
    }
    let Some(bytes) = modified_utf8_nul_reserved(&units) else {
        return UtfCharsCopy::OutOfCHeap;
    };
    // `bytes` holds no 0x00 (see `push_modified_utf8_units`), and its
    // reserved room for the terminator means `CString::new` does not grow it.
    std::ffi::CString::new(bytes).map_or(UtfCharsCopy::NotAString, UtfCharsCopy::Copied)
}

/// The modified UTF-8 of `units` in a buffer reserved fallibly with room for
/// the terminating NUL (not yet pushed), or `None` when the C heap cannot hold
/// it.
pub(in crate::native::jni) fn modified_utf8_nul_reserved(units: &[u16]) -> Option<Vec<u8>> {
    let len = modified_utf8_len_units(units);
    let mut bytes: Vec<u8> = reserve_elements_copy(len.checked_add(1)?)?;
    push_modified_utf8_units(units, &mut bytes);
    Some(bytes)
}

// ---- Index 170: ReleaseStringUTFChars ----
pub(in crate::native::jni) extern "C" fn jni_release_string_utf_chars(_env: JNIEnv, _str: JString, chars: *const c_char) {
    if !chars.is_null() {
        // Free the CString allocated by GetStringUTFChars
        unsafe {
            drop(std::ffi::CString::from_raw(chars as *mut c_char));
        }
    }
}

// ---- Index 171: GetArrayLength ----
pub(in crate::native::jni) extern "C" fn jni_get_array_length(_env: JNIEnv, array: JArray) -> JSize {
    // gcd d5/f: a leaf (a header read); see `ForeignJniEntry::enter_leaf`.
    let _fx = ForeignJniEntry::enter_leaf(array);
    if array == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(array)?;
        Some(shared.mem.heap.array_length(oref) as JSize)
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 172: NewObjectArray ----
pub(in crate::native::jni) extern "C" fn jni_new_object_array(
    _env: JNIEnv,
    length: JSize,
    clazz: JClass,
    init: JObject,
) -> JArray {
    let _fx = ForeignJniEntry::enter();
    if length < 0 {
        raise_jni_negative_array_size(length);
        return 0;
    }
    with_shared_vm(|shared| {
        // gcj j1/jn: HotSpot's length refusal comes first, and runs no
        // collection.
        if jni_array_refused_for_its_length(shared, length, "an object array") {
            return 0;
        }
        let component_id = jclass_class_id(clazz);
        // gci i13/oa item 4: sized, so a G1 humongous request collects under
        // `G1 Humongous Allocation`.
        let bytes = crate::runtime::interpreter::gc_and_alloc::native_array_footprint(
            ArrayElementType::Reference,
            length as usize,
        );
        let Some(arr) = jni_alloc_or_oom_sized(shared, "an object array", bytes, |heap| {
            heap.try_alloc_array_full(component_id, ArrayElementType::Reference, length as usize)
        }) else {
            return 0;
        };
        // Initialize elements if init is non-null
        if init != 0 {
            if let Some(init_ref) = jobject_to_obj(init) {
                for i in 0..length as usize {
                    let _ =
                        shared
                            .mem
                            .heap
                            .set_array_element(arr, i, Value::Object(Some(init_ref)));
                }
            }
        }
        new_local_handle(arr)
    })
    .unwrap_or(0)
}

// ---- Index 173: GetObjectArrayElement ----
pub(in crate::native::jni) extern "C" fn jni_get_object_array_element(_env: JNIEnv, array: JArray, index: JSize) -> JObject {
    // gcd d10/j: runs no Java on its ordinary path; the out-of-bounds raise
    // escalates through `with_jni_context` (see
    // `ForeignJniEntry::enter_vm_only`).
    let _fx = ForeignJniEntry::enter_vm_only();
    if array == 0 {
        return 0;
    }
    // `Err(length)`: `index` is outside the array. HotSpot throws
    // `ArrayIndexOutOfBoundsException` here; this returned NULL with nothing
    // pending, which a native reads as a stored null. Raised after the
    // context borrow is released (round 12 wave 1, lane rt).
    let got = with_shared_vm(|shared| {
        let oref = jobject_to_obj(array)?;
        if let Some(length) = jni_array_index_out_of_bounds(shared, oref, index) {
            return Some(Err(length));
        }
        Some(Ok(
            match shared.mem.heap.get_array_element(oref, index as usize) {
                Ok(Value::Object(Some(r))) => new_local_handle(r),
                _ => 0,
            },
        ))
    })
    .flatten();
    match got {
        Some(Ok(handle)) => handle,
        Some(Err(length)) => {
            raise_jni_index_out_of_bounds(index, length);
            0
        }
        None => 0,
    }
}

/// `Some(length)` when `oref` is an array and `index` is outside it; `None`
/// when the index is in bounds, and for a non-array (which the element
/// accessors below refuse on their own, as before).
pub(in crate::native::jni) fn jni_array_index_out_of_bounds(
    shared: &SharedVm,
    oref: ObjectRef,
    index: JSize,
) -> Option<usize> {
    if shared.mem.heap.kind_of(oref) != crate::memory::heap::ObjectKind::Array {
        return None;
    }
    let length = shared.mem.heap.array_length(oref);
    // Cast: a non-negative `JSize` fits `usize`.
    (index < 0 || index as usize >= length).then_some(length)
}

/// A pending `ArrayIndexOutOfBoundsException` with HotSpot's JNI message for
/// a signed element index (`Index -1 out of bounds for length 3`);
/// [`region_bounds_ok`] is the region form (HotSpot's `check_bounds` text).
pub(in crate::native::jni) fn raise_jni_index_out_of_bounds(index: JSize, length: usize) {
    raise_jni_throwable(
        "java/lang/ArrayIndexOutOfBoundsException",
        &format!("Index {index} out of bounds for length {length}"),
    );
}

/// `New<Type>Array` / `NewObjectArray` with a negative length: HotSpot leaves
/// `NegativeArraySizeException` pending (message: the length), as `newarray`
/// throws. This returned NULL with nothing pending, which a native reads as an
/// allocation failure (round 12 wave 1, lane rt).
pub(in crate::native::jni) fn raise_jni_negative_array_size(length: JSize) {
    raise_jni_throwable("java/lang/NegativeArraySizeException", &length.to_string());
}

/// Leave a `class_name` throwable with `msg` pending for the current JNI call,
/// or the `ThrowNew` sentinel when none can be built (as
/// [`raise_jni_region_error`] does).
pub(in crate::native::jni) fn raise_jni_throwable(class_name: &str, msg: &str) {
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
        pend_unbuilt_throwable(heap_exhausted);
    }
}

/// gcd d4/k: did `create_exception_object` fail for want of heap (as opposed
/// to a class that cannot be loaded, or no thread context)?
pub(in crate::native::jni) fn is_heap_exhaustion(e: &crate::error::MethodCallFailed) -> bool {
    matches!(
        e,
        crate::error::MethodCallFailed::InternalError(crate::error::VmError::Runtime(
            crate::error::RuntimeError::OutOfMemoryError { .. }
        ))
    )
}

/// gcd d4/k (`docs/feature-designs/real-jdk-mode.md`):
/// leave pending what a JNI function could not build. When the build failed
/// for want of heap, that is the VM's preallocated `OutOfMemoryError`, as
/// HotSpot throws when it cannot allocate the exception it meant to throw --
/// a real object, so `ExceptionOccurred` hands the native a real throwable.
/// Otherwise (no thread context, a class that will not load, no singleton
/// yet) the `u64::MAX` sentinel, as before: flagged, never a silent success,
/// and turned into a VM-internal failure at the native's return.
pub(in crate::native::jni) fn pend_unbuilt_throwable(heap_exhausted: bool) {
    if heap_exhausted {
        if let Some(oom) = with_shared_vm(|shared| *shared.mem.singleton_oom.read()).flatten() {
            set_jni_pending_exception_object(oom);
            return;
        }
    }
    JNI_PENDING_EXCEPTION.with(|cell| cell.set(u64::MAX));
}

// ---- Index 174: SetObjectArrayElement ----
pub(in crate::native::jni) extern "C" fn jni_set_object_array_element(
    _env: JNIEnv,
    array: JArray,
    index: JSize,
    val: JObject,
) {
    let _fx = ForeignJniEntry::enter();
    if array == 0 {
        return;
    }
    // HotSpot throws `ArrayIndexOutOfBoundsException` for an index outside
    // the array and `ArrayStoreException` for a value that is not an instance
    // of the component; both were silently dropped here (the store just did
    // not happen). Raised after the context borrow is released (round 12
    // wave 1, lane rt).
    enum Refused {
        Index(usize),
        Store(String),
    }
    let refused = with_shared_vm(|shared| {
        // The value first, as in `SetObjectField`: it may mint a mirror.
        let value = jobject_to_obj(val);
        let oref = jobject_to_obj(array)?;
        if let Some(length) = jni_array_index_out_of_bounds(shared, oref, index) {
            return Some(Refused::Index(length));
        }
        // The shared `aastore` rule (fails open on imprecise types, like the
        // opcode), asked only of a reference array.
        if let Some(v) = value {
            if shared.mem.heap.element_type_of(oref) == ArrayElementType::Reference
                && !crate::runtime::interpreter::aastore_element_assignable(shared, oref, v)
            {
                return Some(Refused::Store(jni_array_store_message(
                    shared, oref, v, index,
                )));
            }
        }
        let _ = shared
            .mem
            .heap
            .set_array_element(oref, index as usize, Value::Object(value));
        None
    })
    .flatten();
    match refused {
        Some(Refused::Index(length)) => raise_jni_index_out_of_bounds(index, length),
        Some(Refused::Store(msg)) => raise_jni_throwable("java/lang/ArrayStoreException", &msg),
        None => {}
    }
}

/// HotSpot's `SetObjectArrayElement` refusal text: `type mismatch: can not
/// store java.lang.String to java.lang.Integer[0]`, the array written as its
/// bottom component with one `[]` per further dimension (`int[0][]`).
pub(in crate::native::jni) fn jni_array_store_message(
    shared: &SharedVm,
    array: ObjectRef,
    value: ObjectRef,
    index: JSize,
) -> String {
    // JIT round 21 wave 24, lane names (item 5 of
    // `r21-review-w23-findings-FIXED-20261020.md`, `CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`):
    // an array value and the array's bottom component are named by their
    // BOTTOM class's hidden flag, as `Klass::external_name()` names them; the
    // shape rule stays the fallback and the `=0` text.
    let value_name = match crate::runtime::interpreter::array_descriptor_of(shared, value) {
        Some(desc) => {
            let bottom = crate::runtime::exceptions::bottom_class_hidden(
                shared,
                shared.mem.heap.class_id_of(value),
                &desc,
            );
            crate::runtime::exceptions::external_name_by_bottom_class_or(
                &desc,
                bottom,
                crate::runtime::exceptions::hotspot_external_name,
            )
        }
        None => shared
            .classes
            .class_manager
            .read()
            .get_class(shared.mem.heap.class_id_of(value))
            .map(|c| crate::runtime::exceptions::hotspot_external_name_of_class(&c.name, c.hidden))
            .unwrap_or_default(),
    };
    let desc = crate::runtime::interpreter::array_descriptor_of(shared, array).unwrap_or_default();
    let dims = desc.bytes().take_while(|&b| b == b'[').count();
    let bottom = desc.get(dims..).unwrap_or("");
    let bottom_hidden = crate::runtime::exceptions::bottom_class_hidden(
        shared,
        shared.mem.heap.class_id_of(array),
        &desc,
    );
    let bottom_name = match bottom {
        "Z" => "boolean".to_string(),
        "B" => "byte".to_string(),
        "C" => "char".to_string(),
        "S" => "short".to_string(),
        "I" => "int".to_string(),
        "J" => "long".to_string(),
        "F" => "float".to_string(),
        "D" => "double".to_string(),
        other => {
            let body = other
                .strip_prefix('L')
                .and_then(|s| s.strip_suffix(';'))
                .unwrap_or(other);
            crate::runtime::exceptions::external_name_by_bottom_class_or(body, bottom_hidden, |b| {
                b.replace('/', ".")
            })
        }
    };
    let mut msg = format!("type mismatch: can not store {value_name} to {bottom_name}[{index}]");
    for _ in 1..dims {
        msg.push_str("[]");
    }
    msg
}

new_prim_array!(jni_new_boolean_array, ArrayElementType::Boolean); // 175
new_prim_array!(jni_new_byte_array, ArrayElementType::Byte); // 176
new_prim_array!(jni_new_char_array, ArrayElementType::Char); // 177
new_prim_array!(jni_new_short_array, ArrayElementType::Short); // 178
new_prim_array!(jni_new_int_array, ArrayElementType::Int); // 179
new_prim_array!(jni_new_long_array, ArrayElementType::Long); // 180
new_prim_array!(jni_new_float_array, ArrayElementType::Float); // 181
new_prim_array!(jni_new_double_array, ArrayElementType::Double); // 182
