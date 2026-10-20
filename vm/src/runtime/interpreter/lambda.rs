// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `invokedynamic` lambda dispatch, and the coercions it needs.
//!
//! A lambda call site does not name a method. It names a bootstrap that
//! produced a synthetic class implementing a functional interface, and
//! this VM has to get from the call site's descriptor to the
//! implementation method the bootstrap captured — then make the
//! arguments fit, which is where most of the file goes.
//!
//! The coercion is not incidental. The SAM descriptor and the
//! implementation descriptor differ legally: a generic SAM erases to
//! `Object`, so an `int` argument at the call site arrives boxed and the
//! implementation wants it unboxed, or widened, or both. Getting that
//! wrong produces a `ClassCastException` naming two types that look
//! compatible, which is why `cce_display_class_name` and
//! `lambda_arg_provably_not_instance` exist: the message is the only
//! thing the user sees.
//!
//! `try_lambda_dispatch` is the entry point; everything else here
//! either feeds it or is one of its fast paths.

use super::*;

/// Per-phase timing for [`try_lambda_dispatch`], armed by
/// `CRATONVM_DBG=lambda-prof`.
///
/// Exists because a lambda's SAM call measures ~4.2 us on this VM against ~18 ns
/// for the *identical* interface call on a named class (monomorphic call sites,
/// both measurement orders — see the `LambdaProbe2` numbers in the WebFlux
/// throughput record) — a ~220x penalty HotSpot does not have, and large enough
/// to dominate lambda-dense workloads such as Spring context startup. Naming the
/// term needs the total and the parts measured in the same run: reading the
/// source offers four plausible candidates (the call-site clone, argument
/// coercion, the by-name class resolution, the target invoke) and cannot rank
/// them.
///
/// Off by default and behind a `OnceLock`, so an unprofiled run pays one relaxed
/// load per dispatch.
pub(crate) mod lambda_prof {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;

    pub(crate) static CALLS: AtomicU64 = AtomicU64::new(0);
    /// Whole `try_lambda_dispatch`, entry to return.
    pub(crate) static TOTAL_NS: AtomicU64 = AtomicU64::new(0);
    /// The `lambda_proxies` read + `LambdaCallSite` clone.
    pub(crate) static LOOKUP_NS: AtomicU64 = AtomicU64::new(0);
    /// Everything between the lookup and the target invoke: descriptor splits,
    /// capture prepending, `coerce_lambda_args`.
    pub(crate) static PREP_NS: AtomicU64 = AtomicU64::new(0);
    /// The target invoke itself — the impl body plus whatever class resolution
    /// the chosen invoke entry point does on the way in.
    pub(crate) static TARGET_NS: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| cratonvm_types::flags::runtime_var("CRATONVM_DBG_LAMBDA_PROF").is_ok())
    }

    pub(crate) fn add(counter: &AtomicU64, ns: u64) {
        counter.fetch_add(ns, Ordering::Relaxed);
    }

    /// How often to print. Every N dispatches, so a profile lands even on a run
    /// that is killed at a timeout rather than exiting cleanly.
    pub(crate) const REPORT_EVERY: u64 = 200_000;

    pub(crate) fn report() {
        let calls = CALLS.load(Ordering::Relaxed).max(1);
        let total = TOTAL_NS.load(Ordering::Relaxed);
        let lookup = LOOKUP_NS.load(Ordering::Relaxed);
        let prep = PREP_NS.load(Ordering::Relaxed);
        let target = TARGET_NS.load(Ordering::Relaxed);
        let other = total.saturating_sub(lookup + prep + target);
        eprintln!(
            "[LAMBDA-PROF] calls={calls} total={}ns/call  lookup={}  prep={}  target={}  other={}",
            total / calls,
            lookup / calls,
            prep / calls,
            target / calls,
            other / calls,
        );
    }
}

/// Engagement census for the lambda JIT tier-up path (`CRATONVM_DBG_LAMBDA_JIT=1`).
///
/// A flat A/B on this path cannot tell "the compiled body did not help" from
/// "no compiled body was ever entered": both read as no change. So every
/// number this path is quoted with is quoted beside the count of calls that
/// actually took it. `ELIGIBLE` is the denominator (dispatches that passed the
/// gates), `COMPILED_HITS` the ones that found a compiled body, `FAST_RETURNS`
/// the ones that returned through it, `DECLINES` the ones that entered the
/// primitive and fell back, `NOMINATIONS` the tier-up enqueues that exist only
/// because of this path.
///
/// Counters are plain relaxed atomics behind a `OnceLock` gate, so an
/// unprofiled run pays one relaxed load per dispatch and nothing else. The
/// report is periodic (not at-exit) so a profile lands even on a run killed at
/// a timeout — same reasoning as `lambda_prof::REPORT_EVERY`.
pub(crate) mod lambda_jit {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;

    pub(crate) static ELIGIBLE: AtomicU64 = AtomicU64::new(0);
    pub(crate) static COMPILED_HITS: AtomicU64 = AtomicU64::new(0);
    pub(crate) static FAST_RETURNS: AtomicU64 = AtomicU64::new(0);
    pub(crate) static DECLINES: AtomicU64 = AtomicU64::new(0);
    pub(crate) static NOMINATIONS: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| cratonvm_types::flags::runtime_var("CRATONVM_DBG_LAMBDA_JIT").is_ok())
    }

    #[inline]
    pub(crate) fn bump(counter: &AtomicU64) {
        if on() {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// How often to print, counted in eligible dispatches.
    pub(crate) const REPORT_EVERY: u64 = 200_000;

    #[inline]
    pub(crate) fn maybe_report() {
        if !on() {
            return;
        }
        let eligible = ELIGIBLE.load(Ordering::Relaxed);
        if eligible % REPORT_EVERY == 0 && eligible > 0 {
            report();
        }
    }

    pub(crate) fn report() {
        eprintln!(
            "[LAMBDA-JIT] eligible={} compiled_hits={} fast_returns={} declines={} nominations={} {}",
            ELIGIBLE.load(Ordering::Relaxed),
            COMPILED_HITS.load(Ordering::Relaxed),
            FAST_RETURNS.load(Ordering::Relaxed),
            DECLINES.load(Ordering::Relaxed),
            NOMINATIONS.load(Ordering::Relaxed),
            super::lambda_site_prof::line(),
        );
    }
}

/// LambdaMetafactory argument adaptation (`samMethodType` → `instantiatedMethodType`).
///
/// When a functional-interface SAM has erased parameters (commonly `Object`,
/// from an unbounded type variable) but the lambda is *instantiated* with a more
/// specific type argument, javac's generated bridge method inserts a `checkcast`
/// to the instantiated parameter type before calling the implementation method —
/// throwing `ClassCastException` for an incompatible runtime argument (e.g.
/// `Map<String,Object>.forEach((k, v) -> …)` where a non-`String` key was stored
/// through a raw reference). CratonVM dispatches the lambda body *directly* from
/// [`try_lambda_dispatch`] / `NativeContextImpl::invoke_virtual`, bypassing that
/// synthetic bridge, so this helper replays the cast.
///
/// Only SAM-supplied args (`args[num_captures..]`) are checked, each against the
/// corresponding `instantiatedMethodType` parameter. Conservative posture (never
/// a spurious `ClassCastException`): a `null` argument, a primitive parameter, a
/// non-narrowing (instantiated == erased) parameter, an unloaded target type, or
/// any case we can't decide without risk all pass without throwing.
pub(super) fn checkcast_lambda_instantiated_args(
    shared: &SharedVm,
    thread: &JvmThread,
    handles: &[Option<usize>],
    sam_desc: &str,
    inst_desc: &str,
    args: &[Value],
    num_captures: usize,
) -> Result<(), MethodCallFailed> {
    let (sam_params, _) = split_method_descriptor_ref(sam_desc);
    let (inst_params, _) = split_method_descriptor_ref(inst_desc);
    for (sam_idx, inst_tok) in inst_params.iter().enumerate() {
        // Only a reference instantiated param can carry a checkcast.
        if !is_reference_desc(inst_tok) {
            continue;
        }
        // No narrowing vs the erased SAM param → the bridge inserts no cast.
        if sam_params
            .get(sam_idx)
            .map(|s| s == inst_tok)
            .unwrap_or(false)
        {
            continue;
        }
        // SAM-supplied args follow the captures in `args`.
        //
        // GC-safety: `args` is a plain Rust Vec copy handed to us by the
        // caller, not itself a GC root -- only `thread.native_pin_roots`
        // (which the caller pinned every object arg into before this call)
        // is remapped by a moving GC. A prior loop iteration's
        // `lambda_arg_provably_not_instance` call can trigger a GC through the
        // class loads it performs (the target, array components), which leaves any later
        // `args[idx]` read here pointing at a stale, already-evacuated
        // address. Read the CURRENT address back through the caller's pin
        // (`handles`) instead of the raw `args` slice. Confirmed live via
        // CRATONVM_DBG_STALE_OBJREF during WildFly parallel-extension-add --
        // see wildfly-parallel-boot-stale-objectref-residual.md.
        let idx = num_captures + sam_idx;
        let pin = handles.get(idx).copied().flatten();
        let unpinned = match pin {
            Some(h) => thread.native_pin_roots[h],
            None => match args.get(idx) {
                Some(Value::Object(Some(o))) => *o,
                // null (a `checkcast` of null always succeeds), primitive, or missing.
                _ => continue,
            },
        };
        // The pin is read afresh at every use, INSIDE the predicate too: it
        // loads the target class, and the object it then screens (a proxy's
        // interface list, an annotation proxy's type) must be the post-load
        // address. The message below reads it again for the same reason.
        let read_obj = || pin.map_or(unpinned, |h| thread.native_pin_roots[h]);
        if lambda_arg_provably_not_instance(shared, &read_obj, inst_tok) {
            let obj_ref = read_obj();
            let obj_class_id = shared.mem.heap.class_id_of(obj_ref);
            let stored_class_name = shared
                .classes
                .class_manager
                .read()
                .get_class(obj_class_id)
                .map(|c| c.name.to_string());
            // A lambda proxy argument is named as the `checkcast` opcode names
            // it (`lambda_proxy_display_name`, wave 49).
            let proxy_display = match stored_class_name {
                None => lambda_proxy_display_name(shared, obj_class_id),
                Some(_) => None,
            };
            let obj_class_name = stored_class_name
                // Name the id, not just "?" — see the matching note on the
                // `checkcast` opcode's fallback.
                .unwrap_or_else(|| format!("?class_id={obj_class_id}"));
            // Dotted here; the proxy's name already is (and its `/` is its own).
            // JIT round 21 wave 25, lane hexname (item 5 of
            // `r21-review-w23-findings-20261019.md`,
            // `CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`): by the argument's BOTTOM
            // class's hidden flag, as the `checkcast` opcode renders it; the
            // shape rule stays the fallback and the `=0` text.
            let obj_display_name = match proxy_display {
                Some(display) => display,
                None => {
                    let display = cce_display_class_name(shared, obj_ref, &obj_class_name);
                    crate::runtime::exceptions::external_name_by_bottom_class_or(
                        &display,
                        crate::runtime::exceptions::bottom_class_hidden(
                            shared,
                            obj_class_id,
                            &display,
                        ),
                        crate::runtime::exceptions::hotspot_external_name,
                    )
                }
            };
            let target_binary = inst_tok
                .strip_prefix('L')
                .and_then(|d| d.strip_suffix(';'))
                .unwrap_or(inst_tok);
            let target_binary = crate::runtime::exceptions::hotspot_external_name(target_binary);
            // CRATONVM_DBG_CCE_BT: same attribution hook as the `checkcast`
            // opcode, plus whether this argument was read through the pinned
            // path (`via_pin`) — re-establishing the 2026-07-15 session's
            // temporary instrumentation permanently (that session measured
            // via_pin=true on every captured stale read here).
            if crate::runtime::interpreter::dbg_cce_bt_enabled() {
                let via_pin = handles.get(idx).copied().flatten().is_some();
                eprintln!(
                    "CRATONVM_DBG_CCE_BT: site=lambda_instantiated_args obj={} @0x{:x} target={} via_pin={via_pin}",
                    obj_display_name,
                    obj_ref.as_ptr() as usize,
                    target_binary
                );
            }
            // Same dotted-name shape as the `checkcast` opcode (tools such as
            // mockk's `JvmAutoHinter` parse this text).
            return Err(RuntimeError::ClassCastException {
                message: format!("{} cannot be cast to {}", obj_display_name, target_binary),
            }
            .into());
        }
    }
    Ok(())
}

/// The host class of lambda proxy `class_id` -- its id when the indy
/// bootstrap recorded one, and its internal name -- or `None` for any other
/// class. The same answer as `NativeContext::lambda_proxy_host`: the recorded
/// defining class first, else the implementation method's owner.
pub(crate) fn lambda_proxy_host(
    shared: &SharedVm,
    class_id: ClassId,
) -> Option<(Option<ClassId>, String)> {
    // Lambda proxy ids are always >= 0x8000_0000; an ordinary class takes no lock.
    if class_id.as_u32() < 0x8000_0000 {
        return None;
    }
    let recorded = shared
        .classes
        .lambda_proxy_hosts
        .read()
        .get(&class_id)
        .copied();
    if let Some(host_id) = recorded {
        let name = shared
            .classes
            .class_manager
            .read()
            .get_class(host_id)
            .map(|c| c.name.to_string());
        if let Some(name) = name {
            return Some((Some(host_id), name));
        }
    }
    let owner = shared
        .classes
        .lambda_proxies
        .read()
        .get(&class_id)
        .map(|cs| cs.impl_handle.class_name.to_string())?;
    Some((None, owner))
}

/// The external name a lambda proxy's `ClassCastException` names it by:
/// `<host>$$Lambda/0x<id as 16 hex digits>`, the name its `Class.getName()`
/// answers (`native-builtins` `lang_class::lambda_proxy_class_name`), in the
/// shape of HotSpot's hidden lambda class names. `None` for any other class.
/// Already dotted: the `/` before `0x` is part of the name, so it must not go
/// through the internal-to-binary `/` rewrite (interpreter round i1 wave 49,
/// lane L7: `checkcast` of a lambda printed `?class_id=2147483658 cannot be
/// cast to ...`).
pub(crate) fn lambda_proxy_display_name(shared: &SharedVm, class_id: ClassId) -> Option<String> {
    let (_, host) = lambda_proxy_host(shared, class_id)?;
    Some(format!(
        "{}$$Lambda/0x{:016x}",
        host.replace('/', "."),
        class_id.as_u32()
    ))
}

/// Return the Java-visible class name for a failed cast.
///
/// The immutable `Map.of` factories and `Collections.unmodifiableMap` use the
/// same native storage stamp. `Object.getClass()` deliberately translates that
/// stamp to the corresponding JDK implementation class, but a VM-generated
/// `ClassCastException` previously exposed the private stamp instead. Besides
/// being observably unlike HotSpot, that broke `LambdaSafe`: it identifies an
/// erased-generic mismatch by comparing the exception prefix with
/// `argument.getClass().getName()`.
///
/// Keep this mapping in lockstep with `native-builtins`' `getClass()` mapping
/// for maps. The backing map's physical slot layout follows the loaded JDK
/// class, so resolve its `size` field rather than assuming a fixed slot.
///
/// `pub(crate)`, not `pub(super)`: the JIT's `jit_aastore` (`vm/src/jit/
/// helpers.rs`) is the FIFTH site asking this same question and the third
/// `aastore` twin — see `docs/internal/jdk-only/W8-E11-1-jit-aastore-third-twin-and-the-check-only-helper.md`.
/// `interpreter.rs` already does `pub use lambda::*;` and a glob re-export
/// caps at the item's own visibility, so widening the item here is the whole
/// change: no new re-export, and the JIT reaches it as
/// `crate::runtime::interpreter::cce_display_class_name`, the same module path
/// it already uses for `aastore_element_assignable`.
pub(crate) fn cce_display_class_name(
    shared: &SharedVm,
    obj_ref: ObjectRef,
    raw_name: &str,
) -> String {
    // An array receiver must render as its own type, not its component's.
    // The header word of a reference array holds the COMPONENT class id, so
    // the caller's `class_id_of` -> `class.name` lookup yields
    // `java/lang/String` for a `String[]` and produces the nonsensical, and
    // actively misleading, `java.lang.String cannot be cast to
    // java.lang.String` (the TestObjectDataType failure, chased for a session
    // as a class-identity split). HotSpot renders the descriptor instead:
    // `[Ljava.lang.String;`.
    if let Some(desc) = array_descriptor_of(shared, obj_ref) {
        return desc;
    }
    // A lambda proxy has no class in the store, so a caller's by-id lookup
    // answered `?` for it: the three `aastore` twins (`opcodes.rs`,
    // `interpreter.rs`, the JIT's `jit_aastore`) said
    // `ArrayStoreException: ?` where HotSpot names the hidden lambda class.
    // Named as its `getClass().getName()` names it (interpreter round i4 wave
    // 84, lane L2); `checkcast` already passed that name in. One id compare
    // for every other class.
    if let Some(name) = lambda_proxy_display_name(shared, shared.mem.heap.class_id_of(obj_ref)) {
        return name;
    }
    // The stamp table used to be inlined here FOR MAPS ONLY, so every other
    // internal stamp fell through and printed its private name — the exact
    // thing this function exists to prevent, applied to one of the eleven
    // stamps that need it. MEASURED 2026-08-21, Compatible mode,
    // `cratonvm-r5.exe`:
    //
    //   (AbstractCollection) List.of(1,2,3)
    //     -> class cratonvm.internal.UnmodifiableList cannot be cast to
    //        class java.util.AbstractCollection
    //
    // against HotSpot's `java.util.ImmutableCollections$ListN`. The `LambdaSafe`
    // breakage described above — it compares the exception prefix against
    // `argument.getClass().getName()` — was therefore still live for every
    // List, Set and Collection receiver.
    //
    // `unmod_stamp_display_name` is now the one table, and it is the SAME
    // function the `instanceof`/`checkcast` DECISION reads, so the message and
    // the verdict cannot drift apart again. It returns `None` for every
    // non-stamp class (the overwhelmingly common case) without touching a
    // field. H18-1.
    let Some(family) = unmod_stamp_display_name(shared, obj_ref, raw_name) else {
        return raw_name.to_string();
    };
    // A MESSAGE wants the size-discriminated form; a SUBTYPE answer does not
    // (`Map1` and `MapN` are supertype-identical — see
    // `unmod_stamp_display_name`). So the refinement lives here, at the only
    // caller that can observe it, and not in the shared table. This is the
    // pre-existing map behaviour, generalised to List and Set rather than
    // replaced: the size still comes from the backing collection's own `size`
    // FIELD, never from `invoke_virtual("size")`, so this stays safepoint-free
    // and the receiver cannot move under it.
    let refined = match family {
        "java/util/ImmutableCollections$MapN" if unmod_backing_size(shared, obj_ref) == Some(1) => {
            "java/util/ImmutableCollections$Map1"
        }
        "java/util/ImmutableCollections$ListN"
            if matches!(unmod_backing_size(shared, obj_ref), Some(1..=2)) =>
        {
            "java/util/ImmutableCollections$List12"
        }
        "java/util/ImmutableCollections$SetN"
            if matches!(unmod_backing_size(shared, obj_ref), Some(1..=2)) =>
        {
            "java/util/ImmutableCollections$Set12"
        }
        other => other,
    };
    refined.to_string()
}

/// The element count of an unmodifiable wrapper's backing collection, read from
/// its `size` FIELD.
///
/// Deliberately not `invoke_virtual(this, "size", "()I")`, which is how
/// `native-builtins` reaches the same number for `getClass()`: that runs Java
/// code, and this is called while building a `ClassCastException` from a
/// receiver the interpreter holds as a bare local. A field read cannot
/// safepoint.
///
/// `None` when the backing is absent or is an array/foreign layout with no
/// `size` field — the caller then keeps the `…N` form, which is what HotSpot
/// reports for everything but the one- and two-element cases anyway.
fn unmod_backing_size(shared: &SharedVm, obj_ref: ObjectRef) -> Option<i32> {
    if shared.mem.heap.num_fields(obj_ref) == 0 {
        return None;
    }
    let backing = match shared.mem.heap.get_field(obj_ref, 0) {
        Value::Object(Some(backing)) => backing,
        _ => return None,
    };
    let class_id = shared.mem.heap.class_id_of(backing);
    let cm = shared.classes.class_manager.read();
    let raw = find_field_recursive(class_id, "size", &cm.class_store)
        .map(|(field_index, _, _)| shared.mem.heap.get_field(backing, field_index));
    match raw {
        Some(Value::Int(n)) => Some(n),
        _ => None,
    }
}

/// `true` iff the object `obj` reads is *provably* not an instance of the
/// reference descriptor `desc_tok` (`L...;` or `[...`). Fails open (returns
/// `false`) whenever the answer can't be established without risk — a target
/// that fails to load, an array-vs-non-array shape we can't decide, or a
/// non-class descriptor — so a genuine instance is never rejected.
///
/// This CAN load classes (the target, as the bridge's `checkcast` would, and
/// array components), and a load can reach a safepoint. So the object is not
/// passed by value: `obj` must return its CURRENT address — read back through
/// a GC-remapped pin — every time it is called, and the caller must re-read it
/// the same way after this returns. A caller that holds only a raw address
/// must use [`lambda_arg_cast_verdict`] with `may_load == false` instead.
pub(super) fn lambda_arg_provably_not_instance(
    shared: &SharedVm,
    obj: &dyn Fn() -> ObjectRef,
    desc_tok: &str,
) -> bool {
    // With `may_load` the verdict is always decided; `None` cannot occur.
    lambda_arg_cast_verdict(shared, obj, desc_tok, true).unwrap_or(false)
}

/// The lambda paths' default-ON switches, read once per VM (gci GC round wave
/// i14, lane sx): `SharedVm::lambda_switches`, set in `SharedVm::new`.
///
/// Each was a process `OnceLock<bool>` latch (nio21 waves 15-16), so the first
/// reader in the process fixed the answer for every VM after it and a test's
/// `with_thread_overrides` could not reach them once any test had read them.
/// Every reader holds the VM, and each switch is read on a per-call lambda
/// path, so a field load replaces the latch's load. In production the flag
/// snapshot is immutable, so every VM reads what the latch read. The value
/// rule (`runtime_flag_default_on`: off only for `0|false|off|no`) is
/// unchanged. The readers are the `*_enabled(shared)` functions below and
/// `invoke_fast::lambda_door_compiled_enabled`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LambdaSwitches {
    /// `CRATONVM_LAMBDA_CAST_PASS_MEMO` ([`lambda_cast_pass_memo_enabled`]).
    pub(crate) cast_pass_memo: bool,
    /// `CRATONVM_LAMBDA_IMPL_REFUSAL_MEMO` ([`lambda_impl_refusal_memo_enabled`]).
    pub(crate) impl_refusal_memo: bool,
    /// `CRATONVM_LAMBDA_IMPL_CODE_MEMO` ([`lambda_impl_code_memo_enabled`]).
    pub(crate) impl_code_memo: bool,
    /// `CRATONVM_LAMBDA_HANDLER_IMPL_ONESHOT`
    /// ([`lambda_handler_impl_oneshot_enabled`]).
    pub(crate) handler_impl_oneshot: bool,
    /// `CRATONVM_LAMBDA_SITE_ONESHOT` ([`lambda_site_oneshot_enabled`]).
    pub(crate) site_oneshot: bool,
    /// `CRATONVM_LAMBDA_DOOR_COMPILED`
    /// (`invoke_fast::lambda_door_compiled_enabled`).
    pub(crate) door_compiled: bool,
    /// `CRATONVM_LAMBDA_OWNER_MEMO_ON_YIELD` (interpreter round i6 wave 125,
    /// lane L6, page `i125-L6`): DEFAULT OFF, unlike the six above
    /// (`runtime_flag_on`). A performance switch only; see
    /// [`record_lambda_global_impl_owner_on_yield`].
    pub(crate) owner_memo_on_yield: bool,
}

impl LambdaSwitches {
    /// Read the six default-on switches, and the default-off
    /// `owner_memo_on_yield`, from the flag snapshot in force on the calling
    /// thread (a test's `with_thread_overrides` included).
    pub(crate) fn from_flags() -> Self {
        use cratonvm_types::flags::runtime_flag_default_on;
        Self {
            owner_memo_on_yield: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_LAMBDA_OWNER_MEMO_ON_YIELD",
            ),
            cast_pass_memo: runtime_flag_default_on("CRATONVM_LAMBDA_CAST_PASS_MEMO"),
            impl_refusal_memo: runtime_flag_default_on("CRATONVM_LAMBDA_IMPL_REFUSAL_MEMO"),
            impl_code_memo: runtime_flag_default_on("CRATONVM_LAMBDA_IMPL_CODE_MEMO"),
            handler_impl_oneshot: runtime_flag_default_on("CRATONVM_LAMBDA_HANDLER_IMPL_ONESHOT"),
            site_oneshot: runtime_flag_default_on("CRATONVM_LAMBDA_SITE_ONESHOT"),
            door_compiled: runtime_flag_default_on("CRATONVM_LAMBDA_DOOR_COMPILED"),
        }
    }
}

/// `CRATONVM_LAMBDA_CAST_PASS_MEMO` (default ON; `0|false|off|no` is the kill
/// switch). nio21 wave 15 lane L5: [`lambda_arg_cast_verdict`] remembers, per
/// thread, the `(vm, argument class, instantiated target)` triples it has
/// already PROVED pass, and answers them again without taking a lock.
///
/// Why: a generic SAM (`Function<Integer,Integer>`, `BiConsumer<String,List>`)
/// reaches its impl through the replayed bridge `checkcast`, and every call
/// re-ran the whole verdict -- up to five `class_manager` read acquisitions
/// and an `is_lambda_proxy_class` lock -- to answer the same "passes" again
/// (`lambda_arg_cast_verdict` 3.2-4.8% of the w12 `Nio20JitRows optional` and
/// `Nio21TreeMapRows hdr` profiles, almost all of it `lock_shared`).
///
/// Soundness. Only the arms whose answer is a fact about the two CLASSES are
/// memoized: `obj_class_id` is (a subclass of) the loaded target, the
/// loader-scoped/loader-aware name identity, `synthetic_implements`, a lambda
/// proxy's own functional interface / supertype set, and the same-name
/// loader-split carve-out. Each of those is monotone for a given class pair:
/// a loaded class's supertypes never change (JVMS §5.3.5; a redefinition may
/// not change the hierarchy, JVMTI `RedefineClasses`), `ClassId`s are never
/// recycled (`ClassStore::add` appends) and lambda proxy registrations are
/// never removed. The per-OBJECT arms (annotation proxy, dynamic proxy
/// instance) and every fail-open default are recomputed on every call, as
/// before; a FAILING verdict is never memoized. The key carries the VM's
/// process-unique `vm_identity`, so two VMs on one thread never share a row.
///
/// Read once per VM ([`LambdaSwitches`], gci i14/sx).
#[inline]
fn lambda_cast_pass_memo_enabled(shared: &SharedVm) -> bool {
    shared.lambda_switches.cast_pass_memo
}

/// Rows in the per-thread cast-pass memo (direct-mapped).
const CAST_PASS_MEMO_ROWS: usize = 64;

/// One memo row: `(vm_identity, argument class id, target internal name)`.
type CastPassRow = Option<(usize, u32, Box<str>)>;

thread_local! {
    /// See [`lambda_cast_pass_memo_enabled`]. Direct-mapped: a collision
    /// evicts, which only costs the next call a full verdict.
    static LAMBDA_CAST_PASS_MEMO: std::cell::RefCell<Vec<CastPassRow>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The memo row for `(obj_class_id, target)`. `target` is the internal name
/// (`java/lang/String`), never empty on the paths that reach here.
#[inline]
fn cast_pass_memo_index(obj_class_id: ClassId, target: &str) -> usize {
    let b = target.as_bytes();
    let tail = b.last().copied().unwrap_or(0) as usize;
    let mid = b.get(b.len() / 2).copied().unwrap_or(0) as usize;
    let mut h = (obj_class_id.as_u32() as usize) ^ (b.len() << 20) ^ (tail << 8) ^ (mid << 14);
    h = h.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (h >> 32) % CAST_PASS_MEMO_ROWS
}

/// Has this thread already proved `obj_class_id` passes a cast to `target`
/// in this VM?
#[inline]
fn cast_pass_memo_hit(shared: &SharedVm, obj_class_id: ClassId, target: &str) -> bool {
    let on = lambda_cast_pass_memo_enabled(shared);
    cast_pass_memo_hit_keyed(on, shared.vm_identity, obj_class_id, target)
}

/// [`cast_pass_memo_hit`] with the VM's switch and identity spelled out
/// (gci i14/sx: the unit test drives both arms with no VM).
#[inline]
fn cast_pass_memo_hit_keyed(
    on: bool,
    vm_identity: usize,
    obj_class_id: ClassId,
    target: &str,
) -> bool {
    if !on {
        return false;
    }
    let at = cast_pass_memo_index(obj_class_id, target);
    LAMBDA_CAST_PASS_MEMO.with(|memo| {
        let Ok(memo) = memo.try_borrow() else {
            return false;
        };
        matches!(
            memo.get(at),
            Some(Some((vm, cid, tok)))
                if *vm == vm_identity && *cid == obj_class_id.as_u32() && &**tok == target
        )
    })
}

/// Record a PROVED pass (see [`lambda_cast_pass_memo_enabled`] for which
/// arms may call this).
#[inline]
fn cast_pass_memo_note(shared: &SharedVm, obj_class_id: ClassId, target: &str) {
    let on = lambda_cast_pass_memo_enabled(shared);
    cast_pass_memo_note_keyed(on, shared.vm_identity, obj_class_id, target);
}

/// [`cast_pass_memo_note`] with the VM's switch and identity spelled out.
#[inline]
fn cast_pass_memo_note_keyed(on: bool, vm_identity: usize, obj_class_id: ClassId, target: &str) {
    if !on {
        return;
    }
    let at = cast_pass_memo_index(obj_class_id, target);
    LAMBDA_CAST_PASS_MEMO.with(|memo| {
        let Ok(mut memo) = memo.try_borrow_mut() else {
            return;
        };
        if memo.len() < CAST_PASS_MEMO_ROWS {
            memo.resize_with(CAST_PASS_MEMO_ROWS, || None);
        }
        memo[at] = Some((vm_identity, obj_class_id.as_u32(), Box::from(target)));
    });
}

/// The decision behind [`lambda_arg_provably_not_instance`]: `Some(true)` when
/// the object is provably not an instance of `desc_tok`, `Some(false)` when the
/// cast passes (or fails open).
///
/// `may_load == false` is the safepoint-free form, for a caller holding the
/// object as a raw address it cannot re-read (the JIT's direct lambda arm,
/// whose arguments are compiled-code registers). It never loads a class, and
/// answers `None` where deciding would need a load — the caller must then
/// treat the cast as undecided and take the path that can load. `obj` is
/// read afresh after every step that may load, never cached across one.
pub(super) fn lambda_arg_cast_verdict(
    shared: &SharedVm,
    obj: &dyn Fn() -> ObjectRef,
    desc_tok: &str,
    may_load: bool,
) -> Option<bool> {
    // Array instantiated type: decide via the array-assignability rules.
    if desc_tok.starts_with('[') {
        let obj_ref = obj();
        if shared.mem.heap.kind_of(obj_ref) != cratonvm_types::ObjectKind::Array {
            return Some(false); // not an array — fail open
        }
        // The exact shapes first: this reads the header only, never loads.
        if let Some(assignable) = array_cast_fast_verdict(shared, obj_ref, desc_tok) {
            return Some(!assignable);
        }
        if !may_load {
            // Covariance needs both components resolved, which may load.
            return None;
        }
        return match array_descriptor_of(shared, obj_ref) {
            // `array_is_assignable_to` may load a component class; nothing
            // reads the object after it.
            Some(src) => Some(!array_is_assignable_to(shared, &src, desc_tok)),
            None => Some(false),
        };
    }
    let target = match desc_tok.strip_prefix('L').and_then(|d| d.strip_suffix(';')) {
        Some(t) => t,
        None => return Some(false),
    };
    if target == "java/lang/Object" {
        return Some(false);
    }
    // nio21 w18 L2: an ARRAY argument cast to a class or interface type. An
    // array header carries its COMPONENT's class id, so every arm below asked
    // about `String` when handed a `String[]`, and a `Function<String,..>`
    // applied (raw) to a `String[]` ran its body where HotSpot's bridge throws
    // `ClassCastException`. JVMS §4.10.1.2 / §6.5 `checkcast`: an array type
    // is assignable to `Object`, `Cloneable` and `java.io.Serializable` only.
    // Decided from the header, no load. Kill switch
    // `CRATONVM_LAMBDA_ARRAY_ARG_CAST=0` (asked only for an array argument).
    if shared.mem.heap.kind_of(obj()) == cratonvm_types::ObjectKind::Array
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_LAMBDA_ARRAY_ARG_CAST")
    {
        return Some(!matches!(
            target,
            "java/lang/Cloneable" | "java/io/Serializable"
        ));
    }
    // A class id is stable across a moving collection; the address is not.
    let obj_class_id = shared.mem.heap.class_id_of(obj());
    // nio21 w15 L5: a pass this thread already proved for this class pair
    // (`lambda_cast_pass_memo_enabled`), answered before any lock.
    if cast_pass_memo_hit(shared, obj_class_id, target) {
        return Some(false);
    }
    // Lambda proxies use VM-only synthetic class IDs which intentionally do not
    // have ClassStore metadata. Any other id without one has no class graph
    // to prove a mismatch with, so it keeps this helper's fail-open contract.
    if shared
        .classes
        .class_manager
        .read()
        .get_class(obj_class_id)
        .is_none()
    {
        return lambda_proxy_arg_cast_verdict(shared, obj_class_id, target, may_load);
    }
    // The bridge this is standing in for holds a real `checkcast`, and a
    // `checkcast` RESOLVES its target class (JVMS §5.4.3.1) before comparing.
    // So must this: "not loaded yet" is a statement about the class store, not
    // about the object, and answering `false` there skips the cast entirely for
    // every instantiated type whose first mention IS this call site.
    //
    // Spring's `ApplicationListener.forPayload` is exactly that shape. Its
    // lambda's instantiated parameter is `PayloadApplicationEvent`, which
    // nothing has referenced when the first `ContextRefreshedEvent` is
    // multicast — so the cast was skipped, the body ran on the wrong event, and
    // `event.getPayload()` raised `NoSuchMethodError`.
    // `SimpleApplicationEventMulticaster.doInvokeListener` catches
    // `ClassCastException` for precisely this case ("possibly a lambda-defined
    // listener which we could not resolve the generic event type for") and
    // suppresses it; a `NoSuchMethodError` walks straight past that catch and
    // fails the context refresh
    // (`DevToolsR2dbcAutoConfigurationTests$Pooled`,
    // `probes/SpringForPayloadListenerProbe.java` — whose `preload` argument
    // resolves the type up front and made the same run pass, which is what
    // identified this branch).
    //
    // A load that FAILS still fails open: an instantiated type that is not on
    // the classpath at all is the one case where guessing is worse than
    // deferring, and it leaves the pre-existing behaviour untouched.
    let loaded_target = {
        let cm = shared.classes.class_manager.read();
        cm.get_loaded_class_id(target)
    };
    let loaded_target = match loaded_target {
        Some(tcid) => tcid,
        // The safepoint-free form cannot resolve; the caller's loading path
        // will, and every later call then finds the class here.
        None if !may_load => return None,
        // Resolve it, exactly as the bridge's `checkcast` would on first
        // execution. `load_class_concurrent` loads and links without running
        // `<clinit>`, which is the resolution a `checkcast` performs.
        None => match shared.load_class_concurrent(target) {
            Ok(tcid) => tcid,
            Err(_) => return Some(false),
        },
    };
    let (target_cid, is_sub, target_is_interface) = {
        let cm = shared.classes.class_manager.read();
        (
            loaded_target,
            obj_class_id == loaded_target || cm.is_subclass_of(obj_class_id, loaded_target),
            cm.get_class(loaded_target)
                .map(|c| c.is_interface())
                .unwrap_or(false),
        )
    };
    // The lambda bridge descriptor carries a binary name only.  In a forked
    // class-loader run the loader-blind lookup above may select the app copy
    // of that name even though the value (and the bridge that owns it) use a
    // child-defined copy.  Consult the value's exact defining namespace before
    // calling the mismatch proven; this is the same identity rule used by the
    // loader-aware checkcast path.  Restrict it to user loaders so ordinary
    // bootstrap/application delegation remains unchanged.
    let loader_scoped_is_sub = if crate::runtime::env_cache::loader_aware_resolution() {
        let cm = shared.classes.class_manager.read();
        cm.get_loader_id(obj_class_id)
            .filter(|loader| matches!(loader, cratonvm_types::ClassLoaderId::UserDefined(_)))
            .and_then(|loader| cm.class_defined_by_loader_exact(target, loader))
            .is_some_and(|scoped_target| cm.is_subclass_of(obj_class_id, scoped_target))
    } else {
        false
    };
    // AOTSVC-1: the checks above only prove a match via ClassId identity
    // (`is_sub`) or an exact defining-loader-namespace lookup
    // (`loader_scoped_is_sub`, which requires the object's OWN loader to have
    // already resolved `target` under its own namespace). Neither covers the
    // case exercised by `AotServices.factories().load(...)`-style SPI
    // discovery (Spring's `SpringFactoriesLoader.instantiateFactory`):
    // `ClassUtils.forName(implementationName, TCCL)` + `Constructor.newInstance`
    // allocate the service object using the EXACT ClassId resolved through the
    // caller's classloader argument, but never drive that same loader's
    // `loadClass` for the interface types the service implements — so
    // `class_defined_by_loader_exact(target, obj's loader)` can miss even
    // though the object's own `interfaces` list (populated at define/link time
    // from its own class file) already carries a same-named entry. This is the
    // identical name-vs-identity gap `loader_aware_name_assignable` already
    // closes for the ordinary bytecode `checkcast` opcode — reuse it here so
    // direct lambda dispatch (which bypasses that opcode, see this function's
    // caller) gets the same loader-faithful answer instead of a false
    // `ClassCastException` for a same-named, different-loader interface copy
    // (e.g. `TestRuntimeHintsRegistrar` under `@CompileWithForkedClassLoader`).
    //
    // The screens that never load run first, and the object is re-read after
    // the target load above rather than reused from before it.
    //
    // nio21 w15 L5: same screens in the same order; only the class-pair ones
    // (everything but the annotation proxy's per-object answer) are memoized.
    if is_sub || loader_scoped_is_sub || synthetic_implements(shared, obj_class_id, target) {
        cast_pass_memo_note(shared, obj_class_id, target);
        return Some(false);
    }
    if annotation_proxy_satisfies_target(shared, obj(), target) {
        return Some(false);
    }
    if loader_aware_name_assignable(shared, obj_class_id, target_cid, target) {
        cast_pass_memo_note(shared, obj_class_id, target);
        return Some(false);
    }
    // The two screens that may load: a lambda proxy's functional interface and
    // markers (`load_class_concurrent`), and a dynamic proxy's interface list
    // (`load_class` when the target name is not unique). Each is a no-op for
    // an object that is not of its kind, so the safepoint-free form runs them
    // only when they cannot load, and is undecided otherwise.
    if may_load {
        if lambda_proxy_satisfies(shared, obj_class_id, target_cid) {
            cast_pass_memo_note(shared, obj_class_id, target);
            return Some(false);
        }
        if proxy_instance_satisfies_target(shared, obj(), target) {
            return Some(false);
        }
    } else {
        if shared.classes.is_lambda_proxy_class(obj_class_id) {
            return None;
        }
        let target_unique = shared
            .classes
            .class_manager
            .read()
            .find_unique_class_by_name(target)
            .is_some();
        if target_unique {
            if proxy_instance_satisfies_target(shared, obj(), target) {
                return Some(false);
            }
        } else if class_is_generated_proxy_or_shim(shared, obj_class_id) {
            return None;
        }
    }
    // Loader-split carve-out (gated): under loader-faithful resolution the same
    // logical type can exist as several per-loader copies (a Hibernate
    // bytecode-enhanced entity and its un-enhanced global copy, distinct
    // `ClassId`s sharing a name). `get_loaded_class_id(target)` picks only ONE,
    // so a lambda-arg `checkcast` to that type (e.g. adapting the receiver of a
    // `ImmutableEntity::getName` method reference) would spuriously reject an
    // instance of the OTHER copy — `X cannot be cast to X`. Treat an object
    // whose class NAME equals the target's as an instance. Gate-off unchanged;
    // only same-named cross-loader pairs are affected, never distinct types.
    if crate::runtime::env_cache::loader_aware_resolution() {
        let same_name = shared
            .classes
            .class_manager
            .read()
            .get_class(obj_class_id)
            .map(|c| &*c.name == target)
            .unwrap_or(false);
        if same_name {
            cast_pass_memo_note(shared, obj_class_id, target);
            return Some(false);
        }
    }
    // Provenance carve-out: a bare `java/lang/Object` receiver (`cid == 0`, or a
    // synthetic alloc that lost its class identity and now reports
    // `java/lang/Object`) carries no interface table, so we cannot prove it does
    // NOT implement an interface target. Many VM-synthesised objects that really
    // do implement marker interfaces (`java/io/Serializable`, `Comparable`, a
    // functional interface, …) on HotSpot land here. Only a *concrete-class*
    // target can be soundly rejected for such an object (e.g. `Object` → `String`
    // in the motivating `Map<String,Object>.forEach` case). For an interface
    // target, fail open. This never weakens the class-narrowing fix.
    //
    // Some early synthetic stubs are later used as interface edges before their
    // own `ACC_INTERFACE` metadata is trustworthy. Elasticsearch's
    // `Writeable$Writer.write` bridge is one such path: the target
    // `Writeable` id is present in implementors' `interfaces` lists, but a
    // concurrently observed value can carry the bare `Object` stamp. Treat that
    // target as interface-like too, matching the conservative posture above.
    let obj_is_bare = obj_class_id == ClassId::new(0)
        || shared
            .classes
            .class_manager
            .read()
            .get_class(obj_class_id)
            .map(|c| &*c.name == "java/lang/Object")
            .unwrap_or(true);
    // nio21 w18 L2 (host w18 `Nio21LambdaCast` `ser object`): the carve-out
    // above is for a VM-synthesised carrier that LOST its class identity and so
    // carries state. A field-less plain object stamped `java/lang/Object` is a
    // genuine `new Object()`: it implements no interface, and HotSpot's proxy
    // `checkcast` fails (`class java.lang.Object cannot be cast to class
    // java.io.Serializable ...`). Fail-open for it let the body run and die on
    // an `IncompatibleClassChangeError` at its first `invokeinterface`. Kill
    // switch `CRATONVM_LAMBDA_BARE_OBJECT_CAST=0` (asked only on this arm).
    let genuine_plain_object = obj_is_bare && {
        let obj_ref = obj();
        shared.mem.heap.kind_of(obj_ref) == cratonvm_types::ObjectKind::Object
            && shared.mem.heap.num_fields(obj_ref) == 0
            && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_LAMBDA_BARE_OBJECT_CAST")
    };
    if obj_is_bare
        && !genuine_plain_object
        && (target_is_interface || target_used_as_interface(shared, target_cid))
    {
        return Some(false);
    }
    Some(true)
}

/// [`lambda_arg_cast_verdict`] for an argument whose class id has no
/// class-store entry: a lambda proxy is cast to `target` the way its spun
/// class's bridge `checkcast` would, from the supertypes the proxy is known to
/// have (`Object`, its functional interface, its markers and
/// `Serializable` when flagged: the set `checkcast` asks
/// [`lambda_proxy_satisfies`]). Before interpreter round i1 wave 50 (lane L4,
/// `L7W49LambdaArgCheck`) every proxy argument failed open and the body ran
/// on it, failing at its first use of the wrong type (`NoSuchMethodError:
/// 'int java.lang.Runnable.length()'` for a `String` parameter).
///
/// Any other id without an entry still fails open (`Some(false)`). The
/// safepoint-free form (`may_load == false`) decides only the common match,
/// a target that IS the proxy's functional interface, and is undecided
/// (`None`) otherwise: the caller's loading form decides.
#[inline(never)]
fn lambda_proxy_arg_cast_verdict(
    shared: &SharedVm,
    obj_class_id: ClassId,
    target: &str,
    may_load: bool,
) -> Option<bool> {
    if !shared.classes.is_lambda_proxy_class(obj_class_id) {
        return Some(false);
    }
    if !may_load {
        let is_own_interface = shared
            .classes
            .lambda_proxies
            .read()
            .get(&obj_class_id)
            .is_some_and(|site| &*site.functional_interface == target);
        if is_own_interface {
            // A registered proxy's functional interface never changes.
            cast_pass_memo_note(shared, obj_class_id, target);
            return Some(false);
        }
        return None;
    }
    let loaded = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(target);
    let target_cid = match loaded {
        Some(cid) => cid,
        None => match shared.load_class_concurrent(target) {
            Ok(cid) => cid,
            // An instantiated type that does not load fails open, as the
            // class-store arm does.
            Err(_) => return Some(false),
        },
    };
    if lambda_proxy_satisfies(shared, obj_class_id, target_cid) {
        cast_pass_memo_note(shared, obj_class_id, target);
        return Some(false);
    }
    Some(true)
}

pub(super) fn target_used_as_interface(shared: &SharedVm, target_cid: ClassId) -> bool {
    let cm = shared.classes.class_manager.read();
    let is_interface = cm
        .class_store()
        .iter()
        .any(|class| class.interfaces.iter().any(|iface| *iface == target_cid));
    is_interface
}

/// Widen a primitive value from `from_tok` to `to_tok` per JVM numeric promotion.
pub(super) fn widen_primitive(from_tok: &str, to_tok: &str, v: Value) -> Value {
    let as_i32 = |v: &Value| -> Option<i32> {
        match v {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    };
    match (from_tok, to_tok, &v) {
        ("I" | "B" | "S" | "C" | "Z", "J", _) => {
            if let Some(i) = as_i32(&v) {
                // Widening: i32 -> i64 (sign-extended, JVM i2l)
                return Value::Long(i as i64);
            }
        }
        ("I" | "B" | "S" | "C" | "Z", "F", _) => {
            if let Some(i) = as_i32(&v) {
                // Cast: integer-to-float numeric conversion (JVM i2f/i2d/l2f/l2d semantics)
                return Value::Float(i as f32);
            }
        }
        ("I" | "B" | "S" | "C" | "Z", "D", _) => {
            if let Some(i) = as_i32(&v) {
                // Cast: integer-to-float numeric conversion (JVM i2f/i2d/l2f/l2d semantics)
                return Value::Double(i as f64);
            }
        }
        // Cast: integer-to-float numeric conversion (JVM i2f/i2d/l2f/l2d semantics)
        ("J", "F", Value::Long(l)) => return Value::Float(*l as f32),
        // Cast: integer-to-float numeric conversion (JVM i2f/i2d/l2f/l2d semantics)
        ("J", "D", Value::Long(l)) => return Value::Double(*l as f64),
        // Cast: numeric/representation conversion
        ("F", "D", Value::Float(f)) => return Value::Double(*f as f64),
        _ => {}
    }
    v
}

/// Coerce the return value from the impl descriptor back to the SAM descriptor.
pub fn coerce_return(
    shared: &SharedVm,
    thread: &mut JvmThread,
    sam_ret: &str,
    impl_ret: &str,
    v: Option<Value>,
) -> Result<Option<Value>, MethodCallFailed> {
    if sam_ret == "V" {
        return Ok(None);
    }
    let raw = match v {
        Some(x) => x,
        None => return Ok(None),
    };
    if sam_ret == impl_ret {
        return Ok(Some(raw));
    }
    // SAM expects reference (Object/Integer/etc), impl returned primitive — box.
    if is_reference_desc(sam_ret) && is_primitive_desc(impl_ret) {
        let ch = impl_ret.chars().next().ok_or_else(|| {
            MethodCallFailed::InternalError(VmError::Internal {
                message: "empty primitive descriptor token in coerce_return".to_string(),
            })
        })?;
        let boxed = box_primitive(shared, thread, ch, raw)?;
        return Ok(Some(boxed));
    }
    // SAM expects primitive, impl returned reference — unbox.
    if is_primitive_desc(sam_ret) && is_reference_desc(impl_ret) {
        // A `null` return cannot be unboxed: HotSpot's spun class raises a
        // message-less `NullPointerException` (wave 23, lane L4; see
        // `coerce_arg`). It used to hand the null back as the primitive.
        if matches!(raw, Value::Object(None)) {
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
        let ch = sam_ret.chars().next().ok_or_else(|| {
            MethodCallFailed::InternalError(VmError::Internal {
                message: "empty primitive descriptor token in coerce_return".to_string(),
            })
        })?;
        if is_wrapper_desc(impl_ret) {
            return Ok(Some(unbox_wrapper(shared, ch, raw)));
        }
        return unbox_lambda_return_via_supertype(shared, thread, ch, raw).map(Some);
    }
    // Both primitive: maybe widen.
    if is_primitive_desc(sam_ret) && is_primitive_desc(impl_ret) {
        return Ok(Some(widen_primitive(impl_ret, sam_ret, raw)));
    }
    Ok(Some(raw))
}

/// Whether `token` is the descriptor of one of the eight primitive wrappers.
fn is_wrapper_desc(token: &str) -> bool {
    matches!(
        token,
        "Ljava/lang/Integer;"
            | "Ljava/lang/Long;"
            | "Ljava/lang/Short;"
            | "Ljava/lang/Byte;"
            | "Ljava/lang/Character;"
            | "Ljava/lang/Boolean;"
            | "Ljava/lang/Float;"
            | "Ljava/lang/Double;"
    )
}

/// A lambda's return of a reference type that is NOT a wrapper (`Object`,
/// `Number`, `String`, ...) into a primitive SAM return `prim`: what the
/// JDK's spun class does (`TypeConvertingMethodAdapter.convertType`, a
/// reference that is not a wrapper "is a super type of the target"): cast to
/// `Number` for a numeric `prim` (`Character` for `char`, `Boolean` for
/// `boolean`) and unbox through it, so `Number.intValue()` narrows a `Long`
/// and a `String` is `ClassCastException: class java.lang.String cannot be
/// cast to class java.lang.Number`.
///
/// Interpreter round i1 wave 50, lane L4 (`L4W49ReflectiveMetafactory` row
/// `bad-return`, a reflective `metafactory` with an `int` SAM over a
/// `String`-returning implementation): the wrapper read of field 0 handed the
/// `String`'s `value` array to the `int` return slot and PANICKED the VM.
#[cold]
#[inline(never)]
fn unbox_lambda_return_via_supertype(
    shared: &SharedVm,
    thread: &mut JvmThread,
    prim: char,
    raw: Value,
) -> Result<Value, MethodCallFailed> {
    let Value::Object(Some(obj)) = raw else {
        // Already a primitive (a native implementation), or null (refused by
        // the caller).
        return Ok(raw);
    };
    let intermediate = match prim {
        'C' => "java/lang/Character",
        'Z' => "java/lang/Boolean",
        _ => "java/lang/Number",
    };
    let obj_class_id = shared.mem.heap.class_id_of(obj);
    let (obj_name, is_instance) = {
        let cm = shared.classes.class_manager.read();
        let name = cm.get_class(obj_class_id).map(|c| c.name.to_string());
        let is_instance = match (&name, cm.get_loaded_class_id(intermediate)) {
            (Some(n), _) if n == intermediate => true,
            (Some(_), Some(target)) => cm.is_subclass_of(obj_class_id, target),
            _ => false,
        };
        (name, is_instance)
    };
    if !is_instance {
        let shown = match &obj_name {
            // Wave 25, lane hexname (`CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`):
            // by the value's BOTTOM class's hidden flag, shape rule as fallback.
            Some(n) => {
                let display = cce_display_class_name(shared, obj, n);
                crate::runtime::exceptions::external_name_by_bottom_class_or(
                    &display,
                    crate::runtime::exceptions::bottom_class_hidden(shared, obj_class_id, &display),
                    crate::runtime::exceptions::hotspot_external_name,
                )
            }
            None => lambda_proxy_display_name(shared, obj_class_id)
                .unwrap_or_else(|| format!("?class_id={obj_class_id}")),
        };
        return Err(RuntimeError::ClassCastException {
            message: format!(
                "{shown} cannot be cast to {}",
                crate::runtime::exceptions::hotspot_external_name(&intermediate)
            ),
        }
        .into());
    }
    let standard = matches!(
        obj_name.as_deref(),
        Some(
            "java/lang/Integer"
                | "java/lang/Long"
                | "java/lang/Short"
                | "java/lang/Byte"
                | "java/lang/Float"
                | "java/lang/Double"
                | "java/lang/Character"
                | "java/lang/Boolean"
        )
    );
    if standard {
        return Ok(number_value_as(prim, shared.mem.heap.get_field(obj, 0)));
    }
    // Another `Number` (`BigDecimal`, `AtomicLong`, a user subclass): its own
    // `xxxValue()`, as the spun class calls it.
    let (method, desc) = match prim {
        'B' => ("byteValue", "()B"),
        'S' => ("shortValue", "()S"),
        'J' => ("longValue", "()J"),
        'F' => ("floatValue", "()F"),
        'D' => ("doubleValue", "()D"),
        _ => ("intValue", "()I"),
    };
    use cratonvm_native_api::NativeContext as _;
    // gci i17/na: not under `safe_native_call_impl`; a Rust-side allocation
    // on the way into `xxxValue()` is an `OutOfMemoryError` of this call
    // instead of the infallible arm's abort.
    let answered = crate::runtime::native_oom::catch_alloc_oom_on(thread, |thread| {
        let mut ctx = crate::vm::NativeContextImpl { shared, thread };
        ctx.invoke_virtual(obj, method, desc, &[])
    })
    .map_err(|oom| MethodCallFailed::from(oom.into_runtime_error()))?;
    Ok(answered?.unwrap_or(Value::Int(0)))
}

/// A wrapper's primitive `value` converted to `prim` the way
/// `Number.<prim>Value()` converts it (a primitive cast).
fn number_value_as(prim: char, value: Value) -> Value {
    let (as_long, as_double) = match value {
        Value::Int(i) => (i as i64, i as f64),
        Value::Long(l) => (l, l as f64),
        // Java's `(int)`/`(long)` of a floating value saturate and map NaN
        // to zero, as Rust's `as` does.
        Value::Float(f) => (f as i64, f as f64),
        Value::Double(d) => (d as i64, d),
        other => return other,
    };
    let is_floating = matches!(value, Value::Float(_) | Value::Double(_));
    let as_int = match value {
        Value::Float(f) => f as i32,
        Value::Double(d) => d as i32,
        _ => as_long as i32,
    };
    match prim {
        'I' | 'C' | 'Z' => Value::Int(as_int),
        'B' => Value::Int(as_int as i8 as i32),
        'S' => Value::Int(as_int as i16 as i32),
        'J' => Value::Long(as_long),
        'F' => Value::Float(match value {
            Value::Float(f) => f,
            _ if is_floating => as_double as f32,
            _ => as_long as f32,
        }),
        'D' => Value::Double(as_double),
        _ => value,
    }
}

/// Coerce arguments passed to a lambda SAM invocation to match the impl
/// method's descriptor.
///
/// `sam_desc` and `impl_desc` are method descriptors. `call_args` are the
/// SAM-level args (no captures). `captures_and_args` is `captures ++
/// call_args`. For `InvokeVirtual`/`InvokeInterface`, the first element of
/// `captures_and_args` is the receiver and should NOT be coerced against
/// impl_desc params (impl_desc params describe method params, not the
/// receiver). `receiver_present` distinguishes these cases.
pub fn coerce_lambda_args(
    shared: &SharedVm,
    thread: &mut JvmThread,
    sam_desc: &str,
    impl_desc: &str,
    inst_desc: &str,
    args: &mut Vec<Value>,
    receiver_present: bool,
    num_captures: usize,
) -> Result<(), MethodCallFailed> {
    // Provable no-op fast path, taken by every non-capturing lambda whose SAM,
    // implementation and instantiated types agree — `x -> x + 1`, `Foo::bar`,
    // and the overwhelming majority of the lambdas a reactive stack executes.
    //
    // With all three descriptors identical, `num_captures == 0` and no receiver,
    // `args` lines up token-for-token with both parameter lists, so:
    //   * every `coerce_arg(tok, tok, v)` returns `v` untouched (its first line
    //     is `if sam_tok == impl_tok { return Ok(v) }`), and
    //   * `checkcast_lambda_instantiated_args` skips every parameter, because it
    //     only casts where the instantiated type NARROWS the erased SAM type.
    // The work below is therefore pure overhead here: two descriptor walks, a
    // `handles` vector, and one pin push/truncate per argument, on every call.
    if num_captures == 0 && !receiver_present && sam_desc == impl_desc && sam_desc == inst_desc {
        return Ok(());
    }

    let (sam_params, _sam_ret) = split_method_descriptor_ref(sam_desc);
    let (impl_params, _impl_ret) = split_method_descriptor_ref(impl_desc);

    // The SAM's params correspond to args[num_captures..].
    // The impl's params correspond to args[receiver_skip..] where
    // receiver_skip = 1 if receiver_present else 0.
    // Number of non-receiver impl args should equal (total - receiver_skip).
    let receiver_skip = if receiver_present { 1 } else { 0 };
    if args.len() < receiver_skip {
        return Ok(());
    }

    // GC-safety: the instantiated-type checkcast below can LOAD classes and
    // `coerce_arg` BOXES primitives — both allocate and can trigger a moving
    // young GC while `args` sits in this raw Rust Vec, invisible to the
    // collector (native stale-local family). Pin every object entry into
    // `native_pin_roots` (which the GC remaps), refresh the Vec from the pins
    // after every potentially-allocating step, and keep each pin tracking the
    // CURRENT object for its index as entries are coerced. Pins are truncated
    // on every exit path.
    let pin_base = thread.native_pin_roots.len();
    let mut handles: Vec<Option<usize>> = Vec::with_capacity(args.len());
    for a in args.iter() {
        if let Value::Object(Some(o)) = a {
            handles.push(Some(thread.native_pin_roots.len()));
            thread.native_pin_roots.push(*o);
        } else {
            handles.push(None);
        }
    }

    // LambdaMetafactory argument adaptation: replay the `checkcast` to each
    // instantiated parameter type that the (bypassed) synthetic SAM bridge would
    // have performed, so a narrowed type variable still raises
    // `ClassCastException` for an incompatible argument. Runs before the
    // box/unbox coercion below, mirroring the bridge's cast-then-adapt order.
    if let Err(e) = checkcast_lambda_instantiated_args(
        shared,
        thread,
        &handles,
        sam_desc,
        inst_desc,
        args,
        num_captures,
    ) {
        thread.native_pin_roots.truncate(pin_base);
        return Err(e);
    }
    for (j, h) in handles.iter().enumerate() {
        if let Some(h) = *h {
            args[j] = Value::Object(Some(thread.native_pin_roots[h]));
        }
    }

    // The SAM-supplied args start at index num_captures in `args` (captures
    // come first). Captures themselves may also need boxing if bound as the
    // impl's receiver or first params, but for now we focus on the SAM args,
    // which is where the primitive/reference mismatch occurs.
    let impl_non_recv = &impl_params[..];
    // We want to coerce each arg[i] against the corresponding impl param.
    for i in 0..args.len() {
        if i < receiver_skip {
            continue;
        }
        let impl_idx = i - receiver_skip;
        if impl_idx >= impl_non_recv.len() {
            break;
        }
        // Determine what the caller's "expected" type was. For SAM-supplied
        // args (i >= num_captures), use sam_params[i - num_captures]. For
        // capture args (i < num_captures), we assume they match impl type
        // already (captures are erased at capture time).
        let sam_tok: &str = if i >= num_captures {
            let sam_idx = i - num_captures;
            if sam_idx < sam_params.len() {
                sam_params[sam_idx]
            } else {
                impl_non_recv[impl_idx]
            }
        } else {
            impl_non_recv[impl_idx]
        };
        let impl_tok = impl_non_recv[impl_idx];
        let coerced = match coerce_arg(shared, thread, sam_tok, impl_tok, args[i]) {
            Ok(v) => v,
            Err(e) => {
                thread.native_pin_roots.truncate(pin_base);
                return Err(e);
            }
        };
        // Keep this index's pin tracking the (possibly freshly boxed) object.
        match coerced {
            Value::Object(Some(o)) => {
                if let Some(h) = handles[i] {
                    thread.native_pin_roots[h] = o;
                } else {
                    handles[i] = Some(thread.native_pin_roots.len());
                    thread.native_pin_roots.push(o);
                }
            }
            _ => handles[i] = None,
        }
        args[i] = coerced;
        // coerce_arg may have allocated (boxing) — refresh every pinned entry.
        for (j, h) in handles.iter().enumerate() {
            if let Some(h) = *h {
                args[j] = Value::Object(Some(thread.native_pin_roots[h]));
            }
        }
    }
    thread.native_pin_roots.truncate(pin_base);
    Ok(())
}

/// Bug B: distinguish the SAM from a same-name, same-arity *overloaded default*
/// method on the functional interface. A functional interface may declare
/// default methods named like the SAM with the same arity but different
/// parameter types — e.g. `AnnotationFilter`'s SAM `matches(String)` plus
/// defaults `matches(Class)` / `matches(Annotation)`. The arity guard in
/// `try_lambda_dispatch` can't tell them apart, so `FILTER.matches(someClass)`
/// was wrongly routed into the `matches(String)` lambda body (passing a Class
/// where a String was expected → the lambda always returned false).
///
/// Returns `false` when the call is such an overloaded default (so the caller
/// falls through and runs the real default method, which converts the argument
/// and re-invokes the SAM). Only CONCRETE (non-`Object`) reference SAM params
/// are checked; generic/erased (`Object`) and primitive params are skipped, so
/// the hot stream/lambda path stays byte-identical. A param is treated as
/// compatible unless the runtime arg is a non-null object provably NOT an
/// instance of the SAM param type (mirrors the `instanceof` opcode's checks).
///
/// SAFEPOINT-FREE: `args` are raw addresses the caller cannot re-read, so no
/// screen here may load a class. The two screens that could (a lambda-proxy
/// argument's functional interface, a dynamic proxy's interface list when the
/// target name is not unique) fail open instead — the same "can't judge →
/// compatible" answer as an unloaded SAM param type.
pub(crate) fn lambda_args_sam_compatible(
    shared: &SharedVm,
    sam_descriptor: &str,
    args: &[Value],
) -> bool {
    let (params, _ret) = split_method_descriptor_ref(sam_descriptor);
    for (i, pd) in params.iter().enumerate() {
        if pd.starts_with('[') {
            // Array-typed SAM param. This was previously covered by the
            // `!pd.starts_with('L')` catch-all below (arrays don't start
            // with 'L'), which unconditionally skipped it -- "never
            // second-guess". That silently let a same-named, same-arity
            // interface DEFAULT method whose one differing parameter is a
            // scalar reference where the real SAM wants an array (e.g.
            // JRuby 10.x's `BlockCallback` -- abstract SAM
            // `call(ThreadContext, IRubyObject[], Block)` plus five
            // default overloads sharing the name "call", including
            // `call(ThreadContext, IRubyObject, Block)`) get misjudged as
            // SAM-compatible. `try_lambda_dispatch` then fed the raw
            // scalar argument directly into the array-typed lambda body
            // instead of falling through to the real default method (which
            // wraps the scalar into a 1-element array before re-invoking
            // the SAM) -- observed as `RubyEnumerable.packEnumValues`
            // calling `arraylength` on a bare `RubySymbol` during
            // `Enumerable#partition`'s per-element block callback
            // (JRubyScriptTemplateTests GC-ARRAY-GUARD investigation,
            // 2026-07-15). A present, non-null, non-array argument here is
            // provably NOT an instance of this SAM param -> treat as an
            // overloaded default, same as the concrete-class mismatch case
            // below.
            if let Some(Value::Object(Some(a))) = args.get(i) {
                if shared.mem.heap.kind_of(*a) != cratonvm_types::ObjectKind::Array {
                    return false;
                }
            }
            continue; // null / missing / genuinely an array -- don't second-guess further
        }
        if !pd.starts_with('L') || *pd == "Ljava/lang/Object;" {
            continue; // generic/erased or non-reference param -- never second-guess
        }
        let arg = match args.get(i) {
            Some(Value::Object(Some(a))) => *a,
            _ => continue, // null / primitive / missing — don't second-guess
        };
        let target = &pd[1..pd.len() - 1];
        let arg_cid = shared.mem.heap.class_id_of(arg);
        let (target_cid, base) = {
            let cm = shared.classes.class_manager.read();
            match cm.get_loaded_class_id(target) {
                Some(tcid) => (tcid, arg_cid == tcid || cm.is_subclass_of(arg_cid, tcid)),
                None => continue, // SAM param type not loaded — can't judge → compatible
            }
        };
        if base
            || loader_aware_name_assignable(shared, arg_cid, target_cid, target)
            || synthetic_implements(shared, arg_cid, target)
            || annotation_proxy_satisfies_target(shared, arg, target)
        {
            continue;
        }
        // A lambda-proxy argument is answered by `lambda_proxy_satisfies`,
        // which may load its functional interface: undecidable here, so
        // compatible. (It used to be called, after the raw `arg` had already
        // been read, with later arguments still to be read raw.)
        if shared.classes.is_lambda_proxy_class(arg_cid) {
            continue;
        }
        // `proxy_instance_satisfies_target` loads only when the target name is
        // not unique; the non-loading form of the same split as
        // `lambda_arg_cast_verdict(.., may_load = false)`.
        let target_unique = shared
            .classes
            .class_manager
            .read()
            .find_unique_class_by_name(target)
            .is_some();
        if target_unique {
            if proxy_instance_satisfies_target(shared, arg, target) {
                continue;
            }
        } else if class_is_generated_proxy_or_shim(shared, arg_cid) {
            continue;
        }
        return false; // arg provably not an instance of a concrete SAM param → overloaded default
    }
    true
}

/// Does the lambda proxy `proxy_class_id` (whose call site is `call_site`)
/// implement `method_name` + `descriptor` as a way into its body?
///
/// The ONE predicate for every lambda-dispatch copy (`try_lambda_dispatch`,
/// `NativeContextImpl::invoke_virtual`, the stale-receiver recovery in
/// `vm_exec.rs`), so they cannot disagree again. The spun class implements
/// exactly the SAM descriptor plus any `altMetafactory` `FLAG_BRIDGES`
/// descriptor of the SAM name (`record_lambda_proxy_bridges`). A same-named
/// default overload on the functional interface is neither, so it keeps
/// running its own bytecode (commit a857ee34c, `InetAddressFilter`).
///
/// On `true` the caller runs the SAM path with `descriptor` in the SAM
/// descriptor's place: `coerce_lambda_args` / `coerce_return` then convert
/// bridge → impl exactly as HotSpot's spun bridge does (cast, unbox, box).
#[inline]
pub(crate) fn lambda_accepts_descriptor(
    shared: &SharedVm,
    proxy_class_id: ClassId,
    call_site: &crate::classloading::resolution::LambdaCallSite,
    method_name: &str,
    descriptor: &str,
) -> bool {
    method_name == &*call_site.sam_method_name
        && (descriptor == &*call_site.sam_descriptor
            || crate::runtime::invokedynamic::lambda_proxy_has_bridge(
                shared.vm_identity,
                proxy_class_id,
                descriptor,
            ))
}

/// Loader-faithful dispatch target for a lambda's implementation method.
///
/// The `invokedynamic` that materialised this lambda lives in the class recorded
/// in `lambda_proxy_hosts` (its enclosing/defining class). Resolve the impl
/// method's owner through THAT class's loader (JVMS §5.4.3 initiating loader) so
/// a lambda whose enclosing class was defined by a child / bytecode-enhancing
/// loader dispatches to that loader's copy of the impl method — not the flat
/// global copy the by-name `invoke_shared`/`load_class` path would pick. This
/// mirrors the `invokespecial`/`invokevirtual` divergence override already
/// applied to ordinary bytecode dispatch (search "dispatch_override").
///
/// Concrete failure without this: a Hibernate `@BytecodeEnhanced` test's
/// `s -> { new Continent(); ... }` lambda ran the UN-enhanced host copy, so
/// `new Continent` resolved the un-enhanced entity and the reflective
/// `Field.set` on it mismatched the enhanced mapped class.
///
/// Returns the loader-local `ClassId` only when it diverges from the by-name
/// resolution; `None` (gate off, built-in host loader, or no per-loader copy)
/// keeps the legacy name-based dispatch byte-for-byte.
pub(crate) fn lambda_impl_dispatch_override(
    shared: &SharedVm,
    call_site: &crate::classloading::resolution::LambdaCallSite,
) -> Option<ClassId> {
    if !crate::runtime::env_cache::loader_aware_resolution() {
        return None;
    }
    // Nothing to override when no user-defined loader has ever defined a class
    // in this process: `lookup_loader_initiated` below already returns `None`
    // unless `get_loader_id(host)` is `UserDefined(_)`, and this atomic is
    // exactly the "could that ever be true" question — `register_defining_loader`
    // is called whenever any `ClassId` is assigned a `UserDefined` identity, so
    // `false` guarantees no class anywhere has one. Behaviour-preserving; the
    // same short-circuit, for the same reason, already sits inside
    // `lookup_loader_initiated`.
    //
    // Hoisted here because the lambda path reached it only AFTER a
    // `lambda_proxy_hosts` read lock + hash, and paid that per LAMBDA CALL.
    // Measured on a lambda-only profile: this function plus its `_driven`
    // sibling plus `lambda_global_impl_owner` were 11.3% of the run, essentially
    // all of it re-deriving a per-proxy constant that is `None` for every
    // program without a custom classloader.
    if !cratonvm_native_builtins::classloader::any_defining_loader_registered(&shared.loader_identity) {
        return None;
    }
    let host = *shared
        .classes
        .lambda_proxy_hosts
        .read()
        .get(&call_site.proxy_class_id)?;
    let name = &call_site.impl_handle.class_name;
    lookup_loader_initiated(shared, host, name).filter(|cid| *cid != ClassId::new(0))
}

/// Like [`lambda_impl_dispatch_override`], but for the call site that actually
/// EXECUTES a static method-reference lambda's impl method (as opposed to the
/// read-only callers above that only ever consult an already-populated cache).
///
/// A static method reference (e.g. `EnvironmentPostProcessorsFactory::
/// fromSpringFactories`) captured by a `@CompileWithForkedClassLoader`-style
/// isolated loader's own class can be the VERY FIRST reference to its impl
/// owner from that loader's namespace — before any other bytecode (`new`,
/// `checkcast`, `invokestatic`) has driven that loader's `loadClass` and
/// populated `initiating_resolution_cache`/the loader's exact-class index.
/// `lambda_impl_dispatch_override`'s `lookup_loader_initiated` only reads that
/// cache; on a cold miss it returns `None` and the caller falls through to the
/// loader-blind `invoke_shared(class_name, ...)`, which resolves to whichever
/// same-named class the FLAT global store already holds (typically an
/// unrelated, earlier-loaded Application-loader copy) — running the wrong
/// loader's impl method body entirely, not just naming the wrong `Class`
/// object. Concrete failure: `SpringApplication`'s
/// `EnvironmentPostProcessorApplicationListener` (fork-loader-defined)
/// constructs `postProcessorsFactory = EnvironmentPostProcessorsFactory::
/// fromSpringFactories` and calls `.apply(classLoader)` before anything else
/// in the fork ever touches `EnvironmentPostProcessorsFactory` by name; the
/// resulting `new SpringFactoriesEnvironmentPostProcessorsFactory(...)` ran
/// under the Application loader's copy, so a sibling SPI implementation
/// (`CloudFoundryVcapEnvironmentPostProcessor`, correctly fork-loader-defined)
/// failed `Class.equals` against it and its constructor's `DeferredLogFactory`
/// argument resolved to `null`.
///
/// Actively drives the host loader's `loadClass` (same as the `New`/`Ldc`
/// resolution path's gate-on branch) on a cache miss, instead of only
/// consulting what's already cached. Falls back to the passive check (and
/// then to `None`, preserving legacy behavior) whenever the gate is off, the
/// host loader is built-in, or driving the loader fails to produce a class.
pub(crate) fn lambda_impl_dispatch_override_driven(
    shared: &SharedVm,
    thread: &mut JvmThread,
    call_site: &crate::classloading::resolution::LambdaCallSite,
) -> Option<ClassId> {
    if let Some(cid) = lambda_impl_dispatch_override(shared, call_site) {
        return Some(cid);
    }
    if !crate::runtime::env_cache::loader_aware_resolution() {
        return None;
    }
    // Same short-circuit as the passive sibling, and here it subsumes the
    // `UserDefined(_)` test four lines below: if no class in the process has a
    // user-defined defining loader, `get_loader_id(host)` cannot return one.
    // Without it this arm took a SECOND `lambda_proxy_hosts` read lock and a
    // `class_manager` read lock per lambda call, to reach that same verdict.
    if !cratonvm_native_builtins::classloader::any_defining_loader_registered(&shared.loader_identity) {
        return None;
    }
    let host = *shared
        .classes
        .lambda_proxy_hosts
        .read()
        .get(&call_site.proxy_class_id)?;
    if !matches!(
        shared.classes.class_manager.read().get_loader_id(host),
        Some(cratonvm_types::ClassLoaderId::UserDefined(_))
    ) {
        return None;
    }
    let name = &call_site.impl_handle.class_name;
    // Its own `Lookup` door of the request prologue (interpreter round i6
    // wave 123, lane L5, `i39-L5`).
    drive_defining_loader_load_door(shared, thread, host, name, "lambda-impl-owner")
        .filter(|cid| *cid != ClassId::new(0))
}

/// The memoised global implementation-owner `ClassId` for `call_site`, or
/// `None` on the first dispatch (before anything has resolved it).
///
/// See `ClassRealm::lambda_impl_owner_memo` for why this exists — in short, the
/// by-name resolution it replaces was measured at ~2,300-3,080 ns of a
/// ~3,000-3,900 ns lambda dispatch. Callers must consult
/// [`lambda_impl_dispatch_override_driven`] FIRST: this table holds only the
/// answer the loader-blind path would produce, and must never pre-empt a
/// loader-faithful one.
#[inline]
fn lambda_global_impl_owner(
    shared: &SharedVm,
    call_site: &crate::classloading::resolution::LambdaCallSite,
) -> Option<ClassId> {
    shared
        .classes
        .lambda_impl_owner_memo
        .read()
        .get(&call_site.proxy_class_id)
        .copied()
}

/// Record the global implementation owner for `call_site` after a by-name
/// dispatch has already succeeded through it.
///
/// Resolves the name through the *loaded* table only (never a load): the call
/// that just returned proves the class is loaded and initialised, so a miss here
/// means something raced or the name is not globally visible, and the right
/// answer is to memoise nothing and let the next call take the slow path again.
fn record_lambda_global_impl_owner(
    shared: &SharedVm,
    call_site: &crate::classloading::resolution::LambdaCallSite,
) {
    if lambda_global_impl_owner(shared, call_site).is_some() {
        return;
    }
    let resolved = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(&call_site.impl_handle.class_name);
    let Some(cid) = resolved.filter(|c| *c != ClassId::new(0)) else {
        return;
    };
    shared
        .classes
        .lambda_impl_owner_memo
        .write()
        .insert(call_site.proxy_class_id, cid);
}

/// [`record_lambda_global_impl_owner`] for a by-name static dispatch that did
/// not return but YIELDED (a virtual thread parked inside the impl body).
/// Interpreter round i6 wave 125, lane L6 (page
/// `i125-L6-virtual-thread-owned-frames-miss-lambda-memo-and-osr`), under
/// `CRATONVM_LAMBDA_OWNER_MEMO_ON_YIELD` (default off).
///
/// Without it, a lambda whose impl parks on EVERY call never fills the memo:
/// the record sits after `invoke_static_shared(..)?`, the yield leaves through
/// the `?`, and the Rust frame is gone at the remount. Every call then takes
/// the by-name path, whose `execute` pushes an OWNED frame that the virtual
/// thread's OSR offer refuses.
///
/// A yield does not prove what a return proves: the yield may come from the
/// declaring class's own `<clinit>`, still running. So the owner is recorded
/// only when the named class is fully INITIALIZED. That implies its
/// superclasses are too (JVMS §5.5), so the declaring class of an inherited
/// static is as well (interface statics are not inherited by `C::m`). The memo
/// then holds exactly the answer a returning call would have filed; the
/// current call is untouched (it resumes in its frame and returns through the
/// continuation's adapters), and later calls take the memoised arm, which
/// answers as the by-name arm does (the reason the memo exists at all).
fn record_lambda_global_impl_owner_on_yield(
    shared: &SharedVm,
    call_site: &crate::classloading::resolution::LambdaCallSite,
) {
    if lambda_global_impl_owner(shared, call_site).is_some() {
        return;
    }
    let resolved = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(&call_site.impl_handle.class_name);
    let Some(cid) = resolved.filter(|c| *c != ClassId::new(0)) else {
        return;
    };
    if !crate::vm::is_class_initialized_via_manager(shared, cid) {
        return;
    }
    shared
        .classes
        .lambda_impl_owner_memo
        .write()
        .insert(call_site.proxy_class_id, cid);
}

/// The implementation-owner `ClassId` for `call_site`: the loader-faithful
/// override when one applies, otherwise the global by-name answer — resolved
/// once, then served from `lambda_impl_owner_memo`.
///
/// Replaces four identical `class_manager.write().load_class(name)` sites, one
/// per lambda kind that needs an owner up front (`InvokeSpecial`,
/// `NewInvokeSpecial`, `GetStatic`, `PutStatic`). Each of them took the VM-wide
/// class-manager **write** lock on every dispatch; `load_class` is idempotent
/// for an already-loaded name, so from the second dispatch onward that lock was
/// being held to re-derive a constant — and holding a global write lock per
/// lambda call serialises every other thread against it.
fn lambda_impl_owner_class_id(
    shared: &SharedVm,
    thread: &mut JvmThread,
    call_site: &crate::classloading::resolution::LambdaCallSite,
) -> Result<ClassId, MethodCallFailed> {
    if let Some(cid) = lambda_impl_dispatch_override_driven(shared, thread, call_site) {
        return Ok(cid);
    }
    if let Some(cid) = lambda_global_impl_owner(shared, call_site) {
        return Ok(cid);
    }
    let cid = shared
        .classes
        .class_manager_write_settling()
        .load_class(&call_site.impl_handle.class_name)?;
    if cid != ClassId::new(0) {
        shared
            .classes
            .lambda_impl_owner_memo
            .write()
            .insert(call_site.proxy_class_id, cid);
    }
    Ok(cid)
}

/// Resolve a lambda implementation that is private in its declaring class.
///
/// LambdaMetafactory may encode a private synthetic lambda body as an
/// `InvokeVirtual` method handle. That handle is nevertheless bound to the
/// resolved owner method: virtual dispatch on the captured object's concrete
/// subclass is incorrect when that subclass happens to declare a same-named
/// synthetic `lambda$...` method. Preserve the declaring class in that case.
///
/// Memoised per `(vm_identity, proxy_class_id)` in [`PRIVATE_IMPL_MEMO`]:
/// the derivation took two `class_manager` reads, a by-name class hash and a
/// recursive method walk on EVERY virtual method-reference call. Both key
/// halves are never recycled (`NEXT_VM_IDENTITY`, `next_lambda_id`).
///
/// The one input that can change under a fixed key is the loader-faithful
/// owner (`lambda_impl_dispatch_override`), which reads the lazily filled
/// loader-initiated cache: a memo taken before that fill would pin the
/// loader-blind owner. So an answer is memoised only when the override
/// answered, or when no user-defined loader has ever defined a class
/// (`any_defining_loader_registered`): the proxy's host was then defined by a
/// built-in loader, for which the override is `None` forever. An owner that
/// is not loaded yet is not memoised either. Redefinition keeps `ClassId`s
/// and cannot change a method's modifiers or add methods, so it needs no
/// invalidation; rows go with their VM ([`forget_vm_private_impl_memo`]) or
/// with an unloaded owner ([`forget_unloaded_private_impl_memo`]).
pub(crate) fn lambda_private_impl_dispatch_class(
    shared: &SharedVm,
    call_site: &crate::classloading::resolution::LambdaCallSite,
) -> Option<ClassId> {
    let key = (shared.vm_identity, call_site.proxy_class_id);
    if let Some(&memo) = private_impl_memo().read().get(&key) {
        return memo;
    }
    let override_owner = lambda_impl_dispatch_override(shared, call_site);
    let owner_id = override_owner.or_else(|| {
        shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(&call_site.impl_handle.class_name)
    })?;
    let answer = {
        let cm = shared.classes.class_manager.read();
        crate::classloading::find_method_recursive(
            owner_id,
            &call_site.impl_handle.member_name,
            &call_site.impl_handle.descriptor,
            &cm.class_store,
        )
        .and_then(|(method, declaring_id)| {
            method
                .access_flags
                .contains(MethodAccessFlags::PRIVATE)
                .then_some(declaring_id)
        })
    };
    if override_owner.is_some()
        || !cratonvm_native_builtins::classloader::any_defining_loader_registered(&shared.loader_identity)
    {
        private_impl_memo().write().insert(key, answer);
    }
    answer
}

/// JIT round 20 wave 12, lane qLam (correctness;
/// `jr20-lambda-virtual-arm-selects-by-name-not-by-override-20261012`): the
/// class whose declaration JVMS §5.4.6 SELECTS for a `REF_invokeVirtual` /
/// `REF_invokeInterface` impl and a receiver of class `receiver`, when that is
/// NOT the declaration the by-name walk (`find_method_recursive(receiver, ..)`)
/// lands on -- e.g. `Base::m` with a package-private `Base.m` and a receiver
/// whose class, in another runtime package, declares its own `m()`: HotSpot
/// runs `Base.m`, the walk finds the subclass's. `None` when the two agree, and
/// whenever selection keeps the walk's answer
/// (`selection::receiver_dispatch_moves_to`: a hierarchy with a compatibility
/// class in it, an abstract or conflicting selection), so the arm never starts
/// failing where it answered before. The owner is the handle's class, through
/// the loader-faithful override like every other owner of this arm.
pub(crate) fn lambda_virtual_selection_moves_to(
    shared: &SharedVm,
    call_site: &crate::classloading::resolution::LambdaCallSite,
    receiver: ClassId,
) -> Option<ClassId> {
    // Read before the class-manager guard: it may take its own.
    let override_owner = lambda_impl_dispatch_override(shared, call_site);
    let cm = shared.classes.class_manager.read();
    let owner =
        override_owner.or_else(|| cm.get_loaded_class_id(&call_site.impl_handle.class_name))?;
    crate::runtime::resolve::selection::receiver_dispatch_moves_to(
        cm.class_store(),
        receiver,
        owner,
        &call_site.impl_handle.member_name,
        &call_site.impl_handle.descriptor,
    )
}

/// `(vm_identity, proxy_class_id)` → [`lambda_private_impl_dispatch_class`]'s
/// answer. See that function for the memo rule.
type PrivateImplMemo = rustc_hash::FxHashMap<(usize, ClassId), Option<ClassId>>;

static PRIVATE_IMPL_MEMO: std::sync::OnceLock<parking_lot::RwLock<PrivateImplMemo>> =
    std::sync::OnceLock::new();

fn private_impl_memo() -> &'static parking_lot::RwLock<PrivateImplMemo> {
    PRIVATE_IMPL_MEMO.get_or_init(|| parking_lot::RwLock::new(PrivateImplMemo::default()))
}

/// Drop every private-impl memo row of a disposed VM (and, riding the same
/// hook, every receiver-native memo row).
pub(crate) fn forget_vm_private_impl_memo(vm_identity: usize) {
    if let Some(memo) = PRIVATE_IMPL_MEMO.get() {
        memo.write().retain(|&(vid, _), _| vid != vm_identity);
    }
    if let Some(memo) = RECEIVER_NATIVE_MEMO.get() {
        memo.write().retain(|&(vid, _, _), _| vid != vm_identity);
    }
}

/// `(vm_identity, proxy_class_id, receiver class id)` → `(native registry
/// generation, id)`. See [`lambda_receiver_native_id`].
type ReceiverNativeMemo = rustc_hash::FxHashMap<
    (usize, ClassId, ClassId),
    (u32, Option<cratonvm_native_api::NativeMethodId>),
>;

static RECEIVER_NATIVE_MEMO: std::sync::OnceLock<parking_lot::RwLock<ReceiverNativeMemo>> =
    std::sync::OnceLock::new();

/// Past this many rows a new `(proxy, receiver)` pair is answered without
/// being memoised, so a method reference applied to an unbounded family of
/// receiver classes cannot grow the table without bound.
const RECEIVER_NATIVE_MEMO_CAP: usize = 1 << 16;

fn receiver_native_memo() -> &'static parking_lot::RwLock<ReceiverNativeMemo> {
    RECEIVER_NATIVE_MEMO.get_or_init(|| parking_lot::RwLock::new(ReceiverNativeMemo::default()))
}

/// The registry id of the RECEIVER-KEYED native a virtual method-reference
/// call must run (`try_lambda_dispatch`'s virtual arm, "THE RECEIVER-KEYED
/// NATIVE"): `Some` exactly when the receiver's own class does NOT declare the
/// implementation method and the registry has a native for
/// `(receiver_class, member_name, descriptor)`.
///
/// Lambda dispatch plan stage 3
/// (`interpreter-L6-proposal-lambda-proxy-dispatch-plan-FIXED-20260930.md`). The derivation
/// took a `class_manager` read, a method walk and a three-string registry hash
/// on every call. Memoised per `(vm_identity, proxy_class_id,
/// receiver_class_id)` (the proxy fixes the member and descriptor, the
/// receiver id fixes `receiver_class`), valid while the native registry
/// generation it was taken at holds, so a later `register_*` pass re-derives
/// it. The declared-method half cannot change under a fixed key: neither id
/// is ever reused, and redefinition cannot add or remove methods.
///
/// Only the ID is memoised, never the admission: the caller still runs
/// `native_override::admit_forced_native_id` on every call, so the §7 routing
/// and its `--jdk-only` dispatch counting are exactly as before.
pub(super) fn lambda_receiver_native_id(
    shared: &SharedVm,
    proxy_class_id: ClassId,
    receiver_class_id: ClassId,
    receiver_class: &str,
    member_name: &str,
    descriptor: &str,
) -> Option<cratonvm_native_api::NativeMethodId> {
    let registry = &shared.natives.native_methods;
    // Read BEFORE deriving: a registration racing the derivation leaves a row
    // stamped with the older generation, which the next call re-derives.
    let generation = registry.generation();
    let key = (shared.vm_identity, proxy_class_id, receiver_class_id);
    if let Some(&(stamp, id)) = receiver_native_memo().read().get(&key) {
        if stamp == generation {
            return id;
        }
    }
    let declares = shared
        .classes
        .class_manager
        .read()
        .get_class(receiver_class_id)
        .map(|c| c.find_method(member_name, descriptor).is_some());
    let id = match declares {
        Some(false) => registry.resolve_id(receiver_class, member_name, descriptor),
        // Declared by the receiver's class (the downstream native-precedence
        // rules decide), or a receiver class this VM cannot name.
        _ => None,
    };
    let mut memo = receiver_native_memo().write();
    if memo.len() < RECEIVER_NATIVE_MEMO_CAP || memo.contains_key(&key) {
        memo.insert(key, (generation, id));
    }
    id
}

/// Drop the private-impl memo rows whose answer names an unloaded class (the
/// proxy of such a row is unreachable with it; its id is never reused).
pub(crate) fn forget_unloaded_private_impl_memo(
    vm_identity: usize,
    unloaded: &rustc_hash::FxHashSet<ClassId>,
) {
    if unloaded.is_empty() {
        return;
    }
    if let Some(memo) = PRIVATE_IMPL_MEMO.get() {
        memo.write().retain(|&(vid, _), answer| {
            vid != vm_identity || !answer.is_some_and(|cid| unloaded.contains(&cid))
        });
    }
    // Receiver-native rows keyed by an unloaded receiver class: that class
    // can never be a receiver again (ids are never reused).
    if let Some(memo) = RECEIVER_NATIVE_MEMO.get() {
        memo.write()
            .retain(|&(vid, _, receiver), _| vid != vm_identity || !unloaded.contains(&receiver));
    }
}

/// Drop the private-impl and receiver-native memo rows KEYED by a lambda proxy
/// whose host class unloaded (`invokedynamic::forget_unloaded_lambda_proxy_rows`).
/// Such a proxy has no live instance any more (its pin row kept the host's
/// loader alive while one existed), and its id is never reused.
pub(crate) fn forget_dead_lambda_proxy_memo_rows(
    vm_identity: usize,
    dead_proxies: &rustc_hash::FxHashSet<ClassId>,
) {
    if dead_proxies.is_empty() {
        return;
    }
    if let Some(memo) = PRIVATE_IMPL_MEMO.get() {
        memo.write()
            .retain(|&(vid, proxy), _| vid != vm_identity || !dead_proxies.contains(&proxy));
    }
    if let Some(memo) = RECEIVER_NATIVE_MEMO.get() {
        memo.write()
            .retain(|&(vid, proxy, _), _| vid != vm_identity || !dead_proxies.contains(&proxy));
    }
}

/// Try to run a concrete default method declared by a lambda proxy's
/// functional interface.
///
/// Lambda proxy ids are synthetic and do not live in the class store. Calls to
/// non-SAM interface defaults normally reach the real functional interface via
/// direct dispatch, but an invoke through a superinterface can resolve to that
/// superinterface's abstract declaration first. Groovy's
/// `CompilationUnit$PhaseOperation.doPhaseOperation(CompilationUnit)` is one
/// such shape: the receiver is an `ISourceUnitOperation`/
/// `IPrimaryClassNodeOperation` lambda, while the resolved declaration is the
/// abstract parent interface.
pub(super) fn try_lambda_default_method_dispatch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj_class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
    full_args: &[Value],
) -> Result<Option<Option<Value>>, MethodCallFailed> {
    let (interface_name, interface_id_hint) =
        match shared.classes.lambda_call_site_for(obj_class_id) {
            Some(call_site) => (
                call_site.functional_interface.clone(),
                call_site.functional_interface_id,
            ),
            None => return Ok(None),
        };

    // Memoised like `invokedynamic`'s copy of this flag: an uncached read is a
    // `getenv` (a syscall on Windows) per default-method call on a lambda.
    // Read once per VM into `VmConfig::interp_traces` (interpreter round i2
    // wave 59, lane L7; a process `OnceLock` before).
    if shared
        .config
        .interp_trace(crate::config::interp_trace::LAMBDA_DISPATCH)
    {
        eprintln!(
            "[DBG_LAMBDA] default-dispatch proxy={:?} method={}{} hint={:?}",
            obj_class_id, method_name, method_descriptor, interface_id_hint
        );
    }
    let declaring_id = {
        let cm = shared.classes.class_manager.read();
        // Prefer the loader-resolved interface captured at bootstrap time:
        // a bare name lookup returns an arbitrary copy when several loaders
        // define the same interface, executing the default method in the
        // wrong loader's context.
        let Some(interface_id) =
            interface_id_hint.or_else(|| cm.get_loaded_class_id(&interface_name))
        else {
            return Ok(None);
        };
        match crate::classloading::find_method_recursive(
            interface_id,
            method_name,
            method_descriptor,
            &cm.class_store,
        ) {
            Some((method, declaring_id)) if method.code().is_some() && !method.is_abstract() => {
                Some(declaring_id)
            }
            _ => None,
        }
    };

    let Some(declaring_id) = declaring_id else {
        return Ok(None);
    };
    crate::vm::invoke_on_class_shared_no_retarget(
        shared,
        thread,
        declaring_id,
        method_name,
        method_descriptor,
        full_args,
    )
    .map(Some)
}

/// Key for the two thread-local dispatch caches below:
/// `(vm_identity, proxy ClassId, receiver ClassId)`.
///
/// The `vm_identity` component is load-bearing, not decoration. A `ClassId` is
/// unique only *within* one VM, and one OS thread can run bytecode in two VMs
/// (the inline test modules build a `SharedVm` per test on one thread; a host
/// thread can be attached to two `Vm`s). Keyed on the two `ClassId`s alone, a
/// lookup in VM B hit VM A's entry for the numerically-equal ids and dispatched
/// VM A's cached method body — or VM A's cached field index — against VM B's
/// classes.
///
/// The `RedefineGate` does NOT catch that. Its staleness handle comes from the
/// class manager of whichever VM populated the entry, so consulted from VM B it
/// reports VM A's (unchanged) redefine generation and answers "fresh". The gate
/// guards against redefinition, not against identity collision; only the key
/// can do the latter.
///
/// See `vm-process-global-state-round-2.md`.
type VmScopedClassPairKey = (usize, u32, u32);

/// One [`LAMBDA_IMPL_BYTECODE_CACHE`] row: the impl template, its redefinition
/// gate, and (nio21 wave 16 L2) the compiled body the last successful
/// `JitCache::get` found for it.
///
/// The body memo is a `Weak` stamped with the `JitCache::generation()` read
/// before that probe, so it answers only while no publication or withdrawal
/// has happened since -- the T2.2 epoch rule `jit_probe_is_current` applies to
/// a MISS, here applied to a HIT. A `Weak` keeps no withdrawn body mapped
/// (`release_withdrawn_code_owners`' concern) and an upgrade that fails is a
/// re-probe. Without it every compiled lambda call through
/// [`try_invoke_cached_lambda_impl`] hashed the impl's three strings
/// (`compute_jit_key_hash` + `JitCache::get`, 3.1% of the w15 `rlock`
/// profile). Kill switch `CRATONVM_LAMBDA_IMPL_CODE_MEMO=0`.
pub(super) struct LambdaImplRow {
    cached: Arc<CachedBytecodeMethod>,
    gate: RedefineGate,
    code_generation: u64,
    code: std::sync::Weak<crate::jit::CompiledMethod>,
}

thread_local! {
    pub(super) static LAMBDA_IMPL_BYTECODE_CACHE: std::cell::RefCell<
        rustc_hash::FxHashMap<VmScopedClassPairKey, LambdaImplRow>
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

// (The interpreter's TDigest `Function<Integer, Double>` adapter collapse,
// `try_tdigest_lambda_double_get`, and its field cache lived here. Its only
// caller — a class-name recognizer in the interpreter's `invokestatic` arm —
// was removed in interpreter round i1, wave 2; see
// docs/internal/fixed-bugs/interpreter-L1-tdigest-recognizer-in-invokestatic-arm-FIXED-20260923.md.)

// ---------------------------------------------------------------------------
// The lambda call site's own cached invoke target
// ---------------------------------------------------------------------------
//
// `jit_invoke_virtual_mic` answers a compiled caller's `invokeinterface`. For
// an ordinary named class it is entered ONCE per call site: it resolves the
// receiver's compiled body, stores it in the monomorphic inline cache, and
// from then on the inline cascade emitted in `jit/src/x64.rs` calls the callee
// straight from machine code without re-entering Rust at all. Measured on
// `probes/SamHotLoopProbe.java` with `CRATONVM_DBG=mic-prof`: `mic_calls=1`
// across 2 200 000 dispatches, 11.7 ns/op.
//
// A lambda receiver could never reach that. The lambda arm of that helper sits
// BEFORE the inline cache and returns from inside it, so the slot is never
// populated and never probed: `mic_calls=2197000`, `lambda=2197000`,
// `hit_entry=0`, `miss=0` — one full Rust helper round trip, ~819 cycles, on
// every single SAM call, 352 ns/op. That is the 30x, and it is not "lambdas
// are slow": it is one early `return` standing between a SAM call site and the
// same inline cache every other interface call site gets.
//
// The inline cache itself cannot hold a lambda: its cascade calls the cached
// entry with the caller's own argument registers, and a SAM call's registers
// are not the impl method's — the proxy receiver has to be dropped and the
// captured values prepended. So this is the next best thing, and it is what
// known-issues/perf/lambda-sam-dispatch-bypasses-the-cached-invoke-path-20260817.md
// section 4 asks for: a cached invoke target belonging to the CALL SITE, so the
// second and every later invocation of a given proxy skips `try_lambda_dispatch`
// — the descriptor clones, the coercion walk, the impl-owner memo lookup, the
// `full_args` Vec, the JIT-cache probe and the entry guard — and goes
// receiver-fields to registers to compiled body.
//
// Everything that makes a lambda dispatch complicated is decided ONCE, when the
// site is built. A shape that needs any of it is cached as ineligible and never
// asked again.

/// A lambda call site whose dispatch has been reduced to "read the captures,
/// call the compiled impl".
///
/// Held per proxy `ClassId` in a thread-local, so reading one costs no lock.
/// `Rc`, not `Arc`: it never leaves the thread that built it.
pub(crate) struct LambdaJitSite {
    /// The SAM this site answers. A functional interface may declare
    /// same-named default overloads, so a call must match BOTH name and
    /// descriptor before this site may serve it — the same authority rule
    /// `try_lambda_dispatch` applies.
    sam_method_name: Arc<str>,
    sam_descriptor: Arc<str>,
    /// Captured values live in proxy object fields `0..num_captures`, in
    /// impl-parameter order.
    num_captures: usize,
    /// The leading descriptor byte of each captured value, in the same order —
    /// the impl's first `num_captures` parameter types.
    ///
    /// The Rust arm has no use for these: it reads `Value`s back out of the heap
    /// and switches on the tag it finds. The inline-cache thunk does, because it
    /// emits one typed load per capture and has no tag to consult. Decided here
    /// with the rest of the site, so the emitter is handed facts rather than a
    /// descriptor to re-parse on every install.
    capture_descs: Vec<u8>,
    /// `num_captures` + the SAM's parameter count == the impl's arity.
    total_args: usize,
    /// The `checkcast` the synthetic bridge would have performed, for each SAM
    /// parameter that needs one: `(index among the SAM's own arguments, the
    /// instantiated type token)`.
    ///
    /// A generic functional interface erases its parameters to `Object`, so
    /// `Function<Integer,Integer>` — the shape `CompletableFuture` composition
    /// is built out of — reaches its impl through a javac bridge that casts
    /// each argument to the instantiated type first. Coercion is still the
    /// identity for these (both tokens are references), so only the cast is
    /// left, and a cast that would FAIL simply declines this arm: the generic
    /// path then throws the `ClassCastException` with the message it has always
    /// produced. Empty for the great majority of sites.
    checkcasts: Vec<(usize, Arc<str>)>,
    /// nio21 w15 L5: per entry of `checkcasts`, the argument class id whose
    /// cast this site last PROVED passes (`u32::MAX`: none yet). A
    /// monomorphic generic site then replays its bridge `checkcast` as one
    /// compare per argument instead of a full [`lambda_arg_cast_verdict`].
    /// Sound for the reason the verdict's own memo is
    /// ([`lambda_cast_pass_memo_enabled`]): only a pass the verdict proved
    /// from a class-pair arm is recorded ([`lambda_site_cast_passes`]), and
    /// such a pass only ever stays a pass. Same kill switch as that memo.
    cast_pass_cids: Vec<std::cell::Cell<u32>>,
    /// The impl method, for the JIT-cache probe and the redefinition gate.
    cached: Arc<CachedBytecodeMethod>,
    gate: RedefineGate,
    /// The compiled impl body, re-probed only when `JitCache::generation()`
    /// moves. `None` at the current generation means "not compiled yet" — the
    /// warmup counter in `try_invoke_cached_lambda_impl` is what eventually
    /// changes that, which is why that counter and this fast path are one
    /// feature under one kill switch.
    code: std::cell::RefCell<Option<cratonvm_jit::RetainedCode>>,
    code_generation: std::cell::Cell<u64>,
    /// The inline-cache slot pair this site's thunk was last installed into,
    /// or `(0, 0)` for none.
    ///
    /// Without a latch the Rust arm re-writes the cache slot on every call it
    /// still serves — measured at 202 000 "installs" for what should be a
    /// handful — which is not merely wasted work: the slot's first cache line
    /// is the one the emitted cascade loads on every dispatch from every
    /// thread, and storing to it repeatedly is how a fast path pays for its own
    /// existence.
    ///
    /// **It records WHICH slots, not merely that it installed, and that is the
    /// whole point.** A bare `bool` latched the site for the life of the
    /// process — but the slots belong to the CALLER'S COMPILED BODY, and a
    /// caller gets recompiled: C1 then C2, or an OSR body published beside the
    /// entry one. The new body's slots are fresh and empty, and a site latched
    /// `true` could never fill them, so every later dispatch fell back to the
    /// Rust arm for good.
    ///
    /// Measured on `LambdaJitTierUp.warmChecksum`: both thunks installed at
    /// `site_direct=0` and `site_direct=1` — immediately — and `site_direct`
    /// still reached 397 002 of 800 000, which is
    /// `test_inline_cache_takes_over_the_sam_call_site` failing while its
    /// `site_adapters=2` says the feature engaged. Comparing the pair bounds
    /// re-installs by the number of distinct bodies, which is small, instead of
    /// by the number of calls, which is not.
    adapter_slots: std::cell::Cell<(i64, i64)>,
    /// Latched the first time this site's compiled body DEOPTS.
    ///
    /// A deopt means the body did not complete, and the direct arm has no way
    /// to resume it: the reconstructed frame belongs to the `lambda$...` impl,
    /// while the compiled caller this arm returns into only understands its own
    /// deopts. The interpreter's one-shot path knows the impl's identity and
    /// resumes such a frame precisely, so from the first deopt on, this site
    /// sends its calls there instead. One latch, never cleared: a body that
    /// deopted once under this call site will do it again.
    direct_disabled: std::cell::Cell<bool>,
    /// The constant this site's impl body returns unconditionally, if it is
    /// one of those bodies. Decided once here from the bytecode rather than
    /// per call, because it is a property of the method.
    ///
    /// See [`const_int_return_of`]: a constant body is the one case where a
    /// wrong answer needs no baseline and no repeat runs, so the FIRST wrong
    /// call is proof.
    const_return: Option<i32>,
    /// JIT round 20 wave 12, lane qLam (DISP20-3): `Some((bound, receiver
    /// class))` for an INSTANCE-method impl site
    /// ([`lambda_instance_jit_site`]): the impl was resolved against that exact
    /// receiver class, and the receiver is the proxy's capture 0 (`bound`) or
    /// the SAM's argument 0. `None` for every static-impl site.
    receiver: Option<(bool, ClassId)>,
    /// JIT round 20 wave 12, lane qLam (the `hdr` follow-up): the Rust-side
    /// adapter this site's call needs -- LambdaMetafactory's box / unbox, or a
    /// native target -- or `None` for a site whose call is the plain shuffle.
    /// A site with one is served by the compiled direct arm only, never by a
    /// machine thunk ([`LambdaJitSite::rust_only`]).
    adapt: Option<Box<LambdaAdapt>>,
}

/// JIT round 20 wave 12, lane qLam: what a [`LambdaJitSite`] must do around
/// its impl call beyond the register shuffle. Decided once, when the site is
/// built (`CRATONVM_JIT_LAMBDA_SITE_BOXING`, `CRATONVM_JIT_LAMBDA_SITE_NATIVE`).
pub(crate) struct LambdaAdapt {
    /// `(index among the SAM's own arguments, the impl's primitive, the exact
    /// wrapper class)`: the argument must be an instance of exactly that
    /// wrapper (anything else -- `null`, another class -- declines the call to
    /// the general path, which raises what LambdaMetafactory's class raises),
    /// and the impl receives its unboxed value.
    unbox: Vec<(usize, char, ClassId)>,
    /// Box the impl's primitive return through the wrapper's `valueOf`
    /// (identity-preserving caches included). Only 32-bit kinds (`I Z B S C F`),
    /// so a returned `i64::MIN` is never a value: it is the escaping-exception
    /// sentinel and passes through unboxed.
    box_ret: Option<char>,
    /// An instance impl whose receiver class's own body is shadowed by a
    /// reviewed `Intrinsic`: the call is a compiled `invokevirtual` of that
    /// method, served by the JIT's native site cache under this (leaked, one
    /// per site) call-site record.
    native: Option<&'static cratonvm_jit::JitInvokeInfo>,
}

impl LambdaAdapt {
    pub(crate) fn unbox_list(&self) -> &[(usize, char, ClassId)] {
        &self.unbox
    }

    pub(crate) fn box_return(&self) -> Option<char> {
        self.box_ret
    }

    pub(crate) fn native_info(&self) -> Option<&'static cratonvm_jit::JitInvokeInfo> {
        self.native
    }
}

impl LambdaJitSite {
    /// Does this site answer THIS call? Name and descriptor both, never one.
    pub(crate) fn serves(&self, method_name: &str, descriptor: &str) -> bool {
        // nio21 w15 L5: the call site's strings are usually the very interned
        // `Arc<str>`s the site was built from, so identity answers without a
        // `memcmp`; content equality stays the authority.
        let same = |a: &str, b: &str| (a.as_ptr() == b.as_ptr() && a.len() == b.len()) || a == b;
        same(&*self.sam_method_name, method_name) && same(&*self.sam_descriptor, descriptor)
    }

    pub(crate) fn num_captures(&self) -> usize {
        self.num_captures
    }

    /// The captured values' descriptor bytes, in impl-parameter order — what
    /// the inline-cache thunk emits one load each from.
    pub(crate) fn capture_descs(&self) -> &[u8] {
        &self.capture_descs
    }

    pub(crate) fn total_args(&self) -> usize {
        self.total_args
    }

    /// Claim the inline-cache install for THIS slot pair, returning `true` the
    /// first time this site is asked to fill it.
    ///
    /// Returns `false` for a repeat of the same pair — the 202 000-re-install
    /// case the latch exists for — and `true` once more when the caller has
    /// been recompiled and handed the site a different pair. See
    /// [`LambdaJitSite::adapter_slots`].
    pub(crate) fn claim_adapter_install(&self, mic_ptr: i64, pic_ptr: i64) -> bool {
        let slots = (mic_ptr, pic_ptr);
        if self.adapter_slots.get() == slots {
            return false;
        }
        self.adapter_slots.set(slots);
        true
    }

    /// Does this site's SAM call need a `checkcast` replayed per call? A
    /// hand-emitted thunk cannot ask a class-hierarchy question.
    pub(crate) fn has_checkcasts(&self) -> bool {
        !self.checkcasts.is_empty()
    }

    /// nio21 w18 L2: the guards a cast inline-cache thunk would bake for this
    /// site -- `(index among the SAM's own arguments, the argument class id
    /// this site PROVED passes that cast)` per replayed `checkcast` -- or
    /// `None` while any cast has no proved class yet, or targets an array type
    /// (an array header names its component, so a class-id compare cannot
    /// decide an array cast). `None` too for a site with no casts (the plain
    /// thunk serves it).
    ///
    /// A proved pass is a fact about two classes and only ever stays a pass
    /// (`cast_pass_cids`), so the class id may be baked into machine code;
    /// the thunk still sends every array and every other class to the Rust
    /// path, which decides it in full.
    pub(crate) fn cast_thunk_guards(&self) -> Option<Vec<(usize, u32)>> {
        if self.checkcasts.is_empty() {
            return None;
        }
        self.checkcasts
            .iter()
            .zip(&self.cast_pass_cids)
            .map(|((sam_idx, tok), cid)| {
                let cid = cid.get();
                (tok.starts_with('L') && cid != u32::MAX).then_some((*sam_idx, cid))
            })
            .collect()
    }

    /// Is the implementation a static method — i.e. does dropping the proxy
    /// receiver lose nothing? Always true for a non-capturing javac lambda, and
    /// asserted rather than assumed because the thunk drops it outright.
    pub(crate) fn is_static_impl(&self) -> bool {
        self.cached.is_static
    }

    /// JIT round 20 wave 12, lane qLam (DISP20-3): the receiver guard an
    /// INSTANCE-method impl site's thunk bakes -- where the receiver is and
    /// the exact class the impl was resolved against -- or `None` for a
    /// static-impl site.
    /// JIT round 20 wave 12, lane qLam: the site's Rust-side adapter, if any.
    pub(crate) fn adapt(&self) -> Option<&LambdaAdapt> {
        self.adapt.as_deref()
    }

    /// Must every call of this site go through the Rust direct arm (an
    /// adapter no machine thunk performs)?
    pub(crate) fn rust_only(&self) -> bool {
        self.adapt.is_some()
    }

    pub(crate) fn receiver_thunk_guard(
        &self,
    ) -> Option<cratonvm_jit::lambda_adapter::ReceiverGuard> {
        use cratonvm_jit::lambda_adapter::{ReceiverGuard, ReceiverSource};
        let (bound, class_id) = self.receiver?;
        Some(ReceiverGuard {
            source: if bound {
                ReceiverSource::Capture0
            } else {
                ReceiverSource::SamArg0
            },
            class_id: class_id.as_u32(),
        })
    }

    /// The implementation's declaring class, for the inline cache's own
    /// diagnostic record of what it cached.
    pub(crate) fn impl_class_name(&self) -> &str {
        &self.cached.class_name
    }

    /// The impl body's `CachedBytecodeMethod`.
    ///
    /// The direct compiled arm needs it for one thing: to RESUME a
    /// reconstructed frame when that body deoptimizes.
    /// [`jit_bridge::resume_deopted_body_at_point`](super::resume_deopted_body_at_point) keys
    /// every resume attempt on the trapped method's own identity, and until
    /// this accessor existed the arm could not name the impl — so it dropped
    /// the frame and let its caller re-run the body from entry, which
    /// double-executed whatever the body had already committed. See
    /// [`LambdaJitSite::direct_disabled`].
    pub(crate) fn cached_impl(&self) -> &Arc<CachedBytecodeMethod> {
        &self.cached
    }

    /// The constant this site's impl must return, if its body is a bare
    /// push-and-return. `None` for every other site, which is nearly all of
    /// them.
    pub(crate) fn const_return(&self) -> Option<i32> {
        self.const_return
    }

    /// The impl's own name, for the constant-probe report — the SAM name
    /// (`test`, `apply`) would name the interface rather than the body that
    /// gave the wrong answer.
    pub(crate) fn impl_method_name(&self) -> &str {
        &self.cached.method_name
    }

    /// The `checkcast`s this site replays per call: `(index among the SAM's
    /// own arguments, the instantiated type token)`.
    pub(crate) fn checkcast_list(&self) -> &[(usize, Arc<str>)] {
        &self.checkcasts
    }

    /// Has the impl's declaring class been redefined since this site was built?
    pub(crate) fn gate_is_stale(&self) -> bool {
        self.gate.is_stale()
    }

    /// May the direct compiled arm still serve this site? See
    /// [`LambdaJitSite::direct_disabled`].
    pub(crate) fn direct_enabled(&self) -> bool {
        !self.direct_disabled.get()
    }

    /// Latch this site off the direct arm after its body deoptimized.
    pub(crate) fn disable_direct(&self) {
        self.direct_disabled.set(true);
    }
}

/// What the shape analysis concluded about a proxy class.
enum SiteVerdict {
    /// Serve this call site directly.
    Eligible(std::rc::Rc<LambdaJitSite>),
    /// This SHAPE can never be served here (boxing SAM, instance-method
    /// reference, handler-bearing body). Remember it; never ask again.
    Never,
    /// Not yet — something this needs is not resolved at this moment (the impl
    /// owner is only memoized after one full dispatch has loaded the class and
    /// run its `<clinit>`, and the body has to be compiled before there is
    /// anything to call). Do NOT remember: the answer changes.
    NotYet,
}

thread_local! {
    /// `None` is a NEGATIVE entry — this proxy class was examined and found
    /// permanently ineligible. Caching the refusal matters as much as caching
    /// the site: without it, every call of a boxing lambda would redo the whole
    /// shape analysis and then fall through to the generic path anyway, which
    /// is strictly worse than not having this fast path at all.
    ///
    /// Keyed `(vm, proxy, receiver word)`: the receiver word is
    /// [`LAMBDA_SITE_KEY_STATIC`] for the per-proxy site
    /// ([`lambda_jit_site`]); JIT round 20 wave 12 (lane qLam, DISP20-3) adds
    /// the instance-method impl sites of [`lambda_instance_jit_site`] under the
    /// receiver's exact class id, and that function's per-proxy shape verdict
    /// under the `LAMBDA_SITE_KEY_INSTANCE_*` words. A receiver class id is a
    /// real class's (below `LAMBDA_PROXY_ID_BASE`), so the words never meet.
    static LAMBDA_JIT_SITE_CACHE: std::cell::RefCell<
        rustc_hash::FxHashMap<(usize, u32, u32), Option<std::rc::Rc<LambdaJitSite>>>,
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// [`LAMBDA_JIT_SITE_CACHE`]'s receiver word for the per-proxy (static-impl)
/// site.
const LAMBDA_SITE_KEY_STATIC: u32 = u32::MAX;
/// The instance-shape verdict rows of [`lambda_instance_jit_site`] (values are
/// always `None`; the KEY that is present is the verdict): an unbound
/// instance-method impl (`Type::m`), a bound one (`x::m`), or a proxy that is
/// never an instance site.
const LAMBDA_SITE_KEY_INSTANCE_UNBOUND: u32 = u32::MAX - 1;
const LAMBDA_SITE_KEY_INSTANCE_BOUND: u32 = u32::MAX - 2;
const LAMBDA_SITE_KEY_INSTANCE_NEVER: u32 = u32::MAX - 3;
/// JIT round 20 wave 12 (lane qLam, `CRATONVM_JIT_LAMBDA_SITE_BOXING`): the
/// per-proxy row of a STATIC impl site that needs a box / unbox adapter
/// ([`lambda_instance_jit_site`]); `None` = no such site.
const LAMBDA_SITE_KEY_STATIC_ADAPTED: u32 = u32::MAX - 4;

/// Drop the compiled impl bodies this thread's lambda sites still hold after
/// those bodies were withdrawn from the JIT cache, and return how many.
///
/// A site re-probes its body only when it is next CALLED, so a site that is
/// never called again kept a withdrawn body mapped for the life of the thread
/// -- the pin `jit_bridge::release_withdrawn_code_owners` exists to remove. The
/// site forgets its probe too (`u64::MAX`, the value it is built with), so a
/// later call re-probes rather than trusting a generation that predates the
/// withdrawal.
///
/// `try_borrow*`: this can run from a safepoint poll nested inside a lambda
/// dispatch, and a site that is borrowed right now is in use, not dead. It is
/// skipped and caught by the next scan.
pub(crate) fn release_withdrawn_lambda_code() -> usize {
    let released = LAMBDA_JIT_SITE_CACHE.with(|cache| {
        let Ok(cache) = cache.try_borrow() else {
            return 0;
        };
        cache
            .values()
            .flatten()
            .filter(|site| release_withdrawn_site_code(site))
            .count()
    });
    // nio21 w16 L2: the interpreted lambda door's sites and the one-shot
    // sites probe their impl body too (`invoke_fast::lambda_door_compiled_body`,
    // `try_lambda_site_oneshot`), so they can hold a withdrawn one the same way.
    let released = released
        + LAMBDA_ONESHOT_SITE_CACHE.with(|cache| {
            let Ok(cache) = cache.try_borrow() else {
                return 0;
            };
            cache
                .values()
                .flatten()
                .filter(|site| release_withdrawn_site_code(site))
                .count()
        });
    released
        + LAMBDA_DOOR_SITES.with(|sites| {
            let Ok(sites) = sites.try_borrow() else {
                return 0;
            };
            sites
                .values()
                .flat_map(|s| s.entries.iter())
                .filter_map(|e| e.site.as_ref())
                .filter(|site| release_withdrawn_site_code(site))
                .count()
        })
}

/// [`release_withdrawn_lambda_code`] for one site: drop its body if the JIT
/// cache withdrew it, and forget the probe. `true` if it dropped one.
fn release_withdrawn_site_code(site: &LambdaJitSite) -> bool {
    let Ok(mut code) = site.code.try_borrow_mut() else {
        return false;
    };
    if code
        .as_ref()
        .is_some_and(|c| c.retired.load(std::sync::atomic::Ordering::Acquire))
    {
        *code = None;
        site.code_generation.set(u64::MAX);
        return true;
    }
    false
}

/// Look up — building on first use — the direct-call site for a lambda proxy.
///
/// Returns `None` for any shape this fast path may not serve, or any moment at
/// which it cannot yet.
pub(crate) fn lambda_jit_site(
    shared: &SharedVm,
    proxy_class_id: ClassId,
    method_name: &str,
    descriptor: &str,
) -> Option<std::rc::Rc<LambdaJitSite>> {
    if !crate::runtime::env_cache::jit_lambda_tierup()
        || !crate::runtime::env_cache::jit_lambda_site()
        || crate::runtime::env_cache::disable_jit()
    {
        return None;
    }
    let key = (
        shared.vm_identity,
        proxy_class_id.as_u32(),
        LAMBDA_SITE_KEY_STATIC,
    );
    let slot = LAMBDA_JIT_SITE_CACHE.with(|cache| cache.borrow().get(&key).cloned());
    match slot {
        Some(Some(site)) if !site.gate.is_stale() => {
            return site.serves(method_name, descriptor).then_some(site);
        }
        // Redefined out from under us — drop it and re-derive.
        Some(Some(_)) => LAMBDA_JIT_SITE_CACHE.with(|cache| {
            cache.borrow_mut().remove(&key);
        }),
        // A remembered refusal.
        Some(None) => return None,
        None => {}
    }
    // nio21 wave 21 L1: a handler-bearing static impl is admitted once both
    // compiled services route a throwable it leaves through its own table
    // (`CRATONVM_JIT_LAMBDA_HANDLER_IMPL`, default ON).
    let allow_handlers = crate::runtime::exceptions::lambda_handler_impl_enabled();
    match build_lambda_jit_site(shared, proxy_class_id, allow_handlers) {
        SiteVerdict::Eligible(site) => {
            LAMBDA_JIT_SITE_CACHE.with(|cache| {
                cache
                    .borrow_mut()
                    .insert(key, Some(std::rc::Rc::clone(&site)));
            });
            site.serves(method_name, descriptor).then_some(site)
        }
        SiteVerdict::Never => {
            LAMBDA_JIT_SITE_CACHE.with(|cache| {
                cache.borrow_mut().insert(key, None);
            });
            None
        }
        SiteVerdict::NotYet => None,
    }
}

// ---------------------------------------------------------------------------
// Instance-method impl sites (JIT round 20 wave 12, lane qLam: W15L5-5 /
// W17L1-5 / W18L2-2 = DISP20-3 of docs/internal/proposals/jit/jit-r20-dispatch-proposals.md)
// ---------------------------------------------------------------------------
//
// `build_lambda_jit_site` refuses every impl but a static one, so a SAM call
// to `x::m` / `Type::m` / an interface default took `try_lambda_dispatch`'s
// virtual arm on every call: the coercion walk, the receiver screens, the
// receiver-native probe, the cached-template row, and either a one-shot
// compiled entry or `invoke_on_class_shared`'s by-name ladder.
//
// What makes an instance impl different is only WHICH body runs: the virtual
// arm resolves it against the receiver's class (`try_invoke_cached_lambda_impl`
// -> `build_lambda_impl_cached(receiver class, ..)`). So the site is keyed by
// `(proxy, exact receiver class)` and binds the body that class resolves to,
// under exactly the virtual arm's own gates -- no private-owner binding
// (`lambda_private_impl_dispatch_class`), no lambda-proxy receiver, no native
// shadow on the receiver's chain or the declaring class (the template's own
// refusal), no `synchronized` body -- plus the compiled direct arm's (no
// exception table, identity coercions, at most a replayed `checkcast` per
// argument). In the compiled impl's ABI the receiver is Java argument 0, so the
// site's argument vector is `(captures.., samArgs..)` with the receiver first
// either way: a bound reference's receiver IS capture 0, an unbound one's IS
// SAM argument 0. The inline-cache thunk guards the receiver's class id
// (`cratonvm_jit::lambda_adapter::lambda_receiver_adapter_entry`), so any
// other class, a null (the virtual arm's NPE) or an array takes the site's own
// resolving helper.

/// `CRATONVM_JIT_LAMBDA_INSTANCE_THUNK` (default ON, `=0` restores the
/// static-only site): read when a proxy's instance shape is first analysed,
/// once per proxy per thread, never per call (and by the callee-deopt
/// service's identity test, on a refusal only).
pub(crate) fn lambda_instance_thunk_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_LAMBDA_INSTANCE_THUNK")
}

/// The descriptor-only half of an instance-method impl site, a property of the
/// proxy's call site alone.
struct LambdaInstanceShape {
    /// The receiver is the proxy's capture 0 (`x::m`), else SAM argument 0.
    bound: bool,
    num_captures: usize,
    /// Leading descriptor byte of each capture, receiver (`L`) first.
    capture_descs: Vec<u8>,
    /// Receiver + the impl's own parameters == captures + SAM parameters.
    total_args: usize,
    checkcasts: Vec<(usize, Arc<str>)>,
    /// qLam boxing arm (`adapt_ok` only): SAM arguments to unbox, `(index among
    /// the SAM's own arguments, the impl's primitive)`.
    unbox: Vec<(usize, char)>,
    /// qLam boxing arm (`adapt_ok` only): the impl's 32-bit primitive return
    /// to box.
    box_ret: Option<char>,
}

/// JIT round 20 wave 12, lane qLam: `CRATONVM_JIT_LAMBDA_SITE_BOXING` (default
/// ON): may the compiled direct arm's sites perform LambdaMetafactory's exact
/// box / unbox? Read when a proxy's shape is first analysed (once per proxy
/// per thread).
pub(crate) fn lambda_site_boxing_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_LAMBDA_SITE_BOXING")
}

/// JIT round 20 wave 12, lane qLam: `CRATONVM_JIT_LAMBDA_SITE_NATIVE` (default
/// ON): may an instance site whose receiver class's own body is shadowed by a
/// reviewed `Intrinsic` be served through the JIT native site cache? Read when
/// a `(proxy, receiver class)` site is built.
pub(crate) fn lambda_site_native_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_LAMBDA_SITE_NATIVE")
}

/// Parameter `i` of an instance-method impl with its receiver in front: `L`
/// (a reference) for the receiver, else the impl's declared parameter `i - 1`.
fn lambda_instance_param<'a>(impl_params: &[&'a str], i: usize) -> &'a str {
    if i == 0 {
        "L"
    } else {
        impl_params[i - 1]
    }
}

/// [`LambdaInstanceShape`] of a call site, or `None` for a shape this arm
/// never serves: an impl kind other than `REF_invokeVirtual` /
/// `REF_invokeInterface` (`REF_invokeSpecial` binds its owner, statics are
/// [`build_lambda_jit_site`]'s), a coercion that is not the identity (boxing,
/// unboxing, widening, a `void` SAM over a valued impl), or an arity the
/// receiver-first layout does not explain.
fn lambda_instance_shape(
    call_site: &crate::classloading::resolution::LambdaCallSite,
    adapt_ok: bool,
) -> Option<LambdaInstanceShape> {
    if !matches!(
        call_site.impl_handle.kind,
        MethodHandleKind::InvokeVirtual | MethodHandleKind::InvokeInterface
    ) {
        return None;
    }
    // The static builder's identity rule (`coerce_arg` / `coerce_return`
    // return their input unchanged exactly for equal tokens or two references).
    let identity = |a: &str, b: &str| a == b || (is_reference_desc(a) && is_reference_desc(b));
    let (sam_params, sam_ret) = split_method_descriptor_ref(&call_site.sam_descriptor);
    let (impl_params, impl_ret) = split_method_descriptor_ref(&call_site.impl_handle.descriptor);
    let (inst_params, _inst_ret) = split_method_descriptor_ref(&call_site.instantiated_descriptor);
    let mut box_ret: Option<char> = None;
    if !identity(sam_ret, impl_ret) {
        // qLam boxing arm: LambdaMetafactory's exact box of a 32-bit return.
        box_ret = Some(lambda_box_return_prim(sam_ret, impl_ret).filter(|_| adapt_ok)?);
    }
    let num_captures = call_site.capture_types.len();
    // The receiver, then the impl's declared parameters: what the compiled
    // instance body takes, and what LambdaMetafactory feeds it from
    // `(captures.., samArgs..)`.
    let total_args = 1 + impl_params.len();
    if total_args != num_captures + sam_params.len() {
        return None;
    }
    let mut unbox: Vec<(usize, char)> = Vec::new();
    for (k, sam_tok) in sam_params.iter().enumerate() {
        let impl_tok = lambda_instance_param(&impl_params, num_captures + k);
        if !identity(sam_tok, impl_tok) {
            // qLam boxing arm: an exact-wrapper unbox (never the receiver,
            // whose token is a reference).
            let prim = lambda_unbox_param_prim(sam_tok, impl_tok, inst_params.get(k).copied())
                .filter(|_| adapt_ok)?;
            unbox.push((k, prim));
        }
    }
    let capture_descs = (0..num_captures)
        .map(|i| {
            lambda_instance_param(&impl_params, i)
                .as_bytes()
                .first()
                .copied()
        })
        .collect::<Option<Vec<u8>>>()?;
    // What `checkcast_lambda_instantiated_args` would check (the static
    // builder's rule, unchanged): a reference instantiated token that narrows
    // the erased SAM token. An unbound receiver's own cast is one of them.
    let mut checkcasts: Vec<(usize, Arc<str>)> = Vec::new();
    for (k, inst_tok) in inst_params.iter().enumerate() {
        if !is_reference_desc(inst_tok) {
            continue;
        }
        // qLam: an unboxed argument's cast is the exact-wrapper test itself.
        if unbox.iter().any(|&(u, _)| u == k) {
            continue;
        }
        if sam_params.get(k).map(|s| s == inst_tok).unwrap_or(false) {
            continue;
        }
        if k >= sam_params.len() {
            return None;
        }
        checkcasts.push((k, Arc::from(*inst_tok)));
    }
    Some(LambdaInstanceShape {
        bound: num_captures > 0,
        num_captures,
        capture_descs,
        total_args,
        checkcasts,
        unbox,
        box_ret,
    })
}

/// The direct-call site for a SAM call whose impl is an INSTANCE method,
/// resolved against the call's own receiver (`proxy`'s capture 0 for a bound
/// reference, `args_slice[1]` -- SAM argument 0 -- for an unbound one).
/// `args_slice` is the compiled caller's argument words, proxy first.
///
/// `None` for a static-impl proxy (that is [`lambda_jit_site`]'s), any shape
/// this arm never serves, a null or array receiver (the virtual arm raises the
/// NPE / decides the call), or a moment at which the site cannot be built yet.
/// Per thread, in [`LAMBDA_JIT_SITE_CACHE`] (see its key note), so a repeat
/// costs one or two hash probes and no lock.
pub(crate) fn lambda_instance_jit_site(
    shared: &SharedVm,
    proxy_class_id: ClassId,
    proxy: ObjectRef,
    args_slice: &[i64],
    method_name: &str,
    descriptor: &str,
) -> Option<std::rc::Rc<LambdaJitSite>> {
    if !crate::runtime::env_cache::jit_lambda_tierup()
        || !crate::runtime::env_cache::jit_lambda_site()
        || crate::runtime::env_cache::disable_jit()
    {
        return None;
    }
    let vm = shared.vm_identity;
    let proxy_word = proxy_class_id.as_u32();
    // JIT round 20 wave 12 (lane qLam, `CRATONVM_JIT_LAMBDA_SITE_BOXING`): a
    // STATIC impl that needs LambdaMetafactory's exact box / unbox (which
    // `lambda_jit_site` refuses) has its own per-proxy row holding the site.
    let static_adapted = LAMBDA_JIT_SITE_CACHE.with(|cache| {
        cache
            .borrow()
            .get(&(vm, proxy_word, LAMBDA_SITE_KEY_STATIC_ADAPTED))
            .cloned()
    });
    match static_adapted {
        Some(Some(site)) if !site.gate.is_stale() => {
            return site.serves(method_name, descriptor).then_some(site);
        }
        Some(Some(_)) => {
            // Redefined: forget the row (and the never-verdict beside it) and
            // analyse again on the next call.
            LAMBDA_JIT_SITE_CACHE.with(|cache| {
                let mut cache = cache.borrow_mut();
                cache.remove(&(vm, proxy_word, LAMBDA_SITE_KEY_STATIC_ADAPTED));
                cache.remove(&(vm, proxy_word, LAMBDA_SITE_KEY_INSTANCE_NEVER));
            });
            return None;
        }
        Some(None) | None => {}
    }
    // The per-proxy shape verdict: `Some(Some(bound))`, `Some(None)` for
    // never, `None` for not analysed yet.
    let verdict = LAMBDA_JIT_SITE_CACHE.with(|cache| {
        let cache = cache.borrow();
        if cache.contains_key(&(vm, proxy_word, LAMBDA_SITE_KEY_INSTANCE_UNBOUND)) {
            Some(Some(false))
        } else if cache.contains_key(&(vm, proxy_word, LAMBDA_SITE_KEY_INSTANCE_BOUND)) {
            Some(Some(true))
        } else if cache.contains_key(&(vm, proxy_word, LAMBDA_SITE_KEY_INSTANCE_NEVER)) {
            Some(None)
        } else {
            None
        }
    });
    let bound = match verdict {
        Some(Some(bound)) => bound,
        Some(None) => return None,
        None => {
            let call_site = shared.classes.lambda_call_site_for(proxy_class_id);
            let boxing = lambda_site_boxing_enabled();
            // qLam: a static impl's adapted site, built once per proxy.
            if boxing
                && call_site
                    .as_ref()
                    .is_some_and(|cs| matches!(cs.impl_handle.kind, MethodHandleKind::InvokeStatic))
            {
                let allow_handlers = crate::runtime::exceptions::lambda_handler_impl_enabled();
                let site = match build_lambda_jit_site_adapting(
                    shared,
                    proxy_class_id,
                    allow_handlers,
                    true,
                ) {
                    // An identity site is `lambda_jit_site`'s; this row holds
                    // only an adapted one.
                    SiteVerdict::Eligible(site) => site.adapt.is_some().then_some(site),
                    SiteVerdict::Never => None,
                    SiteVerdict::NotYet => return None,
                };
                LAMBDA_JIT_SITE_CACHE.with(|cache| {
                    let mut cache = cache.borrow_mut();
                    cache.insert(
                        (vm, proxy_word, LAMBDA_SITE_KEY_STATIC_ADAPTED),
                        site.as_ref().map(std::rc::Rc::clone),
                    );
                    // A static impl is never an instance site.
                    cache.insert((vm, proxy_word, LAMBDA_SITE_KEY_INSTANCE_NEVER), None);
                });
                return site.filter(|site| site.serves(method_name, descriptor));
            }
            let shape = if lambda_instance_thunk_enabled() {
                call_site
                    .and_then(|call_site| lambda_instance_shape(&call_site, boxing))
                    .map(|shape| shape.bound)
            } else {
                None
            };
            let row = match shape {
                Some(false) => LAMBDA_SITE_KEY_INSTANCE_UNBOUND,
                Some(true) => LAMBDA_SITE_KEY_INSTANCE_BOUND,
                None => LAMBDA_SITE_KEY_INSTANCE_NEVER,
            };
            LAMBDA_JIT_SITE_CACHE.with(|cache| {
                cache.borrow_mut().insert((vm, proxy_word, row), None);
            });
            shape?
        }
    };
    // The receiver word, read where the thunk reads it.
    let receiver_raw = if bound {
        match shared.mem.heap.get_field(proxy, 0) {
            Value::Object(Some(r)) => r.as_ptr() as i64, // Cast: JIT ABI -- pointer to i64
            _ => 0,
        }
    } else {
        args_slice.get(1).copied().unwrap_or(0)
    };
    if receiver_raw == 0 {
        // A null receiver: the virtual arm raises HotSpot's NPE.
        return None;
    }
    // SAFETY: a non-zero reference word is a live object pointer -- the same
    // assumption every other raw-argument arm in the JIT bridge makes (the
    // shape analysis proved this slot is a reference).
    let receiver = unsafe { ObjectRef::from_raw(receiver_raw as *mut u8) };
    // An array's header names its COMPONENT's class id.
    if shared.mem.heap.kind_of(receiver) == cratonvm_types::ObjectKind::Array {
        return None;
    }
    let receiver_class_id = shared.mem.heap.class_id_of(receiver);
    if receiver_class_id == ClassId::new(0)
        || receiver_class_id.as_u32() >= crate::vm::realms::class_realm::LAMBDA_PROXY_ID_BASE
    {
        // A generic-id object, or another lambda proxy (the virtual arm
        // re-dispatches through that proxy's own SAM).
        return None;
    }
    let key = (vm, proxy_word, receiver_class_id.as_u32());
    let slot = LAMBDA_JIT_SITE_CACHE.with(|cache| cache.borrow().get(&key).cloned());
    match slot {
        Some(Some(site)) if !site.gate.is_stale() => {
            return site.serves(method_name, descriptor).then_some(site);
        }
        Some(Some(_)) => LAMBDA_JIT_SITE_CACHE.with(|cache| {
            cache.borrow_mut().remove(&key);
        }),
        Some(None) => return None,
        None => {}
    }
    match build_lambda_instance_jit_site(shared, proxy_class_id, bound, receiver_class_id) {
        SiteVerdict::Eligible(site) => {
            LAMBDA_JIT_SITE_CACHE.with(|cache| {
                cache
                    .borrow_mut()
                    .insert(key, Some(std::rc::Rc::clone(&site)));
            });
            site.serves(method_name, descriptor).then_some(site)
        }
        SiteVerdict::Never => {
            LAMBDA_JIT_SITE_CACHE.with(|cache| {
                cache.borrow_mut().insert(key, None);
            });
            None
        }
        SiteVerdict::NotYet => None,
    }
}

/// Does one of THIS thread's instance-method impl sites of `proxy_class_id`
/// bind exactly the method `key_class.key_method key_desc`? The compiled
/// caller's callee-deopt service asks it (through
/// `jit::helpers::lambda_site_resolves_to`) to recognise a stash left by a body
/// it entered through a receiver thunk: that body is the one the RECEIVER's
/// class resolves to (`Derived.m` for a `Base::m` handle), which the
/// metafactory's handle does not name. Only a site this thread built can have
/// installed the thunk the stashing call went through. Cold: asked on a refusal
/// of the handle's own identity only.
pub(crate) fn lambda_instance_site_binds(
    shared: &SharedVm,
    proxy_class_id: ClassId,
    key_class: &str,
    key_method: &str,
    key_desc: &str,
) -> bool {
    let vm = shared.vm_identity;
    let proxy_word = proxy_class_id.as_u32();
    LAMBDA_JIT_SITE_CACHE.with(|cache| {
        let Ok(cache) = cache.try_borrow() else {
            return false;
        };
        cache.iter().any(|(&(v, p, _), site)| {
            v == vm
                && p == proxy_word
                && site.as_ref().is_some_and(|site| {
                    site.receiver.is_some()
                        && &*site.cached.class_name == key_class
                        && &*site.cached.method_name == key_method
                        && &*site.cached.method_descriptor == key_desc
                })
        })
    })
}

/// JIT round 20, lane rev12 (correctness, unconditional): may this site bind
/// the method `find_method_recursive` finds from the RECEIVER's class? That
/// walk is name-and-descriptor only, while JVMS 5.4.6 selects only a method
/// that OVERRIDES the resolved one (5.4.5): never a private method, and a
/// package-private resolved method only from its own runtime package. So when
/// the receiver's walk lands on another declaring class than the handle
/// owner's walk, the site is refused unless the selected method is not
/// private and the resolved one is public or protected -- the cases where
/// the two walks provably agree with JVMS. A refusal keeps the call on the
/// general path; it never moves one onto the cached body.
fn lambda_receiver_selection_overrides(
    shared: &SharedVm,
    call_site: &crate::classloading::resolution::LambdaCallSite,
    receiver_class_id: ClassId,
) -> bool {
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    let cm = shared.classes.class_manager.read();
    let Some(owner_id) = cm.get_loaded_class_id(&call_site.impl_handle.class_name) else {
        return false;
    };
    let store = cm.class_store();
    let name = &*call_site.impl_handle.member_name;
    let descriptor = &*call_site.impl_handle.descriptor;
    let Some((resolved, resolved_in)) =
        crate::classloading::find_method_recursive(owner_id, name, descriptor, store)
    else {
        return false;
    };
    let Some((selected, selected_in)) =
        crate::classloading::find_method_recursive(receiver_class_id, name, descriptor, store)
    else {
        return false;
    };
    if selected_in == resolved_in {
        return true;
    }
    !selected.access_flags.contains(MethodAccessFlags::PRIVATE)
        && resolved
            .access_flags
            .intersects(MethodAccessFlags::PUBLIC | MethodAccessFlags::PROTECTED)
}

/// [`build_lambda_jit_site`] for an instance-method impl and one exact
/// receiver class: the body `try_lambda_dispatch`'s virtual arm would run for
/// a receiver of that class through its cached template, and only when that
/// arm would take the template (see the section note above).
fn build_lambda_instance_jit_site(
    shared: &SharedVm,
    proxy_class_id: ClassId,
    bound: bool,
    receiver_class_id: ClassId,
) -> SiteVerdict {
    let Some(call_site) = shared.classes.lambda_call_site_for(proxy_class_id) else {
        return SiteVerdict::Never;
    };
    let Some(shape) = lambda_instance_shape(&call_site, lambda_site_boxing_enabled()) else {
        return SiteVerdict::Never;
    };
    if shape.bound != bound || shared.classes.is_lambda_proxy_class(receiver_class_id) {
        return SiteVerdict::Never;
    }
    // A private impl binds its declaring class, never the receiver's
    // (`exact_impl_owner` in the virtual arm). That verdict reads the owner by
    // name, so an owner not loaded globally is refused rather than guessed.
    // (The read guard is a statement temporary: released before the private
    // verdict, which takes its own.)
    let owner_loaded = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(&call_site.impl_handle.class_name)
        .is_some();
    if !owner_loaded || lambda_private_impl_dispatch_class(shared, &call_site).is_some() {
        return SiteVerdict::Never;
    }
    if !lambda_receiver_selection_overrides(shared, &call_site, receiver_class_id) {
        return SiteVerdict::Never;
    }
    // The virtual arm's template for this receiver class: `None` for a native
    // shadow anywhere on the receiver's chain or the declaring class, an
    // abstract or `synchronized` body, an unresolvable class.
    //
    // qLam (`CRATONVM_JIT_LAMBDA_SITE_NATIVE`): a native shadow that is the
    // receiver class's own reviewed `Intrinsic` is served through the JIT
    // native site cache instead ([`lambda_native_instance_target`]).
    let (cached, gate, native) = match build_lambda_impl_cached(
        shared,
        receiver_class_id,
        &call_site.impl_handle.member_name,
        &call_site.impl_handle.descriptor,
    ) {
        Some((cached, gate)) => (cached, gate, None),
        None if lambda_site_native_enabled() => {
            match lambda_native_instance_target(
                shared,
                receiver_class_id,
                &call_site.impl_handle.member_name,
                &call_site.impl_handle.descriptor,
            ) {
                Some((cached, gate, info)) => (cached, gate, Some(info)),
                None => return SiteVerdict::Never,
            }
        }
        None => return SiteVerdict::Never,
    };
    // A handler-bearing body is never entered by a direct compiled call here
    // (the static site admits one under `CRATONVM_JIT_LAMBDA_HANDLER_IMPL`;
    // this arm does not), nor a static or monitor-taking one.
    if cached.is_static || cached.is_synchronized || !cached.exception_table.is_empty() {
        return SiteVerdict::Never;
    }
    let cast_pass_cids: Vec<std::cell::Cell<u32>> = shape
        .checkcasts
        .iter()
        .map(|_| std::cell::Cell::new(u32::MAX))
        .collect();
    // qLam: the Rust-side adapter (box / unbox, native target), if any.
    let adapt = if shape.unbox.is_empty() && shape.box_ret.is_none() && native.is_none() {
        None
    } else {
        let Some(unbox) = lambda_unbox_with_wrapper_ids(shared, &shape.unbox) else {
            return SiteVerdict::NotYet;
        };
        Some(Box::new(LambdaAdapt {
            unbox,
            box_ret: shape.box_ret,
            native,
        }))
    };
    if let Some(adapt) = &adapt {
        note_adapted_site(&cached, adapt);
    }
    SiteVerdict::Eligible(std::rc::Rc::new(LambdaJitSite {
        sam_method_name: Arc::clone(&call_site.sam_method_name),
        sam_descriptor: Arc::clone(&call_site.sam_descriptor),
        num_captures: shape.num_captures,
        capture_descs: shape.capture_descs,
        total_args: shape.total_args,
        checkcasts: shape.checkcasts,
        cast_pass_cids,
        cached,
        gate,
        code: std::cell::RefCell::new(None),
        code_generation: std::cell::Cell::new(u64::MAX),
        adapter_slots: std::cell::Cell::new((0, 0)),
        direct_disabled: std::cell::Cell::new(false),
        // The constant-return probe's self-test runs a thunk with made-up
        // arguments, which a receiver guard would read: not for this site.
        const_return: None,
        receiver: Some((bound, receiver_class_id)),
        adapt,
    }))
}

/// JIT round 20 wave 12, lane qLam (`CRATONVM_JIT_LAMBDA_SITE_NATIVE`; the
/// w18-L2 item 6 audit): the native target of an instance-method impl whose
/// cached template was refused for a native shadow, when -- and only when --
/// every door this VM has would run that same native for a receiver of this
/// exact class:
///
/// * the receiver class itself declares the method, with concrete bytecode
///   (not `native`, not static, not synchronized), and is a real class (not a
///   class-file-less representation, not an interface, not a
///   `cratonvm/internal/` wrapper) -- so `invoke_on_class_shared_inner`'s
///   exact-class-native prefix and its receiver-native rescues do not apply,
///   and the general virtual arm's receiver-keyed native (registered on a
///   receiver that does NOT declare the method) cannot either;
/// * the registration for exactly that triple is a reviewed `Intrinsic`, and
///   `resolve_dispatch` -- §7, the one decision point -- answers `Intrinsic` for
///   it under this VM's policy (step 2: the intrinsic shadows the bytecode in
///   both modes; `CRATONVM_PURE_BYTECODE` turns it back to bytecode, and then
///   this declines). The by-name ladder reaches the same callback whichever of
///   its arms answers: the compatibility name chain admits the registry's
///   callback for the declaring triple, and its tail asks `resolve_dispatch`.
///   A `Bridge` or `SyntheticStub` is never admitted here: there the modes'
///   ladders and the compiled `invokevirtual` site cache can disagree.
///
/// The call itself goes through the JIT native site cache (the door a
/// compiled `invokevirtual` of the method takes), which re-checks the
/// registry generation, redefinition, the capability gate and `--jdk-only`
/// admission, and declines (the general path then runs) whenever it would not
/// serve. Returns the method's bytecode template and redefinition gate (for the
/// site's identity and staleness only; the template is never run here) and the
/// leaked call-site record the site cache is keyed by (one per site).
fn lambda_native_instance_target(
    shared: &SharedVm,
    receiver_class_id: ClassId,
    method_name: &str,
    descriptor: &str,
) -> Option<(
    Arc<CachedBytecodeMethod>,
    RedefineGate,
    &'static cratonvm_jit::JitInvokeInfo,
)> {
    let class_name: Arc<str> = {
        let cm = shared.classes.class_manager.read();
        let store = cm.class_store();
        let (method, declaring_id) = crate::classloading::find_method_recursive(
            receiver_class_id,
            method_name,
            descriptor,
            store,
        )?;
        if declaring_id != receiver_class_id
            || method.is_native()
            || method.is_static()
            || method.is_synchronized()
            || method.code().is_none()
        {
            return None;
        }
        let class = store.get(receiver_class_id)?;
        if class.dispatch_lacks_class_file()
            || class.is_interface()
            || class.name.starts_with("cratonvm/internal/")
        {
            return None;
        }
        let registration =
            shared
                .natives
                .native_methods
                .find_with_kind(&class.name, method_name, descriptor)?;
        if registration.1 != cratonvm_native_api::NativeKind::Intrinsic {
            return None;
        }
        match crate::vm::resolve_dispatch(
            crate::vm::dispatch_policy(shared),
            class,
            method,
            Some(registration),
        ) {
            crate::vm::DispatchDecision::Intrinsic(_) => {}
            _ => return None,
        }
        Arc::clone(&class.name)
    };
    let (cached, gate) = build_bytecode_callee_cached_admitting_native(
        shared,
        receiver_class_id,
        method_name,
        descriptor,
    )?;
    // One call-site record per (receiver class, method, descriptor) for this
    // VM, for the life of the process: the native site cache keys its memo by
    // this address. Interned (JIT r20 lane rev12): the lambda site caches are
    // per thread, and a fresh leak per build grew without bound with threads.
    let info: &'static cratonvm_jit::JitInvokeInfo = *shared
        .classes
        .lambda_native_infos
        .lock()
        .entry((
            receiver_class_id,
            Box::from(method_name),
            Box::from(descriptor),
        ))
        .or_insert_with(|| {
            Box::leak(Box::new(cratonvm_jit::JitInvokeInfo {
                class_name: Box::leak(String::from(&*class_name).into_boxed_str()),
                method_name: Box::leak(String::from(method_name).into_boxed_str()),
                descriptor: Box::leak(String::from(descriptor).into_boxed_str()),
                num_jit_args: 1 + count_method_params(descriptor),
                return_type: crate::jit::return_type(descriptor),
                invoke_kind: 0,
                declaring_class_id: 0,
                owner_class_id: 0,
            }))
        });
    Some((cached, gate, info))
}

// ---------------------------------------------------------------------------
// The interpreted lambda call door (SBR5 lane interp)
// ---------------------------------------------------------------------------
//
// An interpreted `invokeinterface` on a lambda proxy used to leave the dispatch
// loop: door miss, cache miss, vtable-fast miss, `execute_invoke_kind` (constant
// pool decode, `Value` arguments), `try_lambda_dispatch` (descriptor walks,
// `coerce_lambda_args`, owner memo, thread-local impl cache) and a NESTED
// interpreter entry for the impl body -- measured 730-1160 ns for `x -> 7`
// against 19-30 ns on HotSpot `-Xint` (`Sbr5InterpKern lambda`), and 44% of a
// cold `MethodHandles.countedLoop` (it builds ~600 lambda calls). For the shape
// the JIT's `LambdaJitSite` already recognizes (static impl, coercion provably
// the identity, at most a `checkcast` per argument) that detour computes a
// constant: read the captures, call the impl. This door answers it from the
// loop: the impl's frame is pushed with the captures as its leading locals and
// the SAM arguments moved off the caller's stack, and the impl's own return
// pushes its value to the caller like any callee's.
//
// Per thread, keyed `(vm, caller class, constant-pool index)` because the
// call-site's SAM arity is a property of that key and the receiver sits that
// deep on the stack; the entries under a key are the proxy classes seen there.
// A `None` entry is a remembered refusal (a shape the door never serves). The
// entries are filled by the SLOW path, after a successful dispatch has proved
// the impl owner loaded and initialized (`note_lambda_door_site`).

struct LambdaDoorEntry {
    proxy: u32,
    site: Option<std::rc::Rc<LambdaJitSite>>,
}

struct LambdaDoorSite {
    /// SAM arguments of this call-site, receiver excluded.
    num_args: usize,
    /// `class_redefinition_count()` when `entries` were recorded.
    redefinitions: u64,
    entries: Vec<LambdaDoorEntry>,
}

/// Distinct proxy classes remembered per call-site; a megamorphic site stops
/// adding entries and keeps the general path for the rest.
const LAMBDA_DOOR_ENTRIES_PER_SITE: usize = 8;

thread_local! {
    static LAMBDA_DOOR_SITES: std::cell::RefCell<
        // addr-keyed: not a heap address -- (VM identity, caller class id, cp index).
        rustc_hash::FxHashMap<(usize, u32, u16), LambdaDoorSite>,
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
    /// Set once this thread has recorded any door site; the loop's cheap
    /// admission test, so a thread with no lambda call pays one TLS read.
    static LAMBDA_DOOR_LIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether this thread has any lambda door site at all.
#[inline]
pub(super) fn lambda_door_live() -> bool {
    LAMBDA_DOOR_LIVE.with(std::cell::Cell::get)
}

/// The door's site for `(caller, cp_index)` and the receiver's proxy class:
/// `receiver_proxy` is handed the call-site's SAM arity and answers the proxy
/// class of the receiver that deep on the stack, or `None` when it is not a
/// lambda proxy. Returns `None` for an unknown call-site, a proxy never
/// recorded, a remembered refusal, or a site whose impl class was redefined
/// (that entry is dropped, so the slow path records the fresh one).
///
/// `receiver_proxy` runs under this thread's table borrow and must not call
/// back into lambda dispatch.
#[inline]
pub(super) fn lambda_door_lookup(
    vm_identity: usize,
    caller: ClassId,
    cp_index: u16,
    receiver_proxy: impl FnOnce(usize) -> Option<ClassId>,
) -> Option<std::rc::Rc<LambdaJitSite>> {
    LAMBDA_DOOR_SITES.with(|sites| {
        let mut sites = sites.borrow_mut();
        let site = sites.get_mut(&(vm_identity, caller.as_u32(), cp_index))?;
        // A redefinition replaces a class's constant pool under the same
        // `ClassId`, so `(caller, cp_index)` may name another call-site
        // afterwards (the invoke cache retires its entries the same way,
        // `InvokeCache::retire_after_redefinition`). Any redefinition since
        // this site was recorded forgets it; the slow path records it afresh.
        if site.redefinitions != cratonvm_classloading::class_redefinition_count() {
            site.entries.clear();
            return None;
        }
        let proxy = receiver_proxy(site.num_args)?.as_u32();
        let at = site.entries.iter().position(|e| e.proxy == proxy)?;
        let hit = site.entries[at].site.as_ref()?;
        if hit.gate_is_stale() {
            site.entries.swap_remove(at);
            return None;
        }
        Some(std::rc::Rc::clone(hit))
    })
}

/// Record, after a SUCCESSFUL general-path lambda dispatch of `proxy` at
/// `(caller, cp_index)`, whether the door may serve that pair from now on.
///
/// Eligibility is `build_lambda_jit_site`'s (handler-bearing impls admitted)
/// plus that the call's own name and descriptor are the SAM's and its arity is
/// the impl's less the captures. A shape that is not yet decidable (the impl
/// owner is memoized only by a completed dispatch) records nothing and is
/// asked again by the next slow call.
pub(crate) fn note_lambda_door_site(
    shared: &SharedVm,
    caller: ClassId,
    cp_index: u16,
    proxy: ClassId,
    method_name: &str,
    descriptor: &str,
    num_args: usize,
) {
    let key = (shared.vm_identity, caller.as_u32(), cp_index);
    // Read BEFORE the shape analysis, like every resolution that fills a cache:
    // a redefinition that lands during it makes this record already stale.
    let redefinitions = cratonvm_classloading::class_redefinition_count();
    let known = LAMBDA_DOOR_SITES.with(|sites| {
        sites.borrow().get(&key).is_some_and(|s| {
            s.entries.len() >= LAMBDA_DOOR_ENTRIES_PER_SITE
                || s.entries.iter().any(|e| e.proxy == proxy.as_u32())
        })
    });
    if known {
        return;
    }
    let site = match build_lambda_jit_site(shared, proxy, true) {
        SiteVerdict::NotYet => return,
        SiteVerdict::Never => None,
        SiteVerdict::Eligible(site) => (site.serves(method_name, descriptor)
            && site.total_args() == site.num_captures() + num_args)
            .then_some(site),
    };
    LAMBDA_DOOR_SITES.with(|sites| {
        let mut sites = sites.borrow_mut();
        let entry = sites.entry(key).or_insert_with(|| LambdaDoorSite {
            num_args,
            redefinitions,
            entries: Vec::new(),
        });
        // A site recorded before a redefinition is forgotten (see the lookup),
        // and what is recorded now starts under the current count.
        if entry.redefinitions != redefinitions {
            entry.entries.clear();
            entry.redefinitions = redefinitions;
        }
        // One descriptor per call-site: a disagreeing arity cannot happen, and
        // if it somehow did, this key must not serve anything.
        if entry.num_args != num_args {
            return;
        }
        entry.entries.push(LambdaDoorEntry {
            proxy: proxy.as_u32(),
            site,
        });
    });
    LAMBDA_DOOR_LIVE.with(|live| live.set(true));
}

/// The shape analysis, run once per proxy class per thread.
///
/// Every `Never` here is a shape whose dispatch is not "read the captures, call
/// the impl" — a boxing SAM, a generic call site needing the `checkcast` the
/// synthetic bridge would have performed, an instance-method reference, a body
/// with its own exception table. Those keep the generic path, unchanged.
///
/// `allow_handlers` is the interpreted lambda door's (`lambda_door_*`): a
/// handler-bearing impl body is barred from a direct COMPILED call, but an
/// interpreted frame runs its exception table like any other callee's.
fn build_lambda_jit_site(
    shared: &SharedVm,
    proxy_class_id: ClassId,
    allow_handlers: bool,
) -> SiteVerdict {
    build_lambda_jit_site_adapting(shared, proxy_class_id, allow_handlers, false)
}

/// [`build_lambda_jit_site`], and with `adapt_ok` (JIT round 20 wave 12, lane
/// qLam: `CRATONVM_JIT_LAMBDA_SITE_BOXING`, asked only by the compiled direct
/// arm's [`lambda_instance_jit_site`]) also the shapes whose coercion is
/// LambdaMetafactory's exact box / unbox ([`lambda_box_return_prim`],
/// [`lambda_unbox_param_prim`]), recorded in the site's [`LambdaAdapt`]. With
/// `adapt_ok` false it is the function it always was: every caller other than
/// that arm (the interpreted door, the one-shot sites) passes false, because
/// none of them performs an adapter.
fn build_lambda_jit_site_adapting(
    shared: &SharedVm,
    proxy_class_id: ClassId,
    allow_handlers: bool,
    adapt_ok: bool,
) -> SiteVerdict {
    let Some(call_site) = shared.classes.lambda_call_site_for(proxy_class_id) else {
        return SiteVerdict::Never;
    };

    // Only a static impl. An InvokeVirtual/InvokeInterface impl takes a
    // captured receiver as arg 0 and re-dispatches virtually on it; binding one
    // fixed body here would answer for a receiver whose class decides.
    if !matches!(call_site.impl_handle.kind, MethodHandleKind::InvokeStatic) {
        return SiteVerdict::Never;
    }

    // Coercion must be provably the identity, argument by argument, and the
    // only thing left over may be a `checkcast`.
    //
    // `coerce_arg` returns its input unchanged in exactly two cases: equal
    // tokens (see its own "LOAD-BEARING BEYOND THIS FUNCTION" note), and two
    // REFERENCE tokens — neither its unbox arm (`Object` SAM over a primitive
    // impl), its box arm, nor its widening arm can fire when both sides are
    // references. `coerce_return` has the same three arms in the same order and
    // so the same two identity cases. Anything else is real work this arm must
    // not skip.
    let identity = |a: &str, b: &str| a == b || (is_reference_desc(a) && is_reference_desc(b));
    let (sam_params, sam_ret) = split_method_descriptor_ref(&call_site.sam_descriptor);
    let (impl_params, impl_ret) = split_method_descriptor_ref(&call_site.impl_handle.descriptor);
    // qLam: read before the identity walk, which asks it for an unbox.
    let (inst_params, _inst_ret) = split_method_descriptor_ref(&call_site.instantiated_descriptor);
    let mut box_ret: Option<char> = None;
    if !identity(sam_ret, impl_ret) {
        match lambda_box_return_prim(sam_ret, impl_ret).filter(|_| adapt_ok) {
            Some(prim) => box_ret = Some(prim),
            None => return SiteVerdict::Never,
        }
    }
    let num_captures = call_site.capture_types.len();
    if impl_params.len() != num_captures + sam_params.len() {
        return SiteVerdict::Never;
    }
    let mut unbox: Vec<(usize, char)> = Vec::new();
    for (k, sam_tok) in sam_params.iter().enumerate() {
        let impl_tok = impl_params[num_captures + k];
        if !identity(sam_tok, impl_tok) {
            match lambda_unbox_param_prim(sam_tok, impl_tok, inst_params.get(k).copied())
                .filter(|_| adapt_ok)
            {
                Some(prim) => unbox.push((k, prim)),
                None => return SiteVerdict::Never,
            }
        }
    }
    // What `checkcast_lambda_instantiated_args` would check, decided once:
    // a reference instantiated token that differs from the erased SAM token.
    // Its own loop skips every other case.
    let mut checkcasts: Vec<(usize, Arc<str>)> = Vec::new();
    for (k, inst_tok) in inst_params.iter().enumerate() {
        if !is_reference_desc(inst_tok) {
            continue;
        }
        // qLam: an unboxed argument's cast is the exact-wrapper test itself.
        if unbox.iter().any(|&(u, _)| u == k) {
            continue;
        }
        if sam_params.get(k).map(|s| s == inst_tok).unwrap_or(false) {
            continue;
        }
        if k >= sam_params.len() {
            // An instantiated descriptor longer than the SAM's is a shape this
            // arm has no mapping for.
            return SiteVerdict::Never;
        }
        checkcasts.push((k, Arc::from(*inst_tok)));
    }
    let cast_pass_cids: Vec<std::cell::Cell<u32>> = checkcasts
        .iter()
        .map(|_| std::cell::Cell::new(u32::MAX))
        .collect();

    // A loader-local divergence must dispatch on the exact class the generic
    // path would choose, not on the global copy this site would bind.
    if lambda_impl_dispatch_override(shared, &call_site).is_some() {
        return SiteVerdict::Never;
    }
    // The impl's owner is memoized only after a full dispatch has loaded the
    // class and run its `<clinit>` — so a miss here is `NotYet`, not `Never`.
    let Some(owner) = lambda_global_impl_owner(shared, &call_site) else {
        return SiteVerdict::NotYet;
    };

    let Some((cached, gate)) = build_lambda_impl_cached(
        shared,
        owner,
        &call_site.impl_handle.member_name,
        &call_site.impl_handle.descriptor,
    ) else {
        // A native shadow, an abstract or `synchronized` body, an unresolvable
        // class: all properties of the shape, none of them transient.
        return SiteVerdict::Never;
    };
    // A handler-bearing callee is never entered by a direct compiled call —
    // the same gate as every other direct-call site in this VM
    // (`mic_callee_has_exception_table`, `osr_callee_bars_direct_call`).
    if (!allow_handlers && !cached.exception_table.is_empty())
        || cached.is_synchronized
        || !cached.is_static
    {
        return SiteVerdict::Never;
    }
    // The captures are the impl's leading parameters — the same slice
    // `lambda_jit_site_capture_args` reads out of the proxy, in the same order.
    let capture_descs: Vec<u8> = impl_params[..num_captures]
        .iter()
        .filter_map(|tok| tok.as_bytes().first().copied())
        .collect();
    if capture_descs.len() != num_captures {
        // An empty parameter token is malformed metadata, not a shape.
        return SiteVerdict::Never;
    }
    // qLam: the adapter's wrapper classes, resolved now (a boot class is
    // loaded long before a wrapper can be passed; not yet loaded = ask again).
    let adapt = if unbox.is_empty() && box_ret.is_none() {
        None
    } else {
        let Some(unbox) = lambda_unbox_with_wrapper_ids(shared, &unbox) else {
            return SiteVerdict::NotYet;
        };
        Some(Box::new(LambdaAdapt {
            unbox,
            box_ret,
            native: None,
        }))
    };
    if let Some(adapt) = &adapt {
        note_adapted_site(&cached, adapt);
    }
    // Read off the impl BEFORE it is moved into the site: a property of the
    // method, decided once. Not for an adapted site (its raw return is not
    // what the caller receives).
    let const_return = if adapt.is_some() {
        None
    } else {
        const_int_return_of(&cached.code, !cached.exception_table.is_empty())
    };
    if crate::runtime::env_cache::jit_lambda_const_probe() {
        // One line per SITE, not per call: a handful for a whole run. It is
        // what tells you whether the impl you care about is even reachable by
        // this probe -- a `site_const_screened=0` means nothing without it,
        // because a body this machinery never serves cannot be screened.
        eprintln!(
            "[cratonvm-lambda-const] site impl={}.{}{} const_return={const_return:?} \
             code_len={} code={:02x?}",
            cached.class_name,
            cached.method_name,
            cached.method_descriptor,
            cached.code.len(),
            &cached.code[..cached.code.len().min(6)],
        );
    }
    SiteVerdict::Eligible(std::rc::Rc::new(LambdaJitSite {
        sam_method_name: Arc::clone(&call_site.sam_method_name),
        sam_descriptor: Arc::clone(&call_site.sam_descriptor),
        num_captures,
        capture_descs,
        total_args: impl_params.len(),
        checkcasts,
        cast_pass_cids,
        cached,
        gate,
        code: std::cell::RefCell::new(None),
        code_generation: std::cell::Cell::new(u64::MAX),
        adapter_slots: std::cell::Cell::new((0, 0)),
        direct_disabled: std::cell::Cell::new(false),
        const_return,
        receiver: None,
        adapt,
    }))
}

/// The wrapper class of a JVM primitive descriptor byte.
fn lambda_wrapper_of_prim(prim: char) -> Option<&'static str> {
    Some(match prim {
        'Z' => "java/lang/Boolean",
        'B' => "java/lang/Byte",
        'S' => "java/lang/Short",
        'C' => "java/lang/Character",
        'I' => "java/lang/Integer",
        'J' => "java/lang/Long",
        'F' => "java/lang/Float",
        'D' => "java/lang/Double",
        _ => return None,
    })
}

/// Is `tok` the field descriptor `L<class>;` of exactly `class`?
fn lambda_desc_names(tok: &str, class: &str) -> bool {
    tok.len() == class.len() + 2
        && tok.starts_with('L')
        && tok.ends_with(';')
        && &tok[1..tok.len() - 1] == class
}

/// JIT round 20 wave 12, lane qLam: the primitive whose return
/// LambdaMetafactory boxes for this `(SAM return, impl return)` pair, when the
/// direct arm may do it: the impl returns a 32-bit primitive (`I Z B S C F`:
/// a raw `i64::MIN` is then never a value) and the SAM's erased return is
/// `Object` or that exact wrapper. `None` for every other pair (widening,
/// unboxing a return, `long` / `double`, a `Number` / `Comparable` return),
/// which the general path keeps.
fn lambda_box_return_prim(sam_ret: &str, impl_ret: &str) -> Option<char> {
    let mut chars = impl_ret.chars();
    let prim = chars.next()?;
    if chars.next().is_some() || !matches!(prim, 'I' | 'Z' | 'B' | 'S' | 'C' | 'F') {
        return None;
    }
    let wrapper = lambda_wrapper_of_prim(prim)?;
    (sam_ret == "Ljava/lang/Object;" || lambda_desc_names(sam_ret, wrapper)).then_some(prim)
}

/// JIT round 20 wave 12, lane qLam: the primitive LambdaMetafactory unboxes a
/// SAM argument into for the impl parameter `impl_tok`, when the direct arm
/// may do it: a reference SAM token, a primitive impl token, and the
/// instantiated type (or, with none narrower, the SAM's own) is EXACTLY that
/// primitive's wrapper -- so the bridge's cast and the unbox are one exact
/// class test. No widening after the unbox (an `Integer` into a `long`), no
/// `Number` source: the general path keeps those.
fn lambda_unbox_param_prim(sam_tok: &str, impl_tok: &str, inst_tok: Option<&str>) -> Option<char> {
    if !is_reference_desc(sam_tok) {
        return None;
    }
    let mut chars = impl_tok.chars();
    let prim = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let wrapper = lambda_wrapper_of_prim(prim)?;
    let narrowed = inst_tok.filter(|t| is_reference_desc(t)).unwrap_or(sam_tok);
    lambda_desc_names(narrowed, wrapper).then_some(prim)
}

/// The positive control of the qLam adapter arms (`CRATONVM_DBG_JITC=1`): one
/// line per adapted site built (per thread), naming the impl and the adapter.
fn note_adapted_site(cached: &CachedBytecodeMethod, adapt: &LambdaAdapt) {
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] lambda-adapted-site impl={}.{}{} unbox={} box={:?} native={}",
            cached.class_name,
            cached.method_name,
            cached.method_descriptor,
            adapt.unbox.len(),
            adapt.box_ret,
            adapt.native.is_some(),
        );
    }
}

/// The wrapper class ids of an unbox list, `None` while one is not loaded.
fn lambda_unbox_with_wrapper_ids(
    shared: &SharedVm,
    unbox: &[(usize, char)],
) -> Option<Vec<(usize, char, ClassId)>> {
    let cm = shared.classes.class_manager.read();
    unbox
        .iter()
        .map(|&(k, prim)| {
            let wrapper = lambda_wrapper_of_prim(prim)?;
            cm.get_loaded_class_id(wrapper).map(|cid| (k, prim, cid))
        })
        .collect()
}

/// JIT round 20 wave 12, lane qLam: box an adapted site's raw 32-bit impl
/// return as LambdaMetafactory does -- through the wrapper's own `valueOf`
/// (its caches, its identity), exactly as `coerce_return` boxes on the
/// general path -- and return the reference as a JIT-ABI word. The caller
/// passes `i64::MIN` (an escaping exception) through without calling this.
pub(crate) fn lambda_box_return_bits(
    shared: &SharedVm,
    thread: &mut JvmThread,
    prim: char,
    bits: i64,
) -> Result<i64, MethodCallFailed> {
    let value = match prim {
        // Cast: JIT ABI -- the int category travels sign-extended.
        'I' | 'Z' | 'B' | 'S' | 'C' => Value::Int(bits as i32),
        // Cast: JIT ABI -- float bits zero-extended.
        'F' => Value::Float(f32::from_bits(bits as u32)),
        _ => return Ok(bits),
    };
    match box_primitive(shared, thread, prim, value)? {
        Value::Object(Some(obj)) => {
            // The handoff root every JIT fast path that returns a fresh
            // reference sets (`jit_integer_value_of_direct`).
            thread.native_pending_return = Some(obj);
            Ok(obj.as_ptr() as i64) // Cast: JIT ABI -- pointer to i64
        }
        _ => Ok(0),
    }
}

/// JIT round 20 wave 12, lane qLam: an exact wrapper's unboxed value as the
/// impl parameter's JIT-ABI word (the general path's `unbox_wrapper`).
pub(crate) fn lambda_unbox_bits(shared: &SharedVm, prim: char, wrapper: ObjectRef) -> Option<i64> {
    Some(
        match unbox_wrapper(shared, prim, Value::Object(Some(wrapper))) {
            Value::Int(x) => x as i64, // Cast: JIT ABI -- i64 register convention
            Value::Long(x) => x,
            Value::Float(x) => x.to_bits() as i64, // Cast: JIT ABI -- float bits to i64
            Value::Double(x) => x.to_bits() as i64, // Cast: JIT ABI -- double bits to i64
            _ => return None,
        },
    )
}

/// The compiled impl body for this site, or `None` while it is still
/// interpreted.
///
/// Epoch-guarded like every other JIT-cache probe in this VM: the string-keyed
/// `JitCache::get` — a hash, an `ArcSwap` load and a `memcmp`, together 5.6% of
/// a lambda-shape profile when it ran per invocation — is skipped entirely
/// while this site's snapshot of `JitCache::generation()` is still current.
pub(crate) fn lambda_jit_site_code(
    shared: &SharedVm,
    site: &LambdaJitSite,
) -> Option<cratonvm_jit::RetainedCode> {
    let generation = shared.jit.jit_cache.generation();
    if site.code_generation.get() != generation {
        let found = shared
            .jit
            .jit_cache
            .read()
            .get(
                &site.cached.class_name,
                &site.cached.method_name,
                &site.cached.method_descriptor,
                site.cached.declaring_class_id,
            )
            .map(cratonvm_jit::RetainedCode::new);
        *site.code.borrow_mut() = found;
        site.code_generation.set(generation);
        // Forget WHICH slots too: a republished impl means the thunk that
        // was installed points at the old body. This lifts the latch when the
        // IMPL is recompiled; `claim_adapter_install` comparing slot pointers
        // is what covers the CALLER being recompiled, which this never saw.
        site.adapter_slots.set((0, 0));
        // A moved generation means a publication or an invalidation — including
        // the recompile that a de-speculation drives. The body being probed now
        // is not the one that deopted, so the latch that took this site off the
        // direct arm is lifted with it. Without this the first uncommon trap in
        // a body's life would exile its call site permanently, even after the
        // speculation that failed had been compiled out.
        site.direct_disabled.set(false);
    }
    site.code.borrow().clone()
}

/// Replay the `checkcast` the synthetic bridge would have done, for the SAM
/// arguments this site recorded as needing one.
///
/// `true` means every cast passes (or there were none) and the direct call may
/// proceed. `false` means one would THROW — this arm declines and the generic
/// path raises the `ClassCastException` with the message it has always built
/// (`cce_display_class_name` and all), which is worth far more than saving a
/// dispatch on a call that is about to fail anyway.
///
/// `sam_args` are raw JIT-ABI registers, so a reference is a pointer and `0` is
/// `null` — which every `checkcast` accepts.
pub(crate) fn lambda_jit_site_checkcasts_pass(
    shared: &SharedVm,
    site: &LambdaJitSite,
    sam_args: &[i64],
) -> bool {
    for (at, (sam_idx, _)) in site.checkcasts.iter().enumerate() {
        let Some(raw) = sam_args.get(*sam_idx).copied() else {
            continue;
        };
        if raw == 0 {
            continue;
        }
        // SAFETY: a non-zero reference register is a live object pointer — the
        // same assumption every other raw-argument arm in the JIT bridge makes.
        let obj = unsafe { ObjectRef::from_raw(raw as *mut u8) };
        // The safepoint-free verdict. These registers (and the caller's
        // receiver, and the later casts' registers) are raw addresses nothing
        // remaps, so a class load here — which can reach a safepoint — would
        // leave the direct call passing stale pointers. A cast that needs a
        // load is undecided: decline, and the generic path, which pins its
        // arguments, resolves it; later calls then find the class loaded.
        if !lambda_site_cast_passes(shared, site, at, obj) {
            return false;
        }
    }
    true
}

/// nio21 w15 L5: does `obj` pass `site`'s `at`-th replayed `checkcast`
/// (`LambdaJitSite::checkcast_list()[at]`)? The safepoint-free verdict
/// (`may_load == false`): `false` for a failing cast AND for one that would
/// need a class load, exactly as `lambda_arg_cast_verdict(.., false)`'s
/// `Some(true) | None`.
///
/// A class this site already saw pass is answered from
/// `LambdaJitSite::cast_pass_cids` with one compare. The interpreted lambda
/// door (`invoke_fast`) may call it too, in place of its own verdict call.
pub(crate) fn lambda_site_cast_passes(
    shared: &SharedVm,
    site: &LambdaJitSite,
    at: usize,
    obj: ObjectRef,
) -> bool {
    let Some((_, inst_tok)) = site.checkcasts.get(at) else {
        return true;
    };
    let memo = site.cast_pass_cids.get(at);
    // Arrays are excluded: an array header's class id names its component, so
    // the id alone does not identify the argument's type.
    let memo_on = lambda_cast_pass_memo_enabled(shared) && inst_tok.starts_with('L');
    if memo_on {
        let obj_class_id = shared.mem.heap.class_id_of(obj);
        // nio21 w18 L2: an ARRAY argument's header names its COMPONENT's
        // class id, so a `String[]` matched a memo row proved for `String`
        // and passed a `checkcast String` HotSpot fails. An array never
        // answers from, or writes, the class-id memo; the verdict decides it.
        let is_array = shared.mem.heap.kind_of(obj) == cratonvm_types::ObjectKind::Array;
        if !is_array && memo.is_some_and(|m| m.get() == obj_class_id.as_u32()) {
            return true;
        }
        match lambda_arg_cast_verdict(shared, &|| obj, inst_tok, false) {
            Some(false) => {
                // Recorded only when the verdict's class-pair memo now holds
                // the pair: a pass from a per-object arm (annotation / dynamic
                // proxy instance) or a fail-open default is not a class fact,
                // and the next object must be asked again.
                let target = inst_tok
                    .strip_prefix('L')
                    .and_then(|d| d.strip_suffix(';'))
                    .unwrap_or("");
                if let Some(m) = memo.filter(|_| !is_array) {
                    if cast_pass_memo_hit(shared, obj_class_id, target) {
                        m.set(obj_class_id.as_u32());
                    }
                }
                true
            }
            Some(true) | None => false,
        }
    } else {
        matches!(
            lambda_arg_cast_verdict(shared, &|| obj, inst_tok, false),
            Some(false)
        )
    }
}

/// Read this site's captured values out of the proxy object, in
/// impl-parameter order, as raw JIT-ABI registers.
///
/// Captures occupy proxy fields `0..num_captures` — the same layout
/// `try_lambda_dispatch` reads them from, in the same order the impl declares
/// them.
pub(crate) fn lambda_jit_site_capture_args(
    shared: &SharedVm,
    site: &LambdaJitSite,
    proxy: ObjectRef,
    out: &mut [i64],
) {
    for (i, slot) in out.iter_mut().enumerate().take(site.num_captures) {
        *slot = match shared.mem.heap.get_field(proxy, i) {
            Value::Int(x) => x as i64, // Cast: JIT ABI -- i64 register convention
            Value::Long(x) => x,
            Value::Float(x) => x.to_bits() as i64, // Cast: JIT ABI -- float bits to i64
            Value::Double(x) => x.to_bits() as i64, // Cast: JIT ABI -- double bits to i64
            Value::Object(Some(obj)) => obj.as_ptr() as i64, // Cast: JIT ABI -- pointer to i64
            _ => 0,
        };
    }
}

/// Engagement census for the direct call site (`CRATONVM_DBG=lambda-jit`).
///
/// Same reason as `lambda_jit`'s counters: a flat A/B on this path cannot tell
/// "the direct call did not help" from "no direct call ever happened".
pub(crate) mod lambda_site_prof {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(crate) static SITE_CALLS: AtomicU64 = AtomicU64::new(0);
    pub(crate) static SITE_DIRECT: AtomicU64 = AtomicU64::new(0);
    pub(crate) static SITE_NO_CODE: AtomicU64 = AtomicU64::new(0);
    pub(crate) static SITE_REFUSED: AtomicU64 = AtomicU64::new(0);
    /// Refused because the body had already deoptimized under this site.
    pub(crate) static SITE_DEOPTED: AtomicU64 = AtomicU64::new(0);
    /// Refused because the argument shape did not match the site's.
    pub(crate) static SITE_ARITY: AtomicU64 = AtomicU64::new(0);
    /// Deopted bodies whose reconstructed frame was RESUMED and run to
    /// completion by the direct arm, so the body's already-committed side
    /// effects were not repeated.
    ///
    /// NOT gated on the census switch, unlike its neighbours: this counts a
    /// correctness event, and its sibling below counts a correctness RESIDUAL.
    /// A number that only exists when a debug variable was set cannot be used
    /// to answer "did any call re-execute" after the fact.
    pub(crate) static SITE_RESUMED: AtomicU64 = AtomicU64::new(0);
    /// Deopted bodies the direct arm could NOT resume, so the generic path
    /// re-ran them from entry. Every one of these re-executes whatever the
    /// compiled body committed before it trapped. Expected to be 0; a non-zero
    /// value is the remaining exposure of the defect
    /// `resume_deopted_body` was wired in for.
    pub(crate) static SITE_UNRESUMABLE: AtomicU64 = AtomicU64::new(0);
    /// Inline-cache thunks installed. Counts SITES, not calls — every dispatch
    /// after one of these lands never reaches Rust at all, which is precisely
    /// why the per-call counters go quiet when the feature is working and this
    /// one is the only evidence left.
    pub(crate) static SITE_ADAPTERS: AtomicU64 = AtomicU64::new(0);

    /// Of those, the ones whose lambda CAPTURES — the thunks that read the
    /// proxy's body rather than only shuffling registers.
    ///
    /// Counted apart from [`SITE_ADAPTERS`] because a suite full of
    /// non-capturing lambdas keeps that total healthy while every capturing
    /// site quietly falls back to Rust, and "the thunk installed" would then be
    /// true of a feature that never engaged for the shape under test.
    pub(crate) static SITE_CAPTURE_ADAPTERS: AtomicU64 = AtomicU64::new(0);

    #[inline]
    pub(crate) fn bump(counter: &AtomicU64) {
        if super::lambda_jit::on() {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// How often to print, counted in direct calls. Small enough that a probe
    /// run of a few hundred thousand dispatches reports several times.
    pub(crate) const REPORT_EVERY: u64 = 100_000;

    #[inline]
    pub(crate) fn maybe_report() {
        if !super::lambda_jit::on() {
            return;
        }
        let direct = SITE_DIRECT.load(Ordering::Relaxed);
        if direct > 0 && direct % REPORT_EVERY == 0 {
            super::lambda_jit::report();
        }
    }

    pub(crate) fn line() -> String {
        let konst = super::lambda_const_probe_counts();
        format!(
            "site_calls={} site_direct={} site_no_code={} site_refused={} site_deopted={} \
             site_resumed={} site_unresumable={} \
             site_arity={} site_adapters={} site_cap_adapters={} site_shape_collisions={} \
             site_const_screened={} site_const_mismatch={} site_const_dirty_high={} \
             site_const_opaque={}",
            SITE_CALLS.load(Ordering::Relaxed),
            SITE_DIRECT.load(Ordering::Relaxed),
            SITE_NO_CODE.load(Ordering::Relaxed),
            SITE_REFUSED.load(Ordering::Relaxed),
            SITE_DEOPTED.load(Ordering::Relaxed),
            SITE_RESUMED.load(Ordering::Relaxed),
            SITE_UNRESUMABLE.load(Ordering::Relaxed),
            SITE_ARITY.load(Ordering::Relaxed),
            SITE_ADAPTERS.load(Ordering::Relaxed),
            // Beside the total, never instead of it: a run full of
            // non-capturing lambdas keeps `site_adapters` healthy while every
            // capturing site falls back to Rust, and a probe reading only the
            // total cannot tell those apart.
            SITE_CAPTURE_ADAPTERS.load(Ordering::Relaxed),
            // Nonzero means the thunk cache was asked to reuse a thunk across
            // two different emitted SHAPES and refused. Reported here so the
            // fix cannot go inert unnoticed: on a workload without the hazard
            // it reads 0, and 0 is the honest answer rather than a missing one.
            cratonvm_jit::lambda_adapter::lambda_adapter_shape_collisions(),
            // The constant-return probe. `screened` is its engagement counter:
            // a `mismatch=0` beside a `screened=0` says the probe never ran,
            // not that the answers were right. `opaque` counts the sites whose
            // calls it structurally cannot see -- see `const_probe_note_opaque`.
            konst.0,
            konst.1,
            konst.2,
            konst.3,
        )
    }
}

/// The lambda tier-up engagement counters, for tests that must prove they
/// EXERCISE the fast path rather than merely agreeing with HotSpot while it
/// never ran.
///
/// Returns `(interpreter fast returns, JIT-side direct calls, tier-up
/// nominations)`. Counting is gated on `CRATONVM_DBG_LAMBDA_JIT` — the same
/// switch the `[LAMBDA-JIT]` line rides on — so an ordinary run pays one
/// relaxed load per dispatch and nothing else. A caller that wants numbers
/// must set that variable BEFORE the first lambda dispatch, because the gate
/// is read once into a `OnceLock`.
pub fn lambda_jit_engagement() -> (u64, u64, u64) {
    use std::sync::atomic::Ordering;
    (
        lambda_jit::FAST_RETURNS.load(Ordering::Relaxed),
        lambda_site_prof::SITE_DIRECT.load(Ordering::Relaxed),
        lambda_jit::NOMINATIONS.load(Ordering::Relaxed),
    )
}

/// Census shims for the JIT-side direct call arm (`crate::jit::helpers`), which
/// lives outside this module and so cannot touch the counters directly.
#[inline]
pub(crate) fn lambda_site_bump_calls() {
    lambda_site_prof::bump(&lambda_site_prof::SITE_CALLS);
}
#[inline]
pub(crate) fn lambda_site_bump_direct() {
    lambda_site_prof::bump(&lambda_site_prof::SITE_DIRECT);
    // The census has to be driven from HERE as well. Once the direct arm is
    // serving a workload, `try_invoke_cached_lambda_impl` — where the other
    // half of these counters is reported from — is barely reached at all, so a
    // report keyed only to that path prints nothing on exactly the runs where
    // the fast path is working. A census that goes quiet when the thing it
    // counts starts working is not a census.
    lambda_site_prof::maybe_report();
}
#[inline]
pub(crate) fn lambda_site_bump_no_code() {
    lambda_site_prof::bump(&lambda_site_prof::SITE_NO_CODE);
}
#[inline]
pub(crate) fn lambda_site_bump_refused() {
    lambda_site_prof::bump(&lambda_site_prof::SITE_REFUSED);
}
#[inline]
pub(crate) fn lambda_site_bump_deopted() {
    lambda_site_prof::bump(&lambda_site_prof::SITE_DEOPTED);
}
/// A deopted body whose reconstructed frame the direct arm RESUMED. Ungated —
/// see [`lambda_site_prof::SITE_RESUMED`].
#[inline]
pub(crate) fn lambda_site_bump_resumed() {
    lambda_site_prof::SITE_RESUMED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}
/// A deopted body the direct arm could NOT resume, so the generic path re-ran
/// it from entry. Ungated — see [`lambda_site_prof::SITE_UNRESUMABLE`].
#[inline]
pub(crate) fn lambda_site_bump_unresumable() {
    lambda_site_prof::SITE_UNRESUMABLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// `(resumed, unresumable)` — the deopt outcome split for a lambda body.
///
/// `unresumable` is the number of SAM calls whose compiled body trapped and was
/// then re-executed from entry by the generic path, side effects and all. It is
/// the metric a regression test asserts is zero.
///
/// **`resumed` counts two doors, not one.** The direct arm
/// (`try_lambda_site_direct_call`) is the original, and since 2026-09-08 the
/// compiled caller's dispatch helper (`try_resume_trapped_callee`) bumps it too
/// when it claims a lambda body's frame through the metafactory's
/// `impl_handle`. Both answer the same question — was a trapped lambda body's
/// frame SPENT rather than dropped — and a split that reported only one of them
/// would read as zero engagement on a run where the other door did all the
/// work, which is precisely the shape
/// `lambda-callee-deopt-is-orphaned-by-the-sam-name-check-20260908` had.
///
/// **`pub` for `vm/tests/`, and it has to be.** Its only readers are
/// integration tests, which live in a separate crate: they see neither
/// `pub(crate)` nor `#[cfg(test)]`. That makes this a permanent entry on the
/// test-only-public-API ratchet, carried deliberately -- see the
/// "third disposition" note in `vm/tests/no_test_only_public_api.rs` before
/// trying to gate or delete it.
pub fn lambda_site_deopt_outcomes() -> (u64, u64) {
    use std::sync::atomic::Ordering;
    (
        lambda_site_prof::SITE_RESUMED.load(Ordering::Relaxed),
        lambda_site_prof::SITE_UNRESUMABLE.load(Ordering::Relaxed),
    )
}
/// NOT gated on the census switch. An installed thunk is a lasting change to a
/// call site, not a per-call event, so one relaxed increment per SITE is free
/// and the number is what `lambda_jit_adapter_installs` reports to the tests
/// that must prove the inline cache really took over.
#[inline]
pub(crate) fn lambda_site_bump_adapter(captures: usize) {
    lambda_site_prof::SITE_ADAPTERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if captures > 0 {
        lambda_site_prof::SITE_CAPTURE_ADAPTERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// How many SAM call sites have had an inline-cache thunk installed.
pub fn lambda_jit_adapter_installs() -> u64 {
    lambda_site_prof::SITE_ADAPTERS.load(std::sync::atomic::Ordering::Relaxed)
}

/// How many of those sites belong to a CAPTURING lambda — the thunks that read
/// captured values out of the proxy's body.
///
/// Separate from [`lambda_jit_adapter_installs`] on purpose: a workload with any
/// non-capturing lambda in it keeps that total above zero whatever happens to
/// the capturing ones, so it cannot answer "did the capture path engage".
pub fn lambda_jit_capture_adapter_installs() -> u64 {
    lambda_site_prof::SITE_CAPTURE_ADAPTERS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Print the lambda census ONCE at process exit, under the same
/// `CRATONVM_DBG=lambda-jit` switch. Called from the VM's shutdown path.
///
/// # Why the periodic report is not enough, measured
///
/// `lambda_jit::maybe_report` fires every 200 000 *eligible* dispatches and
/// `lambda_site_prof::maybe_report` every 100 000 *direct* calls. Both
/// thresholds were chosen for a microbenchmark that does millions of one shape,
/// and neither is reachable by an ordinary application workload: a census over
/// 24 Tomcat JUnit classes — 129 s of real work, one of them running 21 HTTP
/// tests against a live connector — printed **nothing at all**, and the obvious
/// reading of that ("no lambda activity") is not one the instrument can
/// support. It could equally have been 199 999 eligible dispatches and fifty
/// installed thunks.
///
/// An instrument that is silent below a threshold no real workload crosses
/// cannot answer "does this feature reach anything", which is the only question
/// worth asking of a fast path. So the totals are printed once, unconditionally,
/// at the end of every gated run.
///
/// Prints even when every counter is zero. That is deliberate: a zero line is
/// the answer "nothing reached this path", and it is only worth anything if it
/// can be told apart from the switch having been off — which the absence of a
/// line cannot.
pub fn report_lambda_census_at_exit() {
    if !lambda_jit::on() {
        return;
    }
    eprint!("[LAMBDA-JIT] EXIT ");
    lambda_jit::report();
}

#[inline]
pub(crate) fn lambda_site_bump_arity() {
    lambda_site_prof::bump(&lambda_site_prof::SITE_ARITY);
}

/// Build (or re-derive) the `CachedBytecodeMethod` for a lambda implementation
/// method, with the redefinition gate that decides when it goes stale.
///
/// Extracted so the consumers agree by construction: the interpreted dispatch
/// cache below, the compiled call site's own direct-call target
/// (`build_lambda_jit_site`), and — since 2026-08-22 —
/// `jit::helpers::try_jit_static_bytecode_callee`, which needs exactly this
/// question answered for an ordinary `invokestatic` callee the JIT did not
/// compile. All must refuse the same shapes — a native shadow, a
/// `synchronized` or abstract body, a class that cannot be resolved — and a
/// second, hand-copied version of these checks is exactly how one of them would
/// come to admit a body another refuses.
///
/// Nothing here is lambda-specific: it takes a class id and a (name,
/// descriptor) and returns the interpreter frame template for whatever
/// `find_method_recursive` lands on, or `None`.
pub(crate) fn build_lambda_impl_cached(
    shared: &SharedVm,
    receiver_class_id: ClassId,
    method_name: &str,
    descriptor: &str,
) -> Option<(Arc<CachedBytecodeMethod>, RedefineGate)> {
    build_bytecode_callee_cached(shared, receiver_class_id, method_name, descriptor, false)
}

/// [`build_lambda_impl_cached`], optionally admitting an `ACC_SYNCHRONIZED`
/// INSTANCE body.
///
/// `admit_synchronized` is for a consumer that owns the implicit monitor
/// itself — acquires it before installing the frame and records it in
/// `Frame::monitor_on_exit`, which every frame removal releases. The template
/// then carries `is_synchronized: true` so that consumer can tell. Every
/// consumer that does NOT pass it keeps the refusal byte-for-byte: its frame
/// builder does no monitor work, and admitting the body there would run it
/// unlocked. A static synchronized body is admitted too: its monitor is the
/// class mirror, which `install_and_run_cached_frame_monitored` takes only when
/// it already exists, and declines otherwise.
pub(crate) fn build_bytecode_callee_cached(
    shared: &SharedVm,
    receiver_class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    admit_synchronized: bool,
) -> Option<(Arc<CachedBytecodeMethod>, RedefineGate)> {
    build_bytecode_callee_cached_inner(
        shared,
        receiver_class_id,
        method_name,
        descriptor,
        admit_synchronized,
        false,
    )
}

/// JIT round 20 wave 12, lane qLam: [`build_lambda_impl_cached`]'s template
/// WITHOUT the native-shadow refusal, for `lambda_native_instance_target`,
/// which has already decided (through `resolve_dispatch`) that the registered
/// `Intrinsic` answers the call and keeps the template only as the site's
/// identity and redefinition gate. Never run as a frame -- except by
/// `jit::helpers::resolve_virtual_bytecode_callee` under
/// `CRATONVM_USER_LOADER_RESOURCE_BYTECODE_COMPILED` (interpreter round i6
/// wave 132, lane L5b), for the one `java/lang/ClassLoader` resource body the
/// `Bridge` native would itself re-enter, after its own check that no other
/// native sits between the receiver and that body.
pub(crate) fn build_bytecode_callee_cached_admitting_native(
    shared: &SharedVm,
    receiver_class_id: ClassId,
    method_name: &str,
    descriptor: &str,
) -> Option<(Arc<CachedBytecodeMethod>, RedefineGate)> {
    build_bytecode_callee_cached_inner(
        shared,
        receiver_class_id,
        method_name,
        descriptor,
        false,
        true,
    )
}

fn build_bytecode_callee_cached_inner(
    shared: &SharedVm,
    receiver_class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    admit_synchronized: bool,
    admit_native_shadow: bool,
) -> Option<(Arc<CachedBytecodeMethod>, RedefineGate)> {
    let cm = shared.classes.class_manager.read();
    let store = cm.class_store();
    let Some((method, declaring_id)) = crate::classloading::find_method_recursive(
        receiver_class_id,
        method_name,
        descriptor,
        store,
    ) else {
        return None;
    };
    let Some(class) = store.get(declaring_id) else {
        return None;
    };
    // Cached bytecode bypasses native dispatch, which must retain precedence.
    //
    // PROBE THE RECEIVER'S CLASS, NOT ONLY THE DECLARING ONE. This is the
    // difference between the two doors a bound method reference and a lambda
    // body take, and it is the whole of the defect that
    // `a-bound-method-reference-is-a-different-dispatch-door-20260828.md`
    // (retired to docs internal on 2026-08-30, so the prefix is dropped)
    // records:
    //
    //   * an ordinary `it.remove()` is an `invokeinterface`, and
    //     `dispatch_virtual.rs` probes the registry with the RECEIVER's runtime
    //     class -- `java/util/HashMap$KeyIterator`, which is exactly the name
    //     `MAP_KEY_ITR_CARRIERS` registers the native under;
    //   * `it::remove` arrives here, and `find_method_recursive` resolves to
    //     where `remove()` is DECLARED -- `java/util/HashMap$HashIterator` --
    //     for which nothing is registered. The single declaring-class probe
    //     missed, the real `HashIterator.remove()` bytecode ran against an
    //     instance this VM minted, and it raised `IllegalStateException` off a
    //     `lastReturned` field nothing had written.
    //
    // MEASURED (`probes/MethodRefDoorProbe`, HotSpot 25.0.4+7, identical in
    // both modes): `it::remove` threw ISE for `HashSet`, `HashMap.keySet()` and
    // `Properties.keySet()` where `it.remove()` worked; `ArrayList` and
    // `Hashtable` passed for opposite reasons, which is what made the pair a
    // discriminator -- `ArrayList$Itr` DECLARES its own `remove()` so the two
    // classes coincide, and `Hashtable`'s enumerator is java.base's own.
    //
    // Declining is the fix rather than dispatching the native here: the caller
    // falls back to `invoke_on_class_shared`, which already owns native
    // precedence, virtual retargeting, monitors and the exception rules. This
    // can only ever move a call from the fast cached path to the ordinary one,
    // never the other way, and it runs once per (proxy, receiver) cache BUILD
    // rather than per call.
    let native_shadows_receiver = {
        let mut probe = Some(receiver_class_id);
        let mut hit = false;
        while let Some(cid) = probe {
            let Some(c) = store.get(cid) else { break };
            if shared
                .natives
                .native_methods
                .find(&c.name, method_name, descriptor)
                .is_some()
            {
                hit = true;
                break;
            }
            if cid == declaring_id {
                break;
            }
            probe = c.superclass;
        }
        hit
    };
    // The declaring class is probed separately because it is reachable as an
    // INTERFACE (a default method found by `find_method_recursive`'s phase 2),
    // which the superclass walk above never visits.
    let native_on_declaring = shared
        .natives
        .native_methods
        .find(&class.name, method_name, descriptor)
        .is_some();
    if (native_shadows_receiver || native_on_declaring) && !admit_native_shadow {
        return None;
    }
    // `synchronized` needs the monitor enter/exit this frame builder does
    // not do, and `native` has no bytecode to cache. `static` used to be
    // refused here too, which excluded the single most common lambda
    // shape in Java: javac compiles a NON-capturing lambda body to a
    // private *static* synthetic method, so every `() -> ...` that
    // captures nothing missed this fast path and took the generic
    // by-name invoke on every single call. Statics are cacheable — the
    // frame builder is receiver-agnostic (`init_locals_pooled` copies
    // `args` into locals from slot 0, which is already how both shapes
    // arrive) — provided the class is initialised, which the caller
    // guarantees by only reaching here after a full dispatch has run.
    let is_synchronized = method.is_synchronized();
    if (is_synchronized && !admit_synchronized) || method.is_native() {
        return None;
    }
    let Some(code_attr) = method.code() else {
        return None;
    };
    let is_static = method.is_static();
    let c = Arc::new(
        CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: declaring_id,
            class_name: Arc::clone(&class.name),
            method_name: Arc::from(method_name),
            method_descriptor: Arc::from(descriptor),
            source_file: class.source_file.as_deref().map(Arc::from),
            // The per-method memo: one padded body per method, not per site.
            code: crate::runtime::frame::padded_bytecode_for_method(
                declaring_id,
                method_name,
                descriptor,
                &code_attr.code,
                &code_attr.exception_table,
            ),
            exception_table: Arc::from(code_attr.exception_table.as_slice()),
            max_stack: code_attr.max_stack,
            max_locals: code_attr.max_locals,
            num_params: count_method_params(descriptor) as u16,
            is_synchronized,
            is_static,
        })
        // The pool generation the code indexes, read under the guard the code was
        // copied under (interpreter round i1 wave 43, lane L2;
        // `CachedBytecodeMethod::pool_generation`).
        .with_pool_generation(cratonvm_classloading::class_redefinition_count()),
    );
    let gate = RedefineGate::snapshot(cm.class_redefine_generation_handle(declaring_id));
    Some((c, gate))
}

/// Give a callee entered through a cached interpreter frame template the
/// tier-up nomination the ordinary dispatch path would have given it — and say
/// when the caller must NOT take that template at all, because a compiled body
/// already exists.
///
/// # Why this has to exist separately from the frame builder
///
/// `Frame::new_pooled_cached` + `execute_prebuilt_frame` runs a method without
/// touching `profile_store.increment_invocation` or `jit.jit_cache`, so a
/// callee reached ONLY that way is never nominated and stays interpreted for
/// the life of the process. That is the defect the LAMBDA-JIT-TIERUP block in
/// [`try_invoke_cached_lambda_impl`] answers inline for lambda SAM bodies —
/// read its comment for the measurement, which is the same one that applies
/// here. `jit::helpers`' three dispatch templates
/// (`try_jit_static_bytecode_callee`, `try_jit_virtual_bytecode_callee`,
/// `try_jit_special_bytecode_callee`) build the same frame from the same
/// `CachedBytecodeMethod` and inherited the same hole, so the block is
/// factored out here rather than written a fourth time.
///
/// # Why it also answers "is it already compiled?"
///
/// MEASURED 2026-08-23 on `probes/ExchangeProbe.java`: the instance template,
/// wired in WITHOUT this, cost **15%** across three interleaved rounds
/// (36.7 / 36.6 / 39.9 ms/op against 31.6 / 30.9 / 35.8). Both halves of that
/// loss are here. A template that intercepts a callee the JIT HAS compiled
/// does not merely fail to help — it runs the interpreter instead of the
/// compiled body, which is a straight loss; and on the MIC path that is the
/// COMMON case, because the compile probe two arms above has often just
/// published one. Answering `true` there sends the caller back to
/// `invoke_or_native`, which knows how to enter compiled code.
///
/// The probe is epoch-guarded exactly like its three twins: while this entry's
/// snapshot of `JitCache::generation()` is current, nothing has been published
/// or invalidated since the last miss, so the string-keyed `JitCache::get` is
/// skipped. The invocation is still COUNTED in that case, or the method could
/// never reach the threshold that makes re-probing worthwhile.
/// `CRATONVM_JIT_BC_CALLEE_STAND_DOWN_MEMO` (JIT round 19 wave 7, lane
/// handler7, SYNCDOOR4-3 of `jit-r19-syncdoor4-proposals.md`; opt-in, default
/// OFF): may [`bytecode_callee_compiled_or_nominate`] answer "compiled, stand
/// down" from its per-method memo word while the JIT cache's generation has
/// not moved since the probe that found the body? Without it every call of an
/// unbound special / static / virtual site whose callee IS compiled re-runs
/// the string-keyed `JitCache::get` under the cache's read lock before the
/// door stands down (the probe memo recorded only misses). The memo lives in
/// the method's own `CachedBytecodeMethod` (per VM), not in a static. Read
/// once per generation per method, on the probe's hit arm only.
fn bc_callee_stand_down_memo_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_BC_CALLEE_STAND_DOWN_MEMO")
}

pub(crate) fn bytecode_callee_compiled_or_nominate(
    shared: &SharedVm,
    thread: &JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
) -> bool {
    if crate::runtime::env_cache::disable_jit() {
        return false;
    }
    let jit_generation = shared.jit.jit_cache.generation();
    // JIT round 19 wave 7 (lane handler7, SYNCDOOR4-3): a hit recorded under
    // `CRATONVM_JIT_BC_CALLEE_STAND_DOWN_MEMO` at this generation stands down
    // on the memo alone; the string-keyed probe below found the same body and
    // nothing has been published or invalidated since. Never set with the
    // switch off (`record_jit_probe_found` records it without the stand-down
    // tag), so off this is one compare that never matches.
    use cratonvm_jit_api::{JIT_PROBE_FOUND_TAG, JIT_PROBE_STAND_DOWN_TAG};
    let probe_word = cached.jit_probe_word();
    if probe_word == jit_generation | JIT_PROBE_FOUND_TAG | JIT_PROBE_STAND_DOWN_TAG {
        return true;
    }
    if !cached.jit_probe_is_current(jit_generation) {
        let found = shared.jit.jit_cache.read().get(
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            cached.declaring_class_id,
        );
        match found {
            // A wrapped-entry body (`requires_wrapped_entry`, i.e. an
            // `ACC_SYNCHRONIZED` method the background tier published for
            // `execute_jit_call`'s monitor wrapper) is NOT one the caller's
            // by-name tail would enter: every raw door refuses it, and
            // `invoke_or_native` interprets the method. Standing down for it
            // traded this template for the same interpreter behind a by-name
            // resolution. Only the virtual and static doors admit a
            // synchronized template (`build_bytecode_callee_cached(..,
            // admit_synchronized: true)`; the special door too under
            // `CRATONVM_JIT_SPECIAL_BC_SYNC`, JIT round 16 wave 8), and each
            // serves it through `install_and_run_cached_frame_monitored`, which
            // ENTERS the wrapped body under the door's hold (static since round
            // 11 wave 6, instance since round 12 wave 8) when
            // `jit_bridge::door_locked_wrapped_body` admits it, and interprets
            // the template otherwise. The miss is recorded so the
            // string-keyed probe is not repeated until the cache changes.
            // MEASURED 2026-09-23: `Throwable.fillInStackTrace()` from an
            // OSR-compiled `new AIOOBE(msg)` loop -- 397 000 of 397 000
            // `out_virtual_bc_refused` before, all of them this arm.
            Some(compiled) if compiled.requires_wrapped_entry => {
                cached.record_jit_probe_miss(jit_generation);
                return false;
            }
            Some(_) => {
                // SYNCDOOR4-3: the switch is read once per generation (the
                // first hit records it either way), so the off arm pays the
                // same probe as before and no per-call flag read.
                if probe_word != jit_generation | JIT_PROBE_FOUND_TAG {
                    cached.record_jit_probe_found(
                        jit_generation,
                        bc_callee_stand_down_memo_enabled(),
                    );
                }
                return true;
            }
            None => cached.record_jit_probe_miss(jit_generation),
        }
    }
    // A current probe memo cannot tell "no body" from "a wrapped body", so a
    // `static synchronized` callee whose wrapped body the monitored door
    // ENTERS was counted here on every call, although that body's entry
    // counter already carries its calls to the tiered manager (round 11
    // wave 7, proposal 23). Asked of the door's per-thread memo only, and only
    // for a body the door will actually enter; anything else is counted.
    if cached.is_synchronized
        && super::jit_bridge::door_will_enter_memoized_wrapped_body(shared, thread, cached)
    {
        return false;
    }
    // A virtual thread is excluded from NOMINATION for the same reason the
    // lambda twin excludes it from compiled ENTRY: compiled code carries none
    // of the unmount points the interpreter path does.
    if matches!(thread.kind, crate::threading::ThreadKind::Virtual) {
        return false;
    }
    let threshold = crate::runtime::env_cache::jit_invocation_threshold();
    let cnt = shared
        .jit
        .profile_store
        .increment_invocation(cached.invoc_key());
    let should_attempt = should_offer_tier_up(cnt, threshold);
    if !should_attempt {
        return false;
    }
    if !crate::runtime::env_cache::bg_compile() {
        // `CRATONVM_BG_COMPILE=0` restores INLINE compilation on the mutator,
        // and every other nomination site honours it — without this arm the
        // off-switch would silently stop these callees compiling at all rather
        // than change how they compile.
        let gate = RedefineGate::snapshot(
            shared
                .classes
                .class_manager
                .read()
                .class_redefine_generation_handle(cached.declaring_class_id),
        );
        let _ = try_jit_upgrade_with_gate(shared, cached, gate);
    } else {
        ensure_bg_compiler_started(shared);
        // The REAL invocation count, not the stride boundary — see the
        // invokestatic twin, where stride-boundary `+= 1` counting deflated the
        // manager's hotness view 64x.
        let recommended_tier = offer_invocation_to_tiered_manager(shared, &*cached, cnt as u64);
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] bc-callee-tiered-enqueue {}.{}{} tier={recommended_tier:?} invoc_count={cnt}",
                cached.class_name, cached.method_name, cached.method_descriptor,
            );
        }
    }
    false
}

thread_local! {
    /// nio21 w15 L5: `(vm, proxy, receiver class)` pairs whose
    /// [`build_lambda_impl_cached`] template was REFUSED, with the
    /// `class_redefinition_count()` read before that build.
    ///
    /// A refusal used to be rebuilt on every call: a `String::trim` method
    /// reference (whose receiver class has a registered native, so the
    /// template declines for native precedence) paid a `class_manager` read, a
    /// `find_method_recursive` walk and two native-registry probes per element
    /// before falling back to `invoke_on_class_shared` anyway. The refusal is
    /// a property of the pair's classes and the native registry; remembering
    /// it can only keep a call on the general path, which is the
    /// always-correct one (`build_bytecode_callee_cached`: "This can only ever
    /// move a call from the fast cached path to the ordinary one"). A
    /// redefinition anywhere re-asks.
    static LAMBDA_IMPL_REFUSED: std::cell::RefCell<
        rustc_hash::FxHashMap<VmScopedClassPairKey, u64>
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// `CRATONVM_LAMBDA_IMPL_REFUSAL_MEMO` (default ON; `0|false|off|no` is the
/// kill switch): see [`LAMBDA_IMPL_REFUSED`]. Read once per VM
/// ([`LambdaSwitches`], gci i14/sx).
#[inline]
fn lambda_impl_refusal_memo_enabled(shared: &SharedVm) -> bool {
    shared.lambda_switches.impl_refusal_memo
}

/// `CRATONVM_LAMBDA_IMPL_CODE_MEMO` (nio21 wave 16 L2; default ON,
/// `0|false|off|no` is the kill switch): [`try_invoke_cached_lambda_impl`]
/// answers a compiled impl from its row's generation-stamped body memo
/// ([`LambdaImplRow`]) instead of a string-keyed `JitCache::get` per call.
/// Read once per VM ([`LambdaSwitches`], gci i14/sx).
#[inline]
fn lambda_impl_code_memo_enabled(shared: &SharedVm) -> bool {
    shared.lambda_switches.impl_code_memo
}

/// `CRATONVM_LAMBDA_HANDLER_IMPL_ONESHOT` (nio21 wave 16 L2; default ON,
/// `0|false|off|no` restores the bar): a lambda impl that declares an
/// exception table may be entered compiled through
/// [`execute_jit_call_oneshot`], which routes a throw through the impl's own
/// table. See the gate in [`try_invoke_cached_lambda_impl`]. Also gates the
/// interpreted lambda door's compiled entry for such an impl
/// (`invoke_fast::execute_lambda_door`), which uses the loop-integrated
/// `execute_jit_call_decoded` and the same routing. Read once per VM
/// ([`LambdaSwitches`], gci i14/sx).
#[inline]
pub(crate) fn lambda_handler_impl_oneshot_enabled(shared: &SharedVm) -> bool {
    shared.lambda_switches.handler_impl_oneshot
}

/// Stamp `key`'s row with the body a probe at `generation` found (see
/// [`LambdaImplRow`]). `try_borrow_mut`: a row borrowed right now is in use
/// by an outer dispatch on this thread, and the memo is only an optimisation.
fn note_lambda_impl_code(
    key: VmScopedClassPairKey,
    generation: u64,
    body: &Arc<crate::jit::CompiledMethod>,
) {
    LAMBDA_IMPL_BYTECODE_CACHE.with(|cache| {
        if let Ok(mut cache) = cache.try_borrow_mut() {
            if let Some(row) = cache.get_mut(&key) {
                row.code_generation = generation;
                row.code = Arc::downgrade(body);
            }
        }
    });
}

pub(super) fn try_invoke_cached_lambda_impl(
    shared: &SharedVm,
    thread: &mut JvmThread,
    proxy_class_id: ClassId,
    receiver_class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Result<Option<Option<Value>>, MethodCallFailed> {
    let key = (
        shared.vm_identity,
        proxy_class_id.as_u32(),
        receiver_class_id.as_u32(),
    );
    // nio21 w16 L2: read ONCE, before the row's body memo is consulted and
    // before any probe below, so a memo stamped with it can only be older than
    // a racing publication, never newer (see `LambdaImplRow`).
    let jit_generation = shared.jit.jit_cache.generation();
    let code_memo_on = lambda_impl_code_memo_enabled(shared);
    // Wrapped in `RetainedCode` the moment it is upgraded: a bare `Arc` that
    // turned out to be a withdrawn body's last owner would unmap it outside
    // the retirement queue when dropped
    // (`jit-code-uaf-outside-retirement-queue-FIXED-20260918.md`).
    let mut memo_code: Option<cratonvm_jit::RetainedCode> = None;
    let cached = LAMBDA_IMPL_BYTECODE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        match cache.get(&key) {
            Some(row) if !row.gate.is_stale() => {
                if code_memo_on && row.code_generation == jit_generation {
                    memo_code = row.code.upgrade().map(cratonvm_jit::RetainedCode::new);
                }
                Some(Arc::clone(&row.cached))
            }
            Some(_) => {
                cache.remove(&key);
                None
            }
            None => None,
        }
    });
    let cached = match cached {
        Some(c) if &*c.method_name == method_name && &*c.method_descriptor == descriptor => c,
        Some(_) => return Ok(None),
        None => {
            // nio21 w15 L5: a pair whose template was already refused at the
            // current redefinition count is not rebuilt (see
            // `LAMBDA_IMPL_REFUSED`).
            let redefinitions = cratonvm_classloading::class_redefinition_count();
            if lambda_impl_refusal_memo_enabled(shared)
                && LAMBDA_IMPL_REFUSED.with(|refused| {
                    refused
                        .try_borrow()
                        .is_ok_and(|r| r.get(&key) == Some(&redefinitions))
                })
            {
                return Ok(None);
            }
            let Some((c, gate)) =
                build_lambda_impl_cached(shared, receiver_class_id, method_name, descriptor)
            else {
                if lambda_impl_refusal_memo_enabled(shared) {
                    LAMBDA_IMPL_REFUSED.with(|refused| {
                        if let Ok(mut r) = refused.try_borrow_mut() {
                            r.insert(key, redefinitions);
                        }
                    });
                }
                return Ok(None);
            };
            LAMBDA_IMPL_BYTECODE_CACHE.with(|cache| {
                cache.borrow_mut().insert(
                    key,
                    LambdaImplRow {
                        cached: Arc::clone(&c),
                        gate,
                        code_generation: u64::MAX,
                        code: std::sync::Weak::new(),
                    },
                );
            });
            c
        }
    };
    // An instance impl takes the receiver in `args[0]`; a static one does not.
    let expected_args = cached.num_params as usize + usize::from(!cached.is_static);
    if args.len() != expected_args {
        return Ok(None);
    }
    // LAMBDA-JIT-TIERUP — a lambda SAM implementation reached through lambda
    // dispatch used to touch NEITHER `profile_store.increment_invocation` NOR
    // `jit.jit_cache`, so it could never be nominated for JIT compilation, no
    // matter how many times it was called. Confirmed with `CRATONVM_DBG_JITC=1`
    // against `probes/SamDispatchDecompositionProbe.java`: a lambda's synthetic
    // `lambda$...` method never once appeared in the tiered-enqueue/bg-compile
    // log, while the byte-identical body reached through a named or anonymous
    // class (ordinary `invokeinterface`, which DOES count invocations at its
    // cache site in `dispatch_virtual.rs`) compiled within a few hundred calls
    // and ran ~40x faster. See
    // known-issues/perf/lambda-sam-dispatch-bypasses-the-cached-invoke-path-20260817.md.
    //
    // The shape mirrors the twins (`execute_invokestatic_cached`,
    // `dispatch_virtual.rs`'s poly-cache arm): probe the JIT cache first,
    // epoch-guarded, and only count invocations while the probe is still
    // missing. It differs from them in the primitive it enters compiled code
    // with — `execute_jit_call_oneshot` rather than `execute_jit_call_decoded`
    // — because this function is a one-shot subroutine that must return a
    // `Value`, not a step of the interpreter's dispatch loop that can be handed
    // a pushed frame. Reusing the loop-integrated primitive here is what
    // crashed the first attempt (section 5.3 of that page); see
    // `execute_jit_call_oneshot`'s own doc comment.
    //
    // Gates, in order of cost. `exception_table.is_empty()` is the same
    // restriction every other direct-compiled-call site in this VM applies
    // (`mic_callee_has_exception_table`, `osr_callee_bars_direct_call`, the
    // poly-cache arm's own `cached.exception_table.is_empty()`): a
    // handler-bearing callee is never entered by a direct compiled call. A
    // virtual thread is excluded because compiled entry carries none of the
    // unmount points the interpreter path does, matching the TDigest fast path
    // this block replaces.
    //
    // nio21 wave 16 L2 (`CRATONVM_LAMBDA_HANDLER_IMPL_ONESHOT`, default ON):
    // the exception-table bar is lifted for THIS door. It is not a "direct"
    // call in the sense of the cascade bars it was copied from: those `CALL`
    // the raw entry from machine code and have no Rust frame at which to route
    // a throw, whereas `execute_jit_call_oneshot` drains the body's signals and
    // routes a throw through the IMPL's own table (`oneshot_route_exception` ->
    // `route_jit_signal_exception_noting_ir_catch` with `cached` the callee),
    // running the handler frame it materialises to completion. That is the
    // routing `execute_invokestatic_cached` has always used for a
    // handler-bearing static callee, with no table bar at all -- and a lambda
    // impl is a static (or receiver-first) callee. With the bar, every
    // `try { .. } finally { lock.unlock(); }` lambda body (and every method
    // reference to such a method: `Nio20JitRows::rlockOp`) ran in a NESTED
    // interpreter on every call, compiled or not: the 11%
    // `execute_frame_from_index` of the w15 `rlock` / `lock` profiles.
    // r9w2-vm2: the one-shot compiled entry pins the reference arguments for
    // the whole compiled activation and, when it DECLINES after the body ran
    // (a deopt with no resumable frame), writes their current — possibly
    // relocated by a moving collection inside the body — addresses back into
    // the buffer it was handed. `args` is the caller's read-only copy, so the
    // compiled arm copies it into this stack buffer sized by the shared JIT ABI
    // limit (sixteen words on x86-64). The one-shot door applies its own
    // needs-heap and diagnostic-cap checks before entry. After a decline,
    // the interpreted fallback below reads the refreshed
    // copy instead of the pre-call one.
    let mut oneshot_args = [Value::Uninitialized; super::jit_bridge::JIT_ABI_MAX_JAVA_ARGS];
    let mut args: &[Value] = args;
    if crate::runtime::env_cache::jit_lambda_tierup()
        && !crate::runtime::env_cache::disable_jit()
        && !matches!(thread.kind, crate::threading::ThreadKind::Virtual)
        && !cached.is_synchronized
        && (cached.exception_table.is_empty() || lambda_handler_impl_oneshot_enabled(shared))
    {
        lambda_jit::bump(&lambda_jit::ELIGIBLE);
        // Epoch-guarded exactly like the twins: skip the string-keyed
        // `JitCache::get` while this entry's snapshot of
        // `JitCache::generation()` is still current, because no publication or
        // invalidation has happened since the probe that missed. The
        // generation was read at the top, BEFORE probing, so a racing
        // publication can only cause a redundant re-probe, never a missed one.
        // nio21 w16 L2: a HIT at that generation is answered from the row's
        // body memo (`LambdaImplRow`) without the probe either.
        let compiled = if let Some(body) = memo_code.take() {
            Some(body)
        } else if cached.jit_probe_is_current(jit_generation) {
            None
        } else {
            let found = shared.jit.jit_cache.read().get(
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
                cached.declaring_class_id,
            );
            if found.is_none() {
                cached.record_jit_probe_miss(jit_generation);
            } else if code_memo_on {
                if let Some(body) = found.as_ref() {
                    note_lambda_impl_code(key, jit_generation, body);
                }
            }
            // Held as `RetainedCode` across the call: if the cache withdraws the
            // body while it runs, this local is its last owner, and a bare `Arc`
            // dropped at the end of this arm unmapped a published body outside
            // the retirement queue
            // (`jit-code-uaf-outside-retirement-queue-FIXED-20260918.md`).
            found.map(cratonvm_jit::RetainedCode::new)
        };
        match compiled {
            Some(compiled) => {
                lambda_jit::bump(&lambda_jit::COMPILED_HITS);
                let n = args.len();
                if n <= oneshot_args.len() {
                    oneshot_args[..n].copy_from_slice(args);
                    if let Some(value) = execute_jit_call_oneshot(
                        shared,
                        thread,
                        &compiled,
                        &cached,
                        &mut oneshot_args[..n],
                    )? {
                        lambda_jit::bump(&lambda_jit::FAST_RETURNS);
                        lambda_jit::maybe_report();
                        screen_const_return_value(&cached, value.as_ref(), "interp-oneshot");
                        return Ok(Some(value));
                    }
                    // Re-read through the write-back (see `oneshot_args`).
                    args = &oneshot_args[..n];
                }
                // Declined (ABI limit, or a deopt with no resumable frame):
                // nothing was executed that must not be repeated, so fall
                // through to the interpreted frame build below with the same
                // `args`.
                lambda_jit::bump(&lambda_jit::DECLINES);
            }
            None => {
                // Warmup counter, mirroring the twins' `.or_else` arm. This is
                // the half that fixes the root cause: without it the method is
                // never nominated, so the probe above can never hit.
                let invoc_key = cached.invoc_key();
                let threshold = crate::runtime::env_cache::jit_invocation_threshold();
                let cnt = shared.jit.profile_store.increment_invocation(invoc_key);
                let should_attempt = should_offer_tier_up(cnt, threshold);
                if should_attempt && !crate::runtime::env_cache::bg_compile() {
                    // `CRATONVM_BG_COMPILE=0` is the documented opt-out that
                    // restores INLINE compilation on the mutator, and the twins
                    // both honour it. Without this arm a lambda impl would be
                    // nominated to a worker that is never started and stay
                    // interpreted forever in that mode — the off-switch would
                    // silently disable the whole feature rather than change how
                    // it compiles. It also makes compilation SYNCHRONOUS, which
                    // is what lets a test assert that a body really is compiled
                    // by a known iteration instead of hoping a background
                    // worker won the race.
                    let gate = RedefineGate::snapshot(
                        shared
                            .classes
                            .class_manager
                            .read()
                            .class_redefine_generation_handle(cached.declaring_class_id),
                    );
                    let _ = try_jit_upgrade_with_gate(shared, &cached, gate);
                    lambda_jit::bump(&lambda_jit::NOMINATIONS);
                } else if should_attempt {
                    ensure_bg_compiler_started(shared);
                    // Real invocation count — see the invokestatic twin:
                    // stride-boundary `+= 1` counting deflated the manager's
                    // hotness view 64x.
                    super::jit_bridge::release_withdrawn_code_owners(thread);
                    let recommended_tier =
                        offer_invocation_to_tiered_manager(shared, &*cached, cnt as u64);
                    lambda_jit::bump(&lambda_jit::NOMINATIONS);
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] lambda-tiered-enqueue {}.{}{} tier={recommended_tier:?} invoc_count={cnt}",
                            cached.class_name, cached.method_name, cached.method_descriptor,
                        );
                    }
                }
            }
        }
        lambda_jit::maybe_report();
    }
    // Taken BEFORE `cached` is moved into the frame, and only when the probe is
    // on: an unconditional `Arc::clone` here would be a refcount bump on every
    // interpreted lambda call in the process, for a diagnostic that is off.
    let const_screen_impl = if crate::runtime::env_cache::jit_lambda_const_probe()
        && const_int_return_of(&cached.code, !cached.exception_table.is_empty()).is_some()
    {
        Some(Arc::clone(&cached))
    } else {
        None
    };
    thread.refill_pools_from_shared(
        &shared.mem.operand_stack_pool,
        &shared.mem.tag_pool,
        cached.max_locals as usize,
        (cached.max_stack as usize).max(16) + 8,
    );
    // In-place install rather than a by-value build: `execute_prebuilt_frame`
    // pushes through `push_frame_and_fire_entry`, which harvests and trims the
    // retired frame slot, so every interpreted lambda call used to destroy the
    // slot the next interpreted call at that depth would have rebuilt itself
    // in. See `install_and_run_cached_frame`.
    let out =
        install_and_run_cached_frame(shared, thread, cached, args, Some("lambda-impl")).map(Some);
    // The INTERPRETED arm, and the one that matters most for a constant body:
    // a two-byte `iconst_1; ireturn` may never be nominated for compilation at
    // all, so the two compiled screens above would never see it.
    if let (Some(impl_method), Ok(Some(value))) = (&const_screen_impl, &out) {
        screen_const_return_value(impl_method, value.as_ref(), "interp-frame");
    }
    out
}

thread_local! {
    /// nio21 wave 16 L2: the shape analysis [`try_lambda_site_oneshot`] serves
    /// from, per `(vm, proxy class)`. Built with handler-bearing impls ADMITTED
    /// (the one-shot entry routes a throw through the impl's own table), so it
    /// is not [`LAMBDA_JIT_SITE_CACHE`], whose sites the compiled caller's
    /// direct arm and inline-cache thunk use and which must keep refusing
    /// them. `None` is a remembered refusal. Proxy ids are never reused
    /// (`alloc_lambda_proxy_id` is a counter), so a dead proxy's row is
    /// unreachable rather than wrong.
    static LAMBDA_ONESHOT_SITE_CACHE: std::cell::RefCell<
        // addr-keyed: not a heap address -- (VM identity, proxy class id); proxy ids are never reused.
        rustc_hash::FxHashMap<(usize, u32), Option<std::rc::Rc<LambdaJitSite>>>,
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// `CRATONVM_LAMBDA_SITE_ONESHOT` (nio21 wave 16 L2; default ON,
/// `0|false|off|no` is the kill switch): see [`try_lambda_site_oneshot`].
/// Read once per VM ([`LambdaSwitches`], gci i14/sx).
#[inline]
fn lambda_site_oneshot_enabled(shared: &SharedVm) -> bool {
    shared.lambda_switches.site_oneshot
}

/// Does `v` already have the representation a compiled parameter of
/// descriptor tag `tag` takes? Anything else is a coercion, which belongs to
/// the general path.
fn value_fits_param_tag(v: &Value, tag: u8) -> bool {
    match tag {
        b'I' | b'Z' | b'B' | b'C' | b'S' => matches!(v, Value::Int(_)),
        b'J' => matches!(v, Value::Long(_)),
        b'F' => matches!(v, Value::Float(_)),
        b'D' => matches!(v, Value::Double(_)),
        b'L' | b'[' => matches!(v, Value::Object(_)),
        _ => false,
    }
}

/// nio21 wave 16 L2: a SAM call on a lambda proxy whose impl is COMPILED and
/// whose shape `LambdaJitSite` recognizes (static impl, the call's name and
/// descriptor the SAM's, coercion provably the identity, at most a replayed
/// `checkcast` per argument) is entered straight through
/// [`execute_jit_call_oneshot`] -- what [`try_invoke_cached_lambda_impl`]
/// would do after [`try_lambda_dispatch_inner`]'s call-site lookup,
/// descriptor walks, `coerce_lambda_args`, owner memo and template cache, all
/// of which compute a constant for such a shape. That detour is the per-call
/// cost a compiled caller pays for a lambda its direct arm refuses (a
/// handler-bearing impl: `k -> { lock.lock(); try { .. } finally {
/// lock.unlock(); } }`, the w15 `lock` row) and an interpreted caller pays
/// whenever the interpreted door does not serve the site.
///
/// `None`: not served, nothing done -- the general path runs, and is what
/// counts an impl that is not compiled yet toward its nomination. `Some`: the
/// call's complete answer, as `try_lambda_dispatch` returns it. A one-shot
/// that declines after its body ran re-runs the impl interpreted HERE, from
/// the arguments it wrote back (a moving collection inside the body may have
/// relocated them), exactly as `try_invoke_cached_lambda_impl` does; the
/// caller's own copy would be stale.
#[allow(clippy::too_many_arguments)]
fn try_lambda_site_oneshot(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj_ref: ObjectRef,
    obj_class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
    call_args: &[Value],
) -> Option<Result<Option<Option<Value>>, MethodCallFailed>> {
    // One compare for an ordinary receiver: proxy ids sit above the base.
    if obj_class_id.as_u32() < crate::vm::realms::class_realm::LAMBDA_PROXY_ID_BASE
        || !lambda_site_oneshot_enabled(shared)
        || !crate::runtime::env_cache::jit_lambda_tierup()
        || crate::runtime::env_cache::disable_jit()
        || matches!(thread.kind, crate::threading::ThreadKind::Virtual)
    {
        return None;
    }
    let key = (shared.vm_identity, obj_class_id.as_u32());
    let slot = LAMBDA_ONESHOT_SITE_CACHE.with(|cache| cache.borrow().get(&key).cloned());
    let site = match slot {
        Some(Some(site)) if !site.gate_is_stale() => site,
        // The impl's class was redefined: forget it and let the general path
        // (which re-derives the owner) run this call.
        Some(Some(_)) => {
            LAMBDA_ONESHOT_SITE_CACHE.with(|cache| {
                cache.borrow_mut().remove(&key);
            });
            return None;
        }
        Some(None) => return None,
        None => {
            if !shared.classes.is_lambda_proxy_class(obj_class_id) {
                return None;
            }
            match build_lambda_jit_site(shared, obj_class_id, true) {
                SiteVerdict::Eligible(site) => {
                    LAMBDA_ONESHOT_SITE_CACHE.with(|cache| {
                        cache
                            .borrow_mut()
                            .insert(key, Some(std::rc::Rc::clone(&site)));
                    });
                    site
                }
                SiteVerdict::Never => {
                    LAMBDA_ONESHOT_SITE_CACHE.with(|cache| {
                        cache.borrow_mut().insert(key, None);
                    });
                    return None;
                }
                SiteVerdict::NotYet => return None,
            }
        }
    };
    if !site.serves(method_name, method_descriptor) {
        return None;
    }
    let num_captures = site.num_captures();
    let total_args = site.total_args();
    if num_captures + call_args.len() != total_args
        || total_args > super::jit_bridge::JIT_ABI_MAX_JAVA_ARGS
    {
        return None;
    }
    let cached = site.cached_impl();
    if cached.is_synchronized
        || !cached.is_static
        || (!cached.exception_table.is_empty() && !lambda_handler_impl_oneshot_enabled(shared))
    {
        return None;
    }
    let facts = cached.descriptor_facts();
    if facts.param_tags_overflow || facts.param_tag_len as usize != total_args {
        return None;
    }
    // Not compiled yet: the general path counts this call toward nomination.
    let code = lambda_jit_site_code(shared, &site)?;
    // The replayed bridge `checkcast`s. A cast that would throw, or would need
    // a class load, declines: the general path raises the CCE it always has.
    for (at, (sam_idx, _)) in site.checkcast_list().iter().enumerate() {
        match call_args.get(*sam_idx) {
            Some(Value::Object(Some(obj))) => {
                if !lambda_site_cast_passes(shared, &site, at, *obj) {
                    return None;
                }
            }
            Some(Value::Object(None)) => {}
            _ => return None,
        }
    }
    let mut args = [Value::Uninitialized; super::jit_bridge::JIT_ABI_MAX_JAVA_ARGS];
    for (i, slot) in args.iter_mut().enumerate().take(num_captures) {
        *slot = shared.mem.heap.get_field(obj_ref, i);
    }
    args[num_captures..total_args].copy_from_slice(call_args);
    if !args[..total_args]
        .iter()
        .zip(&facts.param_tags[..total_args])
        .all(|(v, &tag)| value_fits_param_tag(v, tag))
    {
        return None;
    }
    lambda_jit::bump(&lambda_jit::COMPILED_HITS);
    match execute_jit_call_oneshot(shared, thread, &code, cached, &mut args[..total_args]) {
        Ok(Some(value)) => {
            lambda_jit::bump(&lambda_jit::FAST_RETURNS);
            lambda_jit::maybe_report();
            screen_const_return_value(cached, value.as_ref(), "interp-oneshot");
            Some(Ok(Some(value)))
        }
        Err(e) => Some(Err(e)),
        Ok(None) => {
            // Declined: re-run interpreted from the written-back arguments.
            lambda_jit::bump(&lambda_jit::DECLINES);
            drop(code);
            let cached = Arc::clone(cached);
            thread.refill_pools_from_shared(
                &shared.mem.operand_stack_pool,
                &shared.mem.tag_pool,
                cached.max_locals as usize,
                (cached.max_stack as usize).max(16) + 8,
            );
            Some(
                install_and_run_cached_frame(
                    shared,
                    thread,
                    cached,
                    &args[..total_args],
                    Some("lambda-impl"),
                )
                .map(Some),
            )
        }
    }
}

/// Try to dispatch a method call on a lambda proxy object.
///
/// Returns:
/// - `Ok(Some(Some(value)))` — lambda handled the call and produced a return value
/// - `Ok(Some(None))` — lambda handled the call (void return)
/// - `Ok(None)` — not a lambda proxy, fall through to normal dispatch
///
/// WP2.5: also called from `proxy_invoke_handler_shared` when the
/// `InvocationHandler` is itself a lambda — the synthetic lambda
/// proxy's class_id is not in the class store, so the standard
/// `invoke_or_native` fallback would mis-route to the abstract
/// `java/lang/reflect/InvocationHandler.invoke` (which has no Code
/// attribute).
thread_local! {
    pub(super) static LAMBDA_DISPATCH_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

pub(super) fn lambda_dispatch_active() -> bool {
    LAMBDA_DISPATCH_DEPTH.with(|depth| depth.get() != 0)
}

/// Is `descriptor` `(L<iface>;)L<iface>;` or `()L<iface>;`? The two shapes of
/// the functional-interface default methods `try_lambda_dispatch` offers to a
/// registered native. Allocation-free: it replaces two `format!`s per call.
fn is_self_typed_default_descriptor(iface: &str, descriptor: &str) -> bool {
    let Some(without_semi) = descriptor.strip_suffix(';') else {
        return false;
    };
    let Some((params, ret_name)) = without_semi.split_once(")L") else {
        return false;
    };
    if ret_name != iface {
        return false;
    }
    match params.strip_prefix('(') {
        Some("") => true,
        Some(param) => param.strip_prefix('L').and_then(|p| p.strip_suffix(';')) == Some(iface),
        None => false,
    }
}

/// [`try_lambda_dispatch_inner`], and the one thing a virtual thread's yield
/// out of the impl body needs from this door (interpreter round i1 wave 25,
/// lane L7): the impl runs in a nested Rust call whose `coerce_return` never
/// runs for a continuation, so the conversion is recorded on the thread for
/// the remounted impl frame's return
/// ([`super::note_continuation_return_boundary`]).
pub(crate) fn try_lambda_dispatch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj_ref: ObjectRef,
    obj_class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
    call_args: &[Value],
) -> Result<Option<Option<Value>>, MethodCallFailed> {
    // JIT round 20 rev13b (correctness, unconditional): an ARRAY of lambda
    // proxies (`Array.newInstance(lambda.getClass(), n)`) carries the proxy's
    // id in its header (a reference array's header holds its COMPONENT), so
    // callers that ask `is_lambda_proxy_class(class_id_of(receiver))` route
    // `proxies.toString()` / `hashCode()` / `getClass()` here. An array is no
    // proxy: it dispatches on `Object`, as the JIT door already decides
    // (`try_lambda_proxy_sam_dispatch`, `jit_dispatch_class`). `Ok(None)` hands
    // the call back to ordinary dispatch.
    if shared.mem.heap.kind_of(obj_ref) == cratonvm_types::ObjectKind::Array {
        return Ok(None);
    }
    // Interpreter round i2 wave 60, lane L1 (`CRATONVM_JVMTI_C_LAMBDA_FRAMES`):
    // while a C JVMTI env is bound, a call that enters the proxy's functional
    // method (or a bridge) gets that method's frame row and, with
    // `CRATONVM_JVMTI_C_LAMBDA_EVENTS`, its `MethodEntry` / `MethodExit`
    // ([`dispatch_with_proxy_frame`]). One plain test otherwise, in a build
    // with a C JVMTI table only.
    #[cfg(feature = "experimental-debug")]
    if shared.debug.debugger_gates.c_lambda_frames_armed() {
        return dispatch_with_proxy_frame(
            shared,
            thread,
            obj_ref,
            obj_class_id,
            method_name,
            method_descriptor,
            call_args,
        );
    }
    try_lambda_dispatch_noted(
        shared,
        thread,
        obj_ref,
        obj_class_id,
        method_name,
        method_descriptor,
        call_args,
    )
}

/// Does a C JVMTI env list lambda proxy frames in `shared`
/// (`DebuggerGates::c_lambda_frames_armed`)? Then every SAM call goes through
/// [`try_lambda_dispatch`], which records the proxy method's frame: the
/// compiled callers' own lambda arms (`jit::helpers::try_lambda_site_direct_call`,
/// which also installs the inline-cache thunks, and
/// `try_fast_lambda_int_to_double_apply`) stand down, as the interpreter's
/// lambda door does (the i54-L1 retirement: a hot loop's calls listed no proxy
/// frame, `tools/probes/interp/L1/L1K54JvmtiLambdaProxyLocalsAndFields.java`
/// row `hot`). `false` in a build without the C JVMTI table.
#[inline]
pub(crate) fn lambda_proxy_frames_armed(shared: &SharedVm) -> bool {
    #[cfg(feature = "experimental-debug")]
    {
        shared.debug.debugger_gates.c_lambda_frames_armed()
    }
    #[cfg(not(feature = "experimental-debug"))]
    {
        let _ = shared;
        false
    }
}

/// [`try_lambda_dispatch`] while a C JVMTI env lists lambda proxy frames
/// (interpreter round i2 wave 60, lane L1;
/// `docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-does-not-know-the-vms-lambda-proxy-classes-FIXED-20261007.md`).
/// When the call enters the proxy's functional method or a bridge (the SAM
/// name with a descriptor [`lambda_accepts_descriptor`] admits), the method
/// gets a row on the thread for the C table's stack functions
/// (`jvmti::native_env::NativeFrameRow::enter_lambda_proxy`) for the whole
/// call, and, with `DebuggerGates::c_lambda_events`, the agents listening
/// for them get its `MethodEntry` before the body runs and its `MethodExit`
/// after (`was_popped_by_exception` for a throw; none for a virtual
/// thread's yield), as HotSpot posts them for the spun class's method
/// (MEASURED, `L1W60JvmtiLambdaMethodEvents`). Any other call is the plain
/// dispatch. Cold: only while a C JVMTI env is bound.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
fn dispatch_with_proxy_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj_ref: ObjectRef,
    obj_class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
    call_args: &[Value],
) -> Result<Option<Option<Value>>, MethodCallFailed> {
    let site = shared
        .classes
        .lambda_call_site_for(obj_class_id)
        .filter(|site| {
            *site.sam_method_name == *method_name
                && lambda_accepts_descriptor(
                    shared,
                    obj_class_id,
                    site,
                    method_name,
                    method_descriptor,
                )
        });
    let Some(site) = site else {
        return try_lambda_dispatch_noted(
            shared,
            thread,
            obj_ref,
            obj_class_id,
            method_name,
            method_descriptor,
            call_args,
        );
    };
    let index = crate::runtime::lambda_proxy::lambda_proxy_forwarding_index(
        shared,
        obj_class_id,
        &site,
        method_descriptor,
    );
    // The method's listing facts, for a debugger's listing made under its
    // locks (the i54-L1 retirement).
    crate::jvmti::native_env::note_lambda_proxy_row(shared, obj_class_id, index);
    // SAFETY: `thread` is this dispatch's live `&mut JvmThread` and outlives
    // the row, as for the JNI arms' rows. With the frame's locals (the
    // proxy, then the arguments as passed; the i54-L1 retirement), which the
    // C table's local-variable functions read.
    let _row = unsafe {
        crate::jvmti::native_env::LambdaProxyFrame::enter(
            thread as *mut JvmThread,
            obj_class_id,
            index,
            obj_ref,
            call_args,
        )
    };
    drop(site);
    // Widening: the proxy's u32 id and the u16 index into their halves of the
    // id (the C table's `jmethodID` of it).
    let method_id = (u64::from(obj_class_id.as_u32()) << 32) | u64::from(index);
    let events = shared.debug.debugger_gates.c_lambda_events();
    let vm = shared.vm_identity;
    let tid = thread.thread_id.0;
    let mut posted = false;
    if events && crate::runtime::jvmti::any_method_entry_listener_active_for_vm(vm) {
        // gci i11/ja: delivered under this thread's own handle.
        super::post_on_own_thread(thread, || {
            crate::runtime::jvmti::fire_method_entry_for_vm(vm, tid, method_id)
        });
        posted = true;
    }
    // The i54-L1 retirement: a JDWP session's `MethodEntry` of the proxy's
    // method, at its code index 0, the proxy's frame listed on top.
    posted |= super::report_lambda_proxy_method_event(shared, thread, obj_class_id, index, None);
    // An agent's callback or a debugger park may have collected: the
    // receiver and the arguments are read again from the frame's pins.
    let refreshed = if posted { _row.current() } else { None };
    let (obj_ref, args): (ObjectRef, &[Value]) = match &refreshed {
        Some((receiver, args)) if args.len() == call_args.len() => (*receiver, args.as_slice()),
        _ => (obj_ref, call_args),
    };
    let mut result = try_lambda_dispatch_noted(
        shared,
        thread,
        obj_ref,
        obj_class_id,
        method_name,
        method_descriptor,
        args,
    );
    let c_exit = events && crate::runtime::jvmti::any_method_exit_listener_active_for_vm(vm);
    let jdwp_exit = shared.debug.debugger_gates.jdwp_method_events_armed();
    if !c_exit && !jdwp_exit {
        return result;
    }
    let (popped, value, object) = match &result {
        Ok(Some(value)) => (
            false,
            super::exception_dispatch::to_local_value(value.as_ref()),
            match value {
                Some(Value::Object(Some(o))) => Some(*o),
                _ => None,
            },
        ),
        Ok(None) => (false, crate::runtime::jvmti::LocalValue::Object(None), None),
        Err(MethodCallFailed::ExceptionThrown(e)) => (
            true,
            crate::runtime::jvmti::LocalValue::Object(None),
            Some(*e),
        ),
        // A yield or an internal failure is no exit of the method.
        Err(_) => return result,
    };
    // The returned object or the exception stays reachable (and is written
    // back where a collection moved it) across the agent's callback and a
    // debugger park, as the native funnel's exit post keeps it.
    let at = thread.native_pin_roots.len();
    if let Some(object) = object {
        thread.native_pin_roots.push(object);
    }
    if c_exit {
        // gci i11/ja: delivered under this thread's own handle.
        super::post_on_own_thread(thread, || {
            crate::runtime::jvmti::fire_method_exit_for_vm(vm, tid, method_id, popped, value)
        });
    }
    // JDWP: a normal return only (HotSpot's back end reports no exit for a
    // method an exception ended), with the value as the method returns it.
    if !popped {
        let current = object.and_then(|_| thread.native_pin_roots.get(at).copied());
        let returned = match &result {
            Ok(Some(Some(Value::Object(Some(_))))) => Some(Value::Object(current)),
            Ok(Some(v)) => *v,
            _ => None,
        };
        let return_type = method_descriptor
            .rsplit(')')
            .next()
            .and_then(|ret| ret.bytes().next())
            .unwrap_or(b'V');
        super::report_lambda_proxy_method_event(
            shared,
            thread,
            obj_class_id,
            index,
            Some((return_type, returned)),
        );
    }
    if object.is_some() {
        if let Some(moved) = thread.native_pin_roots.get(at).copied() {
            match &mut result {
                Ok(Some(Some(Value::Object(Some(o))))) => *o = moved,
                Err(MethodCallFailed::ExceptionThrown(e)) => *e = moved,
                _ => {}
            }
        }
    }
    thread.native_pin_roots.truncate(at);
    result
}

/// The spun constructor's `MethodEntry` and `MethodExit` (method 0 of lambda
/// proxy `proxy`), as HotSpot posts them while the lambda's instance `obj`
/// is built (MEASURED, `tools/probes/interp/L1/L1K54JvmtiLambdaProxyEvents.java`,
/// `L1K54JdiLambdaProxyEvents`; the i54-L1 retirement): to the C JVMTI envs
/// listening for them and to a JDWP session's requests, with the
/// constructor's frame (`this`, then `captures`) recorded meanwhile. Answers
/// the instance at its current address (an agent's callback or a debugger
/// park may have collected). Three loads when nothing listens.
#[cfg(feature = "experimental-debug")]
#[inline]
pub(crate) fn post_lambda_proxy_constructor_events(
    shared: &SharedVm,
    thread: &mut JvmThread,
    proxy: ClassId,
    obj: ObjectRef,
    captures: &[Value],
) -> ObjectRef {
    let gates = &shared.debug.debugger_gates;
    if !gates.c_lambda_frames_armed()
        || !(gates.method_events_armed() || gates.jdwp_method_events_armed())
    {
        return obj;
    }
    constructor_events(shared, thread, proxy, obj, captures)
}

#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
fn constructor_events(
    shared: &SharedVm,
    thread: &mut JvmThread,
    proxy: ClassId,
    obj: ObjectRef,
    captures: &[Value],
) -> ObjectRef {
    let vm = shared.vm_identity;
    let tid = thread.thread_id.0;
    let events = shared.debug.debugger_gates.c_lambda_events();
    crate::jvmti::native_env::note_lambda_proxy_row(shared, proxy, 0);
    // SAFETY: this thread's own live `JvmThread`, which outlives the frame.
    let frame = unsafe {
        crate::jvmti::native_env::LambdaProxyFrame::enter(
            thread as *mut JvmThread,
            proxy,
            0,
            obj,
            captures,
        )
    };
    // Widening: the proxy's id into the high half; the constructor is method 0.
    let method_id = u64::from(proxy.as_u32()) << 32;
    if events && crate::runtime::jvmti::any_method_entry_listener_active_for_vm(vm) {
        // gci i11/ja: delivered under this thread's own handle.
        super::post_on_own_thread(thread, || {
            crate::runtime::jvmti::fire_method_entry_for_vm(vm, tid, method_id)
        });
    }
    super::report_lambda_proxy_method_event(shared, thread, proxy, 0, None);
    if events && crate::runtime::jvmti::any_method_exit_listener_active_for_vm(vm) {
        // gci i11/ja: delivered under this thread's own handle.
        super::post_on_own_thread(thread, || {
            crate::runtime::jvmti::fire_method_exit_for_vm(
                vm,
                tid,
                method_id,
                false,
                super::exception_dispatch::to_local_value(None),
            )
        });
    }
    super::report_lambda_proxy_method_event(shared, thread, proxy, 0, Some((b'V', None)));
    let current = frame.current().map_or(obj, |(receiver, _)| receiver);
    drop(frame);
    current
}

/// [`try_lambda_dispatch_inner`] with its continuation note (the body of
/// [`try_lambda_dispatch`] before wave 60).
fn try_lambda_dispatch_noted(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj_ref: ObjectRef,
    obj_class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
    call_args: &[Value],
) -> Result<Option<Option<Value>>, MethodCallFailed> {
    let depth_before = thread.frames.len();
    let result = try_lambda_dispatch_inner(
        shared,
        thread,
        obj_ref,
        obj_class_id,
        method_name,
        method_descriptor,
        call_args,
    );
    if matches!(
        result,
        Err(MethodCallFailed::InternalError(
            VmError::ContinuationYield { .. }
        ))
    ) {
        super::note_continuation_return_boundary(thread, depth_before, method_descriptor);
    }
    result
}

/// The receiver prologue of the native lambda dispatch's two
/// `InvokeVirtual | InvokeInterface` arms (the Scala specialised-function
/// bridge and the ordinary SAM path of [`try_lambda_dispatch_inner`]), in one
/// place (interpreter round i4 wave 92, lane L4; proposal
/// `i52-L4-proposal-one-receiver-prologue-for-the-lambda-dispatch-arms`).
///
/// `Ok(false)`: no receiver at all; each arm decides what that means. A null
/// receiver is HotSpot's spun class's own `invokevirtual` / `invokeinterface`
/// on null: a message-less NPE raised in the hidden proxy frame
/// (`String::length` applied to `null`; probe `L4W52LambdaNullReceiver`,
/// interpreter round i1 wave 52, lane L4). It used to run the target's body
/// with a null `this`; wave 52 had to add the refusal to both arms.
#[inline]
fn lambda_receiver_present(full_args: &[Value]) -> Result<bool, MethodCallFailed> {
    match full_args.first() {
        None => Ok(false),
        Some(Value::Object(None)) => {
            Err(RuntimeError::NullPointerException { message: None }.into())
        }
        Some(_) => Ok(true),
    }
}

/// What both virtual lambda arms read off the receiver: see
/// [`lambda_virtual_receiver`].
struct LambdaVirtualReceiver {
    /// The receiver's class id; `None` for a non-reference receiver.
    class_id: Option<ClassId>,
    /// The receiver class's own interned name, else the impl handle's class.
    class_name: Arc<str>,
    /// The loader-faithful override: the receiver's exact class id when its
    /// name resolves globally to a different (per-loader) copy.
    virtual_override: Option<ClassId>,
}

/// The receiver class, its name and the loader-faithful `virtual_override`
/// of a virtual lambda arm (see [`lambda_receiver_present`] for the shared
/// refusals). `is_proxy` answers "is this class id a lambda proxy" (the
/// ordinary arm passes its memoised answer).
///
/// Loader-faithful (gated): when the receiver's runtime class diverges from
/// the by-name global resolution -- a per-loader (e.g. bytecode-enhanced)
/// copy -- the arm dispatches on the receiver's EXACT class id so the lambda
/// body runs that loader's copy. `invoke_or_native` otherwise collapses the
/// name to the global copy, running the un-enhanced lambda body (`new
/// Country` -> un-enhanced entity -> reflective `Field.set` mismatch).
/// Mirrors the ordinary invoke_virtual divergence override.
#[inline]
fn lambda_virtual_receiver(
    shared: &SharedVm,
    receiver: &Value,
    impl_class: &Arc<str>,
    is_proxy: &dyn Fn(ClassId) -> bool,
) -> LambdaVirtualReceiver {
    let class_id = match receiver {
        Value::Object(Some(r)) => Some(shared.mem.heap.class_id_of(*r)),
        _ => None,
    };
    let class_name: Arc<str> = match class_id {
        Some(rcv) => shared
            .classes
            .class_manager
            .read()
            .get_class(rcv)
            .map(|c| Arc::clone(&c.name))
            .unwrap_or_else(|| Arc::clone(impl_class)),
        None => Arc::clone(impl_class),
    };
    let virtual_override = if crate::runtime::env_cache::loader_aware_resolution() {
        class_id.filter(|rcv| {
            *rcv != ClassId::new(0) && !is_proxy(*rcv) && {
                let cm = shared.classes.class_manager.read();
                // Only when `class_name` truly names the receiver's own real
                // class (not the impl-handle fallback for a lambda-proxy /
                // generic-ClassId receiver) AND that name globally resolves
                // to a DIFFERENT (per-loader) copy.
                cm.get_class(*rcv)
                    .map(|c| *c.name == *class_name)
                    .unwrap_or(false)
                    && cm.get_loaded_class_id(&class_name) != Some(*rcv)
            }
        })
    } else {
        None
    };
    LambdaVirtualReceiver {
        class_id,
        class_name,
        virtual_override,
    }
}

fn try_lambda_dispatch_inner(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj_ref: ObjectRef,
    obj_class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
    call_args: &[Value],
) -> Result<Option<Option<Value>>, MethodCallFailed> {
    // S-bytebuddy r4 — independent recursion guard for lambda dispatch.
    // Lambda SAM implementations can re-enter `try_lambda_dispatch` via
    // `invoke_or_native` / `invoke_shared` when the impl body itself
    // invokes another lambda (the common `stream.map(x -> ...).filter(y
    // -> ...)` shape). The aggregate EXEC_DEPTH guard catches this only
    // after the Rust stack has grown by ~10 frames per turn. A dedicated
    // counter trips much earlier with a tight cap.
    thread_local! {
        static LAMBDA_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }
    struct LambdaDepthGuard;
    impl Drop for LambdaDepthGuard {
        fn drop(&mut self) {
            LAMBDA_DEPTH.with(|d| {
                let v = d.get();
                d.set(v.saturating_sub(1));
            });
        }
    }
    let ldepth = LAMBDA_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v
    });
    if ldepth > 2_000 {
        LAMBDA_DEPTH.with(|d| {
            let v = d.get();
            d.set(v.saturating_sub(1));
        });
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::StackOverflowError,
        )));
    }
    let _lambda_depth_guard = LambdaDepthGuard;

    struct LambdaDispatchGuard;
    impl Drop for LambdaDispatchGuard {
        fn drop(&mut self) {
            LAMBDA_DISPATCH_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
        }
    }
    LAMBDA_DISPATCH_DEPTH.with(|depth| depth.set(depth.get() + 1));
    let _lambda_dispatch_guard = LambdaDispatchGuard;

    // `CRATONVM_DBG=lambda-prof` — see [`lambda_prof`]. The guard measures the
    // whole call on every exit path (including the `return Ok(None)` fallthroughs
    // that hand the call back to ordinary interface dispatch), because a term
    // that only shows up on one arm would otherwise be invisible.
    let prof = lambda_prof::on();
    let prof_entry = prof.then(std::time::Instant::now);
    struct ProfTotalGuard(Option<std::time::Instant>);
    impl Drop for ProfTotalGuard {
        fn drop(&mut self) {
            let Some(t0) = self.0 else { return };
            lambda_prof::add(&lambda_prof::TOTAL_NS, t0.elapsed().as_nanos() as u64);
            let n = lambda_prof::CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n % lambda_prof::REPORT_EVERY == 0 {
                lambda_prof::report();
            }
        }
    }
    let _prof_total_guard = ProfTotalGuard(prof_entry);

    // nio21 wave 16 L2: a compiled impl of a shape `LambdaJitSite` recognizes
    // is entered here, before the call-site lookup, the descriptor walks and
    // the argument coercion below (which compute the identity for such a
    // shape). Under the dispatch guards above, so `lambda_dispatch_active` and
    // the recursion cap hold exactly as for the arms below.
    if let Some(answer) = try_lambda_site_oneshot(
        shared,
        thread,
        obj_ref,
        obj_class_id,
        method_name,
        method_descriptor,
        call_args,
    ) {
        return answer;
    }

    // Look up the lambda proxy metadata for this ClassId.
    let lookup_start = prof.then(std::time::Instant::now);
    // r9w2-vm2: `lambda_call_site_for` answers an ordinary receiver (every id
    // below `LAMBDA_PROXY_ID_BASE`) with one compare instead of an `RwLock`
    // acquisition and a hash probe; a proxy still pays the real lookup.
    let Some(call_site) = shared.classes.lambda_call_site_for(obj_class_id) else {
        return Ok(None); // Not a lambda proxy
    };
    if let Some(t) = lookup_start {
        lambda_prof::add(&lambda_prof::LOOKUP_NS, t.elapsed().as_nanos() as u64);
    }
    // Marks the end of "prep" and the start of "target" for whichever
    // MethodHandleKind arm runs below; each arm stamps it immediately before its
    // own invoke. Left `None` on the arms that are not instrumented, which then
    // report their whole cost under `other`.
    let prep_start = prof.then(std::time::Instant::now);
    if crate::runtime::env_cache::lambda_dbg() {
        eprintln!(
            "[cratonvm-dbg] lambda dispatch entry: cid={} sam={}.{} impl={}.{}{} kind={:?}",
            obj_class_id,
            call_site.functional_interface,
            method_name,
            call_site.impl_handle.class_name,
            call_site.impl_handle.member_name,
            call_site.impl_handle.descriptor,
            call_site.impl_handle.kind,
        );
    }

    // A functional interface may declare same-named default overloads of its
    // SAM. Dispatching a lambda by method name, arity, or runtime argument
    // assignability is unsound: null is assignable to both `InetAddress` and
    // `InetSocketAddress`, so the latter default could be skipped entirely.
    // The bytecode call-site descriptor is the authoritative identity. Only the
    // exact SAM descriptor, or a `FLAG_BRIDGES` bridge the spun class would
    // implement, may enter the lambda body; any other descriptor must fall
    // through to ordinary interface/default-method dispatch.
    if method_name == &*call_site.sam_method_name
        && !lambda_accepts_descriptor(
            shared,
            obj_class_id,
            &call_site,
            method_name,
            method_descriptor,
        )
    {
        return Ok(None);
    }

    // Only intercept calls to the SAM (single abstract method). Default
    // methods on the functional interface (e.g. Function.andThen,
    // Predicate.and) are dispatched directly via the native registry on
    // the functional interface class.
    if method_name != &*call_site.sam_method_name {
        // RScala.1 bridge: Scala 3 produces lambdas whose functional
        // interface is `scala/runtime/java8/JFunctionN$mcXYZ$sp`, which
        // declares the SAM as a primitive-specialized method (e.g.
        // `apply$mcII$sp(I)I`). The Scala stdlib's `Function1` has a
        // default `apply$mcII$sp(int)` that boxes + calls `apply(Object)`,
        // while `JFunction1$mcII$sp` has a default `apply(Object)` that
        // unboxes + calls `apply$mcII$sp(int)`. Without picking the
        // maximally-specific default, they ping-pong until StackOverflow.
        // Bridge here: if a non-SAM method is `apply` on a Scala
        // specialized function interface, unbox args → call SAM → box.
        if method_name == "apply"
            && call_site
                .functional_interface
                .starts_with("scala/runtime/java8/JFunction")
            && call_site.sam_method_name.starts_with("apply$mc")
        {
            // Parse the specialization tag from the SAM name, e.g.
            // "apply$mcII$sp" → ("I","I") for (I)I.
            // Format: apply$mc<RET><ARG...>$sp.
            let tag = call_site
                .sam_method_name
                .strip_prefix("apply$mc")
                .and_then(|s| s.strip_suffix("$sp"));
            if let Some(tag) = tag {
                // First char = return type, rest = arg types.
                let mut chars = tag.chars();
                let ret_ch = chars.next().unwrap_or('V');
                let arg_chars: Vec<char> = chars.collect();

                // Unbox each argument (call_args are all boxed Object).
                let mut unboxed: Vec<Value> = Vec::with_capacity(arg_chars.len());
                for (i, &ac) in arg_chars.iter().enumerate() {
                    let v = call_args.get(i).copied().unwrap_or(Value::Object(None));
                    let u = match (ac, v) {
                        ('I' | 'Z' | 'B' | 'S' | 'C', Value::Object(Some(b))) => {
                            shared.mem.heap.get_field(b, 0)
                        }
                        ('J', Value::Object(Some(b))) => shared.mem.heap.get_field(b, 0),
                        ('F', Value::Object(Some(b))) => shared.mem.heap.get_field(b, 0),
                        ('D', Value::Object(Some(b))) => shared.mem.heap.get_field(b, 0),
                        (_, other) => other,
                    };
                    unboxed.push(u);
                }

                // Reenter lambda dispatch with the SAM method name and
                // unboxed args. Note: call_args we pass are the SAM's
                // primitive args (captures are read inside).
                let sam_name = call_site.sam_method_name.clone();
                // Drop call_site borrow before recursing indirectly.
                // Recursion depth is bounded (one hop to SAM path).
                drop(call_site);
                // Reborrow fresh to avoid use-after-move.
                let lcs = {
                    let proxies = shared.classes.lambda_proxies.read();
                    proxies.get(&obj_class_id).cloned()
                };
                let lcs = match lcs {
                    Some(l) => l,
                    None => {
                        crate::runtime::diagnostics::record_swallow(
                            shared,
                            "lambda-dispatch",
                            "proxy-lost-mid-dispatch",
                            &format!("class_id={} method={}", obj_class_id, method_name),
                        );
                        return Ok(None);
                    }
                };

                // Execute the SAM path directly (mirrors the code below).
                let num_captures = lcs.capture_types.len();
                let mut full_args: Vec<Value> = Vec::with_capacity(num_captures + unboxed.len());
                for i in 0..num_captures {
                    full_args.push(shared.mem.heap.get_field(obj_ref, i));
                }
                full_args.extend(unboxed);
                let _ = sam_name; // sam path uses lcs.impl_handle

                // Dispatch to the implementation handle.
                let result_val = match lcs.impl_handle.kind {
                    MethodHandleKind::InvokeStatic => {
                        if let Some(impl_cid) = lambda_impl_dispatch_override(shared, &lcs) {
                            // Initializes the declaring class, as the by-name
                            // arm below does (JVMS §5.5; wave 15).
                            crate::vm::invoke_static_on_class_no_retarget(
                                shared,
                                thread,
                                impl_cid,
                                &lcs.impl_handle.class_name,
                                &lcs.impl_handle.member_name,
                                &lcs.impl_handle.descriptor,
                                &full_args,
                            )?
                        } else {
                            // REF_invokeStatic initializes the DECLARING class
                            // only (JVMS §5.5; wave 14).
                            crate::vm::invoke_static_shared(
                                shared,
                                thread,
                                &lcs.impl_handle.class_name,
                                &lcs.impl_handle.member_name,
                                &lcs.impl_handle.descriptor,
                                &full_args,
                            )?
                        }
                    }
                    MethodHandleKind::InvokeVirtual | MethodHandleKind::InvokeInterface => {
                        // The shared receiver prologue (`lambda_receiver_present`,
                        // `lambda_virtual_receiver`). This arm's own decisions:
                        // no receiver at all is a recorded swallow handed back
                        // to ordinary dispatch (the ordinary arm below calls it
                        // an internal error), and a receiver that is itself a
                        // lambda proxy is not re-dispatched through its SAM
                        // here (the ordinary arm does that).
                        if !lambda_receiver_present(&full_args)? {
                            crate::runtime::diagnostics::record_swallow(
                                shared,
                                "lambda-dispatch",
                                "virtual-no-receiver",
                                &format!(
                                    "class={} member={}",
                                    lcs.impl_handle.class_name, lcs.impl_handle.member_name
                                ),
                            );
                            return Ok(None);
                        }
                        let receiver = lambda_virtual_receiver(
                            shared,
                            &full_args[0],
                            &lcs.impl_handle.class_name,
                            &|cid: ClassId| shared.classes.is_lambda_proxy_class(cid),
                        );
                        let receiver_class = receiver.class_name;
                        // JIT round 20 wave 12 (lane qLam; correctness): JVMS
                        // §5.4.6 selection, as in the ordinary virtual arm below.
                        let selected = receiver
                            .class_id
                            .filter(|rcv| {
                                *rcv != ClassId::new(0)
                                    && !shared.classes.is_lambda_proxy_class(*rcv)
                            })
                            .and_then(|rcv| lambda_virtual_selection_moves_to(shared, &lcs, rcv));
                        if let Some(selected) = selected {
                            crate::vm::invoke_on_class_shared_no_retarget(
                                shared,
                                thread,
                                selected,
                                &lcs.impl_handle.member_name,
                                &lcs.impl_handle.descriptor,
                                &full_args,
                            )?
                        } else if let Some(rcv_cid) = receiver.virtual_override {
                            invoke_on_class_shared(
                                shared,
                                thread,
                                rcv_cid,
                                &lcs.impl_handle.member_name,
                                &lcs.impl_handle.descriptor,
                                &full_args,
                            )?
                        } else {
                            invoke_or_native(
                                shared,
                                thread,
                                &receiver_class,
                                &lcs.impl_handle.member_name,
                                &lcs.impl_handle.descriptor,
                                &full_args,
                            )?
                        }
                    }
                    other => {
                        crate::runtime::diagnostics::record_swallow(
                            shared,
                            "lambda-dispatch",
                            "unsupported-handle-kind",
                            &format!(
                                "kind={:?} class={} member={}",
                                other, lcs.impl_handle.class_name, lcs.impl_handle.member_name
                            ),
                        );
                        return Ok(None);
                    }
                };

                // Box the primitive return to match apply(Object)Object
                // by invoking the respective `valueOf` static.
                let box_one = |shared: &SharedVm,
                               thread: &mut JvmThread,
                               cls: &str,
                               desc: &str,
                               prim: Value|
                 -> Result<Value, MethodCallFailed> {
                    let r = invoke_shared(shared, thread, cls, "valueOf", desc, &[prim])?;
                    Ok(r.unwrap_or(Value::Object(None)))
                };
                let raw = result_val.unwrap_or(Value::Object(None));
                let boxed = match (ret_ch, raw) {
                    ('V', _) => Some(Value::Object(None)),
                    ('Z', Value::Int(i)) => Some(box_one(
                        shared,
                        thread,
                        "java/lang/Boolean",
                        "(Z)Ljava/lang/Boolean;",
                        Value::Int(i),
                    )?),
                    ('B', Value::Int(i)) => Some(box_one(
                        shared,
                        thread,
                        "java/lang/Byte",
                        "(B)Ljava/lang/Byte;",
                        Value::Int(i),
                    )?),
                    ('S', Value::Int(i)) => Some(box_one(
                        shared,
                        thread,
                        "java/lang/Short",
                        "(S)Ljava/lang/Short;",
                        Value::Int(i),
                    )?),
                    ('C', Value::Int(i)) => Some(box_one(
                        shared,
                        thread,
                        "java/lang/Character",
                        "(C)Ljava/lang/Character;",
                        Value::Int(i),
                    )?),
                    ('I', Value::Int(i)) => Some(box_one(
                        shared,
                        thread,
                        "java/lang/Integer",
                        "(I)Ljava/lang/Integer;",
                        Value::Int(i),
                    )?),
                    ('J', Value::Long(l)) => Some(box_one(
                        shared,
                        thread,
                        "java/lang/Long",
                        "(J)Ljava/lang/Long;",
                        Value::Long(l),
                    )?),
                    ('F', Value::Float(f)) => Some(box_one(
                        shared,
                        thread,
                        "java/lang/Float",
                        "(F)Ljava/lang/Float;",
                        Value::Float(f),
                    )?),
                    ('D', Value::Double(d)) => Some(box_one(
                        shared,
                        thread,
                        "java/lang/Double",
                        "(D)Ljava/lang/Double;",
                        Value::Double(d),
                    )?),
                    (_, v) => Some(v),
                };
                return Ok(Some(boxed));
            }
        }

        // A native registered for a self-typed default method of the
        // functional interface: `(L<iface>;)L<iface>;` (andThen/compose/and/or)
        // or `()L<iface>;` (negate/identity).
        //
        // Probed with the CALL's OWN descriptor, and only when that descriptor
        // has one of those two shapes. This used to probe both shapes whatever
        // was called, so e.g. `thenComparing(Function)` on a `Comparator`
        // lambda would dispatch a native registered for the
        // `thenComparing(Comparator)` overload, handing it a `Function`. It also
        // built both probe descriptors with `format!`, plus the argument
        // vector, on every non-SAM call on every lambda -- before knowing
        // whether anything was registered at all.
        //
        // Every such default (`Function.andThen`, `Predicate.negate`,
        // `Comparator.reversed`, ...) has a real `Code` body in `java.base`, so
        // this door makes a registered native beat concrete bytecode -- the
        // force-intercept shape. It is admitted through the same §7 helper the
        // force interceptors use (`admit_forced_native`, `bytecode_available:
        // true`): `Compatible` is unchanged (a registered native still wins),
        // `--jdk-only` hands a `Bridge` back to the real default body and
        // rejects a `SyntheticStub`, and the dispatch is counted in the census.
        // It used to call the callback bare -- no policy, no census, and no
        // argument pinning for a native that re-enters Java.
        let iface = &*call_site.functional_interface;
        if is_self_typed_default_descriptor(iface, method_descriptor) {
            let callback = match super::native_override::admit_forced_native(
                shared,
                iface,
                method_name,
                method_descriptor,
            ) {
                Ok(Some(callback)) => Some(callback),
                Ok(None) => None,
                Err(violation) => {
                    return Err(MethodCallFailed::InternalError(
                        crate::error::VmError::JdkOnly(violation),
                    ));
                }
            };
            if let Some(callback) = callback {
                // Receiver prepended.
                let mut full_args = Vec::with_capacity(1 + call_args.len());
                full_args.push(Value::Object(Some(obj_ref)));
                full_args.extend_from_slice(call_args);
                let result = crate::vm::safe_native_call(shared, thread, callback, &full_args)?;
                return Ok(Some(result));
            }
        }
        // Not found in native registry — fall through to normal dispatch
        return Ok(None);
    }

    // Read captured values from the proxy object's fields.
    let num_captures = call_site.capture_types.len();
    let mut full_args: Vec<Value> = Vec::with_capacity(num_captures + call_args.len());
    for i in 0..num_captures {
        full_args.push(shared.mem.heap.get_field(obj_ref, i));
    }
    // Append the invocation arguments (passed by the caller after the receiver).
    full_args.extend_from_slice(call_args);

    // Dispatch based on the implementation method handle kind.
    //
    // The CALL's descriptor stands in for the SAM's: it IS the SAM descriptor,
    // or a recorded bridge, whose conversions to the impl types are then the
    // ones `coerce_lambda_args` / `coerce_return` perform (see
    // `lambda_accepts_descriptor`).
    let sam_desc: &str = method_descriptor;
    let impl_desc = call_site.impl_handle.descriptor.clone();
    let inst_desc = call_site.instantiated_descriptor.clone();
    // Only the RETURN token is read here. This used to be two
    // `split_method_descriptor` calls, i.e. a full parameter walk plus a `Vec`
    // plus a `String` per parameter of both descriptors, allocated and dropped
    // on every lambda invocation, for two `&str`s.
    let sam_ret = descriptor_return_ref(&sam_desc);
    let impl_ret = descriptor_return_ref(&impl_desc);
    match call_site.impl_handle.kind {
        MethodHandleKind::InvokeStatic => {
            // Static method: all args are parameters (no receiver).
            coerce_lambda_args(
                shared,
                thread,
                &sam_desc,
                &impl_desc,
                &inst_desc,
                &mut full_args,
                false,
                num_captures,
            )?;
            if crate::runtime::env_cache::lambda_dbg() {
                eprintln!(
                    "[cratonvm-dbg] lambda static-pre-invoke: {}.{}{} args={}",
                    call_site.impl_handle.class_name,
                    call_site.impl_handle.member_name,
                    call_site.impl_handle.descriptor,
                    full_args.len(),
                );
            }
            let target_start = prep_start.map(|t| {
                lambda_prof::add(&lambda_prof::PREP_NS, t.elapsed().as_nanos() as u64);
                std::time::Instant::now()
            });
            // gen r4w3/rooting: `lambda_impl_dispatch_override_driven` can drive
            // a user loader's `loadClass` (Java, so a moving GC) while
            // `full_args` — captures plus call args — sits in a bare Vec. Pin
            // its objects across that call and write the current addresses
            // back before any branch below consumes it; the guard's drop
            // restores the pin watermark on every exit.
            let full_args_root = InvokeArgsRootGuard::new(thread, &full_args);
            let override_cid = lambda_impl_dispatch_override_driven(shared, thread, &call_site);
            full_args_root.refresh(&mut full_args);
            drop(full_args_root);
            let result = if let Some(impl_cid) = override_cid {
                // Loader-faithful: the enclosing class was defined by a user
                // loader whose copy of the impl owner diverges from the global
                // one; dispatch on the exact loader-local class (static → no
                // receiver retarget), initializing the declaring class first
                // as the by-name arm below does (JVMS §5.5; wave 15).
                crate::vm::invoke_static_on_class_no_retarget(
                    shared,
                    thread,
                    impl_cid,
                    &call_site.impl_handle.class_name,
                    &call_site.impl_handle.member_name,
                    &call_site.impl_handle.descriptor,
                    &full_args,
                )?
            } else if let Some(owner) = lambda_global_impl_owner(shared, &call_site) {
                // Steady state. Reaching here at all means a previous dispatch
                // already went through `invoke_shared` below, which loaded the
                // class, ran `<clinit>`, and dispatched — so from the second call
                // on, everything `invoke_shared` does before the actual invoke is
                // re-derivation of a constant, and `lambda-prof` measured that
                // re-derivation plus the by-name method lookup at ~2.8 us of a
                // ~3.6 us dispatch.
                //
                // The cached-bytecode path is the same one the
                // Virtual/Interface arm already uses; it enters the impl body
                // with a pooled prebuilt frame and no name lookup at all. It
                // declines (`Ok(None)`) for anything it cannot serve — a native
                // shadow, a `synchronized` or abstract body, an arity mismatch, a
                // redefined class — and then the generic path below runs.
                match try_invoke_cached_lambda_impl(
                    shared,
                    thread,
                    obj_class_id,
                    owner,
                    &call_site.impl_handle.member_name,
                    &call_site.impl_handle.descriptor,
                    &full_args,
                )? {
                    Some(v) => v,
                    None => invoke_on_class_shared(
                        shared,
                        thread,
                        owner,
                        &call_site.impl_handle.member_name,
                        &call_site.impl_handle.descriptor,
                        &full_args,
                    )?,
                }
            } else {
                // REF_invokeStatic initializes the DECLARING class only
                // (JVMS §5.5): `C::m` for an `m` inherited from `P` runs
                // `P.<clinit>`, never `C.<clinit>` (wave 14).
                let r = crate::vm::invoke_static_shared(
                    shared,
                    thread,
                    &call_site.impl_handle.class_name,
                    &call_site.impl_handle.member_name,
                    &call_site.impl_handle.descriptor,
                    &full_args,
                );
                // `CRATONVM_LAMBDA_OWNER_MEMO_ON_YIELD` (interpreter round i6
                // wave 125, lane L6, default off): a park inside the impl
                // leaves through the `?` below, so a lambda that parks on
                // every call never reaches the record after it. Recorded on
                // the yield too, once the class is initialized
                // (`record_lambda_global_impl_owner_on_yield`). Off: one
                // per-VM bool load on a yield only.
                if shared.lambda_switches.owner_memo_on_yield
                    && matches!(
                        r,
                        Err(MethodCallFailed::InternalError(
                            VmError::ContinuationYield { .. }
                        ))
                    )
                {
                    record_lambda_global_impl_owner_on_yield(shared, &call_site);
                }
                let r = r?;
                // Only after it succeeded: a name that failed to resolve or
                // whose `<clinit>` threw must not be remembered as an answer.
                record_lambda_global_impl_owner(shared, &call_site);
                r
            };
            if let Some(t) = target_start {
                lambda_prof::add(&lambda_prof::TARGET_NS, t.elapsed().as_nanos() as u64);
            }
            if crate::runtime::env_cache::lambda_dbg() {
                eprintln!(
                    "[cratonvm-dbg] lambda static-post-invoke: {}.{}{} result={:?}",
                    call_site.impl_handle.class_name,
                    call_site.impl_handle.member_name,
                    call_site.impl_handle.descriptor,
                    result.is_some(),
                );
            }
            Ok(Some(coerce_return(
                shared, thread, &sam_ret, &impl_ret, result,
            )?))
        }
        MethodHandleKind::InvokeVirtual | MethodHandleKind::InvokeInterface => {
            // Virtual/interface: first arg is receiver, rest are parameters.
            // The null refusal is `lambda_receiver_present`'s, shared with
            // the Scala bridge arm above; no receiver at all is an internal
            // error here (that arm records a swallow and declines).
            if !lambda_receiver_present(&full_args)? {
                return Err(VmError::Internal {
                    message: "lambda dispatch: InvokeVirtual/InvokeInterface with no args"
                        .to_string(),
                }
                .into());
            }
            // Coerce args (keep receiver at [0] unchanged for virtual dispatch).
            coerce_lambda_args(
                shared,
                thread,
                &sam_desc,
                &impl_desc,
                &inst_desc,
                &mut full_args,
                true,
                num_captures,
            )?;
            let private_impl_class = lambda_private_impl_dispatch_class(shared, &call_site);
            // Round 7 — if the receiver is itself a lambda proxy whose SAM
            // matches the impl_handle's member name, recurse through
            // try_lambda_dispatch directly. Without this, downstream
            // `invoke_or_native` falls back to the cp interface name (because
            // class_manager.get_class fails on lambda-proxy class_ids), then
            // resolves the abstract interface declaration with no Code attribute
            // and surfaces an AbstractMethodError. Concrete tripwire: Spring
            // Boot's `CacheOverrides.close()` does
            // `forEach(CacheOverride::close)` and the iterated items are
            // themselves NOOP `CacheOverride` lambdas declared as static
            // fields on `SoftReferenceConfigurationPropertyCache` — every
            // item is a lambda proxy, never a concrete CacheOverride.
            // nio21 w15 L5: the answer is kept for the two later receiver
            // screens below, which asked `is_lambda_proxy_class` (an `RwLock`
            // read for every id at or above the proxy base) of the same class
            // id again. Proxy registration is never undone, so the answer
            // cannot change in between.
            let mut recv_proxy_answer: Option<(ClassId, bool)> = None;
            if let Value::Object(Some(r)) = &full_args[0] {
                let rcv_class_id = shared.mem.heap.class_id_of(*r);
                let recv_is_lambda = shared.classes.is_lambda_proxy_class(rcv_class_id);
                recv_proxy_answer = Some((rcv_class_id, recv_is_lambda));
                if recv_is_lambda {
                    let inner = try_lambda_dispatch(
                        shared,
                        thread,
                        *r,
                        rcv_class_id,
                        &call_site.impl_handle.member_name,
                        &call_site.impl_handle.descriptor,
                        &full_args[1..],
                    )?;
                    if let Some(inner_v) = inner {
                        return Ok(Some(coerce_return(
                            shared, thread, &sam_ret, &impl_ret, inner_v,
                        )?));
                    }
                }
            }
            // Resolve the actual class of the receiver for virtual dispatch:
            // its class id, its interned name (the class's own `Arc<str>`, not
            // a `String` copy: this allocated a heap `String` on every call
            // through a virtual method reference such as `map(String::trim)`)
            // and the loader-faithful override (`lambda_virtual_receiver`).
            let recv_is_proxy = |cid: ClassId| match recv_proxy_answer {
                Some((known, answer)) if known == cid => answer,
                _ => shared.classes.is_lambda_proxy_class(cid),
            };
            let LambdaVirtualReceiver {
                class_id: recv_class_id_opt,
                class_name: receiver_class,
                virtual_override,
            } = lambda_virtual_receiver(
                shared,
                &full_args[0],
                &call_site.impl_handle.class_name,
                &recv_is_proxy,
            );
            // Diagnostic (CRATONVM_DBG_LAMBDA): when a lambda dispatch receiver
            // resolves to an unknown/zero class (the stale-captured-reference
            // family — NoSuchMethodError like "java/lang/Object.get(I)D"),
            // dump the raw pointer, its class id, the load_and_forward result,
            // and a FRESH re-read of the proxy's capture field. Discriminates
            // "stale baked into the proxy field" (fresh re-read returns the
            // same dead pointer) from a transient Rust-local staleness.
            if crate::runtime::env_cache::lambda_dbg() {
                if let (Some(cid), Value::Object(Some(r))) = (recv_class_id_opt, &full_args[0]) {
                    if cid == ClassId::new(0) {
                        let fwd = shared.mem.heap.load_and_forward(*r);
                        let fwd_cid = shared.mem.heap.class_id_of(fwd);
                        let fresh = shared.mem.heap.get_field(obj_ref, 0);
                        let (fresh_ptr, fresh_cid) = match fresh {
                            Value::Object(Some(f)) => {
                                (f.as_ptr() as usize, Some(shared.mem.heap.class_id_of(f)))
                            }
                            _ => (0, None),
                        };
                        eprintln!(
                            "[lambda-nsme-diag] recv={:p} cid={:?} fwd={:p} fwd_cid={:?} \
                             proxy={:p} fresh_field=0x{:x} fresh_cid={:?} impl={}.{}{}",
                            r.as_ptr(),
                            cid,
                            fwd.as_ptr(),
                            fwd_cid,
                            obj_ref.as_ptr(),
                            fresh_ptr,
                            fresh_cid,
                            call_site.impl_handle.class_name,
                            call_site.impl_handle.member_name,
                            call_site.impl_handle.descriptor,
                        );
                    }
                }
            }
            // A private instance lambda body is encoded by javac as an
            // InvokeVirtual handle, but it retains invokespecial semantics:
            // resolve it on the handle's declaring class, not by walking the
            // captured receiver's hierarchy. Synthetic lambda names are not
            // unique across a hierarchy (for example both Spring Data's
            // AnnotationBasedPersistentProperty and AbstractPersistentProperty
            // have lambda$new$2), so receiver-based lookup can execute a
            // different private body with the same name and descriptor.
            //
            // `private_impl_class` (computed above) IS that owner. This used to
            // be followed by a second derivation, `private_impl_owner`: the
            // same owner expression (`lambda_impl_dispatch_override`, else the
            // by-name loaded id), then the owner's OWN `find_method` tested for
            // `PRIVATE`. It could never add an answer: a private method is
            // never abstract (JVMS §4.6), so when the owner declares it,
            // `find_method_recursive` returns the owner's own declaration
            // first and `private_impl_class` is already `Some(owner)`. It cost
            // two `class_manager` read acquisitions, a by-name class hash and a
            // method walk on every virtual-arm lambda call (lambda dispatch
            // plan, stage 2).
            // JIT round 20 wave 12 (lane qLam;
            // `jr20-lambda-virtual-arm-selects-by-name-not-by-override-20261012`,
            // correctness, unconditional): every branch below selects the body
            // by NAME on the receiver's chain (`find_method_recursive`), which
            // takes a non-overriding method -- a package-private one of another
            // runtime package, a private one -- for the override. Where JVMS
            // §5.4.6 selects another declaration, that class's own method is
            // bound exactly as a private impl's owner is (no retarget).
            let exact_impl_owner = private_impl_class.or_else(|| {
                recv_class_id_opt
                    .filter(|rcv| *rcv != ClassId::new(0) && !recv_is_proxy(*rcv))
                    .and_then(|rcv| lambda_virtual_selection_moves_to(shared, &call_site, rcv))
            });
            // THE RECEIVER-KEYED NATIVE. An ordinary `it.remove()` is an
            // `invokeinterface`, and `dispatch_virtual.rs` probes the registry
            // with the RECEIVER's runtime class. A bound method reference
            // `it::remove` arrives HERE, where every remaining branch resolves
            // the method first and so probes with the class that DECLARES it.
            // For this VM's collection iterators those are different names:
            // the native is registered on `java/util/HashMap$KeyIterator`
            // (`MAP_KEY_ITR_CARRIERS`), which does not declare `remove()` --
            // `java/util/HashMap$HashIterator` does. Resolution walked past the
            // registration and ran the real body against an instance this VM
            // minted, which raised `IllegalStateException` off a `lastReturned`
            // field nothing had written.
            //
            // MEASURED (`probes/MethodRefDoorProbe`, HotSpot 25.0.4+7,
            // IDENTICAL in both modes): `it::remove` threw ISE for `HashSet`,
            // `HashMap.keySet()` and `Properties.keySet()` where the direct
            // `it.remove()` worked. `ArrayList` and `Hashtable` passed for
            // opposite reasons, which is what makes the pair a discriminator
            // rather than a guess -- `ArrayList$Itr` DECLARES its own
            // `remove()`, so the two class names coincide, and `Hashtable`'s
            // view iterator is java.base's own `Hashtable$Enumerator` with no
            // native in front of it at all.
            //
            // SCOPED to the case where the receiver's exact class does NOT
            // declare the method. When it does declare one, the native and the
            // body are both on the same class, resolution lands on the name the
            // registration is keyed to, and the existing native-precedence
            // rules downstream already decide between them -- this must not
            // pre-empt that decision. What is left is exactly the case where
            // the registration is the only thing registered for the receiver's
            // own class and nothing downstream will ever look at it.
            //
            // The receiver's class inherits a concrete body for this method, so
            // a native here beats bytecode: admitted through the §7
            // force-intercept helper like every other such door (`Compatible`
            // unchanged; `--jdk-only` hands a `Bridge` to the inherited body and
            // rejects a `SyntheticStub`; the dispatch is counted). It was a
            // bare `find`.
            //
            // The "does the receiver's class declare it, and which registry
            // id" half is memoised (`lambda_receiver_native_id`, dispatch plan
            // stage 3); the admission still runs on every call.
            let receiver_native_id = if exact_impl_owner.is_none() {
                recv_class_id_opt
                    .filter(|rcv| *rcv != ClassId::new(0))
                    .and_then(|rcv| {
                        lambda_receiver_native_id(
                            shared,
                            call_site.proxy_class_id,
                            rcv,
                            &receiver_class,
                            &call_site.impl_handle.member_name,
                            &call_site.impl_handle.descriptor,
                        )
                    })
            } else {
                None
            };
            let receiver_native = match receiver_native_id {
                Some(id) => match super::native_override::admit_forced_native_id(
                    shared,
                    id,
                    &receiver_class,
                    &call_site.impl_handle.member_name,
                    &call_site.impl_handle.descriptor,
                ) {
                    Ok(callback) => callback,
                    Err(violation) => {
                        return Err(MethodCallFailed::InternalError(VmError::JdkOnly(violation)));
                    }
                },
                None => None,
            };
            let cached_result = if exact_impl_owner.is_none() && receiver_native.is_none() {
                recv_class_id_opt
                    .filter(|rcv| *rcv != ClassId::new(0))
                    .map(|rcv| {
                        try_invoke_cached_lambda_impl(
                            shared,
                            thread,
                            obj_class_id,
                            rcv,
                            &call_site.impl_handle.member_name,
                            &call_site.impl_handle.descriptor,
                            &full_args,
                        )
                    })
                    .transpose()?
                    .flatten()
            } else {
                None
            };
            // gen r4w3/rooting: the NoSuchMethodError retry below re-sends
            // `full_args` after a first dispatch attempt that can already have
            // run Java (class loading / `<clinit>`) and so moved its objects.
            // Pin them across that attempt and refresh before the retry; the
            // guard is dropped (watermark restored) right after the retry.
            let full_args_root = InvokeArgsRootGuard::new(thread, &full_args);
            let result = if let Some(callback) = receiver_native {
                // `safe_native_call` rather than a bare `callback(..)`: it is
                // what every other door uses, and it owns the argument pinning
                // a native that re-enters Java needs.
                crate::vm::safe_native_call(shared, thread, callback, &full_args)
            } else if let Some(owner_id) = exact_impl_owner {
                crate::vm::invoke_on_class_shared_no_retarget(
                    shared,
                    thread,
                    owner_id,
                    &call_site.impl_handle.member_name,
                    &call_site.impl_handle.descriptor,
                    &full_args,
                )
            } else if let Some(result) = cached_result {
                Ok(result)
            } else if let Some(rcv_cid) = virtual_override {
                invoke_on_class_shared(
                    shared,
                    thread,
                    rcv_cid,
                    &call_site.impl_handle.member_name,
                    &call_site.impl_handle.descriptor,
                    &full_args,
                )
            } else if let Some(rcv_cid) = recv_class_id_opt.filter(|rcv| {
                *rcv != ClassId::new(0)
                    && !recv_is_proxy(*rcv)
                    && shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(*rcv)
                        .is_some()
            }) {
                // The captured receiver is already a loaded, concrete object.
                // Dispatch by its ClassId instead of converting it back to a class
                // name and re-entering invoke_shared's class-loader path for every
                // SAM call. invoke_on_class_shared keeps the normal native
                // precedence, virtual retargeting, monitor, and exception rules.
                invoke_on_class_shared(
                    shared,
                    thread,
                    rcv_cid,
                    &call_site.impl_handle.member_name,
                    &call_site.impl_handle.descriptor,
                    &full_args,
                )
            } else {
                invoke_or_native(
                    shared,
                    thread,
                    &receiver_class,
                    &call_site.impl_handle.member_name,
                    &call_site.impl_handle.descriptor,
                    &full_args,
                )
            };
            // If receiver's class didn't have the SAM method, fall back to the
            // class specified in the lambda call site. Handles objects with
            // generic ClassId (stub/Object) targeting a specific class.
            //
            // HIB-CV-31 FIX: the retry must fire ONLY when the SAM itself failed
            // to resolve on the receiver — i.e. the `NoSuchMethodError` names
            // exactly `(receiver_class, member_name)`. The old guard fired for
            // ANY `NoSuchMethodError`, including one raised deep INSIDE a
            // successfully-dispatched `onFlush` body (e.g. H2's `IOUtils.readFully`
            // calling `in.read()` on a corrupted `InputStream` reference →
            // `java/lang/Object.read()I` NSME). Because `receiver_class`
            // (`DefaultFlushEventListener`) != `impl_handle.class_name`
            // (the abstract `FlushEventListener`), the old guard re-dispatched
            // `onFlush` onto the abstract interface — which has no Code attribute
            // — fabricating a misleading `AbstractMethodError` that both masked
            // the real in-body error and reported a phantom dispatch failure.
            // Constraining the NSME to the SAM's own (class, method) makes the
            // retry serve only its intended case (generic-ClassId receiver) and
            // lets genuine in-body linkage errors propagate unchanged.
            let result = match &result {
                Err(MethodCallFailed::InternalError(VmError::Linkage(
                    LinkageError::NoSuchMethodError {
                        class_name: nsme_class,
                        method_name: nsme_method,
                        ..
                    },
                ))) if *receiver_class != *call_site.impl_handle.class_name
                    && nsme_class.as_str() == &*receiver_class
                    && nsme_method.as_str() == &*call_site.impl_handle.member_name =>
                {
                    // gen r4w3/rooting: current addresses, not the pre-attempt ones.
                    full_args_root.refresh(&mut full_args);
                    invoke_or_native(
                        shared,
                        thread,
                        &call_site.impl_handle.class_name,
                        &call_site.impl_handle.member_name,
                        &call_site.impl_handle.descriptor,
                        &full_args,
                    )
                }
                _ => result,
            };
            drop(full_args_root);
            let r = result?;
            Ok(Some(coerce_return(shared, thread, &sam_ret, &impl_ret, r)?))
        }
        MethodHandleKind::InvokeSpecial => {
            // Special: dispatch on the declaring class (no virtual lookup).
            //
            // `invokespecial` always targets an instance method (private or
            // super-call); the lambda capture list therefore begins with the
            // bound `this`, which sits at `full_args[0]` once captures are
            // prepended.  `coerce_lambda_args` must skip that slot — passing
            // `receiver_present=false` here causes the argument-to-parameter
            // index map to slide by one, leaving the last SAM-supplied arg
            // unconverted.  Concrete failure: Flink's `getRawValueFromOption`
            // lambda binds `getRawValue(String, Z)` from a
            // `BiFunction<String, Boolean, ...>`; with the off-by-one the
            // trailing `Boolean` reaches the impl's `boolean` slot still
            // boxed, and `iload_2` later raises
            // "expected int on stack, got ref(...)".
            coerce_lambda_args(
                shared,
                thread,
                &sam_desc,
                &impl_desc,
                &inst_desc,
                &mut full_args,
                true,
                num_captures,
            )?;
            // Loader-faithful owner resolution (gated): prefer the enclosing
            // loader's copy of the impl class when it diverges from the global.
            //
            // gen r4w3/rooting: owner resolution can drive a user loader's
            // `loadClass` (Java, so a moving GC); pin `full_args` across it and
            // write the current addresses back before the dispatch reads them.
            // The guard's drop restores the watermark on the `?` exit too.
            let full_args_root = InvokeArgsRootGuard::new(thread, &full_args);
            let class_id = lambda_impl_owner_class_id(shared, thread, &call_site)?;
            full_args_root.refresh(&mut full_args);
            drop(full_args_root);
            // A REF_invokeSpecial lambda target is statically bound to its
            // implementation owner.  In particular, an Interface.crate::runtime::m
            // method reference must reach that interface default method even
            // when the receiver overrides m.  Retargeting here can resolve a
            // same-named synthetic lambda helper on the receiver instead and
            // recurse through the default method indefinitely.
            let result = crate::vm::invoke_on_class_shared_no_retarget(
                shared,
                thread,
                class_id,
                &call_site.impl_handle.member_name,
                &call_site.impl_handle.descriptor,
                &full_args,
            )?;
            Ok(Some(coerce_return(
                shared, thread, &sam_ret, &impl_ret, result,
            )?))
        }
        MethodHandleKind::NewInvokeSpecial => {
            // Constructor reference: allocate object, call <init>, return the object.
            // Loader-faithful owner resolution (gated), same rationale as above.
            //
            // Residual 4 (2026-07-20, core-spring-boot-test-config-data-and-classpath-scan-cluster-FIXED.md):
            // this used the PASSIVE-only `lambda_impl_dispatch_override` (cache
            // read, never drives a cold miss) with a loader-blind
            // `load_class(name)` fallback — the exact InvokeStatic gap already
            // fixed by `lambda_impl_dispatch_override_driven` (see that
            // function's own doc comment), just never mirrored onto this sibling
            // MethodHandleKind. A constructor-reference lambda
            // (`SomeType::new`, e.g. Spring AOT's generated
            // `AotApplicationContextInitializer::new` factory) whose impl class
            // is the very FIRST thing touched from a fork loader's namespace hit
            // the same loader-blind fallback and minted an Application-loader
            // copy instead of the fork's own. (Independently fixed upstream on
            // origin/dev with the same shape; kept in sync here.)
            //
            // GC-safety: `full_args` is a bare `Vec<Value>` — the proxy's
            // captured fields plus the call arguments — and it is NOT a GC
            // root. Everything from here to the `<init>` dispatch can run a
            // moving young collection: owner resolution below can drive a
            // user loader's `loadClass`, `ensure_class_initialized_shared`
            // runs the class's `<clinit>`, and `gc_alloc_object` allocates. A
            // collection there relocates the captured objects and leaves every
            // entry in this vector naming the pre-move address, which is then
            // laid straight into `<init>`'s locals.
            //
            // MEASURED on BindableTests under `CRATONVM_DBG_GC_STRESS=262144`:
            // `TestMethodTestDescriptor::new`, reached as a constructor
            // reference, received `arg[4]` — the `enclosingInstanceTypes`
            // `Supplier` — at the address a collection had moved it away from
            // one cycle earlier, and every later `get()` on it dispatched
            // through an all-zero header. The `new_obj` pin below already
            // documents exactly this hazard for the receiver; the arguments
            // needed the same protection and did not have it.
            //
            // gen r4w3/rooting: the pins are taken BEFORE owner resolution
            // (which can run Java); they used to be taken after it, so they
            // captured addresses a collection there had already vacated.
            //
            // `fa_base` is the group base, so one `truncate` releases the whole
            // set — including `new_obj`'s pin — on every exit from here on.
            //
            // First the SAM-to-`<init>` conversions every other kind applies:
            // unboxing (`Function<Integer, Foo> f = Foo::new` over `Foo(int)`)
            // and the instantiated-type `checkcast`. This arm skipped them
            // until wave 17 (the native-callback copy, now deleted, did not).
            // No receiver: captures and SAM arguments are all `<init>`
            // parameters. It pins and releases its own roots.
            coerce_lambda_args(
                shared,
                thread,
                &sam_desc,
                &impl_desc,
                &inst_desc,
                &mut full_args,
                false,
                num_captures,
            )?;
            let fa_base = thread.native_pin_roots.len();
            let fa_handles: Vec<Option<usize>> = full_args
                .iter()
                .map(|v| match v {
                    Value::Object(Some(o)) => {
                        let h = thread.native_pin_roots.len();
                        thread.native_pin_roots.push(*o);
                        Some(h)
                    }
                    _ => None,
                })
                .collect();
            let class_id = match lambda_impl_owner_class_id(shared, thread, &call_site) {
                Ok(cid) => cid,
                Err(e) => {
                    thread.native_pin_roots.truncate(fa_base);
                    return Err(e);
                }
            };
            // Array-constructor reference (`SomeType[]::new`, e.g. as an
            // `IntFunction<SomeType[]>` — the mechanism behind
            // `Collection.toArray(SomeType[]::new)` and any direct user code).
            // `load_class` already resolves an array-shaped impl class name
            // (e.g. "[Ljava/nio/ByteBuffer;") to its synthesized array
            // ClassId (JVMS 5.3.3 — array classes are never loaded from a
            // class file), but everything below this point assumes a
            // REGULAR object: it allocates `num_total_fields` (0 for an
            // array class) object slots and dispatches `<init>`, which does
            // not exist for arrays. That produced a zero-field object
            // wearing the array's ClassId — no length header, no element
            // storage, and the requested length (the sole `IntFunction`
            // argument) silently discarded — which a later `checkcast` to
            // the real array type then rejects
            // (`ClassCastException: ... cannot be cast to [Lyour/Type;`).
            // Detect the array case up front and dispatch to real array
            // allocation instead.
            let array_info = shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .and_then(|c| c.array_info.clone());
            if let Some(array_info) = array_info {
                // gen r4w3/rooting: this branch reads only the int length
                // from `full_args`, so it needs none of the argument pins.
                thread.native_pin_roots.truncate(fa_base);
                let length =
                    full_args
                        .first()
                        .and_then(Value::as_int)
                        .ok_or_else(|| VmError::Internal {
                            message: "array-constructor-reference: missing length arg".to_string(),
                        })?;
                if length < 0 {
                    return Err(RuntimeError::NegativeArraySizeException { size: length }.into());
                }
                let (element_type, component_class_id) = if array_info.array_dimension == 1 {
                    match &*array_info.leaf_component_name {
                        "boolean" => (ArrayElementType::Boolean, ClassId::new(0)),
                        "char" => (ArrayElementType::Char, ClassId::new(0)),
                        "float" => (ArrayElementType::Float, ClassId::new(0)),
                        "double" => (ArrayElementType::Double, ClassId::new(0)),
                        "byte" => (ArrayElementType::Byte, ClassId::new(0)),
                        "short" => (ArrayElementType::Short, ClassId::new(0)),
                        "int" => (ArrayElementType::Int, ClassId::new(0)),
                        "long" => (ArrayElementType::Long, ClassId::new(0)),
                        _ => (ArrayElementType::Reference, array_info.component_class_id),
                    }
                } else {
                    (ArrayElementType::Reference, array_info.component_class_id)
                };
                let arr = gc_alloc_array(
                    shared,
                    thread,
                    component_class_id,
                    element_type,
                    length as usize,
                )?;
                // gen r4w3/rooting: `maybe_gc` can collect and `arr` is only a
                // Rust local; pin it across and return the current address.
                let arr_pin = thread.native_pin_roots.len();
                thread.native_pin_roots.push(arr);
                maybe_gc(shared, thread);
                let arr = thread.native_pin_roots[arr_pin];
                thread.native_pin_roots.truncate(arr_pin);
                return Ok(Some(Some(Value::Object(Some(arr)))));
            }
            // (The argument pins `fa_base`/`fa_handles` were taken above,
            // before owner resolution — see the GC-safety note there.)
            if let Err(e) = ensure_class_initialized_shared(shared, thread, class_id) {
                thread.native_pin_roots.truncate(fa_base);
                return Err(e);
            }
            // Use `num_total_fields` (inherited + declared instance fields),
            // matching the `New` opcode. `c.fields.len()` is wrong here: it
            // counts this class's declared fields *including statics* while
            // omitting inherited instance fields, so a subclass constructor
            // reference would under-allocate and trip the GC `get_field`
            // bounds guard on any inherited-field access.
            let num_fields = shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|c| c.num_total_fields)
                .unwrap_or(0);
            let new_obj = match gc_alloc_object(shared, thread, class_id, num_fields) {
                Ok(o) => o,
                Err(e) => {
                    thread.native_pin_roots.truncate(fa_base);
                    return Err(e);
                }
            };
            // Refresh the arguments from their pins — the read that makes the
            // pinning above worth anything.
            for (j, h) in fa_handles.iter().enumerate() {
                if let Some(h) = *h {
                    if let Some(cur) = thread.native_pin_roots.get(h).copied() {
                        full_args[j] = Value::Object(Some(cur));
                    }
                }
            }
            // Build <init> args: [new_obj, ...full_args]. The constructor body
            // can allocate and trigger a moving GC; the Java frame/locals are
            // remapped, but this Rust local `new_obj` is not. Pin the receiver
            // across `<init>` and return the forwarded object ref. (Since wave
            // 17 this is also the native-callback path: `vm_exec.rs`'s copy of
            // this arm is gone.)
            let new_obj_pin = thread.native_pin_roots.len();
            thread.native_pin_roots.push(new_obj);
            // A virtual thread whose `<init>` parks or sleeps must not unmount
            // here: `<init>` returns `void` to a remounted caller that was
            // owed the new object, which this Rust frame hands back only after
            // the call (interpreter round i1 wave 26, lane L4; the
            // `ContinuationReturnAdapter` the value-return arms apply cannot
            // carry an object into a `void` return). The continuation is
            // pinned for the call, as HotSpot pins one with a native frame on
            // it: the park blocks the carrier (`native_callee_memo::
            // pin_continuation`). One thread-kind test per constructor
            // reference on a platform thread.
            let continuation_pin = matches!(thread.kind, crate::threading::ThreadKind::Virtual)
                .then(crate::runtime::native_callee_memo::pin_continuation);
            let init_result = (|| -> Result<(), MethodCallFailed> {
                let mut init_args = Vec::with_capacity(1 + full_args.len());
                init_args.push(Value::Object(Some(new_obj)));
                init_args.extend_from_slice(&full_args);
                invoke_on_class_shared(
                    shared,
                    thread,
                    class_id,
                    &call_site.impl_handle.member_name,
                    &call_site.impl_handle.descriptor,
                    &init_args,
                )?;
                Ok(())
            })();
            drop(continuation_pin);
            let forwarded = thread
                .native_pin_roots
                .get(new_obj_pin)
                .copied()
                .unwrap_or(new_obj);
            // Releases `new_obj`'s pin and the argument pins together —
            // `fa_base` is below `new_obj_pin`.
            thread.native_pin_roots.truncate(fa_base);
            init_result?;
            // A `void` SAM (`Consumer<String> c = Foo::new`) discards the new
            // object: `coerce_return` answers `None` for it, as for every other
            // kind, where this arm answered the object and the invoke door
            // pushed it onto the caller's operand stack (wave 17).
            let made = Some(Value::Object(Some(forwarded)));
            Ok(Some(coerce_return(
                shared, thread, &sam_ret, &impl_ret, made,
            )?))
        }
        MethodHandleKind::GetField => {
            // Field getter: first arg is the object, return the field value.
            if full_args.is_empty() {
                return Err(VmError::Internal {
                    message: "lambda dispatch: GetField with no args".to_string(),
                }
                .into());
            }
            match &full_args[0] {
                Value::Object(Some(target_ref)) => {
                    // We need to resolve the field index. For simplicity, do a field lookup.
                    let target_class_id = shared.mem.heap.class_id_of(*target_ref);
                    let field_index = {
                        let cm = shared.classes.class_manager.read();
                        find_field_recursive(
                            target_class_id,
                            &call_site.impl_handle.member_name,
                            &cm.class_store,
                        )
                        .map(|(idx, _, _)| idx)
                        .ok_or_else(|| VmError::Internal {
                            message: format!(
                                "lambda dispatch: field {} not found",
                                call_site.impl_handle.member_name
                            ),
                        })?
                    };
                    let value = shared.mem.heap.get_field(*target_ref, field_index);
                    crate::memory::ref_discovery::keep_alive_referent_read(
                        shared,
                        *target_ref,
                        field_index,
                        value,
                    );
                    Ok(Some(Some(value)))
                }
                _ => Err(VmError::Internal {
                    message: "lambda dispatch: GetField on null".to_string(),
                }
                .into()),
            }
        }
        MethodHandleKind::GetStatic => {
            let class_id = lambda_impl_owner_class_id(shared, thread, &call_site)?;
            ensure_class_initialized_shared(shared, thread, class_id)?;
            let field_index = {
                let cm = shared.classes.class_manager.read();
                find_field_recursive(
                    class_id,
                    &call_site.impl_handle.member_name,
                    &cm.class_store,
                )
                .map(|(idx, _, _)| idx)
                .ok_or_else(|| VmError::Internal {
                    message: format!(
                        "lambda dispatch: static field {} not found",
                        call_site.impl_handle.member_name
                    ),
                })?
            };
            let value = crate::vm::get_static_shared(shared, class_id, field_index);
            Ok(Some(Some(value)))
        }
        MethodHandleKind::PutField => {
            if full_args.len() < 2 {
                return Err(VmError::Internal {
                    message: "lambda dispatch: PutField needs object + value".to_string(),
                }
                .into());
            }
            match &full_args[0] {
                Value::Object(Some(target_ref)) => {
                    let target_class_id = shared.mem.heap.class_id_of(*target_ref);
                    let field_index = {
                        let cm = shared.classes.class_manager.read();
                        find_field_recursive(
                            target_class_id,
                            &call_site.impl_handle.member_name,
                            &cm.class_store,
                        )
                        .map(|(idx, _, _)| idx)
                        .ok_or_else(|| VmError::Internal {
                            message: format!(
                                "lambda dispatch: field {} not found",
                                call_site.impl_handle.member_name
                            ),
                        })?
                    };
                    shared
                        .mem
                        .heap
                        .set_field(*target_ref, field_index, full_args[1]);
                    Ok(Some(None))
                }
                _ => Err(VmError::Internal {
                    message: "lambda dispatch: PutField on null".to_string(),
                }
                .into()),
            }
        }
        MethodHandleKind::PutStatic => {
            if full_args.is_empty() {
                return Err(VmError::Internal {
                    message: "lambda dispatch: PutStatic needs a value".to_string(),
                }
                .into());
            }
            // gen r4w4/rooting2 (residual item 7): owner resolution can run a
            // user `loadClass` and `ensure_class_initialized_shared` runs
            // `<clinit>`, both Java, so the value about to be stored is pinned
            // across them and refreshed before the store -- the same guard the
            // invoke arms above use.
            let full_args_root = InvokeArgsRootGuard::new(thread, &full_args);
            let class_id = lambda_impl_owner_class_id(shared, thread, &call_site)?;
            ensure_class_initialized_shared(shared, thread, class_id)?;
            full_args_root.refresh(&mut full_args);
            drop(full_args_root);
            let field_index = {
                let cm = shared.classes.class_manager.read();
                find_field_recursive(
                    class_id,
                    &call_site.impl_handle.member_name,
                    &cm.class_store,
                )
                .map(|(idx, _, _)| idx)
                .ok_or_else(|| VmError::Internal {
                    message: format!(
                        "lambda dispatch: static field {} not found",
                        call_site.impl_handle.member_name
                    ),
                })?
            };
            crate::vm::set_static_shared(shared, class_id, field_index, full_args[0]);
            Ok(Some(None))
        }
    }
}

use std::sync::atomic::{AtomicU64, Ordering};

/// The constant an int-category method body returns unconditionally, if it is
/// one of those bodies.
///
/// Only the shapes javac emits for `return <literal>;` — a push and an
/// `ireturn`, nothing else in the method. `CompletionStages.alwaysTrue` is
/// `iconst_1; ireturn`, two bytes. Deliberately narrow: the point is a body
/// whose correct answer is knowable WITHOUT running it, so anything requiring
/// analysis is not a candidate.
///
/// `ireturn` covers the whole int category — `boolean`, `byte`, `char`,
/// `short`, `int` — which is where the functional interfaces that matter here
/// (`IntPredicate`, `Predicate`, `BiPredicate`) return.
pub(crate) fn const_int_return_of(code: &[u8], has_handlers: bool) -> Option<i32> {
    const IRETURN: u8 = 0xAC;
    if has_handlers {
        // A handler can transfer control to a bci past the `ireturn`, so the
        // prefix below would no longer describe every path through the body.
        return None;
    }
    // A PREFIX match, not an exact-length one: this VM's `code` slice is
    // zero-padded (`alwaysTrue` arrives as `[04, ac, 00, 00]` — `iconst_1;
    // ireturn; nop; nop`), and an exact `[push, IRETURN]` pattern silently
    // matched nothing at all on the very method this probe was written for.
    //
    // Sound because the prefix ENDS in an unconditional return and contains no
    // branch: with no handlers, nothing can reach a later bci, so whatever
    // follows cannot execute.
    match *code {
        // iconst_m1 .. iconst_5
        [op @ 0x02..=0x08, IRETURN, ..] => Some(i32::from(op) - 0x03),
        // bipush n
        [0x10, n, IRETURN, ..] => Some(i32::from(n as i8)),
        // sipush n
        [0x11, hi, lo, IRETURN, ..] => Some(i32::from(i16::from_be_bytes([hi, lo]))),
        _ => None,
    }
}

/// Calls screened by the constant-return probe, and what they found.
///
/// UNGATED atomics rather than a debug-only structure: a correctness counter
/// that only exists when a debug variable is set cannot answer "did this ever
/// happen" after the fact, and the whole value of this probe is that ONE wrong
/// call is proof.
static CONST_SCREENED: AtomicU64 = AtomicU64::new(0);
static CONST_MISMATCH: AtomicU64 = AtomicU64::new(0);
static CONST_DIRTY_HIGH: AtomicU64 = AtomicU64::new(0);
static CONST_OPAQUE: AtomicU64 = AtomicU64::new(0);

/// Count one SITE whose calls this probe cannot see — a constant-returning
/// impl that was given an emitted inline-cache thunk. The thunk tail-jumps to
/// the impl and returns straight to its compiled caller, so no Rust runs on
/// that path and every call it serves goes unscreened.
///
/// A site count, not a call count: the calls are exactly what cannot be
/// counted here.
///
/// This is the probe's honesty counter. A run with `site_const_mismatch=0` and
/// a large `site_const_opaque` has not cleared the impl of anything; it has
/// only failed to look. `CRATONVM_JIT_LAMBDA_CONST_PROBE=strict` refuses those
/// thunks so the number goes to zero — at the cost of the fast path, and of
/// changing the very codegen under suspicion.
pub(crate) fn const_probe_note_opaque() {
    CONST_OPAQUE.fetch_add(1, Ordering::Relaxed);
}

/// Screen one observed return value against the constant its body must return.
///
/// `raw` is the full JIT-ABI register word. The Java value is its low 32 bits,
/// so that is what decides a MISMATCH; a correct low half carried in a word
/// with junk above it is counted separately, because a caller that reads the
/// register at the wrong width would see the junk and not the value.
pub(crate) fn const_probe_screen(
    expected: i32,
    raw: i64,
    impl_class: &str,
    impl_name: &str,
    arm: &str,
) {
    CONST_SCREENED.fetch_add(1, Ordering::Relaxed);
    let observed = raw as i32;
    if observed != expected {
        let n = CONST_MISMATCH.fetch_add(1, Ordering::Relaxed);
        if n < 32 {
            eprintln!(
                "[cratonvm-lambda-const] WRONG ANSWER from a constant body: \
                 {impl_class}.{impl_name} must return {expected}, observed {observed} \
                 (raw={raw:#x}) via {arm}"
            );
        }
        return;
    }
    if (raw as u64) >> 32 != 0 {
        let n = CONST_DIRTY_HIGH.fetch_add(1, Ordering::Relaxed);
        if n < 8 {
            eprintln!(
                "[cratonvm-lambda-const] {impl_class}.{impl_name} returned the right \
                 int ({expected}) in a word with a dirty upper half (raw={raw:#x}) via {arm}"
            );
        }
    }
}

/// `(screened, mismatched, dirty-upper-half, opaque)` — the constant-return
/// probe's counters, for tests and for the census line.
pub(crate) fn lambda_const_probe_counts() -> (u64, u64, u64, u64) {
    (
        CONST_SCREENED.load(Ordering::Relaxed),
        CONST_MISMATCH.load(Ordering::Relaxed),
        CONST_DIRTY_HIGH.load(Ordering::Relaxed),
        CONST_OPAQUE.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod const_return_decode_tests {
    use super::const_int_return_of;

    /// `CompletionStages.alwaysTrue` is these two bytes, and the probe's whole
    /// premise is recognising them.
    #[test]
    fn iconst_1_ireturn_is_the_constant_one() {
        assert_eq!(const_int_return_of(&[0x04, 0xAC], false), Some(1));
        assert_eq!(const_int_return_of(&[0x03, 0xAC], false), Some(0));
        assert_eq!(const_int_return_of(&[0x02, 0xAC], false), Some(-1));
        assert_eq!(const_int_return_of(&[0x08, 0xAC], false), Some(5));
    }

    #[test]
    fn pushed_literals_decode_with_their_sign() {
        assert_eq!(const_int_return_of(&[0x10, 0xFF, 0xAC], false), Some(-1));
        assert_eq!(const_int_return_of(&[0x10, 0x7F, 0xAC], false), Some(127));
        assert_eq!(
            const_int_return_of(&[0x11, 0xFF, 0x00, 0xAC], false),
            Some(-256)
        );
    }

    /// The regression that made the first version of this probe screen NOTHING:
    /// this VM hands out a zero-padded `code` slice, so `alwaysTrue` arrives as
    /// four bytes and an exact-length pattern missed the one method the probe
    /// exists for. `site_const_screened=0` was the only thing that showed it.
    #[test]
    fn trailing_padding_does_not_hide_a_constant_body() {
        assert_eq!(
            const_int_return_of(&[0x04, 0xAC, 0x00, 0x00], false),
            Some(1)
        );
        assert_eq!(
            const_int_return_of(&[0x10, 0x2A, 0xAC, 0x00], false),
            Some(42)
        );
    }

    /// With a handler in the table, a bci past the `ireturn` is reachable, so
    /// the prefix no longer describes every path and the body is not a
    /// candidate.
    #[test]
    fn a_handler_makes_the_prefix_argument_invalid() {
        assert_eq!(const_int_return_of(&[0x04, 0xAC, 0x00, 0x00], true), None);
    }

    /// Anything that is not a push and an `ireturn` is not a candidate: a body
    /// the probe cannot predict must not be screened against a guess.
    #[test]
    fn a_body_that_does_anything_else_is_not_a_constant() {
        assert_eq!(const_int_return_of(&[0x04], false), None);
        // iload_0; ireturn — returns an argument, not a constant.
        assert_eq!(const_int_return_of(&[0x1A, 0xAC], false), None);
        // iconst_1; areturn — right constant, wrong return category.
        assert_eq!(const_int_return_of(&[0x04, 0xB0], false), None);
        assert_eq!(const_int_return_of(&[], false), None);
    }
}

/// Screen an interpreted/one-shot SAM result against the constant its body must
/// return. The `Value`-typed twin of `jit::helpers::screen_const_return`.
///
/// A non-int `Value` from a body whose bytecode ends in `ireturn` is itself
/// wrong, and is reported as such rather than skipped: this arm reconstructs
/// the return value, so a coercion that lost the type is exactly the kind of
/// defect worth catching here.
pub(crate) fn screen_const_return_value(
    cached: &CachedBytecodeMethod,
    value: Option<&Value>,
    arm: &str,
) {
    if !crate::runtime::env_cache::jit_lambda_const_probe() {
        return;
    }
    let Some(expected) = const_int_return_of(&cached.code, !cached.exception_table.is_empty())
    else {
        return;
    };
    let raw = match value {
        Some(Value::Int(v)) => i64::from(*v),
        // Anything else is already a mismatch. `i64::from(expected) ^ 1` can
        // never equal the expectation, so the screen reports it as the wrong
        // answer it is instead of silently agreeing.
        _ => i64::from(expected) ^ 1,
    };
    const_probe_screen(expected, raw, &cached.class_name, &cached.method_name, arm);
}

#[cfg(test)]
mod self_typed_default_descriptor_tests {
    use super::is_self_typed_default_descriptor;

    const FUNCTION: &str = "java/util/function/Function";
    const COMPARATOR: &str = "java/util/Comparator";

    #[test]
    fn accepts_the_two_self_typed_shapes() {
        assert!(is_self_typed_default_descriptor(
            FUNCTION,
            "(Ljava/util/function/Function;)Ljava/util/function/Function;"
        ));
        assert!(is_self_typed_default_descriptor(
            COMPARATOR,
            "()Ljava/util/Comparator;"
        ));
    }

    /// The overload the old two-shape probe could confuse: the call's own
    /// descriptor takes a `Function`, and must not be served by a native
    /// registered for the `(Comparator)` shape.
    #[test]
    fn rejects_another_overload_of_the_same_name() {
        assert!(!is_self_typed_default_descriptor(
            COMPARATOR,
            "(Ljava/util/function/Function;)Ljava/util/Comparator;"
        ));
        assert!(!is_self_typed_default_descriptor(
            COMPARATOR,
            "(Ljava/util/function/Function;Ljava/util/Comparator;)Ljava/util/Comparator;"
        ));
    }

    #[test]
    fn rejects_other_return_types_and_malformed_input() {
        assert!(!is_self_typed_default_descriptor(
            FUNCTION,
            "(Ljava/util/function/Function;)Ljava/lang/Object;"
        ));
        assert!(!is_self_typed_default_descriptor(FUNCTION, "()V"));
        assert!(!is_self_typed_default_descriptor(FUNCTION, "()I"));
        assert!(!is_self_typed_default_descriptor(FUNCTION, ""));
        assert!(!is_self_typed_default_descriptor(
            FUNCTION,
            "Ljava/util/function/Function;)Ljava/util/function/Function;"
        ));
        // A prefix of the interface name is not the interface.
        assert!(!is_self_typed_default_descriptor(
            "java/util/function/Func",
            "()Ljava/util/function/Function;"
        ));
    }

    /// The self-typed default door runs a registered native in front of
    /// concrete `java.base` bytecode, so it must be admitted through the §7
    /// force-intercept helper and called through `safe_native_call` — never a
    /// bare `find` + callback, which had no policy, no census and no pinning.
    /// Structural, like `native_override`'s dial-door scans: a door that stops
    /// asking the policy is invisible to every counter.
    #[test]
    fn self_typed_default_door_is_policy_routed() {
        let src = include_str!("lambda.rs");
        let start = src
            .find("if is_self_typed_default_descriptor(iface, method_descriptor) {")
            .expect("the self-typed default door moved; this scan reads the wrong text");
        let rest = &src[start..];
        let end = rest
            .find("// Not found in native registry")
            .expect("the door's end marker moved");
        let door = &rest[..end];
        assert!(door.contains("native_override::admit_forced_native("));
        assert!(door.contains("crate::vm::safe_native_call("));
        assert!(
            !door.contains(".native_methods"),
            "the door probes the registry directly again, bypassing dispatch policy"
        );
    }
}

#[cfg(test)]
mod private_impl_memo_tests {
    use super::{
        forget_unloaded_private_impl_memo, forget_vm_private_impl_memo,
        lambda_private_impl_dispatch_class, private_impl_memo, ClassId, SharedVm,
    };
    use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};
    use std::sync::Arc;

    fn site(proxy: u32, owner: &str, name: &str, desc: &str) -> LambdaCallSite {
        LambdaCallSite {
            functional_interface: Arc::from("java/util/function/ToIntFunction"),
            functional_interface_id: None,
            sam_method_name: Arc::from("applyAsInt"),
            sam_descriptor: Arc::from("(Ljava/lang/Object;)I"),
            impl_handle: MethodHandle {
                kind: MethodHandleKind::InvokeVirtual,
                class_name: Arc::from(owner),
                member_name: Arc::from(name),
                descriptor: Arc::from(desc),
            },
            instantiated_descriptor: Arc::from("(Ljava/lang/Object;)I"),
            capture_types: vec![],
            proxy_class_id: ClassId::new(proxy),
            serializable_flag: false,
        }
    }

    /// A public impl method has no private declaring class; with no
    /// user-defined loader in the process the answer is memoised for the
    /// proxy, and the row goes with its VM.
    #[test]
    fn a_public_impl_answer_is_memoised_and_dropped_with_its_vm() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        if shared
            .classes
            .class_manager
            .write()
            .load_class("java/lang/Object")
            .is_err()
        {
            return; // Stripped VM (no class library on the test classpath).
        }
        let cs = site(0x7FF0_0001, "java/lang/Object", "hashCode", "()I");
        assert_eq!(lambda_private_impl_dispatch_class(&shared, &cs), None);
        let key = (shared.vm_identity, cs.proxy_class_id);
        // The flag only ever goes false -> true, so `false` now means it was
        // `false` when the memo rule was evaluated.
        if !cratonvm_native_builtins::classloader::any_defining_loader_registered(&shared.loader_identity) {
            assert_eq!(private_impl_memo().read().get(&key).copied(), Some(None));
        }
        // Served from the memo: same answer.
        assert_eq!(lambda_private_impl_dispatch_class(&shared, &cs), None);
        forget_vm_private_impl_memo(shared.vm_identity);
        assert!(private_impl_memo().read().get(&key).is_none());
    }

    /// A row whose answer names an unloaded class is dropped; a row of
    /// another VM is not.
    #[test]
    fn a_row_naming_an_unloaded_class_is_forgotten() {
        // gcg g2/k: identities from the real counter (were `usize::MAX - 11/12`).
        let (vm, other_vm) = (
            crate::vm::vm_init::test_vm_identity(),
            crate::vm::vm_init::test_vm_identity(),
        );
        let dead = ClassId::new(0x7FF0_0100);
        let live = ClassId::new(0x7FF0_0101);
        {
            let mut memo = private_impl_memo().write();
            memo.insert((vm, ClassId::new(0x7FF0_0002)), Some(dead));
            memo.insert((vm, ClassId::new(0x7FF0_0003)), Some(live));
            memo.insert((other_vm, ClassId::new(0x7FF0_0002)), Some(dead));
        }
        let unloaded: rustc_hash::FxHashSet<ClassId> = [dead].into_iter().collect();
        forget_unloaded_private_impl_memo(vm, &unloaded);
        {
            let memo = private_impl_memo().read();
            assert!(memo.get(&(vm, ClassId::new(0x7FF0_0002))).is_none());
            assert_eq!(
                memo.get(&(vm, ClassId::new(0x7FF0_0003))).copied(),
                Some(Some(live))
            );
            assert!(memo.get(&(other_vm, ClassId::new(0x7FF0_0002))).is_some());
        }
        forget_vm_private_impl_memo(vm);
        forget_vm_private_impl_memo(other_vm);
    }
}

#[cfg(test)]
mod receiver_native_memo_tests {
    use super::{
        forget_unloaded_private_impl_memo, forget_vm_private_impl_memo, lambda_receiver_native_id,
        receiver_native_memo, ClassId, SharedVm,
    };

    /// Lambda dispatch plan stage 3: a row stamped with the current native
    /// registry generation is served as is; a row stamped with another one is
    /// re-derived (here for a receiver class the VM cannot name, which has no
    /// receiver-keyed native) and re-stamped. Rows go with an unloaded
    /// receiver class and with their VM.
    #[test]
    fn the_receiver_native_id_is_memoised_per_registry_generation() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let vm = shared.vm_identity;
        let generation = shared.natives.native_methods.generation();
        let proxy = ClassId::new(0x7FF0_0201);
        let (served, stale) = (ClassId::new(0x7FF0_0202), ClassId::new(0x7FF0_0203));
        let planted = cratonvm_native_api::NativeMethodId::from_u32(7);
        {
            let mut memo = receiver_native_memo().write();
            memo.insert((vm, proxy, served), (generation, Some(planted)));
            memo.insert(
                (vm, proxy, stale),
                (generation.wrapping_add(1), Some(planted)),
            );
        }
        assert_eq!(
            lambda_receiver_native_id(&shared, proxy, served, "x/Y", "m", "()V"),
            Some(planted)
        );
        assert_eq!(
            lambda_receiver_native_id(&shared, proxy, stale, "x/Y", "m", "()V"),
            None
        );
        assert_eq!(
            receiver_native_memo()
                .read()
                .get(&(vm, proxy, stale))
                .copied(),
            Some((generation, None))
        );
        let unloaded: rustc_hash::FxHashSet<ClassId> = [stale].into_iter().collect();
        forget_unloaded_private_impl_memo(vm, &unloaded);
        assert!(receiver_native_memo()
            .read()
            .get(&(vm, proxy, stale))
            .is_none());
        assert!(receiver_native_memo()
            .read()
            .get(&(vm, proxy, served))
            .is_some());
        forget_vm_private_impl_memo(vm);
        assert!(!receiver_native_memo()
            .read()
            .keys()
            .any(|&(vid, _, _)| vid == vm));
    }

    /// Wave 8: the rows KEYED by a proxy whose host unloaded go from both
    /// memos (`forget_unloaded_lambda_proxy_rows`); another proxy's rows and
    /// another VM's rows stay.
    #[test]
    fn rows_keyed_by_a_dead_proxy_are_forgotten() {
        let (vm, other_vm) = (usize::MAX - 21, usize::MAX - 22);
        let (dead, live) = (ClassId::new(0x7FF0_0301), ClassId::new(0x7FF0_0302));
        let receiver = ClassId::new(0x7FF0_0303);
        {
            let mut memo = super::private_impl_memo().write();
            memo.insert((vm, dead), None);
            memo.insert((vm, live), None);
            memo.insert((other_vm, dead), None);
            let mut natives = receiver_native_memo().write();
            natives.insert((vm, dead, receiver), (0, None));
            natives.insert((vm, live, receiver), (0, None));
            natives.insert((other_vm, dead, receiver), (0, None));
        }
        let dead_proxies: rustc_hash::FxHashSet<ClassId> = [dead].into_iter().collect();
        super::forget_dead_lambda_proxy_memo_rows(vm, &dead_proxies);
        {
            let memo = super::private_impl_memo().read();
            assert!(!memo.contains_key(&(vm, dead)));
            assert!(memo.contains_key(&(vm, live)));
            assert!(memo.contains_key(&(other_vm, dead)));
            let natives = receiver_native_memo().read();
            assert!(!natives.contains_key(&(vm, dead, receiver)));
            assert!(natives.contains_key(&(vm, live, receiver)));
            assert!(natives.contains_key(&(other_vm, dead, receiver)));
        }
        forget_vm_private_impl_memo(vm);
        forget_vm_private_impl_memo(other_vm);
    }
}

#[cfg(test)]
mod i6_l6_cast_verdict_tests {
    use super::{lambda_arg_cast_verdict, lambda_arg_provably_not_instance, ObjectRef, SharedVm};

    const NEVER_DEFINED: &str = "Lcratonvm/test/L6NeverDefinedCastTarget;";

    fn vm_with_object() -> Option<(SharedVm, ObjectRef)> {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let object = shared
            .classes
            .class_manager
            .write()
            .load_class("java/lang/Object")
            .ok()?;
        let obj = shared.mem.heap.alloc_object(object, 0);
        Some((shared, obj))
    }

    /// The JIT direct arm's form must never load: a target nobody has loaded
    /// is undecided (the arm declines), and is still unloaded afterwards.
    #[test]
    fn the_safepoint_free_verdict_is_undecided_on_an_unloaded_target_and_loads_nothing() {
        let Some((shared, obj)) = vm_with_object() else {
            return; // Stripped VM (no class library on the test classpath).
        };
        assert_eq!(
            lambda_arg_cast_verdict(&shared, &|| obj, NEVER_DEFINED, false),
            None
        );
        assert!(shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id("cratonvm/test/L6NeverDefinedCastTarget")
            .is_none());
        // The loading form resolves it, fails, and fails open.
        assert_eq!(
            lambda_arg_cast_verdict(&shared, &|| obj, NEVER_DEFINED, true),
            Some(false)
        );
        assert!(!lambda_arg_provably_not_instance(
            &shared,
            &|| obj,
            NEVER_DEFINED
        ));
    }

    /// The decided shapes answer the same in both forms.
    #[test]
    fn decided_shapes_agree_across_both_forms() {
        let Some((shared, obj)) = vm_with_object() else {
            return;
        };
        for may_load in [false, true] {
            // Every reference is an `Object`.
            assert_eq!(
                lambda_arg_cast_verdict(&shared, &|| obj, "Ljava/lang/Object;", may_load),
                Some(false)
            );
            // A non-array against an array type fails open.
            assert_eq!(
                lambda_arg_cast_verdict(&shared, &|| obj, "[Ljava/lang/String;", may_load),
                Some(false)
            );
            // A non-class token fails open.
            assert_eq!(
                lambda_arg_cast_verdict(&shared, &|| obj, "I", may_load),
                Some(false)
            );
        }
    }

    /// Structural: the JIT arm's registers are raw addresses nothing remaps,
    /// so it must ask the non-loading form, never the loading one.
    #[test]
    fn the_jit_site_checkcast_uses_the_non_loading_form() {
        let src = include_str!("lambda.rs");
        let start = src
            .find("pub(crate) fn lambda_jit_site_checkcasts_pass(")
            .unwrap_or(0);
        let rest = &src[start..];
        let end = rest
            .find("pub(crate) fn lambda_jit_site_capture_args(")
            .unwrap_or(0);
        assert!(start > 0 && end > 0, "the scanned function moved");
        let body = &rest[..end];
        assert!(body.contains("lambda_arg_cast_verdict(shared, &|| obj, inst_tok, false)"));
        assert!(!body.contains("lambda_arg_provably_not_instance("));
    }
}

#[cfg(test)]
mod i7_l6_bridge_descriptor_tests {
    use super::{lambda_accepts_descriptor, lambda_args_sam_compatible, ClassId, SharedVm, Value};
    use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};
    use std::sync::Arc;

    fn site(proxy: ClassId, iface: &str, name: &str, sam: &str) -> LambdaCallSite {
        LambdaCallSite {
            functional_interface: Arc::from(iface),
            functional_interface_id: None,
            sam_method_name: Arc::from(name),
            sam_descriptor: Arc::from(sam),
            impl_handle: MethodHandle {
                kind: MethodHandleKind::InvokeStatic,
                class_name: Arc::from("cratonvm/test/L6BridgeHost"),
                member_name: Arc::from("lambda$main$0"),
                descriptor: Arc::from(sam),
            },
            instantiated_descriptor: Arc::from(sam),
            capture_types: vec![],
            proxy_class_id: proxy,
            serializable_flag: false,
        }
    }

    /// The exact SAM descriptor and a recorded `FLAG_BRIDGES` descriptor enter
    /// the body; a same-named default overload, another name, and another
    /// proxy's bridge do not.
    #[test]
    fn a_recorded_bridge_is_accepted_beside_the_exact_sam_descriptor() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let proxy = shared.alloc_lambda_proxy_id();
        let other = shared.alloc_lambda_proxy_id();
        let cs = site(proxy, "cratonvm/test/StrM", "m", "()Ljava/lang/String;");
        let accepts = |id: ClassId, name: &str, desc: &str| {
            lambda_accepts_descriptor(&shared, id, &cs, name, desc)
        };
        assert!(accepts(proxy, "m", "()Ljava/lang/String;"));
        assert!(!accepts(proxy, "m", "()Ljava/lang/Object;"));
        crate::runtime::invokedynamic::record_lambda_proxy_bridges(
            shared.vm_identity,
            proxy,
            &[Arc::from("()Ljava/lang/Object;")],
        );
        assert!(accepts(proxy, "m", "()Ljava/lang/Object;"));
        // A same-named overload that is not a bridge (a857ee34c).
        assert!(!accepts(
            proxy,
            "m",
            "(Ljava/lang/Object;)Ljava/lang/String;"
        ));
        // The bridge belongs to the SAM name only.
        assert!(!accepts(proxy, "n", "()Ljava/lang/Object;"));
        // And to its own proxy only.
        assert!(!accepts(other, "m", "()Ljava/lang/Object;"));
        crate::runtime::invokedynamic::forget_vm_generic_indy_sites(shared.vm_identity);
    }

    /// The SAM compatibility screen is safepoint-free: a lambda-proxy argument
    /// whose functional interface would have to be LOADED to answer is
    /// compatible (undecidable), where it used to drive that load and answer
    /// "incompatible" from a raw address.
    #[test]
    fn the_sam_compatibility_screen_does_not_load_for_a_lambda_argument() {
        const NEVER: &str = "cratonvm/test/L6NeverLoadableIface";
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let Ok(string) = shared
            .classes
            .class_manager
            .write()
            .load_class("java/lang/String")
        else {
            return; // Stripped VM (no class library on the test classpath).
        };
        let proxy = shared.alloc_lambda_proxy_id();
        shared
            .classes
            .lambda_proxies
            .write()
            .insert(proxy, Arc::new(site(proxy, NEVER, "run", "()V")));
        let arg = shared.mem.heap.alloc_object(proxy, 0);
        if shared.mem.heap.class_id_of(arg) != proxy || string == ClassId::new(0) {
            return;
        }
        assert!(lambda_args_sam_compatible(
            &shared,
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(arg))]
        ));
        assert!(shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(NEVER)
            .is_none());
    }

    /// Structural: the three lambda-dispatch copies ask the one predicate, and
    /// the stale-receiver recovery holds no `lambda_proxies` guard across it.
    #[test]
    fn every_lambda_dispatch_copy_asks_the_shared_descriptor_predicate() {
        let lambda_src = include_str!("lambda.rs");
        let start = lambda_src
            .find("pub(crate) fn try_lambda_dispatch(")
            .unwrap_or(0);
        assert!(start > 0, "try_lambda_dispatch moved");
        let head = &lambda_src[start..];
        let head = &head[..head.find("// Read captured values").unwrap_or(0)];
        assert!(head.contains("lambda_accepts_descriptor("));

        let exec_src = include_str!("../../vm/vm_exec.rs").replace("\r\n", "\n");
        let start = exec_src
            .find("fn recover_stale_lambda_receiver_from_native_pins(")
            .unwrap_or(0);
        assert!(start > 0, "the stale-receiver recovery moved");
        let rest = &exec_src[start..];
        let recovery = &rest[..rest.find("\n\x7d\n").unwrap_or(0)];
        assert!(recovery.contains("lambda_accepts_descriptor("));
        assert!(!recovery.contains("lambda_proxies.read()"));
        assert!(!exec_src.contains("descriptor == &*lcs.sam_descriptor"));
        assert!(
            exec_src
                .matches("interpreter::lambda_accepts_descriptor(")
                .count()
                >= 2
        );
    }
}

/// Interpreter round i1 wave 50, lane L4: a lambda's non-wrapper return into a
/// primitive SAM return converts as `Number.<prim>Value()` does
/// (`unbox_lambda_return_via_supertype`, probe `L4W50LambdaReturnUnbox`).
#[cfg(test)]
mod i50_l4_return_unbox_tests {
    use super::*;

    #[test]
    fn number_values_narrow_and_widen_as_number_does() {
        assert_eq!(
            number_value_as('I', Value::Long(4_294_967_297)),
            Value::Int(1)
        );
        assert_eq!(number_value_as('I', Value::Double(3.9)), Value::Int(3));
        assert_eq!(number_value_as('B', Value::Int(300)), Value::Int(44));
        assert_eq!(number_value_as('S', Value::Long(70_000)), Value::Int(4_464));
        assert_eq!(number_value_as('J', Value::Int(-7)), Value::Long(-7));
        assert_eq!(number_value_as('J', Value::Float(f32::NAN)), Value::Long(0));
        assert_eq!(number_value_as('F', Value::Double(0.5)), Value::Float(0.5));
        assert_eq!(number_value_as('D', Value::Int(2)), Value::Double(2.0));
        assert_eq!(number_value_as('C', Value::Int(120)), Value::Int(120));
    }

    #[test]
    fn only_the_eight_wrappers_take_the_wrapper_read() {
        for w in [
            "Integer",
            "Long",
            "Short",
            "Byte",
            "Character",
            "Boolean",
            "Float",
            "Double",
        ] {
            assert!(is_wrapper_desc(&format!("Ljava/lang/{w};")), "{w}");
        }
        for other in [
            "Ljava/lang/Object;",
            "Ljava/lang/Number;",
            "Ljava/lang/String;",
            "I",
        ] {
            assert!(!is_wrapper_desc(other), "{other}");
        }
    }
}

/// nio21 wave 15 lane L5: the per-thread cast-pass memo
/// (`lambda_cast_pass_memo_enabled`) answers only the exact
/// `(vm, class, target)` triple it was given.
#[cfg(test)]
mod w15_l5_cast_pass_memo_tests {
    use super::*;

    #[test]
    fn cast_pass_memo_is_keyed_by_vm_class_and_target() {
        let vm = 0x5715_0001usize;
        let cid = ClassId::new(4242);
        assert!(!cast_pass_memo_hit_keyed(true, vm, cid, "java/lang/Integer"));
        cast_pass_memo_note_keyed(true, vm, cid, "java/lang/Integer");
        assert!(cast_pass_memo_hit_keyed(true, vm, cid, "java/lang/Integer"));
        assert!(!cast_pass_memo_hit_keyed(true, vm + 1, cid, "java/lang/Integer"));
        assert!(!cast_pass_memo_hit_keyed(
            true,
            vm,
            ClassId::new(4243),
            "java/lang/Integer"
        ));
        assert!(!cast_pass_memo_hit_keyed(true, vm, cid, "java/lang/Integes"));
        assert!(!cast_pass_memo_hit_keyed(true, vm, cid, "java/lang/Number"));
    }

    /// gci i14/sx: with the VM's switch off (`CRATONVM_LAMBDA_CAST_PASS_MEMO=0`
    /// read into `LambdaSwitches`) nothing is recorded and nothing answers,
    /// a row recorded while on included. Both arms run in one process: the
    /// switch is no longer a process latch.
    #[test]
    fn gci_i14sx_cast_pass_memo_off_records_and_answers_nothing() {
        let vm = 0x5715_0002usize;
        let cid = ClassId::new(4244);
        cast_pass_memo_note_keyed(false, vm, cid, "java/lang/Long");
        assert!(
            !cast_pass_memo_hit_keyed(true, vm, cid, "java/lang/Long"),
            "off records nothing"
        );
        cast_pass_memo_note_keyed(true, vm, cid, "java/lang/Long");
        assert!(cast_pass_memo_hit_keyed(true, vm, cid, "java/lang/Long"));
        assert!(
            !cast_pass_memo_hit_keyed(false, vm, cid, "java/lang/Long"),
            "off answers nothing"
        );
    }

    #[test]
    fn cast_pass_memo_index_stays_in_bounds() {
        for id in [0u32, 1, 77, u32::MAX - 1, u32::MAX] {
            for t in ["", "a", "java/lang/String", "java/util/List"] {
                assert!(cast_pass_memo_index(ClassId::new(id), t) < CAST_PASS_MEMO_ROWS);
            }
        }
    }
}

/// gci GC round wave i14, lane sx: the six lambda switches are read once per
/// VM into `SharedVm::lambda_switches` (they were process `OnceLock` latches),
/// so a VM built under a per-thread override sees it, and two VMs built under
/// different overrides answer differently in one process.
#[cfg(test)]
mod gci_i14sx_lambda_switch_tests {
    use super::*;

    const NAMES: [&str; 6] = [
        "CRATONVM_LAMBDA_CAST_PASS_MEMO",
        "CRATONVM_LAMBDA_IMPL_REFUSAL_MEMO",
        "CRATONVM_LAMBDA_IMPL_CODE_MEMO",
        "CRATONVM_LAMBDA_HANDLER_IMPL_ONESHOT",
        "CRATONVM_LAMBDA_SITE_ONESHOT",
        "CRATONVM_LAMBDA_DOOR_COMPILED",
    ];

    fn all(s: LambdaSwitches) -> [bool; 6] {
        [
            s.cast_pass_memo,
            s.impl_refusal_memo,
            s.impl_code_memo,
            s.handler_impl_oneshot,
            s.site_oneshot,
            s.door_compiled,
        ]
    }

    /// The value rule is `runtime_flag_default_on`'s: unset is ON, only the
    /// off words turn a switch off, and each field reads its own name.
    #[test]
    fn gci_i14sx_lambda_switches_follow_thread_overrides() {
        use cratonvm_types::flags::with_thread_overrides;
        let unset: Vec<(&str, Option<&str>)> = NAMES.iter().map(|n| (*n, None)).collect();
        let s = with_thread_overrides(&unset, LambdaSwitches::from_flags);
        assert_eq!(all(s), [true; 6], "default ON");
        for (i, name) in NAMES.iter().enumerate() {
            for word in ["0", "false", "off", "no"] {
                let mut edits = unset.clone();
                edits[i] = (*name, Some(word));
                let s = with_thread_overrides(&edits, LambdaSwitches::from_flags);
                let mut want = [true; 6];
                want[i] = false;
                assert_eq!(all(s), want, "{name}={word}");
            }
        }
    }

    /// Two VMs in one process, one built with every switch off: each keeps
    /// its own answer, and the readers answer from the VM they are given.
    #[test]
    fn gci_i14sx_lambda_switches_are_per_vm() {
        let off: Vec<(&str, Option<&str>)> = NAMES.iter().map(|n| (*n, Some("0"))).collect();
        let unset: Vec<(&str, Option<&str>)> = NAMES.iter().map(|n| (*n, None)).collect();
        let vm_off = cratonvm_types::flags::with_thread_overrides(&off, || {
            SharedVm::new(crate::config::VmConfig::default())
        });
        let vm_on = cratonvm_types::flags::with_thread_overrides(&unset, || {
            SharedVm::new(crate::config::VmConfig::default())
        });
        assert_eq!(all(vm_off.lambda_switches), [false; 6]);
        assert_eq!(all(vm_on.lambda_switches), [true; 6]);
        assert!(!lambda_cast_pass_memo_enabled(&vm_off));
        assert!(lambda_cast_pass_memo_enabled(&vm_on));
        assert!(!lambda_impl_refusal_memo_enabled(&vm_off));
        assert!(lambda_impl_code_memo_enabled(&vm_on));
        assert!(!lambda_handler_impl_oneshot_enabled(&vm_off));
        assert!(lambda_site_oneshot_enabled(&vm_on));
        // The memo answers through the VM's own switch.
        let cid = ClassId::new(4245);
        cast_pass_memo_note(&vm_on, cid, "java/lang/Short");
        assert!(cast_pass_memo_hit(&vm_on, cid, "java/lang/Short"));
        cast_pass_memo_note(&vm_off, cid, "java/lang/Short");
        assert!(!cast_pass_memo_hit(&vm_off, cid, "java/lang/Short"));
    }

    /// No process latch is left in the readers (the ratchet's count fell by
    /// six for these). Needles are assembled so this test does not match
    /// itself.
    #[test]
    fn gci_i14sx_lambda_switch_readers_have_no_process_latch() {
        let src = include_str!("lambda.rs");
        let latch = ["static ON: std::sync::", "OnceLock<bool>"].concat();
        assert!(!src.contains(&latch), "lambda.rs keeps a process OnceLock<bool> latch");
        let fast = include_str!("invoke_fast.rs");
        let door = ["fn lambda_door_compiled_enabled(", "shared: &SharedVm) -> bool"].concat();
        assert!(fast.contains(&door), "the door switch reads the VM");
    }
}

/// Interpreter round i6 wave 125, lane L6 (page `i125-L6`):
/// `CRATONVM_LAMBDA_OWNER_MEMO_ON_YIELD` is default OFF, read once per VM,
/// and its record sits on the yield arm, ahead of the `?` that leaves on it.
#[cfg(test)]
mod i125_l6_owner_memo_on_yield_tests {
    use super::*;

    #[test]
    fn i125_l6_owner_memo_on_yield_is_default_off_and_follows_a_truthy_word() {
        use cratonvm_types::flags::with_thread_overrides;
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some("false"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            let s = with_thread_overrides(
                &[("CRATONVM_LAMBDA_OWNER_MEMO_ON_YIELD", word)],
                LambdaSwitches::from_flags,
            );
            assert_eq!(s.owner_memo_on_yield, on, "{word:?}");
        }
    }

    #[test]
    fn i125_l6_the_yield_record_precedes_the_question_mark() {
        let src = include_str!("lambda.rs");
        let record = ["record_lambda_global_impl_owner_on_yield(", "shared, &call_site);"].concat();
        let question = ["let r = ", "r?;"].concat();
        let at_record = src.find(&record).expect("the yield arm records the owner");
        let at_question = src[at_record..].find(&question).map(|i| i + at_record);
        assert!(at_question.is_some(), "the `?` follows the yield arm's record");
    }
}

#[cfg(test)]
mod w16_l2_lambda_oneshot_tests {
    use super::*;

    /// The one-shot arms hand a compiled impl only arguments already in its
    /// parameters' representation; every mismatch is a coercion the general
    /// path owns, so it must answer `false`.
    #[test]
    fn value_fits_param_tag_accepts_only_the_verbatim_representation() {
        for tag in [b'I', b'Z', b'B', b'C', b'S'] {
            assert!(value_fits_param_tag(&Value::Int(7), tag));
            assert!(!value_fits_param_tag(&Value::Long(7), tag));
            assert!(!value_fits_param_tag(&Value::Object(None), tag));
        }
        assert!(value_fits_param_tag(&Value::Long(-1), b'J'));
        assert!(!value_fits_param_tag(&Value::Int(-1), b'J'));
        assert!(value_fits_param_tag(&Value::Float(1.5), b'F'));
        assert!(!value_fits_param_tag(&Value::Double(1.5), b'F'));
        assert!(value_fits_param_tag(&Value::Double(1.5), b'D'));
        assert!(!value_fits_param_tag(&Value::Float(1.5), b'D'));
        assert!(value_fits_param_tag(&Value::Object(None), b'L'));
        assert!(value_fits_param_tag(&Value::Object(None), b'['));
        assert!(!value_fits_param_tag(&Value::Int(0), b'L'));
        assert!(!value_fits_param_tag(&Value::Uninitialized, b'L'));
        assert!(!value_fits_param_tag(&Value::Int(0), b'V'));
    }

    /// The withdrawn-code sweep, now over three site caches, finds nothing to
    /// drop on a thread that has built no site and no impl row.
    #[test]
    fn empty_caches_release_nothing() {
        let key = (0x16_0002usize, 1u32, 2u32);
        LAMBDA_IMPL_BYTECODE_CACHE.with(|cache| assert!(cache.borrow().get(&key).is_none()));
        assert_eq!(release_withdrawn_lambda_code(), 0);
    }
}

/// JIT round 20 wave 12 (lane qLam, DISP20-3): the descriptor-only half of an
/// instance-method impl site.
#[cfg(test)]
mod qlam_instance_shape_tests {
    use super::{lambda_instance_shape, ClassId};
    use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};

    fn site(
        kind: MethodHandleKind,
        sam: &str,
        impl_desc: &str,
        inst: &str,
        captures: &[char],
    ) -> LambdaCallSite {
        LambdaCallSite {
            functional_interface: "java/util/function/Function".into(),
            functional_interface_id: None,
            sam_method_name: "apply".into(),
            sam_descriptor: sam.into(),
            impl_handle: MethodHandle {
                kind,
                class_name: "test/Box".into(),
                member_name: "m".into(),
                descriptor: impl_desc.into(),
            },
            instantiated_descriptor: inst.into(),
            capture_types: captures.to_vec(),
            proxy_class_id: ClassId::new(0x8000_0D01),
            serializable_flag: false,
        }
    }

    /// Unbound `Box::name` as a `Function<Box,String>`: the receiver is SAM
    /// argument 0, whose bridge cast is replayed; no captures.
    #[test]
    fn an_unbound_reference_is_receiver_first_with_its_cast() {
        let s = site(
            MethodHandleKind::InvokeVirtual,
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            "()Ljava/lang/String;",
            "(Ltest/Box;)Ljava/lang/String;",
            &[],
        );
        let shape = lambda_instance_shape(&s, false).expect("an instance shape");
        assert!(!shape.bound);
        assert_eq!(shape.num_captures, 0);
        assert_eq!(shape.total_args, 1);
        assert!(shape.capture_descs.is_empty());
        assert_eq!(shape.checkcasts.len(), 1);
        assert_eq!(shape.checkcasts[0].0, 0);
        assert_eq!(&*shape.checkcasts[0].1, "Ltest/Box;");
    }

    /// Bound `box::add` as a `Function<String,String>` through an interface
    /// handle: the receiver is capture 0 (a reference load), the SAM argument
    /// is the impl's parameter 0.
    #[test]
    fn a_bound_reference_takes_its_receiver_from_capture_zero() {
        let s = site(
            MethodHandleKind::InvokeInterface,
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            "(Ljava/lang/String;)Ljava/lang/String;",
            "(Ljava/lang/String;)Ljava/lang/String;",
            &['L'],
        );
        let shape = lambda_instance_shape(&s, false).expect("an instance shape");
        assert!(shape.bound);
        assert_eq!(shape.capture_descs, vec![b'L']);
        assert_eq!(shape.total_args, 2);
        assert_eq!(
            shape.checkcasts.len(),
            1,
            "the String cast of SAM argument 0"
        );
    }

    /// Never served here: a boxing return (`String::length` as a `Function`),
    /// an unboxing parameter, a `void` SAM over a valued impl, a static or a
    /// special handle, an arity the receiver-first layout does not explain.
    #[test]
    fn non_identity_and_non_virtual_shapes_are_refused() {
        let boxing = site(
            MethodHandleKind::InvokeVirtual,
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            "()I",
            "(Ljava/lang/String;)Ljava/lang/Integer;",
            &[],
        );
        assert!(lambda_instance_shape(&boxing, false).is_none());
        // With the boxing arm: `String::length` as a `Function` boxes its int.
        let shape = lambda_instance_shape(&boxing, true).expect("a boxing shape");
        assert_eq!(shape.box_ret, Some('I'));
        assert!(shape.unbox.is_empty());
        let unboxing = site(
            MethodHandleKind::InvokeVirtual,
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            "(I)Ljava/lang/String;",
            "(Ltest/Box;Ljava/lang/Integer;)Ljava/lang/String;",
            &[],
        );
        assert!(lambda_instance_shape(&unboxing, false).is_none());
        // With the boxing arm: SAM argument 1 is exactly an `Integer`, unboxed.
        let shape = lambda_instance_shape(&unboxing, true).expect("an unboxing shape");
        assert_eq!(shape.unbox, vec![(1, 'I')]);
        assert_eq!(
            shape.checkcasts.len(),
            1,
            "only the receiver's cast is replayed"
        );
        assert_eq!(shape.checkcasts[0].0, 0);
        let void_over_value = site(
            MethodHandleKind::InvokeVirtual,
            "(Ljava/lang/Object;)V",
            "(Ljava/lang/Object;)Z",
            "(Ljava/lang/Object;)V",
            &['L'],
        );
        assert!(lambda_instance_shape(&void_over_value, false).is_none());
        assert!(lambda_instance_shape(&void_over_value, true).is_none());
        for kind in [
            MethodHandleKind::InvokeStatic,
            MethodHandleKind::InvokeSpecial,
        ] {
            let s = site(
                kind,
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                "()Ljava/lang/String;",
                "(Ltest/Box;)Ljava/lang/String;",
                &[],
            );
            assert!(lambda_instance_shape(&s, false).is_none());
        }
        let arity = site(
            MethodHandleKind::InvokeVirtual,
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            "(Ljava/lang/String;)Ljava/lang/String;",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[],
        );
        assert!(lambda_instance_shape(&arity, false).is_none());
    }

    /// The boxing arm's two token rules: only LambdaMetafactory's EXACT box /
    /// unbox, and only 32-bit returns.
    #[test]
    fn the_box_and_unbox_token_rules_are_exact() {
        use super::{lambda_box_return_prim, lambda_unbox_param_prim};
        assert_eq!(lambda_box_return_prim("Ljava/lang/Object;", "I"), Some('I'));
        assert_eq!(
            lambda_box_return_prim("Ljava/lang/Integer;", "I"),
            Some('I')
        );
        assert_eq!(lambda_box_return_prim("Ljava/lang/Object;", "Z"), Some('Z'));
        assert_eq!(lambda_box_return_prim("Ljava/lang/Object;", "F"), Some('F'));
        assert_eq!(
            lambda_box_return_prim("Ljava/lang/Object;", "J"),
            None,
            "64-bit"
        );
        assert_eq!(
            lambda_box_return_prim("Ljava/lang/Object;", "D"),
            None,
            "64-bit"
        );
        assert_eq!(lambda_box_return_prim("Ljava/lang/Number;", "I"), None);
        assert_eq!(lambda_box_return_prim("Ljava/lang/Long;", "I"), None);
        assert_eq!(lambda_box_return_prim("V", "I"), None);
        assert_eq!(
            lambda_box_return_prim("Ljava/lang/Object;", "Ljava/lang/String;"),
            None
        );
        let int = Some("Ljava/lang/Integer;");
        assert_eq!(
            lambda_unbox_param_prim("Ljava/lang/Object;", "I", int),
            Some('I')
        );
        assert_eq!(
            lambda_unbox_param_prim("Ljava/lang/Integer;", "I", None),
            Some('I')
        );
        assert_eq!(
            lambda_unbox_param_prim("Ljava/lang/Object;", "J", int),
            None,
            "no widening after the unbox"
        );
        assert_eq!(
            lambda_unbox_param_prim("Ljava/lang/Object;", "I", Some("Ljava/lang/Number;")),
            None
        );
        assert_eq!(
            lambda_unbox_param_prim("Ljava/lang/Object;", "I", None),
            None
        );
        assert_eq!(
            lambda_unbox_param_prim("I", "I", None),
            None,
            "not a reference"
        );
        assert_eq!(
            lambda_unbox_param_prim("Ljava/lang/Object;", "D", Some("Ljava/lang/Double;")),
            Some('D')
        );
    }

    /// A primitive SAM argument never stands for the receiver.
    #[test]
    fn a_primitive_receiver_slot_is_refused() {
        let s = site(MethodHandleKind::InvokeVirtual, "(I)I", "()I", "(I)I", &[]);
        assert!(lambda_instance_shape(&s, false).is_none());
    }
}
