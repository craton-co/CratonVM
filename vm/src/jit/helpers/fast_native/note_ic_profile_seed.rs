#[allow(unused_imports)]
use super::*;

/// nio21 wave 16, lane L4 (`CRATONVM_JIT_IR_IC_PROFILE_SEED`;
/// `r21-eagerly-compiled-callee-has-no-receiver-profile-to-splice-CLOSED-20261012.md`):
/// the inline-cache miss of a site the optimizing planner armed
/// (`cratonvm_jit::IcProfileSeedSite`: an own-site virtual call in a loop of a
/// method-entry body whose receiver profile was starved at compile time --
/// the eager callee chain compiled the method before it ever ran).
///
/// 1. The receiver's exact class, when it is new to the site, is written into
///    the method's receiver profile at the site's bci as
///    `MIN_GUARDED_RECEIVER_EVIDENCE` observations
///    (`ProfileStore::seed_ic_observed_receiver`). The miss helper sees every
///    distinct class that reaches the site (a class the MIC or PIC serves never
///    enters it), so the row is the set of classes the site has seen.
/// 2. The first such record asks for ONE optimizing recompile of the method
///    (`ProfileStore::claim_ic_seed_recompile`: once per method, a VM budget),
///    through the late-bind C2 retry door, whose publication supersedes the
///    starved body and forwards its baked callers to the new one. Only with a
///    background compiler and the C2 supersede on, as the late-bind recompile.
///
/// The recompile reads a profile with evidence at the site, so its planner
/// splices the class's body behind the exact class guard
/// (`ir::profile_guarded_receiver_class`), whose miss edge is the ordinary
/// call: no Java-visible answer can change, only which body runs inline.
///
/// SAFETY: as [`jit_invoke_virtual_mic`]: `vm_ptr` the live VM, `args_ptr` the
/// compiled caller's argument words with the receiver first.
// JIT round 23 wave 2f (lane L1): out of line, so the frame of the helper on every
// compiled-call level (which stays live for the depth of a recursion) does not reserve
// this door's locals; it returns before the call that recurses.
#[cold]
#[inline(never)]
pub unsafe fn note_ic_profile_seed(vm_ptr: i64, seed: &cratonvm_jit::IcProfileSeedSite, args_ptr: i64) {
    let receiver_bits = *(args_ptr as *const i64) as u64;
    if receiver_bits == 0 || (receiver_bits & 0x7) != 0 || receiver_bits >= (1u64 << 48) {
        return;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let receiver_ref = ObjectRef::from_raw(receiver_bits as usize as *mut u8);
    // An array receiver answers no class here (its header id is its
    // component's), and class 0 is never a guard class.
    let Some(receiver_class) = jit_dispatch_class(vm, receiver_ref).class_id() else {
        return;
    };
    let receiver_cid = receiver_class.as_u32();
    if receiver_cid == 0 || !seed.note_receiver(receiver_cid) {
        return;
    }
    let key = crate::jit::profile::MethodKey {
        class_id: seed.class_id,
        method_name: std::sync::Arc::clone(&seed.method_name),
        descriptor: std::sync::Arc::clone(&seed.descriptor),
    };
    let store = &vm.jit.profile_store;
    let seeded = store.seed_ic_observed_receiver(
        &key,
        seed.pc,
        receiver_cid,
        cratonvm_jit::MIN_GUARDED_RECEIVER_EVIDENCE,
    );
    let dbg = crate::runtime::env_cache::dbg_jitc();
    if dbg {
        eprintln!(
            "[cratonvm-jitc] ir-ic-profile-seed {}.{}{} @pc={}: receiver class {} {}",
            seed.class_name,
            seed.method_name,
            seed.descriptor,
            seed.pc,
            receiver_cid,
            if seeded {
                "recorded"
            } else {
                "already in the profile"
            },
        );
    }
    // j27 HANGHUNT (`CRATONVM_JIT_IR_IC_SEED_CLAIM_ON_ENQUEUE`, default ON, `=0` kills): a row
    // the interpreter filled before this seed still asks; the armed seed
    // proves the body was compiled starved.
    let claim_on_enqueue = cratonvm_jit::ic_seed_claim::ic_seed_claim_on_enqueue_enabled();
    if !cratonvm_jit::ic_seed_claim::ic_seed_asks_recompile(seeded, claim_on_enqueue)
        || !crate::runtime::env_cache::bg_compile()
        || !crate::runtime::env_cache::c2_supersede()
    {
        return;
    }
    if !store.claim_ic_seed_recompile(&key) {
        return;
    }
    if dbg {
        eprintln!(
            "[cratonvm-jitc] ir-ic-profile-seed recompile requested {}.{}{} (site pc={})",
            seed.class_name, seed.method_name, seed.descriptor, seed.pc,
        );
    }
    // The C2 retry (`TieredCompilationManager::request_c2_retry`, under the
    // name "ic-profile-seed" since JIT r22 wave 2, nio21 w16 L4 P1; it was
    // the late-bind door's): one more optimizing compile, refused while one
    // is already queued or the method's retry budget is spent.
    let answer = vm.jit.tiered_manager.request_ic_seed_recompile(
        &crate::jit::tiered::MethodKey::with_class_id(
            ClassId::new(seed.class_id),
            std::sync::Arc::clone(&seed.class_name),
            std::sync::Arc::clone(&seed.method_name),
            std::sync::Arc::clone(&seed.descriptor),
        ),
    );
    // j27 HANGHUNT: a refused retry does not spend the claim (same switch).
    if !cratonvm_jit::ic_seed_claim::ic_seed_claim_kept(answer, claim_on_enqueue) {
        store.release_ic_seed_recompile(&key);
        if dbg {
            eprintln!(
                "[cratonvm-jitc] ir-ic-profile-seed claim RELEASED {}.{}{} (site pc={}): retry refused",
                seed.class_name, seed.method_name, seed.descriptor, seed.pc,
            );
        }
    }
    // JIT round 21 lane indyconst (`CRATONVM_JIT_ONE_SHOT_RETRY_OWED`): the
    // claim is spent and the site's MIC now serves this class, so nothing asks
    // again; a task holding the slot may have read the profile before the row.
    crate::runtime::interpreter::jit_bridge::owe_one_shot_retry_if_already_queued(
        vm,
        answer,
        seed.class_id,
        &seed.method_name,
        &seed.descriptor,
        "ir-ic-profile-seed",
    );
}


/// JIT round 19 (lane mega19, MEGA5-18-4,
/// `CRATONVM_JIT_MEGA_GATE_FIRST_NO_INLINE_PUBLISH`, default OFF): whether the
/// resolving helper leaves the site's MIC alone because the site's code tests
/// its (set) megamorphic gate before the MIC and every inline way
/// (`JitPICSlot::skips_inline_publication`; the PIC's own `install` skips its
/// inline ways on the same answer). A skipped publication only leaves a cache
/// cold; the helper's own MIC fast path then misses to its full resolution.
///
/// # Safety
///
/// `pic_ptr` is 0 or the live `JitPICSlot` the calling site passed.
pub unsafe fn mic_publication_skipped(pic_ptr: i64) -> bool {
    pic_ptr != 0 && (*(pic_ptr as *const JitPICSlot)).skips_inline_publication()
}

/// Round 13 (lane crash): is this thread's native stack at or below the
/// compiled-recursion floor (`compute_self_call_stack_floor`, the value the
/// self-call guard and the frame-cached `CMP RSP, [rbp - floor]` use)?
///
/// The resolving dispatch helpers ask it before calling a compiled callee, so
/// a recursion closed through inline-cache hits -- which the per-thread
/// dispatch DEPTH never counts -- still ends in a catchable
/// `StackOverflowError` rather than a SIGSEGV on the guard page.
/// `CRATONVM_JIT_IC_STACK_GUARD=0` answers `false`; the flag is read only
/// below the floor, so the common path is one TLS load and a compare.
pub fn jit_native_stack_exhausted() -> bool {
    let probe = 0u8;
    let sp_now = &probe as *const u8 as usize;
    let floor = JIT_SELF_CALL_STACK_FLOOR.with(|f| {
        let v = f.get();
        if v != usize::MAX {
            v
        } else {
            let computed = compute_self_call_stack_floor(sp_now, current_self_call_budget());
            f.set(computed);
            computed
        }
    });
    sp_now <= floor
}



/// gch h1/fr (2026-10-08, `gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes`):
/// `[MIC_TRACE] <callee> kind=<k> cycles=<n>`, printed when the
/// [`jit_invoke_virtual_mic`] call it guards returns, under
/// `CRATONVM_DBG_MIC_TRACE` (the switch of `jit_invoke_dispatch`'s
/// `[DISP_TRACE]`). vg5's trace showed only `[DISP_TRACE]` lines because this
/// door printed none, so a call served here could not be told apart from one
/// no Rust door served at all (a machine-code inline-cache hit or a direct
/// call). Off, it is one cached load per call.
pub struct MicDoorTrace {
    pub label: String,
    pub t0: u64,
}

impl MicDoorTrace {
    #[inline]
    pub fn new(info: &JitInvokeInfo) -> Option<Self> {
        if !mic_prof::trace_enabled() {
            return None;
        }
        Some(Self {
            label: mic_door_trace_label(info),
            t0: mic_prof::now(),
        })
    }
}

impl Drop for MicDoorTrace {
    fn drop(&mut self) {
        eprintln!(
            "[MIC_TRACE] {} cycles={}",
            self.label,
            mic_prof::now().wrapping_sub(self.t0)
        );
    }
}

/// The callee part of a `[MIC_TRACE]` line: `[DISP_TRACE]`'s format.
pub fn mic_door_trace_label(info: &JitInvokeInfo) -> String {
    format!(
        "{}.{}{} kind={}",
        info.class_name, info.method_name, info.descriptor, info.invoke_kind
    )
}


/// `CRATONVM_JIT_MH_REAL_FIELD_LANE` (performance, **default ON**, `=0` is
/// the kill switch; 2026-10-18, page
/// `docs/feature-designs/real-jdk-mode.md`).
/// Read once per process.
pub fn jit_mh_real_field_lane_enabled() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    pub static STATE: AtomicU8 = AtomicU8::new(0);
    match STATE.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on =
                true;
            STATE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

/// Lines of [`MH_REAL_FIELD_LANE_NEG`] (a power of two, direct-mapped).
pub const MH_REAL_FIELD_LANE_NEG_SLOTS: usize = 8;

thread_local! {
    /// Handles [`try_jit_mh_real_field_lane`] can never serve, by (VM
    /// identity, handle word, address epoch): `jit_real_field_lane_never_serves`
    /// said so for that very handle in that epoch, and its answer does not
    /// change for the handle's life. Filed only when the handle's word needed
    /// no forwarding, so the key is the word compiled code passes.
    pub static MH_REAL_FIELD_LANE_NEG: std::cell::Cell<[(usize, usize, u64); MH_REAL_FIELD_LANE_NEG_SLOTS]> =
        const { std::cell::Cell::new([(0, 0, 0); MH_REAL_FIELD_LANE_NEG_SLOTS]) };
}

/// A compiled `MethodHandle.invokeExact` / `invoke` site whose handle is a
/// warm real field accessor (or a proven `bindTo` of one), served here
/// without the native funnel: `None` declines with nothing disturbed, and the
/// helper then routes the call as before.
///
/// Before this, such a call reached the same field access through the whole
/// compiled-to-native route: the constant-bind and compiled-link redirects'
/// probes (asked and declined on every call), the site-cached native door's
/// `VarHandle` / byte-view / segment probes, the argument decode, the
/// `poly_call_site` arm, the full `safe_native_call_*` funnel, the `invoke` /
/// `invokeExact` native's own lane order, then the poly return's unbox. That
/// made a warm getter cost ~0.9 us with the JIT against ~0.5 us interpreted
/// (`L4W95FieldHandleWarm`), and a compiled `()I` bound getter ~3.6 us, where
/// the interpreter's lane answered in ~0.7 us (HotSpot: 10-25 ns).
///
/// What is served is exactly what the natives' before-linker lanes serve,
/// through their own guards (`lang_invoke::jit_real_field_lane_warm`), and only
/// when everything those lanes need is already on file: no Java, no frame
/// walk, no allocation, no GC point. So the call's Java-visible behaviour is
/// the interpreter's lane's, as it already was for an interpreted caller. The
/// arguments are forwarded and heap-validated first, as the helper body does
/// before its own native door; a reference answer goes back raw, with no
/// handoff root, since no GC point follows the read. A getter whose answer is
/// not the site's own variant declines after the read (a read has no
/// effect).
///
/// SAFETY: as [`jit_invoke_virtual_mic`]: pointers supplied by the compiled
/// caller for the current live VM.
// JIT round 23 wave 2f (lane L1): out of line, so the frame of the helper on every
// compiled-call level (which stays live for the depth of a recursion) does not reserve
// this door's locals; it returns before the call that recurses.
#[inline(never)]
pub unsafe fn try_jit_mh_real_field_lane(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
) -> Option<i64> {
    if vm_ptr == 0 || info_ptr == 0 || args_ptr == 0 || !(1..=3).contains(&num_args) {
        return None;
    }
    let info = &*(info_ptr as *const JitInvokeInfo);
    if info.invoke_kind != 0
        || info.declaring_class_id == 0
        || !matches!(info.method_name, "invokeExact" | "invoke")
        || info.class_name != "java/lang/invoke/MethodHandle"
        || !jit_mh_real_field_lane_enabled()
    {
        return None;
    }
    // The site's parameter kinds, one word each, after the handle.
    let mut kinds = [0u8; 2];
    let mut count = 0usize;
    let d = info.descriptor.as_bytes();
    if d.first() != Some(&b'(') {
        return None;
    }
    let mut i = 1usize;
    while *d.get(i)? != b')' {
        let kind = d[i];
        while d.get(i) == Some(&b'[') {
            i += 1;
        }
        if *d.get(i)? == b'L' {
            i += d[i..].iter().position(|&b| b == b';')?;
        }
        i += 1;
        if count >= kinds.len() {
            return None;
        }
        kinds[count] = kind;
        count += 1;
    }
    if count + 1 != num_args as usize {
        return None;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    // A handle this lane can never serve (a method handle, an adapter) was
    // remembered by its word: decline before any heap work. The key is only
    // compared, never dereferenced, and a moved handle just misses.
    let neg_key = (
        vm.vm_identity,
        *(args_ptr as *const i64) as usize,
        vm.mem.address_epoch(),
    );
    let neg_slot = (neg_key.1 >> 3) & (MH_REAL_FIELD_LANE_NEG_SLOTS - 1);
    if MH_REAL_FIELD_LANE_NEG.with(|n| n.get()[neg_slot] == neg_key) {
        return None;
    }
    // The helper body's own prologue before it reads a word of the heap.
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    let raw_args = std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize);
    let (forwarded, validated) = forward_jit_reference_args_validated(vm, info, raw_args);
    let args = forwarded.as_deref().unwrap_or(raw_args);
    let handle = prevalidated_jit_ref(validated, 0, args[0])?;
    let mut extra = [Value::Object(None); 2];
    for i in 0..count {
        let raw = args[i + 1];
        extra[i] = match kinds[i] {
            b'I' | b'Z' | b'B' | b'S' | b'C' => Value::Int(raw as i32),
            b'J' => Value::Long(raw),
            b'F' => Value::Float(f32::from_bits(raw as u32)),
            b'D' => Value::Double(f64::from_bits(raw as u64)),
            b'L' | b'[' if raw == 0 => Value::Object(None),
            b'L' | b'[' => Value::Object(Some(prevalidated_jit_ref(validated, i + 1, raw)?)),
            _ => return None,
        };
    }
    let (thread, _jit_thread_guard) = jit_thread_mut()?;
    let answer = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        let served = cratonvm_native_builtins::lang_invoke::jit_real_field_lane_warm(
            &mut ctx,
            handle,
            &extra[..count],
            info.descriptor,
            info.declaring_class_id,
        );
        match served {
            Some(answer) => answer,
            None => {
                if forwarded.is_none()
                    && cratonvm_native_builtins::lang_invoke::jit_real_field_lane_never_serves(
                        &mut ctx, handle,
                    )
                {
                    MH_REAL_FIELD_LANE_NEG.with(|n| {
                        let mut lines = n.get();
                        lines[neg_slot] = neg_key;
                        n.set(lines);
                    });
                }
                return None;
            }
        }
    };
    let value = match answer {
        Ok(value) => value,
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    Some(match (info.return_type, value) {
        (b'V', _) => 0,
        (b'I' | b'Z' | b'B' | b'S' | b'C', Some(Value::Int(v))) => v as i64,
        (b'J', Some(Value::Long(v))) => v,
        (b'F', Some(Value::Float(v))) => v.to_bits() as i64,
        (b'D', Some(Value::Double(v))) => v.to_bits() as i64,
        (b'L' | b'[', Some(Value::Object(None))) => 0,
        // No handoff root (`native_pending_return`): nothing between the
        // field read and this return is a GC point, and a root left set here
        // would outlive the hand-off (handoff-w36b).
        (b'L' | b'[', Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
        // Not the site's own variant: a getter's read had no effect, so the
        // native route answers the call instead.
        _ => return None,
    })
}

/// JIT round 21, lane dispatch (`CRATONVM_JIT_IC_MIC_PLUS_FOUR`, page
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// may [`jit_invoke_virtual_mic_body`] leave `receiver_cid` out of the site's
/// inline ways? Only at a MIC-first optimizing-tier site whose MIC now holds
/// that class with an entry, and only where the MIC is still published (a
/// gate-first megamorphic site never reads it, `skips_inline_publication`).
/// The class is then served by the MIC inline, by the shared megamorphic
/// table (`publish_mega_dispatch_entry`, still called) and by the helper;
/// never a wrong body: a skipped install publishes nothing.
pub fn pic_way_redundant_with_mic(mic: &JitMICSlot, pic: &JitPICSlot, receiver_cid: u32) -> bool {
    !pic.skips_inline_publication() && pic.mic_serves_class_inline(mic, receiver_cid)
}

/// Body of [`jit_invoke_virtual_mic`], run under its panic guard.
pub unsafe fn jit_invoke_virtual_mic_body(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
    mic_ptr: i64,
    pic_ptr: i64,
) -> i64 {
    let res = jit_invoke_virtual_mic_body_inner(vm_ptr, info_ptr, args_ptr, num_args, mic_ptr, pic_ptr);
    if res != i64::MIN && num_args == 3 && info_ptr != 0 {
        let info = &*(info_ptr as *const JitInvokeInfo);
        if info.invoke_kind == 0
            && info.class_name == "java/lang/reflect/Method"
            && info.method_name == "invoke"
            && info.descriptor == "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;"
        {
            crate::jit::helpers::dispatch::try_populate_method_invoke_cache(
                vm_ptr, info, args_ptr,
            );
        }
    }
    res
}

unsafe fn jit_invoke_virtual_mic_body_inner(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
    mic_ptr: i64,
    pic_ptr: i64,
) -> i64 {
    // JIT round 14 wave 2 (lane mic, M8-1): past the probe that missed, before
    // any probe of ours -- an inline-cache quiescent point. Must stay first.
    cratonvm_jit::jit_ic_quiescent_point();
    // JIT round 16 (lane mh, MH15-5): see `mh_constant_bind_redirect`.
    let mut mh_link_threw = false;
    if let Some(route) =
        mh_constant_bind_redirect(vm_ptr, info_ptr, args_ptr, num_args, &mut mh_link_threw)
    {
        return mh_constant_bind_call(vm_ptr, route, args_ptr, num_args);
    }
    if mh_link_threw {
        // Round 18 wave 11 (lane mhresid): the site's link stashed its error.
        return i64::MIN;
    }
    // JIT round 20 wave 19, lane pRefl (`CRATONVM_JIT_MH_COMPILED_LINK`): a
    // linked `invokeExact`, an `invokeBasic` or a `linkTo*` calls its Java
    // target as a compiled call, not through its native door.
    if let Some(bits) = mh_compiled_link_redirect(vm_ptr, info_ptr, args_ptr, num_args) {
        return bits;
    }
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Round-7 fix (CRIT, audit §3): SATB safepoint flush.
    jit_safepoint_flush_satb(vm_ptr);
    mic_prof::dump_maybe();
    let _cyc_total = mic_prof::CycGuard::new(&mic_prof::CYC_MIC_TOTAL);
    let vm = &*(vm_ptr as *const SharedVm);
    let info = &*(info_ptr as *const JitInvokeInfo);
    // gch h1/fr: this door's per-call line, on `[DISP_TRACE]`'s switch.
    let _mic_trace = MicDoorTrace::new(info);
    let _callee_guard = if crate::runtime::env_cache::jit_putfield_diag() {
        Some(JitCalleeGuard::new(info))
    } else {
        None
    };
    let cv_trace = cv_trace_match(info);
    if cv_trace {
        let mut bits = [0i64; 4];
        if num_args > 0 && !(args_ptr as *const i64).is_null() {
            for (i, b) in bits.iter_mut().enumerate().take((num_args as usize).min(4)) {
                *b = *(args_ptr as *const i64).add(i);
            }
        }
        eprintln!(
            "[cv-mic-entry] cp_class={} num_args={} a0={:#x} a1={:#x}",
            info.class_name, num_args, bits[0], bits[1]
        );
    }
    if num_args < 0 || (num_args > 0 && (args_ptr as *const i64).is_null()) {
        if cv_trace {
            eprintln!("[cv-mic-earlyout] bad num_args/args_ptr -> silent 0");
        }
        return 0;
    }
    let args_slice = if num_args == 0 {
        &[] as &[i64]
    } else {
        std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize)
    };

    // BUG-1: native-stack recursion guard for the JIT→JIT virtual dispatch
    // path (the `invokevirtual` sibling of `jit_invoke_dispatch`). Same
    // contract: bump the per-thread JIT-dispatch depth, and on overflow stash
    // a catchable `StackOverflowError` + return the `i64::MIN` deopt sentinel
    // rather than recursing into a compiled callee and blowing the OS stack.
    // Held for the rest of the call so the level releases on every return.
    if vm.config.compatibility_mode == cratonvm_types::CompatibilityMode::Compatible
        && crate::runtime::interpreter::http_carrier_declaring_class(info.class_name)
    {
        return crate::jit::helpers::dispatch::jit_invoke_dispatch_body(
            vm_ptr, info_ptr, args_ptr, num_args,
        );
    }
    let _jit_dispatch_depth_guard =
        match enter_jit_dispatch(vm, constructor_receiver(info, args_slice)) {
            Ok(g) => g,
            Err(sentinel) => return sentinel,
        };
    // Round 13 (lane crash): the native-stack floor check for a recursion
    // whose other levels are raw inline-cache hits now runs inside
    // `enter_jit_dispatch` above (round 13 wave 2, lane mega: patch 2), which
    // the inline MIC/PIC guard's low-stack edge reaches through this helper.

    let (thread, _jit_thread_guard) = match jit_thread_mut() {
        Some(t) => t,
        None => {
            if cv_trace {
                eprintln!("[cv-mic-earlyout] no jit thread -> silent 0");
            }
            return 0;
        }
    };

    if args_slice.is_empty() {
        if cv_trace {
            eprintln!("[cv-mic-earlyout] empty args -> silent 0");
        }
        return 0;
    }
    if args_slice.len() == 2
        && args_slice[1] == 0
        && matches!(
            (info.method_name, info.descriptor),
            ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
                | (
                    "getResources",
                    "(Ljava/lang/String;)Ljava/util/Enumeration;"
                )
                | (
                    "getResourceAsStream",
                    "(Ljava/lang/String;)Ljava/io/InputStream;"
                )
        )
        && null_resource_name_receiver_is_class_loader(
            vm,
            args_slice[0],
            info.method_name,
            info.descriptor,
        )
    {
        set_jit_pending_npe();
        return i64::MIN;
    }
    let receiver_raw = args_slice[0];
    if receiver_raw == 0 {
        // Null receiver: throw NullPointerException (JVM semantics), mirroring
        // the `jit_invoke_dispatch` fix. Set the pending-NPE flag + return the
        // i64::MIN sentinel so the interpreter's post-JIT drain builds the real
        // NPE and routes it through the method's exception table. Was `return
        // 0`, which silently swallowed a null-receiver call in hot JIT'd code.
        //
        // `INVOKE_RECEIVER`, and not the bare setter, for the reason the
        // dispatch-helper twin gives: the code names the opcode family that
        // trapped so `runtime::interpreter::jit_npe_message` can corroborate
        // the bci it recovers from this frame's safepoint-id slot. The two
        // doors must agree — a `getMessage()` that depended on whether the call
        // site had warmed into a monomorphic inline cache would be its own
        // wrong answer.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // Defensive: a receiver slot carrying tagged-long bits (low 3 bits set
    // or value above the 48-bit canonical-address ceiling) is not a valid
    // heap pointer.  Bail out with rc=0 (the dispatcher's "no result"
    // path); this mirrors the receiver_raw == 0 short-circuit above and
    // avoids the `ObjectRef::from_raw` alignment panic.
    let receiver_bits = receiver_raw as u64;
    if (receiver_bits & 0x7) != 0 || receiver_bits >= (1u64 << 48) {
        if cv_trace {
            eprintln!(
                "[cv-mic-earlyout] misaligned/tagged receiver {:#x} -> silent 0",
                receiver_bits
            );
        }
        return 0;
    }
    // SAFETY: receiver_bits is non-zero, 8-byte aligned, and within the
    // 48-bit canonical address space — matches the invariants required by
    // ObjectRef::from_raw for live heap objects.
    // The SATB flush above may have moved either the receiver or a reference
    // parameter.  A MIC hit jumps straight into compiled code with this raw
    // slice, so canonicalize every reference before the class lookup and the
    // eventual direct call.
    //
    // FUNNEL17-2 (`CRATONVM_JIT_NATIVE_WALK_ONCE`, JIT round 19 wave 2, lane
    // funnel2): see the sibling in `jit_invoke_dispatch_body`. Between here
    // and the native-site door below only the array-`clone` arm can run
    // anything, and it returns when it does.
    let (forwarded_args, prevalidated) = if vm.natives.native_methods.jit_native_walk_once() {
        forward_jit_reference_args_validated(vm, info, args_slice)
    } else {
        (forward_jit_reference_args(vm, info, args_slice), 0)
    };
    let args_slice = forwarded_args.as_deref().unwrap_or(args_slice);
    // jr20 wave 9, lane gcE (gch-h1jp item 2): a forwarded Rust copy of the
    // arguments is in no root set, so every arm below that calls a compiled
    // body with it roots its words across the call and the sentinel service
    // (`CompileArgPins`, read back through `args_now`): a re-run from entry
    // after a moving collection inside the callee must not read the copy's
    // pre-move words. Rare (ZGC's lazy forwarding), so the cost is too.
    let args_forwarded = forwarded_args.is_some();
    let receiver_raw = args_slice[0];
    let receiver_ref = ObjectRef::from_raw(receiver_raw as usize as *mut u8);

    // Strings round (`str16`): an ARRAY receiver's `clone()`. JDK 25
    // `Arrays.copyOf` / `copyOfRange` return `original.clone()` for a
    // whole-array copy (every `StringBuilder.toString()` of a builder whose
    // `value` is exactly full), and the site cache below declines every array
    // receiver (its class-id key would alias the component's class), so each
    // call took the whole MIC resolution and a by-name `invoke_or_native` walk
    // to reach `Object.clone`'s registered native. Call that native here,
    // resolved and admitted by the same policy test the leaf-native doors use.
    if info.method_name == "clone"
        && info.descriptor == "()Ljava/lang/Object;"
        && args_slice.len() == 1
        && matches!(
            jit_dispatch_class(vm, receiver_ref),
            JitDispatchClass::JavaLangObject
        )
    {
        let clone_native = vm
            .natives
            .native_methods
            .resolve_id("java/lang/Object", "clone", "()Ljava/lang/Object;")
            .and_then(|id| {
                let callback = vm.natives.native_methods.callback_of(id)?;
                admit_jit_fast_native_resolved(
                    vm,
                    "java/lang/Object",
                    "clone",
                    "()Ljava/lang/Object;",
                    callback,
                    Some(id),
                )
            });
        if let Some((callback, native_id)) = clone_native {
            count_jit_native_dispatch(vm, native_id);
            let values = [Value::Object(Some(receiver_ref))];
            return match crate::vm::safe_native_call(vm, thread, callback, &values) {
                Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
                Ok(_) => 0,
                Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
            };
        }
    }

    // Leaf-native fast path — the invokevirtual/invokeinterface half.
    //
    // This entry point, not `jit_invoke_dispatch`, is where compiled virtual
    // calls arrive: the x64 backend emits `jit_invoke_virtual_mic` for them and
    // reserves the generic dispatcher for static/special sites and bailouts.
    // Hooking only the dispatcher left every *instance* leaf native — the whole
    // `AtomicInteger`/`AtomicLong` accessor set, i.e. item 2 of the AQS doc —
    // still paying the full funnel, with the site never even reaching
    // `resolve_native_site` to be counted as a refusal. `AtomicInteger.get`
    // measured 1026 ns before and 926 ns after the dispatcher-only version:
    // unchanged, while `Math.abs` (static, same mechanism) went 330 -> 84.
    //
    // Placed immediately after `forward_jit_reference_args` so the receiver this
    // reads is the post-SATB-flush address, and before the MIC/PIC machinery,
    // which a native leaf has no use for — there is no compiled callee to cache.
    //
    // `NATIVE_SITE_CACHE` is keyed on `(vm_identity, info_ptr)`, and a
    // `JitInvokeInfo` box dies with its `CompiledMethod`, so the key can already
    // name a different call site. Revalidate here rather than at the MIC/PIC
    // block further down, which this path returns before ever reaching. See
    // `flush_raw_entry_dispatch_caches`.
    flush_raw_entry_dispatch_caches();
    // Diagnostic (`CRATONVM_DBG_SITE_ALIAS`): does this key still name its site?
    // The gate is here rather than inside, for the same reason as the sibling
    // call site above: an all-off run must not make the call at all. This was
    // the ONE of the three call sites that had the comment but not the `if`, so
    // the "diagnostic" printed `[site-alias]` lines on stderr and grew the
    // unbounded `SITE_IDENTITY` map on every raw-entry native dispatch in an
    // ordinary run — caught by `scripts/jdk-only-strict-probes.sh`, which saw
    // the lines in BOTH CratonVM arms of `JdkOnlyPlatformProbe`.
    if site_alias_detect_enabled() {
        note_site_identity(jit_site_key(vm.vm_identity, info_ptr as usize), info);
    }
    if let Some(result) = try_jit_site_cached_native_dispatch(
        vm,
        Some(thread),
        info,
        jit_site_key(vm.vm_identity, info_ptr as usize),
        args_slice,
        prevalidated,
    ) {
        disp_census::note(disp_census::MIC_SITE_NATIVE);
        return result;
    }
    // Round 15 wave 3 (lane vh3): an invoke-exact `VarHandle` the site cache
    // declined is judged here, as `jit_invoke_dispatch`'s tail does; every arm
    // below ends in `invoke_or_native`, which cannot. A name test, then one
    // relaxed load until the VM mints an exact handle.
    if crate::vm::vm_exec::is_var_handle_signature_polymorphic_receiver(info.class_name) {
        if let Some(result) = jit_mic_var_handle_exact_refusal(vm, &mut *thread, info, args_slice) {
            return result;
        }
    }

    // WS1 (kafka JIT throughput): the `Value` decode is deferred. The MIC-hit
    // fast path dispatches straight off the raw `args_slice` and never
    // materializes `Value`s; only the lambda, register-overflow-bailout and
    // full-resolution paths pay for the decode. Eagerly building this Vec
    // (one heap alloc + a descriptor parse) on every call — including pure
    // cache hits — was a measured contributor to JIT'd call-heavy code
    // running slower than the interpreter.
    let decode_values = || -> JitDecodedArgs {
        let mut values = JitDecodedArgs::with_capacity(args_slice.len());
        values.push(Value::Object(Some(receiver_ref)));
        let mut desc_iter = DescriptorParamIter::new(info.descriptor);
        for &raw in &args_slice[1..] {
            let val = match desc_iter.next() {
                Some(b'I') | Some(b'B') | Some(b'C') | Some(b'S') | Some(b'Z') => {
                    Value::Int(raw as i32)
                }
                Some(b'J') => Value::Long(raw),
                Some(b'F') => Value::Float(f32::from_bits(raw as u32)),
                Some(b'D') => Value::Double(f64::from_bits(raw as u64)),
                Some(b'L') | Some(b'[') => {
                    if raw == 0 {
                        Value::Object(None)
                    } else {
                        // Tagged-long bits leaked into an L/[ slot (e.g. a
                        // primitive that should have been boxed before this
                        // virtual call) are downgraded to null instead of
                        // being treated as a heap pointer. The alignment +
                        // 48-bit-ceiling check alone is NOT sufficient: a
                        // round-number primitive long (e.g. a millisecond
                        // timeout like 60000/120000) is also 8-byte-aligned
                        // and well under 2^48, so it silently passed as a
                        // "plausible" pointer here and `ObjectRef::from_raw`
                        // fabricated a bogus reference into unmapped memory --
                        // confirmed via a live gdb repro: `XnioWorker$Builder
                        // .set(Option, Object)` receiving an unboxed 120000
                        // (0x1d4c0) segfaulted inside `read_string` when the
                        // bogus ObjectRef's header was dereferenced. Match
                        // `decode_dispatch_values`'s sibling path (used by the
                        // cache-miss/bailout decode) and require actual heap
                        // membership, not just bit-pattern plausibility.
                        let bits = raw as u64;
                        let validated = if (bits & 0x7) == 0 && bits < (1u64 << 48) {
                            vm.mem.heap.is_object_address(bits as usize)
                        } else {
                            None
                        };
                        if cv_trace && validated.is_none() {
                            eprintln!(
                                "[cv-mic-decode] arg bits {:#x} FAILED is_object_address -> downgraded to null",
                                bits
                            );
                        }
                        match validated {
                            Some(obj) => Value::Object(Some(obj)),
                            None => Value::Object(None),
                        }
                    }
                }
                _ => Value::Int(raw as i32),
            };
            values.push(val);
        }
        values
    };

    // Interpreter round i3 wave 81, lane L3 (`CRATONVM_JIT_REFLECT_DOOR_DIRECT`,
    // per VM, default OFF): a compiled `Method.invoke` /
    // `Constructor.newInstance` call. Both doors are served by a forced
    // native in either mode (the interpreter's cached door,
    // `native_override_for_cached_reflect_invoke`), and the native site cache
    // above refuses the `invoke` name (`site_name_is_special_cased`), so
    // without this arm every call ran the MIC resolution, the compile probe
    // and the by-name `invoke_or_native` cascade below to reach the same
    // callback. The site's owner pins the receiver: both classes are final
    // and declare the method, so the call-site triple is the receiver's.
    // Off: one registry field load per helper call that gets here.
    if vm.natives.native_methods.jit_reflect_door_direct() {
        if let Some(callback) = crate::runtime::interpreter::jit_forced_reflect_door_native(
            vm,
            info.class_name,
            info.method_name,
            info.descriptor,
        ) {
            let values = decode_values();
            // Interpreter round i3 wave 83, lane L3
            // (`CRATONVM_JIT_REFLECT_DOOR_LEAN_SITE`, default off): file the
            // site in the lean native table, so the next call is served at the
            // helper's first statement (`try_lean_native_door`) instead of
            // after this body's prefix. Off: one per-VM bool load.
            if vm.config.jit_reflect_door_lean_site {
                reflect_door_lean_site_fill(vm, info, info_ptr as usize, receiver_ref, callback);
            }
            // The census's `gap_in` split (`CRATONVM_REFLECT_DOOR_PHASE_STATS`).
            crate::vm::reflect_door_gate_stamp(
                vm,
                cratonvm_native_builtins::ReflectDoorGatePoint::Call,
            );
            return match crate::vm::safe_native_call(vm, thread, callback, &values) {
                Ok(Some(Value::Int(v))) => v as i64,
                Ok(Some(Value::Long(v))) => v,
                Ok(Some(Value::Float(f))) => f.to_bits() as i64,
                Ok(Some(Value::Double(d))) => d.to_bits() as i64,
                Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
                Ok(Some(Value::Object(None)) | None) => 0,
                Ok(_) => 0,
                Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
            };
        }
    }

    // ClassLoader's resource accessors require a non-null resource name. The
    // MIC fast path dispatches on a custom loader's runtime class and can call
    // inherited JDK bytecode without consulting the ClassLoader callback that
    // owns this contract. Handle the null-only case before cache lookup; normal
    // resource calls still use the loader's virtual implementation unchanged.
    //
    // Only for a receiver that IS a ClassLoader, as at the other two sites: an
    // unrelated interface with a `getResource(String)` method (a `Res` of the
    // user's own) must run its own implementation. This site lacked the check,
    // which went unseen while a protected call site's method stayed
    // interpreted; round 11 wave 8's putstatic admission compiled one
    // (R11RtGetResourceNull: 56000 spurious NPEs).
    if args_slice.len() == 2
        && args_slice[1] == 0
        && matches!(
            (info.method_name, info.descriptor),
            ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
                | (
                    "getResources",
                    "(Ljava/lang/String;)Ljava/util/Enumeration;"
                )
                | (
                    "getResourceAsStream",
                    "(Ljava/lang/String;)Ljava/io/InputStream;"
                )
        )
        && null_resource_name_receiver_is_class_loader(
            vm,
            args_slice[0],
            info.method_name,
            info.descriptor,
        )
    {
        // §7 routing. This was a bare `NativeMethodRegistry::find`, which
        // discards the `NativeKind` and so could not tell a reviewed intrinsic
        // from a `SyntheticStub` — the exact shape §1.3 forbids invoking. It
        // now resolves kind-aware and goes through the shared admission
        // helper, which also yields the §4 census handle.
        //
        // Unlike the three cached fast paths, this site resolves per call —
        // but it is guarded by `args_slice[1] == 0`, i.e. only a `null`
        // resource name reaches it, which is the JVM-mandated NPE path and
        // never hot. The full policy round trip is affordable here precisely
        // because this edge is an error path, not a throughput path.
        //
        // Declining (`None`) falls through to the ordinary MIC resolution
        // below — byte-for-byte what already happened whenever `find` returned
        // `None`, and a path that ends in `invoke_or_native`, which is itself
        // policy-checked.
        let loader_native = vm
            .natives
            .native_methods
            .resolve_id("java/lang/ClassLoader", info.method_name, info.descriptor)
            .and_then(|id| {
                let callback = vm.natives.native_methods.callback_of(id)?;
                admit_jit_fast_native_resolved(
                    vm,
                    "java/lang/ClassLoader",
                    info.method_name,
                    info.descriptor,
                    callback,
                    Some(id),
                )
            });
        if let Some((callback, native_id)) = loader_native {
            let values = decode_values();
            count_jit_native_dispatch(vm, native_id);
            return match crate::vm::safe_native_call(vm, thread, callback, &values) {
                Ok(Some(Value::Int(v))) => v as i64,
                Ok(Some(Value::Long(v))) => v,
                Ok(Some(Value::Float(f))) => f.to_bits() as i64,
                Ok(Some(Value::Double(d))) => d.to_bits() as i64,
                Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
                Ok(Some(Value::Object(None)) | None) => 0,
                Ok(_) => 0,
                Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
            };
        }
    }

    // Round 11 wave 13 (lane `icwide`): read through `jit_dispatch_class`, so
    // an array's id here is 0 rather than its COMPONENT's. That is what keeps
    // an array of annotation proxies off the annotation arm and an array of
    // lambda proxies off the lambda arms below (both keyed on this id, and
    // neither screened by `receiver_is_plain_object`); every other reader
    // either is screened by it or refuses class 0.
    let receiver_dispatch = jit_dispatch_class(vm, receiver_ref);
    let receiver_class_id = receiver_dispatch
        .class_id()
        .unwrap_or_else(|| ClassId::new(0));
    let receiver_cid = receiver_class_id.as_u32();
    // A reference array stores its COMPONENT class id in
    // `ObjectHeader.class_id` — the same invariant the KC26 array.clone() note
    // further down calls out — so `receiver_cid` alone cannot tell a `Foo[]`
    // from a `Foo`. Per JVMS §4.4.1 an array class inherits `Object`'s method
    // table, not its component's, so every class-id-keyed fast path below must
    // refuse array receivers and let the resolving miss path (which asks
    // `virtual_dispatch_target_for_receiver`, and gets `java/lang/Object`)
    // handle them. Without this a MIC warmed on a `Foo` receiver called
    // straight into compiled `Foo.equals` with a `Foo[]` as `this`, and its
    // first `checkcast Foo` threw
    // `ClassCastException: class [LFoo; cannot be cast to class Foo` — the
    // `ResolvableType[]`/`ResolvableType` Spring Boot failure this was found
    // as. The inline machine-code cascades in `jit/src/x64.rs`,
    // `jit/src/ir_lower.rs` and `jit/src/runtime_lowering.rs` carry the same
    // guard against `ObjectHeader.kind`.
    let receiver_is_plain_object = receiver_dispatch.class_id().is_some();

    // Real-layout Matcher methods are registered natives, so they can never
    // publish a compiled entry into the ordinary MIC/PIC. Without this leaf
    // path every `find`/`start`/`end`/`group` hit decodes a heap-allocated
    // `Vec<Value>`, retries a compile probe, and performs generic virtual/native
    // resolution. Guarding the exact receiver class preserves subclass
    // overrides; the feature gate in `matcher_native_callback_uncached`
    // preserves the native opt-out, and class redefinition keeps using the
    // generic resolver.
    //
    // §7 routing — this leaf is the one HOT by-name native fast path in this
    // file that re-decides on every dispatch rather than at cache-fill time
    // (the `ClassLoader` intercept above is also unmemoized, but only a null
    // resource name reaches it, so it can afford the real thing). Wave 1
    // therefore hoisted the Real-JDK question to one
    // `dispatch_policy(vm).is_jdk_only()` gate and skipped the leaf wholesale
    // under strict, because calling `admit_jit_fast_native` per dispatch would
    // put a three-string registry hash — and, in strict mode, a class-manager
    // read lock plus a hierarchy walk — on a documented 19x hot path.
    //
    // Closed 2026-08-10 by [`admit_matcher_leaf_native`], the per-triple memo
    // the wave-2 record asked for: eight `NativeCallSite` cells for the census
    // handle and a per-VM generation-keyed verdict cell for the strict
    // admission. Warm cost is two `&'static str` compares and one (Compatible)
    // or two (strict) relaxed loads, so the leaf keeps its shape while the
    // dispatch becomes counted in BOTH modes and strict regains the fast route
    // instead of merely being safe without it. See that function for the full
    // argument.
    //
    // The site's static class is asked FIRST (`matcher_leaf_site_class`): the
    // triples are plain names (`start()I`, `end()I`, `find()Z`) that unrelated
    // classes declare too, and for such a site `is_exact_matcher_class` takes
    // the class-manager read lock on every call (it memoizes only a match),
    // as does `class_was_redefined` once anything has been redefined.
    if matcher_leaf_site_class(info) && !class_was_redefined(vm, receiver_class_id) {
        if let Some((callback, native_id)) = admit_matcher_leaf_native(vm, info) {
            if is_exact_matcher_class(vm, receiver_class_id) {
                if let Some(result) = call_matcher_native_raw(
                    vm,
                    thread,
                    info,
                    receiver_ref,
                    args_slice,
                    callback,
                    native_id,
                ) {
                    // §4 census: one `Option` test and one relaxed add, on the
                    // id resolved by the memo above. Counted here rather than
                    // before the call because `call_matcher_native_raw`
                    // declines an arity it cannot build a frame for, and a
                    // declined fast path is not a dispatch.
                    count_jit_native_dispatch(vm, native_id);
                    JIT_COUNTERS.with(|b| JitCounterBlock::bump(&b.matcher_leaf_hits));
                    return result;
                }
            }
        }
    }

    // AnnotationProxy receiver: synthetic class with no bytecode methods, so the
    // cache-miss path below would resolve on it, fail the compile-probe, and
    // call `invoke_or_native` only to hit its annotation rescue — every call.
    // Route straight to the shared annotation dispatch here. Uses the lock-free
    // cached cid (warmed by `invoke_or_native`'s fast-path); before it is warmed
    // the hint is `u32::MAX` (never a real cid) so this simply falls through.
    if receiver_cid == crate::vm::annotation_proxy_cid_hint(vm) {
        mic_prof::bump(&mic_prof::MIC_LAMBDA);
        let values = decode_values();
        match crate::vm::annotation_proxy_invoke_shared(
            vm,
            thread,
            receiver_ref,
            info.method_name,
            &values[1..],
        ) {
            Ok(result) => {
                return match result {
                    Some(Value::Int(v)) => v as i64,
                    Some(Value::Long(v)) => v,
                    Some(Value::Float(f)) => f.to_bits() as i64,
                    Some(Value::Double(d)) => d.to_bits() as i64,
                    Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
                    Some(Value::Object(None)) | None => 0,
                    _ => 0,
                };
            }
            Err(e) => return handle_jit_dispatch_error(vm, thread, e, info),
        }
    }

    // Lambda-proxy receiver: its class id is a synthetic id absent from the
    // class store, so the resolution below derives an EMPTY class name and
    // `invoke_or_native("")` surfaces as a message-less
    // `NoClassDefFoundError` — e.g. `Consumer.accept` inside a JIT-compiled
    // `CollectionUtils.forEachInReverseOrder` (JUnit5 listener notification)
    // died on the first compiled execution. Mirror the interpreter's
    // invokeinterface route: dispatch through the lambda's SAM impl_handle.
    if vm.classes.is_lambda_proxy_class(receiver_class_id) {
        mic_prof::bump(&mic_prof::MIC_LAMBDA);
        // The lambda call site's own cached invoke target. Tried FIRST: it is
        // the only arm here that does not re-derive the whole dispatch, and
        // when it declines it has done nothing.
        if let Some(result) = try_lambda_site_direct_call(
            vm,
            thread,
            receiver_ref,
            receiver_class_id,
            info,
            args_slice,
            vm_ptr,
            Some((mic_ptr, pic_ptr)),
        ) {
            return result;
        }
        match try_fast_lambda_int_to_double_apply(
            vm,
            thread,
            receiver_ref,
            receiver_class_id,
            info,
            args_slice,
        ) {
            Ok(Some(result)) => return result,
            Ok(None) => {}
            Err(error) => return handle_jit_dispatch_error(vm, thread, error, info),
        }
        let values = decode_values();
        match crate::runtime::interpreter::try_lambda_dispatch(
            vm,
            thread,
            receiver_ref,
            receiver_class_id,
            info.method_name,
            info.descriptor,
            &values[1..],
        ) {
            Ok(Some(result)) => {
                return match result {
                    Some(Value::Int(v)) => v as i64,
                    Some(Value::Long(v)) => v,
                    Some(Value::Float(f)) => f.to_bits() as i64,
                    Some(Value::Double(d)) => d.to_bits() as i64,
                    Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
                    Some(Value::Object(None)) | None => 0,
                    _ => 0,
                };
            }
            // SAM arity/name mismatch (e.g. a same-named default method) —
            // fall through; the CP interface fallback below dispatches the
            // default body via the static call-site class.
            Ok(None) => {}
            Err(e) => return handle_jit_dispatch_error(vm, thread, e, info),
        }
    }

    if mic_ptr == 0 {
        return jit_invoke_dispatch_body(vm_ptr, info_ptr, args_ptr, num_args);
    }
    let mic = &*(mic_ptr as *const JitMICSlot);
    // Companion to the call in `jit_invoke_dispatch`: both of this function's
    // resolution sites read `VIRTUAL_TARGET_CACHE`, so both need it revalidated
    // against the current class-identity generation first.
    flush_class_identity_dispatch_memos(vm);
    // Flush this inline cache ONCE per redefinition, not on every dispatch.
    //
    // This was `hierarchy_was_redefined(vm, receiver_class_id)`, which is
    // permanently true once any class in the receiver's superclass chain has
    // been redefined. It sat in front of an unconditional `clear_compiled_entry`
    // + `clear_entries`, so after a single `Mockito.mock(Foo.class)` every
    // subsequent call on a `Foo` erased its own MIC and PIC and then re-resolved
    // the target from scratch — for the life of the process. That is the bulk
    // of the ~30 us/call redefined-dispatch cost.
    //
    // The slot now stamps the redefinition epoch it was last validated against
    // (`JitMICSlot::redefine_epoch`, in what used to be padding). A mismatch
    // means a redefinition has happened since this slot was populated, so the
    // cached entry may name evicted code: flush, restamp, and resolve normally.
    // A match means the slot was populated after the most recent redefinition
    // and is as trustworthy as any other inline cache. Steady state is one
    // relaxed load and a compare.
    let epoch_now = vm.jit.jit_cache.redefine_epoch();
    // Read first; write only when the epoch actually moved.
    //
    // This was an unconditional `swap`, i.e. a locked read-modify-write on the
    // MIC slot's first eight bytes on EVERY helper-side dispatch — including
    // every run in which nothing is ever redefined, where it stored the value
    // that was already there. `redefine_epoch` lives at offset 4 and
    // `cached_class_id` at offset 0 (see `JitMICSlot`'s layout note), so the
    // store dirtied the very line the inline machine-code MIC cascade loads on
    // every probe, and did so from each thread sharing the site.
    //
    // The semantics are unchanged: a load that already equals `epoch_now`
    // proves the slot was populated after the most recent redefinition, which
    // is exactly what the `swap`'s `prev == epoch_now` arm concluded.
    let redefine_jit_quiesced = if mic
        .redefine_epoch
        .load(std::sync::atomic::Ordering::Acquire)
        == epoch_now
    {
        false
    } else {
        mic.redefine_epoch
            .swap(epoch_now, std::sync::atomic::Ordering::AcqRel)
            != epoch_now
    };
    if redefine_jit_quiesced {
        mic.clear_compiled_entry();
        if pic_ptr != 0 {
            let pic = &*(pic_ptr as *const JitPICSlot);
            pic.clear_entries();
        }
        VIRTUAL_DISPATCH_CACHE.with(|dc| dc.borrow_mut().clear());
    }
    // Interpreter-only mode (a JVMTI interpreter-only event, or a JDWP
    // breakpoint / step / suspension): every arm below that enters a compiled
    // callee, or compiles one to publish into the MIC/PIC, stands down with the
    // redefinition quiescence, exactly as `jit_invoke_dispatch_body`'s arms do.
    // The callee then runs through `invoke_or_native`, whose interpreter doors
    // decline compiled code the same way, so MethodEntry/MethodExit and
    // breakpoints are delivered. The inline machine-code cascade never reaches
    // this helper while its slot holds an entry; the edge into the mode
    // flushes every slot (`JitCache::clear_inline_caches`, wave 9), and while
    // the mode holds this helper publishes nothing. Interpreter round i1
    // wave 6, L7.
    let compiled_entry_forbidden =
        redefine_jit_quiesced || crate::runtime::interpreter::jvmti_requires_interpreter(vm);
    // This helper publishes raw compiled entries into `VIRTUAL_DISPATCH_CACHE`
    // for callees the MIC/PIC cannot hold, so it owes that cache the same
    // generation revalidation `jit_invoke_dispatch` does.
    flush_raw_entry_dispatch_caches();
    let cached_cid = mic
        .cached_class_id
        .load(std::sync::atomic::Ordering::Acquire);

    // A callee that declares an exception table is barred from the MIC and the
    // PIC (`mic_callee_has_exception_table`), so nothing ever writes this slot's
    // class id for it and **every** call to it is a "miss" — for the life of the
    // process. `VIRTUAL_DISPATCH_CACHE` exists precisely to serve those callees,
    // and the consult used to sit on the miss path *behind* `mic.record_miss`
    // and `virtual_dispatch_target_cached` — a memoized-but-real class-name
    // resolution and round-trip test the warm hit does not need. It is hoisted
    // here, ahead of the megamorphic PIC probe as well, because a hit means a
    // compiled Java callee was published for exactly this `(site, receiver
    // class)` pair and none of the work below can improve on that.
    //
    // Ordering obligations, all already discharged above: `forward_jit_reference_args`
    // has canonicalised the arguments, `flush_raw_entry_dispatch_caches` and
    // `flush_class_identity_dispatch_memos` have revalidated the cache against
    // the current generations, and `redefine_jit_quiesced` carries the
    // redefinition epoch. `cached_cid != receiver_cid` keeps the MIC's own
    // machine-callable entry first when it has one.
    //
    // The consult it replaces was additionally gated on `cacheable_receiver &&
    // globally_named`, and dropping those here is sound rather than convenient:
    // both are PUBLICATION conditions (`publish_mic_rust_cached_entry` is called
    // under them), and the key carries the receiver's own class id — so a hit
    // means a compiled callee was published for exactly this `(site, receiver
    // class)` pair while those conditions held. Re-testing them would mean
    // calling `virtual_dispatch_target_cached`, which is the work this hoist
    // exists to skip. What is kept is the receiver guard the MIC's own fast path
    // uses one branch below — `receiver_is_plain_object` — so this arm is no
    // weaker than the machine-code entry it stands in for.
    //
    // Worth 11x on a monomorphic interface call whose callee holds a never-taken
    // `try`/`catch`: 220 ns against 19 ns for the identical call without one
    // (`probes/NativeFunnelFloorProbe.java`, ABBA on one binary). Most of that
    // 11x is the inline machine-code cascade this callee cannot be published
    // into at all, which is a separate and much larger piece of work; this is
    // the part that can be taken off the Rust route without touching codegen.
    if cached_cid != receiver_cid && receiver_is_plain_object && !compiled_entry_forbidden {
        if let Some(rc) = try_mic_rust_cached_entry(
            vm,
            thread,
            info,
            info_ptr,
            receiver_cid,
            vm_ptr,
            args_slice,
            args_forwarded,
        ) {
            disp_census::note(disp_census::MIC_RUST_CACHE);
            return rc;
        }
    }

    if crate::runtime::env_cache::jit_mic_dbg() {
        let (pic_classes, pic_entries, pic_contexts) = if pic_ptr == 0 {
            (
                [0; cratonvm_jit::JIT_PIC_ENTRIES],
                [0; cratonvm_jit::JIT_PIC_ENTRIES],
                [false; cratonvm_jit::JIT_PIC_ENTRIES],
            )
        } else {
            let pic = &*(pic_ptr as *const JitPICSlot);
            let ways: [(u32, u64, bool); cratonvm_jit::JIT_PIC_ENTRIES] =
                std::array::from_fn(|i| pic.way(i).unwrap_or((0, 0, false)));
            (
                std::array::from_fn(|i| ways[i].0),
                std::array::from_fn(|i| ways[i].1),
                std::array::from_fn(|i| ways[i].2),
            )
        };
        eprintln!(
            "[JIT_MIC] {}.{}{} cached_cid={} recv_cid={} entry={} pic_ptr={:#x} pic_classes={:?} pic_entries={:?} pic_contexts={:?}",
            info.class_name,
            info.method_name,
            info.descriptor,
            cached_cid,
            receiver_cid,
            mic.cached_entry().0,
            pic_ptr,
            pic_classes,
            pic_entries,
            pic_contexts,
        );
    }

    // Megamorphic helper fast path. Generated code probes only the four-entry
    // inline PIC; receiver types beyond that capacity land here. The PIC keeps
    // a bounded secondary target cache so those misses still avoid repeated
    // class hierarchy resolution and compile-cache probing.
    if cached_cid != receiver_cid
        && receiver_is_plain_object
        && pic_ptr != 0
        && direct_virtual_compiled_callee_entry_enabled()
        && !compiled_entry_forbidden
    {
        let pic = &*(pic_ptr as *const JitPICSlot);
        if let Some((entry, needs_context)) = pic.lookup_megamorphic(receiver_cid) {
            // Round 11 wave 18 (lane hashmap): a hashed way keeps the body that
            // was current when it was installed, and nothing re-points it when
            // a later publication of the same method SUPERSEDES that body
            // (`JitCache::put` marks it `superseded` and touches no inline
            // cache). So a callee first compiled badly -- a deferred `new`
            // resolved per execution, a C1 body before its C2 -- stayed the
            // body this site called for the life of the caller. The owner this
            // arm pins for the call anyway (it is the pin
            // `try_call_compiled_entry_reentrant` takes) answers the question
            // for free: a superseded or retired body is not called, the call
            // takes the resolving miss arm below, which finds the current body,
            // retires the stale way and installs the new one (or, with no free
            // way, hands it to the per-thread entry cache the hoisted consult
            // above serves).
            let pinned = if entry != 0 {
                cratonvm_jit::pin_jit_code_range_owner(entry as usize)
            } else {
                None
            };
            let stale = pinned.as_deref().is_some_and(|cm| {
                cm.superseded.load(std::sync::atomic::Ordering::Acquire)
                    || cm.retired.load(std::sync::atomic::Ordering::Acquire)
            });
            if entry != 0 && !stale {
                disp_census::note(disp_census::MIC_PIC);
                mic_prof::bump(&mic_prof::MIC_HIT_ENTRY);
                // A table callee's sentinel can RE-RUN it at its handler with
                // these arguments, so root them across the call and its
                // servicing when this site ever published one (the site flag
                // covers the PIC too). See `try_mic_rust_cached_entry`.
                // gcg g6/jm: not for a body that owns its arguments.
                let owns = pinned
                    .as_deref()
                    .is_some_and(cratonvm_jit::callee_body_owns_its_arguments);
                let pins = (args_forwarded
                    || door_pins_call_args(
                        mic.callee_rerun_pins(),
                        owns,
                        door_owned_args_unpinned_enabled(vm),
                    ))
                .then(|| CompileArgPins::pin_raw(vm, thread, info, args_slice));
                // `try_call_compiled_entry_reentrant`, with the pin taken
                // above instead of a second registry round trip.
                note_cached_entry_arm(&CACHED_ENTRY_REGISTRY_PINS);
                let called = call_compiled_entry_under_owner(
                    pinned.as_deref(),
                    entry as usize,
                    needs_context,
                    vm_ptr,
                    args_slice,
                );
                if let Some(result) = called {
                    // A megamorphically-dispatched callee deopts exactly like a
                    // monomorphically-dispatched one, and this arm used to
                    // return its `i64::MIN` straight to the compiled caller —
                    // which reads it as its OWN deopt, so the callee's stashed
                    // frame surfaces at an unrelated sink that de-speculates
                    // the wrong method and cannot resume it. See
                    // `handle_compiled_callee_deopt_sentinel`.
                    let result = if result == i64::MIN {
                        handle_rust_armed_callee_sentinel(
                            vm,
                            thread,
                            info,
                            ClassId::new(receiver_cid),
                            args_slice,
                            pins.as_ref(),
                        )
                        .unwrap_or(result)
                    } else {
                        result
                    };
                    if let Some(pins) = pins {
                        pins.release(thread);
                    }
                    return result;
                }
                if let Some(pins) = pins {
                    pins.release(thread);
                }
            }
        }
        // Round 12 wave 2 (lane calls, W2-1): the site's own caches missed;
        // ask the VM's shared megamorphic table, which every other site of
        // this method fills, before the full resolution below.
        note_unbound_selector_table_held(vm, info, pic, receiver_cid);
        if let Some(rc) = try_mega_dispatch_table_entry(
            vm,
            thread,
            info,
            pic,
            receiver_class_id,
            vm_ptr,
            args_slice,
            epoch_now,
        ) {
            disp_census::note(disp_census::MIC_MEGA_TABLE);
            return rc;
        }
    }

    // --- Monomorphic Inline Cache: fast path ---
    // If the receiver ClassId matches the cached value AND we have a cached
    // entry pointer, dispatch directly without any class_manager lookup or
    // method resolution.  This is the zero-overhead dispatch path.
    // `is_empty_class_id` (round 9 wave 2): a fresh slot holds
    // `JitMICSlot::EMPTY_CLASS_ID`, a zeroed one `0`; neither is a cached class.
    if cached_cid == receiver_cid
        && !JitMICSlot::is_empty_class_id(cached_cid)
        && receiver_is_plain_object
    {
        mic.note_dispatch_hit();

        // Try the cached compiled entry pointer (true inline cache hit). The
        // entry and its ABI flag are decoded from ONE load of the tagged word,
        // exactly as generated code does.
        let (entry, needs_ctx) = mic.cached_entry();
        if direct_virtual_compiled_callee_entry_enabled() && entry != 0 && !compiled_entry_forbidden
        {
            // Direct call to the compiled callee — same ABI as `jit_invoke_dispatch`
            // uses after a JIT-cache hit (receiver + params in `args_slice`, optional
            // leading `vm_ptr` when the entry word's context tag is set).  **Do not** pass
            // `(vm_ptr, info_ptr, args_ptr, num_args)` here; that was a mis-invocation
            // that corrupts the stack and surfaces as Windows AV / Linux SIGSEGV.
            // Register-table call straight off the raw arg slots — no `Value`
            // decode on this hot path. `try_call_compiled_entry` returns
            // `None` exactly when the callee has more args than the transmute
            // tables cover (16 ABI words, 8 under its kill switch); only then decode the args
            // and route through the interpreter instead of silently returning
            // 0 from a truncated register-arg table.
            mic_prof::bump(&mic_prof::MIC_HIT_ENTRY);
            // As the megamorphic arm above: root the arguments across a call
            // whose sentinel may re-run a table callee with them. gcg g6/jm:
            // not for a body that owns its arguments (asked only when the
            // site flag would pin: one registry lookup on that path).
            let pins = (args_forwarded
                || (mic.callee_rerun_pins()
                    && door_pins_call_args(
                        true,
                        cratonvm_jit::direct_callee_owns_its_arguments(entry as usize),
                        door_owned_args_unpinned_enabled(vm),
                    )))
            .then(|| CompileArgPins::pin_raw(vm, thread, info, args_slice));
            let rc_opt = {
                let _g = mic_prof::CycGuard::new(&mic_prof::CYC_HIT_ENTRY_CALL);
                try_call_compiled_entry_reentrant(entry as usize, needs_ctx, vm_ptr, args_slice)
            };
            if let Some(rc) = rc_opt {
                let rc = if rc == i64::MIN {
                    handle_rust_armed_callee_sentinel(
                        vm,
                        thread,
                        info,
                        receiver_class_id,
                        args_slice,
                        pins.as_ref(),
                    )
                    .unwrap_or(rc)
                } else {
                    rc
                };
                if let Some(pins) = pins {
                    pins.release(thread);
                }
                return rc;
            }
            if let Some(pins) = pins {
                pins.release(thread);
            }
            let values = decode_values();
            return bail_to_interpreter(vm, thread, info, &values);
        }

        mic_prof::bump(&mic_prof::MIC_HIT_NOENTRY);
        // gci i12/mf: a (site, receiver class) whose slow call already ended
        // in a table-bound JNI native goes straight to the door, skipping the
        // resolution, the compile probe and the by-name tail below
        // (`CRATONVM_JNI_DISPATCH_MEMO`). One byte compare when it is off.
        if let Some(hit) = jit_jni_vsite_lookup(
            vm,
            thread,
            info,
            info_ptr as usize,
            receiver_dispatch.class_id(),
        ) {
            let mut values = decode_values();
            return jit_jni_vsite_call(
                vm,
                thread,
                info,
                &hit,
                &mut values[..],
                &hit.dispatch_class,
            );
        }
        // Entry not cached yet — use cached class name for fast dispatch.
        //
        // KC26 array.clone() bug: array receivers store the COMPONENT class
        // id in their header (per the documented invariant in
        // `runtime/interpreter.rs`). Falling through to
        // `get_class(receiver_class_id).name` would resolve dispatch on the
        // component (e.g. `OptionCategory`/`Enum`) and surface
        // `Enum.clone() → CloneNotSupportedException` for every array clone
        // of an enum type. Per JVMS §4.4.1, array classes inherit their
        // method table from `Object`; short-circuit accordingly.
        let dispatch_target = virtual_dispatch_target_cached(
            vm,
            receiver_ref,
            info,
            jit_site_key(vm.vm_identity, info_ptr as usize),
        );
        if dispatch_target.selection_error {
            if let Some(error) = jit_selection_failure(vm, thread, receiver_ref, info) {
                return handle_jit_dispatch_error(vm, thread, error, info);
            }
        }
        // Round 17 wave 2 (lane mega2, MEGA17-3, opt-in
        // `CRATONVM_JIT_MEGA_CACHE_RETARGETED`): a receiver whose §5.4.6
        // selection moved to a superclass publishes like a receiver-named one,
        // with its body compiled for the SELECTED class (`compile_cid`). Every
        // cache below is keyed by the receiver's class id at this site (or its
        // selector), and selection is a function of exactly those, so the entry
        // can never answer for a receiver whose selection differs. See
        // `CachedDispatchTarget::selected_cid`.
        // gci i12/mf: a selection error never fills a compiled JNI site row.
        let jni_vsite_fill_ok = !dispatch_target.selection_error;
        let cacheable_receiver = dispatch_target.publishable();
        let compile_cid = dispatch_target.compile_cid(receiver_cid);
        let globally_named = dispatch_target.globally_named;
        // Round 15 wave 2 (lane mega, MG7-1): the resolution passed; bind the
        // site's shared-table selector now (switch off: nothing happens).
        if cacheable_receiver && pic_ptr != 0 && !compiled_entry_forbidden {
            bind_mega_selector_at_resolution(
                vm,
                info,
                &*(pic_ptr as *const JitPICSlot),
                receiver_cid,
            );
        }
        // An entryless slot can be retargeted by another thread after the
        // class-id probe above. Its cached class name is therefore not a
        // coherent companion to this receiver; resolve from the receiver
        // itself until there is a callable entry to use.
        let class_name = dispatch_target.class_name;

        // See `try_mic_rust_cached_entry`. Its key carries the receiver's own
        // class id and every entry in it was compiled for that id, so
        // `globally_named` is not a condition here (as at the hoisted consult).
        if cacheable_receiver && !compiled_entry_forbidden {
            if let Some(rc) = try_mic_rust_cached_entry(
                vm,
                thread,
                info,
                info_ptr,
                receiver_cid,
                vm_ptr,
                args_slice,
                args_forwarded,
            ) {
                return rc;
            }
        }
        if cv_trace {
            eprintln!(
                "[cv-mic-hit-noentry] resolved_class={} recv_cid={} cacheable={}",
                class_name, receiver_cid, cacheable_receiver
            );
        }

        // `decode_values` yields exactly `[receiver, args...]` — the full
        // argument vector `invoke_or_native` expects. (This path previously
        // rebuilt it twice via `values[1..].to_vec()` + a fresh prepend.)
        // Mutable: the compile probe below re-reads its objects from pins.
        let mut full_args = decode_values();

        // Try to compile callee for next time (populate the MIC's tagged entry word).
        //
        // VIRTUAL DISPATCH FIX: resolve the callee from the RECEIVER's class
        // (`class_name`, derived from `receiver_class_id` above), NOT from
        // `info.class_name` (the *static* call-site type). When the receiver
        // overrides a concrete superclass method — e.g. a `Long` reached
        // through an `Object`-typed `equals(Object)Z` call site — the static
        // type resolves to `Object.equals` (identity `==`) instead of
        // `Long.equals` (value comparison). The MIC then cached that identity
        // entry against the receiver's class id and invoked it on every
        // monomorphic hit, so two distinct but equal-valued boxes compared
        // unequal (bc-java InterleaveTest, junit assertEquals(Object,Object)).
        // `find_method_recursive` (inside `try_jit_compile_callee`) walks up
        // from the receiver class to the real override.
        // LOADER IDENTITY: the by-name door (`try_jit_compile_callee`) resolves
        // the callee BY NAME. When the receiver's class is not the class that name globally
        // resolves to, that hands back ANOTHER loader's copy of the method, and
        // the entry is then cached against THIS receiver's class id — so every
        // monomorphic hit machine-CALLs a body compiled for a different copy.
        //
        // Measured 2026-08-11 on ApplicationContextAotGeneratorTests: each
        // `@CompileWithForkedClassLoader` test defines its own
        // `DynamicJavaFileManager`, so one run showed that name resolving to
        // eight-plus distinct class ids (2690, 8510, 10377, 14030, 15877,
        // 17724, 19569, 21414, ...). The copy that got compiled has its
        // `instanceof DynamicClassFileObject` site interned against ITS OWN
        // `DynamicClassFileObject` id, so the check correctly answered false for
        // the receiver's file object, `super.inferBinaryName` ran, and
        // `JavacFileManager` threw on a file object it did not create.
        //
        // That was answered first by refusing any receiver whose name is not
        // globally unique (`globally_named`). Since interpreter round i1 wave 7
        // the compile is asked for the receiver's OWN class id
        // (`try_jit_compile_callee_for_class`), which resolves the method from
        // that class and probes/publishes under its id, so every such receiver
        // gets its own copy's body and the gate is gone.
        // Has the compiler already declined this callee for this receiver? See
        // `MIC_COMPILE_DECLINED` — the probe below is the single most expensive
        // thing on this arm and it answered `None` on every one of 6.59 M calls
        // in the measured run.
        let declined_key = (
            jit_site_key(vm.vm_identity, info_ptr as usize),
            receiver_cid,
        );
        let skip_probe =
            MIC_COMPILE_DECLINED.with(|m| match m.borrow_mut().get_mut(&declined_key) {
                Some(n) => {
                    *n = n.wrapping_add(1);
                    *n % MIC_COMPILE_PROBE_RETRY != 0
                }
                None => false,
            });
        if skip_probe {
            note_cached_entry_arm(&MIC_COMPILE_PROBE_MEMO_SKIPS);
        }
        let compile_res = if skip_probe
            || !direct_virtual_compiled_callee_entry_enabled()
            || compiled_entry_forbidden
            || !cacheable_receiver
        {
            // Attribute the skip. All of these land in `pub_probe_none`
            // below without a probe ever running, which is why that counter
            // reading equal to `hit_noentry` could not distinguish "the compiler
            // refused every callee" from "we never asked".
            if skip_probe {
                // Already attributed above, and deliberately NOT folded into any
                // `not_probed_*` counter: those name gates, this names a memo.
            } else if !direct_virtual_compiled_callee_entry_enabled() {
                mic_prof::bump(&mic_prof::NOT_PROBED_DISABLED);
            } else if compiled_entry_forbidden {
                // A redefinition, or interpreter-only mode.
                mic_prof::bump(&mic_prof::NOT_PROBED_REDEFINE);
            } else {
                mic_prof::bump(&mic_prof::NOT_PROBED_UNCACHEABLE);
            }
            None
        } else {
            let _g = mic_prof::CycGuard::new(&mic_prof::CYC_COMPILE_PROBE);
            // The compile can run `<clinit>` and collect: root the decoded
            // objects across it and re-read them, so the interpreter route
            // below does not run on pre-move addresses. See [`CompileArgPins`].
            let pins = CompileArgPins::pin_values(vm, thread, &full_args);
            let probed = crate::runtime::interpreter::try_jit_compile_callee_for_class(
                vm,
                // The receiver's own class, or the §5.4.6-selected one (MEGA17-3).
                ClassId::new(compile_cid),
                &class_name,
                info.method_name,
                info.descriptor,
                // JIT-dispatch callee compile — optimized (C2-equivalent) tier.
                true,
            );
            pins.release_values(thread, &mut full_args);
            if probed.is_none() {
                // A probe ACTUALLY ran and the compiler declined. This is the
                // only one of the four `pub_probe_none` causes that names a
                // compile refusal, and `CRATONVM_DBG=callee-probe` says which.
                mic_prof::bump(&mic_prof::PROBE_RETURNED_NONE);
                MIC_COMPILE_DECLINED.with(|m| {
                    m.borrow_mut().entry(declined_key).or_insert(0);
                });
            } else {
                // It compiles now — drop the refusal so a later eviction of the
                // published entry re-probes immediately instead of waiting out
                // the retry interval.
                MIC_COMPILE_DECLINED.with(|m| {
                    m.borrow_mut().remove(&declined_key);
                });
            }
            probed
        };
        // `full_args[0]` is the receiver, re-read from its pin if the probe ran.
        let receiver_ref = match full_args.first() {
            Some(Value::Object(Some(receiver))) => *receiver,
            _ => receiver_ref,
        };
        if cv_trace {
            eprintln!(
                "[cv-mic-compile] site={}.{}{} recv_cid={} resolved_class={} cacheable={} globally_named={} compiled={}",
                info.class_name,
                info.method_name,
                info.descriptor,
                receiver_cid,
                class_name,
                cacheable_receiver,
                globally_named,
                compile_res.is_some(),
            );
        }
        // Keep handler-bearing methods on the helper path. A raw compiled
        // entry can leave a pending exceptional frame that the caller cannot
        // safely resume while the HTTP request is still active.
        if compile_res.is_none() {
            mic_prof::bump(&mic_prof::PUB_PROBE_NONE);
        }
        // Read before the destructuring `if let` below partially moves it.
        let callee_has_compiled_code = compile_res.is_some();
        if let Some((_callee_pin, entry_ptr, needs_ctx)) = compile_res {
            // `_callee_pin` holds the callee artifact across the publications
            // below: `update`/`install` take their own keep-alive by resolving
            // the entry, and that resolution can only succeed while the
            // artifact is alive.
            // jit-invokedynamic-groovy-regression fix: also never publish an
            // artifact containing an unconditional invokedynamic trap — the
            // inline MIC/PIC cascade would machine-CALL it, letting the trap's
            // sentinel + stashed frame bail through the compiled caller's
            // epilogue past the only point able to resume it precisely.
            let callee_barred_by_table = !mic_publish_exception_table_callees()
                && mic_callee_has_exception_table(vm, ClassId::new(receiver_cid), info);
            // Read off the artifact being published, not re-probed by name: the
            // name-keyed probe reads another loader's row when the receiver's
            // class name is not globally unique.
            let callee_has_indy_trap = _callee_pin.has_indy_trap;
            if callee_barred_by_table && !callee_has_indy_trap && cacheable_receiver {
                publish_mic_rust_cached_entry(
                    vm.vm_identity,
                    info_ptr,
                    receiver_cid,
                    entry_ptr,
                    needs_ctx,
                    true,
                );
            }
            if callee_barred_by_table || callee_has_indy_trap {
                mic_prof::bump(&mic_prof::PUB_BARRED);
            }
            if !callee_barred_by_table && !callee_has_indy_trap {
                mic_prof::bump(&mic_prof::PUB_PUBLISHED);
                mark_site_rerun_pins_before_publish(vm, mic, receiver_cid, info, entry_ptr != 0);
                // Publish through `update`, never with a raw store: `update` is
                // the only writer that also resolves and RETAINS the callee's
                // `Arc<CompiledMethod>` in the slot's `compiled_owner`.
                //
                // Until 2026-07-27 this branch stored the entry word
                // directly, so a slot reached through the "class cached, target
                // unresolved" shape (what `prepopulate` seeds and what
                // `clear_compiled_entry` leaves behind after every
                // invalidation) ended up holding a RAW entry pointer with no
                // keep-alive. The next tier-up `put` for that callee replaced
                // its shard snapshot, dropped the last `Arc`, and `munmap`ped
                // the body — while the inline `MOV R11,[mic+8]; CALL R11`
                // cascade emitted by `jit/src/x64.rs` still called it. That is
                // the ElasticSearch `NodeConnectionsServiceTests` SIGSEGV
                // (`rip == r11 ==` first byte of a retired code mapping) and,
                // once the address was recycled by a later allocation, the
                // json-smart "re-parse returned another method's result"
                // corruption.
                // t1 wave 4 (lane Y2): a refusal here reaches this VM's report.
                let _jdk_only_scope = jdk_only_ic_publication_scope(
                    vm,
                    crate::vm::dispatch_policy(vm).is_jdk_only(),
                );
                // MEGA5-18-4: a gate-first megamorphic site never reads its MIC.
                if !mic_publication_skipped(pic_ptr) {
                    mic.update(
                        receiver_cid,
                        &class_name,
                        entry_ptr as u64,
                        needs_ctx,
                        crate::vm::dispatch_policy(vm).is_jdk_only(),
                    );
                }
                // CRIT-1 — also populate the co-allocated PIC so the
                // inline 4-way cascade in `jit/src/x64.rs` hits on the
                // next invocation. Without this the cascade's empty
                // (class_id == 0) slots always fail and every dispatch
                // pays the full helper cost. We only install when we
                // actually have an entry_ptr to publish; a 0 entry_ptr
                // in a PIC slot would force the inline cascade to call
                // through a null function pointer.
                if pic_ptr != 0 && entry_ptr != 0 {
                    let pic = &*(pic_ptr as *const JitPICSlot);
                    // JIT round 21, lane dispatch (`CRATONVM_JIT_IC_MIC_PLUS_FOUR`):
                    // a MIC-first site's MIC already serves this class inline;
                    // its four ways are kept for four other classes.
                    if !pic_way_redundant_with_mic(mic, pic, receiver_cid)
                        && pic.install(
                            receiver_cid,
                            &class_name,
                            entry_ptr as u64,
                            needs_ctx,
                            crate::vm::dispatch_policy(vm).is_jdk_only(),
                        )
                    {
                        disp_census::note(disp_census::MIC_GRACE_LAG_REFUSED);
                        request_ic_grace_if_due(vm, Some(thread.thread_id));
                    }
                    // Round 12 wave 2 (lane calls, W2-1): and the VM's shared
                    // megamorphic table, for every other site of this method.
                    publish_mega_dispatch_entry(
                        vm,
                        info,
                        pic,
                        receiver_cid,
                        entry_ptr as u64,
                        needs_ctx,
                        epoch_now,
                    );
                }
                withdraw_ic_publication_if_redefined(
                    vm,
                    epoch_now,
                    mic,
                    (pic_ptr != 0).then(|| &*(pic_ptr as *const JitPICSlot)),
                );
            }
            // Round 12 wave 1 (lane `calls`, W18-3): as the miss arm below,
            // enter the body the probe just answered.
            if entry_ptr != 0
                && cacheable_receiver
                && !callee_has_indy_trap
                && !compiled_entry_forbidden
            {
                if let Some(rc) = call_body_the_mic_probe_just_found(
                    vm,
                    thread,
                    info,
                    receiver_class_id,
                    entry_ptr,
                    needs_ctx,
                    vm_ptr,
                    &full_args[..],
                    // gch h1/jp: the caller's block, unless the door read a
                    // forwarded Rust copy of it.
                    forwarded_args.is_none().then_some(args_slice),
                    callee_barred_by_table || mic.callee_rerun_pins(),
                    epoch_now,
                ) {
                    disp_census::note(disp_census::MIC_MISS_CALLED_COMPILED);
                    return rc;
                }
            }
        }

        // The uncompiled-callee fast path. Reached only after the MIC entry,
        // the PIC, `VIRTUAL_DISPATCH_CACHE` and the compile probe have all
        // declined, and gated on `compile_res.is_none()` so a callee that DOES
        // have compiled code is never diverted into the interpreter — the
        // by-name `invoke_or_native` below would have entered that code.
        // nio21 w19 L2: the selected method compiled under its DECLARING
        // class's key (an inherited `final` method such as
        // `ReentrantLock$Sync.tryRelease` reached on a `NonfairSync`), which the
        // receiver-keyed probe above cannot see. See
        // `mic_tail_call_selected_declaring_body`.
        if !callee_has_compiled_code && !compiled_entry_forbidden && receiver_is_plain_object {
            if let Some(rc) = mic_tail_call_selected_declaring_body(
                vm,
                thread,
                info,
                info_ptr,
                receiver_class_id,
                vm_ptr,
                &full_args[..],
                forwarded_args.is_none().then_some(args_slice),
                mic.callee_rerun_pins(),
                epoch_now,
                Some((mic_ptr, pic_ptr, &*class_name)),
            ) {
                disp_census::note(disp_census::MIC_MISS_CALLED_COMPILED);
                return rc;
            }
        }
        if !callee_has_compiled_code {
            if let Some(r) = try_jit_virtual_bytecode_callee(
                vm,
                thread,
                info,
                jit_site_key(vm.vm_identity, info_ptr as usize),
                receiver_ref,
                receiver_class_id,
                &full_args,
                false,
            ) {
                return match r {
                    Ok(Some(Value::Int(v))) => v as i64,
                    Ok(Some(Value::Long(v))) => v,
                    Ok(Some(Value::Float(f))) => f.to_bits() as i64,
                    Ok(Some(Value::Double(d))) => d.to_bits() as i64,
                    Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
                    Ok(Some(Value::Object(None)) | None) => 0,
                    Ok(_) => 0,
                    Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
                };
            }
        }
        // gci i12/mf: the compiled virtual call-site rows' fill probe brackets
        // the by-name tail (armed only with the memo on).
        let jni_vsite_probe = crate::vm::vm_exec::jni_jit_vsite_probe_arm(
            vm,
            thread,
            info.method_name,
            info.descriptor,
        );
        let invoke_res = {
            let _g = mic_prof::CycGuard::new(&mic_prof::CYC_INVOKE);
            crate::vm::dbg_dispatch_tally(
                "jit_mic_tail",
                &class_name,
                info.method_name,
                info.descriptor,
            );
            // gch h1/jp (fr review item 3): the S111r12 rescue below resends
            // `full_args`; a failed attempt re-reads it from pins.
            crate::vm::invoke_or_native_keeping_args_current(
                vm,
                thread,
                &class_name,
                info.method_name,
                info.descriptor,
                &mut full_args[..],
                !info.class_name.is_empty() && &*class_name != info.class_name,
            )
        };
        if let Some(save) = jni_vsite_probe {
            crate::vm::vm_exec::jni_jit_vsite_probe_finish(
                vm,
                thread,
                save,
                ClassId::new(info.declaring_class_id),
                crate::vm::vm_exec::jni_jit_vsite(info_ptr as usize),
                receiver_class_id,
                &class_name,
                info.class_name,
                info.method_name,
                info.descriptor,
                invoke_res.is_ok() && jni_vsite_fill_ok && receiver_is_plain_object,
            );
        }
        // S111r12 — JIT MIC fast-path rescue: same CP-class fallback
        // as the cache-miss branch below (see comment there).
        //
        // Round-fix (Jetty): a thrown exception from the MIC dispatch must
        // be routed through `handle_jit_dispatch_error` (stash + return the
        // `i64::MIN` deopt sentinel) — the old code merely logged it and
        // returned 0, silently swallowing the exception and letting the JIT
        // caller run on with a bogus value.
        let result = match invoke_res {
            Ok(v) => v,
            Err(e)
                if !info.class_name.is_empty()
                    && &*class_name != info.class_name
                    && is_dispatch_no_such_method_miss(
                        &e,
                        &class_name,
                        info.method_name,
                        info.descriptor,
                    ) =>
            {
                match crate::vm::invoke_or_native(
                    vm,
                    thread,
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    &full_args,
                ) {
                    Ok(v) => v,
                    Err(e2) => {
                        return handle_jit_dispatch_error(vm, thread, e2, info);
                    }
                }
            }
            Err(e) => {
                return handle_jit_dispatch_error(vm, thread, e, info);
            }
        };

        if cv_trace {
            eprintln!(
                "[cv-mic-hit-result] is_null={}",
                matches!(result, Some(Value::Object(None)) | None)
            );
        }
        return match result {
            Some(Value::Int(v)) => v as i64,
            Some(Value::Long(v)) => v,
            Some(Value::Float(f)) => f.to_bits() as i64,
            Some(Value::Double(d)) => d.to_bits() as i64,
            Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
            Some(Value::Object(None)) | None => 0,
            _ => 0,
        };
    }

    // --- Cache miss: full resolution + update cache ---
    mic.note_dispatch_miss();

    // gci i12/mf: as the entryless-hit arm above, for a receiver class the
    // monomorphic slot does not hold.
    if let Some(hit) = jit_jni_vsite_lookup(
        vm,
        thread,
        info,
        info_ptr as usize,
        receiver_dispatch.class_id(),
    ) {
        let mut values = decode_values();
        return jit_jni_vsite_call(vm, thread, info, &hit, &mut values[..], &hit.dispatch_class);
    }

    let dispatch_target = virtual_dispatch_target_cached(
        vm,
        receiver_ref,
        info,
        jit_site_key(vm.vm_identity, info_ptr as usize),
    );
    if dispatch_target.selection_error {
        if let Some(error) = jit_selection_failure(vm, thread, receiver_ref, info) {
            return handle_jit_dispatch_error(vm, thread, error, info);
        }
    }
    // Round 17 wave 2 (lane mega2, MEGA17-3): as in the entryless-hit arm above.
    // gci i12/mf: as in the entryless-hit arm above.
    let jni_vsite_fill_ok = !dispatch_target.selection_error;
    let cacheable_receiver = dispatch_target.publishable();
    let compile_cid = dispatch_target.compile_cid(receiver_cid);
    let class_name = dispatch_target.class_name;
    // Round 15 wave 2 (lane mega, MG7-1): as in the entryless-hit arm above.
    if cacheable_receiver && receiver_is_plain_object && pic_ptr != 0 && !compiled_entry_forbidden {
        bind_mega_selector_at_resolution(vm, info, &*(pic_ptr as *const JitPICSlot), receiver_cid);
    }
    if cv_trace {
        eprintln!(
            "[cv-mic-miss] resolved_class={} recv_cid={} cacheable={}",
            class_name, receiver_cid, cacheable_receiver
        );
    }

    // The `VIRTUAL_DISPATCH_CACHE` consult that used to be here has moved ABOVE
    // `record_miss` and `virtual_dispatch_target_cached` — see the note at the
    // `cached_cid` load. It is not merely reordered: the entries this cache can
    // hold are published only for a `cacheable_receiver` plain-object
    // receiver, which is a subset of the condition the hoisted consult tests,
    // so nothing that could hit here fails to hit there.
    mic_prof::bump(&mic_prof::MIC_MISS);
    // Try to compile callee for cached entry. Resolve by the RECEIVER's class
    // (`class_name`), not the static `info.class_name` — see the matching
    // VIRTUAL DISPATCH FIX in the cache-hit branch above. `class_name` here is
    // an `Arc<str>`; deref to `&str` for the resolver.
    // LOADER IDENTITY: the by-name door (`try_jit_compile_callee`) resolves the
    // callee BY NAME. When the receiver's class is not the class that name
    // globally resolves to, that hands back ANOTHER loader's copy of the
    // method, and the entry is then cached against THIS receiver's class id —
    // so every monomorphic hit machine-CALLs a body compiled for a different
    // copy.
    //
    // Measured 2026-08-11 on ApplicationContextAotGeneratorTests: each
    // `@CompileWithForkedClassLoader` test defines its own
    // `DynamicJavaFileManager`, so one run showed that name resolving to
    // eight-plus distinct class ids (2690, 8510, 10377, 14030, 15877,
    // 17724, 19569, 21414, ...). The copy that got compiled has its
    // `instanceof DynamicClassFileObject` site interned against ITS OWN
    // `DynamicClassFileObject` id, so the check correctly answered false for
    // the receiver's file object, `super.inferBinaryName` ran, and
    // `JavacFileManager` threw on a file object it did not create.
    //
    // The compile is asked for the receiver's OWN class id, so it resolves the
    // method from that class and probes/publishes under its id; see the
    // matching note in the cache-hit branch (the former `globally_named` gate).
    //
    // Decoded BEFORE the probe, not after it: the probe can run `<clinit>` and
    // collect, and a decode afterwards would validate and pass on the pre-move
    // words of the staging block, which is in no oop map. The decoded objects
    // are pinned across the probe instead. See [`CompileArgPins`].
    // `decode_values` yields exactly the `[receiver, args...]` vector
    // `invoke_or_native` expects.
    let mut full_args = decode_values();
    // Round 11 wave 18 (lane hashmap): the same `MIC_COMPILE_DECLINED` throttle
    // the entryless-hit arm above applies. This arm is where EVERY call lands
    // for a receiver class other than the one the MIC was populated with (the
    // slot is monomorphic for its lifetime, `JitMICSlot::update`), so a callee
    // the compiler declined for that class re-ran the probe, its argument pins
    // and the by-name indy probe below on every call, for the life of the
    // site. Keyed and flushed exactly as there: one retry per
    // `MIC_COMPILE_PROBE_RETRY` calls, and every generation / class-identity
    // flush drops the memo with the other site-keyed memos.
    let miss_declined_key = (
        jit_site_key(vm.vm_identity, info_ptr as usize),
        receiver_cid,
    );
    let miss_skip_probe =
        MIC_COMPILE_DECLINED.with(|m| match m.borrow_mut().get_mut(&miss_declined_key) {
            Some(n) => {
                *n = n.wrapping_add(1);
                *n % MIC_COMPILE_PROBE_RETRY != 0
            }
            None => false,
        });
    if miss_skip_probe {
        note_cached_entry_arm(&MIC_COMPILE_PROBE_MEMO_SKIPS);
    }
    let compile_res = if miss_skip_probe
        || !direct_virtual_compiled_callee_entry_enabled()
        || compiled_entry_forbidden
        || !cacheable_receiver
    {
        None
    } else {
        let _g = mic_prof::CycGuard::new(&mic_prof::CYC_COMPILE_PROBE);
        let pins = CompileArgPins::pin_values(vm, thread, &full_args);
        let probed = crate::runtime::interpreter::try_jit_compile_callee_for_class(
            vm,
            // The receiver's own class, or the §5.4.6-selected one (MEGA17-3).
            ClassId::new(compile_cid),
            &class_name,
            info.method_name,
            info.descriptor,
            // JIT-dispatch callee compile — optimized (C2-equivalent) tier.
            true,
        );
        pins.release_values(thread, &mut full_args);
        // JIT r20 w19 yLadder (DISP20-7): an inherited method's body published
        // under its DECLARING class, which the receiver-keyed probe cannot see;
        // published and called below exactly as a probe answer.
        let probed = probed.or_else(|| probe_selected_declaring_body(vm, info, receiver_class_id));
        // As the entryless-hit arm: record a refusal, drop one that no longer
        // holds.
        if probed.is_none() {
            MIC_COMPILE_DECLINED.with(|m| {
                m.borrow_mut().entry(miss_declined_key).or_insert(0);
            });
        } else {
            MIC_COMPILE_DECLINED.with(|m| {
                m.borrow_mut().remove(&miss_declined_key);
            });
        }
        probed
    };
    // `full_args[0]` is the receiver, re-read from its pin if the probe ran.
    let receiver_ref = match full_args.first() {
        Some(Value::Object(Some(receiver))) => *receiver,
        _ => receiver_ref,
    };
    // `_callee_pin` must outlive the `mic.update` / `pic.install` below — see
    // the matching note in the cache-hit branch.
    let (_callee_pin, entry_ptr, needs_ctx) = match compile_res {
        Some((pin, ptr, nc)) => (Some(pin), ptr as u64, nc),
        None => (None, 0, false),
    };

    // A machine-code MIC/PIC call has no interpreter boundary at which the
    // callee's local handler can be resumed. Leave such callees on the checked
    // helper path; ordinary handler-free callees retain the raw-entry fast path.
    let callee_barred_by_table = !mic_publish_exception_table_callees()
        && mic_callee_has_exception_table(vm, ClassId::new(receiver_cid), info);
    // jit-invokedynamic-groovy-regression fix — see the matching gate in the
    // cache-hit branch above: an indy-trap-bearing artifact must stay on the
    // dispatch helper, never in a machine-called MIC/PIC entry. Read off the
    // artifact being published when there is one (the name-keyed probe reads
    // another loader's row for a class name that is not globally unique).
    //
    // Round 11 wave 18 (lane hashmap): with no body the answer can reach only
    // the class-only `mic.update` below (entry 0: `publish_mic_rust_cached_entry`
    // and `pic.install` both do nothing for it), and that update changes
    // nothing unless the slot is still empty -- a slot holding another class
    // is monomorphic for its lifetime and `update` returns at once. So the
    // by-name probe (a class-manager read lock, a JIT-cache read lock and a
    // string-keyed lookup) is asked only while the slot is empty, instead of on
    // every call from every receiver class the slot does not hold.
    let callee_has_indy_trap = match _callee_pin.as_ref() {
        Some(pin) => pin.has_indy_trap,
        None if !JitMICSlot::is_empty_class_id(
            mic.cached_class_id
                .load(std::sync::atomic::Ordering::Acquire),
        ) =>
        {
            false
        }
        None => compiled_entry_has_indy_trap(vm, &class_name, info.method_name, info.descriptor),
    };
    if callee_barred_by_table && !callee_has_indy_trap && cacheable_receiver {
        publish_mic_rust_cached_entry(
            vm.vm_identity,
            info_ptr,
            receiver_cid,
            entry_ptr as usize,
            needs_ctx,
            true,
        );
    }
    if cacheable_receiver && !callee_barred_by_table && !callee_has_indy_trap {
        mark_site_rerun_pins_before_publish(vm, mic, receiver_cid, info, entry_ptr != 0);
        // t1 wave 4 (lane Y2): a refusal here reaches this VM's report.
        let _jdk_only_scope =
            jdk_only_ic_publication_scope(vm, crate::vm::dispatch_policy(vm).is_jdk_only());
        // Update all MIC fields atomically (needs_ctx must match compiled entry ABI)
        // MEGA5-18-4: a gate-first megamorphic site never reads its MIC.
        if !mic_publication_skipped(pic_ptr) {
            mic.update(
                receiver_cid,
                &class_name,
                entry_ptr,
                needs_ctx,
                crate::vm::dispatch_policy(vm).is_jdk_only(),
            );
        }

        // CRIT-1 — Populate the co-allocated PIC so the inline 4-way
        // cascade emitted in `jit/src/x64.rs` actually hits on subsequent
        // dispatches. Eager allocation made `pic_inline` always-true at
        // codegen, so the cascade is always emitted but stays cold until
        // the helper publishes entries here. Mirror the MIC update with
        // a `pic.install(...)` so the next call with the same receiver
        // class takes the inline fast path (5 cycles slot-0 hit vs the
        // full helper call). LFU eviction inside `install` handles
        // megamorphic spillover automatically.
        if pic_ptr != 0 && entry_ptr != 0 {
            let pic = &*(pic_ptr as *const JitPICSlot);
            // JIT round 21, lane dispatch (`CRATONVM_JIT_IC_MIC_PLUS_FOUR`): as
            // the probe-hit publication above.
            if !pic_way_redundant_with_mic(mic, pic, receiver_cid)
                && pic.install(
                    receiver_cid,
                    &class_name,
                    entry_ptr,
                    needs_ctx,
                    crate::vm::dispatch_policy(vm).is_jdk_only(),
                )
            {
                disp_census::note(disp_census::MIC_GRACE_LAG_REFUSED);
                request_ic_grace_if_due(vm, Some(thread.thread_id));
            }
            // Round 12 wave 2 (lane calls, W2-1): and the VM's shared
            // megamorphic table, for every other site of this method.
            publish_mega_dispatch_entry(
                vm,
                info,
                pic,
                receiver_cid,
                entry_ptr,
                needs_ctx,
                epoch_now,
            );
        }
        // Round 11 wave 18 (lane hashmap): a receiver class that got a body but
        // that NEITHER cache this helper consults can now serve. The MIC holds
        // another class for its lifetime, and the PIC's hashed table (the only
        // part of the PIC this helper reads; its four inline ways are read by
        // machine code only, and not at all at a single-pass site too wide for
        // the register file) had no free way, or there is no PIC. Every later
        // call from this class used to land back here and pay the full
        // resolution: argument decode, compile probe (a JIT-cache hit under
        // two read locks) and a by-name `invoke_or_native`, per call, while a
        // compiled body sat in the JIT cache. It now goes to the per-thread
        // entry cache the hoisted `try_mic_rust_cached_entry` consult reads,
        // under the same conditions the table-barred publication above meets:
        // a receiver-derived target compiled for this receiver class id.
        // `rerun_pins` is the site's sticky flag, set before any table callee
        // is published here (`mark_site_rerun_pins_before_publish` above).
        if entry_ptr != 0
            && vm.jit.jit_cache.redefine_epoch() == epoch_now
            && mic
                .cached_class_id
                .load(std::sync::atomic::Ordering::Acquire)
                != receiver_cid
            && (pic_ptr == 0
                || (*(pic_ptr as *const JitPICSlot))
                    .lookup_megamorphic(receiver_cid)
                    .is_none())
        {
            publish_mic_rust_cached_entry(
                vm.vm_identity,
                info_ptr,
                receiver_cid,
                entry_ptr as usize,
                needs_ctx,
                mic.callee_rerun_pins(),
            );
        }
        withdraw_ic_publication_if_redefined(
            vm,
            epoch_now,
            mic,
            (pic_ptr != 0).then(|| &*(pic_ptr as *const JitPICSlot)),
        );
    }

    // Round 12 wave 1 (lane `calls`, W18-3): the probe just answered a body
    // for this receiver's class; enter it rather than re-resolving by name.
    if entry_ptr != 0
        && cacheable_receiver
        && receiver_is_plain_object
        && !callee_has_indy_trap
        && !compiled_entry_forbidden
    {
        if let Some(rc) = call_body_the_mic_probe_just_found(
            vm,
            thread,
            info,
            receiver_class_id,
            entry_ptr as usize,
            needs_ctx,
            vm_ptr,
            &full_args[..],
            // gch h1/jp: as the entryless-hit arm.
            forwarded_args.is_none().then_some(args_slice),
            callee_barred_by_table || mic.callee_rerun_pins(),
            epoch_now,
        ) {
            disp_census::note(disp_census::MIC_MISS_CALLED_COMPILED);
            return rc;
        }
    }

    // `full_args` was decoded above, ahead of the compile probe.

    // The uncompiled-callee fast path — same gate and same reasoning as the
    // cache-hit branch above: `entry_ptr == 0` means the compile probe found no
    // compiled body for this callee, so nothing is being diverted away from one.
    if entry_ptr == 0 {
        if let Some(r) = try_jit_virtual_bytecode_callee(
            vm,
            thread,
            info,
            jit_site_key(vm.vm_identity, info_ptr as usize),
            receiver_ref,
            receiver_class_id,
            &full_args,
            false,
        ) {
            return match r {
                Ok(Some(Value::Int(v))) => v as i64,
                Ok(Some(Value::Long(v))) => v,
                Ok(Some(Value::Float(f))) => f.to_bits() as i64,
                Ok(Some(Value::Double(d))) => d.to_bits() as i64,
                Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
                Ok(Some(Value::Object(None)) | None) => 0,
                Ok(_) => 0,
                Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
            };
        }
    }

    // gci i12/mf: as the entryless-hit arm's tail.
    let jni_vsite_probe =
        crate::vm::vm_exec::jni_jit_vsite_probe_arm(vm, thread, info.method_name, info.descriptor);
    let invoke_res = {
        let _g = mic_prof::CycGuard::new(&mic_prof::CYC_INVOKE);
        crate::vm::dbg_dispatch_tally(
            "jit_mic_tail",
            &class_name,
            info.method_name,
            info.descriptor,
        );
        // gch h1/jp (fr review item 3): as the entryless-hit arm's tail.
        crate::vm::invoke_or_native_keeping_args_current(
            vm,
            thread,
            &class_name,
            info.method_name,
            info.descriptor,
            &mut full_args[..],
            !info.class_name.is_empty() && &*class_name != info.class_name,
        )
    };
    if let Some(save) = jni_vsite_probe {
        crate::vm::vm_exec::jni_jit_vsite_probe_finish(
            vm,
            thread,
            save,
            ClassId::new(info.declaring_class_id),
            crate::vm::vm_exec::jni_jit_vsite(info_ptr as usize),
            receiver_class_id,
            &class_name,
            info.class_name,
            info.method_name,
            info.descriptor,
            invoke_res.is_ok() && jni_vsite_fill_ok && receiver_is_plain_object,
        );
    }
    // S111r12 — JIT MIC virtual-dispatch rescue. When the receiver's
    // runtime class (e.g. `java/lang/Comparable` for a malformed
    // ClassLoader instance) does not declare the CP-resolved method,
    // `invoke_or_native` raises `NoSuchMethodError`. The CP method-ref
    // class carried in `info.class_name` (e.g. `java/lang/ClassLoader`)
    // is the spec-correct resolution target — retry through it.
    // Mirrors the S111r10 receiver-walk fallback for invokeinterface
    // and the S111r8 cid=0 → CP-class fallback in `execute_invoke`.
    let result = match invoke_res {
        Ok(v) => v,
        Err(e)
            if !info.class_name.is_empty()
                && &*class_name != info.class_name
                && is_dispatch_no_such_method_miss(
                    &e,
                    &class_name,
                    info.method_name,
                    info.descriptor,
                ) =>
        {
            match crate::vm::invoke_or_native(
                vm,
                thread,
                info.class_name,
                info.method_name,
                info.descriptor,
                &full_args,
            ) {
                Ok(v) => v,
                Err(e2) => {
                    return handle_jit_dispatch_error(vm, thread, e2, info);
                }
            }
        }
        // Round-fix (Jetty): route a thrown exception through
        // `handle_jit_dispatch_error` (stash + return the `i64::MIN` deopt
        // sentinel) rather than logging and returning a bogus 0.
        Err(e) => {
            return handle_jit_dispatch_error(vm, thread, e, info);
        }
    };

    if cv_trace {
        eprintln!(
            "[cv-mic-miss-result] is_null={}",
            matches!(result, Some(Value::Object(None)) | None)
        );
    }
    match result {
        Some(Value::Int(v)) => v as i64,
        Some(Value::Long(v)) => v,
        Some(Value::Float(f)) => f.to_bits() as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Uncommon Trap / Deoptimization
// ---------------------------------------------------------------------------

/// Deopt reason codes passed from JIT-compiled code.
/// These map to `cratonvm_jit::deopt::DeoptReason` variants.
pub const DEOPT_REASON_NULL_CHECK: i64 = 0;

/// Does this `--compatible` VM serve `ThreadLocal.get()` via the side-table
/// `phases_early::native_tl_get`?
#[inline]
pub(in crate::jit::helpers) fn thread_local_compat_fast_serves(vm: &SharedVm) -> bool {
    !crate::vm::dispatch_policy(vm).is_jdk_only()
        && vm
            .natives
            .native_methods
            .resolve_id("java/lang/ThreadLocal", "get", "()Ljava/lang/Object;")
            .and_then(|id| vm.natives.native_methods.callback_of(id))
            .is_some_and(cratonvm_native_builtins::phases_early::is_tl_compat_get_native)
}

fn compat_tl_get_compute_ok(vm: &SharedVm, cid: ClassId) -> u8 {
    let cm = vm.classes.class_manager.read();
    let mut c = cid;
    for _ in 0..32 {
        let Some(class) = cm.class_store.get(c) else {
            return 0;
        };
        let name = &*class.name;
        if name == "java/lang/ThreadLocal" || name == "java/lang/InheritableThreadLocal" {
            return 1;
        }
        if class.find_method("get", "()Ljava/lang/Object;").is_some() {
            return 0;
        }
        match class.superclass {
            Some(parent) => c = parent,
            None => return 0,
        }
    }
    0
}

#[inline]
fn compat_tl_get_receiver_ok(vm: &SharedVm, cid: ClassId) -> bool {
    const COMPAT_TL_MEMO_TAG: u32 = 0x4000_0000;
    let raw = cid.as_u32();
    if raw & 0xC000_0000 != 0 {
        return compat_tl_get_compute_ok(vm, cid) != 0;
    }
    super::jit_scoped_memory_put_int_direct::tl_kind_memo(vm, raw | COMPAT_TL_MEMO_TAG, || {
        compat_tl_get_compute_ok(vm, cid)
    }) != 0
}

/// Fast direct helper for `--compatible` `ThreadLocal.get()` / `InheritableThreadLocal.get()`.
pub(in crate::jit) unsafe extern "C" fn jit_compat_thread_local_get_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_compat_thread_local_get_direct_body(vm_ptr, receiver);
    let bits = contain(
        "jit_compat_thread_local_get_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    native_return_handed_to_compiled_code();
    bits
}

unsafe fn jit_compat_thread_local_get_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    'fast: {
        if (receiver as u64 & 0x7) != 0 || (receiver as u64) >= (1u64 << 48) {
            break 'fast;
        }
        let mru_safe =
            !crate::vm::dispatch_policy(vm).is_jdk_only()
                && !vm.mem.heap.concurrent_relocation_active();
        if mru_safe {
            let epoch = vm.mem.address_epoch();
            if let Some(val_bits) =
                cratonvm_native_builtins::phases_early::tl_compat_mru_lookup(
                    vm.vm_identity,
                    epoch,
                    receiver as usize,
                )
            {
                if val_bits == 1 {
                    return 0;
                }
                if let Some((thread, _guard)) = jit_thread_mut() {
                    let obj = ObjectRef::from_raw(val_bits as *mut u8);
                    thread.native_pending_return = Some(obj);
                    return val_bits as i64;
                }
                break 'fast;
            }
        }
        if !thread_local_compat_fast_serves(vm) {
            break 'fast;
        }
        let Some(this) = vm.mem.heap.is_object_address(receiver as usize) else {
            break 'fast;
        };
        let cid = vm.mem.heap.class_id_of_validated(this);
        if !compat_tl_get_receiver_ok(vm, cid) {
            break 'fast;
        }
        let Some((thread, _guard)) = jit_thread_mut() else {
            break 'fast;
        };
        let thread_ptr = thread as *mut _;
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread_ptr,
        };
        if let Some(Value::Object(obj)) =
            cratonvm_native_builtins::phases_early::tl_compat_get_hit_fast(&mut ctx, this)
        {
            if !vm.mem.heap.concurrent_relocation_active() {
                let post_epoch = vm.mem.address_epoch();
                let val_bits = obj.map_or(1, |o| o.as_ptr() as usize);
                cratonvm_native_builtins::phases_early::tl_compat_mru_insert(
                    vm.vm_identity,
                    post_epoch,
                    this.as_ptr() as usize,
                    val_bits,
                );
            }
            if let Some(o) = obj {
                (*thread_ptr).native_pending_return = Some(o);
                return o.as_ptr() as i64;
            }
            return 0;
        }
    }
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &super::jit_scoped_memory_put_int_direct::THREAD_LOCAL_GET_DIRECT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

/// Leaf twin of [`jit_compat_thread_local_get_direct`]: lockless/alloc-free MRU
/// hit path (`-1` on miss or concurrent relocation).
pub(in crate::jit) unsafe extern "C" fn jit_compat_thread_local_get_leaf(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    if receiver == 0 || (receiver as u64 & 0x7) != 0 || (receiver as u64) >= (1u64 << 48) {
        return -1;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    if crate::vm::dispatch_policy(vm).is_jdk_only()
        || vm.mem.heap.concurrent_relocation_active()
    {
        return -1;
    }
    let epoch = vm.mem.address_epoch();
    let Some(val_bits) = cratonvm_native_builtins::phases_early::tl_compat_mru_lookup(
        vm.vm_identity,
        epoch,
        receiver as usize,
    ) else {
        return -1;
    };
    if val_bits == 1 {
        0
    } else {
        val_bits as i64
    }
}

/// Leaf twin address for `WELL_KNOWN_LEAF_TWIN_THREAD_LOCAL_GET`.
pub(in crate::jit::helpers) unsafe fn thread_local_get_leaf_twin_for(vm: &SharedVm) -> i64 {
    if super::jit_scoped_memory_put_int_direct::thread_local_real_fast_serves(vm) {
        super::jit_scoped_memory_put_int_direct::jit_thread_local_get_leaf as *const () as usize as i64
    } else if thread_local_compat_fast_serves(vm) {
        jit_compat_thread_local_get_leaf as *const () as usize as i64
    } else {
        0
    }
}
