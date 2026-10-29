// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//! Shared machinery for the interpreter's monomorphic invoke fast doors, and
//! the two non-virtual doors themselves (`invokestatic`, `invokespecial`).
//!
//! # Why
//!
//! The 2026-09-02 pass gave `invokevirtual` / `invokeinterface` a fast door
//! and measured the two call shapes it did **not** cover as the worst
//! remaining ones: `invokestatic` at ~250 ns and `invokespecial` at ~430 ns
//! against HotSpot's ~4 ns, and unchanged across that whole pass because they
//! were its controls. They reach different dispatchers —
//! `execute_invokestatic_cached` for `0xb8`, the `CachedInvokeTarget::Bytecode`
//! arm of `execute_invokevirtual_cached` for `0xb7` — but they pay the same
//! three per-call costs the virtual door removed:
//!
//! * the cache entry is **cloned** out of the inline cache (two `Arc`
//!   increments and two decrements, because `RedefineGate` carries its own
//!   `Arc<AtomicU32>`),
//! * every argument is decoded `CompactValue -> Value -> CompactValue` through
//!   a `[Value; 8]` / `[Value; 16]` stack array, only to be handed to the
//!   interception chain and then re-encoded into the callee's locals,
//! * and `invokestatic` takes a sharded `RwLock` read plus a hash lookup for
//!   the invocation counter **on every call**.
//!
//! Everything else those dispatchers ask is a constant of the callee or of the
//! call site, memoized on `CachedBytecodeMethod` since the first pass.
//!
//! # What a door promises
//!
//! A door either performs the whole call and returns `Some`, or returns `None`
//! **with the operand stack untouched**, and the general dispatcher then runs
//! exactly as before. Nothing here is a second implementation of a semantic:
//! every check a door makes is one the general path makes, in the same order,
//! and every shape it cannot prove is declined rather than approximated.
//!
//! What the non-virtual doors decline, so the general path keeps owning it:
//! a target that is not a plain bytecode method, a `synchronized` callee whose
//! monitor is contended (or, for a static one, whose class mirror does not
//! exist yet; see `door_monitor_acquire`), a callee with a registered native
//! or a non-zero intercept shape, a null
//! receiver (`invokespecial`), a loader-split owner, more than eight
//! parameters, any argument whose operand-stack slot is not already in the
//! representation the callee's locals want, a full frame stack, a virtual
//! thread, PGO profiling, and every invoke diagnostic. A class redefinition
//! is no longer among them: it retires the entries it can change and nothing
//! else (see `DOORS_SURVIVE_REDEFINITION`).
//!
//! ## Why the verbatim transfer needs no `refresh_stale_object_args`
//!
//! The general path materialises arguments into a Rust `[Value; N]`, which the
//! collector does not scan, and repairs them afterwards with
//! `refresh_stale_object_args` (a `load_and_forward` per reference). A door
//! copies operand-stack slots into the callee's locals with **no safepoint in
//! between** — no allocation, no poll, no Java call — so the references it
//! moves are exactly as current as the operand stack the collector just
//! scanned. There is nothing to repair, and nothing that could go stale.
//!
//! Kill switch: `CRATONVM_JIT_NO_NONVIRTUAL_FAST_DOOR=1`
//! (`CRATONVM_JIT=-nonvirtual-fast-door`) turns off both doors in this file.
//! The virtual door keeps its own `CRATONVM_JIT_NO_INVOKE_FAST_DOOR`.

use super::site_cache::site_stats;
use super::*;

/// Capacity of [`ArgSlots`]: the receiver plus the eight parameters
/// `DescriptorFacts` keeps inline; a longer descriptor is declined.
const ARG_SLOTS_LEN: usize = cratonvm_jit_api::DescriptorFacts::INLINE_PARAMS + 1;

/// Arguments read off the operand stack, paired with the descriptor tag that
/// decides how many local slots each occupies.
///
/// # Why the entries start uninitialized (interpreter round i1 wave 27, L4)
///
/// Every door call builds one of these in its own Rust frame. Until wave 27
/// it was a plain `[(CompactValue, u8); 9]` that `empty_arg_slots` filled
/// with nine `(null, 'L')` entries: 144 bytes stored on every call to use
/// `total_args` of them (16 bytes for `InvokeDoorCostBench`'s one-`int`
/// `static-call`), although nothing ever read an entry the call had not
/// written
/// (`docs/feature-designs/jdk-only-mode.md`).
/// The entries are now `MaybeUninit`, so creating the array stores nothing
/// but the count, and the only way to read them is [`ArgSlots::filled`]: the
/// prefix the last SUCCESSFUL [`read_args_verbatim`] wrote, whose length that
/// call recorded. A failed read records `0`, so a caller that ignored the
/// failure would see no arguments rather than stale or uninitialized ones.
///
/// Never move one by value: it lives in the door's frame and is passed as
/// `&mut` / `&` (wave 24's `memmove` regression was this array returned by
/// value twice per call).
pub(super) struct ArgSlots {
    slots: [std::mem::MaybeUninit<(CompactValue, u8)>; ARG_SLOTS_LEN],
    /// Entries `0..filled` are initialized; always `<= ARG_SLOTS_LEN`.
    filled: usize,
}

impl ArgSlots {
    /// The arguments the last successful [`read_args_verbatim`] read, in
    /// operand-stack order (the receiver first when it read one).
    #[inline(always)]
    pub(super) fn filled(&self) -> &[(CompactValue, u8)] {
        debug_assert!(self.filled <= ARG_SLOTS_LEN);
        // SAFETY: `filled` is written only by `read_args_verbatim`, which sets
        // it to `total_args` after writing entries `0..total_args` (having
        // refused `total_args > ARG_SLOTS_LEN` before writing any), or to `0`
        // after a failed read; `empty_arg_slots` starts it at `0`. So
        // `0..filled` is in bounds and initialized. `MaybeUninit<T>` has
        // `T`'s layout, so the cast pointer addresses `filled` consecutive
        // valid `T`s, borrowed for `&self`'s lifetime.
        unsafe {
            std::slice::from_raw_parts(
                self.slots.as_ptr().cast::<(CompactValue, u8)>(),
                self.filled,
            )
        }
    }

    /// The object address in entry 0 -- the receiver, for a read made with
    /// `has_receiver` -- or `None` when that entry is null, not an object,
    /// or was not read.
    #[inline(always)]
    pub(super) fn receiver_ptr(&self) -> Option<u64> {
        self.filled().first().and_then(|(cv, _)| cv.as_object_ptr())
    }
}

/// An empty [`ArgSlots`] to fill: nothing is stored but the count.
#[inline(always)]
pub(super) fn empty_arg_slots() -> ArgSlots {
    ArgSlots {
        slots: [std::mem::MaybeUninit::uninit(); ARG_SLOTS_LEN],
        filled: 0,
    }
}

/// Read `total_args` operand-stack slots without popping, checking each
/// against the descriptor tag the callee's locals expect.
///
/// `has_receiver` makes slot 0 the receiver (tag `L`) and shifts the
/// descriptor's parameter tags by one, exactly as
/// `ParamTags::get_with_receiver` does.
///
/// Returns `None` — leaving the stack untouched — if any slot is not already
/// in the representation `Frame::new_pooled_cached_compact` would store: an
/// unmarked category-2 slot, an `Int(0)`-as-null, a smuggled long. Those are
/// the shapes `pop_arg_for_descriptor_checked` exists to coerce, and coercion
/// belongs to the general path that owns its three recorded corruption bugs.
#[inline]
pub(super) fn read_args_verbatim(
    stack: &crate::runtime::ValueStack,
    facts: &cratonvm_jit_api::DescriptorFacts,
    total_args: usize,
    has_receiver: bool,
    slots: &mut ArgSlots,
) -> bool {
    let ok = read_args_verbatim_into(stack, facts, total_args, has_receiver, &mut slots.slots);
    // Recorded after the writes, and only for a read that validated every
    // slot: the invariant `ArgSlots::filled` relies on.
    slots.filled = if ok { total_args } else { 0 };
    ok
}

/// The body of [`read_args_verbatim`]: validates and writes entries
/// `0..total_args` of `slots`, and returns `true` only if it wrote every one.
/// Refuses a read longer than `ARG_SLOTS_LEN` before writing anything.
#[inline(always)]
fn read_args_verbatim_into(
    stack: &crate::runtime::ValueStack,
    facts: &cratonvm_jit_api::DescriptorFacts,
    total_args: usize,
    has_receiver: bool,
    slots: &mut [std::mem::MaybeUninit<(CompactValue, u8)>; ARG_SLOTS_LEN],
) -> bool {
    if !arg_read_in_bounds(stack, facts, total_args, has_receiver) {
        return false;
    }
    for i in 0..total_args {
        let depth = total_args - 1 - i;
        let (cv, kind) = stack.peek_with_kind_at(depth);
        let tag = arg_tag(facts, has_receiver, i);
        if !arg_slot_is_verbatim(cv, kind, tag) {
            return false;
        }
        slots[i] = std::mem::MaybeUninit::new((cv, tag));
    }
    true
}

/// The bounds [`read_args_verbatim`] checks before it reads any slot.
#[inline(always)]
fn arg_read_in_bounds(
    stack: &crate::runtime::ValueStack,
    facts: &cratonvm_jit_api::DescriptorFacts,
    total_args: usize,
    has_receiver: bool,
) -> bool {
    if stack.len() < total_args || total_args > ARG_SLOTS_LEN {
        return false;
    }
    // The tag walk indexes `facts.param_tags`, which is a fixed
    // `[u8; INLINE_PARAMS]`. A descriptor with more parameters than that has
    // no inline tags to read, and one shorter than `total_args` claims would
    // read past its own length. Both doors already refuse an overflowing
    // descriptor before calling here; this is the helper standing on its own.
    let num_params = total_args - usize::from(has_receiver);
    !(facts.param_tags_overflow || num_params > facts.param_tag_len as usize)
}

/// The descriptor tag of argument `i` in operand-stack order: `L` for the
/// receiver when there is one, then the descriptor's parameter tags.
#[inline(always)]
fn arg_tag(facts: &cratonvm_jit_api::DescriptorFacts, has_receiver: bool, i: usize) -> u8 {
    if has_receiver {
        if i == 0 {
            b'L'
        } else {
            facts.param_tags[i - 1]
        }
    } else {
        facts.param_tags[i]
    }
}

/// Is an operand-stack slot already in the representation a callee local of
/// descriptor tag `tag` takes (see [`read_args_verbatim`])?
#[inline(always)]
fn arg_slot_is_verbatim(cv: CompactValue, kind: u8, tag: u8) -> bool {
    match tag {
        b'L' | b'[' => cv.is_object() || cv.is_null(),
        b'J' => kind == crate::runtime::ValueStack::KIND_MARK_LONG,
        b'D' => kind == crate::runtime::ValueStack::KIND_MARK_DOUBLE,
        b'F' => cv.as_float().is_some(),
        b'I' | b'Z' | b'B' | b'C' | b'S' => cv.as_int().is_some(),
        _ => false,
    }
}

/// Stage 2b of the contiguous interpreter stack (interpreter round i1 wave 38,
/// lane L7; `CRATONVM_JIT_OVERLAP_ARGS=1`): [`read_args_verbatim`]'s checks,
/// slot for slot, without copying the slots anywhere. The door then lays the
/// callee's locals over them where they are ([`push_frame_in_place`]).
#[inline]
pub(super) fn validate_args_verbatim(
    stack: &crate::runtime::ValueStack,
    facts: &cratonvm_jit_api::DescriptorFacts,
    total_args: usize,
    has_receiver: bool,
) -> bool {
    if !arg_read_in_bounds(stack, facts, total_args, has_receiver) {
        return false;
    }
    (0..total_args).all(|i| {
        let (cv, kind) = stack.peek_with_kind_at(total_args - 1 - i);
        arg_slot_is_verbatim(cv, kind, arg_tag(facts, has_receiver, i))
    })
}

/// The receiver of a call whose `total_args` validated arguments (the
/// receiver first) are still on `stack`: [`ArgSlots::receiver_ptr`] for a
/// door that did not copy them (stage 2b).
#[inline(always)]
pub(super) fn receiver_ptr_on_stack(
    stack: &crate::runtime::ValueStack,
    total_args: usize,
) -> Option<u64> {
    stack.peek_with_kind_at(total_args - 1).0.as_object_ptr()
}

/// The callee entry a door commits to (proposal
/// `interpreter-L7-proposal-a-cached-frame-install-without-refcount-or-flag-traffic-RETIRED-20261015`,
/// item 1; interpreter round i1 wave 47, lane L7).
///
/// A door found the entry BORROWED from `thread.invoke_cache` and used to
/// clone it (one locked increment) to end that borrow before it passed
/// `thread` whole; the install then dropped the retired slot's old handle
/// (one locked decrement) -- two full barriers per warm call, although in
/// the common case of slot reuse (recursion, a loop calling one callee) the
/// slot's old handle IS a handle to the same entry.
///
/// `SlotAlias` is that case: a second `Arc` value for the entry that owns no
/// count of its own (`ManuallyDrop`, never dropped), made only when the
/// retired slot at this depth holds the entry itself
/// ([`FrameStack::retired_slot_runs`](crate::runtime::frame::FrameStack::retired_slot_runs)).
/// The allocation stays alive for as long as the alias is used because the
/// slot keeps its handle: nothing between the door's check and its push runs
/// Java or reaches a safepoint (the doors' own invariant, which the verbatim
/// argument transfer already rests on), so the retired slot is neither
/// rebuilt nor trimmed in between; the invoke cache holds a handle too. A
/// door path that can reach a safepoint (the virtual door's inline compile)
/// takes an owned handle first ([`Self::into_owned`]). The push consumes the
/// alias by rebuilding the slot around the handle it already holds
/// (`FrameStack::push_cached_compact_reusing_same`); every other consumer
/// takes [`Self::into_owned`], a clone, exactly what the door took before.
///
/// Built only under `VmConfig::frame_install_borrowed`
/// (`CRATONVM_FRAME_INSTALL_BORROWED`).
pub(super) enum DoorCallee {
    Owned(Arc<CachedBytecodeMethod>),
    SlotAlias(std::mem::ManuallyDrop<Arc<CachedBytecodeMethod>>),
}

impl DoorCallee {
    /// The door's handle to `cached`: an alias when the retired slot at this
    /// depth already runs it and the switch is on, else a clone (the
    /// pre-wave-47 door). `frames` is the thread's frame stack, a field
    /// disjoint from the invoke cache `cached` is borrowed from.
    #[inline(always)]
    pub(super) fn of(
        shared: &SharedVm,
        frames: &crate::runtime::frame::FrameStack,
        cached: &Arc<CachedBytecodeMethod>,
    ) -> Self {
        if shared.config.frame_install_borrowed
            && shared.config.frame_slot_reuse
            && frames.retired_slot_runs(cached)
        {
            // SAFETY: an `Arc` built from the entry's own pointer without an
            // increment, and never dropped (`ManuallyDrop`): it owns no count.
            // The allocation outlives it (see the type's note: the retired
            // slot's handle, and the invoke cache's).
            let alias = unsafe { Arc::from_raw(Arc::as_ptr(cached)) };
            return DoorCallee::SlotAlias(std::mem::ManuallyDrop::new(alias));
        }
        DoorCallee::Owned(Arc::clone(cached))
    }

    /// An owned handle: the owned one, or a clone of the alias's entry.
    #[inline]
    pub(super) fn into_owned(self) -> Arc<CachedBytecodeMethod> {
        match self {
            DoorCallee::Owned(cached) => cached,
            DoorCallee::SlotAlias(alias) => Arc::clone(&alias),
        }
    }
}

impl std::ops::Deref for DoorCallee {
    type Target = Arc<CachedBytecodeMethod>;

    #[inline(always)]
    fn deref(&self) -> &Arc<CachedBytecodeMethod> {
        match self {
            DoorCallee::Owned(cached) => cached,
            DoorCallee::SlotAlias(alias) => alias,
        }
    }
}

impl From<Arc<CachedBytecodeMethod>> for DoorCallee {
    #[inline(always)]
    fn from(cached: Arc<CachedBytecodeMethod>) -> Self {
        DoorCallee::Owned(cached)
    }
}

/// Commit a door: drop the arguments from the caller's stack, build the
/// callee's frame from the slots verbatim, and push it.
///
/// There is no safepoint between the `read_args_verbatim` that produced
/// `slots` and this call, which is what makes the references in them current
/// (see the module note).
#[inline]
pub(super) fn push_frame_verbatim(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cached: DoorCallee,
    slots: &ArgSlots,
    total_args: usize,
    monitor: Option<ObjectRef>,
) -> CachedCallResult {
    // `slots.filled()` is exactly the `total_args` entries the successful
    // `read_args_verbatim` wrote; the count is passed as well because the
    // operand stack is trimmed by it.
    debug_assert_eq!(slots.filled().len(), total_args);
    thread.frames[frame_idx].stack.discard_top(total_args);
    // Stage 2 of the contiguous interpreter stack (argument overlap,
    // interpreter round i1 wave 37, lane L7): off by default, one load and
    // a predicted branch on a per-VM config field when off.
    let cached = if shared.config.overlap_interpreter_args {
        match push_frame_overlapping(
            shared,
            thread,
            frame_idx,
            cached.into_owned(),
            slots,
            monitor,
        ) {
            Ok(done) => return done,
            Err(cached) => DoorCallee::Owned(cached),
        }
    } else {
        cached
    };
    push_frame_stage1(shared, thread, cached, slots, monitor)
}

/// The stage-1 install of [`push_frame_verbatim`], after the caller's stack
/// has given the arguments up: rebuild the retired slot at this depth, or
/// take the first-call path. Always inlined, so `push_frame_verbatim` keeps
/// the code it had; [`push_frame_in_place`] takes it when the slab refuses
/// an in-place overlap.
#[inline(always)]
fn push_frame_stage1(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: DoorCallee,
    slots: &ArgSlots,
    monitor: Option<ObjectRef>,
) -> CachedCallResult {
    // The frame this call returns into was retired in place, not destroyed, so
    // its four buffers are still in the slot at this depth. Rebuilding in them
    // skips the whole pool round trip: no `(Vec, Vec)` tuple popped and pushed
    // back, no four `Vec` headers taken and reinstalled, and no ~220-byte
    // `Frame` constructed and moved into the stack. Measured 2026-09-02, that
    // churn is what `frame_build` and `ret_recycle` are mostly made of --
    // together 40% of an interpreted `invokestatic` -- and unlike the buffer
    // FILL it needs nothing from precise oop maps.
    //
    // The first call at any depth finds no retired slot and takes the ordinary
    // path below, which is also what `CRATONVM_JIT_NO_FRAME_SLOT_REUSE`
    // restores for every call.
    // `has_retired_slot` is exactly the condition under which
    // `push_cached_compact_reusing` succeeds (`depth < buf.len()`, with the
    // frame stack borrowed mutably throughout), so `cached` is MOVED into the
    // slot. Until wave 22 (lane L4) this cloned it and let the original drop
    // at the return: two atomic read-modify-writes on the callee's shared
    // refcount on every door call that reuses a slot, which is every warm
    // one.
    if shared.config.frame_slot_reuse && thread.frames.has_retired_slot() {
        // A slot still holding pooled buffers is converted to a slab window
        // once (wave 32).
        if thread.frames.retired_slot_holds_pooled_buffers() {
            thread.convert_retired_slot_to_window();
        }
        // Wave 47 (lane L7): a slot that already runs the entry keeps its own
        // handle to it (`DoorCallee::SlotAlias`), and the install makes no
        // reference-count update.
        let pushed = match cached {
            DoorCallee::Owned(cached) => thread
                .frames
                .push_cached_compact_reusing(cached, slots.filled()),
            DoorCallee::SlotAlias(alias) => {
                site_stats::bump_if(shared.config.site_census, site_stats::INSTALL_SAME);
                thread
                    .frames
                    .push_cached_compact_reusing_same(&alias, slots.filled())
            }
        };
        debug_assert!(pushed, "a retired slot is always reusable");
        install_door_monitor(thread, monitor);
        fire_method_entry_after_push_in_loop(shared, thread);
        super::gci_i11jd_witness::note_entry(shared, thread);
        return CachedCallResult::FramePushed;
    }
    push_frame_verbatim_fresh(shared, thread, cached.into_owned(), slots, monitor)
}

/// [`push_frame_verbatim`] with `VmConfig::overlap_interpreter_args`
/// (`CRATONVM_JIT_OVERLAP_ARGS=1`): the callee's locals are laid over the
/// arguments the caller pushed, which `push_frame_verbatim` has just
/// discarded, so nothing is copied for them
/// (`FrameStack::push_cached_compact_overlapping`, stage 2 of the contiguous
/// interpreter stack; interpreter round i1 wave 37, lane L7).
///
/// `Err(cached)`, with the frame stack unchanged, sends the caller down the
/// stage-1 paths: under a JDWP debugger (it reads a returning frame's locals,
/// which an overlapped frame gives up to its caller's push), under the two
/// diagnostic switches that name another build (`CRATONVM_JIT_NO_FRAME_SLOT_REUSE`,
/// `CRATONVM_JIT_NO_FRAME_EMPLACE`), when the caller is not the top frame,
/// and whenever the frame stack refuses the overlap. A slot still holding
/// pooled buffers is converted first, exactly as the reuse path does, so the
/// window the slot is rebuilt in never frees a pooled buffer.
///
/// Out of line so the switch-off door keeps the code it had.
#[inline(never)]
fn push_frame_overlapping(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cached: Arc<CachedBytecodeMethod>,
    slots: &ArgSlots,
    monitor: Option<ObjectRef>,
) -> Result<CachedCallResult, Arc<CachedBytecodeMethod>> {
    if !shared.config.frame_slot_reuse
        || !shared.config.frame_emplace
        || frame_idx + 1 != thread.frames.len()
        || crate::runtime::jvmti::debugger_observes_locals(shared)
    {
        return Err(cached);
    }
    if thread.frames.retired_slot_holds_pooled_buffers() {
        thread.convert_retired_slot_to_window();
    }
    thread
        .frames
        .push_cached_compact_overlapping(cached, slots.filled())?;
    install_door_monitor(thread, monitor);
    fire_method_entry_after_push_in_loop(shared, thread);
    super::gci_i11jd_witness::note_entry(shared, thread);
    Ok(CachedCallResult::FramePushed)
}

/// Is stage 2b on for this VM? The same per-VM switch as stage 2
/// (`VmConfig::overlap_interpreter_args`, `CRATONVM_JIT_OVERLAP_ARGS=1`): with
/// it on, the static and virtual doors validate their arguments in place
/// ([`validate_args_verbatim`]) and push through [`push_frame_in_place`]
/// instead of copying them into an [`ArgSlots`]. One load of a config field;
/// off, the doors run exactly the stage-1 code they had.
#[inline(always)]
pub(super) fn in_place_args(shared: &SharedVm) -> bool {
    shared.config.overlap_interpreter_args
}

/// Stage 2b of the contiguous interpreter stack (interpreter round i1 wave 38,
/// lane L7): commit a door whose `total_args` arguments are still on the
/// caller's operand stack, validated by [`validate_args_verbatim`] with no
/// safepoint since, and never copied. The callee's window starts at the
/// caller's first argument slot and its locals are laid where the arguments
/// are (`FrameStack::push_cached_compact_in_place`), a category-2 argument
/// moving them up within the window.
///
/// Refused on exactly the stage-2 refusals ([`push_frame_overlapping`]): then,
/// and only then, the arguments are copied into an [`ArgSlots`] after all
/// (the validation stands: nothing between it and here can safepoint) and the
/// stage-1 install runs, as `push_frame_verbatim` would have run it.
///
/// Out of line, like [`push_frame_overlapping`], so the switch-off door keeps
/// its code.
#[inline(never)]
pub(super) fn push_frame_in_place(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cached: DoorCallee,
    total_args: usize,
    has_receiver: bool,
    monitor: Option<ObjectRef>,
) -> CachedCallResult {
    let cached = cached.into_owned();
    let facts = *cached.descriptor_facts();
    let refused = !shared.config.frame_slot_reuse
        || !shared.config.frame_emplace
        || frame_idx + 1 != thread.frames.len()
        || crate::runtime::jvmti::debugger_observes_locals(shared);
    let cached = if refused {
        cached
    } else {
        let mut tags = [0u8; ARG_SLOTS_LEN];
        for (i, tag) in tags.iter_mut().enumerate().take(total_args) {
            *tag = arg_tag(&facts, has_receiver, i);
        }
        if thread.frames.retired_slot_holds_pooled_buffers() {
            thread.convert_retired_slot_to_window();
        }
        match thread
            .frames
            .push_cached_compact_in_place(cached, &tags[..total_args])
        {
            Ok(()) => {
                install_door_monitor(thread, monitor);
                fire_method_entry_after_push_in_loop(shared, thread);
                super::gci_i11jd_witness::note_entry(shared, thread);
                return CachedCallResult::FramePushed;
            }
            Err(cached) => cached,
        }
    };
    let mut slots = empty_arg_slots();
    let read = read_args_verbatim(
        &thread.frames[frame_idx].stack,
        &facts,
        total_args,
        has_receiver,
        &mut slots,
    );
    debug_assert!(read, "a validated argument read cannot fail");
    thread.frames[frame_idx].stack.discard_top(total_args);
    push_frame_stage1(shared, thread, DoorCallee::Owned(cached), &slots, monitor)
}

/// [`push_frame_verbatim`] when no slot is retired at this depth: the first
/// call at a depth, and every call under `CRATONVM_JIT_NO_FRAME_SLOT_REUSE`.
///
/// Out of line so the warm door keeps only the slot-reuse path (interpreter
/// round i1 wave 32). Inlined, this arm's pool refill, its two constructors
/// and the slab's window-or-pooled emplace doubled `push_frame_verbatim` to
/// ~1400 instructions and spilled the reuse path's live values to the stack,
/// which measured +5-15% on `--nojit` call rows against wave 28.
#[inline(never)]
fn push_frame_verbatim_fresh(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: Arc<CachedBytecodeMethod>,
    slots: &ArgSlots,
    monitor: Option<ObjectRef>,
) -> CachedCallResult {
    thread.refill_pools_from_shared(
        &shared.mem.operand_stack_pool,
        &shared.mem.tag_pool,
        cached.max_locals as usize,
        (cached.max_stack as usize).max(16) + 8,
    );
    // No retired slot: the first call at this depth. Take the buffers from the
    // pools and build the frame IN the slot rather than on the Rust stack --
    // `push` would move ~220 bytes into the same place. The buffers are taken
    // before the frame stack is borrowed, which is also what keeps the pools
    // and `FrameStack` from wanting `&mut thread` at once.
    if !shared.config.frame_emplace {
        let frame = Frame::new_pooled_cached_compact(
            cached,
            slots.filled(),
            &mut thread.locals_pool,
            &mut thread.stacks_pool,
        );
        push_frame_and_fire_entry_in_loop(shared, thread, frame);
        install_door_monitor(thread, monitor);
        super::gci_i11jd_witness::note_entry(shared, thread);
        return CachedCallResult::FramePushed;
    }
    // In one window of the frame stack's slot slab, or (under
    // `CRATONVM_JIT_NO_LOCALS_SLAB`) the pooled parts emplaced as before.
    thread.frames.emplace_cached_compact_args(
        cached,
        slots.filled(),
        &mut thread.locals_pool,
        &mut thread.stacks_pool,
    );
    install_door_monitor(thread, monitor);
    fire_method_entry_after_push_in_loop(shared, thread);
    super::gci_i11jd_witness::note_entry(shared, thread);
    CachedCallResult::FramePushed
}

/// Record on the just-pushed frame the monitor its door acquired, so
/// `pop_and_recycle_frame_with_reason` releases it however the frame leaves.
///
/// Written by index from the frame stack's top rather than folded into the
/// push, because the three push shapes install the frame differently and one of
/// them fires `MethodEntry` on the way; doing it after the push is the one
/// ordering that is identical on all three.
#[inline]
fn install_door_monitor(thread: &mut JvmThread, monitor: Option<ObjectRef>) {
    if let Some(obj) = monitor {
        if let Some(top) = thread.frames.last_mut() {
            debug_assert!(
                top.monitor_on_exit.is_none(),
                "a freshly pushed frame already owns a monitor"
            );
            top.monitor_on_exit = Some(obj);
        }
    }
}

/// `CRATONVM_JIT_NO_DOOR_SYNC=1` — restore the pre-2026-09-08 refusal, where
/// every fast door declined a `synchronized` callee outright.
///
/// # Why the doors take them now
///
/// The refusal was the single largest reason any door declined anything.
/// `CRATONVM_DBG_FIELD_SITE=1` on `probes/CollatorSplit.java` (H2's BNF
/// autocompletion reaches `RuleBasedCollator.compare` through
/// `StringUtils.startsWithIgnoringCase`, and ICU's normaliser drives
/// `StringBuffer` — a final class whose accessors are all `synchronized` — one
/// character at a time):
///
/// ```text
/// [invoke-door] declines by reason (total 3092393):
/// [invoke-door]  888279  28.7%  special: cached target is VirtualBytecode on a non-special call
/// [invoke-door]  888105  28.7%  virtual: callee is SYNCHRONIZED
/// ```
///
/// The two rows are one population: the virtual door is tried first, and the
/// non-virtual door that runs after it declines the same call again. 888 105
/// of the run's 2.47 M interpreted calls — **36%** — were pushed off the
/// ~150 ns door onto the ~430 ns general path for that one reason.
///
/// # What it costs to take them
///
/// Only an UNCONTENDED acquire. A contended one declines to the general path,
/// which owns the GC-blocked wait protocol (`monitor_enter_synchronized_method`
/// pins and remaps the arguments across a moving collection); none of that
/// belongs in a door. The release rides on `Frame::monitor_on_exit`, a field
/// that has existed — with the GC's remap and root-scan support already wired —
/// since the stackless-dispatch design that never shipped it, and every frame
/// removal in the interpreter funnels through
/// `pop_and_recycle_frame_with_reason`: normal return, exception unwind, and
/// the orphan sweeps `execute`/`resume_continuation` run after an early
/// `return Err`.
///
/// Read once per VM into `VmConfig::door_sync` (interpreter round i2 wave 58,
/// lane L7; a process `OnceLock` before).
#[inline(always)]
pub(crate) fn door_sync_enabled(shared: &SharedVm) -> bool {
    shared.config.door_sync
}

/// Acquire an `ACC_SYNCHRONIZED` callee's monitor for a door, or refuse.
///
/// `Some(Some(obj))` — acquired uncontended; the caller MUST store `obj` in the
/// pushed frame's `monitor_on_exit`. `Some(None)` — nothing to acquire.
/// `None` — the door must decline, and the general path re-does the call.
///
/// Declining on contention is not a fallback, it is the design: the blocking
/// acquire has to pin and remap the arguments across a moving GC, and a door
/// that tried would be a second, unaudited copy of
/// `monitor_enter_synchronized_method`.
#[inline]
pub(crate) fn door_monitor_acquire(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &CachedBytecodeMethod,
    receiver: Option<ObjectRef>,
) -> Option<Option<ObjectRef>> {
    if !cached.is_synchronized {
        return Some(None);
    }
    if !door_sync_enabled(shared) {
        return None;
    }
    // A static synchronized method locks the class mirror. CREATING one can
    // allocate, and a door has no safepoint between `read_args_verbatim` and
    // the push, so only an EXISTING mirror is taken: a map read, the
    // non-allocating fast path of `get_or_create_class_mirror` (the same
    // object every other door locks). While none exists the door declines, and
    // the general path's first call creates it.
    let obj = match receiver {
        Some(obj) => obj,
        None if cached.is_static => shared
            .classes
            .class_mirrors
            .read()
            .get(&cached.declaring_class_id)
            .copied()?,
        None => return None,
    };
    if shared
        .threads
        .monitors
        .enter_or_contend(obj, thread.thread_id)
        .is_some()
    {
        // Contended: `enter_or_contend` did NOT acquire, so there is nothing to
        // release and nothing to retract. The general path blocks properly.
        return None;
    }
    // Same ownership publish the uncontended arm of `monitor_enter_blocking`
    // makes. Skipping it would leave `getLockedMonitors()` blind to exactly the
    // monitors the doors now serve.
    shared
        .threads
        .thread_registry
        .complete_jmx_monitor_enter(thread.thread_id, obj);
    Some(Some(obj))
}

/// The questions that are constants of the callee, asked once here for every
/// door: nothing to intercept, no registered native shadowing the bytecode,
/// not `synchronized`, and a descriptor whose parameter tags are all inline.
///
/// `force_native_cache` is filled by the general path; until it has answered
/// `false` once — or if it answered `true` — this is not a call a door may
/// take, because only the general path knows how to route it to the native.
#[inline]
pub(super) fn callee_is_plain_bytecode<'c>(
    shared: &SharedVm,
    cached: &'c CachedBytecodeMethod,
    num_params: usize,
) -> Option<&'c cratonvm_jit_api::DescriptorFacts> {
    // `is_synchronized` is NOT refused here any more; `door_monitor_acquire`
    // owns that decision, because it is the only place that can also take the
    // monitor. See `door_sync_enabled`.
    if cached.is_synchronized && !door_sync_enabled(shared) {
        return None;
    }
    if cached.force_native_cache.get() != Some(&false) {
        return None;
    }
    let shape = *cached.intercept_shape_cache.get_or_init(|| {
        intercept_shape_of(
            cached.class_name.as_ref(),
            cached.method_name.as_ref(),
            cached.method_descriptor.as_ref(),
        )
    });
    if shape != 0 {
        return None;
    }
    if !shared.config.descriptor_facts {
        return None;
    }
    let facts = cached.descriptor_facts();
    if facts.param_tags_overflow
        || num_params > cratonvm_jit_api::DescriptorFacts::INLINE_PARAMS
        || facts.param_tag_len as usize != num_params
    {
        return None;
    }
    Some(facts)
}

/// How often a door folds its per-callee invocation counter into the shared
/// profile store. The store still sees every call; it sees them in batches, so
/// the per-call path is one relaxed `fetch_add` instead of a sharded lock and
/// a hash lookup.
const INVOCATION_SYNC_EVERY: u32 = 16;

/// The tier-up bookkeeping a door owes, in the shape the general path performs
/// it: count the call, and on the threshold (and every `JIT_RETRY_STRIDE`
/// after it) offer the method to the tiered manager.
///
/// Returns `false` when the caller must decline the door — the non-background
/// upgrade path wants the cache entry's `RedefineGate`, which a door holding
/// only a borrowed `Arc` does not have.
#[inline]
pub(super) fn note_invocation_for_tierup(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
) -> bool {
    let threshold = shared.config.jit_invocation_threshold;
    let cnt = count_door_invocation(shared, cached);
    let should_attempt = should_offer_tier_up(cnt, threshold);
    if !should_attempt {
        return true;
    }
    if !crate::runtime::env_cache::bg_compile() {
        // The inline upgrade needs the entry's gate; leave it to the general
        // dispatcher, which has it.
        return false;
    }
    ensure_bg_compiler_started(shared);
    // The same release the general dispatchers make at this stride. This
    // door is where a hot static callee's calls go once it is warm, so a
    // thread running interpreted code through it would otherwise never
    // release the withdrawn bodies its caches pin. Measured: without this
    // line the in-process sweep test reclaimed 1 of 79 withdrawn bodies.
    super::jit_bridge::release_withdrawn_code_owners(thread);
    let _ = offer_invocation_to_tiered_manager(shared, &**cached, cnt as u64);
    true
}

/// Kill switch for the doors' plain (non-atomic read-modify-write) invocation
/// count (interpreter round i1 wave 22, lane L4). `true`: [`count_door_invocation`]
/// bumps the callee's counter with a relaxed load and a relaxed store. `false`:
/// the saturating `fetch_update` of waves 1-21, a `lock cmpxchg` per counted
/// call.
///
/// # Why a lossy counter is the right one
///
/// The count is a tier-up heuristic, not an accounting: HotSpot's own
/// interpreter invocation counters are plain, racy increments for the same
/// reason. What the locked form cost was paid on EVERY call a door counts —
/// every warm static, private, super and constructor call of a callee that
/// is not compiled yet, and for the life of the process every call of a
/// callee the tiered manager never compiles (settled, refused, too big) —
/// and a `lock cmpxchg` is a full fence that also drains the store buffer the
/// door has just filled building the callee's frame. Two threads racing on
/// one callee can lose an increment (the count reaches the threshold a few
/// calls late, or repeats a value, so an offer can be made twice — the tiered
/// manager's `tiering_settled` stamp and its own queue absorb that); it can
/// neither wrap (the saturation below is kept) nor reach the threshold early.
const DOOR_COUNT_IS_PLAIN: bool = true;

/// Count one call of `cached` the way a door counts it: on the callee's own
/// counter, folded into the shared profile store every
/// [`INVOCATION_SYNC_EVERY`] calls. Returns the new count.
#[inline]
pub(super) fn count_door_invocation(shared: &SharedVm, cached: &CachedBytecodeMethod) -> u32 {
    use std::sync::atomic::Ordering;
    // Saturating: `fetch_add(1) + 1` wrapped the per-call-site counter after
    // 2^32 calls -- a panic in a debug build, and in release a counter that
    // restarts below the threshold, so a method that was hot enough to be
    // offered every 64 calls silently stops being offered for 500 calls.
    let cnt = if DOOR_COUNT_IS_PLAIN {
        // See `DOOR_COUNT_IS_PLAIN`: no locked instruction on the call path.
        let previous = cached.interp_invocations.load(Ordering::Relaxed);
        match previous.checked_add(1) {
            Some(next) => {
                cached.interp_invocations.store(next, Ordering::Relaxed);
                next
            }
            None => previous,
        }
    } else {
        match cached
            .interp_invocations
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        {
            Ok(previous) => previous + 1,
            Err(saturated) => saturated,
        }
    };
    if cnt == 1 {
        // JIT round 19, lane eager19 (EAGERCOLD18-4): the FIRST call folds at
        // once, so `ProfileStore::invocation_count > 0` ("has run", the eager
        // chain's gates) is exact for a callee every call of which came
        // through a door; the stride fold at 16 then adds the other 15.
        if door_exact_first_count_enabled() {
            shared
                .jit
                .profile_store
                .add_invocations(cached.invoc_key(), 1);
        }
    } else if cnt % INVOCATION_SYNC_EVERY == 0 {
        let fold = if cnt == INVOCATION_SYNC_EVERY && door_exact_first_count_enabled() {
            INVOCATION_SYNC_EVERY - 1
        } else {
            INVOCATION_SYNC_EVERY
        };
        shared
            .jit
            .profile_store
            .add_invocations(cached.invoc_key(), fold);
    }
    cnt
}

/// `CRATONVM_JIT_DOOR_EXACT_FIRST_COUNT` (JIT round 19, lane eager19,
/// EAGERCOLD18-4 of `docs/feature-designs/jit-ir-optimizations.md`; **default OFF**): does
/// [`count_door_invocation`] fold a method's first call into the profile store
/// immediately (and 15 instead of 16 at the first stride), so the store's
/// count never reads `0` for a method that has run? Asked at most twice per
/// method (its 1st and 16th door call), never on the per-call path.
#[cold]
fn door_exact_first_count_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_DOOR_EXACT_FIRST_COUNT")
}

/// What [`door_tier_up`] tells a door to do with the call it has accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DoorTierUp {
    /// Counted (and offered, at a stride): serve the call interpreted.
    Serve,
    /// The callee has a compiled body: decline, the general arm enters it.
    CompiledBody,
    /// An inline (`CRATONVM_BG_COMPILE=0`) upgrade is due: decline, the
    /// general arm has the entry's gate it needs.
    InlineUpgradeDue,
}

/// The tier-up bookkeeping of the non-virtual door for a `Bytecode` entry
/// (interpreter round i1 wave 19; out of line since wave 22, lane L4): the
/// compiled-body probe, then the count and the stride offer, in the static
/// door's order.
///
/// `#[inline(never)]` on purpose. [`nonvirtual_door_finish`] is inlined into
/// two callers — the special door in the dispatch loop's `0xb7` arm and the
/// virtual door's private hand-off, itself inlined into the `0xb6` arm — and
/// this block (a `JitCache` read lock on a generation change, the tiered
/// manager's offer, the withdrawn-code release) is most of the code wave 19
/// added to both. Under `--nojit` none of it runs, but all of it was inlined
/// into the interpreter's dispatch function, and the wave-19 measurement
/// (`SpecialDoorTierUpBench`, `--nojit`, +6-9% on four rows) cannot be
/// explained by the few loads and branches the `--nojit` path executes. Out of
/// line, the `--nojit` door is the wave-18 door plus one predicate; with the
/// JIT on it costs one call per counted call, which the plain count
/// ([`DOOR_COUNT_IS_PLAIN`]) more than repays.
#[inline(never)]
pub(super) fn door_tier_up(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
) -> DoorTierUp {
    if callee_has_compiled_body(shared, cached) {
        return DoorTierUp::CompiledBody;
    }
    if !note_invocation_for_tierup(shared, thread, cached) {
        return DoorTierUp::InlineUpgradeDue;
    }
    DoorTierUp::Serve
}

/// Whether a JIT-compiled body exists for this callee. A door declines when
/// one does — `execute_jit_call` wants `Value` arguments and its own ABI
/// checks, which is the general dispatcher's job.
#[inline]
pub(super) fn callee_has_compiled_body(shared: &SharedVm, cached: &CachedBytecodeMethod) -> bool {
    let jit_generation = shared.jit.jit_cache.generation();
    if cached.jit_probe_is_current(jit_generation) {
        return false;
    }
    let found = shared.jit.jit_cache.read().get(
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
        cached.declaring_class_id,
    );
    if found.is_none() {
        cached.record_jit_probe_miss(jit_generation);
        return false;
    }
    true
}

/// What a non-virtual door owes a bytecode callee on the thread it runs on
/// (interpreter round i5 wave 114, lane L6, `CRATONVM_VTHREAD_NONVIRTUAL_DOORS`;
/// [`vthread_door_gate`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VthreadDoorGate {
    /// Not a virtual thread, or a callee the virtual-thread doors admit
    /// compiled: the door's own (platform) tier-up block, whose compiled-body
    /// decline hands the call to the general arm that enters it.
    Platform,
    /// A virtual thread and a callee whose compiled entry the general arm
    /// refuses there: served interpreted, with no compiled-body probe and no
    /// tier-up offer (the general arms make neither for it).
    Interpret,
    /// A `synchronized` callee on a virtual thread: declined, so its monitor
    /// is taken by the general arm (`monitor_enter_synchronized_method_or_defer`,
    /// which may unmount a contended virtual thread) and never by the door's
    /// uncontended acquire.
    DeclineSynchronized,
}

/// The non-virtual doors' answer for `cached` on `thread`. The doors run on a
/// virtual thread only under `CRATONVM_VTHREAD_NONVIRTUAL_DOORS`
/// (`interpreter::execute_frame_from_index`, `nonvirtual_fast_door_on`); on any
/// other thread this is one `kind` compare. The admission mirrors the general
/// arm of each door exactly: `execute_invokestatic_cached`'s `Bytecode` arm
/// (`compiled_entry_refused`) for the static door, and
/// `execute_invokevirtual_cached`'s special arm (its `on_virtual_thread` gate)
/// for the special one, so a call is compiled where the general arm would
/// compile it and interpreted where it would interpret it.
///
/// Interpreter round i6 wave 115, lane L6 (`CRATONVM_VTHREAD_VIRTUAL_DOOR`):
/// the virtual / interface door asks it too, with `static_door = false`,
/// which is also exactly the general `VirtualBytecode` arm's
/// `vthread_refused` (`dispatch_virtual::execute_invokevirtual_cached_probed`).
#[inline]
pub(super) fn vthread_door_gate(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &CachedBytecodeMethod,
    static_door: bool,
) -> VthreadDoorGate {
    if !matches!(thread.kind, crate::threading::ThreadKind::Virtual) {
        return VthreadDoorGate::Platform;
    }
    if cached.is_synchronized {
        return VthreadDoorGate::DeclineSynchronized;
    }
    let config = &shared.config;
    // `--nojit`: nothing to probe, count or enter (the doors' tier-up blocks
    // are skipped either way); no leaf scan.
    if !config.jit_enabled {
        return VthreadDoorGate::Interpret;
    }
    let admitted = if static_door {
        crate::vm::jdk_continuation::vthread_may_enter_compiled_method(shared, thread, cached)
    } else {
        (config.vthread_compiled_instance_leaf
            || config.vthread_compiled_learned_entry
            || config.vthread_compiled_app_entry)
            && crate::vm::jdk_continuation::vthread_may_enter_compiled_method(shared, thread, cached)
    };
    if admitted {
        VthreadDoorGate::Platform
    } else {
        VthreadDoorGate::Interpret
    }
}

/// Count one call of `cached` whose compiled entry a virtual thread is refused
/// (`CRATONVM_VTHREAD_LOCAL_CALL_COUNT`, wave 114, lane L6): the door's count
/// ([`count_door_invocation`]) until the entry's count is past
/// `threshold + LEARNED_NOMINATION_WINDOW_CALLS`, then nothing at all (no
/// store, so the entry's line stays shared between carriers), answering a
/// count at which no stride is due: past the window the refused arm offers
/// nothing anyway. Returns the count for `should_offer_tier_up`.
#[inline]
pub(super) fn count_refused_vthread_invocation(
    shared: &SharedVm,
    cached: &CachedBytecodeMethod,
    threshold: u32,
) -> u32 {
    // `quiet - threshold` is the window plus one: not a multiple of the retry
    // stride, and past the nomination window.
    if let Some(quiet) = threshold
        .checked_add(crate::vm::jdk_continuation::LEARNED_NOMINATION_WINDOW_CALLS)
        .and_then(|end| end.checked_add(1))
    {
        if cached
            .interp_invocations
            .load(std::sync::atomic::Ordering::Relaxed)
            >= quiet
        {
            return quiet;
        }
    }
    count_door_invocation(shared, cached)
}

/// The static door's count of a call it serves interpreted under
/// [`VthreadDoorGate::Interpret`]. `false`: decline instead, without counting,
/// because the next count is a due stride inside the learned arm's nomination
/// window (`jdk_continuation::vthread_learned_nominates`); the general arm
/// then counts the call on the same counter
/// (`CRATONVM_VTHREAD_NONVIRTUAL_DOORS` implies
/// `CRATONVM_VTHREAD_LOCAL_CALL_COUNT`) and makes the nomination. At most
/// `LEARNED_NOMINATION_STRIDES` such declines per entry.
#[inline]
pub(super) fn vthread_refused_door_count(shared: &SharedVm, cached: &CachedBytecodeMethod) -> bool {
    let threshold = shared.config.jit_invocation_threshold;
    if shared.config.vthread_compiled_learned && crate::runtime::env_cache::bg_compile() {
        let next = cached
            .interp_invocations
            .load(std::sync::atomic::Ordering::Relaxed)
            .saturating_add(1);
        if should_offer_tier_up(next, threshold)
            && next.saturating_sub(threshold)
                < crate::vm::jdk_continuation::LEARNED_NOMINATION_WINDOW_CALLS
        {
            return false;
        }
    }
    let _ = count_refused_vthread_invocation(shared, cached, threshold);
    true
}

/// Engagement census for the two doors. `CRATONVM_DBG_FIELD_SITE=1` prints
/// `door: static hit/miss special hit/miss` with the rest of the site caches,
/// and names the first few reasons a door declined — a door that never fires
/// is invisible on a wall clock, which is how the field fast path shipped its
/// first version measuring nothing.
#[cold]
#[inline(never)]
fn report_decline(kind: &'static str, why: &'static str) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static REPORTED: AtomicU32 = AtomicU32::new(0);
    if !site_stats::on() {
        return;
    }
    // COUNT EVERY DECLINE, not just the first twelve. `door: special
    // hit=390521 miss=2075102` on a collator workload says the door is
    // declining 84% of the calls and the twelve printed lines cannot say which
    // of the ten reasons that is -- and the ten want different repairs. The
    // table has one row per (kind, reason) pair, so it is bounded by the number
    // of `decline!` sites; a linear scan under a mutex is free at a call site
    // that only runs when the census is armed.
    {
        let mut table = DECLINE_REASONS.lock();
        match table.iter_mut().find(|(k, w, _)| *k == kind && *w == why) {
            Some(row) => row.2 += 1,
            None => table.push((kind, why, 1)),
        }
    }
    if REPORTED.fetch_add(1, Ordering::Relaxed) >= 12 {
        return;
    }
    eprintln!("[invoke-door] {kind} declined: {why}");
}

/// One row per `(door, reason)` pair the run declined on. See [`report_decline`].
static DECLINE_REASONS: parking_lot::Mutex<Vec<(&'static str, &'static str, u64)>> =
    parking_lot::Mutex::new(Vec::new());

/// Print the decline tally, highest first. Called from the final site-cache
/// dump so a short run still reports.
pub(crate) fn dump_decline_reasons() {
    {
        use std::sync::atomic::Ordering;
        let elided = FRAMELESS_CENSUS[FRAMELESS_EMPTY_ELIDED].load(Ordering::Relaxed);
        let framed = FRAMELESS_CENSUS[FRAMELESS_EMPTY_FRAMED].load(Ordering::Relaxed);
        if elided + framed > 0 {
            eprintln!(
                "[invoke-door] empty-body callees: elided={elided} framed={framed} (elision {})",
                if EMPTY_BODY_ELISION { "on" } else { "off" }
            );
        }
        let elided = FRAMELESS_CENSUS[FRAMELESS_CTOR_ELIDED].load(Ordering::Relaxed);
        let framed = FRAMELESS_CENSUS[FRAMELESS_CTOR_FRAMED].load(Ordering::Relaxed);
        if elided + framed > 0 {
            eprintln!(
                "[invoke-door] trivial constructors: elided={elided} framed={framed} (elision {})",
                if TRIVIAL_CTOR_ELISION { "on" } else { "off" }
            );
        }
        let elided = FRAMELESS_CENSUS[FRAMELESS_FIELD_CTOR_ELIDED].load(Ordering::Relaxed);
        let framed = FRAMELESS_CENSUS[FRAMELESS_FIELD_CTOR_FRAMED].load(Ordering::Relaxed);
        if elided + framed > 0 {
            eprintln!(
                "[invoke-door] field-store constructors: elided={elided} framed={framed} (elision {})",
                if FIELD_CTOR_ELISION { "on" } else { "off" }
            );
        }
        let chained = FRAMELESS_CENSUS[FRAMELESS_SUPER_CHAIN_ELIDED].load(Ordering::Relaxed);
        if chained > 0 {
            eprintln!(
                "[invoke-door] constructors elided over a super chain: {chained} (of the two lines above)"
            );
        }
    }
    let table = DECLINE_REASONS.lock();
    if table.is_empty() {
        return;
    }
    let mut rows: Vec<_> = table.clone();
    rows.sort_by(|a, b| b.2.cmp(&a.2));
    let total: u64 = rows.iter().map(|r| r.2).sum();
    eprintln!("[invoke-door] declines by reason (total {total}):");
    for (kind, why, n) in rows {
        let pct = if total == 0 {
            0.0
        } else {
            n as f64 * 100.0 / total as f64
        };
        eprintln!("[invoke-door]   {n:>10}  {pct:5.1}%  {kind}: {why}");
    }
}

/// Record a decline by the VIRTUAL door, which has no `hit/miss` counters of
/// its own but is the door tried FIRST for `invokevirtual`.
///
/// Without this the census attributed its declines to the non-virtual door
/// that runs after it: "special: cached target is VirtualBytecode on a
/// non-special call" was 40% of every decline on a collator workload and meant
/// only "the virtual door already said no", naming nothing.
#[inline]
pub(crate) fn note_virtual_decline(why: &'static str) {
    report_decline("virtual", why);
}

/// Kill switch for the doors' redefinition handling (interpreter round i1
/// wave 18, lane L4). `true`: a redefinition anywhere costs the doors nothing
/// beyond what it retires. The CALLER side of an entry — a redefined caller's
/// constant pool may name another member at the same `(class, cp index)` —
/// is retired by the invoke cache itself (`InvokeCache::get` drops its maps
/// once after any redefinition, `INVOKE_CACHE_RETIRES_REDEFINED_CALLERS`),
/// so the doors inherit it with the general dispatchers. The TARGET side is
/// per class: the lookup filters an entry whose declaring class was
/// redefined since the fill (`RedefineGate::is_stale`), and an entry filled
/// after a redefinition of its target is served as the general path serves
/// it: since wave 39 probed for a compiled body and offered for tier-up like
/// any other at the `Bytecode` arms and the static and non-virtual doors, and
/// since wave 40 at the `VirtualBytecode` arm and the virtual door too
/// ([`REDEFINED_TARGETS_TIER_UP`]). `false`: the
/// historical latch — every door declines every call for the rest of the
/// process after the first redefinition of any class, and a redefined
/// target always takes the general path.
pub(super) const DOORS_SURVIVE_REDEFINITION: bool = true;

/// Whether the doors are latched off by a redefinition: never, unless the
/// kill switch restores the latch.
#[inline(always)]
pub(super) fn doors_latched_off_by_redefinition() -> bool {
    !DOORS_SURVIVE_REDEFINITION && crate::classloading::any_class_redefined()
}

/// Whether a door declines a target whose declaring class was ever
/// redefined (the entry's gate generation): only under the kill switch.
#[inline(always)]
pub(super) fn door_declines_redefined_target(gate_generation: u32) -> bool {
    !DOORS_SURVIVE_REDEFINITION && gate_generation != 0
}

/// Kill switch for the tier-up of a target redefined before its entry was
/// filled (interpreter round i1 wave 39, lane L2;
/// `docs/feature-designs/profile-guided-inlining.md`).
/// `true`: the static and non-virtual doors, and the `Bytecode` arms of
/// `execute_invokestatic_cached` and `execute_invokevirtual_cached`, probe
/// such a target for a compiled body and offer it to the tiered manager like
/// any other; since wave 40 (lane L2) so do the virtual door and the
/// `VirtualBytecode` arm, and a compiled caller's bytecode-callee templates
/// serve it (`jit::helpers::callee_template_declines_redefined_target`). The entry's gate is checked on every hit, so an entry filled
/// before the class's latest redefinition never reaches here, and one filled
/// after it holds the current body, which every compiled body of the class
/// is compiled from (a redefinition racing a compile makes the body
/// unpublishable, and withdraws what depends on the class). The upgrade door
/// and the background worker compile a redefined class since wave 38.
/// `false`: the pre-wave-39 policy -- served interpreted, counted by the
/// static door only, never probed and never offered.
/// SWITCHED OFF by the wave-40 orchestrator (2026-10-04): with it on, a
/// multi-class redefinition tears for compiled readers
/// (`tools/probes/interp/L3/L3W38RedefineTwoClassesSamePool.java`, 60
/// interleaved runs on the host: `dev` 59/60 and 60/60, on 36/60 and 47/60
/// even with the publication hold, off 60/60 and 60/60). See
/// `docs/feature-designs/class-resolution-and-caching.md`.
/// SWITCHED BACK ON by interpreter round i1 wave 42, lane L2, with the
/// bytecode-read witness of the compile doors (`jit_bridge::try_jit_compile_callee_slow`:
/// a compile of the old bytecode was published after the redefinition); in a
/// commit of its own, so the host can judge the page's table with and without it.
pub(super) const REDEFINED_TARGETS_TIER_UP: bool = true;

/// Whether a door or `Bytecode` arm treats a target whose declaring class was
/// redefined before the fill (a non-zero gate generation) as one that never
/// tiers up: only under [`REDEFINED_TARGETS_TIER_UP`]'s kill switch. A
/// constant `false` otherwise, so the doors carry no branch for it.
#[inline(always)]
pub(super) fn tier_up_skips_redefined_target(gate_generation: u32) -> bool {
    !REDEFINED_TARGETS_TIER_UP && gate_generation != 0
}

/// Note a decline and return `None`, in one expression.
///
/// One out-of-line call per site ([`note_decline`]): since wave 50
/// `site_stats::bump` tests its gate inline, and the doors' 34 decline sites
/// each grew by that `OnceLock` test, which cost the special door's hit
/// (`InvokeDoorCostBench` `super-call` +5-8% locally, +3.8% on the host)
/// for a count that only `CRATONVM_DBG_FIELD_SITE` reads (interpreter round
/// i1 wave 51, lane L7).
macro_rules! decline {
    ($kind:literal, $miss:expr, $why:literal) => {{
        note_decline($miss, $kind, $why);
        return None;
    }};
}

/// The body of every [`decline!`]: the miss count and the reason census.
#[cold]
#[inline(never)]
fn note_decline(miss: usize, kind: &'static str, why: &'static str) {
    site_stats::bump(miss);
    report_decline(kind, why);
}

// ── invokestatic (0xb8) ──────────────────────────────────────────────────

/// Monomorphic `invokestatic` fast door. See the module note for the contract.
#[inline]
pub(super) fn execute_invokestatic_fast_door(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    site_pc: usize,
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    const MISS: usize = site_stats::DOOR_STATIC_MISS;
    if doors_latched_off_by_redefinition() {
        decline!("static", MISS, "a class was redefined");
    }
    let caller_class_id = thread.frames[frame_idx].class_id;
    // The owner stamp (`CRATONVM_INTERP_STATIC_OWNER_STAMP`; interpreter
    // round i4 wave 84, lane L7): read BEFORE the lookup, whose returned
    // borrow of the invoke cache lasts until the entry is owned below. Only
    // once a user-defined loader has defined a class: before that the guard
    // is one load (`any_defining_loader_registered`) and there is nothing to
    // stamp.
    let owner_stamp_on = shared.config.interp_static_owner_stamp
        && cratonvm_native_builtins::classloader::any_defining_loader_registered(&shared.loader_identity);
    let owner_stamp = if owner_stamp_on {
        thread
            .invoke_cache
            .site_stamp(caller_class_id, cp_index, false, site_pc as u32)
    } else {
        None
    };
    // The lookup has already retired an entry whose caller or target was
    // redefined since it was filled (see `DOORS_SURVIVE_REDEFINITION`).
    //
    // The entry is BORROWED until the tier-up block (wave 24): everything
    // before it reads only `thread.frames`, a field disjoint from
    // `thread.invoke_cache`, so an empty static body is answered without the
    // `Arc` clone and drop the non-virtual door also stopped paying for
    // `Object.<init>`.
    let (cached, target_redefined) =
        match thread
            .invoke_cache
            .get(caller_class_id, cp_index, false, site_pc as u32)
        {
            Some(CachedInvokeTarget::Bytecode { cached, gate }) => {
                if door_declines_redefined_target(gate.generation) {
                    decline!("static", MISS, "the target class has been redefined");
                }
                (cached, tier_up_skips_redefined_target(gate.generation))
            }
            Some(CachedInvokeTarget::Native { .. }) => {
                decline!("static", MISS, "cached target is a registered native")
            }
            Some(CachedInvokeTarget::Jit { .. }) => {
                decline!("static", MISS, "cached target is a compiled body")
            }
            Some(_) => decline!("static", MISS, "cached target is not plain bytecode"),
            None => decline!("static", MISS, "inline cache miss"),
        };
    if !cached.is_static {
        decline!("static", MISS, "cached target is not static");
    }
    // Static synchronized is served (round 11 wave 6): `door_monitor_acquire`
    // takes the class mirror when it already exists. It used to be declined
    // HERE, before the tier-up bookkeeping below, because a late refusal of
    // EVERY call double-counted each one (this door, then the general path)
    // and reached the compile threshold at half the calls. The late refusal
    // is now the exception -- the first call, before any mirror exists, and a
    // contended acquire -- so only those calls are counted twice.
    let num_params = cached.num_params as usize;
    let Some(facts) = callee_is_plain_bytecode(shared, &cached, num_params) else {
        decline!(
            "static",
            MISS,
            "callee is synchronized, native-backed, intercepted, or over-arity"
        );
    };
    // The loader-split guard the general path applies to every static hit.
    // Bitmap-gated: two relaxed loads when no defining loader is registered.
    //
    // Under the owner stamp the verdict is answered from the site's stamp
    // when it names this entry's declaring class and the VM's current
    // definition / resolution epochs -- exactly the inputs
    // `cached_static_owner_stale`'s thread-local memo keys on (the cache is
    // this thread's, of this VM). The epochs are read before the verdict, so
    // a move during it leaves a stamp the next call refuses. A "not stale"
    // answer is restamped once the entry's borrow ends (`restamp`).
    let mut restamp: Option<[u64; 3]> = None;
    if owner_stamp_on {
        let (definition, resolution) = static_owner_epochs(shared);
        let want = [
            u64::from(cached.declaring_class_id.as_u32()),
            definition,
            resolution,
        ];
        if owner_stamp != Some(want) {
            if cached_static_owner_stale(shared, caller_class_id, &cached) {
                decline!("static", MISS, "loader-split owner");
            }
            restamp = Some(want);
        }
    } else if cached_static_owner_stale(shared, caller_class_id, &cached) {
        decline!("static", MISS, "loader-split owner");
    }
    if thread
        .frames
        .at_frame_limit(0, shared.config.max_stack_depth)
    {
        decline!("static", MISS, "frame stack is full");
    }
    // The arguments are read BEFORE the tier-up bookkeeping below, for the
    // reason the synchronized refusal above gives: a coercion decline taken
    // after `note_invocation_for_tierup` had counted the call was counted again
    // by the general path (`profile_store.increment_invocation`), so a callee
    // whose sites pass unmarked category-2 values reached its threshold at
    // half the calls. Nothing from here to the push reaches a safepoint (the
    // tier-up offer only queues a compile on the background worker).
    //
    // Stage 2b (wave 38, lane L7): with argument overlap on, the slots are
    // validated where they are and never copied (`push_frame_in_place`).
    // Not on a virtual thread (interpreter round i5 wave 114, lane L6,
    // `CRATONVM_VTHREAD_NONVIRTUAL_DOORS`): the general arm a virtual thread
    // took before never laid a callee's locals over its caller's stack, and a
    // frozen continuation's precise root deposit was not reviewed for it.
    let in_place =
        in_place_args(shared) && !matches!(thread.kind, crate::threading::ThreadKind::Virtual);
    let mut slots = empty_arg_slots();
    let verbatim = if in_place {
        validate_args_verbatim(&thread.frames[frame_idx].stack, facts, num_params, false)
    } else {
        read_args_verbatim(
            &thread.frames[frame_idx].stack,
            facts,
            num_params,
            false,
            &mut slots,
        )
    };
    if !verbatim {
        decline!("static", MISS, "an argument slot needs coercion");
    }
    // An empty static body is answered without a frame and without tier-up
    // bookkeeping, as the non-virtual door answers one since wave 20
    // (`STATIC_DOOR_ELIDES_EMPTY_BODY`, wave 22). The invoke cache is filled
    // only once the declaring class is initialized (`dispatch_static.rs`, the
    // fill's initialization gate; an initializer's own calls go to
    // `CLINIT_SITE_MEMO`, never to this cache), so no initialization is
    // skipped with the frame.
    if STATIC_DOOR_ELIDES_EMPTY_BODY && body_returns_at_entry(&cached) {
        let elide = empty_body_elidable(shared, &cached);
        note_empty_body_call(shared.config.site_census, elide);
        if elide {
            if let Some(words) = restamp {
                thread.invoke_cache.set_site_stamp(
                    caller_class_id,
                    cp_index,
                    false,
                    site_pc as u32,
                    words,
                );
            }
            thread.frames[frame_idx].stack.discard_top(num_params);
            site_stats::bump_if(shared.config.site_census, site_stats::DOOR_STATIC_HIT);
            dbg_invoke_stats_record(shared.config.invoke_stats, 0);
            return Some(Ok(CachedCallResult::Handled));
        }
    }
    // A frame will be pushed (or a late decline taken): own the entry now,
    // which ends the borrow of `thread.invoke_cache` before `thread` is
    // passed whole. `push_frame_verbatim` moves this clone into the frame --
    // or, when the retired slot already runs the entry, this is an alias of
    // the slot's handle and no clone is made (`DoorCallee`, wave 47).
    let cached = DoorCallee::of(shared, &thread.frames, cached);
    // The entry is owned now: the invoke cache is free for the stamp.
    if let Some(words) = restamp {
        thread
            .invoke_cache
            .set_site_stamp(caller_class_id, cp_index, false, site_pc as u32, words);
    }
    // Interpreter round i5 wave 114, lane L6 (`CRATONVM_VTHREAD_NONVIRTUAL_DOORS`,
    // the only way a virtual thread reaches this door): the general arm's
    // virtual-thread answer for this callee. One `kind` compare elsewhere.
    let vthread_gate = vthread_door_gate(shared, thread, &cached, true);
    match vthread_gate {
        VthreadDoorGate::Platform => {}
        VthreadDoorGate::DeclineSynchronized => {
            decline!("static", MISS, "synchronized callee on a virtual thread")
        }
        VthreadDoorGate::Interpret => {
            if shared.config.jit_enabled && !vthread_refused_door_count(shared, &cached) {
                decline!(
                    "static",
                    MISS,
                    "virtual thread: a learned nomination stride is due"
                );
            }
        }
    }
    if shared.config.jit_enabled && vthread_gate == VthreadDoorGate::Platform {
        if target_redefined {
            // `execute_invokestatic_cached`'s `Bytecode` arm for a redefined
            // target: the call is counted, but no compiled body is probed for
            // and nothing is offered to the tiered manager.
            let _ = count_door_invocation(shared, &cached);
        } else {
            if callee_has_compiled_body(shared, &cached) {
                decline!("static", MISS, "callee has a compiled body");
            }
            if !note_invocation_for_tierup(shared, thread, &cached) {
                decline!("static", MISS, "an inline tier-up attempt is due");
            }
        }
    }
    // A static synchronized callee locks its EXISTING class mirror (see
    // `door_monitor_acquire`); no mirror yet, or contention, declines.
    let Some(monitor) = door_monitor_acquire(shared, thread, &cached, None) else {
        decline!(
            "static",
            MISS,
            "static synchronized: contended, or no class mirror yet"
        );
    };
    site_stats::bump_if(shared.config.site_census, site_stats::DOOR_STATIC_HIT);
    dbg_invoke_stats_record(shared.config.invoke_stats, 0);
    if in_place {
        return Some(Ok(push_frame_in_place(
            shared, thread, frame_idx, cached, num_params, false, monitor,
        )));
    }
    Some(Ok(push_frame_verbatim(
        shared, thread, frame_idx, cached, &slots, num_params, monitor,
    )))
}

// ── interpreted lambda call (0xb9 on a lambda proxy) ────────────────────

/// The `invokeinterface` door for a lambda proxy receiver of a shape
/// `LambdaJitSite` recognizes (SBR5 lane interp; see the note above
/// `lambda::lambda_door_lookup`).
///
/// Reached after the virtual door declined, so an ordinary receiver costs it
/// one thread-local read (`lambda_door_live`) and, once the thread has a lambda
/// site, one table probe. It serves a call only when it can reproduce
/// `try_lambda_dispatch`'s answer exactly: every operand is in the shape the
/// callee's locals take verbatim, every replayed `checkcast` passes without a
/// class load, and the frame stack has room. A compiled impl body is entered
/// from here since nio21 wave 16 L2 ([`lambda_door_call_compiled`]; before, the
/// door declined it to the general path's one-shot entry). Anything else
/// returns `None` and the call takes the general path, which throws the
/// exceptions.
#[inline(never)]
pub(super) fn execute_lambda_door(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    // The `invokeinterface`'s own pc, for the receiver profile (JIT r23 w2 L2).
    site_pc: usize,
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    if !super::lambda::lambda_door_live() {
        return None;
    }
    if shared.config.overlap_interpreter_args {
        return None;
    }
    // Interpreter round i6 wave 115, lane L6 (`CRATONVM_VTHREAD_VIRTUAL_DOOR`,
    // the only way a virtual thread reaches this door): not on a virtual
    // thread. Its tier-up block counts and offers a callee the virtual-thread
    // general lambda arm was never reviewed against; it keeps that arm.
    //
    // Interpreter round i6 wave 122, lane L6 (`CRATONVM_VTHREAD_LAMBDA_DOOR`,
    // default off; proposal `i114-L6-proposal-the-virtual-door-on-virtual-threads`,
    // "Wave 122"): a JDK virtual thread takes the door too, and then skips its
    // tier-up block entirely (`on_virtual_thread` below), as the general arm
    // (`lambda::try_invoke_cached_lambda_impl`) skips its whole compiled block
    // on a virtual thread: no compiled-body probe, no count, no offer, no
    // compiled entry. What is left is the interpreted push both answer.
    let on_virtual_thread = matches!(thread.kind, crate::threading::ThreadKind::Virtual);
    if on_virtual_thread
        && !(shared.config.vthread_lambda_door && thread.jdk_continuation.jdk())
    {
        return None;
    }
    #[cfg(feature = "experimental-debug")]
    if shared.debug.debugger_gates.c_lambda_frames_armed() {
        return None;
    }
    let caller_class_id = thread.frames[frame_idx].class_id;
    let (site, recv_ptr, recv_class) = {
        let stack = &thread.frames[frame_idx].stack;
        let heap = &shared.mem.heap;
        let mut recv_ptr = 0u64;
        let mut recv_class = ClassId::new(0);
        let site = super::lambda::lambda_door_lookup(
            shared.vm_identity,
            caller_class_id,
            cp_index,
            |num_args| {
                if stack.len() <= num_args {
                    return None;
                }
                let ptr = stack.peek_compact_at(num_args).as_object_ptr()?;
                if !heap.is_object_start(ptr as usize) {
                    return None;
                }
                // SAFETY: `ptr` is a registered object start on this heap.
                let header = unsafe { &*(ptr as *const cratonvm_gc::ObjectHeader) };
                if header.kind() == cratonvm_types::ObjectKind::Array {
                    return None;
                }
                let class_id = header.class_id;
                if !shared.classes.is_lambda_proxy_class(class_id) {
                    return None;
                }
                recv_ptr = ptr;
                recv_class = class_id;
                Some(class_id)
            },
        )?;
        (site, recv_ptr, recv_class)
    };
    let cached: &Arc<CachedBytecodeMethod> = site.cached_impl();
    let num_captures = site.num_captures();
    let total_args = site.total_args();
    let num_args = total_args - num_captures;
    if !shared.config.descriptor_facts || total_args > ARG_SLOTS_LEN {
        return None;
    }
    let facts = cached.descriptor_facts();
    if facts.param_tags_overflow
        || total_args > cratonvm_jit_api::DescriptorFacts::INLINE_PARAMS
        || facts.param_tag_len as usize != total_args
    {
        return None;
    }
    if thread
        .frames
        .at_frame_limit(0, shared.config.max_stack_depth)
    {
        return None;
    }
    let mut slots = empty_arg_slots();
    {
        let stack = &thread.frames[frame_idx].stack;
        // The replayed `checkcast`s, over the operand stack's own slots. A
        // `Some(false)` verdict is "no cast needed to throw"; anything else
        // (a failing cast, or one that would need a class load) declines.
        //
        // nio21 w16 L2 (proposal 6 of `jit-r21-nio-proposals-w15-L5.md`):
        // through the site's per-cast pass memo (`lambda_site_cast_passes`),
        // one class-id compare on a monomorphic site; its `false` is exactly
        // the verdict's `Some(true) | None`.
        for (at, (sam_idx, _)) in site.checkcast_list().iter().enumerate() {
            let (cv, _) = stack.peek_with_kind_at(num_args - 1 - *sam_idx);
            let Some(ptr) = cv.as_object_ptr() else {
                continue;
            };
            if ptr == 0 {
                continue;
            }
            // SAFETY: a non-null object slot of a verified operand stack holds
            // a live heap address, as `read_args_verbatim` relies on.
            let obj = unsafe { ObjectRef::from_raw(ptr as *mut u8) };
            if !super::lambda::lambda_site_cast_passes(shared, &site, at, obj) {
                return None;
            }
        }
        // SAFETY: `recv_ptr` was a registered object start under this same
        // safepoint-free span (nothing since has run Java).
        let proxy = unsafe { ObjectRef::from_raw(recv_ptr as *mut u8) };
        for i in 0..num_captures {
            let tag = facts.param_tags[i];
            let (cv, kind) = match shared.mem.heap.get_field(proxy, i) {
                Value::Int(v) => (CompactValue::int(v), 0u8),
                Value::Float(v) => (CompactValue::float(v), 0u8),
                Value::Long(v) => (
                    CompactValue::long(v),
                    crate::runtime::ValueStack::KIND_MARK_LONG,
                ),
                Value::Double(v) => (
                    CompactValue::double_raw(v),
                    crate::runtime::ValueStack::KIND_MARK_DOUBLE,
                ),
                Value::Object(Some(obj)) => (CompactValue::object(obj.as_ptr() as u64), 0u8),
                Value::Object(None) => (CompactValue::null(), 0u8),
                _ => return None,
            };
            if !arg_slot_is_verbatim(cv, kind, tag) {
                return None;
            }
            slots.slots[i] = std::mem::MaybeUninit::new((cv, tag));
        }
        for k in 0..num_args {
            let (cv, kind) = stack.peek_with_kind_at(num_args - 1 - k);
            let tag = facts.param_tags[num_captures + k];
            if !arg_slot_is_verbatim(cv, kind, tag) {
                return None;
            }
            slots.slots[num_captures + k] = std::mem::MaybeUninit::new((cv, tag));
        }
        slots.filled = total_args;
    }
    // The tier-up bookkeeping the static door owes its callee.
    //
    // nio21 wave 16 L2 (`CRATONVM_LAMBDA_DOOR_COMPILED`, default ON): a
    // compiled impl is entered FROM THIS DOOR, through the loop-integrated
    // `execute_jit_call_decoded` (the virtual door's primitive,
    // `virtual_door_call_compiled`), instead of declining to the general path.
    // The body is the site's own generation-memoised probe
    // (`lambda_jit_site_code`), so a hit costs no string-keyed
    // `JitCache::get`; before, every call paid that probe twice (here and in
    // `try_invoke_cached_lambda_impl`) plus the whole `try_lambda_dispatch`
    // round trip, and a handler-bearing impl then ran in a NESTED interpreter
    // because the one-shot arm barred its exception table (the w15 `rlock`
    // profile: `Nio20JitRows::rlockOp` interpreted per call, every
    // `LOCK.lock()` an interpreter->compiled transition). Off: the decline,
    // exactly as before.
    //
    // Wave 122 (lane L6): not on a virtual thread (see `on_virtual_thread`).
    if shared.config.jit_enabled && !on_virtual_thread {
        if lambda_door_compiled_enabled(shared) {
            if let Some(compiled) = lambda_door_compiled_body(shared, thread, &site) {
                site_stats::bump_if(shared.config.site_census, site_stats::DOOR_STATIC_HIT);
                // JIT round 23 wave 2, lane L2: the door commits to this call,
                // so the site's receiver profile names the proxy (as the
                // virtual door records a bytecode receiver).
                super::dispatch_virtual::lambda_record_receiver(
                    shared,
                    &thread.frames,
                    frame_idx,
                    site_pc,
                    recv_class,
                );
                return Some(lambda_door_call_compiled(
                    shared,
                    thread,
                    frame_idx,
                    Arc::clone(cached),
                    compiled,
                    recv_ptr,
                    num_captures,
                    num_args,
                ));
            }
        }
        if callee_has_compiled_body(shared, cached) {
            return None;
        }
        if !note_invocation_for_tierup(shared, thread, cached) {
            return None;
        }
    }
    site_stats::bump_if(shared.config.site_census, site_stats::DOOR_STATIC_HIT);
    // JIT round 23 wave 2, lane L2: committed (see the compiled arm above).
    super::dispatch_virtual::lambda_record_receiver(
        shared,
        &thread.frames,
        frame_idx,
        site_pc,
        recv_class,
    );
    // The receiver and the SAM arguments leave the caller's stack; the impl's
    // locals are the captures followed by those arguments.
    thread.frames[frame_idx].stack.discard_top(num_args + 1);
    let callee = DoorCallee::of(shared, &thread.frames, cached);
    Some(Ok(push_frame_stage1(shared, thread, callee, &slots, None)))
}

/// `CRATONVM_LAMBDA_DOOR_COMPILED` (nio21 wave 16 L2; default ON,
/// `0|false|off|no` is the kill switch): the interpreted lambda door enters a
/// compiled impl itself ([`lambda_door_call_compiled`]) instead of declining
/// the call to `try_lambda_dispatch`. Read once per VM
/// (`lambda::LambdaSwitches::door_compiled`, gci i14/sx; it was a process
/// `OnceLock` latch).
#[inline]
fn lambda_door_compiled_enabled(shared: &SharedVm) -> bool {
    shared.lambda_switches.door_compiled
}

/// The compiled impl body the lambda door may enter for `site`, or `None` to
/// keep the door's existing answer (count and interpret, or decline).
///
/// The refusals are `try_invoke_cached_lambda_impl`'s, the arm this replaces
/// for the door's shapes: the lambda tier-up switch and `--nojit`, a virtual
/// thread (compiled entry carries none of the interpreter's unmount points),
/// a `synchronized` impl (never a door site; asserted), and a handler-bearing
/// impl while `CRATONVM_LAMBDA_HANDLER_IMPL_ONESHOT=0`. The body is the site's
/// own memo, re-probed only when `JitCache::generation()` moves.
fn lambda_door_compiled_body(
    shared: &SharedVm,
    thread: &JvmThread,
    site: &super::lambda::LambdaJitSite,
) -> Option<cratonvm_jit::RetainedCode> {
    if matches!(thread.kind, crate::threading::ThreadKind::Virtual)
        || !crate::runtime::env_cache::jit_lambda_tierup()
        || crate::runtime::env_cache::disable_jit()
    {
        return None;
    }
    let cached = site.cached_impl();
    if cached.is_synchronized || !cached.is_static {
        return None;
    }
    if !cached.exception_table.is_empty()
        && !super::lambda::lambda_handler_impl_oneshot_enabled(shared)
    {
        return None;
    }
    super::lambda::lambda_jit_site_code(shared, site)
}

/// The lambda door's compiled call (nio21 wave 16 L2): the static impl's
/// arguments are the proxy's captures followed by the SAM arguments, which
/// leave the caller's operand stack together with the receiver; the impl's
/// return value is pushed to the caller by `execute_jit_call_decoded`, as for
/// any compiled callee the virtual door enters (`dispatch_virtual`'s
/// `virtual_door_call_compiled` is the template, outcome for outcome).
///
/// Only reached after the door's own checks passed: every replayed
/// `checkcast` passes, every operand and capture is in its verbatim shape and
/// the frame stack has room, so nothing below can decline before the commit.
/// A throw in the body is routed through the impl's own table and, when a
/// handler matches, its frame is pushed for the dispatch loop to run
/// (`FramePushed`); a decline (ABI, deopt with no resumable frame) re-runs the
/// impl interpreted from the same, possibly relocated, arguments.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn lambda_door_call_compiled(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cached: Arc<CachedBytecodeMethod>,
    compiled: cratonvm_jit::RetainedCode,
    recv_ptr: u64,
    num_captures: usize,
    num_args: usize,
) -> Result<CachedCallResult, MethodCallFailed> {
    // `total_args <= ARG_SLOTS_LEN` (the door's check), well inside the buffer.
    const MAX_INLINE_ARGS: usize = 16;
    const _: () = assert!(ARG_SLOTS_LEN <= MAX_INLINE_ARGS);
    let total_args = num_captures + num_args;
    let mut args_buf = [Value::Uninitialized; MAX_INLINE_ARGS];
    // SAFETY: `recv_ptr` was a registered object start under this same
    // safepoint-free span (nothing since has run Java or collected).
    let proxy = unsafe { ObjectRef::from_raw(recv_ptr as *mut u8) };
    for (i, slot) in args_buf.iter_mut().enumerate().take(num_captures) {
        *slot = shared.mem.heap.get_field(proxy, i);
    }
    {
        let facts = cached.descriptor_facts();
        let frame = &mut thread.frames[frame_idx];
        for k in (0..num_args).rev() {
            let tag = facts.param_tags[num_captures + k];
            args_buf[num_captures + k] = match frame.stack.pop_arg_for_descriptor_checked(tag) {
                Ok(v) => v,
                Err(e) => return Err(MethodCallFailed::from(e)),
            };
        }
        // The proxy receiver: the impl is static and never sees it.
        frame.stack.discard_top(1);
    }
    let args_slice = &mut args_buf[..total_args];
    refresh_stale_object_args(shared, args_slice);
    let ret = cached.return_tag();
    let heap = compiled.needs_heap();
    match execute_jit_call_decoded(
        shared,
        thread,
        frame_idx,
        &compiled,
        total_args as u16, // Cast: small param count (<= ARG_SLOTS_LEN)
        ret,
        heap,
        &cached,
        args_slice,
    ) {
        Ok(Some(ccr)) => return Ok(ccr),
        Ok(None) => {}
        Err(e) => return Err(e),
    }
    // DECLINED: nothing that must not be repeated ran (or the decline already
    // judged the re-run, `door_rerun_or_refuse`), and `args_buf` holds the
    // references at their current addresses (the write-back). The impl is
    // never `synchronized` (`lambda_door_compiled_body`), so no monitor.
    drop(compiled);
    thread.refill_pools_from_shared(
        &shared.mem.operand_stack_pool,
        &shared.mem.tag_pool,
        cached.max_locals as usize,
        (cached.max_stack as usize).max(16) + 8,
    );
    install_cached_frame(
        shared,
        thread,
        cached,
        &args_buf[..total_args],
        None,
        None,
        false,
    );
    Ok(CachedCallResult::FramePushed)
}

// -- non-virtual dispatch (0xb7, and 0xb6 on a non-virtual target) -------

/// Fast door for a call whose target is **not** virtually dispatched: every
/// cached `invokespecial`, and every `invokevirtual` whose target resolved to
/// a fixed method rather than a vtable slot.
///
/// That second case is not a corner: `javac` 25 emits `invokevirtual` for a
/// private instance method (JEP 181 nestmates), and the resolver caches such
/// a target as `CachedInvokeTarget::Bytecode` — no receiver class, no vtable
/// index. `execute_invokevirtual_fast_door` only accepts `VirtualBytecode`,
/// so before this door those calls reached the general dispatcher's
/// `Bytecode` arm and measured ~430 ns against a virtual call's ~245 ns.
///
/// See the module note for the contract.
///
/// **Tier-up follows the arm it replaces.** A `Bytecode` entry is counted and
/// probed for a compiled body exactly as the `Bytecode` arm of
/// `execute_invokevirtual_cached` does since `600311cbe` (2026-09-18): a
/// target redefined before the fill too since wave 39
/// ([`REDEFINED_TARGETS_TIER_UP`]), not under `--nojit` (and a virtual
/// thread never reaches a door); a callee with a compiled body is declined so
/// the general arm enters it. A `VirtualBytecode` entry is never counted: the
/// general arm's tier-up chain is guarded by `!is_special`. A body that
/// returns at pc 0 (`Object.<init>`) is the one exception, served uncounted
/// ([`body_returns_at_entry`]) and, since wave 20, without a frame
/// ([`EMPTY_BODY_ELISION`]). Once the general arm has found a compiled body
/// for a `Bytecode` entry it stores the site as a `Jit` entry
/// ([`NONVIRTUAL_ARM_UPGRADES_TO_JIT`]), which this door declines on its kind
/// without a `JitCache` probe. Until interpreter round i1 wave
/// 19 this door counted nothing, so a hot private method, super call or
/// constructor served here never reached the JIT through this route; the kill
/// switch is [`SPECIAL_DOOR_TIERS_UP`].
///
/// An `invokespecial` site caches as either target shape depending on which
/// resolver filled it, so both are accepted. For `VirtualBytecode` the
/// receiver's class is re-checked against the entry exactly as the general
/// path does: the target of an `invokespecial` does not depend on the
/// receiver's class, but the entry records one and a mismatch is the general
/// path's polymorphic route.
///
/// **A decline on the entry's kind hands the entry over** (interpreter round
/// i1 wave 22, lane L4; the proposal
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// `door_probe` is set to what the probe found (`Found`, or `Miss`) when the
/// door declines right at the probe, with nothing done since, and the
/// dispatch loop passes it to `execute_invokevirtual_cached_probed`, which
/// would otherwise hash the same key again. That is every call of a compiled
/// super call or constructor (`Jit`), of a registered native reached by
/// `invokespecial`, and every cold miss. The general dispatcher clones the
/// entry out of the cache anyway, so the clone taken here replaces that one
/// rather than adding to it. A decline after the probe (inside
/// [`nonvirtual_door_prelude`] / [`nonvirtual_door_finish`]) leaves `door_probe` untouched: `NotProbed`.
/// Kill switch: [`SPECIAL_DOOR_HANDS_OVER_ITS_PROBE`].
///
/// `fast_field` is the dispatch loop's hoisted `field_fast::fast_field_zgc`
/// answer, the heap the field-store constructor's frameless answer
/// ([`field_ctor_frameless`]) stores through (interpreter round i2 wave 61,
/// lane L7: it re-derived that answer, nine process memo and flag reads, on
/// every such constructor call).
#[inline]
#[allow(clippy::too_many_arguments)]
pub(super) fn execute_nonvirtual_fast_door(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    is_special: bool,
    site_pc: usize,
    fast_field: Option<&cratonvm_gc::zgc::ZgcRealHeap>,
    door_probe: &mut DoorProbe,
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    const MISS: usize = site_stats::DOOR_SPECIAL_MISS;
    // Hand what the probe found to the general dispatcher, then decline.
    macro_rules! hand_over {
        ($probe:expr) => {{
            if SPECIAL_DOOR_HANDS_OVER_ITS_PROBE {
                *door_probe = $probe;
            }
        }};
    }
    if doors_latched_off_by_redefinition() {
        decline!("special", MISS, "a class was redefined");
    }
    let caller_class_id = thread.frames[frame_idx].class_id;
    // The entry is BORROWED from the thread's invoke cache (wave 24): the
    // borrowed half of the door ([`nonvirtual_door_prelude`]) touches only
    // `thread.frames`, a disjoint field, so an empty body -- `Object.<init>`,
    // the end of every constructor chain -- is answered without an `Arc`
    // clone and drop (two atomic read-modify-writes per call). The clone is
    // taken only when a frame is going to be pushed, which moves it into the
    // frame (`push_frame_verbatim`).
    let (cached_ref, expected_receiver, target_redefined) =
        match thread
            .invoke_cache
            .get(caller_class_id, cp_index, is_special, site_pc as u32)
        {
            Some(CachedInvokeTarget::Bytecode { cached, gate }) => {
                if door_declines_redefined_target(gate.generation) {
                    decline!("special", MISS, "the target class has been redefined");
                }
                (
                    cached,
                    None,
                    tier_up_skips_redefined_target(gate.generation),
                )
            }
            Some(CachedInvokeTarget::VirtualBytecode {
                cached,
                gate,
                receiver_class_id,
            }) if is_special => {
                if door_declines_redefined_target(gate.generation) {
                    decline!("special", MISS, "the target class has been redefined");
                }
                (
                    cached,
                    Some(*receiver_class_id),
                    tier_up_skips_redefined_target(gate.generation),
                )
            }
            // SPLIT BY VARIANT. A single "not plain bytecode" reason covered 77% of
            // every door decline on a collator workload and named nothing: a
            // `Native` target is a door that can never serve it, a `VirtualBytecode`
            // on a NON-special call is a door that declines what the virtual door
            // should have taken, and a `Jit` target is a callee the general path has
            // to enter anyway. The three want completely different repairs.
            // Every arm below declines on the entry's kind, right at the
            // probe: each hands the entry over (see the doc comment).
            Some(other @ CachedInvokeTarget::Native { .. }) => {
                hand_over!(DoorProbe::Found(other.clone()));
                decline!("special", MISS, "cached target is a registered native")
            }
            Some(other @ CachedInvokeTarget::VirtualNative { .. }) => {
                hand_over!(DoorProbe::Found(other.clone()));
                decline!(
                    "special",
                    MISS,
                    "cached target is a virtual registered native"
                )
            }
            Some(other @ CachedInvokeTarget::VirtualBytecode { .. }) => {
                hand_over!(DoorProbe::Found(other.clone()));
                decline!(
                    "special",
                    MISS,
                    "cached target is VirtualBytecode on a non-special call"
                )
            }
            Some(other @ CachedInvokeTarget::Jit { .. }) => {
                hand_over!(DoorProbe::Found(other.clone()));
                decline!("special", MISS, "cached target is a compiled body")
            }
            Some(other) => {
                hand_over!(DoorProbe::Found(other.clone()));
                decline!("special", MISS, "cached target is not plain bytecode")
            }
            None => {
                hand_over!(DoorProbe::Miss);
                decline!("special", MISS, "inline cache miss")
            }
        };
    let mut slots = empty_arg_slots();
    let prelude = nonvirtual_door_prelude(
        shared,
        &mut thread.frames,
        frame_idx,
        caller_class_id,
        is_special,
        cached_ref,
        expected_receiver,
        &mut slots,
    )?;
    let NonvirtualPrelude::Frame { total_args } = prelude else {
        return Some(Ok(CachedCallResult::Handled));
    };
    // A field-store constructor answered without a frame (wave 28,
    // `FIELD_CTOR_ELISION`). Here rather than in the prelude because it needs
    // the thread's field-site cache (a field disjoint from the invoke cache
    // the entry is borrowed from). The screen is a few byte compares on the
    // callee's body; the rest is out of line.
    // t1 wave 6, lane G6: or the inner-class shape, stores before `super()`
    // (`CRATONVM_INTERP_FIELD_CTOR_PRE_SUPER_STORES`, default ON since
    // 2026-10-29; one per-VM bool load, and only for a body the first screen
    // refused).
    if FIELD_CTOR_ELISION
        && is_special
        && (field_ctor_prefilter(cached_ref)
            || (shared.config.interp_field_ctor_pre_super_stores
                && field_ctor_pre_super_prefilter(cached_ref)))
        && field_ctor_frameless(
            shared,
            &mut thread.frames,
            &mut thread.site_tables.fast_field_sites,
            frame_idx,
            cached_ref,
            &slots,
            total_args,
            fast_field,
        )
    {
        return Some(Ok(CachedCallResult::Handled));
    }
    // Wave 47 (lane L7): an alias of the retired slot's handle when it runs
    // the entry already (`DoorCallee`), else the clone this always took.
    let cached = DoorCallee::of(shared, &thread.frames, cached_ref);
    nonvirtual_door_finish(
        shared,
        thread,
        frame_idx,
        cached,
        &slots,
        total_args,
        expected_receiver,
        target_redefined,
    )
}

/// Kill switch for the non-virtual door's tier-up bookkeeping (interpreter
/// round i1 wave 19, lane L4). `true`: a `Bytecode` entry served by
/// [`nonvirtual_door_finish`] is counted and offered to the tiered manager, and
/// a callee with a compiled body is declined to the general arm that enters
/// it — what `execute_invokevirtual_cached`'s `Bytecode` arm does. `false`:
/// the historical door, which counted nothing and ran every callee
/// interpreted.
pub(super) const SPECIAL_DOOR_TIERS_UP: bool = true;

/// Kill switch for the non-virtual door's hand-over of its inline-cache probe
/// (interpreter round i1 wave 22, lane L4; see
/// [`execute_nonvirtual_fast_door`]). `true`: a decline on the entry's kind
/// passes the entry (or the miss) to the general dispatcher. `false`: the
/// general dispatcher probes the key again, as before wave 22.
pub(super) const SPECIAL_DOOR_HANDS_OVER_ITS_PROBE: bool = true;

/// Whether `cached`'s first instruction is `return` (0xb1): the method does
/// nothing but return (a `synchronized` one's monitor is still taken by
/// `door_monitor_acquire`). Exact for any body, padded or not — nothing runs
/// before pc 0, so nothing after it is reachable.
#[inline]
pub(super) fn body_returns_at_entry(cached: &CachedBytecodeMethod) -> bool {
    cached.code.first() == Some(&0xb1)
}

/// Kill switch for the `Bytecode` arm's upgrade of a non-virtual key to a
/// `Jit` entry (interpreter round i1 wave 20, lane L3). `true`: when
/// `execute_invokevirtual_cached`'s `Bytecode` arm finds a compiled body for
/// an instance target (an `invokespecial`, or a private `invokevirtual`), it
/// stores the site's entry as `CachedInvokeTarget::Jit`, as
/// `execute_invokestatic_cached` does for a static one; the non-virtual door
/// then declines on the entry's kind without probing `JitCache`, and the
/// general dispatcher enters the entry's body through the same `Bytecode` arm
/// without probing it either. `false`: the wave-19 shape, where both probed
/// `JitCache` on every call of a compiled callee.
pub(super) const NONVIRTUAL_ARM_UPGRADES_TO_JIT: bool = true;

/// Kill switch for the non-virtual door's frameless call of an empty body
/// (interpreter round i1 wave 20, lane L3; the proposal
/// `docs/feature-designs/jdk-only-mode.md`).
/// `true`: a callee whose first instruction is `return`
/// ([`body_returns_at_entry`]; `Object.<init>` ends every constructor chain)
/// is answered by popping its arguments, with no frame pushed, whenever
/// [`empty_body_elidable`] allows it. `false`: the frame is pushed and popped
/// as for any other callee.
pub(super) const EMPTY_BODY_ELISION: bool = true;

/// May the door answer a call of an empty body (one that
/// [`body_returns_at_entry`]) without a frame? The body can neither throw,
/// allocate, nor reach a safepoint, so the frame is observable only through
/// an event about it or a stop in it:
///
/// * not `synchronized` — the monitor enter and exit are the one thing such
///   a call does, and the enter can block;
/// * no JVMTI listener that needs the interpreter in this VM (MethodEntry,
///   MethodExit, FramePop, SingleStep, field watches, Exception) and no JDWP
///   request that concerns the callee (a breakpoint in it, a step request, a
///   suspension) — the same question the compiled-code doors ask before they
///   run a body in place of its frame (`jvmti_requires_interpreter_for`).
///
/// Unarmed, that is one `Acquire` load and, with the JDWP surface compiled
/// in, one relaxed load. Finalizable instances are registered at allocation
/// in this VM (`gc_and_alloc::init_new_instance`), not when `Object.<init>`
/// returns, so skipping the frame changes nothing there.
#[inline]
pub(super) fn empty_body_elidable(shared: &SharedVm, cached: &CachedBytecodeMethod) -> bool {
    EMPTY_BODY_ELISION
        && !cached.is_synchronized
        && !super::jit_bridge::jvmti_requires_interpreter_for(shared, cached)
}

/// Door calls answered without a frame, and those that kept their frame
/// (elision off, a `synchronized` body, an observer), indexed by the
/// `FRAMELESS_*` constants below: an empty body ([`EMPTY_BODY_ELISION`]) and,
/// since wave 25, a trivial `Object`-subclass constructor
/// ([`TRIVIAL_CTOR_ELISION`]). Counted only under `CRATONVM_DBG_FIELD_SITE=1`,
/// printed with the decline tally. One array, not a static per counter: the
/// per-VM statics ratchet (`vm/tests/per_vm_state_statics_ratchet.rs`) counts
/// declarations, and these are diagnostic tallies of the process.
static FRAMELESS_CENSUS: [std::sync::atomic::AtomicU64; 7] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];
/// Of the elided trivial and field-store constructors, those whose first call
/// was a super CHAIN (a non-`Object` direct superclass;
/// `CRATONVM_INTERP_TRIVIAL_CTOR_SUPER_CHAIN`, interpreter round i4 wave 94).
const FRAMELESS_SUPER_CHAIN_ELIDED: usize = 6;
const FRAMELESS_EMPTY_ELIDED: usize = 0;
const FRAMELESS_EMPTY_FRAMED: usize = 1;
const FRAMELESS_CTOR_ELIDED: usize = 2;
const FRAMELESS_CTOR_FRAMED: usize = 3;
/// [`FIELD_CTOR_ELISION`] (wave 28): answered without a frame, and matched
/// the shape but kept the frame (a site not filled yet, an observer, a
/// value the field does not take, ...).
const FRAMELESS_FIELD_CTOR_ELIDED: usize = 4;
const FRAMELESS_FIELD_CTOR_FRAMED: usize = 5;

/// Kill switch for the virtual door's frameless call of an empty body
/// (interpreter round i1 wave 22, lane L4; the proposal
/// `docs/feature-designs/jdk-only-mode.md`,
/// "does the virtual door deserve it too"). `true`: a monomorphic (or
/// poly-served) `invokevirtual` / `invokeinterface` whose callee
/// [`body_returns_at_entry`] is answered by popping its arguments whenever
/// [`empty_body_elidable`] allows it (`dispatch_virtual.rs`,
/// `execute_invokevirtual_fast_door`). `false`: the frame is pushed.
pub(super) const VIRTUAL_DOOR_ELIDES_EMPTY_BODY: bool = true;

/// Kill switch for the static door's frameless call of an empty static body
/// (interpreter round i1 wave 22, lane L4; stage 3 of the same proposal).
/// The door only ever holds an entry the general path filled, which it does
/// after the declaring class's initialization barrier, so eliding the frame
/// changes no initialization order: the pushed frame would have run nothing
/// either.
pub(super) const STATIC_DOOR_ELIDES_EMPTY_BODY: bool = true;

/// Census for [`EMPTY_BODY_ELISION`] (and the virtual and static doors'
/// twins): one relaxed increment when `CRATONVM_DBG_FIELD_SITE` is armed,
/// one cached bool otherwise. `census` is `VmConfig::site_census` (wave 60,
/// lane L7: the caller's config load, where this read the process gate).
#[inline]
pub(super) fn note_empty_body_call(census: bool, elided: bool) {
    if !census {
        return;
    }
    let counter = if elided {
        &FRAMELESS_CENSUS[FRAMELESS_EMPTY_ELIDED]
    } else {
        &FRAMELESS_CENSUS[FRAMELESS_EMPTY_FRAMED]
    };
    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// [`note_empty_body_call`] for [`TRIVIAL_CTOR_ELISION`]: a trivial
/// constructor answered without a frame, or one that kept it (an observer, or
/// a body the verdict refused).
#[inline]
fn note_trivial_ctor_call(census: bool, elided: bool) {
    if !census {
        return;
    }
    let counter = if elided {
        &FRAMELESS_CENSUS[FRAMELESS_CTOR_ELIDED]
    } else {
        &FRAMELESS_CENSUS[FRAMELESS_CTOR_FRAMED]
    };
    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Count an elided constructor whose first call was a super chain
/// ([`FRAMELESS_SUPER_CHAIN_ELIDED`]); `chain_frames` is
/// [`CtorVerdict::chain_frames`].
#[inline]
fn note_super_chain_elided(census: bool, chain_frames: u8) {
    if census && chain_frames > 1 {
        FRAMELESS_CENSUS[FRAMELESS_SUPER_CHAIN_ELIDED]
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Kill switch for the non-virtual door's frameless call of a trivial
/// constructor (interpreter round i1 wave 25, lane L4; the proposal
/// `docs/feature-designs/jdk-only-mode.md`).
/// `true`: an `invokespecial` of a `<init>()V` whose body is exactly javac's
/// default constructor of a direct `java/lang/Object` subclass
/// (`aload_0; invokespecial java/lang/Object.<init>()V; return`) is answered
/// by popping the receiver, with no frame, whenever [`trivial_ctor_elidable`]
/// allows it -- the call it makes is the one [`EMPTY_BODY_ELISION`] already
/// answers without a frame. `false`: the constructor's frame is pushed.
pub(super) const TRIVIAL_CTOR_ELISION: bool = true;

/// Whether `cached` has the SHAPE of a trivial `Object`-subclass constructor:
/// `<init>()V` whose code is the five bytes `2a b7 hi lo b1`. The one fact the
/// bytes cannot show -- that `hi lo` names `java/lang/Object.<init>()V` in the
/// declaring class's constant pool -- is [`trivial_object_ctor`]'s. The same
/// byte test as `jit_bridge::is_elidable_construction`, the JIT's twin.
///
/// On the method's REAL length (`jit_bridge::cached_method_code_len`): every
/// invoke-cache entry carries its body through
/// `frame::padded_bytecode_for_method`, two speculative-read zero bytes longer
/// than the class file's. Until interpreter round i1 wave 28 (lane L7) this
/// compared the padded length with 5, the JIT twin's number for the unpadded
/// class-file body, so no constructor the real build cached ever had the
/// shape and the wave-25 elision never engaged outside its unit tests (which
/// built unpadded entries).
#[inline]
fn body_is_trivial_ctor_shape(cached: &CachedBytecodeMethod) -> bool {
    let code = &cached.code;
    super::jit_bridge::cached_method_code_len(cached) == 5
        && code[0] == 0x2a
        && code[1] == 0xb7
        && code[4] == 0xb1
        && !cached.is_synchronized
        && cached.method_name.as_ref() == "<init>"
        && cached.method_descriptor.as_ref() == "()V"
}

/// Sets in the per-thread [`TRIVIAL_CTOR_MEMO`], two ways each. Since wave
/// 28 the memo answers every constructor the field-store screen passes as well
/// as the default ones, and a miss is a `class_manager` read and a
/// constant-pool walk, so it must stay rare: 32 entries, and two ways so that
/// two hot constructors whose bodies map to one set do not evict each other
/// on every call (a direct-mapped table does exactly that).
const TRIVIAL_CTOR_MEMO_SETS: usize = 16;

/// Everything the door asks about one constructor that does not change while
/// the method does, decided once per method and memoized
/// ([`TRIVIAL_CTOR_MEMO`]).
#[derive(Clone, Copy)]
struct CtorVerdict {
    /// [`trivial_object_ctor_uncached`]: the body's first instruction calls a
    /// plain `Object.<init>` of a direct `Object` subclass (or, under
    /// `CRATONVM_INTERP_TRIVIAL_CTOR_SUPER_CHAIN`, a chain of trivial
    /// superclass constructors ending there).
    object_init_first: bool,
    /// How many frames that first call pushes when it is framed: 1 for
    /// `Object.<init>`, `k + 1` for a chain of `k` trivial superclass
    /// constructors (wave 94). The frameless answer needs room for them all
    /// (`!at_frame_limit(chain_frames)`), so a call that would overflow keeps
    /// its frames and overflows where it did. 0 when `object_init_first` is
    /// false.
    chain_frames: u8,
    /// The [`FIELD_CTOR_ELISION`] stores (`field_ctor_plan`), in
    /// `plan[..plan_len]`; `plan_len == 0` means the constructor is not a
    /// field-store constructor this door answers (not `object_init_first`,
    /// another body shape, or a `long` / `double` parameter).
    plan_len: u8,
    plan: [(u16, CtorValue); super::field_fast::CTOR_FIELD_STORES_MAX],
}

/// One [`TRIVIAL_CTOR_MEMO`] entry: `(vm identity, the method's padded body,
/// its declaring class, class redefinition count, verdict)`.
type TrivialCtorMemoSlot = Option<(usize, Arc<[u8]>, ClassId, u64, CtorVerdict)>;

/// An empty [`TRIVIAL_CTOR_MEMO`] set (a `const` item, so the array of sets
/// can be written as a repeat of it).
const TRIVIAL_CTOR_MEMO_EMPTY_SET: [TrivialCtorMemoSlot; 2] = [None, None];

thread_local! {
    /// [`trivial_object_ctor`]'s verdicts, two-way set-associative on the
    /// address of the method's padded body.
    ///
    /// A cache of facts about ONE method, keyed the way the per-VM statics
    /// ratchet asks (`vm/tests/per_vm_state_statics_ratchet.rs`): by the VM's
    /// never-reused `vm_identity`, not its address. Keyed by `cached.code`
    /// since interpreter round i1 wave 28 (lane L7), not by the entry: every
    /// call site's inline-cache entry is its own `Arc<CachedBytecodeMethod>`,
    /// so a loop with more constructor SITES than slots re-ran the verdict (a
    /// `class_manager` read) on every call, while the padded body is one `Arc`
    /// per method (`frame::padded_bytecode_for_method`, which never lets two
    /// methods share one). The body is held by an `Arc` clone, so `Arc::ptr_eq`
    /// means identity (its address cannot be recycled while the memo names
    /// it, the argument `DOOR_RECV_MEMO` in `dispatch_virtual.rs` makes); the
    /// declaring class is compared as well, because the verdict reads that
    /// class. The process redefinition count (`class_redefinition_count`)
    /// expires every verdict at once when any class is redefined: a
    /// redefinition replaces a constant pool under an unchanged `ClassId`, and
    /// the verdict reads one. Filled only when the door meets a constructor of
    /// the trivial shape.
    static TRIVIAL_CTOR_MEMO: std::cell::RefCell<
        [[TrivialCtorMemoSlot; 2]; TRIVIAL_CTOR_MEMO_SETS],
    > = const {
        std::cell::RefCell::new([TRIVIAL_CTOR_MEMO_EMPTY_SET; TRIVIAL_CTOR_MEMO_SETS])
    };
}

/// Is `cached` -- which has [`body_is_trivial_ctor_shape`] (or, since wave 28,
/// [`field_ctor_plan`]'s shape) -- a constructor of a direct
/// `java/lang/Object` subclass whose first instruction calls `Object.<init>`,
/// served as plain bytecode that returns at entry? For the five-byte shape
/// that is javac's default constructor. Memoized per thread
/// ([`TRIVIAL_CTOR_MEMO`]); the uncached answer is
/// [`trivial_object_ctor_uncached`].
#[inline]
fn trivial_object_ctor(shared: &SharedVm, cached: &Arc<CachedBytecodeMethod>) -> bool {
    ctor_verdict(shared, cached).object_init_first
}

/// The memoized [`CtorVerdict`] of `cached` (interpreter round i1 wave 28,
/// lane L7b: the field-store plan joined the memo, so a constructor the
/// field-store door answers is not re-parsed per call, and one it refuses
/// -- another parent, a `long` / `double` parameter, another body shape --
/// costs one memo probe).
#[inline]
fn ctor_verdict(shared: &SharedVm, cached: &Arc<CachedBytecodeMethod>) -> CtorVerdict {
    let vm_key = shared.vm_identity;
    let redefinitions = crate::classloading::class_redefinition_count();
    // `CRATONVM_INTERP_CTOR_VERDICT_ON_ENTRY` (interpreter round i6 wave 119,
    // lane L7, default off): the verdict the entry itself carries, stamped as
    // the per-thread memo stamps its rows; a stale or foreign stamp falls
    // through to that memo. Off: one per-VM bool load.
    let on_entry = shared.config.interp_ctor_verdict_on_entry;
    let cell = if on_entry {
        cached
            .ctor_verdict_memo
            .get()
            .and_then(|cell| cell.downcast_ref::<EntryCtorVerdictCell>())
    } else {
        None
    };
    if let Some(cell) = cell {
        let memo = cell.load();
        if memo.vm == vm_key && memo.redefinitions == redefinitions {
            return memo.verdict;
        }
    }
    let verdict = ctor_verdict_per_thread(shared, cached, vm_key, redefinitions);
    if on_entry {
        let fresh = EntryCtorVerdict {
            vm: vm_key,
            redefinitions,
            verdict,
        };
        // Wave 120 (L9's review): a cell left stale by a redefinition is
        // refilled, not kept stale for the life of the VM; the first fill
        // installs the cell (a racing installer's verdict is dropped).
        match cell {
            Some(cell) => cell.refill(fresh),
            None => {
                let _ = cached
                    .ctor_verdict_memo
                    .set(Box::new(EntryCtorVerdictCell::new(fresh)));
            }
        }
    }
    verdict
}

/// One verdict of [`EntryCtorVerdictCell`]: the verdict and the stamp the
/// per-thread [`TRIVIAL_CTOR_MEMO`] would have filed it under (the VM's
/// never-reused identity, the process redefinition count). The entry is one
/// method's (its body `Arc` and declaring class are fixed), so those two are
/// the whole key. Interpreter round i6 wave 119, lane L7.
struct EntryCtorVerdict {
    vm: usize,
    redefinitions: u64,
    verdict: CtorVerdict,
}

/// What [`CachedBytecodeMethod::ctor_verdict_memo`] holds under
/// `CRATONVM_INTERP_CTOR_VERDICT_ON_ENTRY` (interpreter round i6 waves
/// 119-120, lane L7): the current [`EntryCtorVerdict`], replaceable. Wave 119
/// stored the verdict itself in the entry's `OnceLock`, so once a
/// redefinition anywhere moved the process count past its stamp the entry
/// answered nothing for the rest of the VM's life (L9's review); the
/// `OnceLock` now installs this cell once and the cell is refilled.
///
/// Readers load one pointer (`Acquire`) and read the verdict it names, with
/// no read-modify-write; a refill swaps in a fresh box (`AcqRel`). The
/// replaced box is not freed while a reader might still hold it: it is kept
/// in `retired` until the cell itself drops (the entry's last `Arc`), so the
/// `&EntryCtorVerdict` a reader got from [`Self::load`] stays valid for as
/// long as it can borrow the cell. A stale stamp is a miss and a racing
/// refill with an older stamp only causes another refill: answers are
/// compared against the stamp, never trusted blindly. Refills stop at
/// [`Self::MAX_REFILLS`] (the per-thread memo answers after that), which
/// bounds what `retired` can hold.
struct EntryCtorVerdictCell {
    current: std::sync::atomic::AtomicPtr<EntryCtorVerdict>,
    retired: std::sync::Mutex<Vec<Box<EntryCtorVerdict>>>,
}

impl EntryCtorVerdictCell {
    /// Refills kept before the cell stops refilling.
    const MAX_REFILLS: usize = 64;

    fn new(first: EntryCtorVerdict) -> Self {
        Self {
            current: std::sync::atomic::AtomicPtr::new(Box::into_raw(Box::new(first))),
            retired: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The current verdict.
    #[inline]
    fn load(&self) -> &EntryCtorVerdict {
        let current = self.current.load(std::sync::atomic::Ordering::Acquire);
        // SAFETY: `current` is never null (set from `Box::into_raw` in `new`
        // and `refill`), and the box it names is freed only in `Drop`, which
        // needs `&mut self`: every box `current` ever named is either still
        // current or held in `retired`, both of which live as long as `self`.
        unsafe { &*current }
    }

    /// Replace the current verdict with `fresh`, keeping the old box alive.
    fn refill(&self, fresh: EntryCtorVerdict) {
        let mut retired = match self.retired.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if retired.len() >= Self::MAX_REFILLS {
            return;
        }
        let old = self.current.swap(
            Box::into_raw(Box::new(fresh)),
            std::sync::atomic::Ordering::AcqRel,
        );
        // SAFETY: `old` came from `Box::into_raw` (in `new` or an earlier
        // `refill`) and was just unpublished by the swap; it is re-owned once
        // and kept in `retired`, so a reader that loaded it before the swap
        // still reads a live box.
        retired.push(unsafe { Box::from_raw(old) });
    }
}

impl Drop for EntryCtorVerdictCell {
    fn drop(&mut self) {
        let current = *self.current.get_mut();
        // SAFETY: `current` came from `Box::into_raw` and, with `&mut self`,
        // no reader can hold it; the retired boxes drop with `retired`.
        drop(unsafe { Box::from_raw(current) });
    }
}

/// [`ctor_verdict`] through the per-thread memo ([`TRIVIAL_CTOR_MEMO`]), as
/// before wave 119.
#[inline]
fn ctor_verdict_per_thread(
    shared: &SharedVm,
    cached: &Arc<CachedBytecodeMethod>,
    vm_key: usize,
    redefinitions: u64,
) -> CtorVerdict {
    let declaring = cached.declaring_class_id;
    // Cast: a body address used only to pick a memo set; identity is settled
    // by `Arc::ptr_eq` below, never by this value.
    let addr = Arc::as_ptr(&cached.code) as *const u8 as usize;
    let set = ((addr >> 4) ^ (addr >> 12)) & (TRIVIAL_CTOR_MEMO_SETS - 1);
    let answer = |entry: &TrivialCtorMemoSlot| match entry {
        Some((vm, body, class, seen, verdict))
            if *vm == vm_key
                && *seen == redefinitions
                && *class == declaring
                && Arc::ptr_eq(body, &cached.code) =>
        {
            Some(*verdict)
        }
        _ => None,
    };
    let memo = TRIVIAL_CTOR_MEMO.with(|memo| {
        let memo = memo.borrow();
        let ways = &memo[set];
        answer(&ways[0]).or_else(|| answer(&ways[1]))
    });
    if let Some(verdict) = memo {
        return verdict;
    }
    let verdict = ctor_verdict_uncached(shared, cached);
    // The newest verdict in way 0, the previous way-0 entry demoted to way 1.
    TRIVIAL_CTOR_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        let ways = &mut memo[set];
        ways[1] = ways[0].take();
        ways[0] = Some((
            vm_key,
            Arc::clone(&cached.code),
            declaring,
            redefinitions,
            verdict,
        ));
    });
    verdict
}

/// The [`CtorVerdict`] of `cached`, unmemoized: the class-manager verdict,
/// then (only for a body that passes it and [`field_ctor_prefilter`]) the
/// field-store plan and the descriptor's parameter categories.
#[cold]
#[inline(never)]
fn ctor_verdict_uncached(shared: &SharedVm, cached: &CachedBytecodeMethod) -> CtorVerdict {
    let chain_frames = trivial_object_ctor_uncached(shared, cached).unwrap_or(0);
    let mut verdict = CtorVerdict {
        object_init_first: chain_frames > 0,
        chain_frames,
        plan_len: 0,
        plan: [(0u16, CtorValue::Null); super::field_fast::CTOR_FIELD_STORES_MAX],
    };
    // `CRATONVM_INTERP_FIELD_CTOR_WIDE_VALUES` (interpreter round i6 wave 115,
    // lane L7, default off): a `long` / `double` parameter or constant is
    // admitted, and the plan's locals are mapped to argument indices by the
    // descriptor. Off: such a constructor keeps its frame, as before.
    let wide = shared.config.interp_field_ctor_wide_values;
    // `CRATONVM_INTERP_FIELD_CTOR_LDC_VALUES` (interpreter round i6 wave 118,
    // lane L7, default off): a numeric `ldc` / `ldc_w` / `ldc2_w` value is
    // admitted, read from the declaring class's pool once, here. Off: such a
    // constructor keeps its frame, as before.
    let ldc = shared.config.interp_field_ctor_ldc_values;
    // t1 wave 6, lane G6 (`CRATONVM_INTERP_FIELD_CTOR_PRE_SUPER_STORES`,
    // default ON since 2026-10-29): an inner / local class's constructor, stores before its
    // `super()` call. Its first instruction is not the call, so the verdict
    // above is `None` for it (`object_init_first` false: no other door reads
    // it as trivial); here the call is read where the parse found it.
    if !verdict.object_init_first
        && shared.config.interp_field_ctor_pre_super_stores
        && field_ctor_pre_super_prefilter(cached)
        && (wide || !descriptor_has_category2_param(&cached.method_descriptor))
    {
        let code_len = super::jit_bridge::cached_method_code_len(cached);
        if let Some((mut plan, n, call_pc)) = cached
            .code
            .get(..code_len)
            .and_then(|code| field_ctor_pre_super_plan(code, wide, ldc))
        {
            let chain_frames =
                trivial_object_ctor_uncached_at(shared, cached, call_pc).unwrap_or(0);
            if let (true, Ok(n8)) = (chain_frames > 0, u8::try_from(n)) {
                // The pool pass runs without `ldc` too: it also checks the
                // pre-`super()` null checks ([`CtorValue::NullChecked`]), and
                // answers `true` at once for a plan with neither.
                if (!wide || ctor_plan_locals_to_args(&mut plan[..n], &cached.method_descriptor))
                    && ctor_plan_constants_from_pool(shared, cached, &mut plan[..n])
                {
                    verdict.object_init_first = true;
                    verdict.chain_frames = chain_frames;
                    verdict.plan = plan;
                    verdict.plan_len = n8;
                }
            }
        }
        return verdict;
    }
    if verdict.object_init_first
        && field_ctor_prefilter(cached)
        && (wide || !descriptor_has_category2_param(&cached.method_descriptor))
    {
        let code_len = super::jit_bridge::cached_method_code_len(cached);
        if let Some((mut plan, n)) = cached.code.get(..code_len).and_then(|code| {
            if wide || ldc {
                field_ctor_plan_with(code, wide, ldc)
            } else {
                field_ctor_plan(code)
            }
        }) {
            if let Ok(n8) = u8::try_from(n) {
                if (!wide || ctor_plan_locals_to_args(&mut plan[..n], &cached.method_descriptor))
                    && (!ldc || ctor_plan_constants_from_pool(shared, cached, &mut plan[..n]))
                {
                    verdict.plan = plan;
                    verdict.plan_len = n8;
                }
            }
        }
    }
    verdict
}

/// Replace a [`field_ctor_plan_with`] plan's [`CtorValue::Ldc`] /
/// [`CtorValue::Ldc2`] entries by the numeric constants the declaring class's
/// pool holds there (interpreter round i6 wave 118, lane L7;
/// `CRATONVM_INTERP_FIELD_CTOR_LDC_VALUES`): `ldc` of an `Integer` / `Float`,
/// `ldc2_w` of a `Long` / `Double`, exactly the values the interpreter's `ldc`
/// pushes (a numeric constant needs no resolution, JVMS 5.4.3). `false` (the
/// plan is unusable, the constructor keeps its frame) for any other entry, or
/// when the class is gone. The verdict that called this is memoized per
/// thread under the process redefinition count, so a redefined pool is read
/// again.
///
/// Also (t1 i147 close-out) a [`CtorValue::NullChecked`] entry becomes
/// [`CtorValue::LocalNonNull`] when [`is_jdk_require_non_null`] holds for
/// its `invokestatic`; `false` otherwise.
fn ctor_plan_constants_from_pool(
    shared: &SharedVm,
    cached: &CachedBytecodeMethod,
    plan: &mut [(u16, CtorValue)],
) -> bool {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    if !plan.iter().any(|(_, v)| {
        matches!(
            v,
            CtorValue::Ldc(_) | CtorValue::Ldc2(_) | CtorValue::NullChecked(..)
        )
    }) {
        return true;
    }
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(cached.declaring_class_id) else {
        return false;
    };
    let pool = &class.constant_pool;
    for (_, value) in plan.iter_mut() {
        *value = match *value {
            CtorValue::NullChecked(arg, check) => {
                if !is_jdk_require_non_null(&cm, class, check) {
                    return false;
                }
                CtorValue::LocalNonNull(arg)
            }
            CtorValue::Ldc(index) => match pool.get(index) {
                Some(ConstantPoolEntry::Integer(v)) => CtorValue::Int(*v),
                Some(ConstantPoolEntry::Float(f)) => CtorValue::Float(*f),
                _ => return false,
            },
            CtorValue::Ldc2(index) => match pool.get(index) {
                Some(ConstantPoolEntry::Long(v)) => CtorValue::Long(v.get()),
                Some(ConstantPoolEntry::Double(d)) => CtorValue::Double(d.get()),
                _ => return false,
            },
            other => other,
        };
    }
    true
}

/// Whether `class`'s pool entry `check` is a `Methodref` to
/// `java/util/Objects.requireNonNull(Ljava/lang/Object;)Ljava/lang/Object;`
/// whose loaded body, for a non-null argument, only returns it: the class has
/// real bytes, the method is static, not native, not synchronized, its
/// first instruction is `aload_0; ifnonnull` and the branch lands on the last
/// two bytes, `aload_0; areturn` (JDK 25's `if (obj == null) throw ...;
/// return obj;`). Then skipping the call for a non-null argument is the
/// call's effect. A retransformed body fails the test (and a redefinition
/// re-runs it: the verdict memo is stamped with the redefinition count).
fn is_jdk_require_non_null(
    cm: &cratonvm_classloading::ClassManager,
    class: &cratonvm_classloading::Class,
    check: u16,
) -> bool {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    let pool = &class.constant_pool;
    let Some(ConstantPoolEntry::MethodReference {
        class_index,
        name_and_type_index,
    }) = pool.get(check)
    else {
        return false;
    };
    if pool.get_class_name(*class_index) != Some("java/util/Objects")
        || pool.get_name_and_type(*name_and_type_index)
            != Some(("requireNonNull", "(Ljava/lang/Object;)Ljava/lang/Object;"))
    {
        return false;
    }
    let Some(objects) = cm
        .find_class_by_name_for_class("java/util/Objects", class.id)
        .and_then(|id| cm.get_class(id))
    else {
        return false;
    };
    let Some(method) =
        objects.find_method("requireNonNull", "(Ljava/lang/Object;)Ljava/lang/Object;")
    else {
        return false;
    };
    if !objects.origin.has_real_bytes()
        || !method.is_static()
        || method.is_native()
        || method.is_synchronized()
    {
        return false;
    }
    let Some(code) = method.code().map(|c| &c.code[..]) else {
        return false;
    };
    match code {
        [0x2a, 0xc7, hi, lo, ..] => {
            // A backward (negative) offset reads as a large one and fails.
            let target = 1 + usize::from(u16::from_be_bytes([*hi, *lo]));
            target + 2 == code.len() && code[target..] == [0x2a, 0xb0]
        }
        _ => false,
    }
}

/// For each local slot of a constructor's parameters (the receiver in slot
/// 0), the argument index whose value starts there and whether that argument
/// is a `long` / `double`; `None` for a slot no argument starts at (the second
/// half of a category-2 parameter, or past the parameters). `None` for a
/// malformed descriptor, or one with more parameters than an argument index
/// can name. Interpreter round i6 wave 115, lane L7.
fn ctor_param_slots(descriptor: &str) -> Option<[Option<(u8, bool)>; 256]> {
    let bytes = descriptor.as_bytes();
    if bytes.first() != Some(&b'(') {
        return None;
    }
    let mut slots = [None; 256];
    slots[0] = Some((0u8, false));
    let mut slot = 1usize;
    let mut arg = 1usize;
    let mut i = 1usize;
    loop {
        let tag = *bytes.get(i)?;
        if tag == b')' {
            return Some(slots);
        }
        let wide = matches!(tag, b'J' | b'D');
        let mut end = i;
        while *bytes.get(end)? == b'[' {
            end += 1;
        }
        if *bytes.get(end)? == b'L' {
            end += bytes.get(end..)?.iter().position(|&b| b == b';')?;
        }
        let index = u8::try_from(arg).ok()?;
        if let Some(entry) = slots.get_mut(slot) {
            *entry = Some((index, wide));
        }
        slot += if wide { 2 } else { 1 };
        arg += 1;
        i = end + 1;
    }
}

/// Map a [`field_ctor_plan_with`] plan's local slots to argument indices of
/// `descriptor` ([`ctor_param_slots`]): each [`CtorValue::Local`] must name a
/// slot a category-1 argument starts at, each [`CtorValue::LocalWide`] one a
/// `long` / `double` argument starts at, and both become `Local(argument)`.
/// `false` (the plan is unusable, the constructor keeps its frame) for any
/// other slot, the receiver's included. Interpreter round i6 wave 115, lane L7.
fn ctor_plan_locals_to_args(plan: &mut [(u16, CtorValue)], descriptor: &str) -> bool {
    let Some(slots) = ctor_param_slots(descriptor) else {
        return false;
    };
    for (_, value) in plan.iter_mut() {
        let (slot, want_wide) = match *value {
            CtorValue::Local(slot) | CtorValue::NullChecked(slot, _) => (slot, false),
            CtorValue::LocalWide(slot) => (slot, true),
            _ => continue,
        };
        match slots[usize::from(slot)] {
            Some((arg, wide)) if arg != 0 && wide == want_wide => {
                *value = match *value {
                    CtorValue::NullChecked(_, check) => CtorValue::NullChecked(arg, check),
                    _ => CtorValue::Local(arg),
                }
            }
            _ => return false,
        }
    }
    true
}

/// Whether a method descriptor has a `long` or `double` parameter. Such a
/// parameter takes one argument slot but two local slots, so the field-store
/// door, which reads local `n` as argument `n`, refuses the constructor.
fn descriptor_has_category2_param(descriptor: &str) -> bool {
    let bytes = descriptor.as_bytes();
    let mut i = match bytes.first() {
        Some(b'(') => 1,
        _ => return true,
    };
    while let Some(&b) = bytes.get(i) {
        match b {
            b')' => return false,
            b'J' | b'D' => return true,
            b'[' => {
                while bytes.get(i) == Some(&b'[') {
                    i += 1;
                }
                if bytes.get(i) == Some(&b'L') {
                    while bytes.get(i).is_some_and(|&c| c != b';') {
                        i += 1;
                    }
                }
                i += 1;
            }
            b'L' => {
                while bytes.get(i).is_some_and(|&c| c != b';') {
                    i += 1;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    // No `)`: not a method descriptor; refuse.
    true
}

/// The verdict of [`trivial_object_ctor`], read from the class manager.
///
/// Since interpreter round i1 wave 28 (lane L7) the verdict is about the
/// constructor's FIRST instruction, whatever follows it, so the field-store
/// constructors of [`FIELD_CTOR_ELISION`] share it: `aload_0; invokespecial
/// hi lo` at pc 0, on a constructor of any descriptor. For javac's default
/// constructor (`()V`, five bytes) that is exactly the wave-25 verdict.
///
/// * The entry's code is the declaring class's CURRENT code for this
///   `<init>` (an entry that outlived a redefinition names the old bytes,
///   whose `hi lo` indexes the old pool).
/// * Constant-pool entry `hi lo` of the declaring class is a `Methodref` to
///   `java/lang/Object.<init>()V` -- what `jit_bridge::is_elidable_construction`
///   checks for the JIT.
/// * The declaring class's superclass is `java/lang/Object`, whose
///   `<init>()V` is unsynchronized and returns at pc 0 (so the nested call is
///   the one [`EMPTY_BODY_ELISION`] answers without a frame).
/// * No registered native would run for `java/lang/Object.<init>()V` instead
///   of that bytecode: the resolver the JIT asks
///   (`jit_bridge::elidable_ctor_native_would_run`), so the two tiers cannot
///   disagree about what the nested call runs.
///
/// The constructor itself was already cached as plain bytecode (its entry is
/// `Bytecode` / `VirtualBytecode`, and `callee_is_plain_bytecode` passed), so
/// no native stands in front of it. Finalizable instances are registered at
/// allocation in this VM (`gc_and_alloc::init_new_instance`), so skipping the
/// frame loses no registration. It does lose the constructor's `return`,
/// which is HotSpot's registration point as this VM models it
/// (`exception_dispatch::finalizer_init_returned`: re-register an object a
/// failed earlier `<init>` withdrew, mark its row constructed); so a
/// declaring class that has a finalizer keeps its frame
/// ([`ctor_elision_keeps_finalizable`], t1 wave 8, lane I24).
///
/// # The super chain (interpreter round i4 wave 94, lane L7)
///
/// Under `CRATONVM_INTERP_TRIVIAL_CTOR_SUPER_CHAIN` (`VmConfig::interp_trivial_ctor_super_chain`,
/// default off; the "recursive rule over a super chain" of
/// `i24-L4-proposal-frameless-trivial-constructor-in-the-special-door`) the
/// first call may instead name the DIRECT superclass `S`'s `<init>()V`, when
/// that constructor is itself javac's default one (`aload_0; invokespecial
/// hi lo; return`, five bytes) and recursively so up to `java/lang/Object`,
/// at most [`CTOR_SUPER_CHAIN_MAX`] links. Each link must hold what the
/// `Object` link holds, plus:
///
/// * the `Methodref`'s class name is the name of the link's direct
///   superclass in the store (javac's `super()`; the verifier admits no other
///   class for the `uninitializedThis` call but the class itself, and
///   `this(...)` never has the trivial five-byte shape with `()V` calling
///   itself, which the name test also refuses);
/// * the superclass has real class bytes (`ClassOrigin::has_real_bytes`): a
///   compatibility stub can be promoted to other bytes in place, which only
///   the class-redefinition count would not see; a class with real bytes
///   changes its method bodies only by redefinition, which the memo's
///   `class_redefinition_count` stamp retires;
/// * no interception arm names the superclass constructor
///   (`intercept_shape_of`), and no registered native would run instead
///   (`elidable_ctor_native_would_run`, asked of every link after the
///   class-manager read is dropped, as for `Object`).
///
/// Nothing observable is skipped: each link's body runs only the next link's
/// call, and the classes are initialized already (`new C` initialized `C`
/// and, before it, every superclass). The answer is `Some(frames)`, the
/// frames the framed first call pushes (1 for `Object.<init>`).
#[cold]
#[inline(never)]
fn trivial_object_ctor_uncached(shared: &SharedVm, cached: &CachedBytecodeMethod) -> Option<u8> {
    trivial_object_ctor_uncached_at(shared, cached, 0)
}

/// [`trivial_object_ctor_uncached`] for the `aload_0; invokespecial hi lo` at
/// `call_pc` instead of pc 0 (t1 wave 6, lane G6: the `super()` call of a
/// constructor with stores before it, [`field_ctor_pre_super_plan`]). Every
/// rule is the same; only where the call is read differs.
#[cold]
#[inline(never)]
fn trivial_object_ctor_uncached_at(
    shared: &SharedVm,
    cached: &CachedBytecodeMethod,
    call_pc: usize,
) -> Option<u8> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    // The class-file bytes, without the entry's two padding bytes (see
    // `body_is_trivial_ctor_shape`): what `init.code()` below holds.
    let code_len = super::jit_bridge::cached_method_code_len(cached);
    if code_len < call_pc + 5 || cached.method_name.as_ref() != "<init>" {
        return None;
    }
    let code = &cached.code[..code_len];
    if code[call_pc] != 0x2a || code[call_pc + 1] != 0xb7 {
        return None;
    }
    let super_chain = shared.config.interp_trivial_ctor_super_chain;
    // The classes whose `<init>()V` the first call runs, outermost first; the
    // last is `java/lang/Object`.
    let mut links: Vec<Arc<str>> = Vec::new();
    {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(cached.declaring_class_id)?;
        let init = class.find_method("<init>", cached.method_descriptor.as_ref())?;
        if init.code().map(|c| &c.code[..]) != Some(&code[..]) {
            return None;
        }
        // t1 wave 8, lane I24: the `<init>`-return hook needs this frame.
        if class.has_finalizer && ctor_elision_keeps_finalizable(shared) {
            return None;
        }
        let mut cur = class;
        let mut cp_index = u16::from_be_bytes([code[call_pc + 2], code[call_pc + 3]]);
        loop {
            let cp = &cur.constant_pool;
            let (target, name_and_type) = match cp.get(cp_index) {
                Some(ConstantPoolEntry::MethodReference {
                    class_index,
                    name_and_type_index,
                }) => (cp.get_class_name(*class_index)?, *name_and_type_index),
                _ => return None,
            };
            if cp.get_name_and_type(name_and_type) != Some(("<init>", "()V")) {
                return None;
            }
            let sup = cur.superclass.and_then(|id| cm.get_class(id))?;
            if &*sup.name != target {
                return None;
            }
            let sup_init = sup.find_method("<init>", "()V")?;
            if sup_init.is_synchronized() {
                return None;
            }
            let sup_code = &sup_init.code()?.code[..];
            links.push(Arc::clone(&sup.name));
            if &*sup.name == "java/lang/Object" {
                if sup_code.first().copied() != Some(0xb1) {
                    return None;
                }
                break;
            }
            if !super_chain
                || links.len() >= CTOR_SUPER_CHAIN_MAX
                || !sup.origin.has_real_bytes()
                || sup_code.len() != 5
                || sup_code[0] != 0x2a
                || sup_code[1] != 0xb7
                || sup_code[4] != 0xb1
                || intercept_shape_of(sup.name.as_ref(), "<init>", "()V") != 0
            {
                return None;
            }
            cp_index = u16::from_be_bytes([sup_code[2], sup_code[3]]);
            cur = sup;
        }
    }
    for link in &links {
        if super::jit_bridge::elidable_ctor_native_would_run(shared, link) {
            return None;
        }
    }
    u8::try_from(links.len()).ok()
}

/// Longest super chain [`trivial_object_ctor_uncached`] follows, counting the
/// `java/lang/Object` link (wave 94). Deeper hierarchies keep their frames.
const CTOR_SUPER_CHAIN_MAX: usize = 8;

/// `CRATONVM_INTERP_CTOR_ELISION_KEEPS_FINALIZABLE` (t1 wave 8, lane I24;
/// default ON, `=0` restores the old answer): the special door's frameless
/// constructor (trivial, super chain, field-store, pre-super stores: every
/// verdict goes through [`trivial_object_ctor_uncached_at`]) refuses a
/// constructor whose DECLARING class has a finalizer, as the JIT twin
/// (`jit_bridge::is_elidable_construction`) always has.
///
/// Why: this VM registers a finalizable object at its allocation and models
/// HotSpot's `Object.<init>`-return registration with two hooks
/// (`exception_dispatch`, JIT round 21 lane finalize): the unwinder withdraws
/// the registration of an object whose constructor threw before `super()`,
/// and the interpreter's `return` arm of a constructor frame on a finalizable
/// receiver (`finalizer_init_returned`) re-registers an object an earlier
/// `<init>` attempt withdrew (a creator that catches the throw and retries
/// `<init>` on the same object) and marks its row constructed (so a later
/// JNI / unverified re-`<init>` that throws before `super()` withdraws
/// nothing). A frameless constructor runs no `return`, so with the elision a
/// retried object stayed withdrawn and never finalized, and a constructed
/// row stayed unmarked -- HotSpot finalizes both. Both shapes are reachable
/// only from UNVERIFIED code (a boot class, `-Xverify:none`): a verified
/// creator cannot retry `<init>` in its handler, and a verified `<init>`
/// takes an uninitialized receiver (`docs/internal/fixed-bugs/
/// r21-finalize-reinit-residuals-FIXED-20261020.md`), so this is the JIT
/// twin's rule made the interpreter's, at the price of the elision for
/// finalizable classes only. The creator's
/// `invokespecial` of an uninitialized `new F` must name `F.<init>`, so the
/// declaring class is the receiver's class there; a superclass constructor
/// elided from a subclass constructor frame is covered by that frame's own
/// `return` (refusing a finalizable declaring class there is only
/// conservative). Only asked for a class with a finalizer, on the verdict's
/// cold path (memoized per method), so the flag read is rare. Moot, and
/// `false`, when the hooks themselves are off
/// (`CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT`,
/// `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN`).
#[cold]
fn ctor_elision_keeps_finalizable(shared: &SharedVm) -> bool {
    super::exception_dispatch::finalizer_register_at_object_init_enabled(&shared.config)
        && shared.config.finalizer_reregister_at_init_return
        && cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_INTERP_CTOR_ELISION_KEEPS_FINALIZABLE",
        )
}

/// May the door answer a call of a trivial constructor (one that
/// [`body_is_trivial_ctor_shape`] and [`trivial_object_ctor`] accept) without
/// a frame? What [`empty_body_elidable`] asks, for BOTH frames the call would
/// have pushed (the constructor's and `Object.<init>`'s), so the VM-wide
/// observer question: no JVMTI listener that needs the interpreter and no
/// JDWP request in force. A constructor is never `synchronized`.
///
/// The answer is `Some(chain_frames)` ([`CtorVerdict::chain_frames`]) when the
/// call may be elided and the frame stack has room for every frame the framed
/// call would push (wave 94: more than two over a super chain), `None`
/// otherwise.
#[inline]
fn trivial_ctor_elidable(
    shared: &SharedVm,
    frames: &crate::runtime::frame::FrameStack,
    cached: &Arc<CachedBytecodeMethod>,
) -> Option<u8> {
    if !TRIVIAL_CTOR_ELISION || super::jit_bridge::jvmti_requires_interpreter(shared) {
        return None;
    }
    let verdict = ctor_verdict(shared, cached);
    (verdict.object_init_first
        && !frames.at_frame_limit(
            usize::from(verdict.chain_frames),
            shared.config.max_stack_depth,
        ))
    .then_some(verdict.chain_frames)
}

/// Kill switch for the non-virtual door's frameless call of a FIELD-STORE
/// constructor (interpreter round i1 wave 28, lane L7; stage 2 of
/// `docs/internal/retired/i24-L4-proposal-frameless-trivial-constructor-in-the-special-door-20260927-RETIRED-20261027.md`).
/// `true`: an `invokespecial` of an `<init>` whose body is `aload_0;
/// invokespecial java/lang/Object.<init>()V`, then one to
/// `field_fast::CTOR_FIELD_STORES_MAX` stores `aload_0; <value>; putfield`
/// of a parameter or a small constant, then `return` -- javac's constructor
/// of a direct `Object` subclass that only assigns its fields, `Point(int a,
/// int b) { this.a = a; this.b = b; }` or `int x = 7;` -- is answered by
/// making those stores and popping the arguments, with no frame, whenever
/// [`field_ctor_frameless`] allows it. `false`: the frame is pushed.
pub(super) const FIELD_CTOR_ELISION: bool = true;

/// Longest real body [`field_ctor_plan`] can accept: the four-byte
/// `Object.<init>` call, `CTOR_FIELD_STORES_MAX` stores of at most seven bytes
/// (`aload_0; sipush b1 b2; putfield hi lo`), the `return`.
const FIELD_CTOR_MAX_LEN: usize = 4 + 7 * super::field_fast::CTOR_FIELD_STORES_MAX + 1;

/// Where a [`field_ctor_plan`] store's value comes from.
#[derive(Clone, Copy, Debug, PartialEq)]
enum CtorValue {
    /// `iconst_*`, `bipush`, `sipush`.
    Int(i32),
    /// `fconst_*`.
    Float(f32),
    /// `aconst_null`.
    Null,
    /// A parameter: `iload`, `fload` or `aload` of local `n >= 1`. Once the
    /// verdict has mapped locals to arguments ([`ctor_plan_locals_to_args`],
    /// `CRATONVM_INTERP_FIELD_CTOR_WIDE_VALUES`), the ARGUMENT index (the
    /// receiver is argument 0), of any category; without the switch the two
    /// are the same number (no category-2 parameter is admitted).
    Local(u8),
    /// `lconst_*` (interpreter round i6 wave 115, lane L7; parsed only under
    /// `CRATONVM_INTERP_FIELD_CTOR_WIDE_VALUES`).
    Long(i64),
    /// `dconst_*` (the same).
    Double(f64),
    /// `lload` / `dload` of local `n >= 1` (the same): turned into
    /// [`Self::Local`] of the argument that local holds by
    /// [`ctor_plan_locals_to_args`], which also checks that argument is
    /// `long` / `double`.
    LocalWide(u8),
    /// `ldc` / `ldc_w` of constant-pool entry `n` (interpreter round i6 wave
    /// 118, lane L7; parsed only under `CRATONVM_INTERP_FIELD_CTOR_LDC_VALUES`):
    /// turned into [`Self::Int`] / [`Self::Float`] by
    /// [`ctor_plan_constants_from_pool`], which refuses any other entry (a
    /// `String`, a `Class`, a dynamic constant: a reference, or a resolution).
    Ldc(u16),
    /// `ldc2_w` of entry `n` (the same): turned into [`Self::Long`] /
    /// [`Self::Double`].
    Ldc2(u16),
    /// `aload` of local `n >= 1`, then `dup; invokestatic #m; pop`: javac
    /// 25's null check of an inner class's outer instance before its
    /// `this$0` store (parsed only before the `super()` call of
    /// [`field_ctor_pre_super_plan`]). Turned into [`Self::LocalNonNull`] by
    /// [`ctor_plan_constants_from_pool`], which checks that `#m` is the JDK's
    /// own `java/util/Objects.requireNonNull(Object)`.
    NullChecked(u8, u16),
    /// [`Self::Local`] of a reference argument the body null-checks: a null
    /// argument keeps the frame, so the framed body throws HotSpot's own
    /// `NullPointerException` before any store.
    LocalNonNull(u8),
}

/// The cheap byte screen in front of [`field_ctor_frameless`], asked of every
/// `invokespecial` the door is about to push a frame for: the real length is
/// in range and the body opens `aload_0; invokespecial ..; aload_0` and ends
/// `return`. Everything else about the shape is [`field_ctor_plan`]'s.
#[inline(always)]
fn field_ctor_prefilter(cached: &CachedBytecodeMethod) -> bool {
    let len = super::jit_bridge::cached_method_code_len(cached);
    let code = &cached.code;
    // Not `synchronized` (t1 wave-2 lane K review): a format-checked `<init>`
    // never is, but a boot class is format-checked neither here nor on
    // HotSpot, whose interpreter then takes the monitor; a frameless answer
    // would skip it.
    !cached.is_synchronized
        && (10..=FIELD_CTOR_MAX_LEN).contains(&len)
        && code[0] == 0x2a
        && code[1] == 0xb7
        && code[4] == 0x2a
        && code[len - 1] == 0xb1
}

/// Parse a field-store constructor's class-file body (no padding): `aload_0;
/// invokespecial hi lo`, then 1..=`CTOR_FIELD_STORES_MAX` stores `aload_0;
/// <value>; putfield fh fl`, then `return` and nothing after it. Returns the
/// stores in order, `(field constant-pool index, value)`, and how many there
/// are; `None` for any other body. Pure bytes: which method `hi lo` names is
/// [`trivial_object_ctor`]'s question, and each field's resolution the
/// thread's field-site cache's ([`super::field_fast::ctor_field_stores_fast`]).
/// (Since wave 115, the switch-off form of [`field_ctor_plan_with`].)
fn field_ctor_plan(
    code: &[u8],
) -> Option<(
    [(u16, CtorValue); super::field_fast::CTOR_FIELD_STORES_MAX],
    usize,
)> {
    field_ctor_plan_with(code, false, false)
}

/// [`field_ctor_plan`], and with `wide` (`CRATONVM_INTERP_FIELD_CTOR_WIDE_VALUES`,
/// interpreter round i6 wave 115, lane L7) also the category-2 values:
/// `lconst_*` / `dconst_*` ([`CtorValue::Long`] / [`CtorValue::Double`]) and
/// `lload` / `dload` of a local `n >= 1` ([`CtorValue::LocalWide`], a LOCAL
/// slot until [`ctor_plan_locals_to_args`] maps it).
fn field_ctor_plan_with(
    code: &[u8],
    wide: bool,
    ldc: bool,
) -> Option<(
    [(u16, CtorValue); super::field_fast::CTOR_FIELD_STORES_MAX],
    usize,
)> {
    const MAX: usize = super::field_fast::CTOR_FIELD_STORES_MAX;
    if code.len() < 10 || code[0] != 0x2a || code[1] != 0xb7 {
        return None;
    }
    let mut plan = [(0u16, CtorValue::Null); MAX];
    let mut n = 0usize;
    let mut pc = 4usize;
    loop {
        match *code.get(pc)? {
            0xb1 if pc + 1 == code.len() && n > 0 => return Some((plan, n)),
            0x2a => {}
            _ => return None,
        }
        if n == MAX {
            return None;
        }
        let (field_cp, value, next) = ctor_store_at(code, pc, wide, ldc, false)?;
        plan[n] = (field_cp, value);
        n += 1;
        pc = next;
    }
}

/// One field store `aload_0; <value>; putfield fh fl` at `pc` of a
/// constructor body, as [`field_ctor_plan_with`] reads it (the same value
/// opcodes, `wide` and `ldc` as there): `(field constant-pool index, value,
/// pc after the putfield)`, or `None` for anything else. Split out of
/// [`field_ctor_plan_with`] (t1 wave 6, lane G6) so the pre-`super()` stores
/// of [`field_ctor_pre_super_plan`] are read by the same code. With
/// `null_check`, an `aload` value may be followed by javac's
/// `dup; invokestatic; pop` ([`CtorValue::NullChecked`]).
fn ctor_store_at(
    code: &[u8],
    pc: usize,
    wide: bool,
    ldc: bool,
    null_check: bool,
) -> Option<(u16, CtorValue, usize)> {
    if *code.get(pc)? != 0x2a {
        return None;
    }
    let op = *code.get(pc + 1)?;
    let (mut value, width) = match op {
        0x01 => (CtorValue::Null, 1),
        0x02..=0x08 => (CtorValue::Int(i32::from(op) - 3), 1),
        0x0b => (CtorValue::Float(0.0), 1),
        0x0c => (CtorValue::Float(1.0), 1),
        0x0d => (CtorValue::Float(2.0), 1),
        // Cast: bipush's operand is a signed byte.
        0x10 => (CtorValue::Int(i32::from(*code.get(pc + 2)? as i8)), 2),
        0x11 => {
            let hi = *code.get(pc + 2)?;
            let lo = *code.get(pc + 3)?;
            (CtorValue::Int(i32::from(i16::from_be_bytes([hi, lo]))), 3)
        }
        // iload_1..3, fload_1..3, aload_1..3 (local 0 is `this`).
        0x1b..=0x1d => (CtorValue::Local(op - 0x1a), 1),
        0x23..=0x25 => (CtorValue::Local(op - 0x22), 1),
        0x2b..=0x2d => (CtorValue::Local(op - 0x2a), 1),
        // iload, fload, aload of a numbered local.
        0x15 | 0x17 | 0x19 => match *code.get(pc + 2)? {
            0 => return None,
            local => (CtorValue::Local(local), 2),
        },
        // Wave 115 (`wide`): lconst_0/1, dconst_0/1, lload_1..3,
        // dload_1..3, lload / dload of a numbered local.
        0x09 | 0x0a if wide => (CtorValue::Long(i64::from(op) - 9), 1),
        0x0e if wide => (CtorValue::Double(0.0), 1),
        0x0f if wide => (CtorValue::Double(1.0), 1),
        0x1f..=0x21 if wide => (CtorValue::LocalWide(op - 0x1e), 1),
        0x27..=0x29 if wide => (CtorValue::LocalWide(op - 0x26), 1),
        0x16 | 0x18 if wide => match *code.get(pc + 2)? {
            0 => return None,
            local => (CtorValue::LocalWide(local), 2),
        },
        // Wave 118 (`ldc`): ldc, ldc_w, ldc2_w (the entry is read by the
        // verdict, `ctor_plan_constants_from_pool`).
        0x12 if ldc => (CtorValue::Ldc(u16::from(*code.get(pc + 2)?)), 2),
        0x13 if ldc => {
            let hi = *code.get(pc + 2)?;
            let lo = *code.get(pc + 3)?;
            (CtorValue::Ldc(u16::from_be_bytes([hi, lo])), 3)
        }
        0x14 if ldc => {
            let hi = *code.get(pc + 2)?;
            let lo = *code.get(pc + 3)?;
            (CtorValue::Ldc2(u16::from_be_bytes([hi, lo])), 3)
        }
        _ => return None,
    };
    let mut put = pc + 1 + width;
    let is_aload = matches!(op, 0x2b..=0x2d | 0x19);
    if let (true, CtorValue::Local(local), Some(&[0x59, 0xb8, hi, lo, 0x57])) =
        (null_check && is_aload, value, code.get(put..put + 5))
    {
        value = CtorValue::NullChecked(local, u16::from_be_bytes([hi, lo]));
        put += 5;
    }
    if *code.get(put)? != 0xb5 {
        return None;
    }
    let field_cp = u16::from_be_bytes([*code.get(put + 1)?, *code.get(put + 2)?]);
    Some((field_cp, value, put + 3))
}

/// The cheap byte screen for a field-store constructor with stores BEFORE
/// its `super()` call ([`field_ctor_pre_super_plan`]; t1 wave 6, lane G6,
/// `CRATONVM_INTERP_FIELD_CTOR_PRE_SUPER_STORES`): the real length is in
/// range, the body opens `aload_0` followed by anything but `invokespecial`
/// (that is [`field_ctor_prefilter`]'s shape) and ends `return`.
#[inline(always)]
fn field_ctor_pre_super_prefilter(cached: &CachedBytecodeMethod) -> bool {
    let len = super::jit_bridge::cached_method_code_len(cached);
    let code = &cached.code;
    !cached.is_synchronized
        && (10..=FIELD_CTOR_MAX_LEN).contains(&len)
        && code[0] == 0x2a
        && code[1] != 0xb7
        && code[len - 1] == 0xb1
}

/// t1 wave 6, lane G6 (`CRATONVM_INTERP_FIELD_CTOR_PRE_SUPER_STORES`, default
/// ON since 2026-10-29; the third cut of
/// `docs/internal/retired/t1-interp2-proposal-proxy-hit-path-cuts-20261025-RETIRED-20261029.md`,
/// for `i147-orch-proposal-proxy-new-proxy-instance-cache-hit-...`): parse
/// javac's constructor of an INNER or LOCAL / anonymous class, whose outer
/// instance (`this$0`) and captured values (`val$x`) are stored before the
/// `super()` call: one or more stores `aload_0; <value>; putfield`, then
/// `aload_0; invokespecial hi lo`, then zero or more stores, then `return`,
/// at most `CTOR_FIELD_STORES_MAX` stores in all. `AbstractClassLoaderValue$Sub`
/// (`Sub(K key) { this.key = key; }`: `this$0`, `super()`, `key`), made on
/// every `Proxy.newProxyInstance` cache hit, and `HashMap$KeySet`-style
/// views over a trivial `AbstractSet` chain are this shape.
///
/// Returns the stores in body order, their count and the pc of the
/// `super()` call's `aload_0`; `None` for any other body, the
/// [`field_ctor_plan_with`] shape included (no store before the call).
///
/// Why answering it without a frame is the framed call's effect: the call at
/// the returned pc must still pass [`trivial_object_ctor_uncached`] (a chain
/// of trivial constructors ending at a plain `Object.<init>`, no native, no
/// interception), so it does nothing observable, and the stores are made in
/// body order by the same validated, all-or-nothing site code as every
/// field-store constructor's. A store before `super()` is legal only to a
/// field the class itself declares (JVMS 4.10.1.9 `putfield` on
/// `uninitializedThis`), which is what the site's `<init>` final-field
/// admission already checks for each store. No instruction can throw, so
/// nothing distinguishes "stores, then a no-op, then stores" from the stores
/// in that order.
fn field_ctor_pre_super_plan(
    code: &[u8],
    wide: bool,
    ldc: bool,
) -> Option<(
    [(u16, CtorValue); super::field_fast::CTOR_FIELD_STORES_MAX],
    usize,
    usize,
)> {
    const MAX: usize = super::field_fast::CTOR_FIELD_STORES_MAX;
    if code.len() < 10 || code[0] != 0x2a || code[1] == 0xb7 {
        return None;
    }
    let mut plan = [(0u16, CtorValue::Null); MAX];
    let mut n = 0usize;
    let mut pc = 0usize;
    // The stores before the call.
    let call_pc = loop {
        if *code.get(pc)? != 0x2a {
            return None;
        }
        if *code.get(pc + 1)? == 0xb7 {
            break pc;
        }
        if n == MAX {
            return None;
        }
        let (field_cp, value, next) = ctor_store_at(code, pc, wide, ldc, true)?;
        plan[n] = (field_cp, value);
        n += 1;
        pc = next;
    };
    if n == 0 {
        return None;
    }
    // The stores after it, then `return` and nothing else.
    pc = call_pc + 4;
    loop {
        match *code.get(pc)? {
            0xb1 if pc + 1 == code.len() => return Some((plan, n, call_pc)),
            0x2a => {}
            _ => return None,
        }
        if n == MAX {
            return None;
        }
        let (field_cp, value, next) = ctor_store_at(code, pc, wide, ldc, false)?;
        plan[n] = (field_cp, value);
        n += 1;
        pc = next;
    }
}

/// [`note_empty_body_call`] for [`FIELD_CTOR_ELISION`].
#[inline]
fn note_field_ctor_call(census: bool, elided: bool) {
    if !census {
        return;
    }
    let counter = if elided {
        &FRAMELESS_CENSUS[FRAMELESS_FIELD_CTOR_ELIDED]
    } else {
        &FRAMELESS_CENSUS[FRAMELESS_FIELD_CTOR_FRAMED]
    };
    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Answer a field-store constructor call without a frame
/// ([`FIELD_CTOR_ELISION`]), or return `false` having changed nothing so the
/// door pushes the frame. Called by [`execute_nonvirtual_fast_door`] for an
/// `invokespecial` that [`nonvirtual_door_prelude`] cleared for a push (the
/// receiver non-null, the arguments read verbatim into `slots`, the stack
/// below its limit) and that passed [`field_ctor_prefilter`]; nothing since
/// that read can reach a safepoint, so the references in `slots` are current.
///
/// What the framed call would do, and why answering without it is the same:
///
/// * `Object.<init>` -- plain bytecode that returns at entry, on a direct
///   `Object` subclass, with no native in front of it ([`trivial_object_ctor`],
///   the wave-25 verdict): nothing, which the door already answers without a
///   frame (`EMPTY_BODY_ELISION`).
/// * each `putfield` -- the store the quickened arm would make in the
///   constructor's frame, through the same site and store code
///   (`field_fast::ctor_field_stores_fast`), all or nothing, after the
///   `Object.<init>` call as in the body. A value it cannot store verbatim, a
///   site this thread has not filled (the first calls: the framed body's
///   slow `putfield` fills it), a foreign final, a watched field -- the frame.
/// * nothing else: no branch, no allocation, no call that can throw or
///   safepoint. Finalizable instances are registered at allocation
///   (`gc_and_alloc::init_new_instance`).
/// * observers -- the VM-wide `jvmti_requires_interpreter` (MethodEntry/Exit,
///   FramePop, single step, a JDWP request in force) keeps both frames, as for
///   the trivial constructor; a field watch keeps them too.
/// * stack depth -- the framed call needs this frame and `Object.<init>`'s,
///   so one below the limit it keeps its frame and overflows where it did.
/// * locals -- only parameters are read, and only when every parameter is
///   category-1, so local `n` is argument slot `n`; under
///   `CRATONVM_INTERP_FIELD_CTOR_WIDE_VALUES` (wave 115) the verdict maps each
///   local to the argument that starts there (`ctor_plan_locals_to_args`), and
///   a `long` / `double` value is stored with its kind mark.
///
/// Like the empty body and the trivial constructor, it is not counted for
/// tier-up.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn field_ctor_frameless(
    shared: &SharedVm,
    frames: &mut crate::runtime::frame::FrameStack,
    sites: &mut super::site_cache::FastFieldSiteCache,
    frame_idx: usize,
    cached: &Arc<CachedBytecodeMethod>,
    slots: &ArgSlots,
    total_args: usize,
    fast_field: Option<&cratonvm_gc::zgc::ZgcRealHeap>,
) -> bool {
    // One memo probe answers everything that is a constant of the method
    // (wave 28, lane L7b): whether its first instruction is a plain
    // `Object.<init>`, and its parsed stores, `plan_len == 0` for a
    // constructor this door never answers -- another parent (`super(); this.x
    // = x;`), a `long` / `double` parameter, a body the screen passed but the
    // parse refuses, a method not named `<init>`. Such a constructor used to
    // pay the parse and the per-call checks below on every call before it was
    // declined (`L7W28FramelessCtorBench` `new-long-param` +16%).
    let verdict = ctor_verdict(shared, cached);
    if !verdict.object_init_first || verdict.plan_len == 0 {
        return false;
    }
    let plan = &verdict.plan[..usize::from(verdict.plan_len)];
    let elided = field_ctor_stores_answered(
        shared,
        frames,
        sites,
        cached,
        slots,
        total_args,
        plan,
        verdict.chain_frames,
        fast_field,
    );
    note_field_ctor_call(shared.config.site_census, elided);
    if !elided {
        return false;
    }
    note_super_chain_elided(shared.config.site_census, verdict.chain_frames);
    frames[frame_idx].stack.discard_top(total_args);
    site_stats::bump_if(shared.config.site_census, site_stats::DOOR_SPECIAL_HIT);
    dbg_invoke_stats_record(shared.config.invoke_stats, 0);
    true
}

/// The checks and the stores of [`field_ctor_frameless`], for a body whose
/// plan is `plan` and whose [`trivial_object_ctor`] verdict is `true`.
/// `true`: every store was made. `fast_field` is the caller's
/// `field_fast::fast_field_zgc` answer. `chain_frames` is the verdict's
/// [`CtorVerdict::chain_frames`]: the frames the first call would push (1 for
/// `Object.<init>`; more over a super chain, wave 94), all of which must fit.
#[inline]
#[allow(clippy::too_many_arguments)]
fn field_ctor_stores_answered(
    shared: &SharedVm,
    frames: &crate::runtime::frame::FrameStack,
    sites: &mut super::site_cache::FastFieldSiteCache,
    cached: &Arc<CachedBytecodeMethod>,
    slots: &ArgSlots,
    total_args: usize,
    plan: &[(u16, CtorValue)],
    chain_frames: u8,
    fast_field: Option<&cratonvm_gc::zgc::ZgcRealHeap>,
) -> bool {
    if frames.at_frame_limit(usize::from(chain_frames), shared.config.max_stack_depth)
        || super::jit_bridge::jvmti_requires_interpreter(shared)
    {
        return false;
    }
    let args = slots.filled();
    // `CRATONVM_INTERP_FIELD_CTOR_WIDE_VALUES` (wave 115, lane L7): the
    // verdict mapped the plan's locals to argument indices by the descriptor,
    // so a `long` / `double` argument is admitted; off, refused as before
    // (local `n` is argument `n` only without one).
    let wide = shared.config.interp_field_ctor_wide_values;
    if args.len() != total_args
        || (!wide && args[1..].iter().any(|&(_, tag)| tag == b'J' || tag == b'D'))
    {
        return false;
    }
    let Some(recv) = slots.receiver_ptr() else {
        return false;
    };
    // ZGC answers through the quickened arms' raw stores. The Generational
    // and G1 heaps, which have no quickened arms, answer through the heap's
    // field accessor under `CRATONVM_INTERP_FIELD_CTOR_GEN_G1` (tier-1 round
    // wave 2, lane K; `field_fast::ctor_field_stores_accessor`); off, they keep
    // the frame as before.
    if fast_field.is_none()
        && !(shared.config.interp_field_ctor_gen_g1
            && super::field_fast::ctor_accessor_heap_admitted(shared))
    {
        return false;
    }
    // Each store's value and its operand-stack kind mark, as the quickened
    // `putfield` hands them to `put_field_value`: the long / double mark for a
    // category-2 value (an argument of tag `J` / `D`, or a wave-115 constant),
    // none otherwise.
    let mut stores = [(0u16, CompactValue::null(), 0u8); super::field_fast::CTOR_FIELD_STORES_MAX];
    for (store, &(field_cp, value)) in stores.iter_mut().zip(plan) {
        let (value, kind) = match value {
            CtorValue::Int(v) => (CompactValue::int(v), 0),
            CtorValue::Float(f) => (CompactValue::float(f), 0),
            CtorValue::Null => (CompactValue::null(), 0),
            CtorValue::Long(v) => (
                CompactValue::long(v),
                crate::runtime::ValueStack::KIND_MARK_LONG,
            ),
            // Stored verbatim with its mark (`double_raw`'s contract): a
            // `dconst_*`, or (wave 118) an `ldc2_w` double, whose NaN payload
            // the framed `ldc2_w` keeps too.
            CtorValue::Double(d) => (
                CompactValue::double_raw(d),
                crate::runtime::ValueStack::KIND_MARK_DOUBLE,
            ),
            CtorValue::Local(local) => match args.get(usize::from(local)) {
                Some(&(cv, tag)) if local != 0 => match tag {
                    b'J' => (cv, crate::runtime::ValueStack::KIND_MARK_LONG),
                    b'D' => (cv, crate::runtime::ValueStack::KIND_MARK_DOUBLE),
                    _ => (cv, 0),
                },
                _ => return false,
            },
            // A null outer instance keeps the frame: the body's
            // `Objects.requireNonNull` throws, before any store.
            CtorValue::LocalNonNull(local) => match args.get(usize::from(local)) {
                Some(&(cv, _)) if local != 0 && !cv.is_null() => (cv, 0),
                _ => return false,
            },
            // Never left in a verdict's plan (mapped to `Local`, to
            // `LocalNonNull`, or to a numeric constant).
            CtorValue::LocalWide(_)
            | CtorValue::Ldc(_)
            | CtorValue::Ldc2(_)
            | CtorValue::NullChecked(..) => return false,
        };
        *store = (field_cp, value, kind);
    }
    match fast_field {
        Some(zgc) => super::field_fast::ctor_field_stores_fast(
            zgc,
            sites,
            cached.declaring_class_id,
            recv,
            &stores[..plan.len()],
            shared.config.site_census,
        ),
        None => super::field_fast::ctor_field_stores_accessor(
            shared,
            sites,
            cached.declaring_class_id,
            recv,
            &stores[..plan.len()],
            shared.config.site_census,
        ),
    }
}

/// The non-virtual door's body, [`execute_nonvirtual_fast_door`] after its cache lookup: the entry is
/// `cached`, with `expected_receiver` the class a `VirtualBytecode` entry
/// records (`None` for `Bytecode`), and `target_redefined` whether the entry's
/// target is kept from tier-up because its class was redefined before the
/// fill ([`tier_up_skips_redefined_target`]: never, since wave 39, unless the
/// kill switch is flipped).
///
/// Split out so the VIRTUAL door, which probes the same inline-cache key first
/// for an `invokevirtual`, can serve a `Bytecode` entry it found without the
/// dispatch loop probing the key a second time through this door (the
/// `invokevirtual` of a private nestmate method — `javac` 25's shape — used to
/// hash the same site twice per call). Every decline and every hit is
/// recorded in the census. (A `census: bool` parameter kept a decline out of
/// it for a caller whose decline a later door recorded; both callers passed
/// `true` since wave 6, when the dispatch loop's `0xb6` arm stopped running
/// [`execute_nonvirtual_fast_door`] after the hand-off, and wave 22 removed
/// it.)
///
/// Since wave 24 this body is two functions, [`nonvirtual_door_prelude`] and
/// [`nonvirtual_door_finish`], so both callers can run the first half on the
/// entry they BORROWED from the invoke cache and clone its `Arc` only for a
/// frame push; each caller composes the two (there is no combined function).
///
/// What [`nonvirtual_door_prelude`] leaves for [`nonvirtual_door_finish`].
pub(super) enum NonvirtualPrelude {
    /// An empty body, answered without a frame: the call is complete.
    Elided,
    /// Every decline the borrowed half can take has passed; the arguments
    /// were read into the caller's `slots` (and are still on the caller's
    /// stack) for the frame push.
    ///
    /// The slots live in the caller, not in this variant: `ArgSlots` is 152
    /// bytes (144 then), and carrying it out of the prelude and into the finish moved it
    /// twice per call (a `memmove` in the profile, and the `ctor` /
    /// `super-call` rows of `InvokeDoorCostBench` up to 18% slower on a
    /// fat-LTO build than before the split).
    Frame { total_args: usize },
}

/// The half of the non-virtual door that needs only the caller's frames and
/// a BORROWED entry (interpreter round i1 wave 24, lane L4): every per-call
/// decline up to the receiver checks, then the frameless answer for an empty
/// body (`EMPTY_BODY_ELISION`). An empty body is never counted for tier-up
/// (see [`nonvirtual_door_finish`]), so answering it before the tier-up block
/// changes no count. `None` is a recorded decline.
///
/// Split so [`execute_nonvirtual_fast_door`] can run it on the entry it
/// borrowed from `thread.invoke_cache` -- the frames are a disjoint field --
/// and clone the entry's `Arc` only when a frame is pushed.
#[allow(clippy::too_many_arguments)]
#[inline]
pub(super) fn nonvirtual_door_prelude(
    shared: &SharedVm,
    frames: &mut crate::runtime::frame::FrameStack,
    frame_idx: usize,
    caller_class_id: ClassId,
    is_special: bool,
    cached: &Arc<CachedBytecodeMethod>,
    expected_receiver: Option<ClassId>,
    slots: &mut ArgSlots,
) -> Option<NonvirtualPrelude> {
    const MISS: usize = site_stats::DOOR_SPECIAL_MISS;
    if cached.is_static {
        decline!("special", MISS, "cached target is static");
    }
    // The owner re-check the general path makes on every special hit — the
    // same function, so the door and the general path cannot disagree. It is
    // one relaxed load while no user-defined loader has registered a defining
    // class, and a per-thread memo probe (no `class_manager` lock) after that.
    // A mismatch declines; the general dispatcher runs next and evicts.
    if is_special && cached_static_owner_stale(shared, caller_class_id, cached) {
        decline!("special", MISS, "loader-split owner");
    }
    let num_params = cached.num_params as usize;
    let total_args = num_params + 1;
    let Some(facts) = callee_is_plain_bytecode(shared, cached, num_params) else {
        decline!(
            "special",
            MISS,
            "callee is synchronized, native-backed, intercepted, or over-arity"
        );
    };
    if frames.at_frame_limit(0, shared.config.max_stack_depth) {
        decline!("special", MISS, "frame stack is full");
    }
    if !read_args_verbatim(&frames[frame_idx].stack, facts, total_args, true, slots) {
        decline!("special", MISS, "an argument slot needs coercion");
    }
    // A null receiver is the general path's business: it builds the helpful
    // NPE. `read_args_verbatim` accepts a null in slot 0 because a reference
    // parameter may legitimately be null; the receiver may not.
    let Some(recv_ptr) = slots.receiver_ptr() else {
        decline!("special", MISS, "null or non-object receiver");
    };
    if let Some(expected) = expected_receiver {
        // The validator's yes/no, without building an `ObjectRef` (wave 50,
        // lane L7; see `execute_invokevirtual_fast_door`).
        if !shared.mem.heap.is_object_start(recv_ptr as usize) {
            decline!("special", MISS, "receiver is not a live heap object");
        }
        // SAFETY: `recv_ptr` is a registered object start on this heap.
        let header = unsafe { &*(recv_ptr as *const cratonvm_gc::ObjectHeader) };
        if header.class_id != expected {
            decline!("special", MISS, "receiver class differs from the entry");
        }
        // Interpreter round i7 wave 136, lane L6: an array's header holds its
        // COMPONENT's class id, so a `C[]` matched an entry guarded for `C`.
        // An interface sender's `invokespecial` (the verifier lets any
        // reference stand for an interface type) is cached guarded by the
        // receiver class whose check passed
        // (`invoke::memo_interface_special_site`); an array of that class must
        // reach the slow path's check, as the general arm sends it.
        if header.kind() == cratonvm_types::ObjectKind::Array {
            decline!("special", MISS, "receiver is an array");
        }
    }
    // A body that is a lone `return` is answered without a frame
    // (`EMPTY_BODY_ELISION`): every check above that could decline — null
    // receiver, owner, stack depth, argument shapes — has run, so what is left
    // of the call is popping its arguments. An empty body is never counted
    // for tier-up (see `nonvirtual_door_finish`), so answering it here, before
    // that block, is the order the door always had.
    if body_returns_at_entry(cached) {
        let elide = empty_body_elidable(shared, cached);
        note_empty_body_call(shared.config.site_census, elide);
        if elide {
            frames[frame_idx].stack.discard_top(total_args);
            site_stats::bump_if(shared.config.site_census, site_stats::DOOR_SPECIAL_HIT);
            dbg_invoke_stats_record(shared.config.invoke_stats, 0);
            return Some(NonvirtualPrelude::Elided);
        }
    }
    // A trivial constructor (`aload_0; invokespecial Object.<init>; return`,
    // javac's default constructor of a direct `Object` subclass) is answered
    // the same way (`TRIVIAL_CTOR_ELISION`, wave 25): its one call is the
    // empty `Object.<init>` the branch above answers without a frame, on the
    // receiver this call already checked non-null, so what is left of both
    // calls is popping the receiver. Like the empty body it is not counted
    // for tier-up (a compiled copy does nothing faster, and the JIT elides
    // the same constructor at its call sites).
    //
    // Stack depth (wave 28): the framed call pushes TWO frames, the
    // constructor's (the check above admits it) and then `Object.<init>`'s,
    // which the door declines at a full stack so that the general path throws
    // `StackOverflowError` from inside the constructor. So the frameless answer
    // also needs room for the second frame; one below the limit, the call
    // keeps its frame and overflows exactly where it always did.
    if TRIVIAL_CTOR_ELISION
        && is_special
        && body_is_trivial_ctor_shape(cached)
        && !frames.at_frame_limit(1, shared.config.max_stack_depth)
    {
        let chain_frames = trivial_ctor_elidable(shared, frames, cached);
        note_trivial_ctor_call(shared.config.site_census, chain_frames.is_some());
        if let Some(chain_frames) = chain_frames {
            note_super_chain_elided(shared.config.site_census, chain_frames);
            frames[frame_idx].stack.discard_top(total_args);
            site_stats::bump_if(shared.config.site_census, site_stats::DOOR_SPECIAL_HIT);
            dbg_invoke_stats_record(shared.config.invoke_stats, 0);
            return Some(NonvirtualPrelude::Elided);
        }
    }
    Some(NonvirtualPrelude::Frame { total_args })
}

/// The half of the non-virtual door that needs the whole thread and an OWNED
/// entry: the tier-up block, the monitor of a `synchronized` callee, and the
/// frame push, which moves `cached` into the frame. Runs only after
/// [`nonvirtual_door_prelude`] answered `Frame`, with nothing between that
/// can reach a safepoint, so `slots` is still what the caller's stack holds.
#[allow(clippy::too_many_arguments)]
#[inline]
pub(super) fn nonvirtual_door_finish(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cached: DoorCallee,
    slots: &ArgSlots,
    total_args: usize,
    expected_receiver: Option<ClassId>,
    target_redefined: bool,
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    const MISS: usize = site_stats::DOOR_SPECIAL_MISS;
    // The tier-up block of `execute_invokevirtual_cached`'s `Bytecode` arm,
    // under the same conditions, after every decline the general path would
    // also count (so a declined call is not counted twice) and in the static
    // door's order: the probe before the count. A `VirtualBytecode` entry is
    // left alone — the general arm's chain refuses `is_special`. The arm's
    // virtual-thread exclusion needs no test here: a virtual thread never
    // reaches a door (`invoke_fast_door_on`). Nothing here reaches a safepoint
    // (the offer only queues a background compile), so `slots` stays valid
    // for the push.
    //
    // One deliberate difference: a body that returns at pc 0 (`Object.<init>`
    // and every other empty `void` method — the hottest `invokespecial` in any
    // allocating program) is served uncounted, as before — since wave 20
    // without a frame when no observer needs one (`EMPTY_BODY_ELISION`, in
    // `nonvirtual_door_prelude`). One that reaches this function keeps its
    // frame (a `synchronized` body, an observer) and is still not counted. It
    // does no work a compiled body could do faster, and entering one costs the
    // general arm's JIT call per constructor chain.
    //
    // The per-call test is the four register / cached-flag reads below; the
    // work itself is out of line (`door_tier_up`, wave 22), so under
    // `--nojit` this door is the wave-18 door plus one predicate.
    let empty_body = body_returns_at_entry(&cached);
    // Interpreter round i5 wave 114, lane L6 (`CRATONVM_VTHREAD_NONVIRTUAL_DOORS`,
    // the only way a virtual thread reaches this door; the paragraph above
    // predates it): on a virtual thread the general special arm neither counts
    // nor probes a callee whose compiled entry it refuses, so neither does the
    // door; a `synchronized` callee declines to it. One `kind` compare on any
    // other thread.
    let vthread_gate = vthread_door_gate(shared, thread, &cached, false);
    if vthread_gate == VthreadDoorGate::DeclineSynchronized {
        decline!("special", MISS, "synchronized callee on a virtual thread");
    }
    if SPECIAL_DOOR_TIERS_UP
        && vthread_gate == VthreadDoorGate::Platform
        && expected_receiver.is_none()
        && !target_redefined
        && !empty_body
        && shared.config.jit_enabled
    {
        match door_tier_up(shared, thread, &cached) {
            DoorTierUp::Serve => {}
            DoorTierUp::CompiledBody => {
                decline!("special", MISS, "callee has a compiled body")
            }
            DoorTierUp::InlineUpgradeDue => {
                decline!("special", MISS, "an inline tier-up attempt is due")
            }
        }
    }
    // The receiver is `slots[0]`: `read_args_verbatim` ran with the
    // has-receiver flag, and there is no safepoint between it and the push.
    let recv = slots.receiver_ptr().map(|ptr| {
        // SAFETY: the slot came off the caller operand stack as an object
        // reference, so the address is a live object start on this heap.
        unsafe { ObjectRef::from_raw(ptr as *mut u8) }
    });
    let Some(monitor) = door_monitor_acquire(shared, thread, &cached, recv) else {
        decline!("special", MISS, "synchronized callee: contended or static");
    };
    site_stats::bump_if(shared.config.site_census, site_stats::DOOR_SPECIAL_HIT);
    dbg_invoke_stats_record(shared.config.invoke_stats, 0);
    Some(Ok(push_frame_verbatim(
        shared, thread, frame_idx, cached, slots, total_args, monitor,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts_of(descriptor: &str) -> cratonvm_jit_api::DescriptorFacts {
        cratonvm_jit_api::DescriptorFacts::of(descriptor)
    }

    fn stack_with(values: &[(CompactValue, u8)]) -> crate::runtime::ValueStack {
        let mut s = crate::runtime::ValueStack::new(values.len() + 8);
        for (cv, kind) in values {
            match *kind {
                crate::runtime::ValueStack::KIND_MARK_LONG => s.push_compact_long(*cv),
                crate::runtime::ValueStack::KIND_MARK_DOUBLE => s.push_compact_double(*cv),
                _ => s.push_compact(*cv),
            }
        }
        s
    }

    /// The tag walk must line up with `ParamTags::get_with_receiver`: slot 0
    /// is the receiver when there is one, and the descriptor's tags follow.
    #[test]
    fn the_receiver_shifts_the_parameter_tags_by_one() {
        let facts = facts_of("(IJ)V");
        let long_bits = CompactValue::long(7);
        let stack = stack_with(&[
            (CompactValue::null(), 0),
            (CompactValue::int(1), 0),
            (long_bits, crate::runtime::ValueStack::KIND_MARK_LONG),
        ]);
        let mut slots = empty_arg_slots();
        assert!(read_args_verbatim(&stack, &facts, 3, true, &mut slots));
        assert_eq!(slots.filled()[0].1, b'L', "receiver");
        assert_eq!(slots.filled()[1].1, b'I');
        assert_eq!(slots.filled()[2].1, b'J');
        // Without a receiver the same stack is an (I, J) static call.
        let stack = stack_with(&[
            (CompactValue::int(1), 0),
            (long_bits, crate::runtime::ValueStack::KIND_MARK_LONG),
        ]);
        let mut slots = empty_arg_slots();
        assert!(read_args_verbatim(&stack, &facts, 2, false, &mut slots));
        assert_eq!(slots.filled()[0].1, b'I');
        assert_eq!(slots.filled()[1].1, b'J');
    }

    /// `ArgSlots` starts uninitialized (wave 27): `filled()` must expose
    /// exactly the prefix the last successful read wrote, nothing before the
    /// first read, and nothing after a failed one -- even a failed re-read
    /// of slots a successful read had filled (the virtual door re-reads after
    /// an inline compile).
    #[test]
    fn filled_is_the_prefix_of_the_last_successful_read() {
        let mut slots = empty_arg_slots();
        assert!(slots.filled().is_empty(), "nothing read yet");
        assert!(slots.receiver_ptr().is_none());
        let facts = facts_of("(IJ)V");
        let stack = stack_with(&[
            (CompactValue::int(1), 0),
            (
                CompactValue::long(2),
                crate::runtime::ValueStack::KIND_MARK_LONG,
            ),
        ]);
        assert!(read_args_verbatim(&stack, &facts, 2, false, &mut slots));
        assert_eq!(slots.filled().len(), 2);
        assert_eq!(slots.filled()[0].0.as_int(), Some(1));
        // The same stack read as `(II)V` fails on the second slot, after the
        // first was rewritten: the failure must leave no claim behind.
        let facts = facts_of("(II)V");
        assert!(!read_args_verbatim(&stack, &facts, 2, false, &mut slots));
        assert!(slots.filled().is_empty(), "a failed read exposes nothing");
        // A read longer than the array is refused before any write.
        let facts = facts_of("(IIIIIIII)V"); // eight: the inline maximum
        let stack = stack_with(&[(CompactValue::int(3), 0); 10]);
        assert!(!read_args_verbatim(&stack, &facts, 10, true, &mut slots));
        assert!(slots.filled().is_empty());
        // The maximum is accepted: receiver plus eight.
        assert!(
            !read_args_verbatim(&stack, &facts, 9, true, &mut slots),
            "an int is no receiver"
        );
        let mut s = crate::runtime::ValueStack::new(16);
        s.push_compact(CompactValue::null());
        for _ in 0..8 {
            s.push_compact(CompactValue::int(4));
        }
        assert!(read_args_verbatim(&s, &facts, 9, true, &mut slots));
        assert_eq!(slots.filled().len(), 9);
        assert!(
            slots.receiver_ptr().is_none(),
            "a null receiver has no address"
        );
    }

    /// A category-2 argument whose slot carries no kind mark is exactly the
    /// shape `pop_arg_for_descriptor_checked` exists to disambiguate, and the
    /// door must decline it rather than guess.
    #[test]
    fn an_unmarked_category_two_argument_is_declined() {
        let facts = facts_of("(J)V");
        let stack = stack_with(&[(CompactValue::long(7), 0)]); // pushed unmarked
        let mut slots = empty_arg_slots();
        assert!(!read_args_verbatim(&stack, &facts, 1, false, &mut slots));
        // Marked, it is accepted.
        let stack = stack_with(&[(
            CompactValue::long(7),
            crate::runtime::ValueStack::KIND_MARK_LONG,
        )]);
        let mut slots = empty_arg_slots();
        assert!(read_args_verbatim(&stack, &facts, 1, false, &mut slots));
    }

    /// An `int` where the descriptor says reference (the `Int(0)`-as-null
    /// shape) is declined, and so is a reference where it says `int`.
    #[test]
    fn a_slot_that_disagrees_with_the_descriptor_is_declined() {
        let facts = facts_of("(Ljava/lang/Object;)V");
        let stack = stack_with(&[(CompactValue::int(0), 0)]);
        let mut slots = empty_arg_slots();
        assert!(!read_args_verbatim(&stack, &facts, 1, false, &mut slots));

        let facts = facts_of("(I)V");
        let stack = stack_with(&[(CompactValue::null(), 0)]);
        let mut slots = empty_arg_slots();
        assert!(!read_args_verbatim(&stack, &facts, 1, false, &mut slots));
    }

    /// Reading never pops: a decline must leave the stack exactly as it was
    /// for the general dispatcher.
    #[test]
    fn reading_arguments_does_not_disturb_the_stack() {
        let facts = facts_of("(IJ)V");
        let stack = stack_with(&[(CompactValue::int(1), 0), (CompactValue::long(2), 0)]);
        let before = stack.len();
        let mut slots = empty_arg_slots();
        assert!(!read_args_verbatim(&stack, &facts, 2, false, &mut slots));
        assert_eq!(stack.len(), before);
    }

    /// A descriptor with more parameters than `DescriptorFacts` keeps inline
    /// has no tag array to walk, so every door declines it.
    #[test]
    fn an_overflowing_descriptor_is_declined() {
        let facts = facts_of("(IIIIIIIII)V"); // nine
        assert!(facts.param_tags_overflow);
        let stack = stack_with(&[(CompactValue::int(1), 0); 9]);
        let mut slots = empty_arg_slots();
        assert!(!read_args_verbatim(&stack, &facts, 9, false, &mut slots));
    }

    /// A `(J)V` callee entry the doors treat as plain bytecode (the general
    /// path has answered `force_native_cache` with `false`).
    fn long_taking_callee(declaring: ClassId, is_static: bool) -> Arc<CachedBytecodeMethod> {
        let cached = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: declaring,
                class_name: Arc::from("p/DoorCountTarget"),
                method_name: Arc::from("take"),
                method_descriptor: Arc::from("(J)V"),
                source_file: None,
                // `nop; return`: not a body that returns at pc 0, which the
                // special door serves without its tier-up bookkeeping.
                code: Arc::from(vec![0x00u8, 0xB1].as_slice()),
                exception_table: Arc::from(vec![].as_slice()),
                max_stack: 0,
                max_locals: if is_static { 2 } else { 3 },
                num_params: 1,
                is_synchronized: false,
                is_static,
            },
        ));
        let _ = cached.force_native_cache.set(false);
        cached
    }

    fn caller_thread(caller: ClassId) -> JvmThread {
        let mut thread =
            JvmThread::new(crate::threading::jvm_thread::ThreadId(4_848), "door-count");
        thread.frames.push(Frame::new(
            caller,
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        ));
        thread
    }

    /// The virtual door for an `invokevirtual` at constant-pool index 5, pc 0,
    /// of the caller in frame 0, with no quickened-field heap.
    fn virtual_door_at_cp5(
        shared: &SharedVm,
        thread: &mut JvmThread,
    ) -> Option<Result<CachedCallResult, MethodCallFailed>> {
        let mut probe = DoorProbe::NotProbed;
        // The hand-off flag as the dispatch loop hoists it under the door's
        // admission (`nonvirtual_fast_door_on`).
        let handoff_on = !crate::runtime::env_cache::no_nonvirtual_fast_door();
        execute_invokevirtual_fast_door(
            shared, thread, 0, 5, false, handoff_on, None, 0, &mut probe,
        )
    }

    /// Wave 8: a call the virtual door declines because an argument slot needs
    /// coercion must not have been counted by the door first -- the general
    /// path counts it again, and the callee reached its compile threshold at
    /// half the calls. The unmarked `long` is the shape: it declines, and the
    /// callee's counter must not move.
    #[test]
    fn the_virtual_door_does_not_count_a_call_it_declines_for_coercion() {
        use std::sync::atomic::Ordering;
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        // High enough that no other VM in the test process is likely to have
        // defined a `java.util` class at this id (the bitmap is process-wide).
        let receiver_class = ClassId::new(987_001);
        let caller = ClassId::new(987_002);
        let cached = long_taking_callee(receiver_class, false);
        let entry = || CachedInvokeTarget::VirtualBytecode {
            receiver_class_id: receiver_class,
            cached: Arc::clone(&cached),
            gate: RedefineGate::never_stale(),
        };
        let receiver = shared.mem.heap.alloc_object(receiver_class, 0);

        // A marked long: the door serves the call, and counts it once.
        let mut thread = caller_thread(caller);
        thread.invoke_cache.put(caller, 5, false, 0, entry());
        thread.frames[0]
            .stack
            .push(Value::Object(Some(receiver)))
            .expect("an 8-slot stack takes the receiver");
        thread.frames[0]
            .stack
            .push_compact_long(CompactValue::long(7));
        let served = virtual_door_at_cp5(&shared, &mut thread);
        if !matches!(served, Some(Ok(CachedCallResult::FramePushed))) {
            // Declined for a reason of this stripped VM or of the test
            // process (a redefinition, a tier-up switch): nothing to compare.
            return;
        }
        let counted = cached.interp_invocations.load(Ordering::Relaxed);
        if counted == 0 {
            // The door's tier-up block did not run in this process.
            return;
        }
        assert_eq!(counted, 1);

        // An unmarked long: declined, uncounted, stack untouched.
        let mut thread = caller_thread(caller);
        thread.invoke_cache.put(caller, 5, false, 0, entry());
        thread.frames[0]
            .stack
            .push(Value::Object(Some(receiver)))
            .expect("an 8-slot stack takes the receiver");
        thread.frames[0].stack.push_compact(CompactValue::long(7));
        let declined = virtual_door_at_cp5(&shared, &mut thread);
        assert!(declined.is_none(), "an unmarked long needs coercion");
        assert_eq!(
            cached.interp_invocations.load(Ordering::Relaxed),
            1,
            "the declined call is the general path's to count"
        );
        assert_eq!(thread.frames[0].stack.len(), 2, "nothing was popped");
    }

    /// The `invokestatic` door's twin of the test above: its argument read
    /// now runs before `note_invocation_for_tierup`.
    #[test]
    fn the_static_door_does_not_count_a_call_it_declines_for_coercion() {
        use std::sync::atomic::Ordering;
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let declaring = ClassId::new(987_003);
        let caller = ClassId::new(987_004);
        let cached = long_taking_callee(declaring, true);
        let entry = || CachedInvokeTarget::Bytecode {
            cached: Arc::clone(&cached),
            gate: RedefineGate::never_stale(),
        };

        let mut thread = caller_thread(caller);
        thread.invoke_cache.put(caller, 6, false, 0, entry());
        thread.frames[0]
            .stack
            .push_compact_long(CompactValue::long(7));
        let served = execute_invokestatic_fast_door(&shared, &mut thread, 0, 6, 0);
        if !matches!(served, Some(Ok(CachedCallResult::FramePushed))) {
            return;
        }
        let counted = cached.interp_invocations.load(Ordering::Relaxed);
        if counted == 0 {
            // `CRATONVM_DISABLE_JIT` is set for this process: nothing counts.
            return;
        }
        assert_eq!(counted, 1);

        let mut thread = caller_thread(caller);
        thread.invoke_cache.put(caller, 6, false, 0, entry());
        thread.frames[0].stack.push_compact(CompactValue::long(7));
        assert!(
            execute_invokestatic_fast_door(&shared, &mut thread, 0, 6, 0).is_none(),
            "an unmarked long needs coercion"
        );
        assert_eq!(
            cached.interp_invocations.load(Ordering::Relaxed),
            1,
            "the declined call is the general path's to count"
        );
        assert_eq!(thread.frames[0].stack.len(), 1, "nothing was popped");
    }

    /// A gate for a target class redefined once BEFORE the entry was filled:
    /// generation 1, and not stale.
    fn redefined_before_the_fill() -> RedefineGate {
        let gate = RedefineGate::snapshot(Arc::new(std::sync::atomic::AtomicU32::new(1)));
        assert!(!gate.is_stale());
        gate
    }

    /// Interpreter round i1 wave 18, lane L4: the static door serves an entry
    /// whose target was redefined before it was filled — it used to decline
    /// every such entry, and every call at all once any class anywhere had
    /// been redefined. It serves it the way `execute_invokestatic_cached`
    /// does: counted and, since wave 39 (lane L2,
    /// [`REDEFINED_TARGETS_TIER_UP`]), probed for a compiled body and offered
    /// to the tiered manager like any other target (the count is the same
    /// either way).
    #[test]
    fn the_static_door_serves_a_target_redefined_before_the_fill() {
        use std::sync::atomic::Ordering;
        if !DOORS_SURVIVE_REDEFINITION || cratonvm_jit_api::descriptor_facts_disabled() {
            return;
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let declaring = ClassId::new(987_011);
        let caller = ClassId::new(987_012);
        let cached = long_taking_callee(declaring, true);
        let mut thread = caller_thread(caller);
        thread.invoke_cache.put(
            caller,
            6,
            false,
            0,
            CachedInvokeTarget::Bytecode {
                cached: Arc::clone(&cached),
                gate: redefined_before_the_fill(),
            },
        );
        thread.frames[0]
            .stack
            .push_compact_long(CompactValue::long(7));
        let served = execute_invokestatic_fast_door(&shared, &mut thread, 0, 6, 0);
        assert!(
            matches!(served, Some(Ok(CachedCallResult::FramePushed))),
            "a redefined target is served like any other"
        );
        let counted = cached.interp_invocations.load(Ordering::Relaxed);
        if crate::runtime::env_cache::disable_jit() {
            assert_eq!(counted, 0, "nothing counts under --nojit");
        } else {
            assert_eq!(
                counted, 1,
                "the general path counts a redefined target's calls"
            );
        }
    }

    /// The special door's twin: a `Bytecode` entry for a target redefined
    /// before the fill is served and, since wave 39 (lane L2,
    /// [`REDEFINED_TARGETS_TIER_UP`]), counted once like any other `Bytecode`
    /// entry (`the_special_door_counts_a_bytecode_entry_once`); the general
    /// `Bytecode` arm probes and offers it too. Under the kill switch it is
    /// served uncounted, as before.
    #[test]
    fn the_special_door_serves_a_target_redefined_before_the_fill() {
        if !DOORS_SURVIVE_REDEFINITION || cratonvm_jit_api::descriptor_facts_disabled() {
            return;
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let declaring = ClassId::new(987_013);
        let caller = ClassId::new(987_014);
        let cached = long_taking_callee(declaring, false);
        let receiver = shared.mem.heap.alloc_object(declaring, 0);
        let mut thread = caller_thread(caller);
        thread.invoke_cache.put(
            caller,
            7,
            true,
            0,
            CachedInvokeTarget::Bytecode {
                cached: Arc::clone(&cached),
                gate: redefined_before_the_fill(),
            },
        );
        thread.frames[0]
            .stack
            .push(Value::Object(Some(receiver)))
            .expect("an 8-slot stack takes the receiver");
        thread.frames[0]
            .stack
            .push_compact_long(CompactValue::long(7));
        let served = execute_nonvirtual_fast_door(
            &shared,
            &mut thread,
            0,
            7,
            true,
            0,
            crate::runtime::interpreter::field_fast::fast_field_zgc(&shared),
            &mut DoorProbe::NotProbed,
        );
        assert!(
            matches!(served, Some(Ok(CachedCallResult::FramePushed))),
            "a redefined target is served like any other"
        );
        let expected = u32::from(
            SPECIAL_DOOR_TIERS_UP
                && REDEFINED_TARGETS_TIER_UP
                && !crate::runtime::env_cache::disable_jit(),
        );
        assert_eq!(
            cached
                .interp_invocations
                .load(std::sync::atomic::Ordering::Relaxed),
            expected,
            "a redefined target's call is counted like any other `Bytecode` entry's"
        );
    }

    /// The special door at constant-pool index 7, pc 0, of the caller in
    /// frame 0, for a `(J)V` callee whose receiver and marked `long` argument
    /// are pushed first.
    fn special_door_call(
        shared: &SharedVm,
        receiver: ObjectRef,
        entry: CachedInvokeTarget,
        caller: ClassId,
    ) -> (
        JvmThread,
        Option<Result<CachedCallResult, MethodCallFailed>>,
    ) {
        let mut thread = caller_thread(caller);
        thread.invoke_cache.put(caller, 7, true, 0, entry);
        thread.frames[0]
            .stack
            .push(Value::Object(Some(receiver)))
            .expect("an 8-slot stack takes the receiver");
        thread.frames[0]
            .stack
            .push_compact_long(CompactValue::long(7));
        let served = execute_nonvirtual_fast_door(
            shared,
            &mut thread,
            0,
            7,
            true,
            0,
            crate::runtime::interpreter::field_fast::fast_field_zgc(&shared),
            &mut DoorProbe::NotProbed,
        );
        (thread, served)
    }

    /// Interpreter round i1 wave 19, lane L4: the special door counts a
    /// `Bytecode` entry's call once, as the general `Bytecode` arm does since
    /// `600311cbe` — it used to count nothing, so a hot private method, super
    /// call or constructor it served never tiered up through this route.
    #[test]
    fn the_special_door_counts_a_bytecode_entry_once() {
        use std::sync::atomic::Ordering;
        if !SPECIAL_DOOR_TIERS_UP || cratonvm_jit_api::descriptor_facts_disabled() {
            return;
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let declaring = ClassId::new(987_021);
        let caller = ClassId::new(987_022);
        let cached = long_taking_callee(declaring, false);
        let receiver = shared.mem.heap.alloc_object(declaring, 0);
        let entry = CachedInvokeTarget::Bytecode {
            cached: Arc::clone(&cached),
            gate: RedefineGate::never_stale(),
        };
        let (_thread, served) = special_door_call(&shared, receiver, entry, caller);
        if !matches!(served, Some(Ok(CachedCallResult::FramePushed))) {
            // Declined for a reason of this stripped VM or of the test
            // process (a loader-split memo, a tier-up switch).
            return;
        }
        let expected = u32::from(!crate::runtime::env_cache::disable_jit());
        assert_eq!(cached.interp_invocations.load(Ordering::Relaxed), expected);
    }

    /// The general arm's tier-up chain refuses `is_special` for a
    /// `VirtualBytecode` entry, so the door leaves that entry uncounted.
    #[test]
    fn the_special_door_does_not_count_a_virtual_bytecode_entry() {
        use std::sync::atomic::Ordering;
        if cratonvm_jit_api::descriptor_facts_disabled() {
            return;
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let declaring = ClassId::new(987_023);
        let caller = ClassId::new(987_024);
        let cached = long_taking_callee(declaring, false);
        let receiver = shared.mem.heap.alloc_object(declaring, 0);
        let entry = CachedInvokeTarget::VirtualBytecode {
            receiver_class_id: declaring,
            cached: Arc::clone(&cached),
            gate: RedefineGate::never_stale(),
        };
        let (_thread, served) = special_door_call(&shared, receiver, entry, caller);
        if !matches!(served, Some(Ok(CachedCallResult::FramePushed))) {
            return;
        }
        assert_eq!(cached.interp_invocations.load(Ordering::Relaxed), 0);
    }

    /// A `Bytecode` callee with a published compiled body is declined, with
    /// the operand stack untouched and the call uncounted: the general arm
    /// enters the body (`execute_jit_call_decoded`). The door used to run it
    /// interpreted.
    #[test]
    fn the_special_door_declines_a_callee_with_a_compiled_body() {
        use std::sync::atomic::Ordering;
        if !SPECIAL_DOOR_TIERS_UP
            || crate::runtime::env_cache::disable_jit()
            || cratonvm_jit_api::descriptor_facts_disabled()
        {
            return;
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let declaring = ClassId::new(987_025);
        let caller = ClassId::new(987_026);
        let cached = long_taking_callee(declaring, false);
        let receiver = shared.mem.heap.alloc_object(declaring, 0);
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&[0xC3]); // ret
        shared.jit.jit_cache.write().put(
            Arc::clone(&cached.class_name),
            Arc::clone(&cached.method_name),
            Arc::clone(&cached.method_descriptor),
            declaring,
            cratonvm_jit::CompiledMethod::new(buf),
        );
        let entry = CachedInvokeTarget::Bytecode {
            cached: Arc::clone(&cached),
            gate: RedefineGate::never_stale(),
        };
        let (thread, served) = special_door_call(&shared, receiver, entry, caller);
        assert!(served.is_none(), "the general arm enters the compiled body");
        assert_eq!(thread.frames.len(), 1, "no frame was pushed");
        assert_eq!(thread.frames[0].stack.len(), 2, "nothing was popped");
        assert_eq!(cached.interp_invocations.load(Ordering::Relaxed), 0);
    }

    /// A body that returns at pc 0 (`Object.<init>`'s shape) is served
    /// interpreted and uncounted even with a compiled body published: the
    /// door keeps the wave-18 behaviour for it.
    #[test]
    fn the_special_door_serves_an_empty_body_without_tier_up() {
        use std::sync::atomic::Ordering;
        if cratonvm_jit_api::descriptor_facts_disabled() {
            return;
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let declaring = ClassId::new(987_029);
        let caller = ClassId::new(987_030);
        let cached = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: declaring,
                class_name: Arc::from("p/DoorEmptyTarget"),
                method_name: Arc::from("<init>"),
                method_descriptor: Arc::from("(J)V"),
                source_file: None,
                code: Arc::from(vec![0xB1u8, 0x00, 0x00].as_slice()),
                exception_table: Arc::from(vec![].as_slice()),
                max_stack: 0,
                max_locals: 3,
                num_params: 1,
                is_synchronized: false,
                is_static: false,
            },
        ));
        let _ = cached.force_native_cache.set(false);
        assert!(body_returns_at_entry(&cached));
        let receiver = shared.mem.heap.alloc_object(declaring, 0);
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&[0xC3]); // ret
        shared.jit.jit_cache.write().put(
            Arc::clone(&cached.class_name),
            Arc::clone(&cached.method_name),
            Arc::clone(&cached.method_descriptor),
            declaring,
            cratonvm_jit::CompiledMethod::new(buf),
        );
        let entry = CachedInvokeTarget::Bytecode {
            cached: Arc::clone(&cached),
            gate: RedefineGate::never_stale(),
        };
        let (_thread, served) = special_door_call(&shared, receiver, entry, caller);
        // Served with a frame, or (wave 20, `EMPTY_BODY_ELISION`) without one.
        if !matches!(
            served,
            Some(Ok(CachedCallResult::FramePushed | CachedCallResult::Handled))
        ) {
            return;
        }
        assert_eq!(cached.interp_invocations.load(Ordering::Relaxed), 0);
    }

    /// The virtual door's hand-off of a private `invokevirtual` (a `Bytecode`
    /// entry) counts the call as the special door does.
    #[test]
    fn the_virtual_door_hand_off_counts_a_private_callee() {
        use std::sync::atomic::Ordering;
        if !SPECIAL_DOOR_TIERS_UP
            || crate::runtime::env_cache::no_nonvirtual_fast_door()
            || cratonvm_jit_api::descriptor_facts_disabled()
        {
            return;
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let declaring = ClassId::new(987_027);
        let caller = ClassId::new(987_028);
        let cached = long_taking_callee(declaring, false);
        let receiver = shared.mem.heap.alloc_object(declaring, 0);
        let mut thread = caller_thread(caller);
        thread.invoke_cache.put(
            caller,
            5,
            false,
            0,
            CachedInvokeTarget::Bytecode {
                cached: Arc::clone(&cached),
                gate: RedefineGate::never_stale(),
            },
        );
        thread.frames[0]
            .stack
            .push(Value::Object(Some(receiver)))
            .expect("an 8-slot stack takes the receiver");
        thread.frames[0]
            .stack
            .push_compact_long(CompactValue::long(7));
        let served = virtual_door_at_cp5(&shared, &mut thread);
        if !matches!(served, Some(Ok(CachedCallResult::FramePushed))) {
            return;
        }
        let expected = u32::from(!crate::runtime::env_cache::disable_jit());
        assert_eq!(cached.interp_invocations.load(Ordering::Relaxed), expected);
    }

    /// The virtual door's twin, which reads the generation itself: a
    /// `VirtualBytecode` entry for a receiver's method redefined before the
    /// fill is served and, since wave 40 (lane L2,
    /// [`REDEFINED_TARGETS_TIER_UP`]), counted exactly as the same entry of a
    /// class never redefined is (the door's tier-up block may not run in a
    /// stripped VM or under a tier-up switch; then neither is counted). Under
    /// the kill switch it is served uncounted, as before.
    #[test]
    fn the_virtual_door_serves_a_target_redefined_before_the_fill() {
        use std::sync::atomic::Ordering;
        if !DOORS_SURVIVE_REDEFINITION || cratonvm_jit_api::descriptor_facts_disabled() {
            return;
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let receiver_class = ClassId::new(987_015);
        let caller = ClassId::new(987_016);
        // One call at the door with `gate`; the served result and the callee's
        // count.
        let call = |gate: RedefineGate| {
            let cached = long_taking_callee(receiver_class, false);
            let receiver = shared.mem.heap.alloc_object(receiver_class, 0);
            let mut thread = caller_thread(caller);
            thread.invoke_cache.put(
                caller,
                5,
                false,
                0,
                CachedInvokeTarget::VirtualBytecode {
                    receiver_class_id: receiver_class,
                    cached: Arc::clone(&cached),
                    gate,
                },
            );
            thread.frames[0]
                .stack
                .push(Value::Object(Some(receiver)))
                .expect("an 8-slot stack takes the receiver");
            thread.frames[0]
                .stack
                .push_compact_long(CompactValue::long(7));
            let served = virtual_door_at_cp5(&shared, &mut thread);
            (
                matches!(served, Some(Ok(CachedCallResult::FramePushed))),
                cached.interp_invocations.load(Ordering::Relaxed),
            )
        };
        let (plain_served, plain_counted) = call(RedefineGate::never_stale());
        let (served, counted) = call(redefined_before_the_fill());
        if !plain_served {
            // Declined for a reason of this stripped VM or of the test
            // process: nothing to compare.
            return;
        }
        assert!(served, "a redefined target is served like any other");
        if REDEFINED_TARGETS_TIER_UP {
            assert_eq!(
                counted, plain_counted,
                "a redefined target's call is counted like any other"
            );
        } else {
            assert_eq!(counted, 0);
        }
    }

    /// A `(J)V` instance method whose body is a lone `return`, in the padded
    /// shape the caches hold.
    fn empty_callee(declaring: ClassId, is_synchronized: bool) -> Arc<CachedBytecodeMethod> {
        let cached = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: declaring,
                class_name: Arc::from("p/DoorEmptyTarget"),
                method_name: Arc::from("<init>"),
                method_descriptor: Arc::from("(J)V"),
                source_file: None,
                code: Arc::from(vec![0xB1u8, 0x00, 0x00].as_slice()),
                exception_table: Arc::from(vec![].as_slice()),
                max_stack: 0,
                max_locals: 3,
                num_params: 1,
                is_synchronized,
                is_static: false,
            },
        ));
        let _ = cached.force_native_cache.set(false);
        cached
    }

    /// Interpreter round i1 wave 20, lane L3: the special door answers a call
    /// of an empty body by popping its arguments — no frame is pushed, the
    /// call is uncounted, and the caller continues (`Handled`).
    #[test]
    fn the_special_door_calls_an_empty_body_without_a_frame() {
        use std::sync::atomic::Ordering;
        if cratonvm_jit_api::descriptor_facts_disabled() {
            return;
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let declaring = ClassId::new(987_033);
        let caller = ClassId::new(987_034);
        let cached = empty_callee(declaring, false);
        let receiver = shared.mem.heap.alloc_object(declaring, 0);
        let entry = CachedInvokeTarget::Bytecode {
            cached: Arc::clone(&cached),
            gate: RedefineGate::never_stale(),
        };
        let (thread, served) = special_door_call(&shared, receiver, entry, caller);
        if served.is_none() {
            // Declined for a reason of this stripped VM or of the test
            // process (a loader-split memo): the general path owns the call.
            return;
        }
        if empty_body_elidable(&shared, &cached) {
            assert!(matches!(served, Some(Ok(CachedCallResult::Handled))));
            assert_eq!(thread.frames.len(), 1, "no frame was pushed");
            assert_eq!(
                thread.frames[0].stack.len(),
                0,
                "the receiver and the argument were popped"
            );
        } else {
            assert!(matches!(served, Some(Ok(CachedCallResult::FramePushed))));
            assert_eq!(thread.frames.len(), 2);
        }
        assert_eq!(cached.interp_invocations.load(Ordering::Relaxed), 0);
    }

    /// A `synchronized` empty body keeps its frame: the monitor enter and
    /// exit are what the call does. A plain one is elidable in a VM with no
    /// agent and no debugger.
    #[test]
    fn only_an_unobserved_unsynchronized_empty_body_is_elidable() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let plain = empty_callee(ClassId::new(987_035), false);
        let synchronized = empty_callee(ClassId::new(987_036), true);
        assert!(body_returns_at_entry(&plain) && body_returns_at_entry(&synchronized));
        assert!(!empty_body_elidable(&shared, &synchronized));
        assert_eq!(empty_body_elidable(&shared, &plain), EMPTY_BODY_ELISION);
    }

    /// A `(J)V` STATIC method whose body is a lone `return`.
    fn empty_static_callee(declaring: ClassId) -> Arc<CachedBytecodeMethod> {
        let cached = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: declaring,
                class_name: Arc::from("p/DoorEmptyStatic"),
                method_name: Arc::from("hook"),
                method_descriptor: Arc::from("(J)V"),
                source_file: None,
                code: Arc::from(vec![0xB1u8, 0x00, 0x00].as_slice()),
                exception_table: Arc::from(vec![].as_slice()),
                max_stack: 0,
                max_locals: 2,
                num_params: 1,
                is_synchronized: false,
                is_static: true,
            },
        ));
        let _ = cached.force_native_cache.set(false);
        cached
    }

    /// Interpreter round i1 wave 22, lane L4 (`STATIC_DOOR_ELIDES_EMPTY_BODY`):
    /// the static door answers an empty static body by popping its argument,
    /// uncounted, with no frame.
    #[test]
    fn the_static_door_calls_an_empty_body_without_a_frame() {
        use std::sync::atomic::Ordering;
        if cratonvm_jit_api::descriptor_facts_disabled() {
            return;
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let declaring = ClassId::new(987_045);
        let caller = ClassId::new(987_046);
        let cached = empty_static_callee(declaring);
        let mut thread = caller_thread(caller);
        thread.invoke_cache.put(
            caller,
            6,
            false,
            0,
            CachedInvokeTarget::Bytecode {
                cached: Arc::clone(&cached),
                gate: RedefineGate::never_stale(),
            },
        );
        thread.frames[0]
            .stack
            .push_compact_long(CompactValue::long(7));
        let served = execute_invokestatic_fast_door(&shared, &mut thread, 0, 6, 0);
        if served.is_none() {
            // Declined for a reason of this stripped VM (a loader-split
            // memo): the general path owns the call.
            return;
        }
        if STATIC_DOOR_ELIDES_EMPTY_BODY && empty_body_elidable(&shared, &cached) {
            assert!(matches!(served, Some(Ok(CachedCallResult::Handled))));
            assert_eq!(thread.frames.len(), 1, "no frame was pushed");
            assert_eq!(thread.frames[0].stack.len(), 0, "the argument was popped");
            assert_eq!(cached.interp_invocations.load(Ordering::Relaxed), 0);
        } else {
            assert!(matches!(served, Some(Ok(CachedCallResult::FramePushed))));
            assert_eq!(thread.frames.len(), 2);
        }
    }

    /// Interpreter round i1 wave 22, lane L4 (`VIRTUAL_DOOR_ELIDES_EMPTY_BODY`):
    /// the virtual door answers a monomorphic call of an empty body by
    /// popping the receiver and the argument, uncounted, with no frame.
    #[test]
    fn the_virtual_door_calls_an_empty_body_without_a_frame() {
        use std::sync::atomic::Ordering;
        if cratonvm_jit_api::descriptor_facts_disabled() {
            return;
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let receiver_class = ClassId::new(987_047);
        let caller = ClassId::new(987_048);
        let cached = empty_callee(receiver_class, false);
        let receiver = shared.mem.heap.alloc_object(receiver_class, 0);
        let mut thread = caller_thread(caller);
        thread.invoke_cache.put(
            caller,
            5,
            false,
            0,
            CachedInvokeTarget::VirtualBytecode {
                receiver_class_id: receiver_class,
                cached: Arc::clone(&cached),
                gate: RedefineGate::never_stale(),
            },
        );
        thread.frames[0]
            .stack
            .push(Value::Object(Some(receiver)))
            .expect("an 8-slot stack takes the receiver");
        thread.frames[0]
            .stack
            .push_compact_long(CompactValue::long(7));
        let served = virtual_door_at_cp5(&shared, &mut thread);
        if served.is_none() {
            // Declined for a reason of this stripped VM or of the test
            // process: the general path owns the call.
            return;
        }
        if VIRTUAL_DOOR_ELIDES_EMPTY_BODY && empty_body_elidable(&shared, &cached) {
            assert!(matches!(served, Some(Ok(CachedCallResult::Handled))));
            assert_eq!(thread.frames.len(), 1, "no frame was pushed");
            assert_eq!(
                thread.frames[0].stack.len(),
                0,
                "the receiver and the argument were popped"
            );
            assert_eq!(cached.interp_invocations.load(Ordering::Relaxed), 0);
        } else {
            assert!(matches!(served, Some(Ok(CachedCallResult::FramePushed))));
            assert_eq!(thread.frames.len(), 2);
        }
    }

    /// One more `int` than the receiver leaves room for in the compiled-code
    /// ABI, so the doors decline the call (see [`nine_argument_callee`]).
    const WIDE_JAVA_PARAMS: usize = crate::runtime::interpreter::jit_bridge::JIT_ABI_MAX_JAVA_ARGS;

    /// An instance method taking [`WIDE_JAVA_PARAMS`] `int`s: with the receiver
    /// that is one argument more than the compiled-code ABI passes
    /// (`JIT_ABI_MAX_JAVA_ARGS`, sixteen on x86-64 since round 12 wave 2), so `execute_jit_call_decoded` declines it
    /// before entering any body and the `Bytecode` arm pushes the interpreted
    /// frame. That lets a test drive the arm's compiled branch with a dummy
    /// body that never runs.
    fn nine_argument_callee(declaring: ClassId) -> Arc<CachedBytecodeMethod> {
        let cached = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: declaring,
                class_name: Arc::from("p/WideSpecialTarget"),
                method_name: Arc::from("wide"),
                method_descriptor: Arc::from(
                    format!("({})V", "I".repeat(WIDE_JAVA_PARAMS)).as_str(),
                ),
                source_file: None,
                code: Arc::from(vec![0x00u8, 0xB1].as_slice()),
                exception_table: Arc::from(vec![].as_slice()),
                max_stack: 0,
                max_locals: (WIDE_JAVA_PARAMS + 1) as _,
                num_params: WIDE_JAVA_PARAMS as _,
                is_synchronized: false,
                is_static: false,
            },
        ));
        let _ = cached.force_native_cache.set(false);
        cached
    }

    /// A caller frame with the receiver and [`WIDE_JAVA_PARAMS`] `int`s on its stack.
    fn nine_argument_caller(caller: ClassId, receiver: ObjectRef) -> JvmThread {
        let mut thread =
            JvmThread::new(crate::threading::jvm_thread::ThreadId(4_849), "special-jit");
        thread.frames.push(Frame::new(
            caller,
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            32,
            4,
            &[],
        ));
        thread.frames[0]
            .stack
            .push(Value::Object(Some(receiver)))
            .expect("a 32-slot stack takes the receiver");
        for i in 0..WIDE_JAVA_PARAMS as i32 {
            thread.frames[0]
                .stack
                .push_int(i)
                .expect("a 32-slot stack takes the ints");
        }
        thread
    }

    /// A `ret` stub to publish as a compiled body; never entered by these
    /// tests (see [`nine_argument_callee`]).
    fn dummy_body() -> cratonvm_jit::CompiledMethod {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&[0xC3]); // ret
        cratonvm_jit::CompiledMethod::new(buf)
    }

    /// Interpreter round i1 wave 20, lane L3 (`NONVIRTUAL_ARM_UPGRADES_TO_JIT`):
    /// a `Jit` entry under a special key is served by the general dispatcher's
    /// `Bytecode` arm with the entry's body in hand. The `Jit` arm used to
    /// answer `CacheMiss`, sending every such call to `execute_invoke`. The
    /// special door declines the entry on its kind, the stack untouched.
    #[test]
    fn a_special_jit_entry_is_served_by_the_bytecode_arm() {
        if !NONVIRTUAL_ARM_UPGRADES_TO_JIT {
            return;
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let declaring = ClassId::new(987_037);
        let caller = ClassId::new(987_038);
        let cached = nine_argument_callee(declaring);
        let receiver = shared.mem.heap.alloc_object(declaring, 0);
        let compiled = cratonvm_jit::RetainedCode::new(Arc::new(dummy_body()));
        let needs_heap = compiled.needs_heap();
        let mut thread = nine_argument_caller(caller, receiver);
        thread.invoke_cache.put(
            caller,
            7,
            true,
            0,
            CachedInvokeTarget::Jit {
                compiled,
                num_params: WIDE_JAVA_PARAMS as _,
                return_type: b'V',
                needs_heap,
                cached: Arc::clone(&cached),
                gate: RedefineGate::never_stale(),
                supersede_epoch: shared.jit.supersede_gate(),
            },
        );
        assert!(
            execute_nonvirtual_fast_door(
                &shared,
                &mut thread,
                0,
                7,
                true,
                0,
                crate::runtime::interpreter::field_fast::fast_field_zgc(&shared),
                &mut DoorProbe::NotProbed
            )
            .is_none(),
            "the door declines a compiled entry on its kind"
        );
        assert_eq!(
            thread.frames[0].stack.len(),
            WIDE_JAVA_PARAMS + 1,
            "nothing was popped"
        );
        let outcome = execute_invokevirtual_cached(&shared, &mut thread, 0, 7, 0, true, false);
        assert!(
            matches!(outcome, Ok(CachedCallResult::FramePushed)),
            "the arm runs the call: {outcome:?}"
        );
        assert_eq!(thread.frames.len(), 2, "the declined body ran interpreted");
        assert_eq!(thread.frames[0].stack.len(), 0);
    }

    /// Interpreter round i1 wave 22, lane L4 (`SPECIAL_DOOR_HANDS_OVER_ITS_PROBE`):
    /// the special door hands the `Jit` entry it declined to the general
    /// dispatcher, which serves the call from it without probing the key.
    /// The key is evicted between the two, so a second probe would miss.
    #[test]
    fn the_special_door_hands_a_declined_entry_to_the_general_dispatcher() {
        if !NONVIRTUAL_ARM_UPGRADES_TO_JIT || !SPECIAL_DOOR_HANDS_OVER_ITS_PROBE {
            return;
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let declaring = ClassId::new(987_043);
        let caller = ClassId::new(987_044);
        let cached = nine_argument_callee(declaring);
        let receiver = shared.mem.heap.alloc_object(declaring, 0);
        let compiled = cratonvm_jit::RetainedCode::new(Arc::new(dummy_body()));
        let needs_heap = compiled.needs_heap();
        let mut thread = nine_argument_caller(caller, receiver);
        thread.invoke_cache.put(
            caller,
            7,
            true,
            0,
            CachedInvokeTarget::Jit {
                compiled,
                num_params: WIDE_JAVA_PARAMS as _,
                return_type: b'V',
                needs_heap,
                cached: Arc::clone(&cached),
                gate: RedefineGate::never_stale(),
                supersede_epoch: shared.jit.supersede_gate(),
            },
        );
        let mut probe = DoorProbe::NotProbed;
        assert!(
            execute_nonvirtual_fast_door(&shared, &mut thread, 0, 7, true, 0, None, &mut probe)
                .is_none(),
            "the door declines a compiled entry on its kind"
        );
        assert!(
            matches!(probe, DoorProbe::Found(CachedInvokeTarget::Jit { .. })),
            "the declined entry is handed over"
        );
        thread.invoke_cache.evict(caller, 7, true);
        let outcome =
            execute_invokevirtual_cached_probed(&shared, &mut thread, 0, 7, 0, true, false, probe);
        assert!(
            matches!(outcome, Ok(CachedCallResult::FramePushed)),
            "served from the handed-over entry: {outcome:?}"
        );

        // A miss is handed over as a miss: the general dispatcher answers
        // `CacheMiss` without probing.
        let mut thread = nine_argument_caller(caller, receiver);
        let mut probe = DoorProbe::NotProbed;
        assert!(execute_nonvirtual_fast_door(
            &shared,
            &mut thread,
            0,
            7,
            true,
            0,
            None,
            &mut probe
        )
        .is_none());
        assert!(matches!(probe, DoorProbe::Miss));
        assert!(matches!(
            execute_invokevirtual_cached_probed(&shared, &mut thread, 0, 7, 0, true, false, probe),
            Ok(CachedCallResult::CacheMiss)
        ));
        assert_eq!(
            thread.frames[0].stack.len(),
            WIDE_JAVA_PARAMS + 1,
            "nothing was popped"
        );
    }

    /// The `Bytecode` arm stores a compiled body it found for a special site
    /// as the site's `Jit` entry, as the static arm does, so neither the door
    /// nor the arm probes `JitCache` for it again.
    #[test]
    fn the_bytecode_arm_upgrades_a_special_site_whose_body_it_found() {
        if !NONVIRTUAL_ARM_UPGRADES_TO_JIT || crate::runtime::env_cache::disable_jit() {
            return;
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let declaring = ClassId::new(987_039);
        let caller = ClassId::new(987_040);
        let cached = nine_argument_callee(declaring);
        let receiver = shared.mem.heap.alloc_object(declaring, 0);
        shared.jit.jit_cache.write().put(
            Arc::clone(&cached.class_name),
            Arc::clone(&cached.method_name),
            Arc::clone(&cached.method_descriptor),
            declaring,
            dummy_body(),
        );
        let mut thread = nine_argument_caller(caller, receiver);
        thread.invoke_cache.put(
            caller,
            7,
            true,
            0,
            CachedInvokeTarget::Bytecode {
                cached: Arc::clone(&cached),
                gate: RedefineGate::never_stale(),
            },
        );
        let outcome = execute_invokevirtual_cached(&shared, &mut thread, 0, 7, 0, true, false);
        assert!(
            matches!(outcome, Ok(CachedCallResult::FramePushed)),
            "the arm runs the call: {outcome:?}"
        );
        assert!(
            matches!(
                thread.invoke_cache.get(caller, 7, true, 0),
                Some(CachedInvokeTarget::Jit { .. })
            ),
            "the site now holds the compiled entry"
        );
    }

    /// A private `invokevirtual` and an `invokestatic` of one `Methodref`
    /// share a cache key, so the upgrade above can leave an INSTANCE `Jit`
    /// entry where an `invokestatic` looks. The static arm refuses it before
    /// popping (JVMS §6.5: the slow path raises the
    /// `IncompatibleClassChangeError`), as its `Bytecode` arm refuses an
    /// instance `Bytecode` entry.
    #[test]
    fn invokestatic_refuses_an_instance_jit_entry_before_popping() {
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let declaring = ClassId::new(987_041);
        let caller = ClassId::new(987_042);
        let cached = nine_argument_callee(declaring);
        let compiled = cratonvm_jit::RetainedCode::new(Arc::new(dummy_body()));
        let needs_heap = compiled.needs_heap();
        let mut thread = caller_thread(caller);
        thread.invoke_cache.put(
            caller,
            5,
            false,
            0,
            CachedInvokeTarget::Jit {
                compiled,
                num_params: WIDE_JAVA_PARAMS as _,
                return_type: b'V',
                needs_heap,
                cached: Arc::clone(&cached),
                gate: RedefineGate::never_stale(),
                supersede_epoch: shared.jit.supersede_gate(),
            },
        );
        thread.frames[0]
            .stack
            .push_int(3)
            .expect("an 8-slot stack takes one int");
        let outcome = execute_invokestatic_cached(&shared, &mut thread, 0, 5, 0);
        assert!(
            matches!(outcome, Ok(CachedCallResult::CacheMiss)),
            "declined: {outcome:?}"
        );
        assert_eq!(thread.frames[0].stack.len(), 1, "nothing was popped");
        assert!(
            thread.invoke_cache.get(caller, 5, false, 0).is_none(),
            "the wrong-kind entry is evicted"
        );
    }

    // ── Wave 25: the trivial constructor (`TRIVIAL_CTOR_ELISION`) ──────────

    /// A `<init>()V` entry with `code` in the padded shape the caches hold.
    /// (Until wave 28 this helper said so and built the UNPADDED body, which
    /// is why the tests below passed while the real build never elided.)
    fn ctor_entry(declaring: ClassId, code: &[u8]) -> Arc<CachedBytecodeMethod> {
        let cached = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: declaring,
                class_name: Arc::from("w25/TrivialCtor"),
                method_name: Arc::from("<init>"),
                method_descriptor: Arc::from("()V"),
                source_file: None,
                code: crate::runtime::frame::padded_bytecode(code),
                exception_table: Arc::from(vec![].as_slice()),
                max_stack: 1,
                max_locals: 1,
                num_params: 0,
                is_synchronized: false,
                is_static: false,
            },
        ));
        let _ = cached.force_native_cache.set(false);
        cached
    }

    /// The shape test is the JIT's byte test plus the name and descriptor:
    /// javac's default constructor is exactly `2a b7 hi lo b1`.
    #[test]
    fn only_the_default_constructor_bytes_have_the_trivial_shape() {
        let declaring = ClassId::new(987_101);
        let trivial: [u8; 5] = [0x2a, 0xb7, 0x00, 0x05, 0xb1];
        assert!(body_is_trivial_ctor_shape(&ctor_entry(declaring, &trivial)));
        // A field store before the super call, a longer body, another opcode.
        assert!(!body_is_trivial_ctor_shape(&ctor_entry(
            declaring,
            &[0x2a, 0xb7, 0x00, 0x05, 0x00, 0xb1]
        )));
        assert!(!body_is_trivial_ctor_shape(&ctor_entry(
            declaring,
            &[0x2a, 0xb6, 0x00, 0x05, 0xb1]
        )));
        assert!(!body_is_trivial_ctor_shape(&ctor_entry(
            declaring,
            &[0x2b, 0xb7, 0x00, 0x05, 0xb1]
        )));
        // The same bytes under another name or descriptor are not a
        // constructor the rule speaks about.
        let mut other = CachedBytecodeMethod::clone(&ctor_entry(declaring, &trivial));
        other.method_name = Arc::from("init");
        assert!(!body_is_trivial_ctor_shape(&other));
        let mut other = CachedBytecodeMethod::clone(&ctor_entry(declaring, &trivial));
        other.method_descriptor = Arc::from("(I)V");
        assert!(!body_is_trivial_ctor_shape(&other));
    }

    /// Wave 28 regression test: the shape is read on the body the invoke
    /// caches really hold -- `frame::padded_bytecode_for_method`, as
    /// `populate_invoke_cache` builds it, two zero bytes longer than the class
    /// file's -- and not on the class-file length. The wave-25 test compared
    /// the padded length with 5, so on the real build no constructor ever had
    /// the shape and the elision never engaged.
    #[test]
    fn the_trivial_shape_is_read_on_the_padded_body_the_caches_hold() {
        let declaring = ClassId::new(987_105);
        let class_file_body: [u8; 5] = [0x2a, 0xb7, 0x00, 0x05, 0xb1];
        let cached = CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: declaring,
            class_name: Arc::from("w28/PaddedCtor"),
            method_name: Arc::from("<init>"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            code: crate::runtime::frame::padded_bytecode_for_method(
                declaring,
                "<init>",
                "()V",
                &class_file_body,
                &[],
            ),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 1,
            max_locals: 1,
            num_params: 0,
            is_synchronized: false,
            is_static: false,
        });
        assert_eq!(
            cached.code.len(),
            class_file_body.len() + 2,
            "the cache pads"
        );
        assert!(body_is_trivial_ctor_shape(&cached));
    }

    /// A trivial-SHAPED constructor whose declaring class the class manager
    /// cannot show (no constant pool to read) keeps its frame: the verdict
    /// never rests on the bytes alone.
    #[test]
    fn the_special_door_keeps_the_frame_of_a_constructor_it_cannot_prove_trivial() {
        if cratonvm_jit_api::descriptor_facts_disabled() {
            return;
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let declaring = ClassId::new(987_102);
        let caller = ClassId::new(987_103);
        let cached = ctor_entry(declaring, &[0x2a, 0xb7, 0x00, 0x05, 0xb1]);
        assert!(
            !trivial_object_ctor(&shared, &cached),
            "no class, no verdict"
        );
        let receiver = shared.mem.heap.alloc_object(declaring, 0);
        let mut thread = caller_thread(caller);
        thread.invoke_cache.put(
            caller,
            7,
            true,
            0,
            CachedInvokeTarget::Bytecode {
                cached: Arc::clone(&cached),
                gate: RedefineGate::never_stale(),
            },
        );
        thread.frames[0]
            .stack
            .push(Value::Object(Some(receiver)))
            .expect("an 8-slot stack takes the receiver");
        let served = execute_nonvirtual_fast_door(
            &shared,
            &mut thread,
            0,
            7,
            true,
            0,
            crate::runtime::interpreter::field_fast::fast_field_zgc(&shared),
            &mut DoorProbe::NotProbed,
        );
        if served.is_none() {
            // Declined for a reason of this stripped VM (a loader-split memo).
            return;
        }
        assert!(matches!(served, Some(Ok(CachedCallResult::FramePushed))));
        assert_eq!(thread.frames.len(), 2, "the constructor's frame was pushed");
    }

    /// Class-file bytes for `this_name extends super_name` with one method,
    /// `<init>()V` = `aload_0; invokespecial ctor_owner.<init>()V; return`.
    /// Constant pool: #1 Class this, #3 Class super, #5 Methodref #6.#8,
    /// #6 Class ctor_owner, #8 NameAndType <init> ()V, #11 "Code".
    fn w25_class_bytes(this_name: &str, super_name: &str, ctor_owner: &str) -> Vec<u8> {
        fn utf8(b: &mut Vec<u8>, s: &str) {
            b.push(1);
            b.extend_from_slice(&u16::try_from(s.len()).unwrap().to_be_bytes());
            b.extend_from_slice(s.as_bytes());
        }
        let mut b: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x34];
        b.extend_from_slice(&12u16.to_be_bytes()); // constant_pool_count
        b.extend_from_slice(&[7, 0x00, 0x02]); // #1 Class #2
        utf8(&mut b, this_name); // #2
        b.extend_from_slice(&[7, 0x00, 0x04]); // #3 Class #4
        utf8(&mut b, super_name); // #4
        b.extend_from_slice(&[10, 0x00, 0x06, 0x00, 0x08]); // #5 Methodref #6.#8
        b.extend_from_slice(&[7, 0x00, 0x07]); // #6 Class #7
        utf8(&mut b, ctor_owner); // #7
        b.extend_from_slice(&[12, 0x00, 0x09, 0x00, 0x0A]); // #8 NameAndType #9:#10
        utf8(&mut b, "<init>"); // #9
        utf8(&mut b, "()V"); // #10
        utf8(&mut b, "Code"); // #11
                              // public super, this #1, super #3, no interfaces, no fields.
        b.extend_from_slice(&[0x00, 0x21, 0x00, 0x01, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00]);
        b.extend_from_slice(&[0x00, 0x01]); // methods_count
        b.extend_from_slice(&[0x00, 0x01, 0x00, 0x09, 0x00, 0x0A, 0x00, 0x01]); // public <init>()V
        b.extend_from_slice(&[0x00, 0x0B]); // "Code"
        b.extend_from_slice(&17u32.to_be_bytes()); // attribute_length
        b.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // max_stack, max_locals
        b.extend_from_slice(&5u32.to_be_bytes()); // code_length
        b.extend_from_slice(&[0x2a, 0xb7, 0x00, 0x05, 0xb1]);
        b.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // no handlers, no attributes
        b.extend_from_slice(&[0x00, 0x00]); // no class attributes
        b
    }

    /// Wave 25: against classes the class manager really holds, the verdict
    /// reads the constant pool -- a default constructor of a direct `Object`
    /// subclass is trivial, one that calls another superclass constructor is
    /// not -- and a proven trivial constructor is called without a frame.
    /// Skips (saying so) when this unit-test VM cannot define the classes, or
    /// when its `java/lang/Object` is not the plain one (a synthetic image, a
    /// registered constructor native): the verdict must then be `false`.
    #[test]
    fn a_proven_trivial_constructor_is_called_without_a_frame() {
        use crate::classloading::ClassLoaderId;
        if cratonvm_jit_api::descriptor_facts_disabled() {
            return;
        }
        // The wave-25 rule: `Object` only. The super chain (wave 94) is set
        // explicitly here, and on in its own test below.
        let mut config = crate::config::VmConfig::default();
        config.interp_trivial_ctor_super_chain = false;
        let shared = Arc::new(SharedVm::new(config));
        let define = |name: &str, bytes: &[u8]| {
            shared
                .classes
                .class_manager_write()
                .define_class_with_options(
                    name,
                    bytes,
                    ClassLoaderId::Bootstrap,
                    cratonvm_classloading::DefineClassOptions {
                        skip_verification: true,
                        ..Default::default()
                    },
                )
                .ok()
        };
        let object = "java/lang/Object";
        let (Some(trivial), Some(base)) = (
            define(
                "w25/Trivial",
                &w25_class_bytes("w25/Trivial", object, object),
            ),
            define("w25/Base", &w25_class_bytes("w25/Base", object, object)),
        ) else {
            eprintln!("skipped: this unit-test VM cannot define a class");
            return;
        };
        let Some(sub) = define(
            "w25/Sub",
            &w25_class_bytes("w25/Sub", "w25/Base", "w25/Base"),
        ) else {
            eprintln!("skipped: this unit-test VM cannot define a subclass");
            return;
        };
        let code: [u8; 5] = [0x2a, 0xb7, 0x00, 0x05, 0xb1];
        // Not `java/lang/Object.<init>`: never trivial without the super
        // chain, whatever `Object` is.
        assert_eq!(
            trivial_object_ctor_uncached(&shared, &ctor_entry(sub, &code)),
            None
        );
        // Bytes that are not the class's own `<init>()V` (a stale entry).
        assert_eq!(
            trivial_object_ctor_uncached(
                &shared,
                &ctor_entry(trivial, &[0x2a, 0xb7, 0x00, 0x06, 0xb1])
            ),
            None
        );
        let object_plain = {
            let cm = shared.classes.class_manager.read();
            cm.get_class(trivial)
                .and_then(|c| c.superclass)
                .and_then(|id| cm.get_class(id))
                .and_then(|o| o.find_method("<init>", "()V"))
                .is_some_and(|m| {
                    !m.is_synchronized() && m.code().is_some_and(|c| c.code.first() == Some(&0xb1))
                })
        } && !super::super::jit_bridge::elidable_ctor_native_would_run(
            &shared, object,
        );
        let cached = ctor_entry(trivial, &code);
        assert_eq!(
            trivial_object_ctor_uncached(&shared, &cached),
            object_plain.then_some(1)
        );
        assert_eq!(
            trivial_object_ctor_uncached(&shared, &ctor_entry(base, &code)),
            object_plain.then_some(1)
        );
        if !object_plain {
            eprintln!("skipped: this unit-test VM's java/lang/Object.<init> is not plain bytecode");
            return;
        }
        // The memo answers what the verdict answered.
        assert!(trivial_object_ctor(&shared, &cached));
        assert!(trivial_object_ctor(&shared, &cached));
        let caller = ClassId::new(987_104);
        let receiver = shared.mem.heap.alloc_object(trivial, 0);
        let mut thread = caller_thread(caller);
        thread.invoke_cache.put(
            caller,
            7,
            true,
            0,
            CachedInvokeTarget::Bytecode {
                cached: Arc::clone(&cached),
                gate: RedefineGate::never_stale(),
            },
        );
        thread.frames[0]
            .stack
            .push(Value::Object(Some(receiver)))
            .expect("an 8-slot stack takes the receiver");
        let served = execute_nonvirtual_fast_door(
            &shared,
            &mut thread,
            0,
            7,
            true,
            0,
            crate::runtime::interpreter::field_fast::fast_field_zgc(&shared),
            &mut DoorProbe::NotProbed,
        );
        if served.is_none() {
            return;
        }
        if trivial_ctor_elidable(&shared, &caller_thread(caller).frames, &cached).is_some() {
            assert!(matches!(served, Some(Ok(CachedCallResult::Handled))));
            assert_eq!(thread.frames.len(), 1, "no frame was pushed");
            assert_eq!(thread.frames[0].stack.len(), 0, "the receiver was popped");
        } else {
            assert!(matches!(served, Some(Ok(CachedCallResult::FramePushed))));
        }
    }

    /// t1 wave 8, lane I24 (`CRATONVM_INTERP_CTOR_ELISION_KEEPS_FINALIZABLE`,
    /// default on): the default constructor of a class with a finalizer is
    /// never proven trivial while the `<init>`-return finalizer hooks are on,
    /// because the frameless call would skip that frame's `return`
    /// (`exception_dispatch::finalizer_init_returned`); `=0` restores the old
    /// verdict. Skips, saying so, where the wave-25 test skips.
    #[test]
    fn t1w8_i24_a_finalizable_class_keeps_its_constructor_frame() {
        use crate::classloading::ClassLoaderId;
        if cratonvm_jit_api::descriptor_facts_disabled() {
            return;
        }
        let mut config = crate::config::VmConfig::default();
        config.interp_trivial_ctor_super_chain = false;
        let shared = Arc::new(SharedVm::new(config));
        let object = "java/lang/Object";
        let defined = shared
            .classes
            .class_manager_write()
            .define_class_with_options(
                "w8i24/Fin",
                &w25_class_bytes("w8i24/Fin", object, object),
                ClassLoaderId::Bootstrap,
                cratonvm_classloading::DefineClassOptions {
                    skip_verification: true,
                    ..Default::default()
                },
            )
            .ok();
        let Some(fin) = defined else {
            eprintln!("skipped: this unit-test VM cannot define a class");
            return;
        };
        let code: [u8; 5] = [0x2a, 0xb7, 0x00, 0x05, 0xb1];
        if trivial_object_ctor_uncached(&shared, &ctor_entry(fin, &code)).is_none() {
            eprintln!("skipped: this unit-test VM's java/lang/Object.<init> is not plain bytecode");
            return;
        }
        shared
            .classes
            .class_manager_write()
            .class_store
            .get_mut(fin)
            .expect("defined above")
            .has_finalizer = true;
        let hooks = shared.config.finalizer_register_at_object_init
            && shared.config.finalizer_reregister_at_init_return;
        for (word, refused) in [(None, hooks), (Some("0"), false), (Some("1"), hooks)] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_INTERP_CTOR_ELISION_KEEPS_FINALIZABLE", word)],
                || {
                    assert_eq!(
                        trivial_object_ctor_uncached(&shared, &ctor_entry(fin, &code)),
                        if refused { None } else { Some(1) },
                        "{word:?}"
                    );
                },
            );
        }
    }

    // ── Wave 28: the field-store constructor (`FIELD_CTOR_ELISION`) ────────

    /// javac's field-assigning constructors of a direct `Object` subclass
    /// parse into their stores, in order; any other body does not.
    #[test]
    fn field_ctor_plan_reads_only_field_store_constructors() {
        // Point(int a, int b) { this.a = a; this.b = b; }
        let point: [u8; 15] = [
            0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x1b, 0xb5, 0x00, 0x07, 0x2a, 0x1c, 0xb5, 0x00, 0x08,
            0xb1,
        ];
        let (plan, n) = field_ctor_plan(&point).expect("Point's constructor");
        assert_eq!(n, 2);
        assert_eq!(plan[0], (7, CtorValue::Local(1)));
        assert_eq!(plan[1], (8, CtorValue::Local(2)));
        // class WithInit { int x = 7; }
        let with_init: [u8; 11] = [
            0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x10, 0x07, 0xb5, 0x00, 0x07, 0xb1,
        ];
        let (plan, n) = field_ctor_plan(&with_init).expect("an initializer");
        assert_eq!((n, plan[0]), (1, (7, CtorValue::Int(7))));
        // sipush -200, fconst_1, aconst_null, aload 4.
        let mixed: [u8; 28] = [
            0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x11, 0xff, 0x38, 0xb5, 0x00, 0x07, 0x2a, 0x0c, 0xb5,
            0x00, 0x08, 0x2a, 0x01, 0xb5, 0x00, 0x09, 0x2a, 0x19, 0x04, 0xb5, 0x00, 0x0a, 0xb1,
        ];
        let (plan, n) = field_ctor_plan(&mixed).expect("four stores");
        assert_eq!(n, 4);
        assert_eq!(plan[0], (7, CtorValue::Int(-200)));
        assert_eq!(plan[1], (8, CtorValue::Float(1.0)));
        assert_eq!(plan[2], (9, CtorValue::Null));
        assert_eq!(plan[3], (10, CtorValue::Local(4)));
        let refuse: [&[u8]; 9] = [
            // the default constructor: the trivial rule's, not this one's
            &[0x2a, 0xb7, 0x00, 0x01, 0xb1],
            // a store before the super call (JDK 25 flexible constructor bodies)
            &[0x2a, 0x1b, 0xb5, 0x00, 0x07, 0x2a, 0xb7, 0x00, 0x01, 0xb1],
            // `this.self = this`
            &[0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x2a, 0xb5, 0x00, 0x07, 0xb1],
            // `iload 0`
            &[
                0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x15, 0x00, 0xb5, 0x00, 0x07, 0xb1,
            ],
            // a call after the stores
            &[
                0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x1b, 0xb5, 0x00, 0x07, 0x2a, 0xb6, 0x00, 0x09, 0xb1,
            ],
            // `ldc` (a String constant)
            &[
                0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x12, 0x03, 0xb5, 0x00, 0x07, 0xb1,
            ],
            // `lconst_1` into a long field
            &[0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x0a, 0xb5, 0x00, 0x07, 0xb1],
            // a byte after the `return`
            &[
                0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x1b, 0xb5, 0x00, 0x07, 0xb1, 0x00,
            ],
            // five stores: over `CTOR_FIELD_STORES_MAX`
            &[
                0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x03, 0xb5, 0x00, 0x07, 0x2a, 0x03, 0xb5, 0x00, 0x07,
                0x2a, 0x03, 0xb5, 0x00, 0x07, 0x2a, 0x03, 0xb5, 0x00, 0x07, 0x2a, 0x03, 0xb5, 0x00,
                0x07, 0xb1,
            ],
        ];
        for body in refuse {
            assert!(field_ctor_plan(body).is_none(), "{body:02x?}");
        }
    }

    /// Interpreter round i6 wave 115, lane L7
    /// (`CRATONVM_INTERP_FIELD_CTOR_WIDE_VALUES`): the wide parse reads the
    /// category-2 loads and constants, and the verdict maps the locals to
    /// argument indices by the descriptor's slot widths.
    #[test]
    fn the_wide_field_ctor_plan_maps_locals_to_arguments() {
        // Entry(int key, long value, Object tag) { this.key = key;
        // this.value = value; this.tag = tag; this.flag = 1L; }
        // locals: 1 = key, 2-3 = value, 4 = tag.
        let entry: [u8; 25] = [
            0x2a, 0xb7, 0x00, 0x01, // aload_0; invokespecial Object.<init>
            0x2a, 0x1b, 0xb5, 0x00, 0x07, // iload_1
            0x2a, 0x20, 0xb5, 0x00, 0x08, // lload_2
            0x2a, 0x19, 0x04, 0xb5, 0x00, 0x09, // aload 4
            0x2a, 0x0a, 0xb5, 0x00, // lconst_1; putfield 0x000a (continued)
            0x0a,
        ];
        let mut body = entry.to_vec();
        body.push(0xb1);
        assert!(field_ctor_plan(&body).is_none(), "off: no category-2 load");
        let (mut plan, n) = field_ctor_plan_with(&body, true, false).expect("the wide parse");
        assert_eq!(n, 4);
        assert_eq!(plan[1], (8, CtorValue::LocalWide(2)));
        assert_eq!(plan[3], (10, CtorValue::Long(1)));
        assert!(ctor_plan_locals_to_args(
            &mut plan[..n],
            "(IJLjava/lang/Object;)V"
        ));
        assert_eq!(plan[0], (7, CtorValue::Local(1)));
        assert_eq!(plan[1], (8, CtorValue::Local(2)));
        assert_eq!(plan[2], (9, CtorValue::Local(3)));
        assert_eq!(plan[3], (10, CtorValue::Long(1)));
        // Vec2(double x, double y): dload_1, dload_3, and dconst_0.
        let vec2: [u8; 20] = [
            0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x27, 0xb5, 0x00, 0x07, 0x2a, 0x29, 0xb5, 0x00, 0x08,
            0x2a, 0x0e, 0xb5, 0x00, 0x09, 0xb1,
        ];
        let (mut plan, n) = field_ctor_plan_with(&vec2, true, false).expect("Vec2");
        assert!(ctor_plan_locals_to_args(&mut plan[..n], "(DD)V"));
        assert_eq!(plan[0], (7, CtorValue::Local(1)));
        assert_eq!(plan[1], (8, CtorValue::Local(2)));
        assert_eq!(plan[2], (9, CtorValue::Double(0.0)));
        // A load of the second half of a long, a category mismatch, the
        // receiver as a wide load, a slot past the parameters: refused.
        for (load, descriptor) in [
            (CtorValue::Local(2), "(J)V"),
            (CtorValue::LocalWide(1), "(I)V"),
            (CtorValue::Local(1), "(J)V"),
            (CtorValue::LocalWide(0), "(J)V"),
            (CtorValue::Local(3), "(II)V"),
        ] {
            let mut plan = [(7u16, load)];
            assert!(!ctor_plan_locals_to_args(&mut plan, descriptor), "{load:?} {descriptor}");
        }
        // Arrays and references take one slot whatever their element type.
        let slots = ctor_param_slots("([JLjava/lang/Long;D[[DI)V").expect("a descriptor");
        assert_eq!(slots[1], Some((1, false)));
        assert_eq!(slots[2], Some((2, false)));
        assert_eq!(slots[3], Some((3, true)));
        assert_eq!(slots[4], None);
        assert_eq!(slots[5], Some((4, false)));
        assert_eq!(slots[6], Some((5, false)));
        assert_eq!(slots[7], None);
        assert!(ctor_param_slots("not a descriptor").is_none());
        assert!(ctor_param_slots("(I").is_none());
    }

    /// Interpreter round i6 wave 118, lane L7
    /// (`CRATONVM_INTERP_FIELD_CTOR_LDC_VALUES`): the `ldc` parse reads the
    /// three constant loads as pool indices; without the flag it refuses them.
    #[test]
    fn the_ldc_field_ctor_plan_reads_pool_indices() {
        // this.a = 100000 (ldc #9); this.b = 2.5f (ldc_w #0x0102);
        // this.c = 5L (ldc2_w #11)
        let body: [u8; 21] = [
            0x2a, 0xb7, 0x00, 0x01, // aload_0; invokespecial Object.<init>
            0x2a, 0x12, 0x09, 0xb5, 0x00, 0x07, // ldc #9
            0x2a, 0x13, 0x01, 0x02, 0xb5, 0x00, 0x08, // ldc_w #258
            0x2a, 0x14, 0x00, 0x0b, // ldc2_w #11 (putfield follows)
        ];
        let mut code = body.to_vec();
        code.extend_from_slice(&[0xb5, 0x00, 0x0a, 0xb1]);
        assert!(field_ctor_plan(&code).is_none(), "off: no ldc");
        assert!(field_ctor_plan_with(&code, true, false).is_none(), "wide alone: no ldc");
        let (plan, n) = field_ctor_plan_with(&code, false, true).expect("the ldc parse");
        assert_eq!(n, 3);
        assert_eq!(plan[0], (7, CtorValue::Ldc(9)));
        assert_eq!(plan[1], (8, CtorValue::Ldc(258)));
        assert_eq!(plan[2], (10, CtorValue::Ldc2(11)));
    }

    /// The screen reads the padded body the caches hold, like the trivial
    /// constructor's shape test.
    #[test]
    fn field_ctor_prefilter_reads_the_padded_body() {
        let declaring = ClassId::new(987_106);
        let point: [u8; 15] = [
            0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x1b, 0xb5, 0x00, 0x07, 0x2a, 0x1c, 0xb5, 0x00, 0x08,
            0xb1,
        ];
        let entry = |code: &[u8]| {
            CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: declaring,
                class_name: Arc::from("w28/Point"),
                method_name: Arc::from("<init>"),
                method_descriptor: Arc::from("(II)V"),
                source_file: None,
                code: crate::runtime::frame::padded_bytecode(code),
                exception_table: Arc::from(vec![].as_slice()),
                max_stack: 2,
                max_locals: 3,
                num_params: 2,
                is_synchronized: false,
                is_static: false,
            })
        };
        assert!(field_ctor_prefilter(&entry(&point)));
        assert!(!field_ctor_prefilter(&entry(&[
            0x2a, 0xb7, 0x00, 0x01, 0xb1
        ])));
        assert!(
            !field_ctor_prefilter(&entry(&point[..14])),
            "no `return` at the end"
        );
    }

    // ── t1 wave 6, lane G6: stores before `super()` ───────────────────────

    /// javac's inner-class constructor shape, `Sub(K key) { this.key = key; }`
    /// of a non-static `Sub`: `this$0` stored, then `super()`, then `key`.
    const G6_INNER: [u8; 15] = [
        0x2a, 0x2b, 0xb5, 0x00, 0x0c, // aload_0; aload_1; putfield #12 (this$0)
        0x2a, 0xb7, 0x00, 0x05, // aload_0; invokespecial #5 (super())
        0x2a, 0x2c, 0xb5, 0x00, 0x0d, // aload_0; aload_2; putfield #13 (key)
        0xb1,
    ];

    /// The pre-`super()` parse reads the inner / local class shapes, in body
    /// order with the call's pc, and nothing else: not the plain field-store
    /// shape (no store before the call), not a body without a call, with two
    /// calls, with something after `return`, or with too many stores.
    #[test]
    fn g6_pre_super_plan_reads_only_inner_class_constructors() {
        let (plan, n, call_pc) =
            field_ctor_pre_super_plan(&G6_INNER, false, false).expect("Sub's constructor");
        assert_eq!((n, call_pc), (2, 5));
        assert_eq!(plan[0], (12, CtorValue::Local(1)));
        assert_eq!(plan[1], (13, CtorValue::Local(2)));
        // `HashMap$KeySet`: only `this$0`, then a (trivial) `super()`.
        let view = [0x2a, 0x2b, 0xb5, 0x00, 0x0c, 0x2a, 0xb7, 0x00, 0x05, 0xb1];
        let (plan, n, call_pc) = field_ctor_pre_super_plan(&view, false, false).expect("a view");
        assert_eq!((n, call_pc, plan[0]), (1, 5, (12, CtorValue::Local(1))));
        // An anonymous class capturing an int and the outer instance, with a
        // field initializer after `super()`: `val$n`, `this$0`, super, `x = 7`.
        let anon = [
            0x2a, 0x1c, 0xb5, 0x00, 0x0e, 0x2a, 0x2b, 0xb5, 0x00, 0x0c, 0x2a, 0xb7, 0x00, 0x05,
            0x2a, 0x10, 0x07, 0xb5, 0x00, 0x0f, 0xb1,
        ];
        let (plan, n, call_pc) = field_ctor_pre_super_plan(&anon, false, false).expect("anon");
        assert_eq!((n, call_pc), (3, 10));
        assert_eq!(
            plan[..3],
            [
                (14, CtorValue::Local(2)),
                (12, CtorValue::Local(1)),
                (15, CtorValue::Int(7)),
            ]
        );
        // The plain shape is `field_ctor_plan`'s, not this parse's.
        let point = [
            0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x1b, 0xb5, 0x00, 0x07, 0x2a, 0x1c, 0xb5, 0x00, 0x08,
            0xb1,
        ];
        assert!(field_ctor_pre_super_plan(&point, false, false).is_none());
        // No `super()` call at all.
        let no_call = [
            0x2a, 0x2b, 0xb5, 0x00, 0x0c, 0x2a, 0x2c, 0xb5, 0x00, 0x0d, 0xb1,
        ];
        assert!(field_ctor_pre_super_plan(&no_call, false, false).is_none());
        // Two calls.
        let mut two = G6_INNER.to_vec();
        two.splice(14..14, [0x2a, 0xb7, 0x00, 0x05]);
        assert!(field_ctor_pre_super_plan(&two, false, false).is_none());
        // Bytes after `return`.
        let mut tail = G6_INNER.to_vec();
        tail.push(0x00);
        assert!(field_ctor_pre_super_plan(&tail, false, false).is_none());
        // Five stores (the door makes at most four).
        let mut five = vec![];
        for f in 0..3u8 {
            five.extend_from_slice(&[0x2a, 0x2b, 0xb5, 0x00, 0x20 + f]);
        }
        five.extend_from_slice(&[0x2a, 0xb7, 0x00, 0x05]);
        for f in 3..5u8 {
            five.extend_from_slice(&[0x2a, 0x2b, 0xb5, 0x00, 0x20 + f]);
        }
        five.push(0xb1);
        assert!(field_ctor_pre_super_plan(&five, false, false).is_none());
        // A call into something else between the stores (`getfield` value).
        let getfield = [
            0x2a, 0x2b, 0xb4, 0x00, 0x09, 0xb5, 0x00, 0x0c, 0x2a, 0xb7, 0x00, 0x05, 0xb1,
        ];
        assert!(field_ctor_pre_super_plan(&getfield, false, false).is_none());
        // The refactored plain parse still reads the plain shape.
        assert_eq!(field_ctor_plan(&point).map(|(_, n)| n), Some(2));
    }

    /// javac 25's inner-class constructor, `AbstractClassLoaderValue$Sub`'s
    /// on the JDK 25 image: `this$0` goes through `dup; invokestatic
    /// Objects.requireNonNull; pop` before its store. The G6 parse refused
    /// it (the value was not followed by `putfield`), so the switch never
    /// answered the i147 `Sub` it was built for.
    const G6_JAVAC25_INNER: [u8; 20] = [
        0x2a, 0x2b, 0x59, 0xb8, 0x00, 0x11, 0x57, 0xb5, 0x00, 0x0c, // this$0, null-checked
        0x2a, 0xb7, 0x00, 0x05, // super()
        0x2a, 0x2c, 0xb5, 0x00, 0x0d, // key
        0xb1,
    ];

    /// The pre-`super()` parse reads javac 25's null-checked `this$0` store
    /// as [`CtorValue::NullChecked`] with the `invokestatic`'s pool index,
    /// before the call only: the plain parse and the stores after `super()`
    /// still refuse it, and a check followed by anything but `pop; putfield`
    /// is refused.
    #[test]
    fn g6_pre_super_plan_reads_javac25_null_checked_outer_store() {
        let (plan, n, call_pc) = field_ctor_pre_super_plan(&G6_JAVAC25_INNER, false, false)
            .expect("javac 25's Sub constructor");
        assert_eq!((n, call_pc), (2, 10));
        assert_eq!(plan[0], (12, CtorValue::NullChecked(1, 0x11)));
        assert_eq!(plan[1], (13, CtorValue::Local(2)));
        // The same store after `super()` is not javac's shape.
        let after = [
            0x2a, 0x2b, 0xb5, 0x00, 0x0c, 0x2a, 0xb7, 0x00, 0x05, 0x2a, 0x2c, 0x59, 0xb8, 0x00,
            0x11, 0x57, 0xb5, 0x00, 0x0d, 0xb1,
        ];
        assert!(field_ctor_pre_super_plan(&after, false, false).is_none());
        // Nor in the plain shape.
        let plain = [
            0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x2b, 0x59, 0xb8, 0x00, 0x11, 0x57, 0xb5, 0x00, 0x07,
            0xb1,
        ];
        assert!(field_ctor_plan(&plain).is_none());
        // `dup; invokestatic` kept on the stack (no `pop`) is refused.
        let mut no_pop = G6_JAVAC25_INNER.to_vec();
        no_pop[6] = 0x00;
        assert!(field_ctor_pre_super_plan(&no_pop, false, false).is_none());
        // An int value is never null-checked.
        let mut int = G6_JAVAC25_INNER.to_vec();
        int[1] = 0x1b;
        assert!(field_ctor_pre_super_plan(&int, false, false).is_none());
    }

    /// The screen passes the inner shape on the padded body, and refuses the
    /// plain shape (the other screen's) and a body without `return` last.
    #[test]
    fn g6_pre_super_prefilter_reads_the_padded_body() {
        let entry = |code: &[u8]| {
            CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(987_606),
                class_name: Arc::from("g6/Outer$Sub"),
                method_name: Arc::from("<init>"),
                method_descriptor: Arc::from("(Lg6/Outer;Ljava/lang/Object;)V"),
                source_file: None,
                code: crate::runtime::frame::padded_bytecode(code),
                exception_table: Arc::from(vec![].as_slice()),
                max_stack: 2,
                max_locals: 3,
                num_params: 2,
                is_synchronized: false,
                is_static: false,
            })
        };
        assert!(field_ctor_pre_super_prefilter(&entry(&G6_INNER)));
        assert!(!field_ctor_prefilter(&entry(&G6_INNER)));
        assert!(!field_ctor_pre_super_prefilter(&entry(&[
            0x2a, 0xb7, 0x00, 0x01, 0x2a, 0x1b, 0xb5, 0x00, 0x07, 0xb1
        ])));
        assert!(!field_ctor_pre_super_prefilter(&entry(&G6_INNER[..14])));
    }

    /// Class-file bytes for `this_name extends java/lang/Object` with a field
    /// `outer` (`Ljava/lang/Object;`, synthetic final, as `this$0`) and one
    /// method `<init>(Ljava/lang/Object;)V` = `aload_0; aload_1; putfield
    /// #12; aload_0; invokespecial java/lang/Object.<init>()V; return`.
    /// Constant pool as [`w25_class_bytes`]'s, plus #12 Fieldref #1.#13,
    /// #13 NameAndType #14:#15, #14 "outer", #15 "Ljava/lang/Object;",
    /// #16 "(Ljava/lang/Object;)V".
    fn g6_inner_class_bytes(this_name: &str) -> Vec<u8> {
        g6_inner_class_bytes_with(this_name, None)
    }

    /// [`g6_inner_class_bytes`], or with `Some(check_class)` javac 25's
    /// body ([`G6_JAVAC25_INNER`]'s first ten bytes, then the call and
    /// `return`: `this$0` null-checked by `invokestatic #17`), the pool
    /// extended by #17 Methodref #18.#20, #18 Class #19, #19 `check_class`,
    /// #20 NameAndType #21:#22, #21 "requireNonNull",
    /// #22 "(Ljava/lang/Object;)Ljava/lang/Object;".
    fn g6_inner_class_bytes_with(this_name: &str, check_class: Option<&str>) -> Vec<u8> {
        fn utf8(b: &mut Vec<u8>, s: &str) {
            b.push(1);
            b.extend_from_slice(&u16::try_from(s.len()).unwrap().to_be_bytes());
            b.extend_from_slice(s.as_bytes());
        }
        let object = "java/lang/Object";
        let mut b: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x34];
        let count: u16 = if check_class.is_some() { 23 } else { 17 };
        b.extend_from_slice(&count.to_be_bytes()); // constant_pool_count
        b.extend_from_slice(&[7, 0x00, 0x02]); // #1 Class #2
        utf8(&mut b, this_name); // #2
        b.extend_from_slice(&[7, 0x00, 0x04]); // #3 Class #4
        utf8(&mut b, object); // #4
        b.extend_from_slice(&[10, 0x00, 0x06, 0x00, 0x08]); // #5 Methodref #6.#8
        b.extend_from_slice(&[7, 0x00, 0x07]); // #6 Class #7
        utf8(&mut b, object); // #7
        b.extend_from_slice(&[12, 0x00, 0x09, 0x00, 0x0A]); // #8 NameAndType #9:#10
        utf8(&mut b, "<init>"); // #9
        utf8(&mut b, "()V"); // #10
        utf8(&mut b, "Code"); // #11
        b.extend_from_slice(&[9, 0x00, 0x01, 0x00, 0x0D]); // #12 Fieldref #1.#13
        b.extend_from_slice(&[12, 0x00, 0x0E, 0x00, 0x0F]); // #13 NameAndType #14:#15
        utf8(&mut b, "outer"); // #14
        utf8(&mut b, "Ljava/lang/Object;"); // #15
        utf8(&mut b, "(Ljava/lang/Object;)V"); // #16
        if let Some(check_class) = check_class {
            b.extend_from_slice(&[10, 0x00, 0x12, 0x00, 0x14]); // #17 Methodref #18.#20
            b.extend_from_slice(&[7, 0x00, 0x13]); // #18 Class #19
            utf8(&mut b, check_class); // #19
            b.extend_from_slice(&[12, 0x00, 0x15, 0x00, 0x16]); // #20 NameAndType #21:#22
            utf8(&mut b, "requireNonNull"); // #21
            utf8(&mut b, "(Ljava/lang/Object;)Ljava/lang/Object;"); // #22
        }
        let body: Vec<u8> = match check_class {
            Some(_) => [&G6_JAVAC25_INNER[..10], &[0x2a, 0xb7, 0x00, 0x05, 0xb1][..]].concat(),
            None => G6_INNER_BODY.to_vec(),
        };
        let (header, field): ([u8; 8], [u8; 10]) = (
            // public super, this #1, super #3, no interfaces.
            [0x00, 0x21, 0x00, 0x01, 0x00, 0x03, 0x00, 0x00],
            // One field: final synthetic, #14, #15, no attributes.
            [0x00, 0x01, 0x10, 0x10, 0x00, 0x0E, 0x00, 0x0F, 0x00, 0x00],
        );
        b.extend_from_slice(&header);
        b.extend_from_slice(&field);
        b.extend_from_slice(&[0x00, 0x01]); // methods_count
        b.extend_from_slice(&[0x00, 0x01, 0x00, 0x09, 0x00, 0x10, 0x00, 0x01]); // <init>(Object)V
        b.extend_from_slice(&[0x00, 0x0B]); // "Code"
        let code_len = u32::try_from(body.len()).unwrap();
        b.extend_from_slice(&(12 + code_len).to_be_bytes()); // attribute_length
        b.extend_from_slice(&[0x00, 0x03, 0x00, 0x02]); // max_stack, max_locals
        b.extend_from_slice(&code_len.to_be_bytes()); // code_length
        b.extend_from_slice(&body);
        b.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // no handlers, no attributes
        b.extend_from_slice(&[0x00, 0x00]); // no class attributes
        b
    }

    /// [`g6_inner_class_bytes`]'s constructor body.
    const G6_INNER_BODY: [u8; 10] = [0x2a, 0x2b, 0xb5, 0x00, 0x0c, 0x2a, 0xb7, 0x00, 0x05, 0xb1];

    /// Against a class the class manager really holds: with
    /// `CRATONVM_INTERP_FIELD_CTOR_PRE_SUPER_STORES` the verdict reads the
    /// `super()` call where the parse found it (pc 5) and carries the
    /// pre-`super()` store; without it the constructor is not answered
    /// (`object_init_first` false, no plan), exactly as before. Skips (saying
    /// so) when this unit-test VM cannot define the class or its
    /// `java/lang/Object.<init>` is not plain bytecode.
    #[test]
    fn g6_the_verdict_reads_the_super_call_after_the_stores_only_with_the_switch() {
        use crate::classloading::ClassLoaderId;
        if cratonvm_jit_api::descriptor_facts_disabled() {
            return;
        }
        let verdict_with = |on: bool| -> Option<(Option<u8>, CtorVerdict)> {
            let config = crate::config::VmConfig {
                interp_field_ctor_pre_super_stores: on,
                ..crate::config::VmConfig::default()
            };
            let shared = SharedVm::new(config);
            let id = shared
                .classes
                .class_manager_write()
                .define_class_with_options(
                    "g6/Inner",
                    &g6_inner_class_bytes("g6/Inner"),
                    ClassLoaderId::Bootstrap,
                    cratonvm_classloading::DefineClassOptions {
                        skip_verification: true,
                        ..Default::default()
                    },
                )
                .ok()?;
            let cached = CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: id,
                class_name: Arc::from("g6/Inner"),
                method_name: Arc::from("<init>"),
                method_descriptor: Arc::from("(Ljava/lang/Object;)V"),
                source_file: None,
                code: crate::runtime::frame::padded_bytecode(&G6_INNER_BODY),
                exception_table: Arc::from(vec![].as_slice()),
                max_stack: 2,
                max_locals: 2,
                num_params: 1,
                is_synchronized: false,
                is_static: false,
            });
            Some((
                trivial_object_ctor_uncached_at(&shared, &cached, 5),
                ctor_verdict_uncached(&shared, &cached),
            ))
        };
        let Some((chain_on, on)) = verdict_with(true) else {
            eprintln!("skipped: this unit-test VM cannot define a class");
            return;
        };
        let Some((_, off)) = verdict_with(false) else {
            eprintln!("skipped: this unit-test VM cannot define a class");
            return;
        };
        // Off: unchanged (the first instruction is not the call).
        assert!(!off.object_init_first);
        assert_eq!(off.plan_len, 0);
        if chain_on != Some(1) {
            eprintln!("skipped: this unit-test VM's java/lang/Object.<init> is not plain bytecode");
            assert!(!on.object_init_first && on.plan_len == 0);
            return;
        }
        assert!(on.object_init_first);
        assert_eq!(on.chain_frames, 1);
        assert_eq!(on.plan_len, 1);
        assert_eq!(on.plan[0], (12, CtorValue::Local(1)));
    }

    /// Class-file bytes for a stand-in `java/util/Objects` (for a unit-test
    /// VM without the JDK image) whose one method is `public static
    /// requireNonNull(Ljava/lang/Object;)Ljava/lang/Object;` =
    /// `aload_0; ifnonnull +5; aconst_null; athrow; aload_0; areturn`: the
    /// shape [`is_jdk_require_non_null`] reads.
    fn g6_objects_class_bytes() -> Vec<u8> {
        fn utf8(b: &mut Vec<u8>, s: &str) {
            b.push(1);
            b.extend_from_slice(&u16::try_from(s.len()).unwrap().to_be_bytes());
            b.extend_from_slice(s.as_bytes());
        }
        let mut b: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x34];
        b.extend_from_slice(&8u16.to_be_bytes()); // constant_pool_count
        b.extend_from_slice(&[7, 0x00, 0x02]); // #1 Class #2
        utf8(&mut b, "java/util/Objects"); // #2
        b.extend_from_slice(&[7, 0x00, 0x04]); // #3 Class #4
        utf8(&mut b, "java/lang/Object"); // #4
        utf8(&mut b, "requireNonNull"); // #5
        utf8(&mut b, "(Ljava/lang/Object;)Ljava/lang/Object;"); // #6
        utf8(&mut b, "Code"); // #7
        // public final super, this #1, super #3, no interfaces, no fields.
        b.extend_from_slice(&[0x00, 0x31, 0x00, 0x01, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00]);
        b.extend_from_slice(&[0x00, 0x01]); // methods_count
        b.extend_from_slice(&[0x00, 0x09, 0x00, 0x05, 0x00, 0x06, 0x00, 0x01]); // public static
        b.extend_from_slice(&[0x00, 0x07]); // "Code"
        b.extend_from_slice(&20u32.to_be_bytes()); // attribute_length
        b.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // max_stack, max_locals
        b.extend_from_slice(&8u32.to_be_bytes()); // code_length
        b.extend_from_slice(&[0x2a, 0xc7, 0x00, 0x05, 0x01, 0xbf, 0x2a, 0xb0]);
        b.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // no handlers, no attributes
        b.extend_from_slice(&[0x00, 0x00]); // no class attributes
        b
    }

    /// i147 close-out, against classes the class manager really holds: with
    /// `CRATONVM_INTERP_FIELD_CTOR_PRE_SUPER_STORES`, javac 25's inner-class
    /// constructor (`this$0` through `Objects.requireNonNull`) is answered,
    /// its outer instance a [`CtorValue::LocalNonNull`], when the check is
    /// `java/util/Objects`'s; the same call into another class keeps the
    /// frame. Skips (saying so) as
    /// [`g6_the_verdict_reads_the_super_call_after_the_stores_only_with_the_switch`].
    #[test]
    fn g6_the_verdict_answers_javac25_null_checked_outer_only_for_the_jdk_check() {
        use crate::classloading::ClassLoaderId;
        if cratonvm_jit_api::descriptor_facts_disabled() {
            return;
        }
        let options = || cratonvm_classloading::DefineClassOptions {
            skip_verification: true,
            ..Default::default()
        };
        let verdict_for = |check_class: &str| -> Option<(Option<u8>, CtorVerdict)> {
            let config = crate::config::VmConfig {
                interp_field_ctor_pre_super_stores: true,
                ..crate::config::VmConfig::default()
            };
            let shared = SharedVm::new(config);
            let id = {
                let mut cm = shared.classes.class_manager_write();
                if cm
                    .find_class_by_name_for_loader("java/util/Objects", ClassLoaderId::Bootstrap)
                    .is_none()
                {
                    cm.define_class_with_options(
                        "java/util/Objects",
                        &g6_objects_class_bytes(),
                        ClassLoaderId::Bootstrap,
                        options(),
                    )
                    .ok()?;
                }
                cm.define_class_with_options(
                    "g6/Inner25",
                    &g6_inner_class_bytes_with("g6/Inner25", Some(check_class)),
                    ClassLoaderId::Bootstrap,
                    options(),
                )
                .ok()?
            };
            let body = [&G6_JAVAC25_INNER[..10], &[0x2a, 0xb7, 0x00, 0x05, 0xb1][..]].concat();
            let cached = CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: id,
                class_name: Arc::from("g6/Inner25"),
                method_name: Arc::from("<init>"),
                method_descriptor: Arc::from("(Ljava/lang/Object;)V"),
                source_file: None,
                code: crate::runtime::frame::padded_bytecode(&body),
                exception_table: Arc::from(vec![].as_slice()),
                max_stack: 3,
                max_locals: 2,
                num_params: 1,
                is_synchronized: false,
                is_static: false,
            });
            Some((
                trivial_object_ctor_uncached_at(&shared, &cached, 10),
                ctor_verdict_uncached(&shared, &cached),
            ))
        };
        let Some((chain, jdk)) = verdict_for("java/util/Objects") else {
            eprintln!("skipped: this unit-test VM cannot define a class");
            return;
        };
        let Some((_, other)) = verdict_for("g6/Objects") else {
            eprintln!("skipped: this unit-test VM cannot define a class");
            return;
        };
        // Another class's `requireNonNull`: the frame stays.
        assert!(!other.object_init_first);
        assert_eq!(other.plan_len, 0);
        if chain != Some(1) {
            eprintln!("skipped: this unit-test VM's java/lang/Object.<init> is not plain bytecode");
            assert!(!jdk.object_init_first && jdk.plan_len == 0);
            return;
        }
        assert!(jdk.object_init_first);
        assert_eq!(jdk.chain_frames, 1);
        assert_eq!(jdk.plan_len, 1);
        assert_eq!(jdk.plan[0], (12, CtorValue::LocalNonNull(1)));
    }

    /// Interpreter round i6 wave 119, lane L7
    /// (`CRATONVM_INTERP_CTOR_VERDICT_ON_ENTRY`): with the switch, a verdict
    /// the entry carries under the current stamp answers; a stale stamp does
    /// not (the per-thread memo / the uncached verdict answer instead).
    #[test]
    fn an_entry_verdict_answers_under_its_own_stamp_only() {
        let config = crate::config::VmConfig {
            interp_ctor_verdict_on_entry: true,
            ..crate::config::VmConfig::default()
        };
        let shared = SharedVm::new(config);
        let entry = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(987_119),
                class_name: Arc::from("w119/NoSuchClass"),
                method_name: Arc::from("<init>"),
                method_descriptor: Arc::from("()V"),
                source_file: None,
                code: crate::runtime::frame::padded_bytecode(&[0x2a, 0xb7, 0x00, 0x01, 0xb1]),
                exception_table: Arc::from(vec![].as_slice()),
                max_stack: 1,
                max_locals: 1,
                num_params: 0,
                is_synchronized: false,
                is_static: false,
            },
        ));
        let planted = CtorVerdict {
            object_init_first: true,
            chain_frames: 3,
            plan_len: 0,
            plan: [(0u16, CtorValue::Null); super::super::field_fast::CTOR_FIELD_STORES_MAX],
        };
        let now = crate::classloading::class_redefinition_count();
        let _ = entry
            .ctor_verdict_memo
            .set(Box::new(EntryCtorVerdictCell::new(EntryCtorVerdict {
                vm: shared.vm_identity,
                redefinitions: now,
                verdict: planted,
            })));
        let answered = ctor_verdict(&shared, &entry);
        if crate::classloading::class_redefinition_count() == now {
            assert_eq!(answered.chain_frames, 3, "the entry's own verdict answers");
        }
        // Another VM's (or a stale) stamp is not redeemed: the class does not
        // exist in this VM, so the real verdict is "not trivial".
        let other = Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: ClassId::new(987_120),
                class_name: Arc::from("w119/NoSuchClass2"),
                method_name: Arc::from("<init>"),
                method_descriptor: Arc::from("()V"),
                source_file: None,
                code: crate::runtime::frame::padded_bytecode(&[0x2a, 0xb7, 0x00, 0x01, 0xb1]),
                exception_table: Arc::from(vec![].as_slice()),
                max_stack: 1,
                max_locals: 1,
                num_params: 0,
                is_synchronized: false,
                is_static: false,
            },
        ));
        let _ = other
            .ctor_verdict_memo
            .set(Box::new(EntryCtorVerdictCell::new(EntryCtorVerdict {
                vm: shared.vm_identity.wrapping_add(1),
                redefinitions: now,
                verdict: planted,
            })));
        assert!(!ctor_verdict(&shared, &other).object_init_first);
        // Wave 120 (L9's review): the stale cell was refilled with this VM's
        // stamp and real verdict, not left stale.
        let refilled = other
            .ctor_verdict_memo
            .get()
            .and_then(|cell| cell.downcast_ref::<EntryCtorVerdictCell>())
            .expect("the cell stays installed");
        if crate::classloading::class_redefinition_count() == now {
            assert_eq!(refilled.load().vm, shared.vm_identity);
            assert!(!refilled.load().verdict.object_init_first);
        }
    }

    /// Interpreter round i6 wave 120, lane L7: a refill keeps a reference
    /// loaded before it valid, and refills stop at the cap.
    #[test]
    fn an_entry_verdict_cell_refills_and_keeps_old_answers_alive() {
        let verdict = |frames: u8| CtorVerdict {
            object_init_first: true,
            chain_frames: frames,
            plan_len: 0,
            plan: [(0u16, CtorValue::Null); super::super::field_fast::CTOR_FIELD_STORES_MAX],
        };
        let cell = EntryCtorVerdictCell::new(EntryCtorVerdict {
            vm: 1,
            redefinitions: 0,
            verdict: verdict(1),
        });
        let before = cell.load();
        cell.refill(EntryCtorVerdict {
            vm: 1,
            redefinitions: 1,
            verdict: verdict(2),
        });
        assert_eq!(before.verdict.chain_frames, 1, "the old answer is still readable");
        assert_eq!(cell.load().redefinitions, 1);
        for i in 0..(EntryCtorVerdictCell::MAX_REFILLS as u64 + 10) {
            cell.refill(EntryCtorVerdict {
                vm: 1,
                redefinitions: 2 + i,
                verdict: verdict(3),
            });
        }
        // The cap: the last refill that landed was the 64th.
        assert_eq!(
            cell.load().redefinitions,
            EntryCtorVerdictCell::MAX_REFILLS as u64
        );
    }

    /// Wave 28 (L7b): a `long` / `double` parameter is found by the
    /// descriptor, once per method, and only at the top level of the
    /// parameter list (not inside a class name or an array's element class).
    #[test]
    fn category2_parameters_are_read_from_the_descriptor() {
        for (descriptor, cat2) in [
            ("()V", false),
            ("(II)V", false),
            ("(JI)V", true),
            ("(ID)V", true),
            ("(Ljava/lang/Double;I)V", false),
            ("([JI)V", false),
            ("([[Ljava/lang/Long;D)V", true),
            ("(Ljava/util/List;[D)V", false),
            ("not a descriptor", true),
        ] {
            assert_eq!(
                descriptor_has_category2_param(descriptor),
                cat2,
                "{descriptor}"
            );
        }
    }

    /// Interpreter round i4 wave 94, lane L7: with
    /// `VmConfig::interp_trivial_ctor_super_chain` the verdict follows a chain
    /// of default constructors (`w94/Leaf` -> `w94/Mid` -> `w94/Base` ->
    /// `Object`) and answers the frames the framed call would push; without
    /// it the same constructor is not trivial. Skips (saying so) when this
    /// unit-test VM cannot define the classes.
    #[test]
    fn the_super_chain_verdict_follows_default_constructors_to_object() {
        use crate::classloading::ClassLoaderId;
        if cratonvm_jit_api::descriptor_facts_disabled() {
            return;
        }
        let verdict_with = |chain: bool| -> Option<(Option<u8>, Option<u8>, bool)> {
            let mut config = crate::config::VmConfig::default();
            config.interp_trivial_ctor_super_chain = chain;
            let shared = Arc::new(SharedVm::new(config));
            let define = |name: &str, bytes: &[u8]| {
                shared
                    .classes
                    .class_manager_write()
                    .define_class_with_options(
                        name,
                        bytes,
                        ClassLoaderId::Bootstrap,
                        cratonvm_classloading::DefineClassOptions {
                            skip_verification: true,
                            ..Default::default()
                        },
                    )
                    .ok()
            };
            let object = "java/lang/Object";
            let base = define("w94/Base", &w25_class_bytes("w94/Base", object, object))?;
            let mid = define(
                "w94/Mid",
                &w25_class_bytes("w94/Mid", "w94/Base", "w94/Base"),
            )?;
            let leaf = define(
                "w94/Leaf",
                &w25_class_bytes("w94/Leaf", "w94/Mid", "w94/Mid"),
            )?;
            // What the chain rests on besides the bytes: a plain
            // `Object.<init>`, real class bytes for the two inner links, and
            // no registered native in front of any link.
            let plain = {
                let cm = shared.classes.class_manager.read();
                let object_plain = cm
                    .get_class(base)
                    .and_then(|c| c.superclass)
                    .and_then(|id| cm.get_class(id))
                    .and_then(|o| o.find_method("<init>", "()V"))
                    .is_some_and(|m| {
                        !m.is_synchronized()
                            && m.code().is_some_and(|c| c.code.first() == Some(&0xb1))
                    });
                let real = [base, mid]
                    .iter()
                    .all(|&id| cm.get_class(id).is_some_and(|c| c.origin.has_real_bytes()));
                object_plain && real
            } && ["w94/Mid", "w94/Base", object].iter().all(|name| {
                !super::super::jit_bridge::elidable_ctor_native_would_run(&shared, name)
            });
            let code: [u8; 5] = [0x2a, 0xb7, 0x00, 0x05, 0xb1];
            Some((
                trivial_object_ctor_uncached(&shared, &ctor_entry(leaf, &code)),
                trivial_object_ctor_uncached(&shared, &ctor_entry(mid, &code)),
                plain,
            ))
        };
        let Some((leaf_off, mid_off, _)) = verdict_with(false) else {
            eprintln!("skipped: this unit-test VM cannot define the classes");
            return;
        };
        assert_eq!((leaf_off, mid_off), (None, None), "Object only without the chain");
        let Some((leaf_on, mid_on, plain)) = verdict_with(true) else {
            eprintln!("skipped: this unit-test VM cannot define the classes");
            return;
        };
        assert_eq!(leaf_on, plain.then_some(3), "Leaf -> Mid -> Base -> Object");
        assert_eq!(mid_on, plain.then_some(2), "Mid -> Base -> Object");
    }
}
