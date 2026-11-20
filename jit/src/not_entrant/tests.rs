use super::*;
use crate::ExecutableBuffer;

/// TF15-2 (round 16 wave 16, lane chains2): the re-dispatch line is due at
/// the first call and at every power of two, with the running count.
#[test]
fn redispatch_lines_are_due_at_powers_of_two() {
    let r = record(2, false);
    assert_eq!(r.redispatch_count(), 0);
    let due: Vec<u64> = (0..20).filter_map(|_| r.note_redispatch()).collect();
    assert_eq!(due, vec![1, 2, 4, 8, 16]);
    assert_eq!(r.redispatch_count(), 20);
}

/// HANDLER11-5 (JIT round 19 wave 12, lane handler12): a body a
/// caller-held site bound forwards only to a body bound the same way, in
/// both the supersede's static half and the stub's dynamic half.
#[test]
fn a_caller_held_sites_body_forwards_only_to_a_bound_body() {
    let old = answering(1);
    let new = answering(2);
    for body in [&old, &new] {
        body.stamp_published_key(&Arc::from("h12/S"), &Arc::from("m"), &Arc::from("()I"), 7);
    }
    let mut old = old;
    let mut new = new;
    old.entry_java_args = 0;
    new.entry_java_args = 0;
    assert!(
        caller_held_forward_refusal(false, &new).is_none(),
        "an unbound body forwards"
    );
    old.mark_bound_by_a_caller_held_site();
    assert!(caller_held_forward_refusal(true, &new).is_some());
    assert!(
        !supersede_forward_admissible(&old, &new),
        "the supersede leaves a caller-held site's body entrant"
    );
    new.mark_bound_by_a_caller_held_site();
    assert!(caller_held_forward_refusal(true, &new).is_none());
    assert!(supersede_forward_admissible(&old, &new));
    let mut rec = record(0, false);
    rec.caller_held_bound = true;
    let unbound = answering(3);
    unbound.stamp_published_key(&Arc::from("h12/S"), &Arc::from("m"), &Arc::from("()I"), 7);
    let mut unbound = unbound;
    unbound.entry_java_args = 0;
    assert_eq!(
        rec.forward_target_refusal(&unbound),
        Some("the patched body is a caller-held site's; target not bound by one")
    );
}

#[test]
fn a_guarded_virtual_thread_entry_cannot_forward_to_an_unguarded_body() {
    let mut old = answering(1);
    let mut new = answering(2);
    old.entry_java_args = 0;
    new.entry_java_args = 0;
    old.vthread_boundary_guards = true;
    let mut rec = record(0, false);
    rec.vthread_boundary_guards = true;
    assert!(!supersede_forward_admissible(&old, &new));
    assert_eq!(
        rec.forward_target_refusal(&new),
        Some("target lacks virtual-thread boundaries")
    );
    new.vthread_boundary_guards = true;
    assert!(supersede_forward_admissible(&old, &new));
}

/// `NOP5; MOV EAX, 7; RET`: a body that answers 7 until it is patched.
fn seven() -> CompiledMethod {
    answering(7)
}

/// `NOP5; MOV EAX, value; RET`.
fn answering(value: u8) -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&ENTRY_PATCH_PAD);
    buf.emit(&[0xB8, value, 0x00, 0x00, 0x00, 0xC3]);
    CompiledMethod::new(buf)
}

fn record(num_java_args: usize, needs_context: bool) -> Box<NotEntrantRecord> {
    Box::new(NotEntrantRecord::new(
        &Arc::from("i22l6/C"),
        &Arc::from("m"),
        &Arc::from("(JJJJJJJJ)J"),
        0x2206,
        true,
        0,
        needs_context,
        num_java_args,
    ))
}

/// Stands in for the VM's `jit_not_entrant_entry`: folds what the stub
/// passed into one number, so the call's result proves every argument
/// arrived in order, register-passed and stack-passed alike.
extern "C" fn fold_args(record: i64, regs: i64, stack_args: i64) -> i64 {
    // SAFETY: the stub passes the live record it baked and the spill and
    // stack pointers of the call it intercepted.
    let rec = unsafe { &*(record as *const NotEntrantRecord) };
    // SAFETY: as above.
    let args = unsafe { rec.java_args(regs as *const i64, stack_args as *const i64) };
    // SAFETY: as above.
    let ctx = unsafe { rec.context_arg(regs as *const i64) }.unwrap_or(0);
    let folded: i64 = args
        .iter()
        .enumerate()
        .map(|(i, v)| v * (i as i64 + 1))
        .sum();
    folded + 1000 * rec.num_java_args as i64 + 100_000 * ctx
}

#[test]
fn a_patched_entry_hands_every_argument_to_the_helper_and_returns_its_answer() {
    let body = seven();
    // SAFETY: the body ignores its arguments and returns 7.
    let before = unsafe { body.try_call(&[1, 2, 3, 4, 5, 6, 7, 8]) }.expect("call");
    assert_eq!(before, 7);
    assert!(!body.is_not_entrant());
    body.install_not_entrant_stub(record(8, false), fold_args as usize)
        .expect("patch");
    assert!(body.is_not_entrant());
    assert_eq!(body.code_bytes()[0], 0xE9, "the entry is a JMP now");
    // SAFETY: the entry now jumps to the stub, which calls `fold_args`.
    let after = unsafe { body.try_call(&[1, 2, 3, 4, 5, 6, 7, 8]) }.expect("call");
    // 1*1 + 2*2 + ... + 8*8 = 204, plus the count.
    assert_eq!(after, 204 + 8000);
    // Again: the patch is permanent, and a second patch is refused
    // without touching it.
    assert_eq!(
        // SAFETY: the installed stub and its argument layout are unchanged from the call above.
        unsafe { body.try_call(&[1, 2, 3, 4, 5, 6, 7, 8]) }.expect("call"),
        8204
    );
    assert_eq!(
        body.install_not_entrant_stub(record(8, false), fold_args as usize),
        Err(NotEntrantRefusal::NoEntryPad)
    );
}

#[test]
fn a_context_body_passes_the_context_before_its_java_arguments() {
    let body = seven();
    body.install_not_entrant_stub(record(2, true), fold_args as usize)
        .expect("patch");
    // SAFETY: the entry jumps to the stub, which calls `fold_args`.
    let got = unsafe { body.try_call(&[3, 10, 20]) }.expect("call");
    assert_eq!(got, 10 + 2 * 20 + 2000 + 300_000);
}

#[test]
fn a_body_without_the_pad_or_an_identity_is_left_alone() {
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xB8, 0x07, 0x00, 0x00, 0x00, 0xC3, 0x90, 0x90]);
    let bare = CompiledMethod::new(buf);
    assert_eq!(
        bare.install_not_entrant_stub(record(0, false), fold_args as usize),
        Err(NotEntrantRefusal::NoEntryPad)
    );
    // SAFETY: unpatched: MOV EAX, 7; RET.
    assert_eq!(unsafe { bare.try_call(&[]) }.expect("call"), 7);
    let hook = NotEntrantHook {
        helper: fold_args as usize,
        vm_ptr: 0,
    };
    assert_eq!(
        seven().make_not_entrant(hook),
        Err(NotEntrantRefusal::NoIdentity),
        "never published: nothing names the method"
    );
}

/// The redefinition pass: a stale body is collected while published,
/// refused while it still is, and patched once the eviction withdrew it.
#[test]
fn a_withdrawn_stale_body_is_made_not_entrant_and_a_published_one_is_not() {
    extern "C" fn kind(record: i64, _regs: i64, _stack: i64) -> i64 {
        // SAFETY: the stub passes the live record it baked.
        let rec = unsafe { &*(record as *const NotEntrantRecord) };
        1000 + i64::from(rec.info.invoke_kind)
    }
    let cache = JitCache::new();
    let class = cratonvm_types::ClassId::new(0x2206);
    let mut body = seven();
    body.entry_java_args = 0;
    cache.put(
        Arc::from("i22l6/C"),
        Arc::from("m"),
        Arc::from("()I"),
        class,
        body,
    );
    let stale = cache.redefinition_stale_bodies(class, "i22l6/C", false);
    assert_eq!(stale.len(), 1);
    assert_eq!(cache.make_not_entrant(&stale), 0, "not armed: nothing");
    cache.arm_not_entrant(NotEntrantHook {
        helper: kind as usize,
        vm_ptr: 0,
    });
    assert_eq!(cache.make_not_entrant(&stale), 0, "still published");
    assert!(!stale[0].is_not_entrant());
    let (withdrawn, _) = cache.invalidate_for_redefinition(class, "i22l6/C");
    assert_eq!(withdrawn, 1);
    assert_eq!(cache.make_not_entrant(&stale), 1);
    assert!(stale[0].is_not_entrant());
    // SAFETY: the entry jumps to the stub, which calls `kind`.
    let got = unsafe { stale[0].try_call(&[]) }.expect("call");
    assert_eq!(
        got, 1003,
        "a static method is re-dispatched as invokestatic"
    );
    assert_eq!(cache.not_entrant_counts().0, 1);
    // Wave 23: withdrawn by the redefinition, for the back-edge poll.
    assert!(stale[0].is_withdrawn_by_redefinition());
    assert_eq!(cache.bodies_withdrawn_by_redefinition(), 1);
    for b in stale {
        crate::defer_jit_owner(Some(b));
    }
}

/// Round 16 wave 15 (lane trapfix): a body a deoptimization's eviction
/// withdrew (`JitCache::remove`) is made not entrant without the
/// redefinition's bookkeeping -- not marked withdrawn by a redefinition,
/// the redefinition count unmoved -- and a published one is still refused.
#[test]
fn an_evicted_body_is_made_not_entrant_without_the_redefinition_marks() {
    extern "C" fn kind(record: i64, _regs: i64, _stack: i64) -> i64 {
        // SAFETY: the stub passes the live record it baked.
        let rec = unsafe { &*(record as *const NotEntrantRecord) };
        2000 + i64::from(rec.info.invoke_kind)
    }
    let cache = JitCache::new();
    cache.arm_not_entrant(NotEntrantHook {
        helper: kind as usize,
        vm_ptr: 0,
    });
    let class = cratonvm_types::ClassId::new(0x1615);
    let mut body = seven();
    body.entry_java_args = 0;
    cache.put(
        Arc::from("r16w15/C"),
        Arc::from("m"),
        Arc::from("()I"),
        class,
        body,
    );
    let live = cache.get("r16w15/C", "m", "()I", class).expect("published");
    assert_eq!(
        cache.make_not_entrant_after_eviction(std::slice::from_ref(&live)),
        0,
        "still published"
    );
    assert!(!live.is_not_entrant());
    cache.remove("r16w15/C", "m", "()I", class);
    assert!(cache.get("r16w15/C", "m", "()I", class).is_none());
    assert_eq!(
        cache.make_not_entrant_after_eviction(std::slice::from_ref(&live)),
        1
    );
    assert!(live.is_not_entrant());
    // SAFETY: the entry jumps to the stub, which calls `kind`.
    let got = unsafe { live.try_call(&[]) }.expect("call");
    assert_eq!(
        got, 2003,
        "a static method is re-dispatched as invokestatic"
    );
    assert!(!live.is_withdrawn_by_redefinition());
    assert_eq!(cache.bodies_withdrawn_by_redefinition(), 0);
    // Idempotent: a second eviction pass patches nothing more.
    assert_eq!(
        cache.make_not_entrant_after_eviction(std::slice::from_ref(&live)),
        0
    );
    assert_eq!(cache.not_entrant_counts().0, 1);
    crate::defer_jit_owner(Some(live));
}


/// Round 16 wave 15 (lane trapfix): a refused eviction patch does not hold
/// the redefinition's unpatched constant-pool floor.
#[test]
fn a_refused_eviction_patch_holds_no_unpatched_stamp_floor() {
    let cache = JitCache::new();
    cache.arm_not_entrant(NotEntrantHook {
        helper: fold_args as usize,
        vm_ptr: 0,
    });
    let class = cratonvm_types::ClassId::new(0x1616);
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xB8, 0x07, 0x00, 0x00, 0x00, 0xC3, 0x90, 0x90]);
    let mut bare = CompiledMethod::new(buf);
    bare.entry_java_args = 0;
    bare.compile_cp_stamp = Some(17);
    cache.put(
        Arc::from("r16w15/D"),
        Arc::from("m"),
        Arc::from("()I"),
        class,
        bare,
    );
    let live = cache.get("r16w15/D", "m", "()I", class).expect("published");
    cache.remove("r16w15/D", "m", "()I", class);
    assert_eq!(
        cache.make_not_entrant_after_eviction(std::slice::from_ref(&live)),
        0,
        "no entry pad: refused"
    );
    assert_eq!(cache.min_unpatched_cp_stamp(), None);
    assert!(!live.is_withdrawn_by_redefinition());
    crate::defer_jit_owner(Some(live));
}

/// Wave 25 (lane L6): a body the pass retires but cannot patch holds the
/// cache's unpatched floor at its compile's constant-pool stamp, once,
/// for as long as it lives
/// (`interpreter-L6-a-refused-not-entrant-patch-leaves-an-old-body-below-the-census-floor-FIXED`).
#[test]
fn a_refused_body_holds_the_unpatched_stamp_floor_while_it_lives() {
    let cache = JitCache::new();
    cache.arm_not_entrant(NotEntrantHook {
        helper: fold_args as usize,
        vm_ptr: 0,
    });
    let class = cratonvm_types::ClassId::new(0x2506);
    // No entry pad: the patch is refused (`NoEntryPad`).
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xB8, 0x07, 0x00, 0x00, 0x00, 0xC3, 0x90, 0x90]);
    let mut bare = CompiledMethod::new(buf);
    bare.entry_java_args = 0;
    bare.compile_cp_stamp = Some(41);
    cache.put(
        Arc::from("i25l6/C"),
        Arc::from("m"),
        Arc::from("()I"),
        class,
        bare,
    );
    let stale = cache.redefinition_stale_bodies(class, "i25l6/C", false);
    assert_eq!(stale.len(), 1);
    let _ = cache.invalidate_for_redefinition(class, "i25l6/C");
    assert_eq!(cache.min_unpatched_cp_stamp(), None);
    assert_eq!(cache.make_not_entrant(&stale), 0, "refused");
    assert!(stale[0].is_withdrawn_by_redefinition());
    assert_eq!(cache.min_unpatched_cp_stamp(), Some(41));
    assert_eq!(cache.make_not_entrant(&stale), 0);
    assert_eq!(cache.unpatched_bodies.lock().len(), 1, "once per body");
    // A lower stamp wins while its body lives, and is forgotten after.
    let mut older = seven();
    older.compile_cp_stamp = Some(3);
    let older = Arc::new(older);
    cache.note_unpatched(&older);
    assert_eq!(cache.min_unpatched_cp_stamp(), Some(3));
    drop(older);
    assert_eq!(cache.min_unpatched_cp_stamp(), Some(41));
    for b in stale {
        crate::defer_jit_owner(Some(b));
    }
}

/// Wave 25 (lane L6), stage 1 of
/// `i20-L1-proposal-per-body-copied-bytecode-dependencies`: a copy the VM
/// notes while a compilation is open on the thread is carried by every
/// body built inside it (a nested compile shares the list), by none built
/// before or after, and a scoped redefinition of the copied class
/// collects the body that carries it.
#[test]
fn a_copy_noted_in_an_open_compile_is_carried_by_its_bodies() {
    let cache = JitCache::new();
    let copied = cratonvm_types::ClassId::new(0x2507);
    let caller = cratonvm_types::ClassId::new(0x2508);
    let before = seven();
    let (inside, nested) = {
        let _outer = crate::open_compile_epoch_witness();
        cache.note_bytecode_copied(copied);
        let nested = {
            let _inner = crate::open_compile_epoch_witness();
            seven()
        };
        (seven(), nested)
    };
    let after = seven();
    assert!(inside.copied_bytecode_of(copied));
    assert!(
        nested.copied_bytecode_of(copied),
        "a nested compile shares the list"
    );
    assert!(!inside.copied_bytecode_of(caller));
    assert!(!before.copied_bytecode_of(copied));
    assert!(
        !after.copied_bytecode_of(copied),
        "the outermost witness closed the list"
    );
    assert_eq!(
        cache.bytecode_was_copied(copied),
        !crate::SCOPE_REDEFINITIONS_BY_COPIED_CLASSES,
        "the per-VM mark, unless stage 3 is on"
    );
    // Noted with no compilation open: always the per-VM mark.
    let unscoped = cratonvm_types::ClassId::new(0x2509);
    cache.note_bytecode_copied(unscoped);
    assert!(cache.bytecode_was_copied(unscoped));

    let mut inside = inside;
    inside.entry_java_args = 0;
    cache.put(
        Arc::from("i25l6/Caller"),
        Arc::from("run"),
        Arc::from("()I"),
        caller,
        inside,
    );
    let stale = cache.redefinition_stale_bodies(copied, "i25l6/Copied", false);
    assert_eq!(stale.len(), 1, "the body that copied the class");
    assert!(cache
        .redefinition_stale_bodies(unscoped, "i25l6/Other", false)
        .is_empty());
    for b in stale {
        crate::defer_jit_owner(Some(b));
    }
}

/// A tier-up leaves the old body out of every map, but a caller that
/// baked it keeps entering it; the scan finds it through that caller's
/// roots and patches it with the published one.
#[test]
fn a_superseded_body_only_a_baked_caller_reaches_is_made_not_entrant_too() {
    extern "C" fn answer(record: i64, _regs: i64, _stack: i64) -> i64 {
        // SAFETY: the stub passes the live record it baked.
        let rec = unsafe { &*(record as *const NotEntrantRecord) };
        2000 + i64::from(rec.info.invoke_kind)
    }
    let cache = JitCache::new();
    let target = "i22l6/Superseded";
    let caller = "i22l6/SupersededCaller";
    let redefined = cratonvm_types::ClassId::new(0x2207);
    let callers = cratonvm_types::ClassId::new(0x2208);
    let put =
        |class: &str, name: &str, id: cratonvm_types::ClassId, mut body: CompiledMethod| {
            body.entry_java_args = 0;
            cache.put(
                Arc::from(class),
                Arc::from(name),
                Arc::from("()I"),
                id,
                body,
            );
        };
    put(target, "own", redefined, seven());
    let old = cache
        .get(target, "own", "()I", redefined)
        .expect("published");
    let mut direct = seven();
    // Cast: an address, as the compile doors record a baked callee.
    direct._direct_callee_entries = vec![old.entry_ptr() as usize];
    put(caller, "direct", callers, direct);
    assert!(
        cache.get(caller, "direct", "()I", callers).is_some(),
        "the caller publishes, rooting the old body"
    );
    // The tier-up: a new body under the same key.
    put(target, "own", redefined, seven());
    let stale = cache.redefinition_stale_bodies(redefined, target, false);
    assert_eq!(
        stale.len(),
        2,
        "the published body, and the superseded one its caller roots"
    );
    cache.arm_not_entrant(NotEntrantHook {
        helper: answer as usize,
        vm_ptr: 0,
    });
    // Arming may already have redirected the displaced body; the
    // invalidation must patch the remaining published body as well.
    let redirected_on_arm = usize::from(old.is_not_entrant());
    let _ = cache.invalidate_for_redefinition(redefined, target);
    assert_eq!(cache.make_not_entrant(&stale) + redirected_on_arm, 2);
    assert!(old.is_not_entrant());
    assert!(old.retired.load(std::sync::atomic::Ordering::Acquire));
    // SAFETY: the old entry now jumps to its stub, which calls `answer`.
    assert_eq!(unsafe { old.try_call(&[]) }.expect("call"), 2003);
    crate::defer_jit_owner(Some(old));
    for b in stale {
        crate::defer_jit_owner(Some(b));
    }
}

/// Interpreter round i1 wave 23, lane L6: the stub's forwarding prefix is
/// as long as its documentation says, and a buffer emitted to its last
/// byte still has room for the whole stub past a 16-byte boundary.
#[test]
fn the_forwarding_stub_fits_the_tail_of_a_full_buffer() {
    let plain = not_entrant_stub_bytes(0x1111, 0x2222, 0, false);
    let forwarding = not_entrant_stub_bytes(0x1111, 0x2222, 0x3333, false);
    // Wave 25: the one-compare form, for a layout with the two flags side
    // by side -- 32 bytes, both early-outs on the slow path.
    let combined = not_entrant_stub_bytes(0x1111, 0x2222, 0x3333, true);
    assert_eq!(combined.len(), plain.len() + 32);
    assert_eq!(
        &combined[22..27],
        &[0x66, 0x41, 0x83, 0x3A, 0x00],
        "CMP WORD [R10], 0"
    );
    assert_eq!(
        &combined[29..32],
        &[0x41, 0xFF, 0x23],
        "JMP [R11] closes it"
    );
    for (at, op) in [(16usize, 0x74u8), (27, 0x75)] {
        assert_eq!(combined[at], op);
        assert_eq!(at + 2 + usize::from(combined[at + 1]), 32);
    }
    assert_eq!(&combined[32..], &plain[..]);
    assert_eq!(
        not_entrant_stub_bytes(0x1111, 0x2222, 0, true),
        plain,
        "no forward word, no prefix"
    );
    assert_eq!(forwarding.len(), plain.len() + 41);
    assert!(forwarding.len() <= NOT_ENTRANT_STUB_MAX);
    assert_eq!(&forwarding[..2], &[0x49, 0xBB], "MOV R11, imm64 opens it");
    assert_eq!(
        &forwarding[38..41],
        &[0x41, 0xFF, 0x23],
        "JMP [R11] closes it"
    );
    // Every early-out lands on the slow path's first byte.
    for (at, op) in [(16usize, 0x74u8), (26, 0x75), (36, 0x75)] {
        assert_eq!(forwarding[at], op);
        assert_eq!(at + 2 + usize::from(forwarding[at + 1]), 41);
    }
    assert_eq!(&forwarding[41..], &plain[..]);
    // 49 bytes: the next 16-byte boundary is 15 bytes further on.
    let mut buf = ExecutableBuffer::new(49).expect("executable buffer");
    buf.emit(&ENTRY_PATCH_PAD);
    buf.emit(&[0xB8, 0x07, 0x00, 0x00, 0x00, 0xC3]);
    buf.emit(&[0x90; 38]);
    assert_eq!(buf.pos(), 49);
    let full = CompiledMethod::new(buf);
    full.install_not_entrant_stub(record(2, false), fold_args as usize)
        .expect("the tail holds the forwarding stub");
    // SAFETY: the entry jumps to the stub; no forward yet, so the helper.
    assert_eq!(unsafe { full.try_call(&[10, 20]) }.expect("call"), 2050);
}

/// The forward: once the record names the body now published under its
/// key, the stub jumps to it (the caller gets that body's answer, not the
/// helper's), and falls back to the helper as soon as that body is
/// retired or superseded.
#[test]
fn a_patched_entry_forwards_to_the_current_body_until_it_is_withdrawn() {
    use std::sync::atomic::Ordering;
    let cache = JitCache::new();
    let class = cratonvm_types::ClassId::new(0x2206);
    let mut nine = answering(9);
    nine.entry_java_args = 2;
    cache.put(
        Arc::from("i22l6/C"),
        Arc::from("m"),
        Arc::from("(JJJJJJJJ)J"),
        class,
        nine,
    );
    let current = cache
        .get("i22l6/C", "m", "(JJJJJJJJ)J", class)
        .expect("published");
    let old = seven();
    old.install_not_entrant_stub(record(2, false), fold_args as usize)
        .expect("patch");
    // SAFETY: the entry jumps to the stub, whose forward word is empty.
    assert_eq!(unsafe { old.try_call(&[10, 20]) }.expect("call"), 2050);
    let rec = old.not_entrant.get().expect("patched");
    assert!(rec.forward_to(&current));
    assert!(rec.forward_to(&current), "idempotent");
    assert!(rec
        .forward_target()
        .is_some_and(|t| Arc::ptr_eq(&t, &current)));
    // SAFETY: the stub now jumps to `current`: MOV EAX, 9; RET.
    assert_eq!(unsafe { old.try_call(&[10, 20]) }.expect("call"), 9);
    current.retired.store(true, Ordering::SeqCst);
    // SAFETY: retired: the stub's slow path, the helper.
    assert_eq!(unsafe { old.try_call(&[10, 20]) }.expect("call"), 2050);
    current.retired.store(false, Ordering::SeqCst);
    current.superseded.store(true, Ordering::SeqCst);
    // SAFETY: as above.
    assert_eq!(unsafe { old.try_call(&[10, 20]) }.expect("call"), 2050);
    assert!(
        !rec.forward_to(&current),
        "a superseded body is not admitted"
    );
    current.superseded.store(false, Ordering::SeqCst);
    // SAFETY: back: the same forward serves again.
    assert_eq!(unsafe { old.try_call(&[10, 20]) }.expect("call"), 9);
    drop(old);
    crate::defer_jit_owner(Some(current));
}


/// hm5: the supersede patch is asked only for a C2 body the stub could
/// forward to -- same arity, same ABI, not wrapped, no `invokedynamic`
/// trap, a different body -- and never of an already patched C1 body.
#[test]
fn hm5_a_supersede_is_patched_only_where_its_stub_can_forward() {
    let with_args = |mut body: CompiledMethod, n: u16| {
        body.entry_java_args = n;
        body
    };
    let old = with_args(answering(7), 2);
    let new = with_args(answering(9), 2);
    assert!(super::supersede_forward_admissible(&old, &new));
    assert!(
        !super::supersede_forward_admissible(&old, &old),
        "the same body"
    );
    assert!(
        !super::supersede_forward_admissible(&old, &with_args(answering(9), 3)),
        "another arity"
    );
    assert!(
        !super::supersede_forward_admissible(&old, &with_args(answering(9), u16::MAX)),
        "an unstamped arity"
    );
    let mut ctx_buf = ExecutableBuffer::new(64).expect("executable buffer");
    ctx_buf.emit(&ENTRY_PATCH_PAD);
    ctx_buf.emit(&[0xB8, 0x09, 0x00, 0x00, 0x00, 0xC3]);
    let ctx = with_args(CompiledMethod::new_with_context(ctx_buf), 2);
    assert!(
        !super::supersede_forward_admissible(&old, &ctx),
        "another ABI (a context pointer)"
    );
    let mut wrapped = with_args(answering(9), 2);
    wrapped.requires_wrapped_entry = true;
    assert!(
        !super::supersede_forward_admissible(&old, &wrapped),
        "wrapped"
    );
    let mut indy = with_args(answering(9), 2);
    indy.has_indy_trap = true;
    assert!(
        !super::supersede_forward_admissible(&old, &indy),
        "indy trap"
    );
    let patched = with_args(seven(), 2);
    patched
        .install_not_entrant_stub(record(2, false), fold_args as usize)
        .expect("patch");
    assert!(
        !super::supersede_forward_admissible(&patched, &new),
        "already not entrant"
    );
}

/// Admission: another key, another arity or another ABI never forwards,
/// and neither does a target that reaches the patched body through its
/// baked callee roots (a strong-reference cycle).
#[test]
fn a_forward_is_refused_for_another_method_shape_or_a_cycle() {
    let cache = JitCache::new();
    let class = cratonvm_types::ClassId::new(0x2206);
    let put = |name: &str, args: u16, mut body: CompiledMethod| {
        body.entry_java_args = args;
        cache.put(
            Arc::from("i22l6/C"),
            Arc::from(name),
            Arc::from("(JJJJJJJJ)J"),
            class,
            body,
        );
        cache
            .get("i22l6/C", name, "(JJJJJJJJ)J", class)
            .expect("published")
    };
    let other_name = put("other", 2, answering(9));
    let other_arity = put("m", 3, answering(9));
    let old = seven();
    old.install_not_entrant_stub(record(2, false), fold_args as usize)
        .expect("patch");
    let rec = old.not_entrant.get().expect("patched");
    assert!(!rec.forward_to(&other_name), "another method");
    assert!(!rec.forward_to(&other_arity), "another arity");
    let mut ctx_buf = ExecutableBuffer::new(64).expect("executable buffer");
    ctx_buf.emit(&ENTRY_PATCH_PAD);
    ctx_buf.emit(&[0xB8, 0x09, 0x00, 0x00, 0x00, 0xC3]);
    let ctx = put("m", 2, CompiledMethod::new_with_context(ctx_buf));
    assert!(!rec.forward_to(&ctx), "another ABI (a context pointer)");
    assert!(rec.forward_target().is_none());

    // The cycle: a published callee `own`, patched, and a body for the
    // record's key that baked a call to it.
    let own = put("own", 2, answering(5));
    own.install_not_entrant_stub(record(2, false), fold_args as usize)
        .expect("patch");
    let mut caller = answering(9);
    // Cast: an address, as the compile doors record a baked callee.
    caller._direct_callee_entries = vec![own.entry_ptr() as usize];
    let caller = put("m", 2, caller);
    assert!(
        caller
            ._direct_callee_roots
            .iter()
            .any(|r| Arc::ptr_eq(r.arc(), &own)),
        "publication roots the baked callee"
    );
    let own_rec = own.not_entrant.get().expect("patched");
    assert!(
        !own_rec.forward_to(&caller),
        "`caller` reaches the patched body: a cycle"
    );
    for b in [other_name, other_arity, ctx, own, caller] {
        crate::defer_jit_owner(Some(b));
    }
    drop(old);
}

/// str16: through the cache, the same cycle IS forwarded -- registered by
/// the target's entry and released when the target is superseded, so the
/// cycle cannot keep it mapped -- and the stub's word goes back to `0`.
#[test]
fn str16_a_cycle_forward_is_installed_and_released_on_supersede() {
    let cache = JitCache::new();
    // The class id `record` names.
    let class = cratonvm_types::ClassId::new(0x2206);
    let put = |name: &str, body: CompiledMethod| {
        let mut body = body;
        body.entry_java_args = 2;
        cache.put(
            Arc::from("i22l6/C"),
            Arc::from(name),
            Arc::from("(JJJJJJJJ)J"),
            class,
            body,
        );
        cache
            .get("i22l6/C", name, "(JJJJJJJJ)J", class)
            .expect("published")
    };
    let own = put("own", answering(5));
    own.install_not_entrant_stub(record(2, false), fold_args as usize)
        .expect("patch");
    let mut caller = answering(9);
    // Cast: an address, as the compile doors record a baked callee.
    caller._direct_callee_entries = vec![own.entry_ptr() as usize];
    let caller = put("m", caller);
    let own_rec = own.not_entrant.get().expect("patched");
    assert!(!own_rec.forward_to(&caller), "the bare form still refuses");
    let (installed_before, released_before) = cache.cycle_forward_census();
    assert!(
        cache.forward_not_entrant(own_rec, &caller),
        "the cache forwards across the cycle"
    );
    assert!(own_rec
        .forward_target()
        .is_some_and(|t| Arc::ptr_eq(&t, &caller)));
    assert!(
        cache.forward_not_entrant(own_rec, &caller),
        "idempotent once installed"
    );
    assert!(cache.cycle_forward_census().0 > installed_before);

    // A newer body for the record's key supersedes `caller`: the forward
    // lets go of it.
    let newer = put("m", answering(11));
    assert!(own_rec.forward_target().is_none(), "released on supersede");
    assert!(cache.cycle_forward_census().1 > released_before);
    assert!(
        cache.forward_not_entrant(own_rec, &newer),
        "and forwards to the newer body (no cycle now)"
    );
    assert!(own_rec
        .forward_target()
        .is_some_and(|t| Arc::ptr_eq(&t, &newer)));
    for b in [own, caller, newer] {
        crate::defer_jit_owner(Some(b));
    }
}

/// `whole_cache`: every body is collected, not only the class's own.
#[test]
fn a_whole_cache_redefinition_collects_every_body() {
    let cache = JitCache::new();
    let redefined = cratonvm_types::ClassId::new(0x2309);
    let other = cratonvm_types::ClassId::new(0x230A);
    for (class, id) in [("i23l6/Redefined", redefined), ("i23l6/Other", other)] {
        let mut body = seven();
        body.entry_java_args = 0;
        cache.put(Arc::from(class), Arc::from("m"), Arc::from("()I"), id, body);
    }
    let scoped = cache.redefinition_stale_bodies(redefined, "i23l6/Redefined", false);
    assert_eq!(scoped.len(), 1);
    let whole = cache.redefinition_stale_bodies(redefined, "i23l6/Redefined", true);
    assert_eq!(whole.len(), 2);
    assert!(JitCache::is_stale_for_redefinition(
        &scoped[0],
        redefined,
        "i23l6/Redefined"
    ));
    assert_eq!(cache.mark_withdrawn_by_redefinition(&whole), 2);
    assert_eq!(cache.mark_withdrawn_by_redefinition(&whole), 0, "once each");
    assert_eq!(cache.bodies_withdrawn_by_redefinition(), 2);
    for b in scoped.into_iter().chain(whole) {
        crate::defer_jit_owner(Some(b));
    }
}

/// Interpreter round i1 wave 24, lane L6: a whole-cache redefinition
/// withdraws every body whose compilation began before its flush, whether
/// or not anything can reach it (the VM's optimizing OSR body that the
/// memo already forgot), and no body compiled after it; the answer is
/// recorded on the body for the exit sinks.
#[test]
fn a_whole_cache_redefinition_withdraws_every_body_compiled_before_its_flush() {
    let cache = JitCache::new();
    let before_flush = seven();
    cache.note_whole_cache_redefinition();
    assert!(
        !cache.body_withdrawn_by_redefinition(&before_flush),
        "no flush yet: no barrier"
    );
    let count = cache.bodies_withdrawn_by_redefinition();
    cache.clear_all();
    cache.note_whole_cache_redefinition();
    assert_eq!(
        cache.bodies_withdrawn_by_redefinition(),
        count + 1,
        "the VM's loop-exit handshake trigger moves"
    );
    assert!(
        !before_flush.is_withdrawn_by_redefinition(),
        "never scanned"
    );
    assert!(cache.body_withdrawn_by_redefinition(&before_flush));
    assert!(before_flush.is_withdrawn_by_redefinition(), "and recorded");
    let after_flush = seven();
    assert!(!cache.body_withdrawn_by_redefinition(&after_flush));
}

/// Interpreter round i1 wave 24, lane L6: mappings that follow one another
/// page for page are patched under one protection change; a gap, or a
/// mapping not starting on a page, ends the run (and on Windows every
/// mapping is its own run: `VirtualProtect` cannot span two allocations).
#[test]
fn adjacent_mappings_are_patched_as_one_run() {
    let p = X86_64_BASE_PAGE;
    let patches = [
        (p, 100),
        (2 * p, p + 1),
        (4 * p, 8),
        (6 * p, 8),
        (7 * p + 8, 8),
    ];
    let runs = adjacent_patch_runs(&patches);
    if cfg!(target_os = "windows") {
        assert_eq!(
            runs,
            vec![
                (0, 1, p, 2 * p),
                (1, 2, 2 * p, 4 * p),
                (2, 3, 4 * p, 5 * p),
                (3, 4, 6 * p, 7 * p),
                (4, 5, 7 * p + 8, 8 * p),
            ]
        );
    } else {
        assert_eq!(
            runs,
            vec![
                (0, 3, p, 5 * p),
                (3, 4, 6 * p, 7 * p),
                (4, 5, 7 * p + 8, 8 * p)
            ]
        );
    }
    assert!(adjacent_patch_runs(&[]).is_empty());
}

/// `NOP5; MOV R11, flag; TEST BYTE [R11], 0xFF; JNZ +0; MOV EAX, 7; RET`:
/// a body with one exit poll in the IR tier's outlined form, its site
/// recorded as both x86-64 tiers record theirs (the condition-code byte,
/// offset 20).
fn with_exit_poll(flag: &'static u8) -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&ENTRY_PATCH_PAD);
    buf.emit(&[0x49, 0xBB]);
    // Cast: the flag's address, baked as a 64-bit immediate.
    buf.emit(&(flag as *const u8 as u64).to_le_bytes());
    buf.emit(&[0x41, 0xF6, 0x03, 0xFF]);
    buf.emit(&[0x0F, 0x85, 0x00, 0x00, 0x00, 0x00]);
    buf.emit(&[0xB8, 0x07, 0x00, 0x00, 0x00, 0xC3]);
    let mut body = CompiledMethod::new(buf);
    body.entry_java_args = 0;
    body.exit_poll_sites = vec![20];
    // Cast: the flag's address, as the tiers record it.
    body.exit_poll_flag = flag as *const u8 as usize;
    body
}

/// Interpreter round i1 wave 26, lane L6: a body superseded while a frame
/// still runs it (here: held by the test, as a running frame's guard holds
/// it) is in no map and no baked root, yet a redefinition of a class it
/// spliced finds it, withdraws it and forces its exit poll; a forcing
/// that is refused is what keeps the loop-exit retries.
#[test]
fn a_superseded_body_a_frame_still_runs_is_found_and_its_polls_forced() {
    let flag: &'static u8 = Box::leak(Box::new(0u8));
    let cache = JitCache::new();
    let caller = cratonvm_types::ClassId::new(0x2661);
    let spliced = cratonvm_types::ClassId::new(0x2662);
    let publish = |body: CompiledMethod| {
        cache.put(
            Arc::from("i26l6/Caller"),
            Arc::from("loop"),
            Arc::from("()I"),
            caller,
            body,
        );
        cache.get("i26l6/Caller", "loop", "()I", caller)
    };
    let mut splicing = with_exit_poll(flag);
    splicing.inlined_methods = vec![("i26l6/Spliced".into(), "value".into(), "()I".into())];
    let running = publish(splicing).expect("published");
    // A tier-up publishes a new body under the same key: `running` is
    // superseded, out of every map, and reachable from nothing baked.
    let current = publish(with_exit_poll(flag)).expect("the new body");
    assert!(!Arc::ptr_eq(&running, &current));
    let before = cache.bodies_withdrawn_by_redefinition();
    let stale = cache.redefinition_stale_bodies(spliced, "i26l6/Spliced", false);
    assert!(
        stale.iter().any(|b| Arc::ptr_eq(b, &running)),
        "the superseded body a frame still runs is collected"
    );
    assert!(
        !stale.iter().any(|b| Arc::ptr_eq(b, &current)),
        "the current body did not splice the class"
    );
    assert_eq!(cache.mark_withdrawn_by_redefinition(&stale), 1);
    assert_eq!(
        cache.force_withdrawn_exit_polls(&stale, spliced, "i26l6/Spliced"),
        1
    );
    assert_eq!(running.code_bytes()[20], 0x81, "JNZ became JNO");
    assert_eq!(
        current.code_bytes()[20],
        0x85,
        "the current body's poll is untouched"
    );
    assert!(cache.every_withdrawal_forced_since(before));

    // A withdrawn body whose recorded site is not a poll: nothing is
    // written, and the retries stay.
    let mut odd = with_exit_poll(flag);
    odd.exit_poll_sites = vec![3];
    odd.inlined_methods = vec![("i26l6/Spliced".into(), "value".into(), "()I".into())];
    let odd = Arc::new(odd);
    let before = cache.bodies_withdrawn_by_redefinition();
    assert_eq!(cache.mark_withdrawn_by_redefinition(&[Arc::clone(&odd)]), 1);
    assert_eq!(
        cache.force_withdrawn_exit_polls(&[Arc::clone(&odd)], spliced, "i26l6/Spliced"),
        0
    );
    assert!(!cache.every_withdrawal_forced_since(before));
    assert!(
        cache.every_withdrawal_forced_since(cache.bodies_withdrawn_by_redefinition() + 1),
        "a later snapshot is past it"
    );
    for b in stale.into_iter().chain([running, current, odd]) {
        crate::defer_jit_owner(Some(b));
    }
}

/// Interpreter round i1 wave 27, lane L6: a body its own class's
/// redefinition spared (an obsolete activation, whose verdict is "stay"
/// for good) is not forced by a later redefinition of another class
/// either, while a body that only spliced that other class still is.
#[test]
fn a_body_spared_as_obsolete_is_not_forced_by_a_later_redefinition() {
    let flag: &'static u8 = Box::leak(Box::new(0u8));
    let cache = JitCache::new();
    let own = cratonvm_types::ClassId::new(0x2771);
    let other = cratonvm_types::ClassId::new(0x2772);
    let mut obsolete = with_exit_poll(flag);
    obsolete.owner_class_id = own.as_u32();
    let obsolete = Arc::new(obsolete);
    let mut splicing = with_exit_poll(flag);
    splicing.inlined_methods = vec![("i27l6/Other".into(), "value".into(), "()I".into())];
    let splicing = Arc::new(splicing);
    let first = [Arc::clone(&obsolete)];
    assert_eq!(cache.mark_withdrawn_by_redefinition(&first), 1);
    assert_eq!(
        cache.force_withdrawn_exit_polls(&first, own, "i27l6/Own"),
        0,
        "its own class's redefinition spares it"
    );
    assert_eq!(obsolete.code_bytes()[20], 0x85);
    // A later (whole-cache) redefinition of another class lists both.
    let second = [Arc::clone(&obsolete), Arc::clone(&splicing)];
    assert_eq!(cache.mark_withdrawn_by_redefinition(&second), 1);
    assert_eq!(
        cache.force_withdrawn_exit_polls(&second, other, "i27l6/Other"),
        1,
        "only the splicing body is forced"
    );
    assert_eq!(obsolete.code_bytes()[20], 0x85, "still spared");
    assert_eq!(splicing.code_bytes()[20], 0x81, "JNZ became JNO");
    drop((first, second));
    crate::defer_jit_owner(Some(obsolete));
    crate::defer_jit_owner(Some(splicing));
}

/// Interpreter round i1 wave 45, lane L2: after a redefinition that
/// renumbered its class's pool, an OSR body of the class's own bytecode is
/// marked and forced (with `OWN_CLASS_RENUMBERED_OSR_BODIES_LEAVE` on),
/// while a method-entry body of the class, and an OSR body after a
/// redefinition that moved no constant, stay spared as before.
#[test]
fn an_own_class_osr_body_leaves_after_a_renumbering_redefinition() {
    let flag: &'static u8 = Box::leak(Box::new(0u8));
    let cache = JitCache::new();
    let own = cratonvm_types::ClassId::new(0x4521);
    let own_body = |osr: bool| {
        let mut body = with_exit_poll(flag);
        body.owner_class_id = own.as_u32();
        body.compiled_via_osr = osr;
        Arc::new(body)
    };
    let loop_body = own_body(true);
    let entry_body = own_body(false);
    let kept_body = own_body(true);
    let renumbering = [Arc::clone(&loop_body), Arc::clone(&entry_body)];
    assert_eq!(cache.mark_withdrawn_by_redefinition(&renumbering), 2);
    let forced = cache.force_withdrawn_exit_polls_after(&renumbering, own, "i45l2/Own", true);
    if OWN_CLASS_RENUMBERED_OSR_BODIES_LEAVE {
        assert_eq!(forced, 1, "the OSR body only");
        assert!(loop_body.leaves_as_renumbered_obsolete());
        assert_eq!(loop_body.code_bytes()[20], 0x81, "JNZ became JNO");
    } else {
        assert_eq!(forced, 0, "switched off: spared as in wave 44");
        assert!(!loop_body.leaves_as_renumbered_obsolete());
        assert_eq!(loop_body.code_bytes()[20], 0x85);
    }
    assert!(!entry_body.leaves_as_renumbered_obsolete());
    assert_eq!(
        entry_body.code_bytes()[20],
        0x85,
        "a method-entry body stays spared"
    );
    // A redefinition that moved no constant spares the OSR body too.
    let constant_only = [Arc::clone(&kept_body)];
    assert_eq!(cache.mark_withdrawn_by_redefinition(&constant_only), 1);
    assert_eq!(
        cache.force_withdrawn_exit_polls_after(&constant_only, own, "i45l2/Own", false),
        0
    );
    assert!(!kept_body.leaves_as_renumbered_obsolete());
    assert_eq!(kept_body.code_bytes()[20], 0x85);
    drop((renumbering, constant_only));
    crate::defer_jit_owner(Some(loop_body));
    crate::defer_jit_owner(Some(entry_body));
    crate::defer_jit_owner(Some(kept_body));
}

/// Interpreter round i1 wave 46, lane L2: after a redefinition that
/// renumbered its class's pool, a METHOD-ENTRY body of the class's own
/// bytecode is marked and forced (with `OWN_CLASS_RENUMBERED_ENTRY_BODIES_LEAVE`
/// on) only when a stash sink can rebuild its frame in its own bytecode:
/// a compiled source and a pool stamp, not `ACC_SYNCHRONIZED`, no monitor
/// in its code. The VM's verdict memo starts unasked.
#[test]
fn an_own_class_entry_body_leaves_only_when_a_sink_can_rebuild_it() {
    let flag: &'static u8 = Box::leak(Box::new(0u8));
    let cache = JitCache::new();
    let own = cratonvm_types::ClassId::new(0x4621);
    let source = |code: &[u8], is_synchronized: bool| {
        Arc::new(crate::CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: own,
                class_name: Arc::from("i46l2/Own"),
                method_name: Arc::from("spin"),
                method_descriptor: Arc::from("()V"),
                source_file: None,
                code: Arc::from(code),
                exception_table: Arc::from(Vec::new().as_slice()),
                max_stack: 1,
                max_locals: 1,
                num_params: 0,
                is_synchronized,
                is_static: true,
            },
        ))
    };
    let entry_body = |src: Option<Arc<crate::CachedBytecodeMethod>>| {
        let mut body = with_exit_poll(flag);
        body.owner_class_id = own.as_u32();
        body.compile_cp_stamp = Some(1);
        if let Some(src) = src {
            assert!(body.stamp_compiled_source(src));            }
        Arc::new(body)
    };
    // `return`, padded as the VM pads bytecode.
    let plain = entry_body(Some(source(&[0xb1, 0, 0], false)));
    let synchronized = entry_body(Some(source(&[0xb1, 0, 0], true)));
    // `aload_0; monitorenter; return`.
    let locking = entry_body(Some(source(&[0x2a, 0xc2, 0xb1, 0, 0], false)));
    let unstamped = entry_body(None);
    let bodies = [
        Arc::clone(&plain),
        Arc::clone(&synchronized),
        Arc::clone(&locking),
        Arc::clone(&unstamped),
    ];
    assert_eq!(cache.mark_withdrawn_by_redefinition(&bodies), 4);
    let forced = cache.force_withdrawn_exit_polls_after(&bodies, own, "i46l2/Own", true);
    if OWN_CLASS_RENUMBERED_ENTRY_BODIES_LEAVE {
        assert_eq!(forced, 1, "the resumable body only");
        assert!(plain.leaves_as_renumbered_obsolete_entry());
        assert_eq!(plain.renumbered_entry_resumes(), None, "not asked yet");
        assert_eq!(plain.code_bytes()[20], 0x81, "JNZ became JNO");
        plain.note_renumbered_entry_resumes(true);
        assert_eq!(plain.renumbered_entry_resumes(), Some(true));
    } else {
        assert_eq!(forced, 0, "switched off: spared as in wave 45");
        assert!(!plain.leaves_as_renumbered_obsolete());
        assert_eq!(plain.code_bytes()[20], 0x85);
    }
    for spared in [&synchronized, &locking, &unstamped] {
        assert!(!spared.leaves_as_renumbered_obsolete());
        assert_eq!(spared.code_bytes()[20], 0x85, "no sink could rebuild it");
    }
    drop(bodies);
    crate::defer_jit_owner(Some(plain));
    crate::defer_jit_owner(Some(synchronized));
    crate::defer_jit_owner(Some(locking));
    crate::defer_jit_owner(Some(unstamped));
}

/// Interpreter round i1 wave 47, lane L2: bodies a redefinition that moved
/// no constant spared as their class's obsolete activations stay spared
/// by a later redefinition of another class, but an interpreter-only
/// withdrawal forces the ones whose exit can be resumed (with
/// `INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE` on): an OSR body and a
/// method-entry body a sink can rebuild, not a synchronized one. The
/// verdict memo is keyed by the redefinition count.
#[test]
fn an_interpreter_only_withdrawal_forces_resumable_obsolete_bodies() {
    let flag: &'static u8 = Box::leak(Box::new(0u8));
    let cache = JitCache::new();
    let own = cratonvm_types::ClassId::new(0x4721);
    let other = cratonvm_types::ClassId::new(0x4722);
    let source = |is_synchronized: bool| {
        Arc::new(crate::CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: own,
                class_name: Arc::from("i47l2/Own"),
                method_name: Arc::from("spin"),
                method_descriptor: Arc::from("()V"),
                source_file: None,
                code: Arc::from([0xb1u8, 0, 0].as_slice()),
                exception_table: Arc::from(Vec::new().as_slice()),
                max_stack: 1,
                max_locals: 1,
                num_params: 0,
                is_synchronized,
                is_static: true,
            },
        ))
    };
    let own_body = |osr: bool, src: Option<Arc<crate::CachedBytecodeMethod>>| {
        let mut body = with_exit_poll(flag);
        body.owner_class_id = own.as_u32();
        body.compiled_via_osr = osr;
        body.compile_cp_stamp = Some(1);
        if let Some(src) = src {
            assert!(body.stamp_compiled_source(src));            }
        Arc::new(body)
    };
    let loop_body = own_body(true, None);
    let entry_body = own_body(false, Some(source(false)));
    let synchronized = own_body(false, Some(source(true)));
    let bodies = [
        Arc::clone(&loop_body),
        Arc::clone(&entry_body),
        Arc::clone(&synchronized),
    ];
    assert_eq!(cache.mark_withdrawn_by_redefinition(&bodies), 3);
    assert_eq!(
        cache.force_withdrawn_exit_polls_after(&bodies, own, "i47l2/Own", false),
        0,
        "its own class's constant-only redefinition spares every one"
    );
    assert_eq!(
        cache.force_withdrawn_exit_polls(&bodies, other, "i47l2/Other"),
        0,
        "a redefinition of another class keeps the spare"
    );
    let forced = cache.force_exit_polls_for_the_interpreter(&bodies, "<interpreter-only>");
    // The synchronized body leaves too once the interpreter resumes a
    // locking activation (`INTERPRETER_ONLY_OBSOLETE_LOCKING_BODIES_LEAVE`).
    let locking = INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE
        && INTERPRETER_ONLY_OBSOLETE_LOCKING_BODIES_LEAVE;
    if INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE {
        assert_eq!(
            forced,
            2 + usize::from(locking),
            "the OSR body and the resumable method-entry bodies"
        );
        assert_eq!(loop_body.code_bytes()[20], 0x81, "JNZ became JNO");
        assert_eq!(entry_body.code_bytes()[20], 0x81, "JNZ became JNO");
    } else {
        assert_eq!(forced, 0, "switched off: spared as in wave 46");
        assert_eq!(loop_body.code_bytes()[20], 0x85);
        assert_eq!(entry_body.code_bytes()[20], 0x85);
    }
    assert_eq!(
        synchronized.code_bytes()[20],
        if locking { 0x81 } else { 0x85 },
        "rebuilt only for a locking resume"
    );
    assert!(
        !entry_body.leaves_as_renumbered_obsolete(),
        "no renumbering mark"
    );
    assert_eq!(entry_body.obsolete_entry_resumes_at(7), None, "never asked");
    entry_body.note_obsolete_entry_resumes_at(7, true);
    assert_eq!(entry_body.obsolete_entry_resumes_at(7), Some(true));
    assert_eq!(
        entry_body.obsolete_entry_resumes_at(8),
        None,
        "a later redefinition voids it"
    );
    entry_body.note_obsolete_entry_resumes_at(8, false);
    assert_eq!(entry_body.obsolete_entry_resumes_at(8), Some(false));
    drop(bodies);
    crate::defer_jit_owner(Some(loop_body));
    crate::defer_jit_owner(Some(entry_body));
    crate::defer_jit_owner(Some(synchronized));
}

/// Interpreter round i1 wave 26, lane L6: a body the cache never published
/// (the VM's optimizing OSR body) is listed for as long as it lives, and a
/// body with no exit poll is not listed at all.
#[test]
fn an_unpublished_exit_poll_body_is_listed_while_it_lives() {
    let flag: &'static u8 = Box::leak(Box::new(0u8));
    let cache = JitCache::new();
    let osr = Arc::new(with_exit_poll(flag));
    let pollless = Arc::new(seven());
    cache.note_exit_poll_body(&osr);
    cache.note_exit_poll_body(&pollless);
    let listed = cache.live_unpublished_exit_poll_bodies();
    assert_eq!(listed.len(), 1);
    assert!(Arc::ptr_eq(&listed[0], &osr));
    drop(listed);
    drop(osr);
    assert!(cache.live_unpublished_exit_poll_bodies().is_empty());
    drop(pollless);
}

/// Interpreter round i1 wave 24, lane L6: once a redefinition fenced
/// publications, a compile that began before it (the old bytecode) cannot
/// publish -- so it cannot slip in between the not-entrant scan and the
/// eviction -- and one that began after it can.
#[test]
fn a_fenced_redefinition_refuses_the_old_bytecodes_compiles() {
    let cache = JitCache::new();
    let put = |class: &str, id: cratonvm_types::ClassId, mut body: CompiledMethod| {
        body.entry_java_args = 0;
        cache.put(Arc::from(class), Arc::from("m"), Arc::from("()I"), id, body);
        cache.get(class, "m", "()I", id)
    };
    for (class, id, whole_cache) in [
        ("i24l6/Scoped", cratonvm_types::ClassId::new(0x2441), false),
        ("i24l6/Whole", cratonvm_types::ClassId::new(0x2442), true),
    ] {
        let old = seven();
        cache.fence_redefinition_publications(id, class, whole_cache);
        assert!(
            put(class, id, old).is_none(),
            "{class}: begun before the fence, refused"
        );
        let fresh = put(class, id, seven());
        assert!(fresh.is_some(), "{class}: begun after it, published");
        crate::defer_jit_owner(fresh);
    }
}

/// The VM helper's stand-in for the supersede-redirect tests: a call the
/// stub could not forward answers 4242.
extern "C" fn w16_slow(_record: i64, _regs: i64, _stack: i64) -> i64 {
    4242
}

/// nio21 wave 16 (lane L1): an armed cache makes the body a publication
/// displaces not entrant -- a single-pass body displaced by another
/// single-pass body too -- and the displaced stub forwards to the current
/// body; an optimizing body displaced by a single-pass one is left alone.
#[test]
fn w16l1_a_displaced_body_is_redirected_to_its_successor() {
    use std::sync::atomic::Ordering;
    let cache = JitCache::new();
    if !cache.supersede_redirect {
        return; // `CRATONVM_JIT_SUPERSEDE_REDIRECT=0` in this environment.
    }
    cache.arm_not_entrant(NotEntrantHook {
        helper: w16_slow as usize,
        vm_ptr: 0,
    });
    let class = cratonvm_types::ClassId::new(0x1601);
    let put = |mut body: CompiledMethod| {
        body.entry_java_args = 0;
        cache.put(
            Arc::from("w16l1/C"),
            Arc::from("m"),
            Arc::from("()I"),
            class,
            body,
        );
        cache.get("w16l1/C", "m", "()I", class).expect("published")
    };
    let first = put(answering(7));
    assert!(
        !first.is_not_entrant(),
        "a first publication displaces nothing"
    );
    let second = put(answering(9));
    assert!(first.is_not_entrant(), "C1 displaced by C1: redirected");
    assert!(first.retired.load(Ordering::Acquire));
    assert!(!second.is_not_entrant());
    assert_eq!(cache.supersede_redirect_counts(), (1, 0));
    // SAFETY: the entry jumps to the stub; no forward yet: the helper.
    assert_eq!(unsafe { first.try_call(&[]) }.expect("call"), 4242);
    let rec = first.not_entrant.get().expect("patched");
    assert!(cache.forward_not_entrant(rec, &second));
    // SAFETY: the stub forwards to `second`: MOV EAX, 9; RET.
    assert_eq!(unsafe { first.try_call(&[]) }.expect("call"), 9);

    let mut optimizing = answering(11);
    optimizing.used_ir_backend = true;
    let optimizing = put(optimizing);
    assert!(second.is_not_entrant(), "C1 displaced by C2: redirected");
    // SAFETY: `first` forwards to `second`, superseded now: the slow
    // path, where the VM would re-forward to the current body.
    assert_eq!(unsafe { first.try_call(&[]) }.expect("call"), 4242);
    assert!(cache.forward_not_entrant(rec, &optimizing));
    // SAFETY: re-forwarded: MOV EAX, 11; RET.
    assert_eq!(unsafe { first.try_call(&[]) }.expect("call"), 11);

    let single = put(answering(13));
    assert!(
        !optimizing.is_not_entrant(),
        "C2 displaced by C1: its baked callers keep the optimizing code"
    );
    assert_eq!(cache.supersede_redirect_counts(), (2, 0));
    for b in [first, second, optimizing, single] {
        crate::defer_jit_owner(Some(b));
    }
}

/// Displaced C1 bodies must be redirected when the hook becomes available:
/// their counters are no longer in the published map, so waiting for the
/// next C2 publication can strand the calls that would trigger that tier.
#[test]
fn w16l1_a_body_displaced_before_arming_is_redirected_on_arming() {
    let cache = JitCache::new();
    if !cache.supersede_redirect {
        return;
    }
    let class = cratonvm_types::ClassId::new(0x1602);
    let put = |name: &str, mut body: CompiledMethod| {
        body.entry_java_args = 0;
        cache.put(
            Arc::from("w16l1/D"),
            Arc::from(name),
            Arc::from("()I"),
            class,
            body,
        );
        cache.get("w16l1/D", name, "()I", class).expect("published")
    };
    let a = put("m", answering(7));
    let b = put("m", answering(9));
    let other_old = put("n", answering(5));
    let other_new = put("n", answering(6));
    assert!(!a.is_not_entrant(), "not armed: nothing patched");
    let generation = cache.generation();
    let hook = NotEntrantHook { helper: w16_slow as usize, vm_ptr: 0 };
    cache.arm_not_entrant(hook);
    assert!(a.is_not_entrant(), "arming must serve the waiting C1 body without another publication");
    assert!(other_old.is_not_entrant(), "arming serves every pending key");
    assert!(!b.is_not_entrant() && !other_new.is_not_entrant());
    assert!(cache.generation() > generation, "retirement invalidates walk memos");
    assert_eq!(cache.supersede_redirect_counts(), (2, 0));
    for (old, new, answer) in [(&a, &b, 9), (&other_old, &other_new, 6)] {
        assert!(cache.forward_not_entrant(old.not_entrant.get().expect("patched"), new));
        // SAFETY: the entry forwards to the current no-argument leaf.
        assert_eq!(unsafe { old.try_call(&[]) }.expect("call"), answer);
    }
    cache.arm_not_entrant(hook);
    assert_eq!(cache.supersede_redirect_counts(), (2, 0), "arming is idempotent");
    let c = put("m", answering(11));
    assert!(a.is_not_entrant(), "the waiting body");
    assert!(b.is_not_entrant(), "the body this publication displaced");
    assert!(!c.is_not_entrant());
    assert!(other_old.is_not_entrant(), "another key was served on arming");
    // SAFETY: no forward yet: the helper.
    assert_eq!(unsafe { a.try_call(&[]) }.expect("call"), 4242);
    for x in [a, b, c, other_old, other_new] {
        crate::defer_jit_owner(Some(x));
    }
}

/// STASH20-2: `NOP5; SUB RSP, 0x28; NOP x nops; CALL rel32 target;
/// ADD RSP, 0x28; RET` -- a caller that baked a direct call to `target`
/// (16-aligned at the call, with the Win64 shadow space), and the offset
/// of the call's return address. `None` when `target` is out of `rel32`
/// reach of this buffer.
fn calling(target: usize, nops: usize) -> Option<(CompiledMethod, usize)> {
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&ENTRY_PATCH_PAD);
    buf.emit(&[0x48, 0x83, 0xEC, 0x28]);
    for _ in 0..nops {
        buf.emit(&[0x90]);
    }
    let ret_off = buf.pos() + 5;
    // Cast: addresses, for the displacement.
    let ret_pc = buf.as_ptr() as usize + ret_off;
    let rel = i32::try_from(target as i64 - ret_pc as i64).ok()?;
    buf.emit(&[0xE8]);
    buf.emit(&rel.to_le_bytes());
    buf.emit(&[0x48, 0x83, 0xC4, 0x28, 0xC3]);
    Some((CompiledMethod::new(buf), ret_off))
}

/// JIT r23 wave 10, lane L6: `calling`'s twin with the optimizing tier's
/// `MOV RAX, imm64 ; CALL RAX`, the imm64 on an 8-byte boundary when
/// `aligned`, one byte past it otherwise. Answers the return offset.
fn calling_abs64(target: usize, aligned: bool) -> (CompiledMethod, usize) {
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&ENTRY_PATCH_PAD);
    buf.emit(&[0x48, 0x83, 0xEC, 0x28]);
    // Cast: an address, for its alignment only.
    let at = buf.as_ptr() as usize + buf.pos();
    let mut pad = crate::ir_lower::direct_call_imm_align_pad(at);
    if !aligned {
        pad = (pad + 1) % 8;
    }
    for _ in 0..pad {
        buf.emit(&[0x90]);
    }
    buf.emit(&[0x48, 0xB8]);
    buf.emit(&(target as u64).to_le_bytes());
    buf.emit(&[0xFF, 0xD0]);
    let ret_off = buf.pos();
    buf.emit(&[0x48, 0x83, 0xC4, 0x28, 0xC3]);
    (CompiledMethod::new(buf), ret_off)
}


/// STASH20-2 (JIT round 20 wave 19, lane yStash): a caller that baked the
/// displaced body's entry is re-pointed at the current body once the
/// stub forwards there, atomically, and only a caller that roots the
/// displaced body, at a direct `CALL rel32` of it whose displacement lies
/// in one 8-byte word. The rebind clears the forward word for the next
/// stale caller, whose slow call publishes the same kept forward again.
#[test]
fn ystash_a_stale_direct_call_is_rebound_to_the_current_body() {
    let cache = JitCache::new();
    if !cache.supersede_redirect {
        return; // `CRATONVM_JIT_SUPERSEDE_REDIRECT=0` in this environment.
    }
    cache.arm_not_entrant(NotEntrantHook {
        helper: w16_slow as usize,
        vm_ptr: 0,
    });
    let class = cratonvm_types::ClassId::new(0x2019);
    let callers = cratonvm_types::ClassId::new(0x201A);
    let put = |class_name: &str, id: cratonvm_types::ClassId, mut body: CompiledMethod| {
        body.entry_java_args = 0;
        cache.put(
            Arc::from(class_name),
            Arc::from("m"),
            Arc::from("()I"),
            id,
            body,
        );
        cache.get(class_name, "m", "()I", id).expect("published")
    };
    let old = put("ys/T", class, answering(7));
    // Cast: an address, as the compile doors record a baked callee.
    let old_entry = old.entry_ptr() as usize;
    let built = (
        calling(old_entry, 0),
        calling(old_entry, 3),
        calling(old_entry, 0),
    );
    let (Some((mut caller, ret_off)), Some((mut straddling, s_off)), Some((unrooted, u_off))) =
        built
    else {
        crate::defer_jit_owner(Some(old));
        return; // The buffers landed out of `rel32` reach of each other.
    };
    caller._direct_callee_entries = vec![old_entry];
    straddling._direct_callee_entries = vec![old_entry];
    let caller = put("ys/Caller", callers, caller);
    let straddling = put("ys/Straddling", callers, straddling);
    let unrooted = put("ys/Unrooted", callers, unrooted);
    // Cast: addresses, the return sites of the three calls.
    let ret_pc = caller.entry_ptr() as usize + ret_off;
    let s_ret = straddling.entry_ptr() as usize + s_off;
    let u_ret = unrooted.entry_ptr() as usize + u_off;
    // SAFETY: the caller calls `old`: MOV EAX, 7; RET.
    assert_eq!(unsafe { caller.try_call(&[]) }.expect("call"), 7);

    let new = put("ys/T", class, answering(9));
    // Cast: addresses, for the reach test.
    if i32::try_from(new.entry_ptr() as i64 - ret_pc as i64).is_err() {
        for b in [old, new, caller, straddling, unrooted] {
            crate::defer_jit_owner(Some(b));
        }
        return; // The new body landed out of `rel32` reach of the caller.
    }
    assert!(old.is_not_entrant(), "C1 displaced by C1: redirected");
    let rec = old.not_entrant.get().expect("patched");
    assert_eq!(
        cache.rebind_stale_call_site(rec, &new, &caller, ret_pc),
        Err("no forward of the stub owns the current body"),
        "nothing to rebind to before the VM installs the forward"
    );
    // SAFETY: the stub has no forward yet: the helper answers.
    assert_eq!(unsafe { caller.try_call(&[]) }.expect("call"), 4242);
    assert!(cache.forward_not_entrant(rec, &new));
    // SAFETY: the stub forwards to `new`: MOV EAX, 9; RET.
    assert_eq!(unsafe { caller.try_call(&[]) }.expect("call"), 9);

    // Cast: an address inside the caller that ends no CALL.
    let not_a_call = caller.entry_ptr() as usize + 5;
    assert_eq!(
        cache.rebind_stale_call_site(rec, &new, &caller, not_a_call),
        Err("the return site is not a direct CALL rel32")
    );
    assert_eq!(
        cache.rebind_stale_call_site(rec, &new, &straddling, s_ret),
        Err("the call's displacement straddles an 8-byte word")
    );
    assert_eq!(
        cache.rebind_stale_call_site(rec, &new, &unrooted, u_ret),
        Err("the caller does not root the patched body")
    );
    assert!(!caller.has_rebound_call_sites(), "a refusal marks nothing");

    assert_eq!(
        cache.rebind_stale_call_site(rec, &new, &caller, ret_pc),
        Ok(true)
    );
    assert!(caller.has_rebound_call_sites());
    // SAFETY: the caller's code is mapped and readable while it lives.
    let disp = unsafe { ((ret_pc - 4) as *const i32).read_unaligned() };
    // Cast: an address and a sign-extended displacement.
    assert_eq!(
        ret_pc.wrapping_add(disp as isize as usize),
        new.entry_ptr() as usize
    );
    // SAFETY: the caller calls `new` directly now.
    assert_eq!(unsafe { caller.try_call(&[]) }.expect("call"), 9);
    assert_eq!(
        cache.rebind_stale_call_site(rec, &new, &caller, ret_pc),
        Err("the return site's CALL does not name the patched body"),
        "rebound once"
    );
    // The word was cleared: the next stale call goes through the VM once,
    // and its forward re-publishes the kept box instead of a new one.
    // SAFETY: the stub's forward word is clear: the helper answers.
    assert_eq!(unsafe { old.try_call(&[]) }.expect("call"), 4242);
    assert!(cache.forward_not_entrant(rec, &new));
    assert_eq!(
        rec.forwards.lock().len(),
        1,
        "the kept forward, re-published"
    );
    // SAFETY: forwarded to `new` again.
    assert_eq!(unsafe { old.try_call(&[]) }.expect("call"), 9);
    assert!(
        rec.rebinds.load(std::sync::atomic::Ordering::Relaxed) == 1,
        "one site rebound: no cycle forward may be installed for this record any more"
    );
    // Lane rev16: the rebound caller bakes `new` now, so withdrawing `new`
    // withdraws the caller too (the forward it skips tested `retired`).
    assert!(cache.get("ys/Straddling", "m", "()I", callers).is_some());
    cache.remove("ys/T", "m", "()I", class);
    assert!(new.retired.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        cache.get("ys/Caller", "m", "()I", callers).is_none(),
        "a caller rebound at a withdrawn body is withdrawn with it"
    );
    assert!(
        cache.get("ys/Straddling", "m", "()I", callers).is_some(),
        "a caller that still calls through the stub keeps its body"
    );
    for b in [old, new, caller, straddling, unrooted] {
        crate::defer_jit_owner(Some(b));
    }
}

/// STASH20-1's stand-in for the VM's tiered manager: the method `cyc` is a
/// sticky recursion-cycle member.
struct CycleSink;

impl crate::JitBodyWithdrawalSink for CycleSink {
    fn bodies_withdrawn(&self, _keys: &[crate::WithdrawnMethodKey]) {}

    fn forward_must_dispatch(
        &self,
        _class_name: &str,
        method_name: &str,
        _descriptor: &str,
        _class_id: u32,
    ) -> bool {
        method_name == "cyc"
    }
}

/// STASH20-1 (JIT round 20 wave 19, lane yStash): the displaced body of a
/// method whose forward the VM would refuse stays entrant (its baked
/// callers keep running it, not a by-name re-dispatch per call); another
/// method's displaced body is redirected as before.
#[test]
fn ystash_a_cycle_members_displaced_body_stays_entrant() {
    use std::sync::atomic::Ordering;
    let cache = JitCache::new();
    if !cache.supersede_redirect || !supersede_skip_refused_forward_enabled() {
        return; // A kill switch is set in this environment.
    }
    let sink: Arc<dyn crate::JitBodyWithdrawalSink> = Arc::new(CycleSink);
    if !cache.install_withdrawal_sink(Arc::downgrade(&sink)) {
        return; // `CRATONVM_JIT_WITHDRAWAL_DEMOTES=0`: no sink, no verdict.
    }
    cache.arm_not_entrant(NotEntrantHook {
        helper: w16_slow as usize,
        vm_ptr: 0,
    });
    let class = cratonvm_types::ClassId::new(0x201B);
    let put = |name: &str, mut body: CompiledMethod| {
        body.entry_java_args = 0;
        cache.put(
            Arc::from("ys/Cycle"),
            Arc::from(name),
            Arc::from("()I"),
            class,
            body,
        );
        cache
            .get("ys/Cycle", name, "()I", class)
            .expect("published")
    };
    let cyc_old = put("cyc", answering(7));
    let cyc_new = put("cyc", answering(9));
    assert!(
        !cyc_old.is_not_entrant(),
        "the VM would refuse the forward: left entrant"
    );
    assert!(!cyc_old.retired.load(Ordering::Acquire));
    // SAFETY: unpatched: MOV EAX, 7; RET.
    assert_eq!(unsafe { cyc_old.try_call(&[]) }.expect("call"), 7);
    let m_old = put("m", answering(5));
    let m_new = put("m", answering(6));
    assert!(
        m_old.is_not_entrant(),
        "a method the VM forwards is redirected"
    );
    assert_eq!(cache.supersede_redirect_counts(), (1, 0));
    for b in [cyc_old, cyc_new, m_old, m_new] {
        crate::defer_jit_owner(Some(b));
    }
    drop(sink);
}

/// JIT round 23 wave 1, lane L1 (THROWS22-1, `CRATONVM_JIT_OSR_PROFIT_EXIT`):
/// arming a single-pass OSR body for a profit exit forces its exit polls
/// once, sets the fact the poll verdict asks, and leaves
/// `exit_polls_forced` alone, so a later redefinition's force pass still
/// accepts the (already forced) sites and marks the body. A method-entry
/// body and a withdrawn body are never armed or told to leave for profit.
#[test]
fn r23_l1_an_armed_single_pass_osr_body_has_its_polls_forced_once() {
    use std::sync::atomic::Ordering;
    let flag: &'static u8 = Box::leak(Box::new(0u8));
    let cache = JitCache::new();
    if !cache.osr_profit_exit_enabled() {
        // `CRATONVM_JIT_OSR_PROFIT_EXIT=0` in the test environment.
        let mut b = with_exit_poll(flag);
        b.compiled_via_osr = true;
        assert_eq!(cache.arm_osr_profit_exit(&Arc::new(b)), Err("switch-off"));
        return;
    }
    let entry = Arc::new(with_exit_poll(flag));
    assert_eq!(cache.arm_osr_profit_exit(&entry), Err("not-an-osr-body"));
    // An IR OSR body that records exit polls is armable (the late-bind OSR
    // exit); one with none is refused like any other.
    let mut ir = with_exit_poll(flag);
    ir.compiled_via_osr = true;
    ir.used_ir_backend = true;
    ir.exit_poll_sites.clear();
    assert_eq!(cache.arm_osr_profit_exit(&Arc::new(ir)), Err("no-exit-polls"));
    let mut bare = with_exit_poll(flag);
    bare.compiled_via_osr = true;
    bare.exit_poll_sites.clear();
    assert_eq!(cache.arm_osr_profit_exit(&Arc::new(bare)), Err("no-exit-polls"));
    assert_eq!(cache.osr_profit_exits_armed(), 0, "no refusal arms");

    let mut b = with_exit_poll(flag);
    b.compiled_via_osr = true;
    let body = Arc::new(b);
    assert!(!body.osr_profit_exit_armed());
    assert!(!cache.osr_profit_exit_may_leave(&body), "not armed yet");
    assert_eq!(body.code_bytes()[20], 0x85);
    assert_eq!(cache.arm_osr_profit_exit(&body), Ok(1));
    assert_eq!(body.code_bytes()[20], 0x81, "JNZ became JNO");
    assert!(body.osr_profit_exit_armed());
    assert!(cache.osr_profit_exit_may_leave(&body));
    assert_eq!(cache.osr_profit_exits_armed(), 1);
    assert_eq!(cache.arm_osr_profit_exit(&body), Ok(0), "armed once");
    assert_eq!(cache.osr_profit_exits_armed(), 1);

    // A later redefinition still forces the body (same byte) and marks it.
    assert!(!body.exit_polls_forced.load(Ordering::Acquire));
    assert_eq!(body.force_exit_polls(), Ok(1));
    assert_eq!(body.code_bytes()[20], 0x81);
    assert!(body.exit_polls_forced.load(Ordering::Acquire));

    // Withdrawn: the redefinition owns its exits now.
    assert_eq!(cache.mark_withdrawn_by_redefinition(&[Arc::clone(&body)]), 1);
    assert!(!cache.osr_profit_exit_may_leave(&body));
    let mut late = with_exit_poll(flag);
    late.compiled_via_osr = true;
    late.withdrawn_by_redefinition
        .store(true, Ordering::Release);
    assert_eq!(
        cache.arm_osr_profit_exit(&Arc::new(late)),
        Err("redefinition-owns-its-exits")
    );

    assert_eq!(cache.note_osr_profit_exit_verdict(), 1);
    assert_eq!(cache.note_osr_profit_exit_taken(), 1);
    assert_eq!(cache.osr_profit_exit_counts(), (1, 1, 1));
    crate::defer_jit_owner(Some(body));
    crate::defer_jit_owner(Some(entry));
}

/// JIT round 23 wave 1, lane L1: the exit-poll shape check answers a poll
/// forced already with its own byte, so the redefinition's pass does not
/// refuse a body the OSR profit exit forced first.
#[test]
fn r23_l1_a_forced_poll_is_still_a_well_shaped_poll() {
    let flag = 0x7000_0000usize;
    let at = 0x1000usize;
    let sp = [0x41, 0xF6, 0x43, 0x00, 0xFF, 0x0F, 0x80, 0, 0, 0, 0];
    assert_eq!(forced_exit_poll_cc(&sp, at, 6, flag), Some(0x80));
    let ir = [0x41, 0xF6, 0x03, 0xFF, 0x0F, 0x81, 0, 0, 0, 0];
    assert_eq!(forced_exit_poll_cc(&ir, at, 5, flag), Some(0x81));
    // Any other condition code is still refused.
    let jl = [0x41, 0xF6, 0x03, 0xFF, 0x0F, 0x8C, 0, 0, 0, 0];
    assert_eq!(forced_exit_poll_cc(&jl, at, 5, flag), None);
}

/// Review item 1 of `r23-w1-review-osr-profit-exit-FIXED-20261021.md`: a body
/// whose profit exit cannot be taken is disarmed, its polls written back
/// and never armed again; a redefinition's force stands over a disarm.
#[test]
fn r23_l1_a_disarmed_body_gets_its_polls_back_unless_a_redefinition_forced_them() {
    use std::sync::atomic::Ordering;
    let flag: &'static u8 = Box::leak(Box::new(0u8));
    let cache = JitCache::new();
    if !cache.osr_profit_exit_enabled() {
        return;
    }
    let mut b = with_exit_poll(flag);
    b.compiled_via_osr = true;
    let body = Arc::new(b);
    assert_eq!(cache.disarm_osr_profit_exit(&body), 0, "not armed: nothing");
    assert_eq!(cache.arm_osr_profit_exit(&body), Ok(1));
    assert_eq!(body.code_bytes()[20], 0x81);
    assert_eq!(cache.disarm_osr_profit_exit(&body), 1);
    assert_eq!(body.code_bytes()[20], 0x85, "JNO back to JNZ");
    assert!(!body.osr_profit_exit_armed());
    assert!(!cache.osr_profit_exit_may_leave(&body));
    assert_eq!(cache.osr_profit_exit_disarms(), 1);
    assert_eq!(cache.arm_osr_profit_exit(&body), Err("disarmed-before"));
    assert_eq!(cache.disarm_osr_profit_exit(&body), 0, "once");

    let mut f = with_exit_poll(flag);
    f.compiled_via_osr = true;
    let forced = Arc::new(f);
    assert_eq!(cache.arm_osr_profit_exit(&forced), Ok(1));
    assert_eq!(forced.force_exit_polls(), Ok(1), "a redefinition forces it too");
    assert!(forced.exit_polls_forced.load(Ordering::Acquire));
    assert_eq!(cache.disarm_osr_profit_exit(&forced), 0);
    assert_eq!(forced.code_bytes()[20], 0x81, "the redefinition's force stands");
    assert!(!forced.osr_profit_exit_armed());
    assert_eq!(cache.osr_profit_exit_disarms(), 2);
}

/// t1 lane jitea (L1W1-2): the single-pass OSR bodies a later `put_osr`
/// superseded while a frame still runs them are listed for the profit
/// arm -- same owner class and label only, never the current body, an IR
/// body, another method, or a body armed already.
#[test]
fn t1_jitea_superseded_single_pass_osr_bodies_are_listed() {
    let flag: &'static u8 = Box::leak(Box::new(0u8));
    let cache = JitCache::new();
    let make = |label: &str, owner: u32, ir: bool| {
        let mut b = with_exit_poll(flag);
        b.compiled_via_osr = true;
        b.used_ir_backend = ir;
        b.method_label = label.to_string();
        b.owner_class_id = owner;
        let body = Arc::new(b);
        cache.note_exit_poll_body(&body);
        body
    };
    let label = "t1/Loop.run:()V";
    let current = make(label, 7, false);
    let old = make(label, 7, false);
    let other_owner = make(label, 8, false);
    let other_method = make("t1/Loop.walk:()V", 7, false);
    let ir = make(label, 7, true);
    let listed = cache.superseded_single_pass_osr_bodies(&current);
    assert_eq!(listed.len(), 1, "only the superseded sibling");
    assert!(Arc::ptr_eq(&listed[0], &old));
    assert!(cache.superseded_single_pass_osr_bodies(&ir).is_empty(), "IR current");
    if cache.osr_profit_exit_enabled() {
        assert_eq!(cache.arm_osr_profit_exit(&old), Ok(1));
        assert!(
            cache.superseded_single_pass_osr_bodies(&current).is_empty(),
            "armed already"
        );
    }
    drop(listed);
    for b in [current, old, other_owner, other_method, ir] {
        crate::defer_jit_owner(Some(b));
    }
}
