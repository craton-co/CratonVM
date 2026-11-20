#[allow(unused_imports)]
use super::*;

/// Round 13 wave 13 (lane trace3): append the JDK frames of the method the
/// innermost captured frame was calling, when the VM served that method
/// without pushing them (`stackwalker::native_standin_frames`; the census is
/// the `Thread.sleep` and `Object.wait` families, for the throwables HotSpot
/// raises inside their native leaf). Both modes;
/// `CRATONVM_THROWABLE_NATIVE_STANDIN_FRAMES=0` restores the caller-on-top
/// trace. The switch is read only once a capture passed every screen, so an
/// ordinary throw pays the lock-free opcode screen and at most one
/// class-manager read. Re-capped: HotSpot counts these frames against
/// `MaxJavaStackTraceDepth` like any other.
///
/// Round 14 wave 5 (lane trace4): also on a virtual thread
/// (`stackwalker::virtual_thread_standin_frames`: the `Object.wait` rows only),
/// and, when the census answers nothing, the one `(Native Method)` frame of
/// any `native` method the innermost frame was calling
/// (`stackwalker::native_leaf_frame`, T3-1), both under the same switch.
///
/// Round 14 wave 6 (lane trace5; TR5-3): `timed_join` is the hint a served
/// `Thread.join(long)` left before raising `InterruptedException`
/// (`JvmThread::served_timed_join_hint`): on a platform thread, when the
/// census answered nothing for that throwable, the chain from the direct
/// `join(long)` call (`stackwalker::timed_join_standin_frames`).
///
/// Round 14 wave 7 (lane trace6): the hint is read by the call and throwable
/// it is taken for. An `InterruptedException` at a served `join()` with the
/// hint `true` gets no frames (TW6-2, `served_join_declines_standin_frames`:
/// a `java.lang.VirtualThread` target); an `IllegalArgumentException` at a
/// served two-site `(long, int)` overload gets the frame of the check the bit
/// names (TW6-3, `served_arg_check_standin_frames`). An interpreted receiver
/// `NullPointerException` is refused before either rule decodes the call
/// (RV6-2, `receiver_npe_gets_no_standin_frames`).
///
/// Round 15 wave 1 (lane trace): a virtual thread took the platform rows; since
/// the merge with interpreter round i1 wave 48 only under
/// `CRATONVM_THROWABLE_STANDIN_VIRTUAL_AS_BOUND=1`
/// ([`virtual_standin_frames_as_bound`], HotSpot's default virtual threads
/// being the reference). When nothing else answered, a served `Class.forName` / its `forName0` gets its frames
/// (TR6-1, `stackwalker::callback_native_leaf_frames`).
pub(in crate::vm::vm_exec) fn append_native_standin_frames<'a>(
    shared: &'a SharedVm,
    cm: &mut Option<CaptureClassManager<'a>>,
    throwable_class: ClassId,
    virtual_thread: bool,
    timed_join: Option<bool>,
    virtual_sleep: Option<bool>,
    served_raise: Option<&crate::runtime::stackwalker::ServedNativeRaise>,
    hidden_marked: Option<&crate::runtime::stackwalker::BacktraceFrame>,
    door_key: Option<(ClassId, u32)>,
    trace: &mut Vec<crate::runtime::stackwalker::BacktraceFrame>,
    max_depth: usize,
    mut site_memo: Option<&mut crate::runtime::stackwalker::StandinSiteMemo>,
) -> bool {
    use crate::runtime::stackwalker::{
        callback_native_leaf_frames, is_member_lookup_throwable, is_standin_throwable,
        may_stand_at_native_call, may_stand_at_standin_call, member_lookup_standin_frames,
        native_leaf_frame, native_standin_frames, receiver_npe_gets_no_standin_frames,
        served_arg_check_standin_frames, served_join_declines_standin_frames,
        timed_join_standin_frames, virtual_thread_standin_frames,
    };
    // JIT round 16 wave 9 (lane throwpath): the answer is whether nothing was
    // appended for a reason that depends only on the innermost frame and the
    // throwable's class -- what the capture memo may key on. Wave 7's memo
    // asked only the lock-free screens below, which answer `true` for every
    // compiled frame, so no compiled throw was ever memoised; the decoded
    // `compiled_innermost_gets_no_standin_frames` is the exact answer.
    //
    // Interpreter round i1 wave 48, lane L1: an interrupted virtual-thread
    // sleep raised in the shape of HotSpot's default virtual threads
    // (`JvmThread::virtual_sleep_hint`) lists `VirtualThread.sleepNanos`'s
    // throw, `Thread.sleepNanos` at its call of it and `Thread.sleep`.
    if let (true, Some(during)) = (virtual_thread, virtual_sleep) {
        if append_virtual_sleep_frames(shared, cm, throwable_class, during, trace, max_depth) {
            return false;
        }
    }
    let Some(innermost) = trace.last() else {
        return true;
    };
    // Interpreter round i2 wave 53, lane L3: a raise the native-call funnel
    // marked (`served_raise`, `JvmThread::served_native_raise`) is answered by
    // `stackwalker::served_raise_frames` once every family below answered
    // nothing -- past the screens that keep the families off a shape they
    // never take (an NPE at an instance invoke above all), and past the site
    // memo, whose "no frames" answer was given without a mark. Believed only
    // when the innermost frame is the native's caller
    // (`ServedNativeRaise::stands_at`).
    if crate::runtime::env_cache::frame_trace() {
        if let Some(mark) = served_raise {
            eprintln!(
                "[SERVED_RAISE_MARK] innermost {} stands_at={}",
                crate::runtime::stackwalker::backtrace_frame_debug(innermost),
                mark.stands_at(innermost)
            );
        }
    }
    // Interpreter round i2 wave 60, lane L3: the native a by-name door
    // entered (`ServedNativeRaise::native_callee`), believed whatever stands
    // on top of the marked frame -- a reflective splice, a dropped hidden
    // frame -- since the capture checked that only the throwable's
    // construction runs above it.
    let mark_callee = served_raise.and_then(|mark| mark.native_callee);
    // Wave 62, lane L3: else the method the innermost door is entering
    // (`door_key`, the thread's door record: `VmConfig::served_raise_door_key`
    // for `Method.invoke`'s door and every static JDK door; one carrier since
    // wave 64, one switch since wave 65), keyed with no hints: the
    // throwable a callee built carries none.
    let door_callee = mark_callee.or(door_key);
    // Wave 61, lane L3: the keyed families read the mark's own hints, whatever
    // stands on top (`VmConfig::served_raise_keyed`). A compiled or inlined
    // innermost frame is a callee the door entered compiled, whose own frame
    // is listed already: no key then.
    let keyed_hints = match mark_callee {
        Some(_) => served_raise
            .filter(|_| true)
            .map(|mark| mark.hints()),
        None => door_key.map(|_| crate::runtime::stackwalker::ServedRaiseHints::default()),
    }
    .filter(|_| {
        !matches!(
            innermost,
            crate::runtime::stackwalker::BacktraceFrame::Compiled { .. }
                | crate::runtime::stackwalker::BacktraceFrame::Inlined { .. }
        )
    });
    // Wave 60, lane L3: a marked frame the hidden drop removed
    // (`DirectMethodHandleAccessor.invokeImpl`, `@Hidden`, at its
    // `invokeExact`) is still the site the mark's rule decodes; its frames go
    // on top of what the drop left, as HotSpot lists them.
    let served_site = match served_raise {
        Some(mark) if mark.stands_at(innermost) => Some(innermost),
        Some(_) => hidden_marked,
        None => None,
    };
    let served_raise = served_raise.filter(|_| served_site.is_some());
    let hints = served_raise.map(|mark| mark.hints()).unwrap_or_default();
    let served_raise = served_raise.is_some();
    let marked = served_raise || door_callee.is_some();
    if marked {
        site_memo = None;
    }
    let census = may_stand_at_standin_call(innermost);
    let mut families = census || may_stand_at_native_call(innermost);
    if !families && !marked {
        return true;
    }
    // JIT round 16 wave 11 (lane throw2, TP16-10;
    // `CRATONVM_THROWABLE_STANDIN_SITE_MEMO=1`, `site_memo` is `Some` only
    // then): this thread already decoded this compiled site for this
    // throwable class, with no redefinition since, and the answer was "no
    // stand-in frames" -- given without the class-manager lock.
    let redefinitions = site_memo
        .as_ref()
        .map(|_| cratonvm_classloading::class_redefinition_count());
    if let (Some(memo), Some(redefinitions)) = (site_memo.as_deref(), redefinitions) {
        if memo.answers(redefinitions, throwable_class, innermost) {
            return true;
        }
    }
    let cm = cm.get_or_insert_with(|| shared.classes.class_manager.read());
    let cm: &crate::classloading::ClassManager = cm;
    let Some(tclass) = cm.class_store.get(throwable_class) else {
        return false;
    };
    if families && receiver_npe_gets_no_standin_frames(innermost, &tclass.name) {
        if !marked {
            return true;
        }
        families = false;
    }
    // Round 15 wave 5 (lane throw1): a compiled innermost frame's opcode,
    // decoded once instead of once per rule below.
    if families
        && crate::runtime::stackwalker::compiled_innermost_gets_no_standin_frames(
            &cm.class_store,
            innermost,
            &tclass.name,
        )
    {
        if !marked {
            if let (Some(memo), Some(redefinitions)) = (site_memo.as_deref_mut(), redefinitions) {
                memo.remember(redefinitions, throwable_class, innermost);
            }
            return true;
        }
        families = false;
    }
    let mut resolve =
        |requester: ClassId, name: &str| cm.find_class_by_name_for_class(name, requester);
    // JIT round 19 wave 5, lane throws5
    // (`CRATONVM_THROWABLE_STANDIN_FILL_CALL_SCREEN=1`, opt-in): an explicit
    // refill (`t.fillInStackTrace()`) is a call only `native_leaf_frame` can
    // answer (`stackwalker::innermost_calls_fill_in_stack_trace`). When it
    // answers nothing, nothing below appends either, and the answer is a
    // function of the frame, the throwable's class and the class store -- the
    // call is in flight, so its owner is loaded; a redefinition is in every
    // key -- so it is memoisable like the opcode screen's.
    if families
        && !marked
        && crate::runtime::stackwalker::standin_fill_call_screen_enabled()
        && crate::runtime::stackwalker::innermost_calls_fill_in_stack_trace(
            &cm.class_store,
            innermost,
        )
        && native_leaf_frame(&cm.class_store, innermost, tclass, &mut resolve).is_none()
    {
        if let (Some(memo), Some(redefinitions)) = (site_memo, redefinitions) {
            memo.remember(redefinitions, throwable_class, innermost);
        }
        return true;
    }
    let mut extra = None;
    if families && census && is_standin_throwable(&tclass.name) {
        if served_join_declines_standin_frames(
            &cm.class_store,
            innermost,
            &tclass.name,
            timed_join == Some(true),
        ) {
            return false;
        }
        // Round 15 wave 1 (lane trace) made this VM's virtual threads -- bound
        // threads running the JDK's platform branches -- take the platform
        // rows (HotSpot `-XX:-VMContinuations`). Interpreter round i1 wave 48,
        // lane L1 then settled the reference as HotSpot's DEFAULT virtual
        // threads (continuations on; the orchestrator's correction, recorded
        // in `docs/feature-designs/real-jdk-mode.md`),
        // whose sleep shape `append_virtual_sleep_frames` builds above and
        // whose `wait()` the virtual screen answers (382, not 389). So for
        // these throwable rows `CRATONVM_THROWABLE_STANDIN_VIRTUAL_AS_BOUND`
        // is read as default OFF (`virtual_standin_frames_as_bound`); `=1`
        // restores the round-15 platform rows. Since round 17 wave 2 the
        // switch's other arms (`Thread.run` middle frames, the walks, a parked
        // bound thread's published stack) read the same default OFF through
        // the one helper (`stackwalker::virtual_standin_as_bound`).
        let as_platform = !virtual_thread || virtual_standin_frames_as_bound();
        extra = if as_platform {
            native_standin_frames(&cm.class_store, innermost, &tclass.name, &mut resolve)
        } else {
            virtual_thread_standin_frames(&cm.class_store, innermost, &tclass.name, &mut resolve)
        };
        let interrupted = &*tclass.name == "java/lang/InterruptedException";
        if extra.is_none() && as_platform && interrupted {
            if let Some(timed) = timed_join {
                extra = timed_join_standin_frames(&cm.class_store, innermost, timed, &mut resolve);
            }
        }
        if extra.is_none() {
            if let Some(second_check) = timed_join {
                extra = served_arg_check_standin_frames(
                    &cm.class_store,
                    innermost,
                    &tclass.name,
                    second_check,
                    &mut resolve,
                );
            }
        }
    }
    // Interpreter round i1 wave 52, lane L3: a served `Class.getField` /
    // `getMethod` / `getConstructor` ... raising its miss lists the JDK
    // method at its `throw` (`stackwalker::member_lookup_standin_frames`).
    //
    // Interpreter round i2 wave 54, lane L3 (item 3 of
    // `i52-L3-proposal-a-bridge-that-raises-names-the-jdk-method-it-stands-for`):
    // under `VmConfig::member_lookup_on_mark` a capture the funnel's mark
    // stands for leaves this family to the mark's rule, which lists the same
    // frames (`stackwalker::i52_l3_member_lookup_tests::the_mark_lists_what_the_family_lists`);
    // a capture without a mark (a lookup that built its own throwable) keeps it.
    let retired_member_lookup = served_raise;
    if families
        && extra.is_none()
        && !retired_member_lookup
        && is_member_lookup_throwable(&tclass.name)
    {
        extra =
            member_lookup_standin_frames(&cm.class_store, innermost, &tclass.name, &mut resolve);
        // Interpreter round i6 wave 116, lane L3: the census proposal
        // `i52-L3-proposal-a-bridge-that-raises-names-the-jdk-method-it-stands-for`
        // asks for before the family is deleted. Every capture the family
        // answers WITHOUT a funnel mark (`served_raise=false answered=true`)
        // is one the mark's rule would not list.
        if crate::runtime::env_cache::dbg_sttrace() {
            eprintln!(
                "STTRACE_DBG_MEMBER_LOOKUP_FAMILY served_raise={served_raise} on_mark=true answered={} throwable={}",
                extra.is_some(),
                tclass.name
            );
        }
    }
    if families && extra.is_none() {
        extra = native_leaf_frame(&cm.class_store, innermost, tclass, &mut resolve)
            .map(|frame| vec![frame]);
    }
    // Round 15 wave 1 (lane trace; TR6-1): a `Class.forName` the VM serves
    // (or its `forName0`) raising a throwable of its own: `forName0 (Native
    // Method)` and, for the served Java overload, its `forName` frames.
    if families && extra.is_none() {
        extra = callback_native_leaf_frames(&cm.class_store, innermost, &tclass.name, &mut resolve);
    }
    // Interpreter round i2 wave 53, lane L3: the funnel's mark (see above).
    if let (true, None, Some(site)) = (served_raise, extra.as_ref(), served_site) {
        extra = crate::runtime::stackwalker::served_raise_frames(
            &cm.class_store,
            site,
            &tclass.name,
            hints,
            &mut resolve,
        );
    }
    // Interpreter round i2 wave 60, lane L3: the native a by-name door
    // entered (see above).
    if extra.is_none() {
        if let Some(callee) = door_callee {
            // Wave 61, lane L3: under the keyed families, the method the door
            // selected -- of any kind -- gets the families' answer by method
            // (`stackwalker::keyed_served_raise_frames`); a native's is its own
            // frame, as before.
            extra = match keyed_hints {
                Some(hints) => crate::runtime::stackwalker::keyed_served_raise_frames(
                    &cm.class_store,
                    callee,
                    &tclass.name,
                    virtual_thread && !virtual_standin_frames_as_bound(),
                    hints,
                    &mut resolve,
                ),
                None => {
                    crate::runtime::stackwalker::door_native_callee_frame(&cm.class_store, callee)
                        .map(|frame| vec![frame])
                }
            };
        }
    }
    let Some(mut extra) = extra else {
        return false;
    };
    // JIT round 23 wave 3, lane L3
    // (`CRATONVM_THROWABLE_C2_INTRINSIC_LEAF_DROP`): an optimizing-tier
    // caller's `System.arraycopy` / `Array.newArray` failure has no native
    // leaf on HotSpot, whose C2 intrinsics raise it with the caller on top
    // (C1 and the interpreter keep it).
    crate::runtime::stackwalker::drop_c2_intrinsic_native_leaf(
        trace,
        &tclass.name,
        &mut extra,
        &mut |activation: crate::runtime::stackwalker::InnermostActivation<'_>| {
            innermost_activation_is_ir(shared, activation)
        },
    );
    if extra.is_empty() {
        return false;
    }
    trace.extend(extra);
    crate::runtime::stackwalker::cap_throwable_trace(trace, max_depth);
    false
}

/// JIT round 23 wave 3, lane L3: does the innermost compiled activation of
/// this thread, the one named `label` at the top of the trace being captured,
/// stand for HotSpot's C2 code? The VM's one rule for that question,
/// `exceptions::body_is_c2_equivalent` (the fast throw's): an optimizing-tier
/// body, or a single-pass body hot enough that HotSpot would have C2 code
/// there. The second arm matters: the IR acceptance gate keeps the
/// single-pass body of many hot methods for good ("evidence: none", measured
/// on `A1Copy.oob` and `newArr` on `n23-w3`), where HotSpot ends in C2.
///
/// Wave 4 (`n23-w3` measurement: the drop never fired): the activation is
/// found by a live walk of this thread's compiled frames
/// (`conservative_roots::active_compiled_frames`), the innermost one with
/// `label`, after a quick look at the published frame record
/// (`innermost_compiled_site_key`), whose mirror names the activation the
/// thread ENTERED compiled code with, not the innermost one, below a
/// compiled-to-compiled call. Answers `false` -- keep the native frame, the
/// interpreter's and C1's answer -- when no live activation has `label`
/// (a capture made after it unwound, from a trap snapshot).
pub(in crate::vm::vm_exec) fn innermost_activation_is_ir(
    shared: &SharedVm,
    activation: crate::runtime::stackwalker::InnermostActivation<'_>,
) -> bool {
    use crate::runtime::stackwalker::InnermostActivation;
    let label = match activation {
        InnermostActivation::Compiled(label) => label,
        // Wave 4 (`n23-w4`, `H8Msgs.k`): a method this VM keeps in the
        // interpreter (here `ineligible-by-policy` after repeated
        // recompiles) that HotSpot, past its C2 threshold, runs as C2 code.
        // The same threshold as a single-pass body's, on the tiered
        // manager's interpreter invocation count of the method.
        InnermostActivation::Interpreted(method) => {
            // r23 w5 review: only where a compiler runs at all. Under
            // `--nojit` (HotSpot's `-Xint`) no method is ever C2 code, and
            // the interpreter keeps the native leaf there.
            if !shared.config.jit_enabled {
                return false;
            }
            let key = crate::jit::tiered::MethodKey::with_class_id(
                method.declaring_class_id,
                std::sync::Arc::clone(&method.class_name),
                std::sync::Arc::clone(&method.method_name),
                std::sync::Arc::clone(&method.method_descriptor),
            );
            return shared.jit.tiered_manager.invocation_count_of(&key)
                >= crate::runtime::exceptions::FAST_THROW_C2_EQUIVALENT_ENTRIES;
        }
    };
    if let Some((_, _, cm_ptr)) = crate::jit::conservative_roots::innermost_compiled_site_key() {
        // SAFETY: `innermost_compiled_site_key` hands out a registered method
        // whose activation is live on this thread below the caller.
        let cm = unsafe { &*cm_ptr };
        if cm.method_label.as_str() == label {
            return crate::runtime::exceptions::body_is_c2_equivalent(cm);
        }
    }
    crate::jit::stack_trace::active_compiled_frames()
        .iter()
        .rev()
        .find(|f| &*f.label == label)
        .is_some_and(|f| {
            f.cm_ptr != 0 && {
                // SAFETY: the walk reports activations live on this thread's
                // stack below the caller; `cm_ptr` is valid while its frame
                // is live (`ActiveCompiledFrame::cm_ptr`).
                let cm = unsafe { &*(f.cm_ptr as *const cratonvm_jit::CompiledMethod) };
                crate::runtime::exceptions::body_is_c2_equivalent(cm)
            }
        })
}

/// Round 15 wave 2 (lane trace2;
/// `r15w1-trace-served-app-loader-cnfe-has-no-loader-frames-FIXED-20260930.md`):
/// `loader_class` is the hint a served base `ClassLoader.loadClass(String)`
/// left right before building its `ClassNotFoundException` for a built-in
/// loader (`JvmThread::served_loader_miss_hint`, taken by every capture). The
/// JDK loader frames that native never pushed
/// (`stackwalker::served_loader_miss_frames`: `ClassLoader.loadClass`, down to
/// `BuiltinClassLoader.loadClass` at its `new ClassNotFoundException`) go on
/// top of whatever the innermost rules appended -- after a served
/// `Class.forName`'s `forName0 (Native Method)` -- in either mode and on any
/// thread. Re-capped like the other served frames. A capture without the hint
/// pays one `Option` test.
pub(in crate::vm::vm_exec) fn append_served_loader_miss_frames<'a>(
    shared: &'a SharedVm,
    cm: &mut Option<CaptureClassManager<'a>>,
    throwable_class: ClassId,
    loader_class: Option<ClassId>,
    trace: &mut Vec<crate::runtime::stackwalker::BacktraceFrame>,
    max_depth: usize,
) {
    let Some(loader_class) = loader_class else {
        return;
    };
    let cm = cm.get_or_insert_with(|| shared.classes.class_manager.read());
    let cm: &crate::classloading::ClassManager = cm;
    let Some(tclass) = cm.class_store.get(throwable_class) else {
        return;
    };
    // Round 15 wave 5 (lane trace5): a `findClass` native that Java code
    // called directly left the hint (a `URLClassLoader` subclass's own call
    // of the protected `findClass`): no `loadClass` ran, HotSpot shows none.
    // `CRATONVM_THROWABLE_SERVED_LOADER_SUBCLASS_FRAMES=0` drops the screen
    // with the subclass hint it guards.
    if trace.last().is_some_and(|innermost| {
        crate::runtime::stackwalker::innermost_calls_find_class(&cm.class_store, innermost)
    }) {
        // Round 15 wave 6 (lane trace6; TR5W5-4): that native's own frame,
        // `URLClassLoader.findClass` at its plain raise, which HotSpot shows
        // on top. `CRATONVM_THROWABLE_FIND_CLASS_NATIVE_FRAME=0` leaves the
        // caller on top, as before.
        let frame = trace.last().and_then(|innermost| {
            crate::runtime::stackwalker::url_find_class_native_frame(
                &cm.class_store,
                innermost,
                loader_class,
                &tclass.name,
            )
        });
        if let Some(frame) = frame {
            {
                trace.push(frame);
                crate::runtime::stackwalker::cap_throwable_trace(trace, max_depth);
            }
        }
        return;
    }
    let Some(extra) = crate::runtime::stackwalker::served_loader_miss_frames(
        &cm.class_store,
        loader_class,
        &tclass.name,
    ) else {
        return;
    };
    trace.extend(extra);
    crate::runtime::stackwalker::cap_throwable_trace(trace, max_depth);
}

/// JIT round 16 wave 13 (lane wmte; `r16-wmte-door-wmte-lists-no-jdk-frames-FIXED-20261001.md`):
/// `raise` is the hint a `MethodHandle` door left right before building its
/// `WrongMethodTypeException` (`JvmThread::mh_wmte_raise_hint`, taken only by
/// a capture of that class). The JDK frames the door never pushed
/// (`stackwalker::mh_wmte_raise_frames`: `Invokers.checkExactType` /
/// `newWrongMethodTypeException`, or `asType` / `setAsTypeCache` /
/// `asTypeUncached` with the caller's `checkGenericType` /
/// `invokeWithArguments` below them) go on top, in either mode and on any
/// thread, re-capped like the other served frames. A capture without the
/// hint pays one `Option` test.
pub(in crate::vm::vm_exec) fn append_mh_wmte_raise_frames<'a>(
    shared: &'a SharedVm,
    cm: &mut Option<CaptureClassManager<'a>>,
    throwable_class: ClassId,
    raise: Option<cratonvm_native_api::MhWmteRaise>,
    trace: &mut Vec<crate::runtime::stackwalker::BacktraceFrame>,
    max_depth: usize,
) {
    let Some(raise) = raise else {
        return;
    };
    let cm = cm.get_or_insert_with(|| shared.classes.class_manager.read());
    let cm: &crate::classloading::ClassManager = cm;
    let Some(tclass) = cm.class_store.get(throwable_class) else {
        return;
    };
    let Some(extra) = crate::runtime::stackwalker::mh_wmte_raise_frames(
        &cm.class_store,
        raise,
        trace.last(),
        &tclass.name,
    ) else {
        return;
    };
    trace.extend(extra);
    crate::runtime::stackwalker::cap_throwable_trace(trace, max_depth);
}

/// Load `java.lang.VirtualThread` (not initialize it) before a virtual-thread
/// sleep's `InterruptedException` is built, so the capture can read
/// `VirtualThread.sleepNanos`'s bytes (interpreter round i1 wave 48 follow-up,
/// lane L1). This VM's virtual threads are `BoundVirtualThread`s and may never
/// have loaded it: the capture holds the class manager's read guard and can
/// only find loaded classes, so on a fresh VM the FIRST interrupted sleep
/// (the host's `L1W48VirtualSleepPath` row `pre sleep0`) got the message but
/// no JDK frames. HotSpot loads the class at startup. One read-locked lookup
/// once it is loaded.
pub(crate) fn preload_virtual_thread_class(shared: &SharedVm) {
    const VIRTUAL_THREAD: &str = "java/lang/VirtualThread";
    let loaded = shared
        .classes
        .class_manager
        .read()
        .resolve_fast_path_class_id(VIRTUAL_THREAD)
        .is_some();
    if !loaded {
        let result = shared.load_class_concurrent(VIRTUAL_THREAD);
        if crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[VIRTUAL_SLEEP_FRAMES] loaded {VIRTUAL_THREAD}: {:?}",
                result.is_ok()
            );
        }
    }
}

/// [`append_native_standin_frames`] for an `InterruptedException` a
/// virtual-thread sleep raised in the shape of HotSpot's default virtual
/// threads (interpreter round i1 wave 48, lane L1;
/// `stackwalker::virtual_sleep_continuation_frames`): `true` when it
/// rebuilt the trace's head.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn append_virtual_sleep_frames<'a>(
    shared: &'a SharedVm,
    cm: &mut Option<CaptureClassManager<'a>>,
    throwable_class: ClassId,
    during: bool,
    trace: &mut Vec<crate::runtime::stackwalker::BacktraceFrame>,
    max_depth: usize,
) -> bool {
    let Some(innermost) = trace.last() else {
        return false;
    };
    let cm = cm.get_or_insert_with(|| shared.classes.class_manager.read());
    let cm: &crate::classloading::ClassManager = cm;
    if cm.class_store.get(throwable_class).map(|c| &*c.name)
        != Some("java/lang/InterruptedException")
    {
        return false;
    }
    let mut resolve =
        |requester: ClassId, name: &str| cm.find_class_by_name_for_class(name, requester);
    let Some((replace, extra)) = crate::runtime::stackwalker::virtual_sleep_continuation_frames(
        &cm.class_store,
        innermost,
        during,
        &mut resolve,
    ) else {
        if crate::runtime::env_cache::frame_trace() {
            let entry = innermost.to_entry();
            eprintln!(
                "[VIRTUAL_SLEEP_FRAMES] none: innermost={}.{}@{} virtual_thread_loaded={}",
                entry.class_name,
                entry.method_name,
                entry.byte_code_index,
                cm.resolve_fast_path_class_id("java/lang/VirtualThread")
                    .is_some()
            );
        }
        return false;
    };
    if let Some(frame) = replace {
        trace.pop();
        trace.push(frame);
    }
    trace.extend(extra);
    crate::runtime::stackwalker::cap_throwable_trace(trace, max_depth);
    true
}

/// `CRATONVM_THROWABLE_STANDIN_VIRTUAL_AS_BOUND` (interpreter round i1 wave
/// 48, lane L1): does a capture on a virtual thread rebuild a stand-in's JDK
/// frames as a platform thread's ([`append_native_standin_frames`])? Read
/// only once a virtual-thread capture stands at a census call of a stand-in
/// throwable, like the area's other throwable-frame switches.
///
/// The same key as round 15's `stackwalker::virtual_standin_as_bound`, which
/// is its single reader since round 17 wave 2 (lane smallfix): default OFF
/// everywhere (it used to read default ON there, so unset, the arms
/// disagreed: `r17-standin-virtual-as-bound-readers-disagreed-FIXED-20261005.md`).
/// `=1`: everything as a bound thread (HotSpot `-XX:-VMContinuations`,
/// round 15 wave 1).
#[cold]
pub(in crate::vm::vm_exec) fn virtual_standin_frames_as_bound() -> bool {
    // Off by default (wave 48 follow-up): the reference is HotSpot's DEFAULT
    // virtual threads, whose interrupted `wait()` lists the virtual branch's
    // `wait0` call (`Object.java:382` on 25.0.3), which the virtual screen
    // answers and this switch would replace by the platform one (389).
    crate::runtime::stackwalker::virtual_standin_as_bound()
}

/// Round 14 wave 3 (lane trace): the `java.lang.Thread.run` frame at the
/// bottom of a started thread's capture, which the VM-served `Thread.run`
/// never pushed (`stackwalker::thread_run_standin_frame`). Only for a capture
/// that `MaxJavaStackTraceDepth` did not cut (HotSpot would have cut this
/// frame first). The task is read as that native reads it: `target`, else
/// `holder.task`. `outermost` is the capture's outermost frame BEFORE the
/// `--real-jdk` hidden-frame drop: a lambda task's `run` is a hidden frame
/// HotSpot leaves out while keeping `Thread.run` below it.
///
/// Round 14 wave 6 (lane trace5; TR5-2): the decision is memoised per thread
/// (`memo`, `JvmThread::thread_run_bottom_memo`), so a throw on an executor
/// worker skips the class-manager lock and the field walks.
/// `CRATONVM_THROWABLE_THREAD_RUN_MEMO=0` never fills it (so never hits).
pub(in crate::vm::vm_exec) fn prepend_thread_run_standin_frame<'a>(
    shared: &'a SharedVm,
    cm: &mut Option<CaptureClassManager<'a>>,
    thread_obj: Option<ObjectRef>,
    memo: &mut Option<crate::runtime::stackwalker::ThreadRunBottomMemo>,
    outermost: Option<&crate::runtime::stackwalker::BacktraceFrame>,
    trace: &mut Vec<crate::runtime::stackwalker::BacktraceFrame>,
    max_depth: usize,
) {
    let (Some(thread_obj), Some(outermost)) = (thread_obj, outermost) else {
        return;
    };
    if max_depth != 0 && trace.len() >= max_depth {
        return;
    }
    // Round 14 wave 4 (lane trace3): a thread class running its own `run()`
    // (a `ForkJoinWorkerThread`, a `Thread` subclass) is refused before the
    // lock, from the object's header.
    let thread_class = shared.mem.heap.class_id_of(thread_obj);
    if crate::runtime::stackwalker::outermost_is_threads_own_run(outermost, thread_class) {
        return;
    }
    let redefinitions = cratonvm_classloading::class_redefinition_count();
    // Round 15 wave 3 (lane trace3): an answer is one frame and how many
    // copies of it go in (one per served `Thread.run` of a nested `Thread`
    // task), the OUTERMOST ones cut first at `MaxJavaStackTraceDepth`.
    let insert_copies = |trace: &mut Vec<crate::runtime::stackwalker::BacktraceFrame>,
                         frame: &crate::runtime::stackwalker::BacktraceFrame,
                         copies: usize| {
        let room = if max_depth == 0 {
            copies
        } else {
            copies.min(max_depth.saturating_sub(trace.len()))
        };
        for _ in 0..room {
            trace.insert(0, frame.clone());
        }
    };
    if let Some((answer, copies)) = memo.as_ref().and_then(|m| {
        m.lookup(redefinitions, thread_class, outermost)
            .map(|answer| (answer, m.copies()))
    }) {
        if let Some(frame) = answer {
            insert_copies(trace, frame, copies);
        }
        return;
    }
    let cm = cm.get_or_insert_with(|| shared.classes.class_manager.read());
    let cm: &crate::classloading::ClassManager = cm;
    let answer = thread_run_bottom_frame(shared, &cm.class_store, thread_obj, outermost);
    {
        let copies = answer.as_ref().map_or(1, |(_, copies)| *copies);
        *memo = crate::runtime::stackwalker::ThreadRunBottomMemo::new(
            redefinitions,
            thread_class,
            outermost,
            answer.as_ref().map(|(frame, _)| frame.clone()),
        )
        .map(|m| m.with_copies(copies));
    }
    if let Some((frame, copies)) = answer {
        insert_copies(trace, &frame, copies);
    }
}

/// `stackwalker::thread_run_standin_frame` for the thread `thread_obj`, its
/// task read as the VM-served `Thread.run` reads it: `target`, else
/// `holder.task`. Shared by the throwable capture and the `StackWalker` walk
/// (round 14 wave 3, lane trace2).
///
/// Round 15 wave 3 (lane trace3;
/// `r15w2-trace2-thread-run-middle-frame-lambda-and-thread-tasks-FIXED-20260930.md`
/// shape 2): answers the frame and how many copies of it go in. A task that is
/// itself a platform `Thread` inheriting `run` (`new Thread(new Thread(task))`)
/// is run by the served `Thread.run` through ITS served `Thread.run`, which runs
/// its own task: one more frame per such hop (HotSpot 25: `Task.run`,
/// `Thread.run`, `Thread.run`), and the rules below judge the last task under
/// the last thread. The objects are read as the native reads them.
/// `CRATONVM_THROWABLE_THREAD_RUN_NESTED_TASKS=0` restores the one-frame rule
/// (which answers nothing for such a chain); read only once a `Thread` task is
/// met.
pub(in crate::vm::vm_exec) fn thread_run_bottom_frame(
    shared: &SharedVm,
    store: &crate::classloading::ClassStore,
    thread_obj: ObjectRef,
    outermost: &crate::runtime::stackwalker::BacktraceFrame,
) -> Option<(crate::runtime::stackwalker::BacktraceFrame, usize)> {
    use crate::runtime::stackwalker::thread_task_enters_thread_run;
    // The most served `Thread.run` hops a chain is followed through.
    const NESTED_THREAD_TASKS_MAX: usize = 8;
    let heap = &shared.mem.heap;
    let task_of = |thread: ObjectRef| served_thread_task_of(shared, store, thread);
    let mut runner = thread_obj;
    let mut task = task_of(runner)?;
    let mut copies = 1usize;
    while copies < NESTED_THREAD_TASKS_MAX
        && thread_task_enters_thread_run(store, heap.class_id_of(task))
        && thread_task_enters_thread_run(store, heap.class_id_of(runner))
        // Round 15 wave 7 (lane drainC): the per-VM latch, not an env read
        // per hop (`r15w3-trace4-review-findings` item 4).
        && shared.threads.thread_layout_cache.nested_tasks_on()
    {
        runner = task;
        // A nested thread without a task ran nothing: no frame came from it.
        task = task_of(runner)?;
        copies += 1;
    }
    thread_run_bottom_frame_for(shared, store, runner, task, outermost).map(|frame| (frame, copies))
}

/// The task the VM-served `Thread.run` of `thread` runs: `target`, else
/// `holder.task` (round 15 wave 3, lane trace3: as the served native reads it,
/// `lang_misc::served_thread_run_task` -- for a `Thread` subclass, the
/// `target` / `holder` fields `java.lang.Thread` declares, not a subclass's
/// own of that name). Split out of [`thread_run_bottom_frame`] (JIT round 19
/// wave 11, lane gc11) for [`add_thread_info_hidden_run_entries`].
pub(in crate::vm::vm_exec) fn served_thread_task_of(
    shared: &SharedVm,
    store: &crate::classloading::ClassStore,
    thread: ObjectRef,
) -> Option<ObjectRef> {
    let heap = &shared.mem.heap;
    let object_field = |obj: ObjectRef, name: &str| -> Option<ObjectRef> {
        let slot = resolve_field_index_in_hierarchy(heap.class_id_of(obj), name, store)?;
        match heap.get_field(obj, slot) {
            Value::Object(Some(o)) => Some(o),
            _ => None,
        }
    };
    let by_name = || {
        object_field(thread, "target")
            .or_else(|| object_field(object_field(thread, "holder")?, "task"))
    };
    let Some(declaring) = thread_class_above(store, heap.class_id_of(thread)) else {
        return by_name();
    };
    // Round 15 wave 4 (lane threadvm): the per-VM latch
    // (`r15w3-trace4-review-findings` item 4), not an env read per call.
    if !shared.threads.thread_layout_cache.declared_fields_on() {
        return by_name();
    }
    let target = resolve_field_index_in_hierarchy(declaring, "target", store);
    let holder = resolve_field_index_in_hierarchy(declaring, "holder", store);
    if target.is_none() && holder.is_none() {
        return by_name();
    }
    let read = |slot: usize| -> Option<ObjectRef> {
        match heap.get_field(thread, slot) {
            Value::Object(Some(o)) => Some(o),
            _ => None,
        }
    };
    target
        .and_then(&read)
        .or_else(|| object_field(holder.and_then(&read)?, "task"))
}



/// The captured index (0 = outermost) whose entry sits at `position` of a
/// trace the served frames `inserted_before` went into, or `captured` when
/// no captured entry sits at or above it: an insertion before `position` is
/// recorded as one before that index, which shifts exactly the captured
/// entries at or above it ([`shift_served_frame_indices`]).
pub(in crate::vm::vm_exec) fn captured_index_at(position: usize, captured: usize, inserted_before: &[usize]) -> usize {
    (0..captured)
        .find(|&c| c + inserted_before.iter().filter(|&&at| at <= c).count() >= position)
        .unwrap_or(captured)
}

/// JIT round 19 proposal wave 11, lane gc11
/// (`CRATONVM_JMX_THREAD_INFO_HIDDEN_RUN_FRAMES`, default ON; page
/// `r19-cross-thread-threadinfo-omits-the-lambda-proxy-run-frame-20261007`):
/// the HIDDEN frames a `ThreadInfo` stack lists between `java.lang.Thread.run`
/// and the task it runs, which `thread_stack_trace_served` (shared with
/// `Thread.getStackTrace`, which hides them) leaves out. MEASURED, HotSpot
/// 25.0.3, a thread parked under a lambda `Runnable`
/// (`R19Sync6LockedDepths` `other-thread`, lane gc11's `Gc11Lam`):
///
/// ```text
/// ThreadInfo / getAllStackTraces      Thread.getStackTrace (other thread)
/// lambda$main$0(Gc11Lam.java:7)       lambda$main$0(Gc11Lam.java:7)
/// Gc11Lam$$Lambda/0x...run(Unknown)   Thread.run(Thread.java:1474)
/// Thread.runWith(Thread.java:1487)
/// Thread.run(Thread.java:1474)
/// ```
///
/// (a `Runnable` class: `R.run`, `Thread.runWith`, `Thread.run`; a method
/// reference: the proxy too). Two insertions, each fail-closed:
///
/// * `Thread.runWith` (`@Hidden`) after a `Thread.run` entry whose next
///   entry is not it -- `--compatible`, whose served `Thread.run` pushes no
///   frame -- only when that entry stands at `run`'s call of `runWith`
///   ([`stackwalker::served_thread_run_debugger_entries`], the rows JVMTI
///   already lists). `--real-jdk` runs the real `runWith`, so its frame is
///   there.
/// * the lambda proxy's SAM frame after the INNERMOST such `runWith` (or
///   `Thread.run` on a JDK without `runWith`), when the thread's served task
///   (`target` / `holder.task`, followed through nested `Thread` tasks as the
///   served `Thread.run` follows them) is a VM lambda proxy whose
///   implementation is the next entry's method -- this VM's lambda dispatch
///   pushes no proxy frame (`stackwalker::lambda_proxy_standin_frames`).
///
/// `inserted_before` gets one captured index per inserted frame (so the
/// locked-monitor attribution keeps its depths). Not for
/// `Thread.getStackTrace`, which hides both frames (the native reads
/// `thread_stack_trace`, untouched here).
pub(in crate::vm::vm_exec) fn add_thread_info_hidden_run_entries(
    shared: &SharedVm,
    thread_obj: ObjectRef,
    trace: &mut Vec<StackTraceEntry>,
    captured: usize,
    inserted_before: &mut Vec<usize>,
) {
    const THREAD: &str = "java/lang/Thread";
    let is =
        |e: &StackTraceEntry, method: &str| &*e.class_name == THREAD && &*e.method_name == method;
    // The screen: a `Thread.run` entry with something above it.
    let any_run = trace
        .iter()
        .enumerate()
        .any(|(i, e)| is(e, "run") && i + 1 < trace.len());
    if !any_run {
        return;
    }
    // The served task's lambda site, under the class-manager lock (the
    // `class_manager` -> `lambda_proxies` nesting `thread_run_bottom_frame_for`
    // documents), and the `runWith` rows.
    let (debugger_rows, site) = {
        let cm = shared.classes.class_manager.read();
        let rows = crate::runtime::stackwalker::served_thread_run_debugger_entries(&cm.class_store);
        let heap = &shared.mem.heap;
        let mut runner = thread_obj;
        let mut task = served_thread_task_of(shared, &cm.class_store, runner);
        for _ in 0..8 {
            let Some(t) = task else {
                break;
            };
            // As `thread_run_bottom_frame` follows a chain: a `Thread` task
            // under a runner that inherits `Thread.run`.
            let enters = |o: ObjectRef| {
                crate::runtime::stackwalker::thread_task_enters_thread_run(
                    &cm.class_store,
                    heap.class_id_of(o),
                )
            };
            if !enters(t) || !enters(runner) {
                break;
            }
            runner = t;
            task = served_thread_task_of(shared, &cm.class_store, runner);
        }
        let site = task.and_then(|t| shared.classes.lambda_call_site_for(heap.class_id_of(t)));
        (rows, site)
    };
    let Some(rows) = debugger_rows else {
        return;
    };
    let run_site_bci = rows.first().map(|e| e.byte_code_index);
    let run_with_row = rows.get(1);
    // `Thread.runWith` after each `Thread.run` that calls it.
    if let Some(run_with_row) = run_with_row {
        let mut k = 0;
        while k + 1 < trace.len() {
            if is(&trace[k], "run")
                && !is(&trace[k + 1], "runWith")
                && Some(trace[k].byte_code_index) == run_site_bci
            {
                let before = captured_index_at(k + 1, captured, inserted_before);
                inserted_before.push(before);
                inserted_before.sort_unstable();
                trace.insert(k + 1, run_with_row.clone());
                k += 1;
            }
            k += 1;
        }
    }
    // The proxy after the innermost hop into the task.
    let Some(site) = site else {
        return;
    };
    let hop = if run_with_row.is_some() {
        "runWith"
    } else {
        "run"
    };
    let Some(at) = trace
        .iter()
        .rposition(|e| is(e, hop))
        .map(|i| i + 1)
        .filter(|&i| i < trace.len())
    else {
        return;
    };
    let body = &trace[at];
    let handle = &site.impl_handle;
    let implements = *handle.class_name == *body.class_name
        && *handle.member_name == *body.method_name
        && body
            .method_descriptor
            .as_deref()
            .is_none_or(|d| *handle.descriptor == *d);
    if !implements {
        return;
    }
    let id = site.proxy_class_id;
    // No class-manager lock held: the host lookup takes it.
    let Some((_, host)) = crate::runtime::interpreter::lambda_proxy_host(shared, id) else {
        return;
    };
    let name: std::sync::Arc<str> =
        std::sync::Arc::from(format!("{host}$$Lambda/0x{:016x}", id.as_u32()));
    let Some(proxy) = crate::runtime::stackwalker::lambda_proxy_standin_entry(&site, &name) else {
        return;
    };
    let before = captured_index_at(at, captured, inserted_before);
    inserted_before.push(before);
    inserted_before.sort_unstable();
    trace.insert(at, proxy);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[THREAD_INFO_HIDDEN_RUN] proxy={name} at={at} inserted={inserted_before:?}");
    }
}

/// `java.lang.Thread`'s id when it is a PROPER superclass of `class_id` (a
/// `Thread` subclass), else `None` (round 15 wave 3, lane trace3).
pub(in crate::vm::vm_exec) fn thread_class_above(
    store: &crate::classloading::ClassStore,
    class_id: ClassId,
) -> Option<ClassId> {
    let mut id = store.get(class_id)?.superclass;
    for _ in 0..64 {
        let current = id?;
        let class = store.get(current)?;
        if &*class.name == "java/lang/Thread" {
            return Some(current);
        }
        id = class.superclass;
    }
    None
}

/// Round 15 wave 4 (lane threadvm;
/// `r15w3-trace4-vm-thread-fields-by-name-patch-FIXED-20260930.md`): the class
/// whose `Thread` fields (`holder`, `target`, `name`, `tid`, `eetop`) a
/// thread of class `class_id` is read through: `java.lang.Thread` when it is
/// a PROPER superclass, so a subclass's own field of the same name is not
/// taken for `Thread`'s (the VM read `holder` by name, so a subclass
/// declaring `Object holder` decided the thread's daemon flag, hence VM
/// shutdown), else `class_id` itself. The field resolved from the answer is
/// still found by name, walking up: on `java.lang.Thread` that is exactly
/// the field it declares, or none. `CRATONVM_THREAD_RUN_THREAD_DECLARED_FIELDS=0`
/// (per VM, read only for a subclass) restores `class_id`.
pub(crate) fn thread_field_owner(
    shared: &SharedVm,
    store: &crate::classloading::ClassStore,
    class_id: ClassId,
) -> ClassId {
    match thread_class_above(store, class_id) {
        Some(thread) if shared.threads.thread_layout_cache.declared_fields_on() => thread,
        _ => class_id,
    }
}

/// Round 15 wave 5 (lane threadvm2; TVM-4, `jit-r15w4-threadvm-proposals-RETIRED-20261003.md`):
/// the slot of `name` on a thread of class `class_id`, exactly
/// `resolve_field_index_in_hierarchy(thread_field_owner(..), name, ..)`, but
/// answered by the per-VM `ThreadLayoutCache` without the class-manager lock
/// once warm: for `java.lang.Thread` itself (the class the memo's slots are
/// bound to) and for a subclass whose "`Thread` above" answer is memoised. A
/// name the memo does not keep, a second `java.lang.Thread` class, a class
/// below no `Thread`, or `CRATONVM_THREAD_LAYOUT_CACHE=0` walks under the
/// lock, as before, and warms the memo for the next call.
/// `resolve_thread_id_from_thread_obj` runs on every unpark and interrupt.
pub(crate) fn thread_field_slot(shared: &SharedVm, class_id: ClassId, name: &str) -> Option<usize> {
    let cache = &shared.threads.thread_layout_cache;
    let resolve_on = |thread_class: ClassId| {
        let cm = shared.classes.class_manager.read();
        resolve_field_index_in_hierarchy(thread_class, name, &cm.class_store)
    };
    if cache.enabled() {
        let thread_class = if cache.is_thread_class(class_id) {
            Some(class_id)
        } else {
            match cache.thread_class_above(class_id) {
                Some(Some(above)) if cache.declared_fields_on() => Some(above),
                _ => None,
            }
        };
        if let Some(thread_class) = thread_class {
            if let Some(slot) = cache.declared_slot(thread_class, name, || resolve_on(thread_class))
            {
                return slot;
            }
        }
    }
    let cm = shared.classes.class_manager.read();
    let store = &cm.class_store;
    if cache.enabled() {
        let above = thread_class_above(store, class_id);
        cache.remember_thread_class_above(class_id, above);
        let is_thread = store
            .get(class_id)
            .is_some_and(|c| &*c.name == "java/lang/Thread");
        if above.is_none() && is_thread {
            // Binds the memo's slots to this `java.lang.Thread`; a second
            // class of that name is refused inside and walks below.
            if let Some(slot) = cache.declared_slot(class_id, name, || {
                resolve_field_index_in_hierarchy(class_id, name, store)
            }) {
                return slot;
            }
        }
    }
    resolve_field_index_in_hierarchy(thread_field_owner(shared, store, class_id), name, store)
}

/// [`thread_run_bottom_frame`] for the served `Thread.run` of `thread_obj`
/// running `task`: the one frame of the round-14 rules.
pub(in crate::vm::vm_exec) fn thread_run_bottom_frame_for(
    shared: &SharedVm,
    store: &crate::classloading::ClassStore,
    thread_obj: ObjectRef,
    task: ObjectRef,
    outermost: &crate::runtime::stackwalker::BacktraceFrame,
) -> Option<crate::runtime::stackwalker::BacktraceFrame> {
    let heap = &shared.mem.heap;
    // Round 14 wave 3 (lane trace2): a lambda task's proxy has no class in the
    // store and pushes no frame, so the outermost frame is its implementation
    // method (`LambdaCallSite::impl_handle`). The id-range test in
    // `lambda_call_site_for` answers an ordinary task without the table's lock.
    //
    // Lock order (round 14 wave 5, lane fixup2; review4 item 2a): every caller
    // holds the `class_manager` READ guard (`store` borrows it), so a lambda
    // task nests `class_manager` (read) -> `lambda_proxies` (read). Decided
    // sound and kept: no path takes `class_manager` while holding
    // `lambda_proxies` -- its writers are insert-only
    // (`invokedynamic.rs`, `lambda_proxy.rs`, `class_realm.rs`, this file),
    // its readers copy out and drop the guard first (`typecheck.rs`
    // `lambda_proxy_satisfies`, `invoke.rs`, `lambda.rs`, this file's
    // dispatch and gpu-offload readers). Keep it that way: a
    // `lambda_proxies` -> `class_manager` nesting anywhere would close a cycle
    // with this one once a writer queues on either lock. Moving the lookup
    // out from under the guard needs the task resolved without `store`, i.e.
    // restructuring both callers' lazily taken guard.
    if let Some(site) = shared.classes.lambda_call_site_for(heap.class_id_of(task)) {
        let handle = &site.impl_handle;
        return crate::runtime::stackwalker::thread_run_standin_frame_for_lambda(
            store,
            outermost,
            heap.class_id_of(thread_obj),
            (&*site.sam_method_name, &*site.sam_descriptor),
            (
                &*handle.class_name,
                &*handle.member_name,
                &*handle.descriptor,
            ),
        );
    }
    crate::runtime::stackwalker::thread_run_standin_frame(
        store,
        outermost,
        heap.class_id_of(thread_obj),
        heap.class_id_of(task),
    )
}

/// Round 14 wave 3 (lane trace2; T14W3-3): the `java.lang.Thread.run` frame
/// between a caller that called the VM-served `Thread.run` (`super.run()` in
/// a `Thread` subclass, a direct `thread.run()`) and the task's `run` it
/// called (`stackwalker::thread_run_middle_frame`). `trace` is
/// OUTERMOST-first and not yet cut to `MaxJavaStackTraceDepth`'s cap by this
/// (the caller re-caps). Runs BEFORE the `--real-jdk` hidden-frame drop: a
/// lambda task's `run` is hidden, and the pair is judged on it. Whether any
/// frame was inserted.
///
/// Round 15 wave 2 (lane trace2; `r15w1-trace-virtual-as-bound-residual-arms`
/// item 2): on a virtual thread too (`virtual_thread`), which is a bound
/// thread whose `t.run()` / `super.run()` enters the same VM-served
/// `Thread.run`; `stackwalker::virtual_standin_as_bound` (read only once a
/// pair passed the screen) set to `0` restores the platform-only rule.
pub(in crate::vm::vm_exec) fn insert_thread_run_middle_frames<'a>(
    shared: &'a SharedVm,
    cm: &mut Option<CaptureClassManager<'a>>,
    trace: &mut Vec<crate::runtime::stackwalker::BacktraceFrame>,
    virtual_thread: bool,
) -> bool {
    use crate::runtime::stackwalker::{
        may_need_thread_run_middle_frame, thread_run_middle_frame, BacktraceFrame,
    };
    if !trace
        .windows(2)
        .any(|pair| may_need_thread_run_middle_frame(&pair[0], &pair[1]))
    {
        return false;
    }
    // Round 17 wave 2 (orchestrator, `r17-standin-virtual-as-bound-readers-disagreed`):
    // `Thread.run` is real bytecode on both thread kinds, and HotSpot prints its
    // middle frame on a virtual thread too, so this insertion no longer asks
    // `virtual_standin_as_bound` (now read default OFF for the stand-in arms).
    let _ = virtual_thread;
    let cm = cm.get_or_insert_with(|| shared.classes.class_manager.read());
    let cm: &crate::classloading::ClassManager = cm;
    let mut resolve =
        |requester: ClassId, name: &str| cm.find_class_by_name_for_class(name, requester);
    let mut inserted = false;
    // Innermost first: an insertion shifts only pairs already judged.
    for callee in (1..trace.len()).rev() {
        if !may_need_thread_run_middle_frame(&trace[callee - 1], &trace[callee]) {
            continue;
        }
        let entry = thread_run_middle_frame(
            &cm.class_store,
            &trace[callee - 1],
            &trace[callee],
            &mut resolve,
        );
        if let Some(entry) = entry {
            trace.insert(callee, BacktraceFrame::Entry(entry));
            inserted = true;
        }
    }
    inserted
}

/// Round 15 wave 1 (lane trace; TR6-1 of `jit-r14-trace6-proposals-RETIRED-20260930.md`):
/// `Class.forName0 (Native Method)` -- and, for a `Class.forName` the VM
/// serves, its Java frames -- between a caller standing at that call and the
/// loader's `loadClass` / the `<clinit>` the native ran
/// (`stackwalker::callback_native_middle_frames`). `trace` is OUTERMOST-first
/// and not yet cut again (the caller re-caps). Any thread; the pairs are
/// judged before the `--real-jdk` hidden-frame drop, as the `Thread.run`
/// middle frames are. Whether any frame was inserted.
pub(in crate::vm::vm_exec) fn insert_callback_native_middle_frames<'a>(
    shared: &'a SharedVm,
    cm: &mut Option<CaptureClassManager<'a>>,
    trace: &mut Vec<crate::runtime::stackwalker::BacktraceFrame>,
) -> bool {
    use crate::runtime::stackwalker::{
        callback_native_middle_frames, may_need_loader_middle_frames,
        served_load_class_middle_frames,
    };
    // Round 15 wave 5 (lane trace5): and the two `ClassLoader.loadClass`
    // frames between a caller of the served base `loadClass` and the
    // `findClass` it ran (`stackwalker::served_load_class_middle_frames`);
    // one screen for both rules.
    if !trace
        .windows(2)
        .any(|pair| may_need_loader_middle_frames(&pair[0], &pair[1]))
    {
        return false;
    }
    let cm = cm.get_or_insert_with(|| shared.classes.class_manager.read());
    let cm: &crate::classloading::ClassManager = cm;
    let mut resolve =
        |requester: ClassId, name: &str| cm.find_class_by_name_for_class(name, requester);
    let mut inserted = false;
    // Innermost first: an insertion shifts only pairs already judged.
    for callee in (1..trace.len()).rev() {
        let frames = callback_native_middle_frames(
            &cm.class_store,
            &trace[callee - 1],
            &trace[callee],
            &mut resolve,
        )
        .or_else(|| {
            served_load_class_middle_frames(
                &cm.class_store,
                &trace[callee - 1],
                &trace[callee],
                &mut resolve,
            )
        });
        if let Some(frames) = frames {
            for (offset, frame) in frames.into_iter().enumerate() {
                trace.insert(callee + offset, frame);
            }
            inserted = true;
        }
    }
    inserted
}

/// [`insert_callback_native_walk_entries`] for the current thread's own
/// listing (`thread_stack_trace_served`: `Thread.getAllStackTraces`,
/// `ThreadInfo`), interpreter round i5 wave 104, lane L3 (hand-back of
/// `L3W104ClinitTriggerListings`, `--compatible` `Field.*` rows: the listing
/// lacked `Unsafe.ensureClassInitialized0 (Native Method)` between
/// `Unsafe.ensureClassInitialized` and the `<clinit>` it ran, which the walk
/// lists). Each inserted entry goes into `inserted_before` as one before the
/// captured index of the callee (`captured_index_at`), so the locked-monitor
/// attribution keeps its depths. `trace` is OUTERMOST-first.
pub(in crate::vm::vm_exec) fn insert_callback_native_listing_entries(
    cm: &crate::classloading::ClassManager,
    trace: &mut Vec<StackTraceEntry>,
    captured: usize,
    inserted_before: &mut Vec<usize>,
) {
    use crate::runtime::stackwalker::{
        callback_native_middle_frames, may_need_loader_middle_frames,
        served_load_class_middle_frames, BacktraceFrame,
    };
    if !trace
        .iter()
        .any(|e| matches!(&*e.method_name, "<clinit>" | "loadClass" | "findClass"))
    {
        return;
    }
    let mut resolve =
        |requester: ClassId, name: &str| cm.find_class_by_name_for_class(name, requester);
    let mut any = false;
    // Innermost first: an insertion shifts only pairs already judged, and
    // records a captured index above every one still to be judged.
    for callee in (1..trace.len()).rev() {
        let caller_frame = BacktraceFrame::Entry(trace[callee - 1].clone());
        let callee_frame = BacktraceFrame::Entry(trace[callee].clone());
        if !may_need_loader_middle_frames(&caller_frame, &callee_frame) {
            continue;
        }
        let middle = callback_native_middle_frames(
            &cm.class_store,
            &caller_frame,
            &callee_frame,
            &mut resolve,
        )
        .or_else(|| {
            served_load_class_middle_frames(
                &cm.class_store,
                &caller_frame,
                &callee_frame,
                &mut resolve,
            )
        });
        let Some(middle) = middle.filter(|m| !m.is_empty()) else {
            continue;
        };
        let before = captured_index_at(callee, captured, inserted_before);
        let count = middle.len();
        for (offset, frame) in middle.into_iter().enumerate() {
            trace.insert(callee + offset, frame.to_entry());
        }
        inserted_before.extend((0..count).map(|_| before));
        any = true;
    }
    if any {
        inserted_before.sort_unstable();
    }
}

/// [`insert_callback_native_middle_frames`] for a `StackWalker` walk's trace
/// (`trace`, OUTERMOST-first, as `stackwalker::capture_stack_walk_trace`
/// built it): `Class.forName0 (Native Method)` -- and a served
/// `Class.forName`'s Java frames -- between a caller standing at that call and
/// the `<clinit>` / `loadClass` the native ran, as HotSpot's walk lists them
/// (interpreter round i2 wave 55, lane L3; probe `L3W54StackFrameInfoShapes`,
/// rows `native-*`: a walk from a static initializer `Class.forName` runs
/// listed no `forName0` frame, in both modes). The pairs are screened
/// lock-free first, as the throwable capture's are.
pub(in crate::vm::vm_exec) fn insert_callback_native_walk_entries(
    cm: &crate::classloading::ClassManager,
    trace: &mut Vec<StackTraceEntry>,
) {
    use crate::runtime::stackwalker::{
        callback_native_middle_frames, may_need_loader_middle_frames,
        served_load_class_middle_frames, BacktraceFrame,
    };
    // Only a `<clinit>`, `loadClass` or `findClass` activation can be the
    // callee; the entries are wrapped (cloned) only past this name compare.
    if !trace
        .iter()
        .any(|e| matches!(&*e.method_name, "<clinit>" | "loadClass" | "findClass"))
    {
        return;
    }
    let mut frames: Vec<BacktraceFrame> =
        trace.iter().cloned().map(BacktraceFrame::Entry).collect();
    if !frames
        .windows(2)
        .any(|pair| may_need_loader_middle_frames(&pair[0], &pair[1]))
    {
        return;
    }
    let mut resolve =
        |requester: ClassId, name: &str| cm.find_class_by_name_for_class(name, requester);
    let mut inserted = false;
    // Innermost first: an insertion shifts only pairs already judged.
    for callee in (1..frames.len()).rev() {
        let middle = callback_native_middle_frames(
            &cm.class_store,
            &frames[callee - 1],
            &frames[callee],
            &mut resolve,
        )
        .or_else(|| {
            served_load_class_middle_frames(
                &cm.class_store,
                &frames[callee - 1],
                &frames[callee],
                &mut resolve,
            )
        });
        if let Some(middle) = middle {
            for (offset, frame) in middle.into_iter().enumerate() {
                frames.insert(callee + offset, frame);
            }
            inserted = true;
        }
    }
    if inserted {
        *trace = frames.iter().map(BacktraceFrame::to_entry).collect();
    }
}

/// Round 14 wave 3 (lane trace2; T14W3-2 and T14W3-3 of
/// `jit-r14-trace3-proposals-RETIRED-20260930.md`): the `java.lang.Thread.run` frames of a
/// `StackWalker` walk of the current platform thread (`trace`, OUTERMOST-first,
/// as `stackwalker::capture_stack_walk_trace` built it): the middle ones
/// (`insert_thread_run_middle_frames`) and the bottom one
/// (`prepend_thread_run_standin_frame`), which the throwable capture already
/// gets. No depth cap: a walk has none. `CRATONVM_STACK_WALK_THREAD_RUN_FRAMES=0`
/// restores the walk without them; the frame-level switches
/// (`CRATONVM_THROWABLE_THREAD_RUN_FRAME`, `..._MIDDLE_FRAME`) apply too.
/// The switch is read only once a pair or the bottom frame passed its
/// lock-free screen.
///
/// Answers where frames went in (round 14 wave 4, lane trace3): each is an
/// index of `trace` AS PASSED (0 = outermost) that a new frame was inserted
/// before, ascending, so a caller that attributed something to the entries
/// as passed can shift it (`thread_jmx_snapshot`'s locked-monitor depths:
/// `r14w4-trace3-jmx-locked-monitor-depths-after-served-frames-patch-FIXED-20260929.md`).
///
/// Round 15 wave 2 (lane trace2; `r15w1-trace-virtual-as-bound-residual-arms`
/// item 2): a walk of the current VIRTUAL thread (`virtual_thread`) too,
/// unless `stackwalker::virtual_standin_as_bound` is `0` (read with the walk
/// switch, once a screen passed). The bottom rule refuses a virtual thread's
/// class by itself (`thread_class_inheriting_run`).
pub(in crate::vm::vm_exec) fn add_thread_run_walk_entries(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    thread_obj: Option<ObjectRef>,
    trace: &mut Vec<StackTraceEntry>,
    virtual_thread: bool,
) -> Vec<usize> {
    use crate::runtime::stackwalker::{
        may_be_thread_run_lambda_frame, may_be_thread_run_task_frame,
        may_need_thread_run_middle_frame, thread_run_middle_frame, BacktraceFrame,
    };
    // Only a `run` frame can be a task's -- or, round 15 wave 3 (lane trace3),
    // a lambda task's body; the entries are wrapped (cloned) for the
    // frame-level screens only past this name compare.
    let run_frame = |e: &StackTraceEntry| {
        crate::runtime::stackwalker::may_be_thread_run_middle_callee_name(&e.method_name)
    };
    let mut enabled_memo: Option<bool> = None;
    let mut enabled = || {
        *enabled_memo.get_or_insert_with(|| {
            // Round 17 wave 2: virtual threads included unconditionally (HotSpot
            // parity; see `insert_thread_run_middle_frames`).
            let _ = virtual_thread;
            true
        })
    };
    let bottom = match (thread_obj, trace.first()) {
        (Some(thread_obj), Some(first)) if &*first.method_name != "main" => {
            // A task's `run()V`, or a lambda task's implementation method.
            let outermost = BacktraceFrame::Entry(first.clone());
            let screened = may_be_thread_run_task_frame(&outermost)
                || may_be_thread_run_lambda_frame(&outermost);
            if screened && enabled() {
                thread_run_bottom_frame(shared, &cm.class_store, thread_obj, &outermost)
            } else {
                None
            }
        }
        _ => None,
    };
    let mut resolve =
        |requester: ClassId, name: &str| cm.find_class_by_name_for_class(name, requester);
    let mut inserted_before = Vec::new();
    // Innermost first: an insertion shifts only entries already judged, so
    // `callee` is still an index of the trace as passed.
    for callee in (1..trace.len()).rev() {
        if !run_frame(&trace[callee]) {
            continue;
        }
        let caller = BacktraceFrame::Entry(trace[callee - 1].clone());
        let task = BacktraceFrame::Entry(trace[callee].clone());
        if !may_need_thread_run_middle_frame(&caller, &task) || !enabled() {
            continue;
        }
        let entry = thread_run_middle_frame(&cm.class_store, &caller, &task, &mut resolve);
        if let Some(entry) = entry {
            trace.insert(callee, entry);
            inserted_before.push(callee);
        }
    }
    // Round 15 wave 3 (lane trace3): one copy per served `Thread.run` of a
    // nested `Thread` task (`thread_run_bottom_frame`).
    if let Some((frame, copies)) = bottom {
        let entry = frame.to_entry();
        for _ in 0..copies {
            trace.insert(0, entry.clone());
            inserted_before.push(0);
        }
    }
    inserted_before.reverse();
    inserted_before
}

/// Round 14 wave 4 (lane trace3; T4-3 of `jit-r14-trace4-proposals-RETIRED-20260930.md`,
/// `r13w13-trace3-vm-served-jdk-methods-residuals` item 6): another thread's
/// published stack (`trace`, OUTERMOST-first) gets the JDK frames of the
/// census method it is parked in (`stackwalker::parked_thread_standin_frames`),
/// as a throwable raised inside that leaf does. HotSpot's `getStackTrace()`
/// of a thread in `Thread.sleep` ends `Thread.sleepNanos0(Native Method)`.
///
/// The screen is the thread's CURRENT block state, not a tag the blocking
/// native deposits (the proposal's form, whose deposit sites are outside this
/// lane): `WAITING` / `TIMED_WAITING` means the thread is inside the blocking
/// native its last deposit stood at, and that deposit is the trace read here
/// (`thread_stack_trace` publishes afresh only for a thread NOT blocked). A
/// thread that unblocks between the two reads shows where it was parked, as
/// the published trace itself already does. `BLOCKED` (re-acquiring the
/// monitor after `wait`) is left out; a `java.lang.VirtualThread` too (its
/// `sleep` / `wait` run `VirtualThread` code; round 15 wave 2 admits this VM's
/// bound virtual threads, see below). `CRATONVM_THREAD_STACK_PARKED_LEAF_FRAMES=0`
/// turns it off; read only after the block-state screen.
///
/// JIT round 25 (`r25-locked-frame-depth-misses-the-park-frame-FIXED-20261024`):
/// `sample` is the block state (and published wait kind) the reader read
/// WITH the trace, never a later read alone: inside the frame-trace pause for
/// another thread's `ThreadInfo` (W6-F, [`HeldThreadRead`]), and for
/// `Thread.getStackTrace()` / dumps the state BEFORE the pause that made the
/// thread publish when it was parked then (`thread_stack_trace_served`). A
/// thread in a park's running phase (`CRATONVM_PARK_LAZY`) publishes inside
/// that pause while it stands in the park's native, and the pause itself ends
/// the phase, so a state read after it raced the thread out of the park
/// (RUNNABLE between two 1 ms parks, or the blocking prologue) and the trace
/// went out without its `Unsafe.park` frame: `MonitorInfo.getLockedStackDepth()`
/// read one or two less than HotSpot. [`parked_standin_frames_for_thread`]
/// still asks that the innermost entry stand at a census call, which a trace
/// published anywhere else does not. Answers how many rows it appended.
/// `CRATONVM_FRAME_TRACE=1` prints
/// `[PARKED_LEAF] block=<n> kind=<k> top=<class>.<method>@<bci> how=<bci|none> added=<n>`.
pub(in crate::vm::vm_exec) fn append_parked_standin_entries(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    sample: ParkedSample,
    thread_obj: ObjectRef,
    trace: &mut Vec<StackTraceEntry>,
) -> usize {
    let added = if matches!(sample.block, 1 | 3) {
        match trace.last().and_then(|innermost| {
            parked_standin_frames_for_thread(shared, cm, Some(thread_obj), innermost)
        }) {
            Some(extra) => {
                let rows = extra.len();
                trace.extend(extra);
                rows
            }
            None => 0,
        }
    } else {
        0
    };
    if crate::runtime::env_cache::frame_trace() {
        let top = trace.len().saturating_sub(added);
        let top = top.checked_sub(1).and_then(|i| trace.get(i));
        eprintln!(
            "[PARKED_LEAF] block={} kind={:?} top={}.{}@{} how={} added={}",
            sample.block,
            sample.kind,
            top.map_or("-", |e| &*e.class_name),
            top.map_or("-", |e| &*e.method_name),
            top.map_or(i32::MIN, |e| e.byte_code_index),
            if added > 0 { "bci" } else { "none" },
            added,
        );
    }
    added
}

/// The JDK frames above `innermost` (another thread's published innermost
/// entry) of the census method it stands at a call of, for a thread whose
/// state the caller has screened ([`append_parked_standin_entries`], and
/// `gc_and_alloc::publish_excused_native_traces` for a thread a frame-trace
/// pause holds in that native): `stackwalker::parked_thread_standin_frames`,
/// refused for a `java.lang.VirtualThread` and, for this VM's bound virtual
/// threads, for a `LockSupport` chain (see below). `thread_obj` `None` is
/// read as a platform thread. `CRATONVM_THREAD_STACK_PARKED_LEAF_FRAMES=0`
/// answers `None`.
pub(crate) fn parked_standin_frames_for_thread(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    thread_obj: Option<ObjectRef>,
    innermost: &StackTraceEntry,
) -> Option<Vec<StackTraceEntry>> {
    if innermost.byte_code_index < 0 || innermost.class_id.is_none() {
        return None;
    }
    let store = &cm.class_store;
    // Round 15 wave 2 (lane trace2; `r15w1-trace-virtual-as-bound-residual-arms`
    // item 1): a virtual thread on this VM is a `ThreadBuilders$BoundVirtualThread`
    // running the platform `sleep` / `wait` / `join` branches, whose served
    // leaf it is parked in exactly as a platform thread is (a yielded one is
    // `TIMED_WAITING` / `WAITING` with the trace its yield deposited, standing
    // at the served call: `resume_virtual_continuation`). Only a
    // `java.lang.VirtualThread` (never created here: `ContinuationSupport` is
    // unsupported) is refused, or every virtual thread under
    // `CRATONVM_THROWABLE_STANDIN_VIRTUAL_AS_BOUND=0`. `VirtualThread` is
    // `final`, so the name compare is exact.
    let thread_class = thread_obj.map(|obj| shared.mem.heap.class_id_of(obj));
    let virtual_thread = thread_class
        .is_some_and(|c| crate::runtime::stackwalker::is_virtual_thread_class(store, c));
    if virtual_thread
        && (thread_class
            .and_then(|c| store.get(c))
            .is_some_and(|c| &*c.name == "java/lang/VirtualThread")
            || !crate::runtime::stackwalker::virtual_standin_as_bound())
    {
        return None;
    }
    let extra = crate::runtime::stackwalker::parked_thread_standin_frames(
        store,
        innermost,
        &mut |requester: ClassId, name: &str| cm.find_class_by_name_for_class(name, requester),
    )?;
    // Round 15 wave 2 (lane trace2; TR5-5): a `LockSupport` park chain is
    // the platform branch, which a bound virtual thread does not take.
    if virtual_thread && !crate::runtime::stackwalker::parked_frames_fit_virtual_thread(&extra) {
        return None;
    }
    Some(extra)
}

/// W6-F: the block state and published wait kind (`None` with
/// `CRATONVM_PUBLISHED_ALL_WAIT_KINDS=0`, or when not waiting) a published
/// trace was read with, for [`append_parked_standin_entries`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::vm::vm_exec) struct ParkedSample {
    pub(in crate::vm::vm_exec) block: u8,
    pub(in crate::vm::vm_exec) kind: Option<crate::threading::thread_registry::PublishedWaitKind>,
}

impl ParkedSample {
    pub(in crate::vm::vm_exec) fn read(registry: &crate::threading::thread_registry::ThreadRegistry, tid: ThreadId) -> Self {
        let block = registry.java_block_state(tid);
        let kind = if matches!(block, 1 | 3) {
            registry.published_wait_kind_of(tid)
        } else {
            None
        };
        Self { block, kind }
    }
}

/// Interpreter round i1 wave 45, lane L3: when the throwable being captured
/// may be one the innermost reflective call's native raised itself (the
/// `InvocationTargetException` wrapping a target's throw, built by its real
/// constructor inside the call: `stackwalker::reflective_raise_site`), give
/// that call's splice the frames HotSpot lists on top of such a trace -- the
/// accessor at its `throw new <that class>` line, the `catch` arm picked by
/// what the constructor was handed (`reflective_raise_entries`). Whether the
/// call really is on top the capture decides (`build_slots_with_splices`);
/// an ordinary capture under a reflective call pays the screen alone.
/// `CRATONVM_DBG_STTRACE=1` prints `STTRACE_DBG_REFLECT raise throwable=<name>
/// cause=<arm> relined=<bool>` per screened capture.
pub(in crate::vm::vm_exec) fn attach_reflective_raise(
    shared: &SharedVm,
    thread: &mut JvmThread,
    throwable_class: ClassId,
    splices: &mut [crate::runtime::stackwalker::ReflectiveSplice],
) {
    use crate::runtime::stackwalker as sw;
    let Some((call, first)) =
        sw::reflective_raise_site(&thread.frames, &thread.reflective_calls, throwable_class)
    else {
        return;
    };
    // Wave 49, lane L3: a `doPrivileged(PrivilegedAction)` bridge raises
    // nothing itself; a throwable its (compiled) action raised is not its.
    if !sw::reflective_kind_may_raise(call.kind) {
        return;
    }
    // Wave 62, lane L3: while the call's door is in flight, a throwable
    // built above it is its target's, not the accessor's own raise: the
    // frames stay at their calls. Wave 64: read from the door record
    // (`ReflectiveCallRow::door_save`, `JvmThread::served_door_in_flight`).
    if sw::reflective_door_keyed(&call, thread.served_door_in_flight) {
        return;
    }
    let Some(entries) = sw::innermost_splice_entries(splices) else {
        return;
    };
    // The classes of the objects the throwable's constructor was handed
    // (`InvocationTargetException(Throwable target)`: its target).
    // Wave 48, lane L3: with no constructor frame to read (a wrapper built
    // by a stub `<init>`, `--compatible`), the target's throwable the native
    // noted (`note_reflective_raise_target`).
    let handed: Vec<ClassId> = first
        .map(|frame| {
            (1..frame.locals_len())
                .filter_map(|i| match frame.get_local(u16::try_from(i).ok()?) {
                    Value::Object(Some(o)) => Some(shared.mem.heap.class_id_of(o)),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_else(|| call.raised.into_iter().collect());
    let cm = shared.classes.class_manager.read();
    let Some(tclass) = cm.class_store.get(throwable_class) else {
        return;
    };
    let cause = handed
        .iter()
        .map(|&id| sw::reflective_raise_cause(&cm.class_store, id))
        .find(|c| *c != sw::ReflectiveRaiseCause::Other)
        .unwrap_or(sw::ReflectiveRaiseCause::Other);
    // Wave 46, lane L3: a failed argument check the native noted names its
    // own frames (`reflective_check_entries`); anything else, or a check
    // whose frames are not found, the wave-45 `catch` arm.
    let raise = call
        .check
        .and_then(|check| {
            sw::reflective_check_entries(&cm.class_store, call.kind, &entries, &tclass.name, check)
        })
        .or_else(|| {
            // nio21 wave 15 L4: the raise site remembered per thread.
            sw::reflective_raise_entries(
                &cm.class_store,
                call.kind,
                &entries,
                &tclass.name,
                cause,
                Some(&mut thread.throwable_capture_memo),
            )
        });
    if crate::runtime::env_cache::dbg_sttrace() {
        eprintln!(
            "STTRACE_DBG_REFLECT raise throwable={} cause={cause:?} relined={} check={:?}",
            tclass.name,
            raise.is_some(),
            call.check
        );
    }
    drop(cm);
    if let Some(raise) = raise {
        sw::set_reflective_raise(splices, &call, raise);
    }
}

/// `(index, value)` pairs indexing a trace AS CAPTURED (0 = outermost),
/// re-indexed into the trace after `add_thread_run_walk_entries` inserted a
/// frame before each index of `inserted_before` (round 14 wave 4, lane
/// trace3): `index + |{at <= index}|`. Frames appended at the innermost end
/// move nothing.
pub(in crate::vm::vm_exec) fn shift_served_frame_indices<T>(
    pairs: Vec<(usize, T)>,
    inserted_before: &[usize],
) -> Vec<(usize, T)> {
    if inserted_before.is_empty() {
        return pairs;
    }
    pairs
        .into_iter()
        .map(|(index, value)| {
            let shift = inserted_before.iter().filter(|&&at| at <= index).count();
            (index + shift, value)
        })
        .collect()
}


/// The row of the native method thread `tid` is blocked in, appended on top
/// of its published `trace` (innermost last), when it is blocked in a
/// registered native standing in for a JDK `native` method
/// (`ThreadRegistry::blocked_in_native`, a `BlockKind::InNative` region under
/// `CRATONVM_BLOCKED_IN_NATIVE_RUNNABLE`) and the top entry's invoke names
/// that native with a fixed target (`gc_and_alloc::native_entry_invoked_at`,
/// as the wave-58 trace of an excused running native names it). HotSpot
/// lists `sun.nio.ch.Net.accept(Native Method)` on top of a thread blocked in
/// `ServerSocket.accept` (interpreter round i2 wave 59, lane L6;
/// `L6W59BlockedNativeStates.java`). Read here, not at
/// the deposit: nothing on the native's own path. Not when the top entry is
/// already a native's row. `CRATONVM_FRAME_TRACE=1` prints
/// `[BLOCKED_NATIVE_ROW] tid=<n> native=<name>`.
///
/// Also (the i54-L6 retirement) the `synchronized native` a thread is
/// blocked ENTERING: contending for a monitor, with the top entry's invoke
/// selecting a synchronized native whose monitor that is
/// (`gc_and_alloc::synchronized_native_entry_named_by_entry`). HotSpot's
/// native wrapper builds the native's frame before it locks, so it lists
/// `staticNative(Native Method)` on top
/// (`L6K54BlockedSyncNativeAndEntryOrder.java`); this
/// VM's native arms take the monitor before the call. Read here too, never
/// on the call's path.
pub(crate) fn append_blocked_native_entry(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    tid: ThreadId,
    trace: &mut Vec<StackTraceEntry>,
) {
    let Some(top) = trace.last() else {
        return;
    };
    if top.line_number == crate::runtime::stackwalker::LINE_NUMBER_NATIVE {
        return;
    }
    let head = if shared.threads.thread_registry.blocked_in_native(tid) {
        crate::runtime::interpreter::gc_and_alloc::native_entry_named_by_entry(
            shared, cm, top, false,
        )
    } else {
        let contended = shared
            .threads
            .thread_registry
            .jmx_lock_snapshot(tid)
            .and_then(|snapshot| snapshot.0);
        contended.and_then(|contended| {
            crate::runtime::interpreter::gc_and_alloc::synchronized_native_entry_named_by_entry(
                shared, cm, top, contended,
            )
        })
    };
    let Some(head) = head else {
        return;
    };
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[BLOCKED_NATIVE_ROW] tid={} native={}.{}",
            tid.0, head.class_name, head.method_name
        );
    }
    trace.push(head);
}

/// One thread's stack as a thread dump prints it (interpreter round i3 wave
/// 81, lane L6; page `i59-L1-sigquit-terminates-the-vm`): `frames` INNERMOST
/// FIRST, and the monitors each frame locked as `(index into frames,
/// monitor)`, the most recently locked first within a frame.
#[derive(Default)]
pub(crate) struct DumpStack {
    pub(crate) frames: Vec<StackTraceEntry>,
    pub(crate) frame_monitors: Vec<(usize, ObjectRef)>,
}

/// The stack of BLOCKED thread `tid` (its published frame trace) as HotSpot
/// 25's thread dump lists it (interpreter round i3 wave 81, lane L6). The
/// cross-thread `Thread.getStackTrace` reader's steps
/// (`thread_stack_trace_served`'s other-thread arm: lines, the served
/// `Thread.run` frames, the parked leaf natives `sleepNanos0` / `wait0` /
/// `Unsafe.park`, the native a thread is blocked in, overload lines), where
/// the dump read only the trace and the blocked native -- so it printed
/// `Thread.java:-1`, no `sleepNanos0` / `wait0` and no `Thread.run` frames --
/// and three a dump adds:
///
/// * `vm_wait` (RUNNABLE, blocked in a wait inside the VM): the native the
///   top frame stands at an invoke of, on top -- HotSpot's Reference Handler
///   lists `waitForReferencePendingList(Native Method)`;
/// * the hidden lambda proxy frame between a call site and the lambda's
///   implementation (`stackwalker::lambda_proxy_standin_frames`,
///   fail-closed; `VmConfig::standin_lambda_proxy_frames`): a thread dump
///   shows hidden frames;
/// * the frame attribution of the thread's monitors, as published with the
///   trace (`ThreadRegistry::jmx_frame_monitors`), the monitor its `wait()`
///   released included (HotSpot prints `- locked` for it).
///
/// Cold: a dump only.
#[cold]
#[inline(never)]
pub(crate) fn thread_dump_stack(
    shared: &SharedVm,
    tid: ThreadId,
    thread_obj: Option<ObjectRef>,
    vm_wait: bool,
) -> DumpStack {
    let registry = &shared.threads.thread_registry;
    let cm = shared.classes.class_manager.read();
    // W6-F fix 6: the block state read with the trace (`HeldTrace`).
    let HeldTrace {
        mut trace,
        parked: block,
    } = HeldTrace::read(registry, tid);
    crate::runtime::stackwalker::resolve_line_numbers_in_place(&cm.class_store, &mut trace);
    let captured_len = trace.len();
    let rows_listed = trace
        .iter()
        .any(|e| &*e.class_name == "java/lang/Thread" && &*e.method_name == "run");
    let inserted = match thread_obj {
        Some(obj) if !rows_listed => {
            add_thread_run_walk_entries(shared, &cm, Some(obj), &mut trace, false)
        }
        _ => Vec::new(),
    };
    if let Some(obj) = thread_obj {
        append_parked_standin_entries(shared, &cm, block, obj, &mut trace);
    }
    append_blocked_native_entry(shared, &cm, tid, &mut trace);
    if vm_wait {
        let head = trace
            .last()
            .filter(|top| top.line_number != crate::runtime::stackwalker::LINE_NUMBER_NATIVE)
            .and_then(|top| {
                crate::runtime::interpreter::gc_and_alloc::native_entry_named_by_entry(
                    shared, &cm, top, false,
                )
            });
        if let Some(head) = head {
            trace.push(head);
        }
    }
    let unresolved = trace.iter().any(|e| {
        e.line_number == crate::runtime::stackwalker::LINE_NUMBER_UNKNOWN
            && e.method_descriptor.is_none()
    });
    if unresolved
    {
        crate::runtime::stackwalker::resolve_published_overloads_by_callee(
            &cm.class_store,
            &mut trace,
        );
    }
    // Innermost first from here on. An entry whose class declares exactly one
    // method of its name gets that method's descriptor, for the proxy match
    // below (a lambda's implementation method has a unique name).
    let mut frames: Vec<StackTraceEntry> = trace.into_iter().rev().collect();
    for entry in frames.iter_mut().filter(|e| e.method_descriptor.is_none()) {
        let Some(class) = entry.class_id.and_then(|id| cm.get_class(id)) else {
            continue;
        };
        let mut named = class
            .methods
            .iter()
            .filter(|m| *m.name == *entry.method_name);
        if let (Some(only), None) = (named.next(), named.next()) {
            entry.method_descriptor = Some(std::sync::Arc::clone(&only.descriptor));
        }
    }
    // A thread contending for a monitor at a `monitorenter`: HotSpot's
    // template interpreter advances the bcp past the `monitorenter` before it
    // takes the slow path (`TemplateTable::monitorenter`), so its dump lists
    // the top frame at the NEXT instruction's line (MEASURED, wave 81:
    // `L6W81ThreadDumpShape`'s blocked frame at the line after its
    // `synchronized`). The published frame stands at the `monitorenter`.
    let contended = registry
        .jmx_lock_snapshot(tid)
        .is_some_and(|snapshot| snapshot.0.is_some());
    if contended {
        if let Some(top) = frames.first_mut() {
            relocate_contended_monitorenter_line(&cm.class_store, top);
        }
    }
    drop(cm);
    let len = frames.len();
    // Bottom-indexed into the captured trace; the served frames shift them.
    let mut monitors: Vec<(usize, ObjectRef)> = registry
        .jmx_frame_monitors(tid, captured_len)
        .map(|pairs| shift_served_frame_indices(pairs, &inserted))
        .unwrap_or_default()
        .into_iter()
        .filter(|(bottom, _)| *bottom < len)
        .map(|(bottom, obj)| (len - 1 - bottom, obj))
        .collect();
    // Most recently locked first within a frame (the lock stack's order,
    // reversed), frames innermost first.
    monitors.reverse();
    monitors.sort_by_key(|(index, _)| *index);
    // A `Thread.run` the VM serves (`--compatible`) pushes no frame for the
    // `@Hidden` `Thread.runWith` it calls; a thread dump shows hidden frames,
    // as JVMTI and JDWP list it (`stackwalker::served_thread_run_debugger_entries`,
    // `runWith` at its `op.run()`). Put it inside every `Thread.run` that
    // has none (JDK 21+; a JDK whose `Thread` has no `runWith` gets nothing).
    // Interpreter round i3 wave 81, lane L6.
    let missing_run_with: Vec<usize> = (0..frames.len())
        .filter(|&i| {
            let e = &frames[i];
            &*e.class_name == "java/lang/Thread"
                && &*e.method_name == "run"
                && (i == 0 || {
                    let inner = &frames[i - 1];
                    !(&*inner.class_name == "java/lang/Thread" && &*inner.method_name == "runWith")
                })
        })
        .collect();
    if !missing_run_with.is_empty() {
        let run_with = {
            let cm = shared.classes.class_manager.read();
            crate::runtime::stackwalker::served_thread_run_debugger_entries(&cm.class_store)
                .and_then(|entries| entries.into_iter().nth(1))
        };
        if let Some(run_with) = run_with {
            let insertions = missing_run_with
                .into_iter()
                .map(|at| (at, vec![run_with.clone()]))
                .collect();
            let (merged, moved) = insert_dump_frames(frames, insertions);
            frames = merged;
            for (index, _) in &mut monitors {
                *index = moved[*index];
            }
        }
    }
    {
        let sites: Vec<std::sync::Arc<crate::classloading::resolution::LambdaCallSite>> = shared
            .classes
            .lambda_proxies
            .read()
            .values()
            .filter(|site| {
                frames
                    .iter()
                    .any(|f| crate::runtime::stackwalker::lambda_site_stands_below(site, f))
            })
            .cloned()
            .collect();
        let candidates: Vec<crate::runtime::stackwalker::LambdaProxyCandidate> = sites
            .into_iter()
            .filter_map(|site| {
                let id = site.proxy_class_id;
                let (_, host) = crate::runtime::interpreter::lambda_proxy_host(shared, id)?;
                let name: std::sync::Arc<str> =
                    std::sync::Arc::from(format!("{host}$$Lambda/0x{:016x}", id.as_u32()));
                Some(crate::runtime::stackwalker::LambdaProxyCandidate { site, name })
            })
            .collect();
        if !candidates.is_empty() {
            let insertions = {
                let cm = shared.classes.class_manager.read();
                crate::runtime::stackwalker::lambda_proxy_standin_frames(
                    &cm.class_store,
                    &frames,
                    &candidates,
                )
            };
            if !insertions.is_empty() {
                let (merged, moved) = insert_dump_frames(frames, insertions);
                frames = merged;
                for (index, _) in &mut monitors {
                    *index = moved[*index];
                }
            }
        }
    }
    DumpStack {
        frames,
        frame_monitors: monitors,
    }
}

/// A running, started thread whose published trace is still EMPTY after the
/// frame-trace pause (or the dump's one pause) gets up to
/// [`EMPTY_TRACE_RETRIES`] more pauses (interpreter round i3 wave 81, lane
/// L6). A peer that does not reach a poll within the pause's grace is frozen
/// and publishes nothing, and a thread that never blocked has nothing
/// published from before, so `Thread.getAllStackTraces()` / `getStackTrace()`
/// answered `<empty>` for a live thread spinning in Java -- the intermittent
/// `L6W61OneDumpPause` DIFF (`all spin-7: RUNNABLE <empty>`; HotSpot always
/// names the running method). Only an empty answer pays; cold.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn retry_empty_running_trace(ctx: &NativeContextImpl<'_>, tid: ThreadId) {
    let registry = &ctx.shared.threads.thread_registry;
    for _ in 0..EMPTY_TRACE_RETRIES {
        if registry.is_blocked(tid)
            || !registry.is_alive(tid)
            || registry.os_tid_of(tid).is_none()
            || !registry.frame_trace_of(tid).is_empty()
        {
            return;
        }
        if crate::runtime::env_cache::frame_trace() {
            eprintln!("[EMPTY_TRACE_RETRY] tid={}", tid.0);
        }
        if !crate::runtime::interpreter::stw_publish_frame_traces(ctx.shared, ctx.thread.thread_id)
        {
            return;
        }
    }
}

/// [`retry_empty_running_trace`]'s bound.
pub(in crate::vm::vm_exec) const EMPTY_TRACE_RETRIES: usize = 2;

/// gci i23/hs: how long a cross-thread `getStackTrace()` waits for a running
/// target to serve its frame-trace handshake before it takes the pause
/// ([`frame_trace_by_handshake`]). A target that polls -- any interpreted
/// allocation, a pause arrival -- serves in microseconds; one in compiled
/// code without an allocation slow path, in a running native, or in a
/// non-allocating interpreted loop does not poll within it, and pays this
/// once before the pause it always took.
pub(in crate::vm::vm_exec) const HANDSHAKE_TRACE_BUDGET: std::time::Duration = std::time::Duration::from_millis(1);

/// gci i23/hs: the requester's wait slice: how often it re-checks whether a
/// pause was requested (it is not GC-safe while it waits, so a pause waits at
/// most this long for it), whether the target blocked or was dropped.
pub(in crate::vm::vm_exec) const HANDSHAKE_TRACE_SLICE: std::time::Duration = std::time::Duration::from_micros(100);

/// gci i23/hs (step 2 of `common-a-proposal-per-thread-handshakes`, opt-in
/// `CRATONVM_GC_HANDSHAKE_FRAME_TRACE`): ask the running thread `tid` to
/// publish its own live trace (`interpreter::frame_trace_handshake_op`) at
/// its next poll, without stopping anyone else. `true` when the published
/// trace is now current and no pause is needed: the target ran the op, or it
/// is blocked (its deposit is current), or it is gone (a pause would publish
/// nothing for it). `false` when it was not served within
/// [`HANDSHAKE_TRACE_BUDGET`] or a pause was requested meanwhile: the op was
/// withdrawn and the caller takes the frame-trace pause, as before.
/// `CRATONVM_FRAME_TRACE=1` prints one `[HANDSHAKE_TRACE]` line per call.
pub(in crate::vm::vm_exec) fn frame_trace_by_handshake(shared: &SharedVm, requester: ThreadId, tid: ThreadId) -> bool {
    use crate::threading::thread_registry::HandshakeOutcome;
    let t0 = std::time::Instant::now();
    let barrier = &shared.mem.gc_barrier;
    let registry = &shared.threads.thread_registry;
    // gcj j1/cm (P4/ph of `common-a-proposal-per-thread-handshakes`;
    // `CRATONVM_GC_HANDSHAKE_LOOP_KICK`, default ON under this opt-in path):
    // once the op is queued, send every interpreter dispatch loop of this VM
    // to its slow path (`interpreter::loop_top_poll`, which polls the
    // handshake queue), so an interpreted target that allocates nothing
    // serves at its next bytecode. Every other loop takes one cold path and
    // settles (the word is only a trigger). `=0`: no kick, the i23/hs reach.
    let kick_loops = registry.handshake_loop_kick();
    // gcj j2/cm2 (`CRATONVM_GC_HANDSHAKE_TARGET_KICK`, opt-in, Generational
    // only; `gcj-j1cm-proposal-target-only-handshake-kick`): kick only the poll
    // word the target's loops last ran on, when it published one; every loop
    // otherwise. `kicked=` reads 0 (none), 1 (every loop) or 2 (the target's).
    let target_kick = registry.handshake_target_kick() && shared.mem.heap.is_generational();
    // t1 lane E (`CRATONVM_GC_HANDSHAKE_POLL_BIT`, default ON since WARNING
    // t1-user-flip-20261026, `=0` kills; every backend; step 3
    // of `common-a-proposal-per-thread-handshakes`): raise the HANDSHAKE bit of
    // the target's published poll word instead -- its interpreter loops come to
    // their slow path at their next top, and its compiled polls under
    // `CRATONVM_JIT_POLL_THREAD_WORD` at their next back edge. `kicked=3`. The
    // kicks below stay the fallback when the target published nothing.
    //
    // t1 lane E wave 3 (`t1-V2-handshake-poll-bit-stale-carrier-arm-skips-the-kick-fallback`):
    // the arm is taken only when the published word is provably the one the
    // target runs on NOW -- its owner OS tid equals the target's current
    // MOUNTED tid (`ThreadRegistry::mounted_os_tid`, read after the op was
    // queued); a virtual thread remounted on another carrier, or an unmounted
    // one, gets the kicks instead. A remount racing that check, or another VM
    // on the same OS thread lowering the bit, is covered by a second chance:
    // an arm still unserved after the first slice runs the kicks too
    // (`kicked=4`). A prompt target settles in microseconds, well inside the
    // first slice, so the fast path never kicks.
    let poll_bit = registry.handshake_poll_bit();
    let kicked = std::cell::Cell::new(0u8);
    let requester_addr = registry.own_jvm_thread_addr(requester);
    let outcome = registry.handshake_and_wait_kicking_twice(
        tid,
        crate::runtime::interpreter::frame_trace_handshake_op(),
        HANDSHAKE_TRACE_BUDGET,
        HANDSHAKE_TRACE_SLICE,
        || barrier.stw_requested.load(std::sync::atomic::Ordering::Acquire),
        |target| {
            if poll_bit
                && target.arm_published_loop_poll_word_handshake(registry.mounted_os_tid(tid))
            {
                kicked.set(3);
            } else if kick_loops {
                kicked.set(frame_trace_loop_kick(barrier, target, target_kick));
            }
        },
        |target| {
            if kicked.get() == 3 && kick_loops {
                frame_trace_loop_kick(barrier, target, target_kick);
                kicked.set(4);
            }
        },
        || {
            if let Some(addr) = requester_addr {
                // SAFETY: own alive JvmThread on this OS thread.
                let req_thread = unsafe { &*(addr as *const crate::threading::jvm_thread::JvmThread) };
                if req_thread.gc_block_state.handshakes.is_pending() {
                    let _ = crate::threading::gc_barrier::take_this_threads_handshake_arm();
                    req_thread.run_pending_handshakes(shared);
                }
            }
        },
    );
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[HANDSHAKE_TRACE] tid={} target={} outcome={outcome:?} us={} kicked={}",
            requester.0,
            tid.0,
            t0.elapsed().as_micros(),
            kicked.get()
        );
    }
    outcome != HandshakeOutcome::NotServed
}

/// The frame-trace handshake's loop kick (gcj j1/cm, j2/cm2), shared by its
/// first kick and the poll-bit arm's second chance (t1 lane E wave 3): the
/// target's published word alone under `CRATONVM_GC_HANDSHAKE_TARGET_KICK`
/// (Generational), else every dispatch loop of the VM. Answers the
/// `kicked=` value: 2 (the target's) or 1 (every loop).
pub(in crate::vm::vm_exec) fn frame_trace_loop_kick(
    barrier: &crate::threading::gc_barrier::GcBarrier,
    target: &crate::threading::jvm_thread::GcBlockState,
    target_kick: bool,
) -> u8 {
    if target_kick && target.kick_published_loop_poll_word() {
        2
    } else {
        barrier.note_code_moved_on_every_loop();
        1
    }
}




/// `entry`'s line, when it stands at a `monitorenter`, becomes the line of
/// the instruction after it, as HotSpot reports a frame blocked entering a
/// monitor (see `thread_dump_stack`). Needs the entry's descriptor; anything
/// else leaves it as it is (interpreter round i3 wave 81, lane L6).
pub(in crate::vm::vm_exec) fn relocate_contended_monitorenter_line(store: &ClassStore, entry: &mut StackTraceEntry) {
    const MONITORENTER: u8 = 0xc2;
    let (Some(class_id), Some(descriptor), Ok(bci)) = (
        entry.class_id,
        entry.method_descriptor.clone(),
        usize::try_from(entry.byte_code_index),
    ) else {
        return;
    };
    let at_enter = store
        .get(class_id)
        .and_then(|class| {
            class
                .methods
                .iter()
                .find(|m| *m.name == *entry.method_name && *m.descriptor == *descriptor)
        })
        .and_then(|m| m.code())
        .is_some_and(|code| code.code.get(bci) == Some(&MONITORENTER));
    if !at_enter {
        return;
    }
    if let Some(line) = crate::runtime::stackwalker::line_number_for_bci(
        store,
        class_id,
        &entry.method_name,
        &descriptor,
        bci + 1,
    ) {
        entry.line_number = line;
    }
}

/// `frames` (innermost first) with each `(at, entries)` of `insertions`
/// (ascending `at`) put before `frames[at]`, and where each old index went
/// (interpreter round i3 wave 81, lane L6).
pub(in crate::vm::vm_exec) fn insert_dump_frames<T>(frames: Vec<T>, insertions: Vec<(usize, Vec<T>)>) -> (Vec<T>, Vec<usize>) {
    let mut out = Vec::with_capacity(frames.len() + insertions.len());
    let mut moved = Vec::with_capacity(frames.len());
    let mut pending = insertions.into_iter().peekable();
    for (i, frame) in frames.into_iter().enumerate() {
        while let Some((_, entries)) = pending.next_if(|(at, _)| *at <= i) {
            out.extend(entries);
        }
        moved.push(out.len());
        out.push(frame);
    }
    for (_, entries) in pending {
        out.extend(entries);
    }
    (out, moved)
}

/// A Java `Thread`'s priority and daemon flag as its object holds them
/// (`Thread.holder`'s `priority` / `daemon`, or `Thread`'s own fields on a
/// layout without a holder), for a thread dump read from a thread that is no
/// Java thread (interpreter round i3 wave 81, lane L6). `None` for what does
/// not read back: an address that is no heap object (a mirror racing a
/// moving collection, as `read_java_thread_tid` guards), no such field, or a
/// priority outside 1..=10.
#[cold]
#[inline(never)]
pub(crate) fn thread_dump_priority_and_daemon(
    shared: &SharedVm,
    thread_obj: ObjectRef,
) -> (Option<i32>, Option<bool>) {
    let heap = &shared.mem.heap;
    if heap.is_heap_addr(thread_obj.as_ptr() as usize).is_none() {
        return (None, None);
    }
    let class_id = heap.class_id_of(thread_obj);
    let holder = thread_field_slot(shared, class_id, "holder").and_then(|slot| {
        match heap.get_field(thread_obj, slot) {
            Value::Object(Some(h)) if heap.is_heap_addr(h.as_ptr() as usize).is_some() => Some(h),
            _ => None,
        }
    });
    let (obj, slots) = match holder {
        Some(holder) => {
            let holder_class = heap.class_id_of(holder);
            let cm = shared.classes.class_manager.read();
            (
                holder,
                (
                    resolve_field_index_in_hierarchy(holder_class, "priority", &cm.class_store),
                    resolve_field_index_in_hierarchy(holder_class, "daemon", &cm.class_store),
                ),
            )
        }
        None => (
            thread_obj,
            (
                thread_field_slot(shared, class_id, "priority"),
                thread_field_slot(shared, class_id, "daemon"),
            ),
        ),
    };
    let priority = slots.0.and_then(|slot| match heap.get_field(obj, slot) {
        Value::Int(p) if (1..=10).contains(&p) => Some(p),
        _ => None,
    });
    let daemon = slots.1.and_then(|slot| match heap.get_field(obj, slot) {
        Value::Int(d) => Some(d != 0),
        _ => None,
    });
    (priority, daemon)
}

/// `Thread.parkBlocker` of a Java `Thread`, when it holds one (interpreter
/// round i3 wave 81, lane L6): a thread dump's `- parking to wait for`.
#[cold]
#[inline(never)]
pub(crate) fn thread_dump_park_blocker(
    shared: &SharedVm,
    thread_obj: ObjectRef,
) -> Option<ObjectRef> {
    let heap = &shared.mem.heap;
    heap.is_heap_addr(thread_obj.as_ptr() as usize)?;
    let slot = thread_field_slot(shared, heap.class_id_of(thread_obj), "parkBlocker")?;
    match heap.get_field(thread_obj, slot) {
        Value::Object(Some(blocker)) if heap.is_heap_addr(blocker.as_ptr() as usize).is_some() => {
            Some(blocker)
        }
        _ => None,
    }
}

/// The external name of `obj`'s class (`java.lang.ref.ReferenceQueue$Lock`)
/// and its address, for a thread dump's lock line (interpreter round i3 wave
/// 81, lane L6). `None` for an address that is no heap object.
#[cold]
#[inline(never)]
pub(crate) fn thread_dump_monitor(
    shared: &SharedVm,
    obj: ObjectRef,
) -> Option<crate::runtime::serviceability::DumpMonitor> {
    let heap = &shared.mem.heap;
    heap.is_heap_addr(obj.as_ptr() as usize)?;
    let class_id = heap.class_id_of(obj);
    // Interpreter round i5 wave 104, lane L6: HotSpot's
    // `print_locked_object_class_name` names a locked `Class` object's class
    // -- `(a java.lang.Class for Foo)` (a `static synchronized` method's
    // monitor, `synchronized (Foo.class)`) -- where this printed
    // `(a java.lang.Class)`. A primitive mirror (no reverse entry) keeps the
    // short form.
    let mirror_of = super::vm_object::class_id_from_mirror(shared, obj);
    let class_name = {
        let cm = shared.classes.class_manager.read();
        let name = cm
            .get_class(class_id)
            .map(|c| crate::runtime::serviceability::external_class_name(&c.name))
            .unwrap_or_else(|| "java.lang.Object".to_string());
        match mirror_of.and_then(|id| cm.get_class(id)) {
            Some(target) if name == "java.lang.Class" => format!(
                "java.lang.Class for {}",
                crate::runtime::serviceability::external_class_name(&target.name)
            ),
            _ => name,
        }
    };
    Some(crate::runtime::serviceability::DumpMonitor {
        address: obj.as_ptr() as usize as u64,
        class_name,
    })
}

/// The module a thread dump prints before a frame's location (interpreter
/// round i3 wave 81, lane L6), as HotSpot's `print_stack_element` prints the
/// class's `ModuleEntry`: `name@version`, or `name` for a module with no
/// version; `None` for the unnamed module (an application class, a lambda
/// proxy) and for an unknown class. A non-boot layer's module by its
/// identity, as `module_identity_of_class` names it.
pub(crate) fn thread_dump_frame_module(
    cm: &crate::classloading::ClassManager,
    class_id: Option<ClassId>,
) -> Option<String> {
    let class = cm.get_class(class_id?)?;
    let (name, version) =
        match crate::classloading::access_control::layer_module_of(class, &cm.module_registry) {
            Some((loader_ns, name)) => (
                name.to_string(),
                cm.module_registry
                    .layer_module_version(loader_ns, name)
                    .map(str::to_string),
            ),
            None => {
                let name = class.module_name.as_deref().filter(|n| !n.is_empty())?;
                (
                    name.to_string(),
                    cm.module_registry.get(name).and_then(|d| d.version.clone()),
                )
            }
        };
    Some(match version.filter(|v| !v.is_empty()) {
        Some(v) => format!("{name}@{v}"),
        None => name,
    })
}


impl NativeContextImpl<'_> {
    /// Is thread `tid` running a JDK `native` method through the native-call
    /// funnel, as the callback its raised excuse word records says
    /// (interpreter round i2 wave 62, lane L6, `CRATONVM_HANDSHAKE_NATIVE_CALLBACK`;
    /// `docs/feature-designs/real-jdk-mode.md`)?
    /// `Some(true)` when the recorded callback stands in for a JDK `native`
    /// method; `Some(false)` when the thread runs no funnel native; `None`
    /// otherwise -- the switch is off, no callback is recorded, or the
    /// classifier does not confirm one -- and the caller captures the stack,
    /// as before. A scan of the native registry; no pause.
    /// `CRATONVM_FRAME_TRACE=1` prints `[THREAD_INFO_CALLBACK] tid=<n>
    /// native=<answer>`.
    #[cold]
    #[inline(never)]
    pub(in crate::vm::vm_exec) fn funnel_native_by_callback(&self, tid: ThreadId) -> Option<bool> {
        if !self.shared.mem.gc_barrier.excused_native_callback() {
            return None;
        }
        let registry = &self.shared.threads.thread_registry;
        let answer = match registry.running_funnel_callback(tid) {
            // Only a positive answer is taken. The classifier knows the
            // registry's triples of classes the built-in loaders loaded; a
            // `false` may be a stand-in for a Java method (HotSpot: not in
            // native) or a native it cannot name (an application loader's
            // class), and the trace's top row decides those, as before.
            Some(callback)
                if crate::runtime::interpreter::funnel_callback_runs_a_native_method(
                    self.shared,
                    callback,
                ) =>
            {
                Some(true)
            }
            // Raised with no callback recorded, or one the classifier does
            // not confirm: capture, as before.
            _ if registry.running_funnel_native(tid) => None,
            // Not raised: in no funnel native at all.
            _ => Some(false),
        };
        if crate::runtime::env_cache::frame_trace() {
            eprintln!("[THREAD_INFO_CALLBACK] tid={} native={answer:?}", tid.0);
        }
        answer
    }

    /// `NativeContext::thread_jmx_snapshot`; with `frameless` (interpreter
    /// round i2 wave 61, lane L6, `CRATONVM_THREAD_INFO_FRAMELESS`) the
    /// target's stack is not captured -- no frame-trace pause for a running
    /// target -- except when the in-native flag needs its top row (a
    /// RUNNABLE thread inside a funnel native, wave 60). The snapshot then
    /// lists no frames and attributes no monitor to one; only a door that
    /// reports neither asks for it.
    ///
    /// JIT round 25 wave 6, lane W6-F fix 6
    /// (`r25-locked-frame-depth-misses-the-park-frame-FIXED-20261024`): ANOTHER
    /// thread's run state, wait kind, stack and lock book are read at ONE
    /// instant ([`HeldThreadRead`]), inside the frame-trace pause
    /// (`stw_publish_frame_traces_with`), where every mutator is either at its
    /// poll with its live trace just published or blocked with its deposit
    /// and cannot leave the blocked region (`leave_blocked_region_flagged_if`
    /// waits the pause out) -- a lazy park or wait gives up its running phase
    /// for the pause and deposits first. HotSpot's `ThreadInfo` is one
    /// safepoint (`VM_ThreadDump`); before, these were separate reads of a
    /// thread that kept running, and a `parkNanos(1 ms)` / `wait(1)` loop was
    /// reported TIMED_WAITING over a stack without its native, with a locked
    /// depth one short, or with the monitor its `wait` released still locked.
    /// The calling thread reads its own live frames; a JDK-driven
    /// `VirtualThread` (its state is its own field) and a frameless door keep
    /// the separate reads, and so does any snapshot when no pause can be taken
    /// (another owns the world). `CRATONVM_FRAME_TRACE=1` prints
    /// `[JMX_SNAPSHOT] own=<b> held=<b> run_state=<n> frames=<n>`.
    pub(in crate::vm::vm_exec) fn jmx_thread_snapshot(
        &self,
        thread_obj: ObjectRef,
        frameless: bool,
    ) -> Option<cratonvm_native_api::ThreadJmxSnapshot> {
        let registry_tid = resolve_thread_id_from_thread_obj(self.shared, thread_obj)?;
        let own = self.thread.java_thread_obj == Some(thread_obj);
        let mut held = None;
        if !(own
            || frameless
            || super::jdk_continuation::virtual_thread_run_state(self, thread_obj).is_some())
        {
            // W9-1: a held read whose waiting target's trace was not
            // republished in its pause is stale (`held_trace_is_stale`); the
            // next pause finds it blocked with its deposit, or publishes it.
            let registry = &self.shared.threads.thread_registry;
            // W10-7: `CRATONVM_JMX_HELD_READ_RETRY=0` takes one read, as
            // before W9-1 (`held_read_attempts`).
            // W10-7 review: inside a `ThreadInfo` dump, the read its own
            // pauses took (`begin_thread_info_dump`), while still current.
            held = self.take_dump_held_read(registry_tid);
            let attempts = if held.is_some() { 0 } else { held_read_attempts() };
            for attempt in 1..=attempts {
                let seq_before = registry.trace_seq(registry_tid);
                let read = crate::runtime::interpreter::stw_publish_frame_traces_with(
                    self.shared,
                    self.thread.thread_id,
                    || HeldThreadRead::read(self.shared, registry_tid, thread_obj, seq_before),
                );
                let stale = read.as_ref().is_some_and(|r| r.stale);
                // W11-1: a read that is not stale can still be one HotSpot never
                // answers -- waiting, yet no native on top even with the
                // stand-in chain, or monitors held with no attribution for
                // the trace (`held_read_shape_ok`).
                let (shaped, lacks_native) = read
                    .as_ref()
                    .map_or((true, false), |r| self.held_read_shape(r, thread_obj));
                let unshaped = !stale && !shaped;
                held = read;
                if !(stale || unshaped) || attempt == attempts {
                    // W11 residual (`r25-jmx-waiting-sample-rare-residual`): a
                    // waiting read still without its native after the last
                    // attempt is the thread's Java part of the wait, which
                    // HotSpot reports RUNNABLE. Only when retrying (with
                    // `CRATONVM_JMX_HELD_READ_RETRY=0` the one read is
                    // answered as is, as before W9-1).
                    if attempts > 1 && lacks_native {
                        if let Some(r) = held.as_mut() {
                            r.answer_running();
                        }
                        if crate::runtime::env_cache::frame_trace() {
                            eprintln!(
                                "[JMX_SNAPSHOT_RUNNING_PHASE] tid={} attempt={attempt}",
                                registry_tid.0
                            );
                        }
                    }
                    break;
                }
                if crate::runtime::env_cache::frame_trace() {
                    eprintln!(
                        "[JMX_SNAPSHOT_STALE] tid={} attempt={attempt} stale={stale} \
                         unshaped={unshaped}",
                        registry_tid.0
                    );
                }
            }
        }
        if crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[JMX_SNAPSHOT] own={own} held={} run_state={:?} frames={:?}",
                held.is_some(),
                held.as_ref().map(|h| h.run_state),
                held.as_ref().map(|h| h.trace.trace.len()),
            );
        }
        self.jmx_thread_snapshot_from(thread_obj, registry_tid, frameless, held)
    }

    /// W10-7 review: the held read of `tid` that the current `ThreadInfo`
    /// dump's pauses took ([`Self::begin_thread_info_dump`]), taken out of
    /// the dump, or `None` -- outside a dump, for a thread it did not read,
    /// or once a later pause (a collection, another capture's pause) has run,
    /// since its object references are valid only up to that.
    pub(in crate::vm::vm_exec) fn take_dump_held_read(&self, tid: ThreadId) -> Option<HeldThreadRead> {
        if !dump_traces_current(self) {
            return None;
        }
        let mut held = self.thread.gc_block_state.dump_held.lock();
        let at = held.iter().position(|(id, _)| *id == tid.0)?;
        let (_, read) = held.swap_remove(at);
        read.downcast::<HeldThreadRead>().ok().map(|read| *read)
    }

    /// JIT round 25 wave 11, lane W11-1: [`held_read_shape_ok`] of `held`:
    /// whether its trace ends in a native frame, as published or with the
    /// parked stand-in chain a waiting thread's trace takes
    /// (`parked_standin_frames_for_thread`, exactly as `finish_published_trace`
    /// appends it), and whether its held monitors have an attribution for
    /// that trace. Read after the pause (the class manager is not taken
    /// inside it).
    ///
    /// The second answer: the read is a waiting one with no native on top
    /// (the W11 residual's fallback, [`HeldThreadRead::answer_running`]).
    pub(in crate::vm::vm_exec) fn held_read_shape(&self, held: &HeldThreadRead, thread_obj: ObjectRef) -> (bool, bool) {
        let native =
            |e: &StackTraceEntry| e.line_number == crate::runtime::stackwalker::LINE_NUMBER_NATIVE;
        let waiting = matches!(held.trace.parked.block, 1 | 3);
        let top = held.trace.trace.last();
        let top_native = waiting
            && (top.is_some_and(native) || {
                let cm = self.shared.classes.class_manager.read();
                top.and_then(|top| {
                    parked_standin_frames_for_thread(self.shared, &cm, Some(thread_obj), top)
                })
                .is_some_and(|rows| rows.last().is_some_and(native))
            });
        let locked = held.book.as_ref().map_or(0, |book| book.2.len());
        (
            held_read_shape_ok(waiting, top_native, locked, held.frame_monitors.is_some()),
            waiting && !top_native,
        )
    }

    /// [`Self::jmx_thread_snapshot`] from `held` -- the one-instant read of
    /// another thread -- or, without it, from separate reads (the calling
    /// thread, a JDK-driven `VirtualThread`, a frameless door, no pause).
    pub(in crate::vm::vm_exec) fn jmx_thread_snapshot_from(
        &self,
        thread_obj: ObjectRef,
        registry_tid: ThreadId,
        frameless: bool,
        held: Option<HeldThreadRead>,
    ) -> Option<cratonvm_native_api::ThreadJmxSnapshot> {
        let (held_run_state, held_book, held_trace, held_frame_monitors) = match held {
            Some(read) => (
                Some(read.run_state),
                Some(read.book),
                Some(read.trace),
                Some(read.frame_monitors),
            ),
            None => (None, None, None, None),
        };
        let run_state = held_run_state.unwrap_or_else(|| self.thread_run_state(thread_obj));
        let thread_id =
            read_java_thread_tid(self.shared, thread_obj).unwrap_or(registry_tid.0) as i64;
        // Round 15 wave 4 (lane threadvm): `java.lang.Thread`'s own `name`
        // (`thread_field_owner`), not a subclass's `String name` field, is
        // `ThreadInfo.getThreadName()`.
        // Forwarded first, as `get_field_by_name` did.
        let (named, _) = self.shared.mem.heap.load_and_forward_checked(thread_obj);
        let name_slot = {
            let cm = self.shared.classes.class_manager.read();
            let class_id = self.shared.mem.heap.class_id_of(named);
            let owner = thread_field_owner(self.shared, &cm.class_store, class_id);
            resolve_field_index_in_hierarchy(owner, "name", &cm.class_store)
        };
        let thread_name = match name_slot.map(|slot| self.shared.mem.heap.get_field(named, slot)) {
            Some(Value::Object(Some(name))) => crate::vm::vm_object::read_java_string(&self.shared.mem.heap, name),
            _ => None,
        }
        .unwrap_or_else(|| format!("Thread-{thread_id}"));
        let mut thread_status = match run_state {
            1 => 0x0001, // RUNNABLE
            2 => 0x0002, // TERMINATED
            3 => 0x0010, // WAITING
            4 => 0x0400, // BLOCKED_ON_MONITOR_ENTER
            5 => 0x0010, // TIMED_WAITING (approximated as WAITING for this bit-field)
            _ => 0,
        };
        // Interpreter round i2 wave 59, lane L6 (`CRATONVM_BLOCKED_IN_NATIVE_RUNNABLE`):
        // JMM's in-native flag, `ThreadInfo.isInNative()`, for a RUNNABLE
        // thread in native code (HotSpot MEASURED: true for a thread in
        // `ServerSocket.accept`, `L6W59BlockedNativeStates`).
        let registry = &self.shared.threads.thread_registry;
        if thread_status == 0x0001
            && registry.blocked_in_native_runnable()
            && registry.thread_in_native(registry_tid)
        {
            thread_status |= JMM_THREAD_STATE_FLAG_NATIVE;
        }
        // Lock ownership. This used to be `..Default::default()`, so `lock`,
        // `lock_owner_id`, `locked_monitors` and `locked_synchronizers` were
        // ALWAYS empty over the native API even though the registry has
        // tracked all four for some time. Everything downstream that reads
        // them was therefore dead by construction: `ThreadInfo.getLockName()`
        // / `getLockOwnerId()` / `getLockedMonitors()` reported nothing, the
        // `findDeadlockedThreads` detector could never observe an edge (it
        // builds its wait-for graph from `lock_owner_id`), and
        // `isObjectMonitorUsageSupported` / `isSynchronizerUsageSupported`
        // were correct to answer false only because of this gap.
        let (
            contended,
            waiting,
            mut locked_monitors,
            locked_synchronizers,
            blocked_time_ms,
            blocked_count,
            waited_time_ms,
            waited_count,
        ) = held_book
            .unwrap_or_else(|| {
                self.shared
                    .threads
                    .thread_registry
                    .jmx_lock_snapshot(registry_tid)
            })
            .unwrap_or((None, None, Vec::new(), Vec::new(), 0, 0, 0, 0));
        // W6-F fix 7: another thread's book without the monitors it does not
        // hold now (the held read filtered its own inside the pause).
        if held_run_state.is_none() && self.thread.java_thread_obj != Some(thread_obj) {
            drop_monitors_not_held(
                &self.shared.threads.monitors,
                registry_tid,
                &mut locked_monitors,
            );
        }
        // A thread blocks on the monitor it is contending for, or waits on the
        // one it called `wait()` on — never both, so prefer the contended one.
        let lock = contended.or(waiting);
        // The native CountDownLatch compatibility implementation waits on the
        // public latch monitor instead of materialising the JDK-private AQS
        // synchronizer. Preserve that monitor's identity hash, but report the
        // logical synchronizer class required by the JMM/ThreadInfo contract.
        // A contended monitor always wins over a wait snapshot and must retain
        // its physical class name.
        //
        // Only where that stand-in is registered (interpreter round i3 wave
        // 71, lane L1): under the `JdkCountDownLatch` row (the default over a
        // real JDK) and under `--real-jdk` the JDK's `await` parks with its
        // `Sync` as the blocker and never waits on the latch's monitor, so a
        // wait on that monitor is the program's own `synchronized (latch) {
        // latch.wait(); }`, which HotSpot names `CountDownLatch`. One
        // registry lookup, on the cold `ThreadInfo` path.
        let lock_class_name = if contended.is_none() {
            waiting.and_then(|object| {
                (self
                    .class_name_arc_of_id(self.class_id_of_object(object))
                    .as_deref()
                    == Some("java/util/concurrent/CountDownLatch")
                    && self
                        .shared
                        .natives
                        .native_methods
                        .kind_of("java/util/concurrent/CountDownLatch", "await", "()V")
                        .is_some())
                .then(|| "java/util/concurrent/CountDownLatch$Sync".to_string())
            })
        } else {
            None
        };
        let (lock_owner_id, lock_owner_name) = match lock {
            Some(obj) => match self.shared.threads.monitors.current_owner(obj) {
                // A thread never reports itself as the owner it is waiting on:
                // that is the `wait()` case, where the monitor is released.
                Some(owner) if owner != registry_tid => (
                    self.shared
                        .threads
                        .thread_registry
                        .java_tid_of(owner)
                        .map_or(-1, |tid| tid as i64),
                    self.shared.threads.thread_registry.thread_name(owner),
                ),
                _ => (-1, None),
            },
            None => (-1, None),
        };
        // Round 14 wave 4 (lane trace3): with the length of the trace as
        // captured and where the served `Thread.run` frames went in.
        // Wave 61 (lane L6): a frameless snapshot captures the stack only
        // for the in-native question below (a funnel native: rare at any
        // instant, and excused by the frame-trace pause, so no wait for it).
        //
        // Wave 62 (lane L6, `CRATONVM_HANDSHAKE_NATIVE_CALLBACK`): with the
        // callback the funnel recorded for the target's raised excuse word,
        // the question is answered from the callback (does it stand in for a
        // JDK `native` method, `funnel_callback_runs_a_native_method`), and a
        // frameless snapshot captures nothing at all.
        let funnel_candidate = thread_status == 0x0001
            && registry.blocked_in_native_runnable()
            && self.thread.java_thread_obj != Some(thread_obj);
        let by_callback = if frameless && funnel_candidate {
            self.funnel_native_by_callback(registry_tid)
        } else {
            None
        };
        if by_callback == Some(true) {
            thread_status |= JMM_THREAD_STATE_FLAG_NATIVE;
        }
        let want_trace = !frameless
            || (funnel_candidate
                && by_callback.is_none()
                && registry.running_funnel_native(registry_tid));
        let (
            stack_trace,
            captured_len,
            inserted_before,
            standin_rows,
            appended_rows,
            thread_state_at_trace,
        ) = if want_trace {
            // W6-F fix 6: the held read's trace, finished like any published
            // one, with the state read at its instant; otherwise the served
            // read and the state it knows (`thread_stack_trace_served`).
            let (mut trace, captured, mut inserted, (standin_rows, appended, state_at_trace)) =
                match held_trace {
                    Some(trace) => {
                        let (trace, captured, inserted, standin_rows, appended) =
                            finish_published_trace(self, thread_obj, registry_tid, trace);
                        (
                            trace,
                            captured,
                            inserted,
                            (standin_rows, appended, Some(run_state)),
                        )
                    }
                    None => thread_stack_trace_served(self, thread_obj, true),
                };
            // JIT round 19 wave 11, lane gc11
            // (`CRATONVM_JMX_THREAD_INFO_HIDDEN_RUN_FRAMES`, default ON): the
            // hidden `Thread.runWith` / lambda-proxy frames a `ThreadInfo`
            // stack lists and `Thread.getStackTrace` does not.
            add_thread_info_hidden_run_entries(
                self.shared,
                thread_obj,
                &mut trace,
                captured,
                &mut inserted,
            );
            (
                trace,
                captured,
                inserted,
                standin_rows,
                appended,
                state_at_trace,
            )
        } else {
            if crate::runtime::env_cache::frame_trace() {
                eprintln!("[THREAD_INFO_FRAMELESS] tid={} no capture", registry_tid.0);
            }
            (Vec::new(), 0, Vec::new(), 0, 0, None)
        };
        // Interpreter round i2 wave 60, lane L6: a thread RUNNING a `native`
        // method (not blocked in one) is in native too -- HotSpot MEASURED
        // `isInNative()` true for a thread inside `Deflater.deflateBytesBytes`
        // (`L6W60RunningNativeIsInNative.java`). The
        // trace names the native on top (the frame-trace pause lists an
        // excused native so) and the thread is still in a funnel native; not
        // the calling thread, which is in this JMX native itself.
        if thread_status == 0x0001
            && registry.blocked_in_native_runnable()
            && self.thread.java_thread_obj != Some(thread_obj)
            && stack_trace.last().is_some_and(|top| {
                top.line_number == crate::runtime::stackwalker::LINE_NUMBER_NATIVE
            })
            && registry.running_funnel_native(registry_tid)
        {
            thread_status |= JMM_THREAD_STATE_FLAG_NATIVE;
        }
        // Which frame locked each monitor (interpreter round i1 wave 24, lane
        // L7; proposal `i23-L7-proposal-per-frame-locked-monitors` stage 1).
        // The calling thread reads its live frames, when the trace is theirs
        // one to one (no compiled frames interleaved); another thread's
        // attribution is what it published with its frame trace at its last
        // blocking deposit, valid only for that same trace and lock stack.
        //
        // JIT round 19 proposal wave 6, lane sync6 (SYNCDOOR618-5,
        // `CRATONVM_JMX_COMPILED_FRAME_MONITOR_DEPTHS`, default ON): with
        // compiled frames interleaved, the calling thread's attribution is
        // laid out over the same walk and checked entry by entry; it already
        // indexes the trace as captured.
        let frame_monitors = if self.thread.java_thread_obj == Some(thread_obj) {
            if captured_len == self.thread.frames.len() {
                Some(crate::runtime::interpreter::held_monitors::frame_locked_monitors(
                    &self.thread.frames,
                ))
            } else if crate::runtime::interpreter::held_monitors::compiled_frame_monitor_depths_enabled()
            {
                crate::runtime::interpreter::held_monitors::frame_locked_monitors_with_compiled(
                    self.shared,
                    &self.thread.frames,
                    &stack_trace,
                    captured_len,
                    &inserted_before,
                )
            } else {
                None
            }
        } else if let Some((keys, live)) = held_frame_monitors
            .is_none()
            .then(|| self.running_locks_at_trace(registry_tid, captured_len))
            .flatten()
        {
            // JIT round 19 wave 12, lane gc12 (`CRATONVM_JMX_RUNNING_LOCKS_AT_TRACE`):
            // a RUNNING thread's monitors as of its trace (those it still
            // holds), and the attribution resolved against that lock stack.
            let pairs = self
                .shared
                .threads
                .thread_registry
                .jmx_frame_monitors_against(registry_tid, captured_len, &keys);
            locked_monitors = live;
            pairs
        } else {
            let registry = &self.shared.threads.thread_registry;
            // JIT round 25 (`r25-locked-frame-depth-misses-the-park-frame-FIXED-20261024`):
            // the trace read and the attribution read are two instants. A
            // trace a frame-trace pause published for the thread in its park
            // (with the stand-in rows on top, `publish_excused_native_traces`)
            // is often followed, before this read, by the thread's own
            // deposit, whose attribution describes the same frames without
            // those rows. An attribution indexes from the BOTTOM, so one made
            // for the trace minus its top stand-in rows is the attribution of
            // this trace too; without it every monitor read depth 0.
            // Each candidate strip (`standin_rows`, a bit per row count),
            // fewest rows first: a real JDK frame below a stand-in row can
            // match a census chain too (`--real-jdk`'s interpreted
            // `parkNanos` under `P.m`), and the shorter strip is then the
            // thread's own trace. And the other way round: the thread's own
            // trace (no rows; this read appended them) beside the pause's
            // attribution, published for the trace WITH them -- the two can
            // be written concurrently, the thread's deposit running inside
            // the very native the pause publishes it in.
            //
            // W6-F fix 6: for the held read, the attribution read inside the
            // same pause as its trace and lock book (`HeldThreadRead`); the
            // strips are then only its fallback.
            match held_frame_monitors {
                Some(held) => held,
                None => registry.jmx_frame_monitors(registry_tid, captured_len),
            }
            .or_else(|| {
                (1..captured_len)
                    .take_while(|&k| k < u32::BITS as usize)
                    .filter(|&k| standin_rows & (1 << k) != 0)
                    .find_map(|k| registry.jmx_trace_monitors(registry_tid, captured_len - k))
            })
            .or_else(|| {
                (appended_rows > 0).then(|| {
                    registry.jmx_trace_monitors(registry_tid, captured_len + appended_rows)
                })?
            })
        };
        if crate::runtime::env_cache::frame_trace() {
            // Round 19 wave 9 (lane gc9): which attribution the snapshot got.
            eprintln!(
                "[JMX_MONITOR_DEPTHS] tid={} self={} captured={captured_len} frames={} trace={} \
                 locked={:x?} attribution={:x?}",
                registry_tid.0,
                self.thread.java_thread_obj == Some(thread_obj),
                self.thread.frames.len(),
                stack_trace.len(),
                locked_monitors
                    .iter()
                    .map(|o| o.as_ptr() as usize)
                    .collect::<Vec<_>>(),
                frame_monitors.as_ref().map(|p| p
                    .iter()
                    .map(|&(i, o)| (i, o.as_ptr() as usize))
                    .collect::<Vec<_>>()),
            );
        }
        // The served frames shift the captured entries' indices (0 = bottom);
        // `attribute_locked_monitors` below takes the final length.
        let frame_monitors =
            frame_monitors.map(|pairs| shift_served_frame_indices(pairs, &inserted_before));
        let (locked_monitors, locked_monitor_depths) =
            crate::runtime::interpreter::held_monitors::attribute_locked_monitors(
                stack_trace.len(),
                locked_monitors,
                frame_monitors,
                waiting,
            );
        Some(cratonvm_native_api::ThreadJmxSnapshot {
            thread_object: Some(thread_obj),
            thread_id,
            thread_name,
            thread_status,
            stack_trace,
            lock,
            lock_class_name,
            lock_owner_id,
            lock_owner_name,
            locked_monitors,
            locked_monitor_depths,
            locked_synchronizers,
            blocked_time_ms,
            blocked_count,
            waited_time_ms,
            waited_count,
            thread_state_at_trace,
            ..Default::default()
        })
    }
}

/// W6-F fix 6: a thread's JMX lock book, as `ThreadRegistry::jmx_lock_snapshot`
/// answers it.
pub(in crate::vm::vm_exec) type JmxLockBook = (
    Option<ObjectRef>,
    Option<ObjectRef>,
    Vec<ObjectRef>,
    Vec<ObjectRef>,
    i64,
    i64,
    i64,
    i64,
);

/// W6-F fix 6: everything another thread's `ThreadInfo` is built from, read
/// at one instant -- inside the frame-trace pause, by
/// `NativeContextImpl::jmx_thread_snapshot` -- from registry state alone (no
/// class-manager lock, no allocation: a peer the pause holds may own either).
pub(in crate::vm::vm_exec) struct HeldThreadRead {
    /// The `thread_run_state` code ([`registry_run_state`]).
    pub(in crate::vm::vm_exec) run_state: u8,
    /// The published trace and the block state it was published under.
    pub(in crate::vm::vm_exec) trace: HeldTrace,
    pub(in crate::vm::vm_exec) book: Option<JmxLockBook>,
    /// The attribution published with `trace`, for its lock stack.
    pub(in crate::vm::vm_exec) frame_monitors: Option<Vec<(usize, ObjectRef)>>,
    /// W9-1: the trace does not describe this instant
    /// ([`held_trace_is_stale`]).
    pub(in crate::vm::vm_exec) stale: bool,
}

/// W9-1: how many held reads [`NativeContextImpl::jmx_thread_snapshot`]
/// takes of a target whose trace was stale ([`held_trace_is_stale`]).
pub(in crate::vm::vm_exec) const HELD_READ_ATTEMPTS: u32 = 3;

/// JIT round 25 wave 10, lane W10-7
/// (`docs/feature-designs/jit-ir-optimizations.md`):
/// the held reads one `ThreadInfo` may take. `CRATONVM_JMX_HELD_READ_RETRY`,
/// **default ON** ([`HELD_READ_ATTEMPTS`]); `=0` is the kill switch (one read,
/// the pre-W9-1 cost: every attempt is a frame-trace pause, so a dump of N
/// lazily parked threads paid up to 3N of them). Read per snapshot; no
/// `static`.
pub(in crate::vm::vm_exec) fn held_read_attempts() -> u32 {
    {
        HELD_READ_ATTEMPTS
    }
}

/// JIT round 25 wave 9, lane W9-1
/// (`docs/feature-designs/real-jdk-mode.md`): is a
/// held read's trace one the thread published BEFORE the pause, though the
/// thread is not blocked? A waiting (1) / timed-waiting (3) thread that is
/// not in the GC-blocked region is in a lazy park's or wait's running phase
/// (`CRATONVM_PARK_LAZY`, `CRATONVM_MONITOR_WAIT_LAZY_PARK`): it publishes its
/// state but deposits nothing, so its standing trace is whatever was
/// published last -- a pause's live publish in its loop, an older park. The
/// pause either publishes it (an excused native), makes it deposit (the
/// phase ends for the pause), or publishes nothing for it (a busy JIT chain,
/// whose own deposit then lands during the pause, after the read). The last
/// case left the read pairing TIMED_WAITING with a stale trace (1 park
/// sample in 12 runs on host c18). `trace_seq` counts every trace write, so
/// an unchanged count since before the pause names that case. A blocked
/// thread's deposit is current whatever the count says.
/// JIT round 25 wave 11, lane W11-1
/// (`docs/feature-designs/jit-ir-optimizations.md`):
/// may a held read of a thread whose block state is WAITING / TIMED_WAITING
/// (`waiting`) be answered as is? HotSpot's one safepoint never lists a
/// waiting thread without the native it waits in on top (`top_native`, after
/// the stand-in chain), nor its monitors without the frames that locked them
/// (`locked` monitors, `attributed`: an attribution published for this very
/// trace). A read that is not stale ([`held_trace_is_stale`]) can still miss
/// either: the trace was republished during the pause by the thread's own
/// deposit (a busy JIT chain, whose excused publication the pause skipped),
/// which writes the trace first and its attribution after, so the read held
/// the new trace beside the old attribution; or a pause-published trace of a
/// thread that left its park between the count read and the pause. Such a
/// read is taken again ([`HELD_READ_ATTEMPTS`]). A thread that is not
/// waiting is answered as is.
pub(in crate::vm::vm_exec) fn held_read_shape_ok(waiting: bool, top_native: bool, locked: usize, attributed: bool) -> bool {
    !waiting || (top_native && (locked == 0 || attributed))
}

pub(in crate::vm::vm_exec) fn held_trace_is_stale(
    block: u8,
    in_blocked_region: bool,
    seq_before: Option<u64>,
    seq_now: Option<u64>,
) -> bool {
    matches!(block, 1 | 3) && !in_blocked_region && seq_before.is_some() && seq_before == seq_now
}

impl HeldThreadRead {
    pub(in crate::vm::vm_exec) fn read(
        shared: &SharedVm,
        tid: ThreadId,
        thread_obj: ObjectRef,
        seq_before: Option<u64>,
    ) -> Self {
        let registry = &shared.threads.thread_registry;
        let trace = HeldTrace::read(registry, tid);
        let stale = held_trace_is_stale(
            trace.parked.block,
            registry.is_blocked(tid),
            seq_before,
            registry.trace_seq(tid),
        );
        let frame_monitors = registry.jmx_frame_monitors(tid, trace.trace.len());
        let mut book = registry.jmx_lock_snapshot(tid);
        if let Some(book) = book.as_mut() {
            drop_monitors_not_held(&shared.threads.monitors, tid, &mut book.2);
        }
        // W11 residual: the state reported is the one the trace was read
        // with. A thread the pause excuses (a lazy park's or wait's running
        // phase) is not stopped, so a second `java_block_state` read could
        // see it enter or leave its wait between the two.
        let block = trace.parked.block;
        Self {
            run_state: registry_run_state_for_block(registry, tid, thread_obj, block),
            book,
            frame_monitors,
            trace,
            stale,
        }
    }

    /// W11 residual: answer this read as the RUNNABLE thread HotSpot sees in
    /// a `sleep` / `wait` / park's Java part, before the native it waits in
    /// (its JVM state turns TIMED_WAITING / WAITING only inside that native).
    /// For a waiting read whose trace has no native on top even after the
    /// last attempt: the thread was in its lazy wait's running phase.
    pub(in crate::vm::vm_exec) fn answer_running(&mut self) {
        self.run_state = 1;
        self.trace.parked = ParkedSample { block: 0, kind: None };
        if let Some(book) = self.book.as_mut() {
            book.1 = None;
        }
    }
}

/// W6-F fix 7: drop from `locked` (thread `tid`'s JMX lock book) every
/// monitor the thread does not hold now: an INFLATED monitor whose owner is
/// another thread or none (`MonitorTable::inflated_not_owned_by`). That is
/// the one its `Object.wait` released, which the lock book keeps naming
/// while it waits; HotSpot's `getLockedMonitors()` never lists it. The
/// `waiting` exclusion of `attribute_locked_monitors` keyed on the JMX
/// waiting-monitor slot alone, and missed a waiter the frame-trace pause holds
/// excused in its wait's running phase (`[JMX_MONITOR_DEPTHS] locked=[M]
/// attribution=Some([(2, M)])` on 124-377 of 2000 `R25WaitingSampleShape`
/// wait samples, host c14), wait kind recorded or not. Inflated ownership is
/// exact and is not keyed on any wait-kind tag; a thin or compiled lock is
/// kept as before (its owner is not read here), and so is one whose
/// ownership cannot be read without blocking.
pub(in crate::vm::vm_exec) fn drop_monitors_not_held(
    monitors: &crate::threading::monitor::MonitorTable,
    tid: ThreadId,
    locked: &mut Vec<ObjectRef>,
) {
    locked.retain(|&monitor| monitors.inflated_not_owned_by(monitor, tid) != Some(true));
}

/// W6-F fix 6: `thread_run_state` of a registered platform thread from the
/// registry alone (no heap read), for [`HeldThreadRead`]: RUNNABLE (1) for a
/// `DestroyJavaVM` mirror, the blocking state (3 WAITING, 4 BLOCKED, 5
/// TIMED_WAITING, else 1) for a live one, TERMINATED (2) otherwise.
pub(in crate::vm::vm_exec) fn registry_run_state(
    registry: &crate::threading::thread_registry::ThreadRegistry,
    tid: ThreadId,
    thread_obj: ObjectRef,
) -> u8 {
    registry_run_state_for_block(registry, tid, thread_obj, registry.java_block_state(tid))
}

/// [`registry_run_state`] with the thread's `java_block_state` already read
/// (`block`): a held read answers the state its trace was read with.
pub(in crate::vm::vm_exec) fn registry_run_state_for_block(
    registry: &crate::threading::thread_registry::ThreadRegistry,
    tid: ThreadId,
    thread_obj: ObjectRef,
    block: u8,
) -> u8 {
    let alive = registry.is_alive(tid);
    if alive && registry.java_side_ended(tid) && !registry.java_side_ended_for(tid, thread_obj) {
        return 1;
    }
    if !alive || registry.java_side_ended(tid) {
        return 2;
    }
    match block {
        1 => 3,
        2 => 4,
        3 => 5,
        _ => 1,
    }
}

/// W6-F fix 6: another thread's published trace (no lines yet) and the
/// block state and wait kind it was read with, in one read.
pub(in crate::vm::vm_exec) struct HeldTrace {
    pub(in crate::vm::vm_exec) trace: Vec<StackTraceEntry>,
    pub(in crate::vm::vm_exec) parked: ParkedSample,
}

impl HeldTrace {
    pub(in crate::vm::vm_exec) fn read(registry: &crate::threading::thread_registry::ThreadRegistry, tid: ThreadId) -> Self {
        let trace = registry.frame_trace_of(tid);
        Self {
            trace,
            parked: ParkedSample::read(registry, tid),
        }
    }
}

/// `CRATONVM_JMX_RUNNING_LOCKS_AT_TRACE`, **default ON** (JIT round 19
/// proposal wave 12, lane gc12; a HotSpot-parity fix, `=0` is the kill
/// switch; page
/// `r19-a-sampled-callers-trace-shows-a-synchronized-callees-monitor-before-the-callee-frame-20261007`).
pub(in crate::vm::vm_exec) fn running_locks_at_trace_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JMX_RUNNING_LOCKS_AT_TRACE")
}

impl NativeContextImpl<'_> {
    /// JIT round 19 wave 12, lane gc12 (`CRATONVM_JMX_RUNNING_LOCKS_AT_TRACE`):
    /// the monitors ANOTHER, running thread held at the instant its trace was
    /// published by a frame-trace pause (`ThreadRegistry::record_trace_locks`),
    /// or `None` -- keep the snapshot's own lock-stack read. Answers the
    /// record's whole lock stack (to resolve the published attribution's slots
    /// against; compared, never dereferenced) and the part of it the thread
    /// STILL holds (what is reported: a monitor held at the trace's instant,
    /// and kept alive by the lock-stack root now).
    ///
    /// `jmx_thread_snapshot` reads the lock stack BEFORE it asks the running
    /// target to publish its trace, so the two were taken at different
    /// instants. For a thread calling a caller-held synchronized callee in a
    /// loop (`R19Gc9CalleeHolds` spin shapes) the read found the callee's
    /// monitor held while the trace was published at the CALLER's poll,
    /// between two calls: no frame of that trace held it, so the builder put
    /// it at the innermost frame -- `driveStatic@1`, where HotSpot, reading
    /// both at one safepoint, never lists the caller. The record pairs the
    /// trace with its own instant. Used only while it still describes the
    /// trace read (`trace_len == captured`), no pause but the publishing one
    /// has ended and no collection moved anything since
    /// (`TraceLockRecord::current_at`), and the target is not blocked now (a
    /// blocked thread's deposit is newer than any pause record, and its own
    /// lock-stack read is current).
    pub(in crate::vm::vm_exec) fn running_locks_at_trace(
        &self,
        tid: crate::ThreadId,
        captured: usize,
    ) -> Option<(Vec<ObjectRef>, Vec<ObjectRef>)> {
        let registry = &self.shared.threads.thread_registry;
        if captured == 0 || registry.is_blocked(tid) || !running_locks_at_trace_enabled() {
            return None;
        }
        // W6-F fix 4: the record of the trace published NOW, so the locks
        // (and their depths) are read with the same trace as the stack -- a
        // deposit since would pair the deposit's stack with the live
        // publish's lock stack (`ThreadInfo` depth 0 on `Unsafe.park`).
        let record = registry.live_trace_locks(tid)?;
        let barrier = &self.shared.mem.gc_barrier;
        let gen_now = barrier
            .gc_generation
            .load(std::sync::atomic::Ordering::Acquire);
        let current =
            record.trace_len == captured && record.current_at(gen_now, barrier.moving_generation());
        if crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[JMX_LOCKS_AT_TRACE] tid={} captured={captured} record_len={} gen={}/{gen_now} \
                 keys={} used={current}",
                tid.0,
                record.trace_len,
                record.gen,
                record.keys.len()
            );
        }
        if !current {
            return None;
        }
        // SAFETY (construction only): an `ObjectRef` made from an address is
        // dereferenced by nothing here; `keys` is only compared. `live` keeps
        // only the addresses the CURRENT lock stack holds, which no moving
        // collection has rewritten since the record (`current_at`), so each
        // names the same object, kept alive by that lock-stack root.
        let keys: Vec<ObjectRef> = record
            .keys
            .iter()
            .map(|&k| unsafe { ObjectRef::from_raw(k as *mut u8) })
            .collect();
        let now = registry
            .jmx_lock_snapshot(tid)
            .map(|snapshot| snapshot.2)
            .unwrap_or_default();
        let live = keys.iter().copied().filter(|k| now.contains(k)).collect();
        Some((keys, live))
    }
}

/// nio21 wave 3, lane L4 (page
/// `r21-aqs-owner-index-is-a-native-call-per-lock-FIXED-20261008.md`):
/// `NativeContext::owned_synchronizer_census` -- HotSpot's
/// `ConcurrentLocksDump::dump_at_safepoint`. In a non-moving pause (the one the
/// JVMTI heap walk and the HPROF dump take), every live instance of
/// `java/util/concurrent/locks/AbstractOwnableSynchronizer` or a subclass whose
/// `exclusiveOwnerThread` is non-null, as `(owner, synchronizer)` in heap
/// order (`find_instances_at_safepoint` + `build_map`'s order). The cost is the
/// dump's, never a lock's.
///
/// `None` when the class is not loaded (no synchronizer can exist: callers
/// treat it as "keep the snapshot's list", which is then empty too) or no
/// pause could be had after a bounded retry (another stop-the-world held the
/// world each time; walking unpaused could read a torn heap). The pairs are
/// raw references: valid until the caller's next allocation or safepoint.
///
/// gci i8/hd (`gci-i3hd-heap-walks-and-diagnostics-review-findings-FIXED-20261017`
/// item 8): the caller is the mutator `thread`, and a refused request means
/// another stop-the-world owns the world -- one that counts this thread and
/// waits for it. The retry used to sleep 1 ms in Rust code, neither arrived
/// nor blocked, so each retry held the other pause up by 1 ms (50 ms in all)
/// and the census then answered `None` anyway. Each refusal now waits one
/// GC-safe slice ([`wait_out_declined_pause_slice`]): the other pause counts
/// the thread and finishes, and the next request usually wins. Bounded by
/// [`CENSUS_PAUSE_RETRY_LIMIT`]. A collection may run during a slice, so
/// nothing raw is held across it (the class id only).
pub(crate) fn owned_synchronizer_census_in(
    shared: &SharedVm,
    thread: &mut JvmThread,
) -> Option<Vec<(ObjectRef, ObjectRef)>> {
    let aos = shared
        .classes
        .class_manager
        .read()
        .find_unique_class_by_name("java/util/concurrent/locks/AbstractOwnableSynchronizer")?;
    let initiator = thread.thread_id;
    let deadline = std::time::Instant::now() + CENSUS_PAUSE_RETRY_LIMIT;
    let pause = loop {
        if let Some(pause) = crate::runtime::interpreter::NonMovingPause::request(shared, initiator) {
            break pause;
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        wait_out_declined_pause_slice(shared, thread);
    };
    let out = owned_synchronizer_walk(shared, aos);
    drop(pause);
    Some(out)
}

/// zt t28/mf: [`owned_synchronizer_census_in`] for a caller that is no
/// mutator (the `SIGQUIT` dispatcher, `jcmd Thread.print -l`): the pause is
/// requested as [`crate::runtime::interpreter::PAUSE_OUTSIDER`], retried
/// every millisecond within [`CENSUS_PAUSE_RETRY_LIMIT`], and each row is
/// rendered under it -- `(owner's Thread.threadId(), "<0x..> (a <class>)")`,
/// HotSpot's `print_locks_on` text -- so no reference outlives the pause.
pub(crate) fn owned_synchronizer_lines_outside(shared: &SharedVm) -> Option<Vec<(u64, String)>> {
    let aos = shared
        .classes
        .class_manager
        .read()
        .find_unique_class_by_name("java/util/concurrent/locks/AbstractOwnableSynchronizer")?;
    let deadline = std::time::Instant::now() + CENSUS_PAUSE_RETRY_LIMIT;
    let pause = loop {
        if let Some(pause) = crate::runtime::interpreter::NonMovingPause::request(
            shared,
            crate::runtime::interpreter::PAUSE_OUTSIDER,
        ) {
            break pause;
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    let lines = owned_synchronizer_walk(shared, aos)
        .into_iter()
        .filter_map(|(owner, synchronizer)| {
            let tid = read_java_thread_tid(shared, owner)?;
            let monitor = thread_dump_monitor(shared, synchronizer)?;
            Some((tid, format!("<0x{:016x}> (a {})", monitor.address, monitor.class_name)))
        })
        .collect();
    drop(pause);
    Some(lines)
}

/// The walk of [`owned_synchronizer_census_in`], under the caller's pause:
/// every object of a subclass of `aos` whose `exclusiveOwnerThread` is set,
/// as `(owner, synchronizer)`, in heap order.
pub(in crate::vm::vm_exec) fn owned_synchronizer_walk(shared: &SharedVm, aos: ClassId) -> Vec<(ObjectRef, ObjectRef)> {
    // The same population `jvmti::native_env::heap::heap_objects` walks: no
    // reserved TLAB tail, no humongous filler.
    let tails = shared.threads.thread_registry.collect_reserved_tlab_tails();
    let heap = &shared.mem.heap;
    let cm = shared.classes.class_manager.read();
    // Per class: `Some(slot)` of `exclusiveOwnerThread` for a synchronizer
    // class, `None` for every other class. A handful of classes per dump.
    let mut slot_of: std::collections::HashMap<u32, Option<usize>> =
        std::collections::HashMap::new();
    let mut out = Vec::new();
    for (ptr, _size) in heap.walk_objects() {
        let addr = ptr as usize;
        if tails
            .iter()
            .any(|&(start, end)| addr >= start && addr < end)
        {
            continue;
        }
        // SAFETY: `walk_objects` yields live object starts, and the pause
        // holds, so nothing moves or is freed under the walk.
        let obj = unsafe { ObjectRef::from_raw(ptr) };
        if heap.get_header(obj).kind() != ObjectKind::Object {
            continue;
        }
        let class_id = heap.class_id_of(obj);
        let slot = *slot_of.entry(class_id.as_u32()).or_insert_with(|| {
            if class_id == aos || cm.is_subclass_of(class_id, aos) {
                resolve_field_index_in_hierarchy(class_id, "exclusiveOwnerThread", &cm.class_store)
            } else {
                None
            }
        });
        let Some(slot) = slot else {
            continue;
        };
        if let Value::Object(Some(owner)) = heap.get_field(obj, slot) {
            out.push((owner, obj));
        }
    }
    drop(cm);
    out
}

/// gci i8/hd: how long [`owned_synchronizer_census_in`] keeps asking for its
/// pause while other stop-the-world pauses own the world (each refusal waits
/// a GC-safe slice, so the time is spent letting those pauses finish).
pub(in crate::vm::vm_exec) const CENSUS_PAUSE_RETRY_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

/// gci i8/hd (`gci-i3hd-heap-walks-and-diagnostics-review-findings-FIXED-20261017`
/// item 8): one 1 ms wait of the mutator `thread` between two refused
/// requests of a non-moving pause, GC-safe, so the pause that refused it --
/// which may be waiting for this very thread -- counts it and goes on.
///
/// A thread already GC-safe (`in_blocked_region` up: a JNI native's C code in
/// native, a blocking region) owes no pause anything: it sleeps. Any other
/// thread waits inside a blocked region, as `jvmti::native_env::gc_safe`
/// does: it deposits its roots, arrives at a pause already in progress
/// (`begin_blocking_region_with_state`), and on the way out waits out the
/// pause, folds what it moved into its frames and native pins, and runs again
/// (`end_blocking_region`). RUNNABLE while waiting under
/// `CRATONVM_VM_INTERNAL_WAITS_RUNNABLE` (HotSpot waits for a VM operation
/// inside the VM), WAITING otherwise, as that function.
pub(in crate::vm::vm_exec) fn wait_out_declined_pause_slice(shared: &SharedVm, thread: &mut JvmThread) {
    const SLICE: std::time::Duration = std::time::Duration::from_millis(1);
    if thread
        .gc_block_state
        .in_blocked_region
        .load(std::sync::atomic::Ordering::Acquire)
    {
        std::thread::sleep(SLICE);
        return;
    }
    let mut ctx = NativeContextImpl { shared, thread };
    if shared.threads.thread_registry.vm_internal_waits_runnable() {
        ctx.begin_vm_wait_region();
    } else {
        ctx.begin_blocking_region();
    }
    std::thread::sleep(SLICE);
    ctx.end_blocking_region();
}


/// Within a dump whose one frame-trace pause (`begin_thread_dump`) is still
/// the latest pause, so every running thread's published trace is of that
/// instant (interpreter round i2 wave 61, lane L6)?
pub(in crate::vm::vm_exec) fn dump_traces_current(ctx: &NativeContextImpl<'_>) -> bool {
    use std::sync::atomic::Ordering;
    let mark = ctx
        .thread
        .gc_block_state
        .dump_traces_gen
        .load(Ordering::Relaxed);
    mark != 0
        && mark
            == ctx
                .shared
                .mem
                .gc_barrier
                .gc_generation
                .load(Ordering::Acquire)
}

/// `thread_stack_trace`, plus the length of the trace as captured /
/// published (before the served frames) and the indices of that trace a
/// served `Thread.run` frame was inserted before
/// (`add_thread_run_walk_entries`; the parked leaf frames are appended at
/// the innermost end and move no index), for `thread_jmx_snapshot`'s
/// locked-monitor attribution (round 14 wave 4, lane trace3;
/// `r14w4-trace3-jmx-locked-monitor-depths-after-served-frames-patch-FIXED-20260929.md`).
///
/// Interpreter round i5 wave 104, lane L3
/// (`i103-L3-clinit-trigger-throwable-traces-lack-served-door-frames` residue
/// (b)): the current thread's listing inside a `<clinit>` a served door runs
/// lists the door's class-init chain below it, as its throwables and walks do
/// (`insert_served_init_chain_listing_entries`); `hidden` (a thread dump or
/// `ThreadInfo`, which list hidden frames on HotSpot) adds the hidden
/// `VarHandleGuards` frame too.
///
/// JIT round 25: the fourth answer is, for another thread, how many rows on
/// top of its PUBLISHED trace are the stand-in chain of the census method
/// the row below them stands at a call of (`published_standin_rows`, a bit
/// per row count), how many such rows this read appended
/// (`append_parked_standin_entries`), and the thread's state at the instant
/// the trace describes when it is known
/// (`ThreadJmxSnapshot::thread_state_at_trace`); `(0, 0, None)` for the
/// current thread.
pub(in crate::vm::vm_exec) fn thread_stack_trace_served(
    ctx: &NativeContextImpl<'_>,
    thread_obj: ObjectRef,
    hidden: bool,
) -> (
    Vec<StackTraceEntry>,
    usize,
    Vec<usize>,
    (usize, usize, Option<u8>),
) {
    // The current thread: walk its LIVE frames (full trace with line numbers).
    if ctx.thread.java_thread_obj == Some(thread_obj) {
        // Wave 104: the guard's class, loaded before the class-manager lock
        // (as `capture_stack_walk_trace_with` loads it).
        let init_chain = ctx.thread.served_native_raise.is_some();
        if init_chain
            && hidden
            && mark_stack_has_var_handle_chain(ctx.thread.served_native_raise.as_deref())
        {
            let _ = ctx
                .shared
                .load_class_concurrent("java/lang/invoke/VarHandleGuards");
        }
        let cm = ctx.shared.classes.class_manager.read();
        let mut trace =
            crate::runtime::stackwalker::capture_full_trace(&cm.class_store, &ctx.thread.frames);
        // Round 15 wave 3 (lane trace4; TR2W2-2): a thread inside served
        // `Thread.run` calls lists their frames from its rows, with its
        // reflective calls' frames, and no rule-built `Thread.run`. Both go in
        // as served frames (`inserted`), so the locked-monitor attribution
        // keeps reading the trace as captured. A trace that is not one entry
        // per slot keeps the path below.
        if !ctx.thread.served_thread_run.is_empty() {
            let rows = ctx.thread.served_thread_run.splices_unmemoized(|| {
                crate::runtime::stackwalker::served_thread_run_entry(&cm.class_store)
            });
            if !rows.is_empty() {
                let mut splices = if ctx.thread.reflective_calls.is_empty() {
                    Vec::new()
                } else {
                    let mut named: crate::runtime::stackwalker::ReflectiveFramesNamed =
                        Default::default();
                    crate::runtime::stackwalker::reflective_splices(
                        &mut named,
                        &ctx.thread.reflective_calls,
                        &mut |kind: cratonvm_native_api::ReflectiveCallKind| {
                            crate::runtime::stackwalker::reflective_entries(&cm.class_store, kind)
                        },
                    )
                };
                crate::runtime::stackwalker::merge_served_thread_run_splices(&mut splices, rows);
                let captured_len = trace.len();
                if let Some(mut inserted) = crate::runtime::stackwalker::splice_frames_reporting(
                    &ctx.thread.frames,
                    &mut trace,
                    &splices,
                ) {
                    insert_callback_native_listing_entries(
                        &cm,
                        &mut trace,
                        captured_len,
                        &mut inserted,
                    );
                    if init_chain {
                        insert_served_init_chain_listing_entries(
                            &cm,
                            &*ctx.thread,
                            &mut trace,
                            captured_len,
                            &mut inserted,
                            hidden,
                        );
                    }
                    return (trace, captured_len, inserted, (0, 0, None));
                }
            }
        }
        // Interpreter round i1 wave 43, lane L3: a reflective call's JDK
        // frames, as HotSpot's `Thread.getStackTrace` lists them.
        if !ctx.thread.reflective_calls.is_empty() {
            // `&self`: named afresh, not kept on the thread (a rare call).
            let mut named: crate::runtime::stackwalker::ReflectiveFramesNamed = Default::default();
            let splices = crate::runtime::stackwalker::reflective_splices(
                &mut named,
                &ctx.thread.reflective_calls,
                &mut |kind: cratonvm_native_api::ReflectiveCallKind| {
                    crate::runtime::stackwalker::reflective_entries(&cm.class_store, kind)
                },
            );
            crate::runtime::stackwalker::splice_reflective_frames(
                &ctx.thread.frames,
                &mut trace,
                &splices,
            );
        }
        let captured_len = trace.len();
        // Round 14 wave 3 (lane trace2): the `Thread.run` frames the
        // VM-served `Thread.run` never pushed. Round 15 wave 2 (lane trace2):
        // on a virtual thread too (see the callee).
        let virtual_thread = matches!(ctx.thread.kind, crate::threading::ThreadKind::Virtual);
        let mut inserted = add_thread_run_walk_entries(
            ctx.shared,
            &cm,
            Some(thread_obj),
            &mut trace,
            virtual_thread,
        );
        // Wave 104 hand-back (`L3W104ClinitTriggerListings`, `--compatible`
        // `Field.*` rows): the `(Native Method)` frame of a native that ran a
        // `<clinit>` (`Unsafe.ensureClassInitialized0`, `Class.forName0`), as
        // the walk lists it (`insert_callback_native_walk_entries`).
        insert_callback_native_listing_entries(&cm, &mut trace, captured_len, &mut inserted);
        if init_chain {
            insert_served_init_chain_listing_entries(
                &cm,
                &*ctx.thread,
                &mut trace,
                captured_len,
                &mut inserted,
                hidden,
            );
        }
        return (trace, captured_len, inserted, (0, 0, None));
    }
    // Another thread: return its last-published frame snapshot — for a
    // parked thread this is the blocking call site.
    let tid = resolve_thread_id_from_thread_obj(ctx.shared, thread_obj);
    let Some(tid) = tid else {
        return (Vec::new(), 0, Vec::new(), (0, 0, None));
    };
    // A RUNNING target has published nothing worth reading.
    //
    // The deposit points are the BLOCKING ones, so the snapshot below
    // answers "where is this thread parked" and nothing else. Ask about a
    // thread that is not parked and the answer is an empty array (it never
    // blocked) or the call site where it blocked LAST — a confident wrong
    // answer. Measured against HotSpot on a spin loop through three named
    // methods, sampled every 2 ms: HotSpot named the running method on all
    // ~840 samples; this returned `<empty>` on 1471 of 1495 and the running
    // method on none. Every in-process sampling profiler, thread dump and
    // hang diagnostic that inspects another thread was reading that.
    //
    // So ask the target to publish, by taking the pause that makes it —
    // another thread cannot walk `JvmThread::frames`, which its own thread
    // owns. Skipped when the target is parked: that is both the common case
    // for a thread dump and the one whose deposit is ALREADY current, so the
    // pause would stop the world to re-derive a stack we already have.
    // Skipped too when another STW owns the world
    // (`stw_publish_frame_traces` returns false), where this read degrades
    // to exactly the behaviour it had before.
    // Interpreter round i2 wave 61, lane L6 (`CRATONVM_THREAD_DUMP_ONE_PAUSE`):
    // not within a dump whose one pause already made it publish
    // (`NativeContext::begin_thread_dump`).
    // gci i23/hs (`CRATONVM_GC_HANDSHAKE_FRAME_TRACE`, opt-in; step 2 of
    // `common-a-proposal-per-thread-handshakes`): ask the target alone first,
    // through a handshake it serves at its next poll; the pause only when it
    // was not served (`frame_trace_by_handshake`).
    // JIT round 25: the block state before the pause below, for the parked
    // leaf frames of a trace the pause makes the thread publish
    // (`append_parked_standin_entries`) -- or, within a dump whose one pause
    // already ran, the state the dump read before it (`dump_parked`).
    let registry = &ctx.shared.threads.thread_registry;
    let in_dump = dump_traces_current(ctx);
    let blocked_at_start = registry.is_blocked(tid);
    let start_state = if in_dump {
        ctx.thread
            .gc_block_state
            .dump_parked
            .lock()
            .iter()
            .find(|&&(t, _)| t == tid.0)
            .map_or(0, |&(_, s)| s)
    } else {
        registry.java_block_state(tid)
    };
    let parked_at_start = matches!(start_state, 1 | 3);
    // Did this capture's own pause or handshake (or the dump's one pause)
    // make the running target publish the trace read below?
    let mut fresh = in_dump && !blocked_at_start;
    // JIT round 25: not the handshake for a thread in a wait's or park's
    // running phase (WAITING / TIMED_WAITING, not GC-blocked). It serves the
    // op at its next poll, which comes only once the park or wait has ENDED,
    // so its trace showed the thread leaving `LockSupport.parkNanos` (no
    // `Unsafe.park`) where HotSpot's handshake, run for a thread in native,
    // lists it inside the park. The pause publishes it where it stands (in
    // its native, or at the blocking deposit the pause makes it take).
    if !blocked_at_start && !in_dump {
        fresh = (ctx.shared.threads.thread_registry.handshake_frame_trace()
            && !parked_at_start
            && frame_trace_by_handshake(ctx.shared, ctx.thread.thread_id, tid))
            || crate::runtime::interpreter::stw_publish_frame_traces(
                ctx.shared,
                ctx.thread.thread_id,
            );
    }
    retry_empty_running_trace(ctx, tid);
    // CR-CLO-1 (`cross-owner-closeout.md` §6).
    //
    // Two stale comments used to sit here. The first claimed line numbers
    // were resolved "now that we hold the ClassStore" — and then called the
    // UNRESOLVED `frame_trace_of`, every entry of which carries
    // `line_number == -1` (the deposit path is `capture_frames_no_lines`,
    // which is lock-free by design and must stay that way). The second
    // claimed the published entry "doesn't carry the ClassId/descriptor
    // needed to resolve source lines"; it has carried `class_id` for some
    // time, and the eager path now also carries `method_index`. Net effect
    // of believing them: every cross-thread thread dump printed line -1.
    //
    // `frame_trace_of_resolved` is `frame_trace_of` followed by
    // `stackwalker::resolve_line_numbers_in_place`, run AFTER the registry
    // lock is dropped (holding L5 across a `ClassStore` walk would invert
    // the usual order) and against the returned copy, so the published
    // snapshot stays exactly as deposited for the next reader. The resolver
    // is fail-closed: it fills an unknown with the line an eager capture
    // would have produced or leaves -1, never writes a wrong line, and
    // never touches an entry that already has one — including the -2 native
    // sentinel. The `class_manager.read()` is the same borrow the
    // current-thread arm above already takes.
    // W6-F fix 6: the trace with the block state and wait kind read with it;
    // the state before the pause decides for a thread parked then.
    let held = HeldTrace::read(registry, tid);
    let parked = if parked_at_start {
        ParkedSample {
            block: start_state,
            kind: held.parked.kind,
        }
    } else {
        held.parked
    };
    let published_innermost_bci = held.trace.last().map(|e| e.byte_code_index);
    if crate::runtime::env_cache::frame_trace() {
        // JIT round 25: which published trace the reader got, and the two
        // block-state reads the parked leaf frames are decided by.
        eprintln!(
            "[XTHREAD_TRACE] tid={} parked_at_start={parked_at_start} state_now={} len={} innermost={:?}",
            tid.0,
            held.parked.block,
            held.trace.len(),
            held.trace
                .last()
                .map(|e| format!("{}.{}@{}", e.class_name, e.method_name, e.byte_code_index)),
        );
    }
    let (trace, captured_len, inserted, standin_rows, appended) = finish_published_trace(
        ctx,
        thread_obj,
        tid,
        HeldTrace {
            trace: held.trace,
            parked,
        },
    );
    // The state of the instant the trace describes (HotSpot reads both at one
    // safepoint): parked, for a trace standing in a census native; RUNNABLE,
    // for one this capture made a running thread publish at a Java poll. A
    // thread BLOCKED entering a monitor, a trace in another native, or one
    // this capture did not make: unknown (the door asks the thread).
    let state_at_trace = if appended > 0 || standin_rows != 0 {
        match parked.block {
            1 => Some(3),
            3 => Some(5),
            _ => None,
        }
    } else if fresh
        && start_state != 2
        && registry.java_block_state(tid) != 2
        && published_innermost_bci.is_some_and(|bci| bci >= 0)
    {
        Some(1)
    } else {
        None
    };
    (
        trace,
        captured_len,
        inserted,
        (standin_rows, appended, state_at_trace),
    )
}

/// W6-F fix 6: another thread's published trace `held` as a stack listing:
/// lines resolved, the `Thread.run` frames the served `Thread.run` never
/// pushed, the native it is parked or blocked in ([`append_parked_standin_entries`],
/// screened by the block state read WITH the trace), overload lines. With
/// the captured length and the served frames' insertion points, as
/// [`thread_stack_trace_served`] answers them.
///
/// JIT round 25: with, as `thread_stack_trace_served`'s fourth answer has
/// them, the stand-in rows on top of the PUBLISHED trace
/// ([`published_standin_rows`], a bit per row count) and the rows this read
/// appended.
pub(in crate::vm::vm_exec) fn finish_published_trace(
    ctx: &NativeContextImpl<'_>,
    thread_obj: ObjectRef,
    tid: ThreadId,
    held: HeldTrace,
) -> (Vec<StackTraceEntry>, usize, Vec<usize>, usize, usize) {
    let HeldTrace { mut trace, parked } = held;
    let cm = ctx.shared.classes.class_manager.read();
    crate::runtime::stackwalker::resolve_line_numbers_in_place(&cm.class_store, &mut trace);
    // Round 14 wave 3 (lane trace2): the bottom `Thread.run` frame (a
    // virtual thread's object is refused by class inside). Round 15 wave 6
    // (lane trace6; TR4W3-2): a published trace that lists a
    // `java.lang.Thread.run` entry was spliced from the thread's served rows,
    // which list every one: the rules must not add theirs.
    let captured_len = trace.len();
    let standin_rows = published_standin_rows(ctx.shared, &cm, thread_obj, &trace);
    let rows_listed = trace
        .iter()
        .any(|e| &*e.class_name == "java/lang/Thread" && &*e.method_name == "run");
    let inserted = if rows_listed {
        Vec::new()
    } else {
        add_thread_run_walk_entries(ctx.shared, &cm, Some(thread_obj), &mut trace, false)
    };
    // Round 14 wave 4 (lane trace3; T4-3): `Thread.sleepNanos0` /
    // `Object.wait0` / `Unsafe.park` and the JDK frames above them, for a
    // thread parked in a `sleep` / `wait` / `join` / park.
    let appended = append_parked_standin_entries(ctx.shared, &cm, parked, thread_obj, &mut trace);
    // Interpreter round i2 wave 59, lane L6: the native a thread is blocked
    // in, on top, as HotSpot lists a thread in `_thread_in_native`.
    append_blocked_native_entry(ctx.shared, &cm, tid, &mut trace);
    // Round 14 wave 4 (lane trace3): an overloaded method's entry gets its
    // line from the call it stands at. `CRATONVM_THREAD_STACK_CALLEE_LINES=0`
    // leaves them at `UNKNOWN`.
    let unresolved = trace.iter().any(|e| {
        e.line_number == crate::runtime::stackwalker::LINE_NUMBER_UNKNOWN
            && e.method_descriptor.is_none()
    });
    if unresolved
    {
        crate::runtime::stackwalker::resolve_published_overloads_by_callee(
            &cm.class_store,
            &mut trace,
        );
    }
    (trace, captured_len, inserted, standin_rows, appended)
}

/// JIT round 25 (`r25-locked-frame-depth-misses-the-park-frame-FIXED-20261024`): which row
/// counts `k` on top of `published` (another thread's published trace,
/// OUTERMOST-first) are the census stand-in chain of the row below them
/// (`parked_standin_frames_for_thread`, compared by class and method), as
/// `gc_and_alloc::publish_excused_native_traces` lists a thread held in such
/// a native: bit `k` set for each. More than one can match (a real JDK frame
/// is a chain row too), so the caller tries them in turn. 0 when none do.
pub(in crate::vm::vm_exec) fn published_standin_rows(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    thread_obj: ObjectRef,
    published: &[StackTraceEntry],
) -> usize {
    // The census chains are short (`Unsafe.park` over `LockSupport.parkNanos`,
    // `Object.wait0` over `Object.wait`, the `sleep` / `join` hops).
    const MAX_ROWS: usize = 4;
    let n = published.len();
    let mut rows = 0usize;
    for k in 1..=MAX_ROWS.min(n.saturating_sub(1)) {
        let below = &published[n - k - 1];
        let Some(chain) = parked_standin_frames_for_thread(shared, cm, Some(thread_obj), below)
        else {
            continue;
        };
        let same = chain.len() == k
            && chain.iter().zip(&published[n - k..]).all(|(c, p)| {
                c.class_name == p.class_name && c.method_name == p.method_name
            });
        if same {
            rows |= 1 << k;
        }
    }
    rows
}


pub(in crate::vm::vm_exec) const CALLER_TAIL_OFF: u8 = 0;
pub(in crate::vm::vm_exec) const CALLER_TAIL_ON: u8 = 1;
pub(in crate::vm::vm_exec) const CALLER_TAIL_VERIFY: u8 = 2;

/// Number of bounded caller-walk answers `CRATONVM_CALLER_TAIL=verify` found
/// equal to the whole capture's tail (a mismatch prints `[CALLER-TAIL-MISMATCH]`
/// and answers with the whole capture instead).
pub(in crate::vm::vm_exec) static CALLER_TAIL_VERIFIED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `CRATONVM_CALLER_TAIL`: `0`/`off` reads the caller off the whole-stack
/// capture as before, `verify` also runs that capture and compares, anything
/// else (the default) is the bounded walk. See
/// [`crate::runtime::stackwalker::capture_tail_trace`].
/// Append `line` to the file named by `CRATONVM_CALLER_TAIL_LOG`, if set: a
/// suite that runs vectors as separate processes with their stderr discarded
/// can still count what `CRATONVM_CALLER_TAIL=verify` compared.
pub(in crate::vm::vm_exec) fn caller_tail_log(line: &str) {
    use std::io::Write;
    if let Ok(path) = Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

pub fn caller_tail_verified() -> u64 {
    CALLER_TAIL_VERIFIED.load(std::sync::atomic::Ordering::Relaxed)
}

pub(in crate::vm::vm_exec) fn caller_tail_mode() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        match Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .as_deref()
            .map(str::trim)
        {
            Ok("0") | Ok("off") | Ok("false") => CALLER_TAIL_OFF,
            Ok("verify") => CALLER_TAIL_VERIFY,
            _ => CALLER_TAIL_ON,
        }
    })
}

impl NativeContextImpl<'_> {
    /// JIT round 21 wave 3, lane walk (WALK21-9 / WALK21-10): does
    /// [`Self::capture_stack_walk_trace_with`] splice nothing into this
    /// thread's walk capture right now -- no reflective call's frames, no
    /// served `Thread.run` rows, no served-init chain mark (which also gates
    /// its `VarHandleGuards` load)? Then that capture is the raw capture plus
    /// its pair rules only.
    pub(in crate::vm::vm_exec) fn stack_walk_capture_is_splice_free(&self) -> bool {
        self.thread.reflective_calls.is_empty()
            && self.thread.served_thread_run.is_empty()
            && self.thread.served_native_raise.is_none()
    }

    /// `NativeExceptionAccess::capture_stack_walk_trace`, and with `deferred`
    /// (JIT round 19 wave 2, lane throws2, WALK17-3 step 1)
    /// `capture_stack_walk_trace_deferred`: the same capture with its lines
    /// deferred (`stackwalker::capture_stack_walk_trace_deferred`), then the
    /// same splices and stand-in entries.
    pub(in crate::vm::vm_exec) fn capture_stack_walk_trace_with(&mut self, deferred: bool) -> Vec<StackTraceEntry> {
        // Interpreter round i5 wave 102, lane L3: a walk under a static
        // VarHandle's served initialization lists the hidden
        // `VarHandleGuards` frame (`insert_served_init_chain_walk_entries`),
        // whose class the capture, holding the class-manager lock, cannot
        // load; HotSpot has it loaded for the call.
        // Wave 104: any mark of the thread's mark stack.
        if self.thread.served_native_raise.is_some()
            && mark_stack_has_var_handle_chain(self.thread.served_native_raise.as_deref())
        {
            let _ = self
                .shared
                .load_class_concurrent("java/lang/invoke/VarHandleGuards");
        }
        // Interpreter round i1 wave 39, lane L3: old activations get no line.
        let cm = self.shared.classes.class_manager.read();
        let mut trace = if deferred {
            crate::runtime::stackwalker::capture_stack_walk_trace_deferred(&cm, &self.thread.frames)
        } else {
            crate::runtime::stackwalker::capture_stack_walk_trace(&cm, &self.thread.frames)
        };
        // Round 15 wave 3 (lane trace4; TR2W2-2): a thread inside served
        // `Thread.run` calls lists their frames from its rows, together with
        // its reflective calls' frames, and no rule-built `Thread.run`. A
        // trace that is not one entry per slot keeps the path below.
        if !self.thread.served_thread_run.is_empty() {
            let thread = &mut *self.thread;
            let rows = thread
                .served_thread_run
                .splices(|| crate::runtime::stackwalker::served_thread_run_entry(&cm.class_store));
            if !rows.is_empty() {
                let mut splices = if thread.reflective_calls.is_empty() {
                    Vec::new()
                } else {
                    crate::runtime::stackwalker::reflective_splices(
                        &mut thread.reflective_frames_named,
                        &thread.reflective_calls,
                        &mut |kind: cratonvm_native_api::ReflectiveCallKind| {
                            crate::runtime::stackwalker::reflective_entries(&cm.class_store, kind)
                        },
                    )
                };
                crate::runtime::stackwalker::merge_served_thread_run_splices(&mut splices, rows);
                if crate::runtime::stackwalker::splice_frames_reporting(
                    &thread.frames,
                    &mut trace,
                    &splices,
                )
                .is_some()
                {
                    return trace;
                }
            }
        }
        // Wave 43, lane L3: a reflective call's JDK frames (the walkers hide
        // them unless the walker shows reflection frames).
        if !self.thread.reflective_calls.is_empty() {
            let thread = &mut *self.thread;
            let splices = crate::runtime::stackwalker::reflective_splices(
                &mut thread.reflective_frames_named,
                &thread.reflective_calls,
                &mut |kind: cratonvm_native_api::ReflectiveCallKind| {
                    crate::runtime::stackwalker::reflective_entries(&cm.class_store, kind)
                },
            );
            crate::runtime::stackwalker::splice_reflective_frames(
                &thread.frames,
                &mut trace,
                &splices,
            );
        }
        // Round 14 wave 3 (lane trace2): the `Thread.run` frames the VM-served
        // `Thread.run` never pushed, as the throwable capture has them.
        // Round 15 wave 2 (lane trace2): on a virtual thread too (see the
        // callee).
        let virtual_thread = matches!(self.thread.kind, crate::threading::ThreadKind::Virtual);
        add_thread_run_walk_entries(
            self.shared,
            &cm,
            self.thread.java_thread_obj,
            &mut trace,
            virtual_thread,
        );
        insert_callback_native_walk_entries(&cm, &mut trace);
        insert_served_init_chain_walk_entries(&cm, &*self.thread, &mut trace);
        trace
    }
}

impl<'a> NativeContextImpl<'a> {
    /// zs s5/nd5 (`ClassRealm::layer_module_pins`): on ZGC, for a module of
    /// a USER loader namespace whose `loader` field names its loader, a JNI
    /// weak handle of the module (the `layer_module_packages` value) and a
    /// pin row `(loader weak, module weak)`; `None` = the permanent strong
    /// root as before (a built-in namespace,
    /// `CRATONVM_ZGC_LAYER_MODULE_LOADER_OWNED=0`). Sweeps the rows of dead
    /// modules first when the list reached its prune mark.
    ///
    /// G1 and Generational take the same rows since the r23 G1 near-full
    /// proxy-define page (`CRATONVM_LAYER_MODULE_LOADER_OWNED_EVERY_GC`,
    /// default ON, `=0` restores their permanent strong root): the strong root
    /// kept every `Proxy.getProxyClass(new ClassLoader() {}, ..)` loader, its
    /// `jdk.proxyN` module and its classes for good there, 3.9 KB a define on
    /// `G1epProxyDefineCost proxy` (HotSpot and ZGC 0), until the
    /// `Zs3Nd3UpcallMints proxy -Xmx64m` heap was full. The pin row and the
    /// JNI weak-global sweep the module rides on are collector-neutral: the
    /// row goes through `metadata_pin` under the loader-metadata licence, as
    /// the `defined-packages` row already does on every collector.
    pub(in crate::vm::vm_exec) fn layer_module_loader_owned(&mut self, loader_ns: u32, module: ObjectRef) -> Option<usize> {
        if loader_ns < cratonvm_types::ClassLoaderId::NATIVE_FIRST_USER_DEFINED
            || !layer_module_loader_owned_here(self.shared)
            || !layer_module_loader_owned_enabled()
        {
            return None;
        }
        let Value::Object(Some(loader)) = self.get_field_by_name(module, "loader") else {
            return None;
        };
        self.prune_layer_module_pins();
        let (loader_h, module_h) = {
            let mut refs = self.shared.natives.jni_global_refs.lock();
            (
                refs.add_weak(loader) as usize,
                refs.add_weak(module) as usize,
            )
        };
        if loader_h == 0 || module_h == 0 {
            let mut refs = self.shared.natives.jni_global_refs.lock();
            refs.remove(loader_h as crate::native::jni::JObject);
            refs.remove(module_h as crate::native::jni::JObject);
            return None;
        }
        self.shared
            .classes
            .layer_module_pins
            .lock()
            .push((loader_h, module_h));
        Some(module_h)
    }

    /// Drop the `layer_module_pins` rows (and their package rows and weak
    /// handles) whose module a collection has cleared, once the list reached
    /// `layer_module_prune_at`; the next mark is twice the survivors (at
    /// least 64), so the sweep is amortised O(1) per record.
    ///
    /// zs s7/nd6 (rv4 residual 1): the package rows go BEFORE the handles. A
    /// JNI handle is its box's address and the next `add` / `add_weak` reuses
    /// a freed one, so a handle freed while a row still named it could be
    /// resolved by `layer_module_of_package` as whatever the reused handle
    /// names. Lock order: the pin list then the JNI table (resolve only),
    /// both released; the package map alone; the JNI table alone (free).
    /// `layer_module_of_package` resolves under the map's read guard (map ->
    /// JNI), so once the retain has run no reader holds a dead handle.
    pub(in crate::vm::vm_exec) fn prune_layer_module_pins(&mut self) {
        use std::sync::atomic::Ordering;
        let classes = &self.shared.classes;
        let dead: Vec<(usize, usize)> = {
            let mut pins = classes.layer_module_pins.lock();
            if pins.len() < classes.layer_module_prune_at.load(Ordering::Relaxed) {
                return;
            }
            let refs = self.shared.natives.jni_global_refs.lock();
            let mut dead = Vec::new();
            pins.retain(|&(loader_h, module_h)| {
                if refs
                    .resolve(module_h as crate::native::jni::JObject)
                    .is_some()
                {
                    return true;
                }
                dead.push((loader_h, module_h));
                false
            });
            classes
                .layer_module_prune_at
                .store((pins.len() * 2).max(64), Ordering::Relaxed);
            dead
        };
        if dead.is_empty() {
            return;
        }
        if layer_module_prune_rows_first_enabled() {
            let gone: rustc_hash::FxHashSet<usize> = dead.iter().map(|&(_, m)| m).collect();
            classes
                .layer_module_packages
                .write()
                .retain(|_, packages| {
                    packages.retain(|_, handle| !gone.contains(handle));
                    !packages.is_empty()
                });
            self.free_layer_module_handles(&dead);
        } else {
            // `CRATONVM_ZGC_LAYER_MODULE_PRUNE_ROWS_FIRST=0`: the s5 order (handles
            // freed first, the rows after).
            self.free_layer_module_handles(&dead);
            let gone: rustc_hash::FxHashSet<usize> = dead.iter().map(|&(_, m)| m).collect();
            classes
                .layer_module_packages
                .write()
                .retain(|_, packages| {
                    packages.retain(|_, handle| !gone.contains(handle));
                    !packages.is_empty()
                });
        }
    }

    pub(in crate::vm::vm_exec) fn free_layer_module_handles(&mut self, dead: &[(usize, usize)]) {
        #[cfg(test)]
        zs7nd6_prune_observer::observe(self.shared, dead);
        let mut refs = self.shared.natives.jni_global_refs.lock();
        for &(loader_h, module_h) in dead {
            refs.remove(module_h as crate::native::jni::JObject);
            refs.remove(loader_h as crate::native::jni::JObject);
        }
    }
}


/// `CRATONVM_ZGC_LAYER_MODULE_PRUNE_ROWS_FIRST` (default on; zs s7/nd6): see
/// `prune_layer_module_pins`. `0` frees the handles before their rows (s5).
pub(in crate::vm::vm_exec) fn layer_module_prune_rows_first_enabled() -> bool {
    #[cfg(test)]
    {
        true
    }
    #[cfg(not(test))]
    {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            true
        })
    }
}

/// Does this VM's collector take the loader-owned layer-module rows? ZGC
/// always (zs s5/nd5); G1 and Generational under
/// `CRATONVM_LAYER_MODULE_LOADER_OWNED_EVERY_GC` (default ON, `=0` kills; see
/// `NativeContextImpl::layer_module_loader_owned`).
pub(in crate::vm::vm_exec) fn layer_module_loader_owned_here(shared: &SharedVm) -> bool {
    super::vm_object::heap_is_zgc(shared) || layer_module_loader_owned_every_gc_enabled()
}

pub(in crate::vm::vm_exec) fn layer_module_loader_owned_every_gc_enabled() -> bool {
    // Unit tests flip it per thread (`flags::override_thread`).
    #[cfg(test)]
    {
        true
    }
    #[cfg(not(test))]
    {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            true
        })
    }
}

/// `CRATONVM_ZGC_LAYER_MODULE_LOADER_OWNED` (default on; zs s5/nd5): see
/// `NativeContextImpl::layer_module_loader_owned`. `0` restores the
/// permanent strong root.
pub(in crate::vm::vm_exec) fn layer_module_loader_owned_enabled() -> bool {
    // Unit tests flip it per thread (`flags::override_thread`).
    #[cfg(test)]
    {
        true
    }
    #[cfg(not(test))]
    {
        true
    }
}



/// Interpreter round i5 wave 100, lane L3 (`L3W100ClinitTriggerWalks` rows
/// `VarHandle.get`): a walk made inside a `<clinit>` that a native ran under a
/// served class-initialization chain (`initialize_class_served_cold` /
/// `ensure_class_initialized_served`: a static VarHandle's holder, whose JDK
/// path is `VarHandle<T>s$FieldStatic*.<mode>` -> `LazyInitializingVarHandle
/// .target` -> `ensureInitialized` -> `initialize` ->
/// `Unsafe.ensureClassInitialized` -> `ensureClassInitialized0`) lists those
/// JDK frames between the marked caller and the `<clinit>`, as HotSpot's walk
/// does; the walk listed none. The frames are the ones the same mark gives a
/// failed initialization's throwable (`stackwalker::served_raise_frames`).
/// Inserted only when the thread's mark is that chain's (a class-init
/// throwable class, with a chain), the frame the initialization pushed first
/// is that `<clinit>`, and the walk lists the marked frame right below it.
/// The hidden `VarHandleGuards.guard_*` frame a `SHOW_HIDDEN_FRAMES` walk
/// adds on HotSpot goes outermost of them since wave 102
/// (`stackwalker::var_handle_guard_entry`).
pub(in crate::vm::vm_exec) fn insert_served_init_chain_walk_entries(
    cm: &crate::classloading::ClassManager,
    thread: &JvmThread,
    trace: &mut Vec<StackTraceEntry>,
) {
    // Interpreter round i5 wave 104, lane L3: every mark of the thread's mark
    // stack (`ServedNativeRaise::outer`), innermost first; a mark whose chain
    // no longer stands inserts nothing.
    let mut next = thread.served_native_raise.as_deref();
    while let Some(mark) = next {
        insert_served_init_chain_entries_for(cm, &thread.frames, mark, trace, true);
        next = mark.outer.as_deref();
    }
}

/// [`insert_served_init_chain_walk_entries`] for one mark: the chain's
/// entries go in right before that `<clinit>` of an OUTERMOST-first `trace`
/// of [`StackTraceEntry`]s (with the hidden `VarHandleGuards` frame first
/// when `with_guard`). Answers where they went (the index of the first
/// inserted entry) and how many, or `None` when nothing went in. Interpreter
/// round i5 wave 104, lane L3 (split out of the wave-100 walk rule so the
/// current thread's own listing, `thread_stack_trace_served`, inserts the
/// same entries).
pub(in crate::vm::vm_exec) fn insert_served_init_chain_entries_for(
    cm: &crate::classloading::ClassManager,
    frames: &crate::runtime::frame::FrameStack,
    mark: &crate::runtime::stackwalker::ServedNativeRaise,
    trace: &mut Vec<StackTraceEntry>,
    with_guard: bool,
) -> Option<(usize, usize)> {
    use crate::runtime::stackwalker::{served_raise_frames, BacktraceFrame};
    // Only a static VarHandle's chain (its first hop is the served `VarHandle`
    // access mode): a method handle's or a reflective door's initialization
    // marks the thread too, and its caller (a lambda proxy's `run`, a spun
    // form) is not a frame this rule may rebuild (w101 hand-back: the
    // `lambda hidden` row lost its `$$Lambda.run` frame).
    // Interpreter round i5 wave 103, lane L3 (`L3W100ClinitTriggerWalks` rows
    // `Field.getInt`, `--real-jdk`): a `Field` value door's chain too
    // (`field_class_init_chain`, first hop `Field.<get|set>*`). Under
    // `--real-jdk` those doors are reviewed `Intrinsic`s
    // (`NativeMethodRegistry::field_door_kind`), so no JDK `Field` frame
    // stands between the caller and the `<clinit>`; HotSpot lists
    // `Field.getInt` .. `MethodHandleAccessorFactory.newFieldAccessor` ..
    // `Unsafe.ensureClassInitialized0`. The caller is the frame that invoked
    // the `Field` method itself, never a spun form. `--compatible` runs the
    // JDK's `Field` bytecode (a `Bridge` loses to it), so its walk already has
    // the real frames and its mark, if any, is not this chain.
    if !served_init_chain_listed(mark) {
        return None;
    }
    let first_pushed = frames.get(mark.depth)?;
    if first_pushed.method_name() != "<clinit>" {
        return None;
    }
    let clinit_class = first_pushed.class_id;
    // OUTERMOST-first: the marked caller is the entry right before the
    // `<clinit>`.
    let pos = trace.iter().enumerate().position(|(i, e)| {
        i > 0
            && e.class_id == Some(clinit_class)
            && &*e.method_name == "<clinit>"
            && trace[i - 1].class_id == Some(mark.class_id)
            && *trace[i - 1].method_name == *mark.method_name
    })?;
    let caller = BacktraceFrame::Entry(trace[pos - 1].clone());
    let mut resolve =
        |requester: ClassId, name: &str| cm.find_class_by_name_for_class(name, requester);
    let chain_frames = served_raise_frames(
        &cm.class_store,
        &caller,
        mark.throwable_class,
        mark.hints(),
        &mut resolve,
    )?;
    // Interpreter round i5 wave 102, lane L3 (row `VarHandle.get hidden`):
    // the hidden `VarHandleGuards.guard_<shape>` frame the caller's
    // `invokevirtual VarHandle.<mode>` links to, outermost of the chain
    // (`capture_stack_walk_trace_with` loads the class first). A walk that
    // does not show hidden frames drops it, as HotSpot's does.
    let guard = if with_guard {
        crate::runtime::stackwalker::var_handle_guard_entry(
            &cm.class_store,
            &trace[pos - 1],
            &mut resolve,
        )
    } else {
        None
    };
    let mut at = pos;
    if let Some(guard) = guard {
        trace.insert(at, guard);
        at += 1;
    }
    let mut count = at - pos;
    for (offset, frame) in chain_frames.into_iter().enumerate() {
        trace.insert(at + offset, frame.to_entry());
        count += 1;
    }
    (count > 0).then_some((pos, count))
}

/// Interpreter round i5 wave 104, lane L3
/// (`i103-L3-clinit-trigger-throwable-traces-lack-served-door-frames` residue
/// (b); `L3W104ClinitTriggerListings`): the current thread's own listing
/// (`thread_stack_trace_served`: `Thread.getAllStackTraces` / `dumpThreads`,
/// `ThreadMXBean` dumps, the synthetic JDK's `Thread.getStackTrace`) inside a
/// `<clinit>` a served door runs lists every standing chain's JDK frames
/// below it, as the walk rule does. Each inserted entry is recorded in
/// `inserted_before` as one before the captured index of the `<clinit>` it
/// goes under (`captured_index_at`), so the locked-monitor attribution
/// (`shift_served_frame_indices`, the layout check of
/// `frame_locked_monitors_with_compiled`) keeps the depths of the captured
/// entries. `with_guard` adds the hidden `VarHandleGuards` frame.
pub(in crate::vm::vm_exec) fn insert_served_init_chain_listing_entries(
    cm: &crate::classloading::ClassManager,
    thread: &JvmThread,
    trace: &mut Vec<StackTraceEntry>,
    captured: usize,
    inserted_before: &mut Vec<usize>,
    with_guard: bool,
) {
    let mut next = thread.served_native_raise.as_deref();
    let mut any = false;
    while let Some(mark) = next {
        // As the throwable capture: only a chain that still stands.
        if served_init_chain_clinit(&thread.frames, mark).is_some() {
            if let Some((pos, count)) =
                insert_served_init_chain_entries_for(cm, &thread.frames, mark, trace, with_guard)
            {
                // `pos` is where the `<clinit>` entry sat before the entries
                // went in: its captured index, under the insertions so far.
                let before = captured_index_at(pos, captured, inserted_before);
                inserted_before.extend((0..count).map(|_| before));
                any = true;
            }
        }
        next = mark.outer.as_deref();
    }
    if any {
        inserted_before.sort_unstable();
    }
}

/// Is `mark` a served class-initialization chain whose JDK frames the walk
/// and capture rules list below the `<clinit>` it ran
/// ([`insert_served_init_chain_walk_entries`],
/// [`insert_served_init_chain_capture_frames`])? A class-init throwable class
/// and a chain whose first hop is a static VarHandle's access mode or a
/// `Field` value door (see the walk rule for why only these two).
pub(in crate::vm::vm_exec) fn served_init_chain_listed(mark: &crate::runtime::stackwalker::ServedNativeRaise) -> bool {
    crate::runtime::stackwalker::CLASS_INIT_FAILURES.contains(&mark.throwable_class)
        && mark
            .chain
            .as_deref()
            .and_then(|chain| chain.hops.first())
            .is_some_and(|hop| {
                hop.class == "java/lang/invoke/VarHandle" || hop.class == "java/lang/reflect/Field"
            })
}

/// The `<clinit>` class of a served class-init chain mark that still stands
/// on `frames` (interpreter round i5 wave 103, lane L3): the mark is
/// [`served_init_chain_listed`], the frame the initialization pushed first
/// (`frames[mark.depth]`) is a `<clinit>`, and the marked caller still stands
/// at the marked call. `None` otherwise. Cold: asked only while a mark stands.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn served_init_chain_clinit(
    frames: &crate::runtime::frame::FrameStack,
    mark: &crate::runtime::stackwalker::ServedNativeRaise,
) -> Option<ClassId> {
    if !served_init_chain_listed(mark) {
        return None;
    }
    let clinit = frames.get(mark.depth)?;
    if clinit.method_name() != "<clinit>" {
        return None;
    }
    let caller = frames.get(mark.depth.checked_sub(1)?)?;
    (caller.last_instr_pc == mark.pc).then_some(clinit.class_id)
}

/// Does any mark of `mark`'s stack (it and its
/// [`outer`](crate::runtime::stackwalker::ServedNativeRaise::outer) marks)
/// name a served class-init chain that still stands on `frames`
/// ([`served_init_chain_clinit`])? Interpreter round i5 wave 104, lane L3.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn served_init_chain_stands(
    frames: &crate::runtime::frame::FrameStack,
    mark: Option<&crate::runtime::stackwalker::ServedNativeRaise>,
) -> bool {
    let mut next = mark;
    while let Some(mark) = next {
        if served_init_chain_clinit(frames, mark).is_some() {
            return true;
        }
        next = mark.outer.as_deref();
    }
    false
}

/// Does `mark`'s stack hold a static VarHandle's served class-init chain
/// (first hop the served `VarHandle` access mode), whose walk lists the hidden
/// `VarHandleGuards` frame? Interpreter round i5 wave 104, lane L3 (wave 102's
/// test, over the whole stack).
pub(in crate::vm::vm_exec) fn mark_stack_has_var_handle_chain(
    mark: Option<&crate::runtime::stackwalker::ServedNativeRaise>,
) -> bool {
    let mut next = mark;
    while let Some(mark) = next {
        if mark
            .chain
            .as_deref()
            .and_then(|chain| chain.hops.first())
            .is_some_and(|hop| hop.class == "java/lang/invoke/VarHandle")
        {
            return true;
        }
        next = mark.outer.as_deref();
    }
    false
}

/// Interpreter round i5 wave 103, lane L3 (`L3W103ClinitTriggerThrowableTraces`;
/// `CRATONVM_THROWABLE_SERVED_INIT_CHAIN_FRAMES`): the throwable-capture twin
/// of [`insert_served_init_chain_walk_entries`]. A throwable built inside the
/// `<clinit>` (`clinit_class`) that a served door runs under `mark`'s chain --
/// not the initialization's own failure, which takes the mark -- lists the
/// chain's JDK frames between the marked caller and the `<clinit>`, as
/// HotSpot's does (the `<clinit>` runs on top of those Java frames there); so
/// does the current thread's `Thread.getStackTrace`, a throwable's capture.
/// `trace` is OUTERMOST-first. The hidden `VarHandleGuards` frame goes in,
/// outermost of the chain, only with `with_guard` (`-XX:+ShowHiddenFrames`,
/// where a throwable keeps hidden frames; interpreter round i5 wave 105, lane
/// L3). `true` when it inserted frames.
pub(in crate::vm::vm_exec) fn insert_served_init_chain_capture_frames<'a>(
    shared: &'a SharedVm,
    cm: &mut Option<CaptureClassManager<'a>>,
    mark: &crate::runtime::stackwalker::ServedNativeRaise,
    clinit_class: ClassId,
    trace: &mut Vec<crate::runtime::stackwalker::BacktraceFrame>,
    max_depth: usize,
    with_guard: bool,
) -> bool {
    use crate::runtime::stackwalker::served_raise_frames;
    // Innermost `<clinit>` of that class standing on the marked caller.
    let Some(pos) = (1..trace.len()).rev().find(|&i| {
        let clinit = trace[i].to_entry();
        if clinit.class_id != Some(clinit_class) || &*clinit.method_name != "<clinit>" {
            return false;
        }
        let caller = trace[i - 1].to_entry();
        caller.class_id == Some(mark.class_id) && *caller.method_name == *mark.method_name
    }) else {
        return false;
    };
    let cm = cm.get_or_insert_with(|| shared.classes.class_manager.read());
    let cm: &crate::classloading::ClassManager = cm;
    let mut resolve =
        |requester: ClassId, name: &str| cm.find_class_by_name_for_class(name, requester);
    let Some(frames) = served_raise_frames(
        &cm.class_store,
        &trace[pos - 1],
        mark.throwable_class,
        mark.hints(),
        &mut resolve,
    ) else {
        return false;
    };
    // The guard the caller's `invokevirtual VarHandle.<mode>` links to (the
    // walk rule's `var_handle_guard_entry`); `None` for a `Field` chain.
    let guard = if with_guard {
        crate::runtime::stackwalker::var_handle_guard_entry(
            &cm.class_store,
            &trace[pos - 1].to_entry(),
            &mut resolve,
        )
        .map(crate::runtime::stackwalker::BacktraceFrame::Entry)
    } else {
        None
    };
    if frames.is_empty() && guard.is_none() {
        return false;
    }
    let tail = trace.split_off(pos);
    trace.extend(guard);
    trace.extend(frames);
    trace.extend(tail);
    crate::runtime::stackwalker::cap_throwable_trace(trace, max_depth);
    true
}

