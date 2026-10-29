// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

// Phase 41: BlockingQueue family — LinkedBlockingQueue, ArrayBlockingQueue
// ===========================================================================

// LinkedBlockingQueue = 4-field synthetic (same as LinkedList + capacity)
pub(crate) const LBQ_FIELD_HEAD: usize = 0; // Object[] backing buffer (NOT an index)
pub(crate) const LBQ_FIELD_TAIL: usize = 1; // Int: head index into the backing buffer
pub(crate) const LBQ_FIELD_SIZE: usize = 2;
pub(crate) const LBQ_FIELD_CAPACITY: usize = 3;
pub(crate) const _LBQ_NUM_FIELDS: usize = 4;

// ---------------------------------------------------------------------------
// FIFO layout (perf P1 fix): the backing `Object[]` (in LBQ_FIELD_HEAD) holds
// the live elements at indices `[head .. head + size)`, where `head` is the
// integer in LBQ_FIELD_TAIL (previously an unused, always-0 slot). A `poll`
// returns element `head` and advances `head` by one — O(1) — instead of the
// former O(n) left-shift of every remaining element (which made an N-element
// drain O(n^2)). The buffer is a linear (non-wrapping) window that drifts
// rightward as elements are polled; `lbq_ensure_capacity` compacts the window
// back to index 0 (and grows when necessary) before any append, so the tail
// never runs off the end. Logical element `i` therefore lives at array index
// `head + i`.

/// Current head index (offset of logical element 0 within the backing array).
pub(crate) fn lbq_head(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    match ctx.get_field(this, LBQ_FIELD_TAIL) {
        Value::Int(v) if v >= 0 => v as usize,
        _ => 0,
    }
}

/// Store the head index.
pub(crate) fn lbq_set_head(ctx: &mut dyn NativeContext, this: ObjectRef, head: usize) {
    ctx.set_field(this, LBQ_FIELD_TAIL, Value::Int(head as i32));
}

pub(crate) fn register_blocking_queue_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // LinkedBlockingQueue
    let lbq = "java/util/concurrent/LinkedBlockingQueue";
    r.register(lbq, "<init>", "()V", native_lbq_init);
    r.register(lbq, "<init>", "(I)V", native_lbq_init_cap);
    // put() blocks until space is available via monitor wait/notify (task #15)
    r.register(lbq, "put", "(Ljava/lang/Object;)V", native_lbq_put_blocking);
    r.register(lbq, "offer", "(Ljava/lang/Object;)Z", native_lbq_offer_bool);
    r.register(lbq, "add", "(Ljava/lang/Object;)Z", native_lbq_offer_bool);
    // take() blocks until an element is available via monitor wait/notify (task #15)
    r.register(
        lbq,
        "take",
        "()Ljava/lang/Object;",
        native_lbq_take_blocking,
    );
    r.register(lbq, "poll", "()Ljava/lang/Object;", native_lbq_poll);
    r.register(lbq, "peek", "()Ljava/lang/Object;", native_lbq_peek);
    r.register(lbq, "size", "()I", native_lbq_size);
    r.register(lbq, "isEmpty", "()Z", native_lbq_is_empty);
    r.register(lbq, "remainingCapacity", "()I", native_lbq_remaining);
    r.register(lbq, "clear", "()V", native_lbq_clear);
    r.register(
        lbq,
        "contains",
        "(Ljava/lang/Object;)Z",
        native_lbq_contains,
    );
    r.register(lbq, "remove", "(Ljava/lang/Object;)Z", native_lbq_remove);
    r.register(lbq, "toArray", "()[Ljava/lang/Object;", native_lbq_to_array);
    r.register(
        lbq,
        "iterator",
        "()Ljava/util/Iterator;",
        native_lbq_iterator,
    );
    // Also register under BlockingQueue interface
    let bq = "java/util/concurrent/BlockingQueue";
    r.register(bq, "put", "(Ljava/lang/Object;)V", native_lbq_put_blocking);
    r.register(bq, "offer", "(Ljava/lang/Object;)Z", native_lbq_offer_bool);
    r.register(bq, "take", "()Ljava/lang/Object;", native_lbq_take_blocking);
    r.register(bq, "poll", "()Ljava/lang/Object;", native_lbq_poll);
    r.register(bq, "peek", "()Ljava/lang/Object;", native_lbq_peek);

    // ArrayBlockingQueue
    let abq = "java/util/concurrent/ArrayBlockingQueue";
    r.register(abq, "<init>", "(I)V", native_abq_init);
    r.register(abq, "<init>", "(IZ)V", native_abq_init_fair);
    r.register(abq, "put", "(Ljava/lang/Object;)V", native_lbq_put_blocking);
    r.register(abq, "offer", "(Ljava/lang/Object;)Z", native_lbq_offer_bool);
    r.register(abq, "add", "(Ljava/lang/Object;)Z", native_lbq_offer_bool);
    r.register(
        abq,
        "take",
        "()Ljava/lang/Object;",
        native_lbq_take_blocking,
    );
    r.register(abq, "poll", "()Ljava/lang/Object;", native_lbq_poll);
    r.register(abq, "peek", "()Ljava/lang/Object;", native_lbq_peek);
    r.register(abq, "size", "()I", native_lbq_size);
    r.register(abq, "isEmpty", "()Z", native_lbq_is_empty);
    r.register(abq, "remainingCapacity", "()I", native_lbq_remaining);
    r.register(abq, "clear", "()V", native_lbq_clear);
    r.register(
        abq,
        "contains",
        "(Ljava/lang/Object;)Z",
        native_lbq_contains,
    );
    r.register(abq, "remove", "(Ljava/lang/Object;)Z", native_lbq_remove);
    r.register(abq, "toArray", "()[Ljava/lang/Object;", native_lbq_to_array);
    r.register(
        abq,
        "iterator",
        "()Ljava/util/Iterator;",
        native_lbq_iterator,
    );

    // ConcurrentLinkedQueue — thread-safety wrappers (round-5 Fix 2 HIGH).
    //
    // The raw `native_lbq_offer_bool` / `native_lbq_poll` do unsynchronised
    // array shifts that corrupt under concurrent producer+consumer load
    // (read-modify-write of LBQ_FIELD_SIZE races; element-shift in `poll`
    // races against `offer` appending at `size`). Real CLQ is lock-free
    // (Michael-Scott queue); we approximate the *correctness* guarantee
    // by serialising every op on the `this` monitor. Not lock-free in
    // throughput, but at least no torn updates.
    let clq = "java/util/concurrent/ConcurrentLinkedQueue";
    r.register(clq, "<init>", "()V", native_lbq_init);
    r.register(clq, "offer", "(Ljava/lang/Object;)Z", native_clq_offer);
    r.register(clq, "add", "(Ljava/lang/Object;)Z", native_clq_offer);
    r.register(clq, "poll", "()Ljava/lang/Object;", native_clq_poll);
    r.register(clq, "peek", "()Ljava/lang/Object;", native_clq_peek);
    r.register(clq, "size", "()I", native_lbq_size);
    r.register(clq, "isEmpty", "()Z", native_lbq_is_empty);
    r.register(
        clq,
        "contains",
        "(Ljava/lang/Object;)Z",
        native_lbq_contains,
    );
    r.register(clq, "remove", "(Ljava/lang/Object;)Z", native_lbq_remove);
    r.register(
        clq,
        "iterator",
        "()Ljava/util/Iterator;",
        native_lbq_iterator,
    );

    // ConcurrentLinkedDeque — same synchronisation rationale as CLQ.
    let cld = "java/util/concurrent/ConcurrentLinkedDeque";
    r.register(cld, "<init>", "()V", native_lbq_init);
    r.register(
        cld,
        "offerFirst",
        "(Ljava/lang/Object;)Z",
        native_cld_offer_first,
    );
    r.register(cld, "offerLast", "(Ljava/lang/Object;)Z", native_clq_offer);
    r.register(cld, "pollFirst", "()Ljava/lang/Object;", native_clq_poll);
    r.register(
        cld,
        "pollLast",
        "()Ljava/lang/Object;",
        native_cld_poll_last,
    );
    r.register(cld, "peekFirst", "()Ljava/lang/Object;", native_clq_peek);
    r.register(
        cld,
        "peekLast",
        "()Ljava/lang/Object;",
        native_cld_peek_last,
    );
    r.register(cld, "size", "()I", native_lbq_size);
    r.register(cld, "isEmpty", "()Z", native_lbq_is_empty);
    r.register(
        cld,
        "iterator",
        "()Ljava/util/Iterator;",
        native_lbq_iterator,
    );
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Round-5 Fix 2 (HIGH): synchronised CLQ/CLD wrappers
// ---------------------------------------------------------------------------
//
// Real JDK ConcurrentLinkedQueue/Deque are lock-free (Michael-Scott /
// concurrent doubly-linked-list). Our synthetic backing is an ArrayList,
// which the LBQ helpers mutate with unsynchronised read-modify-write
// sequences. Producer+consumer racing would tear the size counter and
// drop or duplicate elements.
//
// These wrappers acquire the `this`-object monitor around each op so the
// observable behaviour is serialisable. monitor_enter/exit on `this` is
// also what real JDK code would synchronise on for
// `Collections.synchronizedQueue(queue)`, so semantically identical from
// the bytecode side. NOT lock-free in the throughput sense; treat as a
// correctness-only stopgap until a real Michael-Scott port lands.

// JDK-ONLY-CLASSIFY: stub — correctly tagged already, and worth recording as
// the crate's counter-example. `java.util.concurrent.LinkedBlockingDeque`
// declares zero ACC_NATIVE methods in JDK 25; all 15 registrations shadow
// concrete bytecode with a monitor-wrapped ArrayList, which the comment above
// already calls "a correctness-only stopgap". This is the one place in
// `native-collections` where a `set_category` states a per-subsystem judgement
// instead of inheriting the crate-wide `Bridge`, and it reaches the right
// answer. Under `--jdk-only` the registration is refused and the real
// lock-free bytecode runs, which is the intended outcome.
#[cfg(not(feature = "synthetic-jdk"))]
pub(crate) fn register_linked_blocking_deque_stub_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    let lbd = "java/util/concurrent/LinkedBlockingDeque";
    r.register(lbd, "<init>", "()V", native_lbq_init);
    r.register(lbd, "offer", "(Ljava/lang/Object;)Z", native_clq_offer);
    r.register(lbd, "add", "(Ljava/lang/Object;)Z", native_clq_offer);
    r.register(
        lbd,
        "offerFirst",
        "(Ljava/lang/Object;)Z",
        native_cld_offer_first,
    );
    r.register(lbd, "offerLast", "(Ljava/lang/Object;)Z", native_clq_offer);
    r.register(lbd, "poll", "()Ljava/lang/Object;", native_clq_poll);
    r.register(lbd, "pollFirst", "()Ljava/lang/Object;", native_clq_poll);
    r.register(
        lbd,
        "pollLast",
        "()Ljava/lang/Object;",
        native_cld_poll_last,
    );
    r.register(lbd, "peek", "()Ljava/lang/Object;", native_clq_peek);
    r.register(lbd, "peekFirst", "()Ljava/lang/Object;", native_clq_peek);
    r.register(
        lbd,
        "peekLast",
        "()Ljava/lang/Object;",
        native_cld_peek_last,
    );
    r.register(lbd, "size", "()I", native_lbq_size);
    r.register(lbd, "isEmpty", "()Z", native_lbq_is_empty);
    r.register(lbd, "clear", "()V", native_lbq_clear);
    r.register(
        lbd,
        "iterator",
        "()Ljava/util/Iterator;",
        native_lbq_iterator,
    );
    r.set_category(__prev_cat);
}

pub(crate) fn native_clq_offer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    ctx.monitor_enter(this);
    let r = native_lbq_offer_bool(ctx, args);
    ctx.monitor_exit(this);
    r
}

pub(crate) fn native_clq_poll(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    ctx.monitor_enter(this);
    let r = native_lbq_poll(ctx, args);
    ctx.monitor_exit(this);
    r
}

pub(crate) fn native_clq_peek(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    ctx.monitor_enter(this);
    let r = native_lbq_peek(ctx, args);
    ctx.monitor_exit(this);
    r
}

pub(crate) fn native_cld_poll_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    ctx.monitor_enter(this);
    let r = native_lbq_poll_last(ctx, args);
    ctx.monitor_exit(this);
    r
}

pub(crate) fn native_cld_peek_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    ctx.monitor_enter(this);
    let r = native_lbq_peek_last(ctx, args);
    ctx.monitor_exit(this);
    r
}

/// offerFirst — prepend element; ArrayList backing means O(n) shift but
/// correctness is preserved under concurrent access via the `this` monitor.
pub(crate) fn native_cld_offer_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(mut this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    let elem = args.get(1).cloned().unwrap_or(Value::Object(None));
    ctx.monitor_enter(this);
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    } as usize;
    // Fast path: if the head window has drifted rightward there is already a
    // free slot in front — just decrement head and write there (O(1)).
    let head = lbq_head(ctx, this);
    if head > 0 {
        let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
            Value::Object(Some(a)) => a,
            _ => {
                ctx.monitor_exit(this);
                return Ok(Some(Value::Int(0)));
            }
        };
        ctx.set_array_element(arr, head - 1, elem);
        lbq_set_head(ctx, this, head - 1);
        ctx.set_field(this, LBQ_FIELD_SIZE, Value::Int(size as i32 + 1));
        ctx.monitor_exit(this);
        return Ok(Some(Value::Int(1)));
    }
    // Slow path: head == 0, no front room. Grow/compact so the window fits,
    // then shift the live elements right by one to open index 0.
    lbq_ensure_capacity(ctx, &mut this, size + 1);
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Int(0)));
        }
    };
    // ensure_capacity reset head to 0 if it relocated; re-read defensively.
    let head = lbq_head(ctx, this);
    for i in (1..=size).rev() {
        ctx.set_array_element(arr, head + i, ctx.get_array_element(arr, head + i - 1));
    }
    ctx.set_array_element(arr, head, elem);
    ctx.set_field(this, LBQ_FIELD_SIZE, Value::Int(size as i32 + 1));
    ctx.monitor_exit(this);
    Ok(Some(Value::Int(1)))
}

// Blocking queue implementations — backed by ArrayList internally for simplicity
pub(crate) fn native_lbq_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // GC-safety: see `rooted_across`.
    let (this, arr) = rooted_across1(ctx, this, |ctx| alloc_ref_array(ctx, 16));
    ctx.set_field(this, LBQ_FIELD_HEAD, Value::Object(Some(arr)));
    ctx.set_field(this, LBQ_FIELD_TAIL, Value::Int(0)); // head index = 0
    ctx.set_field(this, LBQ_FIELD_SIZE, Value::Int(0));
    ctx.set_field(this, LBQ_FIELD_CAPACITY, Value::Int(i32::MAX));
    Ok(None)
}

pub(crate) fn native_lbq_init_cap(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let cap = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => i32::MAX,
    };
    // GC-safety: see `rooted_across`.
    let (this, arr) = rooted_across1(ctx, this, |ctx| alloc_ref_array(ctx, cap.max(1) as usize));
    ctx.set_field(this, LBQ_FIELD_HEAD, Value::Object(Some(arr)));
    ctx.set_field(this, LBQ_FIELD_TAIL, Value::Int(0));
    ctx.set_field(this, LBQ_FIELD_SIZE, Value::Int(0));
    ctx.set_field(this, LBQ_FIELD_CAPACITY, Value::Int(cap));
    Ok(None)
}

pub(crate) fn native_abq_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_lbq_init_cap(ctx, args)
}

pub(crate) fn native_abq_init_fair(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Ignore fair param
    native_lbq_init_cap(ctx, args)
}

/// Ensure the backing array can hold `needed` logical elements *starting at
/// the current head index*. If the window `[head .. head + needed)` would run
/// off the end of the array, first try compacting the live `[head .. head +
/// size)` window back to index 0 (no allocation); only allocate a larger
/// buffer when even a compacted window doesn't fit. Resets `head` to 0
/// whenever it relocates elements. Logical element layout (`head + i`) is
/// preserved.
pub(crate) fn lbq_ensure_capacity(ctx: &mut dyn NativeContext, this: &mut ObjectRef, needed: usize) {
    let arr = match ctx.get_field(*this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => return,
    };
    let old_len = ctx.array_length(arr);
    let head = lbq_head(ctx, *this);
    // Fits in the current window — nothing to do.
    if head + needed <= old_len {
        return;
    }
    let size = match ctx.get_field(*this, LBQ_FIELD_SIZE) {
        Value::Int(v) if v >= 0 => v as usize,
        _ => 0,
    };
    // Compact-in-place if the live window fits at index 0 (head has merely
    // drifted right past the tail). This is the common steady-state case for
    // a balanced producer/consumer queue and avoids reallocation.
    if needed <= old_len {
        for i in 0..size {
            ctx.set_array_element(arr, i, ctx.get_array_element(arr, head + i));
        }
        for i in size..old_len {
            ctx.set_array_element(arr, i, Value::Object(None));
        }
        lbq_set_head(ctx, *this, 0);
        return;
    }
    // Grow: allocate a larger buffer and copy the live window to index 0.
    let new_len = (old_len * 2).max(needed).max(16);
    // GC-safety: the allocation can complete a moving young GC; `this` and the
    // old buffer are both bare Rust locals used below. See `rooted_across`.
    let mut arr = arr;
    let new_arr: ObjectRef = rooted_across(ctx, &mut [this, &mut arr], |ctx| {
        alloc_ref_array(ctx, new_len)
    });
    for i in 0..size {
        ctx.set_array_element(new_arr, i, ctx.get_array_element(arr, head + i));
    }
    ctx.set_field(*this, LBQ_FIELD_HEAD, Value::Object(Some(new_arr)));
    lbq_set_head(ctx, *this, 0);
}

// =====================================================================
// task #15 (MED → correctness): blocking-queue park/notify discipline.
//
// Previously `native_lbq_put_blocking` spun 10000 times then fell through
// into `native_lbq_offer` regardless of capacity — losing the "block"
// semantic entirely (silently overflowing the bounded queue). Likewise
// `native_lbq_poll/peek/poll_last/peek_last` were unsynchronised so a
// concurrent producer racing a consumer could tear LBQ_FIELD_SIZE or read
// a half-shifted backing array.
//
// Fix: every read AND every mutation now acquires the `this` monitor, and
// the put/take blocking variants follow standard Java wait/notify
// discipline:
//
//   put:  while (size >= capacity) wait(); offer(); notifyAll();
//   take: while (size == 0)        wait(); poll();  notifyAll();
//
// `lbq_notify_all_locked` is called from every mutator that changes size
// (offer, poll, poll_last, clear, remove) so a blocked put/take wakes
// promptly. A safety timeout of 50 ms on `monitor_wait` guards against
// single-threaded test contexts whose `monitor_notify_all` is a no-op —
// without it the test thread would deadlock; with it the loop simply
// re-checks the predicate and bails (returning normally on take, blocking
// indefinitely on put, which is correct: a producer on a full queue with
// no consumer SHOULD block).
// =====================================================================

/// Internal offer that assumes caller already holds the `this` monitor.
/// Grows the backing array unconditionally — capacity is enforced by the
/// caller (offer_bool / put_blocking).
pub(crate) fn native_lbq_offer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let elem = args.get(1).cloned().unwrap_or(Value::Object(None));
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    lbq_ensure_capacity(ctx, &mut this, (size + 1) as usize);
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => return Ok(None),
    };
    // Append at the logical tail (head + size). ensure_capacity guarantees
    // this slot exists (and may have reset head to 0 by compacting).
    let head = lbq_head(ctx, this);
    ctx.set_array_element(arr, head + size as usize, elem);
    ctx.set_field(this, LBQ_FIELD_SIZE, Value::Int(size + 1));
    Ok(None)
}

pub(crate) fn native_lbq_offer_bool(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    ctx.monitor_enter(this);
    let capacity = match ctx.get_field(this, LBQ_FIELD_CAPACITY) {
        Value::Int(v) if v > 0 => v,
        _ => i32::MAX,
    };
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size >= capacity {
        ctx.monitor_exit(this);
        return Ok(Some(Value::Int(0))); // queue full
    }
    let r = native_lbq_offer(ctx, args);
    // Wake any thread parked in `take()` waiting for an element.
    let _ = ctx.monitor_notify_all(this);
    ctx.monitor_exit(this);
    r?;
    Ok(Some(Value::Int(1)))
}

/// Blocking put: waits until space is available using monitor wait/notify.
/// Loop predicate is re-checked after each wake (handles spurious wakeups
/// and the case where another producer raced in to fill the gap).
pub(crate) fn native_lbq_put_blocking(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let capacity = match ctx.get_field(this, LBQ_FIELD_CAPACITY) {
        Value::Int(v) if v > 0 => v,
        _ => i32::MAX,
    };
    ctx.monitor_enter(this);
    let this_pin = ctx.pin_native_root(this);
    // The ELEMENT crosses the wait too — it is handed to `native_lbq_offer`
    // below, after the loop — so it needs the same pin as the receiver.
    // `args` is the interpreter's pre-call snapshot and nothing refreshes it.
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let elem_pin = match elem {
        Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
        _ => None,
    };
    loop {
        this = ctx.read_native_pin(this_pin, this);
        let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
            Value::Int(v) => v,
            _ => 0,
        };
        if size < capacity {
            break;
        }
        // Park on the monitor with a short timeout so we re-check the
        // predicate periodically — protects against missed-wakeup bugs and
        // makes the wait responsive on mock contexts whose
        // `monitor_notify_all` is a no-op.
        let _ = ctx.monitor_wait(this, Some(50));
    }
    // `monitor_wait` above PARKS, so a collection can have moved (or, for an
    // object the sweep proved dead, reclaimed) both the receiver and the
    // element inside the loop. The shadowed `this` used to be scoped to the
    // loop BODY, so everything below ran on the pre-wait address:
    // `native_lbq_offer` appended into a stale queue, and `monitor_exit`
    // released a monitor at an address `MonitorTable::exit` then dereferenced.
    // Assign through the outer binding instead, and refresh the element the
    // same way.
    this = ctx.read_native_pin(this_pin, this);
    let elem = match (elem, elem_pin) {
        (Value::Object(Some(o)), Some(h)) => Value::Object(Some(ctx.read_native_pin(h, o))),
        _ => elem,
    };
    let r = native_lbq_offer(ctx, &[Value::Object(Some(this)), elem]);
    // Wake any thread parked in `take()` waiting for an element to arrive.
    let _ = ctx.monitor_notify_all(this);
    ctx.monitor_exit(this);
    ctx.unpin_native_roots(this_pin);
    r
}

/// Blocking take: waits until an element is available using monitor wait/notify.
pub(crate) fn native_lbq_take_blocking(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.monitor_enter(this);
    let this_pin = ctx.pin_native_root(this);
    loop {
        this = ctx.read_native_pin(this_pin, this);
        let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
            Value::Int(v) => v,
            _ => 0,
        };
        if size > 0 {
            break;
        }
        let _ = ctx.monitor_wait(this, Some(50));
    }
    // See `native_lbq_put_blocking`: the refresh has to reach the OUTER
    // binding, or the dequeue and the monitor exit below both run on the
    // pre-wait address.
    this = ctx.read_native_pin(this_pin, this);
    let r = lbq_poll_locked(ctx, this);
    // Wake any thread parked in `put()` waiting for a slot to free up.
    let _ = ctx.monitor_notify_all(this);
    ctx.monitor_exit(this);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(r))
}

/// Internal poll that assumes the caller already holds the `this` monitor.
/// Returns `Value::Object(None)` when the queue is empty.
pub(crate) fn lbq_poll_locked(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size == 0 {
        return Value::Object(None);
    }
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => return Value::Object(None),
    };
    // O(1) FIFO dequeue: read the head element, clear its slot and advance the
    // head index by one (no element shifting — see the layout note above).
    let head = lbq_head(ctx, this);
    let elem = ctx.get_array_element(arr, head);
    ctx.set_array_element(arr, head, Value::Object(None));
    if size - 1 == 0 {
        // Empty: reset head to 0 so the window starts fresh.
        lbq_set_head(ctx, this, 0);
    } else {
        lbq_set_head(ctx, this, head + 1);
    }
    ctx.set_field(this, LBQ_FIELD_SIZE, Value::Int(size - 1));
    elem
}

pub(crate) fn native_lbq_poll(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.monitor_enter(this);
    let head = lbq_poll_locked(ctx, this);
    // Wake any thread parked in `put()` — a slot just freed.
    let _ = ctx.monitor_notify_all(this);
    ctx.monitor_exit(this);
    Ok(Some(head))
}

pub(crate) fn native_lbq_poll_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.monitor_enter(this);
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size == 0 {
        ctx.monitor_exit(this);
        return Ok(Some(Value::Object(None)));
    }
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Object(None)));
        }
    };
    let head = lbq_head(ctx, this);
    let tail_idx = head + (size - 1) as usize;
    let tail = ctx.get_array_element(arr, tail_idx);
    ctx.set_array_element(arr, tail_idx, Value::Object(None));
    if size - 1 == 0 {
        lbq_set_head(ctx, this, 0);
    }
    ctx.set_field(this, LBQ_FIELD_SIZE, Value::Int(size - 1));
    let _ = ctx.monitor_notify_all(this);
    ctx.monitor_exit(this);
    Ok(Some(tail))
}

pub(crate) fn native_lbq_peek(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.monitor_enter(this);
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size == 0 {
        ctx.monitor_exit(this);
        return Ok(Some(Value::Object(None)));
    }
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Object(None)));
        }
    };
    let head = lbq_head(ctx, this);
    let front = ctx.get_array_element(arr, head);
    ctx.monitor_exit(this);
    Ok(Some(front))
}

pub(crate) fn native_lbq_peek_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.monitor_enter(this);
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size == 0 {
        ctx.monitor_exit(this);
        return Ok(Some(Value::Object(None)));
    }
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Object(None)));
        }
    };
    let head = lbq_head(ctx, this);
    let tail = ctx.get_array_element(arr, head + (size - 1) as usize);
    ctx.monitor_exit(this);
    Ok(Some(tail))
}

pub(crate) fn native_lbq_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => Ok(Some(Value::Int(v))),
        _ => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_lbq_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
}

pub(crate) fn native_lbq_remaining(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    let cap = match ctx.get_field(this, LBQ_FIELD_CAPACITY) {
        Value::Int(v) => v,
        _ => i32::MAX,
    };
    Ok(Some(Value::Int(cap - size)))
}

pub(crate) fn native_lbq_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.monitor_enter(this);
    ctx.set_field(this, LBQ_FIELD_SIZE, Value::Int(0));
    // Reset the head window so the buffer is reused from index 0.
    lbq_set_head(ctx, this, 0);
    // Wake any thread parked in `put()` — capacity was just fully reclaimed.
    let _ = ctx.monitor_notify_all(this);
    ctx.monitor_exit(this);
    Ok(None)
}

pub(crate) fn native_lbq_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).cloned().unwrap_or(Value::Object(None));
    ctx.monitor_enter(this);
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Int(0)));
        }
    };
    let head = lbq_head(ctx, this);
    // Family-1 fix (cce0079): pinned walk (equals() can move
    // `this`/`arr`/`target`); the monitor exit must use the refreshed
    // receiver or the real monitor leaks (SynchronizedMethodGuard class).
    let this_pin = ctx.pin_native_root(this);
    let arr_pin = ctx.pin_native_root(arr);
    let th = pin_value(ctx, target);
    let mut this = this;
    let mut arr = arr;
    let mut target = target;
    let mut found = false;
    for i in 0..size as usize {
        let elem = ctx.get_array_element(arr, head + i);
        let matched = list_element_matches(ctx, &elem, &target)?;
        this = ctx.read_native_pin(this_pin, this);
        arr = ctx.read_native_pin(arr_pin, arr);
        target = read_pinned_elem(ctx, th, target);
        if matched {
            found = true;
            break;
        }
    }
    ctx.unpin_native_roots(this_pin);
    ctx.monitor_exit(this);
    Ok(Some(Value::Int(if found { 1 } else { 0 })))
}

pub(crate) fn native_lbq_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).cloned().unwrap_or(Value::Object(None));
    ctx.monitor_enter(this);
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Int(0)));
        }
    };
    let head = lbq_head(ctx, this);
    // Family-1 fix (cce0079): the per-element `equals()` dispatch can move
    // `this`/`arr`/`target`. Beyond the stale shift/store, the original code
    // also exited the monitor through the PRE-GC `this` — releasing a stale
    // monitor address leaks the real monitor forever (the
    // SynchronizedMethodGuard bug class). Pin all three, refresh after each
    // dispatch, and act only on refreshed addresses.
    let this_pin = ctx.pin_native_root(this);
    let arr_pin = ctx.pin_native_root(arr);
    let th = pin_value(ctx, target);
    let mut this = this;
    let mut arr = arr;
    let mut target = target;
    let mut found_i = None;
    for i in 0..size as usize {
        let elem = ctx.get_array_element(arr, head + i);
        let matched = list_element_matches(ctx, &elem, &target)?;
        this = ctx.read_native_pin(this_pin, this);
        arr = ctx.read_native_pin(arr_pin, arr);
        target = read_pinned_elem(ctx, th, target);
        if matched {
            found_i = Some(i);
            break;
        }
    }
    ctx.unpin_native_roots(this_pin);
    if let Some(i) = found_i {
        // Shift the tail of the window left by one over the removed slot.
        for j in i..(size - 1) as usize {
            ctx.set_array_element(arr, head + j, ctx.get_array_element(arr, head + j + 1));
        }
        ctx.set_array_element(arr, head + (size - 1) as usize, Value::Object(None));
        if size - 1 == 0 {
            lbq_set_head(ctx, this, 0);
        }
        ctx.set_field(this, LBQ_FIELD_SIZE, Value::Int(size - 1));
        // Wake any thread parked in `put()` — a slot just freed.
        let _ = ctx.monitor_notify_all(this);
        ctx.monitor_exit(this);
        return Ok(Some(Value::Int(1)));
    }
    ctx.monitor_exit(this);
    Ok(Some(Value::Int(0)))
}

pub(crate) fn native_lbq_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.monitor_enter(this);
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Object(None)));
        }
    };
    let head = lbq_head(ctx, this);
    // GC-safety: the allocation can move `this` (the monitor we still have to
    // exit) and the backing buffer we copy out of. See `rooted_across`.
    let mut this = this;
    let mut arr = arr;
    let result: ObjectRef = rooted_across(ctx, &mut [&mut this, &mut arr], |ctx| {
        alloc_ref_array(ctx, size as usize)
    });
    for i in 0..size as usize {
        ctx.set_array_element(result, i, ctx.get_array_element(arr, head + i));
    }
    ctx.monitor_exit(this);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_lbq_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.monitor_enter(this);
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Object(None)));
        }
    };
    let head = lbq_head(ctx, this);
    // GC-safety: see `native_lbq_to_array`; the iterator shell allocates too,
    // and the snapshot is stored into it afterwards.
    let mut this = this;
    let mut arr = arr;
    let mut snap: ObjectRef = rooted_across(ctx, &mut [&mut this, &mut arr], |ctx| {
        alloc_ref_array(ctx, size as usize)
    });
    for i in 0..size as usize {
        ctx.set_array_element(snap, i, ctx.get_array_element(arr, head + i));
    }
    ctx.monitor_exit(this);
    let itr: ObjectRef = rooted_across(ctx, &mut [&mut snap], |ctx| {
        try_alloc_synthetic(ctx, "java/util/concurrent/LinkedBlockingQueue$Itr", 2)
    })?;
    ctx.set_field(itr, 0, Value::Object(Some(snap)));
    ctx.set_field(itr, 1, Value::Int(0));
    Ok(Some(Value::Object(Some(itr))))
}

// ===========================================================================
// Phase 44: Iterator Protocol + Enumeration completion
// ===========================================================================
// Register generic java/util/Iterator interface methods as fallbacks.
// The snapshot iterator pattern (2-field: array=0, cursor=1) is already used
// by all collection iterators. This registers them under the interface name
// so code calling through Iterator<E> references works.

/// The [`NativeKind`](cratonvm_native_api::NativeKind) of the
/// `java/util/Iterator.hasNext()Z` / `next()Ljava/lang/Object;` registrations
/// on the INTERFACE triple -- this crate's pair below. (`native-builtins`
/// kept an older pair that this one overwrote in every boot arm; it was dead
/// and was deleted in JIT round 16 wave 9, lane spliter.)
///
/// `SyntheticStub` by default (JIT round 16 wave 7, lane arraylist): the pair
/// serves only receivers whose class declares no `hasNext`/`next` at all.
/// Selection lands on the abstract interface method only for such a receiver,
/// because every real class implementing `Iterator` has a concrete `hasNext`/`next`.
/// Those receivers are CratonVM's synthetic snapshot wrappers: fabricated
/// classes (`HashMap$KeyItr`, `TreeMap$*Itr`), instances minted on the
/// interface itself (`Path.iterator`, the `PrimitiveIterator$OfInt` that
/// `IntStream.iterator` returns), and class-id-0 slots. Under `--jdk-only`, each of
/// those producers is either refused (no fabrication of a class no JDK declares) or
/// a retired shadow, so the pair has nothing to serve. Measured on the
/// round-16 base binary: `--jdk-only --nojit` with intrinsics disabled, 41
/// iterator families through the interface (`R16ArraylistIterIface`), both
/// rows `invocations: 0` with output identical to HotSpot; the same probe under
/// `--compatible` invokes them (5 / 4), for `IntStream.iterator`.
///
/// As a `Bridge` the pair stayed registered under `--jdk-only` and, through the
/// JIT's constant-pool screen, made every `Iterator.hasNext/next` site read as
/// natively shadowed (`inline-resolve REFUSED ... native-shadow`). As a
/// `SyntheticStub`, `--jdk-only` drops it and `--compatible` registers and
/// dispatches it exactly as before. `CRATONVM_ITERATOR_IFACE_SHIM_STUB=0` is
/// the kill switch: it restores `Bridge`.
pub fn iterator_interface_shim_kind() -> cratonvm_native_api::NativeKind {
    if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ITERATOR_IFACE_SHIM_STUB") {
        cratonvm_native_api::NativeKind::SyntheticStub
    } else {
        cratonvm_native_api::NativeKind::Bridge
    }
}

/// The [`NativeKind`](cratonvm_native_api::NativeKind) of the two SUB-interface
/// shim families: the nine `java/util/ListIterator` interface-triple natives in
/// `register_iterator_protocol_natives` and the nine
/// `java/util/PrimitiveIterator$OfInt` / `$OfLong` / `$OfDouble` natives in
/// `register_primitive_iterator_natives`.
///
/// `SyntheticStub` by default (JIT round 16 wave 8, lane collections), for the
/// reason [`iterator_interface_shim_kind`] gives for the `Iterator` pair: every
/// one of these methods is abstract on its interface (or, for
/// `PrimitiveIterator.next()`, a javac bridge default that real bytecode
/// already serves), so selection reaches the registration only for a receiver
/// minted ON the interface or on a fabricated class: the `--compatible`
/// `IntStream` / `LongStream` / `DoubleStream.iterator` snapshot
/// (`make_primitive_iterator`), and for `ListIterator` no producer at all was
/// found (the probe below reads 0 invocations under `--compatible` too). Under
/// `--jdk-only` the stream producers are retired shadows and fabrication is
/// refused. Measured on the round-16 base binary with
/// `--dump-native-registry`: `R16CollectionsListItr` (16 list families, all nine
/// `ListIterator` methods through the interface), `R16CollectionsPrimItr` and
/// `R16ArraylistIterIface` all print HotSpot's output with every one of the 18
/// rows at `invocations: 0` under `--jdk-only` (`--nojit` with
/// `CRATONVM_DISABLE_INTRINSICS=1`, and with the JIT). Under `--compatible` the
/// `PrimitiveIterator` rows are live (27,015 `OfInt.hasNext` calls on
/// `R16CollectionsPrimItr`) and keep running exactly as before.
///
/// As a `Bridge` each row stayed registered under `--jdk-only`, where it serves
/// nothing and makes the JIT's constant-pool screen read every
/// `ListIterator` / `PrimitiveIterator` site as natively shadowed. As a
/// `SyntheticStub`, `--jdk-only` drops it. `CRATONVM_ITERATOR_SUBIFACE_SHIM_STUB=0`
/// is the kill switch: it restores `Bridge` for all 18.
pub fn iterator_subinterface_shim_kind() -> cratonvm_native_api::NativeKind {
    if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ITERATOR_SUBIFACE_SHIM_STUB") {
        cratonvm_native_api::NativeKind::SyntheticStub
    } else {
        cratonvm_native_api::NativeKind::Bridge
    }
}

/// The [`NativeKind`](cratonvm_native_api::NativeKind) of the six
/// `java/util/Spliterator` interface-triple natives in
/// `register_iterator_protocol_natives` (`estimateSize`, `characteristics`,
/// `getComparator`, `tryAdvance`, `trySplit`, `forEachRemaining`).
///
/// `SyntheticStub` by default (JIT round 16 wave 8, lane collections), for the
/// reason [`iterator_interface_shim_kind`] gives. Four of the six are abstract on
/// `Spliterator`, and `getComparator` / `forEachRemaining` are interface
/// defaults that the real bytecode already serves. So selection reaches these
/// rows only for a receiver minted ON the interface: the 3-field
/// `(Object[], pos, fence)` snapshot that `--compatible`'s stream and
/// `DirectoryStream` natives allocate as `java/util/Spliterator`. Under
/// `--jdk-only` those producers are dropped stubs. Measured on the round-16
/// base binary with `--dump-native-registry`: `R16CollectionsSpliter` (19
/// spliterator families, all six methods through the interface) and
/// `R16CollectionsDirSpl` (`DirectoryStream.spliterator`, `Files.list`) print
/// HotSpot's output under `--jdk-only` (`--nojit` with
/// `CRATONVM_DISABLE_INTRINSICS=1`, and with the JIT) with 0 invocations on all
/// six rows. Under `--compatible` the rows are live and stay registered
/// unchanged; that mode's divergences on `R16CollectionsSpliter` are filed
/// separately (`r16-collections-compatible-spliterator-shims-diverge-FIXED-20261001.md`).
///
/// As a `Bridge` each row stayed registered under `--jdk-only` and, through the
/// JIT's constant-pool screen, made every `Spliterator.tryAdvance` /
/// `forEachRemaining` site in a real stream pipeline read as natively
/// shadowed. `CRATONVM_SPLITERATOR_IFACE_SHIM_STUB=0` is the kill switch: it
/// restores `Bridge`.
pub fn spliterator_interface_shim_kind() -> cratonvm_native_api::NativeKind {
    // Opt-in since JIT round 16 wave 8 verification (host build w8a): two live
    // `--jdk-only` producers still mint instances of the `Spliterator` / `Stream`
    // interfaces themselves (`phases_late/reflect_invoke.rs` StackWalker.walk's
    // stream, and `nio_file.rs` DirectoryStream.spliterator before wave 8b), so
    // dropping these rows raised AbstractMethodError
    // (`wp1_9_stackwalker` log4j tests, `R16CollectionsDirSpl`). Flip back once
    // every producer builds through the JDK's own bytecode.
    //
    // JIT round 17 wave 1 (lane streams): by the producer census in
    // `r17-compatible-stream-residuals-FIXED-20261005.md` they all do now
    // (StackWalker.walk answers `Arrays.stream(Object[])` under `--jdk-only`
    // since SBR w4; DirectoryStream runs the JDK body since wave 8b). The flip
    // waits for one Linux battery with `=1`; the recipe is on that page.
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_SPLITERATOR_IFACE_SHIM_STUB") {
        cratonvm_native_api::NativeKind::SyntheticStub
    } else {
        cratonvm_native_api::NativeKind::Bridge
    }
}

pub(crate) fn register_iterator_protocol_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Generic java/util/Iterator interface — fallback to snapshot iterator.
    // Kind stated, not ambient: see `iterator_interface_shim_kind`.
    let itr = "java/util/Iterator";
    let shim_kind = iterator_interface_shim_kind();
    r.register_with_kind(
        itr,
        "hasNext",
        "()Z",
        native_snapshot_itr_has_next,
        shim_kind,
    );
    r.register_with_kind(
        itr,
        "next",
        "()Ljava/lang/Object;",
        native_snapshot_itr_next,
        shim_kind,
    );
    // §1.4 REVIEWED EXCEPTION — the only `Intrinsic` in this crate.
    //
    // WHY IT CANNOT BE A BRIDGE. `resolve_native_dispatch_wave1`
    // (`vm/src/vm/vm_exec.rs:824`) sends a `Bridge` to the real bytecode
    // whenever bytecode exists, and the force-native sites pass
    // `bytecode_available: true` by construction
    // (`admit_forced_native_id`, `native_override.rs`). The "real bytecode"
    // for `Iterator.remove()` is the interface DEFAULT method, whose entire
    // body is `throw new UnsupportedOperationException("remove")`. So under
    // `--jdk-only` a bridge here is not a choice between our implementation and
    // the JDK's — it is a choice between our implementation and an
    // unconditional throw.
    //
    // WHY THE THROW IS NOT THE HONEST ANSWER. CratonVM allocates
    // `java.util.HashSet` in its own compact layout, so `HashSet.iterator()` is
    // intercepted and the real `HashMap$KeyIterator` — the class whose
    // `remove()` the JDK would have run — cannot exist. Strict mode refuses our
    // fabricated `HashMap$KeyItr` and gets a real `Arrays$ArrayItr` instead
    // (`real_snapshot_iterator`), which declares no `remove()` at all. Every
    // producer of a working `remove()` on that path has been removed by the
    // VM's own substitutions; this registration is what puts one back.
    // `com.sun.jmx.mbeanserver.MXBeanSupport.findMXBeanInterface` reduces a
    // `HashSet` with `it.remove()` inside `while (candidates.size() > 1)`, so
    // without it `ManagementFactory.getPlatformMBeanServer()` fails outright
    // (`regression-suite/src/RJdkJmx.java:133`).
    //
    // WHY THE BREADTH IS SAFE. The `Intrinsic` is broad by necessity — the
    // force-native site keys on the DECLARING class, which is
    // `java/util/Iterator` for exactly the receivers that have no `remove()` of
    // their own — but `native_itr_remove_noop` is not: for any receiver it does
    // not specifically implement it hands the call to that receiver's own real
    // bytecode via `invoke_virtual_bytecode_only`, and only answers UOE where
    // the real bytecode would have. See the fall-through there.
    //
    // NOT applied to `ListIterator.remove`/`set`/`add` below: those are
    // abstract on `ListIterator` (no default to fall back on), so they belong to
    // the sub-interface shim, which `--jdk-only` drops and the receiver's own
    // real bytecode serves (`iterator_subinterface_shim_kind`).
    r.register_with_kind(
        itr,
        "remove",
        "()V",
        native_itr_remove_noop,
        cratonvm_native_api::NativeKind::Intrinsic,
    );

    // ListIterator (extends Iterator). Kind stated, not ambient: see
    // `iterator_subinterface_shim_kind`.
    let litr = "java/util/ListIterator";
    let sub_kind = iterator_subinterface_shim_kind();
    r.register_with_kind(
        litr,
        "hasNext",
        "()Z",
        native_snapshot_itr_has_next,
        sub_kind,
    );
    r.register_with_kind(
        litr,
        "next",
        "()Ljava/lang/Object;",
        native_snapshot_itr_next,
        sub_kind,
    );
    r.register_with_kind(
        litr,
        "hasPrevious",
        "()Z",
        native_list_itr_has_previous,
        sub_kind,
    );
    r.register_with_kind(
        litr,
        "previous",
        "()Ljava/lang/Object;",
        native_list_itr_previous,
        sub_kind,
    );
    r.register_with_kind(
        litr,
        "nextIndex",
        "()I",
        native_list_itr_next_index,
        sub_kind,
    );
    r.register_with_kind(
        litr,
        "previousIndex",
        "()I",
        native_list_itr_previous_index,
        sub_kind,
    );
    r.register_with_kind(litr, "remove", "()V", native_itr_remove_noop, sub_kind);
    r.register_with_kind(
        litr,
        "set",
        "(Ljava/lang/Object;)V",
        native_itr_remove_noop,
        sub_kind,
    );
    r.register_with_kind(
        litr,
        "add",
        "(Ljava/lang/Object;)V",
        native_itr_remove_noop,
        sub_kind,
    );

    // Spliterator stubs. Kind stated, not ambient: see
    // `spliterator_interface_shim_kind`.
    let spl = "java/util/Spliterator";
    let spl_kind = spliterator_interface_shim_kind();
    r.register_with_kind(
        spl,
        "estimateSize",
        "()J",
        native_spliterator_estimate_size,
        spl_kind,
    );
    r.register_with_kind(
        spl,
        "characteristics",
        "()I",
        native_spliterator_characteristics,
        spl_kind,
    );
    r.register_with_kind(
        spl,
        "getComparator",
        "()Ljava/util/Comparator;",
        native_spliterator_get_comparator,
        spl_kind,
    );
    r.register_with_kind(
        spl,
        "tryAdvance",
        "(Ljava/util/function/Consumer;)Z",
        native_spliterator_try_advance,
        spl_kind,
    );
    r.register_with_kind(
        spl,
        "trySplit",
        "()Ljava/util/Spliterator;",
        native_spliterator_try_split,
        spl_kind,
    );
    r.register_with_kind(
        spl,
        "forEachRemaining",
        "(Ljava/util/function/Consumer;)V",
        native_spliterator_for_each_remaining,
        spl_kind,
    );

    // Spliterators factory.
    //
    // Real-JDK gate: `native_spliterators_empty` returns a synthetic instance
    // of the ABSTRACT base `java/util/Spliterators$EmptySpliterator`. The real
    // `Spliterators.emptySpliterator()` returns the concrete subclass
    // `EmptySpliterator$OfRef`, on which the `tryAdvance(Consumer)Z` /
    // `forEachRemaining(Consumer)V` *bridge* methods are declared (the abstract
    // base only declares the erased `tryAdvance(Object)Z`). A caller doing
    // `emptySpliterator().tryAdvance(consumer)` against the synthetic abstract
    // instance therefore fails interface dispatch with
    //   NoSuchMethodError: ...EmptySpliterator.tryAdvance(Ljava/util/function/Consumer;)Z
    // which broke kafka-clients `clients.admin` / `clients.producer*` test
    // discovery (and any empty-spliterator consumer). In real-JDK mode the real
    // bytecode produces the correct `OfRef` (verified: bridge + super
    // `EmptySpliterator.tryAdvance(Object)` run normally), so leave it alone and
    // only install the synthetic factory in synthetic-jdk builds — mirroring the
    // `register_blocking_queue_natives` gate above (synthetic layout vs. real
    // JDK layout conflict).
    //
    // 2026-09-17: the cfg ALONE was the wrong guard, for the reason
    // `NativeMethodRegistry::drops_real_layout_synthetic` spells out (see the
    // `java/io/InputStreamReader` fix in `native-builtins/src/servlet.rs`,
    // found the same day): the Cargo feature decides what is COMPILED, the
    // launcher flag decides which CLASS LIBRARY is LOADED. A `--features
    // synthetic-jdk` binary run against a real JDK satisfied the cfg and
    // installed this abstract-class shim over the real, self-contained
    // `Spliterators.emptySpliterator()` bytecode anyway — exactly the
    // NoSuchMethodError above. Add the runtime half so a feature-enabled
    // binary actually running real-JDK mode leaves the real bytecode alone.
    #[cfg(feature = "synthetic-jdk")]
    if !r.drops_real_layout_synthetic() {
        let spls = "java/util/Spliterators";
        r.register(
            spls,
            "emptySpliterator",
            "()Ljava/util/Spliterator;",
            native_spliterators_empty,
        );
    }

    // Collections.emptyIterator / emptyListIterator / emptyEnumeration
    let colls = "java/util/Collections";
    r.register(
        colls,
        "emptyIterator",
        "()Ljava/util/Iterator;",
        native_empty_iterator,
    );
    r.register(
        colls,
        "emptyListIterator",
        "()Ljava/util/ListIterator;",
        native_empty_list_iterator,
    );
    // Bug fix: previously this allocated `Collections$EmptyIterator` as an
    // Enumeration. EmptyIterator is an Iterator (only has hasNext/next/remove)
    // and lacks hasMoreElements/nextElement, so any subsequent
    // `Enumeration.hasMoreElements()` dispatch (e.g. from
    // `BuiltinClassLoader$1.hasNext`) raised NoSuchMethodError. Allocate the
    // correct `Collections$EmptyEnumeration` class so real-JDK bytecode
    // (`hasMoreElements`/`nextElement` defined on EmptyEnumeration itself)
    // resolves normally.
    r.register(
        colls,
        "emptyEnumeration",
        "()Ljava/util/Enumeration;",
        native_empty_enumeration,
    );
    register_primitive_iterator_natives(r);
    r.set_category(__prev_cat);
}

/// JIT round 16 wave 7 (lane arraylist): the interface-triple
/// `Iterator.hasNext`/`next` shim is a stated `SyntheticStub`
/// ([`iterator_interface_shim_kind`]): refused under `--jdk-only`, registered
/// unchanged under `--compatible`, `Bridge` again with the kill switch.
#[cfg(test)]
mod iterator_interface_shim_kind_tests {
    use super::register_iterator_protocol_natives;
    use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
    use cratonvm_types::compat::CompatibilityMode;

    const ITR: &str = "java/util/Iterator";
    const SHIM: [(&str, &str); 2] = [("hasNext", "()Z"), ("next", "()Ljava/lang/Object;")];

    fn registry(mode: CompatibilityMode) -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        r.set_compatibility_mode(mode);
        register_iterator_protocol_natives(&mut r);
        r
    }

    #[test]
    fn jdk_only_drops_the_shim_and_keeps_remove() {
        let r = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ITERATOR_IFACE_SHIM_STUB", None)],
            || registry(CompatibilityMode::JdkOnly),
        );
        for (m, d) in SHIM {
            assert!(r.find(ITR, m, d).is_none(), "{m}{d} survived --jdk-only");
        }
        // The reviewed `Intrinsic` for the interface DEFAULT `remove()` is not
        // part of the shim and stays.
        assert_eq!(r.kind_of(ITR, "remove", "()V"), Some(NativeKind::Intrinsic));
    }

    #[test]
    fn compatible_registers_the_shim_as_a_stated_stub() {
        let r = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ITERATOR_IFACE_SHIM_STUB", None)],
            || registry(CompatibilityMode::Compatible),
        );
        for (m, d) in SHIM {
            assert_eq!(
                r.kind_of(ITR, m, d),
                Some(NativeKind::SyntheticStub),
                "{m}{d}"
            );
        }
    }

    #[test]
    fn the_kill_switch_restores_the_bridge() {
        let r = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ITERATOR_IFACE_SHIM_STUB", Some("0"))],
            || registry(CompatibilityMode::JdkOnly),
        );
        for (m, d) in SHIM {
            assert_eq!(r.kind_of(ITR, m, d), Some(NativeKind::Bridge), "{m}{d}");
        }
    }

    // JIT round 16 wave 8 (lane collections): the SUB-interface shims,
    // `iterator_subinterface_shim_kind`.
    const SUB_SHIM: [(&str, &str, &str); 18] = [
        ("java/util/ListIterator", "hasNext", "()Z"),
        ("java/util/ListIterator", "next", "()Ljava/lang/Object;"),
        ("java/util/ListIterator", "hasPrevious", "()Z"),
        ("java/util/ListIterator", "previous", "()Ljava/lang/Object;"),
        ("java/util/ListIterator", "nextIndex", "()I"),
        ("java/util/ListIterator", "previousIndex", "()I"),
        ("java/util/ListIterator", "remove", "()V"),
        ("java/util/ListIterator", "set", "(Ljava/lang/Object;)V"),
        ("java/util/ListIterator", "add", "(Ljava/lang/Object;)V"),
        ("java/util/PrimitiveIterator$OfInt", "hasNext", "()Z"),
        (
            "java/util/PrimitiveIterator$OfInt",
            "next",
            "()Ljava/lang/Object;",
        ),
        ("java/util/PrimitiveIterator$OfInt", "nextInt", "()I"),
        ("java/util/PrimitiveIterator$OfLong", "hasNext", "()Z"),
        (
            "java/util/PrimitiveIterator$OfLong",
            "next",
            "()Ljava/lang/Object;",
        ),
        ("java/util/PrimitiveIterator$OfLong", "nextLong", "()J"),
        ("java/util/PrimitiveIterator$OfDouble", "hasNext", "()Z"),
        (
            "java/util/PrimitiveIterator$OfDouble",
            "next",
            "()Ljava/lang/Object;",
        ),
        ("java/util/PrimitiveIterator$OfDouble", "nextDouble", "()D"),
    ];

    #[test]
    fn jdk_only_drops_the_subinterface_shims() {
        let r = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ITERATOR_SUBIFACE_SHIM_STUB", None)],
            || registry(CompatibilityMode::JdkOnly),
        );
        for (c, m, d) in SUB_SHIM {
            assert!(r.find(c, m, d).is_none(), "{c}.{m}{d} survived --jdk-only");
        }
    }

    #[test]
    fn compatible_registers_the_subinterface_shims_as_stated_stubs() {
        let r = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ITERATOR_SUBIFACE_SHIM_STUB", None)],
            || registry(CompatibilityMode::Compatible),
        );
        for (c, m, d) in SUB_SHIM {
            assert_eq!(
                r.kind_of(c, m, d),
                Some(NativeKind::SyntheticStub),
                "{c}.{m}{d}"
            );
        }
    }

    #[test]
    fn the_subinterface_kill_switch_restores_the_bridges() {
        let r = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ITERATOR_SUBIFACE_SHIM_STUB", Some("0"))],
            || registry(CompatibilityMode::JdkOnly),
        );
        for (c, m, d) in SUB_SHIM {
            assert_eq!(r.kind_of(c, m, d), Some(NativeKind::Bridge), "{c}.{m}{d}");
        }
    }

    const SPL: &str = "java/util/Spliterator";
    const SPL_SHIM: [(&str, &str); 6] = [
        ("estimateSize", "()J"),
        ("characteristics", "()I"),
        ("getComparator", "()Ljava/util/Comparator;"),
        ("tryAdvance", "(Ljava/util/function/Consumer;)Z"),
        ("trySplit", "()Ljava/util/Spliterator;"),
        ("forEachRemaining", "(Ljava/util/function/Consumer;)V"),
    ];

    #[test]
    fn jdk_only_drops_the_spliterator_shims() {
        let r = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_SPLITERATOR_IFACE_SHIM_STUB", Some("1"))],
            || registry(CompatibilityMode::JdkOnly),
        );
        for (m, d) in SPL_SHIM {
            assert!(r.find(SPL, m, d).is_none(), "{m}{d} survived --jdk-only");
        }
    }

    #[test]
    fn compatible_registers_the_spliterator_shims_as_stated_stubs() {
        let r = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_SPLITERATOR_IFACE_SHIM_STUB", Some("1"))],
            || registry(CompatibilityMode::Compatible),
        );
        for (m, d) in SPL_SHIM {
            assert_eq!(
                r.kind_of(SPL, m, d),
                Some(NativeKind::SyntheticStub),
                "{m}{d}"
            );
        }
    }

    #[test]
    fn the_spliterator_shims_are_bridges_unless_opted_in() {
        let r = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_SPLITERATOR_IFACE_SHIM_STUB", None)],
            || registry(CompatibilityMode::JdkOnly),
        );
        for (m, d) in SPL_SHIM {
            assert_eq!(r.kind_of(SPL, m, d), Some(NativeKind::Bridge), "{m}{d}");
        }
    }
}

pub(crate) fn native_empty_enumeration(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let en = alloc_empty_array_holder(ctx, "java/util/Collections$EmptyEnumeration", 2)?;
    Ok(Some(Value::Object(Some(en))))
}

pub(crate) fn native_itr_remove_noop(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Dispatcher: the force-native override in `vm_exec::invoke_on_class_shared_inner`
    // routes every `invokeinterface Iterator.remove ()V` here, regardless of
    // the receiver's runtime class. Route by class so iterators that DO
    // support remove (e.g. HashMap$KeyItr backed by a real HashSet) run their
    // own native instead of throwing. Without this, JDK code that does
    // `it.remove()` inside a HashSet/HashMap iteration loop (e.g.
    // `MXBeanSupport.findMXBeanInterface` during platform-MBean registration)
    // unconditionally fails with UnsupportedOperationException, blocking
    // jboss-modules / WildFly / Keycloak boot.
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        let cid = ctx.class_id_of_object(this);
        let cn = ctx.class_name_of_id(cid).unwrap_or_default();
        // The fabricated `HashMap$KeyItr` and the real classes that replaced it
        // (`MAP_KEY_ITR_CARRIERS`) share one body — asked by name, not by a
        // literal, so a new carrier cannot be added without this route seeing it.
        if is_map_key_itr_class(&cn) {
            return native_map_key_itr_remove(ctx, args);
        }
        match cn.as_str() {
            "java/util/ArrayList$Itr" | "java/util/ArrayList$ListItr" => {
                return native_al_itr_remove(ctx, args);
            }
            "java/util/TreeSet$Itr" => return native_ts_itr_remove(ctx, args),
            // `LinkedList.iterator()` returns our forward `LinkedList$Itr`
            // (node-backed, fully mutable). Its `remove()` unlinks the last
            // node returned by `next()` — route to it so JDK code that does
            // `it.remove()` while draining a LinkedList works instead of
            // hitting the throwing default below. (Real-RAF avrora's
            // `Medium$Receiver.earliestNewTransmission` does exactly this on a
            // `LinkedList<Transmission>`, and the UOE it threw polluted
            // System.err → DaCapo digest mismatch. The snapshot-based
            // `LinkedList$ListItr` is intentionally NOT routed here: it has no
            // backing list and correctly throws UOE.)
            "java/util/LinkedList$Itr" => return native_ll_itr_remove(ctx, args),
            // The `--jdk-only` stand-in. `Arrays$ArrayItr` is a REAL JDK class
            // that declares no `remove()`, so without this arm the call reaches
            // the `Iterator.remove()` default and throws — the
            // `MXBeanSupport.findMXBeanInterface` failure this arm exists for.
            // Answers UOE itself when no backing was recorded, so a genuine
            // snapshot (`Arrays.asList(a).iterator()`) is unaffected.
            "java/util/Arrays$ArrayItr" => return native_snapshot_itr_remove(ctx, args),
            // The live sublist iterator. `native_sli_remove` screens the
            // receiver itself (`sli_delegate_foreign`), so an iterator
            // java.base built keeps running its own `remove()` — which is the
            // whole reason this arm can name a REAL JDK class at all.
            SLI_CLASS => return native_sli_remove(ctx, args),
            _ => {}
        }
    }
    // Anything else: hand the call to the receiver's OWN real bytecode rather
    // than answering for it.
    //
    // This matters because the `("java/util/Iterator", "remove", "()V")`
    // registration is `Intrinsic` (see `register_iterator_protocol_natives`),
    // i.e. it wins under `--jdk-only` too, and the force-native entry in
    // `native_override.rs` routes EVERY `Iterator.remove()` here regardless of
    // receiver. A native that swallowed all of them would break every real JDK
    // iterator whose `remove()` works today — far worse than the bug this file
    // is fixing. The two guards:
    //
    //   * `args.len() == 1` — this same callback is also registered for
    //     `ListIterator.set(Object)` and `.add(Object)`, and a 2-argument call
    //     must not be turned into a `remove()`.
    //   * a concrete `remove()V` on the receiver's CLASS chain, and not a
    //     compatibility stub. `method_exists` does not walk interfaces, so a
    //     receiver whose only `remove()` is the throwing `Iterator` default
    //     answers `false` here and gets the identical UOE below without a
    //     re-entry; and it answers `true` for a synthetic stub (whose methods
    //     live in the native registry, not in a class file), where there is no
    //     bytecode to reach — hence the second half of the test.
    if args.len() == 1 {
        if let Some(Value::Object(Some(this))) = args.first().copied() {
            let cid = ctx.class_id_of_object(this);
            let cn = ctx.class_name_of_id(cid).unwrap_or_default();
            if !cn.is_empty()
                && !ctx.is_class_synthetic_stub(&cn)
                && ctx.method_exists(&cn, "remove", "()V")
            {
                return ctx.invoke_virtual_bytecode_only(this, "remove", "()V", &[]);
            }
        }
    }
    // Snapshot-only iterators (no backing collection) genuinely do not
    // support remove — throw the spec-mandated UOE. The message matters: the
    // `Iterator.remove()` default method throws
    // `UnsupportedOperationException("remove")`, while `Collections`
    // unmodifiable views and `ImmutableCollections` throw a message-less one,
    // so this text is what keeps the two distinguishable in a stack trace.
    Err(
        cratonvm_types::error::RuntimeError::UnsupportedOperationException {
            message: "remove".to_string(),
        }
        .into(),
    )
}

/// `remove()` for the `--jdk-only` snapshot stand-in — a real
/// `java.util.Arrays$ArrayItr` whose backing collection is recorded in
/// [`snapshot_itr_backing_table`].
///
/// The real class has `cursor` and `a` and nothing else, so:
///
///   * the element to delete is `a[cursor - 1]` — `ArrayItr.next()` is real
///     bytecode (`return a[cursor++]`), so the cursor already points one past
///     the element it returned;
///   * `cursor` is deliberately NOT rewound. `a` is a SNAPSHOT: removing from
///     the backing collection does not shift it, so rewinding would re-visit
///     an element. This mirrors `native_map_key_itr_remove`, which likewise
///     leaves its cursor alone and only clears `lastRet`.
pub(crate) fn native_snapshot_itr_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let state = match snapshot_itr_backing(ctx, this) {
        Some(s) => s,
        // A genuine snapshot with no collection behind it — `Arrays.asList`,
        // `List.of`, an unmodifiable view. UOE is the correct answer, and it
        // is what the real `Arrays$ArrayItr` would have produced anyway.
        None => {
            return Err(
                cratonvm_types::error::RuntimeError::UnsupportedOperationException {
                    message: "remove".to_string(),
                }
                .into(),
            )
        }
    };
    let cursor_slot = match ctx.resolve_field_index("java/util/Arrays$ArrayItr", "cursor") {
        Some(s) => s,
        None => return Err(unsupported_op()),
    };
    let a_slot = match ctx.resolve_field_index("java/util/Arrays$ArrayItr", "a") {
        Some(s) => s,
        None => return Err(unsupported_op()),
    };
    let cursor = match ctx.get_field(this, cursor_slot) {
        Value::Int(v) => v,
        _ => 0,
    };
    // `cursor == 0` is "next() was never called"; an unchanged cursor since the
    // last removal is "remove() twice in a row". The JDK expresses both as
    // `lastRet < 0` and raises IllegalStateException.
    if cursor <= 0 || state.last_removed_cursor == cursor {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "remove".to_string(),
        }
        .into());
    }
    let arr = match ctx.get_field(this, a_slot) {
        Value::Object(Some(a)) => a,
        _ => return Err(unsupported_op()),
    };
    let last = ctx.get_array_element(arr, (cursor - 1) as usize);
    // GC-SAFETY: the removal below dispatches `hashCode`/`equals`/`compare` and
    // allocates, so the iterator can move under us. Root it and read it back
    // before the bookkeeping write. The backing came out of the side table,
    // which the collector remaps, so it is already a current address.
    let this_pin = ctx.pin_native_root(this);
    let removed = match state.route {
        SnapshotItrRoute::SetLike => {
            let backing = state.backing;
            if is_key_set_view(ctx, backing) {
                native_ksv_remove(ctx, &[Value::Object(Some(backing)), last])
            } else {
                native_hs_remove(ctx, &[Value::Object(Some(backing)), last])
            }
        }
        SnapshotItrRoute::TreeSet => ts_remove_element(ctx, state.backing, last),
        SnapshotItrRoute::LinkedList => {
            native_ll_remove_object(ctx, &[Value::Object(Some(state.backing)), last])
        }
        SnapshotItrRoute::ArrayDeque => {
            native_ad_remove_first_occurrence(ctx, &[Value::Object(Some(state.backing)), last])
        }
        SnapshotItrRoute::ViewCollection => {
            native_al_remove_obj(ctx, &[Value::Object(Some(state.backing)), last])
        }
    };
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let _ = removed?;
    note_snapshot_itr_removed(ctx, this, cursor);
    Ok(None)
}

// ListIterator extras (snapshot-based: field 0 = array, field 1 = cursor)
pub(crate) fn native_list_itr_has_previous(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(if cursor > 0 { 1 } else { 0 })))
}

pub(crate) fn native_list_itr_previous(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    if cursor <= 0 {
        return Ok(Some(Value::Object(None)));
    }
    let new_cursor = cursor - 1;
    ctx.set_field(this, 1, Value::Int(new_cursor));
    Ok(Some(ctx.get_array_element(arr, new_cursor as usize)))
}

pub(crate) fn native_list_itr_next_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(cursor)))
}

pub(crate) fn native_list_itr_previous_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(cursor - 1)))
}

// Spliterator stubs (snapshot-based: field 0 = array, field 1 = cursor)
pub(crate) fn native_spliterator_estimate_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let raw = spl_raw_bits(ctx, this);
    let chars = raw & !SPL_PRIV_MASK;
    // Wave 9 (lane spliter): a stream stage's `WrappingSpliterator` reports
    // its SOURCE's estimate, sized or not (`Stream.of(x, y, z).filter(..)`:
    // characteristics 0x10, estimate 3).
    if raw & SPL_PRIV_FIXED_EST != 0 && ctx.object_num_fields(this) > SPL_FIELD_FIXED_EST {
        if let Value::Long(n) = ctx.get_field(this, SPL_FIELD_FIXED_EST) {
            let taken = match ctx.get_field(this, 1) {
                Value::Int(v) => i64::from(v.max(0)),
                _ => 0,
            };
            return Ok(Some(Value::Long((n - taken).max(0))));
        }
    }
    // Unsized alone is not enough to mean "unknown/too expensive": a
    // CONCURRENT source (`ConcurrentHashMap`'s KeySetView spliterator,
    // `SPLITERATOR_CHM_KEYSET_CHARACTERISTICS` above) is also unsized on
    // real JDK, yet still returns a real, cheaply-tracked estimate from
    // its snapshot array (`RChmKeySetView` asserts
    // `s.spliterator().estimateSize() == 3` for a 3-element view) — it is
    // only the unsized, non-concurrent case (TreeSet's headSet/tailSet/
    // subSet range views, which have no cheap count without a full scan)
    // that HotSpot answers `Long.MAX_VALUE` for.
    if (chars & SPL_SIZED) == 0 && (chars & SPL_CONCURRENT) == 0 {
        return Ok(Some(Value::Long(i64::MAX)));
    }
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Long(0))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    let len = ctx.array_length(arr);
    Ok(Some(Value::Long(len.saturating_sub(cursor) as i64)))
}

/// `Spliterator` characteristic bits, as `java.util.Spliterator` declares them.
pub(crate) const SPL_DISTINCT: i32 = 0x0001;
pub(crate) const SPL_SORTED: i32 = 0x0004;
pub(crate) const SPL_ORDERED: i32 = 0x0010;
pub(crate) const SPL_SIZED: i32 = 0x0040;
pub(crate) const SPL_SUBSIZED: i32 = 0x4000;
/// What every synthetic spliterator reported before the mask moved into the
/// object: an ordered, sized, splittable sequence. Right for the list-shaped
/// producers, and the default for any site that does not say otherwise.
pub(crate) const SPL_LIST_DEFAULT: i32 = SPL_ORDERED | SPL_SIZED | SPL_SUBSIZED;
/// `HashMap.KeySpliterator`: `SIZED | DISTINCT`, and NOT `ORDERED` — claiming an
/// encounter order a hash container does not have is what a stream pipeline
/// reads to decide it may skip a sort.
pub(crate) const SPL_HASH_SET: i32 = SPL_SIZED | SPL_DISTINCT;
pub(crate) const SPL_NONNULL: i32 = 0x0100;
pub(crate) const SPL_IMMUTABLE: i32 = 0x0400;
pub(crate) const SPL_CONCURRENT: i32 = 0x1000;

// PRIVATE bits of the characteristics slot (JIT round 16 wave 9, lane
// spliter). `java.util.Spliterator` declares nothing above `SUBSIZED`
// (0x4000), so these ride in the same `Int` as the public bits and
// [`spl_characteristics`] strips them before anything Java-visible reads the
// slot. They carry what HotSpot's spliterator classes decide in code rather
// than in their characteristics:
//
/// `trySplit()` answers null: a sequential `WrappingSpliterator` (any stream
/// stage with an intermediate op) and a `TreeMap` sub-map key spliterator
/// (`TreeSet.headSet(..)`, `TreeMap.headMap(..).keySet()`).
pub(crate) const SPL_PRIV_NO_SPLIT: i32 = 0x0100_0000;
/// A red-black-tree spliterator (`TreeMap.KeySpliterator` and its value,
/// entry and descending siblings): its first split hands off the elements
/// below the ROOT, so it declines while the root is the first element. In a
/// red-black tree that is only possible at two or fewer elements; at exactly
/// two it depends on insertion order -- `R16SpliterW9` `TreeSet.two` vs
/// `TreeSet.two.rev` measures both. Without [`SPL_PRIV_PAIR_SPLIT`] the
/// ascending order (root first, no split) is the one taken.
pub(crate) const SPL_PRIV_TREE_SPLIT: i32 = 0x0200_0000;
/// `getComparator()` answers `Map.Entry.comparingByKey(..)` over the
/// comparator slot (`TreeMap.EntrySpliterator`): never null, even for a
/// natural-ordered map.
pub(crate) const SPL_PRIV_ENTRY_CMP: i32 = 0x0400_0000;
/// `estimateSize()` answers [`SPL_FIELD_FIXED_EST`] less the elements already
/// taken: a `WrappingSpliterator` reports its SOURCE's estimate, which for an
/// unsized stage (`filter`, `distinct`, `flatMap`) is not the snapshot's
/// length.
pub(crate) const SPL_PRIV_FIXED_EST: i32 = 0x0800_0000;
/// On a STREAM stage's facts only ([`STREAM_FIELD_SPL_FACTS`]): the stage is
/// an intermediate op, so its spliterator is a `WrappingSpliterator`.
pub(crate) const SPL_PRIV_WRAPPED: i32 = 0x1000_0000;
/// With [`SPL_PRIV_TREE_SPLIT`], on a spliterator over a whole two-element
/// tree: its root is NOT the first element the spliterator walks, so the first
/// `trySplit()` does split (round 16 wave 13, lane compatcoll;
/// [`spliterator_tree_pair_root_enabled`]).
pub(crate) const SPL_PRIV_PAIR_SPLIT: i32 = 0x2000_0000;
/// "Described, and the public characteristics really are 0" (JIT round 17
/// wave 2, lane streams2; [`stream_zero_facts_enabled`]). A stage's facts word
/// and a spliterator's characteristics slot both read 0 as "nothing recorded"
/// (the list default), so a source whose `characteristics()` is 0
/// (`Spliterators.spliteratorUnknownSize(it, 0)`, the `Iterable.spliterator()`
/// default) could not be recorded at all. This bit makes such a word non-zero;
/// it is private, so every public answer still reads 0.
pub(crate) const SPL_PRIV_DESCRIBED: i32 = 0x4000_0000;
pub(crate) const SPL_PRIV_MASK: i32 = SPL_PRIV_NO_SPLIT
    | SPL_PRIV_TREE_SPLIT
    | SPL_PRIV_ENTRY_CMP
    | SPL_PRIV_FIXED_EST
    | SPL_PRIV_WRAPPED
    | SPL_PRIV_PAIR_SPLIT
    | SPL_PRIV_DESCRIBED;

/// THE CHARACTERISTICS MATRIX, measured cell by cell rather than derived.
///
/// The first version of this file's spliterator work carried three constants
/// and got three cells right, because the probe that drove it asked about three
/// receivers. `apps/probes/UtilCoverageSweep`'s matrix asks all twenty-eight and
/// found TWELVE wrong — every map VIEW, and the standalone `LinkedHashSet`. The
/// numbers below are HotSpot 25.0.4+7's own answers:
///
/// ```text
///                  keySet   values   entrySet    standalone set
///   HashMap           65       64       65             65
///   LinkedHashMap  16465    16464    16465          16465
///   TreeMap           85       80       85             85
///   Hashtable      16449    16448    16449             --
///   Properties      4353     4352    16449             --
/// ```
///
/// Three things in there are not guessable from the interface:
///
/// * **`values` is never DISTINCT** and, for `HashMap`, not `ORDERED` either --
///   a values view can repeat and a hash map has no encounter order. `TreeMap`'s
///   values ARE ordered but not SORTED: the sort is on the keys.
/// * **`SUBSIZED` follows the JDK's construction, not the container.** The
///   `LinkedHashMap` and `Hashtable` families go through
///   `Spliterators.spliterator(Collection, ..)`, which adds `SIZED | SUBSIZED`;
///   `HashMap`'s and `TreeMap`'s have hand-written spliterator classes that do
///   not claim it. That is why `LinkedHashSet` is 16465 and `HashSet` is 65
///   despite being the same shape of container.
/// * **`Properties` is CONCURRENT | NONNULL and NOT SIZED**, because JDK 25
///   backs it with a `ConcurrentHashMap` -- the same fact behind this record's
///   §6.6 -- and its `entrySet` is the `Hashtable` cell rather than the
///   concurrent one. That asymmetry is HotSpot's; it is recorded here because
///   it is exactly the kind of thing a derived table would smooth over.
pub(crate) const SPL_LINKED_SET: i32 = SPL_SIZED | SPL_DISTINCT | SPL_ORDERED | SPL_SUBSIZED;
/// `LinkedHashMap.values()` -- ordered, sized, splittable, and not distinct.
/// Numerically [`SPL_LIST_DEFAULT`]; kept as its own name so the matrix reads
/// as a matrix and a later change to one cell cannot silently move the other.
pub(crate) const SPL_LINKED_VALUES: i32 = SPL_SIZED | SPL_ORDERED | SPL_SUBSIZED;
/// `HashMap.values()` -- sized and nothing else.
pub(crate) const SPL_HASH_VALUES: i32 = SPL_SIZED;
/// `TreeMap.KeySpliterator`: `SIZED | DISTINCT | SORTED | ORDERED`. No
/// `SUBSIZED` — the JDK's does not claim it either.
pub(crate) const SPL_TREE_SET: i32 = SPL_SIZED | SPL_DISTINCT | SPL_SORTED | SPL_ORDERED;
/// `TreeMap.values()` -- ordered by the keys, so ORDERED but not SORTED, and
/// not DISTINCT.
pub(crate) const SPL_TREE_VALUES: i32 = SPL_SIZED | SPL_ORDERED;
/// `Hashtable`'s views, which take the `Spliterators.spliterator(Collection..)`
/// construction and so claim `SUBSIZED`, but have no encounter order.
pub(crate) const SPL_HASHTABLE_SET: i32 = SPL_SIZED | SPL_DISTINCT | SPL_SUBSIZED;
pub(crate) const SPL_HASHTABLE_VALUES: i32 = SPL_SIZED | SPL_SUBSIZED;
/// `Properties`' key and value views, which are a `ConcurrentHashMap`'s.
pub(crate) const SPL_CHM_SET: i32 = SPL_DISTINCT | SPL_CONCURRENT | SPL_NONNULL;
pub(crate) const SPL_CHM_VALUES: i32 = SPL_CONCURRENT | SPL_NONNULL;

/// Which of the three views a spliterator is being taken over.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SplViewKind {
    Keys,
    Values,
    Entries,
}

/// The measured characteristics for a view over `source`. See the matrix on
/// [`SPL_LINKED_SET`] for where every number comes from.
pub(crate) fn view_spliterator_characteristics(
    ctx: &dyn NativeContext,
    source: ObjectRef,
    kind: SplViewKind,
) -> i32 {
    let values = kind == SplViewKind::Values;
    let name = ctx.class_name_arc_of_id(ctx.class_id_of_object(source));
    // `Properties` before the Hashtable test it would otherwise satisfy: it
    // extends `Hashtable` and is backed by a `ConcurrentHashMap`, and only the
    // key and value views take the concurrent cell.
    if name.as_deref() == Some("java/util/Properties") {
        return match kind {
            SplViewKind::Keys => SPL_CHM_SET,
            SplViewKind::Values => SPL_CHM_VALUES,
            SplViewKind::Entries => SPL_HASHTABLE_SET,
        };
    }
    if is_tree_map_receiver(ctx, source) {
        return if values {
            SPL_TREE_VALUES
        } else {
            SPL_TREE_SET
        };
    }
    if is_lhm_receiver(ctx, source) {
        return if values {
            SPL_LINKED_VALUES
        } else {
            SPL_LINKED_SET
        };
    }
    if is_hashtable_receiver(ctx, source) {
        return if values {
            SPL_HASHTABLE_VALUES
        } else {
            SPL_HASHTABLE_SET
        };
    }
    if values {
        SPL_HASH_VALUES
    } else {
        SPL_HASH_SET
    }
}
/// The slot a synthetic spliterator keeps its characteristics in. Absent on the
/// three-field shape, which keeps [`SPL_LIST_DEFAULT`].
pub(crate) const SPL_FIELD_CHARACTERISTICS: usize = 3;

/// The slot a synthetic spliterator keeps a `getClass()` DISPLAY hint in —
/// read by `GetClassDisplay::Spliterator` in native-builtins. CONTRACT: keep
/// the index and the two non-zero codes below in sync with that reader.
/// Absent on any narrower shape, which is what makes the generic
/// `Spliterators$IteratorSpliterator` default safe for every OTHER
/// `try_alloc_synthetic(ctx, "java/util/Spliterator", ...)` call site in this
/// file: none of them sets this slot, so they all read back 0.
pub(crate) const SPL_FIELD_DISPLAY_HINT: usize = 4;
pub(crate) const SPL_DISPLAY_COLLECTIONS_2: i32 = 1;
pub(crate) const SPL_DISPLAY_RANDOM_ACCESS: i32 = 2;

/// The comparator `getComparator()` answers for a SORTED synthetic
/// spliterator (wave 9, lane spliter): `TreeSet(cmp)`'s comparator, or the
/// map comparator [`SPL_PRIV_ENTRY_CMP`] wraps. Absent or null on every
/// narrower shape, which keeps the natural-order `null`.
pub(crate) const SPL_FIELD_COMPARATOR: usize = 5;
/// The source estimate behind [`SPL_PRIV_FIXED_EST`], a `Value::Long`.
pub(crate) const SPL_FIELD_FIXED_EST: usize = 6;
/// The width of a spliterator that carries the wave-9 facts.
pub(crate) const SPL_FACTS_NUM_FIELDS: usize = 7;

/// The `getClass()` name HotSpot answers for an immutable factory's
/// spliterator, as a hint code for [`SPL_FIELD_DISPLAY_HINT`] (0 = the
/// generic `Spliterators$IteratorSpliterator` default). Mirrors
/// [`immutable_spliterator_characteristics`]'s own branches exactly — same
/// inputs, same shape, because that function's own doc comment says the rule
/// is keyed on the CLASS, which is what this reads out instead of the mask.
/// MEASURED, `apps/probes/ImmutableSplProbe`, HotSpot 25.0.4+7, both modes.
pub(crate) fn immutable_spliterator_display_hint(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    size: i32,
    is_map_view: bool,
) -> i32 {
    let cls = ctx.class_name_arc_of_id(ctx.class_id_of_object(this));
    if is_map_view {
        // keySet/values always take the generic default; only a size-1
        // entrySet is `Collections$2` (`Map1.entrySet()` IS `Set.of(entry)`).
        return match cls.as_deref() {
            Some(UNMOD_ENTRY_SET_CLASS) if size == 1 => SPL_DISPLAY_COLLECTIONS_2,
            _ => 0,
        };
    }
    if size == 1 {
        return SPL_DISPLAY_COLLECTIONS_2;
    }
    match cls.as_deref() {
        Some(UNMOD_LIST_CLASS) => SPL_DISPLAY_RANDOM_ACCESS,
        _ => 0,
    }
}

/// The characteristics of a synthetic spliterator: slot 3 when it has one, and
/// the list default otherwise.
///
/// A producer that knows better writes its own answer into slot 3 and this
/// reads it -- `native_ksv_spliterator` is one, because a `ConcurrentHashMap`
/// key-set spliterator has to report `CONCURRENT` and must not report
/// `ORDERED`. The width check comes first so the three-field shape is
/// untouched: a narrower object never reaches the field read, and one that is
/// wide enough but holds something other than an `Int` there (a different
/// synthetic family reusing the slot) falls back to the default rather than
/// answering garbage.
///
/// Public bits only: the wave-9 `SPL_PRIV_*` bits are stripped here
/// ([`spl_raw_bits`] keeps them).
pub(crate) fn spl_characteristics(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    spl_raw_bits(ctx, this) & !SPL_PRIV_MASK
}

/// Slot 3 as written, private bits included; the list default when absent.
pub(crate) fn spl_raw_bits(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    if ctx.object_num_fields(this) > SPL_FIELD_CHARACTERISTICS {
        if let Value::Int(c) = ctx.get_field(this, SPL_FIELD_CHARACTERISTICS) {
            if c != 0 {
                return c;
            }
        }
    }
    SPL_LIST_DEFAULT
}

/// The comparator slot of a wave-9 spliterator, `Object(None)` when absent.
pub(crate) fn spl_comparator_slot(ctx: &dyn NativeContext, this: ObjectRef) -> Value {
    if ctx.object_num_fields(this) > SPL_FIELD_COMPARATOR {
        if let v @ Value::Object(Some(_)) = ctx.get_field(this, SPL_FIELD_COMPARATOR) {
            return v;
        }
    }
    Value::Object(None)
}

/// Allocate a wave-9 synthetic spliterator over `arr` (all of it): `chars`
/// public and private bits, `comparator` in [`SPL_FIELD_COMPARATOR`], and the
/// source estimate when `fixed_est` is given (which sets
/// [`SPL_PRIV_FIXED_EST`]). `arr` and `comparator` are re-read through pins
/// across the allocation; the caller's own pins are untouched.
pub(crate) fn alloc_facts_spliterator(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    chars: i32,
    comparator: Value,
    fixed_est: Option<usize>,
) -> Result<ObjectRef, MethodCallFailed> {
    let arr_pin = ctx.pin_native_root(arr);
    let cmp_pin = pin_value(ctx, comparator);
    let spl = match try_alloc_synthetic(ctx, "java/util/Spliterator", SPL_FACTS_NUM_FIELDS) {
        Ok(s) => s,
        Err(e) => {
            ctx.unpin_native_roots(arr_pin);
            return Err(e);
        }
    };
    let arr = ctx.read_native_pin(arr_pin, arr);
    let comparator = read_pinned_elem(ctx, cmp_pin, comparator);
    ctx.unpin_native_roots(arr_pin);
    let len = ctx.array_length(arr);
    ctx.set_field(spl, 0, Value::Object(Some(arr)));
    ctx.set_field(spl, 1, Value::Int(0));
    ctx.set_field(spl, 2, Value::Int(len as i32));
    let chars = match fixed_est {
        Some(n) => {
            ctx.set_field(spl, SPL_FIELD_FIXED_EST, Value::Long(n as i64));
            chars | SPL_PRIV_FIXED_EST
        }
        None => chars & !SPL_PRIV_FIXED_EST,
    };
    ctx.set_field(spl, SPL_FIELD_CHARACTERISTICS, Value::Int(chars));
    if let Value::Object(Some(_)) = comparator {
        ctx.set_field(spl, SPL_FIELD_COMPARATOR, comparator);
    }
    Ok(spl)
}

pub(crate) fn native_spliterator_characteristics(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // WAS a constant `0x4050` for every producer. MEASURED against HotSpot
    // (apps/probes/UtilTailShadowSweep 141-145): a `TreeSet`'s spliterator must report
    // `SORTED | DISTINCT` and a `HashSet`'s must NOT report `ORDERED`.
    //
    // Not cosmetic. `Spliterator.getComparator()`'s DEFAULT body is
    // `throw new IllegalStateException()` unless `SORTED` is reported, so
    // `treeSet.spliterator().getComparator()` threw where the JDK answers
    // `null`; and a stream pipeline reads these bits to decide whether it may
    // skip a sort or split in parallel.
    match args.first() {
        Some(Value::Object(Some(this))) => Ok(Some(Value::Int(spl_characteristics(&*ctx, *this)))),
        _ => Ok(Some(Value::Int(SPL_LIST_DEFAULT))),
    }
}

/// `Spliterator.getComparator()` — `null` for a natural-ordered SORTED
/// spliterator, `IllegalStateException` for one that is not sorted at all.
///
/// The interface's own default throws unconditionally, which is right only
/// while nothing reports `SORTED`.
pub(crate) fn native_spliterator_get_comparator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(this))) => Some(*this),
        _ => None,
    };
    let raw = this.map(|t| spl_raw_bits(&*ctx, t)).unwrap_or(0);
    if raw & SPL_SORTED != 0 {
        let this = this.expect("raw bits came from a receiver");
        let cmp = spl_comparator_slot(&*ctx, this);
        // Wave 9 (lane spliter): the comparator the producer recorded --
        // `TreeSet(cmp)` answered `null` here before, HotSpot the comparator.
        // A natural-ordered source records none and keeps `null`.
        if raw & SPL_PRIV_ENTRY_CMP == 0 {
            return Ok(Some(cmp));
        }
        // `TreeMap.EntrySpliterator.getComparator()`:
        // `Map.Entry.comparingByKey(tree.comparator)`, or
        // `Map.Entry.comparingByKey()` for a natural-ordered map -- never null.
        // On a failure to build it, the pre-wave-9 `null` rather than a new
        // exception from a getter.
        let built = match cmp {
            Value::Object(Some(_)) => ctx.invoke(
                "java/util/Map$Entry",
                "comparingByKey",
                "(Ljava/util/Comparator;)Ljava/util/Comparator;",
                &[cmp],
            ),
            _ => ctx.invoke(
                "java/util/Map$Entry",
                "comparingByKey",
                "()Ljava/util/Comparator;",
                &[],
            ),
        };
        return Ok(Some(match built {
            Ok(Some(v @ Value::Object(Some(_)))) => v,
            _ => Value::Object(None),
        }));
    }
    Err(cratonvm_types::error::RuntimeError::IllegalStateException {
        message: String::new(),
    }
    .into())
}

pub(crate) fn native_spliterator_try_advance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Synthetic-spliterator fast path: field 0 is a backing Object[]. If it
    // isn't, this native is being invoked on a real-JDK Spliterator subclass
    // whose field-0 is something else (e.g. an Iterator inside
    // `ServiceLoaderUtil$ServiceLoaderSpliterator`). Returning Int(0) here
    // would silently terminate the caller's stream — instead, signal "no
    // synthetic state" by returning Int(0) only AFTER trying the JDK default
    // method on the receiver class. The dispatcher in `invoke_on_class_shared_inner`
    // now prefers default-method bytecode over this native when the receiver
    // is a non-synthetic class, so reaching here typically means the
    // receiver IS our synthetic shape; the array guard protects against
    // edge cases where dispatch routes the wrong way.
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) if ctx.heap_kind_of(a) == cratonvm_types::ObjectKind::Array => a,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    let len = ctx.array_length(arr);
    if cursor >= len {
        return Ok(Some(Value::Int(0)));
    }
    let elem = ctx.get_array_element(arr, cursor);
    ctx.set_field(this, 1, Value::Int((cursor + 1) as i32));
    ctx.invoke_virtual(consumer, "accept", "(Ljava/lang/Object;)V", &[elem])?;
    Ok(Some(Value::Int(1)))
}

/// `Spliterator.trySplit()` — hand the first half of the remaining elements to
/// a new spliterator and keep the second half.
///
/// This used to be a flat `null` ("cannot split"), which is legal but disables
/// parallel decomposition for EVERY synthetic spliterator in the VM. The real
/// `ArraySpliterator.trySplit` is `int lo = index, mid = (lo + fence) >>> 1;`
/// then `return (lo >= mid) ? null : new ArraySpliterator<>(array, lo, index =
/// mid, chars);` — i.e. null only when fewer than two elements remain.
///
/// The JDK shares one array between both halves and relies on the per-instance
/// `fence` (field 2). We deliberately do NOT do that: the sibling natives in
/// this file (`estimateSize`, `tryAdvance`, `forEachRemaining`) all bound
/// themselves by `array_length(field0)` and ignore field 2, so a shared array
/// with a lowered fence would let the left half run straight on into the right
/// half's elements and emit them twice. Copy the prefix into its own array
/// instead — exact same element partition, no reader has to change, and field 2
/// stays equal to the backing length as at every other construction site.
/// Whether a red-black-tree spliterator ([`SPL_PRIV_TREE_SPLIT`]) with
/// `remaining` elements splits: never below two, always from three (a root
/// with two children), and at exactly two only when its producer recorded
/// that the root is not the first element walked ([`SPL_PRIV_PAIR_SPLIT`]).
pub(crate) fn tree_split_allowed(raw: i32, remaining: usize) -> bool {
    match remaining {
        0 | 1 => false,
        2 => raw & SPL_PRIV_PAIR_SPLIT != 0,
        _ => true,
    }
}

pub(crate) fn native_spliterator_try_split(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Same guard as `tryAdvance`/`forEachRemaining`: dispatch can route a
    // real-JDK Spliterator subclass in here, whose field 0 is not an array
    // (e.g. an Iterator in `ServiceLoaderUtil$ServiceLoaderSpliterator`).
    // "Cannot split" is the safe answer for those.
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) if ctx.heap_kind_of(a) == cratonvm_types::ObjectKind::Array => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    if ctx.object_num_fields(this) < 2 {
        return Ok(Some(Value::Object(None)));
    }
    let lo = match ctx.get_field(this, 1) {
        Value::Int(v) if v > 0 => v as usize,
        _ => 0,
    };
    let fence = ctx.array_length(arr);
    if lo >= fence {
        return Ok(Some(Value::Object(None)));
    }
    let mid = (lo + fence) / 2;
    if lo >= mid {
        // Fewer than two elements remain — the JDK returns null here.
        return Ok(Some(Value::Object(None)));
    }
    // Wave 9 (lane spliter): the split policy HotSpot's spliterator class
    // decides in code (see `SPL_PRIV_NO_SPLIT` / `SPL_PRIV_TREE_SPLIT`), and a
    // left half that keeps this spliterator's characteristics and comparator
    // -- the three-field half answered the list default, so a `HashSet`
    // spliterator's prefix claimed ORDERED and a `TreeSet`'s lost SORTED.
    let facts = spliterator_facts_enabled();
    let raw = spl_raw_bits(ctx, this);
    if facts {
        if raw & SPL_PRIV_NO_SPLIT != 0 {
            return Ok(Some(Value::Object(None)));
        }
        if raw & SPL_PRIV_TREE_SPLIT != 0 && !tree_split_allowed(raw, fence - lo) {
            return Ok(Some(Value::Object(None)));
        }
    }
    let carry = facts && ctx.object_num_fields(this) > SPL_FIELD_CHARACTERISTICS;
    let prefix_len = mid - lo;
    // GC-SAFETY: both allocations below can move `this` and `arr`; pin and
    // re-read rather than holding the bare refs across them.
    let this_pin = ctx.pin_native_root(this);
    let arr_pin = ctx.pin_native_root(arr);
    let prefix = alloc_ref_array(ctx, prefix_len);
    let prefix_pin = ctx.pin_native_root(prefix);
    let arr = ctx.read_native_pin(arr_pin, arr);
    let prefix = ctx.read_native_pin(prefix_pin, prefix);
    for i in 0..prefix_len {
        let elem = ctx.get_array_element(arr, lo + i);
        ctx.set_array_element(prefix, i, elem);
    }
    let left_width = if carry { SPL_FACTS_NUM_FIELDS } else { 3 };
    // Not `?`: a refusal must release this frame's pins (gc-common w20-a).
    let left = match try_alloc_synthetic(ctx, "java/util/Spliterator", left_width) {
        Ok(l) => l,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let left_pin = ctx.pin_native_root(left);
    let prefix = ctx.read_native_pin(prefix_pin, prefix);
    let left = ctx.read_native_pin(left_pin, left);
    ctx.set_field(left, 0, Value::Object(Some(prefix)));
    ctx.set_field(left, 1, Value::Int(0));
    ctx.set_field(left, 2, Value::Int(prefix_len as i32));
    if carry {
        // The source estimate belongs to the whole; each half reports its own
        // snapshot length (only a non-splitting stage carries one anyway).
        ctx.set_field(
            left,
            SPL_FIELD_CHARACTERISTICS,
            Value::Int(raw & !SPL_PRIV_FIXED_EST),
        );
        let this = ctx.read_native_pin(this_pin, this);
        let cmp = spl_comparator_slot(&*ctx, this);
        if let Value::Object(Some(_)) = cmp {
            ctx.set_field(left, SPL_FIELD_COMPARATOR, cmp);
        }
    }
    // This spliterator now covers [mid, fence).
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, 1, Value::Int(mid as i32));
    if ctx.object_num_fields(this) > 2 {
        ctx.set_field(this, 2, Value::Int(fence as i32));
    }
    let left = ctx.read_native_pin(left_pin, left);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(left))))
}

pub(crate) fn native_spliterator_for_each_remaining(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // Synthetic-spliterator fast path: field 0 is a backing Object[]. If it
    // isn't, this native is being invoked on a real-JDK Spliterator subclass
    // (e.g. log4j's `ServiceLoaderUtil$ServiceLoaderSpliterator`, where
    // field 0 is an Iterator) — `array_length` on that would surface the
    // ARRAY-LEN-GUARD warning and the rest of the stream pipeline collapses.
    // Fall back to driving the subclass's own `tryAdvance` until exhausted.
    let arr: ObjectRef = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) if ctx.heap_kind_of(a) == cratonvm_types::ObjectKind::Array => a,
        _ => {
            // Non-synthetic Spliterator subclass — drive its own tryAdvance
            // (which now correctly dispatches to the JDK default-method
            // bytecode for real-JDK subclasses, thanks to the
            // SPLITERATOR-FALLTHROUGH change in `invoke_on_class_shared_inner`).
            // GC-SAFETY: each `tryAdvance` re-enters Java and can move `this` /
            // `consumer`; pin both and re-read from their handles each iteration.
            let this_pin = ctx.pin_native_root(this);
            let con_pin = ctx.pin_native_root(consumer);
            let mut iters: u64 = 0;
            let mut result = Ok(None);
            loop {
                iters += 1;
                if iters > 10_000_000 {
                    break;
                }
                let t = ctx.read_native_pin(this_pin, this);
                let c = ctx.read_native_pin(con_pin, consumer);
                match ctx.invoke_virtual(
                    t,
                    "tryAdvance",
                    "(Ljava/util/function/Consumer;)Z",
                    &[Value::Object(Some(c))],
                ) {
                    Ok(Some(Value::Int(1))) => continue,
                    Ok(_) => break,
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                }
            }
            ctx.unpin_native_roots(this_pin);
            return result;
        }
    };
    // GC-SAFETY: `accept` allocates and can move `this` / the backing array /
    // `consumer`; pin them and re-read each from its handle every iteration.
    let this_pin = ctx.pin_native_root(this);
    let arr_pin = ctx.pin_native_root(arr);
    let con_pin = ctx.pin_native_root(consumer);
    let mut cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    let len = ctx.array_length(arr);
    let mut result = Ok(None);
    while cursor < len {
        let a = ctx.read_native_pin(arr_pin, arr);
        let c = ctx.read_native_pin(con_pin, consumer);
        let elem = ctx.get_array_element(a, cursor);
        cursor += 1;
        if let Err(e) = ctx.invoke_virtual(c, "accept", "(Ljava/lang/Object;)V", &[elem]) {
            result = Err(e);
            break;
        }
    }
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, 1, Value::Int(cursor as i32));
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_spliterators_empty(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // gc-common w20-a: see `alloc_empty_array_holder`.
    let itr = alloc_empty_array_holder(ctx, "java/util/Spliterators$EmptySpliterator", 2)?;
    Ok(Some(Value::Object(Some(itr))))
}

pub(crate) fn native_empty_iterator(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // In the JDK, `Collections.emptyIterator()` is literally
    // `(Iterator<T>) EmptyIterator.EMPTY_ITERATOR`, so
    // `emptyIterator() == emptyIterator()` holds by identity. Code relies on
    // that: Hibernate's StatelessSession persistence-context "is-cleared" check
    // asserts `managedEntitiesIterator() == Collections.emptyIterator()`.
    // Allocating a fresh `EmptyIterator` per call broke the identity (cf.
    // `collections_empty_singleton` for EMPTY_LIST/MAP/SET). Return the real
    // `Collections$EmptyIterator.EMPTY_ITERATOR` singleton when available.
    let _ = ctx.ensure_class_initialized("java/util/Collections$EmptyIterator");
    if let Some(cid) = ctx.class_id_by_name("java/util/Collections$EmptyIterator") {
        if let Some(idx) = ctx.static_field_index_by_name(cid, "EMPTY_ITERATOR") {
            if let v @ Value::Object(Some(_)) = ctx.get_static_field(cid, idx) {
                return Ok(Some(v));
            }
            // Field present but not yet populated: lazily allocate the singleton,
            // store it in the static field (GC-rooted, so it survives
            // relocation), and return it so all callers share one instance.
            let itr = alloc_empty_array_holder(ctx, "java/util/Collections$EmptyIterator", 2)?;
            ctx.set_static_field(cid, idx, Value::Object(Some(itr)));
            return Ok(Some(Value::Object(Some(itr))));
        }
    }
    // Fallback (class/field unavailable): fresh synthetic empty iterator.
    let itr = alloc_empty_array_holder(ctx, "java/util/Collections$EmptyIterator", 2)?;
    Ok(Some(Value::Object(Some(itr))))
}

pub(crate) fn native_empty_list_iterator(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // `Collections.emptyListIterator()` must return a `Collections$EmptyListIterator`
    // (a ListIterator), NOT a plain `Collections$EmptyIterator` (an Iterator).
    // This previously shared `native_empty_iterator`, which always stamped the
    // result as `EmptyIterator` regardless of caller — real `EmptyIterator` has
    // no hasPrevious/previous/nextIndex/previousIndex/set/add, so any code
    // holding the result as a `ListIterator` (e.g. Jetty's
    // `ContextHandler.notifyExitScope` walking an empty listener list) crashed
    // the whole VM with a fatal main-thread NoSuchMethodError on `hasPrevious()Z`
    // (an uncaught linkage error on the primary thread aborts the process,
    // unlike a per-test failure). Mirrors the EMPTY_ITERATOR singleton pattern
    // above, but keyed off `EmptyListIterator`'s own static field so identity
    // (`emptyListIterator() == emptyListIterator()`) still holds.
    let _ = ctx.ensure_class_initialized("java/util/Collections$EmptyListIterator");
    if let Some(cid) = ctx.class_id_by_name("java/util/Collections$EmptyListIterator") {
        if let Some(idx) = ctx.static_field_index_by_name(cid, "EMPTY_ITERATOR") {
            if let v @ Value::Object(Some(_)) = ctx.get_static_field(cid, idx) {
                return Ok(Some(v));
            }
            let itr = alloc_empty_array_holder(ctx, "java/util/Collections$EmptyListIterator", 2)?;
            ctx.set_static_field(cid, idx, Value::Object(Some(itr)));
            return Ok(Some(Value::Object(Some(itr))));
        }
    }
    // Fallback (class/field unavailable): fresh synthetic empty list iterator.
    let itr = alloc_empty_array_holder(ctx, "java/util/Collections$EmptyListIterator", 2)?;
    Ok(Some(Value::Object(Some(itr))))
}

// ===========================================================================
// ScheduledExecutorService / ScheduledThreadPoolExecutor
// ===========================================================================

pub(crate) const STPE_FIELD_POOL_SIZE: usize = 0;
pub(crate) const STPE_FIELD_SHUTDOWN: usize = 1;
pub(crate) const STPE_FIELD_TASK_LIST: usize = 2;
pub(crate) const STPE_NUM_FIELDS: usize = 3;

pub(crate) fn register_scheduled_executor_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/concurrent/ScheduledThreadPoolExecutor";
    r.register(c, "<init>", "(I)V", native_stpe_init);
    r.register(
        c,
        "schedule",
        "(Ljava/lang/Runnable;JLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;",
        native_stpe_schedule,
    );
    r.register(
        c,
        "scheduleAtFixedRate",
        "(Ljava/lang/Runnable;JJLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;",
        native_stpe_schedule_fixed_rate,
    );
    r.register(
        c,
        "scheduleWithFixedDelay",
        "(Ljava/lang/Runnable;JJLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;",
        native_stpe_schedule_fixed_delay,
    );
    r.register(c, "shutdown", "()V", native_stpe_shutdown);
    r.register(
        c,
        "shutdownNow",
        "()Ljava/util/List;",
        native_stpe_shutdown_now,
    );
    r.register(c, "isShutdown", "()Z", native_stpe_is_shutdown);
    r.register(
        c,
        "submit",
        "(Ljava/lang/Runnable;)Ljava/util/concurrent/Future;",
        native_stpe_submit_runnable,
    );
    r.register(
        c,
        "submit",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/Future;",
        native_stpe_submit_callable,
    );

    // Also register under the interface name
    let iface = "java/util/concurrent/ScheduledExecutorService";
    r.register(iface, "<init>", "(I)V", native_stpe_init);
    r.register(
        iface,
        "schedule",
        "(Ljava/lang/Runnable;JLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;",
        native_stpe_schedule,
    );
    r.register(
        iface,
        "scheduleAtFixedRate",
        "(Ljava/lang/Runnable;JJLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;",
        native_stpe_schedule_fixed_rate,
    );
    r.register(
        iface,
        "scheduleWithFixedDelay",
        "(Ljava/lang/Runnable;JJLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;",
        native_stpe_schedule_fixed_delay,
    );
    r.register(iface, "shutdown", "()V", native_stpe_shutdown);
    r.register(
        iface,
        "shutdownNow",
        "()Ljava/util/List;",
        native_stpe_shutdown_now,
    );
    r.register(iface, "isShutdown", "()Z", native_stpe_is_shutdown);
    r.register(
        iface,
        "submit",
        "(Ljava/lang/Runnable;)Ljava/util/concurrent/Future;",
        native_stpe_submit_runnable,
    );
    r.register(
        iface,
        "submit",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/Future;",
        native_stpe_submit_callable,
    );

    // ScheduledFuture instance methods. `schedule(...)` returns an object whose
    // runtime class is the `ScheduledFuture` interface (see alloc_completed_future
    // / native_stpe_schedule). Without these natives, a caller that holds the
    // result as `Future` and calls `cancel`/`isCancelled`/`isDone`/`get` resolves
    // to the abstract `Future` declaration (no Code) and throws AbstractMethodError.
    // The canonical tripwire is JUnit Jupiter's `@Timeout` support, which schedules
    // an interrupt task and calls `future.cancel(false)` in its `finally` block —
    // that single call previously failed *every* test in a `@Timeout`-annotated
    // class (the bulk of the kafka suite). Field layout (alloc_completed_future /
    // native_stpe_schedule): 0 = result value, 1 = state (0=pending, 1=done, 2=cancelled).
    let sf = "java/util/concurrent/ScheduledFuture";
    r.register(sf, "cancel", "(Z)Z", native_sf_cancel);
    r.register(sf, "isCancelled", "()Z", native_sf_is_cancelled);
    r.register(sf, "isDone", "()Z", native_sf_is_done);
    r.register(sf, "get", "()Ljava/lang/Object;", native_sf_get);
    r.register(
        sf,
        "get",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        native_sf_get,
    );
    r.set_category(__prev_cat);
}

/// ScheduledFuture state held in field 1.
pub(crate) const SF_STATE_PENDING: i32 = 0;
pub(crate) const SF_STATE_DONE: i32 = 1;
pub(crate) const SF_STATE_CANCELLED: i32 = 2;

pub(crate) fn sf_state(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
    match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => SF_STATE_DONE,
    }
}

/// `Future.cancel(boolean)` — cancel iff the task has not already run/completed.
/// Returns whether this call performed the transition. JUnit's `@Timeout` reads
/// this: `true` => the scheduled interrupt never fired (no timeout) => test ran
/// to completion normally; `false` => the task already ran => timeout.
pub(crate) fn native_sf_cancel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    if sf_state(ctx, this) == SF_STATE_PENDING {
        ctx.set_field(this, 1, Value::Int(SF_STATE_CANCELLED));
        Ok(Some(Value::Int(1)))
    } else {
        Ok(Some(Value::Int(0)))
    }
}

pub(crate) fn native_sf_is_cancelled(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(
        (sf_state(ctx, this) == SF_STATE_CANCELLED) as i32,
    )))
}

pub(crate) fn native_sf_is_done(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(
        (sf_state(ctx, this) != SF_STATE_PENDING) as i32,
    )))
}

pub(crate) fn native_sf_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, 0)))
}

/// Allocate a completed ScheduledFuture that wraps a result value.
/// Fields: 0 = result value, 1 = done flag (always 1).
pub(crate) fn alloc_completed_future(
    ctx: &mut dyn NativeContext,
    result: Value,
) -> Result<ObjectRef, MethodCallFailed> {
    // gc-common w20-a: `result` (a `Callable`'s answer, in
    // `native_stpe_submit_callable`) is rooted across the future's allocation;
    // it was stored at its pre-allocation address.
    let result_pin = pin_value(ctx, result);
    let future = try_alloc_synthetic(ctx, "java/util/concurrent/ScheduledFuture", 2);
    let result = read_pinned_elem(ctx, result_pin, result);
    if result_pin != usize::MAX {
        ctx.unpin_native_roots(result_pin);
    }
    let future = future?;
    ctx.set_field(future, 0, result);
    ctx.set_field(future, 1, Value::Int(1)); // done = true
    Ok(future)
}

pub(crate) fn native_stpe_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let pool_size = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    // GC-safety: see `rooted_across`.
    let (this, task_arr) = rooted_across1(ctx, this, |ctx| alloc_ref_array(ctx, 16));
    ctx.set_field(this, STPE_FIELD_POOL_SIZE, Value::Int(pool_size));
    ctx.set_field(this, STPE_FIELD_SHUTDOWN, Value::Int(0));
    ctx.set_field(this, STPE_FIELD_TASK_LIST, Value::Object(Some(task_arr)));
    Ok(None)
}

pub(crate) fn native_stpe_schedule(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: this, Runnable, long delay, TimeUnit
    let runnable = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return completed_future_result(ctx, Value::Object(None)),
    };
    // The generic operand-stack pop type-erases Long -> Double bit-pattern.
    let delay = match args.get(2) {
        Some(Value::Long(v)) => *v,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    if delay > 0 {
        // Delayed task: we have no real timer thread, so do NOT fire the task.
        // Return a *pending*, cancellable future. This is exactly the contract
        // JUnit `@Timeout` relies on: the test finishes well before the delay,
        // then `future.cancel(false)` succeeds (returns true => no timeout).
        // Firing the runnable immediately would (a) interrupt the test thread
        // and (b) make cancel() return false => spurious TimeoutException.
        return alloc_pending_future(ctx).map(|f| Some(Value::Object(Some(f))));
    }
    // Zero/negative delay: run immediately (existing simplified behaviour) and
    // return a completed (non-cancellable) future.
    ctx.invoke_virtual(runnable, "run", "()V", &[])?;
    completed_future_result(ctx, Value::Object(None))
}

/// [`alloc_completed_future`] as a native's answer. The `schedule*` /
/// `submit*` natives each spelled this out twice (once for a null task, once
/// after running it); a GC point inside a `return` arm reads, to the
/// stale-handle scanner, like one on the fall-through path (gc-common w20-a).
pub(crate) fn completed_future_result(ctx: &mut dyn NativeContext, result: Value) -> MethodCallResult {
    alloc_completed_future(ctx, result).map(|f| Some(Value::Object(Some(f))))
}

/// Allocate a pending (not-yet-run, cancellable) ScheduledFuture.
/// Fields: 0 = result (null), 1 = state (0 = pending).
pub(crate) fn alloc_pending_future(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let future = try_alloc_synthetic(ctx, "java/util/concurrent/ScheduledFuture", 2)?;
    ctx.set_field(future, 0, Value::Object(None));
    ctx.set_field(future, 1, Value::Int(SF_STATE_PENDING));
    Ok(future)
}

pub(crate) fn native_stpe_schedule_fixed_rate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: this, Runnable, long initialDelay (2 slots), long period (2 slots), TimeUnit
    let runnable = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return completed_future_result(ctx, Value::Object(None)),
    };
    // Execute once immediately (simplified)
    ctx.invoke_virtual(runnable, "run", "()V", &[])?;
    completed_future_result(ctx, Value::Object(None))
}

pub(crate) fn native_stpe_schedule_fixed_delay(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: this, Runnable, long initialDelay (2 slots), long period (2 slots), TimeUnit
    let runnable = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return completed_future_result(ctx, Value::Object(None)),
    };
    // Execute once immediately (simplified)
    ctx.invoke_virtual(runnable, "run", "()V", &[])?;
    completed_future_result(ctx, Value::Object(None))
}

pub(crate) fn native_stpe_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, STPE_FIELD_SHUTDOWN, Value::Int(1));
    Ok(None)
}

pub(crate) fn native_stpe_shutdown_now(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Set shutdown flag
    if let Some(Value::Object(Some(this))) = args.first() {
        ctx.set_field(*this, STPE_FIELD_SHUTDOWN, Value::Int(1));
    }
    // Return empty list
    make_list_of(ctx, &[])
}

pub(crate) fn native_stpe_is_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let shutdown = match ctx.get_field(this, STPE_FIELD_SHUTDOWN) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(i32::from(shutdown != 0))))
}

pub(crate) fn native_stpe_submit_runnable(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let runnable = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return completed_future_result(ctx, Value::Object(None)),
    };
    ctx.invoke_virtual(runnable, "run", "()V", &[])?;
    completed_future_result(ctx, Value::Object(None))
}

pub(crate) fn native_stpe_submit_callable(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let callable = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return completed_future_result(ctx, Value::Object(None)),
    };
    let result = ctx.invoke_virtual(callable, "call", "()Ljava/lang/Object;", &[])?;
    completed_future_result(ctx, result.unwrap_or(Value::Object(None)))
}

// ===========================================================================
// ConcurrentSkipListMap (simplified as sorted array-backed map)
// ===========================================================================
//
// Bug 1 (CRIT round-9 native-misc HIGH-4): the underlying sorted array
// previously had zero synchronization. Concurrent put + remove from two
// threads could corrupt the array (insert shifts elements right while
// remove shifts them left; their writes can interleave to produce a
// duplicated or vanished slot, and binary search reads can observe
// half-updated key ordering).
//
// Fix: stripe-based RwLock guard, keyed by the map's identity hash code.
// All read operations (get/firstKey/lastKey/containsKey/keySet/size/
// isEmpty) take a read lock; all mutating operations (put/remove) take
// a write lock. Stripes are 256 to keep contention low without a global
// hashmap of locks per object. Distinct CSLM instances may collide on
// the same stripe (contention only, not correctness).
//
// TODO(round-11+): replace the array-backed implementation with a real
// lock-free Pugh-style skiplist. Until then this lock keeps the data
// structure observably safe under contention, at the cost of converting
// every operation into a serialised-per-stripe critical section. That's
// still strictly better than the previous behaviour (data corruption +
// occasional panics from out-of-bounds shifts).

pub(crate) const CSLM_FIELD_KEYS: usize = 0;
pub(crate) const CSLM_FIELD_VALUES: usize = 1;
pub(crate) const CSLM_FIELD_SIZE: usize = 2;
#[allow(dead_code)] // used for documentation; allocations go through constructors
pub(crate) const CSLM_NUM_FIELDS: usize = 3;
pub(crate) const CSLM_DEFAULT_CAPACITY: usize = 16;

pub(crate) const CSLM_LOCK_STRIPES: usize = 256;

pub(crate) fn cslm_stripe_for(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> &'static parking_lot::RwLock<()> {
    // MED fix: switched from `std::sync::RwLock` to `parking_lot::RwLock`
    // for consistency with the CHM-segment striped locks earlier in this
    // file (see `seg_locks`). parking_lot's `read()`/`write()` are
    // infallible — no `PoisonError` handling at every call site — and
    // their unpoisoning semantics match the CHM stripes which expect
    // a panicking writer to leave the lock usable for subsequent
    // segments rather than poisoning the whole stripe.
    static STRIPES: std::sync::OnceLock<Vec<parking_lot::RwLock<()>>> = std::sync::OnceLock::new();
    let stripes = STRIPES.get_or_init(|| {
        (0..CSLM_LOCK_STRIPES)
            .map(|_| parking_lot::RwLock::new(()))
            .collect()
    });
    let key = ctx.identity_hash_code(this) as u32;
    // Mix bits so sequentially-allocated objects spread across stripes.
    let idx = ((key ^ (key >> 16)).wrapping_mul(0x9E37_79B1) as usize) % CSLM_LOCK_STRIPES;
    &stripes[idx]
}

/// Per-CSLM custom `Comparator` side-table (fix item 5).
///
/// The synthetic `ConcurrentSkipListMap` class is declared with exactly THREE
/// instance fields (`class_manager.rs`: keys/values/size), so there is no
/// object slot to hold a `Comparator`. Rather than widen the synthetic layout
/// (a cross-crate change), the comparator is kept here keyed by the map's
/// GC-stable `widened_obj_key`, exactly as TreeMap/TreeSet keep their
/// comparators out-of-object. The stored `ObjectRef` is rooted and remapped
/// through `for_each_overlay_ref` so a moving GC keeps it live, and pruned by
/// `gc_prune_dead_collection_overlays` when the map dies.
pub(crate) fn cslm_comparator_table() -> &'static Mutex<OverlayRows<ObjectRef>> {
    static T: std::sync::OnceLock<Mutex<OverlayRows<ObjectRef>>> = std::sync::OnceLock::new();
    T.get_or_init(|| Mutex::new(OverlayRows::new()))
}

/// Read this CSLM's custom comparator (or `Value::Object(None)` for natural
/// ordering).
pub(crate) fn cslm_comparator(ctx: &dyn NativeContext, this: ObjectRef) -> Value {
    let key = widened_obj_key(ctx, this);
    match cslm_comparator_table().lock().unwrap().get(&key).copied() {
        Some(cmp) => Value::Object(Some(cmp)),
        None => Value::Object(None),
    }
}

/// Store this CSLM's custom comparator. A `None` clears any prior entry.
pub(crate) fn cslm_set_comparator(ctx: &dyn NativeContext, this: ObjectRef, cmp: Option<ObjectRef>) {
    let key = widened_obj_key(ctx, this);
    let mut t = cslm_comparator_table().lock().unwrap();
    match cmp {
        Some(c) => {
            t.insert(key, c);
        }
        None => {
            t.remove(&key);
        }
    }
}

pub(crate) fn register_concurrent_skip_list_map_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let c = "java/util/concurrent/ConcurrentSkipListMap";
    r.register(c, "<init>", "()V", native_cslm_init);
    // Fix item 5: the `(Comparator)` constructor. Without this, code that did
    // `new ConcurrentSkipListMap(cmp)` over keys ordered ONLY by `cmp` (incl.
    // non-`Comparable` keys) hit the no-arg `<init>` (or none), so every put
    // fell back to natural ordering and silently dropped/misordered entries.
    r.register(
        c,
        "<init>",
        "(Ljava/util/Comparator;)V",
        native_cslm_init_comparator,
    );
    r.register(
        c,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_cslm_put,
    );
    r.register(
        c,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_cslm_get,
    );
    r.register(
        c,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_cslm_remove,
    );
    r.register(c, "size", "()I", native_cslm_size);
    r.register(c, "isEmpty", "()Z", native_cslm_is_empty);
    r.register(
        c,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_cslm_contains_key,
    );
    r.register(c, "firstKey", "()Ljava/lang/Object;", native_cslm_first_key);
    r.register(c, "lastKey", "()Ljava/lang/Object;", native_cslm_last_key);
    r.register(c, "keySet", "()Ljava/util/Set;", native_cslm_key_set);
    r.set_category(__prev_cat);
}

/// Binary search in the keys array using `comparator` (a custom `Comparator`)
/// or natural ordering when `comparator` is `Value::Object(None)` (fix item 5).
/// Returns Ok(index) if found, Err(insert_pos) if not found.
///
/// Previously this hardcoded `Value::Object(None)`, so a CSLM constructed with
/// a custom comparator (or whose keys are ordered only by that comparator)
/// silently used natural ordering — misordering keys and, for keys the natural
/// comparison treated as equal, dropping puts. With the comparator threaded
/// through, custom-ordered maps behave correctly. The natural-ordering path
/// remains fail-loud on a non-`Comparable` key: `tree_compare` →
/// `natural_compare` → `compare_via_compare_to` dispatches the key's real
/// `compareTo`, which raises (NoSuchMethodError / AbstractMethodError /
/// ClassCastException) and propagates here via `?` instead of silently
/// collapsing the ordering and dropping the entry.
///
/// Family-1 stale-`ObjectRef` fix (2026-08-01): this was the last binary
/// search in the file still holding raw `ObjectRef`s across `tree_compare`.
/// `tm_binary_search`, `ts_binary_search` and `pbq_offer_locked` all pin and
/// re-read; CSLM never did. `tree_compare` dispatches either a user
/// `Comparator.compare` or the key's own `Comparable.compareTo` — a full
/// re-entry into the interpreter that can allocate and complete a moving young
/// collection. Everything the search or its CALLERS touch afterwards is at
/// risk, so all of it is pinned here and written back through `&mut`:
///
/// * `keys`  — re-read by `get_array_element` on the next iteration, and by the
///   caller's shift/insert. A stale array ref reads `Object(None)` (or, on the
///   real heap, whatever now occupies the address) and silently drops writes.
/// * `values` — never touched by the search itself, but every caller stores
///   into it immediately afterwards, so it must ride the same pins. `None` for
///   `containsKey`, which has no value array to keep.
/// * `owner` — the caller writes `CSLM_FIELD_SIZE` through it after the search.
/// * `comparator` and `key` — dereferenced again on the *next* iteration.
///
/// Parameters are `&mut` rather than returned in a tuple deliberately: a caller
/// physically cannot keep using a pre-search copy, because there isn't one.
pub(crate) fn cslm_binary_search(
    ctx: &mut dyn NativeContext,
    owner: &mut ObjectRef,
    keys: &mut ObjectRef,
    values: &mut Option<ObjectRef>,
    size: i32,
    comparator: &mut Value,
    key: &mut Value,
) -> Result<Result<usize, usize>, MethodCallFailed> {
    // `owner_pin` is taken FIRST so `unpin_native_roots(owner_pin)` releases
    // this whole batch (the API unwinds from the base handle onward).
    let owner_pin = ctx.pin_native_root(*owner);
    let keys_pin = ctx.pin_native_root(*keys);
    let values_pin = opt_pin(ctx, *values);
    let comparator_pin = pin_value(ctx, *comparator);
    let key_pin = pin_value(ctx, *key);
    let mut low: usize = 0;
    let mut high = size as usize;
    while low < high {
        let mid = low + (high - low) / 2;
        let mid_key = ctx.get_array_element(*keys, mid);
        // Orientation is compare(existing, searched) — the inverse of
        // `tm_binary_search` — so the branch signs below differ from TreeMap's.
        // Preserved exactly: a non-antisymmetric comparator distinguishes them.
        let cmp = match tree_compare(ctx, comparator, mid_key, *key) {
            Ok(c) => c,
            Err(e) => {
                ctx.unpin_native_roots(owner_pin);
                return Err(e);
            }
        };
        // The comparison ran real bytecode — refresh everything reused below
        // and everything the caller reuses after we return.
        *owner = ctx.read_native_pin(owner_pin, *owner);
        *keys = ctx.read_native_pin(keys_pin, *keys);
        *values = opt_read(ctx, values_pin, *values);
        *comparator = read_pinned_elem(ctx, comparator_pin, *comparator);
        *key = read_pinned_elem(ctx, key_pin, *key);
        if cmp < 0 {
            low = mid + 1;
        } else if cmp > 0 {
            high = mid;
        } else {
            ctx.unpin_native_roots(owner_pin);
            return Ok(Ok(mid));
        }
    }
    ctx.unpin_native_roots(owner_pin);
    Ok(Err(low))
}

/// Read a CSLM's backing key/value arrays, creating them if the receiver was
/// built without running our `<init>` (a deserialized or subclassed map).
///
/// Returns the (possibly relocated) receiver alongside: the lazy
/// `alloc_ref_array` calls below are Java-heap allocations and can move `this`,
/// so a caller that kept its own copy would write `CSLM_FIELD_SIZE` into a
/// dead address. Callers MUST use the returned receiver.
pub(crate) fn cslm_arrays(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> (ObjectRef, ObjectRef, ObjectRef, i32) {
    let (keys_opt, values_opt, size) = cslm_state(ctx, this);
    let mut this = this;
    let mut keys = match keys_opt {
        Some(k) => k,
        None => {
            let (moved, k) =
                rooted_across1(ctx, this, |ctx| alloc_ref_array(ctx, CSLM_DEFAULT_CAPACITY));
            this = moved;
            ctx.set_field(this, CSLM_FIELD_KEYS, Value::Object(Some(k)));
            k
        }
    };
    let values = match values_opt {
        Some(v) => v,
        None => {
            let v = rooted_across(ctx, &mut [&mut this, &mut keys], |ctx| {
                alloc_ref_array(ctx, CSLM_DEFAULT_CAPACITY)
            });
            ctx.set_field(this, CSLM_FIELD_VALUES, Value::Object(Some(v)));
            v
        }
    };
    (this, keys, values, size)
}

/// Grow the key/value arrays if `size + 1` no longer fits, IN PLACE through
/// `&mut`.
///
/// All three references are `&mut` and none is returned, which is the whole
/// contract: two allocations run here, `rooted_across` refreshes what it is
/// given, and a by-value parameter would have handed that refresh to a copy the
/// caller never sees. The old signature took all three by value and returned
/// `(new_keys, new_values)` — so the arrays were fine (the caller rebound them)
/// and `this` was not: `native_cslm_put` had to pin and re-read it by hand
/// around the call, and any future caller that forgot would have read a pre-GC
/// map header.
pub(crate) fn cslm_ensure_capacity(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
    size: i32,
    keys: &mut ObjectRef,
    values: &mut ObjectRef,
) {
    let arr_len = ctx.array_length(*keys);
    let needed = (size + 1) as usize;
    if needed <= arr_len {
        return;
    }
    let new_cap = (arr_len * 2).max(needed).max(CSLM_DEFAULT_CAPACITY);
    // GC-safety: two allocations in a row with `this` and both source arrays
    // live across them (and `new_keys` live across the second). See
    // `rooted_across`.
    let mut new_keys: ObjectRef = rooted_across(ctx, &mut [this, keys, values], |ctx| {
        alloc_ref_array(ctx, new_cap)
    });
    let new_values: ObjectRef =
        rooted_across(ctx, &mut [this, keys, values, &mut new_keys], |ctx| {
            alloc_ref_array(ctx, new_cap)
        });
    for i in 0..(size as usize) {
        ctx.set_array_element(new_keys, i, ctx.get_array_element(*keys, i));
        ctx.set_array_element(new_values, i, ctx.get_array_element(*values, i));
    }
    ctx.set_field(*this, CSLM_FIELD_KEYS, Value::Object(Some(new_keys)));
    ctx.set_field(*this, CSLM_FIELD_VALUES, Value::Object(Some(new_values)));
    *keys = new_keys;
    *values = new_values;
}

pub(crate) fn cslm_state(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> (Option<ObjectRef>, Option<ObjectRef>, i32) {
    let keys = match ctx.get_field(this, CSLM_FIELD_KEYS) {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let values = match ctx.get_field(this, CSLM_FIELD_VALUES) {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let size = match ctx.get_field(this, CSLM_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    (keys, values, size)
}

// ---------------------------------------------------------------------------
// CSLM test hooks
// ---------------------------------------------------------------------------
//
// `register_concurrent_skip_list_map_natives` is deliberately NOT called (see
// the `let _ = register_concurrent_skip_list_map_natives;` no-op in
// `register_collections_natives`, and the `concurrent_skip_list_map_not_
// intercepted` unit test that guards it): the real
// `java.util.concurrent.ConcurrentSkipListMap` bytecode runs instead. The
// implementation below is kept "for reference" so it can be re-enabled.
//
// That is exactly why these hooks exist. The GC-safety fixes applied to this
// family on 2026-08-01 are otherwise untestable — the natives cannot be reached
// through the registry — and an untested fix in code someone may re-enable is
// how the defect comes back. The hooks call the natives directly, so the
// registry stays clean and the pin discipline is still covered.

#[doc(hidden)]
pub fn __test_cslm_init_comparator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_cslm_init_comparator(ctx, args)
}

#[doc(hidden)]
pub fn __test_cslm_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_cslm_put(ctx, args)
}

#[doc(hidden)]
pub fn __test_cslm_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_cslm_get(ctx, args)
}

#[doc(hidden)]
pub fn __test_cslm_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_cslm_size(ctx, args)
}

pub(crate) fn native_cslm_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // GC-safety: see `rooted_across`.
    let (mut this, mut keys) =
        rooted_across1(ctx, this, |ctx| alloc_ref_array(ctx, CSLM_DEFAULT_CAPACITY));
    let values = rooted_across(ctx, &mut [&mut this, &mut keys], |ctx| {
        alloc_ref_array(ctx, CSLM_DEFAULT_CAPACITY)
    });
    ctx.set_field(this, CSLM_FIELD_KEYS, Value::Object(Some(keys)));
    ctx.set_field(this, CSLM_FIELD_VALUES, Value::Object(Some(values)));
    ctx.set_field(this, CSLM_FIELD_SIZE, Value::Int(0));
    // No custom comparator: natural ordering. Clear any stale side-table entry
    // (an identity hash could be recycled across a freed map).
    cslm_set_comparator(ctx, this, None);
    Ok(None)
}

/// `ConcurrentSkipListMap(Comparator)` constructor (fix item 5). Stores the
/// supplied comparator in the side-table so all ordering operations honor it.
pub(crate) fn native_cslm_init_comparator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // A null Comparator argument means "use natural ordering" (matches the JDK,
    // which stores null and falls back to Comparable). A non-null one is kept.
    //
    // gc-common w20-a: read BEFORE the allocations and rooted across them with
    // everything else. The comment here used to say it was read after them
    // "so it is rooted" — the argument slot is, but the copy read out of it
    // after a moving collection is not refreshed by anything.
    let cmp = match args.get(1) {
        Some(Value::Object(Some(c))) => Some(*c),
        _ => None,
    };
    let mut cmp_ref = cmp.unwrap_or(this);
    // GC-safety: see `rooted_across`.
    let mut this = this;
    let mut keys = rooted_across(ctx, &mut [&mut this, &mut cmp_ref], |ctx| {
        alloc_ref_array(ctx, CSLM_DEFAULT_CAPACITY)
    });
    let values = rooted_across(ctx, &mut [&mut this, &mut keys, &mut cmp_ref], |ctx| {
        alloc_ref_array(ctx, CSLM_DEFAULT_CAPACITY)
    });
    let cmp = cmp.map(|_| cmp_ref);
    ctx.set_field(this, CSLM_FIELD_KEYS, Value::Object(Some(keys)));
    ctx.set_field(this, CSLM_FIELD_VALUES, Value::Object(Some(values)));
    ctx.set_field(this, CSLM_FIELD_SIZE, Value::Int(0));
    cslm_set_comparator(ctx, this, cmp);
    Ok(None)
}

pub(crate) fn native_cslm_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mut key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    // Fix item 5: honor the map's custom comparator (natural ordering if none).
    let mut comparator = cslm_comparator(ctx, this);
    // Bug 1: serialise mutating ops on this map's lock stripe.
    let _guard = cslm_stripe_for(ctx, this).write();
    let (mut this, mut keys, values_arr, size) = cslm_arrays(ctx, this);
    let mut values_opt = Some(values_arr);
    // Family-1 stale-ObjectRef fix: the search dispatches the comparator and
    // refreshes everything it is handed. The inserted VALUE is not part of the
    // search, yet it is stored into the backing array afterwards, so it needs
    // its own pin. Taken BEFORE the search's own pins, so the search's
    // `unpin_native_roots(owner_pin)` — which truncates from its own base
    // onward — cannot release it.
    let value_pin = pin_value(ctx, value);
    let search = cslm_binary_search(
        ctx,
        &mut this,
        &mut keys,
        &mut values_opt,
        size,
        &mut comparator,
        &mut key,
    );
    let value = read_pinned_elem(ctx, value_pin, value);
    if value_pin != usize::MAX {
        ctx.unpin_native_roots(value_pin);
    }
    let values_arr = values_opt.unwrap_or(keys);
    match search? {
        Ok(idx) => {
            // Key exists — replace value, return old
            let old = ctx.get_array_element(values_arr, idx);
            ctx.set_array_element(values_arr, idx, value);
            Ok(Some(old))
        }
        Err(pos) => {
            // Insert at pos, shifting elements right.
            //
            // GC-safety: `cslm_ensure_capacity` performs TWO Java-heap
            // allocations. It roots the arrays it returns, but not the
            // caller's `this`/`key`/`value` — and all three are dereferenced
            // below (the size write, and the two inserting stores). Without
            // these pins a growth-triggered young collection made the `size`
            // write land in a dead address (the map silently kept its old
            // count) and stored dangling key/value refs into the array.
            // `this` no longer needs a pin of its own here —
            // `cslm_ensure_capacity` takes it (and both arrays) by `&mut` and
            // refreshes them in place. `key`/`value` are `Value`s, not part of
            // that contract, and still do.
            let key_pin = pin_value(ctx, key);
            let value_pin = pin_value(ctx, value);
            let mut values_arr = values_arr;
            cslm_ensure_capacity(ctx, &mut this, size, &mut keys, &mut values_arr);
            let key = read_pinned_elem(ctx, key_pin, key);
            let value = read_pinned_elem(ctx, value_pin, value);
            let s = size as usize;
            for i in (pos..s).rev() {
                let k = ctx.get_array_element(keys, i);
                let v = ctx.get_array_element(values_arr, i);
                ctx.set_array_element(keys, i + 1, k);
                ctx.set_array_element(values_arr, i + 1, v);
            }
            ctx.set_array_element(keys, pos, key);
            ctx.set_array_element(values_arr, pos, value);
            ctx.set_field(this, CSLM_FIELD_SIZE, Value::Int(size + 1));
            Ok(Some(Value::Object(None)))
        }
    }
}

pub(crate) fn native_cslm_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mut key = args.get(1).copied().unwrap_or(Value::Object(None));
    // Fix item 5: honor the map's custom comparator.
    let mut comparator = cslm_comparator(ctx, this);
    // Bug 1: shared read lock — concurrent reads OK, blocks during writes.
    let _guard = cslm_stripe_for(ctx, this).read();
    let (keys_opt, values_opt, size) = cslm_state(ctx, this);
    let mut keys = match keys_opt {
        Some(k) => k,
        None => return Ok(Some(Value::Object(None))),
    };
    if values_opt.is_none() {
        return Ok(Some(Value::Object(None)));
    }
    // Family-1 stale-ObjectRef fix: even a pure READ dispatches the comparator,
    // so `values_opt` — read AFTER the search — must ride the search's pins.
    let mut this = this;
    let mut values_opt = values_opt;
    let found = cslm_binary_search(
        ctx,
        &mut this,
        &mut keys,
        &mut values_opt,
        size,
        &mut comparator,
        &mut key,
    )?;
    match (found, values_opt) {
        (Ok(idx), Some(values_arr)) => Ok(Some(ctx.get_array_element(values_arr, idx))),
        _ => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_cslm_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mut key = args.get(1).copied().unwrap_or(Value::Object(None));
    // Fix item 5: honor the map's custom comparator.
    let mut comparator = cslm_comparator(ctx, this);
    // Bug 1: serialise mutating ops on this map's lock stripe.
    let _guard = cslm_stripe_for(ctx, this).write();
    let (keys_opt, values_opt, size) = cslm_state(ctx, this);
    let mut keys = match keys_opt {
        Some(k) => k,
        None => return Ok(Some(Value::Object(None))),
    };
    if values_opt.is_none() {
        return Ok(Some(Value::Object(None)));
    }
    // Family-1 stale-ObjectRef fix: the comparator dispatch inside the search
    // can move `this`, both arrays and the searched key; all four are used by
    // the compaction loop and the size write below.
    let mut this = this;
    let mut values_opt = values_opt;
    let found = cslm_binary_search(
        ctx,
        &mut this,
        &mut keys,
        &mut values_opt,
        size,
        &mut comparator,
        &mut key,
    )?;
    let Some(values_arr) = values_opt else {
        return Ok(Some(Value::Object(None)));
    };
    match found {
        Ok(idx) => {
            let old_val = ctx.get_array_element(values_arr, idx);
            let s = size as usize;
            for i in idx..(s - 1) {
                let k = ctx.get_array_element(keys, i + 1);
                let v = ctx.get_array_element(values_arr, i + 1);
                ctx.set_array_element(keys, i, k);
                ctx.set_array_element(values_arr, i, v);
            }
            ctx.set_array_element(keys, s - 1, Value::Object(None));
            ctx.set_array_element(values_arr, s - 1, Value::Object(None));
            ctx.set_field(this, CSLM_FIELD_SIZE, Value::Int(size - 1));
            Ok(Some(old_val))
        }
        Err(_) => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_cslm_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Bug 1: read lock so size cannot tear vs an in-flight put/remove.
    let _guard = cslm_stripe_for(ctx, this).read();
    let size = match ctx.get_field(this, CSLM_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(size)))
}

pub(crate) fn native_cslm_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    // Bug 1: read lock for consistent size observation.
    let _guard = cslm_stripe_for(ctx, this).read();
    let size = match ctx.get_field(this, CSLM_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(i32::from(size == 0))))
}

pub(crate) fn native_cslm_contains_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let mut key = args.get(1).copied().unwrap_or(Value::Object(None));
    // Fix item 5: honor the map's custom comparator.
    let mut comparator = cslm_comparator(ctx, this);
    // Bug 1: shared read lock.
    let _guard = cslm_stripe_for(ctx, this).read();
    let (keys_opt, _, size) = cslm_state(ctx, this);
    let mut keys = match keys_opt {
        Some(k) => k,
        None => return Ok(Some(Value::Int(0))),
    };
    // `containsKey` never touches the value array, so it hands the search a
    // `None` slot — nothing to keep rooted on its behalf.
    let mut this = this;
    let mut values_opt: Option<ObjectRef> = None;
    let found = cslm_binary_search(
        ctx,
        &mut this,
        &mut keys,
        &mut values_opt,
        size,
        &mut comparator,
        &mut key,
    )?
    .is_ok();
    Ok(Some(Value::Int(i32::from(found))))
}

pub(crate) fn native_cslm_first_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "ConcurrentSkipListMap is empty".to_string(),
                }
                .into(),
            )
        }
    };
    // Bug 1: shared read lock.
    let _guard = cslm_stripe_for(ctx, this).read();
    let (keys_opt, _, size) = cslm_state(ctx, this);
    // Treat a None keys array as empty rather than panicking on `unwrap`.
    let keys = match keys_opt {
        Some(k) if size != 0 => k,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "ConcurrentSkipListMap is empty".to_string(),
                }
                .into(),
            );
        }
    };
    Ok(Some(ctx.get_array_element(keys, 0)))
}

pub(crate) fn native_cslm_last_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "ConcurrentSkipListMap is empty".to_string(),
                }
                .into(),
            )
        }
    };
    // Bug 1: shared read lock.
    let _guard = cslm_stripe_for(ctx, this).read();
    let (keys_opt, _, size) = cslm_state(ctx, this);
    // Treat a None keys array as empty rather than panicking on `unwrap`.
    let keys = match keys_opt {
        Some(k) if size != 0 => k,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "ConcurrentSkipListMap is empty".to_string(),
                }
                .into(),
            );
        }
    };
    Ok(Some(ctx.get_array_element(keys, (size - 1) as usize)))
}

pub(crate) fn native_cslm_key_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Fix item 5: carry the map's custom comparator onto the key-set view so
    // its ordering matches the map (the keys array is already sorted by it).
    let comparator = cslm_comparator(ctx, this);
    // Bug 1: shared read lock — snapshot the keys array under the lock.
    let _guard = cslm_stripe_for(ctx, this).read();
    let (keys_opt, _, size) = cslm_state(ctx, this);
    // gc-common w20-a: the keys array and the comparator are pinned across
    // the two allocations below, and the set rooted across the second; all
    // three were used through their pre-allocation addresses.
    let (base, pins) = pin_value_slice(ctx, &[Value::Object(keys_opt), comparator]);
    // Return a TreeSet (ordered by the same comparator) containing all keys.
    let ts = try_alloc_declared_width(ctx, "java/util/TreeSet", TS_NUM_FIELDS);
    let ts = match ts {
        Ok(ts) => ts,
        Err(e) => {
            if base != usize::MAX {
                ctx.unpin_native_roots(base);
            }
            return Err(e);
        }
    };
    let ts_pin = ctx.pin_native_root(ts);
    let buf: ObjectRef = alloc_ref_array(ctx, std::cmp::max(size as usize, TS_DEFAULT_CAPACITY));
    let ts = ctx.read_native_pin(ts_pin, ts);
    ctx.unpin_native_roots(ts_pin);
    let keys_opt = match read_pinned_elem(ctx, pins[0], Value::Object(keys_opt)) {
        Value::Object(k) => k,
        _ => None,
    };
    let comparator = read_pinned_elem(ctx, pins[1], comparator);
    if base != usize::MAX {
        ctx.unpin_native_roots(base);
    }
    if let Some(keys) = keys_opt {
        for i in 0..(size as usize) {
            let k = ctx.get_array_element(keys, i);
            ctx.set_array_element(buf, i, k);
        }
    }
    ts_set_slot(ctx, ts, TS_FIELD_DATA, Value::Object(Some(buf)));
    ts_set_slot(ctx, ts, TS_FIELD_SIZE, Value::Int(size));
    ts_set_slot(ctx, ts, TS_FIELD_COMPARATOR, comparator);
    Ok(Some(Value::Object(Some(ts))))
}

// ===========================================================================
// StampedLock
// ===========================================================================

pub(crate) const SL_FIELD_STATE: usize = 0; // 0=free, 1=write-locked, >=2 means (state-1) readers
pub(crate) const SL_FIELD_STAMP: usize = 1; // monotonic stamp counter
pub(crate) const SL_SPIN_LIMIT: usize = 1000;
#[allow(dead_code)]
pub(crate) const SL_NUM_FIELDS: usize = 2;

// NO REGISTRAR — THESE NATIVES ARE UNREACHABLE AND MUST STAY THAT WAY.
//
// `register_stamped_lock_natives` used to live here and was deleted on
// 2026-08-07 (W7-15). It had not been *called* since 2026-07-28: its only
// reference was a `let _ = register_stamped_lock_natives;` dead-code silencer
// in `register_collections_natives`, which is where the full reasoning still
// sits. Summary: this implementation keeps `state` in slot 0 BY INDEX (a
// `long` on the real class, so every read-back collapses to 0) and
// `writeLock()` gives up after 1000 yields and returns stamp 0 while the
// caller believes it holds the lock — measured at 335 threads inside the
// critical section against HotSpot's 1.
//
// The live surface is
// `native-builtins/src/util_concurrent_ext.rs::register_stamped_lock_natives`
// (25 `StampedLock` triples + 3 `WriteLockView` + 3 `ReadLockView`), a strict
// superset of the eleven this file used to claim. The bodies below are kept
// only because the standing rule is to gate rather than delete; nothing calls
// them, and re-registering them would restore both defects above.

pub(crate) fn native_sl_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, SL_FIELD_STATE, Value::Int(0));
    ctx.set_field(this, SL_FIELD_STAMP, Value::Int(1));
    Ok(None)
}

pub(crate) fn sl_next_stamp(ctx: &mut dyn NativeContext, this: ObjectRef) -> i64 {
    let stamp = match ctx.get_field(this, SL_FIELD_STAMP) {
        Value::Int(v) => v as i64,
        Value::Long(v) => v,
        _ => 1,
    };
    let next = stamp + 1;
    ctx.set_field(this, SL_FIELD_STAMP, Value::Long(next));
    stamp
}

pub(crate) fn native_sl_read_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    // Acquire monitor and spin-wait if write-locked
    ctx.monitor_enter(this);
    let mut state = match ctx.get_field(this, SL_FIELD_STATE) {
        Value::Int(v) => v,
        _ => 0,
    };
    // Wait for write lock to be released (bounded spin)
    let mut spins = 0;
    while state == 1 && spins < SL_SPIN_LIMIT {
        ctx.monitor_exit(this);
        std::thread::yield_now();
        ctx.monitor_enter(this);
        state = match ctx.get_field(this, SL_FIELD_STATE) {
            Value::Int(v) => v,
            _ => 0,
        };
        spins += 1;
    }
    if state == 1 {
        ctx.monitor_exit(this);
        return Ok(Some(Value::Long(0)));
    }
    // state >= 2 means readers present; state == 0 means free
    let new_state = if state == 0 { 2 } else { state + 1 };
    ctx.set_field(this, SL_FIELD_STATE, Value::Int(new_state));
    let stamp = sl_next_stamp(ctx, this);
    ctx.monitor_exit(this);
    Ok(Some(Value::Long(stamp)))
}

pub(crate) fn native_sl_unlock_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.monitor_enter(this);
    let state = match ctx.get_field(this, SL_FIELD_STATE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if state >= 2 {
        let new_state = if state == 2 { 0 } else { state - 1 };
        ctx.set_field(this, SL_FIELD_STATE, Value::Int(new_state));
        if new_state == 0 {
            let _ = ctx.monitor_notify_all(this);
        }
    }
    ctx.monitor_exit(this);
    Ok(None)
}

pub(crate) fn native_sl_write_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    // Acquire monitor and spin-wait if readers or writers present
    ctx.monitor_enter(this);
    let mut state = match ctx.get_field(this, SL_FIELD_STATE) {
        Value::Int(v) => v,
        _ => 0,
    };
    let mut spins = 0;
    while state != 0 && spins < SL_SPIN_LIMIT {
        ctx.monitor_exit(this);
        std::thread::yield_now();
        ctx.monitor_enter(this);
        state = match ctx.get_field(this, SL_FIELD_STATE) {
            Value::Int(v) => v,
            _ => 0,
        };
        spins += 1;
    }
    if state != 0 {
        ctx.monitor_exit(this);
        return Ok(Some(Value::Long(0)));
    }
    ctx.set_field(this, SL_FIELD_STATE, Value::Int(1));
    let stamp = sl_next_stamp(ctx, this);
    ctx.monitor_exit(this);
    Ok(Some(Value::Long(stamp)))
}

pub(crate) fn native_sl_unlock_write(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.monitor_enter(this);
    ctx.set_field(this, SL_FIELD_STATE, Value::Int(0));
    sl_next_stamp(ctx, this);
    let _ = ctx.monitor_notify_all(this);
    ctx.monitor_exit(this);
    Ok(None)
}

pub(crate) fn native_sl_try_optimistic_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let state = match ctx.get_field(this, SL_FIELD_STATE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if state == 1 {
        // Write-locked, return 0 (failure)
        Ok(Some(Value::Long(0)))
    } else {
        let stamp = match ctx.get_field(this, SL_FIELD_STAMP) {
            Value::Int(v) => v as i64,
            Value::Long(v) => v,
            _ => 1,
        };
        Ok(Some(Value::Long(stamp)))
    }
}

pub(crate) fn native_sl_validate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let expected_stamp = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => return Ok(Some(Value::Int(0))),
    };
    if expected_stamp == 0 {
        return Ok(Some(Value::Int(0)));
    }
    let current_stamp = match ctx.get_field(this, SL_FIELD_STAMP) {
        Value::Int(v) => v as i64,
        Value::Long(v) => v,
        _ => 0,
    };
    let state = match ctx.get_field(this, SL_FIELD_STATE) {
        Value::Int(v) => v,
        _ => 0,
    };
    // Valid if stamp hasn't changed and not write-locked
    let valid = expected_stamp == current_stamp && state != 1;
    Ok(Some(Value::Int(i32::from(valid))))
}

pub(crate) fn native_sl_try_read_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let state = match ctx.get_field(this, SL_FIELD_STATE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if state == 1 {
        // Write-locked, can't acquire read lock
        Ok(Some(Value::Long(0)))
    } else {
        let new_state = if state == 0 { 2 } else { state + 1 };
        ctx.set_field(this, SL_FIELD_STATE, Value::Int(new_state));
        let stamp = sl_next_stamp(ctx, this);
        Ok(Some(Value::Long(stamp)))
    }
}

pub(crate) fn native_sl_try_write_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let state = match ctx.get_field(this, SL_FIELD_STATE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if state != 0 {
        // Not free — someone is holding a lock
        Ok(Some(Value::Long(0)))
    } else {
        ctx.set_field(this, SL_FIELD_STATE, Value::Int(1));
        let stamp = sl_next_stamp(ctx, this);
        Ok(Some(Value::Long(stamp)))
    }
}

// ===========================================================================
// Phaser
// ===========================================================================

pub(crate) const PH_FIELD_PARTIES: usize = 0;
pub(crate) const PH_FIELD_ARRIVED: usize = 1;
pub(crate) const PH_FIELD_PHASE: usize = 2;
#[allow(dead_code)]
pub(crate) const PH_NUM_FIELDS: usize = 3;

// NO REGISTRAR — THESE NATIVES ARE UNREACHABLE AND MUST STAY THAT WAY.
//
// `register_phaser_natives` used to live here and was deleted on 2026-08-07
// (W7-15); its only reference was a `let _ = register_phaser_natives;`
// dead-code silencer in `register_collections_natives`, where the full
// reasoning still sits. Summary: this copy keeps parties/arrived/phase in
// object slots 0/1/2, which is neither the real `Phaser` layout
// (state:J/parent/root/evenQ/oddQ) nor the `int[3]` holder that
// `native-builtins/src/phases_early.rs::register_phaser_natives` uses — and
// that holder-based registrar covers all NINE triples this one claimed plus
// `getUnarrivedParties`, `isTerminated` and `forceTermination`. Mixing the two
// corrupts the phaser: `ph_holder` overwrites slot 1 with its array, wiping
// the `arrived` count these bodies keep there.
//
// The bodies below are kept only because the standing rule is to gate rather
// than delete; nothing calls them, and re-registering them re-splits the
// surface.

pub(crate) fn native_ph_init_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, PH_FIELD_PARTIES, Value::Int(0));
    ctx.set_field(this, PH_FIELD_ARRIVED, Value::Int(0));
    ctx.set_field(this, PH_FIELD_PHASE, Value::Int(0));
    Ok(None)
}

pub(crate) fn native_ph_init_parties(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let parties = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    ctx.set_field(this, PH_FIELD_PARTIES, Value::Int(parties));
    ctx.set_field(this, PH_FIELD_ARRIVED, Value::Int(0));
    ctx.set_field(this, PH_FIELD_PHASE, Value::Int(0));
    Ok(None)
}

pub(crate) fn native_ph_register(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let parties = match ctx.get_field(this, PH_FIELD_PARTIES) {
        Value::Int(v) => v,
        _ => 0,
    };
    ctx.set_field(this, PH_FIELD_PARTIES, Value::Int(parties + 1));
    let phase = match ctx.get_field(this, PH_FIELD_PHASE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(phase)))
}

/// Helper: check if all parties have arrived and advance phase if so.
pub(crate) fn ph_maybe_advance(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
    let arrived = match ctx.get_field(this, PH_FIELD_ARRIVED) {
        Value::Int(v) => v,
        _ => 0,
    };
    let parties = match ctx.get_field(this, PH_FIELD_PARTIES) {
        Value::Int(v) => v,
        _ => 0,
    };
    let phase = match ctx.get_field(this, PH_FIELD_PHASE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if parties > 0 && arrived >= parties {
        // Advance phase, reset arrived
        ctx.set_field(this, PH_FIELD_ARRIVED, Value::Int(0));
        ctx.set_field(this, PH_FIELD_PHASE, Value::Int(phase + 1));
        phase + 1
    } else {
        phase
    }
}

pub(crate) fn native_ph_arrive_and_await(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let arrived = match ctx.get_field(this, PH_FIELD_ARRIVED) {
        Value::Int(v) => v,
        _ => 0,
    };
    ctx.set_field(this, PH_FIELD_ARRIVED, Value::Int(arrived + 1));
    let phase = ph_maybe_advance(ctx, this);
    Ok(Some(Value::Int(phase)))
}

pub(crate) fn native_ph_arrive_and_deregister(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let arrived = match ctx.get_field(this, PH_FIELD_ARRIVED) {
        Value::Int(v) => v,
        _ => 0,
    };
    let parties = match ctx.get_field(this, PH_FIELD_PARTIES) {
        Value::Int(v) => v,
        _ => 0,
    };
    ctx.set_field(this, PH_FIELD_ARRIVED, Value::Int(arrived + 1));
    if parties > 0 {
        ctx.set_field(this, PH_FIELD_PARTIES, Value::Int(parties - 1));
    }
    let phase = ph_maybe_advance(ctx, this);
    Ok(Some(Value::Int(phase)))
}

pub(crate) fn native_ph_arrive(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let arrived = match ctx.get_field(this, PH_FIELD_ARRIVED) {
        Value::Int(v) => v,
        _ => 0,
    };
    ctx.set_field(this, PH_FIELD_ARRIVED, Value::Int(arrived + 1));
    let phase = ph_maybe_advance(ctx, this);
    Ok(Some(Value::Int(phase)))
}

pub(crate) fn native_ph_get_phase(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let phase = match ctx.get_field(this, PH_FIELD_PHASE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(phase)))
}

pub(crate) fn native_ph_get_registered_parties(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let parties = match ctx.get_field(this, PH_FIELD_PARTIES) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(parties)))
}

pub(crate) fn native_ph_get_arrived_parties(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let arrived = match ctx.get_field(this, PH_FIELD_ARRIVED) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(arrived)))
}

// ===========================================================================
// PriorityBlockingQueue (sorted array-backed priority queue)
// ===========================================================================

pub(crate) const PBQ_FIELD_DATA: usize = 0;
pub(crate) const PBQ_FIELD_SIZE: usize = 1;
#[allow(dead_code)]
pub(crate) const PBQ_NUM_FIELDS: usize = 2;
pub(crate) const PBQ_DEFAULT_CAPACITY: usize = 16;

pub(crate) fn register_priority_blocking_queue_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // All nine rows below are retired shadows (`RETIRED_SHADOW_L5_TRIPLES`).
    // Over a real JDK the `JdkPriorityBlockingQueue` HotSpot-alignment row
    // (`CRATONVM_COMPAT_JDK_PRIORITY_BLOCKING_QUEUE`, interpreter round i3
    // wave 75, lane L1; on by default since the wave-75 triage) drops every
    // one of them, so the JDK's heap, comparator and `notEmpty` condition run
    // as under `--jdk-only`. Row off (`=0`) and on the synthetic library they
    // keep a SORTED array in the heap's own fields (see
    // `native_pbq_poll_timed`'s doc for what that mix costs); since wave 79
    // they order by the queue's `comparator` and answer an interrupt.
    let c = "java/util/concurrent/PriorityBlockingQueue";
    r.register(c, "<init>", "()V", native_pbq_init);
    r.register(c, "offer", "(Ljava/lang/Object;)Z", native_pbq_offer);
    r.register(c, "poll", "()Ljava/lang/Object;", native_pbq_poll);
    // The TIMED poll too — see `native_pbq_poll_timed` for why leaving it to
    // real bytecode is not an option: that body walks these same two fields as
    // a binary HEAP while every native here keeps them a sorted array.
    r.register(
        c,
        "poll",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        native_pbq_poll_timed,
    );
    r.register(c, "peek", "()Ljava/lang/Object;", native_pbq_peek);
    r.register(c, "put", "(Ljava/lang/Object;)V", native_pbq_put);
    r.register(c, "take", "()Ljava/lang/Object;", native_pbq_take);
    r.register(c, "size", "()I", native_pbq_size);
    r.register(c, "isEmpty", "()Z", native_pbq_is_empty);
    r.set_category(__prev_cat);
}

pub(crate) fn native_pbq_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // Family-1 stale-ObjectRef fix (2026-07-31): `alloc_ref_array` can trigger
    // a young collection that relocates/promotes `this`, so pin it across the
    // allocation and write the fields through the forwarded reference.
    let h_this = ctx.pin_native_root(this);
    let arr = alloc_ref_array(ctx, PBQ_DEFAULT_CAPACITY);
    let this = ctx.read_native_pin(h_this, this);
    ctx.set_field(this, PBQ_FIELD_DATA, Value::Object(Some(arr)));
    ctx.set_field(this, PBQ_FIELD_SIZE, Value::Int(0));
    // Our synthetic offer/poll/peek/etc. manage queue(slot 0)/size(slot 1)
    // directly and ignore the monitor, but the real-JDK PBQ methods we do NOT
    // override — clear(), remove(Object), iterator(), drainTo(), … — all begin
    // with `lock.lock()`. Against the real class the `lock`/`notEmpty` fields
    // are otherwise null here (this native skips the real ctor), so e.g.
    // `clear()` does `null.lock()` -> "Cannot invoke lock on null" (H2 MVStore
    // close -> PriorityBlockingQueue.clear). Seed a genuine ReentrantLock + its
    // Condition so those real methods work. Skipped in synthetic-stub mode
    // (that PBQ layout has no `lock` field).
    if !ctx.is_class_synthetic_stub("java/util/concurrent/PriorityBlockingQueue") {
        pbq_seed_real_lock(ctx, this);
    }
    ctx.unpin_native_roots(h_this);
    Ok(None)
}

/// Seed the real-JDK `PriorityBlockingQueue.lock` (`ReentrantLock`) and
/// `notEmpty` (`Condition`) fields with genuine instances so the real-bytecode
/// PBQ methods we don't override (clear/remove/iterator/drainTo/…), which all
/// take `lock.lock()`, don't NPE on a null lock. GC-safe: `this` and the new
/// lock are pinned across the re-entrant ReentrantLock construction +
/// `newCondition` call (both allocate), and re-read forwarded afterward.
pub(crate) fn pbq_seed_real_lock(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let h_this = ctx.pin_native_root(this);
    let lock =
        match ctx.new_object_initialized("java/util/concurrent/locks/ReentrantLock", "()V", &[]) {
            Ok(Some(Value::Object(Some(l)))) => l,
            _ => {
                ctx.unpin_native_roots(h_this);
                return;
            }
        };
    let h_lock = ctx.pin_native_root(lock);
    let cond = match ctx.invoke(
        "java/util/concurrent/locks/ReentrantLock",
        "newCondition",
        "()Ljava/util/concurrent/locks/Condition;",
        &[Value::Object(Some(lock))],
    ) {
        Ok(Some(v @ Value::Object(Some(_)))) => Some(v),
        _ => None,
    };
    // Re-read forwarded refs (the allocations above may have moved them).
    let lock = ctx.read_native_pin(h_lock, lock);
    let this = ctx.read_native_pin(h_this, this);
    ctx.set_field_by_name(this, "lock", Value::Object(Some(lock)));
    if let Some(cond) = cond {
        ctx.set_field_by_name(this, "notEmpty", cond);
    }
    ctx.unpin_native_roots(h_this);
}

/// Grow the backing array so it can hold `needed` elements.
///
/// Family-1 stale-ObjectRef fix (2026-07-31): `alloc_ref_array` below is a real
/// heap allocation that can trigger a young collection, which relocates (or
/// promotes) both `this` and the *existing* backing array. Both are therefore
/// pinned across the allocation and re-read through their pins before the copy
/// loop and the field write - copying out of a stale `arr` would splice
/// whatever now occupies that address into the queue. Returns the current
/// (possibly forwarded) `this` so the caller never reuses its pre-call copy.
#[must_use]
pub(crate) fn pbq_ensure_capacity(ctx: &mut dyn NativeContext, this: ObjectRef, needed: usize) -> ObjectRef {
    let h_this = ctx.pin_native_root(this);
    let arr = match ctx.get_field(this, PBQ_FIELD_DATA) {
        Value::Object(Some(a)) => a,
        _ => {
            ctx.unpin_native_roots(h_this);
            return this;
        }
    };
    let old_len = ctx.array_length(arr);
    if needed <= old_len {
        ctx.unpin_native_roots(h_this);
        return this;
    }
    let h_arr = ctx.pin_native_root(arr);
    let new_len = (old_len * 2).max(needed).max(PBQ_DEFAULT_CAPACITY);
    let new_arr = alloc_ref_array(ctx, new_len);
    // Re-read both through their pins: the allocation above may have moved
    // them. (`new_arr` itself needs no pin - nothing below it allocates.)
    let this = ctx.read_native_pin(h_this, this);
    let arr = ctx.read_native_pin(h_arr, arr);
    for i in 0..old_len {
        let v = ctx.get_array_element(arr, i);
        ctx.set_array_element(new_arr, i, v);
    }
    ctx.set_field(this, PBQ_FIELD_DATA, Value::Object(Some(new_arr)));
    ctx.unpin_native_roots(h_this);
    this
}

/// MEASURED on the oracle 2026-08-16, alongside its `java.util.PriorityQueue`
/// twin:
///
/// ```text
/// java.util.PriorityQueue    | offer(null) | java.lang.NullPointerException msg=<NO-MESSAGE>
/// j.u.c.PriorityBlockingQueue| offer(null) | java.lang.NullPointerException msg=<NO-MESSAGE>
/// ```
///
/// Both `offer` bodies are `if (e == null) throw new NullPointerException();`
/// compiled to the no-arg constructor (`javap -p -c java.util.PriorityQueue`),
/// so there is no text to carry. `"PriorityBlockingQueue does not permit null
/// elements"` was a FABRICATION — the javadoc sentence, not the message — and
/// `thrownDetail` prints a message when there is one, so it was itself the
/// divergence. `ad_refuse_null`, the `ArrayDeque`/`PriorityQueue` sibling, had
/// already got this right and says so in its own doc comment.
pub(crate) fn pbq_reject_null_element(elem: Value) -> Result<(), MethodCallFailed> {
    if matches!(elem, Value::Object(None)) {
        return Err(bare_npe());
    }
    Ok(())
}

/// Insert `elem` into the sorted backing array (caller holds the monitor).
///
/// Family-1 stale-ObjectRef fix (2026-07-31): the binary search below calls
/// `tree_compare`, which for anything that is not a `String` or a homogeneous
/// primitive wrapper dispatches the element's **real, interpreted**
/// `Comparable.compareTo()` - a full re-entry into the VM that can allocate and
/// trigger a young collection. Every raw `ObjectRef` this function still uses
/// *after* such a call must be pinned and re-read through its pin, exactly like
/// the sibling `tm_binary_search` (see its doc comment). That is all three of:
///
/// * `arr` - dereferenced by `get_array_element` on the *next* loop iteration
///   and by the post-loop shift/insert. A stale `arr` decodes to whatever now
///   occupies that address; observed as a `gen_heap` "left: Object, right:
///   Array" assertion failure in the isolated repro.
/// * `elem` - re-passed to `tree_compare` on every iteration and finally stored
///   by `set_array_element(arr, low, elem)`. A stale `elem` writes a dangling
///   reference into the queue that a later `offer()`/`poll()` reads back as an
///   unrelated object - H2 MVStore's "java.lang.Object cannot be cast to
///   org.h2.mvstore.FileStore$RemovedPageInfo".
/// * `this` - written by the trailing `set_field(PBQ_FIELD_SIZE)`.
///
/// `pbq_ensure_capacity` also allocates, hence it returns the forwarded `this`.
pub(crate) fn pbq_offer_locked(ctx: &mut dyn NativeContext, this: ObjectRef, elem: Value) -> MethodCallResult {
    let size = match ctx.get_field(this, PBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    let h_this = ctx.pin_native_root(this);
    let h_elem = pin_value(ctx, elem);
    let mut elem = elem;
    let mut this = pbq_ensure_capacity(ctx, this, (size + 1) as usize);
    elem = read_pinned_elem(ctx, h_elem, elem);
    let mut arr = match ctx.get_field(this, PBQ_FIELD_DATA) {
        Value::Object(Some(a)) => a,
        _ => {
            ctx.unpin_native_roots(h_this);
            return Ok(Some(Value::Int(1)));
        }
    };
    let h_arr = ctx.pin_native_root(arr);
    // Binary search for the insertion position, ordered by the queue's own
    // `comparator` (set by the JDK's `PriorityBlockingQueue(int, Comparator)`
    // / collection constructors, which still run) or, when it is null or the
    // layout has no such field (the synthetic library), natural ordering.
    // Interpreter round i3 wave 79, lane L1
    // (`i75-L1-compatible-priorityblockingqueue-stand-ins-ignore-comparator-and-interrupt`):
    // this used to pass a null comparator always, so
    // `new PriorityBlockingQueue<>(11, Comparator.reverseOrder())` polled in
    // natural order and a queue of non-`Comparable` elements failed. The
    // comparator is pinned: `tree_compare` runs Java.
    let mut comparator = match ctx.get_field_by_name(this, "comparator") {
        v @ Value::Object(Some(_)) => v,
        _ => Value::Object(None),
    };
    let h_cmp = pin_value(ctx, comparator);
    let mut low: usize = 0;
    let mut high = size as usize;
    while low < high {
        let mid = low + (high - low) / 2;
        let mid_elem = ctx.get_array_element(arr, mid);
        let cmp = match tree_compare(ctx, &comparator, mid_elem, elem) {
            Ok(c) => c,
            Err(e) => {
                ctx.unpin_native_roots(h_this);
                return Err(e);
            }
        };
        // The comparison ran real bytecode - refresh everything reused below.
        this = ctx.read_native_pin(h_this, this);
        arr = ctx.read_native_pin(h_arr, arr);
        elem = read_pinned_elem(ctx, h_elem, elem);
        comparator = read_pinned_elem(ctx, h_cmp, comparator);
        if cmp < 0 {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    // Shift elements right to make room.
    for i in (low..(size as usize)).rev() {
        let v = ctx.get_array_element(arr, i);
        ctx.set_array_element(arr, i + 1, v);
    }
    ctx.set_array_element(arr, low, elem);
    ctx.set_field(this, PBQ_FIELD_SIZE, Value::Int(size + 1));
    ctx.unpin_native_roots(h_this);
    Ok(Some(Value::Int(1)))
}

pub(crate) fn pbq_poll_locked(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    let size = match ctx.get_field(this, PBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size == 0 {
        return Value::Object(None);
    }
    let arr = match ctx.get_field(this, PBQ_FIELD_DATA) {
        Value::Object(Some(a)) => a,
        _ => return Value::Object(None),
    };
    // Remove head (smallest element at index 0).
    let head = ctx.get_array_element(arr, 0);
    for i in 0..(size - 1) as usize {
        let v = ctx.get_array_element(arr, i + 1);
        ctx.set_array_element(arr, i, v);
    }
    ctx.set_array_element(arr, (size - 1) as usize, Value::Object(None));
    ctx.set_field(this, PBQ_FIELD_SIZE, Value::Int(size - 1));
    head
}

pub(crate) fn native_pbq_offer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    pbq_reject_null_element(elem)?;
    // Family-1 stale-ObjectRef fix (2026-07-31): `monitor_enter` can park this
    // thread and `pbq_offer_locked` re-enters the interpreter through the
    // element's `compareTo`, so a collection can run between any two lines
    // here. Pin `this`/`elem` for the whole critical section and re-read them
    // through their pins before every subsequent use.
    let h_this = ctx.pin_native_root(this);
    let h_elem = pin_value(ctx, elem);
    // STW-safepoint fix (2026-07-31): `monitor_enter_gc_safe`, not the plain
    // `monitor_enter`. A plain enter blocks as a *counted* mutator, so a writer
    // waiting on this queue's monitor never arrives at a concurrent
    // stop-the-world barrier and the whole VM wedges ("STW cross-thread JIT
    // takeover is still waiting for cooperative mutators rounds=64 pending=3
    // taken=0"). The escape hatch is only sound for callers that pin and
    // re-read every raw ref across the wait, which is exactly what the pins
    // above now provide - same contract as `ChmMonitorGuard::acquire_gc_safe`.
    let this = ctx.monitor_enter_gc_safe(this);
    let this = ctx.read_native_pin(h_this, this);
    let elem = read_pinned_elem(ctx, h_elem, elem);
    let result = pbq_offer_locked(ctx, this, elem);
    let this = ctx.read_native_pin(h_this, this);
    if result.is_ok() {
        let _ = ctx.monitor_notify_all(this);
    }
    let this = ctx.read_native_pin(h_this, this);
    ctx.monitor_exit(this);
    ctx.unpin_native_roots(h_this);
    result
}

pub(crate) fn native_pbq_poll(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Family-1 stale-ObjectRef fix (2026-07-31): `monitor_enter` can park this
    // thread, so `this` may be relocated before the dequeue runs; the dequeued
    // head likewise has to survive `monitor_exit`.
    let h_this = ctx.pin_native_root(this);
    // STW-safepoint fix (2026-07-31) - see `native_pbq_offer`.
    let this = ctx.monitor_enter_gc_safe(this);
    let this = ctx.read_native_pin(h_this, this);
    let head = pbq_poll_locked(ctx, this);
    let h_head = pin_value(ctx, head);
    ctx.monitor_exit(this);
    let head = read_pinned_elem(ctx, h_head, head);
    ctx.unpin_native_roots(h_this);
    Ok(Some(head))
}

pub(crate) fn native_pbq_peek(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let size = match ctx.get_field(this, PBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let arr = match ctx.get_field(this, PBQ_FIELD_DATA) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_array_element(arr, 0)))
}

pub(crate) fn native_pbq_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_pbq_offer(ctx, args)?;
    Ok(None)
}

/// The `InterruptedException` (no message) `take()` / `poll(long, TimeUnit)`
/// raise for an interrupt: pending at entry (`lockInterruptibly`) or arriving
/// while waiting. Interpreter round i3 wave 79, lane L1.
pub(crate) fn pbq_interrupted() -> MethodCallFailed {
    cratonvm_types::error::RuntimeError::InterruptedException.into()
}

pub(crate) fn native_pbq_take(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // JDK `take()` opens with `lock.lockInterruptibly()`: a pending interrupt
    // raises even when an element is ready. See `pbq_interrupted`.
    if ctx.is_interrupted(true) {
        return Err(pbq_interrupted());
    }
    // Family-1 stale-ObjectRef fix (2026-07-31): this method blocks - both
    // `monitor_enter` and every `monitor_wait` iteration are points at which
    // another thread can run a collection that relocates `this`. Pin it and
    // re-read it through the pin after each blocking call.
    let h_this = ctx.pin_native_root(this);
    // STW-safepoint fix (2026-07-31) - see `native_pbq_offer`.
    let this = ctx.monitor_enter_gc_safe(this);
    let mut this = ctx.read_native_pin(h_this, this);
    loop {
        let size = match ctx.get_field(this, PBQ_FIELD_SIZE) {
            Value::Int(v) => v,
            _ => 0,
        };
        if size > 0 {
            break;
        }
        // The wait's error is the waiter's interrupt (`notEmpty.await()`
        // throws `InterruptedException`); it used to be discarded and the
        // waiter kept waiting.
        let waited = ctx.monitor_wait(this, Some(50));
        this = ctx.read_native_pin(h_this, this);
        if let Err(e) = waited {
            ctx.monitor_exit(this);
            ctx.unpin_native_roots(h_this);
            return Err(e);
        }
    }
    let head = pbq_poll_locked(ctx, this);
    let h_head = pin_value(ctx, head);
    ctx.monitor_exit(this);
    let head = read_pinned_elem(ctx, h_head, head);
    ctx.unpin_native_roots(h_this);
    Ok(Some(head))
}

/// A `java.util.concurrent.TimeUnit` constant's timeout in MILLISECONDS.
///
/// The real `TimeUnit` is a JDK enum: its ordinal lives in the `Enum` base
/// field `ordinal`, while object slot 0 holds the enum `name` (a `String`).
/// Reading slot 0 for the ordinal works against a synthetic one-field stand-in
/// and is WRONG for the real enum — `await(N, SECONDS)` then waits `N` ms — so
/// the named field is preferred and slot 0 is only the fallback.
///
/// This duplicates `native-builtins`' `time_unit_ordinal` /
/// `convert_time_unit_to_millis` pair, and the duplication is a crate boundary
/// rather than a choice: both are private to that crate and `native-collections`
/// does not depend on it. If a third copy is ever wanted, move this one into
/// `native-api` instead of adding it — the scaling-UP arms below have to
/// SATURATE (HotSpot's `TimeUnit.SECONDS.toMillis(Long.MAX_VALUE)` is
/// `Long.MAX_VALUE`, not a wrapped negative), and that is exactly the kind of
/// rule that goes wrong when it is written out a third time.
pub(crate) fn pbq_timeout_millis(ctx: &mut dyn NativeContext, value: i64, unit: Option<ObjectRef>) -> i64 {
    let ordinal = match unit {
        Some(u) => match ctx.get_field_by_name(u, "ordinal") {
            Value::Int(v) => v,
            _ => match ctx.get_field(u, 0) {
                Value::Int(v) => v,
                _ => 2,
            },
        },
        None => 2,
    };
    match ordinal {
        0 => value / 1_000_000,
        1 => value / 1_000,
        2 => value,
        3 => value.saturating_mul(1_000),
        4 => value.saturating_mul(60_000),
        5 => value.saturating_mul(3_600_000),
        6 => value.saturating_mul(86_400_000),
        _ => value,
    }
}

/// `poll(long, TimeUnit)` — the TIMED blocking read.
///
/// # Why this has to be a native, and what happened when it was not
///
/// The natives above own `offer`/`put`/`poll()`/`take()` and keep the queue in
/// a SORTED ARRAY (`PBQ_FIELD_DATA` ascending, `PBQ_FIELD_SIZE` its length).
/// The real `java.util.concurrent.PriorityBlockingQueue` keeps a BINARY HEAP in
/// the same two fields and its own `dequeue()` sifts down through them. The two
/// representations agree only for zero or one element.
///
/// Leaving `poll(long, TimeUnit)` to real bytecode therefore mixed them, and
/// both possible outcomes are bad:
///
/// * **As shipped**, the real body waits on `notEmpty` under `lock`, and the
///   natives signal the receiver's OBJECT MONITOR instead — no `Condition`
///   waiter is on that — so a consumer that actually had to wait was woken only
///   by whatever periodic re-check its `await` made. Measured against HotSpot
///   with an empty queue and a producer that puts after 300 ms: HotSpot returns
///   in under a second, this VM took more than one, and
///   `probes/ConcurrencyUnderGcSweep.java` expired a 120 s phase deadline
///   draining 480 elements.
/// * **Signalling `notEmpty` from the native insert** — the obvious repair, and
///   it was tried — wakes the real `dequeue()` promptly and it then sifts a
///   sorted array as if it were a heap: `NullPointerException: Cannot invoke
///   "java.lang.Comparable.compareTo(Object)" because "key" is null`, and 1 of
///   480 elements consumed. Waking the mixed path is worse than starving it.
///
/// So the fix is to stop mixing: this is `native_pbq_take` with a deadline, on
/// the native representation, coordinated through the same object monitor as
/// every other native here. `null` on timeout, per the `BlockingQueue`
/// contract.
///
/// The remaining real-bytecode readers (`drainTo`, `remove(Object)`,
/// `iterator`) still see the sorted array through a heap-shaped body. That is
/// pre-existing and unchanged here; the representation mismatch is the real
/// defect and it wants its own page.
pub(crate) fn native_pbq_poll_timed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let timeout = match args.get(1) {
        // WP4.5 tag erasure: a `long` popped generically can come back tagged
        // `Double` with the bits intact, exactly as `Object.wait(J)` handles.
        Some(Value::Long(v)) => *v,
        Some(Value::Double(d)) => d.to_bits() as i64,
        _ => 0,
    };
    let unit = match args.get(2) {
        Some(Value::Object(Some(u))) => Some(*u),
        _ => None,
    };
    let timeout_ms = pbq_timeout_millis(ctx, timeout, unit).max(0) as u64;
    // A deadline past what `Instant` can hold is no deadline at all (it used
    // to be an unchecked `+`).
    let deadline =
        std::time::Instant::now().checked_add(std::time::Duration::from_millis(timeout_ms));
    // `poll(long, TimeUnit)` opens with `lock.lockInterruptibly()` too.
    if ctx.is_interrupted(true) {
        return Err(pbq_interrupted());
    }
    // Family-1: this method BLOCKS, so pin the receiver and re-read it after
    // every blocking call — see `native_pbq_take`.
    let h_this = ctx.pin_native_root(this);
    let this = ctx.monitor_enter_gc_safe(this);
    let mut this = ctx.read_native_pin(h_this, this);
    loop {
        let size = match ctx.get_field(this, PBQ_FIELD_SIZE) {
            Value::Int(v) => v,
            _ => 0,
        };
        if size > 0 {
            break;
        }
        let remaining = match deadline {
            Some(d) => d.saturating_duration_since(std::time::Instant::now()),
            None => std::time::Duration::from_millis(50),
        };
        if remaining.is_zero() {
            ctx.monitor_exit(this);
            ctx.unpin_native_roots(h_this);
            return Ok(Some(Value::Object(None)));
        }
        // Bounded, so the deadline is re-checked even if a notify is lost.
        let wait_ms = (remaining.as_millis().min(50) as u64).max(1);
        // The wait's error is the waiter's interrupt (`notEmpty.awaitNanos`
        // throws `InterruptedException`); it used to be discarded.
        let waited = ctx.monitor_wait(this, Some(wait_ms));
        this = ctx.read_native_pin(h_this, this);
        if let Err(e) = waited {
            ctx.monitor_exit(this);
            ctx.unpin_native_roots(h_this);
            return Err(e);
        }
    }
    let head = pbq_poll_locked(ctx, this);
    let h_head = pin_value(ctx, head);
    ctx.monitor_exit(this);
    let head = read_pinned_elem(ctx, h_head, head);
    ctx.unpin_native_roots(h_this);
    Ok(Some(head))
}

pub(crate) fn native_pbq_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let size = match ctx.get_field(this, PBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(size)))
}

pub(crate) fn native_pbq_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    let size = match ctx.get_field(this, PBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(i32::from(size == 0))))
}

// ===========================================================================
// Executors factory additions
// ===========================================================================

pub(crate) fn register_executors_scheduled_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Some runtimes resolve STPE constructors through this crate's registry,
    // so keep both ctor shapes available here (including ThreadFactory variant).
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "<init>",
        "(I)V",
        native_stpe_init,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "<init>",
        "(ILjava/util/concurrent/ThreadFactory;)V",
        native_stpe_init,
    );
    r.register(
        "java/util/concurrent/Executors",
        "newScheduledThreadPool",
        "(I)Ljava/util/concurrent/ScheduledExecutorService;",
        native_executors_new_scheduled_pool,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "setKeepAliveTime",
        "(JLjava/util/concurrent/TimeUnit;)V",
        native_stpe_set_keep_alive_time,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "getKeepAliveTime",
        "(Ljava/util/concurrent/TimeUnit;)J",
        native_stpe_get_keep_alive_time,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "schedule",
        "(Ljava/lang/Runnable;JLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;",
        native_stpe_schedule,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "scheduleAtFixedRate",
        "(Ljava/lang/Runnable;JJLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;",
        native_stpe_schedule_fixed_rate,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "scheduleWithFixedDelay",
        "(Ljava/lang/Runnable;JJLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;",
        native_stpe_schedule_fixed_delay,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "shutdown",
        "()V",
        native_stpe_shutdown,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "isShutdown",
        "()Z",
        native_stpe_is_shutdown,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "setMaximumPoolSize",
        "(I)V",
        native_stpe_set_maximum_pool_size,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "setCorePoolSize",
        "(I)V",
        native_stpe_set_core_pool_size,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "setContinueExistingPeriodicTasksAfterShutdownPolicy",
        "(Z)V",
        native_stpe_ignore_policy_setter,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "setExecuteExistingDelayedTasksAfterShutdownPolicy",
        "(Z)V",
        native_stpe_ignore_policy_setter,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "setRemoveOnCancelPolicy",
        "(Z)V",
        native_stpe_ignore_policy_setter,
    );
    // ScheduledFuture instance methods. `schedule(...)` (native_stpe_schedule)
    // returns an object whose runtime class is the `ScheduledFuture` interface.
    // Without these, a caller holding the result as `Future` and calling
    // `cancel`/`isCancelled`/`isDone`/`get` resolves to the abstract `Future`
    // declaration (no Code) and throws `AbstractMethodError`. Canonical tripwire:
    // JUnit Jupiter's `@Timeout` schedules an interrupt task and calls
    // `future.cancel(false)` in its `finally` block — that single call previously
    // failed *every* test in a `@Timeout`-annotated class (the bulk of the kafka
    // suite). Field layout (alloc_pending_future / alloc_completed_future):
    // 0 = result value, 1 = state (0=pending, 1=done, 2=cancelled).
    let sf = "java/util/concurrent/ScheduledFuture";
    r.register(sf, "cancel", "(Z)Z", native_sf_cancel);
    r.register(sf, "isCancelled", "()Z", native_sf_is_cancelled);
    r.register(sf, "isDone", "()Z", native_sf_is_done);
    r.register(sf, "get", "()Ljava/lang/Object;", native_sf_get);
    r.register(
        sf,
        "get",
        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;",
        native_sf_get,
    );
    r.set_category(__prev_cat);
}

pub(crate) fn native_executors_new_scheduled_pool(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let pool_size = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    let executor = try_alloc_synthetic(
        ctx,
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        STPE_NUM_FIELDS,
    )?;
    // gc-common w20-a: rooted across the task array's allocation (every store
    // below went through the executor's pre-allocation address).
    let executor_pin = ctx.pin_native_root(executor);
    let task_arr: ObjectRef = alloc_ref_array(ctx, 16);
    let executor = ctx.read_native_pin(executor_pin, executor);
    ctx.unpin_native_roots(executor_pin);
    ctx.set_field(executor, STPE_FIELD_POOL_SIZE, Value::Int(pool_size));
    ctx.set_field(executor, STPE_FIELD_SHUTDOWN, Value::Int(0));
    ctx.set_field(
        executor,
        STPE_FIELD_TASK_LIST,
        Value::Object(Some(task_arr)),
    );
    Ok(Some(Value::Object(Some(executor))))
}

// ===========================================================================
// Phase 3.1: java.util.concurrent completeness
// ===========================================================================
//
// This section adds missing CompletionStage methods on CompletableFuture,
// ForkJoinPool.awaitQuiescence, ThreadPoolExecutor stat methods, and
// completes any gaps in the concurrent utilities. These complement the
// registrations already present in native-builtins.

// CompletableFuture field layout (shared with native-builtins):
// field 0 = result value, field 1 = done flag (0=pending, 1=normal, 2=exceptional, 3=cancelled)
//
// Slot 0 aliases the real `volatile Object result` and that aliasing is
// deliberate and load-bearing — `native_cf_complete` stores the JDK's own
// `AltResult`/`NIL` encoding there so the un-intercepted `isDone()`/`get()`
// bytecode agrees with us.
//
// Slot 1 aliases the real `volatile Completion stack`, which is a REFERENCE,
// and the `done` marker written into it is an `Int`. That is the slot-index
// species of natives-over-real-jdk-classes.md §5, and it is NOT repaired here:
// the same Int-vs-reference test is this file's discriminator between a
// synthetic CF and a real one (`cf_is_real_jdk`), it is the only discriminator
// that stays correct for a real SUBCLASS such as `KafkaCompletableFuture`
// (whose slot 1 is still `stack`), and moving it needs a build. See
// W7-66-live-over-allocations.md §"left, with the reason".
//
// What IS repaired: the WIDTH. Real `java.util.concurrent.CompletableFuture`
// declares exactly two instance fields (`javap -p`, JDK 25.0.3.9: `result`,
// `stack`); the two slots below this comment were allocated on every synthetic
// CF and then never read or written anywhere in the workspace. They are gone,
// so the request now equals the declared width.
pub(crate) const CF_FIELD_RESULT: usize = 0;
pub(crate) const CF_FIELD_DONE: usize = 1;

/// `CRATONVM_NATIVE_CF_POSTCOMPLETE_DIRECT` — default-ON, `=0` opts out. Routes
/// the `postComplete()` callback through `invoke_virtual_bytecode_only`
/// instead of `invoke_virtual`, i.e. straight to the interpreter's `execute`
/// rather than through the by-NAME native resolver that misses. See the call
/// site in `native_cf_complete`.
pub(crate) fn cf_postcomplete_direct_enabled() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static GATE: AtomicU8 = AtomicU8::new(0);
    match GATE.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on = match cratonvm_types::flags::runtime_var(
                "CRATONVM_NATIVE_CF_POSTCOMPLETE_DIRECT",
            ) {
                Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
                Err(_) => true,
            };
            GATE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

/// `CRATONVM_NATIVE_CF_POSTCOMPLETE_SKIP` — default-ON, `=0` opts out. See the
/// call site in `native_cf_complete`.
pub(crate) fn cf_postcomplete_skip_enabled() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static GATE: AtomicU8 = AtomicU8::new(0);
    match GATE.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on =
                match cratonvm_types::flags::runtime_var("CRATONVM_NATIVE_CF_POSTCOMPLETE_SKIP") {
                    Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
                    Err(_) => true,
                };
            GATE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}
/// Number of slots a CratonVM *synthetic* `CompletableFuture` carries — the
/// real declared width, because `CF_FIELD_RESULT`/`CF_FIELD_DONE` are the only
/// two slots any native in this crate touches.
pub(crate) const CF_NUM_FIELDS: usize = 2;

pub(crate) fn register_concurrent_completeness_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // The four `CopyOnWriteArrayList` rows are retired shadows; over a real
    // JDK the `JdkCopyOnWrite` HotSpot-alignment row
    // (`CRATONVM_COMPAT_JDK_COPY_ON_WRITE`, default on since the wave-74
    // triage, `=0` reverts; interpreter round
    // i3 wave 72, lane L1) drops them with the class's other stand-ins.
    // Wave 76 (lane L1): the row graduated for them -- over a real JDK they
    // are not registered whatever the row says, so they serve the synthetic
    // class library only (stated `Bridge` above;
    // `i74-L1-proposal-retire-the-cowal-stand-ins-from-the-real-jdk-boot-20261107`).
    if !r.real_jdk() {
        r.register(
            "java/util/concurrent/CopyOnWriteArrayList",
            "addIfAbsent",
            "(Ljava/lang/Object;)Z",
            native_cowal_add_if_absent,
        );
        r.register(
            "java/util/concurrent/CopyOnWriteArrayList",
            "contains",
            "(Ljava/lang/Object;)Z",
            native_cowal_contains,
        );
        r.register(
            "java/util/concurrent/CopyOnWriteArrayList",
            "bulkRemove",
            "(Ljava/util/function/Predicate;)Z",
            native_cowal_bulk_remove_predicate,
        );
        r.register(
            "java/util/concurrent/CopyOnWriteArrayList",
            "addAll",
            "(Ljava/util/Collection;)Z",
            native_cowal_add_all,
        );
    }

    let cf = "java/util/concurrent/CompletableFuture";

    // --- CompletionStage methods ---
    //
    // OVER A REAL JDK THESE SEVEN ARE NOT REGISTERED AT ALL.
    //
    // Each one's real-JDK arm is a pure delegation straight back to the method
    // it shadows — `native_cf_then_compose` is literally
    //
    //     if cf_is_real_jdk(ctx, this) {
    //         return ctx.invoke_special("java/util/concurrent/CompletableFuture",
    //                                   "uniComposeStage", .., &[this, null, fn]);
    //     }
    //
    // and the real `thenCompose(fn)` is `return uniComposeStage(null, fn)`. So
    // registering them buys a native dispatch plus a by-name class resolve plus
    // a nested invoke, to arrive at the bytecode that would otherwise have run.
    // The synthetic 2-field CF model below each of those branches is what they
    // exist for, and it is untouched: `real_jdk()` is false in synthetic mode.
    //
    // MEASURED 2026-08-22, `apps/hibernate-reactive-suite-runner/HibfixCfBound.java`,
    // ns/op on an already-completed stage, same binary, same box. `copy()` and
    // `minimalCompletionStage()` build the SAME `uni*Stage` dependent machinery
    // in the same interpreter but carry no native, so they price the bytecode
    // alone:
    //
    //   copy()                    708      <- no native
    //   minimalCompletionStage() 1149      <- no native
    //   thenApply()              5458      <- native + delegation
    //   thenCompose()           10256      <- native + delegation
    //
    // i.e. roughly 85% of a shadowed call was the shadow. On
    // `MultithreadedInsertionWithLazyConnectionTest` these account for 4.36M
    // native invocations and 37% of all profile samples.
    //
    // `thenCombine` is deliberately still registered: its native raises its own
    // NullPointerException for a null other-stage BEFORE reaching the
    // delegation, so it is not a pure pass-through, and it is not hot on any
    // workload this was measured against.
    //
    // `CRATONVM_CF_DELEGATING_YIELD=0` restores the registrations so one binary
    // can be A/B'd against its own previous behaviour.
    let skip_delegating_cf = r.real_jdk() && cf_delegating_yield_enabled();

    if !skip_delegating_cf {
        r.register(
            cf,
            "thenApply",
            "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
            native_cf_then_apply_p31,
        );

        r.register(
            cf,
            "thenAccept",
            "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletableFuture;",
            native_cf_then_accept_p31,
        );

        // thenRun: run Runnable after completion, return new CF with null result
        r.register(
            cf,
            "thenRun",
            "(Ljava/lang/Runnable;)Ljava/util/concurrent/CompletableFuture;",
            native_cf_then_run,
        );

        // thenCompose: apply Function that returns CompletableFuture, then flatten
        r.register(
            cf,
            "thenCompose",
            "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
            native_cf_then_compose,
        );
    }

    // NOT skipped: see the note above — `thenCombine` is not a pure pass-through.

    // thenCombine: combine results of two CFs with a BiFunction
    r.register(
        cf,
        "thenCombine",
        "(Ljava/util/concurrent/CompletionStage;Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_then_combine,
    );

    // exceptionally: provide fallback if exception occurred
    if !skip_delegating_cf {
        r.register(
            cf,
            "exceptionally",
            "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
            native_cf_exceptionally,
        );

        // handle: BiFunction(result, exception) → new result
        r.register(
            cf,
            "handle",
            "(Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;",
            native_cf_handle,
        );

        // whenComplete: BiConsumer(result, exception) called after completion
        r.register(
            cf,
            "whenComplete",
            "(Ljava/util/function/BiConsumer;)Ljava/util/concurrent/CompletableFuture;",
            native_cf_when_complete,
        );
    }

    // allOf: CompletableFuture[] → CompletableFuture<Void>
    r.register(
        cf,
        "allOf",
        "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_all_of,
    );

    // anyOf: CompletableFuture[] → CompletableFuture<Object>
    r.register(
        cf,
        "anyOf",
        "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_any_of,
    );

    // complete — registered because the real CompletableFuture.complete(null)
    // relies on the static `NIL` AltResult sentinel, which WAS effectively null
    // on CratonVM, so `complete(null)` left `result == null` (isDone() stayed
    // false). That broke e.g. KafkaFuture.allOf(...) whose result is completed
    // with `complete(null)`, leaving it pending forever (get() hangs).
    //
    // **The premise is stale AND the registration is still right.** Both halves
    // were measured on 2026-09-02, and the second is the surprising one.
    //
    // The premise first. Reading the field directly through
    // `--add-opens java.base/java.util.concurrent`:
    //
    //     HotSpot   NIL = java.util.concurrent.CompletableFuture$AltResult@...  NIL.ex = null
    //     CratonVM  NIL = java.util.concurrent.CompletableFuture$AltResult@4c3  NIL.ex = null
    //
    // `NIL` is a proper `AltResult` here now, and with this registration OFF
    // the real bytecode answers `complete(null) -> true, isDone=true,
    // get=null` and `allOf(...)` completes — identical to HotSpot. So the hang
    // this bridge was written to prevent does not reproduce, and "it shadows
    // real bytecode for a reason that has expired" is a fair reading of it.
    //
    // It is still the wrong conclusion. `CompletableFuture` composition is
    // ~20x HotSpot and this native is 2.50 crossings per chain, which makes it
    // look exactly like `AtomicReference.compareAndSet`'s synthetic stub —
    // de-registered on 2026-08-29 for a 1.6x win, on the argument that a stub
    // over one line of real JDK bytecode is a pure tax. MEASURED here, one
    // binary, this switch the only difference, six interleaved reps,
    // `HibfixComposeProbe2` 2 threads x 320 000 chains, load 14-22:
    //
    //     registered (default)   20.64-23.55 s cpu   (median 22.16)   34.6 us/chain
    //     de-registered          66.56-70.49 s cpu   (median 69.52)  108.6 us/chain
    //
    // **3.14x SLOWER with the bridge gone**, ranges disjoint, `wrong=0` in all
    // twelve runs. The native census says why, and it is not "the bytecode is
    // slow" — the crossing does not disappear, it MULTIPLIES (80 000 chains):
    //
    //     CompletableFuture.complete        200 000 ->       0
    //     CompletableFuture.completeValue         0 -> 200 000   (itself a registered native)
    //     Unsafe.compareAndSetInt               619 -> 200 000   (+2.5/chain)
    //     Object.<init>                       2 882 -> 122 174   (+1.5/chain)
    //
    // The real `complete` is `completeValue(value)` — which is ANOTHER
    // registered native, so the boundary is crossed anyway — plus the CAS and
    // the `AltResult`/`Completion` allocation that this one collapses. This
    // bridge is not a shadow in front of cheap bytecode; it is a fast path in
    // front of three more boundary crossings and an allocation.
    //
    // The switch is kept because that is a strong claim and it should stay
    // one run away from being re-checked, not one BUILD away: the "synthetic
    // stub over a real JDK method is a pure tax" pattern is real, it has paid
    // out before, and the next person to notice 2.50 crossings per chain here
    // will reach for it. **Do not flip this default.** If it is ever flipped,
    // the number above is what has to move first.
    //
    // Note also that a class-scoped retirement is the wrong instrument for
    // this cluster even if the per-triple answer were the other way: this
    // native serves a SYNTHETIC CompletableFuture too (an Int `done` marker at
    // slot 1 instead of the real `stack` reference), and retiring a cluster
    // wholesale is the shape that once left `ConcurrentHashMap` with a retired
    // constructor and live mutators, silently losing five of six entries (see
    // `admit_forced_native`'s header).
    //
    // `CRATONVM_NATIVE_CF_COMPLETE=0` — do not register it; the real JDK
    // bytecode runs instead, correctly, and 3.14x slower.
    //
    // Interpreter round i3 wave 80 (lane L1): `complete` is a retired row
    // (`RETIRED_SHADOW_L5_TRIPLES`; `--jdk-only` has run the JDK body since),
    // so the `JdkCompletableFuture` HotSpot-alignment row
    // (`CRATONVM_COMPAT_JDK_COMPLETABLE_FUTURE=1`, default off) drops it over
    // a real JDK with every other stand-in on the class, whatever this switch
    // says. `L1W80CompletableFutureBench` re-prices the 2026-09-02 number.
    if !matches!(
        cratonvm_types::flags::runtime_var("CRATONVM_NATIVE_CF_COMPLETE").as_deref(),
        Ok("0")
    ) {
        r.register(cf, "complete", "(Ljava/lang/Object;)Z", native_cf_complete);
    }

    // completeExceptionally
    r.register(
        cf,
        "completeExceptionally",
        "(Ljava/lang/Throwable;)Z",
        native_cf_complete_exceptionally,
    );

    // isCompletedExceptionally — true for done=2 (exceptional) or done=3 (cancelled)
    r.register(cf, "isCompletedExceptionally", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        // Real encoding (set by real `complete`/`obtrudeException`, including
        // our routed `completeExceptionally`): exceptionally completed iff
        // `result` (slot 0) is an `AltResult` whose `ex` is non-null.
        if let Value::Object(Some(r)) = ctx.get_field(this, 0) {
            if cf_is_alt_result(ctx, r) {
                let ex = ctx.get_field(r, 0);
                return Ok(Some(Value::Int(i32::from(!matches!(
                    ex,
                    Value::Object(None)
                )))));
            }
        }
        // Legacy synthetic `DONE` marker (CratonVM-chained CFs).
        let done = match ctx.get_field(this, CF_FIELD_DONE) {
            Value::Int(d) => d,
            _ => 0,
        };
        Ok(Some(Value::Int(if done == 2 || done == 3 { 1 } else { 0 })))
    });

    // The four one-argument `*Async` stages (delegate to the synchronous stage
    // in the single-threaded synthetic model).
    //
    // OVER A REAL JDK THEY ARE NOT REGISTERED, with the seven above (nio20
    // wave 5, L1). They share those natives, whose real-JDK arm is
    // `uniApplyStage(null, fn)` / `uniAcceptStage` / `uniRunStage` /
    // `uniComposeStage` with a NULL executor -- the SYNCHRONOUS stage -- where
    // the JDK's `thenApplyAsync(fn)` is `uniApplyStage(defaultExecutor(), fn)`:
    // `--compatible` ran the function on the completing (or calling) thread,
    // HotSpot on `ForkJoinPool.commonPool()` (`Thread.currentThread()` and
    // every lock / thread-local the function touches differed). The four are
    // retired rows (`RETIRED_SHADOW_L5_TRIPLES`), so `--jdk-only` already runs
    // the JDK bodies; this is the same row set, gated the same way, so
    // `CRATONVM_CF_DELEGATING_YIELD=0` restores them with the seven.
    if !skip_delegating_cf {
        r.register(
            cf,
            "thenApplyAsync",
            "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
            native_cf_then_apply_p31,
        );

        r.register(
            cf,
            "thenAcceptAsync",
            "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletableFuture;",
            native_cf_then_accept_p31,
        );

        r.register(
            cf,
            "thenRunAsync",
            "(Ljava/lang/Runnable;)Ljava/util/concurrent/CompletableFuture;",
            native_cf_then_run,
        );

        r.register(
            cf,
            "thenComposeAsync",
            "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
            native_cf_then_compose,
        );
    }

    // Also register CompletionStage interface methods
    let cs = "java/util/concurrent/CompletionStage";
    // Same argument as the class-level block above, and it matters MORE here:
    // hibernate-reactive types its whole composition chain as `CompletionStage`,
    // so these are `invokeinterface` sites. Over a real JDK the interface
    // declares them abstract, every implementation has its own body, and this
    // native would delegate to `CompletableFuture` for a receiver that may not
    // be one.
    if !skip_delegating_cf {
        r.register(
            cs,
            "thenApply",
            "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletionStage;",
            native_cf_then_apply_p31,
        );
        r.register(
            cs,
            "thenAccept",
            "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletionStage;",
            native_cf_then_accept_p31,
        );
        r.register(
            cs,
            "thenRun",
            "(Ljava/lang/Runnable;)Ljava/util/concurrent/CompletionStage;",
            native_cf_then_run,
        );
        r.register(
            cs,
            "thenCompose",
            "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletionStage;",
            native_cf_then_compose,
        );
        r.register(
            cs,
            "exceptionally",
            "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletionStage;",
            native_cf_exceptionally,
        );
    }

    // ForkJoinPool.awaitQuiescence is registered by native-builtins after it
    // establishes the real async-worker completion tracker. Keeping a local
    // constant here would overwrite that stateful implementation.

    // --- ThreadPoolExecutor stat methods ---
    let tp = "java/util/concurrent/ThreadPoolExecutor";
    r.register(tp, "getPoolSize", "()I", |ctx, args| {
        let this = tp_arg0(args);
        if let Some(this) = this {
            // invoke_special_bytecode_only (`tp_real_call`), NOT
            // invoke_virtual_bytecode_only: a subclass (e.g. Reactor's
            // BoundedElasticScheduler$BoundedScheduledExecutorService) that
            // overrides this method and calls `super.getPoolSize()` would
            // otherwise re-dispatch straight back into its own override via
            // virtual dispatch on the concrete receiver, re-entering this native
            // forever (the exact StackOverflowError shape already fixed for
            // `shutdownNow` below).
            if let Some(result) = tp_real_call(ctx, this, "getPoolSize", "()I", &[]) {
                return result;
            }
        }
        Ok(Some(Value::Int(1)))
    });
    r.register(tp, "getActiveCount", "()I", |ctx, args| {
        let this = tp_arg0(args);
        if let Some(this) = this {
            if let Some(result) = tp_real_call(ctx, this, "getActiveCount", "()I", &[]) {
                return result;
            }
        }
        Ok(Some(Value::Int(0)))
    });
    r.register(tp, "getCorePoolSize", "()I", native_tp_get_core_pool_size);
    r.register(
        tp,
        "getMaximumPoolSize",
        "()I",
        native_tp_get_maximum_pool_size,
    );
    r.register(tp, "isShutdown", "()Z", native_tp_is_shutdown);
    r.register(tp, "isTerminated", "()Z", native_tp_is_terminated);
    r.register(
        tp,
        "awaitTermination",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        |ctx, args| {
            let this = tp_arg0(args);
            if let Some(this) = this {
                if let Some(result) = tp_real_call(
                    ctx,
                    this,
                    "awaitTermination",
                    "(JLjava/util/concurrent/TimeUnit;)Z",
                    &args[1..],
                ) {
                    return result;
                }
            }
            Ok(Some(Value::Int(1)))
        },
    );
    r.register(tp, "getTaskCount", "()J", |ctx, args| {
        let this = tp_arg0(args);
        if let Some(this) = this {
            if let Some(result) = tp_real_call(ctx, this, "getTaskCount", "()J", &[]) {
                return result;
            }
        }
        Ok(Some(Value::Long(0)))
    });
    r.register(tp, "getCompletedTaskCount", "()J", |ctx, args| {
        let this = tp_arg0(args);
        if let Some(this) = this {
            if let Some(result) = tp_real_call(ctx, this, "getCompletedTaskCount", "()J", &[]) {
                return result;
            }
        }
        Ok(Some(Value::Long(0)))
    });
    r.register(
        tp,
        "shutdownNow",
        "()Ljava/util/List;",
        native_tp_shutdown_now,
    );

    // DELIBERATELY NOT REGISTERED HERE: `ForkJoinPool.invoke(ForkJoinTask)`,
    // `ForkJoinTask.invoke()`, `RecursiveTask.invoke()`,
    // `RecursiveAction.invoke()`.
    //
    // This module used to register all four. Because the registry is
    // last-write-wins and this registrar runs after
    // `phases_early::register_real_jdk_forkjoin_essentials`, those four
    // registrations OVERWROTE the correct implementations (confirmed via
    // `--dump-native-registry`: `registered_by
    // native-collections/src/lib.rs`, `overwrote bridge`, in BOTH
    // `compatible` and `jdk-only` mode). The copies here were wrong in two
    // independent ways:
    //
    //  1. TYPE-CONFUSED HEAP WRITE. They memoised the task result with
    //     `ctx.set_field(task, 0, val)`. Instance field 0 of a real
    //     `java.util.concurrent.ForkJoinTask` is `volatile int status`
    //     (`javap -p -s`), so every `pool.invoke(task)` stored an object
    //     reference into an int slot. Observable on JDK 25: after
    //     `pool.invoke()` of a `RecursiveTask`, HotSpot reads
    //     `status == -2147483648` (the DONE bit) while CratonVM read back a
    //     raw heap-pointer word.
    //  2. NO `()V` SHAPE. `native_fjp_invoke` invoked
    //     `compute()Ljava/lang/Object;` unconditionally and swallowed the
    //     resulting `NoSuchMethodError`, so every `RecursiveAction` passed
    //     to `pool.invoke()` silently never ran.
    //
    // The `phases_early` implementations dispatch through `fjt_entry_point`,
    // which picks `compute()Ljava/lang/Object;` / `compute()V` / `exec()Z`
    // from the receiver's runtime class, pin the task across the call, and
    // memoise into a GC-remapped side table instead of a guessed field index.
    // Do not re-add invoke natives here.
    r.set_category(__prev_cat);
}

/// [`cowal_ensure_lock_and_array`], carrying one argument `Value` across it
/// and returning it at its current address.
///
/// gc-common w20-a: the lazy initialisation allocates, and the natives that
/// read their argument out of `args` AFTER it (`addIfAbsent`, `contains`) got
/// the argument's pre-collection address — `addIfAbsent` then stored it.
pub(crate) fn cowal_ensure_carrying(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
    carried: Value,
) -> Result<Value, MethodCallFailed> {
    let pin = pin_value(ctx, carried);
    let ensured = cowal_ensure_lock_and_array(ctx, this);
    let carried = read_pinned_elem(ctx, pin, carried);
    if pin != usize::MAX {
        ctx.unpin_native_roots(pin);
    }
    ensured.map(|_| carried)
}

/// The synthetic library's `CopyOnWriteArrayList` mirrors `ArrayList`: slot
/// 0 = backing `Object[]`, slot 1 = `int` size. A receiver with no backing
/// array yet gets an empty one.
///
/// Interpreter round i3 wave 77, lane L1: the real-JDK arms (named `lock` /
/// `array`, including the lazy repair of a `null` `lock` that hid a
/// constructor-less mint HotSpot would NPE on) are gone. The four natives
/// that call this are registered only when `!r.real_jdk()` (wave 76), and the
/// synthetic class declares `_f0` / `_f1` (`class_manager`'s
/// `instance_fields(2)`), so neither name could resolve here; the test
/// mocks resolve neither either (`tests/common`).
pub(crate) fn cowal_ensure_lock_and_array(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
) -> MethodCallResult {
    // GC-safety: `new_array` can complete a moving young GC, and every
    // `set_field` below targets `this` -- a bare Rust local that nothing
    // rewrites. See `rooted_across`.
    if matches!(ctx.get_field(*this, 0), Value::Object(None)) {
        let a = rooted_across(ctx, &mut [this], |ctx| {
            ctx.new_array(ArrayElementType::Reference, 0)
        });
        ctx.set_field(*this, 0, Value::Object(Some(a)));
        ctx.set_field(*this, 1, Value::Int(0));
    }
    Ok(None)
}

/// The synthetic layout's (backing array, size); wave 77 removed the real-JDK
/// `array` arm (see [`cowal_ensure_lock_and_array`]).
pub(crate) fn cowal_read_snapshot(ctx: &dyn NativeContext, this: ObjectRef) -> Option<(ObjectRef, usize)> {
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => return None,
    };
    let sz = match ctx.get_field(this, 1) {
        Value::Int(n) => n.max(0) as usize,
        _ => ctx.array_length(arr),
    };
    Some((arr, sz))
}

// `cowal_bump_mod_count` (the guarded `AbstractList.modCount` bump whose
// unguarded first version stored an `Int` over a real COWAL's `lock`, see
// `docs/feature-designs/jdk-only-mode.md`) was called
// only from the real-JDK `array` arms of `addIfAbsent` / `bulkRemove` /
// `addAll`; it went with them in interpreter round i3 wave 77, lane L1. The
// synthetic layout has no `modCount` (its `AbstractList` declares none).

/// The synthetic `CopyOnWriteArrayList` has no `lock` field: the list's own
/// monitor. Wave 77 (lane L1) removed the real-JDK `lock` arm (see
/// [`cowal_ensure_lock_and_array`]); the `Option` stays so
/// [`cowal_exit_monitor`]'s callers are unchanged.
pub(crate) fn cowal_enter_monitor(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    ctx.monitor_enter(this);
    None
}

pub(crate) fn cowal_exit_monitor(ctx: &mut dyn NativeContext, this: ObjectRef, lock_obj: Option<ObjectRef>) {
    if let Some(lo) = lock_obj {
        ctx.monitor_exit(lo);
    } else {
        ctx.monitor_exit(this);
    }
}

pub(crate) fn native_cowal_add_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // gc-common w20-a: read BEFORE the lazy initialisation (which allocates)
    // and carried across it; read after it, the argument copy was the
    // element's pre-collection address, and it is stored into the array.
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let elem = cowal_ensure_carrying(ctx, &mut this, elem)?;

    let lock_obj = cowal_enter_monitor(ctx, this);
    let Some((arr, len)) = cowal_read_snapshot(ctx, this) else {
        cowal_exit_monitor(ctx, this, lock_obj);
        return Ok(Some(Value::Int(0)));
    };
    // Family-1 stale-at-store fix (cce0079): the dedup scan dispatches
    // `equals()` per element and the copy path allocates a fresh array —
    // `this`, `elem`, the snapshot `arr`, and the monitor object all go
    // stale across those, and the unfixed code then stored a pre-GC `elem`
    // address into the published array (a later reader sees a recycled
    // object). Pin everything, scan through the pinned helper, refresh
    // before every use.
    let this_pin = ctx.pin_native_root(this);
    let lock_val = lock_obj.map_or(Value::Object(None), |o| Value::Object(Some(o)));
    let lock_h = pin_value(ctx, lock_val);
    let (found, arr, elem) = pinned_array_search(ctx, arr, len, elem)?;
    let mut this = ctx.read_native_pin(this_pin, this);
    let mut lock_obj = match read_pinned_elem(ctx, lock_h, lock_val) {
        Value::Object(o) => o,
        _ => None,
    };
    if found.is_some() {
        ctx.unpin_native_roots(this_pin);
        cowal_exit_monitor(ctx, this, lock_obj);
        return Ok(Some(Value::Int(0)));
    }

    let ah = ctx.pin_native_root(arr);
    let eh = pin_value(ctx, elem);
    let new_arr = ctx.new_array(ArrayElementType::Reference, len + 1);
    let arr = ctx.read_native_pin(ah, arr);
    let elem = read_pinned_elem(ctx, eh, elem);
    this = ctx.read_native_pin(this_pin, this);
    lock_obj = match read_pinned_elem(ctx, lock_h, lock_val) {
        Value::Object(o) => o,
        _ => None,
    };
    ctx.unpin_native_roots(this_pin);
    for i in 0..len {
        ctx.set_array_element(new_arr, i, ctx.get_array_element(arr, i));
    }
    ctx.set_array_element(new_arr, len, elem);
    ctx.set_field(this, 0, Value::Object(Some(new_arr)));
    ctx.set_field(this, 1, Value::Int((len + 1) as i32));
    cowal_exit_monitor(ctx, this, lock_obj);
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_cowal_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // gc-common w20-a: carried across the lazy initialisation, as in
    // `native_cowal_add_if_absent`.
    let needle = args.get(1).copied().unwrap_or(Value::Object(None));
    let needle = cowal_ensure_carrying(ctx, &mut this, needle)?;
    // Match by Java `equals` semantics, not reference identity — real
    // `CopyOnWriteArrayList.contains` uses `o.equals(elem)`.
    if let Some((arr, len)) = cowal_read_snapshot(ctx, this) {
        // Family-1 fix (cce0079): pinned scan.
        let (found, _, _) = pinned_array_search(ctx, arr, len, needle)?;
        if found.is_some() {
            return Ok(Some(Value::Int(1)));
        }
    }
    Ok(Some(Value::Int(0)))
}

pub(crate) fn native_cowal_bulk_remove_predicate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let pred = match args.get(1).copied().unwrap_or(Value::Object(None)) {
        Value::Object(Some(p)) => p,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("CopyOnWriteArrayList.bulkRemove predicate is null".to_string()),
            }
            .into());
        }
    };

    // gc-common w20-a: the predicate is carried across the lazy initialisation
    // (it was pinned below at its pre-initialisation address).
    let pred = match cowal_ensure_carrying(ctx, &mut this, Value::Object(Some(pred)))? {
        Value::Object(Some(p)) => p,
        _ => pred,
    };

    let lock_obj = cowal_enter_monitor(ctx, this);

    let exit_mon = |ctx: &mut dyn NativeContext, lock_obj: Option<ObjectRef>, this: ObjectRef| {
        cowal_exit_monitor(ctx, this, lock_obj);
    };

    let Some((arr, n)) = cowal_read_snapshot(ctx, this) else {
        exit_mon(ctx, lock_obj, this);
        return Ok(Some(Value::Int(0)));
    };

    // Family-1 (stale ObjectRef across a Java re-entry). `predicate.test` runs
    // USER code on every turn, so it allocates and can collect: the receiver,
    // the snapshot array, the predicate and the monitor all move under this
    // loop. Three of the four were kept as pre-loop addresses — `arr` was
    // dereferenced again on the next turn, and `exit_mon` released a monitor at
    // an address `MonitorTable::exit` dereferences, which is a fault when the
    // slot was reclaimed and a permanently leaked lock when it merely moved.
    //
    // Survivors are recorded as INDICES rather than as `Value`s for the same
    // reason: a `Vec<Value>` of raw references held across the remaining turns
    // is N more stale addresses, and the elements are already kept alive by the
    // snapshot array itself (which `this` roots).
    let this_pin = ctx.pin_native_root(this);
    let arr_pin = ctx.pin_native_root(arr);
    let lock_pin = lock_obj.map(|lo| ctx.pin_native_root(lo));
    let pred_pin = ctx.pin_native_root(pred);
    let refresh_lock =
        |ctx: &dyn NativeContext, lock_obj: Option<ObjectRef>| -> Option<ObjectRef> {
            match (lock_obj, lock_pin) {
                (Some(lo), Some(h)) => Some(ctx.read_native_pin(h, lo)),
                _ => lock_obj,
            }
        };

    let mut survivors: Vec<usize> = Vec::with_capacity(n);
    for i in 0..n {
        let pred = ctx.read_native_pin(pred_pin, pred);
        let arr = ctx.read_native_pin(arr_pin, arr);
        let elem = ctx.get_array_element(arr, i);
        let remove = matches!(
            ctx.invoke_virtual(pred, "test", "(Ljava/lang/Object;)Z", &[elem]),
            Ok(Some(Value::Int(1)))
        );
        if !remove {
            survivors.push(i);
        }
    }

    let this = ctx.read_native_pin(this_pin, this);
    let lock_obj = refresh_lock(ctx, lock_obj);
    if survivors.len() == n {
        exit_mon(ctx, lock_obj, this);
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Int(0)));
    }

    let new_arr = ctx.new_array(ArrayElementType::Reference, survivors.len());
    let new_pin = ctx.pin_native_root(new_arr);
    for (i, &src) in survivors.iter().enumerate() {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let new_arr = ctx.read_native_pin(new_pin, new_arr);
        let v = ctx.get_array_element(arr, src);
        ctx.set_array_element(new_arr, i, v);
    }

    let this = ctx.read_native_pin(this_pin, this);
    let new_arr = ctx.read_native_pin(new_pin, new_arr);
    ctx.set_field(this, 0, Value::Object(Some(new_arr)));
    ctx.set_field(this, 1, Value::Int(survivors.len() as i32));

    let lock_obj = refresh_lock(ctx, lock_obj);
    exit_mon(ctx, lock_obj, this);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_cowal_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1).copied().unwrap_or(Value::Object(None)) {
        Value::Object(Some(c)) => c,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("CopyOnWriteArrayList.addAll on null collection".to_string()),
            }
            .into());
        }
    };
    // gc-common w20-a: the lazy lock/array initialisation allocates, and the
    // argument collection was then asked for `toArray()` at its pre-allocation
    // address. Rooted across it (the receiver is refreshed through `&mut`).
    let mut coll = coll;
    rooted_across(ctx, &mut [&mut coll], |ctx| {
        cowal_ensure_lock_and_array(ctx, &mut this)
    })?;

    // The synthetic two-field layout: repeated `add`. (Wave 77, lane L1: the
    // real-JDK arm -- one copy under the `lock` monitor into the named
    // `array` -- is gone; these natives are registered only on the synthetic
    // library, where `array` / `lock` never resolve.)
    //
    // gc-common w20-a: the receiver is pinned BEFORE `toArray()` (user
    // code, a GC point); it used to be pinned after it, so every `add`
    // below was dispatched on the receiver's pre-`toArray` address.
    {
        let this_pin = ctx.pin_native_root(this);
        let arr_obj: ObjectRef =
            match ctx.invoke_virtual(coll, "toArray", "()[Ljava/lang/Object;", &[]) {
                Ok(Some(Value::Object(Some(a)))) => a,
                _ => {
                    ctx.unpin_native_roots(this_pin);
                    return Ok(Some(Value::Int(0)));
                }
            };
        let n = ctx.array_length(arr_obj);
        let mut any_added = 0i32;
        // `arr_obj` crosses the same `add` re-entry the receiver does, and the
        // element is read OUT of it on every turn — pinning only `this` left
        // the source array as a pre-call address from the second turn on.
        let arr_pin = ctx.pin_native_root(arr_obj);
        for i in 0..n {
            let this = ctx.read_native_pin(this_pin, this);
            let arr_obj = ctx.read_native_pin(arr_pin, arr_obj);
            let elem = ctx.get_array_element(arr_obj, i);
            if matches!(
                ctx.invoke_virtual(this, "add", "(Ljava/lang/Object;)Z", &[elem]),
                Ok(Some(Value::Int(1)))
            ) {
                any_added = 1;
            }
        }
        ctx.unpin_native_roots(this_pin);
        Ok(Some(Value::Int(any_added)))
    }
}

pub(crate) fn native_stpe_set_keep_alive_time(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Shim compatibility: accept and ignore keep-alive tuning.
    Ok(None)
}

pub(crate) fn native_stpe_get_keep_alive_time(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Return a conservative "disabled" value in all units.
    Ok(Some(Value::Long(0)))
}

pub(crate) fn native_stpe_set_maximum_pool_size(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Compatibility no-op for tuning knobs not modeled by the shim scheduler.
    Ok(None)
}

pub(crate) fn native_stpe_set_core_pool_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let size = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    ctx.set_field(this, STPE_FIELD_POOL_SIZE, Value::Int(size.max(0)));
    Ok(None)
}

pub(crate) fn native_stpe_ignore_policy_setter(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

// --- CompletableFuture CompletionStage implementations ---

pub(crate) fn native_cf_then_run(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let runnable = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return cf_null_callback("thenRun"),
    };
    if cf_is_real_jdk(ctx, this) {
        return ctx.invoke_special(
            "java/util/concurrent/CompletableFuture",
            "uniRunStage",
            "(Ljava/util/concurrent/Executor;Ljava/lang/Runnable;)Ljava/util/concurrent/CompletableFuture;",
            &[
                Value::Object(Some(this)),
                Value::Object(None),
                Value::Object(Some(runnable)),
            ],
        );
    }
    match cf_read_state(ctx, this) {
        CfState::Exceptional(e) => Ok(Some(cf_make_completed(ctx, CfState::Exceptional(e))?)),
        CfState::Pending => Ok(Some(cf_make_synthetic(ctx, CfState::Pending)?)),
        CfState::Normal(_) => match ctx.invoke_virtual(runnable, "run", "()V", &[]) {
            Ok(_) => Ok(Some(cf_make_completed(
                ctx,
                CfState::Normal(Value::Object(None)),
            )?)),
            Err(err) => Ok(Some(cf_callback_failed(ctx, err)?)),
        },
    }
}

pub(crate) fn native_cf_then_compose(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let func = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return cf_null_callback("thenCompose"),
    };
    // BUG-17: real-JDK CF — delegate to the real private `uniComposeStage(null, fn)`
    // (== public `thenCompose(fn)`). The synthetic eager path below reads `result`
    // (slot 0) raw and passes it to the function; for a real CF completed with `null`
    // that field holds the `AltResult(NIL)` sentinel, so the function would receive the
    // internal AltResult object instead of `null` (Spring's reactive-cache miss path
    // then mistakes it for a hit and the future resolves to null). Delegating decodes
    // AltResult→null and registers a proper NON-blocking dependent for async-pending
    // sources. See `native_cf_handle` / `native_cf_when_complete` for the same pattern.
    if cf_is_real_jdk(ctx, this) {
        return ctx.invoke_special(
            "java/util/concurrent/CompletableFuture",
            "uniComposeStage",
            "(Ljava/util/concurrent/Executor;Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
            &[
                Value::Object(Some(this)),
                Value::Object(None),
                Value::Object(Some(func)),
            ],
        );
    }
    match cf_read_state(ctx, this) {
        CfState::Exceptional(e) => Ok(Some(cf_make_completed(ctx, CfState::Exceptional(e))?)),
        CfState::Pending => Ok(Some(cf_make_synthetic(ctx, CfState::Pending)?)),
        CfState::Normal(val) => {
            let result_cf = match ctx.invoke_virtual(
                func,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[val],
            ) {
                Ok(result) => result,
                Err(err) => return Ok(Some(cf_callback_failed(ctx, err)?)),
            };
            // The result should be a CompletableFuture; return it directly.
            match result_cf {
                Some(Value::Object(Some(cf_obj))) => Ok(Some(Value::Object(Some(cf_obj)))),
                other => Ok(Some(cf_make_completed(
                    ctx,
                    CfState::Normal(other.unwrap_or(Value::Object(None))),
                )?)),
            }
        }
    }
}

pub(crate) fn native_cf_then_combine(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other_cf = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("CompletableFuture.thenCombine other stage is null".to_string()),
            }
            .into())
        }
    };
    let bi_func = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return cf_null_callback("thenCombine"),
    };
    if cf_is_real_jdk(ctx, this) {
        return ctx.invoke_special(
            "java/util/concurrent/CompletableFuture",
            "biApplyStage",
            "(Ljava/util/concurrent/Executor;Ljava/util/concurrent/CompletionStage;Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;",
            &[
                Value::Object(Some(this)),
                Value::Object(None),
                Value::Object(Some(other_cf)),
                Value::Object(Some(bi_func)),
            ],
        );
    }
    let left = cf_read_state(ctx, this);
    let right = cf_read_state(ctx, other_cf);
    match (left, right) {
        (CfState::Exceptional(e), _) | (_, CfState::Exceptional(e)) => {
            Ok(Some(cf_make_completed(ctx, CfState::Exceptional(e))?))
        }
        (CfState::Pending, _) | (_, CfState::Pending) => {
            Ok(Some(cf_make_synthetic(ctx, CfState::Pending)?))
        }
        (CfState::Normal(val1), CfState::Normal(val2)) => {
            let result = match ctx.invoke_virtual(
                bi_func,
                "apply",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[val1, val2],
            ) {
                Ok(result) => result,
                Err(err) => return Ok(Some(cf_callback_failed(ctx, err)?)),
            };
            Ok(Some(cf_make_completed(
                ctx,
                CfState::Normal(result.unwrap_or(Value::Object(None))),
            )?))
        }
    }
}

pub(crate) fn native_cf_exceptionally(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let handler = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return cf_null_callback("exceptionally"),
    };
    if cf_is_real_jdk(ctx, this) {
        return ctx.invoke_special(
            "java/util/concurrent/CompletableFuture",
            "uniExceptionallyStage",
            "(Ljava/util/concurrent/Executor;Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
            &[
                Value::Object(Some(this)),
                Value::Object(None),
                Value::Object(Some(handler)),
            ],
        );
    }
    match cf_read_state(ctx, this) {
        CfState::Exceptional(exc) => {
            let result = match ctx.invoke_virtual(
                handler,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[exc],
            ) {
                Ok(result) => result,
                Err(err) => return Ok(Some(cf_callback_failed(ctx, err)?)),
            };
            Ok(Some(cf_make_completed(
                ctx,
                CfState::Normal(result.unwrap_or(Value::Object(None))),
            )?))
        }
        CfState::Pending => Ok(Some(cf_make_synthetic(ctx, CfState::Pending)?)),
        CfState::Normal(v) => Ok(Some(cf_make_completed(ctx, CfState::Normal(v))?)),
    }
}

#[allow(dead_code)]
pub(crate) fn native_cf_exceptionally_legacy(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let handler = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            // No handler — mirror the source completion.
            let st = cf_read_state(ctx, this);
            return Ok(Some(cf_make_completed(ctx, st)?));
        }
    };
    match cf_read_state(ctx, this) {
        CfState::Exceptional(exc) => {
            // Recover: apply the handler to the throwable; result = normal value.
            let result = ctx.invoke_virtual(
                handler,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[exc],
            )?;
            Ok(Some(cf_make_completed(
                ctx,
                CfState::Normal(result.unwrap_or(Value::Object(None))),
            )?))
        }
        // Normal (or still pending → treat as its current value) → pass through.
        state => Ok(Some(cf_make_completed(ctx, state)?)),
    }
}

pub(crate) fn native_cf_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let bi_func = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return cf_null_callback("handle"),
    };
    // BUG-17: a real-JDK CF may still be asynchronously PENDING here (e.g. supplyAsync
    // on a worker; reactor's `Mono.fromFuture` subscribes via `handle()`). Delegate to
    // the real, non-overridden private `uniHandleStage(executor=null, fn)` — exactly
    // what public `handle(fn)` does — so a proper NON-blocking dependent registers and
    // fires on completion with the real value. The synthetic eager path below would
    // fire `apply(null,null)` immediately → `.block()`/coroutine-await returns null
    // before the value exists → sync-cache miss → `cache["tb1"]!!` NPE. (Blocking here
    // is NOT viable: it starves the coroutine dispatcher that completes the producer.)
    if cf_is_real_jdk(ctx, this) {
        return ctx.invoke_special(
            "java/util/concurrent/CompletableFuture",
            "uniHandleStage",
            "(Ljava/util/concurrent/Executor;Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;",
            &[
                Value::Object(Some(this)),
                Value::Object(None),
                Value::Object(Some(bi_func)),
            ],
        );
    }
    // Synthetic CF: handle(BiFunction(result, exception)) — pass the (value, exception).
    let (val, exc) = match cf_read_state(ctx, this) {
        CfState::Normal(v) => (v, Value::Object(None)),
        CfState::Exceptional(e) => (Value::Object(None), e),
        CfState::Pending => return Ok(Some(cf_make_synthetic(ctx, CfState::Pending)?)),
    };
    let result = match ctx.invoke_virtual(
        bi_func,
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[val, exc],
    ) {
        Ok(result) => result,
        Err(err) => return Ok(Some(cf_callback_failed(ctx, err)?)),
    };
    // handle always produces a normal completion with the function's result.
    Ok(Some(cf_make_completed(
        ctx,
        CfState::Normal(result.unwrap_or(Value::Object(None))),
    )?))
}

pub(crate) fn native_cf_when_complete(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return cf_null_callback("whenComplete"),
    };
    // BUG-17: real-JDK CF — delegate to the real private `uniWhenCompleteStage(null, c)`
    // (== public `whenComplete(c)`) so the dependent registers NON-blockingly and fires
    // on async completion with the real value, instead of the synthetic eager
    // `accept(null,null)` below. See `native_cf_handle` for the full rationale.
    if cf_is_real_jdk(ctx, this) {
        return ctx.invoke_special(
            "java/util/concurrent/CompletableFuture",
            "uniWhenCompleteStage",
            "(Ljava/util/concurrent/Executor;Ljava/util/function/BiConsumer;)Ljava/util/concurrent/CompletableFuture;",
            &[
                Value::Object(Some(this)),
                Value::Object(None),
                Value::Object(Some(consumer)),
            ],
        );
    }
    // Synthetic CF: pass the real (result, exception) pair to the BiConsumer. Reading the
    // source state layout-agnostically is what makes KafkaFuture.all()/allOf complete its
    // dependent KafkaFutureImpl exceptionally instead of leaving it pending (which
    // made get() block forever — bug-08). The returned CF mirrors the source.
    let (val, exc, source_exceptional) = match cf_read_state(ctx, this) {
        CfState::Normal(v) => (v, Value::Object(None), false),
        CfState::Exceptional(e) => (Value::Object(None), e, true),
        CfState::Pending => return Ok(Some(cf_make_synthetic(ctx, CfState::Pending)?)),
    };
    if let Err(err) = ctx.invoke_virtual(
        consumer,
        "accept",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        &[val.clone(), exc.clone()],
    ) {
        if source_exceptional {
            return Ok(Some(cf_make_completed(ctx, CfState::Exceptional(exc))?));
        }
        return Ok(Some(cf_callback_failed(ctx, err)?));
    }
    let mirror = if matches!(exc, Value::Object(None)) {
        CfState::Normal(val)
    } else {
        CfState::Exceptional(exc)
    };
    Ok(Some(cf_make_completed(ctx, mirror)?))
}

pub(crate) fn native_cf_all_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Real-JDK async inputs (e.g. `CompletableFuture.runAsync` on a worker thread)
    // may still be PENDING when `allOf` is called. The synthetic "all inputs already
    // completed" model below would then return an already-completed CF<Void>, so
    // `allOf(...).join()` returns immediately while the tasks are still running — the
    // root cause of the Spring SyncTaskExecutor concurrency-limit test failures
    // (`allOf(futures).join()` returned in ~4ms with 0 futures done). Delegate to the
    // real JDK private static `andTree` (== the body of the real `allOf`) so the
    // returned CF completes only when EVERY input does. Mirrors the BUG-17 real-JDK
    // delegation in `whenComplete`/`handle`. Synthetic (KafkaFuture-style) inputs,
    // whose completion state is encoded as a `DONE` Int in slot 1, keep the eager
    // model below.
    if let Some(Value::Object(Some(arr))) = args.first() {
        let arr = *arr;
        let len = ctx.array_length(arr);
        let mut any_real = false;
        for i in 0..len {
            if let Value::Object(Some(cf_obj)) = ctx.get_array_element(arr, i) {
                if cf_is_real_jdk(ctx, cf_obj) {
                    any_real = true;
                    break;
                }
            }
        }
        if any_real {
            return ctx.invoke_special(
                "java/util/concurrent/CompletableFuture",
                "andTree",
                "([Ljava/util/concurrent/CompletableFuture;II)Ljava/util/concurrent/CompletableFuture;",
                &[
                    Value::Object(Some(arr)),
                    Value::Int(0),
                    Value::Int(len as i32 - 1),
                ],
            );
        }
    }
    // Synthetic inputs still must not make allOf complete eagerly while any source
    // remains pending. Otherwise allOf(...).join() can observe completion before
    // the modeled source future has reached a terminal state.
    let mut exc: Option<Value> = None;
    let mut pending = false;
    if let Some(Value::Object(Some(arr))) = args.first() {
        let arr = *arr;
        let len = ctx.array_length(arr);
        for i in 0..len {
            if let Value::Object(Some(cf_obj)) = ctx.get_array_element(arr, i) {
                match cf_read_state(ctx, cf_obj) {
                    CfState::Exceptional(e) => {
                        exc = Some(e);
                        break;
                    }
                    CfState::Pending => pending = true,
                    CfState::Normal(_) => {}
                }
            }
        }
    }
    match exc {
        Some(e) => Ok(Some(cf_make_completed(ctx, CfState::Exceptional(e))?)),
        None if pending => Ok(Some(cf_make_synthetic(ctx, CfState::Pending)?)),
        None => Ok(Some(cf_make_completed(
            ctx,
            CfState::Normal(Value::Object(None)),
        )?)),
    }
}

pub(crate) fn native_cf_any_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(cf_make_synthetic(ctx, CfState::Pending)?)),
    };
    let len = ctx.array_length(arr);

    let mut any_real = false;
    for i in 0..len {
        if let Value::Object(Some(cf_obj)) = ctx.get_array_element(arr, i) {
            if cf_is_real_jdk(ctx, cf_obj) {
                any_real = true;
                break;
            }
        }
    }
    if any_real {
        return ctx.invoke_special(
            "java/util/concurrent/CompletableFuture",
            "orTree",
            "([Ljava/util/concurrent/CompletableFuture;II)Ljava/util/concurrent/CompletableFuture;",
            &[
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Int(len as i32 - 1),
            ],
        );
    }

    for i in 0..len {
        if let Value::Object(Some(cf_obj)) = ctx.get_array_element(arr, i) {
            match cf_read_state(ctx, cf_obj) {
                CfState::Pending => {}
                CfState::Normal(v) => return Ok(Some(cf_make_completed(ctx, CfState::Normal(v))?)),
                CfState::Exceptional(e) => {
                    return Ok(Some(cf_make_completed(ctx, CfState::Exceptional(e))?))
                }
            }
        }
    }
    Ok(Some(cf_make_synthetic(ctx, CfState::Pending)?))
}

pub(crate) const CF_ALT_RESULT_CLASS: &str = "java/util/concurrent/CompletableFuture$AltResult";

/// Read the real static `CompletableFuture.NIL` sentinel (`new AltResult(null)`),
/// used to represent a completion whose value is `null`. Returns null only if the
/// field can't be resolved (degraded — leaves the future pending).
pub(crate) fn cf_nil(ctx: &mut dyn NativeContext) -> Value {
    if let Ok(cid) = ctx.ensure_class_initialized("java/util/concurrent/CompletableFuture") {
        if let Some(idx) = ctx.static_field_index_by_name(cid, "NIL") {
            return ctx.get_static_field(cid, idx);
        }
    }
    Value::Object(None)
}

/// True if `this` is a real-JDK `CompletableFuture` (or an app subclass such as
/// `KafkaCompletableFuture`) rather than a CratonVM 4-slot *synthetic* CF. A synthetic
/// CF encodes its completion state as a `DONE` Int in slot 1; a real CF's slot 1 is the
/// `volatile Completion stack` reference (null, or a Completion object) — never an Int.
///
/// BUG-17: real-JDK dependent-stage methods (`handle`, `whenComplete`, …) must NOT use
/// the synthetic eager model, which fires the callback immediately with `(null, null)`
/// while the source is still asynchronously pending (e.g. `supplyAsync` on a worker —
/// reactor's `Mono.fromFuture` subscribes via `handle()`), so `.block()` / a coroutine
/// await returns null before the value is produced → `@Cacheable(sync=true)` cache miss.
pub(crate) fn cf_is_real_jdk(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    !matches!(ctx.get_field(this, CF_FIELD_DONE), Value::Int(_))
}

/// True if `obj`'s runtime class is `CompletableFuture$AltResult`.
pub(crate) fn cf_is_alt_result(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(obj);
    matches!(
        ctx.class_name_arc_of_id(cid).as_deref(),
        Some(CF_ALT_RESULT_CLASS)
    )
}

/// Completion state of a CompletableFuture, read in a layout-agnostic way so the
/// chaining natives behave correctly on BOTH CratonVM-synthetic 4-slot CFs (state
/// encoded as a `DONE` Int in slot 1) and real JDK CFs / app subclasses such as
/// `KafkaCompletableFuture` (state encoded in `result` (slot 0): null = pending,
/// `AltResult(ex)` = exceptional, any other value = normal).
pub(crate) enum CfState {
    Pending,
    Normal(Value),
    Exceptional(Value),
}

pub(crate) fn cf_read_state(ctx: &dyn NativeContext, this: ObjectRef) -> CfState {
    let r = ctx.get_field(this, CF_FIELD_RESULT);
    // Real encoding: result is an AltResult (exceptional, or NIL = normal null).
    if let Value::Object(Some(ro)) = r {
        if cf_is_alt_result(ctx, ro) {
            let ex = ctx.get_field(ro, 0);
            return if matches!(ex, Value::Object(None)) {
                CfState::Normal(Value::Object(None))
            } else {
                CfState::Exceptional(ex)
            };
        }
    }
    // Synthetic encoding: DONE marker (Int) in slot 1.
    match ctx.get_field(this, CF_FIELD_DONE) {
        Value::Int(2) | Value::Int(3) => CfState::Exceptional(r),
        Value::Int(1) => CfState::Normal(r),
        Value::Int(_) => CfState::Pending, // 0 = pending, -1 = deferred
        // slot 1 is a reference (the real `stack` field) → real layout: done iff
        // result is non-null (the AltResult/exceptional case was handled above).
        _ => {
            if matches!(r, Value::Object(None)) {
                CfState::Pending
            } else {
                CfState::Normal(r)
            }
        }
    }
}

/// Allocate a CratonVM *synthetic* `CompletableFuture`.
///
/// Every live caller passes [`CfState::Pending`]; the completed states go
/// through [`cf_make_completed`], which builds a genuine JDK object. The width
/// is [`CF_NUM_FIELDS`] — the real declared width — not the four slots this
/// used to ask for: `CF_FIELD_SOURCE`/`CF_FIELD_HANDLER` were declared and then
/// never read or written anywhere in the workspace, so slots 2 and 3 sat past
/// everything `java.util.concurrent.CompletableFuture` declares with no reader.
/// W7-66-live-over-allocations.md.
/// One implementation, not two: this and [`cf_make_completed`]'s last-resort
/// arm both go through [`cf_make_synthetic_slots`], so the width and the slot
/// map cannot drift apart between them.
pub(crate) fn cf_make_synthetic(
    ctx: &mut dyn NativeContext,
    state: CfState,
) -> Result<Value, MethodCallFailed> {
    cf_make_synthetic_slots(ctx, state)
}

pub(crate) fn cf_null_callback(method: &str) -> MethodCallResult {
    Err(RuntimeError::NullPointerException {
        message: Some(format!("CompletableFuture.{method} callback is null")),
    }
    .into())
}

pub(crate) fn cf_throwable_for_failure(ctx: &mut dyn NativeContext, err: MethodCallFailed) -> Value {
    match err {
        MethodCallFailed::ExceptionThrown(obj) => Value::Object(Some(obj)),
        MethodCallFailed::InternalError(vm_err) => {
            let class_name = match &vm_err {
                cratonvm_types::error::VmError::Runtime(RuntimeError::NullPointerException {
                    ..
                }) => "java/lang/NullPointerException",
                cratonvm_types::error::VmError::Runtime(
                    RuntimeError::IllegalArgumentException { .. },
                ) => "java/lang/IllegalArgumentException",
                cratonvm_types::error::VmError::Runtime(RuntimeError::IllegalStateException {
                    ..
                }) => "java/lang/IllegalStateException",
                cratonvm_types::error::VmError::Runtime(RuntimeError::ClassCastException {
                    ..
                }) => "java/lang/ClassCastException",
                _ => "java/lang/RuntimeException",
            };
            let msg = ctx.create_string(&vm_err.to_string());
            match ctx.new_object_initialized(
                class_name,
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(msg))],
            ) {
                Ok(Some(Value::Object(Some(obj)))) => Value::Object(Some(obj)),
                _ => Value::Object(Some(msg)),
            }
        }
    }
}

pub(crate) fn cf_callback_failed(
    ctx: &mut dyn NativeContext,
    err: MethodCallFailed,
) -> Result<Value, MethodCallFailed> {
    let throwable = cf_throwable_for_failure(ctx, err);
    Ok(cf_make_completed(ctx, CfState::Exceptional(throwable))?)
}

/// Allocate a REAL `java.util.concurrent.CompletableFuture` already completed with
/// `state`, using the un-intercepted `complete`/`obtrudeException` bytecode (which
/// store genuine values/`AltResult`s) so the result is fully compatible with the
/// real `get()`/`join()`/`isDone()` bytecode. Falls back to a synthetic CF only if
/// the real object cannot be constructed.
pub(crate) fn cf_make_completed(
    ctx: &mut dyn NativeContext,
    state: CfState,
) -> Result<Value, MethodCallFailed> {
    match ctx.new_object_initialized("java/util/concurrent/CompletableFuture", "()V", &[]) {
        Ok(Some(Value::Object(Some(cf)))) => {
            match state {
                CfState::Normal(v) => {
                    let _ = ctx.invoke_virtual(cf, "complete", "(Ljava/lang/Object;)Z", &[v]);
                }
                CfState::Exceptional(e) => {
                    let _ = ctx.invoke_virtual(
                        cf,
                        "obtrudeException",
                        "(Ljava/lang/Throwable;)V",
                        &[e],
                    );
                }
                CfState::Pending => {}
            }
            Ok(Value::Object(Some(cf)))
        }
        // The real object could not be constructed. Try the JDK's own static
        // factory before falling back to a slot-written stand-in — this is the
        // remedy W7-49 §6.2 landed as `aio_completed_future` in
        // `native-builtins::http_client`, and it is a different door from
        // `new_object_initialized` above (a static method, no `<init>`
        // interception), so it can succeed where that failed. It writes no
        // index at all: the layout is whatever the loaded class actually is.
        _ => match state {
            CfState::Normal(v) => {
                match ctx.invoke(
                    "java/util/concurrent/CompletableFuture",
                    "completedFuture",
                    "(Ljava/lang/Object;)Ljava/util/concurrent/CompletableFuture;",
                    &[v],
                ) {
                    Ok(Some(real @ Value::Object(Some(_)))) => Ok(real),
                    // The factory is unavailable too (synthetic-JDK mode has no
                    // such method), or answered null. Re-use the same value in
                    // the stand-in rather than losing it.
                    _ => cf_make_synthetic_slots(ctx, CfState::Normal(v)),
                }
            }
            other => cf_make_synthetic_slots(ctx, other),
        },
    }
}

/// The slot-written `CompletableFuture` stand-in, at the real declared width.
/// Last resort only — both [`cf_make_completed`] doors are tried first.
pub(crate) fn cf_make_synthetic_slots(
    ctx: &mut dyn NativeContext,
    state: CfState,
) -> Result<Value, MethodCallFailed> {
    let cf = try_alloc_synthetic(ctx, "java/util/concurrent/CompletableFuture", CF_NUM_FIELDS)?;
    match state {
        CfState::Normal(v) => {
            ctx.set_field(cf, CF_FIELD_RESULT, v);
            ctx.set_field(cf, CF_FIELD_DONE, Value::Int(1));
        }
        CfState::Exceptional(e) => {
            ctx.set_field(cf, CF_FIELD_RESULT, e);
            ctx.set_field(cf, CF_FIELD_DONE, Value::Int(2));
        }
        CfState::Pending => {
            ctx.set_field(cf, CF_FIELD_DONE, Value::Int(0));
        }
    }
    Ok(Value::Object(Some(cf)))
}

pub(crate) fn native_cf_complete(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let value = args.get(1).cloned().unwrap_or(Value::Object(None));

    // Already completed? (real: result != null; synthetic: DONE marker Int != 0)
    let result = ctx.get_field(this, CF_FIELD_RESULT);
    let synth_done = matches!(ctx.get_field(this, CF_FIELD_DONE), Value::Int(d) if d != 0);
    if !matches!(result, Value::Object(None)) || synth_done {
        return Ok(Some(Value::Int(0)));
    }

    // Store the value directly. For a NULL value we must store the JDK's `NIL`
    // sentinel — a real `AltResult(null)` — so the un-intercepted `isDone()`/`get()`
    // bytecode sees `result != null` (done, value = null) instead of leaving the
    // future pending. A non-null value is stored as-is.
    //
    // gc-common w20-a: `cf_nil` asks `ensure_class_initialized` (a GC point on
    // a first use), so the receiver is rooted across it.
    let (this, stored) = if matches!(value, Value::Object(None)) {
        rooted_across1(ctx, this, |ctx| cf_nil(ctx))
    } else {
        (this, value)
    };
    // Distinguish a synthetic CF (slot 1 is the `done` marker, an Int) from a
    // real-JDK `java.util.concurrent.CompletableFuture` (slot 1 is the lock-free
    // `stack` reference field — null or a `Completion`/`Signaller` chain). The
    // object's field COUNT is NOT a reliable discriminator (a real-JDK CF is
    // allocated with ≥4 slots here), but slot 1's value type is: a synthetic CF
    // always carries an Int `done` flag there, whereas the real `stack` field is
    // always a reference. This is the same Int-vs-reference test the already-done
    // check above (`synth_done`) relies on.
    //
    // The COUNT is still not a discriminator, and the reason changed: a
    // synthetic CF is now allocated at the real declared width (two slots), so
    // it is exactly as wide as a real one. A real SUBCLASS is wider than both.
    // Int-vs-reference at slot 1 is the only test that stays correct across all
    // three. W7-66-live-over-allocations.md.
    let synthetic = matches!(ctx.get_field(this, CF_FIELD_DONE), Value::Int(_));
    if synthetic {
        ctx.set_field(this, CF_FIELD_RESULT, stored);
        // Synthetic CF: keep the DONE marker coherent. These have no lock-free
        // `stack` of waiters, so there is nothing to unpark.
        ctx.set_field(this, CF_FIELD_DONE, Value::Int(1));
    } else {
        // nio20 wave 5 (L1): the JDK's `completeValue` is
        // `RESULT.compareAndSet(this, null, v)`, so of two racing completers
        // exactly one answers `true`. A plain read-then-store let both win
        // (the `java.net.http` client completes its response futures from the
        // selector thread while a timeout or cancel may complete them from
        // another) and was not ordered against a concurrent `unipush` /
        // `waitingGet`, which CASes `stack` and then re-reads `result`: with
        // a plain store a waiter could push after the `stack` read below,
        // miss this `result`, and park forever. The CAS (SeqCst) and the
        // volatile `stack` read below are the JDK's Dekker pair.
        if !ctx.compare_and_swap_field(this, CF_FIELD_RESULT, Value::Object(None), stored) {
            return Ok(Some(Value::Int(0)));
        }
        // Real-JDK CompletableFuture. A thread blocked in the genuine
        // `waitingGet()` (untimed `get()`/`join()`) pushes a `Signaller` onto
        // `stack`@1 and parks via `LockSupport.park`. Storing `result` alone does
        // NOT wake it — the real `complete()` runs `postComplete()`, which pops
        // the `stack` and fires each `Signaller` (`LockSupport.unpark` of the
        // waiting thread). Without this the parked thread hangs forever on every
        // cross-thread `complete()` (the timed `get(...)` only "self-heals"
        // because `parkNanos` re-polls `result`). Run `postComplete()` to release
        // waiters — this is the missing half of the synthetic override.
        //
        // ...but only when there is a stack to pop. `postComplete()`'s whole
        // body is `while ((h = f.stack) != null || (f != this && (h = (f =
        // this).stack) != null))`, so with `stack`@1 null it returns having
        // done nothing — and this callback is not cheap: it is a by-NAME
        // virtual dispatch out of a native, which resolves through
        // `invoke_or_native` (two registry probes plus the descriptor-quirk
        // scan, all of them misses — `postComplete` is not a native) and then
        // through `invoke_on_class_shared_inner`. The composition page's
        // "name-keyed lookup, 8.2 %" bucket is that chain: 100 005 missed
        // `CompletableFuture.postComplete()V` registry lookups in 40 000
        // chains, 76 % of every miss on the workload.
        //
        // A `complete()` with no dependents and no waiter is the common shape
        // outside a composition benchmark; there the whole chain now costs one
        // field read. `CRATONVM_NATIVE_CF_POSTCOMPLETE_SKIP=0` restores the
        // unconditional callback so the two arms can be priced in one binary.
        let no_waiters = matches!(
            ctx.get_field_volatile(this, CF_FIELD_DONE),
            Value::Object(None)
        ) && cf_postcomplete_skip_enabled();
        if !no_waiters {
            if cf_postcomplete_direct_enabled() {
                ctx.invoke_virtual_bytecode_only(this, "postComplete", "()V", &[])?;
            } else {
                ctx.invoke_virtual(this, "postComplete", "()V", &[])?;
            }
        }
    }
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_cf_complete_exceptionally(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let exc = args.get(1).cloned().unwrap_or(Value::Object(None));

    // nio20 wave 5 (L1): a REAL-JDK CompletableFuture (slot 1 is the `stack`
    // reference, not the synthetic Int `done`) runs the JDK's own
    // `completeExceptionally` body: `internalComplete(new AltResult(ex))`, a
    // `RESULT` CAS, then `postComplete()`. The arm below checked `result`
    // and then `obtrudeException`ed, an UNCONDITIONAL store: a normal
    // `complete(v)` landing between the check and the store was overwritten
    // by the exception (HotSpot never replaces a completion), and both racers
    // answered `true`. The `java.net.http` client completes its exchange
    // futures from the selector thread while a timeout / cancel completes
    // them exceptionally from another. The body also throws the JDK's
    // `NullPointerException` for a null `ex` on an already-completed future,
    // where the check answered `false`. `invoke_special_bytecode_only` names
    // the base class, so a subclass override (`KafkaCompletableFuture`) is
    // not re-entered, as the `obtrudeException` call below arranged.
    if !matches!(ctx.get_field(this, CF_FIELD_DONE), Value::Int(_)) {
        return ctx.invoke_special_bytecode_only(
            "java/util/concurrent/CompletableFuture",
            "completeExceptionally",
            "(Ljava/lang/Throwable;)Z",
            &[Value::Object(Some(this)), exc],
        );
    }

    // Already completed? `result` (slot 0) non-null means a real completion
    // (a value or an AltResult); a synthetic `DONE` marker (Int in slot 1) means
    // a CratonVM-chained CF already completed. Either way, return false.
    let result = ctx.get_field(this, 0);
    let synth_done = matches!(ctx.get_field(this, CF_FIELD_DONE), Value::Int(d) if d != 0);
    if !matches!(result, Value::Object(None)) || synth_done {
        return Ok(Some(Value::Int(0)));
    }

    // For a REAL-JDK CompletableFuture (the branch below the synthetic check),
    // drive the real (un-intercepted) `obtrudeException` bytecode, which stores a
    // genuine `CompletableFuture$AltResult(ex)` in `result` — exactly the way real
    // `complete()` (which works on CratonVM) stores values/NIL. This keeps the
    // un-intercepted `get()`/`join()`/`isDone()` bytecode and our
    // `isCompletedExceptionally` native consistent, and works regardless of the
    // object's slot layout (real `new CompletableFuture()` objects are allocated
    // with 4 slots, so a field-count layout check cannot distinguish them; the old
    // code wrote an Int `DONE` marker into the real `stack` *reference* field where
    // it was dropped, and stored the raw throwable in `result` so `get()` returned
    // it instead of throwing — bug-08).
    // Use `invoke_special` to call the *base* `CompletableFuture.obtrudeException`
    // exactly, bypassing any subclass override. `KafkaCompletableFuture` overrides
    // `obtrudeException` (and `complete`/`completeExceptionally`) to throw
    // "User code should not complete futures returned from Kafka clients"; a virtual
    // call would hit that override. The internal kafka path
    // (`kafkaCompleteExceptionally` -> `super.completeExceptionally` == this native)
    // legitimately needs the base behaviour.
    let synthetic = matches!(ctx.get_field(this, CF_FIELD_DONE), Value::Int(_));
    if synthetic {
        // Synthetic (CratonVM-minted) CompletableFuture: its slots are
        // positional and its class carries no named fields, so the real
        // `obtrudeException` bytecode — which stores `new AltResult(ex)` into
        // the field *named* `result` — has nothing to bind to. Worse, driving
        // it requires `java.util.concurrent.CompletableFuture` bytecode to be
        // loaded at all; where it is not, the invoke fails and this native
        // returns an exception instead of completing the future.
        //
        // Write the synthetic encoding directly instead: result@0 = the raw
        // Throwable, done@1 = 2 (exceptionally completed). That is exactly what
        // `cf_make_synthetic(CfState::Exceptional)?` produces and what
        // `cf_read_state` / `isCompletedExceptionally` read back, and it makes
        // this native symmetric with its sibling `native_cf_complete`, which
        // likewise stores into the slots for a synthetic CF and only reaches
        // for real bytecode (`postComplete`) on the real-JDK layout. Synthetic
        // CFs have no lock-free `stack` of parked `Signaller`s, so there is
        // nothing to unpark.
        ctx.set_field(this, CF_FIELD_RESULT, exc);
        ctx.set_field(this, CF_FIELD_DONE, Value::Int(2));
        return Ok(Some(Value::Int(1)));
    }
    // gc-common w20-a: `obtrudeException` runs Java (it allocates the
    // `AltResult`), and `postComplete` below was dispatched on the receiver's
    // pre-call address. Rooted across it.
    let (this, obtruded) = rooted_across1(ctx, this, |ctx| {
        ctx.invoke_special(
            "java/util/concurrent/CompletableFuture",
            "obtrudeException",
            "(Ljava/lang/Throwable;)V",
            &[Value::Object(Some(this)), exc],
        )
    });
    obtruded?;
    // Real-JDK CompletableFuture: `obtrudeException` only stores the result;
    // it does NOT fire the lock-free `stack`@1 of parked `Signaller`s. The
    // real `completeExceptionally()` runs `postComplete()` to unpark threads
    // blocked in untimed `get()`/`join()`. Without this they hang forever on a
    // cross-thread exceptional completion — the same defect fixed in
    // `native_cf_complete`.
    //
    // `invoke_virtual_bytecode_only`, not `invoke_virtual`, for the reason the
    // sibling in `native_cf_complete` states and measures: `postComplete` is
    // ordinary JDK bytecode and can never be a native, so the by-NAME resolver
    // is two hashes of the 53-byte triple plus the cold descriptor-quirk
    // rewrite, all of them misses, per completion.
    ctx.invoke_virtual_bytecode_only(this, "postComplete", "()V", &[])?;
    Ok(Some(Value::Int(1)))
}

pub(crate) fn native_cf_then_apply_p31(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let func = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return cf_null_callback("thenApply"),
    };
    if cf_is_real_jdk(ctx, this) {
        return ctx.invoke_special(
            "java/util/concurrent/CompletableFuture",
            "uniApplyStage",
            "(Ljava/util/concurrent/Executor;Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
            &[
                Value::Object(Some(this)),
                Value::Object(None),
                Value::Object(Some(func)),
            ],
        );
    }
    // thenApply: if the source completed exceptionally, propagate the exception
    // WITHOUT calling the function (real CompletionStage semantics); otherwise
    // apply the function to the source value.
    match cf_read_state(ctx, this) {
        CfState::Exceptional(e) => Ok(Some(cf_make_completed(ctx, CfState::Exceptional(e))?)),
        CfState::Pending => Ok(Some(cf_make_synthetic(ctx, CfState::Pending)?)),
        state => {
            let val = match state {
                CfState::Normal(v) => v,
                CfState::Exceptional(_) => unreachable!(),
                CfState::Pending => unreachable!(),
            };
            let result = match ctx.invoke_virtual(
                func,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[val],
            ) {
                Ok(result) => result,
                Err(err) => return Ok(Some(cf_callback_failed(ctx, err)?)),
            };
            Ok(Some(cf_make_completed(
                ctx,
                CfState::Normal(result.unwrap_or(Value::Object(None))),
            )?))
        }
    }
}

pub(crate) fn native_cf_then_accept_p31(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return cf_null_callback("thenAccept"),
    };
    if cf_is_real_jdk(ctx, this) {
        return ctx.invoke_special(
            "java/util/concurrent/CompletableFuture",
            "uniAcceptStage",
            "(Ljava/util/concurrent/Executor;Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletableFuture;",
            &[
                Value::Object(Some(this)),
                Value::Object(None),
                Value::Object(Some(consumer)),
            ],
        );
    }
    match cf_read_state(ctx, this) {
        CfState::Exceptional(e) => Ok(Some(cf_make_completed(ctx, CfState::Exceptional(e))?)),
        CfState::Pending => Ok(Some(cf_make_synthetic(ctx, CfState::Pending)?)),
        CfState::Normal(val) => {
            if let Err(err) =
                ctx.invoke_virtual(consumer, "accept", "(Ljava/lang/Object;)V", &[val])
            {
                return Ok(Some(cf_callback_failed(ctx, err)?));
            }
            Ok(Some(cf_make_completed(
                ctx,
                CfState::Normal(Value::Object(None)),
            )?))
        }
    }
}

// --- ThreadPoolExecutor stat methods ---

// ThreadPoolExecutor uses 2-field layout: field 0 = pool size, field 1 = shutdown flag
pub(crate) const TP_FIELD_SIZE: usize = 0;
pub(crate) const TP_FIELD_SHUTDOWN: usize = 1;

pub(crate) fn tp_arg0(args: &[Value]) -> Option<ObjectRef> {
    match args.first() {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

/// True if `exec` (or its `e`-delegate target) is a real bytecode-constructed
/// `ThreadPoolExecutor` -- i.e. has a real `workers` field -- rather than
/// CratonVM's synthetic 2-field executor placeholder. Mirrors
/// `native-builtins::executor_has_real_workers` (kept separate to avoid a
/// crate dependency cycle).
/// A real receiver's `getCorePoolSize`/`getMaximumPoolSize`/`isShutdown`/
/// `shutdownNow`/etc. must run the REAL bytecode instead of reading the
/// synthetic 2-field slots -- those slots don't exist on a real object's
/// layout (field 0/1 are whatever the real class happens to declare first),
/// so reading them either returns garbage or silently falls through to the
/// hardcoded default, masking real mutations like `setCorePoolSize()`.
pub(crate) fn tp_is_real(ctx: &mut dyn NativeContext, exec: ObjectRef) -> bool {
    let tpe = match ctx.get_field_by_name(exec, "e") {
        Value::Object(Some(inner)) => inner,
        _ => exec,
    };
    matches!(
        ctx.get_field_by_name(tpe, "workers"),
        Value::Object(Some(_))
    )
}

/// Is `exec` ITSELF a real pool: its own `workers` set, and a class not known
/// to be outside `ThreadPoolExecutor` (`native-builtins`'
/// `util_concurrent_ext::is_real_thread_pool_executor`, copied: this crate
/// cannot see that one; an unknown answer is accepted, as `tp_is_real` did).
pub(crate) fn tp_is_real_self(ctx: &mut dyn NativeContext, exec: ObjectRef) -> bool {
    if !matches!(
        ctx.get_field_by_name(exec, "workers"),
        Value::Object(Some(_))
    ) {
        return false;
    }
    let class_id = ctx.class_id_of_object(exec);
    ctx.class_assignable_to_name(class_id, "java/util/concurrent/ThreadPoolExecutor") != Some(false)
}

/// What a `ThreadPoolExecutor` door runs on ([`tp_target`]).
pub(crate) enum TpTarget {
    /// The receiver is a real pool: `ThreadPoolExecutor`'s own body on it.
    Own,
    /// A delegate wrapper whose `e` is a real pool: the wrapper's JDK body,
    /// `e.m(..)`, a virtual call on the pool.
    Delegate(ObjectRef),
    /// CratonVM's synthetic placeholder: its slots.
    Synthetic,
}

/// Round 15 wave 7 (lane drainC; `r15w6-sweepC-review-findings` S1):
/// `tp_is_real` answered "real" for a receiver whose `e` field holds a pool,
/// and the doors then ran `ThreadPoolExecutor` bytecode on the RECEIVER (a
/// wrapper's layout read as a pool's); it also answered "synthetic" for a
/// real pool subclass declaring its own field `e` that holds no pool. The
/// receiver's own `workers` now answers first, and a wrapper delegates to its
/// pool as the JDK wrapper does. Without an `e` field the answer is the old
/// one. `CRATONVM_TPE_DOORS_SELF_FIRST=0` (read only when the receiver has a
/// non-null `e`) restores `tp_is_real`.
pub(crate) fn tp_target(ctx: &mut dyn NativeContext, exec: ObjectRef) -> TpTarget {
    let inner = match ctx.get_field_by_name(exec, "e") {
        Value::Object(Some(inner)) => inner,
        _ => {
            return if matches!(
                ctx.get_field_by_name(exec, "workers"),
                Value::Object(Some(_))
            ) {
                TpTarget::Own
            } else {
                TpTarget::Synthetic
            };
        }
    };
    // Interpreter round i7 wave 137 (lane L1b): the VM's flag snapshot, not a
    // per-call read (this runs on every wrapper executor's `execute`).
    if !cratonvm_types::flags().natives.tpe_doors_self_first {
        return if tp_is_real(ctx, exec) {
            TpTarget::Own
        } else {
            TpTarget::Synthetic
        };
    }
    if tp_is_real_self(ctx, exec) {
        TpTarget::Own
    } else if tp_is_real_self(ctx, inner) {
        TpTarget::Delegate(inner)
    } else {
        TpTarget::Synthetic
    }
}

/// Run `name` / `descriptor` (with `rest`, the arguments after the receiver)
/// on the real pool behind `this` ([`tp_target`]): `ThreadPoolExecutor`'s own
/// body, statically bound (a subclass's `super.m()` cannot re-enter the
/// native), or the delegate's virtual `m`. `None` for the synthetic
/// placeholder.
pub(crate) fn tp_real_call(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name: &str,
    descriptor: &str,
    rest: &[Value],
) -> Option<MethodCallResult> {
    match tp_target(ctx, this) {
        TpTarget::Own => {
            let mut args = Vec::with_capacity(rest.len() + 1);
            args.push(Value::Object(Some(this)));
            args.extend_from_slice(rest);
            Some(ctx.invoke_special_bytecode_only(
                "java/util/concurrent/ThreadPoolExecutor",
                name,
                descriptor,
                &args,
            ))
        }
        TpTarget::Delegate(pool) => Some(ctx.invoke_virtual(pool, name, descriptor, rest)),
        TpTarget::Synthetic => None,
    }
}

pub(crate) fn native_tp_get_core_pool_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match tp_arg0(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(1))),
    };
    // invoke_special_bytecode_only (`tp_real_call`), NOT
    // invoke_virtual_bytecode_only — see the `getPoolSize` registration above /
    // `shutdownNow` below for the infinite-self-recursion hazard this avoids
    // when a subclass overrides this method and calls `super.getCorePoolSize()`.
    if let Some(result) = tp_real_call(ctx, this, "getCorePoolSize", "()I", &[]) {
        return result;
    }
    match ctx.get_field(this, TP_FIELD_SIZE) {
        Value::Int(v) => Ok(Some(Value::Int(v))),
        _ => Ok(Some(Value::Int(1))),
    }
}

pub(crate) fn native_tp_get_maximum_pool_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match tp_arg0(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(1))),
    };
    if let Some(result) = tp_real_call(ctx, this, "getMaximumPoolSize", "()I", &[]) {
        return result;
    }
    match ctx.get_field(this, TP_FIELD_SIZE) {
        Value::Int(v) => Ok(Some(Value::Int(v))),
        _ => Ok(Some(Value::Int(1))),
    }
}

pub(crate) fn native_tp_is_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match tp_arg0(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    // invoke_special_bytecode_only (`tp_real_call`): Reactor's
    // `BoundedElasticScheduler$BoundedScheduledExecutorService` overrides
    // `isShutdown()` and calls `super.isShutdown()` — with
    // invoke_virtual_bytecode_only, virtual dispatch on the concrete receiver
    // re-resolved straight back to that override, whose invokespecial
    // re-entered this same native -> infinite recursion / StackOverflowError
    // (observed live in ReactiveOAuth2ResourceServerAutoConfigurationTests).
    if let Some(result) = tp_real_call(ctx, this, "isShutdown", "()Z", &[]) {
        return result;
    }
    match ctx.get_field(this, TP_FIELD_SHUTDOWN) {
        Value::Int(v) => Ok(Some(Value::Int(v))),
        _ => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_tp_is_terminated(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match tp_arg0(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    // Same infinite-self-recursion hazard as `isShutdown` above.
    if let Some(result) = tp_real_call(ctx, this, "isTerminated", "()Z", &[]) {
        return result;
    }
    match ctx.get_field(this, TP_FIELD_SHUTDOWN) {
        Value::Int(v) => Ok(Some(Value::Int(v))),
        _ => Ok(Some(Value::Int(0))),
    }
}

pub(crate) fn native_tp_shutdown_now(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match tp_arg0(args) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    {
        // Real ThreadPoolExecutor: run the real bytecode so it actually drains
        // workQueue and returns the not-yet-started tasks. Callers like
        // Spring's `ConcurrentTaskExecutorTests.shutdownExecutor()` explicitly
        // `.cancel(true)` each returned task and rely on that list being
        // non-empty; the previous interrupt-and-return-empty-list shortcut
        // left queued FutureTasks neither run nor cancelled, so
        // `future.get(timeout)` threw TimeoutException instead of the
        // expected CancellationException. Real `shutdownNow()` bytecode
        // interrupts its own workers as part of draining the queue (the
        // native worker-interrupt helper this used to need was dead code and
        // is gone, gc-common w20-a).
        //
        // NOT invoke_virtual_bytecode_only: `ScheduledThreadPoolExecutor`
        // overrides `shutdownNow()` as `return super.shutdownNow();` (a
        // single invokespecial on ThreadPoolExecutor, confirmed via javap —
        // no extra STPE-specific behavior). Virtual dispatch on an STPE
        // receiver would re-resolve to STPE's own override, whose
        // `invokespecial ThreadPoolExecutor.shutdownNow` re-enters the native
        // registry and finds this very native again -> infinite recursion /
        // StackOverflowError. `invoke_special_bytecode_only` resolves
        // statically on the NAMED class (ThreadPoolExecutor's own body),
        // exactly like the sibling fix already applied to `shutdown()` above
        // (`tp_real_call`).
        if let Some(result) = tp_real_call(ctx, this, "shutdownNow", "()Ljava/util/List;", &[]) {
            return result;
        }
    }
    ctx.set_field(this, TP_FIELD_SHUTDOWN, Value::Int(1));
    // Return empty list. gc-common w20-a: the list is rooted across its
    // backing array's allocation (it was stored through its old address).
    // nio20 wave 2 (L5): at the RESOLVED `ArrayList` slots (`al_slots`), as
    // every other mint in this file. The raw `(0, 1)` writes were the
    // synthetic layout only; on a real `ArrayList` slot 0 is
    // `AbstractList.modCount`, so the array landed in an int slot and the
    // JDK's `isEmpty()` / `size()` read `elementData` as `size`.
    let __al_n_fields = al_slots(ctx).2;
    let list = try_alloc_synthetic(ctx, "java/util/ArrayList", __al_n_fields)?;
    let list_pin = ctx.pin_native_root(list);
    let arr: ObjectRef = alloc_ref_array(ctx, 0);
    let list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(list_pin);
    al_set_data(ctx, list, arr);
    al_set_size(ctx, list, 0);
    Ok(Some(Value::Object(Some(list))))
}

// ForkJoinPool / ForkJoinTask / RecursiveTask / RecursiveAction `invoke`
// natives used to live here. They are gone: they wrote an object reference
// into `ForkJoinTask.status` (an `int`) and had no `compute()V` shape. The
// live implementations are in
// `native-builtins/src/phases_early.rs::register_real_jdk_forkjoin_essentials`.
// See the note at the deleted registration site in
// `register_concurrent_completeness_natives`.

// ===========================================================================
// Test hooks — exposed so the GC-relocation integration harness in
// `tests/gc_relocation_harness.rs` can write to and read from each of the
// four overlay tables (LinkedList, LinkedHashMap, TreeMap, TreeSet) without
// having to stand up the full registry + native-method dispatch chain.
//
// All four overlay tables are now keyed by `ctx.identity_hash_code(this)`;
// these shims simply delegate to the existing private helpers, so a
// passing test proves the key path is GC-stable end-to-end.
//
// `#[doc(hidden)]` because callers outside the test harness should go
// through the registered native methods.
// ===========================================================================

/// LinkedList overlay read shim. See module-level
/// `identity_hash::obj_key` for the key contract.
#[doc(hidden)]
pub fn __test_ll_get(ctx: &dyn NativeContext, this: ObjectRef, name: &'static str) -> Value {
    ll_get(ctx, this, name)
}
/// LinkedList overlay write shim.
#[doc(hidden)]
pub fn __test_ll_set(ctx: &mut dyn NativeContext, this: ObjectRef, name: &'static str, v: Value) {
    ll_set(ctx, this, name, v);
}
/// LinkedHashMap overlay read shim.
#[doc(hidden)]
pub fn __test_lhm_get(ctx: &dyn NativeContext, this: ObjectRef, name: &str) -> Value {
    lhm_get(ctx, this, name, 0)
}
/// LinkedHashMap overlay write shim.
#[doc(hidden)]
pub fn __test_lhm_set(ctx: &mut dyn NativeContext, this: ObjectRef, name: &str, v: Value) {
    lhm_set(ctx, this, name, 0, v);
}
/// TreeMap array-mode side-table read shim.
#[doc(hidden)]
pub fn __test_tm_get_slot(ctx: &dyn NativeContext, this: ObjectRef, slot: usize) -> Value {
    tm_get_slot(ctx, this, slot)
}
/// TreeMap array-mode side-table write shim.
#[doc(hidden)]
pub fn __test_tm_set_slot(ctx: &mut dyn NativeContext, this: ObjectRef, slot: usize, v: Value) {
    tm_set_slot(ctx, this, slot, v);
}
/// Fast-mode TreeMap side-table write shim: insert `value` under a
/// String key into the BTreeMap that backs a comparator-less TreeMap,
/// exactly as `native_tm_put`'s fast path does (`bt.insert`). Exposes
/// the fast store to the GC-relocation harness so it can prove
/// `gc_scan_collection_overlay_roots`/`gc_update_collection_overlay_refs`
/// root and remap fast-mode object values (finding B1/V1).
#[doc(hidden)]
pub fn __test_tm_fast_put_str(ctx: &dyn NativeContext, this: ObjectRef, key: &str, value: Value) {
    tm_fast_with(ctx, this, |bt| {
        bt.insert(TreeKey::Str(key.encode_utf16().collect()), value);
    });
}
/// Fast-mode TreeMap side-table read shim. Mirror of
/// `__test_tm_fast_put_str`.
#[doc(hidden)]
pub fn __test_tm_fast_get_str(ctx: &dyn NativeContext, this: ObjectRef, key: &str) -> Value {
    tm_fast_with(ctx, this, |bt| {
        bt.get(&TreeKey::Str(key.encode_utf16().collect()))
            .copied()
            .unwrap_or(Value::Object(None))
    })
}
/// TreeSet array-mode side-table read shim.
#[doc(hidden)]
pub fn __test_ts_get_slot(ctx: &dyn NativeContext, this: ObjectRef, slot: usize) -> Value {
    ts_get_slot(ctx, this, slot)
}
/// TreeSet array-mode side-table write shim.
#[doc(hidden)]
pub fn __test_ts_set_slot(ctx: &mut dyn NativeContext, this: ObjectRef, slot: usize, v: Value) {
    ts_set_slot(ctx, this, slot, v);
}

