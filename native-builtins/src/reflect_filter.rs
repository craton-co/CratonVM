//! Core reflection's member filter — reads `jdk.internal.reflect.Reflection`'s
//! own `fieldFilterMap`/`methodFilterMap` static fields directly, rather than
//! reimplementing the set.
//!
//! `javap -p -c jdk.internal.reflect.Reflection` shows the truth is split two
//! ways:
//!
//! * `Reflection`'s own `static {}` block builds `fieldFilterMap` with a
//!   hardcoded `Map.of(Reflection.class, ALL_MEMBERS, AccessibleObject.class,
//!   ALL_MEMBERS, Class.class, Set.of("classLoader", "classData", "modifiers",
//!   "protectionDomain", "primitive"), ClassLoader.class, ALL_MEMBERS,
//!   Constructor.class, ALL_MEMBERS, Field.class, ALL_MEMBERS, Method.class,
//!   ALL_MEMBERS, Module.class, ALL_MEMBERS)` and a fresh, empty
//!   `methodFilterMap` — a direct `putstatic`, never a call to
//!   `registerFieldsToFilter`/`registerMethodsToFilter`.
//! * Every OTHER filtered member reaches the map because some class's own
//!   `<clinit>`, running as real bytecode, calls the public
//!   `registerFieldsToFilter`/`registerMethodsToFilter` API on itself —
//!   `MethodHandles$Lookup` for `lookupClass`/`allowedModes`,
//!   `sun.misc.Unsafe` for `getUnsafe` — which the earlier, ABANDONED design
//!   of this module tried to reproduce by shadowing those two methods with a
//!   Rust-side recorder. That caught only the second half: shadowing the
//!   setter methods means `Reflection`'s own `<clinit>` still runs (its
//!   `putstatic`s are unconditional), but the setter calls from OTHER
//!   classes' `<clinit>`s never reach the REAL `fieldFilterMap`/
//!   `methodFilterMap` fields any more, and a Rust-side copy the query side
//!   never consulted is worse than useless. Measured on
//!   `apps/probes/ReflectMemberFilter.java`: 12 of 18 rows matched HotSpot
//!   with that design (the hardcoded base leaked through by accident, since
//!   nothing shadowed `Reflection`'s own `<clinit>`) but `Class.classLoader`/
//!   `classData`, `ClassLoader`'s fields, `Module.loader` and `Reflection`'s
//!   own two fields — everything from the hardcoded half — stayed VISIBLE.
//!
//! The fix that actually reproduces both halves: do not shadow
//! `registerFieldsToFilter`/`registerMethodsToFilter` at all, let every
//! class's own `<clinit>` run as real bytecode exactly as HotSpot's does, and
//! read `Reflection.fieldFilterMap`/`methodFilterMap` back — whichever way
//! an entry got there — at query time. Zero hand-written list, no
//! version-pinning risk: this IS the JDK's own set, read live.
//!
//! See `core-reflection-has-no-member-filter-20260911.md`.

use cratonvm_native_api::NativeContext;
use cratonvm_types::{ClassId, Value};
use std::collections::HashSet;

const REFLECTION_CLASS: &str = "jdk/internal/reflect/Reflection";

/// `Reflection.ALL_MEMBERS` is `Set.of("*")`, a sentinel meaning "every
/// member of this class is filtered" — `Reflection.filter` special-cases
/// `names.contains("*")` before ever comparing a member's own name (see
/// `jdk.internal.reflect.Reflection.filter`, JDK 25). `Reflection`,
/// `AccessibleObject`, `ClassLoader`, `Constructor`, `Field`, `Method` and
/// `Module` are all keyed to `ALL_MEMBERS` in the hardcoded base map, so a
/// caller that only checks literal-name membership never filters any of
/// their members. Callers must test with [`is_filtered`], not a bare
/// `HashSet::contains`.
const WILDCARD: &str = "*";

/// True if `names` (as returned by [`filtered_field_names`]/
/// [`filtered_method_names`]) filters `member_name` — either by literal name
/// or via the `ALL_MEMBERS` wildcard.
pub(crate) fn is_filtered(names: &HashSet<String>, member_name: &str) -> bool {
    names.contains(WILDCARD) || names.contains(member_name)
}

/// The set of field names HotSpot hides when reflecting over `containing_class`
/// — the value of `Reflection.fieldFilterMap.get(containing_class.class)`,
/// materialised into a plain Rust set once so the caller can test many
/// members without repeating the `Map`/`Set` call chain per member (both a
/// perf concern in a hot `getDeclaredFields` loop and a GC-safety one: this
/// resolves every live `ObjectRef` it touches before returning).
pub(crate) fn filtered_field_names(ctx: &mut dyn NativeContext, containing_class: ClassId) -> HashSet<String> {
    filtered_names(ctx, "fieldFilterMap", containing_class)
}

/// [`filtered_field_names`] for `Reflection.methodFilterMap`.
pub(crate) fn filtered_method_names(ctx: &mut dyn NativeContext, containing_class: ClassId) -> HashSet<String> {
    filtered_names(ctx, "methodFilterMap", containing_class)
}

fn filtered_names(
    ctx: &mut dyn NativeContext,
    map_field: &str,
    containing_class: ClassId,
) -> HashSet<String> {
    let Some(refl_id) = ctx.class_id_by_name(REFLECTION_CLASS) else {
        return HashSet::new();
    };
    let Some(idx) = ctx.static_field_index_by_name(refl_id, map_field) else {
        return HashSet::new();
    };
    let Value::Object(Some(map_obj)) = ctx.get_static_field(refl_id, idx) else {
        return HashSet::new();
    };
    let mirror = ctx.get_class_mirror(containing_class);
    // Pinned across the two virtual calls below: both can allocate (a
    // `HashMap`/`HashSet` lookup does not, in general, but nothing in the
    // NativeContext contract promises `invoke_virtual` never triggers a
    // moving collection, and every other cross-call `ObjectRef` hold in this
    // crate pins defensively for exactly that reason).
    let map_pin = ctx.pin_native_root(map_obj);
    let mirror_pin = ctx.pin_native_root(mirror);
    let map_obj = ctx.read_native_pin(map_pin, map_obj);
    let mirror = ctx.read_native_pin(mirror_pin, mirror);
    let _ = mirror_pin; // pinned as part of the same `map_pin` scope; released together below
    let set_obj = match ctx.invoke_virtual(
        map_obj,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(mirror))],
    ) {
        Ok(Some(Value::Object(Some(set_obj)))) => set_obj,
        _ => {
            ctx.unpin_native_roots(map_pin);
            return HashSet::new();
        }
    };
    let set_pin = ctx.pin_native_root(set_obj);
    let set_obj = ctx.read_native_pin(set_pin, set_obj);
    let names = read_string_set(ctx, set_obj);
    ctx.unpin_native_roots(map_pin);
    names
}

/// Read every element of a `Set<String>` into a Rust set, via `toArray()`
/// rather than assuming an internal representation (the set may be any of
/// `Set.of(...)`'s immutable shapes or a plain `HashSet`).
fn read_string_set(ctx: &mut dyn NativeContext, set_obj: cratonvm_types::ObjectRef) -> HashSet<String> {
    let arr = match ctx.invoke_virtual(set_obj, "toArray", "()[Ljava/lang/Object;", &[]) {
        Ok(Some(Value::Object(Some(arr)))) => arr,
        _ => return HashSet::new(),
    };
    let len = ctx.array_length(arr);
    let mut out = HashSet::with_capacity(len);
    for i in 0..len {
        if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
            if let Some(s) = ctx.read_string(s) {
                out.insert(s);
            }
        }
    }
    out
}
