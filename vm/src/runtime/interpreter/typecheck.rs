// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Type compatibility for `checkcast` / `instanceof` / `aastore`.
//!
//! Moved verbatim out of `interpreter.rs`'s `Helper: lambda proxy type-check for checkcast/instanceof`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.

use super::*;

/// Public re-export for JIT helpers — see [`lambda_proxy_satisfies`].
pub fn lambda_proxy_satisfies_public(
    shared: &SharedVm,
    obj_class_id: ClassId,
    target_class_id: ClassId,
) -> bool {
    lambda_proxy_satisfies(shared, obj_class_id, target_class_id)
}

/// Public re-export for JIT helpers — see [`synthetic_implements`].
pub fn synthetic_implements_public(
    shared: &SharedVm,
    obj_class_id: ClassId,
    target_class_name: &str,
) -> bool {
    synthetic_implements(shared, obj_class_id, target_class_name)
}

/// Instance-aware `instanceof` / `checkcast` admission for an annotation proxy.
///
/// Every annotation proxy shares the synthetic
/// `java/lang/annotation/AnnotationProxy` ClassId, so the class-only
/// `synthetic_implements` path can only affirm the generic
/// `java.lang.annotation.Annotation` supertype. The proxy's *actual* annotation
/// interface lives on the heap object (slot 0 = the `Lpkg/Type;` descriptor),
/// so a precise check requires the instance. Returns `true` iff `obj_ref` is an
/// annotation proxy and `target_class_name` is `java/lang/annotation/Annotation`
/// or the proxy's own annotation interface. This replaces the old blanket
/// "proxy is instanceof every annotation" behaviour that broke Log4j2 plugin
/// injection (a `@Required` proxy passed `instanceof @PluginBuilderAttribute`).
pub fn annotation_proxy_satisfies_target(
    shared: &SharedVm,
    obj_ref: ObjectRef,
    target_class_name: &str,
) -> bool {
    let cid = shared.mem.heap.class_id_of(obj_ref);
    let is_proxy = shared
        .classes
        .class_manager
        .read()
        .get_class(cid)
        .map(|c| &*c.name == "java/lang/annotation/AnnotationProxy")
        .unwrap_or(false);
    if !is_proxy {
        return false;
    }
    if target_class_name == "java/lang/annotation/Annotation" {
        return true;
    }
    // Slot 0 holds the annotation's type descriptor, e.g. `Lpkg/Type;`.
    if let Value::Object(Some(desc_ref)) = shared.mem.heap.get_field(obj_ref, 0) {
        if let Some(desc) = read_java_string(&shared.mem.heap, desc_ref) {
            let internal = desc
                .strip_prefix('L')
                .and_then(|d| d.strip_suffix(';'))
                .unwrap_or(&desc);
            return internal == target_class_name;
        }
    }
    false
}

/// `instanceof` / `checkcast` admission for a `Proxy$Instance` heap object.
///
/// Returns `true` iff `obj_ref` is a dynamic proxy AND `target_class_name`
/// names one of the interfaces the proxy was created with (or a superinterface
/// of one of them). Always-implicit constants (`java/io/Serializable`,
/// `java/lang/Object`) also match per `java.lang.reflect.Proxy` spec.
///
/// The proxy stores its interfaces array at slot
/// [`crate::runtime::proxy::PROXY_FIELD_INTERFACES`] (a `Class[]`).
///
/// See also: [`synthetic_implements`] above for the rationale this lives at
/// the call site (per-instance proxy admission can't be decided from a
/// `class_id` alone, since every proxy lands on the same `Proxy$Instance`
/// ClassId).
pub(crate) fn proxy_instance_satisfies_target(
    shared: &SharedVm,
    obj_ref: cratonvm_types::ObjectRef,
    target_class_name: &str,
) -> bool {
    use crate::runtime::proxy::PROXY_FIELD_INTERFACES;

    let obj_class_id = shared.mem.heap.class_id_of(obj_ref);
    let obj_name = match shared.classes.class_manager.read().get_class(obj_class_id) {
        Some(c) => c.name.to_string(),
        None => return false,
    };
    let is_proxy = &*obj_name == "java/lang/reflect/Proxy$Instance"
        || class_chain_reaches_proxy_instance(shared, obj_class_id);
    if !is_proxy {
        return false;
    }

    // Always-true targets per the `Proxy` contract.
    if target_class_name == "java/io/Serializable" || target_class_name == "java/lang/Object" {
        return true;
    }

    // A real-super generated `$ProxyN` has just the inherited handler field
    // at slot 0; its interfaces are declared on the class. Do not probe the
    // synthetic interfaces slot on it: that is out of bounds and this helper
    // is hot in Spring's conversion/binding path.
    let proxy_cid = shared.mem.heap.class_id_of(obj_ref);
    let has_iface_slot = shared
        .classes
        .class_manager
        .read()
        .get_class(proxy_cid)
        .map(|c| c.num_total_fields >= 2)
        .unwrap_or(false);
    if !has_iface_slot {
        return obj_name == "java/lang/reflect/Proxy$Instance";
    }

    let interfaces_arr = match shared.mem.heap.get_field(obj_ref, PROXY_FIELD_INTERFACES) {
        cratonvm_types::Value::Object(Some(a)) => a,
        _ => {
            // No CratonVM-internal interfaces array in slot 1. For a genuine
            // generated `$ProxyN` the proxied interfaces are recorded in the
            // class itself, so the `is_subclass_of(obj_class, target)` check at
            // the `instanceof`/`checkcast` call site is authoritative — a `true`
            // here would make the proxy `instanceof` EVERYTHING. That is exactly
            // the spring-bug-08 regression: a proxy deserialized via the
            // serialization path restores its handler (slot 0) but NOT this
            // internal interfaces slot, so `proxy instanceof AbstractAssert`
            // wrongly became true and tripped AssertJ's `isEqualTo` guard,
            // collapsing `SerializableTypeWrapperTests` to 1/8. Defer to the
            // class-declared interfaces by returning `false`; only the bare
            // `Proxy$Instance` shim (which has neither a slot-1 array nor its
            // own declared interface set) keeps the old liberal rule.
            return obj_name == "java/lang/reflect/Proxy$Instance";
        }
    };
    // Source the proxy's interface set. The synthetic 3-slot layout stores the
    // `Class[]` at slot 1 (`PROXY_FIELD_INTERFACES`); the real-super layout
    // (proxy-real-classfile migration: the generated `$ProxyN` extends
    // `java.lang.reflect.Proxy`, sole field `h` at slot 0) has no such slot, so
    // its interfaces come from the proxy class's own declared interfaces. Decide
    // by the receiver's field count so we never read slot 1 out of bounds on the
    // 1-field real-super proxy — that OOB read returned null and fell into the
    // liberal "matches every target" fallback below, which made a real-super
    // proxy report `instanceof` TRUE for ARBITRARY types (e.g. `java.lang.Class`).
    // That in turn routed the proxy through `ObjectOutputStream.writeClass` → a
    // null-`name` class descriptor → NPE, breaking proxy serialization
    // round-trips (proxy-real-classfile Increment 4 soak). Mirrors the same
    // slot-1→declared-interfaces fix applied to
    // `proxy_resolve_declaring_class_mirror`.
    let mut iface_cids: Vec<ClassId> = Vec::new();
    if has_iface_slot {
        match shared.mem.heap.get_field(obj_ref, PROXY_FIELD_INTERFACES) {
            cratonvm_types::Value::Object(Some(arr)) => {
                let n = shared.mem.heap.array_length(arr);
                for i in 0..n {
                    // Use the mirror→ClassId mapping we already maintain (slot 0
                    // on real-JDK Class is `cachedConstructor` — too fragile).
                    if let Ok(cratonvm_types::Value::Object(Some(m))) =
                        shared.mem.heap.get_array_element(arr, i)
                    {
                        if let Some(cid) = crate::vm::class_id_from_mirror(shared, m) {
                            iface_cids.push(cid);
                        }
                    }
                }
            }
            _ => {
                // Synthetic-layout proxy with no interfaces array recorded —
                // preserve the historical liberal rule so we don't regress
                // proxies that never went through `Proxy.newProxyInstance`.
                return true;
            }
        }
    };
    let n = shared.mem.heap.array_length(interfaces_arr);
    let target_cid = shared
        .classes
        .class_manager
        .write()
        .load_class(target_class_name)
        .ok();
    for i in 0..n {
        let mirror = match shared.mem.heap.get_array_element(interfaces_arr, i) {
            Ok(cratonvm_types::Value::Object(Some(m))) => m,
            _ => continue,
        };
        // Read the `name` String off the Class mirror via the heap (slot 0
        // on real-JDK Class is `cachedConstructor` — too fragile). Use the
        // mirror→ClassId mapping we already maintain.
        let iface_cid = match crate::vm::class_id_from_mirror(shared, mirror) {
            Some(cid) => cid,
            None => continue,
        };
        // Direct identity match.
        if Some(iface_cid) == target_cid {
            return true;
        }
        // Superinterface walk: target is a superinterface of `iface_cid`?
        if let Some(tcid) = target_cid {
            if shared
                .classes
                .class_manager
                .read()
                .is_subclass_of(iface_cid, tcid)
            {
                return true;
            }
        }
        // Name fallback (synthetic interfaces that may not be loaded yet).
        if let Some(iface_class) = shared.classes.class_manager.read().get_class(iface_cid) {
            if &*iface_class.name == target_class_name {
                return true;
            }
        }
    }
    false
}

/// Does the backing collection in slot 0 of an unmodifiable wrapper reach
/// `iface_name` in its own hierarchy?
///
/// A NAME form rather than resolving `iface_name` to a `ClassId` first: it
/// neither loads nor looks up, which keeps this on the no-safepoint side of
/// [`unmod_stamp_display_name`]'s contract. Slot 0 is `UNMOD_FIELD_BACKING` in
/// `native-collections`.
///
/// `is_assignable_to_name` and NOT `is_subclass_of_by_name`, whose name reads
/// like the right one and is not: that function is the exception-`catch_type`
/// fallback and walks ONLY the superclass chain, because a `catch_type` is
/// never an interface. Every name this function is called with IS an
/// interface, so the supers-only walk could not match one — `ArrayList`
/// reaches `AbstractList`, `AbstractCollection`, `Object` and stops.
///
/// The cost was one whole display class. `unmod_backing_reaches(.., "java/util/
/// RandomAccess")` answered `false` for a `Collections.unmodifiableList(new
/// ArrayList<>(..))`, so the opcodes' display arm picked
/// `Collections$UnmodifiableList` while `getClass()` — whose authority
/// (`native-builtins`' `getclass_backing_is_random_access`) resolves the
/// interface to a `ClassId` and uses the DAG-walking `is_subclass` — reported
/// `Collections$UnmodifiableRandomAccessList`. `instanceof RandomAccess` was
/// `false` and `RandomAccess.class.isInstance(..)` `true` for the same object,
/// which is the one thing `display_class_satisfies_target` exists to prevent.
/// The identical trap is written out at length on the `Path.toString()` branch
/// in `runtime/invokedynamic.rs`; this is its second occurrence.
/// `apps/probes/RandomAccessProbe` is the reproducer, and
/// `classloading`'s `the_supers_only_name_walk_cannot_see_an_interface_the_dag_walk_finds`
/// asserts the divergence between the two walks in both directions.
fn unmod_backing_reaches(
    shared: &SharedVm,
    obj_ref: cratonvm_types::ObjectRef,
    iface_name: &str,
) -> bool {
    if shared.mem.heap.num_fields(obj_ref) == 0 {
        return false;
    }
    let backing = match shared.mem.heap.get_field(obj_ref, 0) {
        cratonvm_types::Value::Object(Some(b)) => b,
        _ => return false,
    };
    object_reaches(shared, backing, iface_name, 0)
}

/// Does `obj` — as the object it STANDS FOR, not as the class it is stamped
/// with — reach `iface_name`?
///
/// For an ordinary receiver this is one name walk. The recursion exists for a
/// wrapper of a wrapper: `Collections.unmodifiableList(List.of("a", "b"))`
/// backs one `cratonvm/internal/UnmodifiableList` stamp with another. All
/// eleven stamps declare only their FAMILY-level interfaces in `vm_init.rs`
/// (`List`, `Collection`, `Serializable`), because everything finer is a
/// property of the instance's backing rather than of the stamp — which is why
/// asking the stamp's own hierarchy about `RandomAccess` answers `false` for
/// every one of them, `List.of`'s included, whose HotSpot display class
/// (`ImmutableCollections$ListN`, via `AbstractImmutableList`) implements it.
///
/// So descend slot 0 (`UNMOD_FIELD_BACKING`): an unmodifiable view carries the
/// marker exactly when the thing it wraps does.
///
/// This function is one half of a PAIR and must not move alone.
/// `native-builtins`' `getclass_object_reaches` — the authority behind
/// `Object.getClass()` — runs the identical rule, and it is a separate
/// implementation because THIS side may not load a class or run Java: the
/// receiver at `op_instanceof`/`op_checkcast` has been popped from the operand
/// stack and is a bare Rust local the collector cannot see. That constraint is
/// also why the rule is structural rather than "consult the display class if it
/// happens to be loaded": an answer that depended on whether anything had
/// called `getClass()` first would be a worse defect than the one this closes.
/// `apps/probes/RandomAccessProbe` prints all three doors per row and an
/// `agree=` column that goes false the moment the pair parts.
fn object_reaches(
    shared: &SharedVm,
    obj: cratonvm_types::ObjectRef,
    iface_name: &str,
    depth: usize,
) -> bool {
    // A wrapper chain is a handful of links; the bound is a cycle guard, not a
    // policy. (`alloc_unmod_wrapper` stores an object that already existed, so
    // slot-0 nesting is acyclic by construction — this is insurance.)
    if depth > 8 {
        return false;
    }
    let cid = shared.mem.heap.class_id_of(obj);
    let is_stamp = {
        let cm = shared.classes.class_manager.read();
        if cm.is_assignable_to_name(cid, iface_name) {
            return true;
        }
        match cm.get_class(cid) {
            Some(c) => c.name.starts_with("cratonvm/internal/Unmodifiable"),
            None => false,
        }
    };
    if !is_stamp || shared.mem.heap.num_fields(obj) == 0 {
        return false;
    }
    match shared.mem.heap.get_field(obj, 0) {
        cratonvm_types::Value::Object(Some(inner)) => {
            object_reaches(shared, inner, iface_name, depth + 1)
        }
        _ => false,
    }
}

/// The JDK class that a `cratonvm/internal/Unmodifiable*` stamp stands for,
/// resolved only as far as its FAMILY.
///
/// `Object.getClass()` already reports a JDK class for these receivers —
/// `native-builtins`' `getclass_display_class_id` is the authority, and this
/// mirrors its stamp table and its slot reads. The ONE thing it deliberately
/// does not do is pick between the size-discriminated forms (`Map1`/`MapN`,
/// `List12`/`ListN`, `Set12`/`SetN`), for two reasons that point the same way:
///
/// * **It cannot afford to.** The authority reaches the size through
///   `invoke_virtual(this, "size", "()I")` — it RUNS JAVA CODE. The caller this
///   exists for is `op_instanceof`/`op_checkcast`, where the receiver has been
///   popped from the operand stack and is a bare Rust local; the VM's own
///   root-collection guard already reports that object as
///   `in_published_snapshot=false` at `site="checkcast"`. Running Java there
///   would let the collector move a receiver nothing can see.
/// * **It does not need to.** MEASURED against the oracle's own class files
///   (`javap`, HotSpot 25.0.3+9): every size pair is supertype-identical.
///   `Map1` and `MapN` both extend `ImmutableCollections$AbstractImmutableMap`
///   and implement `Serializable`, and nothing else; `List12`/`ListN` both
///   extend `AbstractImmutableList`; `Set12`/`SetN` both extend
///   `AbstractImmutableSet`. `RandomAccess` sits on `AbstractImmutableList`, so
///   even that cell is size-independent. The size cannot change a subtype
///   answer, only a name.
///
/// Every read below is a heap field read or a class-manager read: nothing here
/// allocates, loads a class, or reaches a safepoint.
///
/// `None` means "no display alias" — the four iterator/entry stamps
/// (`UnmodifiableItr`, `UnmodifiableListItr`, `UnmodifiableEntryItr`,
/// `UnmodifiableMapEntry`) land here, exactly as they do in the authority.
///
/// docs/known-issues/jdk-only/H18-1-the-opcode-and-the-message-read-different-classes-20260821.md
pub(crate) fn unmod_stamp_display_name(
    shared: &SharedVm,
    obj_ref: cratonvm_types::ObjectRef,
    stamp_name: &str,
) -> Option<&'static str> {
    // Slot 1 is `UNMOD_FIELD_IMMUTABLE` in `native-collections`: set by the
    // `List.of`/`Set.of`/`Map.of`/`copyOf` factories and clear for the
    // `Collections.unmodifiable*` wrappers. It is the ONLY thing that tells the
    // two families apart — they share a stamp. CONTRACT: this index must match
    // `getclass_immutable_marker`, which reads the same slot.
    //
    // Bounds-checked against the OBJECT's header, not the class's declared
    // field count, and the difference is not academic: `vm_init.rs` registers
    // all eleven stamps with `ensure_bootstrap_compat_class(.., name, 1)` while
    // `alloc_unmod_wrapper` allocates them with `try_alloc_synthetic(.., 2)`.
    // A class-side `num_total_fields >= 2` guard — the shape
    // `proxy_instance_satisfies_target` uses a few lines above — would reject
    // every one of these receivers even though slot 1 is really there.
    //
    // Evaluated as a CLOSURE, after the name match rather than before it, so
    // no field is touched for a receiver this function is going to decline.
    // `cce_display_class_name` calls this on every failed cast in the VM, so
    // "declines" includes every `java/lang/*` object in the heap.
    let immutable = || {
        shared.mem.heap.num_fields(obj_ref) > 1
            && matches!(
                shared.mem.heap.get_field(obj_ref, 1),
                cratonvm_types::Value::Int(1)
            )
    };
    Some(match stamp_name {
        "cratonvm/internal/UnmodifiableList" => {
            if immutable() {
                "java/util/ImmutableCollections$ListN"
            } else if unmod_backing_reaches(shared, obj_ref, "java/util/RandomAccess") {
                "java/util/Collections$UnmodifiableRandomAccessList"
            } else {
                "java/util/Collections$UnmodifiableList"
            }
        }
        // `UnmodifiableEntrySet` shares `CollSet`'s display in the authority.
        // That is a KNOWN divergence, not one introduced here: HotSpot reports
        // `Collections$UnmodifiableMap$UnmodifiableEntrySet` for an
        // `unmodifiableMap(...).entrySet()` and this VM reports
        // `Collections$UnmodifiableSet` (MEASURED, both VMs, 2026-08-21 — see
        // H18-2). Correcting it belongs at the authority, with the `getClass()`
        // name; mirroring it here keeps the two doors agreeing, which is the
        // whole point of this function. Both classes are non-`AbstractSet`
        // wrappers over `UnmodifiableCollection`, so no subtype cell moves.
        "cratonvm/internal/UnmodifiableSet" | "cratonvm/internal/UnmodifiableEntrySet" => {
            if immutable() {
                "java/util/ImmutableCollections$SetN"
            } else {
                "java/util/Collections$UnmodifiableSet"
            }
        }
        "cratonvm/internal/UnmodifiableMap" => {
            if immutable() {
                "java/util/ImmutableCollections$MapN"
            } else {
                "java/util/Collections$UnmodifiableMap"
            }
        }
        "cratonvm/internal/UnmodifiableSortedSet" => "java/util/Collections$UnmodifiableSortedSet",
        "cratonvm/internal/UnmodifiableNavigableSet" => {
            "java/util/Collections$UnmodifiableNavigableSet"
        }
        "cratonvm/internal/UnmodifiableCollection" => {
            "java/util/Collections$UnmodifiableCollection"
        }
        _ => return None,
    })
}

/// Does the receiver's DISPLAY class — the one `Object.getClass()` reports —
/// satisfy `target_class_id`?
///
/// `Map.of(...)`, `List.of(...)`, `Set.of(...)`, the `copyOf` family and the
/// `Collections.unmodifiable*` wrappers are all allocated in Compatible mode as
/// one of eleven `cratonvm/internal/Unmodifiable*` stamps, and `vm_init.rs`
/// gives every one of those stamps `java/lang/Object` as its superclass with an
/// accurate INTERFACE list. That is why `Map.of(…) instanceof Map` is right and
/// `Map.of(…) instanceof AbstractMap` is wrong: the interfaces are declared and
/// the superclass chain is not there to walk.
///
/// `Class.isInstance` has answered this correctly since the Spring
/// `GenericConversionService` fix (`native-builtins/src/lang_class.rs`, the
/// "Consistency with `getClass()`" arm). The opcodes never learned to, so the
/// reflective door and the bytecode door disagreed about ONE object at ONE
/// instant — MEASURED on seven of seven receivers, and the `RandomAccess` cell
/// is not cosmetic: `Collections.binarySearch` on an unmodifiable list ran
/// **886x** slower than on the ArrayList inside it, in the same VM and the same
/// run, because `Collections` branches on `list instanceof RandomAccess`.
///
/// # Can only ADMIT
///
/// It runs after every other predicate has declined, and the display class is
/// the class HotSpot genuinely reports for this receiver, so admitting exactly
/// that class's supertypes cannot over-admit. The negative control is
/// `Collections.unmodifiableList(…) instanceof AbstractCollection`, which stays
/// FALSE because `Collections$UnmodifiableRandomAccessList` extends
/// `UnmodifiableCollection`, which extends `Object` — verified against the
/// oracle's class files, not assumed.
///
/// # GC safety
///
/// The classification runs no Java (see [`unmod_stamp_display_name`]) and is
/// complete before anything here can safepoint. The one operation that can —
/// loading the display class on first use — is wrapped in a `native_pin_roots`
/// entry and `obj_ref` is refreshed from it afterwards, which is why this takes
/// `&mut ObjectRef`: `op_checkcast` reads the receiver AGAIN in its failure
/// path, so a move that the caller did not observe would corrupt the
/// `ClassCastException` it is about to build.
///
/// # Cost on the common path
///
/// One class-manager read guard, one `Arc<str>` prefix compare that fails at
/// byte 1 for every `java/…` receiver, and a return. No allocation — strictly
/// cheaper than [`synthetic_implements`] beside it, which clones the receiver's
/// class name into a `String` on every call. It sits LAST in the disjunction,
/// and reaches it only on a path that has already built a `String` for the
/// target name and taken this same lock twice.
///
/// docs/known-issues/jdk-only/H18-1-the-opcode-and-the-message-read-different-classes-20260821.md
pub(crate) fn display_class_satisfies_target(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj_ref: &mut cratonvm_types::ObjectRef,
    obj_class_id: ClassId,
    target_class_id: ClassId,
) -> bool {
    // Screen, and drop the guard before anything else: `unmod_backing_reaches`
    // takes the same read lock and `load_class_concurrent` takes the WRITE
    // lock, on a `parking_lot::RwLock` that is not reentrant. Binding the
    // result to a `let` is what makes the temporary guard drop here rather than
    // at the end of the statement — the trap written out at length on
    // `resolve_component` in `array_is_assignable_to_impl` below.
    let stamp_name = {
        let cm = shared.classes.class_manager.read();
        match cm.get_class(obj_class_id) {
            Some(c) if c.name.starts_with("cratonvm/internal/Unmodifiable") => c.name.to_string(),
            // The overwhelmingly common case: not a stamp, nothing to say.
            _ => return false,
        }
    };
    let Some(display_name) = unmod_stamp_display_name(shared, *obj_ref, &stamp_name) else {
        return false;
    };
    let already_loaded = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(display_name);
    let display_class_id = match already_loaded {
        Some(cid) => cid,
        None => {
            // `Map.of()` is served by a native in Compatible mode, so nothing
            // necessarily loaded `java.util.ImmutableCollections` before this
            // point. Declining on "not loaded yet" would make the answer depend
            // on whether anything had called `getClass()` first, which is a
            // worse defect than the one being fixed. So load it — pinned,
            // because loading allocates a class mirror and can safepoint.
            let pin = thread.native_pin_roots.len();
            thread.native_pin_roots.push(*obj_ref);
            let loaded = shared.load_class_concurrent(display_name);
            *obj_ref = thread
                .native_pin_roots
                .get(pin)
                .copied()
                .unwrap_or(*obj_ref);
            thread.native_pin_roots.truncate(pin);
            match loaded {
                Ok(cid) => cid,
                // The display class is not on this classpath. Answer exactly
                // what this predicate answered before it existed.
                Err(_) => return false,
            }
        }
    };
    if display_class_id == target_class_id {
        return true;
    }
    let cm = shared.classes.class_manager.read();
    cm.is_subclass_of(display_class_id, target_class_id)
}

/// Check if a lambda proxy object satisfies a target class. Lambda proxy ClassIds
/// (>= 0x8000_0000) are not in the class store, so normal `is_subclass_of` always
/// returns false. Instead, we look up the proxy's `functional_interface` and check
/// if that interface is a subclass of the target.
pub(super) fn lambda_proxy_satisfies(
    shared: &SharedVm,
    obj_class_id: ClassId,
    target_class_id: ClassId,
) -> bool {
    let dbg_aci = crate::runtime::env_cache::dbg_loader_trace();
    let proxies = shared.classes.lambda_proxies.read();
    if dbg_aci && !proxies.contains_key(&obj_class_id) {
        eprintln!(
            "[LOADER-TRACE] lambda_proxy_satisfies: obj_class_id={obj_class_id:?} NOT a registered lambda proxy target_class_id={target_class_id:?}"
        );
    }
    if let Some(call_site) = proxies.get(&obj_class_id) {
        let iface_name = call_site.functional_interface.clone();
        drop(proxies); // release lock before loading
        if dbg_aci {
            let target_name_dbg = shared
                .classes
                .class_manager
                .read()
                .get_class(target_class_id)
                .map(|c| c.name.to_string());
            eprintln!(
                "[LOADER-TRACE] lambda_proxy_satisfies: obj_class_id={obj_class_id:?} iface_name={iface_name} target_class_id={target_class_id:?} target_name={target_name_dbg:?}"
            );
        }
        // `LambdaMetafactory.altMetafactory` with `FLAG_SERIALIZABLE` -- every JDK
        // `Comparator.comparing*`, and any `(Iface & Serializable)` intersection
        // cast -- adds `java.io.Serializable` to the spun proxy's interfaces. A
        // plain `metafactory` lambda does NOT implement it: on real HotSpot
        // `(Serializable) (Supplier<String>) () -> "x"` throws ClassCastException.
        // This used to accept Serializable universally because the flag was not
        // tracked, which made every CratonVM lambda look serializable. It is
        // recorded now, so ask the one owner of the rule.
        let target_name = shared
            .classes
            .class_manager
            .read()
            .get_class(target_class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        if &*target_name == "java/io/Serializable" {
            return shared.lambda_proxy_serializability(obj_class_id)
                != cratonvm_native_api::LambdaSerializability::NotSerializable;
        }
        // `altMetafactory`'s FLAG_MARKERS block names ADDITIONAL interfaces the
        // spun proxy implements on top of the functional interface. The proxy's
        // synthetic ClassId has no ClassStore `interfaces` vector to hold them,
        // so they live in a side table beside `lambda_proxies`; ask it before
        // falling through to the functional interface, or an intersection-cast
        // lambda fails its own `instanceof` / `checkcast`.
        if crate::runtime::invokedynamic::lambda_proxy_marker_satisfies(
            shared,
            obj_class_id,
            target_class_id,
            &target_name,
        ) {
            return true;
        }
        // A lambda proxy is defined for precisely this functional-interface
        // name. Its synthetic VM-only ClassId has no ClassStore hierarchy, and
        // a global reload can select a different loader's mirror during forked
        // test execution. The call-site metadata is authoritative here.
        if iface_name.as_ref() == target_name {
            return true;
        }
        let load_result = shared.load_class_concurrent(&iface_name);
        if let Ok(iface_id) = load_result {
            // Plain ClassId-based `is_subclass_of` fails when `iface_name`'s
            // globally-resolved copy (e.g. `AotApplicationContextInitializer`,
            // first loaded under the Application loader) extends a DIFFERENT
            // loader's copy of `target_class_name` than the one THIS checkcast
            // resolved (e.g. the fork loader's own `ApplicationContextInitializer`,
            // `target_class_id`) — both are genuinely "ApplicationContextInitializer"
            // by name, just different per-loader Class objects. Fall back to the
            // same name-based hierarchy walk `checkcast` itself already uses for
            // non-lambda receivers (see `loader_aware_name_assignable`'s call site
            // in `Instruction::Checkcast`) instead of only trusting ClassId identity.
            return shared
                .classes
                .class_manager
                .read()
                .is_subclass_of(iface_id, target_class_id)
                || loader_aware_name_assignable(shared, iface_id, target_class_id, &target_name);
        }
    }
    false
}

pub(super) fn loader_aware_name_assignable(
    shared: &SharedVm,
    obj_class_id: ClassId,
    target_class_id: ClassId,
    target_class_name: &str,
) -> bool {
    if !crate::runtime::env_cache::loader_aware_resolution()
        || is_global_resolution_namespace(target_class_name)
    {
        return false;
    }

    let cm = shared.classes.class_manager.read();
    let Some(obj_class) = cm.get_class(obj_class_id) else {
        return false;
    };
    let Some(target_class) = cm.get_class(target_class_id) else {
        return false;
    };

    if &*obj_class.name == target_class_name && &*target_class.name == target_class_name {
        return true;
    }
    let mut queue: Vec<ClassId> = Vec::new();
    let mut current = Some(obj_class_id);
    while let Some(cid) = current {
        let Some(class) = cm.class_store.get(cid) else {
            break;
        };
        // The resolved target can be a same-named class mirror from a
        // different loader, not only an interface. Compare the structural
        // superclass chain by binary name before relying on ClassId identity.
        if &*class.name == target_class_name {
            return true;
        }
        queue.extend_from_slice(&class.interfaces);
        current = class.superclass;
    }

    if !target_class.is_interface() {
        return false;
    }

    let mut seen: Vec<ClassId> = Vec::new();
    while let Some(iface_id) = queue.pop() {
        if seen.contains(&iface_id) {
            continue;
        }
        seen.push(iface_id);
        let Some(iface) = cm.class_store.get(iface_id) else {
            continue;
        };
        if &*iface.name == target_class_name {
            return true;
        }
        queue.extend_from_slice(&iface.interfaces);
    }

    false
}

// ---------------------------------------------------------------------------
// Helper: array type compatibility for checkcast/instanceof
// ---------------------------------------------------------------------------

/// Compute the JVM array descriptor (e.g. `"[I"`, `"[Ljava/lang/String;"`)
/// for a heap object that is known to be an array. Returns `None` if the
/// object is not actually an array.
pub(crate) fn array_descriptor_of(
    shared: &SharedVm,
    obj_ref: cratonvm_types::ObjectRef,
) -> Option<String> {
    if shared.mem.heap.kind_of(obj_ref) != cratonvm_types::ObjectKind::Array {
        return None;
    }
    let et = shared.mem.heap.element_type_of(obj_ref);
    match et {
        ArrayElementType::Boolean => Some("[Z".to_string()),
        ArrayElementType::Char => Some("[C".to_string()),
        ArrayElementType::Float => Some("[F".to_string()),
        ArrayElementType::Double => Some("[D".to_string()),
        ArrayElementType::Byte => Some("[B".to_string()),
        ArrayElementType::Short => Some("[S".to_string()),
        ArrayElementType::Int => Some("[I".to_string()),
        ArrayElementType::Long => Some("[J".to_string()),
        ArrayElementType::Reference => {
            // The class_id on a Reference array holds the component class id.
            let comp_id = shared.mem.heap.class_id_of(obj_ref);
            let comp_name = shared
                .classes
                .class_manager
                .read()
                .get_class(comp_id)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            if comp_name.is_empty() {
                Some("[Ljava/lang/Object;".to_string())
            } else if comp_name.starts_with('[') {
                // nested array: descriptor is "[" + comp_name
                Some(format!("[{}", comp_name))
            } else {
                Some(format!("[L{};", comp_name))
            }
        }
    }
}

/// Check whether an array object (with descriptor `src_desc`) is assignment-
/// compatible with `target_name`. `target_name` may be:
///   - An array descriptor like "[I" or "[Ljava/lang/Object;".
///   - A class/interface name like "java/lang/Object", "java/io/Serializable",
///     or "java/lang/Cloneable".
pub(crate) fn array_is_assignable_to(shared: &SharedVm, src_desc: &str, target_name: &str) -> bool {
    // Lenient entry point: used by `checkcast` and `aastore`, where the native
    // array-allocation leniency (an `Object[]` standing in for a `T[]` whose real
    // element type a native path didn't preserve) must not provoke a spurious
    // `ClassCastException` / `ArrayStoreException`.
    array_is_assignable_to_impl(shared, src_desc, target_name, true)
}

/// Strict variant for the `instanceof` opcode (SBR-03). Unlike `checkcast`,
/// `instanceof` must answer precisely: `Object[] instanceof I[]` is `false`
/// because `Object` is not assignable to the interface `I`. The lenient
/// `Object[]`→`T[]` fallback used for casts is suppressed here so a genuine
/// `Object[]` is not reported as an instance of an unrelated `T[]`.
pub(crate) fn array_is_instance_of(shared: &SharedVm, src_desc: &str, target_name: &str) -> bool {
    array_is_assignable_to_impl(shared, src_desc, target_name, false)
}

pub(super) fn array_is_assignable_to_impl(
    shared: &SharedVm,
    src_desc: &str,
    target_name: &str,
    lenient: bool,
) -> bool {
    // Every array is an Object and implements Serializable + Cloneable.
    if &*target_name == "java/lang/Object"
        || target_name == "java/io/Serializable"
        || target_name == "java/lang/Cloneable"
    {
        return true;
    }
    if !target_name.starts_with('[') {
        // Array cannot be cast to arbitrary non-array class.
        return false;
    }
    if src_desc == target_name {
        return true;
    }
    // Parse: strip leading '[' from both.
    let src_rest = &src_desc[1..];
    let tgt_rest = &target_name[1..];

    // Primitive component: must match exactly.
    if src_rest.len() == 1 && "ZCBSIJFD".contains(&src_rest[..1]) {
        return src_rest == tgt_rest;
    }
    if tgt_rest.len() == 1 && "ZCBSIJFD".contains(&tgt_rest[..1]) {
        return false;
    }
    // Both are reference-component arrays. Recurse on components.
    //   [Lfoo; vs [Lbar; — extract "foo"/"bar"
    //   [[I vs [[I — nested array
    let extract_component = |desc: &str| -> Option<(bool, String)> {
        // returns (is_array, name)
        if desc.starts_with('[') {
            Some((true, desc.to_string()))
        } else if desc.starts_with('L') && desc.ends_with(';') {
            Some((false, desc[1..desc.len() - 1].to_string()))
        } else {
            None
        }
    };
    let (src_is_arr, src_comp) = match extract_component(src_rest) {
        Some(x) => x,
        None => return false,
    };
    let (tgt_is_arr, tgt_comp) = match extract_component(tgt_rest) {
        Some(x) => x,
        None => return false,
    };
    if src_is_arr && tgt_is_arr {
        return array_is_assignable_to_impl(shared, &src_comp, &tgt_comp, lenient);
    }
    if src_is_arr != tgt_is_arr {
        // One is nested array, the other is an object-component; only compatible
        // if the object component is Object/Serializable/Cloneable.
        if !src_is_arr {
            return false;
        }
        return tgt_comp == "java/lang/Object"
            || tgt_comp == "java/io/Serializable"
            || tgt_comp == "java/lang/Cloneable";
    }
    // Both are reference (non-array) component class names.
    // Lenient fallback (checkcast/aastore only): our native array-allocation
    // paths often create reference arrays with component `java/lang/Object` when
    // the runtime component type is actually a subclass (e.g.
    // `getEnumConstantsShared` returns `[Ljava/lang/Object;` but callers cast to
    // `[LEnum;`). Accept these casts so reflection/enum paths don't spuriously
    // fail. For `instanceof` (lenient == false) this is suppressed: a genuine
    // `Object[]` is NOT an instance of `I[]` (SBR-03).
    if src_comp == "java/lang/Object" {
        return lenient || tgt_comp == "java/lang/Object";
    }
    // Array casts are common on reflection API results. In particular, a
    // correctly typed `Annotation[]` is routinely widened to `Object[]` by
    // JUnit and Spring. The old path unconditionally acquired the
    // class-manager *write* lock and invoked `load_class` for both components,
    // even though these bootstrap types are already loaded. That turns every
    // such cast into a global synchronization point; an imprecise `Object[]`
    // happens to skip it through the lenient fallback above, which masked the
    // cost while violating the reflection return-type contract.
    //
    // Preserve the existing name-based, loader-agnostic semantics, but resolve
    // from the read-side class table first. `load_class_concurrent` retains the
    // old on-demand loading behavior for a genuine miss without forcing the
    // warm path through an exclusive lock.
    let resolve_component = |name: &str| {
        // Do NOT chain `.read()....or_else(|| ...load_class_concurrent...)` in
        // one expression: the `RwLockReadGuard` temporary from `.read()` is not
        // dropped until the end of the *statement*, which -- in a single
        // expression -- includes the `or_else` closure's own execution. On a
        // genuine miss, `load_class_concurrent` takes `class_manager.write()`
        // on the SAME thread that still (per that temporary-lifetime rule)
        // holds its own read guard, self-deadlocking against a non-reentrant
        // `parking_lot::RwLock`. Bind the read result to a `let` first so the
        // guard drops before any write-lock attempt.
        let found = shared
            .classes
            .class_manager
            .read()
            .find_unique_class_by_name(name);
        found.or_else(|| shared.load_class_concurrent(name).ok())
    };
    let src_id = match resolve_component(&src_comp) {
        Some(id) => id,
        None => return false,
    };
    let tgt_id = match resolve_component(&tgt_comp) {
        Some(id) => id,
        None => return false,
    };
    shared
        .classes
        .class_manager
        .read()
        .is_subclass_of(src_id, tgt_id)
}

/// JVMS §aastore covariance check: returns `true` if the (non-null) element
/// `value_ref` may be stored into the reference array `array_ref`, `false` if
/// the store must throw `ArrayStoreException`.
///
/// Shared by the interpreter `aastore` opcode and the JIT `jit_aastore` helper
/// so both enforce the same rule. Only call this for genuine `Object[]`-family
/// (reference-component) arrays with a non-null element; a `null` element is
/// always storable and primitive-component arrays never reach `aastore`.
///
/// Conservative posture: the check is *additive correctness* — it must never
/// produce a FALSE `ArrayStoreException`. Whenever the component or element type
/// cannot be determined precisely (missing class entries, synthetic class ids,
/// `java/lang/Object` component, unresolvable names) it returns `true` (allow
/// the store), matching the lenient fallbacks already in
/// [`array_is_assignable_to`]. It only returns `false` when both types resolve
/// to loaded classes AND the element is provably NOT assignable to the
/// component.
/// `true` when `array_ref`'s component type is exactly `java.lang.Object`.
///
/// # Why this is not just `array_descriptor_of(..) == "[Ljava/lang/Object;"`
///
/// Because that costs a class-manager read lock and TWO `String` allocations
/// (`c.name.to_string()`, then `format!("[L{};", ..)`) on EVERY reference-array
/// store, to answer a question about one `ClassId`. `Object[]` is the single
/// most common reference-array shape there is — `ArrayList`'s `elementData`,
/// every varargs pack, every `toArray()` — so that arm carries most of the
/// traffic and returns `true` without ever reading the string it built.
///
/// MEASURED (`probes/BarrierProbe`, 4M stores, min of five interleaved rounds,
/// 2026-08-24, 8-core Linux host): a non-null reference store into an `Object[]`
/// cost 1335 ms against 614 ms for the byte-identical NULL store — `refself` vs
/// `refnull`, whose only differences inside the interpreter are this covariance
/// check and the post-write barrier. The same 2.2x appears on ZGC, whose
/// post-write barrier does nothing, which is what pins it on this check rather
/// than on the collector.
///
/// The one-slot per-thread memo is keyed by `(vm identity, component ClassId)`.
/// Array stores are overwhelmingly repetitive — a loop filling one array asks
/// about one component class — and the VM identity is in the key because
/// `ClassId`s are per-VM dense indices, so two `SharedVm`s in one process (the
/// test harness builds several) must not share an answer.
#[inline]
fn reference_array_component_is_object(
    shared: &SharedVm,
    array_ref: cratonvm_types::ObjectRef,
) -> bool {
    // On a reference array the header's class id holds the COMPONENT class.
    let comp_id = shared.mem.heap.class_id_of(array_ref).as_u32();
    let key = (shared.vm_identity, comp_id);
    thread_local! {
        static MEMO: std::cell::Cell<Option<((usize, u32), bool)>> =
            const { std::cell::Cell::new(None) };
    }
    if let Some((k, v)) = MEMO.with(|c| c.get()) {
        if k == key {
            return v;
        }
    }
    // A component class the store cannot name answers `true` as well:
    // `array_descriptor_of` substitutes `[Ljava/lang/Object;` for it, so the arm
    // this replaces returned `true` for that case too.
    let v = match shared
        .classes
        .class_manager
        .read()
        .get_class(ClassId::new(comp_id))
    {
        Some(c) => &*c.name == "java/lang/Object",
        None => true,
    };
    MEMO.with(|c| c.set(Some((key, v))));
    v
}

pub(crate) fn aastore_element_assignable(
    shared: &SharedVm,
    array_ref: cratonvm_types::ObjectRef,
    value_ref: cratonvm_types::ObjectRef,
) -> bool {
    // FAST PATH for the `Object[]` RULE stated ~40 lines below, taken without
    // building a descriptor at all. Everything after this point allocates two
    // `String`s before it can look at the component, and this arm is the one
    // most stores land on. See `reference_array_component_is_object` for the
    // measurement and for why the two forms agree on every input.
    if shared.mem.heap.kind_of(array_ref) == cratonvm_types::ObjectKind::Array
        && shared.mem.heap.element_type_of(array_ref) == ArrayElementType::Reference
        && reference_array_component_is_object(shared, array_ref)
    {
        return true;
    }

    // Determine the array's component descriptor by stripping the leading '['
    // from its full descriptor (e.g. "[Ljava/lang/Number;" -> "Ljava/lang/Number;").
    let array_desc = match array_descriptor_of(shared, array_ref) {
        Some(d) => d,
        // Unknown array shape (no descriptor) — fail open, allow the store.
        None => return true,
    };
    if !array_desc.starts_with('[') {
        return true;
    }
    let component = &array_desc[1..];

    // `Object[]` accepts any reference element. This one is a RULE, not a
    // heuristic: every reference value is a `java.lang.Object`, so no component
    // information and no hierarchy walk can ever make the store illegal.
    //
    // `Serializable[]` and `Cloneable[]` used to be admitted by this same arm,
    // on the theory that they too "accept any reference". They do not. They are
    // ordinary marker interfaces and `aastore` checks them like any other —
    // MEASURED on HotSpot 25.0.3 under `-Xint` (`scratchpad/c16/Arm3.java`):
    //
    //   Serializable[] <- Object      ArrayStoreException: java.lang.Object
    //   Serializable[] <- Integer     OK          (Integer -> Number -> Serializable)
    //   Serializable[] <- String      OK
    //   Serializable[] <- int[]       OK          (every ARRAY is Serializable)
    //   Serializable[] <- lambda      ArrayStoreException
    //   Cloneable[]    <- Object      ArrayStoreException: java.lang.Object
    //   Cloneable[]    <- Integer     ArrayStoreException: java.lang.Integer
    //   Cloneable[]    <- String      ArrayStoreException: java.lang.String
    //   Cloneable[]    <- int[]       OK          (every ARRAY is Cloneable)
    //   Cloneable[]    <- ArrayList   OK          (ArrayList implements it)
    //
    // The `Integer` pair is the tell: it is Serializable and is NOT Cloneable,
    // which no blanket can express. The two array rows are the JVMS §4.10.1.2 /
    // JLS §10.7 rule that is easy to miss — every array type implements BOTH,
    // and getting that wrong turns ordinary array-of-array code into spurious
    // exceptions. It stays right without an arm here: an array value falls into
    // the block immediately below, and `array_is_assignable_to_impl` opens with
    // exactly that rule (`target_name == "java/io/Serializable" ||
    // "java/lang/Cloneable"` -> true).
    //
    // A plain object with one of these components is now answered by the same
    // machinery as any other interface component — `is_subclass_of` walks the
    // implemented-interface DAG, so `Serializable[] <- Integer` resolves through
    // `Number`. The one population that has no real interface data is the
    // FABRICATED class, and it is served precisely at the bottom of this
    // function rather than by admitting three component types unconditionally.
    // docs/known-issues/jdk-only/W8-C16-1-serializable-cloneable-are-not-object.md
    if component == "Ljava/lang/Object;" {
        return true;
    }

    // Element is itself an array → use the array-vs-array assignability rules,
    // treating the component descriptor as the target.
    if shared.mem.heap.kind_of(value_ref) == cratonvm_types::ObjectKind::Array {
        let elem_desc = match array_descriptor_of(shared, value_ref) {
            Some(d) => d,
            None => return true,
        };
        // `array_is_assignable_to` wants the target as an array descriptor or a
        // class/interface name. A reference component "L...;" must be unwrapped
        // to a bare class name; an array component "[..." is passed verbatim.
        if component.starts_with('[') {
            return array_is_assignable_to(shared, &elem_desc, component);
        }
        if component.starts_with('L') && component.ends_with(';') {
            let comp_name = &component[1..component.len() - 1];
            return array_is_assignable_to(shared, &elem_desc, comp_name);
        }
        return true;
    }

    // Element is a plain object and the component is an ARRAY type. This is the
    // one narrowing in this function that needs no type information to be
    // sound: the only instances of an array type are arrays (JLS §10.7), and
    // the branch above already established that the heap does not call this
    // value an array. So the store is illegal whatever the component resolves
    // to, and there is no imprecision for a fail-open to hedge against.
    //
    // It used to return `true` here with the other unresolvable cases, which
    // let a `String[][]` be handed an `Integer` (MEASURED 2026-08-28,
    // `probes/DodArrayStoreSweep`, both modes: HotSpot throws
    // `ArrayStoreException`, this VM produced the array).
    if component.starts_with('[') {
        return false;
    }

    // Any other non-`L...;` component IS imprecision — fail open rather than
    // throw, as everywhere else in this function.
    if !(component.starts_with('L') && component.ends_with(';')) {
        return true;
    }
    let comp_name = &component[1..component.len() - 1];

    // Resolve the element's runtime class id.
    //
    // `ClassId(0)` is NOT "unknown". It is `java/lang/Object`: it is the first
    // class this VM loads (`ClassManager::bootstrap_core_classes` puts it first
    // and `ClassStore::add` hands out dense ids from zero), and that identity is
    // measured, not inferred — `vm/src/native/jni.rs`'s `jclass` encoding note
    // records `FindClass("java/lang/Object")` returning `(nil)` from a C probe
    // for exactly this reason.
    //
    // This arm used to `return true` on it unconditionally, described in-comment
    // as "an unknown/synthetic class id (no loaded class entry)". That
    // description was false, and the cost was the entire
    // `<interface>[] <- new Object()` family: HotSpot 25.0.3 throws
    // `ArrayStoreException: java.lang.Object` for `Comparable[] <- Object`, and
    // this predicate answered `true` without ever looking at the component.
    // Because it sits ABOVE the interface arm further down, it — not that arm —
    // is what admitted `RArrayStoreTiers`' `s02`.
    //
    // The job the comment CLAIMED is done properly ~30 lines below by
    // `cm.get_class(value_class_id).is_none()`, which asks the class store
    // instead of pattern-matching an id.
    //
    // What genuinely still needs a hatch here is the OTHER face of `ClassId(0)`:
    // the all-zero header the collector leaves over a reclaimed span reads as
    // `java.lang.Object` too (the H2-CID0 family — see
    // `vm/src/memory/reclaim_guard.rs`). `reclaimed_hole_at` is the precise
    // successor for that exact ambiguity and it has no false positives — a live
    // object is never inside a free block, never past the allocation frontier
    // and never in the inactive semispace — so ask it rather than failing open
    // on every genuine `new Object()`. It takes the heap locks, but only a value
    // that is BOTH `ClassId(0)` AND bound for a non-`Object[]` component gets
    // this far: an `Object[]` component returned above, and an ARRAY value
    // (every primitive array header also carries `ClassId(0)`, having no
    // component class) returned in the block above that. `Serializable[]` and
    // `Cloneable[]` components used to return above as well and now reach here
    // — that is the whole added cost of splitting that arm, and it is the same
    // two heap locks on a component type that is rare in real bytecode.
    let value_class_id = shared.mem.heap.class_id_of(value_ref);
    if value_class_id == ClassId::new(0)
        && shared
            .mem
            .heap
            .reclaimed_hole_at(value_ref.as_ptr() as usize)
            .is_some()
    {
        return true;
    }
    let array_component_class_id = shared.mem.heap.class_id_of(array_ref);
    let comp_id = {
        let cm = shared.classes.class_manager.read();
        if array_component_class_id != ClassId::new(0) {
            cm.get_class(array_component_class_id)
                .filter(|c| &*c.name == comp_name)
                .map(|_| array_component_class_id)
        } else {
            None
        }
    }
    .or_else(|| {
        shared
            .classes
            .class_manager
            .read()
            .find_class_by_name_for_class(comp_name, array_component_class_id)
    })
    .unwrap_or_else(
        || match shared.classes.class_manager_write().load_class(comp_name) {
            Ok(id) => id,
            Err(_) => ClassId::new(0),
        },
    );
    if comp_id == ClassId::new(0) {
        return true;
    }
    // The element class must exist in the hierarchy; if not, fail open.
    {
        let cm = shared.classes.class_manager.read();
        if cm.get_class(value_class_id).is_none() {
            return true;
        }
        // ONE by-name walk, not two checks.
        //
        // `array_descriptor_of` preserves only the component *name*, not its
        // defining-loader ClassId. When a forked loader owns a same-named copy,
        // the global lookup above can resolve the app copy and make a valid
        // `ChildSegment[] <- ChildSegment` store look incompatible — so the
        // component identity is ambiguous here and the predicate's documented
        // fail-open posture applies. The same is true one level up: the stored
        // value can be a *subclass* whose recorded superclass edge points at
        // another same-named mirror.
        //
        // This walk covers both, because it starts at `value_class_id` itself:
        // its first iteration is exactly the `value_class.name == comp_name`
        // test that used to sit above it as a separate early return. That early
        // return was measured to be dead weight — disabling the walk fails
        // `aastore_fails_open_across_a_split_loaders_two_copies_of_one_name`,
        // disabling the early return changes nothing — and two checks that read
        // as independent defences when only one of them can ever fire is worse
        // than one check that says what it does.
        let mut current = Some(value_class_id);
        while let Some(id) = current {
            let Some(class) = cm.get_class(id) else {
                break;
            };
            if &*class.name == comp_name {
                return true;
            }
            current = class.superclass;
        }
        // Provably assignable iff the value's class is a subtype of the
        // component class. `is_subclass_of` walks BOTH the superclass chain and
        // the implemented-interface DAG, transitively through super-interfaces
        // (`classloading/src/class.rs`, `is_subclass_of_inner`), so it answers an
        // INTERFACE component unaided: `Comparable[] <- Integer` is `true` here
        // because `Integer` IMPLEMENTS `Comparable`. That is a different
        // relation from extending it, and it is the one a fix phrased as "is the
        // value a subclass of the component" gets wrong.
        if cm.is_subclass_of(value_class_id, comp_id) {
            return true;
        }
        // An INTERFACE component is NOT a reason to fail open.
        //
        // A blanket `if comp.is_interface() { return true }` used to sit here,
        // ABOVE the `is_subclass_of` call, citing dynamic proxies, annotation
        // proxies and synthetic classes — populations that acquire interfaces at
        // runtime, invisibly to the static hierarchy. Every one of those is now
        // served by a more precise successor BELOW this point (the synthetic-id
        // range test, the `$Proxy`/`AnnotationProxy` name test,
        // `class_chain_reaches_proxy_instance`, `synthetic_implements`), each
        // added after the blanket and none of them reachable while it stood. It
        // got the legal stores right for the same reason it got the illegal ones
        // wrong: it never looked.
        //
        // Measured, HotSpot 25.0.3 vs. this VM under `--nojit` — i.e. this
        // predicate, not the JIT (`regression-suite/src/RArrayStoreTiers.java`,
        // 2026-08-12):
        //   s03 `Runnable[]   <- String`  HotSpot ArrayStoreException, here no-throw
        //   s02 `Comparable[] <- Object`  HotSpot ArrayStoreException, here no-throw
        // s02 was admitted by the `ClassId(0)` arm further up, which fires first;
        // removing this blanket alone would not have moved it. The legal
        // neighbours that must keep passing are s08 `Comparable[] <- Integer` and
        // s09 `Runnable[] <- lambda`. See
        // docs/known-issues/jdk-only/W7-101-aastore-interface-component-blanket.md.
        //
        // The one thing the blanket did provide, restored precisely: an interface
        // component is the relation the by-name superclass walk above cannot
        // cover, because interfaces are not on the superclass chain. Under a
        // split loader (`@CompileWithForkedClassLoader`) the value's `interfaces`
        // vector can name the OTHER loader's copy of the component, and
        // `is_subclass_of` compares ClassIds, so it refuses a legal store. Ask
        // the loader-blind walk that already exists for exactly that shape in JIT
        // `checkcast`/`instanceof`: it walks supers AND interfaces BY NAME.
        //
        // It subsumes the superclass walk above — read the two as one check with
        // a hot, allocation-free fast path, not as independent defences. And it
        // cannot rescue the two rows above: `java/lang/Object` reaches no
        // `java/lang/Comparable` node under any name, and `java/lang/String`
        // reaches no `java/lang/Runnable`.
        if cm.is_assignable_to_name(value_class_id, comp_name) {
            return true;
        }
    }
    // Both types resolved and the element is NOT a subtype of the component:
    // potentially an ArrayStoreException. Several escape hatches keep us from
    // throwing a SPURIOUS one on the VM's imprecise synthetic / dynamic-proxy
    // types, whose true interface set is not recorded in the static hierarchy:
    //   - synthetic lambda/proxy class ids (>= 0x8000_0000) never appear in the
    //     loaded hierarchy, so `is_subclass_of` cannot vouch for them.
    if value_class_id.as_u32() >= 0x8000_0000 {
        return true;
    }
    //   - dynamic proxies (java.lang.reflect.Proxy instances) and annotation
    //     proxies (java/lang/annotation/AnnotationProxy) implement their target
    //     interfaces at RUNTIME, invisibly to `is_subclass_of`. Storing one into
    //     an interface[] is legal on a real JVM (regression: hibernate-smoke
    //     stored an AnnotationProxy into an annotation-type array). Fail open —
    //     but only where the VM genuinely has no record of the interface set.
    //     `recorded_proxy_interface_set` is what separates the two cases, and
    //     the block below states why.
    //
    // Set by the block below when the value is a generated `$ProxyN` whose
    // interface set the VM RECORDED, so the `class_chain_reaches_proxy_instance`
    // hatch further down — which matches the very same values by their
    // SUPERCLASS — does not re-admit what this block just declined. Two
    // fail-open guards that can produce the same wrong answer is exactly the
    // shape `W7-101` §3 was written about: fixing the one you found does not
    // tell you whether it was the one that fired. Here the name test fires
    // first and the chain walk fires immediately after it, so BOTH had to move.
    let mut proxy_interface_set_is_recorded = false;
    {
        let cm = shared.classes.class_manager.read();
        if let Some(cls) = cm.get_class(value_class_id) {
            let vn: &str = &*cls.name;
            // A generated `$ProxyN` whose interface set is RECORDED is not a
            // case of imprecise type information, and the fail-open contract
            // does not reach it.
            //
            // MEASURED on HotSpot 25.0.3+9-LTS (`scratchpad/g12/ProxyStoreProbe.java`,
            // one execution per shape). The oracle has NO proxy rule for
            // `aastore` at all — a proxy is an ordinary class whose interface
            // set is the argument list `Proxy.newProxyInstance` was given and
            // whose superclass is `java.lang.reflect.Proxy`:
            //
            //   A[]            <- proxy(A)      OK
            //   B[]            <- proxy(A)      ArrayStoreException: $Proxy0
            //   Runnable[]     <- proxy(A)      ArrayStoreException: $Proxy0
            //   A[] and B[]    <- proxy(A,B)    OK, both
            //   SuperI[]       <- proxy(SubI)   OK      (superinterface, transitively)
            //   SubI[]         <- proxy(SuperI) ArrayStoreException: $Proxy3
            //   Object[]       <- proxy(A)      OK
            //   Serializable[] <- proxy(A)      OK      (`Proxy implements Serializable`)
            //   Cloneable[]    <- proxy(A)      ArrayStoreException: $Proxy0
            //   Proxy[]        <- proxy(A)      OK      (its superclass)
            //   $Proxy0[]      <- proxy(A)      OK      (its own class)
            //   $Proxy1[]      <- proxy(A)      ArrayStoreException: $Proxy0
            //
            // `Serializable` really is universal here and it is not a proxy
            // rule: `java.lang.reflect.Proxy.class.getInterfaces()` is
            // `[java.io.Serializable]` (measured, same probe), so every proxy
            // inherits it through its superclass. `Cloneable` is the control
            // that proves it is inheritance and not a blanket.
            //
            // The blanket this replaces was `vn.contains("$Proxy")`, and it is
            // what `RArrayStoreInterfaces` s12 measured: `Runnable[] <-
            // Proxy(Marker)` answered no-throw in BOTH tiers where HotSpot
            // throws, while s25 `Marker[] <- Proxy(Marker)` was right for free.
            // The blanket never looked at the interface set — and by this point
            // in the function it does not have to guess at one: the class was
            // defined from `proxy_gen`'s real bytes with
            // `interface_id_overrides` carrying the exact `ClassId`s the caller
            // passed to `Proxy.newProxyInstance`
            // (`native-builtins/src/reflect_annotations.rs`,
            // `define_or_get_proxy_class`), and `classify_defined_origin`
            // records them on `ClassOrigin::GeneratedProxy`.
            //
            // The walk below is deliberately NOT a second opinion on
            // `is_subclass_of`/`is_assignable_to_name` above — it is the same
            // question asked of the ORIGIN's id list rather than the `Class`'s
            // `interfaces` vector, so a proxy stays admitted into an array of
            // an interface it really has even if those two vectors ever drift.
            // It can only ADMIT; the refusal comes from falling through it.
            if let Some(ifaces) = recorded_proxy_interface_set(&cls.origin) {
                proxy_interface_set_is_recorded = true;
                // Inherited from `java.lang.reflect.Proxy` itself, in every
                // mode and under `CRATONVM_REAL_PROXY_SUPER=0` (where the super
                // is the `Proxy$Instance` shim and carries no interfaces of its
                // own). `class_name_is_proxy_super` is the same shared answer
                // the dispatch and chain-walk sites use, so the two spellings
                // cannot drift apart.
                if comp_name == "java/io/Serializable" || class_name_is_proxy_super(comp_name) {
                    return true;
                }
                for &iface_id in ifaces {
                    // By NAME, transitively through super-interfaces — the same
                    // loader-blind walk the component check above uses, for the
                    // same split-loader reason: a proxy's recorded interface id
                    // may be the other loader's copy of the component's name.
                    if cm.is_assignable_to_name(iface_id, comp_name) {
                        return true;
                    }
                }
            }
            // The name blanket, now scoped to the population it was written
            // for: a proxy-shaped value whose interface set this VM does NOT
            // hold. That population is real and is NOT the generated-proxy
            // population — `ensure_synthetic_class` fabricates a `$ProxyN` with
            // `ClassOrigin::GeneratedProxy { interfaces: [] }` (pinned by
            // `classloading/tests/jdk_only_class_origin.rs`,
            // `fabricated_generated_names_are_not_compatibility_stubs`), which
            // is precisely "a proxy name with no interface data" and still
            // fails open here. So does the `AnnotationProxy` handler carrier,
            // whose interface set lives on the heap object rather than the
            // class (see `annotation_proxy_satisfies_target`).
            if !proxy_interface_set_is_recorded
                && (vn == "java/lang/annotation/AnnotationProxy"
                    || vn.ends_with("AnnotationProxy")
                    || vn.contains("$Proxy"))
            {
                return true;
            }
            //   - a FABRICATED class against `Serializable[]` / `Cloneable[]`.
            //
            // This is the other half of splitting the `Object`/`Serializable`/
            // `Cloneable` arm at the top of this function, and it is the half
            // that keeps the contract: this predicate must never produce a
            // FALSE `ArrayStoreException`.
            //
            // A `ClassOrigin::CompatibilityStub` is a stand-in minted because
            // the real class bytes were not found — the whole JDK in
            // synthetic-JDK mode, and any missing class in real-JDK mode. Its
            // interface vector is whatever `class_manager.rs`'s `jdk_interfaces`
            // table declares, which is a curated list and not the class file:
            // it names `java/lang/Cloneable` for exactly TWO classes
            // (`Hashtable`, `Properties`), while HotSpot has it on 81 classes
            // in `java.base`'s `java.util`/`java.lang`/`java.io`/`java.text`
            // packages alone (MEASURED, `scratchpad/c16/score.rs` over a table
            // generated from the runtime image). Without this arm, storing a
            // fabricated `ArrayList` into a `Cloneable[]` would start throwing
            // where the real JVM stores it happily.
            //
            // Deliberately NOT a re-run of the interface blanket `W7-101`
            // deleted. It is scoped three ways: to these two component types,
            // to values whose class the VM admits it fabricated, and to the
            // point AFTER `is_subclass_of` and `is_assignable_to_name` have
            // both declined — so a stub that DOES declare the interface is
            // answered by the hierarchy and never reaches here.
            //
            // `java/lang/Object` is excluded, and that exclusion is the point.
            // A fabricated `Object` is not a case of missing information:
            // `java.lang.Object` implements NO interfaces, which is the
            // definition of the root type and not a fact about a class file. So
            // the two rows that motivated this whole change —
            // `Serializable[] <- new Object()` and `Cloneable[] <- new Object()`,
            // both `ArrayStoreException` on HotSpot — are now refused in BOTH
            // modes, not just where real class bytes exist. Without this line
            // synthetic-JDK mode would keep admitting them, because in that mode
            // `java/lang/Object` is itself a stub.
            //
            // What it costs: `Cloneable[] <- <fabricated Integer>` stays
            // admitted in synthetic-JDK mode, where HotSpot throws. That is the
            // fail-open direction this function is required to prefer, it is
            // bounded by the stub population rather than applying to every
            // value, and it shrinks every time `synthetic_implements`' marker
            // lists below grow — a stub that IS on those lists never reaches
            // here, and a stub that is not is a class this VM genuinely has no
            // interface data for.
            if (comp_name == "java/io/Serializable" || comp_name == "java/lang/Cloneable")
                && vn != "java/lang/Object"
                && cls.origin.is_compatibility_stub()
            {
                return true;
            }
        }
    }
    //   - a value that sits under a proxy SUPERCLASS. Same population as the
    //     name test above, reached by a different road, so it carries the same
    //     scope: a generated `$ProxyN` whose interface set the VM recorded has
    //     already been decided, and re-admitting it here would make the block
    //     above inert. Everything else that reaches this line — the
    //     `Proxy$Instance` shim allocated by `ProxyClassOutcome::Degrade` and
    //     `::Failed`, synthetic-JDK mode's fabricated proxies, a subclass of
    //     either — genuinely has no recorded interface set and still fails
    //     open. `class_chain_reaches_proxy_instance` itself is unchanged: it is
    //     `pub(crate)` and four other callers ask it a different question.
    if !proxy_interface_set_is_recorded
        && class_chain_reaches_proxy_instance(shared, value_class_id)
    {
        return true;
    }
    //   - synthetic classes whose interface relationships are name-based only
    //     (HashMap$Entry, etc.) honour the existing name-based fallback.
    //     Reached by a generated proxy too, and it declines for one: no arm of
    //     it keys on a `$ProxyN` name, and the `AnnotationProxy` arm it does
    //     have matches by exact class name, not by shape.
    if synthetic_implements(shared, value_class_id, comp_name) {
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Helper: walk a class's superclass chain by name to detect Proxy$Instance
// ---------------------------------------------------------------------------

/// Does `name` name a class that a dynamic-proxy receiver can sit under?
///
/// **The two names are not alternatives — which one matches is decided by a
/// flag, and the flag defaults to the SECOND.** `native_builtins::
/// reflect_annotations::proxy_super_class_name()` (`:3767`) returns
/// `"java/lang/reflect/Proxy"` when `real_proxy_super()` is true and
/// `"java/lang/reflect/Proxy$Instance"` otherwise, and `real_proxy_super` is
/// `truthy_word_default_true(src, "CRATONVM_REAL_PROXY_SUPER")`
/// (`types/src/flags.rs:1907`). `build_proxy_spec_for` writes exactly that
/// string into `ProxyClassSpec::super_class` (`reflect_annotations.rs:4303`).
/// So in the SHIPPING configuration a generated `$ProxyN` extends the real
/// `java/lang/reflect/Proxy` and `Proxy$Instance` appears nowhere on its
/// chain: matching only `Proxy$Instance` recognises **no proxy at all**.
///
/// That is why this is a shared `fn` and not two `const`s copied per site.
/// The wide match is **required**, not lenient — the question "is this an
/// over-match?" has the answer "the second arm IS the default arm". The
/// `Proxy$Instance` arm is the one that is now conditional: it serves
/// `CRATONVM_REAL_PROXY_SUPER=0`, the `ProxyClassOutcome::Degrade`/`Failed`
/// fallbacks (which allocate the 3-slot shim *regardless* of the gate,
/// `reflect_annotations.rs:3187`), and synthetic-JDK mode, where the real
/// `java/lang/reflect/Proxy` is a fabricated stub. Both must stay: asking
/// "what serves the class per mode" gives two different answers here, and a
/// narrowing that keeps either one alone breaks the other mode silently.
///
/// **Name-only, and deliberately so.** The one caller that cannot use
/// [`class_chain_reaches_proxy_instance`] is the vtable fast path in
/// `dispatch_virtual.rs`, which holds a live `class_manager` read guard: a
/// nested second read acquisition self-deadlocks under parking_lot's
/// writer-preferring fairness. This predicate takes no lock, so that caller
/// can walk the chain with its own guard and still ask the one question.
///
/// **Known drifted twin, 2026-08-13 (F32).** `dispatch_virtual.rs:478`
/// inlines this walk with `const PROXY_INSTANCE` **only**. It was written
/// 2026-07-02 (`b6805d3df`, "proxy default-method dispatch") when
/// `Proxy$Instance` was the only super, and it was not moved when the
/// real-super gate landed default-on. Consequence, by reading: for a
/// real-super `$ProxyN` that guard computes `is_proxy = false`, so the vtable
/// fast path does **not** cede to `execute_invoke_kind`'s `is_proxy_dispatch`
/// — it resolves the generated method on the receiver's own class and
/// installs a `CachedInvokeTarget::VirtualBytecode`. See
/// `docs/known-issues/jdk-only/F32-1-the-proxy-route-and-the-drifted-twin-20260813.md`.
pub(super) fn class_name_is_proxy_super(name: &str) -> bool {
    // The synthetic 3-slot shim: `CRATONVM_REAL_PROXY_SUPER=0`, the
    // degrade/failed fallbacks, and synthetic-JDK mode.
    const PROXY_INSTANCE: &str = "java/lang/reflect/Proxy$Instance";
    // The real JDK base class — the DEFAULT super of every generated
    // `$ProxyN`, and the only name on a shipped proxy's chain.
    const REAL_PROXY_BASE: &str = "java/lang/reflect/Proxy";
    name == PROXY_INSTANCE || name == REAL_PROXY_BASE
}

/// WP2.5 — walks the superclass chain of `class_id` looking for
/// `java/lang/reflect/Proxy$Instance` or the real-JDK
/// `java/lang/reflect/Proxy` base class. Returns `true` if found within
/// `MAX_DEPTH` hops.
///
/// Used by the cast/instanceof and dispatch hooks below to extend their
/// "is this a proxy?" check from a literal name match to "literal match
/// OR extends `Proxy$Instance`" — matters once the WP2.5-A bytecode
/// emitter starts producing per-(loader, ifaces) `$ProxyN` classes that
/// extend `Proxy$Instance` (and the receiver's runtime class is the
/// generated `$ProxyN`, not the abstract super). The literal-name fast
/// path stays in the caller; this helper only runs on the slow path so
/// the cost is zero on every non-proxy dispatch.
///
/// We could call `class_manager.is_subclass_of(child, parent_id)`, but
/// that needs the ClassId of `Proxy$Instance` which forces a
/// `load_class("Proxy$Instance")` on the slow path. Walking by name is
/// simpler, lock-scoped, and depth-bounded against pathological cycles
/// in user-loaded class graphs.
pub(crate) fn class_chain_reaches_proxy_instance(shared: &SharedVm, class_id: ClassId) -> bool {
    const MAX_DEPTH: usize = 32;

    let cm = shared.classes.class_manager.read();
    let mut current = Some(class_id);
    for _ in 0..MAX_DEPTH {
        let cid = match current {
            Some(c) => c,
            None => return false,
        };
        let class = match cm.get_class(cid) {
            Some(c) => c,
            None => return false,
        };
        if class_name_is_proxy_super(&class.name) {
            return true;
        }
        // Stop early once we hit Object — Proxy$Instance sits below it
        // by construction, so going further is wasted work.
        if &*class.name == "java/lang/Object" {
            return false;
        }
        current = class.superclass;
    }
    false
}

/// The interface set a generated `$ProxyN` was created for, when the VM
/// positively RECORDED it — `None` when it did not.
///
/// This is the discriminator that lets `aastore_element_assignable` stop
/// failing open on every `$Proxy`-named value without losing the population
/// that blanket was written for. The two are genuinely different populations
/// and the origin is what separates them:
///
/// * `define_or_get_proxy_class` (`native-builtins/src/reflect_annotations.rs`)
///   emits real `proxy_gen` bytes and passes `interface_id_overrides` — the
///   exact `ClassId`s handed to `Proxy.newProxyInstance` — so
///   `classify_defined_origin` records a NON-EMPTY list here. The proxy's
///   interface set is not missing information; it is the proxy's whole
///   identity, stated by the code that generated it.
/// * `ensure_synthetic_class` fabricating a `$ProxyN` by name records
///   `GeneratedProxy { interfaces: [] }` — pinned by
///   `classloading/tests/jdk_only_class_origin.rs`'s
///   `fabricated_generated_names_are_not_compatibility_stubs`, which asserts
///   exactly that origin for `com/example/$Proxy42`. An empty list is "no
///   record", not "implements nothing", so it answers `None` and the caller
///   keeps failing open. `vm/src/runtime/interpreter/tests.rs`'s
///   `jdk/proxy3/$Proxy27` fixture is this shape, and so is the
///   `AotIntegrationTests` / Spring `TypeMappedAnnotation.adapt` store it
///   stands for.
///
/// Emptiness is therefore load-bearing and not defensive: dropping the
/// `is_empty` test would turn every fabricated proxy name into a refusal.
///
/// Not a method on `ClassOrigin` because the emptiness rule is this predicate's,
/// not the origin taxonomy's — `--dump-class-origins` reports an empty
/// `generated-proxy` as a generated proxy and is right to.
fn recorded_proxy_interface_set(origin: &cratonvm_classloading::ClassOrigin) -> Option<&[ClassId]> {
    match origin {
        cratonvm_classloading::ClassOrigin::GeneratedProxy { interfaces }
            if !interfaces.is_empty() =>
        {
            Some(&**interfaces)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Helper: name-based type compatibility for synthetic classes
// ---------------------------------------------------------------------------

/// Does the innermost simple name END with `term` as its final word?
///
/// The FAMILY of a JDK collection class is its last word, not any word:
/// `ConcurrentSkipListSet` is a `Set` and `ConcurrentSkipListMap` is a `Map`,
/// and both carry `List` in the middle because a skip list is how they are
/// built. [`simple_name_has_word`] cannot tell those apart — it is deliberately
/// an any-word test, and its over-admission score is measured as such — so the
/// call sites that need the family use this to exclude, never to admit.
///
/// Kept separate from `simple_name_has_word` rather than folded into it because
/// that function's 50/266 score is asserted by `scratchpad/c16/verify.rs`
/// against a verbatim extract; narrowing it would move numbers a probe pins.
fn simple_name_ends_with_word(obj_name: &str, term: &str) -> bool {
    let simple = match obj_name.rfind(['$', '/']) {
        Some(i) => &obj_name[i + 1..],
        None => obj_name,
    };
    simple.ends_with(term)
}

/// Does the INNERMOST SIMPLE name of `obj_name` contain `term` as a camel-case
/// word?
///
/// This is the whole of `synthetic_implements`' collection-family heuristic, and
/// it replaced `obj_name.contains(term)` over the FULL binary name. Two things
/// were wrong with the full-name test, and both are the same mistake:
///
/// 1. **A container lends its name to every member.** `java/util/Collections$*`
///    and `java/util/ImmutableCollections$*` all contain "Collection" through
///    the enclosing class, so `ImmutableCollections$Map1` (a `Map`) answered
///    `instanceof Collection` — the `W8-C4-2` defect — and so did
///    `ImmutableCollections$Access`, `$HasStableDelegates` and `$StableMap`,
///    which that fix did not reach. Naming the containers one at a time is what
///    produced that record; the simple name closes the whole family at once.
/// 2. **A cursor is not a collection.** `ArrayList$Itr`,
///    `ArrayDeque$DeqSpliterator`, `SetN$SetNIterator` — every iterator and
///    spliterator inherits its owner's name and was admitted as a `Collection`.
///
/// And "contains" is not "is": `AbstractQueuedSynchronizer` contains "Queue" but
/// is not one; `TooManyListenersException` contains "List". Requiring the term
/// to end the name or be followed by another capitalised word rules those out
/// without losing `WorkQueue` or `ListResourceBundle`.
///
/// MEASURED, over all 3,462 classes of `java.base`'s
/// `java.util`/`java.lang`/`java.io`/`java.math`/`java.text`/`java.time`/
/// `java.net`/`java.security`/`java.nio` packages, name list and every oracle
/// cell generated from the runtime image itself (`scratchpad/c16/GenJrt.java`
/// walks `jrt:/`, `scratchpad/c16/score.rs` scores it under plain `rustc`), for
/// the six targets this arm decides (`Collection`, `List`, `Set`, `Queue`,
/// `Deque`, `Iterable` — 20,772 name×target cells):
///
/// ```text
///                      OVER-ADMISSIONS   correct admissions
///   full-name contains       410                282
///   simple name + word        50                266
/// ```
///
/// `scratchpad/c16/verify.rs` re-scores THIS function — extracted verbatim, not
/// paraphrased — and asserts those two numbers, so the comment cannot drift from
/// the code without the probe going red.
///
/// Only OVER-ADMISSIONS (this fallback says yes, HotSpot says no) are defects.
/// It can only ADMIT — it runs after `is_subclass_of` has already declined — so
/// a `false` is "no opinion", and summing both directions reports hundreds of
/// bogus "wrong" rows and points at the wrong code. That correction is the one
/// thing to carry forward if this heuristic is revisited.
///
/// The 16 correct admissions lost are inner classes whose OWNER's name carried
/// the meaning (`ReverseOrderListView$Rand`, `CopyOnWriteArrayList$Reversed`,
/// `ConcurrentSkipListMap$Values`, …). None of them is a name the synthetic-JDK
/// fabrication tables in `classloading/src/class_manager.rs` mention, so none is
/// reachable through this fallback in practice — checked, not assumed.
///
/// The 50/266 score is this FUNCTION's, and since 2026-09-02 it is no longer
/// the caller's: the `java/util/` arms in [`synthetic_implements`] additionally
/// exclude a name whose FINAL word contradicts the target
/// ([`simple_name_ends_with_word`]), which is strictly narrower. Re-score the
/// call site, not this helper, if the question is what the VM admits.
///
/// docs/known-issues/jdk-only/W8-C16-2-synthetic-implements-simple-name.md
fn simple_name_has_word(obj_name: &str, term: &str) -> bool {
    let simple = match obj_name.rfind(['$', '/']) {
        Some(i) => &obj_name[i + 1..],
        None => obj_name,
    };
    // A cursor over a collection is never the collection.
    if simple.ends_with("Iterator")
        || simple.ends_with("Spliterator")
        || simple.ends_with("Itr")
        || simple.ends_with("Iter")
    {
        return false;
    }
    let (b, t) = (simple.as_bytes(), term.as_bytes());
    if t.is_empty() || b.len() < t.len() {
        return false;
    }
    (0..=b.len() - t.len()).any(|i| {
        &b[i..i + t.len()] == t && {
            let after = i + t.len();
            after == b.len() || b[after].is_ascii_uppercase() || b[after].is_ascii_digit()
        }
    })
}

/// Synthetic classes (HashMap$Entry, etc.) may not have proper interface
/// relationships in the ClassStore because they were created in Rust without
/// loading a real .class file. This function provides a name-based fallback
/// for common patterns where a concrete inner class should satisfy an interface.
pub(super) fn synthetic_implements(
    shared: &SharedVm,
    obj_class_id: ClassId,
    target_class_name: &str,
) -> bool {
    let obj_class_name = shared
        .classes
        .class_manager
        .read()
        .get_class(obj_class_id)
        .map(|c| c.name.to_string());
    let obj_name = match obj_class_name {
        Some(n) => n,
        None => return false,
    };

    if obj_name == "java/lang/foreign/DowncallHandle"
        && target_class_name == "java/lang/invoke/MethodHandle"
    {
        return true;
    }

    // `System.getLogger()` returns a `cratonvm/internal/SystemLogger`, a
    // CONCRETE synthetic class. Concrete is the whole point: the receiver used
    // to be allocated with the `java/lang/System$Logger` INTERFACE as its
    // class, which left it inert from both directions at once — native lookup
    // drops interface-declared instance natives, and the interface's own
    // methods are abstract, so neither a native nor bytecode served it.
    //
    // The cost of that fix is that the receiver no longer carries the
    // interface in its type hierarchy, so `instanceof System.Logger` and
    // `checkcast` would fail. Nothing hits that today (javac emits no
    // checkcast for `System.getLogger(..)`, whose declared return type is
    // already `System.Logger`), but a caller that stashes one in an `Object`
    // and casts it back would — and that failure would be baffling. Declare
    // the relationship, which is in fact true.
    if obj_name == "cratonvm/internal/SystemLogger"
        && target_class_name == "java/lang/System$Logger"
    {
        return true;
    }

    // Every `MemorySegment` CratonVM mints is a
    // `cratonvm/internal/foreign/MemorySegmentImpl`
    // (`native-builtins`' `panama::CRATON_SEGMENT_CLASS`). Same shape as the
    // logger above and adopted for the same reason — the receiver used to be
    // stamped with the `java/lang/foreign/MemorySegment` INTERFACE — but here
    // the missing relationship was not hypothetical: it is the whole defect.
    //
    // The JDK does not consume a segment through the interface. Its own
    // internals cast it down, and `jdk.incubator.vector` does so on EVERY
    // segment entry point:
    //
    //     ShortVector.fromMemorySegment0Template   pc 22: checkcast AbstractMemorySegmentImpl
    //     IntVector.intoMemorySegment0Template     pc 23: checkcast AbstractMemorySegmentImpl
    //
    // `AbstractVector.defaultReinterpret` reaches the second of those for every
    // `reinterpretAsInts()` — it round-trips the vector through a scratch
    // `MemorySegment.ofArray(new byte[n])` — so GPULlama3's first `matmul` died
    // on a segment CratonVM had minted three frames earlier. Nothing about that
    // is Vector-API-specific: it is every JDK consumer that touches the FFM
    // internals.
    //
    // Two relationships, and both are needed. `MemorySegment` is what user code
    // casts to; `AbstractMemorySegmentImpl` is what the JDK's own code casts to.
    // `SegmentAllocator` comes with the second (the abstract class implements
    // it) and is what `SegmentAllocator.allocate*` call sites check.
    //
    // NOT done by giving the class a real superclass in
    // `class_manager::fabricate_class` — the shape `cratonvm/synthetic/Process`
    // uses. A fabricated class gets `first_field_index: 0`, so
    // `AbstractMemorySegmentImpl`'s `length`/`readOnly`/`scope` would alias
    // slots 0/1/2 of CratonVM's carrier — which hold `ptr`, `size` and `arena` —
    // and `resolve_field_index_in_hierarchy` would start answering those names
    // with the wrong values for the three readers in `native-builtins` that ask
    // by name. See `panama::CRATON_SEGMENT_CLASS` for the full measurement.
    if obj_name == "cratonvm/internal/foreign/MemorySegmentImpl" {
        return matches!(
            target_class_name,
            "java/lang/foreign/MemorySegment"
                | "jdk/internal/foreign/AbstractMemorySegmentImpl"
                | "java/lang/foreign/SegmentAllocator"
        );
    }

    // The `BufferPoolMXBean` instances CratonVM hands out
    // (`native-builtins`' `jmx::CRATON_BUFFER_POOL_CLASS`), same shape and same
    // reason as the two above: the receiver used to be stamped with the
    // `java/lang/management/BufferPoolMXBean` interface, so it had no concrete
    // methods and native lookup drops interface-declared instance natives.
    //
    // `PlatformManagedObject` is the supertype `ManagementFactory
    // .getPlatformMXBeans` is generic over, and the one whose `checkcast` a
    // caller storing the result in a `PlatformManagedObject` variable emits.
    if obj_name == "cratonvm/internal/BufferPool" {
        return matches!(
            target_class_name,
            "java/lang/management/BufferPoolMXBean"
                | "java/lang/management/PlatformManagedObject"
                | "jdk/internal/misc/VM$BufferPool"
        );
    }

    // Map.Entry implementations
    if target_class_name == "java/util/Map$Entry" {
        return matches!(
            &*obj_name,
            "java/util/HashMap$Entry"
                | "java/util/HashMap$Node"
                | "java/util/AbstractMap$SimpleEntry"
                | "java/util/AbstractMap$SimpleImmutableEntry"
                | "java/util/TreeMap$Entry"
                | "java/util/LinkedHashMap$Entry"
                | "java/util/Hashtable$Entry"
                | "java/util/WeakHashMap$Entry"
                | "java/util/concurrent/ConcurrentHashMap$Node"
        );
    }

    // Iterator implementations
    if target_class_name == "java/util/Iterator" {
        return obj_name.contains("$Itr")
            || obj_name.contains("$Iterator")
            || obj_name.contains("$KeyItr")
            || obj_name.contains("$ValueItr")
            || obj_name.contains("$EntryItr");
    }

    // `java/io/Serializable` and `java/lang/Cloneable` — the two JLS §10.7
    // marker interfaces.
    //
    // These arms exist because `aastore_element_assignable` stopped admitting
    // `Serializable[]` / `Cloneable[]` unconditionally (HotSpot throws
    // `ArrayStoreException` for `Serializable[] <- Object`, `Cloneable[] <-
    // Object` and `Cloneable[] <- Integer`; measured, `scratchpad/c16/Arm3.java`).
    // Once that arm is gone the answer comes from the class hierarchy, and for a
    // FABRICATED class the hierarchy is `class_manager.rs`'s `jdk_interfaces`
    // table — which names `java/lang/Cloneable` on exactly two classes. So the
    // marker relationships have to be declarable by name like every other
    // relationship in this function.
    //
    // Every name below is MEASURED on HotSpot 25.0.3, not recalled: the lists
    // were checked row-by-row against a table generated from the runtime image
    // (`scratchpad/c16/GenJrt.java` walks `jrt:/modules/java.base` and reads
    // `Serializable.class.isAssignableFrom(c)` for each of 3,462 classes;
    // `scratchpad/c16/score.rs` asserts every listed name is `true` there). That
    // check threw out two entries that "everyone knows" are on these lists:
    //
    //   java/util/WeakHashMap — NOT Serializable, NOT Cloneable
    //                           (`getInterfaces()` is `[java.util.Map]` alone)
    //   java/util/AbstractMap — declares `clone()` but NOT `Cloneable`
    //
    // `WeakHashMap` is worth a second look: `jdk_interfaces` groups it with
    // `HashMap` and hands it `java/io/Serializable`, so synthetic-JDK mode
    // answers `instanceof Serializable` true where HotSpot answers false. That
    // is a defect in a different crate, recorded as a nomination in
    // docs/known-issues/jdk-only/W8-C16-1-serializable-cloneable-are-not-object.md.
    //
    // Placement: ABOVE the `java/util/Collections$` exclusion below, which
    // returns `false` for every target, not just the collection ones — a
    // `Collections$SingletonList` is genuinely `Serializable` and must not be
    // denied by a guard written about the substring "Collection".
    if target_class_name == "java/io/Serializable" || target_class_name == "java/lang/Cloneable" {
        // JVMS §4.10.1.2 / JLS §10.7: EVERY array type implements both. Named
        // here as well as in `array_is_assignable_to_impl` because this function
        // is also reached from `checkcast`/`instanceof` on a raw array class
        // name, which does not go through that path.
        if obj_name.starts_with('[') {
            return true;
        }
        if target_class_name == "java/lang/Cloneable" {
            return matches!(
                &*obj_name,
                "java/util/ArrayList"
                    | "java/util/LinkedList"
                    | "java/util/Vector"
                    | "java/util/Stack"
                    | "java/util/HashMap"
                    | "java/util/LinkedHashMap"
                    | "java/util/TreeMap"
                    | "java/util/IdentityHashMap"
                    | "java/util/EnumMap"
                    | "java/util/Hashtable"
                    | "java/util/Properties"
                    | "java/util/HashSet"
                    | "java/util/LinkedHashSet"
                    | "java/util/TreeSet"
                    | "java/util/ArrayDeque"
                    | "java/util/BitSet"
                    | "java/util/Date"
                    | "java/util/Calendar"
                    | "java/util/GregorianCalendar"
                    | "java/util/Locale"
                    | "java/util/TimeZone"
                    | "java/util/SimpleTimeZone"
                    | "java/text/Format"
                    | "java/text/DateFormat"
                    | "java/text/SimpleDateFormat"
                    | "java/text/NumberFormat"
                    | "java/text/DecimalFormat"
            );
        }
        return matches!(
            &*obj_name,
            "java/lang/String"
                | "java/lang/Integer"
                | "java/lang/Long"
                | "java/lang/Short"
                | "java/lang/Byte"
                | "java/lang/Float"
                | "java/lang/Double"
                | "java/lang/Boolean"
                | "java/lang/Character"
                | "java/lang/Number"
                | "java/lang/Enum"
                | "java/lang/Throwable"
                | "java/lang/Exception"
                | "java/lang/RuntimeException"
                | "java/lang/Error"
                | "java/lang/StringBuilder"
                | "java/lang/StringBuffer"
                | "java/math/BigInteger"
                | "java/math/BigDecimal"
                | "java/io/File"
                | "java/util/ArrayList"
                | "java/util/LinkedList"
                | "java/util/Vector"
                | "java/util/Stack"
                | "java/util/HashMap"
                | "java/util/LinkedHashMap"
                | "java/util/TreeMap"
                | "java/util/IdentityHashMap"
                | "java/util/EnumMap"
                | "java/util/Hashtable"
                | "java/util/Properties"
                | "java/util/HashSet"
                | "java/util/LinkedHashSet"
                | "java/util/TreeSet"
                | "java/util/ArrayDeque"
                | "java/util/PriorityQueue"
                | "java/util/BitSet"
                | "java/util/Date"
                | "java/util/Calendar"
                | "java/util/GregorianCalendar"
                | "java/util/Locale"
                | "java/util/UUID"
                | "java/util/Currency"
                | "java/util/Random"
                | "java/util/TimeZone"
                | "java/util/SimpleTimeZone"
                | "java/util/AbstractMap$SimpleEntry"
                | "java/util/AbstractMap$SimpleImmutableEntry"
                | "java/util/concurrent/ConcurrentHashMap"
                | "java/util/concurrent/CopyOnWriteArrayList"
                | "java/util/concurrent/ConcurrentLinkedQueue"
                | "java/util/concurrent/ConcurrentSkipListMap"
                | "java/util/concurrent/LinkedBlockingQueue"
                | "java/util/concurrent/ArrayBlockingQueue"
                | "java/util/concurrent/atomic/AtomicInteger"
                | "java/util/concurrent/atomic/AtomicLong"
                | "java/util/concurrent/atomic/AtomicBoolean"
                | "java/util/concurrent/atomic/AtomicReference"
                | "java/net/URI"
                | "java/net/URL"
                | "java/net/InetAddress"
                | "java/net/InetSocketAddress"
                | "java/text/SimpleDateFormat"
                | "java/text/DecimalFormat"
                | "java/time/Duration"
                | "java/time/Instant"
                | "java/time/LocalDate"
                | "java/time/LocalDateTime"
                | "java/time/LocalTime"
                | "java/time/ZonedDateTime"
        );
    }

    // Iterable implementations (all Collection types)
    //
    // `java/util/Collections$*` (the utility class's nested helper/view
    // types — SingletonMap, UnmodifiableMap, SingletonSet, ...) must be
    // excluded from this substring probe: "Collections" itself contains the
    // substring "Collection", so e.g. `Collections$SingletonMap` (a Map,
    // NOT a Collection) matched `obj_name.contains("Collection")` below and
    // was misreported as `instanceof java.util.Collection`/`Iterable`. That
    // broke Groovy's `DefaultTypeTransformation.asCollection`, which checks
    // `instanceof Collection` before `instanceof Map` — it took the
    // Collection branch, cast the SingletonMap to Collection unchanged, and
    // called `.iterator()` directly on it, crashing GroovyMarkupConfigurer
    // bean creation with `NoSuchMethodError: Collections$SingletonMap.
    // iterator()`. This name-based fallback only exists for genuinely
    // synthetic classes with no real interface data (see the function doc);
    // every `Collections$*` class reaching this point is either a REAL
    // loaded JDK class (whose hierarchy the earlier `is_subclass_of` check
    // already resolved precisely) or one of CratonVM's own `cratonvm/
    // internal/Unmodifiable*` stamps (which declare their own accurate
    // interfaces in `vm_init.rs`), so it never needs this heuristic.
    //
    // NOTE, 2026-08-13: the "substring probe" the paragraph above and the
    // `ImmutableCollections$Map` paragraph below both describe is no longer a
    // substring of the FULL name — it is `simple_name_has_word` over the
    // innermost simple name, which closes `Collections$SingletonMap` and
    // `ImmutableCollections$Map1` on its own. These two prefix exclusions are
    // KEPT anyway, and what they now decide was measured rather than assumed:
    // 94 cells, of which HotSpot says `true` for 92 (`Collections$CheckedList`,
    // `$EmptySet`, `$AsLIFOQueue`, …). So they cost 92 correct admissions to
    // prevent 2 over-admissions. Removing them is a pure recall change that
    // wants its own vector — not a same-wave add-on to a change no lane could
    // build. docs/known-issues/jdk-only/W8-C16-2-synthetic-implements-simple-name.md
    if obj_name.starts_with("java/util/Collections$")
        // `java.util.ImmutableCollections` is the CONTAINER class of the
        // `Map.of()` / `List.of()` / `Set.of()` family, and its own name
        // contains the substring "Collection" — the same trap the
        // `Collections$` prefix above exists for, missed because the prefix
        // differs by one word. Its Map members reached the
        // `contains("Collection")` term below and were admitted as
        // `instanceof Collection` / `Iterable`, which HotSpot 25 denies
        // (measured: `Map.of("k","v") instanceof Collection` is false, and
        // `(Collection) Map.of("k","v")` throws). CratonVM let the cast through
        // and died four frames later at `ImmutableCollections$Map1.iterator()`.
        //
        // Deliberately narrower than excluding the whole `ImmutableCollections$`
        // family: `List12`/`ListN`/`Set12`/`SetN` are green today in all three
        // arms, they are admitted by this fallback's `List`/`Set` terms rather
        // than by the accident, and moving a green cell is not something to do
        // in a change the authoring lane cannot build.
        //
        // Scope, stated so it is not read as more than it is: this closes the
        // `--jdk-only` (strict) face only. Under `--real-jdk` the VM stamps a
        // `cratonvm/internal/UnmodifiableMap`, which the `java/util/` prefix
        // below already excludes — that mode gets `Collection` right and
        // `AbstractMap` wrong, by a different mechanism, and stays open.
        // docs/known-issues/jdk-only/W8-C4-2-map-of-instanceof-collection.md
        || obj_name.starts_with("java/util/ImmutableCollections$Map")
        || obj_name == "java/util/ImmutableCollections$AbstractImmutableMap"
    {
        return false;
    }
    if target_class_name == "java/lang/Iterable" {
        // `!…ends_with_word("Map")` is the whole difference between this and
        // the version before 2026-09-02, which answered `true` for a
        // `ConcurrentSkipListMap` — a Map is not an Iterable, and the `List` in
        // its name is the data structure it is built from. Measured with
        // `apps/probes/CollectionViewTypes`; see the same guard on the
        // Collection/List arms below.
        return obj_name.starts_with("java/util/")
            && !simple_name_ends_with_word(&obj_name, "Map")
            && (simple_name_has_word(&obj_name, "List")
                || simple_name_has_word(&obj_name, "Set")
                || simple_name_has_word(&obj_name, "Queue")
                || simple_name_has_word(&obj_name, "Deque")
                || simple_name_has_word(&obj_name, "Collection"));
    }

    // Collection / Set / List supertypes. These MUST stay distinct: a Set is
    // not a List and vice-versa. The previous shape admitted any List/Set/Queue
    // name for ALL THREE targets, so `x instanceof List` returned true for a
    // HashSet / RegularEnumSet (name contains "Set") — which broke real code
    // that branches on `parameters instanceof List` (JUnit's
    // Parameterized$RunnersFactory.allParameters over an `EnumSet`-typed
    // @Parameters: it took the List branch and called `enumSet.get(0)` →
    // NoSuchMethodError RegularEnumSet.get(I)). Match each interface only
    // against the class-name family that actually implements it.
    if obj_name.starts_with("java/util/") {
        let has = |term: &str| simple_name_has_word(&obj_name, term);
        // The FAMILY is the LAST word. `has` is an any-word test by design, so
        // these two exclusions are what keep it from reading a class's
        // implementation out of its name — `ConcurrentSkipListSet` and
        // `ConcurrentSkipListMap` are a skip LIST in construction and a `Set`
        // and a `Map` in type. Both were measured wrong here on 2026-09-02
        // (`apps/probes/CollectionViewTypes`, real-JDK mode, against HotSpot
        // 25.0.3): `aConcurrentSkipListSet instanceof List` and
        // `aConcurrentSkipListMap instanceof Collection/List/Iterable` all
        // answered `true`.
        //
        // Excluding rather than switching to a last-word test, which would be
        // the tidier rule and is wrong: `Collections$SetFromMap` ends in `Map`
        // and is a `Set`, so a last-word rule loses it. Excluding only from the
        // arms whose target the suffix contradicts keeps that cell.
        let ends = |term: &str| simple_name_ends_with_word(&obj_name, term);
        match target_class_name {
            "java/util/Collection" | "java/lang/Iterable" => {
                if !ends("Map")
                    && (has("List")
                        || has("Set")
                        || has("Queue")
                        || has("Deque")
                        || has("Collection"))
                {
                    return true;
                }
            }
            "java/util/List" => {
                // Lists only (ArrayList, LinkedList, CopyOnWriteArrayList,
                // Arrays$ArrayList, …). A Set/Queue is NOT a List.
                if has("List") && !ends("Set") && !ends("Map") {
                    return true;
                }
            }
            "java/util/Set" => {
                if has("Set") {
                    return true;
                }
            }
            // Split, because `Deque extends Queue` and not the reverse: every
            // `Deque` is a `Queue`, and a `PriorityQueue` is not a `Deque`.
            // Sharing one arm made `aPriorityQueue instanceof Deque` answer
            // `true` — same probe, same run as the two above.
            "java/util/Queue" => {
                if has("Queue") || has("Deque") {
                    return true;
                }
            }
            "java/util/Deque" => {
                if has("Deque") {
                    return true;
                }
            }
            _ => {}
        }
    }

    // Comparable
    if target_class_name == "java/lang/Comparable" {
        return matches!(
            &*obj_name,
            "java/lang/String"
                | "java/lang/Integer"
                | "java/lang/Long"
                | "java/lang/Double"
                | "java/lang/Float"
                | "java/lang/Short"
                | "java/lang/Byte"
                | "java/lang/Character"
                | "java/lang/Boolean"
        );
    }

    // NOTE — the dynamic-proxy admission rule lives in the callers (see
    // `proxy_instance_satisfies_target` below), where the proxy *instance*
    // is in scope. We can't decide it here without the instance because a
    // proxy's interface set is per-instance (stored on the heap object),
    // not per-class — every proxy lands on the same synthetic
    // `Proxy$Instance` ClassId.

    // Annotation proxy — name-based path only knows the shared
    // `AnnotationProxy` ClassId, so it can only affirm the generic
    // `java.lang.annotation.Annotation` supertype that EVERY proxy satisfies.
    // The SPECIFIC annotation-interface check is instance-aware (the proxy's
    // real type is stored on the heap object) and handled by
    // `annotation_proxy_satisfies_target`. The previous `|| true` made a proxy
    // `instanceof` EVERY annotation interface, so frameworks that distinguish
    // annotation types via instanceof/cast misbehaved — e.g. Log4j2 plugin
    // injection cast a `@Required` proxy to `@PluginBuilderAttribute`, read an
    // empty `value()`, fell back to the field name, and produced a null
    // attribute ("loggerName has invalid value null").
    if &*obj_name == "java/lang/annotation/AnnotationProxy" {
        return target_class_name == "java/lang/annotation/Annotation";
    }

    // Object is always a valid target
    if target_class_name == "java/lang/Object" {
        return true;
    }

    false
}

/// A generated proxy's RECORDED interface set decides its `aastore` stores.
///
/// The defect these pin was MEASURED, not predicted: the orchestrator diffed
/// `RArrayStoreInterfaces` (108 checks, 27 shapes) against HotSpot 25.0.3+9-LTS
/// and exactly one row of 108 diverged —
///
/// ```text
/// s12  Runnable[]  <- Proxy(Marker)
///   HotSpot   cold=[ArrayStoreException]  hot=[ArrayStoreException]
///   CratonVM  cold=[no-throw]             hot=[no-throw]
/// ```
///
/// — identically in both tiers, i.e. in the shared predicate rather than in
/// codegen. Its neighbour `s25 Marker[] <- Proxy(Marker)` was green, which is
/// the whole trap: a blanket that admits every proxy gets every LEGAL proxy row
/// right for free, so `legalAdmitted=15/15` said nothing at all.
///
/// The rows below are the CratonVM-side equivalents of the oracle sweep in
/// `scratchpad/g12/ProxyStoreProbe.java`, quoted in full at the arm they test.
/// Every one of them is a class this module fabricates, so no assertion here
/// depends on a JDK class resolving — the point is the predicate's shape, and a
/// row that passed vacuously because `java/lang/Runnable` was missing would be
/// worth nothing.
#[cfg(test)]
mod g12_generated_proxy_interface_set {
    use super::{aastore_element_assignable, recorded_proxy_interface_set};
    use cratonvm_classloading::ClassOrigin;
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_types::{ArrayElementType, ClassId, ObjectRef};
    use std::sync::Arc;

    /// One VM with four fabricated interfaces and two proxy classes.
    ///
    /// * `G12Marker`, `G12Other` — unrelated marker interfaces.
    /// * `G12Sub extends G12Super` — the transitive leg (oracle:
    ///   `SuperI[] <- proxy(SubI)` is OK, `SubI[] <- proxy(SuperI)` throws).
    /// * `$Proxy9` — origin `GeneratedProxy { interfaces: [G12Marker] }`, the
    ///   shape `define_or_get_proxy_class` produces from real `proxy_gen` bytes.
    /// * `$Proxy8` — origin `GeneratedProxy { interfaces: [] }`, the shape
    ///   `ensure_synthetic_class` produces from a proxy NAME with no interface
    ///   data. This is the population the old blanket existed for and it must
    ///   keep failing open.
    ///
    /// Neither proxy class is given a real `interfaces` vector or a proxy
    /// superclass, deliberately: `is_subclass_of` and `is_assignable_to_name`
    /// therefore both decline for both of them, so every admission below has to
    /// come from the arm under test rather than from the hierarchy walk above
    /// it. That is what makes the legal rows evidence.
    struct Fixture {
        shared: Arc<crate::vm::SharedVm>,
        marker_arr: ObjectRef,
        other_arr: ObjectRef,
        sub_arr: ObjectRef,
        super_arr: ObjectRef,
        serializable_arr: ObjectRef,
        recorded_proxy: ObjectRef,
        unrecorded_proxy: ObjectRef,
        sub_proxy: ObjectRef,
        super_proxy: ObjectRef,
    }

    /// Fabricate an INTERFACE. Separate `fn`s rather than closures throughout
    /// this module, and every class-manager guard dropped before the next
    /// statement: `aastore_element_assignable` takes the read lock itself, and a
    /// write guard kept alive as a statement temporary would deadlock the test
    /// rather than fail it.
    fn iface(shared: &crate::vm::SharedVm, name: &str) -> ClassId {
        let mut cm = shared.classes.class_manager.write();
        let id = cm
            .try_ensure_synthetic_class(name, 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
        cm.class_store
            .get_mut(id)
            .expect("just fabricated")
            .access_flags |= ClassAccessFlags::INTERFACE;
        id
    }

    /// Fabricate a `$ProxyN` and stamp the interface set the VM is to have
    /// "recorded" for it. An empty `ifaces` leaves the origin as
    /// `ensure_synthetic_class` already set it — `GeneratedProxy` with an empty
    /// list — which is the no-record shape.
    fn proxy_class(shared: &crate::vm::SharedVm, name: &str, ifaces: &[ClassId]) -> ClassId {
        let mut cm = shared.classes.class_manager.write();
        let id = cm
            .try_ensure_synthetic_class(name, 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
        if !ifaces.is_empty() {
            cm.class_store
                .get_mut(id)
                .expect("just fabricated")
                .set_origin(ClassOrigin::GeneratedProxy {
                    interfaces: Arc::from(ifaces.to_vec()),
                });
        }
        id
    }

    fn ref_array(shared: &crate::vm::SharedVm, component: ClassId) -> ObjectRef {
        // A reference array's own class id IS its component class id
        // (JVMS §4.4.1) — that is how `array_descriptor_of` recovers the
        // component name.
        shared
            .mem
            .heap
            .alloc_array(component, ArrayElementType::Reference, 1)
    }

    fn fixture() -> Fixture {
        let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let marker = iface(&shared, "cratonvm/test/G12Marker");
        let other = iface(&shared, "cratonvm/test/G12Other");
        let sup = iface(&shared, "cratonvm/test/G12Super");
        let sub = iface(&shared, "cratonvm/test/G12Sub");
        {
            let mut cm = shared.classes.class_manager.write();
            cm.class_store
                .get_mut(sub)
                .expect("just fabricated")
                .interfaces
                .push(sup);
        }
        let serializable = {
            let mut cm = shared.classes.class_manager_write();
            cm.load_class("java/io/Serializable")
                .expect("Compatible mode fabricates")
        };

        // A digit-suffixed `$ProxyN` simple name is what
        // `is_generated_proxy_name` matches, so all four of these are already
        // `GeneratedProxy` before we touch them — the unrecorded one with an
        // EMPTY list, which is precisely the fabricated-proxy shape.
        let recorded = proxy_class(&shared, "jdk/proxy9/$Proxy9", &[marker]);
        let unrecorded = proxy_class(&shared, "jdk/proxy9/$Proxy8", &[]);
        let super_only = proxy_class(&shared, "jdk/proxy9/$Proxy7", &[sup]);
        let sub_only = proxy_class(&shared, "jdk/proxy9/$Proxy6", &[sub]);

        Fixture {
            marker_arr: ref_array(&shared, marker),
            other_arr: ref_array(&shared, other),
            sub_arr: ref_array(&shared, sub),
            super_arr: ref_array(&shared, sup),
            serializable_arr: ref_array(&shared, serializable),
            recorded_proxy: shared.mem.heap.alloc_object(recorded, 0),
            unrecorded_proxy: shared.mem.heap.alloc_object(unrecorded, 0),
            sub_proxy: shared.mem.heap.alloc_object(sub_only, 0),
            super_proxy: shared.mem.heap.alloc_object(super_only, 0),
            shared,
        }
    }

    /// `recorded_proxy_interface_set` is the whole discriminator, and the
    /// emptiness rule is the half that is easy to delete by accident: an
    /// `ensure_synthetic_class`-fabricated `$ProxyN` carries
    /// `GeneratedProxy { interfaces: [] }`, so treating "is a GeneratedProxy" as
    /// the test would refuse every one of them.
    #[test]
    fn an_empty_recorded_interface_list_is_no_record_at_all() {
        assert!(
            recorded_proxy_interface_set(&ClassOrigin::GeneratedProxy {
                interfaces: Arc::from(vec![ClassId::new(7)]),
            })
            .is_some(),
            "a generated proxy WITH interfaces is a positive record"
        );
        assert!(
            recorded_proxy_interface_set(&ClassOrigin::GeneratedProxy {
                interfaces: Arc::from(Vec::new()),
            })
            .is_none(),
            "an EMPTY list is `no interface data recorded`, not `implements \
             nothing` — a fabricated $ProxyN has exactly this origin and must \
             keep failing open"
        );
        assert!(
            recorded_proxy_interface_set(&ClassOrigin::VmInternal).is_none(),
            "`java/lang/reflect/Proxy$Instance` is VmInternal, not a generated \
             proxy — the shim's interface set lives on the heap object"
        );
        assert!(
            recorded_proxy_interface_set(&ClassOrigin::CompatibilityStub {
                reason: Arc::from("probe"),
            })
            .is_none(),
        );
    }

    /// The measured row. `Runnable[] <- Proxy(Marker)`, in the shape this VM
    /// can fabricate.
    #[test]
    fn a_recorded_proxy_is_refused_by_an_interface_it_does_not_implement() {
        let f = fixture();
        assert!(
            !aastore_element_assignable(&f.shared, f.other_arr, f.recorded_proxy),
            "a proxy generated for G12Marker is not a G12Other; HotSpot 25.0.3 \
             throws ArrayStoreException for `Runnable[] <- Proxy(Marker)` \
             (measured, RArrayStoreInterfaces s12) and the `$Proxy` name \
             blanket admitted it without looking at the interface set"
        );
    }

    /// The control, and the row that says the refusal above is a decision and
    /// not a new blanket in the other direction. `s25` on the fixture.
    ///
    /// Note the fixture gives this proxy class NO `interfaces` vector, so
    /// `is_subclass_of` and `is_assignable_to_name` both declined before the arm
    /// under test ran: this admission comes from the recorded origin alone.
    #[test]
    fn a_recorded_proxy_is_admitted_by_an_interface_it_does_implement() {
        let f = fixture();
        assert!(
            aastore_element_assignable(&f.shared, f.marker_arr, f.recorded_proxy),
            "a proxy generated for G12Marker must still store into a \
             G12Marker[] — RArrayStoreInterfaces s25, green today, and a fix \
             that turns it red has replaced one wrong answer with another"
        );
    }

    /// Transitively, through super-interfaces, in both directions.
    /// MEASURED on HotSpot: `SuperI[] <- proxy(SubI)` is OK and
    /// `SubI[] <- proxy(SuperI)` throws.
    #[test]
    fn the_recorded_walk_is_transitive_and_directional() {
        let f = fixture();
        assert!(
            aastore_element_assignable(&f.shared, f.super_arr, f.sub_proxy),
            "a proxy over G12Sub is a G12Super — the recorded walk must go \
             through super-interfaces, not just compare names"
        );
        assert!(
            !aastore_element_assignable(&f.shared, f.sub_arr, f.super_proxy),
            "a proxy over G12Super is NOT a G12Sub; a walk that answered `true` \
             here would be matching the pair rather than the direction"
        );
    }

    /// The population the blanket was written for, unchanged. This is the
    /// assertion that fails if the fix is phrased as "refuse every proxy".
    ///
    /// `vm/src/runtime/interpreter/tests.rs`'s `jdk/proxy3/$Proxy27` fixture is
    /// this exact shape, and so is the `AotIntegrationTests` /
    /// `TypeMappedAnnotation.adapt` store it stands for.
    #[test]
    fn a_proxy_with_no_recorded_interface_set_still_fails_open() {
        let f = fixture();
        assert!(
            aastore_element_assignable(&f.shared, f.other_arr, f.unrecorded_proxy),
            "a $ProxyN whose interface set this VM never recorded is exactly \
             the imprecise-type-information case the predicate is contractually \
             required to fail OPEN on; only a RECORDED set makes a refusal \
             provable"
        );
    }

    /// `Serializable` is universal for a proxy and it is not a proxy rule:
    /// `java.lang.reflect.Proxy.class.getInterfaces()` is `[java.io.Serializable]`
    /// (MEASURED, `scratchpad/g12/ProxyStoreProbe.java`), so every proxy
    /// inherits it through its superclass — which the fixture's proxy classes
    /// deliberately do not have, so this arm has to state it.
    ///
    /// `Cloneable` is the control on the oracle side (`Cloneable[] <- proxy(A)`
    /// throws) and is not asserted here, because in a bare test VM the outcome
    /// would turn on whether `java/lang/Cloneable` resolves rather than on this
    /// arm — a row that can pass vacuously is not evidence.
    #[test]
    fn every_proxy_is_serializable() {
        let f = fixture();
        assert!(
            aastore_element_assignable(&f.shared, f.serializable_arr, f.recorded_proxy),
            "Serializable[] <- proxy is OK on HotSpot for every proxy, by \
             inheritance from java.lang.reflect.Proxy"
        );
    }
}

/// Pure, allocation-free predicates from this file, pinned in-tree.
///
/// Both functions below decide admissions for the whole VM and neither had a
/// single in-tree assertion before 2026-08-13 (lane F32; verified with
/// `grep -rn simple_name_has_word --include=*.rs`, which returned only the
/// definition and its five call sites). `simple_name_has_word`'s only guard
/// was `scratchpad/c16/verify.rs`, which is not in the repository — so the
/// 20,772-cell measurement its doc comment quotes could not fail anything
/// here. These are the cheap half of that probe: the rows a regression would
/// move first, taken verbatim from the two records.
#[cfg(test)]
mod f32_pure_predicate_tests {
    use super::{class_name_is_proxy_super, simple_name_has_word};

    /// The two names are decided by `CRATONVM_REAL_PROXY_SUPER`, which
    /// defaults ON — so `java/lang/reflect/Proxy` is the arm a shipped
    /// generated `$ProxyN` matches, and `Proxy$Instance` is the arm that
    /// serves the opt-out, the degrade fallbacks and synthetic-JDK mode.
    /// Dropping EITHER breaks a mode with no build error.
    #[test]
    fn both_proxy_supers_are_recognised_and_nothing_else_is() {
        assert!(
            class_name_is_proxy_super("java/lang/reflect/Proxy"),
            "the real base class is the DEFAULT super of every generated \
             $ProxyN (proxy_super_class_name(), real_proxy_super()=true); \
             dropping this arm recognises no proxy at all in the shipping \
             configuration"
        );
        assert!(
            class_name_is_proxy_super("java/lang/reflect/Proxy$Instance"),
            "the synthetic shim is still the allocated shape under \
             CRATONVM_REAL_PROXY_SUPER=0, under ProxyClassOutcome::Degrade \
             and ::Failed, and in synthetic-JDK mode"
        );
        // Not a prefix/suffix/contains test. A generated proxy is recognised
        // by its SUPER, never by its own name — `jdk/proxy1/$Proxy0` is a
        // measured JDK 25.0.3 proxy class name and must NOT match here, or
        // the chain walk would answer before it has walked anything.
        for name in [
            "jdk/proxy1/$Proxy0",
            "java/lang/reflect/ProxyGenerator",
            "java/lang/reflect/Proxy$ProxyBuilder",
            "java/lang/annotation/AnnotationProxy",
            "java/lang/Object",
            "",
        ] {
            assert!(
                !class_name_is_proxy_super(name),
                "{name} is not a proxy SUPERCLASS"
            );
        }
    }

    /// `simple_name_has_word` is `synthetic_implements`' whole collection
    /// heuristic and had no in-tree assertion at all. Only OVER-ADMISSIONS
    /// are defects (the function runs after `is_subclass_of` has declined, so
    /// a `false` is "no opinion"), which is why the negative rows carry the
    /// weight and the positive rows exist only to stop a
    /// `fn(_,_) -> false` from passing.
    ///
    /// **Every row's verdict is MEASURED on HotSpot 25.0.3+9-LTS**
    /// (`X.class.isAssignableFrom(Y)` over `java.base`'s real nested class
    /// names, enumerated with `getDeclaredClasses` rather than recalled).
    ///
    /// **Mutation-checked**, by re-running this exact row set against four
    /// hand-made variants of the function under plain `rustc`. Failing-row
    /// counts, measured:
    ///
    /// ```text
    ///   PRISTINE                                    0
    ///   drop the `ends_with("Iterator")` arm        5   (all `cursor` rows)
    ///   `simple` := the full `obj_name`             1   (ConcurrentSkipListMap$Values)
    ///   full name AND plain `contains` (historical) 7
    ///   drop the uppercase/digit word test          2   (`word` rows)
    /// ```
    ///
    /// The `1` is the honest number and worth keeping: the simple-name rule
    /// and the word rule overlap almost completely, because a container name
    /// usually ends in a lower-case letter (`…Collections$`) which the word
    /// test already refuses. `ConcurrentSkipListMap$Values` is the one
    /// measured row that separates them (`…SkipListMap` puts `List` in front
    /// of a capital `M`), so deleting it would leave the simple-name rule
    /// with no coverage of its own.
    #[test]
    fn a_container_never_lends_its_name_and_a_cursor_is_never_a_collection() {
        // 1. A container lends its name to every member — and must not
        //    (W8-C4-2, W8-C16-2). All MEASURED false on HotSpot.
        for (name, term) in [
            ("java/util/ImmutableCollections$Map1", "Collection"),
            ("java/util/ImmutableCollections$MapN", "Collection"),
            ("java/util/ImmutableCollections$StableMap", "Collection"),
            (
                "java/util/ImmutableCollections$StableMap$StableEntry",
                "Collection",
            ),
            ("java/util/concurrent/ConcurrentSkipListMap$Values", "List"),
            (
                "java/util/concurrent/ConcurrentSkipListMap$KeySpliterator",
                "List",
            ),
        ] {
            assert!(
                !simple_name_has_word(name, term),
                "{name} is not a {term} — its ENCLOSING class is"
            );
        }
        // 2. A cursor is not the thing it walks. Every row here has the term
        //    IN its simple name, so each one really does reach the iterator
        //    arm; a row like `ArrayList$Itr`/`List` would pass vacuously
        //    (simple name `Itr` contains no `List`) and is deliberately not
        //    used. All MEASURED false on HotSpot.
        for (name, term) in [
            ("java/util/ImmutableCollections$SetN$SetNIterator", "Set"),
            ("java/util/ImmutableCollections$ListItr", "List"),
            ("java/util/ArrayList$ListItr", "List"),
            ("java/util/LinkedList$ListItr", "List"),
            ("java/util/ArrayList$ArrayListSpliterator", "List"),
        ] {
            assert!(
                !simple_name_has_word(name, term),
                "{name} is a cursor OVER a {term}, not a {term}"
            );
        }
        // 3. "contains" is not "is". Both MEASURED false on HotSpot.
        assert!(
            !simple_name_has_word(
                "java/util/concurrent/locks/AbstractQueuedSynchronizer",
                "Queue"
            ),
            "AbstractQueuedSynchronizer contains \"Queue\" and is not one"
        );
        assert!(
            !simple_name_has_word("java/util/TooManyListenersException", "List"),
            "TooManyListenersException contains \"List\" and is not one"
        );
        // 4. The admissions that must SURVIVE, covering all three closing
        //    shapes the word test allows. All MEASURED true on HotSpot.
        assert!(
            simple_name_has_word("java/util/ArrayList", "List"),
            "the term ENDS the simple name"
        );
        assert!(
            simple_name_has_word(
                "java/util/ImmutableCollections$StableMap$StableMapEntrySet",
                "Set"
            ),
            "…EntrySet is a Set (measured true) and is nested two deep inside \
             a Map — the container rule must not swallow it"
        );
        assert!(
            simple_name_has_word("java/util/ImmutableCollections$SetN", "Set"),
            "the term is followed by another capitalised word"
        );
        assert!(
            simple_name_has_word("java/util/ImmutableCollections$List12", "List"),
            "a digit closes the word too"
        );
        assert!(simple_name_has_word("java/util/HashSet", "Set"));
        assert!(simple_name_has_word("java/util/ArrayDeque", "Deque"));
        // NOT asserted, and the reason is a correction to this function's own
        // doc comment: it cites `WorkQueue` and `ListResourceBundle` as
        // admissions the word rule is careful not to lose. MEASURED on
        // HotSpot 25.0.3+9, both are FALSE —
        // `Queue.class.isAssignableFrom(ForkJoinPool$WorkQueue)` and
        // `List.class.isAssignableFrom(ListResourceBundle)` are both `false`.
        // They are over-admissions this heuristic knowingly keeps (it can
        // only ADMIT), not correct admissions it preserves, so pinning them
        // green would pin the wrong claim. See
        // docs/known-issues/jdk-only/F32-1-the-proxy-route-and-the-drifted-twin-20260813.md §6.
    }
}
