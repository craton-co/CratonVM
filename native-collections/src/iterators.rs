// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// ===========================================================================
// Iterators — ArrayList$Itr and HashMap$KeyItr
// ===========================================================================

/// The class an `ArrayList`-shaped cursor is minted as when its backing object
/// does NOT carry a real `modCount`.
///
/// `java/util/ArrayList$Itr` is shared by every ArrayList-shaped receiver in the
/// VM, and they are not all alike: a map VIEW carrier has no `modCount` slot at
/// all (it extends `AbstractCollection`, so `AbstractList`'s resolved index
/// names its own first declared REFERENCE field). While the two share a class,
/// the class cannot be allowed to run the real JDK cursor -- HotSpot's
/// `checkForComodification` reads that reference as an int and throws a
/// spurious `ConcurrentModificationException`.
///
/// Splitting them at MINT time is what makes `java/util/ArrayList$Itr` mean
/// "over something with a real `modCount`", which is the precondition the
/// bytecode yield needs and cannot express any other way: the yield is a
/// per-(call site, receiver CLASS) decision, so a per-INSTANCE fact has to be
/// encoded in the class or it cannot be consulted at all.
pub(crate) const AL_VIEW_ITR_CLASS: &str = "cratonvm/internal/ArrayListViewItr";

/// Does `list` carry a real `modCount` — i.e. is it something HotSpot's own
/// `ArrayList$Itr` could legally iterate?
///
/// `ArrayList` and `Vector` both extend `AbstractList` and declare one.
/// Everything else — a `MAP_VIEW_CARRIERS` carrier, a bare `Object`
/// placeholder, an unnamed synthetic — does not, and gets
/// [`AL_VIEW_ITR_CLASS`] instead.
///
/// Deliberately a WHITELIST. The failure this exists to stop is silent (a
/// plausible wrong answer from a reference field read as an int), and the
/// population of ArrayList-shaped receivers has repeatedly turned out to be
/// wider than an audit predicted — `LinkedBlockingQueue.iterator()` mints this
/// class with the snapshot ARRAY in slot 0, a third layout again. A new shape
/// must therefore default to the safe side without anyone remembering to add it.
pub(crate) fn itr_backing_has_real_mod_count(ctx: &dyn NativeContext, list: ObjectRef) -> bool {
    matches!(
        al_slots_and_layout_for(ctx, list).1,
        AlLayout::ArrayList | AlLayout::Vector
    )
}

/// Widest a genuine `java.util.ArrayList$Itr` can be: `cursor`, `lastRet`,
/// `expectedModCount`, `this$0`. Anything wider that reaches the `al_itr_*`
/// natives is one of [`VALUES_ITR_CARRIERS`], whose three snapshot fields live
/// PAST the class's own declared ones. See [`al_itr_alt_base`].
pub(crate) const AL_ITR_PLAIN_MAX_FIELDS: usize = 4;

/// The real iterator class HotSpot answers for a `values()` (or `TreeMap`
/// entrySet) view of each map family, keyed on the VIEW carrier's own class.
///
/// # Why these views and not the set-shaped ones
///
/// `keySet()`/`entrySet()` on the `HashMap` family are HashSet-shaped and reach
/// `native_hs_iterator`, which has minted the right class since
/// `MAP_KEY_ITR_CARRIERS` existed. `values()` is ArrayList-shaped -- it is a
/// `Collection`, not a `Set` -- so it arrives at `native_al_iterator` instead
/// and got a plain `java.util.ArrayList$Itr`. Same snapshot, different door.
/// `TreeMap$EntrySet` is in the same list for the same reason.
///
/// MEASURED against HotSpot 25.0.3+9, `--jdk-only`, one case per process,
/// BEFORE this change -- every row was `java.util.ArrayList$Itr`:
///
/// ```text
///   HashMap.values()             HashMap$ValueIterator
///   LinkedHashMap.values()       LinkedHashMap$LinkedValueIterator
///   TreeMap.values()             TreeMap$ValueIterator
///   TreeMap.entrySet()           TreeMap$EntryIterator
///   ConcurrentHashMap.values()   ConcurrentHashMap$ValueIterator
/// ```
///
/// # THERE MAY ONLY BE ONE PRODUCER OF A CARRIER CLASS
///
/// A registration and a force-native gate both key on the CLASS and neither can
/// tell two producers apart, so a snapshot carrier added here CAPTURES every
/// other object of that class in the VM. MEASURED three times, twice as a
/// failure:
///
///  * `Hashtable$ValueCollection` is absent from this list because its family
///    answers one class for all three views (`Hashtable$Enumerator`) and gets
///    java.base's own cursor from `real_ht_view_enumerator`. Adding it reddened
///    `RJdkEnumerations`.
///  * `ConcurrentHashMap$ValuesView` was added on 2026-08-27 for the parity it
///    buys, while `ConcurrentHashMap.elements()` was still building a REAL
///    `ConcurrentHashMap$ValueIterator` over the published mirror table. That
///    real cursor then ran these snapshot bodies over slots nothing had
///    written: `elements()` handed back `null` and `hasMoreElements()` never
///    said false, in BOTH modes, on a three-entry map — `RJdkEnumerations`, and
///    `RJdkJmx` downstream of it
///    (`rjdkenumerations-is-red-on-dev-from-the-chm-values-cursor`).
///  * RESOLVED 2026-08-28 by removing the OTHER producer rather than this
///    entry. `elements()` and `keys()` now hand back the same snapshot carriers
///    `values().iterator()` and `keySet().iterator()` do — the real classes
///    implement `Enumeration` as well as `Iterator`, so both keep HotSpot's
///    class name and this file owns every object of both classes. See
///    `native_chm_elements`, which carries the measurement, and
///    `native_chm_keys`, which moved with it.
///
/// The alternative — handing `values().iterator()` the REAL cursor — was
/// measured too and is worse: it gives the right class and breaks `remove()`
/// (`size=3 contains=true` against HotSpot's `size=2 contains=false`), because
/// `CHM$ValueIterator.remove()` is JDK bytecode calling `replaceNode` on the
/// published MIRROR table, which is not this VM's authoritative store. That
/// still waits on `table` BEING the authority
/// (`W7-96-chm-table-never-populated.md`); the snapshot does not need it.
///
/// `BaseIterator.hasMoreElements()` is a DIRECT read of the `next` field rather
/// than a call to `hasNext()`, so a carrier here whose class also implements
/// `Enumeration` needs `hasMoreElements`/`nextElement` registered too — see the
/// pair beside the `elements()` registration. Without it the enumeration is
/// EMPTY, which is a quieter wrong answer than the non-terminating one.
pub(crate) const VALUES_ITR_CARRIERS: &[(&str, &str)] = &[
    // TWO COLLECTION FAMILIES, added 2026-08-29 with L3 residual 6.1.
    //
    // Not "values views" -- a `TreeSet`, an `ArrayDeque` and a `PriorityQueue`
    // are sources in their own right -- but they want exactly what this table
    // provides: the family's REAL iterator class, minted with the three
    // snapshot fields past its declared ones, with `native_al_itr_*` bound to
    // it and the third fail-fast door reading its `expectedModCount`.
    //
    // MEASURED (`apps/probes/FailFastShapeProbe`) before the change: every
    // family that hands out its real iterator class is fail-fast and every
    // family that does not is not. These three handed out `Arrays$ArrayItr`
    // (TreeSet, ArrayDeque) or `ArrayList$Itr` (PriorityQueue) where HotSpot
    // 25.0.4+7 hands out the classes below, and none of the three was
    // fail-fast. One defect wearing two faces.
    //
    // Each replaces a FABRICATION -- `java/util/TreeSet$Itr`,
    // `java/util/ArrayDeque$Itr` -- that `--jdk-only` refused and landed off,
    // so this retires two stand-ins rather than adding any.
    //
    // ARRAYDEQUE LEFT THIS TABLE ON 2026-08-30, one family further along the
    // same road: its iterator is not minted here at all now, real or
    // fabricated. A snapshot could carry the class name but never the
    // fail-fast, because `DeqIterator`'s is the ring buffer's layout rather
    // than a counter. Making the layout faithful and standing aside got both.
    ("java/util/TreeSet", "java/util/TreeMap$KeyIterator"),
    ("java/util/PriorityQueue", "java/util/PriorityQueue$Itr"),
    (
        "java/util/HashMap$Values",
        "java/util/HashMap$ValueIterator",
    ),
    (
        "java/util/LinkedHashMap$LinkedValues",
        "java/util/LinkedHashMap$LinkedValueIterator",
    ),
    (
        "java/util/TreeMap$Values",
        "java/util/TreeMap$ValueIterator",
    ),
    (
        "java/util/TreeMap$EntrySet",
        "java/util/TreeMap$EntryIterator",
    ),
    // The one receiver that was still sharing `java/util/ArrayList$Itr` over a
    // backing object with no `modCount`. `ConcurrentHashMap$ValuesView` extends
    // `CollectionView`, an `AbstractCollection`, so `AbstractList`'s resolved
    // index names its first declared REFERENCE field. While it shared
    // `ArrayList$Itr`, letting that class run the real JDK cursor made
    // `checkForComodification` read that reference as an int and throw a
    // spurious `ConcurrentModificationException`. It is also a plain parity fix
    // -- HotSpot names `ConcurrentHashMap$ValueIterator` here.
    //
    // This is the PRECONDITION for the `ArrayList$Itr` bytecode yield: that
    // allow-list entry is safe only while `java/util/ArrayList$Itr` means "an
    // iterator over something with a real `modCount`".
    //
    // `probes/ItrClassProbe` shows four other receivers still on the shared
    // class, and every one is fine:
    //
    //   * `ArrayList` itself.
    //   * `Collections.synchronizedList`, which returns the BACKING list's own
    //     iterator -- exactly what HotSpot does.
    //   * `Vector` / `Stack`, which extend `AbstractList` and so DO have a
    //     `modCount`. Their class name still differs from HotSpot's
    //     `Vector$Itr`; that is a separate, cosmetic gap.
    //   * `PriorityQueue`. It looks like a blocker -- `AbstractQueue` has no
    //     `modCount` -- and is not: `native_pq_iterator` deliberately returns an
    //     `ArrayList$Itr` over an ArrayList-shaped WRAPPER holding a heap-order
    //     snapshot, so `this$0` is a genuine `ArrayList`. Do NOT "fix" it by
    //     adding it here: that mints a real `PriorityQueue$Itr`, whose slot 0 is
    //     `cursor:int`, and the snapshot array stored there is coerced away --
    //     the bug `native_pq_iterator`'s own comment records.
    (
        "java/util/concurrent/ConcurrentHashMap$ValuesView",
        "java/util/concurrent/ConcurrentHashMap$ValueIterator",
    ),
];

/// The iterator class a values-shaped view should mint, or `None` for anything
/// else (an ordinary `ArrayList`, a `COWAL`, an `EnumSet` wrapper).
pub(crate) fn values_itr_carrier_for(ctx: &dyn NativeContext, view: ObjectRef) -> Option<&'static str> {
    let name = ctx.class_name_arc_of_id(ctx.class_id_of_object(view))?;
    VALUES_ITR_CARRIERS
        .iter()
        .find(|(v, _)| *v == &*name)
        .map(|(_, i)| *i)
}

/// `Some(base)` when `itr` is a [`VALUES_ITR_CARRIERS`] iterator, whose three
/// `AL_ITR_FIELD_*` slots live past the class's own declared fields.
///
/// Gated on the object's WIDTH first, and the name lookup is reached only by an
/// object already too wide to be a plain `ArrayList$Itr`. That ordering is the
/// point: these three natives are the per-element `hasNext`/`next`/`remove` of
/// every `ArrayList` iteration in the VM, and this file's own history records
/// name lookups being hunted off exactly these lines. An ordinary list pays one
/// integer compare.
///
/// The snapshot cannot simply overwrite the declared slots the way the
/// fabricated `ArrayList$Itr` shape does: on a real `HashMap$ValueIterator`
/// slots 0 and 1 are `HashIterator`'s `next` and `current`, both REFERENCE
/// typed, and storing an `Int` cursor into one trips the GC guard's autobox
/// path (`W7-84`) once per element.
pub(crate) fn al_itr_alt_base(ctx: &dyn NativeContext, itr: ObjectRef) -> Option<usize> {
    let w = ctx.object_num_fields(itr);
    if w <= AL_ITR_PLAIN_MAX_FIELDS {
        return None;
    }
    let cid = ctx.class_id_of_object(itr);
    let name = ctx.class_name_arc_of_id(cid)?;
    if !VALUES_ITR_CARRIERS.iter().any(|(_, i)| *i == &*name) {
        return None;
    }
    // EXACT, not merely "wider than a plain `ArrayList$Itr`".
    //
    // The name test alone asks "is this class one we mint?", and the answer is
    // yes for an instance of that class REAL JDK BYTECODE allocated, which has
    // no snapshot block at all. `PriorityQueue$Itr` declares six fields, so the
    // width gate above waves it through and `w - AL_ITR_NUM_FIELDS` lands three
    // slots inside the declared ones -- reading `forgetMeNot` as a cursor. That
    // is `two-producers-of-one-carrier-class-is-a-failure-family` with the
    // second producer being the JDK itself, and it became reachable the moment
    // these carriers stopped being classes only this crate ever allocates.
    //
    // Our mint is `class_num_total_fields + AL_ITR_NUM_FIELDS` and nothing else
    // is, so requiring the exact width tells the two apart with certainty
    // rather than by a bound. The `w <= AL_ITR_PLAIN_MAX_FIELDS` early-out is
    // kept ahead of it: an ordinary `ArrayList` iteration still pays one
    // integer compare and never reaches the class lookup.
    (w == ctx.class_num_total_fields(cid) + AL_ITR_NUM_FIELDS).then_some(w - AL_ITR_NUM_FIELDS)
}

/// Run the receiver's OWN bytecode when it reached these natives by
/// INHERITANCE rather than by this crate's mint.
///
/// `java.util.ArrayDeque$DescendingIterator extends DeqIterator`, so the moment
/// `DeqIterator` joined [`VALUES_ITR_CARRIERS`] the JDK became a second
/// producer of objects these natives are registered for -- and a real
/// `DescendingIterator` has the four declared fields and none of the snapshot
/// block. MEASURED on the build that added it
/// (`apps/probes/DequeListShadowSweep` row 76):
/// `new ArrayDeque<>(List.of("a","b","c")).descendingIterator()` walked to
/// `[]` where HotSpot walks `[c, b, a]`, because the natives read a cursor and
/// a list out of `cursor`/`remaining`.
///
/// `two-producers-of-one-carrier-class-is-a-failure-family` with the second
/// producer being java.base itself, which no amount of single-producer
/// discipline in this crate can prevent: it is a SUBCLASS relationship, and
/// dispatch resolves an inherited method to the class that declares it.
///
/// Returns `None` for a receiver that is ours, so the caller proceeds with the
/// native body. Same shape and the same reason as [`asl_delegate_foreign`];
/// `invoke_virtual_bytecode_only` is what keeps this from recursing straight
/// back into the same registration.
pub(crate) fn al_itr_delegate_foreign(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> Option<MethodCallResult> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    let name = ctx.class_name_arc_of_id(ctx.class_id_of_object(this))?;
    // The plain shape this crate mints, at its name-resolved slots.
    if &*name == "java/util/ArrayList$Itr" || &*name == AL_VIEW_ITR_CLASS {
        return None;
    }
    // A carrier class: ours only if it carries the snapshot block, which
    // `al_itr_alt_base` decides on the EXACT width. A JDK-allocated instance of
    // the same class -- or of a subclass, which arrives under the declaring
    // carrier's name -- has the declared width and no block.
    if VALUES_ITR_CARRIERS.iter().any(|(_, i)| *i == &*name)
        && al_itr_alt_base(&*ctx, this).is_some()
    {
        return None;
    }
    Some(ctx.invoke_virtual_bytecode_only(this, method, descriptor, &args[1..]))
}

/// [`al_itr_slots`] for a receiver: the carrier-aware `(cursor, list, lastRet)`.
pub(crate) fn al_itr_slots_for(ctx: &dyn NativeContext, itr: ObjectRef) -> (usize, usize, usize) {
    match al_itr_alt_base(ctx, itr) {
        Some(b) => (
            b + AL_ITR_FIELD_CURSOR,
            b + AL_ITR_FIELD_LIST,
            b + AL_ITR_FIELD_LAST_RET,
        ),
        None => {
            let (c, l, _) = al_itr_slots(ctx);
            (c, l, al_itr_last_ret_slot(ctx))
        }
    }
}

pub(crate) const AL_ITR_FIELD_LIST: usize = 0;
pub(crate) const AL_ITR_FIELD_CURSOR: usize = 1;
// Dedicated fallback slot for `lastRet` (see `al_itr_last_ret_slot` below).
// Previously absent: `al_itr_last_ret_slot`'s fallback defaulted to slot 1,
// which collides with `AL_ITR_FIELD_CURSOR` -- every `next()` call's
// `lastRet = cursor` write (using the PRE-increment cursor value) then
// clobbered the `cursor = cursor + 1` write one line above it, so cursor
// never advanced past 0 and `hasNext()` (`cursor < size`) stayed true
// forever. Real WildFly repro: any Java loop draining a ServiceLoader-
// backed ArrayList$Itr (e.g. `Module.loadService(Extension.class)`
// iteration during `org.wildfly.extension.core-management`'s real
// `java.desktop`-dependent clinit) spun forever re-appending the same
// element, hammering the (uncapped) `set_field` OOB-write guard.
pub(crate) const AL_ITR_FIELD_LAST_RET: usize = 2;
pub(crate) const AL_ITR_NUM_FIELDS: usize = 3;

/// Resolve ArrayList$Itr field slots: returns (cursor_slot, list_slot, n_fields).
/// Real-JDK layout: cursor (slot 0), lastRet (slot 1), expectedModCount (slot 2),
/// this$0 (slot 3 — the enclosing ArrayList). Falls back to synthetic
/// (list=0, cursor=1) when the real class isn't available.
#[inline]
pub(crate) fn al_itr_slots(ctx: &dyn NativeContext) -> (usize, usize, usize) {
    let cursor = ctx.resolve_field_index("java/util/ArrayList$Itr", "cursor");
    let list = ctx.resolve_field_index("java/util/ArrayList$Itr", "this$0");
    match (cursor, list) {
        (Some(c), Some(l)) => {
            let n = std::cmp::max(c, l) + 1;
            // Account for lastRet/expectedModCount slots between cursor and this$0.
            let n = std::cmp::max(n, 4);
            (c, l, n)
        }
        _ => (AL_ITR_FIELD_CURSOR, AL_ITR_FIELD_LIST, AL_ITR_NUM_FIELDS),
    }
}

/// [`al_itr_slots`] for a MINT, with the real `java/util/ArrayList$Itr` loaded
/// first when it exists and is not loaded yet.
///
/// FS16-3 (`jit-r16-fieldsite-proposals-RETIRED-20261006.md`; JIT round 17 wave 2, lane
/// smallfix). `resolve_field_index` answers only for a LOADED class, and
/// `alloc_arraylist_iterator_as` used to resolve before `try_alloc_synthetic`
/// loaded `ArrayList$Itr`. So the first native `iterator()` of a run that had
/// not loaded the class yet wrote the fallback `(list=0, cursor=1)` shape into
/// a real `ArrayList$Itr` (`cursor` 0, `lastRet` 1, `expectedModCount` 2,
/// `this$0` 3): the list ref landed in `cursor`, `0` in `lastRet`, `this$0`
/// stayed null; and every later read (`al_itr_slots_for`) used the real slots.
/// A values-view iterator (`AL_VIEW_ITR_CLASS`) minted before the load was
/// written at the fallback slots and read at the real ones after it.
///
/// Loads only a class that exists: a name that would be answered by a
/// fabricated stub is left alone (no new stub is minted here), and the policy
/// read happens only on the not-yet-loaded path, i.e. once.
/// `CRATONVM_COMPAT_AL_ITR_LOAD_BEFORE_SLOTS=0` restores the old order.
pub(crate) fn al_itr_slots_loaded(ctx: &mut dyn NativeContext) -> (usize, usize, usize) {
    const ITR: &str = "java/util/ArrayList$Itr";
    if ctx.resolve_field_index(ITR, "cursor").is_none()
        && ctx.class_id_by_name(ITR).is_none()
        && !ctx.would_fabricate_synthetic_stub(ITR)
        && cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_COMPAT_AL_ITR_LOAD_BEFORE_SLOTS",
        )
    {
        let _ = ctx.ensure_class_initialized(ITR);
    }
    al_itr_slots(&*ctx)
}

/// Resolve the `lastRet` slot of `ArrayList$Itr`. The native `next()` /
/// `remove()` implementations must keep this field consistent with real-JDK
/// semantics: `next()` records the index it just returned, and the real-JDK
/// `ArrayList$Itr.remove()` bytecode — when *not* intercepted — guards with
/// `if (lastRet < 0) throw new IllegalStateException()`. Falls back to the
/// synthetic layout's dedicated slot (`AL_ITR_FIELD_LAST_RET`, 2) when the
/// class is not available for name resolution (the real class keeps it at 1).
#[inline]
pub(crate) fn al_itr_last_ret_slot(ctx: &dyn NativeContext) -> usize {
    ctx.resolve_field_index("java/util/ArrayList$Itr", "lastRet")
        .unwrap_or(AL_ITR_FIELD_LAST_RET)
}

/// Resolve the `expectedModCount` slot of `itr`, or `None` when this build has
/// no slot to keep it in.
///
/// Same three exclusions as [`al_mod_count_slot`], and for the same reason: the
/// synthetic `ArrayList$Itr` layout is `(list, cursor, lastRet)` with no fourth
/// field, so a resolver answering an index that collides with one of those would
/// clobber the iterator's own position. `None` means "no comodification
/// detection on this iterator", which is the pre-fix behaviour.
#[inline]
pub(crate) fn al_itr_expected_mod_count_slot(ctx: &dyn NativeContext, itr: ObjectRef) -> Option<usize> {
    // A `VALUES_ITR_CARRIERS` iterator keeps only the three snapshot fields;
    // the class's own `expectedModCount` is one of the declared slots this
    // shape deliberately does not write. No slot means no comodification
    // check, which is the behaviour the values views already had.
    if al_itr_alt_base(ctx, itr).is_some() {
        return None;
    }
    let slot = ctx.resolve_field_index("java/util/ArrayList$Itr", "expectedModCount")?;
    let (cursor_slot, list_slot, _) = al_itr_slots(ctx);
    if slot == cursor_slot
        || slot == list_slot
        || slot == al_itr_last_ret_slot(ctx)
        || slot >= ctx.object_num_fields(itr)
    {
        return None;
    }
    Some(slot)
}

/// The JDK's `ArrayList$Itr.checkForComodification()`: fail the iteration if the
/// list was structurally modified through any path other than this iterator.
///
/// W7-1 family 2. `for (String s : l) { l.add(s); }` is *specified* to die here,
/// and the probe that measured this had to bound the loop precisely because a VM
/// that never throws grows the list until the heap is gone. Both halves are
/// `Option`: an iterator with no `expectedModCount` slot, or a list with no
/// `modCount` slot, keeps the old never-throw behaviour rather than guessing.
pub(crate) fn al_itr_check_comod(
    ctx: &dyn NativeContext,
    itr: ObjectRef,
    list: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let expected = match al_itr_expected_mod_count_slot(ctx, itr) {
        Some(slot) => match ctx.get_field(itr, slot) {
            Value::Int(v) => v,
            _ => return Ok(()),
        },
        None => return Ok(()),
    };
    match al_mod_count(ctx, list) {
        Some(actual) if actual != expected => {
            Err(cratonvm_types::error::RuntimeError::ConcurrentModificationException.into())
        }
        _ => Ok(()),
    }
}

/// Re-seed `itr`'s `expectedModCount` from `list` after a removal THROUGH the
/// iterator — the JDK's `expectedModCount = modCount` line. Without it the
/// iterator's own `remove()` would trip the check it just installed.
pub(crate) fn al_itr_sync_mod_count(ctx: &mut dyn NativeContext, itr: ObjectRef, list: ObjectRef) {
    if let Some(slot) = al_itr_expected_mod_count_slot(ctx, itr) {
        let seen = al_mod_count(ctx, list).unwrap_or(0);
        ctx.set_field(itr, slot, Value::Int(seen));
    }
}

/// The THIRD fail-fast door: a [`VALUES_ITR_CARRIERS`] iterator, i.e. the one
/// `TreeMap.entrySet()`, `TreeMap.values()`, `HashMap.values()` and
/// `LinkedHashMap.values()` hand out.
///
/// It needs its own slot resolver because [`al_itr_expected_mod_count_slot`]
/// deliberately answers `None` here — that helper resolves
/// `ArrayList$Itr.expectedModCount`, which names nothing on a
/// `TreeMap$EntryIterator`. These carriers declare an `expectedModCount` of
/// their OWN, below the undeclared snapshot block at [`al_itr_alt_base`], and
/// that is the slot to use: it is `int`-typed in every carrier, so writing it
/// cannot type-pun a reference the way the `MAP_VIEW_CARRIERS` `modCount`
/// collision described on [`al_mod_count_slot`] would.
///
/// MEASURED, and the reason the encoding below is biased: relaxing
/// `al_itr_expected_mod_count_slot` to cover these carriers instead made every
/// Spring Boot class die inside JUnit discovery with a SPURIOUS
/// `ConcurrentModificationException`. An iterator minted on a path that does
/// not seed reads **0**, and 0 is a perfectly legal generation, so the raw
/// value cannot distinguish "unseeded" from "the source is on generation 0".
/// NO ALTERNATE GENERATION SLOT FOR `ArrayDeque$DeqIterator`, because there is
/// no longer an `ArrayDeque$DeqIterator` this crate mints. That family was
/// RETIRED on 2026-08-30 and real `ArrayDeque.iterator()` bytecode runs.
///
/// It is worth keeping why, because the wrong answer was reached twice here.
/// `DeqIterator` declares no `expectedModCount`; the JDK's fail-fast is a
/// PHYSICAL index into the ring buffer, and `nonNullElementAt` reporting any
/// null it reads as a `ConcurrentModificationException`. So the check fires
/// exactly when a mutation moves the elements out from under that index, and
/// that is a property of the LAYOUT, not of any counter:
///
/// ```text
/// apps/probes/DequeListShadowSweep
///   81 ad fail fast on ADD during iteration      HotSpot CME
///   82 ad fail fast on REMOVE during iteration   HotSpot no-throw
/// ```
///
/// An `add` that fills the buffer grows it, and the JDK's `grow` slides the
/// first leg to the far end and nulls the slots it came from -- straight under
/// a cursor that has already advanced. A `remove` from the head moves `head`
/// past the cursor instead and nulls nothing the cursor will read. Any
/// generation counter moves for both, which is why seeding one closed row 81
/// and opened row 82: one wrong row traded for another.
///
/// What closed both was making the layout faithful and then standing aside --
/// `ad_state` derives the element count instead of storing it, and `ad_grow`
/// and `ad_remove_at_logical` are ports of `ArrayDeque.grow` and
/// `ArrayDeque.delete`. `apps/probes/AdFieldProbe` reads `elements`, `head` and
/// `tail` back through reflection and the deque is byte-identical to HotSpot's
/// through growth and both `delete` branches, so the real iterator is fail-fast
/// for the real reason.
pub(crate) fn al_view_itr_expected_slot(ctx: &dyn NativeContext, itr: ObjectRef) -> Option<usize> {
    let base = al_itr_alt_base(ctx, itr)?;
    let cid = ctx.class_id_of_object(itr);
    let slot = ctx.resolve_field_index_by_class_id(cid, "expectedModCount")?;
    if slot >= base {
        return None;
    }
    Some(slot)
}

/// The generation a values-shaped view carrier is fail-fast against: the
/// SOURCE map's, read through whichever back-reference that carrier has.
///
/// Both readers are tried because the two producers differ. A carrier this
/// crate minted stashes its source in the element buffer's trailing capacity
/// slot ([`values_view_source`]); a real view-class receiver that arrived from
/// elsewhere carries it in its declared `this$0`/`map`
/// ([`values_view_class_source`]). A source with no `modCount` — a `TreeSet`
/// behind a `descendingSet()`, a synthetic layout — answers `None`, which is
/// the no-check case.
pub(crate) fn al_view_generation(ctx: &dyn NativeContext, list: ObjectRef) -> Option<i32> {
    let src = values_view_source(ctx, list).or_else(|| values_view_class_source(ctx, list))?;
    family_snapshot_generation(ctx, src).or_else(|| map_itr_mod_count(ctx, src))
}

/// The generation of a source that is one of the three snapshot-iterator
/// COLLECTION families rather than a map: its SIZE.
///
/// `map_itr_mod_count` answers `None` for both. `TreeSet` declares no `modCount`
/// at all, and `PriorityQueue` declares one that this crate's natives never
/// move. Size is the generation that is actually available for either.
///
/// `ArrayDeque` was the third and is gone: its iterator is real JDK bytecode
/// now, fail-fast off the ring buffer's layout, which a size cannot model in
/// either direction. See `al_view_itr_expected_slot` for the measurement.
///
/// SIZE CAN ONLY MISS, NEVER FALSELY FIRE, and that direction is the whole
/// reason it is acceptable here. A structural change moves the size, so every
/// `add` and every `remove` during an iteration is caught; a balanced
/// `add`+`remove` between two `next()` calls is not, where the JDK's `modCount`
/// would be. That is a recorded gap and not a wrong answer, and the opposite
/// error is the one this file has already paid for once -- the first attempt at
/// the values-view door made every Spring Boot class die inside JUnit discovery
/// with a SPURIOUS `ConcurrentModificationException` (see
/// `al_view_itr_expected_slot`).
///
/// Keyed on the three class names rather than on "has no `modCount`", so no
/// source that exists today changes behaviour.
pub(crate) fn family_snapshot_generation(ctx: &dyn NativeContext, src: ObjectRef) -> Option<i32> {
    // `is_tree_set_shaped`, not the bare class name: `TreeMap.keySet()` hands
    // back a `TreeMap$KeySet` carrier that this crate's `native_ts_*` family
    // serves exactly as it serves a `java/util/TreeSet`, and its iterator comes
    // through the same mint. Matching only the one name left the keySet view
    // out, and its removal then fell through to the MAP path and called
    // `entrySet()` on a `TreeMap$KeySet` -- `NoSuchMethodError`, measured on
    // `MapViewsShadowSweep` row 142.
    if is_tree_set_shaped(ctx, src) {
        // A keySet VIEW's own size is itself a snapshot -- it moves only when
        // something resyncs the carrier -- so a `put` into the source map
        // leaves it unchanged and the check never fires. Read the MAP's
        // generation, which is what the view is really fail-fast against.
        // MEASURED (`apps/probes/MapViewsShadowSweep` row 173): `keySet fail
        // fast on put` answered no-throw where HotSpot raises CME.
        if let Some(map) = ts_view_source(ctx, src) {
            return map_itr_mod_count(ctx, map).or_else(|| Some(tm_state(ctx, map).1));
        }
        return Some(ts_state(ctx, src).1);
    }
    let name = ctx.class_name_arc_of_id(ctx.class_id_of_object(src))?;
    match &*name {
        "java/util/PriorityQueue" => Some(pq_state(ctx, src).1),
        _ => None,
    }
}

/// The stamp a source generation is stored as: `generation + 1`.
///
/// Kept as a pair of free functions with [`view_comod_is_stale`] so the
/// arithmetic that decides whether to throw is testable without a heap. That
/// separation is not cosmetic: the bug this encoding exists to prevent —
/// attempt 1's spurious `ConcurrentModificationException` on every Spring Boot
/// class — is entirely an arithmetic property (`0` had to mean two different
/// things at once), and a unit test can pin it.
#[inline]
pub(crate) fn view_comod_stamp(gen: i32) -> i32 {
    gen.wrapping_add(1)
}

/// `true` iff a stamped iterator has been outlived by its source.
///
/// `stored == 0` is the UNSEEDED encoding and answers `false` — fail open. Any
/// other value is a real stamp and the comparison undoes the bias.
#[inline]
pub(crate) fn view_comod_is_stale(stored: i32, actual: i32) -> bool {
    stored != 0 && actual != stored.wrapping_sub(1)
}

/// Stamp `itr` with the source generation, stored as `generation + 1`.
///
/// The bias is the whole design. `0` means "never seeded, do not check", so a
/// mint path this function was never wired into fails OPEN rather than throwing
/// on its first `next()`. `al_view_itr_check_comod` is the only reader and it
/// subtracts the same 1. (A source sitting on `-1` therefore loses the check;
/// no `bump_map_mod_count` path ever produces that, and losing a check is the
/// safe direction anyway.)
pub(crate) fn al_view_itr_seed(ctx: &mut dyn NativeContext, itr: ObjectRef, list: ObjectRef) {
    if !map_itr_failfast_enabled() {
        return;
    }
    let Some(slot) = al_view_itr_expected_slot(&*ctx, itr) else {
        return;
    };
    let Some(gen) = al_view_generation(&*ctx, list) else {
        return;
    };
    ctx.set_field(itr, slot, Value::Int(view_comod_stamp(gen)));
    if dbg_view_comod() {
        eprintln!(
            "[VIEW-COMOD] seed itr={} slot={slot} gen={gen}",
            ctx.class_name_of_id(ctx.class_id_of_object(itr))
                .unwrap_or_else(|| "<unknown>".into()),
        );
    }
}

/// [`al_itr_check_comod`] for the values-shaped views, whose snapshot made a
/// structural change to the source invisible.
///
/// Fails open on every arm, including the unseeded one — see
/// [`al_view_itr_seed`] for why that arm has to be representable at all.
pub(crate) fn al_view_itr_check_comod(
    ctx: &dyn NativeContext,
    itr: ObjectRef,
    list: ObjectRef,
) -> Result<(), MethodCallFailed> {
    if !map_itr_failfast_enabled() {
        return Ok(());
    }
    let Some(slot) = al_view_itr_expected_slot(ctx, itr) else {
        return Ok(());
    };
    let stored = match ctx.get_field(itr, slot) {
        Value::Int(v) => v,
        _ => return Ok(()),
    };
    if stored == 0 {
        if dbg_view_comod() {
            eprintln!(
                "[VIEW-COMOD] UNSEEDED itr={} slot={slot}",
                ctx.class_name_of_id(ctx.class_id_of_object(itr))
                    .unwrap_or_else(|| "<unknown>".into()),
            );
        }
        return Ok(());
    }
    let Some(actual) = al_view_generation(ctx, list) else {
        return Ok(());
    };
    if dbg_view_comod() {
        eprintln!(
            "[VIEW-COMOD] check itr={} expected={} actual={actual}",
            ctx.class_name_of_id(ctx.class_id_of_object(itr))
                .unwrap_or_else(|| "<unknown>".into()),
            stored.wrapping_sub(1),
        );
    }
    if view_comod_is_stale(stored, actual) {
        return Err(cratonvm_types::error::RuntimeError::ConcurrentModificationException.into());
    }
    Ok(())
}

/// Engagement trace for the values-view comodification check — `seed`,
/// `check` and, decisively, `UNSEEDED`. Two earlier attempts at this door were
/// reverted, the second because its seed was INERT: a run that only ever prints
/// `UNSEEDED` has a live check and a dead stamp, which is exactly the state a
/// pass/fail cell cannot distinguish from "the door works".
#[inline]
pub(crate) fn dbg_view_comod() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_VIEW_COMOD").is_some())
}

/// Off-switch for the map/set iterator comodification check. Default ON.
#[inline]
pub(crate) fn map_itr_failfast_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_NO_MAP_ITERATOR_FAILFAST").is_none()
    })
}

/// The iterator's own declared `expectedModCount` slot, or `None`.
///
/// Same shape and same failure direction as
/// [`al_itr_expected_mod_count_slot`]: a carrier that does not declare the
/// field, or whose declared slot would collide with the undeclared snapshot
/// block this file writes from `key_itr_base` upward, keeps the old
/// never-throw behaviour rather than guessing. A missed exception on an
/// incorrect program is far better than a spurious one on a correct program.

// ─────────────────── per-(vm, ClassId) memo for pure class questions ────────
//
// Several helpers on the per-ELEMENT iterator path answer a question that
// depends only on the receiver's class: "where does this set keep its backing
// map", "which slot is `expectedModCount`", "which slot is `modCount`". Each
// answered it by re-resolving names through the class manager on every call.
//
// MEASURED. `probes/KeySetBench hoisted` (1000-entry `LinkedHashMap`, iterate a
// hoisted `keySet()` view) against the SAME binary with
// `CRATONVM_NO_MAP_ITERATOR_FAILFAST=1`, three interleaved rounds:
//
// ```text
//              fail-fast ON      OFF        ratio
//   hoisted    10930 / 10472 / 10872 ms     5478 / 5469 ms     2.0x
//   perCall    14357 / 13076 ms             5707 / 4409 ms     2.5-3.0x
// ```
//
// i.e. HALF of everything this workload had left after the 2026-08-23 map-view
// fix was `map_itr_check_comod` re-deriving, per element, three facts that are
// constant for the life of the iterator. `perf record` names the same chain
// from the other end: `hashbrown::HashMap<ClassId,()>::insert` 9.91 %,
// `Class::is_subclass_of_inner` 4.59 %, `classify_exact_name` 1.80 %,
// `get_field_by_name` 1.57 %, `find_unique_class_by_name` 1.45 %,
// `resolve_field_index_by_class_id` 1.27 %, `class_id_by_name` 1.14 %.
// (The absolute walls above are from a host at load ~50 and are NOT comparable
// to the quiet-host figures on the known-issue page; the ON/OFF ratio is what
// they establish, and it was taken ABBA-interleaved on one binary.)
//
// The memo is DIRECT-MAPPED and per-thread: one array slot per `ClassId & 127`,
// holding `(vm identity, class id, answer)` so a collision or a second VM in
// the same process is a miss rather than a wrong answer. The VM identity is in
// the key because `ClassId`s are per-VM dense indices and the test harness
// builds several `SharedVm`s in one process.
//
// **Only POSITIVE answers are cached.** These questions are pure functions of
// the class STORE, and the store grows: `hs_map_slot` asks
// `class_id_by_name("java/util/HashSet")`, which answers `None` until that
// class is loaded. Caching that `None` would pin "this receiver has no backing
// map" for the process. A positive answer cannot change the same way — the
// class it names is already loaded — so it is the half that is safe to keep.

/// Slots in a [`class_memo_get`] table. A power of two so the index is a mask.
pub(crate) const CLASS_MEMO_SLOTS: usize = 128;

pub(crate) type ClassMemo<T> = std::cell::RefCell<Vec<Option<(usize, u32, T)>>>;

#[inline]
pub(crate) fn class_memo_get<T: Copy>(table: &ClassMemo<T>, vm: usize, cid: u32) -> Option<T> {
    let b = table.borrow();
    match b.get((cid as usize) & (CLASS_MEMO_SLOTS - 1)) {
        Some(Some((v, c, val))) if *v == vm && *c == cid => Some(*val),
        _ => None,
    }
}

#[inline]
pub(crate) fn class_memo_put<T: Copy>(table: &ClassMemo<T>, vm: usize, cid: u32, val: T) {
    let mut b = table.borrow_mut();
    if b.len() != CLASS_MEMO_SLOTS {
        b.resize(CLASS_MEMO_SLOTS, None);
    }
    b[(cid as usize) & (CLASS_MEMO_SLOTS - 1)] = Some((vm, cid, val));
}

/// [`resolve_field_index_by_class_id`](NativeContext::resolve_field_index_by_class_id)
/// with the answer remembered per `(vm, class)`.
///
/// The underlying resolution hashes the name, walks the superclass chain and
/// compares field names under the class-manager read lock. On the iterator path
/// it is asked the same question — `expectedModCount` on one iterator class,
/// `modCount` on one map class — once per element.
///
/// `name` is NOT part of the key: each call site passes a fixed literal and
/// gets its own table, which is what keeps the key a single `u32` compare.
#[inline]
pub(crate) fn memoized_field_slot(
    ctx: &dyn NativeContext,
    table: &ClassMemo<usize>,
    cid: cratonvm_types::ClassId,
    name: &str,
) -> Option<usize> {
    let vm = ctx.vm_identity();
    let key = cid.as_u32();
    if let Some(slot) = class_memo_get(table, vm, key) {
        return Some(slot);
    }
    let slot = ctx.resolve_field_index_by_class_id(cid, name)?;
    class_memo_put(table, vm, key, slot);
    Some(slot)
}

pub(crate) fn map_itr_expected_mod_slot(ctx: &dyn NativeContext, itr: ObjectRef) -> Option<usize> {
    let cid = ctx.class_id_of_object(itr);
    thread_local! {
        pub(crate) static MEMO: ClassMemo<usize> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let slot = MEMO.with(|t| memoized_field_slot(ctx, t, cid, "expectedModCount"))?;
    let base = key_itr_base(ctx, itr);
    if slot >= base || slot >= ctx.object_num_fields(itr) {
        return None;
    }
    Some(slot)
}

/// The collection whose modification generation this iterator is fail-fast
/// against: for a keySet/entrySet view that is the SOURCE map behind the
/// view's backing, and for a plain `HashSet` it is the backing map itself.
pub(crate) fn map_itr_comod_source(ctx: &dyn NativeContext, itr: ObjectRef) -> Option<ObjectRef> {
    let base = key_itr_base(ctx, itr);
    if ctx.object_num_fields(itr) <= base + MAP_KEY_ITR_FIELD_BACKING {
        return None;
    }
    let coll = match ctx.get_field(itr, base + MAP_KEY_ITR_FIELD_BACKING) {
        Value::Object(Some(c)) => c,
        _ => return None,
    };
    let backing = hs_backing_map(ctx, coll)?;
    Some(view_backing_source(ctx, backing).unwrap_or(backing))
}

/// `src`'s current modification generation, or `None` when it keeps none.
pub(crate) fn map_itr_mod_count(ctx: &dyn NativeContext, src: ObjectRef) -> Option<i32> {
    // By SLOT, memoized per class, and by nothing else.
    //
    // The by-name arm this replaced was described as "the fallback for a
    // receiver whose class metadata has no such field at all -- a fabricated
    // map whose `modCount` lives in an overlay rather than in a declared
    // slot". There is no such overlay: `get_field_by_name` resolves through
    // the SAME class metadata and then reads the SAME heap slot, without the
    // `object_num_fields` bound. So for exactly the receivers it was said to
    // rescue it read past their fields and answered a constant -- and a
    // constant is what [`view_source_generation`] reads as "this map has not
    // changed".
    //
    // So there is ONE implementation of "which slot is this receiver's
    // generation", shared with `bump_map_mod_count` through
    // [`map_mod_count_slot`]. The two must agree: a reader that accepts a slot
    // the writer cannot reach reports "unchanged" forever.
    read_map_mod_count(ctx, src)
}

/// The JDK's `expectedModCount = modCount` line — at mint, and again after a
/// removal made THROUGH the iterator (which bumps the source and would
/// otherwise trip the check the iterator just installed).
pub(crate) fn map_itr_seed_expected(ctx: &mut dyn NativeContext, itr: ObjectRef) {
    if !map_itr_failfast_enabled() {
        return;
    }
    let Some(slot) = map_itr_expected_mod_slot(&*ctx, itr) else {
        return;
    };
    let Some(src) = map_itr_comod_source(&*ctx, itr) else {
        return;
    };
    if let Some(seen) = map_itr_mod_count(&*ctx, src) {
        ctx.set_field(itr, slot, Value::Int(seen));
    }
}

/// The JDK's `HashMap$HashIterator.nextNode()` comodification test.
///
/// These iterators walk a SNAPSHOT taken at `iterator()` time, so a structural
/// change to the source was previously invisible: the loop quietly finished
/// over stale contents where HotSpot throws. MEASURED with
/// `probes/MapModCountProbe`, one process per VM, HotSpot 25.0.3+9:
///
/// ```text
///                  entrySet.put  entrySet.remove  keySet.put      HotSpot
///   HashMap             NONE          NONE           NONE         CME x3
///   LinkedHashMap       NONE          NONE           NONE         CME x3
///   TreeMap             NONE          NONE           NONE         CME x3
///   Hashtable           CME           CME            CME          CME x3
/// ```
///
/// Only `Hashtable` matched, and only because its views hand out java.base's
/// OWN cursor (`real_ht_view_enumerator`) instead of a snapshot.
///
/// The generation this reads is the one `bump_map_mod_count` maintains; it was
/// stuck at 0 for `LinkedHashMap` until `539d962f1`, so this check could not
/// have worked for that family before then.
pub(crate) fn map_itr_check_comod(ctx: &dyn NativeContext, itr: ObjectRef) -> Result<(), MethodCallFailed> {
    if !map_itr_failfast_enabled() {
        return Ok(());
    }
    let Some(slot) = map_itr_expected_mod_slot(ctx, itr) else {
        return Ok(());
    };
    let expected = match ctx.get_field(itr, slot) {
        Value::Int(v) => v,
        _ => return Ok(()),
    };
    let Some(src) = map_itr_comod_source(ctx, itr) else {
        return Ok(());
    };
    match map_itr_mod_count(ctx, src) {
        Some(actual) if actual != expected => {
            Err(cratonvm_types::error::RuntimeError::ConcurrentModificationException.into())
        }
        _ => Ok(()),
    }
}

pub(crate) const MAP_KEY_ITR_FIELD_KEYS: usize = 0;
pub(crate) const MAP_KEY_ITR_FIELD_CURSOR: usize = 1;
pub(crate) const MAP_KEY_ITR_FIELD_TOTAL: usize = 2;
// Backing collection (HashSet/HashMap ref) used by Iterator.remove().
// May be Object(None) for snapshot-only iterators from make_iterator_from_array;
// in that case remove() throws UnsupportedOperationException.
pub(crate) const MAP_KEY_ITR_FIELD_BACKING: usize = 3;
// Index of the last key returned by next(); -1 before first next() and after remove().
pub(crate) const MAP_KEY_ITR_FIELD_LAST_RET: usize = 4;
pub(crate) const MAP_KEY_ITR_NUM_FIELDS: usize = 5;

/// The classes a snapshot key/entry iterator over a HashSet-shaped receiver is
/// minted under, alongside the historical fabricated `java/util/HashMap$KeyItr`.
///
/// `HashMap$KeyItr` is a name no JDK declares — the real one is
/// `HashMap$KeyIterator` — which is both a `getClass()` divergence
/// (`probes/ViewClassProbe`'s `lhs.iterator` row: HotSpot
/// `java.util.LinkedHashMap$LinkedKeyIterator`, CratonVM
/// `java.util.HashMap$KeyItr`) and the reason six `--jdk-only` strict-mode rows
/// used to die with `NoClassDefFoundError` (see `real_snapshot_iterator`).
///
/// Unlike the collection carriers, these DO collide: `HashMap$HashIterator`
/// declares `next`/`current` (references) and `expectedModCount`/`index`
/// (ints), and this VM's five fields would land an `Int` cursor in a reference
/// slot. So the native state goes PAST the declared fields —
/// [`key_itr_base`] — and the object is allocated `declared + 5` wide.
/// Which OWNERSHIP CLUSTER a view carrier belongs to — the unit `H4-1` §2 says
/// the retag has to move in, and the thing this file had a comment about and no
/// code for.
///
/// # The problem, in `H4-1`'s words
///
/// > *"`register_map_view_carrier_natives` — `MAP_VIEW_CARRIERS` holds
/// > `HashMap$Values`, `LinkedHashMap$LinkedValues`, **`TreeMap$Values`**,
/// > **`TreeMap$EntrySet`**, **`Hashtable$ValueCollection`**,
/// > `ConcurrentHashMap$ValuesView`: four families. … So the split has to be
/// > per carrier family, keyed on whether the producing registrar moved — not
/// > per registrar."*
///
/// Both carrier registrars opened with ONE
/// `r.set_category(NativeKind::Bridge)` over the whole list, so moving either
/// of them was all-or-nothing across four ownership clusters. Retagging the
/// registrar as a unit is a partial retag of three families; refusing a
/// carrier whose PRODUCER is still `Bridge` runs the real JDK body over a
/// carrier this crate minted.
///
/// # What this changes, and what it deliberately does not
///
/// Every carrier now declares its family and takes its category from
/// [`carrier_family_kind`], which is asked once per carrier rather than once
/// per registrar. **Today every family answers `Bridge`, so this is a
/// no-behaviour-change refactor** — and that is the point: the structure is in
/// place so that moving `register_hashmap_natives` moves ITS carriers and only
/// its carriers, by construction rather than by a reviewer noticing.
///
/// `H4-1` §2 calls that shape *"strictly safer than §5's, because dispatch
/// keys on class: any class left `Bridge` behaves exactly as today"*.
/// `clippy::enum_variant_names`: every variant ends in `Cluster`, and that is
/// the point rather than an accident of naming. A "cluster" is `H4-1` §2's unit
/// — the set of classes that must move families together because they share
/// storage (`LinkedHashMap` extends `HashMap` and shares its `table`) or a
/// receiver predicate (`Properties` rides with `Hashtable` because
/// `is_hashtable_receiver` accepts both). `HashMap` alone would name a class;
/// `HashMapCluster` names the thing this enum has one variant per.
#[allow(clippy::enum_variant_names)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum CarrierFamily {
    /// `java/util/HashMap` and `java/util/LinkedHashMap` — one cluster, because
    /// `LinkedHashMap` extends `HashMap` and shares its `table`.
    HashMap,
    /// `java/util/Hashtable`. `Properties` rides with it in the registry even
    /// though its storage differs, because `is_hashtable_receiver` accepts both.
    Hashtable,
    /// `java/util/TreeMap` — the one whose `H4-1` §2 example is sharpest,
    /// because a `TreeMap$Values` carrier is minted by `register_tree_map_natives`.
    TreeMap,
    /// `java/util/concurrent/ConcurrentHashMap`.
    Chm,
}

/// The `NativeKind` a carrier of `family` must be registered under: the kind
/// its PRODUCING registrar currently carries.
///
/// All four are `Bridge` today, which is exactly the state that makes this
/// refactor inert. When a cluster moves, change its arm here and its carriers
/// follow — the one edit, in the one place, that `H4-1` §2 asks for.
///
/// **Before moving [`CarrierFamily::HashMap`], read
/// [`register_set_view_carrier_natives`]'s cluster note.** The
/// [`MAP_KEY_ITR_CARRIERS`] rows cannot be refused while a `Hashtable$KeySet`
/// still mints a `HashMap$KeyIterator`; that edge is closed separately by
/// [`key_itr_carrier_for`], and until it is, this arm and the Hashtable one
/// have to move together.
pub(crate) fn carrier_family_kind(family: CarrierFamily) -> cratonvm_native_api::NativeKind {
    match family {
        CarrierFamily::HashMap => cratonvm_native_api::NativeKind::Bridge,
        CarrierFamily::Hashtable => cratonvm_native_api::NativeKind::Bridge,
        CarrierFamily::TreeMap => cratonvm_native_api::NativeKind::Bridge,
        CarrierFamily::Chm => cratonvm_native_api::NativeKind::Bridge,
    }
}

/// The owning cluster of a [`MAP_VIEW_CARRIERS`] / [`SET_VIEW_CARRIERS`] name.
///
/// Exhaustive over both lists by construction: an unlisted name answers
/// `HashMap`, which is the conservative default only because every
/// carrier that reaches here IS in one of the two lists — the registrars are
/// the only callers and they iterate those lists.
pub(crate) fn carrier_family_of(name: &str) -> CarrierFamily {
    match name {
        "java/util/Hashtable$ValueCollection"
        | "java/util/Hashtable$KeySet"
        | "java/util/Hashtable$EntrySet" => CarrierFamily::Hashtable,
        "java/util/TreeMap$Values" | "java/util/TreeMap$EntrySet" => CarrierFamily::TreeMap,
        "java/util/concurrent/ConcurrentHashMap$ValuesView"
        | "java/util/concurrent/ConcurrentHashMap$EntrySetView" => CarrierFamily::Chm,
        _ => CarrierFamily::HashMap,
    }
}

pub(crate) const MAP_KEY_ITR_CARRIERS: &[&str] = &[
    "java/util/HashMap$KeyIterator",
    "java/util/HashMap$EntryIterator",
    "java/util/LinkedHashMap$LinkedKeyIterator",
    "java/util/LinkedHashMap$LinkedEntryIterator",
    // The ConcurrentHashMap views, 2026-08-28. Both were minting the
    // FABRICATED `java/util/HashMap$KeyItr` shape in compatible mode — a
    // Phase-1 fabrication sitting in the middle of the collection surface —
    // and, when `--jdk-only` refused it, landing on `Arrays$ArrayItr`. So ONE
    // receiver answered two different wrong class names depending on the mode,
    // and neither was HotSpot's.
    //
    // Safe here for the reason the `TreeMap$KeyIterator` row above states and
    // the `Hashtable$Enumerator` attempt failed: SINGLE PRODUCER. `keys()` used
    // to build a REAL `ConcurrentHashMap$KeyIterator` over the published mirror
    // table through `chm_real_dual_iterator`; that function is gone and
    // `keys()` now hands back this same snapshot, so nothing else can present
    // an object of either class to the natives registered on it. Two producers
    // of one class is what made `elements()` never terminate — see
    // `native_chm_elements`.
    "java/util/concurrent/ConcurrentHashMap$KeyIterator",
    "java/util/concurrent/ConcurrentHashMap$EntryIterator",
];

/// `true` for the fabricated `HashMap$KeyItr` shape and for every
/// [`MAP_KEY_ITR_CARRIERS`] name — i.e. every receiver whose five native
/// iterator fields live at [`key_itr_base`].
pub(crate) fn is_map_key_itr_class(name: &str) -> bool {
    name == "java/util/HashMap$KeyItr" || MAP_KEY_ITR_CARRIERS.contains(&name)
}

/// Where this iterator's five native fields start.
///
/// Derived from the object's own width rather than from its class, which makes
/// it one header read on a path (`hasNext`/`next`) that runs once per element:
/// every mint site allocates exactly `declared + MAP_KEY_ITR_NUM_FIELDS` slots,
/// so the base is whatever is left over. The fabricated `HashMap$KeyItr`
/// declares nothing and lands on 0, and the legacy three-field snapshot shape
/// (`make_fabricated_iterator_from_array`) is narrower than five and also lands
/// on 0 — which is what keeps `n_fields > base + LAST_RET` telling the two
/// apart, exactly as it did when the base was always zero.
#[inline]
pub(crate) fn key_itr_base(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    ctx.object_num_fields(this)
        .saturating_sub(MAP_KEY_ITR_NUM_FIELDS)
}

/// One declared field of the map a minted iterator was taken over, bounded by
/// what the receiver was actually allocated with.
///
/// The bound is [`map_mod_count_slot`]'s, for its reason: `resolve_field_index`
/// answers for the CLASS, and a map allocated narrower than its class declares
/// would otherwise read a slot it does not own.
#[inline]
pub(crate) fn real_map_field(
    ctx: &dyn NativeContext,
    owner: &str,
    map: ObjectRef,
    name: &str,
) -> Option<Value> {
    let slot = ctx.resolve_field_index(owner, name)?;
    (slot < ctx.object_num_fields(map)).then(|| ctx.get_field(map, slot))
}

/// Write one of a minted iterator's own DECLARED fields by name.
///
/// Bounded by [`key_itr_base`] rather than by the object width: everything at
/// or past the base is this crate's five-field snapshot, and a name that
/// resolved there would be a collision rather than a miss. Answers whether the
/// write landed, so the caller can decline a carrier whose class the image does
/// not declare the way this function expects.
#[inline]
pub(crate) fn set_real_itr_field(
    ctx: &mut dyn NativeContext,
    itr: ObjectRef,
    base: usize,
    name: &str,
    v: Value,
) -> bool {
    let cid = ctx.class_id_of_object(itr);
    let Some(slot) = ctx.resolve_field_index_by_class_id(cid, name) else {
        return false;
    };
    if slot >= base {
        return false;
    }
    ctx.set_field(itr, slot, v);
    true
}

/// Make a minted `HashMap`/`LinkedHashMap` iterator carrier drivable by the
/// image's OWN bytecode, by writing the fields that class's constructor writes.
///
/// # Why this exists, and why the count of it is 171 rows
///
/// This crate mints its iterators under real class names
/// ([`MAP_KEY_ITR_CARRIERS`]) and drives them from the five snapshot fields
/// [`key_itr_base`] points at. The real class's OWN fields -- `this$0`, `next`,
/// `current`, `expectedModCount`, and `index` on the `HashMap` pair -- were
/// left at their allocation defaults, which is invisible for exactly as long
/// as the natives registered on those classes are the only reader.
///
/// A retirement is what ends that. `RETIRED_SHADOW_L1_HM8_TRIPLES`' wave
/// measured the consequence on three trial binaries and §10 item 2 of the lane
/// page records it: retire `LinkedHashMap`'s twelve iterator rows and
/// `LinkedSequencedShadowSweep` dies on 74 of 104 rows with
/// `NullPointerException: "this.this$0" is null` in
/// `LinkedHashMap$LinkedHashIterator.nextNode`; withdraw them and every
/// mutation through a `SequencedMap` default is lost instead, because those
/// defaults are `entrySet().iterator(); next(); remove()` and a VM-minted
/// iterator writes to a model the real map no longer reads. Two failure modes,
/// one cause: the carrier is a real class carrying none of its own state.
///
/// So write the state. The bodies below are the JDK's own constructors, read
/// off `javap -p -c` on all three supported images:
///
/// ```text
///   LinkedHashMap$LinkedHashIterator(LinkedHashMap m, boolean reversed)
///       this$0 = m; reversed = r;
///       next = r ? m.tail : m.head;
///       expectedModCount = m.modCount;  current = null;
///   HashMap$HashIterator(HashMap m)
///       this$0 = m;  expectedModCount = m.modCount;
///       next = current = null;  index = 0;
///       if (m.table != null && m.size > 0)
///           do {} while (index < table.length && (next = table[index++]) == null);
/// ```
///
/// `reversed` is JDK 25's and is not declared on 17 or 21; `current` is null on
/// every image. Both are the value a freshly allocated object already carries,
/// so neither is written and neither has to be version-tested -- the fields
/// that ARE written are the ones whose correct value is not zero.
///
/// # What it deliberately does not touch
///
/// The two `ConcurrentHashMap` carriers. Their real bodies read
/// `ConcurrentHashMap.table`, and on a CratonVM-native CHM that field is null
/// -- measured reflectively at the `native_chm_keyset_view_iterator` mint site,
/// whose comment states it. Seeding a CHM iterator would hand real bytecode an
/// empty walk, which is the failure mode that reads as a pass.
///
/// The fabricated `java/util/HashMap$KeyItr` shape declares no fields at all,
/// so its `key_itr_base` is 0 and every write here declines on the bound.
pub(crate) fn seed_real_map_itr_fields(
    ctx: &mut dyn NativeContext,
    itr: ObjectRef,
    carrier: &str,
    source: ObjectRef,
) {
    let linked = matches!(
        carrier,
        "java/util/LinkedHashMap$LinkedKeyIterator" | "java/util/LinkedHashMap$LinkedEntryIterator"
    );
    let plain = matches!(
        carrier,
        "java/util/HashMap$KeyIterator" | "java/util/HashMap$EntryIterator"
    );
    if !linked && !plain {
        return;
    }
    // The SOURCE has to be the exact class whose fields the carrier's body
    // reads. `getfield java/util/LinkedHashMap.head` on a `Properties` or on a
    // synthetic stand-in is a different object, and an inexact match here is
    // how a seeded field becomes a wrong answer rather than a missing one.
    let owner = if linked {
        "java/util/LinkedHashMap"
    } else {
        "java/util/HashMap"
    };
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(source))
        .as_deref()
        != Some(owner)
    {
        return;
    }
    // Everything below is COMPUTED before anything is written, because the one
    // outcome this function must never produce is a half-seeded carrier: a
    // real `this$0` with a null `next` is a valid iterator over nothing, and a
    // silently empty walk is strictly worse than the `NullPointerException`
    // this seeding exists to remove. The NPE names the class; an empty
    // iteration reads as a pass.
    let occupied = matches!(
        real_map_field(&*ctx, owner, source, "size"),
        Some(Value::Int(n)) if n > 0
    );
    let mc = real_map_field(&*ctx, owner, source, "modCount").unwrap_or(Value::Int(0));
    let mut index: i32 = 0;
    let next = if linked {
        real_map_field(&*ctx, owner, source, "head").unwrap_or(Value::Object(None))
    } else {
        // `HashMap$HashIterator`'s constructor advances to the first occupied
        // bucket and leaves `index` one PAST it.
        let mut found = Value::Object(None);
        if occupied {
            if let Some(Value::Object(Some(tab))) = real_map_field(&*ctx, owner, source, "table") {
                let n = ctx.array_length(tab);
                while (index as usize) < n {
                    let e = ctx.get_array_element(tab, index as usize);
                    index += 1;
                    if matches!(e, Value::Object(Some(_))) {
                        found = e;
                        break;
                    }
                }
            }
        }
        found
    };
    // THE REFUSAL. A map that reports entries and shows none in its real
    // storage is one whose state this crate has not published, and seeding a
    // carrier over it would hand a real body an empty walk. Leave the carrier
    // exactly as it was.
    if occupied && matches!(next, Value::Object(None)) {
        if dbg_hs_itr() {
            eprintln!(
                "[HS-ITR-DBG] seed_real_map_itr_fields DECLINED: {owner} reports \
                 entries and its real storage is empty; leaving {carrier} unseeded"
            );
        }
        return;
    }
    let base = key_itr_base(&*ctx, itr);
    // `this$0` first and as the gate: it is what `nextNode` dereferences before
    // it reads anything else, so a carrier that cannot take it is one no real
    // body can drive and one this function must not half-populate.
    if !set_real_itr_field(ctx, itr, base, "this$0", Value::Object(Some(source))) {
        return;
    }
    set_real_itr_field(ctx, itr, base, "expectedModCount", mc);
    set_real_itr_field(ctx, itr, base, "next", next);
    if !linked {
        set_real_itr_field(ctx, itr, base, "index", Value::Int(index));
    }
}

/// The map a key/entry view was taken over, for a view carried by a set.
///
/// The same two steps `map_itr_comod_source` takes, without the iterator: a
/// set-shaped view's backing map, and then the SOURCE behind that backing when
/// the backing is itself a view.
pub(crate) fn key_itr_view_source(ctx: &dyn NativeContext, set: ObjectRef) -> Option<ObjectRef> {
    let backing = hs_backing_map(ctx, set)?;
    Some(view_backing_source(ctx, backing).unwrap_or(backing))
}

/// The iterator class a `HashSet`-shaped receiver's `iterator()` should mint,
/// matching what HotSpot answers for that receiver.
///
/// `LinkedHashSet.iterator()` is `LinkedHashMap$LinkedKeyIterator` in the JDK
/// because a `LinkedHashSet` is backed by a `LinkedHashMap`; a keySet/entrySet
/// view follows its SOURCE map the same way. Anything else — a plain
/// `HashSet`, a `CopyOnWriteArraySet`, a view of a map family with no iterator
/// carrier of its own — takes the `HashMap` pair, which is what an unqualified
/// `HashSet.iterator()` answers.
pub(crate) fn key_itr_carrier_for(
    ctx: &dyn NativeContext,
    receiver: ObjectRef,
    entryset: bool,
) -> &'static str {
    let name = ctx.class_name_arc_of_id(ctx.class_id_of_object(receiver));
    // A VIEW WHOSE SOURCE IS A `Properties` answers the CHM family, because
    // JDK 25 backs `Properties` with a `ConcurrentHashMap` and
    // `Properties.keySet()` is `map.keySet()`. MEASURED
    // (`apps/probes/ItrClassNameProbe`): HotSpot 25.0.4+7 answers
    // `ConcurrentHashMap$KeyIterator` where this VM answered
    // `HashMap$KeyIterator`. The view's own class already matches
    // (`Collections$SynchronizedSet`) and the view is already live, so the
    // iterator's class was the last of the three to disagree.
    //
    // Checked before the class-name tests because this VM's `Properties`
    // keySet is carried by a `LinkedHashSet`, which would otherwise take the
    // LinkedHashMap pair.
    //
    // NOT the `two-producers-of-one-carrier-class` trap, and the difference is
    // worth stating because it is the same class the CHM views mint. Every
    // name in `MAP_KEY_ITR_CARRIERS` shares ONE object shape -- five fields at
    // `key_itr_base`, derived from the object's WIDTH -- and ONE registrar,
    // `native_map_key_itr_*`. A second producer is a problem when the two
    // shapes differ, as the dormant `PriorityQueue$Itr` row was; here the
    // class name is a LABEL over an identical object, and the natives cannot
    // tell the producers apart because there is nothing to tell apart.
    if let Some(src) = key_itr_view_source(ctx, receiver) {
        if ctx
            .class_name_arc_of_id(ctx.class_id_of_object(src))
            .as_deref()
            == Some("java/util/Properties")
        {
            return if entryset {
                "java/util/concurrent/ConcurrentHashMap$EntryIterator"
            } else {
                "java/util/concurrent/ConcurrentHashMap$KeyIterator"
            };
        }
    }
    // The CHM views answer their OWN family's classes, like every other map
    // family in this function. Checked before the LinkedHashMap test because a
    // CHM view is neither, and falling through would give it the HashMap pair.
    match name.as_deref() {
        Some("java/util/concurrent/ConcurrentHashMap$KeySetView") => {
            return "java/util/concurrent/ConcurrentHashMap$KeyIterator"
        }
        Some("java/util/concurrent/ConcurrentHashMap$EntrySetView") => {
            return "java/util/concurrent/ConcurrentHashMap$EntryIterator"
        }
        _ => {}
    }
    let linked = match name.as_deref() {
        Some("java/util/LinkedHashSet") => true,
        Some("java/util/LinkedHashMap$LinkedKeySet" | "java/util/LinkedHashMap$LinkedEntrySet") => {
            true
        }
        // A SUBCLASS of `LinkedHashSet` is backed by a `LinkedHashMap` exactly as
        // `LinkedHashSet` is. Matching the name alone gave it the `HashMap` pair,
        // whose real `hasNext` reads a `next` that `seed_real_map_itr_fields`
        // (which requires the source to be the pair's own map class) never set,
        // so under `--jdk-only` every `SearchPath extends LinkedHashSet<Path>`
        // (javac's classpath) iterated EMPTY -- `getLocation(CLASS_PATH)` had no
        // entries and every AOT `TestCompiler` compile failed with "package ...
        // does not exist".
        _ => {
            let cid = ctx.class_id_of_object(receiver);
            ctx.class_id_by_name("java/util/LinkedHashSet")
                .is_some_and(|base| ctx.is_subclass(cid, base))
        }
    };
    match (linked, entryset) {
        (true, false) => "java/util/LinkedHashMap$LinkedKeyIterator",
        (true, true) => "java/util/LinkedHashMap$LinkedEntryIterator",
        (false, false) => "java/util/HashMap$KeyIterator",
        (false, true) => "java/util/HashMap$EntryIterator",
    }
}

/// Allocate a snapshot key/entry iterator under `carrier`, wide enough for the
/// class's own declared fields plus this VM's five.
///
/// Falls back to the fabricated `java/util/HashMap$KeyItr` shape when the real
/// class cannot be had, which is the pre-2026-08-13 behaviour and keeps a
/// stripped image iterating rather than failing.
pub(crate) fn alloc_key_itr(
    ctx: &mut dyn NativeContext,
    carrier: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Ok(cid) = ctx.ensure_class_initialized(carrier) {
        if ctx.class_name_arc_of_id(cid).as_deref() == Some(carrier) {
            let n = ctx.class_num_total_fields(cid) + MAP_KEY_ITR_NUM_FIELDS;
            return Ok(ctx.alloc_object(cid, n));
        }
    }
    try_alloc_synthetic(ctx, "java/util/HashMap$KeyItr", MAP_KEY_ITR_NUM_FIELDS)
}

/// CLUSTER NOTE (H4-1, 2026-08-20). Two unrelated families in one registrar:
/// `java/util/ArrayList$Itr` belongs to the ArrayList cluster (whose container
/// registrar is untouched), the [`MAP_KEY_ITR_CARRIERS`] block to the HashMap
/// one. Retagging this function as a unit is therefore a partial retag of
/// ArrayList — `G88-1` §5 did exactly that and its vectors did not ask.
/// The `MAP_KEY_ITR_CARRIERS` half additionally has the Hashtable edge
/// described on [`register_set_view_carrier_natives`].
pub(crate) fn register_iterator_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // ArrayList$Itr
    //
    // `hasNext`/`next` stay `Bridge` here and are retagged `SyntheticStub`
    // below. Allow-listing `java/util/ArrayList$Itr` makes the real JDK cursor
    // run and be COMPILED -- measured 6.2x on `probes/KeySetBench iterList`,
    // and it fixes `cmeClear` too. LANDED 2026-08-27: this was NOT SAFE on its
    // own (a `values()` view over a carrier with no `modCount` reached this
    // same class and threw a spurious `ConcurrentModificationException`), and
    // an `ItrClassProbe` class-name census is not what closes that gap --
    // `IdentityHashMap`'s values view mints its own `IdentityHashMap$ValueIterator`
    // in isolation and still arrived here. What actually closes it is two
    // paragraphs down: [`itr_backing_has_real_mod_count`] gives a
    // no-real-`modCount` backing a DIFFERENT class at mint time
    // ([`AL_VIEW_ITR_CLASS`]), so `java/util/ArrayList$Itr` is a genuine
    // guarantee rather than a class-census wish, and can be allow-listed here
    // safely. Full history and the retired diagnosis in
    // `docs/feature-designs/collections-interception.md`.
    // `hasNext`/`next` are `SyntheticStub`, not `Bridge`, so the yield predicate
    // can reach them and the real JDK cursor runs -- which is what lets the JIT
    // compile it AT ALL. A registered native pins its method out of the JIT
    // entirely: the tier-up counter lives only in the `VirtualBytecode` arm of
    // `dispatch_virtual`, so a site serving a `VirtualNative` target is never
    // counted, nominated or compiled. `ArrayList$Itr.next` was entered 2 000 000
    // times on a 2000x1000 walk and never appeared in `CRATONVM_DBG_JITC`.
    //
    // This is sound ONLY because [`itr_backing_has_real_mod_count`] now splits
    // the mint: a backing without a real `modCount` gets [`AL_VIEW_ITR_CLASS`]
    // instead, so this class name is a genuine guarantee rather than a wish.
    // Removing that split re-opens the spurious-CME hazard on every
    // `for (v : map.values())`.
    //
    // Pairs with `java/util/ArrayList$Itr` on `real_protected_stub_class_common`
    // -- both halves are required, and each alone is useless or worse.
    //
    // `remove` stays `Bridge`: not on the hot path, and it is the one of the
    // three that writes through to the backing list, so it keeps the native
    // `propagate_list_removal` understands. Real-bytecode `next`/`hasNext`
    // beside a native `remove` is what `probes/ItrYieldProbe`'s `lastRet` and
    // cursor-rewind rows exercise.
    //
    // `CRATONVM_ITR_BYTECODE=0` restores `Bridge`: the one-binary A/B and the
    // kill switch.
    let itr_kind = if cratonvm_types::flags::runtime_var("CRATONVM_ITR_BYTECODE")
        .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
    {
        cratonvm_native_api::NativeKind::Bridge
    } else {
        cratonvm_native_api::NativeKind::SyntheticStub
    };
    r.with_category(itr_kind, |r| {
        r.register(
            "java/util/ArrayList$Itr",
            "hasNext",
            "()Z",
            native_al_itr_has_next,
        );
        r.register(
            "java/util/ArrayList$Itr",
            "next",
            "()Ljava/lang/Object;",
            native_al_itr_next,
        );
    });
    r.register(
        "java/util/ArrayList$Itr",
        "remove",
        "()V",
        native_al_itr_remove,
    );
    // The same three bodies on [`AL_VIEW_ITR_CLASS`], the cursor minted for a
    // backing with no real `modCount`. Same layout, same slot triple, same
    // natives -- the ONLY thing the separate class buys is that it is not
    // `java/util/ArrayList$Itr`, so the bytecode yield cannot reach it.
    r.register(AL_VIEW_ITR_CLASS, "hasNext", "()Z", native_al_itr_has_next);
    r.register(
        AL_VIEW_ITR_CLASS,
        "next",
        "()Ljava/lang/Object;",
        native_al_itr_next,
    );
    r.register(AL_VIEW_ITR_CLASS, "remove", "()V", native_al_itr_remove);
    // The per-family values iterators (`VALUES_ITR_CARRIERS`). They need the
    // SAME three bodies for the same reason the `MAP_KEY_ITR_CARRIERS` classes
    // do: their snapshot lives past the class's own declared fields, so the
    // JDK's own `HashIterator`/`PrivateEntryIterator` bodies walk a `next`
    // chain nothing populated.
    //
    // MEASURED omission: the first cut of this change added the classes to the
    // carrier map and to BOTH force-native gates and registered nothing on
    // them, so dispatch found no native and fell through to real bytecode --
    // `values()` drained EMPTY and `remove()` threw `IllegalStateException`
    // from `HashMap$HashIterator.remove`. A gate entry only decides WHO WINS
    // between a native and the bytecode; with no native registered there is
    // nothing for it to prefer. That is the exact mirror of the trap recorded
    // at `a-force-native-gate-entry-with-no-registration-is-silent`.
    for (_, itr) in VALUES_ITR_CARRIERS {
        let itr = *itr;
        // The `JdkConcurrentHashMap` row (wave 76, L1): no CHM carrier is
        // minted under it; a real CHM iterator runs its JDK body.
        if itr.starts_with(CHM_CLASS) && chm_row_registers_nothing(r) {
            continue;
        }
        r.register(itr, "hasNext", "()Z", native_al_itr_has_next);
        r.register(itr, "next", "()Ljava/lang/Object;", native_al_itr_next);
        r.register(itr, "remove", "()V", native_al_itr_remove);
    }

    // The fabricated `HashMap$KeyItr` shape and the real classes that replaced
    // it as the default carrier (`MAP_KEY_ITR_CARRIERS`). The real ones need
    // these registrations for the same reason the collection carriers do: their
    // own JDK bodies walk `next`/`current`/`index`, which this VM's snapshot
    // iterator never populates. Paired with the entries in
    // `force_native_over_real_jdk_bytecode`.
    for c in std::iter::once(&"java/util/HashMap$KeyItr").chain(MAP_KEY_ITR_CARRIERS.iter()) {
        let c = *c;
        // The `JdkConcurrentHashMap` row (wave 76, L1): as above.
        if c.starts_with(CHM_CLASS) && chm_row_registers_nothing(r) {
            continue;
        }
        r.register(c, "hasNext", "()Z", native_map_key_itr_has_next);
        r.register(c, "next", "()Ljava/lang/Object;", native_map_key_itr_next);
        r.register(c, "remove", "()V", native_map_key_itr_remove);
    }
    r.set_category(__prev_cat);
}

pub(crate) fn native_al_itr_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = al_itr_delegate_foreign(ctx, args, "hasNext", "()Z") {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (cursor_slot, list_slot, _) = al_itr_slots_for(&*ctx, this);
    let cursor = match ctx.get_field(this, cursor_slot) {
        Value::Int(c) => c,
        _ => return Ok(Some(Value::Int(0))),
    };
    let list = match ctx.get_field(this, list_slot) {
        Value::Object(Some(l)) => l,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (_, size) = al_state(ctx, list);
    Ok(Some(Value::Int(if cursor < size { 1 } else { 0 })))
}

pub(crate) fn native_al_itr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = al_itr_delegate_foreign(ctx, args, "next", "()Ljava/lang/Object;") {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (cursor_slot, list_slot, _) = al_itr_slots_for(&*ctx, this);
    let cursor = match ctx.get_field(this, cursor_slot) {
        Value::Int(c) => c,
        _ => return Ok(Some(Value::Object(None))),
    };
    let list = match ctx.get_field(this, list_slot) {
        Value::Object(Some(l)) => l,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `ArrayList$Itr.next()` opens with `checkForComodification()`; `hasNext()`
    // deliberately does not, which is why the JDK's failure surfaces on the
    // iteration AFTER the offending `add`, not on the `hasNext` before it.
    al_itr_check_comod(ctx, this, list)?;
    al_view_itr_check_comod(ctx, this, list)?;
    let (data, size) = al_state(ctx, list);
    if cursor >= size {
        // Fix (item 3): `Iterator.next()` past the end must throw
        // `NoSuchElementException` per the JDK contract — returning `null`
        // silently masks the over-read and lets callers treat exhausted
        // iterators as holding a trailing `null` element.
        return Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                // MEASURED on HotSpot 25.0.4+7 (`apps/probes/MapCtorMsgProbe.java`):
                // an exhausted iterator or enumeration in this family answers a
                // NULL `getMessage()`. `Vector`'s enumeration is the one that does
                // not ("Vector Enumeration") and does not come through here. An
                // EMPTY message is this crate's marker for no message.
                message: String::new(),
            }
            .into(),
        );
    }
    let data = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    let val = ctx.get_array_element(data, cursor as usize);
    ctx.set_field(this, cursor_slot, Value::Int(cursor + 1));
    // Record the index just consumed in `lastRet` so a subsequent
    // `Iterator.remove()` is legal. Real-JDK `next()` does `lastRet = i`
    // via `dup_x1; putfield`; because this native shadows the bytecode that
    // write must be reproduced here, otherwise `remove()` sees the
    // constructor's `lastRet == -1` and throws `IllegalStateException`.
    let (_, _, last_ret_slot) = al_itr_slots_for(&*ctx, this);
    ctx.set_field(this, last_ret_slot, Value::Int(cursor));
    Ok(Some(val))
}

/// Native `ArrayList$Itr.remove()` — removes the last element returned by
/// `next()`. The real-JDK bytecode for `remove()` reads `expectedModCount`
/// and calls back into `ArrayList.remove(int)`; because the native-collections
/// model maintains list state outside the real `modCount` machinery, the whole
/// iterator (`hasNext` / `next` / `remove`) must be serviced natively and
/// kept self-consistent. Mirrors real-JDK semantics: remove `data[lastRet]`,
/// rewind `cursor` to `lastRet`, then reset `lastRet` to `-1`.
pub(crate) fn native_al_itr_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = al_itr_delegate_foreign(ctx, args, "remove", "()V") {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let (cursor_slot, list_slot, last_ret_slot) = al_itr_slots_for(&*ctx, this);
    let last_ret = match ctx.get_field(this, last_ret_slot) {
        Value::Int(l) => l,
        _ => -1,
    };
    if last_ret < 0 {
        // JDK contract: `remove()` before `next()` (or twice in a row)
        // throws IllegalStateException.
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "remove".to_string(),
        }
        .into());
    }
    let list = match ctx.get_field(this, list_slot) {
        Value::Object(Some(l)) => l,
        _ => return Ok(None),
    };
    // `ArrayList$Itr.remove()` checks for comodification after the `lastRet`
    // guard above and before touching the list — same order as the JDK, so a
    // stale iterator reports `IllegalStateException` where the JDK does and
    // `ConcurrentModificationException` where the JDK does.
    al_itr_check_comod(ctx, this, list)?;
    al_view_itr_check_comod(ctx, this, list)?;
    // Live `values()` view: capture the element about to be removed and, after
    // removing it from the snapshot, delete the matching entry from the source
    // map. Captured before removal because the shift clobbers the slot.
    let view_src = values_view_source(ctx, list);
    let removed = if view_src.is_some() {
        let (data, _) = al_state(ctx, list);
        data.map(|d| ctx.get_array_element(d, last_ret as usize))
    } else {
        None
    };
    // Delegate to the list's own native removal so backing-array shifting and
    // size bookkeeping stay in one place.
    //
    // GC-SAFETY: `native_al_remove_at`/`propagate_list_removal` dispatch
    // `equals` and allocate, so both `this` and `list` — bare Rust locals — can
    // move under them. The three field writes below and the `modCount` re-read
    // must all use post-move addresses; the `modCount` re-read is new here, so
    // the pin is too.
    // `CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE`: the node behind `lastRet`, found
    // by position before anything runs (see `values_view_node_at`).
    let node_hit = match (view_src, removed) {
        (Some(source), Some(value)) => {
            let reversed = values_view_reversed(&*ctx, list);
            values_view_node_at(&*ctx, source, last_ret as usize, value, reversed)
                .map(|n| (source, n))
        }
        _ => None,
    };
    // CP5-3: the by-value fallback below walks a reversed view's source tail
    // first. Asked now, while `list` is current (pure reads).
    let by_value_reversed = view_src.is_some() && view_by_value_reversed(&*ctx, list);
    let itr_pin = ctx.pin_native_root(this);
    let list_pin = ctx.pin_native_root(list);
    let hit_pins = node_hit
        .map(|(source, (node, _))| (ctx.pin_native_root(source), ctx.pin_native_root(node)));
    // Unpin on the failure paths too: `unpin_native_roots(itr_pin)` unwinds
    // `list_pin` with it (it was pushed later), so one handle releases both.
    if let Err(e) = native_al_remove_at(ctx, &[Value::Object(Some(list)), Value::Int(last_ret)]) {
        ctx.unpin_native_roots(itr_pin);
        return Err(e);
    }
    if let (Some((source, (node, lhm))), Some((source_pin, node_pin))) = (node_hit, hit_pins) {
        // `HashIterator.remove()`: `removeNode(p.hash, p.key, ..)` of THIS
        // node -- no `hashCode()`, and not some other key whose value is
        // equal.
        let source = ctx.read_native_pin(source_pin, source);
        let node = ctx.read_native_pin(node_pin, node);
        if let Err(e) = map_remove_node_by_walk(ctx, source, node, lhm) {
            ctx.unpin_native_roots(itr_pin);
            return Err(e);
        }
    } else if let (Some(source), Some(value)) = (view_src, removed) {
        if let Err(e) = propagate_list_removal_in(ctx, source, value, by_value_reversed) {
            ctx.unpin_native_roots(itr_pin);
            return Err(e);
        }
    }
    let this = ctx.read_native_pin(itr_pin, this);
    let list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(itr_pin);
    // Real-JDK: cursor = lastRet; lastRet = -1; expectedModCount = modCount.
    // The last of the three is what stops the removal we just performed from
    // failing this iterator's own next `next()`.
    ctx.set_field(this, cursor_slot, Value::Int(last_ret));
    ctx.set_field(this, last_ret_slot, Value::Int(-1));
    al_itr_sync_mod_count(ctx, this, list);
    // ...and the same line for the values-shaped views, whose removal was just
    // propagated INTO the source map by `propagate_list_removal` above and so
    // moved the very generation this iterator is watching.
    al_view_itr_seed(ctx, this, list);
    Ok(None)
}

pub(crate) fn native_map_key_itr_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let base = key_itr_base(ctx, this);
    let cursor = match ctx.get_field(this, base + MAP_KEY_ITR_FIELD_CURSOR) {
        Value::Int(c) => c,
        _ => return Ok(Some(Value::Int(0))),
    };
    let total = match ctx.get_field(this, base + MAP_KEY_ITR_FIELD_TOTAL) {
        Value::Int(t) => t,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(if cursor < total { 1 } else { 0 })))
}

pub(crate) fn native_map_key_itr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `HashMap$HashIterator.nextNode()` checks here and `hasNext()` does not,
    // so a loop that runs to exhaustion still throws on its next `next()`.
    map_itr_check_comod(&*ctx, this)?;
    let base = key_itr_base(ctx, this);
    let cursor = match ctx.get_field(this, base + MAP_KEY_ITR_FIELD_CURSOR) {
        Value::Int(c) => c,
        _ => return Ok(Some(Value::Object(None))),
    };
    let total = match ctx.get_field(this, base + MAP_KEY_ITR_FIELD_TOTAL) {
        Value::Int(t) => t,
        _ => return Ok(Some(Value::Object(None))),
    };
    if cursor >= total {
        // W7-1 residual: this answered `null` past the end where
        // `HashMap$KeyItr.next()` throws. `null` is the worst answer available —
        // it is also a legitimate element of a `HashMap` key set, so a caller
        // that over-ran its own `hasNext()` got an element it could not
        // distinguish from a real one and NPE'd somewhere else entirely. The
        // message is `native_snapshot_itr_next`'s verbatim: that function is the
        // other half of this iterator family (it delegates HERE for a
        // `HashMap$KeyItr` receiver) and already throws it on all three of its
        // own exhausted paths, so the two cannot drift into different reports.
        return Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: String::new(),
            }
            .into(),
        );
    }
    let keys = match ctx.get_field(this, base + MAP_KEY_ITR_FIELD_KEYS) {
        Value::Object(Some(arr)) => arr,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val = ctx.get_array_element(keys, cursor as usize);
    ctx.set_field(
        this,
        base + MAP_KEY_ITR_FIELD_CURSOR,
        Value::Int(cursor + 1),
    );
    // Record index just returned for a subsequent Iterator.remove().
    // Iterator allocated with <5 fields (older snapshot iterators) silently
    // ignores the write because alloc_object pre-sized the field block.
    let n_fields = ctx.object_num_fields(this);
    if n_fields > base + MAP_KEY_ITR_FIELD_LAST_RET {
        ctx.set_field(this, base + MAP_KEY_ITR_FIELD_LAST_RET, Value::Int(cursor));
    }
    Ok(Some(val))
}

/// Native `HashMap$KeyItr.remove()` — pairs with `native_hs_iterator` so
/// JDK code that iterates a HashSet (or HashMap.keySet()) and calls
/// `it.remove()` actually deletes the entry, instead of falling through
/// to the `Iterator.remove()` default that throws `UnsupportedOperationException`.
///
/// Trigger case: `com.sun.jmx.mbeanserver.MXBeanSupport.findMXBeanInterface`
/// builds a `HashSet<Class<?>>` of candidate MXBean interfaces and uses
/// `it.remove()` to drop superseded ones during platform-MBean registration.
/// Without this native, `ManagementFactory.getPlatformMBeanServer()` fails
/// with `NotCompliantMBeanException: sun.management.GarbageCollectorImpl: remove`,
/// blocking jboss-modules / WildFly / Keycloak boot.
/// `true` when `o` keeps its elements in the `TreeSet` side-table rather than a
/// bucket array -- a `java.util.TreeSet` or the `TreeMap$KeySet` carrier
/// `native_tm_key_set` mints, both of which carry the `native_ts_*` family.
pub(crate) fn is_tree_set_shaped(ctx: &dyn NativeContext, o: ObjectRef) -> bool {
    matches!(
        ctx.class_name_arc_of_id(ctx.class_id_of_object(o))
            .as_deref(),
        Some("java/util/TreeSet" | "java/util/TreeMap$KeySet")
    )
}

/// `CRATONVM_COMPAT_ITR_REMOVE_BY_NODE` — default ON, `=0` restores removal
/// by key. `HashMap$HashIterator.remove()` is
///
/// ```java
/// if (p == null) throw new IllegalStateException();
/// if (modCount != expectedModCount) throw new ConcurrentModificationException();
/// current = null;
/// removeNode(p.hash, p.key, null, false, false);
/// expectedModCount = modCount;
/// ```
///
/// -- the node's STORED hash, so no `hashCode()` call, and a comodification
/// check. The snapshot iterator held only the elements, so it removed by
/// element (`native_hs_remove`: a `hashCode()` and a chain walk of `equals`)
/// and never checked. Now an ordinary `HashSet` / `LinkedHashSet` iterator
/// carries each element's node ([`itr_snapshot_nodes`]) and removes it the
/// way `removeNode` does ([`map_remove_node_by_walk`]). The same walk serves
/// `retainAll`/`removeAll`'s iterator arm and `keySet().removeIf`. Round 13
/// wave 9, lane hashcompat4 (residual page item 6; `R13ShadowHashSetFamily`
/// scenario 5, 9 `hashCode()` calls against 8).
pub(crate) fn itr_remove_by_node_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_ITR_REMOVE_BY_NODE")
}

/// Whether two element slots hold the same reference (or are both null).
pub(crate) fn value_same_ref(a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Object(None), Value::Object(None)) => true,
        (Value::Object(Some(x)), Value::Object(Some(y))) => std::ptr::eq(x.as_ptr(), y.as_ptr()),
        _ => false,
    }
}

/// Whether an iterator over an ordinary set backed by `backing` carries its
/// nodes ([`itr_snapshot_nodes`]): the switch is on and `backing` has the
/// shape [`hs_backing_nodes`] walks (no view, no overlay row, a `HashMap` or
/// `LinkedHashMap` layout). Pure reads.
pub(crate) fn itr_snapshot_nodes_eligible(ctx: &dyn NativeContext, backing: ObjectRef) -> bool {
    if !itr_remove_by_node_enabled() || view_backing_source(ctx, backing).is_some() {
        return false;
    }
    if is_lhm_receiver(ctx, backing) {
        return true;
    }
    let facts = receiver_facts(ctx, backing);
    facts.has(CF_BUCKET_MAP_NAME)
        && !facts.has(CF_CHM)
        && !facts.has(CF_TREE_MAP)
        && !facts.has(CF_IDENTITY_MAP)
        && !facts.has(CF_UNMOD_WRAPPER)
        && !facts.has(CF_HASHTABLE_ANCESTRY)
        && hm_int_fast_len(ctx, backing).is_none()
}

/// The map whose nodes an iterator over a set backed by `backing` carries,
/// and whether its elements are `Map.Entry`s: `backing` itself for an ordinary
/// set ([`itr_snapshot_nodes_eligible`]), or, under
/// `CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE`, the SOURCE of a `keySet()` /
/// `entrySet()` view over a map [`plain_node_map`] accepts. Pure reads.
pub(crate) fn itr_node_map(ctx: &dyn NativeContext, backing: ObjectRef) -> Option<(ObjectRef, bool)> {
    let Some(source) = view_backing_source(ctx, backing) else {
        return itr_snapshot_nodes_eligible(ctx, backing).then_some((backing, false));
    };
    if !itr_remove_by_node_enabled() || !view_remove_by_node_enabled() {
        return None;
    }
    let entries = match view_backing_kind(ctx, backing) {
        VIEW_KIND_KEYSET => false,
        VIEW_KIND_ENTRYSET => true,
        _ => return None,
    };
    plain_node_map(ctx, source).then_some((source, entries))
}

/// The key an iterator element stands for: the element itself, or, for an
/// entry-set iterator (`entries`), the entry's key slot (a live copy's or a
/// node's, see [`entry_slots`]). A non-object entry answers a value no node
/// key can be. Pure reads.
pub(crate) fn itr_element_key(ctx: &dyn NativeContext, element: Value, entries: bool) -> Value {
    if !entries {
        return element;
    }
    match element {
        Value::Object(Some(e)) => {
            let (k, _, _) = entry_slots(ctx, e);
            ctx.get_field(e, k)
        }
        _ => Value::Int(0),
    }
}

/// The node behind each element of a snapshot `keys` taken from an ordinary
/// set's `backing` map, in the same order -- or empty when the switch is off,
/// the backing is not a shape [`hs_backing_nodes`] walks (views, overlay,
/// CHM, ...), or any node's key is not the very element at its index. Pure
/// reads: no Java, no allocation.
pub(crate) fn itr_snapshot_nodes(
    ctx: &dyn NativeContext,
    backing: ObjectRef,
    keys: &[Value],
) -> Vec<ObjectRef> {
    if !itr_remove_by_node_enabled() || keys.is_empty() {
        return Vec::new();
    }
    let Some((nodes, lhm)) = hs_backing_nodes(ctx, backing) else {
        return Vec::new();
    };
    if nodes.len() != keys.len() {
        return Vec::new();
    }
    let holds = |node: &ObjectRef, key: &Value| {
        let stored = if lhm {
            ctx.get_field(*node, LHM_NODE_KEY)
        } else {
            get_node_key(ctx, *node)
        };
        value_same_ref(stored, *key)
    };
    if nodes.iter().zip(keys).all(|(n, k)| holds(n, k)) {
        return nodes;
    }
    // A reversed `LinkedKeySet` / `LinkedEntrySet` (C3-1) hands its elements
    // out tail first: the same nodes, walked backwards.
    if lhm
        && lhm_reversed_live_views_enabled()
        && nodes.iter().rev().zip(keys).all(|(n, k)| holds(n, k))
    {
        let mut nodes = nodes;
        nodes.reverse();
        return nodes;
    }
    Vec::new()
}

/// A node's stored hash in either layout: `LinkedHashMap$Entry` (`lhm`) or a
/// `HashMap` node (layout-aware, see [`get_node_hash`]).
pub(crate) fn either_node_hash(ctx: &dyn NativeContext, node: ObjectRef, lhm: bool) -> i32 {
    if lhm {
        match ctx.get_field(node, LHM_NODE_HASH) {
            Value::Int(h) => h,
            _ => 0,
        }
    } else {
        get_node_hash(ctx, node)
    }
}

/// A node's key in either layout (see [`either_node_hash`]).
pub(crate) fn either_node_key(ctx: &dyn NativeContext, node: ObjectRef, lhm: bool) -> Value {
    if lhm {
        ctx.get_field(node, LHM_NODE_KEY)
    } else {
        get_node_key(ctx, node)
    }
}

/// `removeNode(p.hash, p.key, null, false, ..)` for a node `p` an iterator
/// handed out, over either node layout (`lhm`): the chain of `p.hash` is
/// walked from its head and the FIRST node that matches -- `e.hash == hash
/// && (e.key == key || key.equals(e.key))` -- is unlinked, exactly as the JDK
/// does. So there is no `hashCode()`, but a same-hash node ahead of `p` whose
/// key is a different object IS asked `p.key.equals(..)`, which unlinking by
/// identity skipped. Answers whether a node was unlinked; `false` for a node
/// that is no longer in its chain. The node is accounted exactly as
/// [`map_unlink_node`] / [`lhm_unlink_node`] account one.
pub(crate) fn map_remove_node_by_walk(
    ctx: &mut dyn NativeContext,
    map: ObjectRef,
    node: ObjectRef,
    lhm: bool,
) -> Result<bool, MethodCallFailed> {
    let next_slot = if lhm { LHM_NODE_NEXT } else { NODE_FIELD_NEXT };
    let hash = either_node_hash(&*ctx, node, lhm);
    let key = either_node_key(&*ctx, node, lhm);
    let (buckets, size, cap) = if lhm {
        lhm_state(&*ctx, map)
    } else {
        map_state(&*ctx, map)
    };
    let Some(buckets) = buckets else {
        return Ok(false);
    };
    let mut cur = ctx.get_array_element(buckets, map_bucket_index(hash, cap));
    // `CRATONVM_COMPAT_HASHMAP_TREE_BINS` / `_LHM_TREE_BINS`: a tree bin is
    // `removeNode(p.hash, p.key, null, false, false)` itself, the JDK's
    // bytecode (for a `LinkedHashMap`, `afterNodeRemoval` unlinks the list).
    let tree_family = if lhm {
        lhm_tree_family(&*ctx, map)
    } else {
        hm_tree_family(&*ctx, map)
    };
    if tree_family && hm_tree_head(&*ctx, cur).is_some() {
        let map_pin = ctx.pin_native_root(map);
        let removed =
            hm_remove_node_bytecode(ctx, map, hash, key, Value::Object(None), false, false);
        let map = ctx.read_native_pin(map_pin, map);
        ctx.unpin_native_roots(map_pin);
        let removed = removed?.is_some();
        if removed && lhm {
            lhm_sync_size_after_bytecode(ctx, map);
        }
        return Ok(removed);
    }
    let map_pin = ctx.pin_native_root(map);
    let node_pin = ctx.pin_native_root(node);
    let key_pin = pin_value(ctx, key);
    // Every pin above `map_pin` is released with it, on every exit.
    let found = (|| -> Result<Option<ObjectRef>, MethodCallFailed> {
        for _ in 0..chain_walk_limit(size) {
            let Value::Object(Some(c)) = cur else {
                return Ok(None);
            };
            let node_now = ctx.read_native_pin(node_pin, node);
            if std::ptr::eq(c.as_ptr(), node_now.as_ptr()) {
                return Ok(Some(c));
            }
            let key_now = read_pinned_elem(ctx, key_pin, key);
            let c_key = either_node_key(&*ctx, c, lhm);
            if either_node_hash(&*ctx, c, lhm) == hash {
                if value_same_ref(c_key, key_now) {
                    return Ok(Some(c));
                }
                if let Value::Object(Some(k)) = key_now {
                    // `key.equals(k)`: Java, so the chain cursor is rooted.
                    let c_pin = ctx.pin_native_root(c);
                    let equal = match c_key {
                        Value::Object(Some(ck)) => map_keys_equal(ctx, k, ck),
                        _ if unbox_wrapper(&*ctx, k).is_some() => Ok(false),
                        _ => ctx
                            .invoke_virtual(k, "equals", "(Ljava/lang/Object;)Z", &[c_key])
                            .map(|r| matches!(r, Some(Value::Int(v)) if v != 0)),
                    };
                    let c = ctx.read_native_pin(c_pin, c);
                    ctx.unpin_native_roots(c_pin);
                    if equal? {
                        return Ok(Some(c));
                    }
                    cur = ctx.get_field(c, next_slot);
                    continue;
                }
            }
            cur = ctx.get_field(c, next_slot);
        }
        Ok(None)
    })();
    let map = ctx.read_native_pin(map_pin, map);
    ctx.unpin_native_roots(map_pin);
    let Some(victim) = found? else {
        return Ok(false);
    };
    Ok(if lhm {
        lhm_unlink_node(ctx, map, victim)
    } else {
        map_unlink_node(ctx, map, victim)
    })
}

/// `Iterator.remove()` by node for an iterator [`native_hs_iterator`] minted
/// with [`itr_snapshot_nodes`]: `Ok(true)` when the element's node was
/// unlinked (the caller then clears `lastRet` and re-seeds
/// `expectedModCount`), `Ok(false)`, before any Java has run, when this
/// iterator carries no nodes or the node no longer holds the element, leaving
/// the removal to the by-key path. The walk can run `equals`
/// ([`map_remove_node_by_walk`]); the caller re-reads its references.
pub(crate) fn key_itr_remove_node(
    ctx: &mut dyn NativeContext,
    itr: ObjectRef,
    base: usize,
    keys: ObjectRef,
    last_ret: i32,
    set: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    let total = match ctx.get_field(itr, base + MAP_KEY_ITR_FIELD_TOTAL) {
        Value::Int(t) if t > 0 => t as usize,
        _ => return Ok(false),
    };
    let last_ret = last_ret as usize;
    if last_ret >= total || ctx.array_length(keys) != total * 2 {
        return Ok(false);
    }
    let Value::Object(Some(node)) = ctx.get_array_element(keys, total + last_ret) else {
        return Ok(false);
    };
    let Some(backing) = hs_backing_map(&*ctx, set) else {
        return Ok(false);
    };
    // An ordinary set's nodes are its backing's; a `keySet()` / `entrySet()`
    // view's are its source's (`CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE`).
    let Some((map, entries)) = itr_node_map(&*ctx, backing) else {
        return Ok(false);
    };
    let element = itr_element_key(&*ctx, ctx.get_array_element(keys, last_ret), entries);
    let lhm = is_lhm_receiver(&*ctx, map);
    let stored = either_node_key(&*ctx, node, lhm);
    if !value_same_ref(stored, element) {
        return Ok(false);
    }
    map_remove_node_by_walk(ctx, map, node, lhm)
}

pub(crate) fn native_map_key_itr_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Iterators allocated via the legacy 3-field snapshot path
    // (`make_iterator_from_array`) have no backing reference — surface
    // UnsupportedOperationException so callers see the JDK-spec behaviour.
    let base = key_itr_base(ctx, this);
    let n_fields = ctx.object_num_fields(this);
    if n_fields <= base + MAP_KEY_ITR_FIELD_LAST_RET {
        return Err(unsupported_op());
    }
    let last_ret = match ctx.get_field(this, base + MAP_KEY_ITR_FIELD_LAST_RET) {
        Value::Int(v) => v,
        _ => -1,
    };
    if last_ret < 0 {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "remove".to_string(),
        }
        .into());
    }
    let backing = match ctx.get_field(this, base + MAP_KEY_ITR_FIELD_BACKING) {
        Value::Object(Some(b)) => b,
        _ => return Err(unsupported_op()),
    };
    let keys = match ctx.get_field(this, base + MAP_KEY_ITR_FIELD_KEYS) {
        Value::Object(Some(a)) => a,
        _ => return Ok(None),
    };
    let key = ctx.get_array_element(keys, last_ret as usize);
    // `CRATONVM_COMPAT_ITR_REMOVE_BY_NODE`: the comodification check the
    // JDK's `remove()` makes after its `IllegalStateException` test, then the
    // node (see `itr_remove_by_node_enabled`).
    if itr_remove_by_node_enabled() {
        map_itr_check_comod(&*ctx, this)?;
        // The walk can run `equals` (Java): the iterator is rooted across it.
        // `Ok(false)` has run no Java, so the references below stay current.
        let itr_pin = ctx.pin_native_root(this);
        let removed = key_itr_remove_node(ctx, this, base, keys, last_ret, backing);
        let itr = ctx.read_native_pin(itr_pin, this);
        ctx.unpin_native_roots(itr_pin);
        match removed {
            Ok(false) => {}
            Ok(true) => {
                ctx.set_field(itr, base + MAP_KEY_ITR_FIELD_LAST_RET, Value::Int(-1));
                map_itr_seed_expected(ctx, itr);
                return Ok(None);
            }
            // `current = null` precedes `removeNode`, so a throwing `equals`
            // still leaves the iterator with nothing to remove.
            Err(e) => {
                ctx.set_field(itr, base + MAP_KEY_ITR_FIELD_LAST_RET, Value::Int(-1));
                return Err(e);
            }
        }
    }
    // Delegate to the receiver's own remove(Object). HashSet.remove unwraps
    // to native_map_remove on the backing HashMap; HashMap.keySet().iterator()
    // would route through native_hs_remove on the synthetic KeySet, which
    // chains to native_map_remove on the underlying map. Either way the
    // entry actually disappears from the source collection.
    // `native_hs_remove` can dispatch equality/hashCode and allocate. Keep the
    // iterator itself rooted across that nested native call: its `lastRet`
    // update below must use the post-GC address, not a from-space reference.
    let this_pin = ctx.pin_native_root(this);
    // A `ConcurrentHashMap.newKeySet()` view is not HashSet-shaped: its slot 0
    // is the backing ConcurrentHashMap, not a bucket-table HashMap, so
    // `native_hs_remove` would read the segments array as buckets. Route it to
    // the view's own remove, which takes the segment monitor.
    // `CollectionView.retainAll`/`removeAll` reach here through `it.remove()`.
    let removed = if is_key_set_view(ctx, backing) {
        native_ksv_remove(ctx, &[Value::Object(Some(backing)), key])
    } else if is_tree_set_shaped(ctx, backing) {
        // A `TreeSet` / `TreeMap$KeySet` backing is NOT bucket-shaped: its
        // elements live in the `ts_array_table` side-table, so `native_hs_remove`
        // reads slot 0 as a bucket array, finds nothing, and reports success
        // having removed nothing.
        //
        // MEASURED, and this is why `TreeMap$KeyIterator` could not simply be
        // added to `MAP_KEY_ITR_CARRIERS` and left there: with the carrier moved
        // and this branch missing, `new TreeSet<>(["a","b","c"])` +
        // `iterator().next(); remove()` left `[a, b, c]` where HotSpot leaves
        // `[b, c]` -- a SILENT no-op. It reddened `RJdkJmx`, whose
        // `ImmutableDescriptor.union` walks a case-insensitive `TreeMap`.
        //
        // The class was right and the behaviour was wrong, which is exactly what
        // a probe that only compares `getClass()` cannot see.
        native_ts_remove(ctx, &[Value::Object(Some(backing)), key])
    } else {
        native_hs_remove(ctx, &[Value::Object(Some(backing)), key])
    };
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let _ = removed?;
    ctx.set_field(this, base + MAP_KEY_ITR_FIELD_LAST_RET, Value::Int(-1));
    // The removal above bumped the source's generation; adopt it, or the next
    // `next()` would trip the check this iterator installed on itself.
    map_itr_seed_expected(ctx, this);
    Ok(None)
}

