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
//! a target that is not a plain bytecode method, a `synchronized` callee, a
//! callee with a registered native or a non-zero intercept shape, a null
//! receiver (`invokespecial`), a loader-split owner, more than eight
//! parameters, any argument whose operand-stack slot is not already in the
//! representation the callee's locals want, a full frame stack, a virtual
//! thread, PGO profiling, class redefinition, and every invoke diagnostic.
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

/// Arguments read off the operand stack, paired with the descriptor tag that
/// decides how many local slots each occupies. Sized for the receiver plus the
/// eight parameters `DescriptorFacts` keeps inline; a longer descriptor is
/// declined.
pub(super) type ArgSlots = [(CompactValue, u8); cratonvm_jit_api::DescriptorFacts::INLINE_PARAMS + 1];

/// An empty [`ArgSlots`] to fill.
#[inline(always)]
pub(super) fn empty_arg_slots() -> ArgSlots {
    [(CompactValue::null(), b'L'); cratonvm_jit_api::DescriptorFacts::INLINE_PARAMS + 1]
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
    if stack.len() < total_args || total_args > slots.len() {
        return false;
    }
    // The tag walk below indexes `facts.param_tags`, which is a fixed
    // `[u8; INLINE_PARAMS]`. A descriptor with more parameters than that has
    // no inline tags to read, and one shorter than `total_args` claims would
    // read past its own length. Both doors already refuse an overflowing
    // descriptor before calling here; this is the helper standing on its own.
    let num_params = total_args - usize::from(has_receiver);
    if facts.param_tags_overflow || num_params > facts.param_tag_len as usize {
        return false;
    }
    for i in 0..total_args {
        let depth = total_args - 1 - i;
        let (cv, kind) = stack.peek_with_kind_at(depth);
        let tag = if has_receiver {
            if i == 0 {
                b'L'
            } else {
                facts.param_tags[i - 1]
            }
        } else {
            facts.param_tags[i]
        };
        let ok = match tag {
            b'L' | b'[' => cv.is_object() || cv.is_null(),
            b'J' => kind == crate::runtime::ValueStack::KIND_MARK_LONG,
            b'D' => kind == crate::runtime::ValueStack::KIND_MARK_DOUBLE,
            b'F' => cv.as_float().is_some(),
            b'I' | b'Z' | b'B' | b'C' | b'S' => cv.as_int().is_some(),
            _ => false,
        };
        if !ok {
            return false;
        }
        slots[i] = (cv, tag);
    }
    true
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
    cached: Arc<CachedBytecodeMethod>,
    slots: &ArgSlots,
    total_args: usize,
) -> CachedCallResult {
    thread.frames[frame_idx].stack.discard_top(total_args);
    thread.refill_pools_from_shared(
        &shared.mem.operand_stack_pool,
        &shared.mem.tag_pool,
        cached.max_locals as usize,
        (cached.max_stack as usize).max(16) + 8,
    );
    let frame = Frame::new_pooled_cached_compact(
        cached,
        &slots[..total_args],
        &mut thread.locals_pool,
        &mut thread.stacks_pool,
    );
    push_frame_and_fire_entry(shared.vm_identity, thread, frame);
    CachedCallResult::FramePushed
}

/// The questions that are constants of the callee, asked once here for every
/// door: nothing to intercept, no registered native shadowing the bytecode,
/// not `synchronized`, and a descriptor whose parameter tags are all inline.
///
/// `force_native_cache` is filled by the general path; until it has answered
/// `false` once — or if it answered `true` — this is not a call a door may
/// take, because only the general path knows how to route it to the native.
#[inline]
pub(super) fn callee_is_plain_bytecode(
    cached: &CachedBytecodeMethod,
    num_params: usize,
) -> Option<&cratonvm_jit_api::DescriptorFacts> {
    if cached.is_synchronized {
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
    if cratonvm_jit_api::descriptor_facts_disabled() {
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
    cached: &Arc<CachedBytecodeMethod>,
) -> bool {
    use std::sync::atomic::Ordering;
    const JIT_RETRY_STRIDE: u32 = 64;
    let threshold = crate::runtime::env_cache::jit_invocation_threshold();
    let cnt = cached.interp_invocations.fetch_add(1, Ordering::Relaxed) + 1;
    if cnt % INVOCATION_SYNC_EVERY == 0 {
        shared
            .jit
            .profile_store
            .add_invocations(cached.invoc_key(), INVOCATION_SYNC_EVERY);
    }
    let should_attempt =
        cnt >= threshold && (cnt == threshold || (cnt - threshold) % JIT_RETRY_STRIDE == 0);
    if !should_attempt {
        return true;
    }
    if !crate::runtime::env_cache::bg_compile() {
        // The inline upgrade needs the entry's gate; leave it to the general
        // dispatcher, which has it.
        return false;
    }
    ensure_bg_compiler_started(shared);
    let tiered_key = crate::jit::tiered::MethodKey::new(
        cached.class_name.as_ref(),
        cached.method_name.as_ref(),
        cached.method_descriptor.as_ref(),
    );
    let _ = shared
        .jit
        .tiered_manager
        .on_method_invocation_observed(&tiered_key, cnt as u64);
    true
}

/// Whether a JIT-compiled body exists for this callee. A door declines when
/// one does — `execute_jit_call` wants `Value` arguments and its own ABI
/// checks, which is the general dispatcher's job.
#[inline]
pub(super) fn callee_has_compiled_body(shared: &SharedVm, cached: &CachedBytecodeMethod) -> bool {
    let jit_generation = cratonvm_jit::jit_cache_generation();
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
    if !site_stats::on() || REPORTED.fetch_add(1, Ordering::Relaxed) >= 12 {
        return;
    }
    eprintln!("[invoke-door] {kind} declined: {why}");
}

/// Note a decline and return `None`, in one expression.
macro_rules! decline {
    ($kind:literal, $miss:expr, $why:literal) => {{
        site_stats::bump($miss);
        report_decline($kind, $why);
        return None;
    }};
}

// ── invokestatic (0xb8) ──────────────────────────────────────────────────

/// Monomorphic `invokestatic` fast door. See the module note for the contract.
#[inline]
pub(super) fn execute_invokestatic_fast_door(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    const MISS: usize = site_stats::DOOR_STATIC_MISS;
    if crate::classloading::any_class_redefined() {
        decline!("static", MISS, "a class was redefined");
    }
    let caller_class_id = thread.frames[frame_idx].class_id;
    let cached = match thread.invoke_cache.get(caller_class_id, cp_index, false) {
        Some(CachedInvokeTarget::Bytecode { cached, gate }) => {
            // A redefined target keeps the general path, which re-resolves.
            if gate.generation != 0 {
                decline!("static", MISS, "the target class has been redefined");
            }
            Arc::clone(cached)
        }
        Some(_) => decline!("static", MISS, "cached target is not plain bytecode"),
        None => decline!("static", MISS, "inline cache miss"),
    };
    if !cached.is_static {
        decline!("static", MISS, "cached target is not static");
    }
    let num_params = cached.num_params as usize;
    let Some(facts) = callee_is_plain_bytecode(&cached, num_params) else {
        decline!(
            "static",
            MISS,
            "callee is synchronized, native-backed, intercepted, or over-arity"
        );
    };
    // The loader-split guard the general path applies to every static hit.
    // Bitmap-gated: two relaxed loads when no defining loader is registered.
    if cached_static_owner_stale(shared, caller_class_id, &cached) {
        decline!("static", MISS, "loader-split owner");
    }
    if thread.frames.len() >= shared.config.max_stack_depth {
        decline!("static", MISS, "frame stack is full");
    }
    if !crate::runtime::env_cache::disable_jit() {
        if callee_has_compiled_body(shared, &cached) {
            decline!("static", MISS, "callee has a compiled body");
        }
        if !note_invocation_for_tierup(shared, &cached) {
            decline!("static", MISS, "an inline tier-up attempt is due");
        }
    }
    let mut slots = empty_arg_slots();
    if !read_args_verbatim(
        &thread.frames[frame_idx].stack,
        facts,
        num_params,
        false,
        &mut slots,
    ) {
        decline!("static", MISS, "an argument slot needs coercion");
    }
    site_stats::bump(site_stats::DOOR_STATIC_HIT);
    dbg_invoke_stats_record(0);
    Some(Ok(push_frame_verbatim(
        shared, thread, frame_idx, cached, &slots, num_params,
    )))
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
/// **This door does not count invocations, because the path it replaces does
/// not either.** Neither arm of `execute_invokevirtual_cached` that a cached
/// `invokespecial` can land in has a tier-up block: the `VirtualBytecode`
/// arm's is guarded by `!is_special`, and the `Bytecode` arm has none at all.
/// Adding counting here would widen which methods reach the optimizing tier,
/// which is a separate project with its own blast radius (see the "untaken
/// levers" note on
/// `docs/known-issues/perf/interpreted-invoke-cost-350ns-20260825.md`). A door
/// must not change tier-up policy on its way past.
///
/// An `invokespecial` site caches as either target shape depending on which
/// resolver filled it, so both are accepted. For `VirtualBytecode` the
/// receiver's class is re-checked against the entry exactly as the general
/// path does: the target of an `invokespecial` does not depend on the
/// receiver's class, but the entry records one and a mismatch is the general
/// path's polymorphic route.
#[inline]
pub(super) fn execute_nonvirtual_fast_door(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    is_special: bool,
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    const MISS: usize = site_stats::DOOR_SPECIAL_MISS;
    if crate::classloading::any_class_redefined() {
        decline!("special", MISS, "a class was redefined");
    }
    let caller_class_id = thread.frames[frame_idx].class_id;
    let (cached, expected_receiver) =
        match thread.invoke_cache.get(caller_class_id, cp_index, is_special) {
        Some(CachedInvokeTarget::Bytecode { cached, gate }) => {
            if gate.generation != 0 {
                decline!("special", MISS, "the target class has been redefined");
            }
            (Arc::clone(cached), None)
        }
        Some(CachedInvokeTarget::VirtualBytecode {
            cached,
            gate,
            receiver_class_id,
        }) if is_special => {
            if gate.generation != 0 {
                decline!("special", MISS, "the target class has been redefined");
            }
            (Arc::clone(cached), Some(*receiver_class_id))
        }
        Some(_) => decline!("special", MISS, "cached target is not plain bytecode"),
        None => decline!("special", MISS, "inline cache miss"),
    };
    if cached.is_static {
        decline!("special", MISS, "cached target is static");
    }
    // The owner re-check the general path makes on every special hit under
    // loader-aware resolution, which is the DEFAULT. `lookup_loader_initiated`
    // early-returns on a one-way latch when no user-defined loader has
    // registered a defining class, so this is one relaxed load for an
    // ordinary application and exactly the general path's cost otherwise.
    // A mismatch declines; the general dispatcher runs next and evicts.
    if is_special
        && crate::runtime::env_cache::loader_aware_resolution()
        && lookup_loader_initiated(shared, caller_class_id, cached.class_name.as_ref())
            .is_some_and(|owner_cid| owner_cid != cached.declaring_class_id)
    {
        decline!("special", MISS, "loader-split owner");
    }
    let num_params = cached.num_params as usize;
    let total_args = num_params + 1;
    let Some(facts) = callee_is_plain_bytecode(&cached, num_params) else {
        decline!(
            "special",
            MISS,
            "callee is synchronized, native-backed, intercepted, or over-arity"
        );
    };
    if thread.frames.len() >= shared.config.max_stack_depth {
        decline!("special", MISS, "frame stack is full");
    }
    let mut slots = empty_arg_slots();
    if !read_args_verbatim(
        &thread.frames[frame_idx].stack,
        facts,
        total_args,
        true,
        &mut slots,
    ) {
        decline!("special", MISS, "an argument slot needs coercion");
    }
    // A null receiver is the general path's business: it builds the helpful
    // NPE. `read_args_verbatim` accepts a null in slot 0 because a reference
    // parameter may legitimately be null; the receiver may not.
    let Some(recv_ptr) = slots[0].0.as_object_ptr() else {
        decline!("special", MISS, "null or non-object receiver");
    };
    if let Some(expected) = expected_receiver {
        if shared
            .mem
            .heap
            .is_object_address(recv_ptr as usize)
            .is_none()
        {
            decline!("special", MISS, "receiver is not a live heap object");
        }
        // SAFETY: `recv_ptr` is a registered object start on this heap.
        let header = unsafe { &*(recv_ptr as *const cratonvm_gc::ObjectHeader) };
        if header.class_id != expected {
            decline!("special", MISS, "receiver class differs from the entry");
        }
    }
    site_stats::bump(site_stats::DOOR_SPECIAL_HIT);
    dbg_invoke_stats_record(0);
    Some(Ok(push_frame_verbatim(
        shared, thread, frame_idx, cached, &slots, total_args,
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
        assert_eq!(slots[0].1, b'L', "receiver");
        assert_eq!(slots[1].1, b'I');
        assert_eq!(slots[2].1, b'J');
        // Without a receiver the same stack is an (I, J) static call.
        let stack = stack_with(&[
            (CompactValue::int(1), 0),
            (long_bits, crate::runtime::ValueStack::KIND_MARK_LONG),
        ]);
        let mut slots = empty_arg_slots();
        assert!(read_args_verbatim(&stack, &facts, 2, false, &mut slots));
        assert_eq!(slots[0].1, b'I');
        assert_eq!(slots[1].1, b'J');
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
}
