#[allow(unused_imports)]
use super::*;


// ===========================================================================
// Stream API — Eager evaluation on Vec<Value>
// ===========================================================================

pub(crate) const STREAM_FIELD_ELEMENTS: usize = 0;
// keycloak #16 Part A: synthetic streams carry their registered BaseStream.onClose
// Runnables in field 1 (an Object[] of Runnable, or Object(None) when none). close()
// runs them once; intermediate ops propagate them; flatMap/concat follow the JDK
// close-handler contract. See docs/feature-designs/native-dispatch-architecture.md.
pub(crate) const STREAM_FIELD_CLOSE_HANDLERS: usize = 1;
/// The full synthetic-Stream layout: slots 0..=4 (see `STREAM_FIELD_LINKED`).
///
/// Was 2 until W7-65 added the linked-or-consumed slot. Growing it makes slots
/// 2 (lazy spliterator) and 3 (op chain) *readable* on streams built by
/// `make_stream` for the first time, which is safe because
/// `Heap::try_alloc_object` allocates through `try_alloc_zeroed`: an unwritten
/// slot reads back as the zero `Value`, so `stream_lazy_spliterator` and
/// `stream_has_chain` both still answer "no". Every reader of slots 2/3/4
/// guards on `object_num_fields` anyway, so the many 1-field ad-hoc stream
/// allocations elsewhere in the workspace are unaffected.
///
/// 5 -> 7 (JIT round 16 wave 9, lane spliter): slots 5 and 6 carry what
/// `Stream.spliterator()` must report ([`STREAM_FIELD_SPL_FACTS`],
/// [`STREAM_FIELD_SPL_CMP`]). Same zeroing argument: an unwritten slot 5 reads
/// back as "no facts", which is the pre-wave-9 answer.
///
/// 7 -> 8 (JIT round 17 wave 8, lane streams4): slot 7 is the next stage
/// ([`STREAM_FIELD_NEXT_STAGE`]). Same zeroing argument: an unwritten slot 7
/// reads back as "no next stage", which is the pre-wave-8 answer.
///
/// 8 -> 9 (JIT round 18 wave 10, lane streams5): slot 8 is the stage's
/// HotSpot class code ([`STREAM_FIELD_STAGE_CLASS`]). Same zeroing argument:
/// an unwritten slot 8 reads back as "a source stage", whose class is the
/// pipeline's `Head`, which is the pre-wave-10 answer.
pub(crate) const STREAM_NUM_FIELDS: usize = 9;
/// The spliterator facts of this stream stage, packed in one `Value::Long`
/// ([`stream_spl_facts`]): the low 32 bits are the `characteristics()` the
/// stage's `spliterator()` reports (plus the private `SPL_PRIV_*` bits), the
/// high 32 bits the HEAD source's element count plus one (0 = unknown). Written
/// by the source natives that know their JDK spliterator (`Stream.of`,
/// `Stream.empty`, the primitive `range`s, `TreeSet.stream`) and by every
/// intermediate op through [`stream_derive_spl_facts`]. Anything else reads 0
/// and `Stream.spliterator()` keeps the list default it always had.
pub(crate) const STREAM_FIELD_SPL_FACTS: usize = 5;
/// The comparator a SORTED source stage's spliterator answers from
/// `getComparator()` (`TreeSet(cmp).stream()`); null otherwise.
pub(crate) const STREAM_FIELD_SPL_CMP: usize = 6;
/// The stage an intermediate op built over this REFERENCE stage, once one was
/// (JIT round 17 wave 8, lane streams4; [`stream_shared_parallel_enabled`]).
/// A reference stage is linked at most once (`stream_link_or_consume`), so
/// following this slot from any stage walks one pipeline down to its newest
/// stage, its tail ([`stream_pipeline_tail`]). The tail's
/// [`STREAM_FACTS_PARALLEL`] bit is the pipeline's mode: HotSpot keeps
/// `parallel` on the shared source stage, which a synthetic stage does not
/// reference, and every new stage copies its upstream's bit, so "the tail's
/// bit" is "the source stage's flag" as long as `parallel()` / `sequential()`
/// write the tail too. Written only by [`stream_record_next_stage`]; null on
/// every primitive carrier and every stage built under the kill switch.
/// Since round 18 wave 10 (lane streams5) also written by
/// [`stream_record_next_stage_any`] for primitive and cross-shape stages
/// ([`stream_shared_parallel_primitive_enabled`]).
pub(crate) const STREAM_FIELD_NEXT_STAGE: usize = 7;
/// The HotSpot class an intermediate stage stands for, as a `Value::Int`
/// index into [`STREAM_STAGE_CLASS_NAMES`] (JIT round 18 wave 10, lane
/// streams5; [`stream_stage_display_class_enabled`]). `0` (or an unwritten
/// slot) is a source stage: the stamp's `*Pipeline$Head` alias. Written only
/// by [`stream_stamp_stage_class`], read only by
/// [`synthetic_stream_stage_display_name`] (`Object.getClass()` /
/// `toString()` in `native-builtins`).
pub(crate) const STREAM_FIELD_STAGE_CLASS: usize = 8;
/// Lazy source spliterator slot, populated only by
/// `StreamSupport.stream(realSpliterator, false)` (see service_loader.rs). When
/// present (and `STREAM_FIELD_ELEMENTS` is still null), the stream has NOT been
/// drained: `forEach` drives the spliterator one element at a time, and every
/// other op materialises it first via `materialize_lazy_stream`.
pub(crate) const STREAM_FIELD_LAZY_SPLITERATOR: usize = 2;
// Slot 3 is `STREAM_FIELD_OP_CHAIN` (declared with the lazy-pipeline code
// below, which is the only thing that writes it).
/// W7-65 — `java.util.stream.AbstractPipeline.linkedOrConsumed`, modelled.
///
/// `Value::Int(1)` once this stream stage has been linked (an intermediate op
/// was applied to it) or consumed (a terminal op read its elements); anything
/// else — including the zero a fresh slot reads back as — means "not yet".
/// Deliberately asymmetric: only the exact `Int(1)` counts, so a stream whose
/// slot was never written can never be mistaken for a used one.
pub(crate) const STREAM_FIELD_LINKED: usize = 4;

/// HotSpot's own message text, measured from
/// `AbstractPipeline.MSG_STREAM_LINKED` in `jdk-25.0.3.9-hotspot/lib/src.zip`.
pub(crate) const STREAM_LINKED_MSG: &str = "stream has already been operated upon or closed";

/// True when `stream` carries the linked-or-consumed slot AND it is set.
///
/// A short-layout stream (the 1-field `Stream` objects several native modules
/// still mint, and the legacy 2-field layout) has no slot to carry the flag and
/// is therefore never reported as used. That is the deliberate direction of the
/// error: a missed reuse is a residual divergence, a spurious one breaks a
/// working pipeline.
pub(crate) fn stream_is_linked(ctx: &dyn NativeContext, stream: ObjectRef) -> bool {
    if ctx.object_num_fields(stream) <= STREAM_FIELD_LINKED {
        return false;
    }
    matches!(ctx.get_field(stream, STREAM_FIELD_LINKED), Value::Int(1))
}

/// Whether `stream` is the synthetic REFERENCE-stream carrier -- the one shape
/// `stream_link_or_consume` may be applied to.
///
/// The primitive carriers are excluded for the reason
/// `stream_link_or_consume` records: they reach the funnel through
/// `int_stream_elements`, which is infallible at 25 call sites, so a throw
/// there would be swallowed and silently degrade the stream to empty.
pub(crate) fn stream_is_reference_carrier(ctx: &dyn NativeContext, stream: ObjectRef) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(stream))
        .as_deref()
        == Some("java/util/stream/Stream")
}

/// The `STREAM_FIELD_LINKED` value of a `StackWalker.walk` stream whose walk
/// has returned while the stream was still unlinked
/// ([`end_stack_walk_stream`]). Interpreter round i1 wave 43, lane L3.
pub(crate) const STREAM_WALK_ENDED: i32 = 2;

/// HotSpot's message for it (`StackStreamFactory.AbstractStackWalker.
/// checkState`, JDK 25 sources; measured by
/// `L3W42StackWalkerApi.java`, row `escaped-stream`).
pub(crate) const STREAM_WALK_ENDED_MSG: &str = "This stack stream is not valid for walking.";

/// The slot count of a synthetic reference stream that carries the
/// linked-or-consumed flag ([`STREAM_FIELD_LINKED`]): a native outside this
/// crate that builds a `java/util/stream/Stream` which must answer
/// `IllegalStateException` on misuse allocates it with this many slots.
pub const SYNTHETIC_STREAM_NUM_FIELDS: usize = STREAM_NUM_FIELDS;

/// A `StackWalker.walk` stream's walk slot (interpreter round i1 wave 44,
/// lane L3). On the walk's own stream, `Value::Int(0)` while the walk's
/// function runs and [`STREAM_WALK_ENDED`] once it has returned
/// ([`begin_stack_walk_stream`], [`end_stack_walk_stream`]); on a stream an
/// intermediate operation derived from it (or from such a derived stream),
/// the walk's stream ([`make_derived_stream`]). Only those streams have the
/// slot ([`WALK_STREAM_NUM_FIELDS`]); every other synthetic stream keeps
/// [`STREAM_NUM_FIELDS`] slots.
///
/// 5 -> 7 (wave 9, lane spliter), behind the two spliterator-fact slots: it
/// must stay the LAST slot, because "has more than `STREAM_FIELD_WALK` slots"
/// is how every reader below recognises a walk stream. 7 -> 8 with the
/// next-stage slot (round 17 wave 8, lane streams4), by the same rule.
pub(crate) const STREAM_FIELD_WALK: usize = STREAM_NUM_FIELDS;

/// The slot count of a `StackWalker.walk` stream, and of a stream derived
/// from one: [`SYNTHETIC_STREAM_NUM_FIELDS`] plus [`STREAM_FIELD_WALK`].
pub const WALK_STREAM_NUM_FIELDS: usize = STREAM_NUM_FIELDS + 1;

/// `stream`, allocated with [`WALK_STREAM_NUM_FIELDS`] slots, is the stream a
/// `StackWalker.walk` hands its function: record that its walk is in
/// progress, so streams derived from it name it (interpreter round i1 wave
/// 44, lane L3). A stream with fewer slots is left alone.
pub fn begin_stack_walk_stream(ctx: &mut dyn NativeContext, stream: ObjectRef) {
    if ctx.object_num_fields(stream) > STREAM_FIELD_WALK {
        ctx.set_field(stream, STREAM_FIELD_WALK, Value::Int(0));
    }
}

/// `StackWalker.walk` has returned: a later operation on its `stream` throws
/// `IllegalStateException("This stack stream is not valid for walking.")`, as
/// HotSpot's walk stream does once its frames are gone (interpreter round i1
/// wave 43, lane L3). A stream the walk already linked or consumed keeps that
/// state (its reuse is the older error, as on HotSpot), and a stream with no
/// flag slot (fewer than [`SYNTHETIC_STREAM_NUM_FIELDS`]) is left alone.
///
/// Since wave 44 a stream an intermediate operation derived from it inside
/// the function (an element snapshot here, a pipeline over the walk's
/// spliterator on HotSpot) refuses too, at its first operation: it names the
/// walk's stream ([`STREAM_FIELD_WALK`]), whose walk slot this marks ended
/// whether or not the stream itself was linked ([`stream_walk_ended`]).
pub fn end_stack_walk_stream(ctx: &mut dyn NativeContext, stream: ObjectRef) {
    if ctx.object_num_fields(stream) > STREAM_FIELD_WALK {
        ctx.set_field(stream, STREAM_FIELD_WALK, Value::Int(STREAM_WALK_ENDED));
    }
    if ctx.object_num_fields(stream) <= STREAM_FIELD_LINKED
        || matches!(ctx.get_field(stream, STREAM_FIELD_LINKED), Value::Int(1))
    {
        return;
    }
    ctx.set_field(stream, STREAM_FIELD_LINKED, Value::Int(STREAM_WALK_ENDED));
}

/// The `StackWalker.walk` stream `stream` is, or was derived from
/// ([`STREAM_FIELD_WALK`]); `None` for every other stream (one slot count
/// read).
pub(crate) fn stream_walk_origin(ctx: &dyn NativeContext, stream: ObjectRef) -> Option<ObjectRef> {
    if ctx.object_num_fields(stream) <= STREAM_FIELD_WALK {
        return None;
    }
    match ctx.get_field(stream, STREAM_FIELD_WALK) {
        Value::Object(Some(walk)) => Some(walk),
        Value::Int(_) => Some(stream),
        _ => None,
    }
}

/// Was `stream` derived from a `StackWalker.walk` stream whose walk has
/// returned ([`end_stack_walk_stream`])? The walk's own stream answers
/// through its linked slot instead (`stream_link_or_consume`).
pub(crate) fn stream_walk_ended(ctx: &dyn NativeContext, stream: ObjectRef) -> bool {
    if ctx.object_num_fields(stream) <= STREAM_FIELD_WALK {
        return false;
    }
    let Value::Object(Some(walk)) = ctx.get_field(stream, STREAM_FIELD_WALK) else {
        return false;
    };
    ctx.object_num_fields(walk) > STREAM_FIELD_WALK
        && matches!(
            ctx.get_field(walk, STREAM_FIELD_WALK),
            Value::Int(STREAM_WALK_ENDED)
        )
}

// ---------------------------------------------------------------------------
// THROWS5-3 (JIT round 19 wave 7, lane walk7; `CRATONVM_STACKWALK_LAZY_FRAMES`,
// opt-in): a `StackWalker.walk` stream whose frames are built on first read.
//
// The walk (`native-builtins` `p59_sw_walk`) allocates the stream's element
// array with every slot null, allocates the stream with
// [`LAZY_WALK_STREAM_NUM_FIELDS`] slots, and pushes a [`LazyWalkGuard`] that
// knows how to build frame `i` for as long as the walk's function runs. A
// stack frame is never null, so a null slot is a frame not built yet.
//
// Every read of such an array goes through [`lazy_walk_fill`] first:
//
// * the two whole-array funnels ([`stream_elements`]'s snapshot read and
//   [`stream_source_elems`]) build every frame not built yet, so every
//   operation that is not listed below reads exactly what it read before;
// * the lazy chain's pull ([`stream_pull_internal`], `--compatible`) builds
//   frame `i` when the pull reaches it, so `skip(k).findFirst()`,
//   `filter(..).findFirst()` and `limit(n)` stop building where they stop
//   reading;
// * `findFirst`/`findAny` and `anyMatch`/`allMatch`/`noneMatch` on a walk
//   stream with no deferred op build frames up to the one that decides;
//   `count()` builds none (the count is the array's length);
// * `skip(k)` / `limit(n)` that run eagerly (`--real-jdk`, where the lazy
//   chain's record class is refused) build a SLICE: a derived stream whose
//   array holds the frames already built and nulls elsewhere, plus its
//   offset into the walk's array ([`STREAM_FIELD_WALK_VIEW`]); its frame `j`
//   is the walk's frame `offset + j`, built into both arrays.
//
// The frames are the frames the eager walk built (the same builder, the same
// index, each built once and kept in the walk's array, so identity holds
// across reads); only WHEN each is built moves. HotSpot builds its frames in
// batches as the stream asks for them, so a function that reads three frames
// paid for three batches' worth there and for every frame here.
//
// Lifetime: the guard is dropped when the function returns, after the walk's
// stream was marked ended; every stream that reads the array after that
// refuses before reading (`stream_link_or_consume`'s two walk arms), so no
// null slot is ever handed out. A stream not derived from the walk (an eager
// copy) holds an array of built frames and is left alone.
// ---------------------------------------------------------------------------

/// The slot count of a lazy `StackWalker.walk` stream and of every stream
/// derived from one: [`WALK_STREAM_NUM_FIELDS`] plus the slice slot
/// ([`STREAM_FIELD_WALK_VIEW`]). [`STREAM_FIELD_WALK`] stays the slot every
/// reader recognises a walk stream by ("more than `STREAM_FIELD_WALK`
/// slots"); the slice slot comes after it, only on these streams.
pub const LAZY_WALK_STREAM_NUM_FIELDS: usize = WALK_STREAM_NUM_FIELDS + 1;

/// A lazy walk slice's offset into the walk's array: `Value::Long` of
/// [`LAZY_WALK_VIEW_MARK`] `|` offset. Any other value (the zero a fresh slot
/// reads back as included) is "not a slice".
pub(crate) const STREAM_FIELD_WALK_VIEW: usize = WALK_STREAM_NUM_FIELDS;

/// The mark bit of a [`STREAM_FIELD_WALK_VIEW`] word.
pub(crate) const LAZY_WALK_VIEW_MARK: i64 = 1 << 62;

/// The frame builder of a lazy walk: frame `index` of the walk's screened
/// frames, as the eager walk builds it.
pub type LazyWalkFrameFn =
    dyn FnMut(&mut dyn NativeContext, usize) -> Result<ObjectRef, MethodCallFailed>;

/// One lazy walk in progress on this thread.
pub(crate) struct LazyWalkSource {
    /// The walk's stream, pinned by the walk for the whole function.
    pub(in crate::streams) stream_pin: usize,
    /// The builder. Shared so the thread's stack is not borrowed while it
    /// runs (it allocates and may run Java, which may walk again).
    pub(in crate::streams) builder: std::rc::Rc<std::cell::RefCell<Box<LazyWalkFrameFn>>>,
    /// Frames built so far (the census, `CRATONVM_DBG_WALK_SECTIONS`).
    pub(in crate::streams) built: std::rc::Rc<std::cell::Cell<usize>>,
}

thread_local! {
    /// The lazy walks in progress on this thread, innermost last. A walk's
    /// function can walk again, so this nests; entries are pushed and popped
    /// by [`LazyWalkGuard`] in strict LIFO order.
    pub(crate) static LAZY_WALK_SOURCES: std::cell::RefCell<Vec<LazyWalkSource>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A lazy walk's registration; dropping it ends the laziness (the walk calls
/// [`end_stack_walk_stream`] first).
pub struct LazyWalkGuard {
    pub(in crate::streams) depth: usize,
    pub(in crate::streams) built: std::rc::Rc<std::cell::Cell<usize>>,
}

impl LazyWalkGuard {
    /// Frames built so far by this walk's streams.
    pub fn built(&self) -> usize {
        self.built.get()
    }
}

impl Drop for LazyWalkGuard {
    fn drop(&mut self) {
        LAZY_WALK_SOURCES.with(|s| s.borrow_mut().truncate(self.depth));
    }
}

/// Register the lazy walk whose stream (allocated with
/// [`LAZY_WALK_STREAM_NUM_FIELDS`] slots, its element array all null, marked
/// by [`begin_stack_walk_stream`]) is pinned at `stream_pin`; `builder`
/// builds frame `i`. The registration lasts until the guard drops, which must
/// happen before `stream_pin` is released.
pub fn push_lazy_walk_source(stream_pin: usize, builder: Box<LazyWalkFrameFn>) -> LazyWalkGuard {
    let built = std::rc::Rc::new(std::cell::Cell::new(0));
    let depth = LAZY_WALK_SOURCES.with(|s| {
        let mut s = s.borrow_mut();
        let depth = s.len();
        s.push(LazyWalkSource {
            stream_pin,
            builder: std::rc::Rc::new(std::cell::RefCell::new(builder)),
            built: built.clone(),
        });
        depth
    });
    LazyWalkGuard { depth, built }
}

/// Where `stream`'s elements come from when they are a lazy walk's: the
/// registration's pin, builder and counter, and the offset of `stream`'s slot
/// 0 into the walk's array. `None` (one thread-local read on the common path)
/// for every other stream: no lazy walk in progress, a stream of no walk, a
/// walk that is not lazy or is over, or an eager copy of a walk's frames.
#[allow(clippy::type_complexity)]
pub(crate) fn lazy_walk_target(
    ctx: &dyn NativeContext,
    stream: ObjectRef,
) -> Option<(
    usize,
    std::rc::Rc<std::cell::RefCell<Box<LazyWalkFrameFn>>>,
    std::rc::Rc<std::cell::Cell<usize>>,
    usize,
)> {
    if LAZY_WALK_SOURCES.with(|s| s.borrow().is_empty()) {
        return None;
    }
    if ctx.object_num_fields(stream) < LAZY_WALK_STREAM_NUM_FIELDS {
        return None;
    }
    let walk = stream_walk_origin(ctx, stream)?;
    if ctx.object_num_fields(walk) < LAZY_WALK_STREAM_NUM_FIELDS
        || !matches!(ctx.get_field(walk, STREAM_FIELD_WALK), Value::Int(0))
    {
        return None;
    }
    let (pin, builder, built) = LAZY_WALK_SOURCES.with(|s| {
        s.borrow()
            .iter()
            .rev()
            .find(|e| ctx.read_native_pin(e.stream_pin, walk) == walk)
            .map(|e| (e.stream_pin, e.builder.clone(), e.built.clone()))
    })?;
    let Value::Object(Some(base)) = ctx.get_field(walk, STREAM_FIELD_ELEMENTS) else {
        return None;
    };
    let Value::Object(Some(own)) = ctx.get_field(stream, STREAM_FIELD_ELEMENTS) else {
        return None;
    };
    if own == base {
        return Some((pin, builder, built, 0));
    }
    match ctx.get_field(stream, STREAM_FIELD_WALK_VIEW) {
        Value::Long(word) if word & LAZY_WALK_VIEW_MARK != 0 => {
            // Cast: an offset into an array, stored from a `usize` below 2^32.
            Some((pin, builder, built, (word & 0xffff_ffff) as usize))
        }
        _ => None,
    }
}

/// The length of `stream`'s element array when its elements are a lazy
/// walk's ([`lazy_walk_target`]); `None` otherwise.
pub(crate) fn lazy_walk_len(ctx: &dyn NativeContext, stream: ObjectRef) -> Option<usize> {
    lazy_walk_target(ctx, stream)?;
    match ctx.get_field(stream, STREAM_FIELD_ELEMENTS) {
        Value::Object(Some(arr)) => Some(ctx.array_length(arr)),
        _ => None,
    }
}

/// Build the frames `from..to` (clamped to the array) of `stream`'s element
/// array that are not built yet, when its elements are a lazy walk's; a
/// no-op for every other stream. A slice's frame is built into the walk's
/// array and copied into the slice's.
pub(crate) fn lazy_walk_fill(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
    from: usize,
    to: usize,
) -> Result<(), MethodCallFailed> {
    let Some((walk_pin, builder, built, offset)) = lazy_walk_target(&*ctx, stream) else {
        return Ok(());
    };
    // `stream_walk_origin` answered above, so the walk's reference is at hand
    // as the fallback of its pin's read.
    let Some(walk) = stream_walk_origin(&*ctx, stream) else {
        return Ok(());
    };
    let stream_pin = ctx.pin_native_root(stream);
    let result = (|| {
        let mut j = from;
        while j < to {
            let stream = ctx.read_native_pin(stream_pin, stream);
            let Value::Object(Some(own)) = ctx.get_field(stream, STREAM_FIELD_ELEMENTS) else {
                break;
            };
            if j >= ctx.array_length(own) {
                break;
            }
            if !matches!(ctx.get_array_element(own, j), Value::Object(None)) {
                j += 1;
                continue;
            }
            let i = offset + j;
            let walk_cur = ctx.read_native_pin(walk_pin, walk);
            let Value::Object(Some(base)) = ctx.get_field(walk_cur, STREAM_FIELD_ELEMENTS) else {
                break;
            };
            if i >= ctx.array_length(base) {
                break;
            }
            if matches!(ctx.get_array_element(base, i), Value::Object(None)) {
                let frame = {
                    let Ok(mut build) = builder.try_borrow_mut() else {
                        return Err(RuntimeError::IllegalStateException {
                            message: STREAM_WALK_ENDED_MSG.to_string(),
                        }
                        .into());
                    };
                    (build)(ctx, i)?
                };
                // Nothing allocates between the build's return and the store.
                let walk_cur = ctx.read_native_pin(walk_pin, walk);
                if let Value::Object(Some(base)) = ctx.get_field(walk_cur, STREAM_FIELD_ELEMENTS) {
                    ctx.set_array_element(base, i, Value::Object(Some(frame)));
                }
                built.set(built.get() + 1);
            }
            if offset != 0 || own != base {
                let walk_cur = ctx.read_native_pin(walk_pin, walk);
                let stream = ctx.read_native_pin(stream_pin, stream);
                if let (Value::Object(Some(base)), Value::Object(Some(own))) = (
                    ctx.get_field(walk_cur, STREAM_FIELD_ELEMENTS),
                    ctx.get_field(stream, STREAM_FIELD_ELEMENTS),
                ) {
                    let frame = ctx.get_array_element(base, i);
                    ctx.set_array_element(own, j, frame);
                }
            }
            j += 1;
        }
        Ok(())
    })();
    ctx.unpin_native_roots(stream_pin);
    result
}

/// Element `index` of `stream`'s lazy walk array, built first if it is not
/// yet ([`lazy_walk_fill`]). The caller checked `index` against
/// [`lazy_walk_len`]. The value is the array's: the caller pins it before it
/// allocates.
pub(crate) fn lazy_walk_element(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
    index: usize,
) -> Result<Value, MethodCallFailed> {
    let stream_pin = ctx.pin_native_root(stream);
    let filled = lazy_walk_fill(ctx, stream, index, index + 1);
    let stream = ctx.read_native_pin(stream_pin, stream);
    ctx.unpin_native_roots(stream_pin);
    filled?;
    Ok(match ctx.get_field(stream, STREAM_FIELD_ELEMENTS) {
        Value::Object(Some(arr)) if index < ctx.array_length(arr) => {
            ctx.get_array_element(arr, index)
        }
        _ => Value::Object(None),
    })
}

/// A short-circuiting terminal on a lazy walk stream with no deferred op
/// (`findFirst`, `findAny`, the three matches): link or consume `this` as
/// [`stream_elements`] would, then hand `each` the elements in order, each
/// built when reached, until it answers [`PullStep::Stop`]. `None` (nothing
/// done) when `this` is not such a stream.
pub(crate) fn lazy_walk_each(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    each: &mut StreamEmit<'_>,
) -> Option<Result<(), MethodCallFailed>> {
    if !stream_is_reference_carrier(&*ctx, this)
        || stream_has_chain(&*ctx, this)
        || stream_lazy_spliterator(&*ctx, this).is_some()
    {
        return None;
    }
    let len = lazy_walk_len(&*ctx, this)?;
    Some((|| {
        stream_link_or_consume(ctx, this)?;
        let this_pin = ctx.pin_native_root(this);
        let mut outcome = Ok(());
        for index in 0..len {
            let cur = ctx.read_native_pin(this_pin, this);
            let step = lazy_walk_element(ctx, cur, index).and_then(|v| each(ctx, v));
            match step {
                Ok(PullStep::Continue) => {}
                Ok(PullStep::Stop) => break,
                Err(e) => {
                    outcome = Err(e);
                    break;
                }
            }
        }
        ctx.unpin_native_roots(this_pin);
        outcome
    })())
}

/// An eager `skip(k)` (`skip == true`) or `limit(n)` on a lazy walk stream
/// with no deferred op: link `this`, then derive the slice of its frames
/// without building any ([`STREAM_FIELD_WALK_VIEW`]). `None` when `this` is
/// not such a stream (the caller runs its eager body).
pub(crate) fn lazy_walk_slice(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    skip: bool,
    n: usize,
) -> Option<MethodCallResult> {
    if !stream_is_reference_carrier(&*ctx, this)
        || stream_has_chain(&*ctx, this)
        || stream_lazy_spliterator(&*ctx, this).is_some()
    {
        return None;
    }
    let (_, _, _, offset) = lazy_walk_target(&*ctx, this)?;
    let len = lazy_walk_len(&*ctx, this)?;
    Some((|| {
        stream_link_or_consume(ctx, this)?;
        let (start, count) = if skip {
            let start = n.min(len);
            (start, len - start)
        } else {
            (0, n.min(len))
        };
        // The frames already built, nulls elsewhere: `make_derived_stream`
        // pins every element it is handed.
        let elements: Vec<Value> = match ctx.get_field(this, STREAM_FIELD_ELEMENTS) {
            Value::Object(Some(arr)) => (start..start + count)
                .map(|j| ctx.get_array_element(arr, j))
                .collect(),
            _ => Vec::new(),
        };
        let r = make_derived_stream(ctx, this, &elements, SplOp::Preserve)?;
        if let Some(Value::Object(Some(dst))) = r {
            if ctx.object_num_fields(dst) > STREAM_FIELD_WALK_VIEW {
                // Cast: an array offset (an array index fits 32 bits).
                let word = LAZY_WALK_VIEW_MARK | ((offset + start) as i64 & 0xffff_ffff);
                ctx.set_field(dst, STREAM_FIELD_WALK_VIEW, Value::Long(word));
            }
        }
        Ok(r)
    })())
}

/// The derived-stream slot count for a stream derived from `walk`
/// ([`stream_walk_origin`]): a lazy walk's derived streams carry the slice
/// slot too.
pub(crate) fn walk_derived_num_fields(ctx: &dyn NativeContext, walk: ObjectRef) -> usize {
    if ctx.object_num_fields(walk) >= LAZY_WALK_STREAM_NUM_FIELDS {
        LAZY_WALK_STREAM_NUM_FIELDS
    } else {
        WALK_STREAM_NUM_FIELDS
    }
}

/// Set the linked-or-consumed flag. No-op on a stream with no slot for it.
pub(crate) fn stream_mark_linked(ctx: &mut dyn NativeContext, stream: ObjectRef) {
    if ctx.object_num_fields(stream) <= STREAM_FIELD_LINKED {
        return;
    }
    ctx.set_field(stream, STREAM_FIELD_LINKED, Value::Int(1));
}

/// The JDK's `linkOrConsume()`: throw if this stage was already linked or
/// consumed, otherwise mark it.
///
/// WHERE THIS IS CALLED FROM, and why there is exactly one call site.
/// `AbstractPipeline` sets the flag in eight places — the two
/// intermediate-stage constructors, `linkOrConsume`, `evaluate(TerminalOp)`,
/// `evaluateToArrayNode`, `sourceStageSpliterator`, `close` and `spliterator`.
/// CratonVM does not run that bytecode: a synthetic stream is an element
/// snapshot, and *every* operation that links or consumes one — intermediate
/// and terminal alike — reads that snapshot through `stream_elements`. So the
/// JDK's eight sites collapse onto that one funnel, and putting the check
/// there is what makes this a 1-site change rather than a 99-site one.
///
/// Reference streams only. `IntStream`/`LongStream`/`DoubleStream` reach the
/// funnel through `int_stream_elements`, which is INFALLIBLE
/// (`.unwrap_or_default()`) at 25 call sites: throwing there would be swallowed
/// and silently degrade the stream to empty, which is worse than the divergence
/// it would close. The primitive streams are left unguarded on purpose — see
/// docs/feature-designs/real-jdk-mode.md.
pub(crate) fn stream_link_or_consume(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
) -> Result<(), MethodCallFailed> {
    // A `StackWalker.walk` stream used after `walk` returned (interpreter
    // round i1 wave 43, lane L3): HotSpot's pipeline links (the flag is set)
    // and its stack-frame spliterator then refuses (`StackStreamFactory.
    // AbstractStackWalker.checkState`). Only an unlinked stream carries the
    // mark (`end_stack_walk_stream`), so a consumed one still answers
    // `STREAM_LINKED_MSG` first, as HotSpot's `evaluate` does.
    if ctx.object_num_fields(stream) > STREAM_FIELD_LINKED
        && matches!(
            ctx.get_field(stream, STREAM_FIELD_LINKED),
            Value::Int(STREAM_WALK_ENDED)
        )
    {
        stream_mark_linked(ctx, stream);
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: STREAM_WALK_ENDED_MSG.to_string(),
        }
        .into());
    }
    if stream_is_linked(ctx, stream) {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: STREAM_LINKED_MSG.to_string(),
        }
        .into());
    }
    // A stream derived from a walk's stream inside the walk's function, used
    // after the walk (interpreter round i1 wave 44, lane L3): HotSpot's
    // derived pipeline links, then its source, the walk's stack-frame
    // spliterator, refuses. A derived stream already linked answered above.
    if stream_walk_ended(ctx, stream) {
        stream_mark_linked(ctx, stream);
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: STREAM_WALK_ENDED_MSG.to_string(),
        }
        .into());
    }
    stream_mark_linked(ctx, stream);
    Ok(())
}

/// The lazy source spliterator stashed on a synthetic stream by
/// `StreamSupport.stream(realSpliterator, false)`, or `None` for an ordinary
/// (already-materialised) stream. Guards on the field count so streams allocated
/// with the legacy 2-field layout are simply never lazy.
pub(crate) fn stream_lazy_spliterator(ctx: &dyn NativeContext, stream: ObjectRef) -> Option<ObjectRef> {
    if ctx.object_num_fields(stream) <= STREAM_FIELD_LAZY_SPLITERATOR {
        return None;
    }
    match ctx.get_field(stream, STREAM_FIELD_LAZY_SPLITERATOR) {
        Value::Object(Some(s)) => Some(s),
        _ => None,
    }
}

/// Drain a real `Spliterator` into an `Object[]` without fabricating anything —
/// the `--real-jdk` path for every `cratonvm/internal/StreamCollector` drain in
/// the workspace.
///
/// The collector those drains hand to `tryAdvance` is a VM-internal object that
/// exists only to BE a `Consumer`, and strict mode refuses to fabricate it. It
/// stands in for no JDK class, so unlike the refused iterator shapes there is
/// nothing to substitute for it — which is why this takes the other road and
/// removes the need for a `Consumer` at all.
///
/// `java.util.Spliterators.iterator(Spliterator)` is public JDK API and returns
/// a real `java.util.Spliterators$1Adapter` that is *itself* both the `Iterator`
/// and the `Consumer`, driving `tryAdvance` from real bytecode.
/// `probes/StrictIterPrimitivesProbe` measures that adapter working under
/// `--real-jdk` (`adapter.class=java.util.Spliterators$1Adapter`), which is what
/// makes this viable where a hand-built Consumer is not. `Spliterators` carries
/// no native override for `iterator`, so the call reaches the real method.
///
/// The accumulator is a real `java.util.ArrayList` rather than a Rust `Vec`:
/// every `next()` re-enters Java and may move the heap, and elements parked in
/// a Rust local across that are exactly the stale-ObjectRef family this file
/// keeps paying for. Growth is the JDK's problem, and only the iterator and the
/// list need pinning.
///
/// `safety_cap` keeps the guarantee the fabricated path had: an infinite
/// spliterator (`Stream.iterate`, a hand-rolled generator) stops rather than
/// exhausting the heap.
pub fn drain_spliterator_via_real_iterator(
    ctx: &mut dyn NativeContext,
    spl: ObjectRef,
    safety_cap: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let spl_pin = ctx.pin_native_root(spl);
    let spl_cur = ctx.read_native_pin(spl_pin, spl);
    let adapter = ctx.invoke(
        "java/util/Spliterators",
        "iterator",
        "(Ljava/util/Spliterator;)Ljava/util/Iterator;",
        &[Value::Object(Some(spl_cur))],
    );
    ctx.unpin_native_roots(spl_pin);
    let it = match adapter? {
        Some(Value::Object(Some(it))) => it,
        _ => return Ok(alloc_ref_array(ctx, 0)),
    };
    let it_pin = ctx.pin_native_root(it);
    let list = ctx.new_object_initialized("java/util/ArrayList", "()V", &[]);
    let list = match list {
        Ok(Some(Value::Object(Some(l)))) => l,
        Ok(_) => {
            ctx.unpin_native_roots(it_pin);
            return Ok(alloc_ref_array(ctx, 0));
        }
        Err(e) => {
            ctx.unpin_native_roots(it_pin);
            return Err(e);
        }
    };
    // Pinned ABOVE `it_pin`, so the single `unpin_native_roots(it_pin)` below
    // releases both — the pins are a stack, and unwinding to the lower base
    // drops everything above it.
    let list_pin = ctx.pin_native_root(list);
    let mut n = 0usize;
    let outcome = loop {
        if n >= safety_cap {
            break Ok(());
        }
        let it_cur = ctx.read_native_pin(it_pin, it);
        let has_next = match ctx.invoke_virtual(it_cur, "hasNext", "()Z", &[]) {
            Ok(Some(Value::Int(v))) => v != 0,
            Ok(_) => false,
            Err(e) => break Err(e),
        };
        if !has_next {
            break Ok(());
        }
        let it_cur = ctx.read_native_pin(it_pin, it);
        let elem = match ctx.invoke_virtual(it_cur, "next", "()Ljava/lang/Object;", &[]) {
            Ok(Some(v)) => v,
            Ok(None) => Value::Object(None),
            Err(e) => break Err(e),
        };
        // `elem` goes straight into the next Java call, whose argument list is
        // rooted for it — no allocation separates the two.
        let list_cur = ctx.read_native_pin(list_pin, list);
        if let Err(e) = ctx.invoke_virtual(list_cur, "add", "(Ljava/lang/Object;)Z", &[elem]) {
            break Err(e);
        }
        n += 1;
    };
    let list_cur = ctx.read_native_pin(list_pin, list);
    let arr = match outcome {
        Ok(()) => ctx.invoke_virtual(list_cur, "toArray", "()[Ljava/lang/Object;", &[]),
        Err(e) => Err(e),
    };
    ctx.unpin_native_roots(it_pin);
    match arr? {
        Some(Value::Object(Some(a))) => Ok(a),
        _ => Ok(alloc_ref_array(ctx, 0)),
    }
}

/// Drain a real `Spliterator` into an exact-sized `Object[]` via its
/// `tryAdvance(Consumer)` contract with a `cratonvm/internal/StreamCollector`
/// (the accept native, registered globally, appends + grows). Pins both objects
/// across the loop since every `tryAdvance` re-enters Java and may move them.
pub(crate) fn drain_spliterator_to_array(
    ctx: &mut dyn NativeContext,
    spl: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    drain_spliterator_to_array_capped(ctx, spl, 1_000_000)
}

/// As [`drain_spliterator_to_array`], but with a caller-chosen safety cap
/// instead of the shared default. Used by `stream_elements_concat_bounded`
/// (FMT-STREAMS-CCE) with a much smaller cap: `Stream.concat`'s bounded path
/// exists ONLY to survive an unbounded/infinite real-pipeline operand without
/// OOMing, and legitimate finite real-pipeline concat operands in practice
/// (e.g. Spring's `MergedAnnotations.stream()`-style use) are far smaller
/// than even a few thousand elements — a smaller cap keeps that safety net
/// cheap without weakening it for any real use case.
pub(crate) fn drain_spliterator_to_array_capped(
    ctx: &mut dyn NativeContext,
    spl: ObjectRef,
    safety_cap: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    // Fallible since 2026-08-05 (Real-JDK wave 2, lane L7). The refusal is not
    // fatal: `drain_spliterator_via_real_iterator` gets the same elements out
    // through public JDK API and keeps the same cap.
    // gc-common w20-a: `spl` is pinned BEFORE the two allocations below (it
    // was pinned after them, so `tryAdvance` was dispatched on its pre-move
    // address), and the collector is rooted across its storage's allocation
    // (the storage was stored through the collector's old address).
    let spl_pin = ctx.pin_native_root(spl);
    let collector = match try_alloc_synthetic(ctx, "cratonvm/internal/StreamCollector", 2) {
        Ok(c) => c,
        Err(_refused) => {
            let spl = ctx.read_native_pin(spl_pin, spl);
            ctx.unpin_native_roots(spl_pin);
            return drain_spliterator_via_real_iterator(ctx, spl, safety_cap);
        }
    };
    let col_pin = ctx.pin_native_root(collector);
    let storage: ObjectRef = alloc_ref_array(ctx, 16);
    let collector = ctx.read_native_pin(col_pin, collector);
    ctx.set_field(collector, 0, Value::Object(Some(storage)));
    ctx.set_field(collector, 1, Value::Int(0));
    let mut n = 0usize;
    loop {
        let s = ctx.read_native_pin(spl_pin, spl);
        let c = ctx.read_native_pin(col_pin, collector);
        match ctx.invoke_virtual(
            s,
            "tryAdvance",
            "(Ljava/util/function/Consumer;)Z",
            &[Value::Object(Some(c))],
        ) {
            Ok(Some(Value::Int(v))) if v != 0 => {
                n += 1;
                if n >= safety_cap {
                    break;
                }
            }
            Ok(_) => break,
            Err(e) => {
                ctx.unpin_native_roots(spl_pin);
                ctx.unpin_native_roots(col_pin);
                return Err(e);
            }
        }
    }
    let col = ctx.read_native_pin(col_pin, collector);
    let len = match ctx.get_field(col, 1) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    let storage = match ctx.get_field(col, 0) {
        Value::Object(Some(a)) => a,
        _ => {
            ctx.unpin_native_roots(spl_pin);
            return Ok(alloc_ref_array(ctx, 0));
        }
    };
    // Family-1 fix (cce0079): the `out` alloc below can trigger a moving GC.
    // The previous code UNPINNED the collector before this point, so the
    // collector + its storage array became garbage, the copy loop read a
    // reclaimed array, and stale/recycled element refs were returned as the
    // stream's contents — surfacing later as the WildFly domain-boot CCE /
    // stale-canary firings in `native_stream_to_array_gen` and
    // `get_array_element` (DE_002/DE_003 captures). Keep `storage` pinned
    // across the alloc and copy from the refreshed address.
    let storage_pin = ctx.pin_native_root(storage);
    let out: ObjectRef = alloc_ref_array(ctx, len);
    let storage = ctx.read_native_pin(storage_pin, storage);
    for i in 0..len {
        let v = ctx.get_array_element(storage, i);
        ctx.set_array_element(out, i, v);
    }
    ctx.unpin_native_roots(spl_pin);
    Ok(out)
}

/// If `stream` is lazy (holds a source spliterator in slot 2 and has no
/// materialised element array yet), drain the spliterator into slot 0 and clear
/// the lazy slot. Idempotent; a no-op for ordinary streams. Called at the top of
/// `stream_elements` so EVERY non-forEach op transparently materialises.
/// # `stream` is `&mut` — `WORKER-5-NOTE-12` N3 (the depth-2 pass)
///
/// Materialising a lazy stream allocates, and the one caller goes straight on
/// to `ctx.class_id_of_object(stream)`. Same shape as `tm_sync_native_state`;
/// see `WORKER-5-NOTE-10` §7 for why the receiver is `&mut` rather than a pin
/// at the call site.
pub(crate) fn materialize_lazy_stream(
    ctx: &mut dyn NativeContext,
    stream_out: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    let pin = ctx.pin_native_root(*stream_out);
    let r = materialize_lazy_stream_inner(ctx, *stream_out);
    *stream_out = ctx.read_native_pin(pin, *stream_out);
    ctx.unpin_native_roots(pin);
    r
}

pub(crate) fn materialize_lazy_stream_inner(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let spl = match stream_lazy_spliterator(ctx, stream) {
        Some(s) => s,
        None => return Ok(()),
    };
    // GC-SAFETY: `drain_spliterator_to_array` drives the source spliterator,
    // which for a `StackWalker` walk allocates heavily (one `StackFrameInfo` +
    // `StackTraceElement` + strings per frame) and can trigger a moving GC that
    // relocates `stream`. Writing the result back through the stale `stream`
    // reference corrupts the heap (zeroed `ClassId(0)` headers / wild
    // `Object.toArray` dispatch) — the `ByteArrayMappingTests` SIGSEGV. Pin
    // `stream` across the drain and read the forwarded reference back.
    let stream_pin = ctx.pin_native_root(stream);
    let arr = match drain_spliterator_to_array(ctx, spl) {
        Ok(arr) => arr,
        Err(e) => {
            ctx.unpin_native_roots(stream_pin);
            return Err(e);
        }
    };
    let stream = ctx.read_native_pin(stream_pin, stream);
    ctx.set_field(stream, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
    ctx.set_field(stream, STREAM_FIELD_LAZY_SPLITERATOR, Value::Object(None));
    ctx.unpin_native_roots(stream_pin);
    Ok(())
}

/// Create a Stream from a slice of values.
/// Public constructor for a fully-functional synthetic `java/util/stream/Stream`
/// backed by a fixed element snapshot. Other native modules (e.g. the NIO
/// `Files.list`/`Files.walk` shims in `native-builtins`) use this to return a
/// working eager Stream without depending on the real `ReferencePipeline`
/// machinery.
pub fn make_stream_from_elements(
    ctx: &mut dyn NativeContext,
    elements: &[Value],
) -> MethodCallResult {
    // Round 17 wave 3 (lane streams3): see
    // [`strict_elements_stream_jdk_pipeline_enabled`].
    if ctx.is_jdk_only() && strict_elements_stream_jdk_pipeline_enabled() {
        return make_jdk_array_stream(ctx, elements);
    }
    make_stream(ctx, elements)
}

/// `Arrays.stream(Object[])` over a fresh array of `elements`: the JDK's own
/// `ReferencePipeline$Head` (`ORDERED | SIZED | SUBSIZED | IMMUTABLE`). Every
/// element is pinned from entry until it is stored; the array is pinned
/// across the call.
pub(crate) fn make_jdk_array_stream(ctx: &mut dyn NativeContext, elements: &[Value]) -> MethodCallResult {
    let (elem_base, elem_handles) = pin_value_slice(ctx, elements);
    let arr = alloc_ref_array(ctx, elements.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, val) in elements.iter().enumerate() {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let val = read_pinned_elem(ctx, elem_handles[i], *val);
        ctx.set_array_element(arr, i, val);
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    let r = ctx.invoke(
        "java/util/Arrays",
        "stream",
        "([Ljava/lang/Object;)Ljava/util/stream/Stream;",
        &[Value::Object(Some(arr))],
    );
    ctx.unpin_native_roots(elem_base.min(arr_pin));
    r
}

pub(crate) fn make_stream(ctx: &mut dyn NativeContext, elements: &[Value]) -> MethodCallResult {
    make_stream_with_fields(ctx, elements, STREAM_NUM_FIELDS)
}

/// [`make_stream`] with `num_fields` slots ([`STREAM_NUM_FIELDS`], or
/// [`WALK_STREAM_NUM_FIELDS`] for a stream derived from a walk's stream).
pub(crate) fn make_stream_with_fields(
    ctx: &mut dyn NativeContext,
    elements: &[Value],
    num_fields: usize,
) -> MethodCallResult {
    let (elem_base, elem_handles) = pin_value_slice(ctx, elements);
    let stream = try_alloc_synthetic(ctx, "java/util/stream/Stream", num_fields)?;
    let stream_pin = ctx.pin_native_root(stream);
    let arr = alloc_ref_array(ctx, elements.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, val) in elements.iter().enumerate() {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let val = read_pinned_elem(ctx, elem_handles[i], *val);
        ctx.set_array_element(arr, i, val);
    }
    let stream = ctx.read_native_pin(stream_pin, stream);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(stream, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
    ctx.set_field(stream, STREAM_FIELD_CLOSE_HANDLERS, Value::Object(None));
    // W7-65: stated here rather than left to `try_alloc_zeroed` two crates
    // away, so the "a fresh stream is unlinked" invariant lives where the
    // layout is built. Slots 2 and 3 still rely on that zeroing, which is why
    // only the exact `Int(1)` is read as linked.
    ctx.set_field(stream, STREAM_FIELD_LINKED, Value::Int(0));
    ctx.unpin_native_roots(if elem_base == usize::MAX {
        stream_pin
    } else {
        elem_base
    });
    Ok(Some(Value::Object(Some(stream))))
}

/// Copy `src`'s close-handler array onto `dst` (intermediate-op propagation).
/// Shares the array ref — `onClose` appends into a fresh array, so this never
/// mutates `src`'s handlers, and `close()` only clears the per-stream field.
pub(crate) fn stream_inherit_close_handlers(ctx: &mut dyn NativeContext, src: ObjectRef, dst: ObjectRef) {
    if src == dst {
        return;
    }
    // W1 fix: several ad-hoc synthetic-stream allocations use a 1-field
    // (elements-only) layout with no close-handler slot. Guard slot-1 access the
    // same way the LAZY_SPLITERATOR (slot 2) / OP_CHAIN (slot 3) readers already
    // do — otherwise this intermediate-op propagation OOB-reads slot 1 on every
    // chained op against such a source, flooding the GC out-of-bounds-field
    // guard. A short-layout stream simply has no handlers to inherit.
    if ctx.object_num_fields(src) <= STREAM_FIELD_CLOSE_HANDLERS
        || ctx.object_num_fields(dst) <= STREAM_FIELD_CLOSE_HANDLERS
    {
        return;
    }
    // Round 18 wave 11 (lane streams5): one close action per pipeline. `dst`
    // names the pipeline's close ROOT instead of copying its current array,
    // so a handler registered on any stage later is seen by every stage
    // ([`stream_one_close_action_enabled`], [`stream_close_root`]).
    if stream_one_close_action_enabled() {
        let root = stream_close_root(&*ctx, src);
        if root != dst && is_synthetic_stream_object(&*ctx, root) {
            ctx.set_field(dst, STREAM_FIELD_CLOSE_HANDLERS, Value::Object(Some(root)));
        }
        return;
    }
    if let Value::Object(Some(arr)) = ctx.get_field(src, STREAM_FIELD_CLOSE_HANDLERS) {
        ctx.set_field(dst, STREAM_FIELD_CLOSE_HANDLERS, Value::Object(Some(arr)));
    }
}

/// `CRATONVM_STREAM_ONE_CLOSE_ACTION` -- default ON, `=0` restores the
/// per-stage handler arrays. JDK 25 keeps ONE close action per pipeline, on
/// the source stage (`AbstractPipeline.sourceStage.sourceCloseAction`):
/// `onClose` on any stage appends to it, and `close()` on any stage runs it
/// once, in registration order, and clears it. A synthetic stage copied its
/// upstream's handler ARRAY when it was built, so a handler registered later
/// on a downstream stage did not run when an upstream stage was closed
/// (`s.map(f).onClose(r); s.close()` ran nothing; HotSpot runs `r`), and
/// closing two stages ran the shared handlers twice. With the switch on, a
/// derived stage's close slot names the pipeline's ROOT stage
/// ([`stream_close_root`]), which alone holds the handler array. JIT round 18
/// wave 11, lane streams5. Read live, per call.
pub(crate) fn stream_one_close_action_enabled() -> bool {
    true
}

/// The stage that holds `stream`'s pipeline close handlers: `stream` itself,
/// unless its close slot names another synthetic stream carrier (the root a
/// derived stage links to, [`stream_inherit_close_handlers`]); then that one.
/// A root's slot holds the handler array (or null), never another stage, so
/// one hop is the normal walk; the bound is a guard. Pure reads.
pub(crate) fn stream_close_root(ctx: &dyn NativeContext, stream: ObjectRef) -> ObjectRef {
    let mut cur = stream;
    for _ in 0..8 {
        if ctx.object_num_fields(cur) <= STREAM_FIELD_CLOSE_HANDLERS {
            break;
        }
        match ctx.get_field(cur, STREAM_FIELD_CLOSE_HANDLERS) {
            Value::Object(Some(o))
                if o != cur
                    && ctx.heap_kind_of(o) != ObjectKind::Array
                    && is_synthetic_stream_object(ctx, o) =>
            {
                cur = o;
            }
            _ => break,
        }
    }
    cur
}

/// `make_stream` for an intermediate op: build the result, then inherit `src`'s
/// close handlers so a later `close()` on the result runs the upstream handlers.
///
/// A result derived from a `StackWalker.walk` stream, or from a stream
/// derived from one, names that walk stream ([`STREAM_FIELD_WALK`]), so it
/// refuses once the walk has returned, as HotSpot's pipeline over the walk's
/// spliterator does (interpreter round i1 wave 44, lane L3).
///
/// `op` is the intermediate operation the result stands for: it decides the
/// spliterator facts the result records ([`stream_derive_spl_facts`]), read
/// from `src` here, before anything allocates.
pub(crate) fn make_derived_stream(
    ctx: &mut dyn NativeContext,
    src: ObjectRef,
    elements: &[Value],
    op: SplOp,
) -> MethodCallResult {
    let facts = stream_derive_spl_facts(&*ctx, src, op);
    let walk = stream_walk_origin(ctx, src);
    let src_pin = ctx.pin_native_root(src);
    // Pinned after `src`: unpinning from `src_pin` releases both.
    let walk_pin = walk.map(|walk| ctx.pin_native_root(walk));
    let num_fields = match walk {
        // THROWS5-3: a lazy walk's derived streams carry its slice slot.
        Some(walk) => walk_derived_num_fields(&*ctx, walk),
        None => STREAM_NUM_FIELDS,
    };
    let r = match make_stream_with_fields(ctx, elements, num_fields) {
        Ok(r) => r,
        Err(e) => {
            ctx.unpin_native_roots(src_pin);
            return Err(e);
        }
    };
    if let Some(Value::Object(Some(dst))) = &r {
        if let Some(packed) = facts {
            stream_set_spl_facts(ctx, *dst, packed);
        }
        let src = ctx.read_native_pin(src_pin, src);
        stream_inherit_close_handlers(ctx, src, *dst);
        if let (Some(walk), Some(walk_pin)) = (walk, walk_pin) {
            let walk = ctx.read_native_pin(walk_pin, walk);
            ctx.set_field(*dst, STREAM_FIELD_WALK, Value::Object(Some(walk)));
        }
        // Round 17 wave 8 (lane streams4): `src` now has a next stage.
        // Pure stores; `src` was re-read above and nothing since allocates.
        stream_record_next_stage(ctx, src, *dst);
    }
    ctx.unpin_native_roots(src_pin);
    Ok(r)
}

// ---------------------------------------------------------------------------
// Spliterator facts of a synthetic stream stage (JIT round 16 wave 9, lane
// spliter; `r16-collections-compatible-spliterator-shims-diverge-FIXED-20261001.md`).
//
// HotSpot's `stream.spliterator()` is the SOURCE spliterator for a stage with
// no intermediate op (`Stream.of(..)` -> an `ArraySpliterator`, `0x4450`,
// splittable), and a `WrappingSpliterator` otherwise: characteristics
// `StreamOpFlag.toCharacteristics(flags)` with SIZED/SUBSIZED taken from the
// source, `getComparator()` null when SORTED, `trySplit()` null for a
// sequential pipeline, and `estimateSize()` the SOURCE's estimate. The
// synthetic stream is an element snapshot that knew none of that, so every
// `spliterator()` answered the list default `0x4050` and split. The facts
// below are what the snapshot was missing; `native_stream_spliterator` copies
// them onto the spliterator it builds.
// ---------------------------------------------------------------------------

/// What an intermediate op does to the characteristics of the stage it
/// builds: its `StreamOpFlag` set, restricted to the four flags
/// `StreamOpFlag.toCharacteristics` maps back. MEASURED arm by arm against
/// HotSpot 25 (`R16SpliterW9`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SplOp {
    /// `peek`, `boxed`, `limit`, `skip` (size-adjusting slices keep SIZED;
    /// the estimate is then the snapshot's own, already-sliced length).
    Preserve,
    /// `map`, `mapToObj`: `NOT_SORTED | NOT_DISTINCT`.
    Map,
    /// `filter`: `NOT_SIZED`.
    Filter,
    /// `flatMap`: `NOT_SORTED | NOT_DISTINCT | NOT_SIZED`.
    FlatMap,
    /// `sorted()`: `IS_ORDERED | IS_SORTED`.
    SortedNatural,
    /// `sorted(Comparator)`: `IS_ORDERED | NOT_SORTED`.
    SortedCmp,
    /// `distinct()`: `IS_DISTINCT | NOT_SIZED`.
    Distinct,
    /// `unordered()` on an ORDERED stage: `NOT_ORDERED` (round 16 wave 13,
    /// lane compatcoll; [`native_stream_unordered`]).
    Unordered,
    /// A PRIMITIVE `distinct()`: JDK 25 `IntPipeline.distinct()` is
    /// `boxed().distinct().mapToInt(i -> i)`, so `IS_DISTINCT | NOT_SIZED`
    /// followed by `NOT_SORTED | NOT_DISTINCT` (round 17 wave 1, lane
    /// streams; [`prim_stage_spl_facts`]).
    PrimDistinct,
    /// `NOT_DISTINCT` alone: `LongStream.asDoubleStream()` (round 18 wave 11,
    /// lane streams5; [`stage_class_spl_op`]).
    NotDistinct,
}

/// The four stream flags `StreamOpFlag.toCharacteristics` maps back.
pub(crate) const SPL_STREAM_FLAG_BITS: i32 = SPL_DISTINCT | SPL_SORTED | SPL_ORDERED | SPL_SIZED;

pub(crate) fn spl_apply_op(flags: i32, op: SplOp) -> i32 {
    match op {
        SplOp::Preserve => flags,
        SplOp::Map => flags & !(SPL_SORTED | SPL_DISTINCT),
        SplOp::Filter => flags & !SPL_SIZED,
        SplOp::FlatMap => flags & !(SPL_SORTED | SPL_DISTINCT | SPL_SIZED),
        SplOp::SortedNatural => flags | SPL_SORTED | SPL_ORDERED,
        SplOp::SortedCmp => (flags | SPL_ORDERED) & !SPL_SORTED,
        SplOp::Distinct => (flags | SPL_DISTINCT) & !SPL_SIZED,
        // Round 18 wave 11 (lane streams5): HotSpot clears SORTED with
        // ORDERED (`TreeSet.stream().unordered()` 0x55 -> 0x41,
        // `IntStream.range(..).unordered()` 0x4555 -> 0x4041).
        SplOp::Unordered if stream_unordered_hotspot_enabled() => {
            flags & !(SPL_ORDERED | SPL_SORTED)
        }
        SplOp::Unordered => flags & !SPL_ORDERED,
        SplOp::PrimDistinct => flags & !(SPL_SIZED | SPL_SORTED | SPL_DISTINCT),
        SplOp::NotDistinct => flags & !SPL_DISTINCT,
    }
}

/// `WrappingSpliterator.characteristics()` for a stage `op` builds over an
/// upstream reporting `upstream` (public bits only): the op applied to the
/// stream flags, then SIZED | SUBSIZED replaced by the source's own pair. The
/// upstream's SUBSIZED is the source's whenever the upstream is still SIZED,
/// and SIZED cannot come back once an op clears it, so carrying it forward is
/// exact.
pub(crate) fn spl_wrapped_characteristics(upstream: i32, op: SplOp) -> i32 {
    let flags = spl_apply_op(upstream & SPL_STREAM_FLAG_BITS, op);
    if flags & SPL_SIZED != 0 {
        flags | (upstream & SPL_SUBSIZED)
    } else {
        flags
    }
}

/// Bit 63 of [`STREAM_FIELD_SPL_FACTS`]: the stage is parallel
/// (`BaseStream.parallel()` was called on it, or on a stage it derives from,
/// and no `sequential()` since). It is not a characteristic, so it rides
/// outside the low 32 bits; [`pack_spl_facts`] caps the head size below it.
/// A slot holding only this bit describes no characteristics
/// ([`stream_spl_facts`] answers `None`). Round 16 wave 14, lane compat2
/// (proposal CC13-3; [`stream_parallel_stage_enabled`]).
pub(crate) const STREAM_FACTS_PARALLEL: i64 = i64::MIN;

/// Pack a stage's facts for [`STREAM_FIELD_SPL_FACTS`]. The head size is
/// capped at `i32::MAX - 1` (no Java array is longer) so the packed value
/// never reaches [`STREAM_FACTS_PARALLEL`].
pub(crate) fn pack_spl_facts(chars: i32, head_size: Option<usize>) -> i64 {
    let hi: u64 = head_size
        .map(|n| (n as u64).min(u64::from(i32::MAX as u32) - 1) + 1)
        .unwrap_or(0);
    ((hi << 32) | u64::from(chars as u32)) as i64
}

/// The `(characteristics, head source size)` a stage recorded, or `None` for
/// a stage nothing described (and for every short-layout stream).
pub(crate) fn stream_spl_facts(ctx: &dyn NativeContext, stream: ObjectRef) -> Option<(i32, Option<usize>)> {
    if ctx.object_num_fields(stream) <= STREAM_FIELD_SPL_FACTS {
        return None;
    }
    match ctx.get_field(stream, STREAM_FIELD_SPL_FACTS) {
        Value::Long(p) if p & !STREAM_FACTS_PARALLEL != 0 => {
            let p = p & !STREAM_FACTS_PARALLEL;
            let chars = p as u64 as u32 as i32;
            let hi = (p as u64 >> 32) as usize;
            Some((chars, hi.checked_sub(1)))
        }
        _ => None,
    }
}

/// The raw [`STREAM_FIELD_SPL_FACTS`] word, `0` for a short-layout stream or
/// an unwritten slot. Pure read.
pub(crate) fn stream_spl_facts_word(ctx: &dyn NativeContext, stream: ObjectRef) -> i64 {
    if ctx.object_num_fields(stream) <= STREAM_FIELD_SPL_FACTS {
        return 0;
    }
    match ctx.get_field(stream, STREAM_FIELD_SPL_FACTS) {
        Value::Long(p) => p,
        _ => 0,
    }
}

/// Record `packed` on `stream`, keeping its [`STREAM_FACTS_PARALLEL`] bit. No
/// allocation; a short-layout stream is left alone.
pub(crate) fn stream_set_spl_facts(ctx: &mut dyn NativeContext, stream: ObjectRef, packed: i64) {
    if ctx.object_num_fields(stream) > STREAM_FIELD_SPL_FACTS {
        let parallel = stream_spl_facts_word(&*ctx, stream) & STREAM_FACTS_PARALLEL;
        ctx.set_field(
            stream,
            STREAM_FIELD_SPL_FACTS,
            Value::Long(packed | parallel),
        );
    }
}

/// Whether `stream` is a parallel stage ([`STREAM_FACTS_PARALLEL`]). False
/// for every stream that cannot carry the bit, and under the kill switch.
/// Pure reads.
pub(crate) fn stream_is_parallel(ctx: &dyn NativeContext, stream: ObjectRef) -> bool {
    // Round 17 wave 8 (lane streams4): the pipeline's mode is its tail's bit
    // ([`STREAM_FIELD_NEXT_STAGE`]); a stage with no next stage is its own tail.
    let stream = stream_pipeline_tail(ctx, stream);
    // The bit first: the common sequential stage never reads the switch.
    stream_spl_facts_word(ctx, stream) & STREAM_FACTS_PARALLEL != 0
        && stream_parallel_stage_enabled()
}

/// `CRATONVM_STREAM_SHARED_PARALLEL` -- default ON, `=0` restores the
/// per-stage bit. JIT round 17 wave 8, lane streams4
/// (`r17-streamsupport-lazy-carrier-residual-shapes-FIXED-20261005.md` residual 5,
/// proposal C2-4 / STR17-2 / STREAMS3-1). JDK 25 `AbstractPipeline.parallel()`
/// is `sourceStage.parallel = true; return this;` and `isParallel()` reads
/// `sourceStage.parallel`, so every stage of one pipeline answers the same
/// mode: `s.map(f).parallel(); s.isParallel()` is `true` on HotSpot. A
/// synthetic stage kept its own bit (bit 63 of [`STREAM_FIELD_SPL_FACTS`]) and
/// answered `false`. With the switch on, a reference stage records the stage
/// built over it ([`stream_record_next_stage`]), `isParallel()` reads the
/// pipeline's tail ([`stream_pipeline_tail`]) and `parallel()` /
/// `sequential()` write the tail as well as the receiver. Only the recording
/// reads the switch: with it off no next stage is ever written, so the tail
/// of every stage is itself. Read live, per call.
pub(crate) fn stream_shared_parallel_enabled() -> bool {
    true
}

/// Bound on [`stream_pipeline_tail`]'s walk. A pipeline has one stage per
/// intermediate op the program wrote; the bound is a guard against a slot
/// some other writer filled, not a policy.
pub(crate) const STREAM_PIPELINE_TAIL_MAX_STEPS: usize = 1 << 16;

/// The newest stage of the pipeline `stream` belongs to: follow
/// [`STREAM_FIELD_NEXT_STAGE`] while it names another synthetic carrier wide
/// enough for the parallel bit. `stream` itself when it has no next stage
/// (every stage under the kill switch, every primitive carrier, every
/// short-layout carrier and every real pipeline stage). Pure reads.
pub(crate) fn stream_pipeline_tail(ctx: &dyn NativeContext, stream: ObjectRef) -> ObjectRef {
    let mut cur = stream;
    for _ in 0..STREAM_PIPELINE_TAIL_MAX_STEPS {
        if ctx.object_num_fields(cur) <= STREAM_FIELD_NEXT_STAGE {
            break;
        }
        // Slot 7 of a REAL pipeline stage is one of its own fields: only a
        // synthetic carrier's slot is a next stage. The class reads run only
        // when the slot holds an object, so a stage with no next stage (the
        // common case) pays one field read.
        match ctx.get_field(cur, STREAM_FIELD_NEXT_STAGE) {
            Value::Object(Some(next))
                if next != cur
                    && stream_carries_parallel_bit(ctx, cur)
                    && stream_carries_parallel_bit(ctx, next) =>
            {
                cur = next;
            }
            _ => break,
        }
    }
    cur
}

/// `dst` is the stage an intermediate op built over `src`: record it as
/// `src`'s next stage ([`STREAM_FIELD_NEXT_STAGE`]) when both are synthetic
/// REFERENCE carriers at a layout that has the slot. Reference only, because
/// only a reference stage is linked once (`stream_link_or_consume`): a
/// primitive carrier can be derived from twice, and its next stage would then
/// name whichever came last. No-op under the kill switch
/// ([`stream_shared_parallel_enabled`]). Pure stores, no allocation.
pub(crate) fn stream_record_next_stage(ctx: &mut dyn NativeContext, src: ObjectRef, dst: ObjectRef) {
    if src == dst
        || ctx.object_num_fields(src) <= STREAM_FIELD_NEXT_STAGE
        || ctx.object_num_fields(dst) <= STREAM_FIELD_NEXT_STAGE
        || !stream_is_reference_carrier(&*ctx, src)
        || !stream_is_reference_carrier(&*ctx, dst)
        || !stream_shared_parallel_enabled()
    {
        return;
    }
    ctx.set_field(src, STREAM_FIELD_NEXT_STAGE, Value::Object(Some(dst)));
}

// ---------------------------------------------------------------------------
// Stage classes and primitive / cross-shape stage links (JIT round 18 wave 10,
// lane streams5; `r17-compatible-synthetic-stream-stage-getclass-is-the-head-
// 20261005.md`, `r17-synthetic-primitive-stream-stages-keep-their-own-parallel-
// bit-20261005.md`).
//
// Every intermediate-op native of the four synthetic stream interfaces is
// registered through [`stream_stage_op`] (or [`stream_slice_op`]) with the
// HotSpot class of the stage that op builds. The wrapper is the one place that
// sees both the receiver and the new stage, so it (1) records the stage's
// class ([`STREAM_FIELD_STAGE_CLASS`]) for `getClass()`, and (2) links the
// receiver to the new stage across every shape (reference, primitive, and
// reference <-> primitive), so one pipeline has one parallel mode.
// ---------------------------------------------------------------------------

/// The HotSpot 25 class of each intermediate stage the synthetic natives
/// model. MEASURED on jdk-25.0.3.9-hotspot (lane streams5,
/// `R18Streams5Stages`), not derived from source order: the anonymous
/// classes are numbered in declaration order inside `ReferencePipeline` /
/// `IntPipeline` / ..., so `flatMapToLong` is `$10` and `flatMapToDouble` `$9`,
/// and a primitive `distinct()` (`boxed().distinct().mapTo*(..)`) is the
/// REFERENCE pipeline's `mapToInt` / `mapToLong` / `mapToDouble` class.
/// The discriminant is the index into [`STREAM_STAGE_CLASS_NAMES`]; `0` is
/// reserved for a source stage (no entry: the stamp's `Head` alias).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(i32)]
pub(crate) enum StageClass {
    RefUnordered = 1,
    RefFilter = 2,
    RefMap = 3,
    /// `Stream.mapToInt`, and `IntStream.distinct()`.
    RefMapToInt = 4,
    /// `Stream.mapToLong`, and `LongStream.distinct()`.
    RefMapToLong = 5,
    /// `Stream.mapToDouble`, and `DoubleStream.distinct()`.
    RefMapToDouble = 6,
    RefFlatMap = 7,
    RefFlatMapToInt = 8,
    RefFlatMapToDouble = 9,
    RefFlatMapToLong = 10,
    RefPeek = 11,
    RefSorted = 12,
    RefDistinct = 13,
    RefSlice = 14,
    /// `IntStream.mapToObj` and `boxed()`.
    IntMapToObj = 15,
    IntAsLong = 16,
    IntAsDouble = 17,
    IntMap = 18,
    IntMapToLong = 19,
    IntMapToDouble = 20,
    IntFlatMap = 21,
    IntFilter = 22,
    IntPeek = 23,
    IntSorted = 24,
    IntSlice = 25,
    /// `LongStream.mapToObj` and `boxed()`.
    LongMapToObj = 26,
    LongAsDouble = 27,
    LongMap = 28,
    LongMapToInt = 29,
    LongMapToDouble = 30,
    LongFlatMap = 31,
    LongFilter = 32,
    LongPeek = 33,
    LongSorted = 34,
    LongSlice = 35,
    /// `DoubleStream.mapToObj` and `boxed()`.
    DoubleMapToObj = 36,
    DoubleMap = 37,
    DoubleMapToInt = 38,
    DoubleMapToLong = 39,
    DoubleFlatMap = 40,
    DoubleFilter = 41,
    DoublePeek = 42,
    DoubleSorted = 43,
    DoubleSlice = 44,
    // Round 18 wave 11 (lane streams5): the primitive `unordered()` stage.
    IntUnordered = 45,
    LongUnordered = 46,
    DoubleUnordered = 47,
}

/// [`StageClass`] discriminant -> internal class name. Index 0 is the source
/// stage and is never answered.
pub(crate) const STREAM_STAGE_CLASS_NAMES: [&str; 48] = [
    "",
    "java/util/stream/ReferencePipeline$1",
    "java/util/stream/ReferencePipeline$2",
    "java/util/stream/ReferencePipeline$3",
    "java/util/stream/ReferencePipeline$4",
    "java/util/stream/ReferencePipeline$5",
    "java/util/stream/ReferencePipeline$6",
    "java/util/stream/ReferencePipeline$7",
    "java/util/stream/ReferencePipeline$8",
    "java/util/stream/ReferencePipeline$9",
    "java/util/stream/ReferencePipeline$10",
    "java/util/stream/ReferencePipeline$15",
    "java/util/stream/SortedOps$OfRef",
    "java/util/stream/DistinctOps$1",
    "java/util/stream/SliceOps$1",
    "java/util/stream/IntPipeline$1",
    "java/util/stream/IntPipeline$2",
    "java/util/stream/IntPipeline$3",
    "java/util/stream/IntPipeline$4",
    "java/util/stream/IntPipeline$5",
    "java/util/stream/IntPipeline$6",
    "java/util/stream/IntPipeline$7",
    "java/util/stream/IntPipeline$10",
    "java/util/stream/IntPipeline$11",
    "java/util/stream/SortedOps$OfInt",
    "java/util/stream/SliceOps$2",
    "java/util/stream/LongPipeline$1",
    "java/util/stream/LongPipeline$2",
    "java/util/stream/LongPipeline$3",
    "java/util/stream/LongPipeline$4",
    "java/util/stream/LongPipeline$5",
    "java/util/stream/LongPipeline$6",
    "java/util/stream/LongPipeline$9",
    "java/util/stream/LongPipeline$10",
    "java/util/stream/SortedOps$OfLong",
    "java/util/stream/SliceOps$3",
    "java/util/stream/DoublePipeline$1",
    "java/util/stream/DoublePipeline$2",
    "java/util/stream/DoublePipeline$3",
    "java/util/stream/DoublePipeline$4",
    "java/util/stream/DoublePipeline$5",
    "java/util/stream/DoublePipeline$8",
    "java/util/stream/DoublePipeline$9",
    "java/util/stream/SortedOps$OfDouble",
    "java/util/stream/SliceOps$4",
    "java/util/stream/IntPipeline$9",
    "java/util/stream/LongPipeline$8",
    "java/util/stream/DoublePipeline$7",
];

/// `CRATONVM_STREAM_STAGE_DISPLAY_CLASS` -- default ON, `=0` restores the
/// `*Pipeline$Head` answer for every stage. Under `--compatible` every
/// synthetic stage was stamped with its stream INTERFACE and `getClass()`
/// mapped the stamp to the pipeline's `Head`, so
/// `Stream.of(1).filter(p).getClass().getName()` was
/// `java.util.stream.ReferencePipeline$Head` where HotSpot answers
/// `java.util.stream.ReferencePipeline$2` (and `toString()`'s class part
/// with it). Only the reader ([`synthetic_stream_stage_display_name`]) reads
/// the switch; the stamp is always recorded. Read live, per call.
pub(crate) fn stream_stage_display_class_enabled() -> bool {
    true
}

/// `CRATONVM_STREAM_SHARED_PARALLEL_PRIMITIVE` -- default ON, `=0` restores
/// the wave-8 reference-only links. Wave 8 ([`stream_shared_parallel_enabled`])
/// gave a REFERENCE pipeline one parallel mode; primitive stages and the
/// cross-shape builders (`Stream.mapToInt`, `IntStream.boxed` / `mapToObj` /
/// `asLongStream`, ...) still kept their own bit, so
/// `IntStream s = IntStream.range(0, 3); s.map(f).parallel(); s.isParallel()`
/// answered `false` (HotSpot: `true`, `AbstractPipeline.sourceStage.parallel`),
/// and a cross-shape stage did not even inherit its upstream's mode
/// (`Stream.of(1).parallel().mapToInt(f).isParallel()` was `false`). With the
/// switch on, [`stream_stage_op`] gives the new stage the upstream
/// pipeline's mode and records it as the upstream's next stage for every
/// shape. A primitive stage is not linked-once (W7-65), so a second
/// derivation from the same primitive stage re-points its next stage to the
/// newer one; HotSpot throws `IllegalStateException` there, so no conforming
/// program can see the difference. Read live, per call.
pub(crate) fn stream_shared_parallel_primitive_enabled() -> bool {
    true
}

/// `CRATONVM_STREAM_SLICE_ARG_CHECK` -- default ON, `=0` restores the old
/// answers. JDK 25 `ReferencePipeline.skip(n)` / `limit(n)` (and the three
/// primitive twins) throw `IllegalArgumentException(Long.toString(n))` for a
/// negative `n`, before linking, and `skip(0)` answers `this` without linking
/// it. The synthetic natives cast `n` to `usize` (`limit(-1)` kept every
/// element, `skip(-1)` dropped every element) and `skip(0)` built and linked
/// a new stage, so `s.skip(0) == s` was `false` and a later `s.map(f)` threw
/// `IllegalStateException`. Read live, per call.
pub(crate) fn stream_slice_arg_check_enabled() -> bool {
    true
}

/// Record `stage` as `dst`'s HotSpot class ([`STREAM_FIELD_STAGE_CLASS`]).
/// `dst` must be a synthetic carrier ([`stream_carries_parallel_bit`]); a
/// carrier too short for the slot is left alone. Pure store.
pub(crate) fn stream_stamp_stage_class(ctx: &mut dyn NativeContext, dst: ObjectRef, stage: StageClass) {
    if ctx.object_num_fields(dst) > STREAM_FIELD_STAGE_CLASS {
        ctx.set_field(dst, STREAM_FIELD_STAGE_CLASS, Value::Int(stage as i32));
    }
}

/// [`stream_stamp_stage_class`] on the stage a native answered in `r`, when
/// it is a synthetic carrier. Pure reads and one store.
pub(crate) fn stream_stamp_result(ctx: &mut dyn NativeContext, r: &MethodCallResult, stage: StageClass) {
    if let Ok(Some(Value::Object(Some(dst)))) = r {
        if stream_carries_parallel_bit(&*ctx, *dst) {
            stream_stamp_stage_class(ctx, *dst, stage);
        }
    }
}

/// The internal class name `getClass()` should report for a synthetic
/// stream carrier that an intermediate op built
/// ([`STREAM_FIELD_STAGE_CLASS`]), e.g. `java/util/stream/ReferencePipeline$2`
/// for a `filter` stage. `None` for a source stage, for any object that is
/// not a full-layout synthetic stream carrier, and under
/// [`stream_stage_display_class_enabled`]'s kill switch: the caller then keeps
/// the stamp's `*Pipeline$Head` alias. Pure reads, no allocation.
pub fn synthetic_stream_stage_display_name(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
) -> Option<&'static str> {
    if ctx.object_num_fields(obj) <= STREAM_FIELD_STAGE_CLASS
        || !is_synthetic_stream_object(ctx, obj)
    {
        return None;
    }
    let Value::Int(code) = ctx.get_field(obj, STREAM_FIELD_STAGE_CLASS) else {
        return None;
    };
    if code <= 0 || !stream_stage_display_class_enabled() {
        return None;
    }
    STREAM_STAGE_CLASS_NAMES.get(code as usize).copied()
}

/// Whether `class_name` (internal form) is one of the four stream INTERFACES
/// a synthetic stream carrier is stamped with -- the stamps whose
/// `getClass()` display is per object ([`synthetic_stream_stage_display_name`]).
pub fn is_synthetic_stream_class_name(class_name: &str) -> bool {
    is_synthetic_stream(class_name)
}

/// Every class name [`synthetic_stream_stage_display_name`] can answer (index
/// 0, the source stage's empty name, included; it names no class). JIT round
/// 20 wave 12, lane qMisc (NIO20-L3-1): native-builtins'
/// `getclass_reports_only_itself` reads the very table the display arm reads,
/// so the two cannot drift.
pub fn synthetic_stream_stage_display_names() -> &'static [&'static str] {
    &STREAM_STAGE_CLASS_NAMES
}

/// Set or clear `stream`'s own [`STREAM_FACTS_PARALLEL`] bit, keeping its
/// facts. No store when the bit is already right. Pure reads and one store.
pub(crate) fn stream_set_own_parallel_bit(ctx: &mut dyn NativeContext, stream: ObjectRef, on: bool) {
    if ctx.object_num_fields(stream) <= STREAM_FIELD_SPL_FACTS {
        return;
    }
    let word = stream_spl_facts_word(&*ctx, stream);
    let next = if on {
        word | STREAM_FACTS_PARALLEL
    } else {
        word & !STREAM_FACTS_PARALLEL
    };
    if next != word {
        ctx.set_field(stream, STREAM_FIELD_SPL_FACTS, Value::Long(next));
    }
}

/// [`stream_record_next_stage`] for ANY two synthetic carriers (both already
/// checked by the caller with [`stream_carries_parallel_bit`]): a primitive
/// stage, or a reference stage feeding a primitive one, or the reverse.
/// No-op under either kill switch. Pure stores.
pub(crate) fn stream_record_next_stage_any(ctx: &mut dyn NativeContext, src: ObjectRef, dst: ObjectRef) {
    if src == dst
        || ctx.object_num_fields(src) <= STREAM_FIELD_NEXT_STAGE
        || ctx.object_num_fields(dst) <= STREAM_FIELD_NEXT_STAGE
        || !stream_shared_parallel_enabled()
        || !stream_shared_parallel_primitive_enabled()
    {
        return;
    }
    ctx.set_field(src, STREAM_FIELD_NEXT_STAGE, Value::Object(Some(dst)));
}

/// Run the intermediate-op native `body` on `args` (receiver first) and, when
/// it answers a NEW synthetic carrier, record that stage's HotSpot class
/// (`stage`) and link it into the receiver's pipeline: the new stage takes
/// the receiver pipeline's parallel mode (read before `body` runs Java) and
/// becomes the receiver's next stage ([`stream_shared_parallel_primitive_enabled`]).
/// A receiver `body` answers unchanged (an identity op) is left alone, as is
/// a real pipeline on either side.
///
/// GC: the receiver is pinned BELOW every pin `body` takes, so `body`'s own
/// unpins (which release from their own base upward) never release it, and
/// the single unpin here also releases anything `body` left pinned. The
/// answered stage is read from `r` after `body` returned; nothing between that
/// read and the stores allocates.
pub(crate) fn stream_stage_op(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    body: cratonvm_native_api::NativeCallback,
    stage: StageClass,
) -> MethodCallResult {
    // Round 18 wave 11 (lane streams5): every op this wraps takes its
    // function first and `Objects.requireNonNull`s it before linking
    // ([`stream_null_function_npe_enabled`]).
    if matches!(args.get(1), Some(Value::Object(None))) && stream_null_function_npe_enabled() {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    }
    let begin = match stream_stage_begin(ctx, args)? {
        Some(begin) => begin,
        None => return body(ctx, args),
    };
    let r = body(ctx, args);
    stream_stage_end(ctx, begin, &r, stage);
    r
}

/// The facts op ([`SplOp`]) of the stage HotSpot builds for `stage`,
/// MEASURED from each op's `StreamOpFlag` set in JDK 25 (`IntPipeline` /
/// `LongPipeline` / ...): `asLongStream` / `asDoubleStream` on an `IntStream`
/// pass `0` (preserve), `LongStream.asDoubleStream` passes `NOT_DISTINCT`.
/// `None` for `sorted` (natural and comparator stages share a class; their
/// natives record their own facts).
pub(crate) fn stage_class_spl_op(stage: StageClass) -> Option<SplOp> {
    use StageClass as S;
    Some(match stage {
        S::RefFilter | S::IntFilter | S::LongFilter | S::DoubleFilter => SplOp::Filter,
        S::RefMap
        | S::RefMapToInt
        | S::RefMapToLong
        | S::RefMapToDouble
        | S::IntMap
        | S::IntMapToObj
        | S::IntMapToLong
        | S::IntMapToDouble
        | S::LongMap
        | S::LongMapToObj
        | S::LongMapToInt
        | S::LongMapToDouble
        | S::DoubleMap
        | S::DoubleMapToObj
        | S::DoubleMapToInt
        | S::DoubleMapToLong => SplOp::Map,
        S::RefFlatMap
        | S::RefFlatMapToInt
        | S::RefFlatMapToLong
        | S::RefFlatMapToDouble
        | S::IntFlatMap
        | S::LongFlatMap
        | S::DoubleFlatMap => SplOp::FlatMap,
        S::RefPeek
        | S::IntPeek
        | S::LongPeek
        | S::DoublePeek
        | S::RefSlice
        | S::IntSlice
        | S::LongSlice
        | S::DoubleSlice
        | S::IntAsLong
        | S::IntAsDouble => SplOp::Preserve,
        S::LongAsDouble => SplOp::NotDistinct,
        S::RefDistinct => SplOp::Distinct,
        S::RefUnordered | S::IntUnordered | S::LongUnordered | S::DoubleUnordered => {
            SplOp::Unordered
        }
        S::RefSorted | S::IntSorted | S::LongSorted | S::DoubleSorted => return None,
    })
}

/// What [`stream_stage_begin`] read off the receiver before the op's body
/// ran Java, and the pin that keeps it.
#[derive(Clone, Copy)]
pub(crate) struct StageBegin {
    pub(in crate::streams) src: ObjectRef,
    pub(in crate::streams) src_pin: usize,
    pub(in crate::streams) src_carrier: bool,
    pub(in crate::streams) src_parallel: bool,
}

/// The receiver half of [`stream_stage_op`]: `None` (run the body bare) for a
/// null receiver; otherwise the receiver's pipeline mode, read before the body
/// runs Java, and a pin on it. A PRIMITIVE carrier that was already linked or
/// consumed refuses here with HotSpot's `IllegalStateException`
/// ([`stream_primitive_link_once_enabled`]); a reference carrier's own funnel
/// (`stream_link_or_consume`) already does.
pub(crate) fn stream_stage_begin(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<Option<StageBegin>, MethodCallFailed> {
    let src = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    stream_primitive_refuse_if_linked(&*ctx, src)?;
    // Pure reads, before the body runs Java and moves `src`.
    let src_carrier = stream_carries_parallel_bit(&*ctx, src);
    let src_parallel = src_carrier && stream_is_parallel(&*ctx, src);
    let src_pin = ctx.pin_native_root(src);
    Ok(Some(StageBegin {
        src,
        src_pin,
        src_carrier,
        src_parallel,
    }))
}

/// The stage half of [`stream_stage_op`]: when the body answered a NEW
/// synthetic carrier, record its class and link it into the receiver's
/// pipeline; then release the receiver's pin (and anything the body left
/// pinned above it). Pure reads and stores after the body returned.
pub(crate) fn stream_stage_end(
    ctx: &mut dyn NativeContext,
    begin: StageBegin,
    r: &MethodCallResult,
    stage: StageClass,
) {
    if let Ok(Some(Value::Object(Some(dst)))) = r {
        let dst = *dst;
        let src = ctx.read_native_pin(begin.src_pin, begin.src);
        if dst != src && stream_carries_parallel_bit(&*ctx, dst) {
            stream_stamp_stage_class(ctx, dst, stage);
            // Round 18 wave 11: a PRIMITIVE stage its native built without
            // facts (the cross-shape and `flatMap` builders) gets the facts of
            // the op it stands for, so its `spliterator()` can answer
            // HotSpot's wrapping spliterator
            // ([`primitive_wrapped_stage_spliterator_enabled`]). Pure reads of
            // `src` (re-read through its pin) and one store.
            if primitive_wrapped_stage_spliterator_enabled()
                && stream_is_primitive_carrier(&*ctx, dst)
                && stream_spl_facts(&*ctx, dst).is_none()
            {
                if let Some(op) = stage_class_spl_op(stage) {
                    if let Some(packed) = prim_stage_spl_facts(&*ctx, src, op) {
                        stream_set_spl_facts(ctx, dst, packed);
                    }
                }
            }
            // Round 18 wave 11: a stage the native built bare (every
            // primitive and cross-shape builder) gets its upstream's close
            // handlers, as `make_derived_stream` gives a reference stage
            // ([`stream_stage_close_handlers_enabled`]).
            if stream_stage_close_handlers_enabled()
                && ctx.object_num_fields(dst) > STREAM_FIELD_CLOSE_HANDLERS
                && !matches!(
                    ctx.get_field(dst, STREAM_FIELD_CLOSE_HANDLERS),
                    Value::Object(Some(_))
                )
            {
                stream_inherit_close_handlers(ctx, src, dst);
            }
            if begin.src_carrier && stream_shared_parallel_primitive_enabled() {
                stream_set_own_parallel_bit(ctx, dst, begin.src_parallel);
                stream_record_next_stage_any(ctx, src, dst);
            }
        }
    }
    ctx.unpin_native_roots(begin.src_pin);
}

/// `CRATONVM_STREAM_NULL_FUNCTION_NPE` -- default ON, `=0` restores the old
/// answer. JDK 25 `ReferencePipeline.map(null)` (and every other intermediate
/// op's function, comparator or consumer argument, in all four shapes) is
/// `Objects.requireNonNull(..)`: a `NullPointerException` before the stage is
/// built or the receiver linked. The synthetic natives answered an EMPTY
/// stream for a null function. JIT round 18 wave 11, lane streams5. Read
/// live, per call.
pub(crate) fn stream_null_function_npe_enabled() -> bool {
    true
}

/// `CRATONVM_STREAM_STAGE_CLOSE_HANDLERS` -- default ON, `=0` restores the
/// old answer. JDK 25 keeps one close action per pipeline (on the source
/// stage), so `IntStream.range(0, 3).onClose(r).map(f).close()` runs `r`.
/// A reference stage inherited its upstream's handlers
/// (`make_derived_stream`); every primitive and cross-shape builder built a
/// bare carrier, so closing it ran nothing. [`stream_stage_end`] copies the
/// upstream's handler array onto a new stage that has none. JIT round 18
/// wave 11, lane streams5. Read live, per call.
pub(crate) fn stream_stage_close_handlers_enabled() -> bool {
    true
}

/// `CRATONVM_STREAM_PRIMITIVE_LINK_ONCE` -- default ON, `=0` restores the
/// reusable primitive stage. JDK 25 `AbstractPipeline` marks a stage linked
/// when an intermediate op is built over it or a terminal op consumes it, and
/// refuses a second use with `IllegalStateException("stream has already been
/// operated upon or closed")`: `IntStream s = range(0, 3); s.map(f);
/// s.map(f)` throws on HotSpot. W7-65 left the primitive carriers unguarded
/// because their element funnel (`int_stream_elements`) is infallible and a
/// throw there would be swallowed into an empty stream. This switch splits
/// the two halves: the funnel only MARKS the primitive carrier (a store, no
/// throw, [`stream_elements`]), and the fallible registration wrappers of
/// every primitive intermediate and terminal op ([`stream_stage_begin`],
/// [`stream_terminal_op`]) REFUSE a marked receiver before anything runs.
/// Read live, per call (JIT round 18 wave 11, lane streams5).
pub(crate) fn stream_primitive_link_once_enabled() -> bool {
    true
}

/// Whether `stream` is a synthetic PRIMITIVE carrier wide enough for the
/// linked slot. Pure reads.
pub(crate) fn stream_is_primitive_carrier(ctx: &dyn NativeContext, stream: ObjectRef) -> bool {
    ctx.object_num_fields(stream) > STREAM_FIELD_LINKED
        && ctx
            .class_name_arc_of_id(ctx.class_id_of_object(stream))
            .is_some_and(|n| is_synthetic_stream(&n) && &*n != "java/util/stream/Stream")
}

/// HotSpot's refusal of a reused PRIMITIVE stage
/// ([`stream_primitive_link_once_enabled`]): `Err` when `stream` is a
/// primitive carrier already linked, consumed or closed. Pure reads.
pub(crate) fn stream_primitive_refuse_if_linked(
    ctx: &dyn NativeContext,
    stream: ObjectRef,
) -> Result<(), MethodCallFailed> {
    if stream_is_primitive_carrier(ctx, stream)
        && stream_is_linked(ctx, stream)
        && stream_primitive_link_once_enabled()
    {
        return Err(RuntimeError::IllegalStateException {
            message: STREAM_LINKED_MSG.to_string(),
        }
        .into());
    }
    Ok(())
}

/// `CRATONVM_PRIMITIVE_WRAPPED_STAGE_SPLITERATOR` -- default ON, `=0` restores
/// the plain array spliterator. `spliterator()` on a DERIVED synthetic
/// primitive stage answered `Spliterators.spliterator(int[], ..)` over the
/// stage's snapshot: `0x4040`, the snapshot's size, splitting, where HotSpot's
/// `IntWrappingSpliterator` reports the stage's flags (`map` `0x4050`, `filter`
/// `0x15` with the source's estimate and no exact size, `unordered` `0x4041`)
/// and does not split a sequential pipeline. `native-builtins`
/// `p_prim_wrapped_spliterator` builds that JDK object from these facts. JIT
/// round 18 wave 11, lane streams5. Read live, per call.
pub(crate) fn primitive_wrapped_stage_spliterator_enabled() -> bool {
    true
}

/// The facts of a DERIVED synthetic primitive stage, for its `spliterator()`
/// ([`primitive_wrapped_stage_spliterator_enabled`]): the public
/// characteristics the stage recorded, the HEAD source's size when known, and
/// the pipeline's parallel mode. `None` for a source stage, a stage nothing
/// described, any non-primitive receiver, and under the kill switches. Pure
/// reads.
pub fn synthetic_primitive_wrapped_stage_facts(
    ctx: &dyn NativeContext,
    stream: ObjectRef,
) -> Option<(i32, Option<usize>, bool)> {
    if !stream_is_primitive_carrier(ctx, stream)
        || !primitive_wrapped_stage_spliterator_enabled()
        || !spliterator_facts_enabled()
    {
        return None;
    }
    match stream_spl_facts(ctx, stream) {
        Some((c, head)) if c & SPL_PRIV_WRAPPED != 0 => {
            Some((c & !SPL_PRIV_MASK, head, stream_is_parallel(ctx, stream)))
        }
        _ => None,
    }
}

/// A synthetic PRIMITIVE stream consumed by a native outside this crate
/// (`native-builtins` `p_{int,long,double}_stream_spliterator`): refuse a
/// reused stage with HotSpot's `IllegalStateException`, then mark it linked
/// ([`stream_primitive_link_once_enabled`]). No-op for any other receiver.
pub fn synthetic_primitive_stream_consume(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
) -> Result<(), MethodCallFailed> {
    stream_primitive_refuse_if_linked(&*ctx, stream)?;
    if stream_is_primitive_carrier(&*ctx, stream) && stream_primitive_link_once_enabled() {
        stream_mark_linked(ctx, stream);
    }
    Ok(())
}

/// A primitive TERMINAL op's registration wrapper: refuse a reused receiver
/// ([`stream_primitive_refuse_if_linked`]), then run `body` (whose element
/// read marks the receiver consumed).
pub(crate) fn stream_terminal_op(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    body: cratonvm_native_api::NativeCallback,
) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        stream_primitive_refuse_if_linked(&*ctx, *this)?;
    }
    body(ctx, args)
}

/// `limit(n)` (`skip == false`) / `skip(n)` on any synthetic stream shape:
/// HotSpot's argument check first ([`stream_slice_arg_check_enabled`]) --
/// a negative `n` is `IllegalArgumentException(Long.toString(n))` and
/// `skip(0)` is `this`, neither linking the receiver -- then
/// [`stream_stage_op`] over the native `body`.
pub(crate) fn stream_slice_op(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    body: cratonvm_native_api::NativeCallback,
    stage: StageClass,
    skip: bool,
) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        let n = match args.get(1) {
            Some(Value::Long(v)) => Some(*v),
            Some(Value::Int(v)) => Some(i64::from(*v)),
            _ => None,
        };
        if let Some(n) = n {
            if (n < 0 || (skip && n == 0)) && stream_slice_arg_check_enabled() {
                if n < 0 {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: n.to_string(),
                    }
                    .into());
                }
                return Ok(Some(Value::Object(Some(*this))));
            }
        }
    }
    stream_stage_op(ctx, args, body, stage)
}

/// `CRATONVM_STREAM_CONCAT_HOTSPOT` -- default ON, `=0` restores the old
/// answers. JDK 25 `Stream.concat(a, b)` is `Objects.requireNonNull(a);
/// Objects.requireNonNull(b);` and then a new `Head` whose mode is
/// `a.isParallel() || b.isParallel()`. The synthetic native read a null
/// operand as an empty stream and always answered a sequential stream
/// (`Stream.concat(Stream.of(1).parallel(), Stream.of(2)).isParallel()` was
/// `false`, HotSpot `true`). JIT round 18 wave 10, lane streams5. Read live,
/// per call.
pub(crate) fn stream_concat_hotspot_enabled() -> bool {
    true
}

/// Whether `stream` (a synthetic carrier or a real pipeline stage) is
/// parallel, as `BaseStream.isParallel()` answers it. Pure reads.
pub(crate) fn stream_any_is_parallel(ctx: &dyn NativeContext, stream: ObjectRef) -> bool {
    if stream_carries_parallel_bit(ctx, stream) {
        stream_is_parallel(ctx, stream)
    } else {
        stream_parallel_stage_enabled()
            && real_pipeline_shape(ctx, stream).is_some_and(|shape| shape.parallel)
    }
}

/// `Stream.concat(a, b)` around the native `body`
/// ([`stream_concat_hotspot_enabled`]): a null operand is the
/// `NullPointerException` `Objects.requireNonNull` throws, and the answered
/// stream takes `a.isParallel() || b.isParallel()` (read before `body` runs
/// Java). The answered stream is read from `r` after `body` returned; nothing
/// between that read and the store allocates.
pub(crate) fn stream_concat_op(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    body: cratonvm_native_api::NativeCallback,
) -> MethodCallResult {
    if !stream_concat_hotspot_enabled() {
        return body(ctx, args);
    }
    let (a, b) = match (args.first(), args.get(1)) {
        (Some(Value::Object(Some(a))), Some(Value::Object(Some(b)))) => (*a, *b),
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    let parallel = stream_any_is_parallel(&*ctx, a) || stream_any_is_parallel(&*ctx, b);
    let r = body(ctx, args);
    if parallel {
        if let Ok(Some(Value::Object(Some(dst)))) = &r {
            if stream_carries_parallel_bit(&*ctx, *dst) {
                stream_set_own_parallel_bit(ctx, *dst, true);
            }
        }
    }
    r
}

/// Record a SOURCE stage's facts: the characteristics of the JDK spliterator
/// that source would have, and the comparator it answers when SORTED.
pub(crate) fn stream_set_source_spl_facts(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
    chars: i32,
    comparator: Value,
) {
    stream_set_spl_facts(ctx, stream, pack_spl_facts(chars, None));
    if ctx.object_num_fields(stream) > STREAM_FIELD_SPL_CMP {
        if let v @ Value::Object(Some(_)) = comparator {
            ctx.set_field(stream, STREAM_FIELD_SPL_CMP, v);
        }
    }
}

/// The element count of the array behind `stream`'s slot 0: a source stage's
/// own elements, and also a deferred stage's, which shares its source's
/// array (`stream_make_lazy_derived`).
pub(crate) fn stream_source_len(ctx: &dyn NativeContext, stream: ObjectRef) -> Option<usize> {
    if ctx.object_num_fields(stream) <= STREAM_FIELD_ELEMENTS {
        return None;
    }
    match ctx.get_field(stream, STREAM_FIELD_ELEMENTS) {
        Value::Object(Some(a)) if ctx.heap_kind_of(a) == cratonvm_types::ObjectKind::Array => {
            Some(ctx.array_length(a))
        }
        _ => None,
    }
}

/// The packed facts of the stage `op` builds over `src`. Pure reads.
///
/// A `src` nothing described counts as an array snapshot reporting the list
/// default (what its own `spliterator()` answers), except one still holding
/// an undrained real spliterator (`StreamSupport.stream(spl, ..)`), whose
/// characteristics nobody read: that stays undescribed (`None`), and so does
/// everything derived from it.
///
/// A parallel `src` passes its [`STREAM_FACTS_PARALLEL`] bit on (HotSpot keeps
/// `parallel` on the shared source stage, so every later stage reports it),
/// even when the characteristics stay undescribed.
pub(crate) fn stream_derive_spl_facts(ctx: &dyn NativeContext, src: ObjectRef, op: SplOp) -> Option<i64> {
    // Wave 15 (lane compat2): a REAL pipeline stage (`IntStream.of(..)`'s
    // `IntPipeline$Head` under `boxed()`) has no carrier slots: its slot 2 is
    // an `AbstractPipeline` field, which the lazy-spliterator test below read
    // as "undrained source" and left the new stage undescribed, so it split.
    if let Some(shape) = real_pipeline_shape(ctx, src) {
        let chars = spl_wrapped_characteristics(SPL_LIST_DEFAULT, op);
        let parallel = if shape.parallel {
            STREAM_FACTS_PARALLEL
        } else {
            0
        };
        return Some(pack_spl_facts(chars | SPL_PRIV_WRAPPED, None) | parallel);
    }
    let parallel = if stream_is_parallel(ctx, src) {
        STREAM_FACTS_PARALLEL
    } else {
        0
    };
    let (upstream, head) = match stream_spl_facts(ctx, src) {
        Some((c, head)) if c & SPL_PRIV_WRAPPED != 0 => (c, head),
        Some((c, _)) => (c, stream_source_len(ctx, src)),
        None => {
            if stream_lazy_spliterator(ctx, src).is_some() {
                return (parallel != 0).then_some(parallel);
            }
            (SPL_LIST_DEFAULT, stream_source_len(ctx, src))
        }
    };
    let chars = spl_wrapped_characteristics(upstream & !SPL_PRIV_MASK, op);
    Some(pack_spl_facts(chars | SPL_PRIV_WRAPPED, head) | parallel)
}

/// The packed facts a PRIMITIVE intermediate op (`IntStream` / `LongStream` /
/// `DoubleStream` `map`, `filter`, `sorted`, `distinct`, `limit`, `skip`,
/// `peek`) records on the stage it builds over `src`: [`stream_derive_spl_facts`],
/// or `None` under `CRATONVM_STREAM_PRIMITIVE_STAGE_FACTS=0` (default ON; JIT
/// round 17 wave 1, lane streams, proposal C2-1). Those ops recorded nothing,
/// so they dropped the parallel bit (`IntStream.range(0, 3).parallel().map(f)
/// .isParallel()` was `false`; HotSpot keeps `parallel` on the source stage)
/// and a reference stage built after one (`.filter(p).boxed()`) started from
/// the list default (`0x4050`, sized) where HotSpot reports the filtered
/// `0x15` with the source's estimate. A primitive stage's own `spliterator()`
/// is unchanged: [`synthetic_stream_spliterator_characteristics`] ignores a
/// wrapped stage. Pure reads, before the op runs Java and moves `src`.
pub(crate) fn prim_stage_spl_facts(ctx: &dyn NativeContext, src: ObjectRef, op: SplOp) -> Option<i64> {
    stream_derive_spl_facts(ctx, src, op)
}

/// What [`real_pipeline_shape`] reads off a real `java.util.stream`
/// pipeline stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RealPipelineShape {
    /// `depth > 0`: an intermediate stage, whose `spliterator()` is a
    /// `WrappingSpliterator`.
    pub(crate) wrapped: bool,
    /// `sourceStage.parallel`, what `AbstractPipeline.isParallel()` answers.
    pub(crate) parallel: bool,
}

/// `CRATONVM_SPLITERATOR_REAL_PIPELINE` -- default ON, `=0` restores the
/// wave-14 answers. Under `--compatible` the synthetic-stream natives also
/// receive REAL pipeline stages (`IntStream.of(int...)` and a collection's
/// `Collection.stream()` default run their bytecode and build
/// `IntPipeline$Head` / `ReferencePipeline$Head`). The natives read those
/// objects' fields as if they were the synthetic carrier's slots: an
/// intermediate real stage's `spliterator()` split (`R16SpliterW9`
/// `IntStream.of.boxed` and `ArrayList.stream.map` answered `split=true`
/// where HotSpot's sequential `WrappingSpliterator` declines), and
/// `parallel()` / `isParallel()` neither set nor read the stage's own
/// `parallel` flag (`R16Compat2Streams` `ArrayList.stream.parallel`). Round 16
/// wave 15, lane compat2. Read live, per call.
pub(crate) fn spliterator_real_pipeline_enabled() -> bool {
    true
}

/// Whether `stream` is a REAL pipeline stage (an `AbstractPipeline`
/// instance) rather than a synthetic carrier, and if so its shape, read by
/// field NAME (`depth`, `sourceStage.parallel`). `None` for every synthetic
/// carrier and under [`spliterator_real_pipeline_enabled`]'s kill switch.
/// Pure reads.
pub(crate) fn real_pipeline_shape(ctx: &dyn NativeContext, stream: ObjectRef) -> Option<RealPipelineShape> {
    if !spliterator_real_pipeline_enabled()
        || !obj_is_instance_of(ctx, stream, "java/util/stream/AbstractPipeline")
    {
        return None;
    }
    let wrapped = matches!(ctx.get_field_by_name(stream, "depth"), Value::Int(d) if d > 0);
    let parallel = match ctx.get_field_by_name(stream, "sourceStage") {
        Value::Object(Some(source)) => {
            matches!(ctx.get_field_by_name(source, "parallel"), Value::Int(p) if p != 0)
        }
        _ => false,
    };
    Some(RealPipelineShape { wrapped, parallel })
}

/// The facts [`native_stream_spliterator`] uses for a real pipeline stage: a
/// wrapped stage reports the list default (what the snapshot always
/// reported) without splitting; a head keeps the old three-field snapshot,
/// which splits as its source spliterator does.
pub(crate) fn real_pipeline_spl_facts(shape: RealPipelineShape) -> Option<(i32, Option<usize>)> {
    shape
        .wrapped
        .then_some((SPL_LIST_DEFAULT | SPL_PRIV_WRAPPED, None))
}

/// `CRATONVM_PRIMITIVE_SPLITERATOR_FACTS` -- default ON, `=0` restores the
/// bare `Spliterators.spliterator(int[], 0, n, 0)`. The primitive
/// `spliterator()` natives (`native-builtins` `p_int_stream_spliterator` and
/// its long / double twins) hand the JDK's array spliterator no
/// characteristics, so `IntStream.range(0, 3).spliterator()` reported
/// `0x4040` where HotSpot's `RangeIntSpliterator` reports `0x4555`
/// (`R16SpliterW9Prim`), although the source native had recorded exactly that
/// ([`STREAM_FIELD_SPL_FACTS`]). Round 16 wave 13, lane compatcoll (proposal
/// SPL9-1). Read per `spliterator()` call.
pub(crate) fn primitive_spliterator_facts_enabled() -> bool {
    // Read live, not latched: one read per bulk copy / `spliterator()` /
    // `unordered()`, and no process-wide cache to keep per VM.
    true
}

/// The public characteristics a synthetic primitive (or reference) stream's
/// SOURCE stage recorded for its JDK spliterator ([`stream_spl_facts`]),
/// for a `spliterator()` native that builds the JDK's own array spliterator
/// and passes them as its `additionalCharacteristics`. `None` -- keep the
/// old `0` -- under the kill switch ([`primitive_spliterator_facts_enabled`])
/// or [`spliterator_facts_enabled`]'s, for a stage nothing described, and for
/// an intermediate-op stage, whose HotSpot spliterator is a
/// `WrappingSpliterator` an array spliterator cannot stand for. Pure reads.
pub fn synthetic_stream_spliterator_characteristics(
    ctx: &dyn NativeContext,
    stream: ObjectRef,
) -> Option<i32> {
    if !primitive_spliterator_facts_enabled() || !spliterator_facts_enabled() {
        return None;
    }
    match stream_spl_facts(ctx, stream) {
        Some((c, _)) if c & SPL_PRIV_WRAPPED == 0 => Some(c & !SPL_PRIV_MASK),
        _ => None,
    }
}

/// `CRATONVM_STREAM_UNORDERED_STAGE` -- default ON, `=0` restores the
/// identity. JDK 25 `AbstractPipeline.unordered()` is
/// `if (!isOrdered()) return this;` and otherwise a NEW `StatelessOp` with
/// `StreamOpFlag.NOT_ORDERED`, which links `this` (a second use of it is
/// `IllegalStateException`) and whose `spliterator()` is a sequential
/// `WrappingSpliterator`: `Stream.of("x","y","z").unordered().spliterator()`
/// reports `0x4040` (ORDERED and IMMUTABLE gone), estimate 3, and does not
/// split (`R16SpliterW9` / `R16SpliterW9Prim` `Stream.of.unordered`). The
/// shared `unordered()` registrations answered `this`, so the stage kept its
/// source's `0x4450` and split. Round 16 wave 13, lane compatcoll (proposal
/// SPL9-3).
pub(crate) fn stream_unordered_stage_enabled() -> bool {
    // Read live, not latched: one read per bulk copy / `spliterator()` /
    // `unordered()`, and no process-wide cache to keep per VM.
    true
}

/// Whether `unordered()` on `stream` builds a new stage: the synthetic
/// REFERENCE stream at its full layout (not a `StackWalker.walk` stream),
/// whose elements are already an array -- no undrained source spliterator,
/// no deferred op chain, so taking them runs no Java and cannot hang on an
/// infinite source -- and whose recorded facts ([`stream_spl_facts`]) say
/// ORDERED. A stage nothing described keeps the identity: its encounter
/// order is unknown here, and HotSpot answers `this` for an unordered one.
/// Pure reads.
pub(crate) fn stream_unordered_derives(ctx: &dyn NativeContext, stream: ObjectRef) -> bool {
    stream_is_reference_carrier(ctx, stream)
        && ctx.object_num_fields(stream) == STREAM_NUM_FIELDS
        && stream_lazy_spliterator(ctx, stream).is_none()
        && !stream_has_chain(ctx, stream)
        && matches!(stream_spl_facts(ctx, stream), Some((c, _)) if c & SPL_ORDERED != 0)
}

/// `BaseStream.unordered()` (every `Stream` / `IntStream` / `LongStream` /
/// `DoubleStream` / `BaseStream` registration in `native-builtins`): a new
/// `NOT_ORDERED` stage over an ORDERED synthetic reference stream
/// ([`stream_unordered_stage_enabled`], [`stream_unordered_derives`]), `this`
/// otherwise. Taking the elements goes through [`stream_elements`], which
/// links `this` -- and refuses an already-linked one with HotSpot's message,
/// as the `StatelessOp` constructor does.
pub fn native_stream_unordered(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(args.first().copied()),
    };
    // Round 18 wave 11 (lane streams5): an ORDERED primitive stage builds
    // HotSpot's `IntPipeline$9` / `LongPipeline$8` / `DoublePipeline$7`.
    if stream_unordered_stage_enabled()
        && stream_unordered_hotspot_enabled()
        && stream_unordered_primitive_derives(&*ctx, this)
    {
        return stream_primitive_unordered_stage(ctx, args);
    }
    if stream_unordered_stage_enabled()
        && stream_lazy_unordered_stage_enabled()
        && stream_unordered_lazy_head(&*ctx, this)
    {
        // Round 18 wave 10 (lane streams5): the new stage is HotSpot's
        // `ReferencePipeline$1` ([`STREAM_FIELD_STAGE_CLASS`]).
        let r = stream_lazy_unordered_stage(ctx, this);
        stream_stamp_result(ctx, &r, StageClass::RefUnordered);
        return r;
    }
    if !stream_unordered_stage_enabled() || !stream_unordered_derives(&*ctx, this) {
        return Ok(Some(Value::Object(Some(this))));
    }
    let elements = stream_elements(ctx, &mut this)?;
    let r = make_derived_stream(ctx, this, &elements, SplOp::Unordered);
    stream_stamp_result(ctx, &r, StageClass::RefUnordered);
    r
}

/// `CRATONVM_STREAM_UNORDERED_HOTSPOT` -- default ON, `=0` restores the
/// round-16 answers. JDK 25 `IntPipeline.unordered()` (and the Long / Double
/// twins) is `if (!isOrdered()) return this;` and otherwise a new
/// `StatelessOp` with `NOT_ORDERED` that links `this`; the synthetic primitive
/// carrier answered `this` for every receiver, so
/// `IntStream.range(0, 3).unordered().getClass()` was `IntPipeline$Head` (the
/// same object) where HotSpot answers `IntPipeline$9`, and its spliterator
/// kept ORDERED. The switch also makes the `NOT_ORDERED` op clear SORTED with
/// ORDERED, as HotSpot reports (`spl_apply_op`). JIT round 18 wave 11, lane
/// streams5. Read live, per call.
pub(crate) fn stream_unordered_hotspot_enabled() -> bool {
    true
}

/// Whether `unordered()` on `stream` builds a new PRIMITIVE stage: a
/// full-layout synthetic primitive carrier whose recorded facts say ORDERED.
/// A stage nothing described keeps the identity, as the reference rule
/// ([`stream_unordered_derives`]) does. Pure reads.
pub(crate) fn stream_unordered_primitive_derives(ctx: &dyn NativeContext, stream: ObjectRef) -> bool {
    stream_is_primitive_carrier(ctx, stream)
        && ctx.object_num_fields(stream) == STREAM_NUM_FIELDS
        && matches!(stream_spl_facts(ctx, stream), Some((c, _)) if c & SPL_ORDERED != 0)
}

/// The `NOT_ORDERED` primitive stage ([`stream_unordered_primitive_derives`]):
/// refuse a reused receiver, copy its elements into a new carrier of the same
/// shape with the `Unordered` facts, and record / link it as every other
/// intermediate op does ([`stream_stage_begin`], [`stream_stage_end`]).
pub(crate) fn stream_primitive_unordered_stage(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let begin = match stream_stage_begin(ctx, args)? {
        Some(begin) => begin,
        None => return Ok(args.first().copied()),
    };
    let mut this = begin.src;
    let facts = prim_stage_spl_facts(&*ctx, this, SplOp::Unordered);
    let shape = ctx
        .class_name_arc_of_id(ctx.class_id_of_object(this))
        .map(|n| n.to_string())
        .unwrap_or_default();
    let elements = int_stream_elements(ctx, &mut this);
    let (r, stage) = match shape.as_str() {
        "java/util/stream/LongStream" => {
            (make_long_stream(ctx, &elements), StageClass::LongUnordered)
        }
        "java/util/stream/DoubleStream" => (
            make_double_stream(ctx, &elements),
            StageClass::DoubleUnordered,
        ),
        _ => (make_int_stream(ctx, &elements), StageClass::IntUnordered),
    };
    let r = with_derived_spl_facts(ctx, r, facts);
    stream_stage_end(ctx, begin, &r, stage);
    r
}

/// Whether `unordered()` on `stream` builds a new stage over its UNDRAINED
/// source ([`stream_lazy_unordered_stage_enabled`]): the full-layout
/// synthetic reference carrier `StreamSupport.stream(spl, ..)` builds, still
/// holding `spl` (slot 0 null, no op chain), whose recorded facts say
/// ORDERED. A source that is not ORDERED keeps the identity, as
/// `AbstractPipeline.unordered()` does. Pure reads.
pub(crate) fn stream_unordered_lazy_head(ctx: &dyn NativeContext, stream: ObjectRef) -> bool {
    stream_is_reference_carrier(ctx, stream)
        && ctx.object_num_fields(stream) == STREAM_NUM_FIELDS
        && stream_lazy_spliterator(ctx, stream).is_some()
        && !stream_has_chain(ctx, stream)
        && matches!(
            ctx.get_field(stream, STREAM_FIELD_ELEMENTS),
            Value::Object(None)
        )
        && matches!(stream_spl_facts(ctx, stream), Some((c, _)) if c & SPL_ORDERED != 0)
}

/// The `NOT_ORDERED` stage `unordered()` builds over a lazy head
/// ([`stream_unordered_lazy_head`]), as JDK 25's `StatelessOp` does: link
/// `this` (a second use of it is `IllegalStateException`), then answer a new
/// full-layout carrier that holds the same undrained source spliterator, the
/// same close handlers and comparator, and the facts of an `Unordered` stage
/// (WRAPPED, so its own `spliterator()` is a non-splitting snapshot rather
/// than the source itself; parallel bit kept). Nothing is drained, so an
/// infinite source stays usable.
pub(crate) fn stream_lazy_unordered_stage(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallResult {
    let facts = stream_derive_spl_facts(&*ctx, this, SplOp::Unordered);
    stream_link_or_consume(ctx, this)?;
    let this_pin = ctx.pin_native_root(this);
    let stage = match try_alloc_synthetic(ctx, "java/util/stream/Stream", STREAM_NUM_FIELDS) {
        Ok(s) => s,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    let this = ctx.read_native_pin(this_pin, this);
    // Pure reads and stores from here on: nothing below allocates.
    let spl = ctx.get_field(this, STREAM_FIELD_LAZY_SPLITERATOR);
    // Round 18 wave 11: the new stage names the pipeline's close root.
    let handlers = if stream_one_close_action_enabled() {
        Value::Object(Some(stream_close_root(&*ctx, this)))
    } else {
        ctx.get_field(this, STREAM_FIELD_CLOSE_HANDLERS)
    };
    let comparator = ctx.get_field(this, STREAM_FIELD_SPL_CMP);
    ctx.set_field(stage, STREAM_FIELD_ELEMENTS, Value::Object(None));
    ctx.set_field(stage, STREAM_FIELD_CLOSE_HANDLERS, handlers);
    ctx.set_field(stage, STREAM_FIELD_LAZY_SPLITERATOR, spl);
    ctx.set_field(stage, STREAM_FIELD_LINKED, Value::Int(0));
    if let Some(packed) = facts {
        ctx.set_field(stage, STREAM_FIELD_SPL_FACTS, Value::Long(packed));
    }
    if let v @ Value::Object(Some(_)) = comparator {
        ctx.set_field(stage, STREAM_FIELD_SPL_CMP, v);
    }
    // Round 17 wave 8 (lane streams4): the head's next stage.
    stream_record_next_stage(ctx, this, stage);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(stage))))
}

/// `CRATONVM_STREAM_PARALLEL_STAGE` -- default ON, `=0` restores the old
/// answers (`parallel()` / `sequential()` change nothing, `isParallel()` is
/// `false`, a stage's spliterator never splits). JDK 25
/// `AbstractPipeline.parallel()` is `sourceStage.parallel = true; return
/// this;`, `sequential()` clears it, and `isParallel()` reads it, so
/// `Stream.of(1, 2, 3).parallel().isParallel()` is `true` on HotSpot. A
/// parallel pipeline's `WrappingSpliterator.trySplit()` splits its source
/// where a sequential one answers null. The shared registrations answered
/// `this` and a constant `false`, so a parallel stage never split and never
/// said it was parallel (wave 9 residual 2, second half). Round 16 wave 14,
/// lane compat2 (proposal CC13-3). Read live, per call.
pub(crate) fn stream_parallel_stage_enabled() -> bool {
    true
}

/// Whether `stream` can carry the parallel bit: one of the synthetic stream
/// carriers ([`is_synthetic_stream`]) wide enough for
/// [`STREAM_FIELD_SPL_FACTS`]. The 1- and 2-field carriers other natives mint
/// cannot, and keep the old answers. Pure reads.
pub(crate) fn stream_carries_parallel_bit(ctx: &dyn NativeContext, stream: ObjectRef) -> bool {
    ctx.object_num_fields(stream) > STREAM_FIELD_SPL_FACTS
        && ctx
            .class_name_arc_of_id(ctx.class_id_of_object(stream))
            .is_some_and(|n| is_synthetic_stream(&n))
}

/// `parallel()` (`on`) / `sequential()` (`!on`) on `this`: set or clear
/// [`STREAM_FACTS_PARALLEL`] in place and answer `this`, as
/// `AbstractPipeline` does. Neither links the stage. No allocation.
pub(crate) fn stream_set_parallel_mode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    on: bool,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(args.first().copied()),
    };
    if stream_parallel_stage_enabled()
        && !stream_carries_parallel_bit(&*ctx, this)
        && real_pipeline_shape(&*ctx, this).is_some()
    {
        // Wave 15: a REAL pipeline stage runs `AbstractPipeline`'s own body,
        // which sets `sourceStage.parallel` and answers `this`.
        let name = if on { "parallel" } else { "sequential" };
        return ctx.invoke_special(
            "java/util/stream/AbstractPipeline",
            name,
            "()Ljava/util/stream/BaseStream;",
            &[Value::Object(Some(this))],
        );
    }
    if stream_parallel_stage_enabled() && stream_carries_parallel_bit(&*ctx, this) {
        // Round 17 wave 8 (lane streams4): the receiver AND its pipeline's
        // tail ([`stream_pipeline_tail`], `this` itself when it has no next
        // stage), so an earlier, already-linked stage switches the mode its
        // pipeline runs and reports, as `sourceStage.parallel` does.
        let tail = stream_pipeline_tail(&*ctx, this);
        for stage in [this, tail] {
            let word = stream_spl_facts_word(&*ctx, stage);
            let word = if on {
                word | STREAM_FACTS_PARALLEL
            } else {
                word & !STREAM_FACTS_PARALLEL
            };
            ctx.set_field(stage, STREAM_FIELD_SPL_FACTS, Value::Long(word));
        }
    }
    Ok(Some(Value::Object(Some(this))))
}

/// `BaseStream.parallel()` on every stream interface (`native-builtins`
/// `register_basestream_mode_overrides`): see
/// [`stream_parallel_stage_enabled`].
pub fn native_stream_parallel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    stream_set_parallel_mode(ctx, args, true)
}

/// `BaseStream.sequential()`: clears what [`native_stream_parallel`] set.
pub fn native_stream_sequential(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    stream_set_parallel_mode(ctx, args, false)
}

/// `BaseStream.isParallel()`: the stage's [`STREAM_FACTS_PARALLEL`] bit;
/// `false` for every receiver that cannot carry it (the old constant).
pub fn native_stream_is_parallel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let parallel = match args.first() {
        Some(Value::Object(Some(o))) => {
            if stream_carries_parallel_bit(&*ctx, *o) {
                stream_is_parallel(&*ctx, *o)
            } else {
                // Wave 15: a REAL pipeline stage answers its source stage's
                // own flag (`AbstractPipeline.isParallel()`).
                stream_parallel_stage_enabled()
                    && real_pipeline_shape(&*ctx, *o).is_some_and(|shape| shape.parallel)
            }
        }
        _ => false,
    };
    Ok(Some(Value::Int(i32::from(parallel))))
}

/// `CRATONVM_STREAM_SOURCE_FACTS` -- default ON, `=0` restores the old
/// answers. More synthetic SOURCE stages record the characteristics of the
/// JDK spliterator they stand for ([`STREAM_FIELD_SPL_FACTS`]): the one-element
/// `IntStream.of(int)` / `LongStream.of(long)` / `DoubleStream.of(double)`
/// (`Streams.*StreamBuilderImpl`, `0x4450`), `String.chars()` (and a Latin-1
/// string's `codePoints()`; `CharsSpliterator`, `0x4450`), and a hash
/// container's `stream()` (`HashSet`, `LinkedHashSet`, the `HashMap` /
/// `LinkedHashMap` / `Hashtable` key, entry and value views: their own
/// spliterator's cell, `0x41` for a `HashSet`). They recorded nothing, so
/// `spliterator()` answered the list default `0x4050` (reference) or `0x4040`
/// (primitive), and every stage derived from a hash source claimed ORDERED
/// (`HashSet.stream().map(f).spliterator()` is `0x40` on HotSpot). Round 16
/// wave 14, lane compat2 (proposals CC13-1 and SPL9-2). Read live, per call.
pub fn stream_source_facts_enabled() -> bool {
    spliterator_facts_enabled()
}

/// Record on `stream` -- a synthetic primitive stream a `native-builtins`
/// SOURCE native just built over an array of its elements -- that its JDK
/// spliterator is an array-style one (`ORDERED | SIZED | SUBSIZED |
/// IMMUTABLE`, `0x4450`). No-op under [`stream_source_facts_enabled`]'s kill
/// switch and for a stream too short to carry the facts. No allocation.
pub fn record_array_source_spliterator_facts(ctx: &mut dyn NativeContext, stream: ObjectRef) {
    if stream_source_facts_enabled() {
        stream_set_source_spl_facts(ctx, stream, SPL_IMMUTABLE_ARRAY, Value::Object(None));
    }
}

/// Record on `stream` -- a synthetic SOURCE stream a `native-builtins` native
/// just built -- that the JDK spliterator it stands for reports `chars`
/// (public bits only; no comparator). JIT round 17 wave 2, lane streams2:
/// `String.lines()` (`LinesSpliterator`, `ORDERED | IMMUTABLE | NONNULL`).
/// Same kill switches as [`record_array_source_spliterator_facts`]; a stream
/// too short for the facts slot is left alone. No allocation.
pub fn record_source_spliterator_characteristics(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
    chars: i32,
) {
    if stream_source_facts_enabled() {
        stream_set_source_spl_facts(ctx, stream, chars & !SPL_PRIV_MASK, Value::Object(None));
    }
}

/// Record on `stream` -- the lazy carrier `StreamSupport.stream(spl, ..)`
/// (`native-builtins` `service_loader.rs`) just built over the REAL
/// spliterator `spl` -- the flags HotSpot's `StreamOpFlag.fromCharacteristics`
/// reads off `spl` at the same point: `spl.characteristics()`, and, for a
/// SORTED source, `spl.getComparator()`, whose non-null answer makes the
/// pipeline not SORTED. Stages derived later start from these flags
/// ([`stream_derive_spl_facts`]) instead of staying undescribed. JIT round 17
/// wave 1, lane streams.
///
/// Both calls are Java and may move `stream`; it is pinned across them and
/// the refreshed reference is returned. A call that fails records nothing
/// and the stream is answered as before (HotSpot would raise the source's
/// exception from `StreamSupport.stream`; the old carrier never asked, and a
/// fixture spliterator with no such method must not start failing). A
/// `stream` too short for the facts slot, or `CRATONVM_STREAM_SOURCE_FACTS=0`
/// / `CRATONVM_SPLITERATOR_FACTS=0`, records nothing (and makes no call).
pub fn record_spliterator_source_facts(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
    spl: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    if !stream_source_facts_enabled() || ctx.object_num_fields(stream) <= STREAM_FIELD_SPL_FACTS {
        return Ok(stream);
    }
    let stream_pin = ctx.pin_native_root(stream);
    // Pinned above `stream_pin`: unpinning at `stream_pin` releases both.
    let spl_pin = ctx.pin_native_root(spl);
    // Round 17 wave 3 (lane streams3): a source's own exception is the answer
    // ([`source_facts_failure_to_raise`]).
    let mut raise: Option<MethodCallFailed> = None;
    let flags = match ctx.invoke_virtual(spl, "characteristics", "()I", &[]) {
        Ok(Some(Value::Int(chars))) if chars & SPL_SORTED != 0 => {
            let spl = ctx.read_native_pin(spl_pin, spl);
            match ctx.invoke_virtual(spl, "getComparator", "()Ljava/util/Comparator;", &[]) {
                Ok(Some(Value::Object(Some(_)))) => Some(chars & !SPL_SORTED),
                Ok(_) => Some(chars),
                Err(e) => {
                    raise = source_facts_failure_to_raise(&*ctx, e);
                    None
                }
            }
        }
        Ok(Some(Value::Int(chars))) => Some(chars),
        Err(e) => {
            raise = source_facts_failure_to_raise(&*ctx, e);
            None
        }
        _ => None,
    };
    if let Some(e) = raise {
        ctx.unpin_native_roots(stream_pin);
        return Err(e);
    }
    let stream = ctx.read_native_pin(stream_pin, stream);
    if let Some(flags) = flags {
        // Round 17 wave 2 (lane streams2): a source reporting 0 is recorded
        // as described ([`SPL_PRIV_DESCRIBED`]); it used to record nothing.
        let flags = flags & !SPL_PRIV_MASK;
        let flags = if flags == 0 && stream_zero_facts_enabled() {
            SPL_PRIV_DESCRIBED
        } else {
            flags
        };
        stream_set_source_spl_facts(ctx, stream, flags, Value::Object(None));
    }
    ctx.unpin_native_roots(stream_pin);
    Ok(stream)
}

/// `CRATONVM_STREAM_SUPPORT_SOURCE_FACTS_THROW` -- default ON, `=0` restores
/// the lenient carrier. Round 17 wave 3, lane streams3
/// (`r17-streamsupport-lazy-carrier-residual-shapes-FIXED-20261005.md` residual 6,
/// proposal STREAMS2-3). HotSpot's `StreamSupport.stream(spl, ..)` calls
/// `spl.characteristics()` (and `getComparator()` for a SORTED source) in the
/// `ReferencePipeline.Head` constructor, so an exception from either is
/// thrown by `StreamSupport.stream` itself. [`record_spliterator_source_facts`]
/// swallowed every failure. With the switch on, a JAVA exception is the
/// answer: a thrown `Throwable` that is not a `LinkageError`, or a VM runtime
/// exception other than `UnsatisfiedLinkError`. A resolution failure (a VM
/// fixture spliterator with no such method: `NoSuchMethodError`,
/// `AbstractMethodError`, a linkage or internal error) is still swallowed,
/// for the reason the lenient form was kept (`vm/src/vm/tests.rs`
/// `stream_support_p69`). Returns the failure to raise, or `None` to record
/// nothing and carry on. Pure reads.
pub(crate) fn source_facts_failure_to_raise(
    ctx: &dyn NativeContext,
    e: MethodCallFailed,
) -> Option<MethodCallFailed> {
    let raise = match &e {
        MethodCallFailed::ExceptionThrown(t) => {
            !obj_is_instance_of(ctx, *t, "java/lang/LinkageError")
        }
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(r)) => !matches!(
            r,
            RuntimeError::UnsatisfiedLinkError { .. }
                | RuntimeError::NoSuchMethodException { .. }
                | RuntimeError::ClassNotFoundException { .. }
                | RuntimeError::InternalError { .. }
        ),
        _ => false,
    };
    raise.then_some(e)
}

/// `CRATONVM_STREAM_SUPPORT_ZERO_FACTS` -- default ON, `=0` restores the
/// undescribed carrier. JIT round 17 wave 2, lane streams2
/// (`r17-streamsupport-lazy-carrier-residual-shapes-FIXED-20261005.md` residual 1,
/// proposal STR17-3). `StreamSupport.stream(spl, ..)` over a source whose
/// `characteristics()` is 0 (`Spliterators.spliteratorUnknownSize(it, 0)`,
/// so every `Iterable.spliterator()` default) recorded nothing, because the
/// facts word reads 0 as "undescribed". A stage over it stayed undescribed:
/// `StreamSupport.stream(spl, false).map(f).spliterator()` split and reported
/// the list default `0x4050`, where HotSpot's sequential
/// `WrappingSpliterator` reports `0` and does not split. With the switch on,
/// such a source records [`SPL_PRIV_DESCRIBED`] and its stages derive `0`.
/// Read live, per call.
pub(crate) fn stream_zero_facts_enabled() -> bool {
    true
}

/// `CRATONVM_STREAM_LAZY_STAGE_ESTIMATE` -- default ON, `=0` restores the
/// snapshot's count. JIT round 17 wave 2, lane streams2
/// (`r17-streamsupport-lazy-carrier-residual-shapes-FIXED-20261005.md` residual 2).
/// An unsized stage (`filter`, `flatMap`, `distinct`) over a lazy
/// `StreamSupport.stream` source has no recorded head size (the source was
/// not drained when the stage was built), so its spliterator's
/// `estimateSize()` was the count left after the op. HotSpot's
/// `WrappingSpliterator.estimateSize()` is the SOURCE spliterator's own
/// estimate (`list.stream().filter(p).spliterator().estimateSize()` is the
/// list's size; `Long.MAX_VALUE` over `spliteratorUnknownSize`). With the
/// switch on, [`native_stream_spliterator`] asks the undrained source its
/// `estimateSize()` just before draining it. Read live, per call.
pub(crate) fn stream_lazy_stage_estimate_enabled() -> bool {
    true
}

/// `CRATONVM_STREAM_LAZY_UNORDERED_STAGE` -- default ON, `=0` restores the
/// identity. JIT round 17 wave 2, lane streams2
/// (`r17-streamsupport-lazy-carrier-residual-shapes-FIXED-20261005.md` residual 4).
/// `unordered()` on an ORDERED lazy head (`list.stream().unordered()` under
/// `--compatible`) answered `this`, because [`stream_unordered_derives`]
/// will not drain a source. HotSpot builds a new `NOT_ORDERED` stage that
/// links the head. With the switch on, [`native_stream_unordered`] builds
/// that stage WITHOUT draining: it takes over the head's undrained source
/// spliterator ([`stream_lazy_unordered_stage`]). Read live, per call.
pub(crate) fn stream_lazy_unordered_stage_enabled() -> bool {
    true
}

/// `CRATONVM_SPLITERATOR_FACTS` (default on; `=0` is the kill switch): the
/// `--compatible` synthetic spliterators report the comparator,
/// characteristics, split policy and estimate HotSpot's do (wave 9, lane
/// spliter). Off restores the pre-wave-9 answers exactly: the facts streams
/// record are then never read, and every producer builds its old shape.
/// Read per `spliterator()` call, never per element.
pub(crate) fn spliterator_facts_enabled() -> bool {
    true
}

// ===========================================================================
// keycloak-16 Part B — lazy / short-circuiting synthetic Stream pipeline
//
// Gated behind `CRATONVM_LAZY_STREAMS` (default OFF — gate-off behaviour is
// byte-identical to the historical eager pipeline). When ON, the value/cardinality
// neutral-or-changing intermediate ops (peek/map/filter/limit/skip) DEFER instead
// of materialising: they record themselves on the result stream's op-chain (slot 3)
// while sharing the upstream's SOURCE element array (slot 0). Terminal ops then
// either pull element-by-element with short-circuit (findFirst/findAny/anyMatch/
// allMatch/noneMatch) or fully materialise via `stream_drain` (count/collect/
// toArray/forEach/reduce/sorted/...). This makes
//   Stream.of(1,2,3,4,5).peek(p).findFirst()
// run `p` exactly once, matching HotSpot's lazy short-circuit semantics, while
// keeping every eager terminal's *result* identical.
// ===========================================================================

/// Deferred-op chain slot on a lazy synthetic stream: an `Object[]` of
/// `cratonvm/stream/LazyOp` records (field0=kind:Int, field1=lambda:Object|null,
/// field2=aux:Long). Present only on streams produced by a deferred intermediate
/// op under `CRATONVM_LAZY_STREAMS`; older 2/3-field streams simply have no chain.
pub(crate) const STREAM_FIELD_OP_CHAIN: usize = 3;
/// A deferred-op stream's layout. Kept as its own name for the call site's
/// intent, but equal to `STREAM_NUM_FIELDS` since W7-65: a lazy stage must
/// carry the linked-or-consumed slot too, or the terminal that finally reads it
/// would have nowhere to record the consume.
pub(crate) const STREAM_NUM_FIELDS_LAZY: usize = STREAM_NUM_FIELDS;

pub(crate) const LAZY_OP_PEEK: i32 = 0;
pub(crate) const LAZY_OP_MAP: i32 = 1;
pub(crate) const LAZY_OP_FILTER: i32 = 2;
pub(crate) const LAZY_OP_LIMIT: i32 = 3;
pub(crate) const LAZY_OP_SKIP: i32 = 4;
pub(crate) const LAZY_OP_FLAT_MAP: i32 = 5;

/// `true` when the deferred/short-circuit synthetic Stream pipeline is active
/// (keycloak-16 Part B). **Default ON** (HotSpot-faithful lazy semantics); opt out
/// to the legacy eager pipeline with `CRATONVM_EAGER_STREAMS=1`. The historical
/// opt-in `CRATONVM_LAZY_STREAMS` is still honored as an explicit force-on so it
/// wins over the opt-out if both are set.
pub(crate) fn lazy_streams_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        if None::<std::ffi::OsString>.is_some() {
            return true; // explicit force-on wins        
}
        None::<std::ffi::OsString>.is_none()    })}

/// A single deferred intermediate op read back from a stream's chain.
#[derive(Clone)]
pub(crate) struct LazyOp {
    pub(in crate::streams) kind: i32,
    pub(in crate::streams) lambda: Option<ObjectRef>,
    pub(in crate::streams) aux: i64,
}

/// `true` if `this` is a synthetic stream that currently carries a non-empty
/// deferred op-chain (slot 3). Cheap guard used by terminals to pick the lazy path.
pub(crate) fn stream_has_chain(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    if ctx.object_num_fields(this) <= STREAM_FIELD_OP_CHAIN {
        return false;
    }
    match ctx.get_field(this, STREAM_FIELD_OP_CHAIN) {
        Value::Object(Some(a)) => ctx.array_length(a) > 0,
        _ => false,
    }
}

/// Read the deferred op-chain (slot 3) into a Vec. Empty for non-lazy streams.
pub(crate) fn stream_read_chain(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<LazyOp> {
    if ctx.object_num_fields(this) <= STREAM_FIELD_OP_CHAIN {
        return Vec::new();
    }
    let arr = match ctx.get_field(this, STREAM_FIELD_OP_CHAIN) {
        Value::Object(Some(a)) => a,
        _ => return Vec::new(),
    };
    let n = ctx.array_length(arr);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        if let Value::Object(Some(rec)) = ctx.get_array_element(arr, i) {
            let kind = ctx.get_field(rec, 0).as_int().unwrap_or(-1);
            let lambda = match ctx.get_field(rec, 1) {
                Value::Object(o) => o,
                _ => None,
            };
            let aux = match ctx.get_field(rec, 2) {
                Value::Long(v) => v,
                Value::Int(v) => v as i64,
                _ => 0,
            };
            out.push(LazyOp { kind, lambda, aux });
        }
    }
    out
}

/// Raw SOURCE elements of a (lazy) synthetic stream — slot 0, BEFORE the op-chain
/// is applied. Drains a lazy spliterator source first.
///
/// The drain is the fallible part, and the `let _ =` on it was the launderer:
/// a spliterator whose `tryAdvance` refused or threw left slot 0 holding
/// whatever it held before, and the pull below then read that as the stream's
/// source — a truncated or empty pipeline, with no exception anywhere.
pub(crate) fn stream_source_elems(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    let this_pin = ctx.pin_native_root(this);
    let mut this_cur = ctx.read_native_pin(this_pin, this);
    let drained = materialize_lazy_stream(ctx, &mut this_cur);
    // THROWS5-3: a lazy walk's frames not built yet are built first (a no-op
    // for every other stream).
    let drained = drained.and_then(|()| {
        let this_cur = ctx.read_native_pin(this_pin, this);
        lazy_walk_fill(ctx, this_cur, 0, usize::MAX)
    });
    let this_cur = ctx.read_native_pin(this_pin, this);
    let elems = match ctx.get_field(this_cur, STREAM_FIELD_ELEMENTS) {
        Value::Object(Some(arr)) => {
            let len = ctx.array_length(arr);
            (0..len).map(|i| ctx.get_array_element(arr, i)).collect()
        }
        _ => Vec::new(),
    };
    // Unpin before propagating: a `?` on `drained` at its own line would skip
    // the truncate below.
    ctx.unpin_native_roots(this_pin);
    drained?;
    Ok(elems)
}

/// `CRATONVM_STREAM_LAZYOP_REFUSAL_MEMO=1` (WALK7-3, JIT round 19 wave 8, lane
/// walk8; default on, default on). Under `--real-jdk` every intermediate op on a
/// synthetic stream asks `try_ensure_synthetic_class("cratonvm/stream/LazyOp")`
/// and is refused: the class manager's write lock, two name classifications,
/// three `String`s, the warn counter and a formatted error, per call. The
/// answer is a function of the VM's policy (no JDK declares the name, and a
/// refusal is never reversed in a VM's life), so with the switch on a thread
/// remembers the refusal for its VM and answers `Ok(None)` (the eager body)
/// without the call. What changes besides the cost: the refusal's warn
/// `occurrence` counter and the per-call `CompatibilityClassRequested`
/// violation count one per thread and VM instead of one per op (the census
/// row itself is deduplicated by name already). Read once per process.
pub(crate) fn lazy_op_refusal_memo_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

thread_local! {
    /// WALK7-3: the `vm_identity` of the VM whose `LazyOp` refusal this thread
    /// saw (`0`: none). Identities are never reused (`NEXT_VM_IDENTITY`
    /// counts from 1), so a stale entry can only miss; no process global.
    pub(crate) static LAZY_OP_REFUSED_VM: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Has this thread already been refused `cratonvm/stream/LazyOp` by this VM
/// ([`lazy_op_refusal_memo_enabled`])? A context with no identity (`0`, the
/// mocks) never memoizes.
pub(crate) fn lazy_op_refusal_memoized(ctx: &dyn NativeContext) -> bool {
    if !lazy_op_refusal_memo_enabled() {
        return false;
    }
    let vm = ctx.vm_identity();
    vm != 0 && LAZY_OP_REFUSED_VM.with(|c| c.get()) == vm
}

/// Remember a `LazyOp` refusal for this thread and VM (switch on only).
pub(crate) fn note_lazy_op_refused(ctx: &dyn NativeContext) {
    if !lazy_op_refusal_memo_enabled() {
        return;
    }
    let vm = ctx.vm_identity();
    if vm != 0 {
        LAZY_OP_REFUSED_VM.with(|c| c.set(vm));
    }
}

/// Build a lazy derived stream that appends op `(kind, lambda, aux)` to `src`'s
/// op-chain while sharing `src`'s SOURCE array (slot 0). Caller must have checked
/// `src` is a synthetic stream. No `invoke_virtual` here, so no safepoint/GC moves
/// the locals between allocations.
///
/// Answers `Ok(None)` — "not deferred, run your eager body" — when the policy
/// will not mint the op record. See the guard at the top of the body.
pub(crate) fn stream_make_lazy_derived(
    ctx: &mut dyn NativeContext,
    src: ObjectRef,
    kind: i32,
    lambda: Option<ObjectRef>,
    aux: i64,
) -> MethodCallResult {
    // Wave 9 (lane spliter): the deferred stage's spliterator facts, read from
    // `src` before anything below allocates.
    let spl_op = match kind {
        LAZY_OP_MAP => Some(SplOp::Map),
        LAZY_OP_FILTER => Some(SplOp::Filter),
        LAZY_OP_FLAT_MAP => Some(SplOp::FlatMap),
        LAZY_OP_PEEK | LAZY_OP_LIMIT | LAZY_OP_SKIP => Some(SplOp::Preserve),
        _ => None,
    };
    let spl_facts = spl_op.and_then(|op| stream_derive_spl_facts(&*ctx, src, op));
    // Real-JDK wave 2 (lane W2-1). Ask the policy BEFORE minting anything, for
    // the same reason the three `cratonvm/internal/StreamCollector` sites do
    // (lane L7, 2026-08-05): a fabrication that happens to run FIRST makes
    // every other site's refusal order-dependent rather than a policy.
    //
    // `cratonvm/stream/LazyOp` is the deferred-op record this function exists
    // to build. No JDK declares that name, so `--real-jdk` refuses it. The
    // refusal used to arrive from the `try_alloc_synthetic` further down —
    // after the new op-chain array was already allocated — and surfaced at the
    // user's own `filter`/`map`/`flatMap`/`limit`/`skip`/`peek` call site as
    // `NoClassDefFoundError: cratonvm/stream/LazyOp`. That killed
    // `RJdkCollections` (`src.stream().filter(...)`, line 169) and `RJdkJmx`
    // (`ManagementFactory.getPlatformMBeanServer()`, i.e. real JDK bytecode
    // reaching a stream internally) outright, on a mode where both pass today.
    //
    // The refusal is not fatal, because deferring is an OPTIMISATION and not a
    // requirement. `Ok(None)` here is the exact answer `stream_try_defer`
    // already gives when `CRATONVM_EAGER_STREAMS=1` is set, and every caller
    // has a complete eager body behind that answer
    // (`native_stream_{filter,map,flat_map,limit,skip,peek}`) — the pipeline
    // that was the shipped default until the lazy one was turned on. Element
    // RESULTS are identical; what strict mode loses is short-circuiting:
    // `peek` runs on every element rather than stopping at the first
    // `findFirst`, and an unbounded source feeding `limit(n)` stops at the
    // drain's safety cap instead of at `n`.
    //
    // Deliberately NOT laundered through `ensure_generated_class`
    // (`ClassOrigin::VmInternal`, which never refuses in either mode). A
    // `LazyOp` record is only VM-internal in shape; its REASON for existing is
    // the synthetic `java.util.stream` model, which is the thing strict mode
    // is supposed to be retiring. Minting it there would make the pipeline work
    // under `--real-jdk` and take the gap off the census at the same time. This
    // spelling keeps the `CompatibilityClassRequested` violation recorded on
    // every refused call, so the census still reports the gap's size.
    //
    // Under the default `Compatible` mode this is not a behaviour change: it is
    // the same `try_ensure_synthetic_class(name, 3)` call, with the same field
    // count, that `try_alloc_synthetic`'s `refused_class` arm below already
    // makes on the first deferred op of the run.
    //
    // GC-SAFETY: the guard sits BEHIND the two pins, not in front of them.
    // Registering a class can allocate (the `java.lang.Class` mirror), and in
    // `Compatible` mode this is the same call that already ran from inside
    // `try_alloc_synthetic` — i.e. with `src` and `lambda` already pinned.
    // Hoisting it above the pins would open a window where a moving young GC
    // relocates the two bare argument refs. On the refusal arm, unpinning at
    // `src_pin` (the lower handle) pops `lambda_pin` with it; the CALLER's own
    // pins were taken first and are below both, so they survive.
    let src_pin = ctx.pin_native_root(src);
    let lambda_pin = lambda.map(|l| ctx.pin_native_root(l)).unwrap_or(usize::MAX);
    // WALK7-3 (`CRATONVM_STREAM_LAZYOP_REFUSAL_MEMO`): a refusal this thread
    // already saw for this VM is answered without asking again.
    if lazy_op_refusal_memoized(&*ctx) {
        ctx.unpin_native_roots(src_pin);
        return Ok(None);
    }
    if ctx
        .try_ensure_synthetic_class("cratonvm/stream/LazyOp", 3)
        .is_err()
    {
        note_lazy_op_refused(&*ctx);
        ctx.unpin_native_roots(src_pin);
        return Ok(None);
    }
    let src_cur = ctx.read_native_pin(src_pin, src);
    // A lazy stage derived from a `StackWalker.walk` stream, or from a stream
    // derived from one, names that walk stream as the eager stages do
    // (`make_derived_stream`), so it refuses once the walk has returned.
    // Interpreter round i1 wave 50, lane L3: the lazy pipeline is the default
    // under `--compatible` (`--real-jdk` refuses `LazyOp` and runs the eager
    // stages), and it dropped the walk slot, so a stream derived inside the
    // walk's function stayed usable after it
    // (`L3W44DerivedWalkStream.java`, `--compatible`).
    // Pinned above `src_pin`: the unpins at `src_pin` release it.
    let walk = stream_walk_origin(&*ctx, src_cur);
    let walk_pin = walk.map(|w| ctx.pin_native_root(w));
    // W7-65: LINK the source.
    //
    // `stream_link_or_consume`'s doc argues the JDK's eight flag sites collapse
    // onto the one funnel in `stream_elements`, because every operation reads
    // the element snapshot through it. That is true of every EAGER operation
    // and false of a lazy one: a lazy intermediate op appends to the op chain
    // and never drains, so it never reached the funnel and never marked its
    // source. Two of the JDK's eight sites are the intermediate-stage
    // constructors, and this function is both of them.
    //
    // MEASURED, `apps/probes/StreamReuseProbe`, compatible mode:
    //
    // ```text
    //   ref filter then filter second     HotSpot IllegalStateException   was ok
    //   ref map then map second           HotSpot IllegalStateException   was ok
    //   ref parent after child linked     HotSpot IllegalStateException   was 3
    // ```
    //
    // AFTER THE `--real-jdk` REFUSAL ARM, NOT BEFORE IT, and that is the whole
    // of why this is here rather than at the top. Strict mode DOES reach this
    // function; it is refused a few lines up, returns `Ok(None)`, and the
    // caller falls back to real java.base bytecode. Marking the source ahead of
    // that refusal armed a stream this VM was about to hand back to the JDK's
    // own pipeline, which then refused the caller's next use of it. Measured as
    // `RJdkCollections` and `RJdkJmx` failing in the `--real-jdk` arm ONLY,
    // 117/119, with both other arms green -- the signature of a change that
    // fires on the strict path and nowhere else.
    //
    // On the error path the pin has to be released by hand: `src_pin` is this
    // frame's base, and `?` here would strand it and everything above it.
    if stream_is_reference_carrier(&*ctx, src_cur) {
        if let Err(e) = stream_link_or_consume(ctx, src_cur) {
            ctx.unpin_native_roots(src_pin);
            return Err(e);
        }
    }
    // FIX (stream-eager-drain-20260715): do NOT eagerly materialize `src`
    // here. A `src` that still holds a live, undrained lazy spliterator
    // (STREAM_FIELD_LAZY_SPLITERATOR, slot 2 -- from
    // `StreamSupport.stream(spliterator, false)`) must have that spliterator
    // propagated forward, UNDRAINED, onto the new derived stream, alongside
    // the extended op-chain built below. Draining it eagerly at THIS point
    // (before the current op is even recorded on the chain) runs the raw
    // `tryAdvance` loop fully disconnected from ANY downstream processing --
    // exactly the "cursor" Iterator bug (`next()` returns `this`; real
    // per-element consumption happens inside the mapper/filter/etc that was
    // about to be appended to the chain) documented in
    // stream-eager-drain-inline-fix-20260715-FIXED.md.
    // The actual drive now happens inline, one element at a time through the
    // FULL chain, the first time a terminal genuinely needs concrete
    // elements (`stream_pull_internal` / `stream_pull_synthetic_downstream`).
    let src_lazy_spl = stream_lazy_spliterator(ctx, src_cur);
    let spl_pin = src_lazy_spl
        .map(|s| ctx.pin_native_root(s))
        .unwrap_or(usize::MAX);
    let source = ctx.get_field(src_cur, STREAM_FIELD_ELEMENTS);
    let source_pin = pin_value(ctx, source);
    let src_chain = if ctx.object_num_fields(src_cur) > STREAM_FIELD_OP_CHAIN {
        match ctx.get_field(src_cur, STREAM_FIELD_OP_CHAIN) {
            Value::Object(Some(a)) => Some(a),
            _ => None,
        }
    } else {
        None
    };
    let src_chain_pin = src_chain
        .map(|a| ctx.pin_native_root(a))
        .unwrap_or(usize::MAX);
    let src_len = match src_chain {
        Some(a) => ctx.array_length(ctx.read_native_pin(src_chain_pin, a)),
        None => 0,
    };
    let new_chain = alloc_ref_array(ctx, src_len + 1);
    let new_chain_pin = ctx.pin_native_root(new_chain);
    if let Some(a) = src_chain {
        let a = ctx.read_native_pin(src_chain_pin, a);
        let new_chain = ctx.read_native_pin(new_chain_pin, new_chain);
        for i in 0..src_len {
            let v = ctx.get_array_element(a, i);
            ctx.set_array_element(new_chain, i, v);
        }
    }
    let rec = try_alloc_synthetic(ctx, "cratonvm/stream/LazyOp", 3)?;
    let rec_pin = ctx.pin_native_root(rec);
    let lambda_cur = lambda.map(|l| ctx.read_native_pin(lambda_pin, l));
    let rec = ctx.read_native_pin(rec_pin, rec);
    ctx.set_field(rec, 0, Value::Int(kind));
    ctx.set_field(rec, 1, Value::Object(lambda_cur));
    ctx.set_field(rec, 2, Value::Long(aux));
    let new_chain = ctx.read_native_pin(new_chain_pin, new_chain);
    ctx.set_array_element(new_chain, src_len, Value::Object(Some(rec)));
    let num_fields = match walk {
        // THROWS5-3: a lazy walk's derived streams carry its slice slot.
        Some(walk) => {
            let walk = ctx.read_native_pin(walk_pin.unwrap_or(usize::MAX), walk);
            walk_derived_num_fields(&*ctx, walk)
        }
        None => STREAM_NUM_FIELDS_LAZY,
    };
    // THROWS5-3: a slice's offset travels with its array (read before the
    // allocations below; a plain word).
    let src_view = {
        let src_now = ctx.read_native_pin(src_pin, src);
        if ctx.object_num_fields(src_now) > STREAM_FIELD_WALK_VIEW {
            Some(ctx.get_field(src_now, STREAM_FIELD_WALK_VIEW))
        } else {
            None
        }
    };
    let stream = try_alloc_synthetic(ctx, "java/util/stream/Stream", num_fields)?;
    // `stream_inherit_close_handlers` can allocate while it reads/copies the
    // upstream handlers. The derived stream has already become the owner of
    // the new op chain by then, so root it before the first store and always
    // return its current address after that call. A raw local here let a
    // moving young GC strand the just-built Stream and its LazyOp chain.
    let stream_pin = ctx.pin_native_root(stream);
    let stream_cur = ctx.read_native_pin(stream_pin, stream);
    let source_cur = read_pinned_elem(ctx, source_pin, source);
    let new_chain = ctx.read_native_pin(new_chain_pin, new_chain);
    ctx.set_field(stream_cur, STREAM_FIELD_ELEMENTS, source_cur);
    ctx.set_field(stream_cur, STREAM_FIELD_CLOSE_HANDLERS, Value::Object(None));
    ctx.set_field(
        stream_cur,
        STREAM_FIELD_OP_CHAIN,
        Value::Object(Some(new_chain)),
    );
    if let Some(packed) = spl_facts {
        stream_set_spl_facts(ctx, stream_cur, packed);
    }
    // Propagate the still-undrained lazy spliterator (if any) forward so the
    // eventual terminal drives it inline through the now-extended chain.
    if let Some(spl) = src_lazy_spl {
        let spl_cur = ctx.read_native_pin(spl_pin, spl);
        ctx.set_field(
            stream_cur,
            STREAM_FIELD_LAZY_SPLITERATOR,
            Value::Object(Some(spl_cur)),
        );
    }
    if let (Some(w), Some(wp)) = (walk, walk_pin) {
        let walk_cur = ctx.read_native_pin(wp, w);
        let stream_cur = ctx.read_native_pin(stream_pin, stream);
        ctx.set_field(stream_cur, STREAM_FIELD_WALK, Value::Object(Some(walk_cur)));
        let has_view_slot = ctx.object_num_fields(stream_cur) > STREAM_FIELD_WALK_VIEW;
        if let Some(view) = src_view.filter(|_| has_view_slot) {
            ctx.set_field(stream_cur, STREAM_FIELD_WALK_VIEW, view);
        }
    }
    let src_cur = ctx.read_native_pin(src_pin, src);
    let stream_cur = ctx.read_native_pin(stream_pin, stream);
    stream_inherit_close_handlers(ctx, src_cur, stream_cur);
    let stream_cur = ctx.read_native_pin(stream_pin, stream);
    // Round 17 wave 8 (lane streams4): `src` now has a next stage. Both
    // re-read from their pins; pure stores.
    let src_cur = ctx.read_native_pin(src_pin, src);
    stream_record_next_stage(ctx, src_cur, stream_cur);
    if spl_pin != usize::MAX {
        ctx.unpin_native_roots(spl_pin);
    }
    ctx.unpin_native_roots(stream_pin);
    ctx.unpin_native_roots(src_pin);
    Ok(Some(Value::Object(Some(stream_cur))))
}

/// `true` iff `this` is one of our synthetic stream interface objects.
pub(crate) fn stream_is_synthetic(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let cn = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_default();
    is_synthetic_stream(&cn)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PullStep {
    Continue,
    Stop,
}

pub(crate) type StreamEmit<'a> =
    dyn FnMut(&mut dyn NativeContext, Value) -> Result<PullStep, MethodCallFailed> + 'a;

pub(crate) fn invoke_deferred_stream_lambda(
    ctx: &mut dyn NativeContext,
    lambda: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    let lambda_pin = ctx.pin_native_root(lambda);
    let (_, arg_handles) = pin_value_slice(ctx, args);
    let lambda = ctx.read_native_pin(lambda_pin, lambda);
    let args = read_value_slice(ctx, &arg_handles, args);
    let result = ctx.invoke_virtual(lambda, method_name, descriptor, &args);
    ctx.unpin_native_roots(lambda_pin);
    result
}

pub(crate) fn pin_lazy_chain_lambdas(ctx: &mut dyn NativeContext, chain: &[LazyOp]) -> Vec<usize> {
    chain
        .iter()
        .map(|op| match op.lambda {
            Some(lambda) => ctx.pin_native_root(lambda),
            None => usize::MAX,
        })
        .collect()
}

pub(crate) fn read_lazy_chain_lambda(ctx: &dyn NativeContext, op: &LazyOp, pin: usize) -> Option<ObjectRef> {
    op.lambda.map(|lambda| match pin {
        usize::MAX => lambda,
        _ => ctx.read_native_pin(pin, lambda),
    })
}

pub(crate) struct StreamPullState {
    pub(in crate::streams) limit_passed: Vec<i64>,
    pub(in crate::streams) skip_done: Vec<i64>,
}

pub(crate) fn stream_new_pull_state(chain_len: usize) -> StreamPullState {
    StreamPullState {
        limit_passed: vec![0i64; chain_len],
        skip_done: vec![0i64; chain_len],
    }
}

pub(crate) fn value_is_exact_class(ctx: &dyn NativeContext, v: Value, class_name: &str) -> bool {
    match v {
        Value::Object(Some(o)) => {
            ctx.class_name_arc_of_id(ctx.class_id_of_object(o))
                .as_deref()
                == Some(class_name)
        }
        _ => false,
    }
}

pub(crate) fn object_is_exact_class(ctx: &dyn NativeContext, o: ObjectRef, class_name: &str) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(o))
        .as_deref()
        == Some(class_name)
}

pub(crate) fn is_placeholder_object_class_cast(
    ctx: &dyn NativeContext,
    input: Value,
    err: &MethodCallFailed,
) -> bool {
    value_is_exact_class(ctx, input, "java/lang/Object")
        && matches!(err, MethodCallFailed::ExceptionThrown(ex) if object_is_exact_class(ctx, *ex, "java/lang/ClassCastException"))
}

pub(crate) fn stream_limit_saturated(chain: &[LazyOp], state: &StreamPullState, start: usize) -> bool {
    for i in start..chain.len() {
        let op = &chain[i];
        if op.kind == LAZY_OP_LIMIT && state.limit_passed[i] >= op.aux {
            return true;
        }
    }
    false
}

pub(crate) fn stream_process_chain(
    ctx: &mut dyn NativeContext,
    mut cur: Value,
    chain: &[LazyOp],
    chain_pins: &[usize],
    start: usize,
    state: &mut StreamPullState,
    emit: &mut StreamEmit<'_>,
) -> Result<PullStep, MethodCallFailed> {
    if stream_limit_saturated(chain, state, start) {
        return Ok(PullStep::Stop);
    }

    let mut cur_pin = pin_value(ctx, cur);
    let mut i = start;
    while i < chain.len() {
        cur = read_pinned_elem(ctx, cur_pin, cur);
        let op = &chain[i];
        let lambda =
            read_lazy_chain_lambda(ctx, op, chain_pins.get(i).copied().unwrap_or(usize::MAX));
        match op.kind {
            LAZY_OP_PEEK => {
                if let Some(l) = lambda {
                    invoke_deferred_stream_lambda(
                        ctx,
                        l,
                        "accept",
                        "(Ljava/lang/Object;)V",
                        &[cur],
                    )?;
                }
            }
            LAZY_OP_MAP => {
                if let Some(l) = lambda {
                    match invoke_deferred_stream_lambda(
                        ctx,
                        l,
                        "apply",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                        &[cur],
                    ) {
                        Ok(r) => {
                            if cur_pin != usize::MAX {
                                ctx.unpin_native_roots(cur_pin);
                            }
                            cur = r.unwrap_or(Value::Object(None));
                            cur_pin = pin_value(ctx, cur);
                        }
                        Err(e) if is_placeholder_object_class_cast(ctx, cur, &e) => {
                            // SPR-AOT-JUNIT-URI.1 (2026-07-08) - JUnit suite
                            // discovery can leak a raw Object placeholder into
                            // DefaultClasspathScanner's URI stream. HotSpot
                            // never exposes it; treating it as absent restores
                            // package scanning while preserving real CCEs.
                            return Ok(PullStep::Continue);
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            LAZY_OP_FILTER => {
                if let Some(l) = lambda {
                    let t = invoke_deferred_stream_lambda(
                        ctx,
                        l,
                        "test",
                        "(Ljava/lang/Object;)Z",
                        &[cur],
                    )?;
                    if !matches!(t, Some(Value::Int(x)) if x != 0) {
                        return Ok(PullStep::Continue);
                    }
                }
            }
            LAZY_OP_LIMIT => {
                state.limit_passed[i] += 1;
            }
            LAZY_OP_SKIP => {
                if state.skip_done[i] < op.aux {
                    state.skip_done[i] += 1;
                    return Ok(PullStep::Continue);
                }
            }
            LAZY_OP_FLAT_MAP => {
                let mapped = if let Some(l) = lambda {
                    invoke_deferred_stream_lambda(
                        ctx,
                        l,
                        "apply",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                        &[cur],
                    )?
                    .unwrap_or(Value::Object(None))
                } else {
                    Value::Object(None)
                };
                if let Value::Object(Some(inner_stream)) = mapped {
                    let inner_pin = ctx.pin_native_root(inner_stream);
                    let pull = stream_pull_any_downstream(
                        ctx,
                        inner_stream,
                        chain,
                        chain_pins,
                        i + 1,
                        state,
                        emit,
                    );
                    let inner_stream = ctx.read_native_pin(inner_pin, inner_stream);
                    let close = ctx
                        .invoke_virtual(inner_stream, "close", "()V", &[])
                        .map(|_| ());
                    ctx.unpin_native_roots(inner_pin);
                    let step = pull?;
                    close?;
                    return Ok(step);
                }
                return Ok(PullStep::Continue);
            }
            _ => {}
        }
        i += 1;
    }
    let cur = read_pinned_elem(ctx, cur_pin, cur);
    let result = emit(ctx, cur);
    if cur_pin != usize::MAX {
        ctx.unpin_native_roots(cur_pin);
    }
    result
}

/// How often the synthetic-stream drain loops re-publish this thread's GC root
/// snapshot: every Nth element, not every element.
///
/// `NativeContext::refresh_root_snapshot`'s contract asks for a publish "right
/// after establishing such a batch of pins (**and optionally again
/// periodically** across a long per-element loop)". The up-front publish is the
/// one that closes the documented gap — it is what makes this thread's
/// `native_pin_roots` visible to a peer-initiated STW collection at all. The
/// per-iteration one is the optional half, and it was running on EVERY element.
///
/// It is not free. Each call is a full deposit: the whole per-frame root
/// snapshot is cleared and rebuilt, `scan_active_jit_frames` runs (including
/// the A5 unregistered-frame probe, a raw word scan of the native stack above
/// the scanner), and the SATB buffer is flushed. Measured on a
/// `StackWalker.walk` over a 120-frame stack, 500 walks
/// (`CRATONVM_DBG_ROOTPROF=1`):
///
/// ```text
/// scan_active_jit_frames by caller: gc-roots=0 safepoint=0 blocked-deposit=86,700
/// jitprobe calls=86,081 words=637,620,424
/// ```
///
/// 638 MILLION words — 5.1 GB of native stack read — for 500 walks, at a
/// constant ~7,400 words (59 KB) per call, with the call COUNT scaling linearly
/// in stack depth because the stream has one element per frame. Neither the
/// collector nor a safepoint asked for any of it: both other callers are zero.
///
/// The pins this refresh exists to publish are pushed ONCE, before the loop,
/// and do not change across iterations — so every one of those deposits
/// republished a bit-identical snapshot. Refreshing every Nth element keeps the
/// periodic republish the contract asks for, with a bounded window, at 1/N the
/// cost. `CRATONVM_GC_STREAM_REFRESH_EACH=1` restores the per-element cadence,
/// so both arms are reachable from one binary.
pub(crate) fn stream_refresh_interval() -> usize {
    static EVERY: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *EVERY.get_or_init(|| {
        if None::<std::ffi::OsString>.is_some() {
            1        } else {
            32        }    })}

/// Should the drain loop re-publish before element `idx`?
///
/// `idx == 0` is never asked (the caller publishes up front, unconditionally).
#[inline]
pub(crate) fn stream_should_refresh(idx: usize) -> bool {
    let every = stream_refresh_interval();
    every <= 1 || idx % every == 0
}

pub(crate) fn stream_pull_internal(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    emit: &mut StreamEmit<'_>,
) -> Result<PullStep, MethodCallFailed> {
    let stream_pin = ctx.pin_native_root(this);
    let stream = ctx.read_native_pin(stream_pin, this);
    let chain = stream_read_chain(ctx, stream);
    let chain_pins = pin_lazy_chain_lambdas(ctx, &chain);

    let result: Result<PullStep, MethodCallFailed> = (|| {
        let stream = ctx.read_native_pin(stream_pin, this);
        // FIX (stream-eager-drain-20260715): a stream still holding a live,
        // undrained lazy spliterator must have its elements driven INLINE
        // through `chain`, one `tryAdvance` at a time -- NOT via
        // `stream_source_elems` (which would fully drain the raw source with
        // a dumb append-only collector BEFORE any op in `chain` ever runs).
        // See stream-eager-drain-inline-fix-20260715-FIXED.md.
        if let Some(spl) = stream_lazy_spliterator(ctx, stream) {
            let mut emit_stopped = false;
            {
                let mut wrapped_emit =
                    |c: &mut dyn NativeContext, v: Value| -> Result<PullStep, MethodCallFailed> {
                        let step = emit(c, v)?;
                        if step == PullStep::Stop {
                            emit_stopped = true;
                        }
                        Ok(step)
                    };
                drain_spliterator_inline(ctx, spl, &chain, &chain_pins, &mut wrapped_emit)?;
            }
            // The spliterator is single-pass and is now spent (exhausted or
            // short-circuited): clear the lazy slot and record an empty
            // element array so a stray second terminal call on the same
            // stream object behaves like an already-drained ordinary stream
            // (mirrors `materialize_lazy_stream`'s post-drain bookkeeping).
            let stream = ctx.read_native_pin(stream_pin, this);
            let empty = alloc_ref_array(ctx, 0);
            let stream = ctx.read_native_pin(stream_pin, this);
            ctx.set_field(stream, STREAM_FIELD_ELEMENTS, Value::Object(Some(empty)));
            ctx.set_field(stream, STREAM_FIELD_LAZY_SPLITERATOR, Value::Object(None));
            return Ok(if emit_stopped {
                PullStep::Stop
            } else {
                PullStep::Continue
            });
        }

        // THROWS5-3: a lazy walk's frames are built as the pull reaches them
        // (`lazy_len`), so a short-circuiting chain builds only what it reads.
        let lazy_len = lazy_walk_len(&*ctx, stream);
        let base = match lazy_len {
            Some(_) => Vec::new(),
            None => stream_source_elems(ctx, stream)?,
        };
        let (_, base_pins) = pin_value_slice(ctx, &base);
        // GC-SAFETY (register-invisible-root closeable gap -- see
        // `NativeContext::refresh_root_snapshot`'s doc comment): same
        // reasoning as `native_stream_for_each`'s materialized-`elements`
        // pins. `emit`/`stream_process_chain` can re-enter Java through an
        // arbitrarily long, JIT-tiering chain (a `flatMap` stage's inner
        // stream is itself pulled through here), so this batch of pins needs
        // the same up-front-plus-per-iteration deposit refresh.
        ctx.refresh_root_snapshot();
        let mut state = stream_new_pull_state(chain.len());
        for idx in 0..lazy_len.unwrap_or(base.len()) {
            if stream_limit_saturated(&chain, &state, 0) {
                return Ok(PullStep::Continue);
            }
            // Periodic, not per-element — see `stream_refresh_interval`.
            if stream_should_refresh(idx) {
                ctx.refresh_root_snapshot();
            }
            let v = match lazy_len {
                // The walk's array is reached through the pinned stream; the
                // element is pinned by `stream_process_chain` before anything
                // allocates.
                Some(_) => {
                    let stream = ctx.read_native_pin(stream_pin, this);
                    lazy_walk_element(ctx, stream, idx)?
                }
                None => read_pinned_elem(ctx, base_pins[idx], base[idx]),
            };
            let mut emit_stopped = false;
            let step = {
                let mut wrapped_emit =
                    |c: &mut dyn NativeContext, v: Value| -> Result<PullStep, MethodCallFailed> {
                        let step = emit(c, v)?;
                        if step == PullStep::Stop {
                            emit_stopped = true;
                        }
                        Ok(step)
                    };
                stream_process_chain(
                    ctx,
                    v,
                    &chain,
                    &chain_pins,
                    0,
                    &mut state,
                    &mut wrapped_emit,
                )?
            };
            if step == PullStep::Stop {
                return Ok(if emit_stopped {
                    PullStep::Stop
                } else {
                    PullStep::Continue
                });
            }
        }
        Ok(PullStep::Continue)
    })();

    ctx.unpin_native_roots(stream_pin);
    result
}

pub(crate) fn stream_pull_iterator_downstream(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
    downstream_chain: &[LazyOp],
    downstream_chain_pins: &[usize],
    downstream_start: usize,
    downstream_state: &mut StreamPullState,
    emit: &mut StreamEmit<'_>,
) -> Result<PullStep, MethodCallFailed> {
    let iterator = match ctx.invoke_virtual(stream, "iterator", "()Ljava/util/Iterator;", &[])? {
        Some(Value::Object(Some(i))) => i,
        _ => return Ok(PullStep::Continue),
    };
    let iter_pin = ctx.pin_native_root(iterator);
    loop {
        if stream_limit_saturated(downstream_chain, downstream_state, downstream_start) {
            ctx.unpin_native_roots(iter_pin);
            return Ok(PullStep::Stop);
        }
        let iterator = ctx.read_native_pin(iter_pin, iterator);
        let has_next = ctx.invoke_virtual(iterator, "hasNext", "()Z", &[])?;
        if !matches!(has_next, Some(Value::Int(x)) if x != 0) {
            ctx.unpin_native_roots(iter_pin);
            return Ok(PullStep::Continue);
        }
        let iterator = ctx.read_native_pin(iter_pin, iterator);
        let next = ctx
            .invoke_virtual(iterator, "next", "()Ljava/lang/Object;", &[])?
            .unwrap_or(Value::Object(None));
        let step = stream_process_chain(
            ctx,
            next,
            downstream_chain,
            downstream_chain_pins,
            downstream_start,
            downstream_state,
            emit,
        )?;
        if step == PullStep::Stop {
            ctx.unpin_native_roots(iter_pin);
            return Ok(PullStep::Stop);
        }
    }
}

pub(crate) fn stream_pull_synthetic_downstream(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
    downstream_chain: &[LazyOp],
    downstream_chain_pins: &[usize],
    downstream_start: usize,
    downstream_state: &mut StreamPullState,
    emit: &mut StreamEmit<'_>,
) -> Result<PullStep, MethodCallFailed> {
    let stream_pin = ctx.pin_native_root(stream);
    let stream_cur = ctx.read_native_pin(stream_pin, stream);
    let chain = stream_read_chain(ctx, stream_cur);
    let chain_pins = pin_lazy_chain_lambdas(ctx, &chain);

    let result: Result<PullStep, MethodCallFailed> = (|| {
        let stream_cur = ctx.read_native_pin(stream_pin, stream);
        // FIX (stream-eager-drain-20260715): a `flatMap` mapper commonly
        // returns a fresh `StreamSupport.stream(sp, false)` -- drive it
        // inline through its own chain too, same as `stream_pull_internal`,
        // instead of materializing it raw first.
        if let Some(spl) = stream_lazy_spliterator(ctx, stream_cur) {
            let mut downstream_stopped = false;
            {
                let mut downstream_emit =
                    |c: &mut dyn NativeContext, v: Value| -> Result<PullStep, MethodCallFailed> {
                        let step = stream_process_chain(
                            c,
                            v,
                            downstream_chain,
                            downstream_chain_pins,
                            downstream_start,
                            downstream_state,
                            emit,
                        )?;
                        if step == PullStep::Stop {
                            downstream_stopped = true;
                        }
                        Ok(step)
                    };
                drain_spliterator_inline(ctx, spl, &chain, &chain_pins, &mut downstream_emit)?;
            }
            let stream_cur = ctx.read_native_pin(stream_pin, stream);
            let empty = alloc_ref_array(ctx, 0);
            let stream_cur = ctx.read_native_pin(stream_pin, stream);
            ctx.set_field(
                stream_cur,
                STREAM_FIELD_ELEMENTS,
                Value::Object(Some(empty)),
            );
            ctx.set_field(
                stream_cur,
                STREAM_FIELD_LAZY_SPLITERATOR,
                Value::Object(None),
            );
            return Ok(if downstream_stopped {
                PullStep::Stop
            } else {
                PullStep::Continue
            });
        }

        let base = stream_source_elems(ctx, stream_cur)?;
        let (_, base_pins) = pin_value_slice(ctx, &base);
        let mut state = stream_new_pull_state(chain.len());
        for (idx, v) in base.iter().copied().enumerate() {
            if stream_limit_saturated(downstream_chain, downstream_state, downstream_start) {
                return Ok(PullStep::Stop);
            }
            if stream_limit_saturated(&chain, &state, 0) {
                return Ok(PullStep::Continue);
            }
            let v = read_pinned_elem(ctx, base_pins[idx], v);
            let mut downstream_stopped = false;
            let step = {
                let mut downstream_emit =
                    |c: &mut dyn NativeContext, v: Value| -> Result<PullStep, MethodCallFailed> {
                        let step = stream_process_chain(
                            c,
                            v,
                            downstream_chain,
                            downstream_chain_pins,
                            downstream_start,
                            downstream_state,
                            emit,
                        )?;
                        if step == PullStep::Stop {
                            downstream_stopped = true;
                        }
                        Ok(step)
                    };
                stream_process_chain(
                    ctx,
                    v,
                    &chain,
                    &chain_pins,
                    0,
                    &mut state,
                    &mut downstream_emit,
                )?
            };
            if step == PullStep::Stop {
                return Ok(if downstream_stopped {
                    PullStep::Stop
                } else {
                    PullStep::Continue
                });
            }
        }
        Ok(PullStep::Continue)
    })();

    ctx.unpin_native_roots(stream_pin);
    result
}

pub(crate) fn stream_pull_any_downstream(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
    downstream_chain: &[LazyOp],
    downstream_chain_pins: &[usize],
    downstream_start: usize,
    downstream_state: &mut StreamPullState,
    emit: &mut StreamEmit<'_>,
) -> Result<PullStep, MethodCallFailed> {
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(stream))
        .unwrap_or_default();
    if class_name == "org/keycloak/utils/ClosingStream" {
        if let Value::Object(Some(delegate)) = ctx.get_field_by_name(stream, "delegate") {
            if delegate != stream {
                return stream_pull_any_downstream(
                    ctx,
                    delegate,
                    downstream_chain,
                    downstream_chain_pins,
                    downstream_start,
                    downstream_state,
                    emit,
                );
            }
        }
    }
    if is_synthetic_stream(&class_name) {
        stream_pull_synthetic_downstream(
            ctx,
            stream,
            downstream_chain,
            downstream_chain_pins,
            downstream_start,
            downstream_state,
            emit,
        )
    } else {
        stream_pull_iterator_downstream(
            ctx,
            stream,
            downstream_chain,
            downstream_chain_pins,
            downstream_start,
            downstream_state,
            emit,
        )
    }
}

/// Drive `this`'s SOURCE elements through its deferred op-chain, invoking `emit`
/// for each element that survives the whole chain. `emit` returns `PullStep::Stop`
/// to short-circuit. Stateful ops (limit/skip) keep per-op counters. Lambda
/// exceptions propagate via `?`.
pub(crate) fn stream_pull<F>(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    mut emit: F,
) -> Result<(), MethodCallFailed>
where
    F: FnMut(&mut dyn NativeContext, Value) -> Result<PullStep, MethodCallFailed>,
{
    let _ = stream_pull_internal(ctx, this, &mut emit)?;
    Ok(())
}

/// [`stream_pull`] for a short-circuit terminal (`findFirst`, `anyMatch`,
/// `allMatch`, `noneMatch`) on a lazy stage: link or consume the stage first,
/// as every other terminal does through `stream_elements`. Interpreter round
/// i1 wave 50, lane L3: these four reached the chain without the funnel, so
/// under `--compatible` (where the lazy pipeline is the default) a second
/// terminal on the same stage answered from a drained source (`Optional.empty`,
/// `0`) instead of HotSpot's `IllegalStateException`, and a stage derived
/// inside a `StackWalker.walk` function stayed usable after the walk
/// (`L3W50LazyDerivedWalkStream.java`, rows
/// `limit-escaped`, `two-lazy-escaped`; `L3W50LazyStreamReuse`).
pub(crate) fn stream_pull_terminal<F>(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    emit: F,
) -> Result<(), MethodCallFailed>
where
    F: FnMut(&mut dyn NativeContext, Value) -> Result<PullStep, MethodCallFailed>,
{
    if stream_is_reference_carrier(&*ctx, this) {
        stream_link_or_consume(ctx, this)?;
    }
    stream_pull(ctx, this, emit)
}

// ===========================================================================
// FIX (stream-eager-drain-20260715): inline per-element drive of a LAZY
// spliterator-sourced stream through its downstream op-chain.
//
// The Part-B chain machinery above (`stream_process_chain` et al.) already
// threads one element through map/filter/peek/limit/skip/flatMap in a single
// interleaved step -- but only once it HAS an element in hand. Every call
// site used to obtain that element via `stream_source_elems` /
// `materialize_lazy_stream`, which for a lazy (`StreamSupport.stream(sp,
// false)`) source fully drains the RAW spliterator with a dumb
// append-only collector BEFORE the chain ever runs on ANY element. That is
// invisible for the common case (`next()` returns an independent fresh
// value each call) but breaks the "cursor" idiom -- `next()` returns `this`;
// the real per-element consumption happens inside a downstream op
// (`org.openqa.selenium.json.JsonInputIterator`/`MapCoercer` being the
// motivating real-world case) -- because `hasNext()` can never observe a
// state change during that disconnected raw drain, so it never returns
// `false`, and the drain runs until `drain_spliterator_to_array`'s
// 1,000,000-iteration safety cap. See
// stream-eager-drain-inline-fix-20260715-FIXED.md.
//
// `drain_spliterator_inline` below fixes this by driving `tryAdvance` itself
// and running EACH element through the chain from inside the reentrant
// `Consumer.accept` callback, before asking for the next one.
// ===========================================================================

/// One in-flight "drive a lazy spliterator inline through its downstream
/// op-chain" job (`drain_spliterator_inline`). Pushed onto
/// `SPLITERATOR_PULL_STACK` for the duration of the `tryAdvance` loop that
/// owns it (via `PullFrameGuard`), popped when that loop ends (normal
/// exhaustion, short-circuit stop, or a propagated lambda error). The
/// `accept(Object)V` native on `cratonvm/internal/StreamChainCollector`
/// (registered below) reads the TOP frame on each reentrant call made from
/// `tryAdvance`.
///
/// `chain`/`chain_pins` are cloned in (cheap: a handful of ops at most) so
/// `accept` never needs to borrow back into the driving function's stack
/// frame for them. `state` is genuinely mutated across calls (limit/skip
/// counters) and lives here for exactly that reason.
pub(crate) struct SpliteratorPullFrame {
    pub(in crate::streams) chain: Vec<LazyOp>,
    pub(in crate::streams) chain_pins: Vec<usize>,
    pub(in crate::streams) state: StreamPullState,
    // SAFETY: `emit` points at a `&mut StreamEmit` owned by
    // `drain_spliterator_inline`'s caller and borrowed for the exact
    // duration of that call. The frame holding this pointer is pushed
    // immediately before, and popped (via `PullFrameGuard`'s `Drop`,
    // unconditionally, including on early return via `?`) immediately
    // after, the `tryAdvance` loop that is the ONLY code that can trigger a
    // reentrant `accept()` call observing this frame. No other code path
    // can extend this pointer's effective lifetime past that window. This
    // mirrors the established `ChmMonitorGuard` raw-pointer-RAII-guard
    // pattern already used elsewhere in this file for the same
    // "&mut dyn NativeContext-shaped borrow must outlive a guard, but is
    // provably scope-bounded" situation.
    pub(in crate::streams) emit: *mut dyn FnMut(&mut dyn NativeContext, Value) -> Result<PullStep, MethodCallFailed>,
    pub(in crate::streams) stop: bool,
    pub(in crate::streams) error: Option<MethodCallFailed>,
}

thread_local! {
    /// Stack of in-flight `drain_spliterator_inline` jobs on this thread.
    /// A `Vec`, not a single slot, because a downstream lambda invoked from
    /// `stream_process_chain` (e.g. a `.map()` function, or a nested
    /// `.flatMap()`) can itself start ANOTHER inline spliterator drive
    /// before the outer one finishes -- always exactly nested (push/pop
    /// discipline), never interleaved, since everything here is synchronous
    /// single-threaded JVM native execution.
    pub(in crate::streams) static SPLITERATOR_PULL_STACK: std::cell::RefCell<Vec<SpliteratorPullFrame>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// RAII guard: pushes a frame on construction, pops it (from the TOP of the
/// stack -- always correct given the strict nesting discipline above) on
/// `Drop`, so an early return (including via `?` on a propagated lambda
/// error) can never leave a stale frame behind for a LATER, unrelated
/// `accept()` call to misread.
pub(crate) struct PullFrameGuard;

impl PullFrameGuard {
    pub(in crate::streams) fn push(frame: SpliteratorPullFrame) -> Self {
        SPLITERATOR_PULL_STACK.with(|s| s.borrow_mut().push(frame));
        PullFrameGuard
    }

    /// Peek this job's `stop` flag and take its `error` (if any) after one
    /// `accept()` call. `error` is taken (moved out) since it can only be
    /// reported once.
    pub(in crate::streams) fn poll(&self) -> (bool, Option<MethodCallFailed>) {
        SPLITERATOR_PULL_STACK.with(|s| {
            let mut s = s.borrow_mut();
            let frame = s.last_mut().expect("PullFrameGuard::poll: frame missing");
            (frame.stop, frame.error.take())
        })
    }
}

impl Drop for PullFrameGuard {
    fn drop(&mut self) {
        SPLITERATOR_PULL_STACK.with(|s| {
            s.borrow_mut().pop();
        });
    }
}

pub(crate) const STREAM_CHAIN_COLLECTOR_CLASS: &str = "cratonvm/internal/StreamChainCollector";

/// `accept(Object)V` native for `cratonvm/internal/StreamChainCollector`.
/// Reentrant callback target for the `tryAdvance` loop in
/// `drain_spliterator_inline`: runs the ONE element it's just been handed
/// straight through the top `SPLITERATOR_PULL_STACK` frame's `chain` (and,
/// through that, the caller's ultimate `emit`) before returning control to
/// `tryAdvance` -- this per-`tryAdvance`-call interleaving is what makes the
/// drive genuinely pull-based rather than a raw disconnected drain.
pub(crate) fn native_stream_chain_collector_accept(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));

    // Snapshot what's needed to run the chain, then release the RefCell
    // borrow BEFORE calling `stream_process_chain` -- that call can re-enter
    // Java (a map/filter/flatMap lambda) which may itself start a NESTED
    // `drain_spliterator_inline` job that pushes onto this same thread-local
    // stack; holding the borrow across the call would panic on that nested
    // `borrow_mut()`.
    let (chain, chain_pins, mut state, emit_ptr, skip) = SPLITERATOR_PULL_STACK.with(|s| {
        let mut s = s.borrow_mut();
        let frame = s
            .last_mut()
            .expect("StreamChainCollector.accept called with no active pull frame");
        let skip = frame.stop || frame.error.is_some();
        (
            frame.chain.clone(),
            frame.chain_pins.clone(),
            std::mem::replace(&mut frame.state, stream_new_pull_state(0)),
            frame.emit,
            skip,
        )
    });

    if skip {
        return Ok(None);
    }

    // SAFETY: see `SpliteratorPullFrame::emit`'s doc comment above.
    let emit: &mut dyn FnMut(&mut dyn NativeContext, Value) -> Result<PullStep, MethodCallFailed> =
        unsafe { &mut *emit_ptr };
    let outcome = stream_process_chain(ctx, elem, &chain, &chain_pins, 0, &mut state, emit);

    SPLITERATOR_PULL_STACK.with(|s| {
        let mut s = s.borrow_mut();
        let frame = s
            .last_mut()
            .expect("StreamChainCollector.accept: frame vanished mid-call");
        frame.state = state;
        match outcome {
            Ok(PullStep::Stop) => frame.stop = true,
            Ok(PullStep::Continue) => {}
            Err(e) => frame.error = Some(e),
        }
    });
    Ok(None)
}

/// Drain a LAZY spliterator-sourced stream's elements INLINE through its
/// downstream op-chain: each `tryAdvance` hands its one element straight to
/// `native_stream_chain_collector_accept`, which runs it through `chain` (and
/// ultimately `emit`) before the loop asks the spliterator for the next
/// element -- HotSpot's pull-based per-element interleaving, and the fix for
/// the "cursor" Iterator idiom described in the module doc comment above.
///
/// Stops calling `tryAdvance` the moment the chain is satisfied (a `limit`,
/// or `emit` returning `PullStep::Stop`), so a short-circuiting terminal over
/// a lazy source genuinely short-circuits instead of first draining
/// everything. Returns the raw `PullStep` of the last element processed
/// (`Stop` if the loop ended via the chain/emit; `Continue` if the
/// spliterator was simply exhausted) -- callers translate that into their
/// own "did the ultimate sink actually ask to stop" result exactly as the
/// array-based `stream_process_chain` loops already do (via their own
/// `emit`/`downstream_emit` wrapper's captured flag), so this is a drop-in
/// replacement for "get `base` then loop `stream_process_chain` over it".
/// The `--real-jdk` road for [`drain_spliterator_inline`]: same per-element
/// interleaving, no fabricated `Consumer`.
///
/// `cratonvm/internal/StreamChainCollector` is a name no JDK image declares, so
/// strict mode refuses to fabricate it and the `?` in
/// [`drain_spliterator_inline`] used to be the last unguarded mint of the family
/// W2-1-strict-refuses-the-synthetic-stream-stack.md was opened for. The
/// substitution is the one `drain_spliterator_via_real_iterator` already proved:
/// `java.util.Spliterators.iterator(Spliterator)` hands back a real
/// `java.util.Spliterators$1Adapter` that is itself both the `Iterator` and the
/// `Consumer`, so the `tryAdvance` drive runs from real bytecode and no
/// `Consumer` of ours is needed.
///
/// What is preserved, and it is the whole point of the inline drain: ONE element
/// is pulled, run through `chain` (and `emit`) to completion, and only then is
/// the next one asked for. A `limit` or an `emit` returning
/// [`PullStep::Stop`] therefore still short-circuits before the source is
/// exhausted — `stream_limit_saturated` is consulted at the top of every
/// iteration exactly as the collector-driven loop consults `guard.poll()`.
///
/// What differs, stated rather than hidden: `Spliterators$1Adapter` buffers one
/// element between `hasNext()` and `next()`, so a source whose `tryAdvance`
/// observes downstream side effects sees them one element later than the
/// collector path does. That is the "cursor" idiom the module comment above
/// describes, and it is why this is the FALLBACK and not the default.
pub(crate) fn drain_spliterator_inline_via_real_iterator(
    ctx: &mut dyn NativeContext,
    spl: ObjectRef,
    chain: &[LazyOp],
    chain_pins: &[usize],
    emit: &mut StreamEmit<'_>,
) -> Result<PullStep, MethodCallFailed> {
    let spl_pin = ctx.pin_native_root(spl);
    let spl_cur = ctx.read_native_pin(spl_pin, spl);
    let adapter = ctx.invoke(
        "java/util/Spliterators",
        "iterator",
        "(Ljava/util/Spliterator;)Ljava/util/Iterator;",
        &[Value::Object(Some(spl_cur))],
    );
    ctx.unpin_native_roots(spl_pin);
    let it = match adapter? {
        Some(Value::Object(Some(it))) => it,
        _ => return Ok(PullStep::Continue),
    };
    let it_pin = ctx.pin_native_root(it);
    let mut state = stream_new_pull_state(chain.len());
    const SAFETY_CAP: usize = 1_000_000;
    let mut n = 0usize;
    let outcome: Result<PullStep, MethodCallFailed> = loop {
        if n >= SAFETY_CAP {
            break Ok(PullStep::Continue);
        }
        if stream_limit_saturated(chain, &state, 0) {
            break Ok(PullStep::Stop);
        }
        let it_cur = ctx.read_native_pin(it_pin, it);
        let has_next = match ctx.invoke_virtual(it_cur, "hasNext", "()Z", &[]) {
            Ok(Some(Value::Int(v))) => v != 0,
            Ok(_) => false,
            Err(e) => break Err(e),
        };
        if !has_next {
            break Ok(PullStep::Continue);
        }
        let it_cur = ctx.read_native_pin(it_pin, it);
        let elem = match ctx.invoke_virtual(it_cur, "next", "()Ljava/lang/Object;", &[]) {
            Ok(Some(v)) => v,
            Ok(None) => Value::Object(None),
            Err(e) => break Err(e),
        };
        // `elem` goes straight into the chain, whose own `pin_value` roots it —
        // nothing allocates between the `next()` return and that pin.
        match stream_process_chain(ctx, elem, chain, chain_pins, 0, &mut state, emit) {
            Ok(PullStep::Stop) => break Ok(PullStep::Stop),
            Ok(PullStep::Continue) => {}
            Err(e) => break Err(e),
        }
        n += 1;
    };
    ctx.unpin_native_roots(it_pin);
    outcome
}

pub(crate) fn drain_spliterator_inline(
    ctx: &mut dyn NativeContext,
    spl: ObjectRef,
    chain: &[LazyOp],
    chain_pins: &[usize],
    emit: &mut StreamEmit<'_>,
) -> Result<PullStep, MethodCallFailed> {
    // W2-1 residual 4. Same idiom, same reason, as
    // `drain_spliterator_to_array_capped`'s `Err(_refused) =>` arm two thousand
    // lines above: a fabrication that happens to run first makes every other
    // site's refusal order-dependent rather than a policy.
    //
    // REACHABILITY, stated so the next reader does not mistake this for a fixed
    // bug: under `--real-jdk` `stream_make_lazy_derived` already returns
    // `Ok(None)` (the `cratonvm/stream/LazyOp` guard), so slot 3 is never
    // written, `stream_has_chain` is false, and every `stream_pull` entry point
    // is gated on it — so strict mode does not reach this function today. The
    // guard is here because "unreachable" is a property of five call sites in
    // two other functions, not of this one, and the previous shape failed by
    // FABRICATING under Compatible and by raising `NoClassDefFoundError` under
    // strict at whichever site got there first.
    let collector = match try_alloc_synthetic(ctx, STREAM_CHAIN_COLLECTOR_CLASS, 0) {
        Ok(c) => c,
        Err(_refused) => {
            return drain_spliterator_inline_via_real_iterator(ctx, spl, chain, chain_pins, emit)
        }
    };
    let spl_pin = ctx.pin_native_root(spl);
    let col_pin = ctx.pin_native_root(collector);

    // SAFETY: `emit`'s borrow covers this entire function body; `guard`
    // pops the frame (unconditionally, via `Drop`) before we return by any
    // path below, so nothing outside this function can ever observe the
    // pointer stashed in the frame. Coerce to a raw pointer at `emit`'s own
    // (short) lifetime, then transmute away the lifetime so it fits the
    // frame's (unbounded) field type -- mirrors `ChmMonitorGuard::acquire`'s
    // `&mut dyn NativeContext` lifetime-erasure a few hundred lines above.
    let emit_raw: *mut dyn FnMut(
        &mut dyn NativeContext,
        Value,
    ) -> Result<PullStep, MethodCallFailed> = emit;
    let emit_ptr: *mut dyn FnMut(
        &mut dyn NativeContext,
        Value,
    ) -> Result<PullStep, MethodCallFailed> = unsafe { core::mem::transmute(emit_raw) };
    let guard = PullFrameGuard::push(SpliteratorPullFrame {
        chain: chain.to_vec(),
        chain_pins: chain_pins.to_vec(),
        state: stream_new_pull_state(chain.len()),
        emit: emit_ptr,
        stop: false,
        error: None,
    });

    const SAFETY_CAP: usize = 1_000_000;
    let mut n = 0usize;
    let mut last_step = PullStep::Continue;
    let loop_result: Result<(), MethodCallFailed> = (|| {
        loop {
            let s = ctx.read_native_pin(spl_pin, spl);
            let c = ctx.read_native_pin(col_pin, collector);
            match ctx.invoke_virtual(
                s,
                "tryAdvance",
                "(Ljava/util/function/Consumer;)Z",
                &[Value::Object(Some(c))],
            ) {
                Ok(Some(Value::Int(v))) if v != 0 => {
                    let (stopped, err) = guard.poll();
                    if let Some(e) = err {
                        return Err(e);
                    }
                    if stopped {
                        last_step = PullStep::Stop;
                        break;
                    }
                    n += 1;
                    if n >= SAFETY_CAP {
                        break;
                    }
                }
                Ok(_) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    })();

    ctx.unpin_native_roots(spl_pin);
    ctx.unpin_native_roots(col_pin);
    drop(guard);
    loop_result?;
    Ok(last_step)
}

/// Fully materialise a lazy stream's op-chain (apply every op to every source
/// element). Used by `stream_elements` when a chain is present so eager terminals'
/// *results* are identical whether or not upstream ops were deferred. A lambda
/// exception propagates via `?` (matching HotSpot, which surfaces it at the
/// terminal) — `stream_elements` is `Result` precisely so this is faithful.
pub(crate) fn stream_apply_chain_full(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<Vec<Value>, MethodCallFailed> {
    // GC-SAFETY: the pull loop below re-invokes the chain's user lambdas
    // (map/flatMap/filter/peek) once per emitted element via `stream_pull` ->
    // `stream_process_chain` -> `invoke_deferred_stream_lambda`, and each
    // dispatch can allocate and trigger a moving GC. Earlier iterations'
    // results were being accumulated into `out` as bare `Value`s with no pin,
    // so a LATER element's lambda call could relocate an EARLIER element
    // already sitting in `out`, leaving it stale by the time a downstream
    // terminal (e.g. `collect(toUnmodifiableList())`'s `make_list_of`) reads
    // it back. Pin every emitted value immediately and rebuild `out` from
    // those pins once the whole pull completes. Observed as class_id 64/65
    // panics inside Xerces' `DefaultXMLSequence` ctor's `particles.stream()
    // .map(...).flatMap(...).collect(Collectors.toUnmodifiableList())`
    // pipeline.
    // cceres3 (PIN-DANGLING live capture, every boot): pinning from INSIDE
    // the pull callback is unsound — the pins land ABOVE the pull machinery's
    // own per-element bases, so its legitimate unpin-to-base truncates them
    // (read_value_slice then silently degraded to the raw, possibly-stale
    // values — the surviving to_array_gen store-canary firings). The pin
    // stack is strictly LIFO per scope; a callback must not pin into its
    // callee's scope. Root the accumulated elements in the GLOBAL root table
    // instead: persistent, GC-remapped, and independent of the pin stack.
    let mut out = Vec::new();
    let mut ghandles: Vec<usize> = Vec::new();
    let pull = stream_pull(ctx, this, |c, v| {
        ghandles.push(match v {
            Value::Object(Some(o)) => c.add_global_root(o),
            _ => usize::MAX,
        });
        out.push(v);
        Ok(PullStep::Continue)
    });
    if let Err(e) = pull {
        for &h in &ghandles {
            if h != usize::MAX {
                ctx.remove_global_root(h);
            }
        }
        return Err(e);
    }
    let healed: Vec<Value> = out
        .iter()
        .zip(&ghandles)
        .map(|(v, &h)| match (v, h) {
            (Value::Object(Some(o)), h) if h != usize::MAX => {
                Value::Object(Some(ctx.resolve_global_root(h).unwrap_or(*o)))
            }
            _ => *v,
        })
        .collect();
    for &h in &ghandles {
        if h != usize::MAX {
            ctx.remove_global_root(h);
        }
    }
    Ok(healed)
}

/// Defer an intermediate op when lazy streams are enabled and `this` is synthetic;
/// otherwise return `None` so the caller runs its eager body. Centralises the
/// gate + synthetic check for peek/map/filter/limit/skip.
pub(crate) fn stream_try_defer(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    kind: i32,
    lambda: Option<ObjectRef>,
    aux: i64,
) -> Result<Option<Value>, MethodCallFailed> {
    if lazy_streams_enabled() && stream_is_synthetic(ctx, this) {
        return Ok(stream_make_lazy_derived(ctx, this, kind, lambda, aux)?);
    }
    Ok(None)
}

/// Run (once) the close handlers registered on a synthetic stream: clear the
/// field first (run-once / re-entrancy safe), then invoke each `Runnable.run()`.
pub(crate) fn stream_run_close_handlers(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    // Round 18 wave 11: the pipeline's handlers live on its close root.
    let this = stream_close_root(&*ctx, this);
    // W1 fix: 1-field synthetic streams have no close-handler slot — nothing to
    // run. Guard mirrors the slot-2/slot-3 presence checks elsewhere.
    if ctx.object_num_fields(this) <= STREAM_FIELD_CLOSE_HANDLERS {
        return Ok(());
    }
    if let Value::Object(Some(arr)) = ctx.get_field(this, STREAM_FIELD_CLOSE_HANDLERS) {
        ctx.set_field(this, STREAM_FIELD_CLOSE_HANDLERS, Value::Object(None));
        // cceres3: pin across GC-capable call (stream stale-at-store wave) —
        // each `run()` re-enters Java; re-read `arr` through the pin per handler.
        let arr_pin = ctx.pin_native_root(arr);
        let len = ctx.array_length(arr);
        // Round 18 wave 11 (lane streams5): JDK 25 composes close handlers
        // with `Streams.composeWithExceptions`: every handler runs, the first
        // throw is rethrown and each later one is `addSuppressed` onto it
        // ([`stream_close_all_handlers_enabled`]). The first exception is
        // pinned above `arr_pin`, so the one unpin below releases both.
        let all = stream_close_all_handlers_enabled();
        let mut first: Option<(MethodCallFailed, Option<usize>)> = None;
        for i in 0..len {
            let arr = ctx.read_native_pin(arr_pin, arr);
            if let Value::Object(Some(r)) = ctx.get_array_element(arr, i) {
                if let Err(e) = ctx.invoke_virtual(r, "run", "()V", &[]) {
                    if !all {
                        ctx.unpin_native_roots(arr_pin);
                        return Err(e);
                    }
                    if first.is_none() {
                        let pin = match &e {
                            MethodCallFailed::ExceptionThrown(t) => Some(ctx.pin_native_root(*t)),
                            MethodCallFailed::InternalError(_) => None,
                        };
                        first = Some((e, pin));
                    } else if let Some((MethodCallFailed::ExceptionThrown(t1), Some(pin))) = &first
                    {
                        let (t1, pin) = (*t1, *pin);
                        if let MethodCallFailed::ExceptionThrown(t2) = e {
                            let t1 = ctx.read_native_pin(pin, t1);
                            if t1 != t2 {
                                // `try { e1.addSuppressed(e2); } catch (Throwable ignore) {}`
                                let _ = ctx.invoke_virtual(
                                    t1,
                                    "addSuppressed",
                                    "(Ljava/lang/Throwable;)V",
                                    &[Value::Object(Some(t2))],
                                );
                            }
                        }
                    }
                }
            }
        }
        let first = first.map(|(e, pin)| match (e, pin) {
            (MethodCallFailed::ExceptionThrown(t), Some(pin)) => {
                MethodCallFailed::ExceptionThrown(ctx.read_native_pin(pin, t))
            }
            (e, _) => e,
        });
        ctx.unpin_native_roots(arr_pin);
        if let Some(e) = first {
            return Err(e);
        }
    }
    Ok(())
}

/// `CRATONVM_STREAM_CLOSE_ALL_HANDLERS` -- default ON, `=0` restores the
/// stop-at-the-first-throw loop. JDK 25 `BaseStream.close()` runs every
/// registered handler even when one throws; the first exception propagates
/// with the later ones attached as suppressed
/// (`Streams.composeWithExceptions`). The synthetic close stopped at the first
/// throw, so later handlers never ran. JIT round 18 wave 11, lane streams5.
/// Read live, per call.
pub(crate) fn stream_close_all_handlers_enabled() -> bool {
    true
}

/// `BaseStream.onClose(Runnable)` for synthetic streams: append the handler to
/// field 1 and return `this`. No-op (returns `this`) for non-synthetic receivers.
pub(crate) fn native_stream_on_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(args.first().copied()),
    };
    let cn = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_default();
    if !is_synthetic_stream(&cn) {
        return Ok(Some(Value::Object(Some(this))));
    }
    // W7-65 residual 4, closed 2026-08-12. `AbstractPipeline.onClose(Runnable)`
    // (line 362 of the JDK 25 source) is the JDK's one CHECK-ONLY site: it
    // throws `IllegalStateException(MSG_STREAM_LINKED)` when the stage is
    // already linked or consumed, and then registers the handler WITHOUT
    // marking. Measured on HotSpot 25 as `reuse.onCloseAfterConsume` = throws,
    // which a reading of the source alone would not have predicted, so the
    // measurement is the authority.
    //
    // Why this cannot fire spuriously, which is the only question that mattered:
    // the read is `stream_is_linked`, whose two guards do the work. It is placed
    // AFTER the `is_synthetic_stream` gate above — deliberately, because slot 4
    // on a REAL pipeline is one of its own fields and could hold `Int(1)` for
    // reasons of its own — and `stream_is_linked` additionally requires the
    // 5-slot layout, so every short-layout stream (the whole
    // `StreamSupport.stream(realSpliterator, false)` family) is exempt. Nothing
    // else in the file writes slot 4, so the flag can only be here because
    // `stream_link_or_consume` put it there.
    //
    // No mark: the JDK does not set here, and adding one would make this the
    // second set site on a path that runs in every try-with-resources.
    if stream_is_linked(ctx, this) {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: STREAM_LINKED_MSG.to_string(),
        }
        .into());
    }
    // W1 fix: a 1-field synthetic stream (e.g. Files.lines's eager array-backed
    // stream) has no close-handler slot to attach to. Reading/writing slot 1
    // would index past the receiver; behave as a no-op (return `this`) instead.
    // CratonVM materialises such streams eagerly, so there is no live resource a
    // close handler would need to release.
    if ctx.object_num_fields(this) <= STREAM_FIELD_CLOSE_HANDLERS {
        return Ok(Some(Value::Object(Some(this))));
    }
    let handler = match args.get(1) {
        Some(Value::Object(Some(h))) => *h,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    // gc-common w20-a: the stream, the handler and the old handler array are
    // all bare locals, and the new array's allocation can move each of them;
    // all three were used through their pre-allocation addresses (the old
    // handlers copied from a stale array, the new array stored into a stale
    // stream). All three are pinned across the allocation and read back.
    // Round 18 wave 11: append to the pipeline's close root
    // ([`stream_close_root`]); `this` itself when nothing links it.
    let root = stream_close_root(&*ctx, this);
    let this_pin = ctx.pin_native_root(this);
    let handler_pin = ctx.pin_native_root(handler);
    let root_pin = ctx.pin_native_root(root);
    let old = match ctx.get_field(root, STREAM_FIELD_CLOSE_HANDLERS) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    };
    let n = old.map_or(0, |o| ctx.array_length(o));
    let old_pin = old.map(|o| ctx.pin_native_root(o));
    let appended: ObjectRef = alloc_ref_array(ctx, n + 1);
    if let (Some(o), Some(h)) = (old, old_pin) {
        let old_now = ctx.read_native_pin(h, o);
        for i in 0..n {
            let v = ctx.get_array_element(old_now, i);
            ctx.set_array_element(appended, i, v);
        }
    }
    let handler = ctx.read_native_pin(handler_pin, handler);
    ctx.set_array_element(appended, n, Value::Object(Some(handler)));
    let this = ctx.read_native_pin(this_pin, this);
    let root = ctx.read_native_pin(root_pin, root);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field(
        root,
        STREAM_FIELD_CLOSE_HANDLERS,
        Value::Object(Some(appended)),
    );
    Ok(Some(Value::Object(Some(this))))
}

/// `BaseStream.close()` for synthetic streams: mark the stage linked-or-consumed,
/// then run the registered close handlers.
///
/// W7-65 residual 3, closed 2026-08-12. `AbstractPipeline.close()` is the JDK's
/// site 7 and the only one that sets **unconditionally, with no preceding
/// check** — which is why `close(); close()` and `count(); close()` both return
/// normally on HotSpot 25 (`reuse.closeTwice`, `reuse.consumeThenClose`) while
/// `close(); count()` throws (`reuse.closeThenCount`). `stream_mark_linked` is
/// the unconditional setter, so all three fall out of one line; using
/// `stream_link_or_consume` here would have made `close()` itself throw and
/// broken every try-with-resources.
///
/// The reason this was left open in W7-65 was "a VM-internal path that closes a
/// synthetic stream earlier than HotSpot would". Censused since, from source:
/// the only `invoke_virtual(_, "close", "()V", _)` calls in this file are the
/// two `flatMap` inner-stream closes — the lazy chain's
/// (`stream_process_chain`) and the eager body's (`native_stream_flat_map`) —
/// and both run AFTER that inner stream's elements have been pulled, which is
/// exactly what the JDK's own `try (Stream<R> result = mapper.apply(u))` does.
/// Neither inner stream is touched again, so the mark ends nothing. No native
/// anywhere else in the workspace closes a `java.util.stream.Stream` it did not
/// just mint and drain.
pub(crate) fn native_stream_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        let cn = ctx
            .class_name_of_id(ctx.class_id_of_object(*this))
            .unwrap_or_default();
        if is_synthetic_stream(&cn) {
            // BEFORE the handlers, matching `AbstractPipeline.close()`, whose
            // first statement is `linkedOrConsumed = true` and whose
            // `closeAction.run()` is last. The order is observable in two ways:
            // a handler that operates on the stream must see it spent (HotSpot
            // throws there too), and a handler that THROWS must still leave the
            // stage marked. Running the handlers first would have got both
            // backwards.
            stream_mark_linked(ctx, *this);
            stream_run_close_handlers(ctx, *this)?;
        }
    }
    Ok(None)
}

