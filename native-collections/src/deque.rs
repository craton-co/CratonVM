// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// ArrayDeque (circular buffer based Deque implementation)
// ===========================================================================

pub(crate) const AD_FIELD_DATA: usize = 0; // Object[] circular buffer
pub(crate) const AD_FIELD_HEAD: usize = 1; // Int head index
pub(crate) const AD_FIELD_TAIL: usize = 2; // Int tail index
/// One PAST the last slot an `ArrayDeque` has. Not a field any more: the count
/// is derived from `head`/`tail` (see [`ad_state`]), nothing writes slot 3, and
/// `synthetic_stub_fields` declares the three the real class declares. Kept as
/// the name for "the extent this native surface needs", which is what the
/// receiver-width bound in `collect_collection_elements` asks for.
pub(crate) const AD_FIELD_SIZE: usize = 3;
/// Elements a default-constructed deque holds before the first grow. The
/// *array* is one slot longer — see [`ad_ensure_capacity`] for why that spare
/// slot is not an optimisation but the JDK's own emptiness invariant.
pub(crate) const AD_DEFAULT_CAPACITY: usize = 16;

/// `ArrayDeque.addFirst`/`addLast` open with `if (e == null) throw new
/// NullPointerException();`, and that refusal is load-bearing rather than
/// defensive: a null in the ring buffer is how real `ArrayDeque` bytecode
/// *encodes* "another thread mutated me while I was iterating".
/// `nonNullElementAt` turns any null it reads into a
/// `ConcurrentModificationException`, so accepting one here does not just lose
/// three `no-throw` observables — it arms the JDK's own iterator to kill a
/// caller that never touched null.
///
/// Measured 2026-08-12, HotSpot 25.0.3.9 vs CratonVM `--real-jdk`, one binary:
/// `dq.add(null)` returned `true`, `dq.size()` answered 3 where HotSpot said 0,
/// the deque printed `[a, null, null, null, x, y, x, z]`, and then
///
/// ```text
/// java.util.ConcurrentModificationException
///     at java.util.ArrayDeque.nonNullElementAt(ArrayDeque.java:268)
///     at java.util.ArrayDeque$DescendingIterator.next(ArrayDeque.java:746)
/// ```
///
/// took the remaining 12 observables of the `dequeEdges` probe section with it.
/// The CME was therefore never an over-throw and never had anything to do with
/// the `modCount` this crate gained this session: real JDK bytecode diagnosed
/// the state this missing check produced, and diagnosed it correctly. See
/// W7-33-differential-dead-sections.
///
/// The check sits in the two funnels rather than at each registration because
/// that is where the JDK puts it — `add`, `offer`, `offerFirst`, `offerLast`,
/// `push` and `addAll` all reach the deque through `addFirst`/`addLast` in
/// both implementations. The one other caller is the `java/util/Queue`
/// interface bridge, which points at the `ArrayDeque` implementation by
/// construction; the null-tolerant `Queue` in the JDK is `LinkedList`, and
/// `((Queue<String>) new LinkedList<String>()).offer(null)` was measured
/// storing its null on both CratonVM modes today, so it does not route here.
///
/// HotSpot's is message-less. `thrownDetail` prints a message when there is
/// one, so inventing one would itself be a divergence.
#[inline]
pub(crate) fn ad_refuse_null(elem: Value) -> Result<(), MethodCallFailed> {
    if matches!(elem, Value::Object(None)) {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    }
    Ok(())
}

pub(crate) fn ad_state(ctx: &dyn NativeContext, this: ObjectRef) -> (Option<ObjectRef>, i32, i32, i32) {
    let data = match ctx.get_field(this, AD_FIELD_DATA) {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let head = match ctx.get_field(this, AD_FIELD_HEAD) {
        Value::Int(v) => v,
        _ => 0,
    };
    let tail = match ctx.get_field(this, AD_FIELD_TAIL) {
        Value::Int(v) => v,
        _ => 0,
    };
    // `size` is DERIVED from `head`/`tail`, never read back from slot 3.
    //
    // Slot 3 used to be ours -- the real `java.util.ArrayDeque` declares
    // exactly `elements`/`head`/`tail`, and `synthetic_stub_fields` padded the
    // class to four so a count could live there. It no longer does: with
    // nothing reading or writing that slot, the table was narrowed to three on
    // 2026-09-12 and the class stopped being padded out of the compact layout
    // (232 bytes empty -> the width three fields actually need).
    // Storing the count there made every real
    // JDK body that mutates the buffer a corruption: `delete(i)` moves `head`
    // or `tail` and cannot know slot 3 exists. `native_ad_remove_first_occurrence`
    // carries the scar -- it exists only to keep real `delete` bytecode away
    // from the deque, after that desync stranded H2's `waitingSessions` queue.
    //
    // Shadowing every mutator is the wrong level to fix that at: the leak is
    // any real body at all, and we cannot register them all. `DeqIterator.remove()`
    // is the proof. Measured 2026-08-30 on `descendingIterator()`, which we do
    // not register and so already ran real bytecode:
    //
    // ```text
    //   fields after remove   HotSpot  cap=4 head=0 tail=2 es=[a, b, null, null]
    //                         CratonVM cap=4 head=0 tail=2 es=[a, b, null, null]
    //   size()                HotSpot  2
    //                         CratonVM 3
    //   toString()            HotSpot  [a, b]
    //                         CratonVM [a, b, null]
    // ```
    //
    // -- the buffer was byte-for-byte right and only the count was wrong, and a
    // null then leaked out of the deque into `toString`, `toArray`, `stream`
    // and a re-walk. `ad_refuse_null`'s doc explains what a null inside an
    // `ArrayDeque` costs: the JDK's own `nonNullElementAt` reads it as
    // "another thread mutated me" and kills the next iteration.
    //
    // The JDK derives its count the same way (`size()` is
    // `sub(tail, head, elements.length)`), which is why it has no such field to
    // desync. Deriving here makes our natives and every real body agree by
    // construction. It is only unambiguous because the buffer is never full --
    // see `ad_ensure_capacity` for why that spare slot is load-bearing.
    let cap = data.map_or(0, |d| ctx.array_length(d)) as i32;
    let size = if cap <= 0 {
        0
    } else {
        (tail - head).rem_euclid(cap)
    };
    (data, head, tail, size)
}

/// Grow the ring buffer so it can hold `min_cap` elements **and still leave one
/// slot free**.
///
/// The spare slot is the whole of the 2026-08-11 `stream()` fix, so it is worth
/// stating why it is not slack. Slots 0..2 of our overlay are not ours: the real
/// `java.util.ArrayDeque` declares exactly `elements`/`head`/`tail` in that
/// order, which is now also exactly what `synthetic_stub_fields` declares --
/// the fourth slot our `size` used to live at is gone, because the count is
/// derived. Everything real JDK bytecode reads about this deque it
/// derives from `head`/`tail` alone — `size()` is `sub(tail, head,
/// elements.length)` — and that arithmetic has no way to distinguish full from
/// empty. The JDK resolves the ambiguity by never letting the buffer fill:
/// `new ArrayDeque()` is `new Object[16 + 1]`, `new ArrayDeque(n)` is
/// `new Object[n + 1]`, and `grow` runs one element early. `head == tail` then
/// means empty, always.
///
/// This function used to grow only when the buffer was *over*full
/// (`min_cap <= old_cap` returned early), so a deque whose element count
/// reached its capacity wrapped `tail` right back onto `head` -- a perfectly
/// consistent state for our natives, which read `size` from slot 3 at the
/// time, and an
/// EMPTY deque to everything else. Measured on the pre-fix binary against
/// HotSpot 25, two elements from `new ArrayDeque<>(List.of("a","b"))`:
///
/// ```text
/// HotSpot   elements=[a, b, null]  head=0  tail=2
/// CratonVM  elements=[a, b]        head=0  tail=0
/// ```
///
/// with `size()`, `toString()`, `contains()`, `getFirst()` and `forEach()` all
/// answering correctly beside it — those are ours — while `stream().count()`,
/// `parallelStream()`, `spliterator()` and `toArray(T[])` (which we do not
/// register, so it is real bytecode) all answered for an empty deque. Sixteen
/// elements in a default deque did the same thing, and `[a,b]` in a
/// 16-slot buffer did not, which is why this hid: it fires only when the count
/// happens to land exactly on the capacity.
///
/// `docs/feature-designs/collections-interception.md`
/// is why it was found at all — an empty collection reads as a pass at every
/// caller that only iterates, so the probe that caught this prints element
/// CONTENT for every member rather than a verdict.
pub(crate) fn ad_grow(ctx: &mut dyn NativeContext, this: &mut ObjectRef, needed: i32) {
    let (data, head, _tail, _size) = ad_state(ctx, *this);
    let Some(old_buf) = data else {
        return;
    };
    let old_cap = ctx.array_length(old_buf) as i32;
    if old_cap <= 0 {
        return;
    }
    let jump = if old_cap < 64 {
        old_cap + 2
    } else {
        old_cap >> 1
    };
    let new_cap = old_cap + if jump < needed { needed } else { jump };

    // GC-safety: the allocation can complete a moving young GC; `this` and the
    // old buffer are both bare Rust locals used below. See `rooted_across`.
    //
    // gc-common w10-g: the grow may collect and retry (`GrowAlloc::Reclaim`).
    // `live` audit: `this` and the old buffer are the roots `rooted_across`
    // holds and rewrites (`this` through the caller's `&mut`); the element was
    // stored into the buffer BEFORE the grow (see `native_ad_add_first`), so it
    // moves with the buffer. Every caller is safe to collect under:
    // `native_ad_add_first` / `native_ad_add_last` return right after the grow,
    // and their only Rust callers (`native_ad_init_from_collection`,
    // `native_ad_add_all`, `ad_rebuild`, and the `add`/`offer*` wrappers) pin
    // the receiver and every pending element and re-read them per element.
    // No other crate calls them.
    let mut old = old_buf;
    let new_buf = rooted_across(ctx, &mut [this, &mut old], |ctx| {
        new_ref_array_grow(ctx, ClassId::new(0), new_cap as usize, GrowAlloc::Reclaim)
    });

    // `Arrays.copyOf` -- SLOT FOR SLOT, not head-first. The old code normalised
    // instead, copying the elements in logical order and resetting
    // `head = 0`/`tail = size`. That is a legal deque and every accessor of
    // ours agreed with it, which is why it stood; it is not the JDK's, and the
    // difference is visible to any real body that holds a PHYSICAL index --
    // which `DeqIterator` does.
    for k in 0..old_cap as usize {
        let v = ctx.get_array_element(old, k);
        ctx.set_array_element(new_buf, k, v);
    }
    ctx.set_field(*this, AD_FIELD_DATA, Value::Object(Some(new_buf)));

    // The wrap-slide. When the elements straddle the end of the buffer, the
    // first leg has to stay flush against the end, so it moves up by exactly
    // the space gained and `head` follows it. `tail == head` is ambiguous
    // between full and empty, and the JDK disambiguates it the same way we do
    // everywhere else -- by looking at whether the head slot holds anything.
    let tail = match ctx.get_field(*this, AD_FIELD_TAIL) {
        Value::Int(v) => v,
        _ => 0,
    };
    let head_occupied = matches!(
        ctx.get_array_element(new_buf, head as usize),
        Value::Object(Some(_))
    );
    if tail < head || (tail == head && head_occupied) {
        let new_space = new_cap - old_cap;
        // `System.arraycopy(es, head, es, head + newSpace, oldCapacity - head)`.
        // The regions overlap and move UP, so this walks from the top down.
        for k in (0..(old_cap - head) as usize).rev() {
            let v = ctx.get_array_element(new_buf, head as usize + k);
            ctx.set_array_element(new_buf, (head + new_space) as usize + k, v);
        }
        for k in head..(head + new_space) {
            ctx.set_array_element(new_buf, k as usize, Value::Object(None));
        }
        ctx.set_field(*this, AD_FIELD_HEAD, Value::Int(head + new_space));
    }
}

pub(crate) fn register_array_deque_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/ArrayDeque";

    r.register(c, "<init>", "()V", native_ad_init);
    r.register(c, "<init>", "(I)V", native_ad_init_capacity);
    // `(Collection)` — see the note on `java/util/Vector`'s. `addLast` per
    // element, which is what `ArrayDeque(Collection)` specifies.
    r.register(
        c,
        "<init>",
        "(Ljava/util/Collection;)V",
        native_ad_init_from_collection,
    );
    r.register(c, "size", "()I", native_ad_size);
    r.register(c, "isEmpty", "()Z", native_ad_is_empty);
    r.register(c, "addFirst", "(Ljava/lang/Object;)V", native_ad_add_first);
    r.register(c, "addLast", "(Ljava/lang/Object;)V", native_ad_add_last);
    r.register(c, "add", "(Ljava/lang/Object;)Z", native_ad_add);
    r.register(
        c,
        "offerFirst",
        "(Ljava/lang/Object;)Z",
        native_ad_offer_first,
    );
    r.register(
        c,
        "offerLast",
        "(Ljava/lang/Object;)Z",
        native_ad_offer_last,
    );
    r.register(c, "offer", "(Ljava/lang/Object;)Z", native_ad_offer);
    r.register(
        c,
        "removeFirst",
        "()Ljava/lang/Object;",
        native_ad_remove_first,
    );
    r.register(
        c,
        "removeLast",
        "()Ljava/lang/Object;",
        native_ad_remove_last,
    );
    r.register(c, "pollFirst", "()Ljava/lang/Object;", native_ad_poll_first);
    r.register(c, "pollLast", "()Ljava/lang/Object;", native_ad_poll_last);
    r.register(c, "poll", "()Ljava/lang/Object;", native_ad_poll_first);
    r.register(c, "getFirst", "()Ljava/lang/Object;", native_ad_get_first);
    r.register(c, "getLast", "()Ljava/lang/Object;", native_ad_get_last);
    r.register(c, "peekFirst", "()Ljava/lang/Object;", native_ad_peek_first);
    r.register(c, "peekLast", "()Ljava/lang/Object;", native_ad_peek_last);
    r.register(c, "peek", "()Ljava/lang/Object;", native_ad_peek_first);
    r.register(c, "push", "(Ljava/lang/Object;)V", native_ad_add_first);
    r.register(c, "pop", "()Ljava/lang/Object;", native_ad_remove_first);
    r.register(c, "element", "()Ljava/lang/Object;", native_ad_get_first);
    r.register(c, "remove", "()Ljava/lang/Object;", native_ad_remove_first);
    r.register(c, "contains", "(Ljava/lang/Object;)Z", native_ad_contains);
    // Object-removal methods. Without these natives they fall through to the
    // real JDK `delete(...)` bytecode, which leaves our synthetic `size` slot
    // stale and silently corrupts the deque (see
    // `native_ad_remove_first_occurrence` — the H2 SYS-lock leak).
    r.register(
        c,
        "remove",
        "(Ljava/lang/Object;)Z",
        native_ad_remove_first_occurrence,
    );
    r.register(
        c,
        "removeFirstOccurrence",
        "(Ljava/lang/Object;)Z",
        native_ad_remove_first_occurrence,
    );
    r.register(
        c,
        "removeLastOccurrence",
        "(Ljava/lang/Object;)Z",
        native_ad_remove_last_occurrence,
    );
    r.register(c, "clear", "()V", native_ad_clear);
    r.register(c, "toArray", "()[Ljava/lang/Object;", native_ad_to_array);
    // NO `iterator` REGISTRATION in real-JDK mode. Real `ArrayDeque.iterator()`
    // bytecode runs, and that is the fix for the fail-fast row this family
    // carried, not a concession. The JDK's `DeqIterator` is fail-fast off a
    // PHYSICAL index into the ring buffer -- `nonNullElementAt` reports any
    // null it reads as a `ConcurrentModificationException` -- so it is exactly
    // as fail-fast as the buffer's layout, and no counter reproduces it.
    // `ad_state` derives the element count and `ad_grow`/`ad_remove_at_logical`
    // reproduce the JDK's own layout byte for byte, so there is nothing left
    // for a shadow to protect. See `native_ad_iterator`'s removal in the same
    // commit.
    //
    // `--synthetic-jdk` HAS NO SUCH BYTECODE, and the consequence is measured
    // 2026-09-02 (`apps/probes/AdDispatch`): after `d.add("a")`, `size()`
    // answers 1 and `peekFirst()` answers "a" while a for-each yields ZERO
    // elements — a silently empty loop, through every declared type and
    // through `addLast` as well. OPEN, and deliberately not fixed here.
    //
    // A `#[cfg(feature = "synthetic-jdk")]` registration was written and
    // REVERTED, because the cheap version of it is the trap this crate already
    // names: minting `java/util/ArrayDeque$Itr` arms the dormant registration
    // at the bottom of `register_snapshot_iterator_natives`, which that comment
    // calls "a trap armed for whoever produces one later". Doing it right means
    // minting the real `ArrayDeque$DeqIterator` with the snapshot fields — i.e.
    // putting ArrayDeque back into `VALUES_ITR_CARRIERS`, which it LEFT on
    // 2026-08-30 — and that table's own doc records `DescendingIterator` making
    // java.base a second producer of the carrier class the last time it was
    // done. Two measured decisions to reverse and a two-producer hazard to
    // re-open; it wants the census that owns this area, not a side edit.
    r.register(c, "toString", "()Ljava/lang/String;", native_ad_to_string);
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        native_ad_for_each,
    );
    r.set_category(__prev_cat);
}

/// `ArrayDeque(Collection)` — the JDK's `this(c.size()); copyElements(c);`.
///
/// **`native_ad_init_capacity` with the SOURCE SIZE, not `native_ad_init`.**
/// The first version of this called the no-arg init, which allocates the
/// default 16 + 1 slots, and the contents were correct — so nothing that reads
/// elements could see the difference. The CAPACITY is observable anyway, and
/// `apps/probes/ItrCarrierCensus` saw it: HotSpot sizes a 3-element deque to 4
/// slots, so a 4th `add` GROWS and reallocates `elements`, and a live
/// `DeqIterator` then reads a null through `nonNullElementAt` and raises
/// `ConcurrentModificationException`. With 17 slots there is no grow, no null,
/// and no CME — `ArrayDeque` came out NOT fail-fast on `add` against HotSpot's
/// fail-fast, one row of that probe, with every element-level check agreeing.
///
/// The `+ 1` inside `native_ad_init_capacity` is what makes `this(c.size())`
/// correct rather than off-by-one; its own comment records the wrap-onto-head
/// bug from getting that wrong.
///
/// Same GC discipline as `native_ll_init_from_collection`:
/// `collect_collection_elements_or_real` re-enters Java and every `addLast`
/// can grow the backing array, so `this` is pinned and each element re-read
/// per iteration rather than held across the call.
pub(crate) fn native_ad_init_from_collection(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `this(c.size())` (C4W4-6, row 3).
    reject_null_collection_asked(&*ctx, args.get(1), NULL_ARG_AD_INIT, NULL_C_SIZE)?;
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let source = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            native_ad_init(ctx, args)?;
            return Ok(None);
        }
    };
    // `CRATONVM_COMPAT_BULK_ADD_JDK_QUESTIONS` (round 15 wave 4, lane
    // compat4): a program's collection is asked what `this(c.size());
    // copyElements(c);` asks -- `size()` (it sizes the ring), then its
    // iterator, one `addLast` per element as it arrives. Round 16 wave 13
    // (lane compatcoll): the walk is `copyElements(c)` -- `c.forEach(..)` --
    // under `CRATONVM_COMPAT_DEQUE_COPY_FOR_EACH` ([`ad_copy_elements`]).
    if bulk_add_jdk_questions_enabled() && copy_source_is_foreign(&*ctx, source) {
        let this_pin = ctx.pin_native_root(this);
        let source_pin = ctx.pin_native_root(source);
        let built = (|| -> Result<(), MethodCallFailed> {
            let src = ctx.read_native_pin(source_pin, source);
            let n = match ctx.invoke_virtual(src, "size", "()I", &[])? {
                Some(Value::Int(n)) => n,
                _ => 0,
            };
            let this_now = ctx.read_native_pin(this_pin, this);
            native_ad_init_capacity(ctx, &[Value::Object(Some(this_now)), Value::Int(n)])?;
            ad_copy_elements(ctx, (this_pin, this), (source_pin, source))
        })();
        ctx.unpin_native_roots(this_pin);
        return built.map(|()| None);
    }
    // The drain runs BEFORE the sizing, which the JDK's `this(c.size())` does
    // not — but `c.size()` and the drained length are the same number, and
    // draining first is what makes the size available without asking the
    // source twice (it may be a view whose `size()` re-walks). `this` is
    // pinned across it because the drain re-enters Java and can collect.
    let this_pin = ctx.pin_native_root(this);
    // Every `?` below releases this frame's pins first (round 15 wave 4, lane
    // compat4: a throwing drain, a refused ring or a null element returned
    // with `this` and the drained elements still pinned).
    let collected = collect_collection_elements_or_real(ctx, source);
    let elems = unpin_on_err(ctx, this_pin, collected)?;
    let this = ctx.read_native_pin(this_pin, this);
    let capacity = i32::try_from(elems.len()).unwrap_or(i32::MAX);
    // Pin the drained elements BEFORE the sizing call, not after it: since
    // gc-common w9-d `native_ad_init_capacity` may collect on a refused
    // allocation, and pinning an address after a move preserves the staleness.
    let (_, handles) = pin_value_slice(ctx, &elems);
    let sized = native_ad_init_capacity(ctx, &[Value::Object(Some(this)), Value::Int(capacity)]);
    unpin_on_err(ctx, this_pin, sized)?;
    for (i, val) in elems.iter().enumerate() {
        let this = ctx.read_native_pin(this_pin, this);
        let val = read_pinned_elem(ctx, handles[i], *val);
        let added = native_ad_add_last(ctx, &[Value::Object(Some(this)), val]);
        unpin_on_err(ctx, this_pin, added)?;
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn native_ad_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // GC-safety: see `rooted_across` — `this` must survive the allocation.
    //
    // `+ 1` is the JDK's own expression: `ArrayDeque()` is literally
    // `elements = new Object[16 + 1]`, and HotSpot 25 prints `elements.len=17`
    // for a fresh deque. The spare slot is the emptiness invariant real
    // bytecode reads this object through — `ad_ensure_capacity` has the full
    // reasoning and the measurement.
    let (this, buf) = rooted_across1(ctx, this, |ctx| {
        alloc_ref_array(ctx, AD_DEFAULT_CAPACITY + 1)
    });
    ctx.set_field(this, AD_FIELD_DATA, Value::Object(Some(buf)));
    ctx.set_field(this, AD_FIELD_HEAD, Value::Int(0));
    ctx.set_field(this, AD_FIELD_TAIL, Value::Int(0));
    Ok(None)
}

pub(crate) fn native_ad_init_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // The real `ArrayDeque(int)` body, transcribed rather than approximated:
    //
    //     elements = new Object[(numElements < 1) ? 1
    //                         : (numElements == Integer.MAX_VALUE) ? Integer.MAX_VALUE
    //                         : numElements + 1];
    //
    // The `+ 1` matters here more than anywhere else in this file, because this
    // constructor is the one `ArrayDeque(Collection)` calls: `this(c.size())`
    // then `copyElements(c)`. With the old `max(n, 1)` a deque built from a
    // 2-element collection got a 2-slot buffer, filled it, and wrapped `tail`
    // onto `head` — `new ArrayDeque<>(List.of("a","b")).stream().count()`
    // answered 0 against HotSpot's 2, in BOTH compatibility modes. See
    // `ad_ensure_capacity`.
    let cap = match args.get(1) {
        Some(Value::Int(c)) if *c < 1 => 1,
        Some(Value::Int(c)) if *c == i32::MAX => i32::MAX as usize,
        Some(Value::Int(c)) => (*c as usize) + 1,
        _ => AD_DEFAULT_CAPACITY + 1,
    };
    // HotSpot throws a catchable OutOfMemoryError for an over-large element
    // array rather than aborting; mirror that instead of the panicking alloc.
    //
    // gc-common w9-d: collect and retry first, as `new Object[n]` does. Legal:
    // this native is reached only through its registration and from
    // `native_ad_init_from_collection` (itself registered-only, which pins
    // everything it holds before calling here), and `this` -- the only
    // reference held -- is in `live`.
    let mut live = [this];
    let buf = match try_ref_array_reclaiming(ctx, ClassId::new(0), cap, &mut live) {
        Some(b) => b,
        None => {
            return Err(cratonvm_types::error::RuntimeError::OutOfMemoryError {
                message: "Requested array size exceeds VM limit".to_string(),
            }
            .into());
        }
    };
    let [this] = live;
    ctx.set_field(this, AD_FIELD_DATA, Value::Object(Some(buf)));
    ctx.set_field(this, AD_FIELD_HEAD, Value::Int(0));
    ctx.set_field(this, AD_FIELD_TAIL, Value::Int(0));
    Ok(None)
}

pub(crate) fn native_ad_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (_, _, _, size) = ad_state(ctx, this);
    Ok(Some(Value::Int(size)))
}

pub(crate) fn native_ad_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1))),
    };
    let (_, _, _, size) = ad_state(ctx, this);
    Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
}

pub(crate) fn native_ad_add_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    // Before anything is stored or grown: see `ad_refuse_null`.
    ad_refuse_null(elem)?;
    // The store comes FIRST, exactly as `ArrayDeque.addFirst` does it:
    //
    //     es[head = dec(head, es.length)] = e;
    //     if (head == tail) grow(1);
    //
    // Growing first and storing after is what this used to do, and it made the
    // element a bare Rust local across a GC-capable allocation -- hence the
    // pin/refresh dance that used to stand here (Family-1 stale-at-store,
    // cce0079). Storing first retires the dance rather than maintaining it: the
    // element is reachable from the buffer before anything can allocate, and
    // `ad_grow` moves it through `get_array_element`/`set_array_element`, which
    // the collector understands.
    let (data, head, tail, _size) = ad_state(ctx, this);
    let cap = data.map_or(0, |d| ctx.array_length(d)) as i32;
    if cap <= 0 {
        return Ok(None);
    }
    let new_head = (head - 1 + cap) % cap;
    if let Some(buf) = data {
        ctx.set_array_element(buf, new_head as usize, elem);
    }
    ctx.set_field(this, AD_FIELD_HEAD, Value::Int(new_head));
    if new_head == tail {
        ad_grow(ctx, &mut this, 1);
    }
    Ok(None)
}

pub(crate) fn native_ad_add_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    // Before anything is stored or grown: see `ad_refuse_null`.
    ad_refuse_null(elem)?;
    // Store first, then advance, then grow if the buffer just filled -- the
    // shape of `ArrayDeque.addLast`. See `native_ad_add_first` for why the
    // order matters to the collector as well as to the layout.
    let (data, head, tail, _size) = ad_state(ctx, this);
    let cap = data.map_or(0, |d| ctx.array_length(d)) as i32;
    if cap <= 0 {
        return Ok(None);
    }
    if let Some(buf) = data {
        ctx.set_array_element(buf, tail as usize, elem);
    }
    let new_tail = (tail + 1) % cap;
    ctx.set_field(this, AD_FIELD_TAIL, Value::Int(new_tail));
    if head == new_tail {
        ad_grow(ctx, &mut this, 1);
    }
    Ok(None)
}

pub(crate) fn native_ad_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_ad_add_last(ctx, args)?;
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_ad_offer_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_ad_add_first(ctx, args)?;
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_ad_offer_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_ad_add_last(ctx, args)?;
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_ad_offer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_ad_add_last(ctx, args)?;
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_ad_remove_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, head, _tail, size) = ad_state(ctx, this);
    if size == 0 {
        return Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "ArrayDeque is empty".to_string(),
            }
            .into(),
        );
    }
    let elem = data.map_or(Value::Object(None), |buf| {
        ctx.get_array_element(buf, head as usize)
    });
    let cap = data.map_or(0, |d| ctx.array_length(d)) as i32;
    // Null the vacated slot, as the real `pollFirst` does (`es[h] = null`).
    // `ad_remove_at_logical` already did this for the interior case and gives
    // the retention reason; the head/tail cases were the two that did not, so a
    // polled deque kept its dropped elements alive and printed them where
    // HotSpot prints `null` (`elements=[a, b]` vs `[null, b, null]` after one
    // `poll()`). Nothing reads outside `head..head+size`, so this was invisible
    // to every accessor — which is exactly why it survived.
    if let Some(buf) = data {
        ctx.set_array_element(buf, head as usize, Value::Object(None));
    }
    let new_head = (head + 1) % cap;
    ctx.set_field(this, AD_FIELD_HEAD, Value::Int(new_head));
    Ok(Some(elem))
}

pub(crate) fn native_ad_remove_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, _head, tail, size) = ad_state(ctx, this);
    if size == 0 {
        return Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "ArrayDeque is empty".to_string(),
            }
            .into(),
        );
    }
    let cap = data.map_or(0, |d| ctx.array_length(d)) as i32;
    let new_tail = (tail - 1 + cap) % cap;
    let elem = data.map_or(Value::Object(None), |buf| {
        ctx.get_array_element(buf, new_tail as usize)
    });
    // Null the vacated slot — same reason as `native_ad_remove_first`.
    if let Some(buf) = data {
        ctx.set_array_element(buf, new_tail as usize, Value::Object(None));
    }
    ctx.set_field(this, AD_FIELD_TAIL, Value::Int(new_tail));
    Ok(Some(elem))
}

/// Delete the element at *logical* index `k` (0 = head), leaving the buffer in
/// the state `ArrayDeque.delete` would have left it in.
///
/// The JDK closes the gap from whichever END is nearer and moves that end's
/// index: fewer elements before the hole, and it slides them forward and
/// advances `head`; otherwise it slides the ones after it back and retracts
/// `tail`. This used to always slide backwards and always retract `tail`, which
/// produces a deque holding the same elements in the same order -- so every
/// accessor of ours agreed, and it stood.
///
/// It is still wrong, and the reason is the same one that makes `ad_grow`'s
/// normalisation wrong: a PHYSICAL index into the buffer is a thing real JDK
/// bodies hold, and `DeqIterator` holds one. Deleting the head of `[a, b, c]`
/// with an iterator open one step in:
///
/// ```text
///   HotSpot   head=1 tail=3 es=[null, b, c, null]   cursor 1 -> reads "b"
///   was       head=0 tail=2 es=[b, c, null, null]   cursor 1 -> reads "c"
/// ```
///
/// -- the same deque, and an iterator that skips an element and then reads a
/// null, which `nonNullElementAt` reports as a `ConcurrentModificationException`
/// against a caller that did nothing wrong.
///
/// The JDK's segmented `System.arraycopy` calls are written here as modular
/// loops. They say the same thing: the array is circular, and the JDK only
/// splits the copy because `arraycopy` is not.
pub(crate) fn ad_remove_at_logical(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    buf: ObjectRef,
    head: i32,
    size: i32,
    cap: i32,
    k: usize,
) {
    let front = k as i32;
    let back = size - front - 1;
    if front < back {
        // Slide the elements BEFORE the hole one place forward, then drop the
        // old head slot. Ties go to the other branch, as in the JDK.
        for j in (0..front).rev() {
            let v = ctx.get_array_element(buf, ((head + j) % cap) as usize);
            ctx.set_array_element(buf, ((head + j + 1) % cap) as usize, v);
        }
        ctx.set_array_element(buf, head as usize, Value::Object(None));
        ctx.set_field(this, AD_FIELD_HEAD, Value::Int((head + 1) % cap));
    } else {
        // Slide the elements AFTER the hole one place back, then drop the slot
        // the tail vacates.
        let hole = (head + front) % cap;
        for j in 0..back {
            let v = ctx.get_array_element(buf, ((hole + j + 1) % cap) as usize);
            ctx.set_array_element(buf, ((hole + j) % cap) as usize, v);
        }
        let new_tail = (head + size - 1) % cap;
        ctx.set_array_element(buf, new_tail as usize, Value::Object(None));
        ctx.set_field(this, AD_FIELD_TAIL, Value::Int(new_tail));
    }
}

/// `ArrayDeque.remove(Object)` / `removeFirstOccurrence(Object)` — remove the
/// first element (scanning from the head) equal to the argument; return whether
/// one was removed.
///
/// These must be intercepted: without a native, they fall through to the real
/// JDK `ArrayDeque.delete(...)` bytecode, which updates `elements`/`head`/`tail`
/// but is oblivious to our synthetic `size` slot (field 3) — so `size()`,
/// `isEmpty()`, iteration and `getFirst()` desync from the real buffer. That
/// silent corruption is what stranded H2's `MVTable.waitingSessions` lock-wait
/// queue (a single `addLast`+`remove(session)` round leaves `size()`==1 instead
/// of 0), so `doLock1`'s `waitingSessions.getFirst() == session` guard never
/// holds and every later DDL dead-ends in "Timeout trying to lock table SYS".
pub(crate) fn native_ad_remove_first_occurrence(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data, head, _tail, size) = ad_state(ctx, this);
    let buf = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Int(0))),
    };
    let cap = ctx.array_length(buf) as i32;
    if cap == 0 || size <= 0 {
        return Ok(Some(Value::Int(0)));
    }
    // Family-1 fix (cce0079): the per-element `equals()` dispatch can move
    // `this`/`buf`/`target` — pin all three, refresh after each dispatch,
    // and shift through the REFRESHED buffer/receiver.
    let this_pin = ctx.pin_native_root(this);
    let buf_pin = ctx.pin_native_root(buf);
    let th = pin_value(ctx, target);
    let mut this = this;
    let mut buf = buf;
    let mut target = target;
    let mut found_k = None;
    for k in 0..size as usize {
        let idx = ((head + k as i32) % cap) as usize;
        let elem = ctx.get_array_element(buf, idx);
        let matched = list_element_matches(ctx, &elem, &target)?;
        this = ctx.read_native_pin(this_pin, this);
        buf = ctx.read_native_pin(buf_pin, buf);
        target = read_pinned_elem(ctx, th, target);
        if matched {
            found_k = Some(k);
            break;
        }
    }
    ctx.unpin_native_roots(this_pin);
    if let Some(k) = found_k {
        ad_remove_at_logical(ctx, this, buf, head, size, cap, k);
        return Ok(Some(Value::Int(1)));
    }
    Ok(Some(Value::Int(0)))
}

/// `ArrayDeque.removeLastOccurrence(Object)` — remove the last element (scanning
/// from the tail) equal to the argument. Same desync hazard as
/// [`native_ad_remove_first_occurrence`].
pub(crate) fn native_ad_remove_last_occurrence(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data, head, _tail, size) = ad_state(ctx, this);
    let buf = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Int(0))),
    };
    let cap = ctx.array_length(buf) as i32;
    if cap == 0 || size <= 0 {
        return Ok(Some(Value::Int(0)));
    }
    // Family-1 fix (cce0079): same pinned walk as
    // `native_ad_remove_first_occurrence`, scanning from the tail.
    let this_pin = ctx.pin_native_root(this);
    let buf_pin = ctx.pin_native_root(buf);
    let th = pin_value(ctx, target);
    let mut this = this;
    let mut buf = buf;
    let mut target = target;
    let mut found_k = None;
    for k in (0..size as usize).rev() {
        let idx = ((head + k as i32) % cap) as usize;
        let elem = ctx.get_array_element(buf, idx);
        let matched = list_element_matches(ctx, &elem, &target)?;
        this = ctx.read_native_pin(this_pin, this);
        buf = ctx.read_native_pin(buf_pin, buf);
        target = read_pinned_elem(ctx, th, target);
        if matched {
            found_k = Some(k);
            break;
        }
    }
    ctx.unpin_native_roots(this_pin);
    if let Some(k) = found_k {
        ad_remove_at_logical(ctx, this, buf, head, size, cap, k);
        return Ok(Some(Value::Int(1)));
    }
    Ok(Some(Value::Int(0)))
}

pub(crate) fn native_ad_poll_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (_, _, _, size) = ad_state(ctx, this);
    if size == 0 {
        return Ok(Some(Value::Object(None)));
    }
    native_ad_remove_first(ctx, args)
}

pub(crate) fn native_ad_poll_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (_, _, _, size) = ad_state(ctx, this);
    if size == 0 {
        return Ok(Some(Value::Object(None)));
    }
    native_ad_remove_last(ctx, args)
}

pub(crate) fn native_ad_get_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, head, _, size) = ad_state(ctx, this);
    if size == 0 {
        return Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "ArrayDeque is empty".to_string(),
            }
            .into(),
        );
    }
    let elem = data.map_or(Value::Object(None), |buf| {
        ctx.get_array_element(buf, head as usize)
    });
    Ok(Some(elem))
}

pub(crate) fn native_ad_get_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, _, tail, size) = ad_state(ctx, this);
    if size == 0 {
        return Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "ArrayDeque is empty".to_string(),
            }
            .into(),
        );
    }
    let cap = data.map_or(0, |d| ctx.array_length(d)) as i32;
    let idx = (tail - 1 + cap) % cap;
    let elem = data.map_or(Value::Object(None), |buf| {
        ctx.get_array_element(buf, idx as usize)
    });
    Ok(Some(elem))
}

pub(crate) fn native_ad_peek_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, head, _, size) = ad_state(ctx, this);
    if size == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let elem = data.map_or(Value::Object(None), |buf| {
        ctx.get_array_element(buf, head as usize)
    });
    Ok(Some(elem))
}

pub(crate) fn native_ad_peek_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, _, tail, size) = ad_state(ctx, this);
    if size == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let cap = data.map_or(0, |d| ctx.array_length(d)) as i32;
    let idx = (tail - 1 + cap) % cap;
    let elem = data.map_or(Value::Object(None), |buf| {
        ctx.get_array_element(buf, idx as usize)
    });
    Ok(Some(elem))
}

pub(crate) fn native_ad_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data, head, _, size) = ad_state(ctx, this);
    if let Some(buf) = data {
        let cap = ctx.array_length(buf);
        // Family-1 fix (cce0079): pinned ring walk — the `equals()` dispatch
        // can move `buf`/`target` mid-scan.
        let buf_pin = ctx.pin_native_root(buf);
        let th = pin_value(ctx, target);
        let mut buf = buf;
        let mut target = target;
        let mut found = false;
        for i in 0..(size as usize) {
            let idx = (head as usize + i) % cap;
            let elem = ctx.get_array_element(buf, idx);
            let matched = list_element_matches(ctx, &elem, &target)?;
            buf = ctx.read_native_pin(buf_pin, buf);
            target = read_pinned_elem(ctx, th, target);
            if matched {
                found = true;
                break;
            }
        }
        ctx.unpin_native_roots(buf_pin);
        if found {
            return Ok(Some(Value::Int(1)));
        }
    }
    Ok(Some(Value::Int(0)))
}

pub(crate) fn native_ad_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Null the live range before resetting the cursors, mirroring the real
    // `clear()`'s `circularClear(elements, head, tail)`. Resetting the three
    // ints alone left every cleared element strongly reachable from the buffer
    // with nothing able to reach it back — `clear()` is the one call a caller
    // makes *specifically* to drop references, so this is the site where the
    // leak is least acceptable and was least visible.
    let (data, head, _tail, size) = ad_state(ctx, this);
    if let Some(buf) = data {
        let cap = ctx.array_length(buf);
        if cap > 0 {
            for i in 0..(size.max(0) as usize).min(cap) {
                let idx = (head as usize + i) % cap;
                ctx.set_array_element(buf, idx, Value::Object(None));
            }
        }
    }
    ctx.set_field(this, AD_FIELD_HEAD, Value::Int(0));
    ctx.set_field(this, AD_FIELD_TAIL, Value::Int(0));
    Ok(None)
}

pub(crate) fn native_ad_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, head, _, size) = ad_state(ctx, this);
    // GC-safety: the allocation can move the ring buffer we copy out of.
    let mut buf_ref = data.unwrap_or(this);
    let arr: ObjectRef = rooted_across(ctx, &mut [&mut buf_ref], |ctx| {
        alloc_ref_array(ctx, size as usize)
    });
    let data = data.map(|_| buf_ref);
    if let Some(buf) = data {
        let cap = ctx.array_length(buf);
        for i in 0..(size as usize) {
            let idx = (head as usize + i) % cap;
            let elem = ctx.get_array_element(buf, idx);
            ctx.set_array_element(arr, i, elem);
        }
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn ad_collect_elements(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<Value> {
    let (data, head, _, size) = ad_state(ctx, this);
    let mut elems = Vec::with_capacity(size as usize);
    if let Some(buf) = data {
        let cap = ctx.array_length(buf);
        for i in 0..(size as usize) {
            let idx = (head as usize + i) % cap;
            elems.push(ctx.get_array_element(buf, idx));
        }
    }
    elems
}

pub(crate) fn native_ad_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(Some(ctx.create_string("[]"))))),
    };
    let elems = ad_collect_elements(ctx, this);
    // gc-common w20-a: rendered through pins (each `toString()` can move the
    // elements not yet rendered).
    let parts = display_units_pinned(ctx, &elems)?;
    let s = wrap_join("[", &parts, ", ", "]");
    Ok(Some(Value::Object(Some(ctx.create_string_from_units(&s)))))
}

pub(crate) fn native_ad_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // RULE F, empty receiver included. MEASURED on `ArrayDeque` and
    // `PriorityQueue` alike. See `reject_null_functional`.
    reject_null_functional(args.get(1))?;
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let elems = ad_collect_elements(ctx, this);
    let consumer_pin = ctx.pin_native_root(consumer);
    let (_, elem_pins) = pin_value_slice(ctx, &elems);
    for (i, e) in elems.iter().enumerate() {
        let consumer = ctx.read_native_pin(consumer_pin, consumer);
        let elem = read_pinned_elem(ctx, elem_pins[i], *e);
        if let Err(e) = ctx.invoke_virtual(consumer, "accept", "(Ljava/lang/Object;)V", &[elem]) {
            ctx.unpin_native_roots(consumer_pin);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(consumer_pin);
    Ok(None)
}

// ===========================================================================
// PriorityQueue (binary min-heap)
// ===========================================================================

pub(crate) const PQ_FIELD_DATA: usize = 0; // Object[] heap array
pub(crate) const PQ_FIELD_SIZE: usize = 1; // Int element count
pub(crate) const PQ_FIELD_COMPARATOR: usize = 2; // Comparator or null
pub(crate) const PQ_DEFAULT_CAPACITY: usize = 11;

pub(crate) fn pq_state(ctx: &dyn NativeContext, this: ObjectRef) -> (Option<ObjectRef>, i32) {
    let data = match ctx.get_field(this, PQ_FIELD_DATA) {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let size = match ctx.get_field(this, PQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    (data, size)
}

pub(crate) fn pq_ensure_capacity(ctx: &mut dyn NativeContext, this: &mut ObjectRef, min_cap: usize) {
    let (data, _size) = pq_state(ctx, *this);
    let old_cap = data.map_or(0, |d| ctx.array_length(d));
    if min_cap <= old_cap {
        return;
    }
    let new_cap = std::cmp::max(old_cap + old_cap / 2 + 1, min_cap);
    // GC-safety: see `rooted_across`.
    //
    // gc-common w10-g: the grow may collect and retry (`GrowAlloc::Reclaim`).
    // `live` audit: `this` and the old buffer are the roots `rooted_across`
    // holds and rewrites. The one caller, `native_pq_add`, pins its receiver
    // and element across this call and re-reads both; its Rust caller,
    // `native_pq_init_from_collection`, pins the receiver and every pending
    // element and re-reads them per element. No other crate calls either.
    let mut old = data.unwrap_or(*this);
    let new_buf = rooted_across(ctx, &mut [this, &mut old], |ctx| {
        new_ref_array_grow(ctx, ClassId::new(0), new_cap, GrowAlloc::Reclaim)
    });
    let data = data.map(|_| old);
    if let Some(old_buf) = data {
        for i in 0..old_cap {
            let val = ctx.get_array_element(old_buf, i);
            ctx.set_array_element(new_buf, i, val);
        }
    }
    ctx.set_field(*this, PQ_FIELD_DATA, Value::Object(Some(new_buf)));
}

/// Compare two elements using comparator or natural ordering (string/int/long comparison).
pub(crate) fn pq_compare(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    a: &Value,
    b: &Value,
) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    let comp = ctx.get_field(this, PQ_FIELD_COMPARATOR);
    if let Value::Object(Some(comparator)) = comp {
        let result = ctx.invoke_virtual(
            comparator,
            "compare",
            "(Ljava/lang/Object;Ljava/lang/Object;)I",
            &[*a, *b],
        )?;
        return Ok(match result {
            Some(Value::Int(v)) => v,
            _ => 0,
        });
    }
    // Natural ordering. PriorityQueue elements are virtually always *boxed*
    // (`pq.add(5)` stores an `Integer`, not a primitive), so dispatch the
    // element's real `Comparable.compareTo` — exactly what HotSpot's
    // `PriorityQueue.siftUpComparable` does. This handles `Integer`/`Long`/
    // `Double`/`String`/... and any user `Comparable` uniformly. (The earlier
    // code only knew `read_string` + raw primitives, so two boxed `Integer`s
    // compared "equal" (0) and the heap never ordered — `5 2 9 3 1`.)
    if let Value::Object(Some(oa)) = a {
        let result = ctx.invoke_virtual(*oa, "compareTo", "(Ljava/lang/Object;)I", &[*b])?;
        return Ok(match result {
            Some(Value::Int(v)) => v,
            _ => 0,
        });
    }
    // Primitive fallback (rare — PQ normally holds boxed objects).
    //
    // Measured UNREACHABLE for `Float`/`Double`: a `PriorityQueue<Double>` boxes,
    // so the `Comparable.compareTo` branch above takes every element and this arm
    // never sees one. `NanSurface2.java` confirms it — `PriorityQueue.drain`,
    // `.bulk.drain`, `.reverse.drain` and `PriorityBlockingQueue.drain` all match
    // HotSpot bit-for-bit with NaNs and signed zeros in the queue.
    //
    // Corrected anyway, because `partial_cmp(..).map_or(0, ..)` is a
    // non-transitive comparator (NaN equal to everything) sitting one refactor
    // away from being reachable, and because leaving one spelling of this rule
    // wrong is how the last five copies survived.
    Ok(match (a, b) {
        (Value::Int(a), Value::Int(b)) => a.cmp(b) as i32,
        (Value::Long(a), Value::Long(b)) => a.cmp(b) as i32,
        (Value::Float(a), Value::Float(b)) => cratonvm_types::jfp::float_compare(*a, *b),
        (Value::Double(a), Value::Double(b)) => cratonvm_types::jfp::double_compare(*a, *b),
        _ => 0,
    })
}

pub(crate) fn pq_sift_up(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    buf: ObjectRef,
    mut idx: usize,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    // Family-1 stale-at-store fix (cce0079): `pq_compare` dispatches the
    // user comparator / `compareTo` (GC-capable) on every iteration —
    // `this`, `buf`, and the two pre-compare element snapshots all go stale
    // across it. Pin the receiver and buffer, refresh them after each
    // dispatch, and RE-READ the two slots from the refreshed buffer before
    // writing the swap back (the pre-compare copies may hold pre-GC
    // addresses, which would poison the queue's backing array).
    let this_pin = ctx.pin_native_root(this);
    let buf_pin = ctx.pin_native_root(buf);
    let mut this = this;
    let mut buf = buf;
    let mut result = Ok(());
    while idx > 0 {
        let parent = (idx - 1) / 2;
        let child_val = ctx.get_array_element(buf, idx);
        let parent_val = ctx.get_array_element(buf, parent);
        let cmp = match pq_compare(ctx, this, &child_val, &parent_val) {
            Ok(c) => c,
            Err(e) => {
                result = Err(e);
                break;
            }
        };
        this = ctx.read_native_pin(this_pin, this);
        buf = ctx.read_native_pin(buf_pin, buf);
        if cmp < 0 {
            let child_val = ctx.get_array_element(buf, idx);
            let parent_val = ctx.get_array_element(buf, parent);
            ctx.set_array_element(buf, idx, parent_val);
            ctx.set_array_element(buf, parent, child_val);
            idx = parent;
        } else {
            break;
        }
    }
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn pq_sift_down(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    buf: ObjectRef,
    mut idx: usize,
    size: usize,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    // Family-1 stale-at-store fix (cce0079): same shape as `pq_sift_up` —
    // refresh `this`/`buf` after every GC-capable `pq_compare` dispatch and
    // re-read the swapped slots from the refreshed buffer.
    let this_pin = ctx.pin_native_root(this);
    let buf_pin = ctx.pin_native_root(buf);
    let mut this = this;
    let mut buf = buf;
    let mut result = Ok(());
    loop {
        let left = 2 * idx + 1;
        if left >= size {
            break;
        }
        let right = left + 1;
        let mut smallest = left;
        if right < size {
            let lv = ctx.get_array_element(buf, left);
            let rv = ctx.get_array_element(buf, right);
            let cmp = match pq_compare(ctx, this, &rv, &lv) {
                Ok(c) => c,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            };
            this = ctx.read_native_pin(this_pin, this);
            buf = ctx.read_native_pin(buf_pin, buf);
            if cmp < 0 {
                smallest = right;
            }
        }
        let cur_val = ctx.get_array_element(buf, idx);
        let small_val = ctx.get_array_element(buf, smallest);
        let cmp = match pq_compare(ctx, this, &small_val, &cur_val) {
            Ok(c) => c,
            Err(e) => {
                result = Err(e);
                break;
            }
        };
        this = ctx.read_native_pin(this_pin, this);
        buf = ctx.read_native_pin(buf_pin, buf);
        if cmp < 0 {
            let cur_val = ctx.get_array_element(buf, idx);
            let small_val = ctx.get_array_element(buf, smallest);
            ctx.set_array_element(buf, idx, small_val);
            ctx.set_array_element(buf, smallest, cur_val);
            idx = smallest;
        } else {
            break;
        }
    }
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn register_priority_queue_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/PriorityQueue";

    r.register(c, "<init>", "()V", native_pq_init);
    r.register(c, "<init>", "(I)V", native_pq_init_capacity);
    r.register(
        c,
        "<init>",
        "(Ljava/util/Comparator;)V",
        native_pq_init_comparator,
    );
    // `(Collection)` — see the note on `java/util/Vector`'s. This one must
    // route through `native_pq_add` rather than copying the source order: a
    // `PriorityQueue` is a heap, and `PriorityQueue(Collection)` on HotSpot
    // heapifies. Copying element order would produce a queue whose `poll()`
    // sequence is the source's, not the comparator's.
    r.register(
        c,
        "<init>",
        "(Ljava/util/Collection;)V",
        native_pq_init_from_collection,
    );
    r.register(c, "size", "()I", native_pq_size);
    r.register(c, "isEmpty", "()Z", native_pq_is_empty);
    r.register(c, "add", "(Ljava/lang/Object;)Z", native_pq_add);
    r.register(c, "offer", "(Ljava/lang/Object;)Z", native_pq_add);
    r.register(c, "peek", "()Ljava/lang/Object;", native_pq_peek);
    r.register(c, "poll", "()Ljava/lang/Object;", native_pq_poll);
    r.register(c, "remove", "(Ljava/lang/Object;)Z", native_pq_remove);
    r.register(c, "contains", "(Ljava/lang/Object;)Z", native_pq_contains);
    r.register(c, "clear", "()V", native_pq_clear);
    r.register(c, "toArray", "()[Ljava/lang/Object;", native_pq_to_array);
    r.register(c, "iterator", "()Ljava/util/Iterator;", native_pq_iterator);
    r.register(c, "toString", "()Ljava/lang/String;", native_pq_to_string);
    r.set_category(__prev_cat);
}

/// `PriorityQueue(Collection)` — `this(); addAll(c);`.
///
/// Routed through `native_pq_add` per element rather than copying the source
/// array, because a `PriorityQueue` is a HEAP: `PriorityQueue(Collection)`
/// heapifies on HotSpot, and a queue built by copying source ORDER would
/// `poll()` in the source's sequence rather than the comparator's. Same GC
/// discipline as `native_ad_init_from_collection`.
/// `obj instanceof <name>`, by class id, using the same idiom the rest of this
/// file does for an assignability question.
pub(crate) fn instance_of_named(ctx: &mut dyn NativeContext, obj: ObjectRef, name: &str) -> bool {
    match ctx.class_id_by_name(name) {
        Some(target) => {
            let cid = ctx.class_id_of_object(obj);
            cid == target || ctx.is_subclass(cid, target)
        }
        None => false,
    }
}

/// The comparator a `PriorityQueue(Collection)` must adopt from its argument,
/// or `None` for a source that imposes none.
///
/// The JDK's constructor branches on `SortedSet` and on `PriorityQueue`, and
/// both carry a comparator that may be null (natural order). A null comparator
/// on a `SortedSet` is therefore NOT the same as "not a SortedSet": both end in
/// a natural-order heap here, so this collapses them, and the distinction would
/// only matter if the two branches differed in some other way. They do not.
///
/// Asked through `invoke_virtual` rather than by reading a field, because the
/// source can be any `SortedSet` -- a `TreeSet`, a `TreeMap` key-set view, a
/// `Collections.synchronizedSortedSet` wrapper -- and only its own
/// `comparator()` knows.
pub(crate) fn sorted_source_comparator(ctx: &mut dyn NativeContext, source: ObjectRef) -> Option<Value> {
    if !instance_of_named(ctx, source, "java/util/SortedSet")
        && !instance_of_named(ctx, source, "java/util/PriorityQueue")
    {
        return None;
    }
    match ctx.invoke_virtual(source, "comparator", "()Ljava/util/Comparator;", &[]) {
        Ok(Some(v @ Value::Object(Some(_)))) => Some(v),
        _ => None,
    }
}

pub(crate) fn native_pq_init_from_collection(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `initElementsFromCollection`: `Object[] es = c.toArray()` (MEASURED,
    // `R15Compat5NullArgMore` row 0; round 15 wave 5 lane compat6).
    reject_null_collection_deref(args.get(1), NULL_C_TO_ARRAY)?;
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let source = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            native_pq_init(ctx, args)?;
            return Ok(None);
        }
    };
    // THE CHECK IS AT RUNTIME, on the object, not on the call site's descriptor.
    // `PriorityQueue(Collection)` opens `if (c instanceof SortedSet<?> ss)` and
    // adopts that set's comparator, so a `TreeSet` reaching this constructor
    // through a `Collection`-typed reference still orders the queue its own way.
    //
    // MEASURED, `apps/probes/PqOptionalShadowSweep` row 34: a reverse-ordered
    // `TreeSet` cast to `Collection` drained `[3, 2, 1]` on HotSpot and
    // `[1, 2, 3]` here -- this constructor built a natural-order heap and threw
    // the comparator away. BOTH MODES, because the sibling `(SortedSet)`
    // overload is not registered at all and runs real bytecode, which is why
    // the row above it (same set, `TreeSet`-typed) always passed: javac picks
    // the more specific overload there, and only the cast reaches this one.
    // gc-common w10-g: `this` and `source` are rooted from here on. The
    // comparator query is a Java call and each init allocates, and both
    // references were bare copies used after them (the init wrote into `this`,
    // the drain walked `source`). Every error path releases the frame.
    let this_pin = ctx.pin_native_root(this);
    let source_pin = ctx.pin_native_root(source);
    // Pure reads, before the comparator query's Java.
    let source_ordered = instance_of_named(ctx, source, "java/util/SortedSet")
        || instance_of_named(ctx, source, "java/util/PriorityQueue");
    let comparator = sorted_source_comparator(ctx, source);
    let this_now = ctx.read_native_pin(this_pin, this);
    let init = match comparator {
        Some(c) => native_pq_init_comparator(ctx, &[Value::Object(Some(this_now)), c]),
        // `native_pq_init` reads only the receiver.
        None => native_pq_init(ctx, &[Value::Object(Some(this_now))]),
    };
    if let Err(e) = init {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }
    let source = ctx.read_native_pin(source_pin, source);
    let elems = match collect_collection_elements_or_real(ctx, source) {
        Ok(elems) => elems,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let (_, handles) = pin_value_slice(ctx, &elems);
    if !source_ordered
        && pq_copy_heapify_enabled()
        && pq_copy_heapifiable(ctx, this_pin, this, &elems, &handles)
    {
        let built = pq_fill_and_heapify(ctx, (this_pin, this), &elems, &handles);
        ctx.unpin_native_roots(this_pin);
        return built.map(|()| None);
    }
    for (i, val) in elems.iter().enumerate() {
        let this = ctx.read_native_pin(this_pin, this);
        let val = read_pinned_elem(ctx, handles[i], *val);
        if let Err(e) = native_pq_add(ctx, &[Value::Object(Some(this)), val]) {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

/// `CRATONVM_COMPAT_PQ_COPY_HEAPIFY` -- default ON, `=0` restores one `offer`
/// per element. Round 15 wave 4 (lane compat5),
/// `r15w4-compat5-review-findings-FIXED-20261001.md` item 1. JDK 25's
/// `PriorityQueue(Collection)` for a source that is neither a `SortedSet` nor a
/// `PriorityQueue` is `initFromCollection(c)`: the `toArray()` elements in
/// order, then `heapify()` -- `siftDown` of every internal node, the last
/// first. N offers build a DIFFERENT (equally valid) heap, and the heap array
/// is what `toString()`, `iterator()`, `toArray()` and `forEach` expose:
/// `new PriorityQueue<>(List.of(5, 4, 3, 2, 1))` is `[1, 2, 3, 5, 4]` on
/// HotSpot and was `[1, 2, 4, 5, 3]`. (An ordered source's array is already a
/// heap, which N in-order offers reproduce, so only this case changes.)
pub(crate) fn pq_copy_heapify_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_PQ_COPY_HEAPIFY")
}

/// Whether [`pq_fill_and_heapify`] may build the queue: at least two elements
/// (one needs no sift), none null, and -- under natural ordering -- every one
/// `Comparable`. A null or a non-`Comparable` element keeps the per-offer path,
/// whose refusal (`NullPointerException` / `ClassCastException`) the natives
/// already raise; the JDK's heapify raises its own from inside `siftDown`.
/// Pure reads.
pub(crate) fn pq_copy_heapifiable(
    ctx: &dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
    elems: &[Value],
    handles: &[usize],
) -> bool {
    if elems.len() < 2 {
        return false;
    }
    let this_now = ctx.read_native_pin(this_pin, this);
    let natural = matches!(
        ctx.get_field(this_now, PQ_FIELD_COMPARATOR),
        Value::Object(None)
    );
    for (i, e) in elems.iter().enumerate() {
        match read_pinned_elem(ctx, handles[i], *e) {
            Value::Object(Some(o)) => {
                if natural && !implements_comparable(ctx, o) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// `initElementsFromCollection` + `heapify()` into the freshly initialised
/// queue `this` (rooted at `this_pin`; the caller releases it): the elements
/// (rooted by `handles`) stored in order, `size` published, then
/// `pq_sift_down` from `(n >>> 1) - 1` down to 0. `this` and its buffer are
/// re-read after every sift, whose comparisons run Java.
pub(crate) fn pq_fill_and_heapify(
    ctx: &mut dyn NativeContext,
    (this_pin, this): (usize, ObjectRef),
    elems: &[Value],
    handles: &[usize],
) -> Result<(), MethodCallFailed> {
    let n = elems.len();
    let mut this_now = ctx.read_native_pin(this_pin, this);
    // Can collect; `this_now` is rewritten by it and every element is pinned.
    pq_ensure_capacity(ctx, &mut this_now, n);
    let this_now = ctx.read_native_pin(this_pin, this);
    let Some(buf) = pq_state(&*ctx, this_now).0 else {
        return Ok(());
    };
    for (i, e) in elems.iter().enumerate() {
        let v = read_pinned_elem(&*ctx, handles[i], *e);
        ctx.set_array_element(buf, i, v);
    }
    ctx.set_field(this_now, PQ_FIELD_SIZE, Value::Int(n as i32));
    let mut k = n / 2;
    while k > 0 {
        k -= 1;
        let this_now = ctx.read_native_pin(this_pin, this);
        let Some(buf) = pq_state(&*ctx, this_now).0 else {
            break;
        };
        pq_sift_down(ctx, this_now, buf, k, n)?;
    }
    Ok(())
}

pub(crate) fn native_pq_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // GC-safety: see `rooted_across`.
    let (this, buf) = rooted_across1(ctx, this, |ctx| alloc_ref_array(ctx, PQ_DEFAULT_CAPACITY));
    ctx.set_field(this, PQ_FIELD_DATA, Value::Object(Some(buf)));
    ctx.set_field(this, PQ_FIELD_SIZE, Value::Int(0));
    ctx.set_field(this, PQ_FIELD_COMPARATOR, Value::Object(None));
    Ok(None)
}

pub(crate) fn native_pq_init_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let cap = match args.get(1) {
        // `PriorityQueue(int)` is `if (initialCapacity < 1) throw new
        // IllegalArgumentException();` — the ONE container in `java.util` where
        // capacity 0 is illegal, so a check copied from `HashMap`'s `< 0`
        // accepts it. Clamping an argument is not validating it: `new
        // PriorityQueue<>(0)` and a legal `new PriorityQueue<>(1)` became
        // indistinguishable. MEASURED (apps/probes/PqOptionalShadowSweep 27-28).
        Some(Value::Int(c)) if *c < 1 => {
            return Err(
                cratonvm_types::error::RuntimeError::IllegalArgumentException {
                    message: format!("{c}"),
                }
                .into(),
            );
        }
        Some(Value::Int(c)) => *c as usize,
        _ => PQ_DEFAULT_CAPACITY,
    };
    // HotSpot throws a catchable OutOfMemoryError for an over-large element
    // array rather than aborting; mirror that instead of the panicking alloc.
    //
    // gc-common w9-d: collect and retry first. Registered-only, and `this` is
    // the only reference held (in `live`).
    let mut live = [this];
    let buf = match try_ref_array_reclaiming(ctx, ClassId::new(0), cap, &mut live) {
        Some(b) => b,
        None => {
            return Err(cratonvm_types::error::RuntimeError::OutOfMemoryError {
                message: "Requested array size exceeds VM limit".to_string(),
            }
            .into());
        }
    };
    let [this] = live;
    ctx.set_field(this, PQ_FIELD_DATA, Value::Object(Some(buf)));
    ctx.set_field(this, PQ_FIELD_SIZE, Value::Int(0));
    ctx.set_field(this, PQ_FIELD_COMPARATOR, Value::Object(None));
    Ok(None)
}

pub(crate) fn native_pq_init_comparator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let comp = args.get(1).copied().unwrap_or(Value::Object(None));
    // GC-safety: `this` and the comparator argument must both survive the
    // allocation below — see `rooted_across`.
    let comp_pin = pin_value(ctx, comp);
    let (this, buf) = rooted_across1(ctx, this, |ctx| alloc_ref_array(ctx, PQ_DEFAULT_CAPACITY));
    let comp = read_pinned_elem(ctx, comp_pin, comp);
    if comp_pin != usize::MAX {
        ctx.unpin_native_roots(comp_pin);
    }
    ctx.set_field(this, PQ_FIELD_DATA, Value::Object(Some(buf)));
    ctx.set_field(this, PQ_FIELD_SIZE, Value::Int(0));
    ctx.set_field(this, PQ_FIELD_COMPARATOR, comp);
    Ok(None)
}

pub(crate) fn native_pq_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (_, size) = pq_state(ctx, this);
    Ok(Some(Value::Int(size)))
}

pub(crate) fn native_pq_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1))),
    };
    let (_, size) = pq_state(ctx, this);
    Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
}

pub(crate) fn native_pq_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    // `java.util.PriorityQueue`: "This queue does not permit null elements",
    // and both funnels declare `@throws NullPointerException if the specified
    // element is null`. `offer` opens with the check and `add` delegates to it,
    // which is why one check here covers the two registrations above.
    //
    // Measured on the binary before this line existed:
    // `PriorityQueue.nullAdd=no-throw` against HotSpot's
    // `java.lang.NullPointerException`. The reason the JDK refuses is the same
    // one `ArrayDeque` has — a null has no place in a comparison heap, and
    // `siftUp` would dereference it on the very next insert — so `ad_refuse_null`
    // is reused rather than duplicated (only its name is `ArrayDeque`-specific).
    // See W7-33-differential-dead-sections R1.
    ad_refuse_null(elem)?;
    // `siftUpComparable` opens `Comparable<? super T> key = (Comparable<? super T>) x;`
    // — the cast is BEFORE the loop, so the FIRST insert into an empty queue
    // with no comparator already throws `ClassCastException`. We accepted it and
    // failed one call later with `NoSuchMethodError` from `compareTo` on a plain
    // Object, which is both the wrong type and the wrong line. MEASURED
    // (apps/probes/PqOptionalShadowSweep 22-23). Only the natural-ordering path
    // casts: a queue WITH a comparator never requires `Comparable`.
    if matches!(
        ctx.get_field(this, PQ_FIELD_COMPARATOR),
        Value::Object(None)
    ) {
        if let Value::Object(Some(e)) = elem {
            if !implements_comparable(ctx, e) {
                let cname = object_class_name_dotted(ctx, e);
                return Err(cratonvm_types::error::RuntimeError::ClassCastException {
                    message: format!("class {cname} cannot be cast to class java.lang.Comparable"),
                }
                .into());
            }
        }
    }
    // Family-1 stale-at-store fix (cce0079): `pq_ensure_capacity`
    // reallocates the heap array on grow (GC-capable) — pin `this`/`elem`
    // across it and refresh both, otherwise the store below writes a pre-GC
    // element address into the fresh buffer.
    let this_pin = ctx.pin_native_root(this);
    let eh = pin_value(ctx, elem);
    let (_, size) = pq_state(ctx, this);
    pq_ensure_capacity(ctx, &mut this, (size + 1) as usize);
    let this = ctx.read_native_pin(this_pin, this);
    let elem = read_pinned_elem(ctx, eh, elem);
    ctx.unpin_native_roots(this_pin);
    let (data, _) = pq_state(ctx, this);
    // `pq_ensure_capacity` allocates a buffer for any `min_cap >= 1`, so this
    // is normally `Some`. Guard against a None backing array (size/data
    // divergence) rather than panicking on `unwrap`.
    let buf = match data {
        Some(b) => b,
        None => return Ok(Some(Value::Int(0))),
    };
    ctx.set_array_element(buf, size as usize, elem);
    ctx.set_field(this, PQ_FIELD_SIZE, Value::Int(size + 1));
    // The JDK's `offer` is `siftUp(i, e); size = i + 1;` -- the comparison that
    // can throw runs BEFORE the size is published, so a refused insert leaves
    // the queue exactly as it was. MEASURED: after a `ClassCastException` from
    // a mixed-type offer, `size()` answered 2 against HotSpot's 1
    // (apps/probes/PqOptionalShadowSweep 25) -- and the rejected element was left in
    // the heap array at an unsorted position, where the next `poll` would find
    // it. Publishing first and sifting after cannot be reordered here (the sift
    // needs the element in place), so the store is UNDONE on the throwing path.
    let this_pin = ctx.pin_native_root(this);
    let sifted = pq_sift_up(ctx, this, buf, size as usize);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if let Err(e) = sifted {
        ctx.set_field(this, PQ_FIELD_SIZE, Value::Int(size));
        let (data, _) = pq_state(ctx, this);
        if let Some(buf) = data {
            ctx.set_array_element(buf, size as usize, Value::Object(None));
        }
        return Err(e);
    }
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_pq_peek(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = pq_state(ctx, this);
    if size == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let elem = data.map_or(Value::Object(None), |buf| ctx.get_array_element(buf, 0));
    Ok(Some(elem))
}

pub(crate) fn native_pq_poll(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = pq_state(ctx, this);
    if size == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let buf = match data {
        Some(b) => b,
        None => return Ok(Some(Value::Object(None))),
    };
    let result = ctx.get_array_element(buf, 0);
    let new_size = size - 1;
    if new_size > 0 {
        let last = ctx.get_array_element(buf, new_size as usize);
        ctx.set_array_element(buf, 0, last);
    }
    ctx.set_field(this, PQ_FIELD_SIZE, Value::Int(new_size));
    if new_size > 0 {
        // Family-1 stale-at-store fix (cce0079): `pq_sift_down` dispatches
        // the comparator (GC-capable) — the popped element must be returned
        // at its CURRENT address, so pin it across the sift and re-read.
        let rh = pin_value(ctx, result);
        let sift = pq_sift_down(ctx, this, buf, 0, new_size as usize);
        let result = read_pinned_elem(ctx, rh, result);
        if rh != usize::MAX {
            ctx.unpin_native_roots(rh);
        }
        sift?;
        return Ok(Some(result));
    }
    Ok(Some(result))
}

pub(crate) fn native_pq_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data, size) = pq_state(ctx, this);
    let buf = match data {
        Some(b) => b,
        None => return Ok(Some(Value::Int(0))),
    };
    // Family-1 stale-at-store fix (cce0079): the find loop dispatches
    // `equals()` per element (GC-capable) — `this`/`buf`/`target` all go
    // stale across it, and the post-loop swap would shift a stale `last`
    // element into a stale array. Pin the receiver, search through the
    // pinned helper, and act on the refreshed addresses it returns.
    let this_pin = ctx.pin_native_root(this);
    let (found_idx, buf, _target) = pinned_array_search(ctx, buf, size as usize, target)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let idx = match found_idx {
        Some(i) => i,
        None => return Ok(Some(Value::Int(0))),
    };
    let new_size = (size - 1) as usize;
    if idx == new_size {
        // Removing last element, no sift needed
        ctx.set_field(this, PQ_FIELD_SIZE, Value::Int(new_size as i32));
        return Ok(Some(Value::Int(1)));
    }
    let last = ctx.get_array_element(buf, new_size);
    ctx.set_array_element(buf, idx, last);
    ctx.set_field(this, PQ_FIELD_SIZE, Value::Int(new_size as i32));
    pq_sift_down(ctx, this, buf, idx, new_size)?;
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_pq_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data, size) = pq_state(ctx, this);
    if let Some(buf) = data {
        // Family-1 fix (cce0079): the naive loop walked a `buf` left stale
        // by the per-element `equals()` dispatch; search via the pinned
        // helper instead.
        let (found, _, _) = pinned_array_search(ctx, buf, size as usize, target)?;
        if found.is_some() {
            return Ok(Some(Value::Int(1)));
        }
    }
    Ok(Some(Value::Int(0)))
}

pub(crate) fn native_pq_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    ctx.set_field(this, PQ_FIELD_SIZE, Value::Int(0));
    Ok(None)
}

pub(crate) fn native_pq_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = pq_state(ctx, this);
    // GC-safety: the allocation can move the heap buffer we copy out of.
    let mut buf_ref = data.unwrap_or(this);
    let arr: ObjectRef = rooted_across(ctx, &mut [&mut buf_ref], |ctx| {
        alloc_ref_array(ctx, size as usize)
    });
    let data = data.map(|_| buf_ref);
    if let Some(buf) = data {
        for i in 0..(size as usize) {
            let elem = ctx.get_array_element(buf, i);
            ctx.set_array_element(arr, i, elem);
        }
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn native_pq_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // The heap-order snapshot, read before anything allocates, then handed to
    // the shared family mint. This used to build its own wrapper and hand back
    // a `java/util/ArrayList$Itr`; HotSpot 25.0.4+7 hands back
    // `java/util/PriorityQueue$Itr`, and the class was the reason this iterator
    // was not fail-fast (`apps/probes/FailFastShapeProbe`).
    //
    // The old comment here said minting `PriorityQueue$Itr` could not work,
    // because `try_alloc_synthetic` honours the real layout and slot 0 is
    // `cursor:int`, so storing the snapshot array there coerced it away. That
    // was true of THAT mint. `alloc_arraylist_iterator_as` puts the three
    // snapshot fields PAST the class's declared ones, which is exactly the
    // mechanism the values views have used since W7-1 -- the objection was to a
    // technique, not to the class.
    let (data, size) = pq_state(ctx, this);
    let n = size.max(0) as usize;
    let mut elems = Vec::with_capacity(n);
    if let Some(buf) = data {
        for i in 0..n {
            elems.push(ctx.get_array_element(buf, i));
        }
    }
    let itr = alloc_family_snapshot_iterator(ctx, this, &elems, "java/util/PriorityQueue$Itr")?;
    Ok(Some(Value::Object(Some(itr))))
}

pub(crate) fn native_pq_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(Some(ctx.create_string("[]"))))),
    };
    let (data, size) = pq_state(ctx, this);
    // gc-common w20-a: the queue array is read back through a pin before each
    // element (each `toString()` runs Java and can move it).
    let mut text = Units::new();
    text.push(u16::from(b'['));
    if let Some(buf) = data {
        push_array_display_units(ctx, &mut text, buf, size.max(0) as usize)?;
    }
    text.push(u16::from(b']'));
    let s = text;
    Ok(Some(Value::Object(Some(ctx.create_string_from_units(&s)))))
}

// ===========================================================================
// Vector (synchronized ArrayList — simplified without real locks)
// ===========================================================================

pub(crate) fn register_vector_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    // RETAGGED (P0 "wholesale Bridge over-tagging"). This is one of the 48
    // registrars that set their OWN category rather than inheriting the
    // ambient one, so a call-site wrapper is inert on it (G85-1) and the line
    // itself has to change.
    //
    // That is not overriding somebody's judgement. This crate's header says
    // the tag "is not a per-registration judgement: it is a dynamic ambient
    // assignment that every callee in this file inherits", and the local
    // save/restore here carries NO rationale — it is the same idiom repeated,
    // not a decision about `java.util.Vector`.
    //
    // MEASURED 2026-08-19 (`--dump-native-registry`, default mode):
    //   registrations 28 | invocations 0 | overwrote 0
    //   real target: 28 CONCRETE BYTECODE, 0 not-declared-here, 0 ACC_NATIVE
    //
    // The cleanest profile in the crate: every row shadows a complete real
    // implementation, nothing is abstract, nothing depends on these winning,
    // and nothing invoked them. Rule 4 with no residue.
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    let c = "java/util/Vector";

    r.register(c, "<init>", "()V", native_al_init);
    r.register(c, "<init>", "(I)V", native_al_init_capacity);
    // `(Collection)` — the copy constructor. Absent until 2026-09-02, so
    // `new Vector<>(someList)` raised `NoSuchMethodError` in `--synthetic-jdk`
    // (real-JDK mode runs the class's own bytecode and never reaches here).
    // Bound to the shared ArrayList implementation like every other method on
    // this class: `al_slots_for` resolves `elementData`/`elementCount` against
    // the RECEIVER's layout, which is the whole reason Vector can share these.
    r.register(
        c,
        "<init>",
        "(Ljava/util/Collection;)V",
        native_al_init_from_collection,
    );
    r.register(c, "size", "()I", native_al_size);
    r.register(c, "isEmpty", "()Z", native_al_is_empty);
    r.register(c, "get", "(I)Ljava/lang/Object;", native_vec_get);
    r.register(
        c,
        "elementAt",
        "(I)Ljava/lang/Object;",
        native_vec_element_at,
    );
    r.register(
        c,
        "set",
        "(ILjava/lang/Object;)Ljava/lang/Object;",
        native_vec_set,
    );
    r.register(
        c,
        "setElementAt",
        "(Ljava/lang/Object;I)V",
        native_vec_set_element_at,
    );
    r.register(c, "add", "(Ljava/lang/Object;)Z", native_al_add_entry);
    r.register(
        c,
        "addElement",
        "(Ljava/lang/Object;)V",
        native_vec_add_element,
    );
    r.register(c, "add", "(ILjava/lang/Object;)V", native_vec_add_at);
    r.register(
        c,
        "insertElementAt",
        "(Ljava/lang/Object;I)V",
        native_vec_insert_element_at,
    );
    r.register(c, "remove", "(I)Ljava/lang/Object;", native_vec_remove_at);
    r.register(c, "remove", "(Ljava/lang/Object;)Z", native_al_remove_obj);
    r.register(
        c,
        "removeElement",
        "(Ljava/lang/Object;)Z",
        native_al_remove_obj,
    );
    r.register(
        c,
        "removeElementAt",
        "(I)V",
        native_vec_remove_element_at_checked,
    );
    r.register(c, "removeAllElements", "()V", native_al_clear);
    r.register(c, "clear", "()V", native_al_clear);
    r.register(c, "contains", "(Ljava/lang/Object;)Z", native_al_contains);
    r.register(c, "indexOf", "(Ljava/lang/Object;)I", native_al_index_of);
    r.register(
        c,
        "lastIndexOf",
        "(Ljava/lang/Object;)I",
        native_al_last_index_of,
    );
    r.register(
        c,
        "firstElement",
        "()Ljava/lang/Object;",
        native_vec_first_element,
    );
    r.register(
        c,
        "lastElement",
        "()Ljava/lang/Object;",
        native_vec_last_element,
    );
    r.register(c, "capacity", "()I", native_vec_capacity);
    r.register(
        c,
        "toArray",
        "()[Ljava/lang/Object;",
        native_al_to_array_entry,
    );
    r.register(c, "iterator", "()Ljava/util/Iterator;", native_al_iterator);
    r.register(c, "toString", "()Ljava/lang/String;", native_al_to_string);
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        native_al_for_each,
    );
    r.set_category(__prev_cat);
}

pub(crate) fn native_vec_set_element_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // setElementAt(Object obj, int index) — reversed param order from set()
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let idx = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(None),
    };
    let (data, size) = al_state(ctx, this);
    // MEASURED 2026-08-13 (scratchpad/orch/V3.java): setElementAt words its
    // two arms like elementAt, NOT like set -- "3 >= 3" past the end and the
    // backing-array capacity text for a negative index.
    if idx < 0 {
        return Err(vec_negative_index(ctx, this, idx));
    }
    if idx >= size {
        return Err(vec_aioobe(idx, format!("{idx} >= {size}")));
    }
    let idx = idx as usize;
    if let Some(buf) = data {
        ctx.set_array_element(buf, idx, elem);
    }
    Ok(None)
}

pub(crate) fn native_vec_add_element(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_al_add(ctx, args)?;
    Ok(None)
}

pub(crate) fn native_vec_insert_element_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // insertElementAt(Object obj, int index) — reversed param order from add(int, Object)
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let idx = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(None),
    };
    // insertElementAt shares add's two arms ("4 > 3", and arraycopy text for
    // a negative index) -- measured, and `i == size` is legal for both.
    // Repack as add(int, Object) and route through the CHECKED Vector form:
    native_vec_add_at(ctx, &[Value::Object(Some(this)), Value::Int(idx), elem])
}

pub(crate) fn native_vec_remove_element_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_al_remove_at(ctx, args)?;
    Ok(None)
}

pub(crate) fn native_vec_first_element(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = al_state(ctx, this);
    if size == 0 {
        return Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                // MEASURED: HotSpot throws with NO message here.
                message: String::new(),
            }
            .into(),
        );
    }
    let elem = data.map_or(Value::Object(None), |buf| ctx.get_array_element(buf, 0));
    Ok(Some(elem))
}

pub(crate) fn native_vec_last_element(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = al_state(ctx, this);
    if size == 0 {
        return Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                // MEASURED: HotSpot throws with NO message here.
                message: String::new(),
            }
            .into(),
        );
    }
    let elem = data.map_or(Value::Object(None), |buf| {
        ctx.get_array_element(buf, (size - 1) as usize)
    });
    Ok(Some(elem))
}

pub(crate) fn native_vec_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (data, _) = al_state(ctx, this);
    let cap = data.map_or(0, |d| ctx.array_length(d));
    Ok(Some(Value::Int(cap as i32)))
}

// ===========================================================================
// Stack (extends Vector with push/pop/peek/search/empty)
// ===========================================================================

pub(crate) fn register_stack_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/Stack";

    // Inherit all Vector methods
    r.register(c, "<init>", "()V", native_al_init);
    r.register(c, "size", "()I", native_al_size);
    r.register(c, "isEmpty", "()Z", native_al_is_empty);
    r.register(c, "get", "(I)Ljava/lang/Object;", native_al_get);
    r.register(
        c,
        "set",
        "(ILjava/lang/Object;)Ljava/lang/Object;",
        native_al_set,
    );
    r.register(c, "add", "(Ljava/lang/Object;)Z", native_al_add_entry);
    r.register(c, "remove", "(I)Ljava/lang/Object;", native_al_remove_at);
    r.register(c, "clear", "()V", native_al_clear);
    r.register(c, "contains", "(Ljava/lang/Object;)Z", native_al_contains);
    r.register(c, "indexOf", "(Ljava/lang/Object;)I", native_al_index_of);
    r.register(
        c,
        "toArray",
        "()[Ljava/lang/Object;",
        native_al_to_array_entry,
    );
    r.register(c, "iterator", "()Ljava/util/Iterator;", native_al_iterator);
    r.register(c, "toString", "()Ljava/lang/String;", native_al_to_string);

    // Stack-specific methods
    r.register(
        c,
        "push",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_stack_push,
    );
    r.register(c, "pop", "()Ljava/lang/Object;", native_stack_pop);
    r.register(c, "peek", "()Ljava/lang/Object;", native_stack_peek);
    r.register(c, "empty", "()Z", native_al_is_empty);
    r.register(c, "search", "(Ljava/lang/Object;)I", native_stack_search);
    r.set_category(__prev_cat);
}

pub(crate) fn native_stack_push(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    native_al_add(ctx, args)?;
    Ok(Some(elem))
}

pub(crate) fn native_stack_pop(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (_, size) = al_state(ctx, this);
    if size == 0 {
        // `java.util.Stack.pop`: "@throws EmptyStackException if this stack is
        // empty". That class extends `RuntimeException` DIRECTLY, so the
        // `NoSuchElementException` this used to raise is not a supertype of it
        // and a `catch (EmptyStackException)` in application code never fired —
        // a mistyped refusal sends the caller down the wrong branch, which is
        // worse than no refusal because it looks handled. Measured:
        // `Stack.popOnEmpty=java.util.NoSuchElementException` against HotSpot's
        // `java.util.EmptyStackException`. See W7-33-differential-dead-sections R2.
        return Err(cratonvm_types::error::RuntimeError::EmptyStackException.into());
    }
    native_al_remove_at(ctx, &[Value::Object(Some(this)), Value::Int(size - 1)])
}

pub(crate) fn native_stack_peek(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = al_state(ctx, this);
    if size == 0 {
        // Same contract as `pop` above: `java.util.Stack.peek` declares
        // `@throws EmptyStackException if this stack is empty`. Not measured by
        // the probe — it only asks `pop` — but the two are one refusal in the
        // JDK (`peek` is what `pop` calls first) and leaving one behind is how
        // the pair drifts apart again.
        return Err(cratonvm_types::error::RuntimeError::EmptyStackException.into());
    }
    let elem = data.map_or(Value::Object(None), |buf| {
        ctx.get_array_element(buf, (size - 1) as usize)
    });
    Ok(Some(elem))
}

pub(crate) fn native_stack_search(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data, size) = al_state(ctx, this);
    // Search from top of stack (last element), return 1-based distance from top
    if let Some(buf) = data {
        // Family-1 fix (cce0079): pinned reverse scan.
        let buf_pin = ctx.pin_native_root(buf);
        let th = pin_value(ctx, target);
        let mut buf = buf;
        let mut target = target;
        let mut found = None;
        for i in (0..(size as usize)).rev() {
            let elem = ctx.get_array_element(buf, i);
            let matched = list_element_matches(ctx, &elem, &target)?;
            buf = ctx.read_native_pin(buf_pin, buf);
            target = read_pinned_elem(ctx, th, target);
            if matched {
                found = Some(i);
                break;
            }
        }
        ctx.unpin_native_roots(buf_pin);
        if let Some(i) = found {
            return Ok(Some(Value::Int((size as usize - i) as i32)));
        }
    }
    Ok(Some(Value::Int(-1)))
}

// ===========================================================================
// Collection bulk operations (addAll, removeAll, retainAll)
// ===========================================================================

/// CLUSTER NOTE (H4-1, 2026-08-20). Eight registrations over FIVE containers —
/// `ArrayList`, `HashSet`, `LinkedList`, `Vector`, `ArrayDeque`. Only the three
/// `HashSet` rows belong to the map/set ownership cluster; the other five
/// belong to containers nothing proposes to move. A whole-registrar retag
/// refuses `ArrayList.removeAll` while `register_arraylist_natives` still owns
/// the elements, which is the partial retag `G88-1` §5 warns about, one level
/// down from where it looked for it.
pub(crate) fn register_bulk_ops_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // ArrayList
    r.register(
        "java/util/ArrayList",
        "removeAll",
        "(Ljava/util/Collection;)Z",
        native_al_remove_all,
    );
    r.register(
        "java/util/ArrayList",
        "retainAll",
        "(Ljava/util/Collection;)Z",
        native_al_retain_all,
    );

    // HashSet
    r.register(
        "java/util/HashSet",
        "addAll",
        "(Ljava/util/Collection;)Z",
        native_hs_add_all,
    );
    r.register(
        "java/util/HashSet",
        "removeAll",
        "(Ljava/util/Collection;)Z",
        native_hs_remove_all,
    );
    r.register(
        "java/util/HashSet",
        "retainAll",
        "(Ljava/util/Collection;)Z",
        native_hs_retain_all,
    );

    // LinkedList
    r.register(
        "java/util/LinkedList",
        "addAll",
        "(Ljava/util/Collection;)Z",
        native_ll_add_all,
    );

    // Vector
    r.register(
        "java/util/Vector",
        "addAll",
        "(Ljava/util/Collection;)Z",
        native_al_add_all,
    );

    // ArrayDeque
    r.register(
        "java/util/ArrayDeque",
        "addAll",
        "(Ljava/util/Collection;)Z",
        native_ad_add_all,
    );
    // MEASURED (apps/probes/DequeListShadowSweep 86/88/90): unregistered, all three
    // ran real `ArrayDeque` bytecode, which maintains `elements`/`head`/`tail`
    // and knows nothing about this VM's FOURTH, synthetic `size` slot. The real
    // body compacted correctly and `size` stayed at the pre-removal count, so
    // `[a,b,c,d].removeIf(==b)` read back as `[a, c, d, null]`. Same door
    // `native_ad_remove_first_occurrence` was registered to close for
    // `remove(Object)`.
    r.register(
        "java/util/ArrayDeque",
        "removeAll",
        "(Ljava/util/Collection;)Z",
        native_ad_remove_all,
    );
    r.register(
        "java/util/ArrayDeque",
        "retainAll",
        "(Ljava/util/Collection;)Z",
        native_ad_retain_all,
    );
    r.register(
        "java/util/ArrayDeque",
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
        native_ad_remove_if,
    );
    r.set_category(__prev_cat);
}

/// Collect a collection's elements, falling back to its real `toArray()`
/// bytecode when the layout heuristics in [`collect_collection_elements`] do
/// not recognise it.
///
/// In the default build the JDK collection classes run real bytecode rather
/// than a synthetic array backing, so a real `ConcurrentLinkedQueue` /
/// `LinkedList` stores its elements in `head`/`tail` Node chains that the
/// field-layout heuristics can't read — `collect_collection_elements` returns
/// empty. The collection's own `size()`/`toArray()` *do* work (real bytecode),
/// so when the heuristics come up empty we ask the collection directly. This
/// is what makes `new ArrayList<>(concurrentLinkedQueue)` /
/// `new HashSet<>(linkedList)` / `addAll(realCollection)` copy the real
/// elements — and, in particular, fixes JUnit 4.13's `Description.getChildren()`
/// (`new ArrayList<>(fChildren)` over a `ConcurrentLinkedQueue` of test
/// methods), which the vintage engine relies on for discovery.
///
/// Driving `toArray()` (not `iterator()`) keeps this recursion-safe: the only
/// native `toArray()` can reach is `native_al_to_array`, which since
/// 2026-08-01 collects through the *other* wrapper, `al_or_collection_elements`
/// — never through this one — so there is still no cycle, only one more
/// virtual call on the deepest path (`toArray()` → `iterator()`). The
/// `size() > 0` guard means a genuinely empty (or unrecognised-and-empty)
/// collection never triggers the extra virtual calls.
///
/// Callers: the copy constructors, `addAll`, `removeAll`, `retainAll`,
/// `containsAll` and `AbstractSet.hashCode`. All read a collection passed as an
/// ARGUMENT, never the receiver of an element-reading native, which is why
/// driving the argument's own `toArray()` from here cannot re-enter.
/// How many times [`collect_collection_elements_or_real`] found its receiver had
/// MOVED across one of its own Java re-entries, and the trace that names them.
///
/// The counter is the engagement evidence for the pinning below. Without it the
/// fix is only "the symptom went away", which a timing-dependent defect can fake
/// on any given run — see [`coll_refresh`]'s call sites.
pub(crate) static COLL_REFRESH_MOVED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[inline]
pub(crate) fn dbg_coll_refresh() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_COLL_REFRESH").is_some())
}

/// One turn of [`collect_indexed_pinned`].
pub(crate) enum IndexedStep {
    /// Keep this value and go on.
    Push(Value),
    /// Keep nothing for this index and go on.
    Skip,
    /// Keep nothing and end the walk.
    Stop,
}

/// Collect the values `step(ctx, 0..n)` answers, where every step is a Java
/// call (`get(i)`, `getName(i)`), keeping each collected reference pinned
/// until the walk is over and returning them read back through their pins.
///
/// gc-common w20-a: the indexed walks in [`collect_collection_elements_pinned`]
/// pushed each answer into a bare `Vec` and then ran the next Java call, so
/// every element but the last came back at its pre-collection address
/// (`Path.getName(i)` allocates on every call). The pins are released before
/// returning; `step` re-reads its own receiver (see [`coll_refresh`]).
pub(crate) fn collect_indexed_pinned(
    ctx: &mut dyn NativeContext,
    n: usize,
    mut step: impl FnMut(&mut dyn NativeContext, usize) -> IndexedStep,
) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(n);
    let mut pins: Vec<usize> = Vec::with_capacity(n);
    let mut base = usize::MAX;
    for i in 0..n {
        match step(ctx, i) {
            IndexedStep::Push(v) => {
                let h = pin_value(ctx, v);
                if base == usize::MAX {
                    base = h;
                }
                out.push(v);
                pins.push(h);
            }
            IndexedStep::Skip => {}
            IndexedStep::Stop => break,
        }
    }
    let out = out
        .iter()
        .zip(pins.iter())
        .map(|(&v, &h)| read_pinned_elem(ctx, h, v))
        .collect();
    if base != usize::MAX {
        ctx.unpin_native_roots(base);
    }
    out
}

/// Re-read a pinned receiver after a GC point, counting the moves.
#[inline]
pub(crate) fn coll_refresh(
    ctx: &mut dyn NativeContext,
    pin: usize,
    stale: ObjectRef,
    site: &'static str,
) -> ObjectRef {
    let cur = ctx.read_native_pin(pin, stale);
    if !std::ptr::eq(cur.as_ptr(), stale.as_ptr()) {
        COLL_REFRESH_MOVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if dbg_coll_refresh() {
            eprintln!(
                "[COLL-REFRESH] {site}: 0x{:x} -> 0x{:x} (total {})",
                stale.as_ptr() as usize,
                cur.as_ptr() as usize,
                COLL_REFRESH_MOVED.load(std::sync::atomic::Ordering::Relaxed),
            );
        }
    }
    cur
}

/// How many receiver moves this helper has absorbed. Zero on a run means the
/// pinning was never load-bearing THERE, not that it is unnecessary.
pub fn collection_refresh_moved_count() -> usize {
    COLL_REFRESH_MOVED.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn collect_collection_elements_or_real(
    ctx: &mut dyn NativeContext,
    coll: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    // GC-SAFETY (Family 1 — the stale-`ObjectRef` shape this file has paid for
    // repeatedly). Every call below re-enters Java and is therefore a GC point,
    // and `coll` arrives as a bare Rust local that nothing roots. A collector
    // that evacuates `coll`'s region between two of them leaves the next one
    // dispatching against an address that no longer holds the object.
    //
    // MEASURED on `org.h2.test.store.TestRandomMapOps`, `-XX:+UseG1GC --Xmx 1g`:
    // the `size()` below returned, G1 evacuated and RECYCLED the region `coll`
    // lived in, and the `toArray()` on the next line dispatched against that
    // region's base address. An all-zero header reads as `ClassId(0)`, which IS
    // `java.lang.Object` — the first class this VM loads — so the failure
    // surfaced as
    //   `NoSuchMethodError: 'java.lang.Object[] java.lang.Object.toArray()'`
    // from `new ArrayList<>(map.keySet())`, naming a class that was never
    // involved. Three controls place it: it reproduces under `--nojit` (so it is
    // NOT a lost JIT root, which is where the "wrong class" reading sends you),
    // `--Xmx 8g` does not reach it (so it needs a collection), and ZGC and
    // generational do not reach it either (so it needs an EVACUATING one).
    //
    // `report_reclaimed_receiver` stayed silent the whole time, and that is not
    // a contradiction: it asks the FREE LIST, and a whole evacuated G1 region is
    // not a free-list block. A quiet reclaim guard is not a clean one.
    let coll_pin = ctx.pin_native_root(coll);
    let r = collect_collection_elements_or_real_pinned(ctx, coll, coll_pin);
    ctx.unpin_native_roots(coll_pin);
    r
}

/// [`collect_collection_elements_or_real`]'s body, with the receiver already
/// pinned so every return path unwinds through one `unpin` in the caller —
/// strictly LIFO, the discipline `collection_elements_generic` documents.
pub(crate) fn collect_collection_elements_or_real_pinned(
    ctx: &mut dyn NativeContext,
    coll: ObjectRef,
    coll_pin: usize,
) -> Result<Vec<Value>, MethodCallFailed> {
    let elems = collect_collection_elements(ctx, coll)?;
    let coll = coll_refresh(ctx, coll_pin, coll, "after collect_collection_elements");
    if !elems.is_empty() {
        if heuristic_snapshot_is_suspect(ctx, coll, &elems) {
            // A plausible-looking but null-holed snapshot of a foreign
            // non-List collection — see `heuristic_snapshot_is_suspect`. Every
            // caller of this helper (addAll / removeAll / retainAll /
            // containsAll / hashCode / copy ctors) is off the iterator
            // native's path, so driving the real `iterator()` is safe.
            // See `al_or_collection_elements_pinned`: the snapshot in hand has
            // just been declared untrustworthy, so a refused re-derivation
            // must not be answered with it.
            //
            // gc-common w20-a: the real walk runs Java, and an empty answer
            // falls back to `elems` — which were returned at their pre-walk
            // addresses. Pinned across it and read back.
            let (base, handles) = pin_value_slice(ctx, &elems);
            let real = collect_via_real_iterator_once(ctx, coll);
            let elems = read_value_slice(ctx, &handles, &elems);
            if base != usize::MAX {
                ctx.unpin_native_roots(base);
            }
            let real = real?;
            if !real.is_empty() {
                return Ok(real);
            }
            return Ok(elems);
        }
        return Ok(elems);
    }
    // A refused `size()`/`toArray()` is not an empty collection. Both used to
    // fall through to the (empty) `elems`, so every copy constructor, `addAll`,
    // `removeAll`, `retainAll` and `containsAll` over a strict-refused argument
    // silently saw zero elements — `addAll` in particular then reports `false`
    // ("collection unchanged"), which reads as an ordinary no-op.
    let real_size = match ctx.invoke_virtual(coll, "size", "()I", &[]) {
        Ok(Some(Value::Int(n))) => n,
        Err(e) => return Err(e),
        _ => 0,
    };
    if real_size <= 0 {
        return Ok(elems);
    }
    // THE line this function's doc block is about: `size()` above is a GC point,
    // so the `coll` this `toArray()` dispatches on has to be re-read.
    let coll = coll_refresh(ctx, coll_pin, coll, "between size() and toArray()");
    let arr: ObjectRef = match ctx.invoke_virtual(coll, "toArray", "()[Ljava/lang/Object;", &[]) {
        Ok(Some(Value::Object(Some(a)))) if ctx.heap_kind_of(a) == ObjectKind::Array => a,
        Err(e) => return Err(e),
        _ => return Ok(elems),
    };
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(ctx.get_array_element(arr, i));
    }
    Ok(out)
}

/// `CRATONVM_COMPAT_SETN_ITERATION_ORDER` -- default ON, `=0` restores the
/// table order. See [`setn_iteration_order`].
pub(crate) fn setn_iteration_order_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_SETN_ITERATION_ORDER")
}

/// The elements of a REAL `ImmutableCollections$SetN` whose open-addressed
/// table is `arr` (`len` slots), in the order its own iterator hands them out
/// -- which is also its `toArray()` order (JDK 25 `SetN.toArray` walks
/// `iterator()`). `SetNIterator` starts at `(SALT32L * len) >>> 32` and steps
/// forward when `REVERSE`, backward otherwise, wrapping and skipping empty
/// slots, BEFORE it reads. [`collect_collection_elements`] read the table
/// front to back, so `new ArrayList<>(s)` / `addAll(s)` / `toArray` over a
/// real `SetN` disagreed with a for-each over the same set in the same run
/// (on HotSpot every one of them is the iterator's order). Real `SetN`s reach
/// the natives under `--compatible` from JDK bytecode that builds one itself,
/// and from every non-empty `Set.of` once CW2-1 mints them
/// ([`jdk_mint_immutable`]).
///
/// `None` -- the caller keeps the table order -- with the switch off, or when
/// `ImmutableCollections` has not finished its `<clinit>` (its `EMPTY_SET` is
/// still null, so `SALT32L` / `REVERSE` may not be set yet). Pure reads.
/// Round 15 wave 3 (lane compat2).
pub(crate) fn setn_iteration_order(ctx: &dyn NativeContext, arr: ObjectRef, len: usize) -> Option<Vec<Value>> {
    if !setn_iteration_order_enabled() {
        return None;
    }
    let cid = ctx.class_id_by_name("java/util/ImmutableCollections")?;
    let read = |name: &str| {
        ctx.static_field_index_by_name(cid, name)
            .map(|idx| ctx.get_static_field(cid, idx))
    };
    if !matches!(read("EMPTY_SET")?, Value::Object(Some(_))) {
        return None;
    }
    let salt = match read("SALT32L")? {
        Value::Long(v) => v,
        _ => return None,
    };
    let reverse = match read("REVERSE")? {
        Value::Int(v) => v != 0,
        _ => return None,
    };
    let present = (0..len)
        .filter(|&i| !matches!(ctx.get_array_element(arr, i), Value::Object(None)))
        .count();
    let mut out = Vec::with_capacity(present);
    if present == 0 {
        return Some(out);
    }
    // `(int) ((SALT32L * elements.length) >>> 32)`: `SALT32L` is below 2^32
    // and `len` below 2^31, so the product is non-negative and the index
    // below `len`.
    let mut idx = ((salt.wrapping_mul(len as i64) as u64) >> 32) as usize;
    if idx >= len {
        return None;
    }
    while out.len() < present {
        loop {
            if reverse {
                idx += 1;
                if idx >= len {
                    idx = 0;
                }
            } else if idx == 0 {
                idx = len - 1;
            } else {
                idx -= 1;
            }
            let v = ctx.get_array_element(arr, idx);
            if !matches!(v, Value::Object(None)) {
                out.push(v);
                break;
            }
        }
    }
    Some(out)
}

/// Collect elements from a Collection (ArrayList, HashSet, LinkedList, etc.)
///
/// GC-SAFETY: same obligation as [`collect_collection_elements_or_real`], and
/// for the same reason — four of the branches below drive the receiver's own
/// Java (`size()` then `get(i)`, `getNameCount()` then `getName(i)`, `size()`
/// then `toArray()`), and each of those calls is a GC point that can evacuate
/// the receiver out from under the next one. `coll` is pinned here so those
/// branches can re-read it; the branches that only read fields need nothing,
/// because a field read is not a GC point.
pub(crate) fn collect_collection_elements(
    ctx: &mut dyn NativeContext,
    coll: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    let coll_pin = ctx.pin_native_root(coll);
    let r = collect_collection_elements_pinned(ctx, coll, coll_pin);
    ctx.unpin_native_roots(coll_pin);
    r
}

pub(crate) fn collect_collection_elements_pinned(
    ctx: &mut dyn NativeContext,
    coll: ObjectRef,
    coll_pin: usize,
) -> Result<Vec<Value>, MethodCallFailed> {
    // Round 49 fix: Collections$UnmodifiableCollection / $UnmodifiableList
    // wrap their backing collection in field `c`.  Recurse into that to
    // surface the wrapped list's elements — without this, callers like
    // `new LinkedHashSet(unmodifiableList)` (Spring's
    // `AutoConfigurationImportSelector.removeDuplicates`) silently see
    // zero elements and the auto-configuration pipeline collapses.
    let cid = ctx.class_id_of_object(coll);
    if let Some(cls_name) = ctx.class_name_of_id(cid) {
        // `ConcurrentHashMap.newKeySet()` / `keySet(V)`: the elements are the
        // backing CHM's keys, held in the segmented native layout. None of the
        // slot heuristics below model that — slot 0 is the map object, not a
        // bucket array — so `new HashSet<>(view)` / `addAll(view)` /
        // `containsAll(view)` would silently see nothing.
        if cls_name == KSV_CLASS {
            return Ok(ksv_collect(ctx, coll));
        }
        // A [`SET_VIEW_CARRIERS`] object THE IMAGE'S OWN BYTECODE minted. This
        // is the receiver shape that appears the moment a map family's view
        // accessors stop being native -- the dial armed on that family, or the
        // family retired -- and the layout probes below cannot see a thing in
        // it, because the only field it has is `this$0`.
        //
        // MEASURED 2026-09-12, `--jdk-only`, one family armed per process on
        // `cratonvm-l1w7-base-20260912` (`apps/probes/L1MapViewToArrayProbe`):
        // `entrySet().toArray()` answered `[0]` for ALL FOUR map families where
        // HotSpot answers `[3]`, while `size()`, `iterator()`, the for-each
        // walk, `stream().count()` and `toString()` over the very same view
        // were already right. The elements were never missing -- the map's
        // `table` holds real `HashMap$Node`/`Hashtable$Entry` nodes and its
        // `count`/`size` field is written -- so this is a decode gap, not the
        // state-model gap the lane page recorded for these families.
        //
        // The KIND has to come from the class, not from the head element: a
        // foreign carrier has no elements to look at, and
        // [`al_view_holds_entries_by_head`] answers "values" for an empty view.
        // Placed BEFORE the HashSet-shaped branch further down, which reaches
        // the same receiver through [`hs_backing_map`] and would hand back the
        // source map's KEYS for an entry set.
        if is_set_view_carrier(&cls_name) && values_view_source(ctx, coll).is_none() {
            if let Some(source) = values_view_class_source(ctx, coll) {
                // `new LinkedKeySet(true)` minted by the image: tail first
                // (C3-1). Asked before anything allocates.
                let reversed = linked_set_view_reversed(&*ctx, coll);
                let source_pin = ctx.pin_native_root(source);
                let pairs = match collect_entries_any(ctx, source) {
                    Ok(p) => p,
                    Err(e) => {
                        ctx.unpin_native_roots(source_pin);
                        return Err(e);
                    }
                };
                let out = if set_view_carrier_holds_entries(&cls_name) {
                    let source = ctx.read_native_pin(source_pin, source);
                    match live_entries_for_pairs(ctx, source_pin, source, &pairs) {
                        Ok(v) => v,
                        Err(e) => {
                            ctx.unpin_native_roots(source_pin);
                            return Err(e);
                        }
                    }
                } else {
                    pairs.into_iter().map(|(k, _)| k).collect()
                };
                ctx.unpin_native_roots(source_pin);
                return Ok(in_view_order(out, reversed));
            }
        }
        // A `java/util/TreeMap$KeySet` THE IMAGE'S OWN BYTECODE minted -- the
        // TreeSet-shaped twin of the arm above, and it needs an arm of its own
        // for the reason [`TM_KEY_SET_CARRIER`] gives: the class is in NEITHER
        // carrier list, so neither `is_set_view_carrier` nor the values
        // predicate sees it, and every layout probe below reads
        // `ts_array_table`, which holds nothing for a view this crate did not
        // mint.
        //
        // The guard is the side table itself: a carrier this crate minted has
        // its elements there (`native_tm_key_set` writes them and
        // `resync_ts_view` keeps them live), so this arm fires for the foreign
        // shape and for nothing else.
        //
        // MEASURED, wave 8, `--jdk-only`, dial armed on `java/util/TreeMap`,
        // `apps/probes/L1MapViewToArrayProbe`: with [`ts_view_source`]'s
        // by-name arm in place but WITHOUT this one, the generic `toArray`
        // fallback still answered EMPTY -- `[TMVIEW] al_or_collection_elements
        // fallback real_size=3 walked=0`. The size question was answered
        // correctly and the walk was not, because the walk drives
        // `iterator()`, whose result is minted under
        // `java/util/TreeMap$KeyIterator` -- a class the same armed scope
        // covers, so its own `hasNext` is declined and the real JDK body reads
        // a `next` field this VM never wrote. Decoding the source map directly
        // needs neither the iterator nor its carrier.
        if cls_name == TM_KEY_SET_CARRIER
            && matches!(ts_get_slot(&*ctx, coll, TS_FIELD_DATA), Value::Object(None))
        {
            if let Some(source) = ts_view_source(&*ctx, coll) {
                if is_tree_map_receiver(ctx, source) {
                    let source_pin = ctx.pin_native_root(source);
                    let keys: Vec<Value> = tm_collect_pairs(ctx, source)
                        .into_iter()
                        .map(|(k, _)| k)
                        .collect();
                    ctx.unpin_native_roots(source_pin);
                    return Ok(keys);
                }
            }
        }
        if cls_name == "java/util/EnumSet" {
            if let Value::Object(Some(backing)) = ctx.get_field(coll, 0) {
                return Ok(collect_collection_elements(ctx, backing)?);
            }
            return Ok(Vec::new());
        }
        // CratonVM's own unmodifiable-view wrappers store the backing
        // collection at slot 0 — recurse into it so `new HashSet(unmodList)`
        // and friends see the wrapped elements.
        if cls_name == UNMOD_LIST_CLASS
            || is_unmod_set_class(&cls_name)
            || cls_name == UNMOD_COLLECTION_CLASS
        {
            if let Value::Object(Some(inner)) = ctx.get_field(coll, UNMOD_FIELD_BACKING) {
                return Ok(collect_collection_elements(ctx, inner)?);
            }
        }
        // `cratonvm/internal/ArrayListSubList` (the backed view returned by
        // ArrayList.subList) has layout (parent, offset, size, expected) — NOT
        // (elementData, size). The generic field probes below misread it as a
        // 2-element array → `[null, <garbage>]`, dropping every element of
        // `new ArrayList<>(list.subList(..))` / `addAll(subList)` and crashing
        // downstream when the garbage ref is dispatched (kcfull #17a: keycloak
        // LDAPDn.getParentDn → LdapName.getPrefix/toString SIGSEGV; also a wild
        // call through StringBuilder.append(Object) → EXCEPTION_ACCESS_VIOLATION).
        // Read the real slice straight out of the parent's backing array instead.
        // Both carriers, and only for a view this VM minted: a `SubList`
        // java.base's own bytecode built carries none of these fields, and
        // falling THROUGH is right for it — the ArrayList-shape heuristic
        // below does not match its `(modCount:int, root:ref, ...)` layout, so
        // it reaches the `toArray()` fallback, which runs the receiver's own
        // correct bytecode.
        if (cls_name == ASL_CLASS || cls_name == ASL_REAL_CLASS)
            && asl_base_checked(ctx, coll).is_some()
        {
            if let Some((parent, offset, size, _expected)) = asl_state(ctx, coll) {
                if size > 0 {
                    if let (Some(data), _) = al_state(ctx, parent) {
                        let mut out = Vec::with_capacity(size as usize);
                        for i in 0..(size as usize) {
                            out.push(ctx.get_array_element(data, offset as usize + i));
                        }
                        return Ok(out);
                    }
                }
            }
            return Ok(Vec::new());
        }
        // kotlin.collections.ArrayAsCollection (the backing view Kotlin's
        // mutableListOf/listOf/setOf(vararg) build via
        // CollectionsKt.asCollection(array, isVarargs)) has layout
        // (values: Object[], isVarargs: Boolean). The generic "f0 = array,
        // f1 = int size" ArrayList-shape heuristic a few lines below reads
        // field 1 as a Value::Int and matches it regardless of whether the
        // real field is an int or a boolean (both are stored as Value::Int
        // on CratonVM) so isVarargs=true gets misread as size=1, and every
        // new ArrayList<>(kotlin.collections.mutableListOf(a, b, ...)) (or
        // HashSet(...), etc.) copy silently truncates to just the FIRST
        // element (isVarargs=false accidentally reads size=0, which falls
        // through to the correct toArray()-based fallback instead -- only
        // the true case is silently wrong). This broke Spring's own
        // InvocableHandlerMethodKotlinTests (a mutableListOf(...) resolver
        // list lost its second element), and would affect any Kotlin code
        // building a mutable collection from 2+ vararg literals. Read the
        // real `values` field by name instead of guessing by slot index.
        if cls_name == "kotlin/collections/ArrayAsCollection" {
            if let Value::Object(Some(arr)) = ctx.get_field_by_name(coll, "values") {
                if ctx.heap_kind_of(arr) == ObjectKind::Array {
                    let len = ctx.array_length(arr);
                    let mut out = Vec::with_capacity(len);
                    for i in 0..len {
                        out.push(ctx.get_array_element(arr, i));
                    }
                    return Ok(out);
                }
            }
            return Ok(Vec::new());
        }
        // `java.util.ImmutableCollections$ListN` / `$SetN` — the real-JDK
        // classes, reached whenever a stream operation falls through to JDK
        // bytecode instead of one of our `toList` natives. Both hold their
        // payload in an `elements` array; ListN since JDK 20 also carries a
        // trailing `boolean allowNulls`, which is exactly the field the
        // generic (Object[], int size) probe further down used to read AS the
        // size. The descriptor guard there now refuses that, but reading the
        // array by NAME here is both correct and direct, so no call site
        // depends on a `toArray()` fallback.
        //
        // The two differ in one respect that matters: a ListN's array IS the
        // element sequence and may legitimately contain nulls, while a SetN's
        // is an open-addressed table whose nulls are empty slots.
        if cls_name == "java/util/ImmutableCollections$ListN"
            || cls_name == "java/util/ImmutableCollections$SetN"
        {
            let keep_nulls = cls_name.ends_with("$ListN");
            if let Value::Object(Some(arr)) = ctx.get_field_by_name(coll, "elements") {
                if ctx.heap_kind_of(arr) == ObjectKind::Array {
                    let len = ctx.array_length(arr);
                    // A `SetN` in its ITERATOR's order (round 15 wave 3).
                    if !keep_nulls {
                        if let Some(out) = setn_iteration_order(&*ctx, arr, len) {
                            return Ok(out);
                        }
                    }
                    let mut out = Vec::with_capacity(len);
                    for i in 0..len {
                        let v = ctx.get_array_element(arr, i);
                        if keep_nulls || !matches!(v, Value::Object(None)) {
                            out.push(v);
                        }
                    }
                    return Ok(out);
                }
            }
            return Ok(Vec::new());
        }
        if cls_name.starts_with("java/util/Collections$Unmodifiable")
            || cls_name.starts_with("java/util/Collections$Synchronized")
            || cls_name.starts_with("java/util/Collections$Checked")
            || cls_name == "java/util/Collections$SingletonList"
            || cls_name == "java/util/Collections$SingletonSet"
        {
            if let Value::Object(Some(inner)) = ctx.get_field_by_name(coll, "c") {
                return Ok(collect_collection_elements(ctx, inner)?);
            }
            // SingletonList and SingletonSet store their sole element in
            // `element`.  That element is allowed to be null, so its presence
            // must be determined by the receiver class, not by its Value
            // representation.  Treating Object(None) as an absent backing
            // collection turned `Collections.singletonList(null)` into an
            // empty removal set, leaving null holes in ArrayList bulk
            // operations and downstream protobuf repeated fields.
            if cls_name == "java/util/Collections$SingletonList"
                || cls_name == "java/util/Collections$SingletonSet"
            {
                return Ok(vec![ctx.get_field_by_name(coll, "element")]);
            }
        }
        // RegularEnumSet — a real-JDK class CratonVM doesn't synthetically
        // back. Its state is a `long elements` bitmask over the inherited
        // `Enum<E>[] universe` array (no element array the slot heuristics
        // below can find — worse, the HashSet heuristic false-matches the
        // `elementType` Class field as a backing map and returns empty). And
        // routing through `iterator()` doesn't help: the `Iterable.iterator`
        // interface native (`native_al_iterator`) also only models ArrayList
        // layout, so a reflective `invoke_virtual("iterator")` yields an empty
        // iterator. Extract elements directly from the bitmask so
        // `EnumSet.allOf(...).toArray()/stream()/forEach()` and
        // `new ArrayList<>(enumSet)` (e.g. JUnit @Parameters over an EnumSet,
        // WildFly subsystem tests) see the real members. JumboEnumSet (>64
        // constants) stores a `long[] elements`; handle both.
        // ArrayDeque — synthetic circular buffer (slot 0 = Object[] data,
        // slot 1 = head, slot 2 = tail, slot 3 = size; see AD_FIELD_*). The
        // generic "f0 = array, f1 = int size" ArrayList heuristic below
        // misreads `head` as the size and returns `data[0..head)` — e.g. two
        // addFirst() on a fresh deque leave head=14, so `deque.stream()`
        // yielded 14 nulls (JUnit 6 ResourceLockAware streams a deque of
        // ancestor descriptors and died on the null receivers). Walk the ring
        // buffer from `head` in deque order instead.
        // Jetty BlockingArrayQueue extends AbstractList but stores elements in a
        // circular `_elements` array with spare null slots. The generic
        // ArrayList-shape probe below can false-match that private array and
        // surface null holes; Jetty `HttpDestination.abortExchanges` does
        // `new ArrayList<>(exchanges)` and then unconditionally dereferences
        // every copied exchange. Walk the public list contract instead.
        if cls_name == "org/eclipse/jetty/util/BlockingArrayQueue" {
            let size = match ctx.invoke_virtual(coll, "size", "()I", &[]) {
                Ok(Some(Value::Int(n))) if n > 0 => n as usize,
                _ => 0,
            };
            // gc-common w20-a: every `get(i)` is a Java call; the elements
            // collected so far are kept pinned (see `collect_indexed_pinned`).
            return Ok(collect_indexed_pinned(ctx, size, |ctx, i| {
                let coll = coll_refresh(ctx, coll_pin, coll, "BlockingArrayQueue.get");
                match ctx.invoke_virtual(
                    coll,
                    "get",
                    "(I)Ljava/lang/Object;",
                    &[Value::Int(i as i32)],
                ) {
                    Ok(Some(v)) if !matches!(v, Value::Object(None)) => IndexedStep::Push(v),
                    _ => IndexedStep::Skip,
                }
            }));
        }

        // `>= AD_FIELD_SIZE`, i.e. the three slots this path actually reads
        // (`elements`, `head`, `tail`) — not four. `ad_state` DERIVES the
        // count from `head`/`tail`, so slot 3 is neither read nor written any
        // more, and `synthetic_stub_fields` narrowed `java/util/ArrayDeque` to
        // the three the real class declares so it stops being padded out of
        // the compact layout. Asking for a fourth slot here would have made
        // this arm unreachable on every real deque.
        if cls_name == "java/util/ArrayDeque" && ctx.object_num_fields(coll) >= AD_FIELD_SIZE {
            let (data, head, _tail, size) = ad_state(ctx, coll);
            if let Some(buf) = data {
                let cap = ctx.array_length(buf);
                if cap > 0 && size >= 0 && (size as usize) <= cap {
                    let mut out = Vec::with_capacity(size as usize);
                    for i in 0..(size as usize) {
                        let idx = (head as usize + i) % cap;
                        out.push(ctx.get_array_element(buf, idx));
                    }
                    return Ok(out);
                }
            }
            return Ok(Vec::new());
        }
        // PriorityQueue / PriorityBlockingQueue — a binary heap held in
        // `queue`/`array` (Object[]) with a separate `size`. The generic
        // "f0 = array, f1 = size" heuristic below doesn't model it (returns
        // empty), which made `toArray()` / `iterator()` / `forEach()` over a
        // PriorityQueue see zero elements (and, with the `iterator()` shim
        // shadowing the real `PriorityQueue$Itr`, recurse). Read it by name;
        // iteration order is heap-array order (matches PriorityQueue$Itr).
        if cls_name == "java/util/PriorityQueue"
            || cls_name == "java/util/concurrent/PriorityBlockingQueue"
        {
            let arr = match ctx.get_field_by_name(coll, "queue") {
                Value::Object(Some(a)) if ctx.heap_kind_of(a) == ObjectKind::Array => Some(a),
                _ => match ctx.get_field_by_name(coll, "array") {
                    Value::Object(Some(a)) if ctx.heap_kind_of(a) == ObjectKind::Array => Some(a),
                    _ => None,
                },
            };
            if let Some(a) = arr {
                let cap = ctx.array_length(a);
                let size = match ctx.get_field_by_name(coll, "size") {
                    Value::Int(v) if v >= 0 && (v as usize) <= cap => v as usize,
                    _ => cap,
                };
                let mut out = Vec::with_capacity(size);
                for i in 0..size {
                    out.push(ctx.get_array_element(a, i));
                }
                return Ok(out);
            }
            return Ok(Vec::new());
        }
        if cls_name == "java/util/RegularEnumSet" || cls_name == "java/util/JumboEnumSet" {
            if let Value::Object(Some(universe)) = ctx.get_field_by_name(coll, "universe") {
                // Membership: walk the inherited `universe` (all constants of
                // the enum, in ordinal order) and keep bits present in the
                // RegularEnumSet/JumboEnumSet `elements` mask. Do this directly
                // instead of routing through `contains()`: inherited
                // AbstractCollection/Set dispatch can land back in this generic
                // native before the concrete RegularEnumSet bytecode is selected.
                let ulen = ctx.array_length(universe);
                let mut out = Vec::with_capacity(ulen);
                for i in 0..ulen {
                    let elem = ctx.get_array_element(universe, i);
                    let contained = match elem {
                        Value::Object(Some(e)) => {
                            enum_set_contains_member(&*ctx, coll, e).unwrap_or(false)
                        }
                        _ => false,
                    };
                    if contained {
                        out.push(elem);
                    }
                }
                return Ok(out);
            }
        }
        // Hibernate persistent collections (org.hibernate.collection.spi.*:
        // PersistentSet / PersistentList / PersistentBag / PersistentSortedSet
        // / …) are REAL bytecode classes, not natively-modelled. The generic
        // ArrayList/array slot probes further down read slots by index on any
        // object, and on a PersistentSet those indices land on unrelated fields
        // — one of which happens to be a ref-array whose head elements are VM
        // singletons, so `new ArrayList<>(persistentSet)` surfaced heap garbage
        // (HIB-CV-18: a reloaded `@ElementCollection Set` came back as
        // [SessionFactoryImpl, <uuid>, BootstrapServiceRegistryImpl]). Read via
        // the collection's own (correct) `toArray()` instead. Returns refs that
        // a GC may later move, so callers that allocate afterwards must re-read
        // (cf. `native_al_init_from_collection`).
        if cls_name.starts_with("org/hibernate/collection/") {
            let size = match ctx.invoke_virtual(coll, "size", "()I", &[]) {
                Ok(Some(Value::Int(n))) => n,
                _ => 0,
            };
            if size <= 0 {
                return Ok(Vec::new());
            }
            let coll = coll_refresh(ctx, coll_pin, coll, "hibernate size()->toArray()");
            if let Ok(Some(Value::Object(Some(arr)))) =
                ctx.invoke_virtual(coll, "toArray", "()[Ljava/lang/Object;", &[])
            {
                if ctx.heap_kind_of(arr) == ObjectKind::Array {
                    let len = ctx.array_length(arr);
                    let mut out = Vec::with_capacity(len);
                    for i in 0..len {
                        out.push(ctx.get_array_element(arr, i));
                    }
                    return Ok(out);
                }
            }
            return Ok(Vec::new());
        }
        // `java/nio/file/Path` — CratonVM's synthetic Path (2 fields: path
        // String + owning FileSystem) implements `Iterable<Path>` over its
        // name elements, but it is NOT an ArrayList-layout object. The
        // ArrayList-shaped natives registered on `java/lang/Iterable`
        // (`iterator`/`forEach` → `native_al_iterator`/`native_al_for_each`)
        // therefore read the wrong slots and iterate zero elements — so
        // `path.forEach(...)` and `((Iterable) path).iterator()` silently see
        // nothing even though `path.iterator()` (the Path-class native) works
        // and `getNameCount()` is correct. This broke Spring Boot's
        // `GenerateAntoraPlaybook` (content-source `url` computed by
        // `root.relativize(playbook).normalize().forEach(p -> url.append("/.."))`
        // came out as "." instead of "./../../../../.."). Walk the path's own
        // `getNameCount()`/`getName(i)` natives (neither is shadowed by a
        // collection bridge, so `invoke_virtual` resolves them on the Path
        // class) to surface the real name elements as `Path` objects.
        if cls_name == "java/nio/file/Path" {
            let count = match ctx.invoke_virtual(coll, "getNameCount", "()I", &[]) {
                Ok(Some(Value::Int(n))) if n > 0 => n,
                _ => 0,
            };
            // gc-common w20-a: every `getName(i)` allocates a Path; the names
            // collected so far are kept pinned (see `collect_indexed_pinned`).
            return Ok(collect_indexed_pinned(ctx, count as usize, |ctx, i| {
                let coll = coll_refresh(ctx, coll_pin, coll, "Path.getName");
                match ctx.invoke_virtual(
                    coll,
                    "getName",
                    "(I)Ljava/nio/file/Path;",
                    &[Value::Int(i as i32)],
                ) {
                    Ok(Some(v)) => IndexedStep::Push(v),
                    _ => IndexedStep::Skip,
                }
            }));
        }
        // Jetty's BlockingArrayQueue (used by HttpDestination.exchanges) is a
        // real AbstractList/BlockingQueue with inherited AbstractList fields
        // followed by `_indexes:int[]`, `_size:AtomicInteger`, locks, and the
        // real `_elements:Object[]`. The broad Arrays$ArrayList heuristic below
        // used to read the resolved `a` slot from this unrelated class, found
        // `_indexes:int[]`, copied its primitive ints into an Object[] snapshot
        // as nulls, and `new ArrayList<>(exchanges)` later surfaced a null
        // HttpExchange during `HttpClient.stop()`. Snapshot through the queue's
        // own indexed List contract instead.
        if cls_name == "org/eclipse/jetty/util/BlockingArrayQueue"
            || obj_is_instance_of(ctx, coll, "org/eclipse/jetty/util/BlockingArrayQueue")
        {
            // `obj_is_instance_of` can resolve a class: re-read first.
            let coll = coll_refresh(ctx, coll_pin, coll, "AbstractList.size");
            let size = match ctx.invoke_virtual(coll, "size", "()I", &[]) {
                Ok(Some(Value::Int(n))) if n > 0 => n.min(16_777_216),
                _ => 0,
            };
            // gc-common w20-a: every `get(i)` is a Java call; the elements
            // collected so far are kept pinned (see `collect_indexed_pinned`).
            return Ok(collect_indexed_pinned(ctx, size as usize, |ctx, i| {
                let coll = coll_refresh(ctx, coll_pin, coll, "AbstractList.get");
                match ctx.invoke_virtual(
                    coll,
                    "get",
                    "(I)Ljava/lang/Object;",
                    &[Value::Int(i as i32)],
                ) {
                    Ok(Some(v)) => IndexedStep::Push(v),
                    _ => IndexedStep::Stop,
                }
            }));
        }
    }
    // org.apache.kafka.common.utils.ImplicitLinkedHashCollection (and its
    // subclasses — ImplicitLinkedHashMultiCollection, the generated message
    // *Collection types like ApiVersionsResponseData$ApiVersionCollection):
    // an open-addressing hash array `elements` with NULL holes for empty
    // buckets plus a `size` count. The generic ArrayList heuristic below reads
    // `elements[0..size]` and so surfaces those null holes — e.g.
    // `ApiVersionCollection.toArray()` returned 5 nulls, so
    // `new LinkedList<>(coll)` carried nulls and `NodeApiVersions.<init>` NPE'd
    // on `apiVersion.apiKey()`. The real `iterator()` walks an embedded linked
    // list and skips holes; mirror that by reading the backing array and
    // dropping nulls. (Membership is exact; element ordering is hash-bucket
    // order rather than the LinkedHash insertion order — toArray callers in
    // this suite don't depend on order, and serialization uses iterator(), not
    // this native.)
    if let Some(ilhc_cid) =
        ctx.class_id_by_name("org/apache/kafka/common/utils/ImplicitLinkedHashCollection")
    {
        if ctx.is_subclass(cid, ilhc_cid) {
            if let Value::Object(Some(arr)) = ctx.get_field_by_name(coll, "elements") {
                if ctx.heap_kind_of(arr) == ObjectKind::Array {
                    let len = ctx.array_length(arr);
                    let mut out = Vec::new();
                    for i in 0..len {
                        let v = ctx.get_array_element(arr, i);
                        if !matches!(v, Value::Object(None)) {
                            out.push(v);
                        }
                    }
                    return Ok(out);
                }
            }
        }
    }
    // KC-Charset fix (2026-05-25): receiver-layout guard for the speculative
    // probe sequence below. `collect_collection_elements` is invoked through
    // generic Collection-interface natives (`addAll`, `retainAll`, `HashSet`
    // ctors, …) whose receiver type is statically `Collection` but at runtime
    // may be ANY object — e.g. the JUnit `--help` bootstrap funnels a
    // `java/nio/charset/Charset` (3 fields) through a generic-collection
    // call site during charset registration, and the blind
    // `ctx.get_field(coll, data_slot)` reads issued below hit slot indices 4
    // and 6 on the 3-slot Charset.  Without the guard the GC fires hundreds
    // of `gen_heap::get_field: out-of-bounds field read dropped` warnings,
    // each probe returns a benign null, and a downstream invariant eventually
    // segfaults the VM.  We compute `n_fields` once and use it to short-circuit
    // any layout probe whose required slot is past the receiver's actual layout.
    // A `values()` view is ArrayList-SHAPED, and the layout probe below reads
    // its `elementData`/`size` slots DIRECTLY. That is the site the
    // lazy-map-views plan page named as "the accessor is nearly, but not quite,
    // a chokepoint", and it was harmless only for as long as every `values()`
    // call minted a FRESH view -- a fresh view is never stale. Caching the view
    // on the source removes that accident, so the read has to go through the
    // funnel like every other reader.
    //
    // MEASURED as a real divergence while the cache was briefly allowed on the
    // `Hashtable` family: `new ArrayList<>(hashtable.values())` after two
    // removals answered 2 where HotSpot answers 1
    // (`probes/MapViewCacheProbe` `ht.values.copyList`), while `size()` and
    // `toString()` on the very same object were right -- only `toArray()` was
    // wrong. A `Hashtable` view arrives inside a
    // `Collections$SynchronizedCollection`, which this function unwraps and
    // RECURSES into, landing here; a bare `HashMap$Values` receiver reaches
    // `native_al_to_array` instead and resyncs there. The family is refused by
    // `cached_live_values_view` now, so that exact route is closed twice over
    // -- this closes the SHAPE rather than the instance.
    //
    // The keySet/entrySet carriers need nothing here: their branch below hands
    // the BACKING to `collect_view_snapshot_ordered`, which reads the SOURCE
    // map and is live whatever the backing happens to hold.
    let coll = match ctx.class_name_arc_of_id(cid).as_deref() {
        Some(n) if is_map_view_carrier(n) && values_view_source(ctx, coll).is_some() => {
            resync_values_view(ctx, coll)?
        }
        _ => coll,
    };
    let n_fields = ctx.object_num_fields(coll);
    // `CRATONVM_COMPAT_LAYOUT_PROBE_OWN_CLASSES` (round 15 wave 3, lane
    // compat3): the two generic `(Object[], int)` probes below answer only for
    // the JDK's / this VM's classes and a program's `ArrayList` / `Vector`
    // subclass, and only with a REFERENCE array. See
    // [`layout_probe_own_classes_enabled`].
    let probe_guard = layout_probe_own_classes_enabled();
    let skip_generic_probes = probe_guard && program_collection_class(&*ctx, coll);
    // S111r-bug-fix (peaceful-sammet): Try ArrayList layout via the
    // field-index resolver so we honour the real-JDK layout
    // (`modCount`/`elementData`/`size` slots from AbstractList/ArrayList) —
    // not just the synthetic (0=data, 1=size) layout. Without this,
    // `HashSet.addAll(arrayList)` silently sees zero elements when running
    // against the real JDK, which is the canonical victim for Spring's
    // `AnnotationTypeMapping.processAliases` populating `claimedAliases`
    // and surfaces as the `@AliasFor ... is not meta-present` chain.
    if !skip_generic_probes {
        let (data_slot, size_slot, _) = al_slots(ctx);
        if data_slot < n_fields && size_slot < n_fields {
            let f_data = ctx.get_field(coll, data_slot);
            let f_size = ctx.get_field(coll, size_slot);
            if let (Value::Object(Some(arr)), Value::Int(size)) = (f_data, f_size) {
                if ctx.heap_kind_of(arr) == ObjectKind::Array
                    && (!probe_guard
                        || ctx.heap_element_type_of(arr) == ArrayElementType::Reference)
                {
                    let len = ctx.array_length(arr);
                    if size >= 0 && len >= size as usize {
                        let mut elems = Vec::with_capacity(size as usize);
                        for i in 0..(size as usize) {
                            elems.push(ctx.get_array_element(arr, i));
                        }
                        return Ok(elems);
                    }
                }
            }
        }
    }
    // Legacy/synthetic ArrayList layout (field 0 = Object[], field 1 = Int size).
    // Skip entirely when the receiver doesn't even have 2 slots — common for
    // 0-field marker classes (cglib's `MethodInterceptorGenerator`, etc.).
    //
    // GUARDED BY THE DECLARED DESCRIPTOR OF SLOT 1, because this shape cannot
    // otherwise tell a `boolean` from an `int`: CratonVM represents both as
    // `Value::Int`, so any `(Object[], boolean)` class reads as "an array and
    // a size", and a `true` reads as size **1**. Every copy of such a
    // collection then silently keeps its FIRST element and drops the rest —
    // `addAll`, the copy constructors and `List.copyOf`, into an ArrayList,
    // LinkedList, Vector or HashSet alike.
    //
    // Not hypothetical, and not one class. `kotlin.collections.
    // ArrayAsCollection` (values, isVarargs) is special-cased by name a few
    // hundred lines above, having cost Spring's Kotlin resolver tests. The
    // same shape is `java.util.ImmutableCollections$ListN`, which since JDK 20
    // carries `(E[] elements, boolean allowNulls)` — and `Stream.toList()`
    // builds it through `listFromTrustedArrayNullsAllowed`, i.e. with
    // `allowNulls = true`. So `Arrays.stream(ints).boxed().toList()` produced
    // a list whose `size()`, `get()`, `iterator()`, `stream()` and `toArray()`
    // all answered 6 while every COPY of it took 1. In GPULlama3 that
    // truncated a tokenized prompt from 16 ids to 11: the model answered a
    // question it had not been asked, and nothing on the path reported
    // anything.
    //
    // Rejecting only on POSITIVE evidence keeps synthetic-JDK mode working —
    // where the layout is fabricated and carries no field metadata,
    // `slot_declared_descriptor` answers `None` and this probe behaves exactly
    // as it did before.
    let f0 = if n_fields >= 1 {
        ctx.get_field(coll, 0)
    } else {
        Value::Object(None)
    };
    let f1 = if n_fields >= 2 {
        ctx.get_field(coll, 1)
    } else {
        Value::Object(None)
    };
    let slot1_is_not_int = matches!(
        slot_declared_descriptor(ctx, cid, 1).as_deref(),
        Some(d) if d != "I"
    );
    if let (Value::Object(Some(arr)), Value::Int(size), false) = (f0, f1, skip_generic_probes) {
        if !slot1_is_not_int
            && ctx.heap_kind_of(arr) == ObjectKind::Array
            && (!probe_guard || ctx.heap_element_type_of(arr) == ArrayElementType::Reference)
        {
            let len = ctx.array_length(arr);
            if size >= 0 && len >= size as usize {
                let mut elems = Vec::with_capacity(size as usize);
                for i in 0..(size as usize) {
                    elems.push(ctx.get_array_element(arr, i));
                }
                return Ok(elems);
            }
        }
    }
    // S111r-bug-fix: Arrays$ArrayList -- single field `a` (Object[]), no size.
    // `Arrays.asList(...)` is heavily used in JDK callers (and Spring uses
    // it indirectly through `Collections.singletonList` / similar wrappers)
    // and has a different layout than `java.util.ArrayList`.
    //
    // locale-azure fix: in real-JDK mode, Arrays$ArrayList inherits
    // AbstractList.modCount at slot 0, so its own `a` field is at slot 1 --
    // NOT slot 0 as this branch originally assumed (that assumption only
    // held in synthetic-jdk mode, which has no modCount ancestor slot).
    // Reading f0 here saw `modCount` (an Int, e.g. 0), which is not an
    // Object, so the branch below silently fell through and
    // `Arrays.asList(...)` contributed zero elements to any addAll/copy
    // constructor call sites (e.g. `LinkedList.addAll(Arrays.asList(x))`
    // returned false and added nothing — MockHttpServletRequest.
    // setPreferredLocales(Arrays.asList(...)) left its LinkedList<Locale>
    // empty, so getLocale()'s getFirst() threw NoSuchElementException).
    // Resolve `a`'s real slot by name and fall back to slot 0 for
    // synthetic-jdk / resolution-failure cases.
    let aal_a_slot = ctx
        .resolve_field_index("java/util/Arrays$ArrayList", "a")
        .unwrap_or(0);
    let f_aal_a = if aal_a_slot < n_fields {
        ctx.get_field(coll, aal_a_slot)
    } else {
        Value::Object(None)
    };
    if obj_is_instance_of(ctx, coll, "java/util/Arrays$ArrayList") {
        if let Value::Object(Some(arr)) = f_aal_a {
            if ctx.heap_kind_of(arr) == ObjectKind::Array {
                let len = ctx.array_length(arr);
                let mut elems = Vec::with_capacity(len);
                for i in 0..len {
                    elems.push(ctx.get_array_element(arr, i));
                }
                return Ok(elems);
            }
        }
    }
    // LinkedList: its real head/tail/size live in the identity-hash-keyed
    // `ll_overlay()` side-table, not in the object's raw field slots (see the
    // ll_get/ll_set block comment above) — once the real java.util.LinkedList
    // class is loaded, the synthetic slot indices (LL_FIELD_HEAD/SIZE) alias
    // real-JDK declared fields instead. Reading those slots directly here
    // (as a prior version of this branch did) saw garbage/zero for a
    // native-populated LinkedList, so `size > 0` never matched and this
    // function fell through to `Vec::new()` — e.g.
    // `someLinkedList.addAll(anotherLinkedList)` (both native_ll_add_all and
    // native_al_add_all route element collection through here) silently
    // added nothing and returned false. Detect a real LinkedList (or
    // subclass) explicitly and read through the overlay-aware ll_get(), the
    // same accessor ll_snapshot_array/native_ll_* already use.
    let is_linkedlist_like = ctx
        .class_id_by_name("java/util/LinkedList")
        .map(|ll| cid == ll || ctx.is_subclass(cid, ll))
        .unwrap_or(false);
    if is_linkedlist_like {
        let size = ll_size(ctx, coll);
        if size > 0 {
            let mut elems = Vec::with_capacity(size as usize);
            let mut cur = match ll_get(ctx, coll, "head") {
                Value::Object(Some(r)) => Some(r),
                _ => None,
            };
            while let Some(node) = cur {
                // Per-node guard: a real LL node has 3 slots (item/next/prev).
                let node_fields = ctx.object_num_fields(node);
                if LL_NODE_ELEM >= node_fields || LL_NODE_NEXT >= node_fields {
                    break;
                }
                elems.push(ctx.get_field(node, LL_NODE_ELEM));
                cur = match ctx.get_field(node, LL_NODE_NEXT) {
                    Value::Object(Some(n)) => Some(n),
                    _ => None,
                };
            }
            return Ok(elems);
        }
        return Ok(Vec::new());
    }
    // S111r28: HashSet / LinkedHashSet — field 0 = backing map. Collect the
    // backing map's keys (in iteration order).
    //
    // GATE (ATNConfig CCE fix): only treat field-0-is-a-HashMap as "this is a
    // HashSet whose elements are the map's keys" when the receiver actually IS a
    // HashSet/subclass. A custom `Set` (or any Collection) can legitimately hold
    // an unrelated `HashMap` at slot 0 whose KEYS are NOT the collection's
    // elements — e.g. Groovy's ANTLR4 `ATNConfigSet` (field 0 =
    // `HashMap<Long,ATNConfig> mergedConfigs`). Without this gate, `toArray()` /
    // `new ArrayList<>(atnConfigSet)` returned the Long keys, so `List.sort` over
    // a STATE_ALT_SORT_COMPARATOR passed a Long to the ATNConfig comparator
    // (ClassCastException Long→ATNConfig; getState() NSME under --nojit) and every
    // Groovy script failed to compile (SpringRepositoriesExtensionTests). For a
    // non-HashSet custom collection we fall through to empty, and the caller
    // (`native_al_to_array` / `collect_collection_elements_or_real`) drives the
    // real overridden `iterator()`/`toArray()`.
    let is_hashset_like = ctx
        .class_id_by_name("java/util/HashSet")
        .map(|hs| cid == hs || ctx.is_subclass(cid, hs))
        .unwrap_or(false);
    // A keySet/entrySet view is HashSet-shaped but is NOT a `HashSet` subclass
    // since it got its own carrier class (`SET_VIEW_CARRIERS`), and it keeps
    // its backing past its declared fields rather than at slot 0. The gate
    // above is about not mistaking an arbitrary `Set` with a `HashMap` in slot
    // 0 for a HashSet; a carrier is not arbitrary, so it is admitted by name.
    let backing_slot = if is_hashset_like {
        Some(HS_FIELD_MAP)
    } else {
        match ctx.class_name_arc_of_id(cid).as_deref() {
            Some(n) if is_set_view_carrier(n) => Some(ctx.class_num_total_fields(cid)),
            _ => None,
        }
    };
    if backing_slot.is_some_and(|s| s < n_fields) {
        if let Value::Object(Some(backing)) = ctx.get_field(coll, backing_slot.unwrap()) {
            // A reversed `LinkedKeySet` / `LinkedEntrySet` (C3-1) is read tail
            // first; asked before the collection, which can allocate.
            let reversed = linked_set_view_reversed(&*ctx, coll);
            // An insertion-ordered set (LinkedHashSet / CopyOnWriteArraySet) is
            // backed by a LinkedHashMap, which keeps its entries in the
            // insertion-order overlay rather than the bucket array — so the
            // bucket-array guard below would miss it. Detect it explicitly;
            // `collect_view_snapshot_ordered` → `lhm_collect_keys` yields the
            // keys in insertion order.
            if is_lhm_receiver(ctx, backing) {
                return Ok(in_view_order(
                    collect_view_snapshot_ordered(ctx, backing)?,
                    reversed,
                ));
            }
            // Otherwise verify it's a HashMap-like: its bucket slot holds an
            // array. Asks `map_buckets_slot` rather than slot 0, which on a
            // real-layout backing is `AbstractMap.keySet`.
            let bucket_slot = map_buckets_slot(ctx, backing);
            if bucket_slot < ctx.object_num_fields(backing) {
                let s0 = ctx.get_field(backing, bucket_slot);
                if let Value::Object(Some(arr)) = s0 {
                    if ctx.heap_kind_of(arr) == ObjectKind::Array {
                        return Ok(in_view_order(
                            collect_view_snapshot_ordered(ctx, backing)?,
                            reversed,
                        ));
                    }
                }
            }
        }
    }
    // TreeSet (and its `keySet`/`descendingSet` views) keep their elements in
    // the `ts_array_table` side-table, NOT in object fields — in real-JDK mode
    // the receiver's only field is the backing `m` TreeMap (null on a native
    // TreeSet), so none of the field heuristics above find anything. Read the
    // sorted state directly so `new ArrayList<>(treeSet)`, `set.addAll(treeSet)`,
    // `new TreeSet<>(treeSet)`, etc. see the real elements (otherwise they
    // silently materialise empty — e.g. the copy constructor produced an empty
    // set). Elements come back in ascending sorted order, matching iteration.
    let is_treeset_like = ctx
        .class_id_by_name("java/util/TreeSet")
        .map(|ts| cid == ts || ctx.is_subclass(cid, ts))
        .unwrap_or(false);
    if is_treeset_like {
        let (data_opt, size, _) = ts_state(ctx, coll);
        if let (Some(data), true) = (data_opt, size > 0) {
            let mut out = Vec::with_capacity(size as usize);
            for i in 0..(size as usize) {
                out.push(ctx.get_array_element(data, i));
            }
            return Ok(out);
        }
    }
    // No layout heuristic matched and it's not an EnumSet. Return empty rather
    // than driving `iterator()` here: the `Iterable.iterator()` native
    // (`native_al_iterator`) itself snapshots non-list collections via THIS
    // function, so calling `iterator()` from here would recurse. Genuinely
    // unmodelled collection layouts therefore materialise empty (status quo for
    // those) — EnumSet, handled directly above, is the case that matters.
    Ok(Vec::new())
}

/// `CRATONVM_COMPAT_BATCH_REMOVE_CONTAINS` -- default ON, `=0` restores the
/// snapshot-and-`equals` walk. `ArrayList.removeAll(c)` / `retainAll(c)` are
/// JDK 25's `batchRemove(c, complement, 0, size)`: `c.contains(es[r])` per
/// element -- the ARGUMENT's own membership test. The natives collected `c`'s
/// elements and compared each with `equals`, which is a different question
/// whenever `c`'s `contains` is not `equals`: a `TreeSet` with a comparator
/// (`String.CASE_INSENSITIVE_ORDER`), an identity set
/// (`Collections.newSetFromMap(new IdentityHashMap<>())`,
/// `IdentityHashMap.keySet()`), a program's own collection -- the list kept or
/// dropped DIFFERENT elements than on HotSpot. It also asked `c` `size()` /
/// `toArray()` / its iterator, which the JDK never does, and cost `n * m`
/// comparisons where a `HashSet` argument costs `n` lookups. See
/// [`al_batch_remove_by_contains`], and [`batch_remove_asks_contains`] for
/// the arguments it is asked of. Round 15 wave 3 (lane compat3),
/// `r15w3-compat3-bulk-ops-ask-the-wrong-questions-FIXED-20260930.md`.
pub(crate) fn batch_remove_contains_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_BATCH_REMOVE_CONTAINS")
}

/// JDK classes whose `contains` is NOT `equals` membership (a comparator, or
/// `==`), by name prefix: sorted sets and their map views, identity-map views,
/// `Collections.newSetFromMap` (whatever its map decides).
pub(crate) const NON_EQUALS_MEMBERSHIP_PREFIXES: [&str; 5] = [
    "java/util/TreeSet",
    "java/util/TreeMap$",
    "java/util/concurrent/ConcurrentSkipList",
    "java/util/IdentityHashMap$",
    "java/util/Collections$SetFromMap",
];

/// Whether [`al_batch_remove_by_contains`] asks `coll.contains(e)`: always
/// correct (it is the JDK's question), but paid only where the snapshot's
/// `equals` can answer differently -- a program's collection
/// ([`copy_source_is_foreign`]), a [`NON_EQUALS_MEMBERSHIP_PREFIXES`] class,
/// or a JDK / this VM's wrapper directly around one. For the JDK's
/// `equals`-membership collections the snapshot answers the same. Pure reads.
pub(crate) fn batch_remove_asks_contains(ctx: &dyn NativeContext, coll: ObjectRef) -> bool {
    if copy_source_is_foreign(ctx, coll) {
        return true;
    }
    let non_equals = |ctx: &dyn NativeContext, o: ObjectRef| {
        ctx.class_name_arc_of_id(ctx.class_id_of_object(o))
            .is_some_and(|n| {
                NON_EQUALS_MEMBERSHIP_PREFIXES
                    .iter()
                    .any(|p| n.starts_with(p))
            })
    };
    if non_equals(ctx, coll) {
        return true;
    }
    let Some(name) = ctx.class_name_arc_of_id(ctx.class_id_of_object(coll)) else {
        return false;
    };
    let inner = if name.starts_with("java/util/Collections$") {
        ctx.get_field_by_name(coll, "c")
    } else if &*name == UNMOD_COLLECTION_CLASS
        || &*name == UNMOD_LIST_CLASS
        || is_unmod_set_class(&name)
    {
        if ctx.object_num_fields(coll) > UNMOD_FIELD_BACKING {
            ctx.get_field(coll, UNMOD_FIELD_BACKING)
        } else {
            Value::Object(None)
        }
    } else {
        return false;
    };
    matches!(inner, Value::Object(Some(i)) if non_equals(ctx, i))
}

/// `c.contains(es[r])` for [`al_batch_remove_by_contains`], `es` and `c` read
/// through their pins (the previous `contains` was Java).
pub(crate) fn batch_contains(
    ctx: &mut dyn NativeContext,
    (buf_pin, buf): (usize, ObjectRef),
    (coll_pin, coll): (usize, ObjectRef),
    r: usize,
) -> Result<bool, MethodCallFailed> {
    let buf = ctx.read_native_pin(buf_pin, buf);
    let coll = ctx.read_native_pin(coll_pin, coll);
    let e = ctx.get_array_element(buf, r);
    match ctx.invoke_virtual(coll, "contains", "(Ljava/lang/Object;)Z", &[e])? {
        Some(Value::Int(v)) => Ok(v != 0),
        _ => Ok(false),
    }
}

/// JDK 25 `ArrayList.batchRemove(c, complement, 0, size)` over an
/// ArrayList-layout `this` ([`batch_remove_contains_enabled`]): the initial run
/// of survivors (`c.contains(e) != complement` ends it; a list with none to
/// drop answers `false` untouched, and an exception there propagates with the
/// list unchanged), then every later element kept iff `c.contains(e) ==
/// complement`, compacted in place; an exception there keeps the unexamined
/// tail (`System.arraycopy(es, r, es, w, end - r)`) and propagates after the
/// `finally`: vacated slots nulled, `size = w` (the `modCount` bump rides
/// [`al_set_size`]). `None` -- nothing asked, the caller's walk runs -- with
/// the switch off, for an argument whose `contains` is `equals` membership
/// anyway ([`batch_remove_asks_contains`]), for a live `values()` view (its
/// removals must reach the source map; the old walk does that for
/// `retainAll`), or when `this` has no readable backing array.
pub(crate) fn al_batch_remove_by_contains(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    coll: ObjectRef,
    complement: bool,
) -> Option<MethodCallResult> {
    if !batch_remove_contains_enabled()
        || !batch_remove_asks_contains(&*ctx, coll)
        || values_view_source(&*ctx, this).is_some()
    {
        return None;
    }
    let (Some(buf), size) = al_state(&*ctx, this) else {
        return None;
    };
    let end = (size.max(0) as usize).min(ctx.array_length(buf));
    let this_pin = ctx.pin_native_root(this);
    let buf_pin = ctx.pin_native_root(buf);
    let coll_pin = ctx.pin_native_root(coll);
    let es = (buf_pin, buf);
    let c = (coll_pin, coll);
    let result = (|| -> MethodCallResult {
        // "Optimize for initial run of survivors".
        let mut r = 0usize;
        loop {
            if r == end {
                return Ok(Some(Value::Int(0)));
            }
            if batch_contains(ctx, es, c, r)? != complement {
                break;
            }
            r += 1;
        }
        let mut w = r;
        r += 1;
        let mut failure = None;
        while r < end {
            match batch_contains(ctx, es, c, r) {
                Ok(hit) => {
                    if hit == complement {
                        let buf = ctx.read_native_pin(buf_pin, buf);
                        let e = ctx.get_array_element(buf, r);
                        ctx.set_array_element(buf, w, e);
                        w += 1;
                    }
                }
                Err(e) => {
                    let buf = ctx.read_native_pin(buf_pin, buf);
                    for k in r..end {
                        let v = ctx.get_array_element(buf, k);
                        ctx.set_array_element(buf, w + (k - r), v);
                    }
                    w += end - r;
                    failure = Some(e);
                    break;
                }
            }
            r += 1;
        }
        // `finally { modCount += end - w; shiftTailOverGap(es, w, end); }`
        let buf = ctx.read_native_pin(buf_pin, buf);
        for k in w..end {
            ctx.set_array_element(buf, k, Value::Object(None));
        }
        let this = ctx.read_native_pin(this_pin, this);
        al_set_size(ctx, this, w as i32);
        match failure {
            Some(e) => Err(e),
            None => Ok(Some(Value::Int(1))),
        }
    })();
    ctx.unpin_native_roots(this_pin);
    Some(result)
}

// ---------------------------------------------------------------------------
// Round 15 wave 4 (lane compat4): the remaining bulk operations ask their
// argument JDK 25's questions (`r15w3-compat3-bulk-ops-ask-the-wrong-
// questions-20260930.md`), and a program's map is copied through its own
// entry iterator (proposal C3W3-2 of `jit-r15w3-compat3-proposals-RETIRED-20261003.md`).
// ---------------------------------------------------------------------------

/// `CRATONVM_COMPAT_DEQUE_BULK_REMOVE_JDK` -- default ON, `=0` restores the
/// snapshot-and-`equals` walk and drops the comodification check.
/// `ArrayDeque.removeAll(c)` / `retainAll(c)` are JDK 25's
/// `bulkRemove(e -> c.contains(e))` / `bulkRemove(e -> !c.contains(e))`: the
/// ARGUMENT's own membership test, asked once per element from head to tail,
/// all of them before anything is removed (so an exception leaves the deque
/// unchanged), then `if (end != tail) throw new
/// ConcurrentModificationException()` when the questions moved the deque's
/// tail. `removeIf(filter)` is the same `bulkRemove`, so it gains that check.
/// `contains` is asked of the arguments [`batch_remove_asks_contains`] names
/// (the `ArrayList` twin's rule); the JDK's `equals`-membership collections
/// keep the snapshot, which answers them the same. See [`ad_bulk_remove_by`].
pub(crate) fn deque_bulk_remove_jdk_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_DEQUE_BULK_REMOVE_JDK")
}

/// `CRATONVM_COMPAT_BULK_ADD_JDK_QUESTIONS` -- default ON, `=0` restores the
/// layout probes plus `size()` + `toArray()` for every argument. For a
/// program's collection ([`copy_source_is_foreign`]) JDK 25 asks:
///
/// * `HashSet.addAll(c)` (`AbstractCollection.addAll`): `c.iterator()`, then
///   per element `hasNext()`, `next()` and the `add`, interleaved;
/// * `LinkedList.addAll(c)` and `LinkedList(c)`: `c.toArray()`, once;
/// * `ArrayDeque.addAll(c)`: `c.size()` (it pre-grows the ring), then
///   `c.forEach(this::addLast)`; `ArrayDeque(c)`: `this(c.size())`, then the
///   same. The walk is `Iterable.forEach`'s default body (`for (T t : this)`);
///   a `c` that overrides `forEach` was asked `iterator()` instead (there is no
///   native `Consumer` to hand it) until round 16 wave 13, which runs the
///   JDK's own `copyElements` ([`deque_copy_for_each_enabled`]).
///
/// `ArrayDeque.addAll` also pre-grows for EVERY argument, as `grow(needed)`
/// does ([`ad_pregrow`]), so the ring's capacity -- which a live
/// `DeqIterator` and reflection see -- is HotSpot's rather than what
/// per-element growth reaches (0 + 100 elements: 101 slots, not 111).
pub(crate) fn bulk_add_jdk_questions_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_BULK_ADD_JDK_QUESTIONS")
}

/// `CRATONVM_COMPAT_MAP_COPY_JDK_QUESTIONS` -- default ON, `=0` restores
/// [`collect_entries_any`] (layout probes, else `isEmpty()` + an
/// `entrySet()` walk collected up front) for every source. Proposal C3W3-2.
/// For a program's map ([`foreign_map_source`]) the copy asks what JDK 25
/// asks, with each put interleaved ([`map_for_each_entry`]):
///
/// * `HashMap(m)`, `HashMap.putAll(m)`, `LinkedHashMap(m)` / `putAll(m)`
///   (`putMapEntries`): `m.size()` -- an answer `<= 0` ends the copy --
///   then `m.entrySet()`, its `iterator()`, per entry `hasNext()`, `next()`,
///   `getKey()`, `getValue()` and the `put`;
/// * `Hashtable.putAll(m)`: the same walk without `size()`;
/// * `TreeMap(m)` / `TreeMap.putAll(m)`: `m.size()` (asked, not a gate), then
///   the walk -- a `SortedMap` source keeps the old path (`buildFromSorted`
///   is a different sequence);
/// * `ConcurrentHashMap.putAll(m)`: `m.size()` only once the table exists
///   (`tryPresize(size() + m.size())`), then the walk, each `putVal` refusing
///   a null key or value;
/// * `Map.copyOf(m)`: after its `isEmpty()`, `m.entrySet().toArray(new
///   Entry[0])`, then `getKey()` / `getValue()` of each entry in array order
///   (`Map.ofEntries`), ahead of the `Map1` / `MapN` checks
///   ([`map_entries_by_to_array`]).
///
/// A throwing question propagates from where the JDK throws it, with the
/// entries put so far kept, and a map whose `entrySet()` disagrees with its
/// fields is copied from its `entrySet()`.
pub(crate) fn map_copy_jdk_questions_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_MAP_COPY_JDK_QUESTIONS")
}

/// `CRATONVM_COMPAT_PUT_ALL_NULL_KEY` -- default ON, `=0` restores the skip.
/// `native_map_put_all` (`HashMap` / `Hashtable` / `Properties.putAll`)
/// dropped every entry whose KEY is null: `hashMap.putAll(m)` lost `m`'s null
/// key (HotSpot keeps it), and `hashtable.putAll(m)` returned normally where
/// `Hashtable.put(null, v)` throws. The entry now reaches the put, whose own
/// `Hashtable` null checks answer; a `Properties` receiver refuses a null key
/// or value with `ConcurrentHashMap.putVal`'s bare NPE (`Properties.putAll`
/// is `map.putAll(t)` over its CHM).
pub(crate) fn put_all_null_key_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_PUT_ALL_NULL_KEY")
}

/// `CRATONVM_COMPAT_LHM_COPY_NO_EVICT` -- default ON, `=0` restores the
/// evicting put. `LinkedHashMap(Map m)` is `putMapEntries(m, false)`: with
/// `evict == false`, `afterNodeInsertion` never asks `removeEldestEntry`.
/// `native_lhm_init_from_map` copied through `native_lhm_put_all`'s
/// evicting put, so a subclass whose `removeEldestEntry` reads a field its
/// own constructor sets AFTER `super(m)` (an LRU cache's `max`, still 0)
/// evicted every copied entry: `new Lru(m, 10)` came out empty.
pub(crate) fn lhm_copy_no_evict_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_LHM_COPY_NO_EVICT")
}

/// `CRATONVM_COMPAT_PUT_ALL_VIRTUAL_PUT` -- default ON, `=0` restores the
/// native put. `Hashtable.putAll` (`for (e : t.entrySet()) put(k, v)`) and
/// `TreeMap.putAll`'s `AbstractMap.putAll` arm call `put` VIRTUALLY, so a
/// program's subclass that overrides `put` (a `Hashtable` normalising its
/// keys, a `TreeMap` counting inserts) sees every entry of a `putAll` -- and
/// of `new Sub(map)`, whose `super(map)` is `putAll`. The natives stored
/// through the JDK class's own put and skipped the override. For a receiver
/// of the program's class ([`class_is_programs`]) each entry is now
/// `invoke_virtual(this, "put", ..)`, which lands on the native put when the
/// class does not override it. `HashMap` / `LinkedHashMap` /
/// `ConcurrentHashMap` / `Properties` `putAll` store through `putVal` (or the
/// backing CHM) and never ask `put`, so they are unchanged.
pub(crate) fn put_all_virtual_put_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_PUT_ALL_VIRTUAL_PUT")
}

/// Whether `obj`'s class is the program's (or a library's): named outside
/// [`COPY_CTOR_OWN_PREFIXES`]. An unnamed class is this VM's. Pure reads.
pub(crate) fn class_is_programs(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    match ctx.class_name_arc_of_id(ctx.class_id_of_object(obj)) {
        Some(n) => !COPY_CTOR_OWN_PREFIXES.iter().any(|p| n.starts_with(p)),
        None => false,
    }
}

/// `CRATONVM_COMPAT_COPY_CTOR_VIRTUAL_BULK` -- default ON, `=0` restores the
/// direct native fill. A JDK 25 copy constructor whose body calls an
/// overridable bulk method -- `TreeMap(Map)` / `ConcurrentHashMap(Map)`:
/// `putAll(m)`; `TreeSet(Collection)`, `HashSet(Collection)` /
/// `LinkedHashSet(Collection)`, `LinkedList(Collection)`: `addAll(c)` --
/// reaches a program subclass's override during `super(c)`
/// ([`copy_ctor_bulk_override`]). The natives filled the receiver directly
/// and skipped it (`r15w4-compat4-bulk-ops-residuals-CLOSED-20261001.md` item 4).
/// Round 15 wave 5, lane compat6.
pub(crate) fn copy_ctor_virtual_bulk_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_COPY_CTOR_VIRTUAL_BULK")
}

/// Under [`copy_ctor_virtual_bulk_enabled`]: whether a program class between
/// `this`'s class and the first JDK / VM class ([`COPY_CTOR_OWN_PREFIXES`])
/// declares `name` + `descriptor` -- the override a copy constructor's
/// virtual bulk call reaches. When none does, the virtual call would land on
/// the native the constructor already runs, so it keeps its direct fill.
/// Pure reads.
pub(crate) fn copy_ctor_bulk_override(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    name: &str,
    descriptor: &str,
) -> bool {
    if !copy_ctor_virtual_bulk_enabled() {
        return false;
    }
    program_class_overrides(ctx, this, name, descriptor)
}

/// Whether a program class between `this`'s class and the first JDK / VM
/// class ([`COPY_CTOR_OWN_PREFIXES`]) declares `name` + `descriptor` (bridges
/// excluded, see `class_declares_method`): the override a virtual call on
/// `this` reaches before any JDK body or registered native. Pure reads.
pub(crate) fn program_class_overrides(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    name: &str,
    descriptor: &str,
) -> bool {
    let mut cid = ctx.class_id_of_object(this);
    for _ in 0..FACTS_WALK_LIMIT {
        let Some(class_name) = ctx.class_name_arc_of_id(cid) else {
            return false;
        };
        if COPY_CTOR_OWN_PREFIXES
            .iter()
            .any(|p| class_name.starts_with(p))
        {
            return false;
        }
        if ctx.class_declares_method(cid, name, descriptor) {
            return true;
        }
        match ctx.superclass_of(cid) {
            Some(s) => cid = s,
            None => return false,
        }
    }
    false
}

/// `this.put(key, value)` dispatched virtually (see
/// [`put_all_virtual_put_enabled`]).
pub(crate) fn map_put_virtual(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: Value,
    value: Value,
) -> MethodCallResult {
    ctx.invoke_virtual(
        this,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[key, value],
    )
}

/// `for (E e : c) body(e)` over `c` (rooted at `c_pin`): `c.iterator()`, then
/// per element `hasNext()`, `next()` and `body`, interleaved. A null iterator
/// is the NPE `hasNext()` on it raises. Each element reaches `body` with
/// nothing run since `next()` returned it; `body` roots what it holds across
/// its own Java. Every pin this takes is released on every exit.
pub(crate) fn for_each_by_iterator(
    ctx: &mut dyn NativeContext,
    (c_pin, c): (usize, ObjectRef),
    mut body: impl FnMut(&mut dyn NativeContext, Value) -> Result<(), MethodCallFailed>,
) -> Result<(), MethodCallFailed> {
    let c = ctx.read_native_pin(c_pin, c);
    let it = match ctx.invoke_virtual(c, "iterator", "()Ljava/util/Iterator;", &[])? {
        Some(Value::Object(Some(it))) => it,
        _ => return Err(bare_npe()),
    };
    let it_pin = ctx.pin_native_root(it);
    let walked = (|| -> Result<(), MethodCallFailed> {
        loop {
            let it_now = ctx.read_native_pin(it_pin, it);
            match ctx.invoke_virtual(it_now, "hasNext", "()Z", &[])? {
                Some(Value::Int(n)) if n != 0 => {}
                _ => return Ok(()),
            }
            let it_now = ctx.read_native_pin(it_pin, it);
            let e = ctx
                .invoke_virtual(it_now, "next", "()Ljava/lang/Object;", &[])?
                .unwrap_or(Value::Object(None));
            body(ctx, e)?;
        }
    })();
    ctx.unpin_native_roots(it_pin);
    walked
}

/// JDK 25 `ArrayDeque.bulkRemove(filter)` over `this`
/// ([`deque_bulk_remove_jdk_enabled`]): `condemn(ctx, e)` is
/// `filter.test(e)`, asked of every element from head to tail before anything
/// moves -- an exception propagates with the deque unchanged -- then a moved
/// `tail` is `ConcurrentModificationException`, and otherwise the survivors
/// are kept in order ([`ad_rebuild`]). Answers whether anything was removed.
pub(crate) fn ad_bulk_remove_by(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    mut condemn: impl FnMut(&mut dyn NativeContext, Value) -> Result<bool, MethodCallFailed>,
) -> MethodCallResult {
    let elems = ad_collect_elements(&*ctx, this);
    let tail0 = ad_state(&*ctx, this).2;
    let this_pin = ctx.pin_native_root(this);
    let (_, e_handles) = pin_value_slice(ctx, &elems);
    let tested = (|| -> Result<Vec<usize>, MethodCallFailed> {
        let mut kept_idx: Vec<usize> = Vec::with_capacity(elems.len());
        for (i, e0) in elems.iter().enumerate() {
            let e = read_pinned_elem(&*ctx, e_handles[i], *e0);
            if !condemn(ctx, e)? {
                kept_idx.push(i);
            }
        }
        // `if (end != tail) throw new ConcurrentModificationException()`.
        let this_now = ctx.read_native_pin(this_pin, this);
        if ad_state(&*ctx, this_now).2 != tail0 {
            return Err(RuntimeError::ConcurrentModificationException.into());
        }
        Ok(kept_idx)
    })();
    let kept_idx = match tested {
        Ok(k) => k,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    if kept_idx.len() == elems.len() {
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Int(0)));
    }
    // Read back through the pins after the last Java (see `ad_batch_remove`).
    let keep: Vec<Value> = kept_idx
        .iter()
        .map(|&i| read_pinned_elem(&*ctx, e_handles[i], elems[i]))
        .collect();
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ad_rebuild(ctx, this, &keep)?;
    Ok(Some(Value::Int(1)))
}

/// `ArrayDeque.removeAll(c)` (`retain == false`) / `retainAll(c)` as JDK 25's
/// `bulkRemove` over `c.contains(e)`; `None` (nothing asked) with the switch
/// off or for an argument whose `contains` is `equals` membership
/// ([`batch_remove_asks_contains`]).
pub(crate) fn ad_bulk_remove_by_contains(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    coll: ObjectRef,
    retain: bool,
) -> Option<MethodCallResult> {
    if !deque_bulk_remove_jdk_enabled() || !batch_remove_asks_contains(&*ctx, coll) {
        return None;
    }
    let coll_pin = ctx.pin_native_root(coll);
    let removed = ad_bulk_remove_by(ctx, this, |ctx, e| {
        let c = ctx.read_native_pin(coll_pin, coll);
        let contained = matches!(
            ctx.invoke_virtual(c, "contains", "(Ljava/lang/Object;)Z", &[e])?,
            Some(Value::Int(v)) if v != 0
        );
        // `removeAll` drops what `c` contains, `retainAll` what it does not.
        Ok(contained != retain)
    });
    ctx.unpin_native_roots(coll_pin);
    Some(removed)
}

/// `ArrayDeque.addAll`'s `if ((needed = (s = size()) + c.size() + 1 -
/// elements.length) > 0) grow(needed);` for `incoming` = the answer of
/// `c.size()` ([`bulk_add_jdk_questions_enabled`]). `this` is rewritten if
/// the grow moves it. A `needed` past `1 << 24` is left to the per-element
/// growth: a count that large is a `size()` the elements will not fill.
pub(crate) fn ad_pregrow(ctx: &mut dyn NativeContext, this: &mut ObjectRef, incoming: i64) {
    let (data, _, _, size) = ad_state(&*ctx, *this);
    let Some(buf) = data else {
        return;
    };
    let cap = ctx.array_length(buf) as i64;
    let needed = i64::from(size) + incoming + 1 - cap;
    if needed > 0 && needed <= (1 << 24) {
        ad_grow(ctx, this, needed as i32);
    }
}

/// JDK 25 `ArrayDeque.addAll(c)` for a program's `c`
/// ([`bulk_add_jdk_questions_enabled`]): `c.size()`, the pre-grow, then each
/// element `addLast`ed as `c`'s iterator hands it out (a null element is
/// `addLast`'s NPE, after the elements ahead of it). Answers `size() > s`.
pub(crate) fn ad_add_all_by_iterator(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    coll: ObjectRef,
) -> MethodCallResult {
    let this_pin = ctx.pin_native_root(this);
    let coll_pin = ctx.pin_native_root(coll);
    let result = (|| -> MethodCallResult {
        let this_now = ctx.read_native_pin(this_pin, this);
        let s = ad_state(&*ctx, this_now).3;
        let c = ctx.read_native_pin(coll_pin, coll);
        let c_size = match ctx.invoke_virtual(c, "size", "()I", &[])? {
            Some(Value::Int(n)) => n,
            _ => 0,
        };
        let mut this_now = ctx.read_native_pin(this_pin, this);
        ad_pregrow(ctx, &mut this_now, i64::from(c_size));
        ad_copy_elements(ctx, (this_pin, this), (coll_pin, coll))?;
        let this_now = ctx.read_native_pin(this_pin, this);
        Ok(Some(Value::Int(i32::from(ad_state(&*ctx, this_now).3 > s))))
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

/// `CRATONVM_COMPAT_DEQUE_COPY_FOR_EACH` -- default ON, `=0` restores the
/// iterator walk. JDK 25 `ArrayDeque(c)` and `ArrayDeque.addAll(c)` both end
/// in the private `copyElements(c)`, which is `c.forEach(this::addLast)`: a
/// program collection that overrides `forEach` is asked `forEach` and never
/// `iterator()`. The natives walked `c.iterator()` because they had no
/// `Consumer` to hand `forEach`, so a `c` whose `iterator()` throws (or
/// differs from its `forEach`) threw where HotSpot copies
/// (`R16CompatdrainColl` `c4i1 deque-ctor-forEach` / `deque-addAll-forEach`:
/// HotSpot `[7, 8] size=1 it=0 .. forEach=1`, base `--compatible` threw the
/// `iterator()`'s `IllegalStateException`). With the switch on the walk runs
/// the JDK's own `copyElements` bytecode ([`ad_copy_elements`]), so the
/// `Consumer` is the JDK's own method-reference lambda and each element
/// reaches `addLast` virtually -- the native `addLast`, or a subclass's
/// override, exactly as on HotSpot. Round 16 wave 13, lane compatcoll
/// (proposal CD16-3, without its `MethodHandleProxies` detour).
pub(crate) fn deque_copy_for_each_enabled() -> bool {
    // Read live, not latched: one read per bulk copy / `spliterator()` /
    // `unordered()`, and no process-wide cache to keep per VM.
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_COMPAT_DEQUE_COPY_FOR_EACH")
}

/// `java.util.ArrayDeque.copyElements(Collection)`'s descriptor.
pub(crate) const AD_COPY_ELEMENTS_DESC: &str = "(Ljava/util/Collection;)V";

/// Whether the `java/util/ArrayDeque` on `this`'s superclass chain declares
/// the private `copyElements(Collection)` -- true for every JDK 9+ class
/// image; false for a synthetic JDK (no bytecode), which keeps the iterator
/// walk. Pure reads.
pub(crate) fn ad_declares_copy_elements(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let mut cid = ctx.class_id_of_object(this);
    for _ in 0..FACTS_WALK_LIMIT {
        let Some(name) = ctx.class_name_arc_of_id(cid) else {
            return false;
        };
        if &*name == "java/util/ArrayDeque" {
            return ctx.class_declares_method(cid, "copyElements", AD_COPY_ELEMENTS_DESC);
        }
        match ctx.superclass_of(cid) {
            Some(s) => cid = s,
            None => return false,
        }
    }
    false
}

/// `copyElements(c)` for the deque `this` (rooted at `this_pin`) and the
/// collection `c` (rooted at `c_pin`), the shared tail of `ArrayDeque(c)` and
/// `addAll(c)` once the ring is sized: under [`deque_copy_for_each_enabled`]
/// the JDK's own body (`c.forEach(this::addLast)`), else -- and on an image
/// without that body -- `for (E e : c) addLast(e)` through `c`'s iterator,
/// the pre-wave-13 walk. An exception from either propagates with the
/// elements added so far kept, as on HotSpot. The caller releases the pins.
pub(crate) fn ad_copy_elements(
    ctx: &mut dyn NativeContext,
    (this_pin, this): (usize, ObjectRef),
    (c_pin, c): (usize, ObjectRef),
) -> Result<(), MethodCallFailed> {
    let this_now = ctx.read_native_pin(this_pin, this);
    if deque_copy_for_each_enabled() && ad_declares_copy_elements(&*ctx, this_now) {
        let c_now = ctx.read_native_pin(c_pin, c);
        return ctx
            .invoke_special_bytecode_only(
                "java/util/ArrayDeque",
                "copyElements",
                AD_COPY_ELEMENTS_DESC,
                &[Value::Object(Some(this_now)), Value::Object(Some(c_now))],
            )
            .map(|_| ());
    }
    for_each_by_iterator(ctx, (c_pin, c), |ctx, e| {
        let this_now = ctx.read_native_pin(this_pin, this);
        native_ad_add_last(ctx, &[Value::Object(Some(this_now)), e])?;
        Ok(())
    })
}

/// Whether [`map_for_each_entry`] asks the map `size()` first, and what the
/// answer decides.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MapSizeAsk {
    /// Not asked (`Hashtable.putAll`; `ConcurrentHashMap.putAll`, whose
    /// caller asks it itself).
    Skip,
    /// `putMapEntries`' `int s = m.size(); if (s > 0) { .. }`: an answer
    /// `<= 0` ends the copy before `entrySet()`.
    Gate,
    /// Asked, its answer otherwise unused (`TreeMap.putAll`'s `mapSize` when
    /// the sorted-map branch is not taken).
    Ask,
}

/// C3W3-2's "a program's map" ([`map_copy_jdk_questions_enabled`]): the map
/// whose code a JDK copy of `source` runs -- `source` itself when its class is
/// not the JDK's or this VM's ([`COPY_CTOR_OWN_PREFIXES`]), or the map a JDK
/// `java/util/Collections$*` wrapper (its `m`) or this VM's unmodifiable
/// wrapper ([`UNMOD_FIELD_BACKING`]) holds directly, whose `size()` /
/// `entrySet()` / iterator / entries each delegate to it one for one. `None`
/// with the switch off, for the JDK's own maps, and when
/// [`map_copy_answers_natively`] says the layout read answers exactly what
/// the questions would. Pure reads.
pub(crate) fn foreign_map_source(ctx: &dyn NativeContext, source: ObjectRef) -> Option<ObjectRef> {
    if !map_copy_jdk_questions_enabled() {
        return None;
    }
    let is_own = |ctx: &dyn NativeContext, o: ObjectRef| -> bool {
        match ctx.class_name_arc_of_id(ctx.class_id_of_object(o)) {
            Some(n) => COPY_CTOR_OWN_PREFIXES.iter().any(|p| n.starts_with(p)),
            // An unnamed class (a synthetic allocation) is this VM's.
            None => true,
        }
    };
    let target = if !is_own(ctx, source) {
        source
    } else {
        let name = ctx.class_name_arc_of_id(ctx.class_id_of_object(source))?;
        let inner = if name.starts_with("java/util/Collections$") {
            ctx.get_field_by_name(source, "m")
        } else if receiver_facts(ctx, source).has(CF_UNMOD_WRAPPER)
            && ctx.object_num_fields(source) > UNMOD_FIELD_BACKING
        {
            ctx.get_field(source, UNMOD_FIELD_BACKING)
        } else {
            return None;
        };
        match inner {
            Value::Object(Some(i)) if !is_own(ctx, i) => i,
            _ => return None,
        }
    };
    if map_copy_answers_natively(ctx, target) {
        return None;
    }
    Some(target)
}

/// Whether a program map's copy questions are all answered by JDK code the
/// natives already read by layout: its class extends `HashMap` /
/// `LinkedHashMap` / `Hashtable`, `TreeMap` or `ConcurrentHashMap`, and no
/// program class between it and that JDK map declares `size()` or
/// `entrySet()` -- the only members a copy asks the map itself (the entry set,
/// its iterator and the entries are then the JDK's own). So `class Attrs
/// extends LinkedHashMap<String, Object> {}` keeps the layout read (the C3W3-3
/// shortcut, for maps). Pure reads.
pub(crate) fn map_copy_answers_natively(ctx: &dyn NativeContext, map: ObjectRef) -> bool {
    let facts = receiver_facts(ctx, map);
    if !(facts.has(CF_BUCKET_MAP_NAME)
        || facts.has(CF_LHM)
        || facts.has(CF_TREE_MAP)
        || facts.has(CF_CHM))
    {
        return false;
    }
    let mut cid = ctx.class_id_of_object(map);
    for _ in 0..FACTS_WALK_LIMIT {
        let Some(name) = ctx.class_name_arc_of_id(cid) else {
            return false;
        };
        if COPY_CTOR_OWN_PREFIXES.iter().any(|p| name.starts_with(p)) {
            return true;
        }
        if ctx.class_declares_method(cid, "size", "()I")
            || ctx.class_declares_method(cid, "entrySet", "()Ljava/util/Set;")
        {
            return false;
        }
        match ctx.superclass_of(cid) {
            Some(s) => cid = s,
            None => return false,
        }
    }
    false
}

/// Whether `obj` is a `java.util.SortedMap` (`TreeMap.putAll`'s `map
/// instanceof SortedMap`). Pure reads.
pub(crate) fn object_is_sorted_map(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    match ctx.class_id_by_name("java/util/SortedMap") {
        Some(cid) => ctx.is_subclass(ctx.class_id_of_object(obj), cid),
        None => false,
    }
}

/// `CRATONVM_COMPAT_TREEMAP_SORTED_SOURCE` -- default ON, `=0` restores
/// [`collect_entries_any`] for a program's `SortedMap` handed to
/// `TreeMap.putAll` / `TreeMap(SortedMap)` (round 15 wave 6, lane sweepC;
/// `r15w4-compat4-bulk-ops-residuals` item 3). On: the JDK 25 questions --
/// `putAll`'s `map.size()`, then on an empty receiver `Objects.equals
/// (comparator, map.comparator())` and `buildFromSorted`'s walk
/// ([`tm_build_from_sorted_walk`]), else `AbstractMap.putAll`'s.
pub(crate) fn tm_sorted_source_questions_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_TREEMAP_SORTED_SOURCE")
}

/// `Objects.equals(a, b)`: identity, then `a.equals(b)` (its exception
/// propagates).
pub(crate) fn tm_objects_equals(
    ctx: &mut dyn NativeContext,
    a: Value,
    b: Value,
) -> Result<bool, MethodCallFailed> {
    match (a, b) {
        (Value::Object(None), Value::Object(None)) => Ok(true),
        (Value::Object(Some(x)), Value::Object(Some(y))) if x == y => Ok(true),
        (Value::Object(Some(x)), other) => Ok(matches!(
            ctx.invoke_virtual(x, "equals", "(Ljava/lang/Object;)Z", &[other])?,
            Some(Value::Int(v)) if v != 0
        )),
        _ => Ok(false),
    }
}

/// `buildFromSorted(mapSize, m.entrySet().iterator(), null, null)` over `m`
/// (rooted at `m_pin`) into the empty `this` (rooted at `this_pin`):
/// `entrySet()` and `iterator()` (asked even when `map_size <= 0`, as the
/// call's arguments are), then exactly `map_size` `next()` -- never
/// `hasNext()` -- each followed by `getKey()` / `getValue()`. A null
/// `entrySet()` / iterator / entry is the helpful NPE of the JDK line
/// (`entry_set_owner` names the static type of `m` there). Residual: each
/// pair is stored through the native `put`, which compares keys (and refuses a
/// null key under natural ordering) where `buildFromSorted` trusts the source's
/// order. Every pin this takes is released on every exit.
pub(crate) fn tm_build_from_sorted_walk(
    ctx: &mut dyn NativeContext,
    (this_pin, this): (usize, ObjectRef),
    (m_pin, m): (usize, ObjectRef),
    map_size: i32,
    entry_set_owner: &str,
) -> Result<(), MethodCallFailed> {
    let npe = |message: String| -> MethodCallFailed {
        RuntimeError::NullPointerException {
            message: Some(message),
        }
        .into()
    };
    let m_now = ctx.read_native_pin(m_pin, m);
    let set = match ctx.invoke_virtual(m_now, "entrySet", "()Ljava/util/Set;", &[])? {
        Some(Value::Object(Some(s))) => s,
        _ => {
            return Err(npe(format!(
                "Cannot invoke \"java.util.Set.iterator()\" because the return value of \
                 \"{entry_set_owner}.entrySet()\" is null"
            )))
        }
    };
    let it = match ctx.invoke_virtual(set, "iterator", "()Ljava/util/Iterator;", &[])? {
        Some(Value::Object(Some(it))) => it,
        _ if map_size <= 0 => return Ok(()),
        _ => {
            return Err(npe(
                "Cannot invoke \"java.util.Iterator.next()\" because \"it\" is null".to_string(),
            ))
        }
    };
    let it_pin = ctx.pin_native_root(it);
    // A `?` below leaves the entry's pins up; the `it_pin` release takes them.
    let walked = (|| -> Result<(), MethodCallFailed> {
        for _ in 0..map_size.max(0) {
            let it_now = ctx.read_native_pin(it_pin, it);
            let entry =
                match ctx.invoke_virtual(it_now, "next", "()Ljava/lang/Object;", &[])? {
                    Some(Value::Object(Some(e))) => e,
                    _ => return Err(npe(
                        "Cannot invoke \"java.util.Map$Entry.getKey()\" because \"entry\" is null"
                            .to_string(),
                    )),
                };
            let e_pin = ctx.pin_native_root(entry);
            let key = ctx
                .invoke_virtual(entry, "getKey", "()Ljava/lang/Object;", &[])?
                .unwrap_or(Value::Object(None));
            let k_pin = pin_value(ctx, key);
            let entry = ctx.read_native_pin(e_pin, entry);
            let value = ctx
                .invoke_virtual(entry, "getValue", "()Ljava/lang/Object;", &[])?
                .unwrap_or(Value::Object(None));
            let key = read_pinned_elem(&*ctx, k_pin, key);
            ctx.unpin_native_roots(e_pin);
            let this_now = ctx.read_native_pin(this_pin, this);
            native_tm_put(ctx, &[Value::Object(Some(this_now)), key, value])?;
        }
        Ok(())
    })();
    ctx.unpin_native_roots(it_pin);
    walked
}

/// JDK 25 `TreeMap.putAll(map)` for a program's `SortedMap` `m` (rooted at
/// `m_pin`; `this` rooted at `this_pin`), under
/// [`tm_sorted_source_questions_enabled`]: `int mapSize = map.size(); if
/// (size == 0 && mapSize != 0 && map instanceof SortedMap && Objects.equals
/// (comparator, map.comparator())) buildFromSorted(..); else
/// super.putAll(map);` -- `AbstractMap.putAll`'s walk, its `put` virtual for a
/// program's receiver.
pub(crate) fn tm_put_all_from_sorted_program_map(
    ctx: &mut dyn NativeContext,
    (this_pin, this): (usize, ObjectRef),
    (m_pin, m): (usize, ObjectRef),
) -> Result<(), MethodCallFailed> {
    let m_now = ctx.read_native_pin(m_pin, m);
    let map_size = match ctx.invoke_virtual(m_now, "size", "()I", &[])? {
        Some(Value::Int(n)) => n,
        _ => 0,
    };
    let this_now = ctx.read_native_pin(this_pin, this);
    let size = match native_tm_size(ctx, &[Value::Object(Some(this_now))])? {
        Some(Value::Int(n)) => n,
        _ => 0,
    };
    if size == 0 && map_size != 0 {
        let m_now = ctx.read_native_pin(m_pin, m);
        let theirs = ctx
            .invoke_virtual(m_now, "comparator", "()Ljava/util/Comparator;", &[])?
            .unwrap_or(Value::Object(None));
        let theirs_pin = pin_value(ctx, theirs);
        let this_now = ctx.read_native_pin(this_pin, this);
        let mine = tm_get_slot(&*ctx, this_now, TM_FIELD_COMPARATOR);
        let theirs = read_pinned_elem(&*ctx, theirs_pin, theirs);
        let same = tm_objects_equals(ctx, mine, theirs);
        if theirs_pin != usize::MAX {
            ctx.unpin_native_roots(theirs_pin);
        }
        if same? {
            return tm_build_from_sorted_walk(
                ctx,
                (this_pin, this),
                (m_pin, m),
                map_size,
                "java.util.Map",
            );
        }
    }
    let this_now = ctx.read_native_pin(this_pin, this);
    let virtual_put = put_all_virtual_put_enabled() && class_is_programs(&*ctx, this_now);
    map_for_each_entry(ctx, (m_pin, m), MapSizeAsk::Skip, |ctx, key, value| {
        let this_now = ctx.read_native_pin(this_pin, this);
        if virtual_put {
            map_put_virtual(ctx, this_now, key, value)?;
        } else {
            native_tm_put(ctx, &[Value::Object(Some(this_now)), key, value])?;
        }
        Ok(())
    })
}

/// `for (Map.Entry e : m.entrySet()) body(e.getKey(), e.getValue())` over `m`
/// (rooted at `m_pin`), after `m.size()` as `size_ask` says: `entrySet()`,
/// its `iterator()`, then per entry `hasNext()`, `next()`, `getKey()`,
/// `getValue()` and `body`, interleaved as the JDK's loops run them. A null
/// `entrySet()` is the helpful NPE of the loop's `iterator()` call, a null
/// entry that of `e.getKey()`, a null iterator the bare one. The key and
/// value reach `body` with nothing run since `getValue()` returned; `body`
/// roots what it holds across its own Java. Every pin this takes is released
/// on every exit.
pub(crate) fn map_for_each_entry(
    ctx: &mut dyn NativeContext,
    m: (usize, ObjectRef),
    size_ask: MapSizeAsk,
    body: impl FnMut(&mut dyn NativeContext, Value, Value) -> Result<(), MethodCallFailed>,
) -> Result<(), MethodCallFailed> {
    map_for_each_entry_sized(ctx, m, size_ask, |_, _| {}, body)
}

/// [`map_for_each_entry`], handing the `size()` answer (when asked, and past
/// a [`MapSizeAsk::Gate`]) to `on_size` before `entrySet()` -- where
/// `putMapEntries` presizes (C4W4-1, [`hashmap_presize_null_table`]).
/// `on_size` runs no Java.
pub(crate) fn map_for_each_entry_sized(
    ctx: &mut dyn NativeContext,
    (m_pin, m): (usize, ObjectRef),
    size_ask: MapSizeAsk,
    on_size: impl FnOnce(&mut dyn NativeContext, i32),
    mut body: impl FnMut(&mut dyn NativeContext, Value, Value) -> Result<(), MethodCallFailed>,
) -> Result<(), MethodCallFailed> {
    if size_ask != MapSizeAsk::Skip {
        let m_now = ctx.read_native_pin(m_pin, m);
        let answer = ctx.invoke_virtual(m_now, "size", "()I", &[])?;
        if size_ask == MapSizeAsk::Gate && !matches!(answer, Some(Value::Int(s)) if s > 0) {
            return Ok(());
        }
        if let Some(Value::Int(s)) = answer {
            on_size(ctx, s);
        }
    }
    let m_now = ctx.read_native_pin(m_pin, m);
    let set = match ctx.invoke_virtual(m_now, "entrySet", "()Ljava/util/Set;", &[])? {
        Some(Value::Object(Some(s))) => s,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some(
                    "Cannot invoke \"java.util.Set.iterator()\" because the return value of \
                     \"java.util.Map.entrySet()\" is null"
                        .to_string(),
                ),
            }
            .into())
        }
    };
    // Nothing runs between `entrySet()` returning and `iterator()` dispatching.
    let it = match ctx.invoke_virtual(set, "iterator", "()Ljava/util/Iterator;", &[])? {
        Some(Value::Object(Some(it))) => it,
        _ => return Err(bare_npe()),
    };
    let it_pin = ctx.pin_native_root(it);
    // A `?` below leaves the entry's pins up; the `it_pin` release after the
    // walk takes them with it.
    let walked = (|| -> Result<(), MethodCallFailed> {
        loop {
            let it_now = ctx.read_native_pin(it_pin, it);
            match ctx.invoke_virtual(it_now, "hasNext", "()Z", &[])? {
                Some(Value::Int(n)) if n != 0 => {}
                _ => return Ok(()),
            }
            let it_now = ctx.read_native_pin(it_pin, it);
            let entry =
                match ctx.invoke_virtual(it_now, "next", "()Ljava/lang/Object;", &[])? {
                    Some(Value::Object(Some(e))) => e,
                    _ => return Err(RuntimeError::NullPointerException {
                        message: Some(
                            "Cannot invoke \"java.util.Map$Entry.getKey()\" because \"e\" is null"
                                .to_string(),
                        ),
                    }
                    .into()),
                };
            let e_pin = ctx.pin_native_root(entry);
            let key = ctx
                .invoke_virtual(entry, "getKey", "()Ljava/lang/Object;", &[])?
                .unwrap_or(Value::Object(None));
            let k_pin = pin_value(ctx, key);
            let entry = ctx.read_native_pin(e_pin, entry);
            let value = ctx
                .invoke_virtual(entry, "getValue", "()Ljava/lang/Object;", &[])?
                .unwrap_or(Value::Object(None));
            let key = read_pinned_elem(&*ctx, k_pin, key);
            ctx.unpin_native_roots(e_pin);
            body(ctx, key, value)?;
        }
    })();
    ctx.unpin_native_roots(it_pin);
    walked
}

/// `Map.copyOf(m)`'s questions after its `isEmpty()`, for a program's `m`
/// ([`map_copy_jdk_questions_enabled`]): `m.entrySet().toArray(new
/// Entry[0])`, then `Map.ofEntries`' `getKey()` / `getValue()` of each entry
/// in array order. A null `entrySet()` / `toArray` answer / entry is the
/// helpful NPE the JDK line raises. The pairs come back read through their
/// pins, for the caller's `of_map` (which pins them before its checks).
pub(crate) fn map_entries_by_to_array(
    ctx: &mut dyn NativeContext,
    m: ObjectRef,
) -> Result<Vec<(Value, Value)>, MethodCallFailed> {
    let npe = |msg: &str| -> MethodCallFailed {
        RuntimeError::NullPointerException {
            message: Some(msg.to_string()),
        }
        .into()
    };
    let m_pin = ctx.pin_native_root(m);
    // Every pin below `m_pin` is released with it, on every exit.
    let collected = (|| -> Result<Vec<(Value, Value)>, MethodCallFailed> {
        let m_now = ctx.read_native_pin(m_pin, m);
        let set = match ctx.invoke_virtual(m_now, "entrySet", "()Ljava/util/Set;", &[])? {
            Some(Value::Object(Some(s))) => s,
            _ => {
                return Err(npe(
                    "Cannot invoke \"java.util.Set.toArray(Object[])\" because the return \
                     value of \"java.util.Map.entrySet()\" is null",
                ))
            }
        };
        let set_pin = ctx.pin_native_root(set);
        let component = ctx
            .class_id_by_name("java/util/Map$Entry")
            .unwrap_or(ClassId::new(0));
        let proto = ctx.new_ref_array(component, 0);
        let set = ctx.read_native_pin(set_pin, set);
        let arr = match ctx.invoke_virtual(
            set,
            "toArray",
            "([Ljava/lang/Object;)[Ljava/lang/Object;",
            &[Value::Object(Some(proto))],
        )? {
            Some(Value::Object(Some(a))) if ctx.heap_kind_of(a) == ObjectKind::Array => a,
            // Not an array (a `toArray` the verifier would have refused).
            Some(Value::Object(Some(_))) => return Ok(Vec::new()),
            _ => {
                return Err(npe(
                    "Cannot read the array length because \"entries\" is null",
                ))
            }
        };
        let len = ctx.array_length(arr);
        let arr_pin = ctx.pin_native_root(arr);
        let mut pinned: Vec<(usize, Value, usize, Value)> = Vec::with_capacity(len);
        for i in 0..len {
            let arr_now = ctx.read_native_pin(arr_pin, arr);
            let entry = match ctx.get_array_element(arr_now, i) {
                Value::Object(Some(e)) => e,
                _ if len == 1 => return Err(npe(
                    "Cannot invoke \"java.util.Map$Entry.getKey()\" because \"entries[0]\" is null",
                )),
                _ => {
                    return Err(npe(
                        "Cannot invoke \"java.util.Map$Entry.getKey()\" because \"entry\" is null",
                    ))
                }
            };
            let e_pin = ctx.pin_native_root(entry);
            let key = ctx
                .invoke_virtual(entry, "getKey", "()Ljava/lang/Object;", &[])?
                .unwrap_or(Value::Object(None));
            let k_pin = pin_value(ctx, key);
            let entry = ctx.read_native_pin(e_pin, entry);
            let value = ctx
                .invoke_virtual(entry, "getValue", "()Ljava/lang/Object;", &[])?
                .unwrap_or(Value::Object(None));
            let v_pin = pin_value(ctx, value);
            pinned.push((k_pin, key, v_pin, value));
        }
        Ok(pinned
            .iter()
            .map(|&(kp, k, vp, v)| {
                (
                    read_pinned_elem(&*ctx, kp, k),
                    read_pinned_elem(&*ctx, vp, v),
                )
            })
            .collect())
    })();
    ctx.unpin_native_roots(m_pin);
    collected
}

/// One `putVal(e.getKey(), e.getValue(), false)` of
/// `ConcurrentHashMap.putAll` into `this` (rooted at `this_pin`), for
/// [`map_for_each_entry`]: a null key or value is the bare NPE, then one
/// `hashCode()` and the segment put -- `native_chm_put_all`'s loop body.
pub(crate) fn chm_put_all_entry(
    ctx: &mut dyn NativeContext,
    (this_pin, this): (usize, ObjectRef),
    key: Value,
    value: Value,
) -> Result<(), MethodCallFailed> {
    chm_bare_npe_on_null(&key, &value)?;
    // Both are non-null objects here, so `key_pin` is this window's base.
    let key_pin = pin_value(ctx, key);
    let value_pin = pin_value(ctx, value);
    let put = (|| -> Result<(), MethodCallFailed> {
        let hash = chm_key_hash(ctx, &key)?;
        let this_now = ctx.read_native_pin(this_pin, this);
        if let Some(seg) = chm_segment_for_mut(ctx, this_now, hash) {
            let (_guard, seg) = ChmMonitorGuard::acquire_gc_safe(ctx, seg);
            let key = read_pinned_elem(&*ctx, key_pin, key);
            let value = read_pinned_elem(&*ctx, value_pin, value);
            chm_seg_put_hashed(ctx, seg, key, value, hash, false)?;
        }
        Ok(())
    })();
    if key_pin != usize::MAX {
        ctx.unpin_native_roots(key_pin);
    }
    put
}

pub(crate) fn native_al_remove_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // RULE C: `batchRemove` opens `Objects.requireNonNull(c)`.
    reject_null_collection(args.get(1))?;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    // `_or_real`, not the bare heuristic reader: `coll` is an arbitrary
    // caller-supplied Collection, and the bare reader answers EMPTY for any
    // layout it does not hand-model — which made `removeAll(foreignCollection)`
    // a silent no-op that still reported `false`. (The wrapper's own doc listed
    // `removeAll`/`retainAll` as callers before they were; fixed 2026-08-01
    // with the `AbstractCollection` interception audit.)
    //
    // `CRATONVM_COMPAT_BATCH_REMOVE_CONTAINS`: the JDK's `c.contains(e)` per
    // element instead of the snapshot below.
    if let Some(r) = al_batch_remove_by_contains(ctx, this, coll, false) {
        return r;
    }
    // The collection runs Java (`size()` / `toArray()` / an iterator), so
    // `this` is rooted across it and re-read (round 15 wave 3, lane compat3:
    // `al_state` below read the pre-call address).
    let collect_pin = ctx.pin_native_root(this);
    let collected = collect_collection_elements_or_real(ctx, coll);
    let this = ctx.read_native_pin(collect_pin, this);
    ctx.unpin_native_roots(collect_pin);
    let coll_elems = collected?;
    let (data, size) = al_state(ctx, this);
    let buf = match data {
        Some(b) => b,
        None => return Ok(Some(Value::Int(0))),
    };
    // Two-pass: read ALL elements + decide keep/remove FIRST, then write the kept
    // ones back. The previous single-pass in-place compaction (read buf[i], write
    // buf[w<i] in the same loop) over-removed *everything* whenever the backing
    // array was exactly full (`capacity == size`) — `removeAll(...)` then returned
    // an empty list (e.g. `new ArrayList<>(Arrays.asList(Errors.values())).removeAll(x)`
    // wiped all 121 elements, emptying a `@MethodSource` stream → bug-14). Doing all
    // reads before any write removes that interleaving.
    //
    // Family-1 fix (cce0079): every `list_element_matches` below dispatches
    // `equals()` (GC-capable), which left `this`/`buf`/the `coll_elems`
    // snapshot/every previously-kept raw element stale — the write-back then
    // stored pre-GC addresses into the array. Pin receiver/buffer/snapshot,
    // refresh per dispatch, and track kept INDICES rather than raw values,
    // re-reading each element from the refreshed buffer at write-back time
    // (kept indices are ascending and >= their write position, so reads
    // always happen before their slot is overwritten).
    let this_pin = ctx.pin_native_root(this);
    let buf_pin = ctx.pin_native_root(buf);
    let (_, ce_handles) = pin_value_slice(ctx, &coll_elems);
    let mut this = this;
    let mut buf = buf;
    let mut kept_idx: Vec<usize> = Vec::with_capacity(size as usize);
    let mut modified = false;
    for read_idx in 0..(size as usize) {
        let elem0 = ctx.get_array_element(buf, read_idx);
        let eh = pin_value(ctx, elem0);
        let mut elem = elem0;
        // `c.contains(elem)` — real element equality (see `list_element_matches`).
        let mut in_coll = false;
        for (ci, ce_orig) in coll_elems.iter().enumerate() {
            let ce = read_pinned_elem(ctx, ce_handles[ci], *ce_orig);
            // A throwing `equals` releases the window (`this_pin` is its base).
            let matched = list_element_matches(ctx, &ce, &elem);
            let matched = unpin_on_err(ctx, this_pin, matched)?;
            buf = ctx.read_native_pin(buf_pin, buf);
            elem = read_pinned_elem(ctx, eh, elem);
            if matched {
                in_coll = true;
                break;
            }
        }
        if eh != usize::MAX {
            ctx.unpin_native_roots(eh);
        }
        if in_coll {
            modified = true;
        } else {
            kept_idx.push(read_idx);
        }
    }
    this = ctx.read_native_pin(this_pin, this);
    buf = ctx.read_native_pin(buf_pin, buf);
    ctx.unpin_native_roots(this_pin);
    if modified {
        for (k, &idx) in kept_idx.iter().enumerate() {
            if k != idx {
                let v = ctx.get_array_element(buf, idx);
                ctx.set_array_element(buf, k, v);
            }
        }
        // Null the vacated tail (ArrayList invariant: slots >= size are null);
        // see native_al_retain_all for why stale tail elements are harmful.
        for i in kept_idx.len()..(size as usize) {
            ctx.set_array_element(buf, i, Value::Object(None));
        }
        al_set_size(ctx, this, kept_idx.len() as i32);
    }
    Ok(Some(Value::Int(if modified { 1 } else { 0 })))
}

pub(crate) fn native_al_retain_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // RULE C: `batchRemove` opens `Objects.requireNonNull(c)`. A `retainAll`
    // that reads a null argument as empty is worse than a no-op — it clears the
    // receiver.
    reject_null_collection(args.get(1))?;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Live `values()` view: a modifying retainAll must also drop the removed
    // elements' entries from the source map. Unlike native_al_remove_obj /
    // native_al_clear, this native previously never consulted
    // values_view_source at all -- a generic gap (affects retainAll() on
    // ANY Map's values() view, not just Properties') found 2026-07-26 while
    // auditing Properties.values() liveness. See properties-keyset-view-not-live-FIXED.md.
    // `CRATONVM_COMPAT_BATCH_REMOVE_CONTAINS` (see `native_al_remove_all`);
    // a live `values()` view keeps the walk below, which propagates.
    if let Some(r) = al_batch_remove_by_contains(ctx, this, coll, true) {
        return r;
    }
    // `_or_real` — see `native_al_remove_all`. A `retainAll` that reads the
    // argument as empty is worse than a no-op: it retains nothing and clears
    // the receiver. `this` is rooted across it and re-read, and the view
    // source is read from the re-read `this` (round 15 wave 3, lane compat3:
    // both were read before the collection's Java).
    let collect_pin = ctx.pin_native_root(this);
    let collected = collect_collection_elements_or_real(ctx, coll);
    let this = ctx.read_native_pin(collect_pin, this);
    ctx.unpin_native_roots(collect_pin);
    let coll_elems = collected?;
    let view_src = values_view_source(ctx, this);
    let (data, size) = al_state(ctx, this);
    let buf = match data {
        Some(b) => b,
        None => return Ok(Some(Value::Int(0))),
    };
    // Two-pass (see native_al_remove_all): read+decide before writing, so a full
    // backing array (capacity == size) is handled correctly.
    //
    // Family-1 fix (cce0079): pinned walk + kept INDICES — see
    // `native_al_remove_all` for the full rationale. `removed_idx` mirrors
    // `kept_idx` for the view-source propagation below.
    let this_pin = ctx.pin_native_root(this);
    let buf_pin = ctx.pin_native_root(buf);
    let vh = view_src.map(|v| ctx.pin_native_root(v));
    let (_, ce_handles) = pin_value_slice(ctx, &coll_elems);
    let mut this = this;
    let mut buf = buf;
    let mut kept_idx: Vec<usize> = Vec::with_capacity(size as usize);
    let mut removed_idx: Vec<usize> = Vec::new();
    let mut modified = false;
    for read_idx in 0..(size as usize) {
        let elem0 = ctx.get_array_element(buf, read_idx);
        let eh = pin_value(ctx, elem0);
        let mut elem = elem0;
        // `c.contains(elem)` — real element equality (see `list_element_matches`).
        let mut in_coll = false;
        for (ci, ce_orig) in coll_elems.iter().enumerate() {
            let ce = read_pinned_elem(ctx, ce_handles[ci], *ce_orig);
            // A throwing `equals` releases the window (`this_pin` is its base).
            let matched = list_element_matches(ctx, &ce, &elem);
            let matched = unpin_on_err(ctx, this_pin, matched)?;
            buf = ctx.read_native_pin(buf_pin, buf);
            elem = read_pinned_elem(ctx, eh, elem);
            if matched {
                in_coll = true;
                break;
            }
        }
        if eh != usize::MAX {
            ctx.unpin_native_roots(eh);
        }
        if in_coll {
            kept_idx.push(read_idx);
        } else {
            modified = true;
            removed_idx.push(read_idx);
        }
    }
    this = ctx.read_native_pin(this_pin, this);
    buf = ctx.read_native_pin(buf_pin, buf);
    let view_src = match (view_src, vh) {
        (Some(v), Some(h)) => Some(ctx.read_native_pin(h, v)),
        _ => None,
    };
    // Snapshot the removed elements' current values before compaction
    // overwrites their slots below (the compaction itself is a local array
    // shift, not GC-capable, but the propagation loop after it is).
    let removed_vals: Vec<Value> = removed_idx
        .iter()
        .map(|&i| ctx.get_array_element(buf, i))
        .collect();
    ctx.unpin_native_roots(this_pin);
    if modified {
        for (k, &idx) in kept_idx.iter().enumerate() {
            if k != idx {
                let v = ctx.get_array_element(buf, idx);
                ctx.set_array_element(buf, k, v);
            }
        }
        // Null the vacated tail so slots >= size are null (the real ArrayList
        // invariant). Leaving stale non-null elements there breaks heuristics
        // like `values_view_source`, which treats a non-null *last* buffer slot
        // as a view-source sentinel — that misfired after retainAll and made a
        // freshly-filtered list (e.g. the JSSE connector's enabled-cipher list)
        // resync to empty, throwing "None of the [ciphers] ... supported".
        for i in kept_idx.len()..(size as usize) {
            ctx.set_array_element(buf, i, Value::Object(None));
        }
        al_set_size(ctx, this, kept_idx.len() as i32);
        if let Some(source) = view_src {
            let source_pin = ctx.pin_native_root(source);
            let (_, removed_handles) = pin_value_slice(ctx, &removed_vals);
            let mut source = source;
            for (i, &orig) in removed_vals.iter().enumerate() {
                let v = read_pinned_elem(ctx, removed_handles[i], orig);
                source = ctx.read_native_pin(source_pin, source);
                let propagated = propagate_list_removal(ctx, source, v);
                unpin_on_err(ctx, source_pin, propagated)?;
            }
            ctx.unpin_native_roots(source_pin);
        }
    }
    Ok(Some(Value::Int(if modified { 1 } else { 0 })))
}

pub(crate) fn native_hs_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(ctx, args, "addAll", "(Ljava/util/Collection;)Z") {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    // `addAll(null)` reaches `AbstractCollection.addAll`, whose `for (E e : c)`
    // dereferences immediately. Answering `false` told the caller the set was
    // unchanged and hid the null. Only an explicitly-passed null throws; a
    // missing argument stays the malformed-call no-op.
    if matches!(args.get(1), Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some(
                "Cannot invoke \"java.util.Collection.iterator()\" because \"c\" is null"
                    .to_string(),
            ),
        }
        .into());
    }
    let coll = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    // GC-safety: `collect_collection_elements_or_real` drives the source's real
    // `iterator()` for a foreign collection, so it allocates and runs Java.
    // `this` is live across it and the pin below was being taken AFTERWARDS —
    // `[deadref-pin]` caught it receiving an address that named no live object
    // on BindableTests under `CRATONVM_DBG_GC_STRESS=262144`. Same shape, same
    // fix, as `native_al_add_all`.
    let this_pin = ctx.pin_native_root(this);
    // `CRATONVM_COMPAT_BULK_ADD_JDK_QUESTIONS` (round 15 wave 4, lane
    // compat4): `AbstractCollection.addAll` is `for (E e : c) if (add(e))
    // modified = true;` -- a program's `c` is walked by its own iterator, one
    // `add` per element as it arrives (no `size()` / `toArray()`).
    if bulk_add_jdk_questions_enabled() && copy_source_is_foreign(&*ctx, coll) {
        let coll_pin = ctx.pin_native_root(coll);
        let mut modified = false;
        let walked = for_each_by_iterator(ctx, (coll_pin, coll), |ctx, e| {
            let this_now = ctx.read_native_pin(this_pin, this);
            if native_hs_add(ctx, &[Value::Object(Some(this_now)), e])? == Some(Value::Int(1)) {
                modified = true;
            }
            Ok(())
        });
        ctx.unpin_native_roots(this_pin);
        walked?;
        return Ok(Some(Value::Int(i32::from(modified))));
    }
    let elems = match collect_collection_elements_or_real(ctx, coll) {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let this = ctx.read_native_pin(this_pin, this);
    // Family-1 fix (cce0079): each `native_hs_add` is GC-capable — an
    // earlier iteration's GC left `this` and every later `elems` slot stale
    // (the callee pins its own args, but was being handed already-dead
    // addresses). Pin and refresh per iteration.
    let (_, handles) = pin_value_slice(ctx, &elems);
    let mut this = this;
    let mut modified = false;
    for (i, e) in elems.iter().enumerate() {
        this = ctx.read_native_pin(this_pin, this);
        let e = read_pinned_elem(ctx, handles[i], *e);
        let result = match native_hs_add(ctx, &[Value::Object(Some(this)), e]) {
            Ok(r) => r,
            Err(err) => {
                ctx.unpin_native_roots(this_pin);
                return Err(err);
            }
        };
        if result == Some(Value::Int(1)) {
            modified = true;
        }
    }
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Int(if modified { 1 } else { 0 })))
}

pub(crate) fn native_hs_remove_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(ctx, args, "removeAll", "(Ljava/util/Collection;)Z") {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        // `AbstractSet.removeAll` opens with `Objects.requireNonNull(c)`.
        Some(Value::Object(None)) if set_bulk_jdk_enabled() => return Err(bare_npe()),
        _ => return Ok(Some(Value::Int(0))),
    };
    // `AbstractSet.removeAll`'s size test and iterator arm
    // (`CRATONVM_COMPAT_SET_BULK_JDK`); the element loop below is its other arm.
    let (this, coll, by_iterator) = if set_bulk_jdk_enabled() {
        match hs_remove_all_jdk(ctx, this, coll) {
            SetBulkStep::Done(r) => return r,
            SetBulkStep::Loop(this, coll, by_iterator) => (this, coll, by_iterator),
        }
    } else {
        (this, coll, false)
    };
    // `_or_real`: a foreign collection arg whose layout the heuristics can't
    // read must not silently extract to empty (which would make removeAll a
    // no-op). Falls back to the collection's real `toArray()`. See
    // `native_hs_contains_all` for the Weld `ImmutableTinySet` case.
    //
    // `by_iterator`: the JDK arm `for (Object e : c) remove(e)` was chosen, so
    // `c` is walked through its own `iterator()`, as that loop does (the
    // fallback's `toArray()` also asks `c.size()`, a call the JDK never makes).
    // `this` is pinned across the walk: both collectors run Java.
    let this_pin0 = ctx.pin_native_root(this);
    let coll_elems = if by_iterator {
        collect_via_real_iterator(ctx, coll)
    } else {
        collect_collection_elements_or_real(ctx, coll)
    };
    let this = ctx.read_native_pin(this_pin0, this);
    ctx.unpin_native_roots(this_pin0);
    let coll_elems = coll_elems?;
    // Family-1 fix (cce0079): same per-iteration pin refresh as
    // `native_hs_add_all`.
    let this_pin = ctx.pin_native_root(this);
    let (_, handles) = pin_value_slice(ctx, &coll_elems);
    let mut this = this;
    let mut modified = false;
    for (i, e) in coll_elems.iter().enumerate() {
        this = ctx.read_native_pin(this_pin, this);
        let e = read_pinned_elem(ctx, handles[i], *e);
        let result = match native_hs_remove(ctx, &[Value::Object(Some(this)), e]) {
            Ok(r) => r,
            Err(err) => {
                ctx.unpin_native_roots(this_pin);
                return Err(err);
            }
        };
        if result == Some(Value::Int(1)) {
            modified = true;
        }
    }
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Int(if modified { 1 } else { 0 })))
}

/// `Collection.removeIf(Predicate)` over a HashSet-layout receiver — including
/// every [`SET_VIEW_CARRIERS`] keySet/entrySet view, where removal has to write
/// THROUGH to the backing map (`native_hs_remove` does that).
///
/// Registered because `ConcurrentHashMap$EntrySetView` *declares* `removeIf`,
/// so the receiver-has-own-bytecode rule ran the JDK body —
/// `return map.removeEntryIf(filter)` — over a CratonVM-minted view whose
/// `map` field is null (the view's state is the backing set in that slot), and
/// every caller got `NullPointerException: Cannot invoke
/// "java.util.concurrent.ConcurrentHashMap.removeEntryIf(...)" because
/// "this.map" is null`. Spring's `DefaultContextCache.remove` does exactly
/// `this.contextMap.entrySet().removeIf(..)` from a `@DirtiesContext`
/// `afterTestClass` callback, which is what made three
/// `core/spring-boot-test` classes report `containersFailed=1` while every
/// test in them passed. The other carriers do not declare `removeIf` and were
/// inheriting `Collection`'s iterator-based default, which already worked —
/// registering here makes the whole family agree rather than leaving one
/// member's correctness resting on the JDK not overriding a default.
///
/// Note that `force_native_over_real_jdk_bytecode` ALREADY listed `removeIf`
/// for these classes: the gate said "prefer the native" and there was no
/// native to prefer, so the lookup fell through to the very bytecode the gate
/// exists to avoid. A gate entry is not a registration.
/// `Collection.removeIf` (which `HashSet` and `LinkedHashSet` inherit) over an
/// ordinary set, as its default body runs it:
///
/// ```java
/// final Iterator<E> each = iterator();
/// while (each.hasNext()) if (filter.test(each.next())) { each.remove(); removed = true; }
/// ```
///
/// -- `next()` and `remove()` each check the set's `modCount`, and `remove()`
/// unlinks the NODE (no `hashCode()`). The element body below removed by
/// element (a `hashCode()` and an `equals` walk per removal) and never
/// noticed a predicate that modified the set. Under
/// `CRATONVM_COMPAT_ITR_REMOVE_BY_NODE`; `None`, before any Java has run,
/// for every shape [`hs_backing_nodes`] does not walk (views included).
/// `HashIterator`'s `modCount != expectedModCount` test on the map that holds
/// the nodes. `false` when it keeps no readable `modCount` (`expected` is
/// `None`).
pub(crate) fn hs_map_mod_moved(ctx: &dyn NativeContext, map: ObjectRef, expected: Option<i32>) -> bool {
    expected.is_some() && read_map_mod_count(ctx, map) != expected
}

pub(crate) fn hs_remove_if_by_nodes(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    pred: ObjectRef,
) -> Option<MethodCallResult> {
    if !itr_remove_by_node_enabled() {
        return None;
    }
    let backing = hs_backing_map(&*ctx, this)?;
    // An ordinary set's nodes live in its backing map; a `keySet()` view's in
    // its SOURCE map (`HashMap$KeySet` inherits `removeIf` too, over a
    // `KeyIterator`). An `entrySet()` view's too, when its elements ARE the
    // nodes (`CRATONVM_COMPAT_ENTRY_SET_NODES` and
    // `CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE`): the predicate is handed the
    // node, as `EntryIterator.next()` hands it. Other views keep the element
    // body.
    let (map, elem) = set_node_source(&*ctx, backing)?;
    let (mut nodes, lhm) = hs_backing_nodes(&*ctx, map)?;
    // A reversed `LinkedKeySet` / `LinkedEntrySet` (C3-1): its iterator, and
    // so `Collection.removeIf`'s predicate, walks tail first.
    if linked_set_view_reversed(&*ctx, this) {
        nodes.reverse();
    }
    Some(remove_if_over_nodes(ctx, this, map, nodes, lhm, pred, elem))
}

/// The map whose nodes a set-shaped receiver over `backing` iterates, and what
/// its iterator hands out for each: `backing` itself (keys) for an ordinary
/// set, the SOURCE for a `keySet()` view (keys) or, when its elements are the
/// nodes (`CRATONVM_COMPAT_ENTRY_SET_NODES` with
/// `CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE`), an `entrySet()` view (the nodes).
/// `None` for any other view. The caller still asks [`hs_backing_nodes`]
/// whether the map is walkable. Pure reads.
pub(crate) fn set_node_source(ctx: &dyn NativeContext, backing: ObjectRef) -> Option<(ObjectRef, NodeElem)> {
    let Some(source) = view_backing_source(ctx, backing) else {
        return Some((backing, NodeElem::Key));
    };
    match view_backing_kind(ctx, backing) {
        VIEW_KIND_KEYSET => Some((source, NodeElem::Key)),
        VIEW_KIND_ENTRYSET
            if view_remove_by_node_enabled() && entry_view_nodes(ctx, source).is_some() =>
        {
            Some((source, NodeElem::Node))
        }
        _ => None,
    }
}

/// What a `removeIf` predicate over a map's nodes is handed for each node:
/// the key (`KeyIterator`), the value (`ValueIterator`) or the node itself
/// (`EntryIterator`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeElem {
    Key,
    Value,
    Node,
}

/// `Collection.removeIf` over a `HashIterator` of `map`'s `nodes` (as
/// [`hs_backing_nodes`] walked them, `lhm` its layout verdict, with nothing
/// run since): `next()` and `remove()` each check `map`'s `modCount`, each
/// element is `elem` of its node, and a condemned node is unlinked as
/// `removeNode(p.hash, p.key, ..)` does ([`map_remove_node_by_walk`]: no
/// `hashCode()`). `anchor` is the receiver, rooted with everything else for
/// the whole walk.
#[allow(clippy::too_many_arguments)]
pub(crate) fn remove_if_over_nodes(
    ctx: &mut dyn NativeContext,
    anchor: ObjectRef,
    map: ObjectRef,
    nodes: Vec<ObjectRef>,
    lhm: bool,
    pred: ObjectRef,
    elem: NodeElem,
) -> MethodCallResult {
    let this = anchor;
    let node_vals: Vec<Value> = nodes.iter().map(|n| Value::Object(Some(*n))).collect();
    let fail_fast = map_itr_failfast_enabled();
    let mut expected = read_map_mod_count(&*ctx, map);
    let this_pin = ctx.pin_native_root(this);
    let map_pin = ctx.pin_native_root(map);
    let pred_pin = ctx.pin_native_root(pred);
    let (_, node_pins) = pin_value_slice(ctx, &node_vals);
    // Every pin above `this_pin` is released with it, on every exit.
    let result = (|| -> MethodCallResult {
        let mut modified = false;
        for (i, nv) in node_vals.iter().enumerate() {
            // `next()`'s comodification check.
            let map_now = ctx.read_native_pin(map_pin, map);
            if fail_fast && hs_map_mod_moved(&*ctx, map_now, expected) {
                return Err(RuntimeError::ConcurrentModificationException.into());
            }
            let Value::Object(Some(node)) = read_pinned_elem(ctx, node_pins[i], *nv) else {
                continue;
            };
            let arg = match elem {
                NodeElem::Key => either_node_key(&*ctx, node, lhm),
                NodeElem::Value if lhm => ctx.get_field(node, LHM_NODE_VALUE),
                NodeElem::Value => get_node_value(&*ctx, node),
                NodeElem::Node => Value::Object(Some(node)),
            };
            let p = ctx.read_native_pin(pred_pin, pred);
            let verdict = ctx.invoke_virtual(p, "test", "(Ljava/lang/Object;)Z", &[arg])?;
            if !matches!(verdict, Some(Value::Int(v)) if v != 0) {
                continue;
            }
            // `remove()`'s.
            let map_now = ctx.read_native_pin(map_pin, map);
            if fail_fast && hs_map_mod_moved(&*ctx, map_now, expected) {
                return Err(RuntimeError::ConcurrentModificationException.into());
            }
            let node = ctx.read_native_pin(node_pins[i], node);
            modified |= map_remove_node_by_walk(ctx, map_now, node, lhm)?;
            // `expectedModCount = modCount`.
            let map_now = ctx.read_native_pin(map_pin, map);
            expected = read_map_mod_count(&*ctx, map_now);
        }
        Ok(Some(Value::Int(i32::from(modified))))
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_hs_remove_if(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // RULE F, ahead of the route — see the note in `native_hs_for_each`.
    reject_null_functional(args.get(1))?;
    if let Some(r) = ksv_route(ctx, args, native_ksv_remove_if) {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let pred = match args.get(1) {
        Some(Value::Object(Some(p))) => *p,
        _ => return Ok(Some(Value::Int(0))),
    };
    if let Some(result) = hs_remove_if_by_nodes(ctx, this, pred) {
        return result;
    }
    // GC-safety: `test` and the write-through `native_hs_remove` both run
    // arbitrary Java. Pin the receiver, the predicate and every element, and
    // re-read each through its pin per iteration. The body is a closure so an
    // early `?` cannot skip the unpin.
    let this_pin = ctx.pin_native_root(this);
    let pred_pin = ctx.pin_native_root(pred);
    let result = (|| -> MethodCallResult {
        let mut this_now = ctx.read_native_pin(this_pin, this);
        resync_view_set(ctx, &mut this_now)?;
        let this_now = ctx.read_native_pin(this_pin, this);
        let backing = match hs_backing_map(ctx, this_now) {
            Some(m) => m,
            None => return Ok(Some(Value::Int(0))),
        };
        // For an entrySet-kind view these "keys" ARE the `Map.Entry` objects,
        // which is what the predicate is handed and what `native_hs_remove`
        // resolves back to a source-map key.
        let elems = hs_view_elements(ctx, this, backing)?;
        let (_, handles) = pin_value_slice(ctx, &elems);
        let mut modified = false;
        for (i, elem) in elems.iter().enumerate() {
            let p = ctx.read_native_pin(pred_pin, pred);
            let e = read_pinned_elem(ctx, handles[i], *elem);
            let verdict = ctx.invoke_virtual(p, "test", "(Ljava/lang/Object;)Z", &[e])?;
            if !matches!(verdict, Some(Value::Int(1))) {
                continue;
            }
            let this_now = ctx.read_native_pin(this_pin, this);
            let e = read_pinned_elem(ctx, handles[i], *elem);
            if native_hs_remove(ctx, &[Value::Object(Some(this_now)), e])? == Some(Value::Int(1)) {
                modified = true;
            }
        }
        Ok(Some(Value::Int(i32::from(modified))))
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

/// `CRATONVM_COMPAT_SET_BULK_JDK` — default ON, `=0` restores the old
/// bodies. `HashSet` / `LinkedHashSet` inherit `retainAll` from
/// `AbstractCollection` and `removeAll` from `AbstractSet`:
///
/// * `retainAll(c)` iterates the set and asks `c.contains(e)` -- the
///   ARGUMENT's own membership (a `TreeSet` with a comparator, an identity
///   set, a `HashSet`) -- then drops the element with `Iterator.remove()`,
///   which unlinks the node without a `hashCode()`. The native compared every
///   element against every element of `c` with `equals` (O(n*m), and the wrong
///   predicate for any `c` whose `contains` is not `equals`), then removed by
///   element (a `hashCode()` and a chain walk each).
/// * `removeAll(c)` throws `NullPointerException` for a null `c` (the native
///   answered `false`), and when `size() <= c.size()` it is the same iterator
///   walk with `c.contains(e)`; only a larger set removes each element of `c`,
///   which is all the native ever did.
///
/// Round 13 wave 8, lane hashcompat3 (`R13ShadowHashSetFamily` scenario 4: 72
/// `hashCode()` calls where HotSpot makes 68).
pub(crate) fn set_bulk_jdk_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_SET_BULK_JDK")
}

/// The nodes of an ordinary `HashSet` / `LinkedHashSet` backing map in the
/// set's iteration order, and whether the backing is a `LinkedHashMap`.
/// `None` for every other shape -- a keySet/entrySet view, an integer-overlay
/// map, a CHM / TreeMap / IdentityHashMap / Hashtable-family backing, or a
/// table holding more nodes than its `size` allows (a cycle) -- which keeps
/// the element-keyed bodies. Pure reads: no Java, no allocation.
pub(crate) fn hs_backing_nodes(ctx: &dyn NativeContext, backing: ObjectRef) -> Option<(Vec<ObjectRef>, bool)> {
    if view_backing_source(ctx, backing).is_some() {
        return None;
    }
    let mut nodes = Vec::new();
    if is_lhm_receiver(ctx, backing) {
        let (_, size, _) = lhm_state(ctx, backing);
        let cap = (size.max(0) as usize).saturating_add(64);
        let mut cur = lhm_get(ctx, backing, "head", LHM_FIELD_HEAD);
        while let Value::Object(Some(n)) = cur {
            if nodes.len() >= cap {
                return None;
            }
            nodes.push(n);
            cur = ctx.get_field(n, LHM_NODE_AFTER);
        }
        return Some((nodes, true));
    }
    let facts = receiver_facts(ctx, backing);
    if !facts.has(CF_BUCKET_MAP_NAME)
        || facts.has(CF_CHM)
        || facts.has(CF_TREE_MAP)
        || facts.has(CF_IDENTITY_MAP)
        || facts.has(CF_UNMOD_WRAPPER)
        || facts.has(CF_HASHTABLE_ANCESTRY)
        || hm_int_fast_len(ctx, backing).is_some()
    {
        return None;
    }
    let (buckets, size, cap) = map_state(ctx, backing);
    let limit = (size.max(0) as usize).saturating_add(64);
    if let Some(b) = buckets {
        for i in map_bucket_scan_order(ctx, backing, cap as usize) {
            let mut nv = ctx.get_array_element(b, i);
            while let Value::Object(Some(n)) = nv {
                if nodes.len() >= limit {
                    return None;
                }
                nodes.push(n);
                nv = ctx.get_field(n, NODE_FIELD_NEXT);
            }
        }
    }
    Some((nodes, false))
}

/// The iterator arm of `AbstractCollection.retainAll` (`keep_contained`) and
/// `AbstractSet.removeAll` (`!keep_contained`) over an ordinary set: for each
/// element in iteration order, `c.contains(e)` is asked of the ARGUMENT, and
/// the node the verdict condemns is unlinked by identity, as
/// `Iterator.remove()` does. `None` (before any Java has run, so the caller's
/// references are still current) when the set is not a shape
/// [`hs_backing_nodes`] accepts.
pub(crate) fn hs_filter_by_contains(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    coll: ObjectRef,
    keep_contained: bool,
) -> Option<MethodCallResult> {
    let backing = hs_backing_map(&*ctx, this)?;
    // A `keySet()` / `entrySet()` view walks its SOURCE's nodes
    // (`CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE`; see `set_node_source`).
    let (map, elem_kind) = if view_backing_source(&*ctx, backing).is_some() {
        if !view_remove_by_node_enabled() {
            return None;
        }
        set_node_source(&*ctx, backing)?
    } else {
        (backing, NodeElem::Key)
    };
    let view = !std::ptr::eq(map.as_ptr(), backing.as_ptr());
    let (mut nodes, lhm) = hs_backing_nodes(&*ctx, map)?;
    // A reversed `LinkedKeySet` / `LinkedEntrySet` (C3-1) asks `c.contains`
    // tail first, as its iterator walks.
    if linked_set_view_reversed(&*ctx, this) {
        nodes.reverse();
    }
    let node_vals: Vec<Value> = nodes.iter().map(|n| Value::Object(Some(*n))).collect();
    let this_pin = ctx.pin_native_root(this);
    let coll_pin = ctx.pin_native_root(coll);
    let map_pin = ctx.pin_native_root(map);
    let (_, node_pins) = pin_value_slice(ctx, &node_vals);
    // Every pin above `this_pin` is released with it, on every exit.
    let result = (|| -> MethodCallResult {
        let mut modified = false;
        for (i, nv) in node_vals.iter().enumerate() {
            let Value::Object(Some(node)) = read_pinned_elem(ctx, node_pins[i], *nv) else {
                continue;
            };
            let elem = match elem_kind {
                NodeElem::Node => Value::Object(Some(node)),
                _ => either_node_key(&*ctx, node, lhm),
            };
            let coll_now = ctx.read_native_pin(coll_pin, coll);
            let verdict =
                ctx.invoke_virtual(coll_now, "contains", "(Ljava/lang/Object;)Z", &[elem])?;
            let contained = matches!(verdict, Some(Value::Int(v)) if v != 0);
            if contained == keep_contained {
                continue;
            }
            let node = ctx.read_native_pin(node_pins[i], node);
            let this_now = ctx.read_native_pin(this_pin, this);
            let backing = if view {
                ctx.read_native_pin(map_pin, map)
            } else {
                match hs_backing_map(&*ctx, this_now) {
                    Some(b) => b,
                    None => continue,
                }
            };
            // `Iterator.remove()` is `removeNode(p.hash, p.key, ..)`, whose
            // walk asks same-hash nodes ahead of `p` `p.key.equals(..)`
            // (`CRATONVM_COMPAT_ITR_REMOVE_BY_NODE`).
            let unlinked = if itr_remove_by_node_enabled() {
                map_remove_node_by_walk(ctx, backing, node, lhm)?
            } else if lhm {
                lhm_unlink_node_movable(ctx, backing, node, false)
            } else {
                map_unlink_node_movable(ctx, backing, node, false)
            };
            modified |= unlinked;
        }
        Ok(Some(Value::Int(i32::from(modified))))
    })();
    ctx.unpin_native_roots(this_pin);
    Some(result)
}

/// What [`hs_remove_all_jdk`] leaves to `native_hs_remove_all`'s element loop.
pub(crate) enum SetBulkStep {
    /// The JDK-shaped body answered.
    Done(MethodCallResult),
    /// Run the element loop (`for (Object e : c) remove(e)`) over these
    /// receiver and argument references, current after any Java this ran;
    /// `true` when the JDK's size test chose that loop, so `c` is walked by
    /// its own iterator (`false`: the old element collection, for a shape the
    /// JDK body does not serve here).
    Loop(ObjectRef, ObjectRef, bool),
}

/// `AbstractSet.removeAll`'s size test and iterator arm (see
/// [`set_bulk_jdk_enabled`]). The set's size is its node count; `c.size()` is
/// asked virtually, as the JDK does.
pub(crate) fn hs_remove_all_jdk(ctx: &mut dyn NativeContext, this: ObjectRef, coll: ObjectRef) -> SetBulkStep {
    // A view's size is its source's (`CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE`,
    // the same selection `hs_filter_by_contains` makes).
    let node_map = hs_backing_map(&*ctx, this).and_then(|b| {
        if view_backing_source(&*ctx, b).is_none() {
            Some(b)
        } else if view_remove_by_node_enabled() {
            set_node_source(&*ctx, b).map(|(m, _)| m)
        } else {
            None
        }
    });
    let own_size = match node_map.and_then(|m| hs_backing_nodes(&*ctx, m)) {
        Some((nodes, _)) => nodes.len(),
        None => return SetBulkStep::Loop(this, coll, false),
    };
    let this_pin = ctx.pin_native_root(this);
    let coll_pin = ctx.pin_native_root(coll);
    let other_size = ctx.invoke_virtual(coll, "size", "()I", &[]);
    let this = ctx.read_native_pin(this_pin, this);
    let coll = ctx.read_native_pin(coll_pin, coll);
    ctx.unpin_native_roots(this_pin);
    let other_size = match other_size {
        Ok(Some(Value::Int(n))) => n,
        Ok(_) => return SetBulkStep::Loop(this, coll, false),
        Err(e) => return SetBulkStep::Done(Err(e)),
    };
    if own_size as i64 > i64::from(other_size) {
        return SetBulkStep::Loop(this, coll, true);
    }
    match hs_filter_by_contains(ctx, this, coll, false) {
        Some(r) => SetBulkStep::Done(r),
        None => SetBulkStep::Loop(this, coll, false),
    }
}

pub(crate) fn native_hs_retain_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A real `CopyOnWriteArraySet` is served by its own bytecode over the
    // `CopyOnWriteArrayList` in `al`, not by this file's map surface.
    // See [`cow_set_route`].
    if let Some(r) = cow_set_route(ctx, args, "retainAll", "(Ljava/util/Collection;)Z") {
        return r;
    }
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    // `retainAll(null)` throws too, but with NO message: the JDK reaches
    // `Objects.requireNonNull(c)` here rather than a dereference, which is the
    // distinction this file's two refusal helpers draw.
    if matches!(args.get(1), Some(Value::Object(None))) {
        return Err(bare_npe());
    }
    let coll = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    // `AbstractCollection.retainAll`: `c.contains(e)`, then `Iterator.remove()`
    // (`CRATONVM_COMPAT_SET_BULK_JDK`). `None` means no Java has run.
    if set_bulk_jdk_enabled() {
        if let Some(r) = hs_filter_by_contains(ctx, this, coll, true) {
            return r;
        }
    }
    // `_or_real`: a foreign collection arg whose layout the heuristics can't
    // read must not silently extract to empty — for retainAll that would empty
    // the whole set (keep nothing). Falls back to the collection's real
    // `toArray()`. See `native_hs_contains_all` for the Weld `ImmutableTinySet`
    // case. `this` is rooted across the collection's Java and re-read (round 15
    // wave 4, lane compat4: its backing map was read at the pre-call address).
    let collect_pin = ctx.pin_native_root(this);
    let collected = collect_collection_elements_or_real(ctx, coll);
    let this = ctx.read_native_pin(collect_pin, this);
    ctx.unpin_native_roots(collect_pin);
    let coll_elems = collected?;
    // Get current HashSet elements (keys of backing HashMap)
    let current = match hs_backing_map(ctx, this) {
        Some(m) => map_collect_keys(ctx, m),
        None => Vec::new(),
    };
    // Family-1 fix (cce0079): both the equality dispatches and the reentrant
    // `native_hs_remove` are GC-capable — pin `this` and both snapshot
    // vectors, refreshing every operand per use.
    let this_pin = ctx.pin_native_root(this);
    let (_, ce_handles) = pin_value_slice(ctx, &coll_elems);
    let (_, cur_handles) = pin_value_slice(ctx, &current);
    let mut this = this;
    let mut modified = false;
    for (i, e_orig) in current.iter().enumerate() {
        let mut e = read_pinned_elem(ctx, cur_handles[i], *e_orig);
        // `coll.contains(e)` — real element equality (see `list_element_matches`).
        let mut should_keep = false;
        for (ci, ce_orig) in coll_elems.iter().enumerate() {
            let ce = read_pinned_elem(ctx, ce_handles[ci], *ce_orig);
            // A throwing `equals` releases the window (`this_pin` is its base).
            let matched = list_element_matches(ctx, &ce, &e);
            let matched = unpin_on_err(ctx, this_pin, matched)?;
            e = read_pinned_elem(ctx, cur_handles[i], e);
            if matched {
                should_keep = true;
                break;
            }
        }
        if !should_keep {
            this = ctx.read_native_pin(this_pin, this);
            if let Err(err) = native_hs_remove(ctx, &[Value::Object(Some(this)), e]) {
                ctx.unpin_native_roots(this_pin);
                return Err(err);
            }
            modified = true;
        }
    }
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Int(if modified { 1 } else { 0 })))
}

pub(crate) fn native_ll_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `addAll(size, c)`: the index is always valid, then `c.toArray()`
    // (C4W4-6, row 1).
    reject_null_collection_asked(&*ctx, args.get(1), NULL_ARG_LL_ADD_ALL, NULL_C_TO_ARRAY)?;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    // `_or_real` — see `native_al_remove_all`. `native_al_add_all` already used
    // it; this sibling did not, so `linkedList.addAll(foreignCollection)` added
    // nothing and reported `false`.
    //
    // `CRATONVM_COMPAT_BULK_ADD_JDK_QUESTIONS` (round 15 wave 4, lane
    // compat4): `addAll(size, c)` opens `Object[] a = c.toArray()`, so a
    // program's `c` is asked exactly that, once. `this` is rooted across the
    // collection's Java (it was pinned only after it), and every `?` below
    // releases this frame's pins first.
    let this_pin = ctx.pin_native_root(this);
    let collected = if bulk_add_jdk_questions_enabled() && copy_source_is_foreign(&*ctx, coll) {
        foreign_to_array_elements(ctx, coll)
    } else {
        collect_collection_elements_or_real(ctx, coll)
    };
    let elems = unpin_on_err(ctx, this_pin, collected)?;
    if elems.is_empty() {
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Int(0)));
    }
    // Family-1 fix (cce0079): every `ll_link_last` allocates a node — an
    // earlier iteration's GC left `this` and every later `elems` slot stale.
    // Pin and refresh per iteration.
    let (_, handles) = pin_value_slice(ctx, &elems);
    let mut this = this;
    for (i, e) in elems.iter().enumerate() {
        this = ctx.read_native_pin(this_pin, this);
        let e = read_pinned_elem(ctx, handles[i], *e);
        let linked = ll_link_last(ctx, this, e);
        unpin_on_err(ctx, this_pin, linked)?;
    }
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Int(1)))
}

/// Rebuild a deque from `keep`, through the natives that own its four slots.
///
/// `clear` + `addLast` rather than an in-place compaction: the ring buffer's
/// `head`/`tail`/`size` triple is exactly what a partial in-place edit gets
/// wrong, and `native_ad_clear` already resets all three consistently.
///
/// Round 15 wave 6 (lane sweepC; `r15w4-compat4-bulk-ops-residuals` item 2):
/// under [`deque_bulk_remove_jdk_enabled`] the survivors are compacted IN
/// PLACE as JDK 25's `bulkRemoveModified` leaves them -- `head` kept, survivor
/// `j` at `(head + j) % capacity`, then `circularClear(es, tail = w, end)` --
/// where the rebuild reset `head` to 0 (same elements and capacity; the
/// `head` / `tail` fields and the raw buffer differ, which reflection sees).
/// Nothing here allocates, so `keep` (read from pins after the last Java)
/// stays valid throughout.
pub(crate) fn ad_rebuild(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    keep: &[Value],
) -> Result<(), MethodCallFailed> {
    if deque_bulk_remove_jdk_enabled() {
        let (data, head, tail, _size) = ad_state(&*ctx, this);
        if let Some(buf) = data {
            let cap = ctx.array_length(buf);
            if cap > 0 && keep.len() < cap && head >= 0 && tail >= 0 {
                let head = head as usize % cap;
                for (j, e) in keep.iter().enumerate() {
                    ctx.set_array_element(buf, (head + j) % cap, *e);
                }
                let new_tail = (head + keep.len()) % cap;
                let end = tail as usize % cap;
                let mut i = new_tail;
                while i != end {
                    ctx.set_array_element(buf, i, Value::Object(None));
                    i = (i + 1) % cap;
                }
                ctx.set_field(this, AD_FIELD_TAIL, Value::Int(new_tail as i32));
                return Ok(());
            }
        }
    }
    let this_pin = ctx.pin_native_root(this);
    let (_, handles) = pin_value_slice(ctx, keep);
    let this_cur = ctx.read_native_pin(this_pin, this);
    let cleared = native_ad_clear(ctx, &[Value::Object(Some(this_cur))]);
    if let Err(e) = cleared {
        ctx.unpin_native_roots(this_pin);
        return Err(e);
    }
    for (i, e) in keep.iter().enumerate() {
        let this_cur = ctx.read_native_pin(this_pin, this);
        let e = read_pinned_elem(ctx, handles[i], *e);
        if let Err(err) = native_ad_add_last(ctx, &[Value::Object(Some(this_cur)), e]) {
            ctx.unpin_native_roots(this_pin);
            return Err(err);
        }
    }
    ctx.unpin_native_roots(this_pin);
    Ok(())
}

/// `ArrayDeque.removeAll(Collection)` / `retainAll(Collection)`.
///
/// `retain` selects which side of the membership test survives.
pub(crate) fn ad_batch_remove(ctx: &mut dyn NativeContext, args: &[Value], retain: bool) -> MethodCallResult {
    reject_null_collection(args.get(1))?;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    // `CRATONVM_COMPAT_DEQUE_BULK_REMOVE_JDK`: the JDK's `c.contains(e)` per
    // element (round 15 wave 4, lane compat4). `None`: nothing has run.
    if let Some(r) = ad_bulk_remove_by_contains(ctx, this, coll, retain) {
        return r;
    }
    // The collection runs Java (`size()` / `toArray()` / an iterator), so
    // `this` is rooted across it and re-read before its elements are read
    // (round 15 wave 4, lane compat4: they were read at the pre-call address).
    let collect_pin = ctx.pin_native_root(this);
    let collected = collect_collection_elements_or_real(ctx, coll);
    let this = ctx.read_native_pin(collect_pin, this);
    ctx.unpin_native_roots(collect_pin);
    let coll_elems = collected?;
    let elems = ad_collect_elements(ctx, this);
    // GC-SAFETY: `list_element_matches` dispatches `equals` and is a GC point,
    // so the receiver and both element vectors go stale across every test.
    let this_pin = ctx.pin_native_root(this);
    let (_, e_handles) = pin_value_slice(ctx, &elems);
    let (_, c_handles) = pin_value_slice(ctx, &coll_elems);
    let mut kept_idx: Vec<usize> = Vec::with_capacity(elems.len());
    for (i, e0) in elems.iter().enumerate() {
        let mut found = false;
        for (ci, c0) in coll_elems.iter().enumerate() {
            let e = read_pinned_elem(ctx, e_handles[i], *e0);
            let c = read_pinned_elem(ctx, c_handles[ci], *c0);
            match list_element_matches(ctx, &c, &e) {
                Ok(true) => {
                    found = true;
                    break;
                }
                Ok(false) => {}
                Err(err) => {
                    ctx.unpin_native_roots(this_pin);
                    return Err(err);
                }
            }
        }
        if found == retain {
            kept_idx.push(i);
        }
    }
    // Family-1: `keep` must be built from the pins AFTER the last GC point, not
    // accumulated during the loop -- an `equals` two iterations later moves an
    // element already pushed, and the rebuild would store the pre-move address.
    let keep: Vec<Value> = kept_idx
        .iter()
        .map(|&i| read_pinned_elem(ctx, e_handles[i], elems[i]))
        .collect();
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if keep.len() == elems.len() {
        return Ok(Some(Value::Int(0)));
    }
    ad_rebuild(ctx, this, &keep)?;
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_ad_remove_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ad_batch_remove(ctx, args, false)
}

pub(crate) fn native_ad_retain_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ad_batch_remove(ctx, args, true)
}

pub(crate) fn native_ad_remove_if(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // RULE F: `Collection.removeIf` opens `Objects.requireNonNull(filter)`.
    reject_null_functional(args.get(1))?;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let filter = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    // `CRATONVM_COMPAT_DEQUE_BULK_REMOVE_JDK`: the same `bulkRemove`, with
    // its `end != tail` comodification check (round 15 wave 4, lane compat4).
    if deque_bulk_remove_jdk_enabled() {
        let filter_pin = ctx.pin_native_root(filter);
        let removed = ad_bulk_remove_by(ctx, this, |ctx, e| {
            let f = ctx.read_native_pin(filter_pin, filter);
            Ok(matches!(
                ctx.invoke_virtual(f, "test", "(Ljava/lang/Object;)Z", &[e])?,
                Some(Value::Int(v)) if v != 0
            ))
        });
        ctx.unpin_native_roots(filter_pin);
        return removed;
    }
    let elems = ad_collect_elements(ctx, this);
    // GC-SAFETY: `test` runs arbitrary user bytecode.
    let this_pin = ctx.pin_native_root(this);
    let filter_pin = ctx.pin_native_root(filter);
    let (_, e_handles) = pin_value_slice(ctx, &elems);
    let mut kept_idx: Vec<usize> = Vec::with_capacity(elems.len());
    for (i, e0) in elems.iter().enumerate() {
        let _ = e0;
        let f = ctx.read_native_pin(filter_pin, filter);
        let e = read_pinned_elem(ctx, e_handles[i], elems[i]);
        let verdict = ctx.invoke_virtual(f, "test", "(Ljava/lang/Object;)Z", &[e]);
        let drop = match verdict {
            Ok(Some(Value::Int(1))) => true,
            Ok(_) => false,
            Err(err) => {
                ctx.unpin_native_roots(this_pin);
                return Err(err);
            }
        };
        if !drop {
            kept_idx.push(i);
        }
    }
    // Family-1: see `ad_batch_remove`.
    let keep: Vec<Value> = kept_idx
        .iter()
        .map(|&i| read_pinned_elem(ctx, e_handles[i], elems[i]))
        .collect();
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if keep.len() == elems.len() {
        return Ok(Some(Value::Int(0)));
    }
    ad_rebuild(ctx, this, &keep)?;
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_ad_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `(s = size()) + c.size() + 1 - elements.length` (C4W4-6, row 2).
    reject_null_collection_asked(&*ctx, args.get(1), NULL_ARG_AD_ADD_ALL, NULL_C_SIZE)?;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    // `CRATONVM_COMPAT_BULK_ADD_JDK_QUESTIONS`: a program's collection is
    // asked `size()`, then walked (round 15 wave 4, lane compat4).
    if bulk_add_jdk_questions_enabled() && copy_source_is_foreign(&*ctx, coll) {
        return ad_add_all_by_iterator(ctx, this, coll);
    }
    // `_or_real` — see `native_al_remove_all`. Same gap as `native_ll_add_all`:
    // `arrayDeque.addAll(foreignCollection)` added nothing. `this` is rooted
    // across the collection's Java (round 15 wave 4, lane compat4: it was
    // pinned only after it).
    let this_pin = ctx.pin_native_root(this);
    let collected = collect_collection_elements_or_real(ctx, coll);
    let elems = unpin_on_err(ctx, this_pin, collected)?;
    if elems.is_empty() {
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Int(0)));
    }
    // Family-1 fix (cce0079): every `native_ad_add_last` can grow the ring
    // buffer (GC-capable) — pin and refresh `this` + pending elements per
    // iteration.
    let (_, handles) = pin_value_slice(ctx, &elems);
    // `grow(needed)` ahead of the copy (`CRATONVM_COMPAT_BULK_ADD_JDK_QUESTIONS`);
    // every reference used below is pinned and re-read.
    if bulk_add_jdk_questions_enabled() {
        let mut this_now = ctx.read_native_pin(this_pin, this);
        ad_pregrow(ctx, &mut this_now, elems.len() as i64);
    }
    let mut this = this;
    for (i, e) in elems.iter().enumerate() {
        this = ctx.read_native_pin(this_pin, this);
        let e = read_pinned_elem(ctx, handles[i], *e);
        if let Err(err) = native_ad_add_last(ctx, &[Value::Object(Some(this)), e]) {
            ctx.unpin_native_roots(this_pin);
            return Err(err);
        }
    }
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Int(1)))
}

// ===========================================================================
// Queue/Deque interface method registrations
// ===========================================================================

pub(crate) fn register_queue_deque_interface_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    // --- java/util/Queue interface ---
    registry.register(
        "java/util/Queue",
        "offer",
        "(Ljava/lang/Object;)Z",
        native_ad_offer,
    );
    registry.register(
        "java/util/Queue",
        "poll",
        "()Ljava/lang/Object;",
        native_ad_poll_first,
    );
    registry.register(
        "java/util/Queue",
        "peek",
        "()Ljava/lang/Object;",
        native_ad_peek_first,
    );
    registry.register(
        "java/util/Queue",
        "add",
        "(Ljava/lang/Object;)Z",
        native_ad_add,
    );
    registry.register(
        "java/util/Queue",
        "remove",
        "()Ljava/lang/Object;",
        native_ad_remove_first,
    );
    registry.register(
        "java/util/Queue",
        "element",
        "()Ljava/lang/Object;",
        native_ad_get_first,
    );
    registry.register("java/util/Queue", "size", "()I", native_ad_size);
    registry.register("java/util/Queue", "isEmpty", "()Z", native_ad_is_empty);

    // --- java/util/Deque interface ---
    registry.register(
        "java/util/Deque",
        "addFirst",
        "(Ljava/lang/Object;)V",
        native_ad_add_first,
    );
    registry.register(
        "java/util/Deque",
        "addLast",
        "(Ljava/lang/Object;)V",
        native_ad_add_last,
    );
    registry.register(
        "java/util/Deque",
        "removeFirst",
        "()Ljava/lang/Object;",
        native_ad_remove_first,
    );
    registry.register(
        "java/util/Deque",
        "removeLast",
        "()Ljava/lang/Object;",
        native_ad_remove_last,
    );
    registry.register(
        "java/util/Deque",
        "peekFirst",
        "()Ljava/lang/Object;",
        native_ad_peek_first,
    );
    registry.register(
        "java/util/Deque",
        "peekLast",
        "()Ljava/lang/Object;",
        native_ad_peek_last,
    );
    registry.register(
        "java/util/Deque",
        "push",
        "(Ljava/lang/Object;)V",
        native_ad_add_first,
    );
    registry.register(
        "java/util/Deque",
        "pop",
        "()Ljava/lang/Object;",
        native_ad_remove_first,
    );
    registry.register("java/util/Deque", "size", "()I", native_ad_size);
    registry.register("java/util/Deque", "isEmpty", "()Z", native_ad_is_empty);

    // `java/util/ArrayDeque$Itr` RETIRED 2026-09-02 by the iterator-carrier
    // census (`iterator-carrier-census-20260902.md`).
    // `hasNext`/`next`/`remove` were bound here to the 2-field snapshot
    // pattern for a class this crate stopped minting on 2026-08-30, when
    // ArrayDeque's iterator became the real `DeqIterator`.
    //
    // The comment that stood here explained why that is dangerous rather than
    // merely dead, and it is kept because the reasoning outlives the rows:
    // `java/util/PriorityQueue$Itr` sat in this same loop describing a 2-field
    // shape the real class has never had, inert because nothing minted it —
    // and the moment `native_pq_iterator` started minting the real class the
    // dormant row won the slot over the `native_al_itr_*` registration that
    // matched the shape actually produced (`owns=True inv=4` here against
    // `owns=False inv=0` there), and iteration reported every queue EMPTY.
    // A registration keyed on a class nobody produces is a trap armed for
    // whoever produces one later, and the dump shows it as a duplicate only
    // once the class exists.
    //
    // It was armed. During the census a first attempt at an ArrayDeque
    // iterator for `--synthetic-jdk` minted `ArrayDeque$Itr` and was reverted.
    //
    // Retired on evidence the deferral asked for and did not have:
    //   * nothing mints the class — no allocation site names it, and
    //     `apps/probes/ItrCarrierCensus` shows the runtime handing out the real
    //     `java/util/ArrayDeque$DeqIterator`;
    //   * the only caller of these natives was `vm::tests::array_deque_iterator`,
    //     which drives them on a hand-built receiver — and it fails at its FIRST
    //     step (`ArrayDeque.iterator() not registered`), stale from the same
    //     2026-08-30 change, unnoticed because its whole
    //     `#[cfg(all(test, feature = "synthetic-jdk"))]` module had not compiled
    //     since `InlineSite` grew a field.
    // So the rows had no reachable consumer at all, and removing them breaks
    // nothing that was working.
    registry.set_category(__prev_cat);
}

/// Generic snapshot-based iterator: field 0 = Object[] snapshot, field 1 = Int cursor.
///
/// Real-JDK fallback: this native is registered at the `java/util/Iterator`
/// *interface* level (see `register_iterator_protocol_natives`) so it
/// otherwise intercepts every real iterator (e.g. `ArrayList$Itr`, whose
/// field 0 is an `int cursor`, not an `Object[]`). When field 0 isn't an
/// array, dispatch to the receiver's concrete bytecode by name.
pub(crate) fn native_snapshot_itr_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    // A key/entry-iterator carrier keeps its snapshot at `key_itr_base`, not at
    // slot 0 — slot 0 there is the JDK class's own `next` field. Route it to the
    // dedicated native; without this the array probe below misses and the
    // iterator falls through to the real-bytecode arm, which walks a `next`
    // chain nothing populated and reports every collection exhausted.
    //
    // Gated on the object's WIDTH, not its class name: this native is the
    // per-element `hasNext` of every snapshot iterator in the VM, and a name
    // lookup here would be paid by all of them. Only a carrier is wider than
    // the five native fields (see `key_itr_base`), so a base of 0 — the
    // fabricated `HashMap$KeyItr` and the legacy three-field shape — keeps
    // reading slot 0 exactly as before.
    if key_itr_base(ctx, this) > 0 {
        let cid = ctx.class_id_of_object(this);
        let cn = ctx.class_name_of_id(cid).unwrap_or_default();
        if is_map_key_itr_class(&cn) {
            return native_map_key_itr_has_next(ctx, args);
        }
    }
    let arr: ObjectRef = match ctx.get_field(this, 0) {
        Value::Object(Some(r)) if ctx.heap_kind_of(r) == ObjectKind::Array => r,
        _ => {
            let cid = ctx.class_id_of_object(this);
            let cn = ctx.class_name_of_id(cid).unwrap_or_default();
            if cn.is_empty() || cn == "java/util/Iterator" || cn == "java/util/ListIterator" {
                return Ok(Some(Value::Int(0)));
            }
            // BOTH-MODES: this native is registered on the *synthetic*
            // snapshot iterators (field 0 = Object[]) AND on the real
            // `java/util/PriorityQueue$Itr` / `ArrayDeque$Itr` and the
            // `java/util/Iterator` interface. In real-JDK mode the real inner
            // iterators don't have our snapshot layout (field 0 is their
            // `cursor` int), so service them from the backing collection.
            if let Some((_queue, size, cursor)) = real_inner_itr_state(ctx, this, &cn) {
                return Ok(Some(Value::Int(if cursor < size { 1 } else { 0 })));
            }
            // The live sublist iterator, routed DELIBERATELY: the by-name
            // re-dispatch below would reach the same body, but only by
            // accident, and only while that fallback keeps working. See
            // `is_live_sublist_itr`.
            if is_live_sublist_itr(&*ctx, this, &cn) {
                return native_sli_has_next(ctx, args);
            }
            // Any other real iterator: try its concrete `hasNext` bytecode by
            // name, but guard against the shadow-recursion — if dispatch
            // re-finds THIS native (no distinct concrete override) `ctx.invoke`
            // recurses unbounded and blows the native stack (H2 TestSampleApps /
            // TestReorderWrites / TestDiskFull / TestFileLockProcess). On
            // re-entry for the same receiver, report exhausted instead.
            return snapshot_itr_guarded_redispatch(ctx, this, &cn, "hasNext", "()Z")
                .unwrap_or(Ok(Some(Value::Int(0))));
        }
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    let len = ctx.array_length(arr) as i32;
    Ok(Some(Value::Int(if cursor < len { 1 } else { 0 })))
}

/// The snapshot iterators' real-mode fallback: dispatch `cn.method` on `this`
/// by name, or `None` when `this` is already inside that fallback (the
/// dispatch re-found this native and would recurse unbounded -- see
/// [`native_snapshot_itr_has_next`]). Shared by `hasNext` and `next`, which
/// carried identical inline copies of the guard; the caller returns whatever
/// this answers, so no local of its outlives the Java it runs.
pub(crate) fn snapshot_itr_guarded_redispatch(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    cn: &str,
    method: &str,
    desc: &str,
) -> Option<MethodCallResult> {
    // One entry per receiver, whatever the method (as the former key-only
    // set was): `hasNext` and `next` share the guard.
    let guard_pin = reentry_guard_enter(ctx, &SNAPITR_FALLBACK, this, SNAPITR_GUARD_TAG)?;
    let r = ctx.invoke(cn, method, desc, &[Value::Object(Some(this))]);
    reentry_guard_leave(ctx, &SNAPITR_FALLBACK, guard_pin);
    Some(r)
}

/// The one method tag [`SNAPITR_FALLBACK`] entries carry: the guard is per
/// receiver, not per method.
pub(crate) const SNAPITR_GUARD_TAG: &str = "snapshot-iterator-fallback";

thread_local! {
    /// Receivers currently inside the snapshot-iterator real-mode fallback,
    /// used to break unbounded `hasNext`/`next` → same-native re-dispatch.
    /// A [`ReentryEntry`] per receiver (a pin handle, gc-common w26-a), not
    /// the raw address and not the identity hash.
    pub(crate) static SNAPITR_FALLBACK: std::cell::RefCell<Vec<ReentryEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Service a REAL-JDK array-indexed inner iterator whose `cursor` field is a
/// 0-based index into the backing collection's element array. Returns
/// `(backing_array, size, cursor)` so callers can compute `hasNext`/`next`
/// without re-dispatching `hasNext`/`next` back into this native (which would
/// recurse unbounded, since the native shadows the iterator's real bytecode).
///
/// `java/util/PriorityQueue$Itr` iterates `queue[0..size]` in heap-array order,
/// so we read the outer `PriorityQueue`'s `queue`/`size` fields by name (the
/// generic collection-element heuristics don't model `PriorityQueue`).
pub(crate) fn real_inner_itr_state(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    cn: &str,
) -> Option<(ObjectRef, i32, i32)> {
    if cn != "java/util/PriorityQueue$Itr" {
        return None;
    }
    // `this$0` is the outer PriorityQueue; `queue`/`size` hold the heap array.
    // (Best-effort: returns None — and the caller falls back to the
    // recursion-safe guard — when the receiver's `this$0`/`queue` can't be
    // resolved, e.g. the synthetic `native_pq_iterator` object whose snapshot
    // was coerced away.)
    let outer = match ctx.get_field_by_name(this, "this$0") {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    let queue = match ctx.get_field_by_name(outer, "queue") {
        Value::Object(Some(q)) if ctx.heap_kind_of(q) == ObjectKind::Array => q,
        _ => return None,
    };
    let size = match ctx.get_field_by_name(outer, "size") {
        Value::Int(v) => v,
        _ => ctx.array_length(queue) as i32,
    };
    let cursor = match ctx.get_field_by_name(this, "cursor") {
        Value::Int(v) => v,
        _ => 0,
    };
    Some((queue, size, cursor))
}

pub(crate) fn native_snapshot_itr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // HashMap$KeyItr layout-compatible iterators (field 0 = Object[], field
    // 1 = cursor) also happen to satisfy the snapshot-iterator shape — but
    // they carry an extra `lastRet` slot that subsequent Iterator.remove()
    // reads. Route via the dedicated KeyItr native so the lastRet write
    // happens, otherwise it.remove() throws spurious IllegalStateException.
    // HashMap$KeyItr layout-compatible iterators (field 0 = Object[], field
    // 1 = cursor) also satisfy the snapshot-iterator shape — but they carry
    // an extra `lastRet` slot that subsequent Iterator.remove() reads.
    // Route via the dedicated KeyItr native so the lastRet write happens,
    // otherwise it.remove() throws spurious IllegalStateException.
    let cid = ctx.class_id_of_object(this);
    let cn = ctx.class_name_of_id(cid).unwrap_or_default();
    if is_map_key_itr_class(&cn) {
        return native_map_key_itr_next(ctx, args);
    }
    // Scoped BY CLASS NAME for the same reason the `lastRet` write near the end
    // of this function is: this native is also the generic `java/util/Iterator`
    // fallback and runs over shapes whose slot 4 belongs to somebody else.
    if cn == "java/util/TreeSet$Itr" {
        ts_itr_check_comod(&*ctx, this)?;
    }
    // Real-JDK fallback: see `native_snapshot_itr_has_next` doc comment.
    let arr: ObjectRef = match ctx.get_field(this, 0) {
        Value::Object(Some(r)) if ctx.heap_kind_of(r) == ObjectKind::Array => r,
        _ => {
            // Real-mode array-indexed inner iterator (PriorityQueue$Itr): return
            // backing[cursor] and advance the real `cursor` field — no
            // re-dispatch into `next` (which shadows the real bytecode → recurse).
            if let Some((queue, size, cursor)) = real_inner_itr_state(ctx, this, &cn) {
                if cursor < size {
                    let elem = ctx.get_array_element(queue, cursor as usize);
                    ctx.set_field_by_name(this, "cursor", Value::Int(cursor + 1));
                    return Ok(Some(elem));
                }
                return Err(
                    cratonvm_types::error::RuntimeError::NoSuchElementException {
                        message: "No more elements".to_string(),
                    }
                    .into(),
                );
            }
            if is_live_sublist_itr(&*ctx, this, &cn) {
                return native_sli_next(ctx, args);
            }
            if !cn.is_empty() && cn != "java/util/Iterator" && cn != "java/util/ListIterator" {
                // Guard the shadow-recursion (same as hasNext).
                if let Some(r) =
                    snapshot_itr_guarded_redispatch(ctx, this, &cn, "next", "()Ljava/lang/Object;")
                {
                    return r;
                }
            }
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "No more elements".to_string(),
                }
                .into(),
            );
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
    // W7-1 family 2: record the index just returned so `native_ts_itr_remove`
    // can enforce the `IllegalStateException` half of the `Iterator.remove()`
    // contract. Scoped to `TreeSet$Itr` BY CLASS NAME on purpose: this native is
    // also the `java/util/Iterator` / `Enumeration` / `ListIterator` interface
    // fallback, so it runs over shapes whose slot 3 belongs to somebody else
    // (`PriorityQueue$Itr`, `ArrayDeque$Itr`, any real iterator whose first
    // field happens to be an `Object[]`) — writing it unconditionally would
    // corrupt them. `HashMap$KeyItr` already left through the dedicated route
    // at the top of this function, which keeps its own `lastRet`.
    if cn == "java/util/TreeSet$Itr" && ctx.object_num_fields(this) > TS_ITR_FIELD_LAST_RET {
        ctx.set_field(this, TS_ITR_FIELD_LAST_RET, Value::Int(cursor));
    }
    snapshot_itr_box_element(ctx, elem)
}

/// `Iterator.next()` returns `Object`, so a snapshot whose array is PRIMITIVE
/// must box the element it hands out.
///
/// JIT round 16 wave 8 (lane collections): the only snapshot with a primitive
/// array is the `PrimitiveIterator$OfInt` / `$OfLong` / `$OfDouble` that
/// `--compatible`'s `IntStream` / `LongStream` / `DoubleStream.iterator`
/// natives mint (`make_primitive_iterator`, an `int[]` / `long[]` /
/// `double[]` at slot 0). Its own `next()` native boxes
/// (`native_primitive_iterator_next_boxed`), but the FIRST
/// `invokeinterface Iterator.next` per receiver class lands on this interface
/// native instead, which returned the raw `Value::Int` / `Value::Long`; the
/// caller read it as a null reference. That was the `null 1 2 3 4` of
/// `IntStream.range(0, 5).iterator()` (page
/// `r16-arraylist-compatible-intstream-iterator-first-element-null-FIXED-20261001.md`).
///
/// A reference element passes through untouched, so every `Object[]`
/// snapshot is unaffected. `CRATONVM_SNAPSHOT_ITR_BOX_PRIMITIVE=0` is the kill
/// switch; it is read only on the primitive branch, which only the first call
/// per receiver class reaches.
pub(crate) fn snapshot_itr_box_element(ctx: &mut dyn NativeContext, elem: Value) -> MethodCallResult {
    if matches!(elem, Value::Object(_))
        || !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_SNAPSHOT_ITR_BOX_PRIMITIVE")
    {
        return Ok(Some(elem));
    }
    Ok(Some(box_primitive_result(ctx, elem)?))
}

// ===========================================================================
