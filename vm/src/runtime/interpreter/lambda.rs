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
        // `lambda_arg_provably_not_instance` call can trigger a GC via its
        // proxy/annotation-satisfies helpers, which leaves any later
        // `args[idx]` read here pointing at a stale, already-evacuated
        // address. Read the CURRENT address back through the caller's pin
        // (`handles`) instead of the raw `args` slice. Confirmed live via
        // CRATONVM_DBG_STALE_OBJREF during WildFly parallel-extension-add --
        // see fixed-suite-bugs/wildfly/wildfly-parallel-boot-stale-objectref-residual.md.
        let idx = num_captures + sam_idx;
        let obj_ref = match handles.get(idx).copied().flatten() {
            Some(h) => thread.native_pin_roots[h],
            None => match args.get(idx) {
                Some(Value::Object(Some(o))) => *o,
                // null (a `checkcast` of null always succeeds), primitive, or missing.
                _ => continue,
            },
        };
        if lambda_arg_provably_not_instance(shared, obj_ref, inst_tok) {
            let obj_class_name = shared
                .classes
                .class_manager
                .read()
                .get_class(shared.mem.heap.class_id_of(obj_ref))
                .map(|c| c.name.to_string())
                // Name the id, not just "?" — see the matching note on the
                // `checkcast` opcode's fallback.
                .unwrap_or_else(|| format!("?class_id={}", shared.mem.heap.class_id_of(obj_ref)));
            let obj_display_name = cce_display_class_name(shared, obj_ref, &obj_class_name);
            let target_binary = inst_tok
                .strip_prefix('L')
                .and_then(|d| d.strip_suffix(';'))
                .unwrap_or(inst_tok)
                .replace('/', ".");
            // CRATONVM_DBG_CCE_BT: same attribution hook as the `checkcast`
            // opcode, plus whether this argument was read through the pinned
            // path (`via_pin`) — re-establishing the 2026-07-15 session's
            // temporary instrumentation permanently (that session measured
            // via_pin=true on every captured stale read here).
            if crate::runtime::interpreter::dbg_cce_bt_enabled() {
                let via_pin = handles.get(idx).copied().flatten().is_some();
                eprintln!(
                    "CRATONVM_DBG_CCE_BT: site=lambda_instantiated_args obj={} @0x{:x} target={} via_pin={via_pin}",
                    obj_display_name.replace('/', "."),
                    obj_ref.as_ptr() as usize,
                    target_binary
                );
            }
            // Same dotted-name shape as the `checkcast` opcode (tools such as
            // mockk's `JvmAutoHinter` parse this text).
            return Err(RuntimeError::ClassCastException {
                message: format!(
                    "{} cannot be cast to {}",
                    obj_display_name.replace('/', "."),
                    target_binary
                ),
            }
            .into());
        }
    }
    Ok(())
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
/// `aastore` twin — see `docs/known-issues/jdk-only/W8-E11-1-jit-aastore-third-twin-and-the-check-only-helper.md`.
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

/// `true` iff `obj_ref` is *provably* not an instance of the reference
/// descriptor `desc_tok` (`L...;` or `[...`). Fails open (returns `false`)
/// whenever the answer can't be established without risk — an unloaded target,
/// an array-vs-non-array shape we can't decide, or a non-class descriptor — so a
/// genuine instance is never rejected. Only consults already-loaded classes (no
/// class loading → no GC, no stale `obj_ref`).
pub(super) fn lambda_arg_provably_not_instance(
    shared: &SharedVm,
    obj_ref: ObjectRef,
    desc_tok: &str,
) -> bool {
    // Array instantiated type: decide via the array-assignability rules.
    if desc_tok.starts_with('[') {
        return match array_descriptor_of(shared, obj_ref) {
            Some(src) => !array_is_assignable_to(shared, &src, desc_tok),
            None => false, // not an array — fail open
        };
    }
    let target = match desc_tok.strip_prefix('L').and_then(|d| d.strip_suffix(';')) {
        Some(t) => t,
        None => return false,
    };
    if target == "java/lang/Object" {
        return false;
    }
    let obj_class_id = shared.mem.heap.class_id_of(obj_ref);
    // Lambda proxies use VM-only synthetic class IDs which intentionally do not
    // have ClassStore metadata.  Without a real class graph we cannot prove a
    // mismatch against the erased bridge parameter, so preserve this helper's
    // fail-open contract and let the normal lambda dispatch validate it.
    if shared
        .classes
        .class_manager
        .read()
        .get_class(obj_class_id)
        .is_none()
    {
        return false;
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
        // Resolve it, exactly as the bridge's `checkcast` would on first
        // execution. `load_class_concurrent` loads and links without running
        // `<clinit>`, which is the resolution a `checkcast` performs.
        None => match shared.load_class_concurrent(target) {
            Ok(tcid) => tcid,
            Err(_) => return false,
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
    if is_sub
        || loader_scoped_is_sub
        || lambda_proxy_satisfies(shared, obj_class_id, target_cid)
        || synthetic_implements(shared, obj_class_id, target)
        || proxy_instance_satisfies_target(shared, obj_ref, target)
        || annotation_proxy_satisfies_target(shared, obj_ref, target)
        || loader_aware_name_assignable(shared, obj_class_id, target_cid, target)
    {
        return false;
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
            return false;
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
    if obj_is_bare && (target_is_interface || target_used_as_interface(shared, target_cid)) {
        return false;
    }
    true
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
        let ch = sam_ret.chars().next().ok_or_else(|| {
            MethodCallFailed::InternalError(VmError::Internal {
                message: "empty primitive descriptor token in coerce_return".to_string(),
            })
        })?;
        return Ok(Some(unbox_wrapper(shared, ch, raw)));
    }
    // Both primitive: maybe widen.
    if is_primitive_desc(sam_ret) && is_primitive_desc(impl_ret) {
        return Ok(Some(widen_primitive(impl_ret, sam_ret, raw)));
    }
    Ok(Some(raw))
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
            || lambda_proxy_satisfies(shared, arg_cid, target_cid)
            || synthetic_implements(shared, arg_cid, target)
            || proxy_instance_satisfies_target(shared, arg, target)
            || annotation_proxy_satisfies_target(shared, arg, target)
        {
            continue;
        }
        return false; // arg provably not an instance of a concrete SAM param → overloaded default
    }
    true
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
    if !cratonvm_native_builtins::classloader::any_defining_loader_registered() {
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
    if !cratonvm_native_builtins::classloader::any_defining_loader_registered() {
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
    drive_defining_loader_load(shared, thread, host, name).filter(|cid| *cid != ClassId::new(0))
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
        .class_manager
        .write()
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
pub(crate) fn lambda_private_impl_dispatch_class(
    shared: &SharedVm,
    call_site: &crate::classloading::resolution::LambdaCallSite,
) -> Option<ClassId> {
    let owner_id = lambda_impl_dispatch_override(shared, call_site).or_else(|| {
        shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(&call_site.impl_handle.class_name)
    })?;
    let cm = shared.classes.class_manager.read();
    let (method, declaring_id) = crate::classloading::find_method_recursive(
        owner_id,
        &call_site.impl_handle.member_name,
        &call_site.impl_handle.descriptor,
        &cm.class_store,
    )?;
    method
        .access_flags
        .contains(MethodAccessFlags::PRIVATE)
        .then_some(declaring_id)
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
    let (interface_name, interface_id_hint) = {
        let proxies = shared.classes.lambda_proxies.read();
        match proxies.get(&obj_class_id) {
            Some(call_site) => (
                call_site.functional_interface.clone(),
                call_site.functional_interface_id,
            ),
            None => return Ok(None),
        }
    };

    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_LAMBDA_DISPATCH").is_some() {
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
/// See `feature-designs/vm-process-global-state-round-2.md`.
type VmScopedClassPairKey = (usize, u32, u32);

thread_local! {
    pub(super) static LAMBDA_IMPL_BYTECODE_CACHE: std::cell::RefCell<
        rustc_hash::FxHashMap<VmScopedClassPairKey, (Arc<CachedBytecodeMethod>, RedefineGate)>
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

// TDigest's private numeric kernels adapt a captured `TDigestDoubleArray.get(int)`
// through `Function<Integer, Double>`. Cache only getters whose entire body is
// the verifier-safe `aload_0; getfield [D; iload_1; daload; dreturn` shape.
// This is an interpreter superinstruction, not a semantic shortcut: any other
// lambda/accessor continues through the normal dispatch path.
thread_local! {
    pub(super) static TDIGEST_DOUBLE_GET_FIELD_CACHE: std::cell::RefCell<
        rustc_hash::FxHashMap<VmScopedClassPairKey, (usize, RedefineGate)>
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

pub(super) fn try_tdigest_lambda_double_get(
    shared: &SharedVm,
    proxy: ObjectRef,
    index: i32,
) -> Option<f64> {
    let proxy_class_id = shared.mem.heap.class_id_of(proxy);
    let call_site = shared
        .classes
        .lambda_proxies
        .read()
        .get(&proxy_class_id)
        .cloned()?;
    if call_site.functional_interface.as_ref() != "java/util/function/Function"
        || call_site.sam_method_name.as_ref() != "apply"
        || call_site.sam_descriptor.as_ref() != "(Ljava/lang/Object;)Ljava/lang/Object;"
        || call_site.capture_types.len() != 1
        || call_site.impl_handle.member_name.as_ref() != "get"
        || call_site.impl_handle.descriptor.as_ref() != "(I)D"
        || !matches!(
            call_site.impl_handle.kind,
            MethodHandleKind::InvokeVirtual | MethodHandleKind::InvokeInterface
        )
    {
        return None;
    }
    let receiver = match shared.mem.heap.get_field(proxy, 0) {
        Value::Object(Some(receiver)) => receiver,
        _ => return None,
    };
    let receiver_class_id = shared.mem.heap.class_id_of(receiver);
    let key = (
        shared.vm_identity,
        proxy_class_id.as_u32(),
        receiver_class_id.as_u32(),
    );
    let field_index = TDIGEST_DOUBLE_GET_FIELD_CACHE
        .with(|cache| {
            let mut cache = cache.borrow_mut();
            match cache.get(&key) {
                Some((field_index, gate)) if !gate.is_stale() => Some(*field_index),
                Some(_) => {
                    cache.remove(&key);
                    None
                }
                None => None,
            }
        })
        .or_else(|| {
            let (declaring_id, field_cp_index, gate) = {
                let cm = shared.classes.class_manager.read();
                let (method, declaring_id) = crate::classloading::find_method_recursive(
                    receiver_class_id,
                    "get",
                    "(I)D",
                    &cm.class_store,
                )?;
                let code = method.code()?;
                if code.code.len() != 7
                    || code.code[0] != 0x2a
                    || code.code[1] != 0xb4
                    || code.code[4] != 0x1b
                    || code.code[5] != 0x31
                    || code.code[6] != 0xaf
                {
                    return None;
                }
                (
                    declaring_id,
                    ((code.code[2] as u16) << 8) | code.code[3] as u16,
                    RedefineGate::snapshot(cm.class_redefine_generation_handle(declaring_id)),
                )
            };
            let field = resolve_field_ref(shared, declaring_id, field_cp_index).ok()?;
            if field.is_static || field.desc_byte != b'[' {
                return None;
            }
            TDIGEST_DOUBLE_GET_FIELD_CACHE.with(|cache| {
                cache.borrow_mut().insert(key, (field.field_index, gate));
            });
            Some(field.field_index)
        })?;
    if index < 0 {
        return None;
    }
    let array = match shared.mem.heap.get_field(receiver, field_index) {
        Value::Object(Some(array)) => array,
        _ => return None,
    };
    match shared
        .mem
        .heap
        .get_array_element(array, index as usize)
        .ok()?
    {
        Value::Double(value) => Some(value),
        _ => None,
    }
}

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
    /// The impl method, for the JIT-cache probe and the redefinition gate.
    cached: Arc<CachedBytecodeMethod>,
    gate: RedefineGate,
    /// The compiled impl body, re-probed only when `jit_cache_generation()`
    /// moves. `None` at the current generation means "not compiled yet" — the
    /// warmup counter in `try_invoke_cached_lambda_impl` is what eventually
    /// changes that, which is why that counter and this fast path are one
    /// feature under one kill switch.
    code: std::cell::RefCell<Option<cratonvm_jit::RetainedCode>>,
    code_generation: std::cell::Cell<u64>,
    /// Latched once this site has an inline-cache thunk.
    ///
    /// Without it the Rust arm re-writes the cache slot on every call it still
    /// serves — measured at 202 000 "installs" for what should be a handful —
    /// which is not merely wasted work: the slot's first cache line is the one
    /// the emitted cascade loads on every dispatch from every thread, and
    /// storing to it repeatedly is how a fast path pays for its own existence.
    adapter_installed: std::cell::Cell<bool>,
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
}

impl LambdaJitSite {
    /// Does this site answer THIS call? Name and descriptor both, never one.
    pub(crate) fn serves(&self, method_name: &str, descriptor: &str) -> bool {
        &*self.sam_method_name == method_name && &*self.sam_descriptor == descriptor
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

    /// Claim the one-time inline-cache install for this site, returning `true`
    /// exactly once. See [`LambdaJitSite::adapter_installed`].
    pub(crate) fn claim_adapter_install(&self) -> bool {
        !self.adapter_installed.replace(true)
    }

    /// Does this site's SAM call need a `checkcast` replayed per call? A
    /// hand-emitted thunk cannot ask a class-hierarchy question.
    pub(crate) fn has_checkcasts(&self) -> bool {
        !self.checkcasts.is_empty()
    }

    /// Is the implementation a static method — i.e. does dropping the proxy
    /// receiver lose nothing? Always true for a non-capturing javac lambda, and
    /// asserted rather than assumed because the thunk drops it outright.
    pub(crate) fn is_static_impl(&self) -> bool {
        self.cached.is_static
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
    /// [`jit_bridge::resume_deopted_body`](super::resume_deopted_body) keys
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
    static LAMBDA_JIT_SITE_CACHE: std::cell::RefCell<
        rustc_hash::FxHashMap<(usize, u32), Option<std::rc::Rc<LambdaJitSite>>>,
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
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
    let key = (shared.vm_identity, proxy_class_id.as_u32());
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
    match build_lambda_jit_site(shared, proxy_class_id) {
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

/// The shape analysis, run once per proxy class per thread.
///
/// Every `Never` here is a shape whose dispatch is not "read the captures, call
/// the impl" — a boxing SAM, a generic call site needing the `checkcast` the
/// synthetic bridge would have performed, an instance-method reference, a body
/// with its own exception table. Those keep the generic path, unchanged.
fn build_lambda_jit_site(shared: &SharedVm, proxy_class_id: ClassId) -> SiteVerdict {
    let Some(call_site) = shared
        .classes
        .lambda_proxies
        .read()
        .get(&proxy_class_id)
        .cloned()
    else {
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
    if !identity(sam_ret, impl_ret) {
        return SiteVerdict::Never;
    }
    let num_captures = call_site.capture_types.len();
    if impl_params.len() != num_captures + sam_params.len() {
        return SiteVerdict::Never;
    }
    for (k, sam_tok) in sam_params.iter().enumerate() {
        if !identity(sam_tok, impl_params[num_captures + k]) {
            return SiteVerdict::Never;
        }
    }
    // What `checkcast_lambda_instantiated_args` would check, decided once:
    // a reference instantiated token that differs from the erased SAM token.
    // Its own loop skips every other case.
    let (inst_params, _inst_ret) = split_method_descriptor_ref(&call_site.instantiated_descriptor);
    let mut checkcasts: Vec<(usize, Arc<str>)> = Vec::new();
    for (k, inst_tok) in inst_params.iter().enumerate() {
        if !is_reference_desc(inst_tok) {
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
    if !cached.exception_table.is_empty() || cached.is_synchronized || !cached.is_static {
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
    // Read off the impl BEFORE it is moved into the site: a property of the
    // method, decided once.
    let const_return = const_int_return_of(&cached.code, !cached.exception_table.is_empty());
    if crate::runtime::env_cache::jit_lambda_const_probe() {
        // One line per SITE, not per call: a handful for a whole run. It is
        // what tells you whether the impl you care about is even reachable by
        // this probe -- a `site_const_screened=0` means nothing without it,
        // because a body this machinery never serves cannot be screened.
        eprintln!(
            "[cratonvm-lambda-const] site impl={}.{}{} const_return={const_return:?} \n             code_len={} code={:02x?}",
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
        cached,
        gate,
        code: std::cell::RefCell::new(None),
        code_generation: std::cell::Cell::new(u64::MAX),
        adapter_installed: std::cell::Cell::new(false),
        direct_disabled: std::cell::Cell::new(false),
        const_return,
    }))
}

/// The compiled impl body for this site, or `None` while it is still
/// interpreted.
///
/// Epoch-guarded like every other JIT-cache probe in this VM: the string-keyed
/// `JitCache::get` — a hash, an `ArcSwap` load and a `memcmp`, together 5.6% of
/// a lambda-shape profile when it ran per invocation — is skipped entirely
/// while this site's snapshot of `jit_cache_generation()` is still current.
pub(crate) fn lambda_jit_site_code(
    shared: &SharedVm,
    site: &LambdaJitSite,
) -> Option<cratonvm_jit::RetainedCode> {
    let generation = cratonvm_jit::jit_cache_generation();
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
        site.adapter_installed.set(false);
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
    for (sam_idx, inst_tok) in &site.checkcasts {
        let Some(raw) = sam_args.get(*sam_idx).copied() else {
            continue;
        };
        if raw == 0 {
            continue;
        }
        // SAFETY: a non-zero reference register is a live object pointer — the
        // same assumption every other raw-argument arm in the JIT bridge makes.
        let obj = unsafe { ObjectRef::from_raw(raw as *mut u8) };
        if lambda_arg_provably_not_instance(shared, obj, inst_tok) {
            return false;
        }
    }
    true
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

/// `(resumed, unresumable)` — the direct arm's deopt outcome split.
///
/// `unresumable` is the number of SAM calls whose compiled body trapped and was
/// then re-executed from entry by the generic path, side effects and all. It is
/// the metric a regression test asserts is zero.
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
    // `jdk-only/a-bound-method-reference-is-a-different-dispatch-door-20260828.md`
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
    if native_shadows_receiver || native_on_declaring {
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
    if method.is_synchronized() || method.is_native() {
        return None;
    }
    let Some(code_attr) = method.code() else {
        return None;
    };
    let is_static = method.is_static();
    let c = Arc::new(CachedBytecodeMethod {
        declaring_class_id: declaring_id,
        class_name: Arc::clone(&class.name),
        method_name: Arc::from(method_name),
        method_descriptor: Arc::from(descriptor),
        source_file: class.source_file.as_deref().map(Arc::from),
        code: crate::runtime::frame::padded_bytecode(&code_attr.code),
        exception_table: Arc::from(code_attr.exception_table.as_slice()),
        max_stack: code_attr.max_stack,
        max_locals: code_attr.max_locals,
        num_params: count_method_params(descriptor) as u16,
        is_synchronized: false,
        is_static,
        force_native_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    });
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
/// snapshot of `jit_cache_generation()` is current, nothing has been published
/// or invalidated since the last miss, so the string-keyed `JitCache::get` is
/// skipped. The invocation is still COUNTED in that case, or the method could
/// never reach the threshold that makes re-probing worthwhile.
pub(crate) fn bytecode_callee_compiled_or_nominate(
    shared: &SharedVm,
    thread: &JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
) -> bool {
    if crate::runtime::env_cache::disable_jit() {
        return false;
    }
    let jit_generation = cratonvm_jit::jit_cache_generation();
    if !cached.jit_probe_is_current(jit_generation) {
        let found = shared.jit.jit_cache.read().get(
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            cached.declaring_class_id,
        );
        if found.is_some() {
            return true;
        }
        cached.record_jit_probe_miss(jit_generation);
    }
    // A virtual thread is excluded from NOMINATION for the same reason the
    // lambda twin excludes it from compiled ENTRY: compiled code carries none
    // of the unmount points the interpreter path does.
    if matches!(thread.kind, crate::threading::ThreadKind::Virtual) {
        return false;
    }
    const JIT_RETRY_STRIDE: u32 = 64;
    let threshold = crate::runtime::env_cache::jit_invocation_threshold();
    let cnt = shared
        .jit
        .profile_store
        .increment_invocation(cached.invoc_key());
    let should_attempt =
        cnt >= threshold && (cnt == threshold || (cnt - threshold) % JIT_RETRY_STRIDE == 0);
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
        let tiered_key = crate::jit::tiered::MethodKey::new(
            cached.class_name.as_ref(),
            cached.method_name.as_ref(),
            cached.method_descriptor.as_ref(),
        );
        // The REAL invocation count, not the stride boundary — see the
        // invokestatic twin, where stride-boundary `+= 1` counting deflated the
        // manager's hotness view 64x.
        let recommended_tier = shared
            .jit
            .tiered_manager
            .on_method_invocation_observed(&tiered_key, cnt as u64);
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] bc-callee-tiered-enqueue {}.{}{} tier={recommended_tier:?} invoc_count={cnt}",
                cached.class_name, cached.method_name, cached.method_descriptor,
            );
        }
    }
    false
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
    let cached = LAMBDA_IMPL_BYTECODE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        match cache.get(&key) {
            Some((cached, gate)) if !gate.is_stale() => Some(Arc::clone(cached)),
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
            let Some((c, gate)) =
                build_lambda_impl_cached(shared, receiver_class_id, method_name, descriptor)
            else {
                return Ok(None);
            };
            LAMBDA_IMPL_BYTECODE_CACHE.with(|cache| {
                cache.borrow_mut().insert(key, (Arc::clone(&c), gate));
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
    if crate::runtime::env_cache::jit_lambda_tierup()
        && !crate::runtime::env_cache::disable_jit()
        && !matches!(thread.kind, crate::threading::ThreadKind::Virtual)
        && !cached.is_synchronized
        && cached.exception_table.is_empty()
    {
        lambda_jit::bump(&lambda_jit::ELIGIBLE);
        // Epoch-guarded exactly like the twins: skip the string-keyed
        // `JitCache::get` while this entry's snapshot of
        // `jit_cache_generation()` is still current, because no publication or
        // invalidation has happened since the probe that missed. Read the
        // generation BEFORE probing so a racing publication can only cause a
        // redundant re-probe, never a missed one.
        let jit_generation = cratonvm_jit::jit_cache_generation();
        let compiled = if cached.jit_probe_is_current(jit_generation) {
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
            }
            found
        };
        match compiled {
            Some(compiled) => {
                lambda_jit::bump(&lambda_jit::COMPILED_HITS);
                if let Some(value) =
                    execute_jit_call_oneshot(shared, thread, &compiled, &cached, args)?
                {
                    lambda_jit::bump(&lambda_jit::FAST_RETURNS);
                    lambda_jit::maybe_report();
                    screen_const_return_value(&cached, value.as_ref(), "interp-oneshot");
                    return Ok(Some(value));
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
                const JIT_RETRY_STRIDE: u32 = 64;
                let invoc_key = cached.invoc_key();
                let threshold = crate::runtime::env_cache::jit_invocation_threshold();
                let cnt = shared.jit.profile_store.increment_invocation(invoc_key);
                let should_attempt = cnt >= threshold
                    && (cnt == threshold || (cnt - threshold) % JIT_RETRY_STRIDE == 0);
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
                    let tiered_key = crate::jit::tiered::MethodKey::new(
                        cached.class_name.as_ref(),
                        cached.method_name.as_ref(),
                        cached.method_descriptor.as_ref(),
                    );
                    // Real invocation count — see the invokestatic twin:
                    // stride-boundary `+= 1` counting deflated the manager's
                    // hotness view 64x.
                    let recommended_tier = shared
                        .jit
                        .tiered_manager
                        .on_method_invocation_observed(&tiered_key, cnt as u64);
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
    let frame = Frame::new_pooled_cached(
        cached,
        args,
        &mut thread.locals_pool,
        &mut thread.stacks_pool,
    );
    let out = execute_prebuilt_frame(shared, thread, frame).map(Some);
    // The INTERPRETED arm, and the one that matters most for a constant body:
    // a two-byte `iconst_1; ireturn` may never be nominated for compilation at
    // all, so the two compiled screens above would never see it.
    if let (Some(impl_method), Ok(Some(value))) = (&const_screen_impl, &out) {
        screen_const_return_value(impl_method, value.as_ref(), "interp-frame");
    }
    out
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

pub(crate) fn try_lambda_dispatch(
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

    // Look up the lambda proxy metadata for this ClassId.
    let lookup_start = prof.then(std::time::Instant::now);
    let call_site = {
        let proxies = shared.classes.lambda_proxies.read();
        match proxies.get(&obj_class_id) {
            Some(lcs) => lcs.clone(),
            None => return Ok(None), // Not a lambda proxy
        }
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
    // exact SAM descriptor may enter the lambda body; any other descriptor must
    // fall through to ordinary interface/default-method dispatch.
    if method_name == &*call_site.sam_method_name && method_descriptor != &*call_site.sam_descriptor
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
                            crate::vm::invoke_on_class_shared_no_retarget(
                                shared,
                                thread,
                                impl_cid,
                                &lcs.impl_handle.member_name,
                                &lcs.impl_handle.descriptor,
                                &full_args,
                            )?
                        } else {
                            invoke_shared(
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
                        if full_args.is_empty() {
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
                        let rcv_id_opt = match &full_args[0] {
                            Value::Object(Some(r)) => Some(shared.mem.heap.class_id_of(*r)),
                            _ => None,
                        };
                        let receiver_class = match rcv_id_opt {
                            Some(rcv) => shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(rcv)
                                .map(|c| c.name.to_string())
                                .unwrap_or_else(|| lcs.impl_handle.class_name.to_string()),
                            None => lcs.impl_handle.class_name.to_string(),
                        };
                        // Loader-faithful (gated): dispatch on the receiver's exact
                        // class_id when it diverges from the by-name global copy.
                        let vov = if crate::runtime::env_cache::loader_aware_resolution() {
                            rcv_id_opt.filter(|rcv| {
                                *rcv != ClassId::new(0)
                                    && !shared.classes.lambda_proxies.read().contains_key(rcv)
                                    && {
                                        let cm = shared.classes.class_manager.read();
                                        cm.get_class(*rcv)
                                            .map(|c| &*c.name == receiver_class.as_str())
                                            .unwrap_or(false)
                                            && cm.get_loaded_class_id(&receiver_class) != Some(*rcv)
                                    }
                            })
                        } else {
                            None
                        };
                        if let Some(rcv_cid) = vov {
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

        // Build full args with receiver prepended.
        let mut full_args = Vec::with_capacity(1 + call_args.len());
        full_args.push(Value::Object(Some(obj_ref)));
        full_args.extend_from_slice(call_args);

        // Try common descriptor patterns for default methods.
        let iface = &call_site.functional_interface;
        let descriptors = [
            format!("(L{iface};)L{iface};"), // andThen/compose/and/or
            format!("()L{iface};"),          // negate/identity
        ];
        for desc in &descriptors {
            if let Some(callback) = shared.natives.native_methods.find(iface, method_name, desc) {
                let mut ctx = crate::vm::NativeContextImpl { shared, thread };
                // Widening: small integer index -> usize (non-negative, fits in pointer width)
                let _ring_idx = cratonvm_native_api::native_ring::record_enter(callback as usize);
                let result = callback(&mut ctx, &full_args);
                cratonvm_native_api::native_ring::record_exit(_ring_idx);
                let result = result?;
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
    let sam_desc = call_site.sam_descriptor.clone();
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
            let result = if let Some(impl_cid) =
                lambda_impl_dispatch_override_driven(shared, thread, &call_site)
            {
                // Loader-faithful: the enclosing class was defined by a user
                // loader whose copy of the impl owner diverges from the global
                // one; dispatch on the exact loader-local class (static → no
                // receiver retarget).
                crate::vm::invoke_on_class_shared_no_retarget(
                    shared,
                    thread,
                    impl_cid,
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
                let r = invoke_shared(
                    shared,
                    thread,
                    &call_site.impl_handle.class_name,
                    &call_site.impl_handle.member_name,
                    &call_site.impl_handle.descriptor,
                    &full_args,
                )?;
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
            if full_args.is_empty() {
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
            if let Value::Object(Some(r)) = &full_args[0] {
                let rcv_class_id = shared.mem.heap.class_id_of(*r);
                let recv_is_lambda = shared
                    .classes
                    .lambda_proxies
                    .read()
                    .contains_key(&rcv_class_id);
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
            // Resolve the actual class of the receiver for virtual dispatch.
            let recv_class_id_opt = match &full_args[0] {
                Value::Object(Some(r)) => Some(shared.mem.heap.class_id_of(*r)),
                _ => None,
            };
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
            let receiver_class = match recv_class_id_opt {
                Some(rcv_class_id) => shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(rcv_class_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_else(|| call_site.impl_handle.class_name.to_string()),
                None => call_site.impl_handle.class_name.to_string(),
            };
            // Loader-faithful (gated): when the receiver's runtime class diverges
            // from the by-name global resolution — a per-loader (e.g. bytecode-
            // enhanced) copy — dispatch on the receiver's EXACT class_id so the
            // lambda body runs that loader's copy. `invoke_or_native` otherwise
            // collapses `receiver_class` (a name) to the global copy, running the
            // un-enhanced lambda body (`new Country` → un-enhanced entity →
            // reflective `Field.set` mismatch). Mirrors the ordinary
            // invoke_virtual divergence override.
            let virtual_override = if crate::runtime::env_cache::loader_aware_resolution() {
                recv_class_id_opt.filter(|rcv| {
                    *rcv != ClassId::new(0)
                        && !shared.classes.lambda_proxies.read().contains_key(rcv)
                        && {
                            let cm = shared.classes.class_manager.read();
                            // Only when `receiver_class` truly names the receiver's
                            // own real class (not the impl-handle fallback for a
                            // lambda-proxy / generic-ClassId receiver) AND that name
                            // globally resolves to a DIFFERENT (per-loader) copy.
                            cm.get_class(*rcv)
                                .map(|c| &*c.name == receiver_class.as_str())
                                .unwrap_or(false)
                                && cm.get_loaded_class_id(&receiver_class) != Some(*rcv)
                        }
                })
            } else {
                None
            };
            // A private instance lambda body is encoded by javac as an
            // InvokeVirtual handle, but it retains invokespecial semantics:
            // resolve it on the handle's declaring class, not by walking the
            // captured receiver's hierarchy. Synthetic lambda names are not
            // unique across a hierarchy (for example both Spring Data's
            // AnnotationBasedPersistentProperty and AbstractPersistentProperty
            // have lambda$new$2), so receiver-based lookup can execute a
            // different private body with the same name and descriptor.
            let impl_owner_id = lambda_impl_dispatch_override(shared, &call_site).or_else(|| {
                shared
                    .classes
                    .class_manager
                    .read()
                    .get_loaded_class_id(&call_site.impl_handle.class_name)
            });
            let private_impl_owner = impl_owner_id.filter(|owner_id| {
                shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(*owner_id)
                    .and_then(|class| {
                        class.find_method(
                            &call_site.impl_handle.member_name,
                            &call_site.impl_handle.descriptor,
                        )
                    })
                    .is_some_and(|method| method.access_flags.contains(MethodAccessFlags::PRIVATE))
            });
            // Keep the existing loader-faithful private implementation owner
            // when it is available; otherwise use the declaring owner found
            // above for private synthetic lambda methods.
            let exact_impl_owner = private_impl_class.or(private_impl_owner);
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
            let receiver_native = if exact_impl_owner.is_none() {
                recv_class_id_opt
                    .filter(|rcv| *rcv != ClassId::new(0))
                    .filter(|rcv| {
                        let cm = shared.classes.class_manager.read();
                        cm.get_class(*rcv)
                            .map(|c| {
                                c.find_method(
                                    &call_site.impl_handle.member_name,
                                    &call_site.impl_handle.descriptor,
                                )
                                .is_none()
                            })
                            .unwrap_or(false)
                    })
                    .and_then(|_| {
                        shared.natives.native_methods.find(
                            &receiver_class,
                            &call_site.impl_handle.member_name,
                            &call_site.impl_handle.descriptor,
                        )
                    })
            } else {
                None
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
                    && !shared.classes.lambda_proxies.read().contains_key(rcv)
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
                ))) if receiver_class.as_str() != &*call_site.impl_handle.class_name
                    && nsme_class.as_str() == receiver_class.as_str()
                    && nsme_method.as_str() == &*call_site.impl_handle.member_name =>
                {
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
            let class_id = lambda_impl_owner_class_id(shared, thread, &call_site)?;
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
            // Residual 4 (2026-07-20, fixed-suite-bugs/springboot/
            // core-spring-boot-test-config-data-and-classpath-scan-cluster-FIXED.md):
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
            let class_id = lambda_impl_owner_class_id(shared, thread, &call_site)?;
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
                maybe_gc(shared, thread);
                return Ok(Some(Some(Value::Object(Some(arr)))));
            }
            ensure_class_initialized_shared(shared, thread, class_id)?;
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
            let new_obj = gc_alloc_object(shared, thread, class_id, num_fields)?;
            // Build <init> args: [new_obj, ...full_args]. The constructor body
            // can allocate and trigger a moving GC; the Java frame/locals are
            // remapped, but this Rust local `new_obj` is not. Pin the receiver
            // across `<init>` and return the forwarded object ref, mirroring the
            // MethodHandle `newInvokeSpecial` path in `vm_exec.rs`.
            let new_obj_pin = thread.native_pin_roots.len();
            thread.native_pin_roots.push(new_obj);
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
            let forwarded = thread
                .native_pin_roots
                .get(new_obj_pin)
                .copied()
                .unwrap_or(new_obj);
            thread.native_pin_roots.truncate(new_obj_pin);
            init_result?;
            Ok(Some(Some(Value::Object(Some(forwarded)))))
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
