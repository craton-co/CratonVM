// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// Phase 14 Step 5: LinkedList
// ===========================================================================

// LinkedList = 3-field synthetic (field 0 = head Node, field 1 = tail Node, field 2 = Int size)
// Node = 3-field synthetic (field 0 = prev Node, field 1 = next Node, field 2 = element value)
//
// As with LinkedHashMap, the synthetic slot indices alias real-JDK declared
// fields once the real `java.util.LinkedList` class is loaded (real-JDK has
// `size`, `first`, `last` after AbstractList's `modCount`). Storing state in
// a per-object overlay map sidesteps the layout collision without altering
// HashMap allocation/resolution. The instance node fields (`LL_NODE_*`) are
// kept slot-based because `LinkedList$Node` is purely synthetic in our VM
// (we never load the real class, since Node is private/inner and bytecode
// doesn't `getfield` it directly).
pub(crate) fn ll_overlay() -> &'static Mutex<OverlayRows<StdHashMap<&'static str, Value>>> {
    static OVERLAY: std::sync::OnceLock<Mutex<OverlayRows<StdHashMap<&'static str, Value>>>> =
        std::sync::OnceLock::new();
    OVERLAY.get_or_init(|| Mutex::new(OverlayRows::new()))
}
// LL overlay key: rekeyed onto `ctx.identity_hash_code(this)` so the
// side-table survives a moving-GC relocation. Originally
// `this.as_ptr() as usize`, which a moving GC invalidates the instant
// it relocates the LinkedList — silently dropping head/tail/size.
/// Map the LinkedList overlay key name to the real JDK heap field name
/// (the overlay uses head/tail; the JDK fields are first/last/size).
pub(crate) fn ll_real_field(name: &str) -> Option<&'static str> {
    match name {
        "head" => Some("first"),
        "tail" => Some("last"),
        "size" => Some("size"),
        _ => None,
    }
}

pub(crate) fn ll_get(ctx: &dyn NativeContext, this: ObjectRef, name: &'static str) -> Value {
    // PERF (gc-common w26-a): the key BEFORE the table lock. `widened_obj_key`
    // takes a registry shard and the process-global owner-index mutex; nested
    // inside this one process-global table lock, it held every other thread's
    // LinkedList op off the table for both.
    let key = widened_obj_key(ctx, this);
    {
        let ov = ll_overlay().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = ov.get(&key).and_then(|m| m.get(name)).copied() {
            return v;
        }
    }
    // Overlay MISS: fall back to the real JDK heap field. This is the path for a
    // LinkedList populated by real bytecode that bypassed native_ll_* — chiefly
    // Java deserialization (LinkedList.readObject -> linkLast writes the real
    // first/last/size + node next/prev, never our overlay). Mirrored writes in
    // ll_set keep the heap consistent for native-populated lists.
    if let Some(real) = ll_real_field(name) {
        if let Some(slot) = ctx.resolve_field_index("java/util/LinkedList", real) {
            if slot < ctx.object_num_fields(this) {
                return ctx.get_field(this, slot);
            }
        }
    }
    Value::Object(None)
}
pub(crate) fn ll_set(ctx: &mut dyn NativeContext, this: ObjectRef, name: &'static str, v: Value) {
    // Key before the table lock, as in `ll_get`.
    let key = widened_obj_key(ctx, this);
    ll_overlay()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(key)
        .or_default()
        .insert(name, v);
    // Mirror the structural pointers to the REAL JDK heap fields so real-bytecode
    // Java serialization works: LinkedList.writeObject reads `size` and walks the
    // real `first`->`next` links (the node next/prev live on the real node fields
    // already). Without this a populated LinkedList serialized as size=0 + zero
    // elements. Overlay stays the read source of truth for native callers.
    if let Some(real) = ll_real_field(name) {
        if let Some(slot) = ctx.resolve_field_index("java/util/LinkedList", real) {
            if slot < ctx.object_num_fields(this) {
                ctx.set_field(this, slot, v);
            }
        }
    }
}

pub(crate) const LL_FIELD_HEAD: usize = 0;
pub(crate) const LL_FIELD_TAIL: usize = 1;
pub(crate) const LL_FIELD_SIZE: usize = 2;
// Real JDK LinkedList$Node layout: item@0, next@1, prev@2 (java.util.LinkedList
// .Node declares them in that order). Use the real order so real-bytecode
// LinkedList.writeObject (`for (Node x = first; x != null; x = x.next)
// s.writeObject(x.item)`) reads the right slots — the old synthetic order
// (prev@0/next@1/elem@2) made `getfield item` read prev.
pub(crate) const LL_NODE_ELEM: usize = 0;
pub(crate) const LL_NODE_NEXT: usize = 1;
pub(crate) const LL_NODE_PREV: usize = 2;

pub(crate) fn ll_alloc_node(
    ctx: &mut dyn NativeContext,
    element: Value,
) -> Result<ObjectRef, MethodCallFailed> {
    // Family-1 stale-at-store fix (cce0079): the node alloc is GC-capable
    // and can move `element` — store its refreshed address, not the pre-GC
    // one (a later reader of the node would see a recycled object).
    let eh = pin_value(ctx, element);
    let node = try_alloc_synthetic(ctx, "java/util/LinkedList$Node", 3)?;
    let element = read_pinned_elem(ctx, eh, element);
    if eh != usize::MAX {
        ctx.unpin_native_roots(eh);
    }
    ctx.set_field(node, LL_NODE_PREV, Value::Object(None));
    ctx.set_field(node, LL_NODE_NEXT, Value::Object(None));
    ctx.set_field(node, LL_NODE_ELEM, element);
    Ok(node)
}

pub(crate) fn ll_size(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    match ll_get(ctx, this, "size") {
        Value::Int(n) => n,
        _ => 0,
    }
}

pub(crate) fn register_linked_list_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/LinkedList";
    registry.register(c, "<init>", "()V", native_ll_init);
    registry.register(
        c,
        "<init>",
        "(Ljava/util/Collection;)V",
        native_ll_init_from_collection,
    );
    registry.register(c, "add", "(Ljava/lang/Object;)Z", native_ll_add);
    // Positional insert/removal — required so the overlay LinkedList stays the
    // single source of truth. Without these, real-JDK `add(int,E)` /
    // `remove(int)` bytecode runs against the JDK field layout (`first`/`last`)
    // that our synthetic `<init>` never populates, silently desyncing the list
    // (e.g. Felix's resolver permutation queue: add(0,perm) writes one
    // structure, isEmpty()/remove(0) read the overlay → permutation lost).
    registry.register(c, "add", "(ILjava/lang/Object;)V", native_ll_add_at);
    registry.register(c, "remove", "(I)Ljava/lang/Object;", native_ll_remove_at);
    // remove-by-VALUE (and the Deque occurrence removers) MUST also hit the
    // overlay — the real-JDK `remove(Object)` bytecode walks `first`/`last`
    // (which our synthetic `<init>` never populates) and silently no-ops, so the
    // overlay never shrinks. This desynced the Apache httpcore-nio pool's
    // `available.remove(entry)` (entries accumulated, eventually a null → NPE at
    // `AbstractNIOConnPool.shutdown`) once keep-alive made the client pool
    // connections (ES-HANG-02 residual 2).
    registry.register(
        c,
        "remove",
        "(Ljava/lang/Object;)Z",
        native_ll_remove_object,
    );
    registry.register(
        c,
        "removeFirstOccurrence",
        "(Ljava/lang/Object;)Z",
        native_ll_remove_object,
    );
    registry.register(
        c,
        "removeLastOccurrence",
        "(Ljava/lang/Object;)Z",
        native_ll_remove_last_occurrence,
    );
    registry.register(c, "addFirst", "(Ljava/lang/Object;)V", native_ll_add_first);
    registry.register(c, "addLast", "(Ljava/lang/Object;)V", native_ll_add_last);
    registry.register(c, "get", "(I)Ljava/lang/Object;", native_ll_get);
    registry.register(
        c,
        "set",
        "(ILjava/lang/Object;)Ljava/lang/Object;",
        native_ll_set,
    );
    registry.register(c, "getFirst", "()Ljava/lang/Object;", native_ll_get_first);
    registry.register(c, "getLast", "()Ljava/lang/Object;", native_ll_get_last);
    registry.register(
        c,
        "removeFirst",
        "()Ljava/lang/Object;",
        native_ll_remove_first,
    );
    registry.register(
        c,
        "removeLast",
        "()Ljava/lang/Object;",
        native_ll_remove_last,
    );
    registry.register(c, "size", "()I", native_ll_size);
    registry.register(c, "isEmpty", "()Z", native_ll_is_empty);
    registry.register(c, "contains", "(Ljava/lang/Object;)Z", native_ll_contains);
    registry.register(
        c,
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
        native_ll_remove_if,
    );
    registry.register(c, "clear", "()V", native_ll_clear);
    registry.register(c, "peek", "()Ljava/lang/Object;", native_ll_peek);
    registry.register(c, "poll", "()Ljava/lang/Object;", native_ll_poll);
    // ---- the four `LinkedList` methods real bytecode was still MUTATING ----
    //
    // `ll_get` reads the overlay FIRST and only falls back to the real
    // `first`/`last`/`size` on a MISS (see its overlay-MISS arm). `ll_set`
    // mirrors overlay -> heap, but nothing mirrors heap -> overlay, and
    // `<init>` seeds the overlay for every natively constructed list — so once
    // a `LinkedList` exists, ANY real-bytecode write to `size`/`first`/`last`
    // is invisible to every native reader, permanently. `native_ll_iterator`
    // records the resulting reproducer (`NullPointerException: Cannot read
    // field "item"` in real `LinkedList$ListItr.previous`) and concludes
    // "keep the natives the single writer". These four registrations are the
    // rest of that conclusion: without them the class still has four public
    // entry points that run real mutating bytecode.
    //
    // Verified against `javap -p -c java.util.LinkedList` (JDK 25.0.3+9), which
    // is what makes this a list of four and not a guess:
    //
    //   pollFirst()  `getfield first` then `invokevirtual unlinkFirst` — real
    //                `unlinkFirst` writes `first`, `size` and `modCount`. The
    //                `java/util/Deque` interface bridge does NOT cover it
    //                (`register_queue_deque_interface_natives` registers
    //                `peekFirst`/`peekLast` but no `pollFirst`/`pollLast`), and
    //                an interface row would not win anyway: `LinkedList`
    //                declares the method itself.
    //   pollLast()   same shape through `unlinkLast`.
    //   addAll(I,C)  links the new nodes inline, writing `first`/`last`/`size`.
    //                Its no-index sibling `addAll(C)` was already registered
    //                (`native_ll_add_all`, in the bulk-op registrar) — this is
    //                the half of the pair that was missed.
    //   descendingIterator()  does NOT go through the registered
    //                `listIterator(I)`: its bytecode is
    //                `new LinkedList$ListItr; dup; …; invokevirtual size;
    //                 invokespecial ListItr.<init>(LinkedList;I)`, i.e. it
    //                constructs the real node-live `ListItr` DIRECTLY, so
    //                `remove()` calls real `LinkedList.unlink`. This is the
    //                exact path `native_ll_iterator`'s comment names as the
    //                measured reproducer, and it was the one door left open.
    //
    // The remaining unregistered `LinkedList` methods were checked and are
    // benign: `remove()`, `element()`, `offerFirst`, `offerLast`, `push`, `pop`
    // are one-line `invokevirtual`s onto methods registered above; `peekFirst`,
    // `peekLast`, `indexOf`, `lastIndexOf` only READ the mirrored heap chain;
    // `clone()` writes only the fresh clone's own fields before populating it
    // through the native `add`.
    registry.register(c, "pollFirst", "()Ljava/lang/Object;", native_ll_poll_first);
    registry.register(c, "pollLast", "()Ljava/lang/Object;", native_ll_poll_last);
    registry.register(
        c,
        "addAll",
        "(ILjava/util/Collection;)Z",
        native_ll_add_all_at,
    );
    registry.register(
        c,
        "descendingIterator",
        "()Ljava/util/Iterator;",
        native_ll_descending_iterator,
    );
    registry.register(c, "offer", "(Ljava/lang/Object;)Z", native_ll_add);
    registry.register(c, "toArray", "()[Ljava/lang/Object;", native_ll_to_array);
    // LinkedList.toArray(T[]) — typed overload. Without this the JDK's
    // node-iterating bytecode runs against our overlay structure and returns an
    // array full of nulls.
    //
    // CORRECTED 2026-08-11: the original of this comment gave the reason as
    // *"the JDK `first` field is always null"*, and that has not been true
    // since `ll_set` began mirroring the overlay into the real fields (see
    // `ll_get`'s overlay-MISS arm). Measured reflectively under
    // `--add-opens java.base/java.util=ALL-UNNAMED`, a native `LinkedList`
    // carries a real `size` and a real `first`/`last` node chain whose
    // `item`/`next`/`prev` walk matches HotSpot's exactly, and
    // `descendingIterator()` — which nothing here intercepts — runs real
    // `LinkedList$ListItr` bytecode over it successfully. The registration is
    // still right, for a different reason: the overlay stays AUTHORITATIVE for
    // `size` on read, so real bytecode that MUTATES the list leaves the two
    // owners disagreeing (`native_ll_iterator` records the reproducer). Keep
    // the natives the single writer; do not "simplify" this away on the
    // strength of the old reason.
    registry.register(
        c,
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
        native_ll_to_array_typed,
    );
    registry.register(c, "toString", "()Ljava/lang/String;", native_ll_to_string);
    registry.register(c, "iterator", "()Ljava/util/Iterator;", native_ll_iterator);
    registry.register(c, "stream", "()Ljava/util/stream/Stream;", native_ll_stream);
    registry.register(
        c,
        "spliterator",
        "()Ljava/util/Spliterator;",
        native_ll_spliterator,
    );
    // SportMe r54: real-JDK LinkedList$ListItr reads `LinkedList.size` and `first`
    // fields via getfield; the `List.sort` default-method path crashed with
    // NoSuchElementException inside Spring's `processDeferredImportSelectors`.
    // Override `listIterator()` / `listIterator(I)` to return a snapshot-array
    // iterator with cursor + list_ref so `next`/`hasNext`/`set` work via our
    // overlay.
    //
    // CORRECTED 2026-08-11: this comment used to blame *"our overlay-based LL
    // never writes those"*, and that reason is stale — `ll_set` mirrors both
    // fields, and the whole real node chain reads back correctly (see the
    // `toArray(T[])` note above). The registration is still correct because the
    // overlay wins on READ while real bytecode writes the heap, so a real
    // `ListItr` that mutates leaves the two disagreeing. A real
    // `Arrays$ArrayItr` cannot stand in either — it has no
    // `previous`/`set`/`add` — and a real `ArrayList$ListItr` over a snapshot
    // would drop `set()` writes silently, which is worse. Reasoned in
    // docs/feature-designs/jdk-only-mode.md
    //
    // UNDER `--jdk-only` THIS PAIR WAS AN OPEN GAP UNTIL 2026-08-11:
    // `cratonvm/internal/LinkedListSnapshotListItr` was refused, so
    // `listIterator()` and every real `AbstractList` method reaching it
    // (`equals`, `hashCode`, `indexOf` against a foreign list) raised
    // `NoClassDefFoundError`. Two independent gates refused it, not one, and
    // both had to move in the same commit:
    //
    //   1. the class:  `try_alloc_synthetic` stamped `CompatibilityStub`, which
    //      §5 forbids. Now minted through `ensure_vm_internal_class`
    //      (`ClassOrigin::VmInternal`) — the door ten `MethodHandles`
    //      combinator carriers took the same day, on the same test
    //      (docs/known-issues/jdk-only/W7-13-strict-mh-insert-wrapper.md). The
    //      class qualifies: no image declares `cratonvm/internal/*`, no class
    //      file could exist for it, and it is a 3-slot tuple of the VM's own
    //      state.
    //   2. its natives: the name was on `VM_MINTED_STAND_IN_RECEIVERS` in
    //      native-api/src/no_image_receiver.rs, so `register()` re-tagged all
    //      nine registrations below from the ambient `Bridge` to
    //      `SyntheticStub` BEFORE the `JdkOnly` arm read the kind — and that
    //      arm then returned without inserting them. Confirmed by
    //      `--dump-native-registry`, which printed `synthetic-stub` for all
    //      nine though this registrar sets `Bridge`. Moved to
    //      `VM_SERVICE_RECEIVERS`.
    //
    // Clearing 1 without 2 leaves strict holding a well-formed carrier with no
    // implementation: the failure moves from `NoClassDefFoundError` at
    // `listIterator()` to `UnsatisfiedLinkError` at the first `hasNext()` —
    // later, further from the cause, and no better for the contract. That is
    // the exact trap `STRICT_STILL_FABRICATES` is kept as an empty table to
    // name ("the two halves of this defect are the registration's kind and the
    // class's existence"), running in the other direction. Measured, not
    // inferred, in
    // docs/feature-designs/collections-interception.md
    //
    // **The gap does NOT leave the census, and that is deliberate.** The row
    // that names it is `native-shadows-bytecode` on
    // `java/util/LinkedList.listIterator`, keyed on the REAL class and on this
    // registration, so no change to the carrier can move it. Only
    // `compatibility-class-requested` for the carrier's own name disappears.
    // CratonVM still serves `listIterator` from a native instead of running the
    // JDK's bytecode; that is an ownership defect and it is still counted.
    //
    // What the same record measures, and what made the old refusal worse than
    // it looked: it was NOT loud everywhere. `new ArrayList<>(List.of("a",
    // "b","c")).equals(linkedList)` answered **false** under `--jdk-only` — no
    // exception, just the wrong answer — because `native_al_equals`'s
    // cross-layout arm called `collection_elements_generic`, whose signature
    // had no error channel, so the `NoClassDefFoundError` its `iterator()` call
    // raised became `Vec::new()` and the two sides differed in length. That
    // helper now returns a `Result` and its whole family with it
    // (docs/feature-designs/jdk-only-mode.md),
    // which matters independently of the mint above: a refusal that reaches a
    // caller with nowhere to put it does not stay a refusal, and the next
    // carrier to be refused here would have been laundered the same way.
    registry.register(
        c,
        "listIterator",
        "()Ljava/util/ListIterator;",
        native_ll_list_iterator,
    );
    registry.register(
        c,
        "listIterator",
        "(I)Ljava/util/ListIterator;",
        native_ll_list_iterator_idx,
    );

    // LinkedList$Itr — synthetic 3-field overlay:
    //   field 0 = current node (the next node to be returned by `next()`)
    //   field 1 = list ref
    //   field 2 = last node returned by `next()` (for `remove()`); null when
    //             `next()` has not been called or after a `remove()`.
    let itr = "java/util/LinkedList$Itr";
    registry.register(itr, "hasNext", "()Z", native_ll_itr_has_next);
    registry.register(itr, "next", "()Ljava/lang/Object;", native_ll_itr_next);
    registry.register(itr, "remove", "()V", native_ll_itr_remove);

    // LinkedList snapshot ListIterator — synthetic 3-field overlay
    //   field 0 = Object[] snapshot of list elements
    //   field 1 = Int cursor (nextIndex)
    //   field 2 = list ref (so set() can mutate the backing LinkedList node)
    //
    // CratonVM-internal class name, deliberately NOT
    // `java/util/LinkedList$ListItr`: in real-JDK mode that name resolves to
    // the REAL preloaded JDK class, so `alloc_synthetic` bound our 3-slot
    // overlay to the real 5-field layout and descriptor-aware field coercion
    // mangled the Int cursor write into the real `next:Node` slot — `next()`
    // never advanced, so `AbstractList.equals` (real bytecode iterating via
    // `listIterator()`) compared element 0 forever and answered false for
    // ANY LinkedList vs other-List comparison (kafka ConfigDef
    // testGroupInference assertEquals(ArrayList, LinkedList)). The interface
    // natives registered on `java/util/ListIterator` share the same
    // array@0/cursor@1 layout, so dispatch through either route agrees.
    // The REAL carrier, preferred by both entry points above. Registered
    // before the fallback so a reader meets them in dispatch order.
    register_ll_list_itr_natives(registry);

    let lit = "cratonvm/internal/LinkedListSnapshotListItr";
    // `register_with_kind`, NOT the ambient `register`, and the difference is
    // the census column `kind_stated` rather than the kind itself — the kind is
    // `Bridge` either way, in both runtime modes, so nothing about dispatch or
    // policy changes here.
    //
    // Until the 2026-08-11 move to `VM_SERVICE_RECEIVERS` these nine carried
    // `kind_stated = true`, because `register()`'s
    // `receiver_declared_by_no_supported_image` arm re-tagged them
    // `SyntheticStub` and STATED the kind (a measurement adjudicated it). The
    // move is the right fix for the kind and it silently dropped the
    // adjudication: an EXCLUSION restores the ambient category, and an ambient
    // category is `kind_stated = false`. `scripts/jdk-only-kind-map.py` scores
    // that as a regression on purpose and refuses it — adjudication there is
    // one-way (`false -> true` freely, never `true -> false`), because a row
    // that goes back to inheriting is a row the next ambient edit moves in
    // silence again. Re-freezing the baseline to the lost bit would have
    // converted the gate into a rubber stamp for exactly the defect it exists
    // to catch.
    //
    // Stating `Bridge` here is the honest form of the same claim the exclusion
    // makes: contract §11 reviewed this receiver as a VM service, and `Bridge`
    // is the only tag that survives `NativeKind::allowed_in(JdkOnly)`.
    // W7-62-ratchets-and-dead-code.md
    registry.register_with_kind(
        lit,
        "hasNext",
        "()Z",
        native_ll_listitr_has_next,
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        lit,
        "next",
        "()Ljava/lang/Object;",
        native_ll_listitr_next,
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        lit,
        "hasPrevious",
        "()Z",
        native_ll_listitr_has_previous,
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        lit,
        "previous",
        "()Ljava/lang/Object;",
        native_ll_listitr_previous,
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        lit,
        "nextIndex",
        "()I",
        native_ll_listitr_next_index,
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        lit,
        "previousIndex",
        "()I",
        native_ll_listitr_previous_index,
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        lit,
        "set",
        "(Ljava/lang/Object;)V",
        native_ll_listitr_set,
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        lit,
        "remove",
        "()V",
        native_ll_listitr_remove,
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        lit,
        "add",
        "(Ljava/lang/Object;)V",
        native_ll_listitr_add,
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// The LinkedList snapshot ListIterator's slots
// ---------------------------------------------------------------------------
//
// 0/1/2 are load-bearing NAMES, not just a layout: the interface-level
// `java/util/ListIterator` natives dispatch on "slot 0 holds an `Object[]`,
// slot 1 an `Int` cursor", so those two indices are shared with every other
// snapshot iterator in this file and must not move.
//
// Slot 3 (`lastRet`) was added 2026-08-15 with `add`/`remove`. Before it, `set`
// wrote at `cursor - 1` — correct after `next()`, WRONG after `previous()`
// (the JDK's `set` targets the element the last `next`/`previous` returned,
// which after `previous()` is at `cursor`), and there was no way at all to
// answer `IllegalStateException` for a `set`/`remove` with no preceding call.
// It is written EXPLICITLY to -1 at construction rather than left to whatever
// `alloc_object` zeroes an undeclared slot to: an int-zero read as a
// meaningful `lastRet` makes `remove()` before any `next()` delete element 0
// instead of raising. Same hazard as `ArrayList$SubList$1`'s `lastRet`, which
// is where this rule comes from.
pub(crate) const LLI_FIELD_ARR: usize = 0;
pub(crate) const LLI_FIELD_CURSOR: usize = 1;
pub(crate) const LLI_FIELD_LIST: usize = 2;
pub(crate) const LLI_FIELD_LASTRET: usize = 3;
pub(crate) const LLI_NUM_FIELDS: usize = 4;

/// `lastRet`, or `None` for a 3-slot iterator minted before that slot existed.
/// The `None` arm keeps the old `cursor - 1` reading rather than inventing a
/// value — a defensive read, since both mint sites now allocate four.
pub(crate) fn lli_last_ret(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i32> {
    if ctx.object_num_fields(this) <= LLI_FIELD_LASTRET {
        return None;
    }
    match ctx.get_field(this, LLI_FIELD_LASTRET) {
        Value::Int(v) => Some(v),
        _ => Some(-1),
    }
}

pub(crate) fn lli_set_last_ret(ctx: &mut dyn NativeContext, this: ObjectRef, v: i32) {
    if ctx.object_num_fields(this) > LLI_FIELD_LASTRET {
        ctx.set_field(this, LLI_FIELD_LASTRET, Value::Int(v));
    }
}

/// The index `set`/`remove` act on: the real `lastRet` when the carrier has
/// one, else the pre-2026-08-15 `cursor - 1`.
pub(crate) fn lli_target_index(ctx: &dyn NativeContext, this: ObjectRef, cursor: i32) -> i32 {
    lli_last_ret(ctx, this).unwrap_or(cursor - 1)
}

/// Re-take the snapshot after a structural change and re-publish it.
///
/// `add`/`remove` mutate the backing `LinkedList`, so the array slot 0 holds is
/// stale the instant they return; every later `next()` would walk the old
/// elements. Re-snapshotting is O(n) per mutation and is what keeps this
/// iterator's reads agreeing with the list it just edited.
pub(crate) fn lli_resnapshot(ctx: &mut dyn NativeContext, this: &mut ObjectRef, list: ObjectRef) {
    let list_at_call = list;
    let arr = rooted_across(ctx, &mut [this], |ctx| ll_snapshot_array(ctx, list_at_call));
    ctx.set_field(*this, LLI_FIELD_ARR, Value::Object(Some(arr)));
}

// ===========================================================================
// `java.util.LinkedList$ListItr` — the real carrier, live over the node chain
// ===========================================================================
//
// WHY THE JDK'S OWN CLASS. `LinkedList.iterator()` is
// `AbstractSequentialList.iterator()`, which IS `listIterator()`, so HotSpot
// answers `java.util.LinkedList$ListItr` for both. This VM answered
// `cratonvm.internal.LinkedListSnapshotListItr` for one and
// `java.util.LinkedList$Itr` for the other — and `LinkedList$Itr` is a class
// the real JDK **does not declare at all** (`javap -p java.util.LinkedList$Itr`
// → class not found). Two carriers, one of them fabricated, for a method the
// JDK implements once.
//
// WHY LIVE AND NOT A SNAPSHOT. The snapshot carrier could not raise
// `ConcurrentModificationException` (it has nothing to compare), and its
// `remove()` had to re-take the whole snapshot to stay consistent — O(n) per
// removal, so a `removeIf`/`Iterator.remove` drain over a `LinkedList` was
// quadratic on the one collection whose entire selling point is O(1) removal.
// A node cursor is the JDK's own `ListItr` state and makes every mutator O(1).
//
// WHY THIS IS SAFE NOW AND WAS NOT BEFORE. The comment this replaces recorded a
// real failure: binding a 3-slot overlay to the name `java/util/LinkedList$ListItr`
// put our `Int` cursor into the real `next:Node` slot, `next()` never advanced,
// and `AbstractList.equals` compared element 0 forever — kafka `ConfigDef`
// `testGroupInference`'s `assertEquals(ArrayList, LinkedList)` answered false
// for ANY LinkedList. The fix is not to avoid the name, it is to stop
// overlapping the declared fields: allocate `class_num_total_fields + N` slots
// and put this VM's state ENTIRELY PAST them, which is what
// `ArrayList$SubList$1` already does one collection over. `lli_base_checked` is
// the matching receiver-ownership test, for the same reason `sli_base_checked`
// is: these natives are also handed every `LinkedList$ListItr` **java.base's
// own bytecode** built, and answering for one of those is the silent wrong
// answer that withdrew the first attempt at this carrier.
//
// An image with no real `java.util.LinkedList$ListItr` to resolve keeps the
// snapshot iterator it has always had — `alloc_lli_view` answers `None` and
// both entry points fall back. Same trade as the sublist carrier.

/// The JDK's own `ListIterator` inside `LinkedList`, and the class
/// `linkedList.iterator().getClass()` answers on HotSpot.
pub(crate) const LLI_CLASS: &str = "java/util/LinkedList$ListItr";

/// The `LinkedList` being walked.
pub(crate) const LLI_SLOT_LIST: usize = 0;
/// The node `next()` will return, or null at the end. The JDK's `next`.
pub(crate) const LLI_SLOT_NEXT: usize = 1;
/// The node the last `next()`/`previous()` returned, or null. The JDK's
/// `lastReturned`, and the flag `set`/`remove` raise `IllegalStateException` on.
pub(crate) const LLI_SLOT_LAST: usize = 2;
/// `nextIndex()`.
pub(crate) const LLI_SLOT_INDEX: usize = 3;
/// The list size this iterator was created against — the poor relation of the
/// JDK's `expectedModCount`. See `lli_check_comodification`.
pub(crate) const LLI_SLOT_EXPECTED_SIZE: usize = 4;
pub(crate) const LLI_SLOTS: usize = 5;

/// The fewest fields a REAL `java.util.LinkedList$ListItr` declares —
/// `lastReturned`, `next`, `nextIndex`, `expectedModCount`. (The real class also
/// carries `this$0`; four is the conservative floor, since the point is only to
/// separate a real class from a fabricated one, which declares zero.)
pub(crate) const LLI_REAL_MIN_FIELDS: usize = 4;

/// Where this iterator's state starts: past the carrier's own declared fields.
#[inline]
pub(crate) fn lli_base(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    ctx.object_num_fields(this).saturating_sub(LLI_SLOTS)
}

/// [`lli_base`], answering `None` for an iterator this VM did not mint.
///
/// A real `LinkedList$ListItr` that java.base built is exactly
/// `class_num_total_fields` wide (`lastReturned`, `next`, `nextIndex`,
/// `expectedModCount`, `this$0`) and carries none of this VM's five. Without
/// this test every native below would read `expectedModCount` as the list.
#[inline]
pub(crate) fn lli_base_checked(ctx: &dyn NativeContext, this: ObjectRef) -> Option<usize> {
    let width = ctx.object_num_fields(this);
    let declared = ctx.class_num_total_fields(ctx.class_id_of_object(this));
    // `declared` is also floored: on an image where the carrier is a fabricated
    // 0-field stand-in, `declared + LLI_SLOTS` equals the width of anything this
    // VM minted AND of anything else five slots wide, so the test would stop
    // discriminating. `alloc_lli_view` refuses to mint on such an image, and
    // this is the matching refusal on the read side.
    if declared < LLI_REAL_MIN_FIELDS || width < declared + LLI_SLOTS {
        return None;
    }
    Some(width - LLI_SLOTS)
}

/// Whether `this` is a live LinkedList list-iterator this VM minted.
pub(crate) fn is_live_ll_list_itr(ctx: &dyn NativeContext, this: ObjectRef, class_name: &str) -> bool {
    class_name == LLI_CLASS && lli_base_checked(ctx, this).is_some()
}

/// Run the receiver's OWN bytecode when it is a `LinkedList$ListItr` this VM
/// did not mint. `None` means the receiver is ours and the caller proceeds.
pub(crate) fn lli_delegate_foreign(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> Option<MethodCallResult> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    if lli_base_checked(&*ctx, this).is_some() {
        return None;
    }
    Some(ctx.invoke_virtual_bytecode_only(this, method, descriptor, &args[1..]))
}

pub(crate) fn lli_obj(ctx: &dyn NativeContext, this: ObjectRef, slot: usize) -> Option<ObjectRef> {
    let base = lli_base_checked(ctx, this)?;
    match ctx.get_field(this, base + slot) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

pub(crate) fn lli_int(ctx: &dyn NativeContext, this: ObjectRef, slot: usize) -> i32 {
    match lli_base_checked(ctx, this) {
        Some(base) => match ctx.get_field(this, base + slot) {
            Value::Int(v) => v,
            _ => 0,
        },
        None => 0,
    }
}

pub(crate) fn lli_put(ctx: &mut dyn NativeContext, this: ObjectRef, slot: usize, v: Value) {
    if let Some(base) = lli_base_checked(&*ctx, this) {
        ctx.set_field(this, base + slot, v);
    }
}

/// The JDK's `checkForComodification`, over SIZE rather than a modCount.
///
/// `LinkedList`'s overlay has no `modCount` and adding one means bumping it in
/// every mutator in this file — a much wider change than this carrier, and one
/// that fails open (a missed bump is a missed exception). Size is the
/// conservative half of the same test: it catches every structural change that
/// alters the length, which is `add`, `remove`, `clear` and `addAll`, and
/// misses only a balanced add+remove between two calls. It can never fire
/// SPURIOUSLY, which is the direction that matters — a false
/// `ConcurrentModificationException` on a correct program is far worse than a
/// missed one on an incorrect program.
pub(crate) fn lli_check_comodification(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    list: ObjectRef,
) -> Result<(), MethodCallFailed> {
    if ll_size(ctx, list) != lli_int(ctx, this, LLI_SLOT_EXPECTED_SIZE) {
        return Err(RuntimeError::ConcurrentModificationException.into());
    }
    Ok(())
}

/// Allocate a live `LinkedList$ListItr` over `list`, positioned at `index`.
///
/// `None` when this image has no real `java.util.LinkedList$ListItr` — the
/// caller then keeps the iterator it has always returned.
///
/// TWO tests, and the second one is the load-bearing half.
/// `ensure_class_initialized` can report `Ok` having FABRICATED a stand-in, and
/// a fabricated stand-in carries exactly the name that was asked for — so the
/// name check alone admits it. The real `java.util.LinkedList$ListItr` declares
/// `lastReturned`, `next`, `nextIndex`, `expectedModCount` and `this$0`; a
/// fabricated one declares nothing. Anchoring on `class_num_total_fields` is
/// what tells them apart, and getting it wrong is not a cosmetic mistake:
/// minting a fabricated `java/util/*` carrier is what §5 of the native contract
/// forbids, and it made `vm::tests::linked_list_iterator` fail on the
/// synthetic-JDK build — that test asks `LinkedList.iterator()` for an iterator
/// and then calls `java/util/LinkedList$Itr.hasNext` on it BY NAME, which is
/// the correct expectation for an image that has no real carrier to wear.
///
/// `list` is the caller's receiver, rewritten in place to its current address
/// on EVERY path, `None` included (gc-common w20-a): the class lookup below can
/// run `<clinit>` on first use, and a caller that fell back to its snapshot
/// iterator after a `None` used to go on with the receiver's pre-collection
/// address. The pin is taken before that lookup for the same reason — taken
/// after it, this function's own allocation used a stale `list` too.
pub(crate) fn alloc_lli_view(
    ctx: &mut dyn NativeContext,
    list_io: &mut ObjectRef,
    index: i32,
) -> Option<ObjectRef> {
    // GC-SAFETY: `ensure_class_initialized` and `alloc_object` both collect and
    // `list` is a bare Rust local the collector cannot see. Root it across both
    // and read it back through its pin before every use.
    let list_pin = ctx.pin_native_root(*list_io);
    let layout = lli_view_layout(ctx);
    *list_io = ctx.read_native_pin(list_pin, *list_io);
    let Some((cid, n)) = layout else {
        ctx.unpin_native_roots(list_pin);
        return None;
    };
    let list = *list_io;
    let size = ll_size(&*ctx, list);
    let itr = ctx.alloc_object(cid, n);
    let itr_pin = ctx.pin_native_root(itr);
    let itr = ctx.read_native_pin(itr_pin, itr);
    let list = ctx.read_native_pin(list_pin, list);
    let base = n - LLI_SLOTS;
    ctx.set_field(itr, base + LLI_SLOT_LIST, Value::Object(Some(list)));
    // The node walk is a plain pointer chase over already-live objects, but it
    // is done AFTER the allocation and through the pinned list.
    let start = if index <= 0 {
        match ll_get(&*ctx, list, "head") {
            Value::Object(Some(n)) => Some(n),
            _ => None,
        }
    } else if index >= size {
        None
    } else {
        ll_node_at(&*ctx, list, index)
    };
    let itr = ctx.read_native_pin(itr_pin, itr);
    ctx.set_field(itr, base + LLI_SLOT_NEXT, Value::Object(start));
    // Written EXPLICITLY, never left to whatever `alloc_object` zero-initialises
    // an undeclared slot to: a non-null `lastReturned` read out of an unwritten
    // slot makes `remove()` before any `next()` unlink a real node instead of
    // raising, and an int-zero `nextIndex` on a `listIterator(k)` silently
    // reports the wrong position. `ArrayList$SubList$1`'s `lastRet` is the same
    // hazard, one collection over.
    ctx.set_field(itr, base + LLI_SLOT_LAST, Value::Object(None));
    ctx.set_field(itr, base + LLI_SLOT_INDEX, Value::Int(index.clamp(0, size)));
    ctx.set_field(itr, base + LLI_SLOT_EXPECTED_SIZE, Value::Int(size));
    *list_io = list;
    ctx.unpin_native_roots(list_pin);
    Some(itr)
}

/// The real `java.util.LinkedList$ListItr`'s class id and the width
/// [`alloc_lli_view`] allocates it at, initialising the class; `None` when
/// this image has no real carrier (see the two tests on `alloc_lli_view`).
pub(crate) fn lli_view_layout(ctx: &mut dyn NativeContext) -> Option<(ClassId, usize)> {
    let cid = ctx.ensure_class_initialized(LLI_CLASS).ok()?;
    if ctx.class_name_arc_of_id(cid).as_deref() != Some(LLI_CLASS) {
        return None;
    }
    let declared = ctx.class_num_total_fields(cid);
    if declared < LLI_REAL_MIN_FIELDS {
        return None;
    }
    Some((cid, declared + LLI_SLOTS))
}

/// `(list, next, lastReturned, nextIndex)` for a receiver this VM minted.
pub(crate) fn lli_state(
    ctx: &dyn NativeContext,
    args: &[Value],
) -> Option<(
    ObjectRef,
    ObjectRef,
    Option<ObjectRef>,
    Option<ObjectRef>,
    i32,
)> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    let list = lli_obj(ctx, this, LLI_SLOT_LIST)?;
    Some((
        this,
        list,
        lli_obj(ctx, this, LLI_SLOT_NEXT),
        lli_obj(ctx, this, LLI_SLOT_LAST),
        lli_int(ctx, this, LLI_SLOT_INDEX),
    ))
}

pub(crate) fn native_lli_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = lli_delegate_foreign(ctx, args, "hasNext", "()Z") {
        return r;
    }
    let (this, list, _next, _last, index) = match lli_state(&*ctx, args) {
        Some(s) => s,
        None => return Ok(Some(Value::Int(0))),
    };
    let _ = this;
    Ok(Some(Value::Int((index < ll_size(&*ctx, list)) as i32)))
}

pub(crate) fn native_lli_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = lli_delegate_foreign(ctx, args, "next", "()Ljava/lang/Object;") {
        return r;
    }
    let (this, list, next, _last, index) = match lli_state(&*ctx, args) {
        Some(s) => s,
        None => return Err(bare_no_such_element(ctx)),
    };
    lli_check_comodification(&*ctx, this, list)?;
    let node = match next {
        Some(n) if index < ll_size(&*ctx, list) => n,
        _ => return Err(bare_no_such_element(ctx)),
    };
    let elem = ctx.get_field(node, LL_NODE_ELEM);
    let following = match ctx.get_field(node, LL_NODE_NEXT) {
        Value::Object(Some(n)) => Some(n),
        _ => None,
    };
    lli_put(ctx, this, LLI_SLOT_LAST, Value::Object(Some(node)));
    lli_put(ctx, this, LLI_SLOT_NEXT, Value::Object(following));
    lli_put(ctx, this, LLI_SLOT_INDEX, Value::Int(index + 1));
    Ok(Some(elem))
}

pub(crate) fn native_lli_has_previous(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = lli_delegate_foreign(ctx, args, "hasPrevious", "()Z") {
        return r;
    }
    let (_this, _list, _next, _last, index) = match lli_state(&*ctx, args) {
        Some(s) => s,
        None => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int((index > 0) as i32)))
}

pub(crate) fn native_lli_previous(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = lli_delegate_foreign(ctx, args, "previous", "()Ljava/lang/Object;") {
        return r;
    }
    let (this, list, next, _last, index) = match lli_state(&*ctx, args) {
        Some(s) => s,
        None => return Err(bare_no_such_element(ctx)),
    };
    lli_check_comodification(&*ctx, this, list)?;
    if index <= 0 {
        return Err(bare_no_such_element(ctx));
    }
    // The JDK's `next = (next == null) ? last : next.prev` — a cursor parked
    // past the end steps back onto the tail.
    let node = match next {
        Some(n) => match ctx.get_field(n, LL_NODE_PREV) {
            Value::Object(Some(p)) => p,
            _ => return Err(bare_no_such_element(ctx)),
        },
        None => match ll_get(&*ctx, list, "tail") {
            Value::Object(Some(t)) => t,
            _ => return Err(bare_no_such_element(ctx)),
        },
    };
    let elem = ctx.get_field(node, LL_NODE_ELEM);
    lli_put(ctx, this, LLI_SLOT_NEXT, Value::Object(Some(node)));
    lli_put(ctx, this, LLI_SLOT_LAST, Value::Object(Some(node)));
    lli_put(ctx, this, LLI_SLOT_INDEX, Value::Int(index - 1));
    Ok(Some(elem))
}

pub(crate) fn native_lli_next_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = lli_delegate_foreign(ctx, args, "nextIndex", "()I") {
        return r;
    }
    Ok(Some(Value::Int(
        lli_state(&*ctx, args).map(|s| s.4).unwrap_or(0),
    )))
}

pub(crate) fn native_lli_previous_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = lli_delegate_foreign(ctx, args, "previousIndex", "()I") {
        return r;
    }
    Ok(Some(Value::Int(
        lli_state(&*ctx, args).map(|s| s.4).unwrap_or(0) - 1,
    )))
}

pub(crate) fn native_lli_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = lli_delegate_foreign(ctx, args, "set", "(Ljava/lang/Object;)V") {
        return r;
    }
    let (this, list, _next, last, _index) = match lli_state(&*ctx, args) {
        Some(s) => s,
        None => return Ok(None),
    };
    let node = match last {
        Some(n) => n,
        None => return Err(sli_illegal_state(ctx)),
    };
    lli_check_comodification(&*ctx, this, list)?;
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    ctx.set_field(node, LL_NODE_ELEM, elem);
    Ok(None)
}

pub(crate) fn native_lli_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = lli_delegate_foreign(ctx, args, "remove", "()V") {
        return r;
    }
    let (this, list, next, last, index) = match lli_state(&*ctx, args) {
        Some(s) => s,
        None => return Ok(None),
    };
    let node = match last {
        Some(n) => n,
        None => return Err(sli_illegal_state(ctx)),
    };
    lli_check_comodification(&*ctx, this, list)?;
    // The JDK's bookkeeping, verbatim: if the cursor was sitting ON the node
    // being removed (a `previous()` then `remove()`), it steps forward onto its
    // successor and the index does NOT move; otherwise the cursor is past it
    // and the index drops by one.
    let following = match ctx.get_field(node, LL_NODE_NEXT) {
        Value::Object(Some(n)) => Some(n),
        _ => None,
    };
    ll_unlink_node(ctx, list, node);
    if next == Some(node) {
        lli_put(ctx, this, LLI_SLOT_NEXT, Value::Object(following));
    } else {
        lli_put(ctx, this, LLI_SLOT_INDEX, Value::Int(index - 1));
    }
    lli_put(ctx, this, LLI_SLOT_LAST, Value::Object(None));
    let expected = lli_int(&*ctx, this, LLI_SLOT_EXPECTED_SIZE);
    lli_put(ctx, this, LLI_SLOT_EXPECTED_SIZE, Value::Int(expected - 1));
    Ok(None)
}

pub(crate) fn native_lli_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(r) = lli_delegate_foreign(ctx, args, "add", "(Ljava/lang/Object;)V") {
        return r;
    }
    let (this, list, next, _last, index) = match lli_state(&*ctx, args) {
        Some(s) => s,
        None => return Ok(None),
    };
    lli_check_comodification(&*ctx, this, list)?;
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    match next {
        Some(succ) => ll_link_before(ctx, list, elem, succ)?,
        None => ll_link_last(ctx, list, elem)?,
    }
    // `add` clears `lastReturned`, so an immediately following `set`/`remove`
    // raises — the JDK's rule, and the reason `listIterator.add then set` is a
    // probe row.
    lli_put(ctx, this, LLI_SLOT_LAST, Value::Object(None));
    lli_put(ctx, this, LLI_SLOT_INDEX, Value::Int(index + 1));
    let expected = lli_int(&*ctx, this, LLI_SLOT_EXPECTED_SIZE);
    lli_put(ctx, this, LLI_SLOT_EXPECTED_SIZE, Value::Int(expected + 1));
    Ok(None)
}

pub(crate) fn register_ll_list_itr_natives(registry: &mut NativeMethodRegistry) {
    let prev = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = LLI_CLASS;
    registry.register(c, "hasNext", "()Z", native_lli_has_next);
    registry.register(c, "next", "()Ljava/lang/Object;", native_lli_next);
    registry.register(c, "hasPrevious", "()Z", native_lli_has_previous);
    registry.register(c, "previous", "()Ljava/lang/Object;", native_lli_previous);
    registry.register(c, "nextIndex", "()I", native_lli_next_index);
    registry.register(c, "previousIndex", "()I", native_lli_previous_index);
    registry.register(c, "set", "(Ljava/lang/Object;)V", native_lli_set);
    registry.register(c, "remove", "()V", native_lli_remove);
    registry.register(c, "add", "(Ljava/lang/Object;)V", native_lli_add);
    registry.set_category(prev);
}

pub(crate) fn ll_snapshot_array(ctx: &mut dyn NativeContext, this: ObjectRef) -> ObjectRef {
    let size = ll_size(ctx, this) as usize;
    // GC-safety: the allocation can complete a moving young GC and the node
    // walk below reads `head` out of `this`. See `rooted_across`.
    let (this, arr) = rooted_across1(ctx, this, |ctx| alloc_ref_array(ctx, size));
    let mut cur = match ll_get(ctx, this, "head") {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let mut i = 0;
    while let Some(node) = cur {
        if i >= size {
            break;
        }
        let elem = ctx.get_field(node, LL_NODE_ELEM);
        ctx.set_array_element(arr, i, elem);
        cur = match ctx.get_field(node, LL_NODE_NEXT) {
            Value::Object(Some(n)) => Some(n),
            _ => None,
        };
        i += 1;
    }
    arr
}

pub(crate) fn native_ll_list_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // The real carrier when this image has one; the snapshot below when it does
    // not (a synthetic-JDK build, where no `java.util.LinkedList$ListItr` exists
    // to wear). `alloc_lli_view` refreshes `this` on the declining path too.
    if let Some(live) = alloc_lli_view(ctx, &mut this, 0) {
        return Ok(Some(Value::Object(Some(live))));
    }
    // GC-safety: the snapshot and the iterator shell each allocate; `this`
    // and the snapshot are both stored afterwards. See `rooted_across`.
    let this_at_call = this;
    let mut arr = rooted_across(ctx, &mut [&mut this], |ctx| {
        ll_snapshot_array(ctx, this_at_call)
    });
    // Minted through the VM-internal door, not the compatibility one. This
    // carrier is VM-internal state, not a stand-in: no image declares a
    // `cratonvm/…` name, nothing is being stood in for, and the three slots are
    // the snapshot, the cursor and the backing list. Contract §1 item 6 permits
    // that in both modes — the door
    // docs/known-issues/jdk-only/W7-13-strict-mh-insert-wrapper.md established
    // for the ten `MethodHandles` combinator carriers, on the same test.
    //
    // Load-bearing pairing: inert without the companion move of this class from
    // `VM_MINTED_STAND_IN_RECEIVERS` to `VM_SERVICE_RECEIVERS` in
    // native-api/src/no_image_receiver.rs, and that move is inert without this.
    // Without the move, `register()` re-tags the nine natives below
    // `SyntheticStub` and `--jdk-only` drops them, so this mint hands strict a
    // well-formed object with no methods and the failure becomes an
    // `UnsatisfiedLinkError` at the first `hasNext()`. Land both or neither.
    //
    // `ensure_vm_internal_class` is infallible, so the `?` this call used to
    // carry is gone. The allocation stays INSIDE `rooted_across`: it can
    // collect, and `arr` and `this` are stored into the result immediately
    // after, so hoisting it out would leave both unrooted across a moving GC.
    let it: ObjectRef = rooted_across(ctx, &mut [&mut this, &mut arr], |ctx| {
        let cid = ctx.ensure_vm_internal_class(
            "cratonvm/internal/LinkedListSnapshotListItr",
            LLI_NUM_FIELDS,
        );
        ctx.alloc_object(cid, LLI_NUM_FIELDS)
    });
    ctx.set_field(it, LLI_FIELD_ARR, Value::Object(Some(arr)));
    ctx.set_field(it, LLI_FIELD_CURSOR, Value::Int(0));
    ctx.set_field(it, LLI_FIELD_LIST, Value::Object(Some(this)));
    lli_set_last_ret(ctx, it, -1);
    Ok(Some(Value::Object(Some(it))))
}

pub(crate) fn native_ll_list_iterator_idx(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    // `listIterator(index)` bounds-checks BEFORE constructing, and the JDK's
    // message is the one `ll_out_of_bounds` builds.
    let size = ll_size(&*ctx, this);
    if idx < 0 || idx > size {
        return Err(ll_out_of_bounds(idx, size));
    }
    let mut this = this;
    // Refreshes `this` on the declining path too (gc-common w20-a).
    if let Some(live) = alloc_lli_view(ctx, &mut this, idx) {
        return Ok(Some(Value::Object(Some(live))));
    }
    // GC-safety: the snapshot and the iterator shell each allocate; `this`
    // and the snapshot are both stored afterwards. See `rooted_across`.
    let this_at_call = this;
    let mut arr = rooted_across(ctx, &mut [&mut this], |ctx| {
        ll_snapshot_array(ctx, this_at_call)
    });
    // Same mint, same pairing, as `native_ll_list_iterator` above.
    let it: ObjectRef = rooted_across(ctx, &mut [&mut this, &mut arr], |ctx| {
        let cid = ctx.ensure_vm_internal_class(
            "cratonvm/internal/LinkedListSnapshotListItr",
            LLI_NUM_FIELDS,
        );
        ctx.alloc_object(cid, LLI_NUM_FIELDS)
    });
    ctx.set_field(it, LLI_FIELD_ARR, Value::Object(Some(arr)));
    ctx.set_field(it, LLI_FIELD_CURSOR, Value::Int(idx.max(0)));
    ctx.set_field(it, LLI_FIELD_LIST, Value::Object(Some(this)));
    lli_set_last_ret(ctx, it, -1);
    Ok(Some(Value::Object(Some(it))))
}

pub(crate) fn native_ll_listitr_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(r)) => r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    let len = ctx.array_length(arr) as i32;
    Ok(Some(Value::Int(if cursor < len { 1 } else { 0 })))
}

pub(crate) fn native_ll_listitr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "No more elements".to_string(),
                }
                .into(),
            )
        }
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(r)) => r,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "No more elements".to_string(),
                }
                .into(),
            )
        }
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    let len = ctx.array_length(arr) as i32;
    if cursor >= len {
        return Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "No more elements".to_string(),
            }
            .into(),
        );
    }
    let elem = ctx.get_array_element(arr, cursor as usize);
    ctx.set_field(this, 1, Value::Int(cursor + 1));
    lli_set_last_ret(ctx, this, cursor);
    Ok(Some(elem))
}

pub(crate) fn native_ll_listitr_has_previous(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(if cursor > 0 { 1 } else { 0 })))
}

pub(crate) fn native_ll_listitr_previous(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "No previous element".to_string(),
                }
                .into(),
            )
        }
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(r)) => r,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "No previous element".to_string(),
                }
                .into(),
            )
        }
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    if cursor <= 0 {
        return Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "No previous element".to_string(),
            }
            .into(),
        );
    }
    let elem = ctx.get_array_element(arr, (cursor - 1) as usize);
    ctx.set_field(this, 1, Value::Int(cursor - 1));
    // The JDK's `previous()` leaves `lastReturned` on the element it returned,
    // which is now AT the cursor — not behind it. `set` after `previous` is the
    // row that tells the two apart.
    lli_set_last_ret(ctx, this, cursor - 1);
    Ok(Some(elem))
}

pub(crate) fn native_ll_listitr_next_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(cursor)))
}

pub(crate) fn native_ll_listitr_previous_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(cursor - 1)))
}

pub(crate) fn native_ll_listitr_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Set the element the last `next()`/`previous()` returned, in both the
    // snapshot array AND the backing LinkedList node, so `List.sort` actually
    // sorts.
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let cursor = match ctx.get_field(this, LLI_FIELD_CURSOR) {
        Value::Int(v) => v,
        _ => 0,
    };
    let idx = lli_target_index(&*ctx, this, cursor);
    if idx < 0 {
        // Was `Ok(None)` — a silent no-op where HotSpot raises. A `set()` with
        // no preceding `next()` is a caller bug, and swallowing it is the
        // "reported success and changed nothing" shape the sublist iterators
        // were fixed out of.
        return Err(sli_illegal_state(ctx));
    }
    if let Value::Object(Some(arr)) = ctx.get_field(this, LLI_FIELD_ARR) {
        ctx.set_array_element(arr, idx as usize, elem);
    }
    if let Value::Object(Some(list)) = ctx.get_field(this, LLI_FIELD_LIST) {
        if let Some(node) = ll_node_at(ctx, list, idx) {
            ctx.set_field(node, LL_NODE_ELEM, elem);
        }
    }
    Ok(None)
}

/// LinkedList's `ListIterator` is backed by an immutable snapshot taken at
/// `listIterator()` time, so structural mutation through the iterator cannot
/// be honoured. Per the JDK contract, an iterator that does not support a
/// mutation must throw `UnsupportedOperationException` rather than silently
/// no-op'ing (which would mask caller bugs and diverge from real JDK
/// behaviour). Used for both `remove()` and `add(Object)`.
/// `ListItr.remove()` — the JDK's body against this VM's snapshot state.
///
/// Was a shared `Err(unsupported_op())` stub with `add`. `LinkedList` is a
/// `List`, `listIterator()` is specified to return a mutating iterator, and
/// `AbstractList`'s own `removeAll`/`retainAll`/`Collection.removeIf` defaults
/// all reach it — so the stub did not merely lose a feature, it made every
/// default method that drains a `LinkedList` through its list-iterator throw.
pub(crate) fn native_ll_listitr_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Err(sli_illegal_state(ctx)),
    };
    let cursor = match ctx.get_field(this, LLI_FIELD_CURSOR) {
        Value::Int(v) => v,
        _ => 0,
    };
    let idx = lli_target_index(&*ctx, this, cursor);
    if idx < 0 {
        return Err(sli_illegal_state(ctx));
    }
    let list = match ctx.get_field(this, LLI_FIELD_LIST) {
        Value::Object(Some(l)) => l,
        _ => return Err(sli_illegal_state(ctx)),
    };
    if let Some(node) = ll_node_at(&*ctx, list, idx) {
        ll_unlink_node(ctx, list, node);
    }
    lli_resnapshot(ctx, &mut this, list);
    // The JDK's `cursor` bookkeeping: removing an element the iterator has
    // already passed shifts everything after it down by one.
    if idx < cursor {
        ctx.set_field(this, LLI_FIELD_CURSOR, Value::Int(cursor - 1));
    }
    lli_set_last_ret(ctx, this, -1);
    Ok(None)
}

/// `ListItr.add(e)` — insert before the implicit cursor, per the JDK: the new
/// element goes where `next()` would have read, `nextIndex()` advances past it,
/// and `lastRet` is cleared so an immediately following `set`/`remove` raises.
pub(crate) fn native_ll_listitr_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let cursor = match ctx.get_field(this, LLI_FIELD_CURSOR) {
        Value::Int(v) => v,
        _ => 0,
    };
    let list = match ctx.get_field(this, LLI_FIELD_LIST) {
        Value::Object(Some(l)) => l,
        _ => return Ok(None),
    };
    native_ll_add_at(
        ctx,
        &[Value::Object(Some(list)), Value::Int(cursor.max(0)), elem],
    )?;
    lli_resnapshot(ctx, &mut this, list);
    ctx.set_field(this, LLI_FIELD_CURSOR, Value::Int(cursor.max(0) + 1));
    lli_set_last_ret(ctx, this, -1);
    Ok(None)
}

pub(crate) fn native_ll_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // Seed the identity-hash header so the overlay key (which is
    // `ctx.identity_hash_code(this)`) is stable from the very first
    // write — protects against a GC firing between `<init>` and the
    // first overlay lookup.
    ih_seed(ctx, this);
    ll_set(ctx, this, "head", Value::Object(None));
    ll_set(ctx, this, "tail", Value::Object(None));
    ll_set(ctx, this, "size", Value::Int(0));
    Ok(None)
}

/// LinkedList.<init>(Collection) — seed the native overlay, then copy the
/// source's elements into it. Without this native, `new LinkedList<>(c)` ran
/// the real JDK bytecode (`this(); addAll(c)`) which links nodes into the real
/// `first`/`last`/`size` fields, while our overridden `size()`/`iterator()`
/// read the native overlay → the copy looked empty. This broke, among others,
/// Mockito's `DefaultRegisteredInvocations.getAll()` (`new LinkedList<>(
/// invocations)`), so `verify(...)`/`getInvocations()` saw zero interactions.
pub(crate) fn native_ll_init_from_collection(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `LinkedList(Collection)` is `this(); addAll(c);`, whose `addAll(size,
    // c)` dereferences `c.toArray()` (C4W4-6, row 4).
    reject_null_collection_asked(&*ctx, args.get(1), NULL_ARG_LL_INIT, NULL_C_TO_ARRAY)?;
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // Seed an empty overlay first (identity-hash header + head/tail/size).
    ih_seed(ctx, this);
    ll_set(ctx, this, "head", Value::Object(None));
    ll_set(ctx, this, "tail", Value::Object(None));
    ll_set(ctx, this, "size", Value::Int(0));

    let source = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None), // null/empty collection — overlay stays empty
    };
    // `this(); addAll(c);`: a program subclass's `addAll` runs during
    // `super(c)` ([`copy_ctor_bulk_override`]). `this` / `source` are the
    // same bare references the fill below starts from.
    if copy_ctor_bulk_override(&*ctx, this, "addAll", "(Ljava/util/Collection;)Z") {
        return ctx
            .invoke_virtual(
                this,
                "addAll",
                "(Ljava/util/Collection;)Z",
                &[Value::Object(Some(source))],
            )
            .map(|_| None);
    }
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — the
    // source drain re-enters Java and every `ll_link_last` allocates a node;
    // re-read `this` + each element per iteration (mirrors native_ll_add_all).
    let this_pin = ctx.pin_native_root(this);
    // `CRATONVM_COMPAT_BULK_ADD_JDK_QUESTIONS` (round 15 wave 4, lane
    // compat4): `addAll(c)`'s one `c.toArray()` for a program's `c`; every `?`
    // below releases this frame's pins first.
    let collected = if bulk_add_jdk_questions_enabled() && copy_source_is_foreign(&*ctx, source) {
        foreign_to_array_elements(ctx, source)
    } else {
        collect_collection_elements_or_real(ctx, source)
    };
    let elems = unpin_on_err(ctx, this_pin, collected)?;
    let (_, handles) = pin_value_slice(ctx, &elems);
    for (i, val) in elems.iter().enumerate() {
        let this = ctx.read_native_pin(this_pin, this);
        let val = read_pinned_elem(ctx, handles[i], *val);
        let linked = ll_link_last(ctx, this, val);
        unpin_on_err(ctx, this_pin, linked)?;
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

/// Snapshot the LinkedList overlay into a `Vec<Value>` (head → tail order).
pub(crate) fn ll_overlay_elements(ctx: &mut dyn NativeContext, this: ObjectRef) -> Vec<Value> {
    let arr = ll_snapshot_array(ctx, this);
    let len = ctx.array_length(arr);
    (0..len).map(|i| ctx.get_array_element(arr, i)).collect()
}

/// LinkedList.stream() — build a stream over the native overlay. Without this,
/// `Collection.stream()`'s default routes through the real `LinkedList`'s own
/// `spliterator()`, which reads the JDK `first`/`size` fields our overlay never
/// populates → an empty stream for a non-empty list. (This was the last link in
/// the Mockito chain: `DefaultRegisteredInvocations.getAll()` does
/// `copy.stream().filter(...).collect(toList())`, so every recorded invocation
/// vanished and `verify(...)` reported "zero interactions".)
pub(crate) fn native_ll_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let elements = ll_overlay_elements(ctx, this);
    make_stream(ctx, &elements)
}

/// LinkedList.spliterator() — a synthetic array-backed Spliterator over the
/// overlay (same shape as `native_stream_spliterator`), so direct
/// `spliterator()` / `StreamSupport.stream(spliterator(), …)` callers also see
/// the real elements.
pub(crate) fn native_ll_spliterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return empty_synthetic_spliterator(ctx);
        }
    };
    let elements = ll_overlay_elements(ctx, this);
    // GC-safety: the collected elements live in a bare `Vec`, which is not a
    // GC root, and both allocations below can collect. Pin the slice and
    // re-read each element at its store; root the array across the shell.
    let (elem_base, elem_pins) = pin_value_slice(ctx, &elements);
    let mut arr = alloc_ref_array(ctx, elements.len());
    for (i, v) in elements.iter().enumerate() {
        ctx.set_array_element(arr, i, read_pinned_elem(ctx, elem_pins[i], *v));
    }
    if elem_base != usize::MAX {
        ctx.unpin_native_roots(elem_base);
    }
    let spl: ObjectRef = rooted_across(ctx, &mut [&mut arr], |ctx| {
        try_alloc_synthetic(ctx, "java/util/Spliterator", 3)
    })?;
    ctx.set_field(spl, 0, Value::Object(Some(arr)));
    ctx.set_field(spl, 1, Value::Int(0));
    ctx.set_field(spl, 2, Value::Int(elements.len() as i32));
    Ok(Some(Value::Object(Some(spl))))
}

pub(crate) fn ll_link_last(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    element: Value,
) -> Result<(), MethodCallFailed> {
    // Family-1 fix (cce0079): the node alloc can move `this` — refresh it
    // before reading/writing the list's head/tail/size.
    let this_pin = ctx.pin_native_root(this);
    let node = ll_alloc_node(ctx, element)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let size = ll_size(ctx, this);
    if let Value::Object(Some(tail)) = ll_get(ctx, this, "tail") {
        ctx.set_field(tail, LL_NODE_NEXT, Value::Object(Some(node)));
        ctx.set_field(node, LL_NODE_PREV, Value::Object(Some(tail)));
        ll_set(ctx, this, "tail", Value::Object(Some(node)));
    } else {
        // Empty list
        ll_set(ctx, this, "head", Value::Object(Some(node)));
        ll_set(ctx, this, "tail", Value::Object(Some(node)));
    }
    ll_set(ctx, this, "size", Value::Int(size + 1));
    Ok(())
}

pub(crate) fn ll_link_first(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    element: Value,
) -> Result<(), MethodCallFailed> {
    // Family-1 fix (cce0079): same as `ll_link_last` — refresh `this`
    // across the node alloc.
    let this_pin = ctx.pin_native_root(this);
    let node = ll_alloc_node(ctx, element)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let size = ll_size(ctx, this);
    if let Value::Object(Some(head)) = ll_get(ctx, this, "head") {
        ctx.set_field(head, LL_NODE_PREV, Value::Object(Some(node)));
        ctx.set_field(node, LL_NODE_NEXT, Value::Object(Some(head)));
        ll_set(ctx, this, "head", Value::Object(Some(node)));
    } else {
        ll_set(ctx, this, "head", Value::Object(Some(node)));
        ll_set(ctx, this, "tail", Value::Object(Some(node)));
    }
    ll_set(ctx, this, "size", Value::Int(size + 1));
    Ok(())
}

pub(crate) fn native_ll_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let element = args.get(1).copied().unwrap_or(Value::Object(None));
    ll_link_last(ctx, this, element)?;
    Ok(Some(Value::Int(1))) // returns true
}

pub(crate) fn native_ll_add_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let element = args.get(1).copied().unwrap_or(Value::Object(None));
    ll_link_first(ctx, this, element)?;
    Ok(None)
}

pub(crate) fn native_ll_add_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let element = args.get(1).copied().unwrap_or(Value::Object(None));
    ll_link_last(ctx, this, element)?;
    Ok(None)
}

/// Insert `element` into a fresh node positioned immediately before `succ`.
/// Mirrors real-JDK `LinkedList.linkBefore`. `succ` must be a live node of
/// `this`. Updates `head`/`size` as needed; `tail` is unaffected because the
/// new node is never the last.
pub(crate) fn ll_link_before(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    element: Value,
    succ: ObjectRef,
) -> Result<(), MethodCallFailed> {
    // Family-1 fix (cce0079): the node alloc can move `this` AND `succ` —
    // refresh both before splicing the new node in.
    let this_pin = ctx.pin_native_root(this);
    let succ_pin = ctx.pin_native_root(succ);
    let node = ll_alloc_node(ctx, element)?;
    let this = ctx.read_native_pin(this_pin, this);
    let succ = ctx.read_native_pin(succ_pin, succ);
    ctx.unpin_native_roots(this_pin);
    let pred = ctx.get_field(succ, LL_NODE_PREV);
    ctx.set_field(node, LL_NODE_PREV, pred);
    ctx.set_field(node, LL_NODE_NEXT, Value::Object(Some(succ)));
    ctx.set_field(succ, LL_NODE_PREV, Value::Object(Some(node)));
    match pred {
        Value::Object(Some(pred_node)) => {
            ctx.set_field(pred_node, LL_NODE_NEXT, Value::Object(Some(node)));
        }
        _ => {
            // succ was the head — node becomes the new head.
            ll_set(ctx, this, "head", Value::Object(Some(node)));
        }
    }
    let size = ll_size(ctx, this);
    ll_set(ctx, this, "size", Value::Int(size + 1));
    Ok(())
}

/// Unlink a live node, returning its element. Mirrors `LinkedList.unlink`.
pub(crate) fn ll_unlink_node(ctx: &mut dyn NativeContext, this: ObjectRef, node: ObjectRef) -> Value {
    let element = ctx.get_field(node, LL_NODE_ELEM);
    let prev = ctx.get_field(node, LL_NODE_PREV);
    let next = ctx.get_field(node, LL_NODE_NEXT);
    match prev {
        Value::Object(Some(prev_node)) => {
            ctx.set_field(prev_node, LL_NODE_NEXT, next);
        }
        _ => {
            // node was the head.
            ll_set(ctx, this, "head", next);
        }
    }
    match next {
        Value::Object(Some(next_node)) => {
            ctx.set_field(next_node, LL_NODE_PREV, prev);
        }
        _ => {
            // node was the tail.
            ll_set(ctx, this, "tail", prev);
        }
    }
    let size = ll_size(ctx, this);
    ll_set(ctx, this, "size", Value::Int((size - 1).max(0)));
    element
}

/// `LinkedList.add(int index, E element)` — positional insert.
pub(crate) fn native_ll_add_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let element = args.get(2).copied().unwrap_or(Value::Object(None));
    let size = ll_size(ctx, this);
    if index < 0 || index > size {
        return Err(ll_out_of_bounds(index, size));
    }
    if index == size {
        ll_link_last(ctx, this, element)?;
    } else {
        match ll_node_at(ctx, this, index) {
            Some(succ) => ll_link_before(ctx, this, element, succ)?,
            None => ll_link_last(ctx, this, element)?,
        }
    }
    Ok(None)
}

/// `LinkedList.remove(int index)` — positional removal, returns the element.
pub(crate) fn native_ll_remove_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let size = ll_size(ctx, this);
    if index < 0 || index >= size {
        return Err(ll_out_of_bounds(index, size));
    }
    match ll_node_at(ctx, this, index) {
        Some(node) => Ok(Some(ll_unlink_node(ctx, this, node))),
        None => Ok(Some(Value::Object(None))),
    }
}

/// Traverse to the node at the given index
pub(crate) fn ll_node_at(ctx: &dyn NativeContext, this: ObjectRef, index: i32) -> Option<ObjectRef> {
    let size = ll_size(ctx, this);
    if index < 0 || index >= size {
        return None;
    }
    if index < size / 2 {
        // Traverse from head
        let mut cur = match ll_get(ctx, this, "head") {
            Value::Object(Some(r)) => r,
            _ => return None,
        };
        for _ in 0..index {
            cur = match ctx.get_field(cur, LL_NODE_NEXT) {
                Value::Object(Some(r)) => r,
                _ => return None,
            };
        }
        Some(cur)
    } else {
        // Traverse from tail
        let mut cur = match ll_get(ctx, this, "tail") {
            Value::Object(Some(r)) => r,
            _ => return None,
        };
        for _ in 0..(size - 1 - index) {
            cur = match ctx.get_field(cur, LL_NODE_PREV) {
                Value::Object(Some(r)) => r,
                _ => return None,
            };
        }
        Some(cur)
    }
}

/// `LinkedList`'s private `outOfBoundsMsg`, shared by every positional
/// operation: `"Index: " + index + ", Size: " + size`.
///
/// The plain `IndexOutOfBoundsException`, not the array subclass — measured on
/// Temurin 25, and the subclass is the direction that breaks a `catch`. Note
/// the COMMA: `ArrayList.add(int, E)` uses this same wording, but
/// `ImmutableCollections.List12` uses a SPACE ("Index: 3 Size: 1"). Three
/// receivers, three shapes; `probes/ListOutOfBoundsProbe` holds all three.
pub(crate) fn ll_out_of_bounds(index: i32, size: i32) -> MethodCallFailed {
    cratonvm_types::error::RuntimeError::ioobe(format!("Index: {index}, Size: {size}")).into()
}

pub(crate) fn native_ll_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(Some(Value::Object(None))),
    };
    match ll_node_at(ctx, this, index) {
        Some(node) => Ok(Some(ctx.get_field(node, LL_NODE_ELEM))),
        None => {
            let size = ll_size(ctx, this);
            Err(ll_out_of_bounds(index, size))
        }
    }
}

/// `LinkedList.set(int, E)` — set the element at `index` in the overlay,
/// returning the previous element. Without this native, the real-JDK
/// `LinkedList.set` bytecode ran against the never-populated `first`/`last`
/// fields and silently did nothing — so `Collections.reverse(linkedList)` (which
/// swaps via `set(i, set(j, get(i)))` for small non-RandomAccess lists) was a
/// no-op, e.g. Gradle `GFileUtils.mkdirs` creating cache dirs child-before-parent
/// → "… is not a directory".
pub(crate) fn native_ll_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(Some(Value::Object(None))),
    };
    let new_val = args.get(2).copied().unwrap_or(Value::Object(None));
    match ll_node_at(ctx, this, index) {
        Some(node) => {
            let old = ctx.get_field(node, LL_NODE_ELEM);
            ctx.set_field(node, LL_NODE_ELEM, new_val);
            Ok(Some(old))
        }
        None => {
            let size = ll_size(ctx, this);
            Err(ll_out_of_bounds(index, size))
        }
    }
}

pub(crate) fn native_ll_get_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "List is empty".to_string(),
                }
                .into(),
            )
        }
    };
    match ll_get(ctx, this, "head") {
        Value::Object(Some(head)) => Ok(Some(ctx.get_field(head, LL_NODE_ELEM))),
        _ => Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "List is empty".to_string(),
            }
            .into(),
        ),
    }
}

pub(crate) fn native_ll_get_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "List is empty".to_string(),
                }
                .into(),
            )
        }
    };
    match ll_get(ctx, this, "tail") {
        Value::Object(Some(tail)) => Ok(Some(ctx.get_field(tail, LL_NODE_ELEM))),
        _ => Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "List is empty".to_string(),
            }
            .into(),
        ),
    }
}

pub(crate) fn ll_unlink_first(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    let head = match ll_get(ctx, this, "head") {
        Value::Object(Some(r)) => r,
        _ => return Value::Object(None),
    };
    let element = ctx.get_field(head, LL_NODE_ELEM);
    let next = ctx.get_field(head, LL_NODE_NEXT);
    let size = ll_size(ctx, this);
    match next {
        Value::Object(Some(next_node)) => {
            ctx.set_field(next_node, LL_NODE_PREV, Value::Object(None));
            ll_set(ctx, this, "head", Value::Object(Some(next_node)));
        }
        _ => {
            ll_set(ctx, this, "head", Value::Object(None));
            ll_set(ctx, this, "tail", Value::Object(None));
        }
    }
    ll_set(ctx, this, "size", Value::Int(size - 1));
    element
}

pub(crate) fn ll_unlink_last(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    let tail = match ll_get(ctx, this, "tail") {
        Value::Object(Some(r)) => r,
        _ => return Value::Object(None),
    };
    let element = ctx.get_field(tail, LL_NODE_ELEM);
    let prev = ctx.get_field(tail, LL_NODE_PREV);
    let size = ll_size(ctx, this);
    match prev {
        Value::Object(Some(prev_node)) => {
            ctx.set_field(prev_node, LL_NODE_NEXT, Value::Object(None));
            ll_set(ctx, this, "tail", Value::Object(Some(prev_node)));
        }
        _ => {
            ll_set(ctx, this, "head", Value::Object(None));
            ll_set(ctx, this, "tail", Value::Object(None));
        }
    }
    ll_set(ctx, this, "size", Value::Int(size - 1));
    element
}

pub(crate) fn native_ll_remove_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "List is empty".to_string(),
                }
                .into(),
            )
        }
    };
    if ll_size(ctx, this) == 0 {
        return Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "List is empty".to_string(),
            }
            .into(),
        );
    }
    Ok(Some(ll_unlink_first(ctx, this)))
}

pub(crate) fn native_ll_remove_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "List is empty".to_string(),
                }
                .into(),
            )
        }
    };
    if ll_size(ctx, this) == 0 {
        return Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "List is empty".to_string(),
            }
            .into(),
        );
    }
    Ok(Some(ll_unlink_last(ctx, this)))
}

pub(crate) fn native_ll_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(ll_size(ctx, this))))
}

pub(crate) fn native_ll_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    Ok(Some(Value::Int(if ll_size(ctx, this) == 0 {
        1
    } else {
        0
    })))
}

/// GC-safe walk of the overlay LinkedList node chain looking for the first
/// node whose element matches `target` (per JDK `equals` semantics via
/// [`list_element_matches`], whose dispatch can trigger a moving GC on every
/// iteration — leaving `this`, `target`, and every node ref stale in the
/// naive walk). Pins and refreshes all of them per iteration, mirroring the
/// established `native_ll_remove_if` idiom. Walks tail→head when
/// `from_tail` (for `removeLastOccurrence`). Returns the REFRESHED
/// `(matching node, this)`.
pub(crate) fn ll_pinned_find(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    target: Value,
    from_tail: bool,
) -> Result<(Option<ObjectRef>, ObjectRef), MethodCallFailed> {
    let this_pin = ctx.pin_native_root(this);
    let th = pin_value(ctx, target);
    let mut this = this;
    let mut target = target;
    // See `pinned_array_search`: recorded, not returned early, so the two
    // `unpin_native_roots` below still run.
    let mut failed: Option<MethodCallFailed> = None;
    let start_field = if from_tail { "tail" } else { "head" };
    let step_slot = if from_tail {
        LL_NODE_PREV
    } else {
        LL_NODE_NEXT
    };
    let mut cur_opt = match ll_get(ctx, this, start_field) {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let mut found = None;
    while let Some(cur) = cur_opt {
        let cur_pin = ctx.pin_native_root(cur);
        let elem = ctx.get_field(cur, LL_NODE_ELEM);
        let matched = match list_element_matches(ctx, &elem, &target) {
            Ok(m) => m,
            Err(e) => {
                failed = Some(e);
                ctx.unpin_native_roots(cur_pin);
                break;
            }
        };
        let cur = ctx.read_native_pin(cur_pin, cur);
        this = ctx.read_native_pin(this_pin, this);
        target = read_pinned_elem(ctx, th, target);
        if matched {
            found = Some(cur);
            ctx.unpin_native_roots(cur_pin);
            break;
        }
        cur_opt = match ctx.get_field(cur, step_slot) {
            Value::Object(Some(r)) => Some(r),
            _ => None,
        };
        ctx.unpin_native_roots(cur_pin);
    }
    ctx.unpin_native_roots(this_pin);
    match failed {
        Some(e) => Err(e),
        None => Ok((found, this)),
    }
}

pub(crate) fn native_ll_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    // Family-1 fix (cce0079): walk via the pinned chain-find helper.
    let (found, _) = ll_pinned_find(ctx, this, target, false)?;
    Ok(Some(Value::Int(if found.is_some() { 1 } else { 0 })))
}

/// `LinkedList.remove(Object)` / `removeFirstOccurrence(Object)` — remove the
/// FIRST node whose element equals the argument (null matches a null element),
/// returning true iff one was removed. Without this overlay native the real-JDK
/// `remove(Object)` bytecode walks the never-populated `first`/`last` fields,
/// finds nothing, and silently no-ops — so the overlay list never shrinks. The
/// Apache httpcore-nio connection pool's `available.remove(entry)` then left
/// stale (and eventually null) entries in the pool's `available` LinkedList,
/// NPEing at `AbstractNIOConnPool.shutdown` (`PoolEntry.close()` on a null
/// receiver) — ES RestClient keep-alive (ES-HANG-02 residual 2).
pub(crate) fn native_ll_remove_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    // Family-1 fix (cce0079): find via the pinned chain walk, then unlink
    // using the REFRESHED node/receiver (the equals dispatches inside the
    // walk can move both — relinking through stale copies corrupted the
    // list structure).
    let (found, this) = ll_pinned_find(ctx, this, target, false)?;
    if let Some(cur) = found {
        ll_unlink_node(ctx, this, cur);
        return Ok(Some(Value::Int(1)));
    }
    Ok(Some(Value::Int(0)))
}

/// `LinkedList.removeIf(Predicate)` — remove all nodes whose element satisfies
/// the predicate. Groovy's SAM detection uses `LinkedList.removeIf` while
/// pruning inherited abstract methods; if this falls through to a path whose
/// iterator does not support `remove`, the failure surfaces as an unrelated
/// reflection `UnsupportedOperationException`.
pub(crate) fn native_ll_remove_if(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    // RULE F, empty receiver included. See `reject_null_functional`.
    reject_null_functional(args.get(1))?;
    let predicate = match args.get(1) {
        Some(Value::Object(Some(p))) => *p,
        _ => return Ok(Some(Value::Int(0))),
    };

    let this_pin = ctx.pin_native_root(this);
    let pred_pin = ctx.pin_native_root(predicate);
    let mut removed = false;
    let mut cur_opt = match ll_get(ctx, this, "head") {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };

    while let Some(cur) = cur_opt {
        let next_orig = match ctx.get_field(cur, LL_NODE_NEXT) {
            Value::Object(Some(n)) => Some(n),
            _ => None,
        };
        let elem_orig = ctx.get_field(cur, LL_NODE_ELEM);

        let cur_pin = ctx.pin_native_root(cur);
        let next_pin = next_orig.map(|n| ctx.pin_native_root(n));
        let elem_pin = pin_value(ctx, elem_orig);

        let predicate = ctx.read_native_pin(pred_pin, predicate);
        let elem = read_pinned_elem(ctx, elem_pin, elem_orig);
        let verdict = match ctx.invoke_virtual(predicate, "test", "(Ljava/lang/Object;)Z", &[elem])
        {
            Ok(v) => v,
            Err(e) => {
                ctx.unpin_native_roots(cur_pin);
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };

        let next_after = next_orig.map(|n| ctx.read_native_pin(next_pin.unwrap(), n));
        if matches!(verdict, Some(Value::Int(v)) if v != 0) {
            let this = ctx.read_native_pin(this_pin, this);
            let cur = ctx.read_native_pin(cur_pin, cur);
            ll_unlink_node(ctx, this, cur);
            removed = true;
        }
        cur_opt = next_after;
        ctx.unpin_native_roots(cur_pin);
    }

    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Int(if removed { 1 } else { 0 })))
}

/// `LinkedList.removeLastOccurrence(Object)` — as above but walks from the tail
/// and removes the LAST matching node. Same overlay-desync rationale.
pub(crate) fn native_ll_remove_last_occurrence(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    // Family-1 fix (cce0079): same as `native_ll_remove_object`, walking
    // tail→head.
    let (found, this) = ll_pinned_find(ctx, this, target, true)?;
    if let Some(cur) = found {
        ll_unlink_node(ctx, this, cur);
        return Ok(Some(Value::Int(1)));
    }
    Ok(Some(Value::Int(0)))
}

pub(crate) fn native_ll_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    ll_set(ctx, this, "head", Value::Object(None));
    ll_set(ctx, this, "tail", Value::Object(None));
    ll_set(ctx, this, "size", Value::Int(0));
    Ok(None)
}

pub(crate) fn native_ll_peek(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    match ll_get(ctx, this, "head") {
        Value::Object(Some(head)) => Ok(Some(ctx.get_field(head, LL_NODE_ELEM))),
        _ => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_ll_poll(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    if ll_size(ctx, this) == 0 {
        return Ok(Some(Value::Object(None)));
    }
    Ok(Some(ll_unlink_first(ctx, this)))
}

/// `LinkedList.pollFirst()` — real bytecode is `first == null ? null :
/// unlinkFirst(first)`, i.e. exactly `poll()`'s body. Registered separately
/// because a native on `poll` does not intercept `pollFirst`, and the real
/// `unlinkFirst` is a second writer on `size`/`first` that `ll_get`'s
/// overlay-first read can never see. See the registrar note.
pub(crate) fn native_ll_poll_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_ll_poll(ctx, args)
}

/// `LinkedList.pollLast()` — `last == null ? null : unlinkLast(last)`. The
/// mirror image of [`native_ll_poll_first`]; `removeLast()` differs only in
/// throwing `NoSuchElementException` on empty where this answers `null`.
pub(crate) fn native_ll_poll_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    if ll_size(ctx, this) == 0 {
        return Ok(Some(Value::Object(None)));
    }
    Ok(Some(ll_unlink_last(ctx, this)))
}

/// `LinkedList.addAll(int index, Collection c)` — the indexed half of the
/// `addAll` pair whose no-index sibling is [`native_ll_add_all`].
///
/// Bounds are checked BEFORE the first link, as the JDK does
/// (`checkPositionIndex` precedes `c.toArray()`), so a bad index cannot leave a
/// half-inserted list. `collect_collection_elements_or_real` is the same
/// accessor `native_ll_add_all` uses, so a foreign (non-native-backed) source
/// collection is read through its own bytecode rather than reported empty.
pub(crate) fn native_ll_add_all_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // The null check for the INDEXED twin. Round 1 reached `addAll(Collection)`
    // and not this one -- the same half-application this file has paid for
    // before. Under C4W4-6 (`CRATONVM_COMPAT_NULL_COLLECTION_MESSAGE`) it
    // follows the JDK body's order, `checkPositionIndex(index)` THEN `Object[]
    // a = c.toArray()`: `addAll(5, null)` on an empty list is the
    // `IndexOutOfBoundsException`, not an NPE, and a valid index the helpful
    // `c.toArray()` message.
    let jdk_order = null_collection_message_enabled();
    if !jdk_order {
        reject_null_collection(args.get(2))?;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let size = ll_size(ctx, this);
    if jdk_order {
        if index < 0 || index > size {
            return Err(ll_out_of_bounds(index, size));
        }
        reject_null_collection_asked(&*ctx, args.get(2), NULL_ARG_LL_ADD_ALL_AT, NULL_C_TO_ARRAY)?;
    }
    let coll = match args.get(2) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    if index < 0 || index > size {
        return Err(ll_out_of_bounds(index, size));
    }
    // Family-1 fix (cce0079), copied from `native_ll_add_all`: every link
    // allocates a node, so an earlier iteration's GC leaves `this` and every
    // later `elems` slot stale. Pin and refresh per iteration. The insertion
    // point walk (`ll_node_at`) must also be redone per element, because the
    // node that was at `at` moves one position along after each insert.
    //
    // The pin is taken BEFORE the element collection, not after: reading a
    // foreign source collection runs its own bytecode, which is a GC point that
    // can relocate `this`.
    let this_pin = ctx.pin_native_root(this);
    let elems = match collect_collection_elements_or_real(ctx, coll) {
        Ok(e) => e,
        Err(err) => {
            ctx.unpin_native_roots(this_pin);
            return Err(err);
        }
    };
    if elems.is_empty() {
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Int(0)));
    }
    let (_, handles) = pin_value_slice(ctx, &elems);
    // The loop's first act is a `read_native_pin`, so this binding is only a
    // carrier — do not pre-refresh it, or the assignment reads as dead.
    let mut this = this;
    let mut at = index;
    let mut failed: Option<MethodCallFailed> = None;
    for (i, e) in elems.iter().enumerate() {
        this = ctx.read_native_pin(this_pin, this);
        let e = read_pinned_elem(ctx, handles[i], *e);
        let step = if at >= ll_size(ctx, this) {
            ll_link_last(ctx, this, e)
        } else {
            match ll_node_at(ctx, this, at) {
                Some(succ) => ll_link_before(ctx, this, e, succ),
                None => ll_link_last(ctx, this, e),
            }
        };
        if let Err(err) = step {
            failed = Some(err);
            break;
        }
        at += 1;
    }
    ctx.unpin_native_roots(this_pin);
    match failed {
        Some(e) => Err(e),
        None => Ok(Some(Value::Int(1))),
    }
}

/// `LinkedList.descendingIterator()` — a REVERSED snapshot iterator.
///
/// Real `descendingIterator()` does not delegate to the registered
/// `listIterator(int)`: it `new`s a `java/util/LinkedList$ListItr` and calls its
/// constructor directly, so the returned iterator is node-live and its
/// `remove()` runs real `LinkedList.unlink` — a second writer on `size`,
/// `first` and `last` that `ll_get` never observes. That is the measured
/// reproducer `native_ll_iterator`'s comment records, and until this
/// registration existed it was reachable from plain Java with no reflection.
///
/// The snapshot is reversed in place before it is handed to
/// `real_snapshot_iterator`, which builds a REAL `java/util/Arrays$ArrayItr`
/// over it — so `hasNext`/`next` are real JDK bytecode over real fields, and
/// only `remove()` is routed back through the natives.
///
/// Residual, stated because `SnapshotItrRoute::LinkedList`'s own doc states it:
/// that route removes the FIRST occurrence of the returned element rather than
/// unlinking the exact node, so on a list holding DUPLICATES a
/// `descendingIterator().remove()` deletes the wrong one of the equal pair. The
/// alternative it replaces deleted the right node and then corrupted `size` for
/// the rest of the list's life, so this is the smaller residual, not the
/// absence of one.
pub(crate) fn native_ll_descending_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // GC-safety: mirrors `native_ll_iterator`'s refusal arm — `ll_snapshot_array`
    // allocates, so `this` is pinned across it and re-read before it is recorded
    // as the iterator's backing. `snap` is pinned too, because the fallback arm
    // below has to hand it on AFTER an allocation attempt.
    let this_pin = ctx.pin_native_root(this);
    let snap = ll_snapshot_array(ctx, this);
    let snap_pin = ctx.pin_native_root(snap);
    let n = ctx.array_length(snap);
    // Reverse in place. Pure array reads/writes: no allocation, so no GC point
    // separates the two ends of a swap.
    if n > 1 {
        let mut lo = 0usize;
        let mut hi = n - 1;
        while lo < hi {
            let a = ctx.get_array_element(snap, lo);
            let b = ctx.get_array_element(snap, hi);
            ctx.set_array_element(snap, lo, b);
            ctx.set_array_element(snap, hi, a);
            lo += 1;
            hi -= 1;
        }
    }
    let this = ctx.read_native_pin(this_pin, this);
    let snap_cur = ctx.read_native_pin(snap_pin, snap);
    match real_snapshot_iterator(ctx, snap_cur, n, Some((this, SnapshotItrRoute::LinkedList))) {
        Ok(v) => {
            ctx.unpin_native_roots(this_pin);
            Ok(v)
        }
        // Only reachable in an image with no real `java/util/Arrays$ArrayItr`,
        // i.e. a `synthetic-jdk` build — where there is no real `LinkedList`
        // bytecode for this method to have been shadowing in the first place.
        // Fall back to the crate's historical snapshot shape rather than
        // propagating a refusal that names a class the strict mode was never
        // asked about. `remove()` is `UnsupportedOperationException` on that
        // shape, which is what every other synthetic-jdk snapshot iterator
        // already answers; the real-JDK arm above is the one that carries the
        // `SnapshotItrRoute::LinkedList` write-back.
        Err(_) => {
            let snap_cur = ctx.read_native_pin(snap_pin, snap);
            ctx.unpin_native_roots(this_pin);
            make_fabricated_iterator_from_array(ctx, snap_cur, n)
        }
    }
}

pub(crate) fn native_ll_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let arr = alloc_ref_array(ctx, 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let size = ll_size(ctx, this) as usize;
    // GC-safety: see `ll_snapshot_array`.
    //
    // gc-common w10-g: registered-only (`LinkedList.toArray()`), so the result
    // array may collect and retry. `live` audit: `this` is the root
    // `rooted_across1` holds and rewrites; the nodes are read after the
    // allocation, through the rewritten `this`.
    let (this, arr) = rooted_across1(ctx, this, |ctx| {
        new_ref_array_grow(ctx, ClassId::new(0), size, GrowAlloc::Reclaim)
    });
    let mut cur_opt = match ll_get(ctx, this, "head") {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let mut i = 0;
    while let Some(cur) = cur_opt {
        let elem = ctx.get_field(cur, LL_NODE_ELEM);
        ctx.set_array_element(arr, i, elem);
        i += 1;
        cur_opt = match ctx.get_field(cur, LL_NODE_NEXT) {
            Value::Object(Some(r)) => Some(r),
            _ => None,
        };
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// `LinkedList.toArray(T[])` — typed-array overload.
///
/// CratonVM's `LinkedList` is an overlay-backed structure (head/next nodes
/// live in our own fields, NOT the JDK's `first`/`last`/`size`). The real
/// JDK's `LinkedList.toArray(T[])` bytecode iterates the JDK `first` node,
/// which is always null in our representation — so without this override the
/// call returns a correctly-sized array full of `null`s. That bug surfaced as
/// ActiveMQ's `--version` NPE: `console.Main.runTaskClass` does
/// `list.toArray(new String[list.size()])` on a `LinkedList`, then
/// `AbstractCommand.parseOptions` calls `.startsWith("-")` on the (null)
/// first element.
pub(crate) fn native_ll_to_array_typed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // RULE C: `toArray(T[])` dereferences the template for its length before
    // anything else, so a null one is an NPE even for an empty list -- with
    // HotSpot's helpful message (MEASURED, `R15Compat5NullArgMore` row 5).
    reject_null_collection_asked(&*ctx, args.get(1), NULL_ARG_LL_TO_ARRAY, NULL_A_LENGTH)?;
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let template = args.get(1).copied().unwrap_or(Value::Object(None));
    let size = ll_size(ctx, this) as usize;
    // Reuse the supplied array when it is large enough; otherwise allocate.
    // GC-safety: the fallback allocation can move `this` (walked below) and
    // the caller-supplied template. See `rooted_across`.
    let mut this = this;
    let mut template_ref = match template {
        Value::Object(Some(a)) => a,
        _ => this,
    };
    let reuse = matches!(template, Value::Object(Some(a)) if ctx.array_length(a) >= size);
    let target = if reuse {
        template_ref
    } else {
        // The result must have the TEMPLATE's runtime type, not `Object[]`.
        // `Collection.toArray(T[])` is specified that way and every sibling
        // implementation here already does it -- an array's heap header stores
        // its component class id, so `class_id_of_object(template)` IS the
        // component id `new_ref_array` wants.
        //
        // This allocated a bare `Object[]`, which is a wrong ANSWER on its own
        // (a caller assigning to `String[]` gets a `ClassCastException` at the
        // `checkcast`) and which also silently disabled the store check added
        // beside it: every reference is storable in an `Object[]`, so the
        // narrowing element the check exists to reject was legal in the array
        // actually being filled. MEASURED: `LinkedList<Object>` holding an
        // Integer, `toArray(new String[0])` -> HotSpot `ArrayStoreException:
        // java.lang.Integer`, this VM a populated `Object[]`.
        //
        // A NULL template still gets `Object[]`, which is correct: there is no
        // requested type to honour.
        // Resolved BEFORE `rooted_across` borrows the roots: a `ClassId` is a
        // plain value and a moving collection cannot invalidate it, whereas
        // reading `template_ref` inside the closure would borrow a root the
        // call already holds mutably.
        let want_comp = match template {
            Value::Object(Some(_)) => Some(ctx.class_id_of_object(template_ref)),
            _ => None,
        };
        // gc-common w10-g: registered-only (`LinkedList.toArray(T[])`), so the
        // allocation may collect and retry. `live` audit: `this` and the
        // template are the roots `rooted_across` holds and rewrites.
        rooted_across(ctx, &mut [&mut this, &mut template_ref], |ctx| {
            new_ref_array_grow(
                ctx,
                want_comp.unwrap_or(ClassId::new(0)),
                size,
                GrowAlloc::Reclaim,
            )
        })
    };
    let mut cur_opt = match ll_get(ctx, this, "head") {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let mut i = 0;
    while let Some(cur) = cur_opt {
        if i >= size {
            break;
        }
        let elem = ctx.get_field(cur, LL_NODE_ELEM);
        // Same `aastore` route as the set views: `LinkedList` inherits
        // `AbstractCollection.toArray(T[])`, whose `ArrayStoreException` names
        // the value rather than the arrays.
        if let Some(e) = cratonvm_native_api::array_store::reject_unstorable(
            ctx,
            target,
            elem,
            cratonvm_native_api::array_store::StoreRoute::Aastore,
        ) {
            return Err(e);
        }
        ctx.set_array_element(target, i, elem);
        i += 1;
        cur_opt = match ctx.get_field(cur, LL_NODE_NEXT) {
            Value::Object(Some(r)) => Some(r),
            _ => None,
        };
    }
    // JDK contract: if the supplied array is longer than the list, the
    // element immediately after the copied range is set to null.
    let target_len = ctx.array_length(target);
    if target_len > size {
        ctx.set_array_element(target, size, Value::Object(None));
    }
    Ok(Some(Value::Object(Some(target))))
}

pub(crate) fn native_ll_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let s = ctx.create_string("[]");
            return Ok(Some(Value::Object(Some(s))));
        }
    };
    let size = ll_size(ctx, this) as usize;
    // gc-common w20-a: collect the elements first (the walk does not
    // allocate), then render them through pins. Rendering inside the walk ran
    // each element's `toString()` while the next node was held raw, so a
    // collection it completed left the walk following a pre-move address.
    let mut elems = Vec::with_capacity(size);
    let mut cur_opt = match ll_get(ctx, this, "head") {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    while let Some(cur) = cur_opt {
        elems.push(ctx.get_field(cur, LL_NODE_ELEM));
        cur_opt = match ctx.get_field(cur, LL_NODE_NEXT) {
            Value::Object(Some(r)) => Some(r),
            _ => None,
        };
    }
    let parts = display_units_pinned(ctx, &elems)?;
    let text = wrap_join("[", &parts, ", ", "]");
    let s = ctx.create_string_from_units(&text);
    Ok(Some(Value::Object(Some(s))))
}

// LinkedList$Itr = 2-field synthetic (field 0 = current node, field 1 = list ref for size tracking)
/// `LinkedList.iterator()`.
///
/// On HotSpot this method does not exist: `LinkedList` inherits
/// `AbstractSequentialList.iterator()`, which is `return listIterator();`. So
/// `iterator()` and `listIterator()` return the SAME class, and
/// `linkedList.iterator() instanceof ListIterator` is true. This VM returned a
/// separate `java/util/LinkedList$Itr` — a name the real JDK does not declare —
/// so both of those were wrong. Route to the same carrier and the divergence
/// closes at the source rather than at nine more registrations.
///
/// The `LinkedList$Itr` body below is retained, unchanged, for the image that
/// has no real `LinkedList$ListItr` to wear.
pub(crate) fn native_ll_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `alloc_lli_view` refreshes `this` even when it declines (gc-common
    // w20-a: its class lookup can collect).
    if let Some(live) = alloc_lli_view(ctx, &mut this, 0) {
        return Ok(Some(Value::Object(Some(live))));
    }
    let head = ll_get(ctx, this, "head");
    // GC-safety: `alloc_synthetic` can collect; `this` and the head node are
    // both stored into the iterator afterwards. See `rooted_across`.
    let head_pin = pin_value(ctx, head);
    let (this, itr) = rooted_across1(ctx, this, |ctx| {
        try_alloc_synthetic(ctx, "java/util/LinkedList$Itr", 3)
    });
    // `java/util/LinkedList$Itr` is a name no JDK image declares (the real one
    // is `LinkedList$ListItr`), so `--jdk-only` refuses it and every
    // `linkedList.iterator()` — including the one real
    // `AbstractCollection.toString()` reaches — died with
    // `NoClassDefFoundError: java/util/LinkedList$Itr` before `hasNext()`.
    //
    // The obvious alternative was to drop the interception and let real
    // `LinkedList` bytecode run, and unlike the other collections here that is
    // genuinely available: the natives mirror their overlay into the real
    // `first`/`last`/`size` fields (`ll_set`; see `ll_get`'s overlay-MISS arm),
    // and `linkedList.descendingIterator()` — which nothing intercepts —
    // already walks that chain and answers `[c,b,a]` under `--jdk-only` today.
    // The two registrar comments below claiming *"the JDK `first` field is
    // always null"* and *"our overlay-based LL never writes those"* have been
    // outlived by that mirror.
    //
    // It is still the wrong answer, and one run says why. Drive a real
    // `ListItr.remove()` (via `descendingIterator()`) against a native
    // LinkedList and the two owners disagree: real `unlink` decrements the real
    // `size` and re-links the real nodes, `ll_get` reads the OVERLAY first and
    // still answers the old size, and the NEXT real iteration walks off the end
    // — `NullPointerException: Cannot read field "item"` at
    // `LinkedList$ListItr.previous`, reproducible in `Compatible` mode on an
    // unmodified binary. Handing iteration to real bytecode would put a second
    // writer on state the natives own. The snapshot keeps one owner.
    //
    // `Compatible` is untouched: `try_alloc_synthetic` succeeds there and the
    // node-live `LinkedList$Itr` is still what `--real-jdk` hands back, with
    // its `remove()`-unlinks-the-node-just-returned semantics intact.
    let itr: ObjectRef = match itr {
        Ok(itr) => itr,
        Err(_refused) => {
            if head_pin != usize::MAX {
                ctx.unpin_native_roots(head_pin);
            }
            let this_pin = ctx.pin_native_root(this);
            let snap: ObjectRef = ll_snapshot_array(ctx, this);
            let n = ctx.array_length(snap);
            let this = ctx.read_native_pin(this_pin, this);
            let real =
                real_snapshot_iterator(ctx, snap, n, Some((this, SnapshotItrRoute::LinkedList)));
            ctx.unpin_native_roots(this_pin);
            return real;
        }
    };
    let head = read_pinned_elem(ctx, head_pin, head);
    if head_pin != usize::MAX {
        ctx.unpin_native_roots(head_pin);
    }
    ctx.set_field(itr, 0, head); // current node
    ctx.set_field(itr, 1, Value::Object(Some(this))); // list ref
    ctx.set_field(itr, 2, Value::Object(None)); // last returned node
    Ok(Some(Value::Object(Some(itr))))
}

pub(crate) fn native_ll_itr_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let has = matches!(ctx.get_field(this, 0), Value::Object(Some(_)));
    Ok(Some(Value::Int(if has { 1 } else { 0 })))
}

pub(crate) fn native_ll_itr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "No more elements".to_string(),
                }
                .into(),
            )
        }
    };
    let cur = match ctx.get_field(this, 0) {
        Value::Object(Some(r)) => r,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "No more elements".to_string(),
                }
                .into(),
            )
        }
    };
    let element = ctx.get_field(cur, LL_NODE_ELEM);
    let next = ctx.get_field(cur, LL_NODE_NEXT);
    ctx.set_field(this, 0, next);
    // Record the node just returned so `remove()` can unlink it.
    ctx.set_field(this, 2, Value::Object(Some(cur)));
    Ok(Some(element))
}

/// Native `LinkedList$Itr.remove()` — removes the element returned by the most
/// recent `next()`. `LinkedList$Itr` is a synthetic class (real-JDK `LinkedList`
/// exposes only `ListItr`), so `Iterator.remove()` would otherwise resolve to
/// the interface default method, which throws `UnsupportedOperationException`.
pub(crate) fn native_ll_itr_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let last = match ctx.get_field(this, 2) {
        Value::Object(Some(n)) => n,
        _ => {
            // `remove()` before `next()`, or twice in a row.
            //
            // Through `sli_illegal_state`, which builds the exception with NO
            // message. `RuntimeError::IllegalStateException` always supplies
            // one, so this printed `java.lang.IllegalStateException: remove`
            // where HotSpot prints `java.lang.IllegalStateException` —
            // `Throwable.toString()` omits the suffix only for a null message.
            // The `"remove"` text belongs to `Iterator.remove()`'s throwing
            // DEFAULT, which is a different exception from a different class;
            // copying it here made the two indistinguishable in a log.
            return Err(sli_illegal_state(ctx));
        }
    };
    let list = match ctx.get_field(this, 1) {
        Value::Object(Some(l)) => l,
        _ => return Ok(None),
    };
    ll_unlink_node(ctx, list, last);
    // Clear `lastReturned` so a second `remove()` without an intervening
    // `next()` correctly throws.
    ctx.set_field(this, 2, Value::Object(None));
    Ok(None)
}

