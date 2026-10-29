// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP1.9 — `java.lang.StackStreamFactory.AbstractStackWalker` native
//! surface.
//!
//! The JDK `StackWalker.walk` implementation delegates through a package
//! -private `StackStreamFactory` pipeline whose entry points are two
//! private natives on `AbstractStackWalker`:
//!
//! * `callStackWalk(long mode, int skip, int batch, int startIndex,
//!                  Object[] frameBuffer, Class<?>[] classBuffer)`
//!   — called once at the start of a walk; supposed to run the user
//!   function against a Stream/iterator built over the frame buffer.
//! * `fetchStackFrames(long mode, long anchor, int batchSize,
//!                    int startIndex, Object[] frameBuffer)`
//!   — called when the lazy stream needs more frames.
//!
//! Our VM already registers the user-facing `StackWalker.walk` /
//! `forEach` / `getInstance` / `getCallerClass` in
//! `phases_late::register_p59_stackwalker` and
//! `stack_walker::register_stack_walker_boot`. Those paths do not route
//! through `AbstractStackWalker`, so `callStackWalk` /
//! `fetchStackFrames` are only reached when real-JDK bytecode calls
//! `StackWalker.walk` and the real-JDK class is loaded (synthetic class
//! registration preempts this in our default configuration).
//!
//! When real-JDK is loaded we must still provide non-panicking
//! implementations of these natives so any application that reflects /
//! subclasses `AbstractStackWalker` (rare but legal) doesn't trap. The
//! implementations below populate `frameBuffer` with `StackFrameInfo`
//! synthetics whose layout mirrors our `StackWalker$StackFrame` (6
//! fields) and return a sentinel anchor / count compatible with the
//! real `AbstractStackWalker.Decoder` ring-buffer protocol.

use std::rc::Rc;

use cratonvm_native_api::{
    NativeContext, NativeHandleScope, NativeKind, NativeMethodRegistry, StackTraceEntry,
};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::Value;

use crate::try_alloc_concurrent_synthetic;

thread_local! {
    /// Stack of the *clean*, ordered frame lists captured by each in-progress
    /// `callStackWalk` on this thread (one entry per active walk; walks can
    /// nest when frame resolution re-enters `StackWalker.walk`).
    ///
    /// `fetchStackFrames` MUST index into this cached list rather than
    /// re-capturing the live stack: by the time the JDK's lazy stream asks for
    /// the next batch, the thread is paused *deeper* inside
    /// `doStackWalk → consumeFrames → <stream pipeline> → lambda`, so a fresh
    /// `capture_stack_trace` returns a stack polluted with
    /// `java.util.stream.*` and `StackStreamFactory$*` frames that
    /// `ordered_stack_walk_frames` does not fully strip. Indexing the
    /// re-captured (polluted) list with the `callStackWalk`-relative cursor
    /// feeds the user function walker-internal frames. For log4j2's
    /// `getCallerClass` (which keys its `LoggerContext` cache on the resolved
    /// caller class) a wrong/internal caller means the cache never hits, so
    /// each log call re-creates a context and re-logs → unbounded
    /// context-creation recursion → native-stack/value-stack corruption
    /// (Hibernate `ByteArrayMappingTests` SIGSEGV). Caching the clean list
    /// keeps every batch the user sees identical to HotSpot's.
    static SW_FRAME_CACHE: std::cell::RefCell<Vec<Rc<Vec<StackTraceEntry>>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Decode a `long` JVM argument that may reach natives as `Value::Long` or,
/// due to interpreter tagging quirks, as `Value::Double` holding the same
/// 8-byte pattern (e.g. small integer anchors appear as denormal `f64`
/// values). Treating only `Long` left `anchor=0` on every
/// `fetchStackFrames` call so `StackWalker` streams never advanced past the
/// first batch (Spring Boot `deduceMainApplicationClass` → null main class).
fn value_as_i64_bits(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Long(l)) => *l,
        Some(Value::Int(i)) => *i as i64,
        Some(Value::Double(d)) => i64::from_ne_bytes(d.to_ne_bytes()),
        _ => 0,
    }
}

/// StackFrameInfo synthetic layout, mirroring the JDK private class
/// (`jdk.internal.vm.StackFrameInfo` / `java.lang.StackFrameInfo`).
/// The 6 slots match `StackWalker$StackFrame` so the same accessors
/// work on both types.
const STACK_FRAME_INFO_FIELDS: usize = 6;

const SF_CLASSNAME: usize = 0;
const SF_METHODNAME: usize = 1;
const SF_FILENAME: usize = 2;
const SF_LINENUMBER: usize = 3;
const SF_BCI: usize = 4;
const SF_DECL_INTERNAL: usize = 5;

/// `java.lang.ClassFrameInfo.RETAIN_CLASS_REF` — the bit of the frame's
/// `flags` field that records whether the `StackWalker` that produced the
/// frame was created with `Option.RETAIN_CLASS_REFERENCE`.
///
/// The real `ClassFrameInfo(StackWalker)` constructor copies it out of the
/// walker; `populate_sfi` does the same via [`walker_retains_class_ref`].
/// JDK 25's `ClassFrameInfo.RETAIN_CLASS_REF_BIT` is `1 << 27`. The low
/// 24 bits are reserved for the member-info flags, including
/// `Modifier.NATIVE` (`0x100`).
pub(crate) const SF_FLAG_RETAIN_CLASS_REF: i32 = 0x0800_0000;

/// `java.lang.reflect.Modifier.NATIVE`, the bit of `ClassFrameInfo.flags`
/// `StackFrameInfo.isNativeMethod()` reads.
const MODIFIER_NATIVE: i32 = 0x100;

/// A native frame's line in a `StackTraceEntry` (and HotSpot's).
const LINE_NUMBER_NATIVE: i32 = -2;

/// The bits of `ClassFrameInfo.flags` above `MEMBER_INFO_FLAGS`
/// (`0x00FF_FFFF`) other than [`SF_FLAG_RETAIN_CLASS_REF`]: no Java reads
/// them (`isHidden` / `isCallerSensitive` mask the low 24 bits, the retain
/// test reads bit 27). Interpreter round i2 wave 54, lane L3: a lazy
/// element's walk keeps its class-redefinition count there, modulo 128 --
/// bits 24..26 the low three, bits 28..31 the high four.
const SF_WALK_COUNT_LOW: u32 = 0x0700_0000;
const SF_WALK_COUNT_HIGH: u32 = 0xF000_0000;
/// The count's modulus.
const SF_WALK_COUNT_MODULUS: u64 = 128;

/// `flags` with the walk's class-redefinition `count` (modulo 128) in its
/// free bits ([`SF_WALK_COUNT_LOW`], [`SF_WALK_COUNT_HIGH`]).
fn sfi_flags_with_walk_count(flags: i32, count: u64) -> i32 {
    // Cast: the count modulo 128 fits in seven bits.
    let c = (count % SF_WALK_COUNT_MODULUS) as u32;
    let bits = ((c & 0x7) << 24) | ((c >> 3) << 28);
    // Cast: a bit pattern, reinterpreted.
    ((flags as u32 & !(SF_WALK_COUNT_LOW | SF_WALK_COUNT_HIGH)) | bits) as i32
}

/// The walk's class-redefinition count modulo 128, from `flags`
/// ([`sfi_flags_with_walk_count`]).
fn sfi_walk_count_of(flags: i32) -> u64 {
    // Cast: a bit pattern, reinterpreted.
    let f = flags as u32;
    u64::from(((f & SF_WALK_COUNT_LOW) >> 24) | (((f & SF_WALK_COUNT_HIGH) >> 28) << 3))
}

/// Was `class_id` redefined between the walk whose count modulo 128 is
/// `walked_mod` and `now`? The walk's count is taken as the latest one not
/// above `now` with that residue: a walk 128 or more redefinitions back is
/// taken as later than it was, which can only miss a redefinition (the
/// frame keeps its line, as when the ring cannot answer), never invent one.
fn sfi_class_redefined_since(
    ctx: &dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    walked_mod: u64,
    now: u64,
) -> bool {
    let span =
        (now % SF_WALK_COUNT_MODULUS + SF_WALK_COUNT_MODULUS - walked_mod) % SF_WALK_COUNT_MODULUS;
    if span == 0 {
        return false;
    }
    let Some(walked_at) = now.checked_sub(span) else {
        return false;
    };
    let mut redefined = false;
    let answered = vm_redefined_classes_between(ctx, walked_at, now, |c| {
        redefined |= c == class_id;
    });
    answered && redefined
}

/// The classes redefined in `(after, upto]` (process redefinition clock) that
/// a walked frame's line and file answer to: THIS VM's own redefinition log
/// when the context has one (`NativeClassAccess::redefined_classes_between`),
/// the process ring otherwise. `false`: the record cannot answer (the readers
/// then keep the frame's line). nio21 wave 6 L1
/// (`r21-stackwalker-frame-lines-blank-on-another-vms-redefinition-FIXED-20261012.md`):
/// the process ring names another VM's class under the same `ClassId`, and a
/// frame of an unredefined class lost its line.
pub(crate) fn vm_redefined_classes_between(
    ctx: &dyn NativeContext,
    after: u64,
    upto: u64,
    mut visit: impl FnMut(cratonvm_types::ClassId),
) -> bool {
    match ctx.redefined_classes_between(after, upto, &mut visit) {
        Some(answered) => answered,
        None => cratonvm_classloading::for_each_redefined_class_between(after, upto, visit),
    }
}

/// The class of a real `StackFrameInfo` `sfi`: its `classOrMemberName`'s
/// mirror (a `Class`, or a `ResolvedMethodName`'s `vmholder`).
fn sfi_class_id(
    ctx: &mut dyn NativeContext,
    sfi: cratonvm_types::ObjectRef,
) -> Option<cratonvm_types::ClassId> {
    let Value::Object(Some(held)) = ctx.get_field_by_name(sfi, "classOrMemberName") else {
        return None;
    };
    let mirror = if is_class_mirror(ctx, held) {
        held
    } else {
        match ctx.get_field_by_name(held, "vmholder") {
            Value::Object(Some(m)) => m,
            _ => return None,
        }
    };
    crate::lang_class::mirror_class_id(&*ctx, mirror)
}

/// The '/'-form internal name of a `StackFrameInfo`'s class: the
/// `SF_DECL_INTERNAL` slot's String, or, when the slot carries a lazy
/// element's walk (wave 54), the frame's class's name.
fn sfi_decl_internal(ctx: &mut dyn NativeContext, sfi: cratonvm_types::ObjectRef) -> String {
    if let Value::Object(Some(s)) = ctx.get_field(sfi, SF_DECL_INTERNAL) {
        if let Some(name) = ctx.read_string(s) {
            return name;
        }
    }
    sfi_class_id(ctx, sfi)
        .and_then(|cid| ctx.class_name_of_id(cid))
        .unwrap_or_default()
}

/// What a lazy `StackFrameInfo` element is built from at its first read
/// (interpreter round i2 wave 54, lane L3): the element the walk built
/// (`populate_sfi`, kept in the `SF_DECL_INTERNAL` slot), settled as
/// HotSpot's `java_lang_StackTraceElement::fill_in` settles it -- no line and
/// no file when the frame's class was redefined since the walk (a native
/// frame keeps its `-2`). `None` for a frame that is not lazy.
struct SfiWalkElement {
    class_id: Option<cratonvm_types::ClassId>,
    class_dotted: String,
    method: String,
    file: Option<String>,
    line: i32,
    /// Interpreter round i4 wave 91, lane L2
    /// (`StackTraceFillSwitches::walk_element_interned_names`, default off;
    /// walk-lines frames only): the class's cached dotted name, kept instead
    /// of copying it into `class_dotted` (then empty).
    class_dotted_arc: Option<std::sync::Arc<str>>,
    /// Same switch: the frame's `name` field, the String the walk interned
    /// (`populate_sfi_from_walk_lines`), stored as the element's
    /// `methodName` instead of decoding it into `method` (then empty) and
    /// interning the text again. Current only until the next allocation.
    method_ref: Option<cratonvm_types::ObjectRef>,
}

fn sfi_element_from_walk(
    ctx: &mut dyn NativeContext,
    sfi: cratonvm_types::ObjectRef,
) -> Option<SfiWalkElement> {
    let Value::Object(Some(walk)) = ctx.get_field(sfi, SF_DECL_INTERNAL) else {
        return None;
    };
    // Wave 55: the walk's lines (`populate_sfi_from_walk_lines`).
    if ctx.object_is_array(walk) {
        return sfi_element_from_walk_lines(ctx, sfi, walk);
    }
    let layout = ste_layout(ctx)?;
    let ste_class = ctx.class_id_by_name("java/lang/StackTraceElement")?;
    if ctx.class_id_of_object(walk) != ste_class {
        return None;
    }
    let read = |ctx: &mut dyn NativeContext, slot: usize| match ctx.get_field(walk, slot) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    };
    let class_dotted = read(ctx, layout.declaring_class)?;
    let method = read(ctx, layout.method_name).unwrap_or_default();
    let mut file = read(ctx, layout.file_name);
    let mut line = match ctx.get_field(walk, layout.line_number) {
        Value::Int(n) => n,
        _ => -1,
    };
    let class_id = sfi_class_id(ctx, sfi);
    let flags = match ctx.get_field_by_name(sfi, "flags") {
        Value::Int(f) => f,
        _ => 0,
    };
    let now = cratonvm_classloading::class_redefinition_count();
    if let Some(cid) = class_id {
        if sfi_class_redefined_since(&*ctx, cid, sfi_walk_count_of(flags), now) {
            if line != LINE_NUMBER_NATIVE {
                line = -1;
            }
            file = None;
            if cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok() {
                eprintln!(
                    "[redefine] stack frame read after its class's redefinition: no line: class {} \
                     (jdk walk) now={now}",
                    cid.as_u32()
                );
            }
        }
    }
    Some(SfiWalkElement {
        class_id,
        class_dotted,
        method,
        file,
        line,
        class_dotted_arc: None,
        method_ref: None,
    })
}

/// Fill `ste` (a fresh, empty element) from a lazy frame's walk
/// ([`sfi_element_from_walk`]): the full origin (module, loader name) as
/// every captured element carries it. Returns `ste`'s current address, or
/// `None` for a frame that is not lazy (nothing written).
///
/// Interpreter round i4 wave 91, lane L2: a walk-lines frame read under
/// `StackTraceFillSwitches::walk_element_interned_names` hands its interned
/// `name` String to the fill as it is (nothing allocates between the read
/// of the field in [`sfi_element_from_walk_lines`] and the fill, which roots
/// it first).
pub(crate) fn fill_element_from_walk(
    ctx: &mut dyn NativeContext,
    ste: cratonvm_types::ObjectRef,
    sfi: cratonvm_types::ObjectRef,
) -> Option<cratonvm_types::ObjectRef> {
    let walk = sfi_element_from_walk(ctx, sfi)?;
    let class_dotted: &str = walk.class_dotted_arc.as_deref().unwrap_or(&walk.class_dotted);
    let class_slashed = class_dotted.replace('.', "/");
    Some(match walk.method_ref {
        Some(method) => crate::lang_misc::fill_captured_stack_trace_element_interned_method(
            ctx,
            ste,
            walk.class_id,
            &class_slashed,
            class_dotted,
            method,
            walk.file.as_deref(),
            walk.line,
        ),
        None => crate::lang_misc::fill_captured_stack_trace_element(
            ctx,
            ste,
            walk.class_id,
            &class_slashed,
            class_dotted,
            &walk.method,
            walk.file.as_deref(),
            walk.line,
        ),
    })
}

/// The element of a real `StackFrameInfo` for the natives that stand in for
/// its accessors: its `ste`, or, for a lazy frame (wave 54), one built now
/// from the walk and memoised into `ste` as the JDK's `toStackTraceElement`
/// does. `None` when there is neither.
///
/// The JDK body memoises under `synchronized (this)`, so every caller, on
/// any thread, gets the SAME element. Interpreter round i3 wave 79, lane L2
/// (`i77-L2-compatible-walker-element-memo-unsynchronized-20261110.md`): the
/// store is a compare-and-set of the null `ste` (a loser answers the
/// winner's element; two threads racing both built one before), and the
/// allocation reclaims and retries before an `OutOfMemoryError`, as the JDK's
/// `new StackTraceElement()` does (`this` is pinned across it).
fn sfi_element(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
) -> Result<Option<cratonvm_types::ObjectRef>, MethodCallFailed> {
    if let Value::Object(Some(ste)) = ctx.get_field_by_name(this, "ste") {
        return Ok(Some(ste));
    }
    if !matches!(
        ctx.get_field(this, SF_DECL_INTERNAL),
        Value::Object(Some(_))
    ) {
        return Ok(None);
    }
    let pin = ctx.pin_native_root(this);
    let fresh = crate::util_concurrent_ext::try_alloc_concurrent_synthetic_reclaiming(
        ctx,
        "java/lang/StackTraceElement",
        4,
    );
    let fresh = match fresh {
        Ok(fresh) => fresh,
        Err(e) => {
            ctx.unpin_native_roots(pin);
            return Err(e);
        }
    };
    let fresh_pin = ctx.pin_native_root(fresh);
    let this = ctx.read_native_pin(pin, this);
    let filled = fill_element_from_walk(ctx, fresh, this);
    let this = ctx.read_native_pin(pin, this);
    let fresh = ctx.read_native_pin(fresh_pin, fresh);
    let answer = if filled.is_some() {
        Some(publish_sfi_element(ctx, this, fresh))
    } else {
        None
    };
    ctx.unpin_native_roots(pin);
    Ok(answer)
}

/// Store `fresh` into `this.ste` unless another thread stored one first, and
/// answer the element the field holds afterwards (the winner's).
fn publish_sfi_element(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    fresh: cratonvm_types::ObjectRef,
) -> cratonvm_types::ObjectRef {
    let Some(slot) = sfi_layout(&*ctx).map(|l| l.ste) else {
        ctx.set_field_by_name(this, "ste", Value::Object(Some(fresh)));
        return fresh;
    };
    if ctx.compare_and_swap_field(this, slot, Value::Object(None), Value::Object(Some(fresh))) {
        return fresh;
    }
    match ctx.get_field(this, slot) {
        Value::Object(Some(winner)) => winner,
        // Not a null-to-element race (a layout the CAS refused): keep the
        // historic plain store.
        _ => {
            ctx.set_field(this, slot, Value::Object(Some(fresh)));
            fresh
        }
    }
}

/// Return the real-JDK carrier's RETAIN_CLASS_REFERENCE state when the
/// receiver has a `ClassFrameInfo.flags` field. Synthetic StackFrame carriers
/// do not have that field and return `None`.
pub(crate) fn class_frame_retains_class_ref(
    ctx: &mut dyn NativeContext,
    frame: cratonvm_types::ObjectRef,
) -> Option<bool> {
    match ctx.get_field_by_name(frame, "flags") {
        Value::Int(flags) => Some((flags & SF_FLAG_RETAIN_CLASS_REF) != 0),
        _ => None,
    }
}

/// Read the `RETAIN_CLASS_REFERENCE` setting of the `StackWalker` that owns an
/// `AbstractStackWalker` receiver, so `populate_sfi` can record it in each
/// frame's `flags` (see [`SF_FLAG_RETAIN_CLASS_REF`]).
///
/// Real JDK layout: `StackStreamFactory.AbstractStackWalker` holds the
/// user-facing `StackWalker` in its `walker` field, and `StackWalker` holds the
/// option as the `retainClassRef` boolean (CratonVM's own synthetic
/// `StackWalker` uses the same field name — see
/// `stack_walker::FIELD_RETAIN_CLASS_REF`).
///
/// FAILS OPEN: every path that cannot see a walker — a caller that passes no
/// receiver, a receiver that is not an `AbstractStackWalker`, or a JDK layout
/// without those fields — reports `true`. `ensureRetainClassRefEnabled` throws
/// on the cleared bit, so guessing "disabled" here would turn an unknown layout
/// into a thrown `UnsupportedOperationException` on every frame, which is
/// strictly worse than the pre-existing lax behaviour.
fn walker_retains_class_ref(
    ctx: &mut dyn NativeContext,
    this: Option<cratonvm_types::ObjectRef>,
) -> bool {
    let Some(asw) = this else {
        return true;
    };
    let walker = match ctx.get_field_by_name(asw, "walker") {
        Value::Object(Some(w)) => w,
        // No `walker` field (missing fields read back as a null object
        // reference) — fail open.
        _ => return true,
    };
    match ctx.get_field_by_name(walker, "retainClassRef") {
        Value::Int(b) => b != 0,
        _ => true,
    }
}

/// Populate a StackFrameInfo from a `StackTraceEntry`.
///
/// GC-SAFETY: this allocates **eight** heap objects (four strings, the class
/// mirror, the `StackFrameInfo`, and the `StackTraceElement`). Under the moving
/// collector every `create_string` / `alloc_*` / `get_class_mirror` call can
/// trigger a young-gen GC that relocates *or collects* any of the earlier,
/// still-unrooted objects (see [`NativeContext::pin_native_root`]). The previous
/// version held all of them in bare locals and then wrote them into `sf`/`ste`
/// with `set_field` — a use-after-move/free that corrupted the heap (zeroed
/// `ClassId(0)` headers, wild `ValueStack::push` reads) when a GC landed in the
/// middle. This is exercised relentlessly by log4j2's `StackWalker`-based
/// `getCallerClass` during Hibernate's deep startup, which is why
/// `ByteArrayMappingTests` SIGSEGV'd. Allocate everything first under pins, read
/// every reference back through its pin, then set the (allocation-free) fields.
///
/// `retain_class_ref` is the owning walker's `RETAIN_CLASS_REFERENCE` setting
/// (see [`walker_retains_class_ref`]); it lands in the frame's `flags` word so
/// `ClassFrameInfo.ensureRetainClassRefEnabled()` can answer honestly instead
/// of reading a hard-coded `0` and rejecting every frame.
/// The p59 `StackWalker.walk`/`forEach` natives build this carrier
/// (`phases_late::reflect_invoke::populate_stack_frame`). It is a bare
/// synthetic whose slots are addressed by index, not by name.
const P59_STACK_FRAME: &str = "java/lang/StackWalker$StackFrame";

/// Slot of the eagerly-resolved declaring-class mirror on [`P59_STACK_FRAME`].
const P59_SF_DECL_MIRROR: usize = 6;

/// True when `obj` is a `java.lang.Class` mirror rather than, say, the
/// `ResolvedMethodName` a real-JDK `ClassFrameInfo` can hold.
fn is_class_mirror(ctx: &mut dyn NativeContext, obj: cratonvm_types::ObjectRef) -> bool {
    // The class's own name `Arc` (wave 79, L2): the same answer without a
    // `String` per call.
    ctx.class_name_arc_of_id(ctx.class_id_of_object(obj))
        .is_some_and(|n| &*n == "java/lang/Class")
}

/// `CRATONVM_COMPAT_SFI_CLASS_NAME_DIRECT` for this VM -- default OFF
/// (`truthy_word`), read once per VM per thread. Interpreter round i3 wave
/// 79, lane L2 (`i77-L2-proposal-compatible-frame-class-name-without-reentry-20261110.md`):
/// the `--compatible` `StackFrameInfo.getClassName()` native answers a frame
/// that keeps its walk's lines from `Class.getName()`'s own memo (the
/// mirror's `name` field, [`class_name_memo`]) before it re-enters Java for
/// `getName()`.
fn compat_sfi_class_name_direct(ctx: &dyn NativeContext) -> bool {
    thread_local! {
        static ON: std::cell::Cell<Option<(usize, bool)>> = const { std::cell::Cell::new(None) };
    }
    let vm = ctx.vm_identity();
    ON.with(|c| match c.get() {
        Some((owner, on)) if owner == vm => on,
        _ => {
            let on =
                cratonvm_types::flags::runtime_flag_on("CRATONVM_COMPAT_SFI_CLASS_NAME_DIRECT");
            c.set(Some((vm, on)));
            on
        }
    })
}

/// The String JDK 25 `Class.getName()` returns without running anything:
/// its `name` field once `initClassName()` (or the VM) stored it -- the
/// same object, so identity is the body's. `None` while the field is null
/// (the caller then calls `getName()`, which fills it) or on a layout with
/// no such `String` field.
fn class_name_memo(
    ctx: &mut dyn NativeContext,
    mirror: cratonvm_types::ObjectRef,
) -> Option<cratonvm_types::ObjectRef> {
    let Value::Object(Some(name)) = ctx.get_field_by_name(mirror, "name") else {
        return None;
    };
    ctx.class_name_arc_of_id(ctx.class_id_of_object(name))
        .is_some_and(|n| &*n == "java/lang/String")
        .then_some(name)
}

/// The `Class` mirror a frame's `classOrMemberName` names: the mirror itself
/// (a bare `Class`, as the carrier held it before the `ResolvedMethodName` was
/// minted for every frame) or the `vmholder` of the `ResolvedMethodName`
/// `populate_sfi` now stores (the shape HotSpot's walk leaves there, and the
/// one the JDK's own `StackFrameInfo.declaringClass()` casts to).
fn class_or_member_mirror(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
) -> Option<cratonvm_types::ObjectRef> {
    if is_class_mirror(ctx, obj) {
        return Some(obj);
    }
    let is_rmn = ctx
        .class_name_of_id(ctx.class_id_of_object(obj))
        .is_some_and(|n| n == "java/lang/invoke/ResolvedMethodName");
    if !is_rmn {
        return None;
    }
    match ctx.get_field_by_name(obj, "vmholder") {
        Value::Object(Some(m)) if is_class_mirror(ctx, m) => Some(m),
        _ => None,
    }
}

// PERF (2026-08-24, `quartz-stackwalker-walk-is-38x-hotspot`): `populate_sfi`
// resolved FIVE field names on `java/lang/StackFrameInfo` for every frame it
// materialised, and every `resolve_field_index` takes the class-manager
// `RwLock`, hashes the class name, `memcmp`s it against the loaded-class table
// and then walks the field list comparing names. At the Quartz stack depth
// (~53) that is ~265 name resolutions per `StackWalker.walk`, and Mockito runs
// one walk per mock invocation.
//
// The layout of `java.lang.StackFrameInfo` is fixed for the life of a VM once
// the class is loaded, so it is resolved once and read from a thread-local
// afterwards. The two rules the sibling bignum layout memo states keep this
// honest, and are repeated here because they are the whole correctness
// argument:
//
//   * scoped by `vm_identity()` -- Rust tests build several independent `Vm`s in
//     one process and a synthetic-JDK VM has no such layout at all, so an entry
//     from another VM is never returned;
//   * only a COMPLETE, successful resolve is stored. Before the class is loaded
//     the resolve legitimately answers `None`, and caching that would pin every
//     later call to the by-name fallback for the life of the process.
//
// `classOrMemberName` and `flags` are declared on the SUPERCLASS
// (`ClassFrameInfo`), so each name is looked up on the subclass first and on
// the superclass second -- the same two-step `getDeclaringClass` already
// performs.
#[derive(Clone, Copy)]
struct SfiLayout {
    class_or_member_name: usize,
    flags: usize,
    name: usize,
    bci: usize,
    ste: usize,
    /// `Object type` (JDK 22+), the expanded descriptor; `None` when the
    /// carrier declares no such `Object` / `String` field.
    type_: Option<usize>,
}

thread_local! {
    static SFI_LAYOUT_TLS: std::cell::Cell<Option<(usize, SfiLayout)>> =
        const { std::cell::Cell::new(None) };
}

fn sfi_field_index(ctx: &dyn NativeContext, field: &str) -> Option<usize> {
    ctx.resolve_field_index("java/lang/StackFrameInfo", field)
        .or_else(|| ctx.resolve_field_index("java/lang/ClassFrameInfo", field))
}

/// The memoized `StackFrameInfo` field layout, or `None` when the real class is
/// not loaded (synthetic-JDK mode, or before first load) -- callers then fall
/// back to `set_field_by_name`, which is exactly what they did before this memo.
fn sfi_layout(ctx: &dyn NativeContext) -> Option<SfiLayout> {
    let vm = ctx.vm_identity();
    if let Some((cached_vm, layout)) = SFI_LAYOUT_TLS.with(std::cell::Cell::get) {
        if cached_vm == vm {
            return Some(layout);
        }
    }
    let layout = SfiLayout {
        class_or_member_name: sfi_field_index(ctx, "classOrMemberName")?,
        flags: sfi_field_index(ctx, "flags")?,
        name: sfi_field_index(ctx, "name")?,
        bci: sfi_field_index(ctx, "bci")?,
        ste: sfi_field_index(ctx, "ste")?,
        type_: sfi_field_index(ctx, "type").filter(|_| {
            ctx.class_id_by_name("java/lang/StackFrameInfo")
                .is_some_and(|cid| {
                    ctx.declared_fields(cid).into_iter().any(|f| {
                        f.name == "type"
                            && matches!(
                                f.descriptor.as_str(),
                                "Ljava/lang/Object;" | "Ljava/lang/String;"
                            )
                    })
                })
        }),
    };
    SFI_LAYOUT_TLS.with(|c| c.set(Some((vm, layout))));
    Some(layout)
}

/// Write one `StackFrameInfo` field through the memoized index when there is
/// one, and by name when there is not.
fn sfi_set(
    ctx: &mut dyn NativeContext,
    sf: cratonvm_types::ObjectRef,
    field: &str,
    idx: Option<usize>,
    value: Value,
) {
    match idx {
        Some(i) => ctx.set_field(sf, i, value),
        None => ctx.set_field_by_name(sf, field, value),
    }
}

/// The real-JDK `java.lang.StackTraceElement` field layout, memoized on the
/// same two rules as [`sfi_layout`] (scoped by `vm_identity`, only a COMPLETE
/// successful resolve is stored).
///
/// `populate_sfi` used to write this carrier by RAW SLOT `0..3`, which is the
/// SYNTHETIC stub's layout (`[class, method, file, line]`). Real JDK 25 declares
///
/// ```text
/// String classLoaderName; String moduleName; String moduleVersion;
/// String declaringClass;  String methodName; String fileName; int lineNumber;
/// Class<?> declaringClassObject; ...
/// ```
///
/// so slot 0 is `classLoaderName` and slot 3 is `declaringClass` — every write
/// landed one field-group early. `getClassName()` read a null loader name,
/// `getLineNumber()` read a String slot, and `StackFrameInfo.toString()` NPE'd
/// inside `StackTraceElement.computeFormat()` on a null `declaringClass`. It
/// went unnoticed because `p59_sw_walk` intercepted `StackWalker.walk` before
/// the JDK's own `callStackWalk` — the only producer of these carriers — could
/// run.
///
/// `declaringClassObject` is part of the layout because `computeFormat()` calls
/// `declaringClassObject.getClassLoader0()` with NO null guard (JDK 25), so a
/// carrier without it turns any `toString()` into an NPE — the same trap
/// `lang_misc::fill_stack_trace_element` documents.
#[derive(Clone, Copy)]
struct SteLayout {
    declaring_class: usize,
    method_name: usize,
    file_name: usize,
    line_number: usize,
    declaring_class_object: usize,
}

thread_local! {
    static STE_LAYOUT_TLS: std::cell::Cell<Option<(usize, SteLayout)>> =
        const { std::cell::Cell::new(None) };
}

fn ste_layout(ctx: &dyn NativeContext) -> Option<SteLayout> {
    let vm = ctx.vm_identity();
    if let Some((cached_vm, layout)) = STE_LAYOUT_TLS.with(std::cell::Cell::get) {
        if cached_vm == vm {
            return Some(layout);
        }
    }
    let idx = |field: &str| ctx.resolve_field_index("java/lang/StackTraceElement", field);
    let layout = SteLayout {
        declaring_class: idx("declaringClass")?,
        method_name: idx("methodName")?,
        file_name: idx("fileName")?,
        line_number: idx("lineNumber")?,
        declaring_class_object: idx("declaringClassObject")?,
    };
    STE_LAYOUT_TLS.with(|c| c.set(Some((vm, layout))));
    Some(layout)
}

/// `MethodType.fromMethodDescriptorString(desc, loader)` for the frame
/// `frame`, with `loader` its DECLARING class's loader, as HotSpot's
/// `StackFrameInfo.getMethodType()` resolves the frame's descriptor
/// (`MethodType.fromDescriptor(descriptor, declaringClass.getClassLoader())`).
/// `null` -- the system loader -- cannot see a class of a user loader's own
/// jar: a frame of a class a `URLClassLoader` (or Spring Boot's
/// `LaunchedClassLoader`) defined, whose method names such a class, answered
/// `TypeNotPresentException`. A frame whose declaring class cannot be read
/// keeps `null`. Interpreter round i2 wave 64, lane L5 follow-up
/// (`docs/feature-designs/class-resolution-and-caching.md`).
pub(crate) fn frame_method_type_for_descriptor(
    ctx: &mut dyn NativeContext,
    frame: cratonvm_types::ObjectRef,
    desc: &str,
) -> MethodCallResult {
    let loader = match ctx.invoke_virtual(frame, "getDeclaringClass", "()Ljava/lang/Class;", &[]) {
        Ok(Some(Value::Object(Some(class)))) => {
            match ctx.invoke_virtual(class, "getClassLoader", "()Ljava/lang/ClassLoader;", &[]) {
                Ok(Some(Value::Object(loader))) => loader,
                _ => None,
            }
        }
        _ => None,
    };
    // GC-safety: the descriptor `String` below can collect and move the
    // loader.
    let pin = loader.map(|l| ctx.pin_native_root(l));
    let desc_str = ctx.create_string(desc);
    let loader = match (pin, loader) {
        (Some(pin), Some(l)) => {
            let l = ctx.read_native_pin(pin, l);
            ctx.unpin_native_roots(pin);
            Some(l)
        }
        _ => None,
    };
    ctx.invoke(
        "java/lang/invoke/MethodType",
        "fromMethodDescriptorString",
        "(Ljava/lang/String;Ljava/lang/ClassLoader;)Ljava/lang/invoke/MethodType;",
        &[Value::Object(Some(desc_str)), Value::Object(loader)],
    )
}

/// The descriptor a frame carrier recorded for its own method when the walk
/// built it (interpreter round i1 wave 48, lane L3): a real
/// `StackFrameInfo`'s `type` while it still holds the descriptor String
/// ([`populate_sfi`]), or the native walk's carrier's descriptor slot
/// (`reflect_invoke::populate_stack_frame`). `None` for a frame that recorded
/// none (a walker without RETAIN_CLASS_REFERENCE, an entry with no
/// descriptor); the accessors then fall back to the class's first overload
/// of the method's name, as before.
pub(crate) fn frame_recorded_descriptor(
    ctx: &mut dyn NativeContext,
    this: cratonvm_types::ObjectRef,
) -> Option<String> {
    if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "type") {
        if let Some(desc) = ctx.read_string(s) {
            return Some(desc);
        }
    }
    crate::phases_late::reflect_invoke::p59_frame_recorded_descriptor(ctx, this)
}

pub(crate) fn populate_sfi(
    ctx: &mut dyn NativeContext,
    entry: &cratonvm_native_api::StackTraceEntry,
    retain_class_ref: bool,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    // Reuse the shared `dotted_class_name` cache (lang_class) so repeat
    // frames for the same class do not re-run `.replace('/', '.')` and
    // allocate a fresh `String` per frame. The cache returns an
    // `Arc<str>` keyed by `ClassId`; first touch computes the dotted
    // form, subsequent reads clone the `Arc`. (Rust-side, no Java alloc.)
    // Prefer the ClassId captured directly off the live interpreter frame
    // (see `StackTraceEntry::class_id`'s doc comment) over a fresh by-name
    // lookup: a class executing its own `<clinit>` is guaranteed loaded, but
    // is not reliably found by `class_id_by_name` from deep inside that same
    // `<clinit>` (observed via `SpringFactoriesLoader`/`EntityManagerFactoryUtils`
    // calling `LogFactory.getLog()` from their own static initializers).
    let cid = entry
        .class_id
        .or_else(|| ctx.class_id_by_name(&entry.class_name));
    if cid.is_none() && crate::nbflags().sfi_null_trace {
        eprintln!(
            "[SFI-NULL-TRACE populate] no ClassId for frame {}.{} (entry.class_id={:?}, by-name lookup also missed)",
            entry.class_name, entry.method_name, entry.class_id
        );
    }
    let dotted = match cid {
        Some(c) => crate::lang_class::dotted_class_name(ctx.vm_identity(), c, &entry.class_name),
        None => std::sync::Arc::from(crate::lang_class::dotted_binary_name(&entry.class_name)),
    };

    // ---- Allocate everything, pinning each object as it is created so the
    // next allocation cannot move/collect it. Pins return sequential handles;
    // `base` (the first) releases the whole batch. ----
    // Class, method and file names are interned on HotSpot (the
    // `StackTraceElement` a frame expands to), so they name the pooled
    // constructor (bigdec page design A, step 1).
    let mut cls_str = ctx.intern_string(&dotted);
    let base = ctx.pin_native_root(cls_str);
    let mut meth_str = ctx.intern_string(&entry.method_name);
    let h_meth = ctx.pin_native_root(meth_str);
    let (mut file_str, h_file) = match &entry.source_file {
        Some(f) => {
            let s = ctx.intern_string(f);
            let h = ctx.pin_native_root(s);
            (Some(s), Some(h))
        }
        None => (None, None),
    };
    // Interpreter round i2 wave 54, lane L3: the element is left to its first
    // read (`sfi_element_from_walk`) on the real carrier only -- the one
    // whose `ste` the JDK's `toStackTraceElement` fills through
    // `StackTraceElement.of` -> `initStackTraceElement` -- and the internal
    // name is not stored then (its slot carries the walk's element).
    let layout = sfi_layout(ctx);
    let lazy_element = layout.is_some() && ctx.sfi_lazy_element();
    let (mut decl_internal, h_decl) = if lazy_element {
        (None, None)
    } else {
        let s = ctx.create_string(&entry.class_name);
        let h = ctx.pin_native_root(s);
        (Some(s), Some(h))
    };
    // The frame's own descriptor, for `getDescriptor()` / `getMethodType()`,
    // which only a RETAIN_CLASS_REFERENCE walker may call: an overloaded
    // method's frame is otherwise named by its first declared overload
    // (interpreter round i1 wave 48, lane L3;
    // `tools/probes/interp/L3/L3W48WalkOverloadDescriptor.java`). Interned,
    // as HotSpot's `expandStackFrameInfo` stores the method's signature.
    let (mut desc_str, h_desc) = match entry.method_descriptor.as_deref() {
        Some(desc) if retain_class_ref => {
            let s = ctx.intern_string(desc);
            let h = ctx.pin_native_root(s);
            (Some(s), Some(h))
        }
        _ => (None, None),
    };
    let (mut class_mirror, h_mirror) = match cid {
        Some(c) => {
            let m = ctx.get_class_mirror(c);
            let h = ctx.pin_native_root(m);
            (Some(m), Some(h))
        }
        None => (None, None),
    };
    // A SECOND mirror, for the `StackTraceElement`'s `declaringClassObject`
    // only. It falls back to `java/lang/Object` when the frame's own class
    // cannot be resolved, because `computeFormat()` dereferences this field
    // with no null guard and one null element NPEs the whole `toString()`.
    // `classOrMemberName` above deliberately does NOT take that fallback: a
    // wrong declaring class is worse than a missing one.
    let (mut ste_mirror, h_ste_mirror) = match class_mirror {
        Some(m) => (Some(m), None),
        None => match ctx.class_id_by_name("java/lang/Object") {
            Some(c) => {
                let m = ctx.get_class_mirror(c);
                let h = ctx.pin_native_root(m);
                (Some(m), Some(h))
            }
            None => (None, None),
        },
    };
    let mut sf =
        try_alloc_concurrent_synthetic(ctx, "java/lang/StackFrameInfo", STACK_FRAME_INFO_FIELDS)?;
    let h_sf = ctx.pin_native_root(sf);
    let mut ste = try_alloc_concurrent_synthetic(ctx, "java/lang/StackTraceElement", 4)?;
    let h_ste = ctx.pin_native_root(ste);
    // Pure-bytecode lever: `StackFrameInfo.declaringClass()` is the JDK's own
    // `JLIA.getDeclaringClass(classOrMemberName)`, a checkcast to
    // `ResolvedMethodName` then `vmholder`. HotSpot's stack walk stores that
    // object, so mint it (its `vmholder` is written by name below) instead of
    // the bare `Class` the retired native override accepted.
    // Now minted for every frame (SBR3 `refmisc`): the JDK's
    // `StackFrameInfo.declaringClass()` is retired, so the real cast has to
    // find the object HotSpot's walk stores there.
    let h_rmn = if class_mirror.is_some() {
        try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/ResolvedMethodName", 1)
            .ok()
            .map(|o| (o, ctx.pin_native_root(o)))
    } else {
        None
    };

    // The pre-filled `ste` of a real-layout `StackTraceElement` is built by the
    // capture's own filler, which also stores the frame class's module, module
    // version, loader name and `format` (HotSpot's `StackTraceElement.of` ->
    // `initStackTraceElement`), so `toStackTraceElement()` of a JDK frame
    // prints `java.base/java.util...` and an application frame answers
    // `getClassLoaderName() == "app"`. It allocates: it runs BEFORE the
    // read-back below, and `ste` is re-read through its pin afterwards.
    if ste_layout(ctx).is_some() {
        let filled = crate::lang_misc::fill_captured_stack_trace_element(
            ctx,
            ste,
            cid,
            &entry.class_name,
            &dotted,
            &entry.method_name,
            entry.source_file.as_deref(),
            entry.line_number,
        );
        ste = filled;
    }

    // ---- All allocations done. Read every reference back through its pin so
    // we use the *current* (post-GC, possibly forwarded) location. ----
    cls_str = ctx.read_native_pin(base, cls_str);
    meth_str = ctx.read_native_pin(h_meth, meth_str);
    if let (Some(s), Some(h)) = (file_str, h_file) {
        file_str = Some(ctx.read_native_pin(h, s));
    }
    if let (Some(s), Some(h)) = (decl_internal, h_decl) {
        decl_internal = Some(ctx.read_native_pin(h, s));
    }
    if let (Some(s), Some(h)) = (desc_str, h_desc) {
        desc_str = Some(ctx.read_native_pin(h, s));
    }
    if let (Some(m), Some(h)) = (class_mirror, h_mirror) {
        class_mirror = Some(ctx.read_native_pin(h, m));
    }
    ste_mirror = match (ste_mirror, h_ste_mirror) {
        // Its own pin (the `class_mirror` was null and we fell back to Object).
        (Some(m), Some(h)) => Some(ctx.read_native_pin(h, m)),
        // Same object as `class_mirror`, already re-read through `h_mirror`.
        (Some(_), None) => class_mirror,
        _ => None,
    };
    sf = ctx.read_native_pin(h_sf, sf);
    ste = ctx.read_native_pin(h_ste, ste);

    // ---- Set fields. These are pure heap writes — no allocation — so no GC
    // can intervene between them. ----
    //
    // Real-JDK layout (jdk-25):
    //   class ClassFrameInfo { Object classOrMemberName; int flags; }
    //   class StackFrameInfo extends ClassFrameInfo {
    //       String name; Object type; int bci;
    //       ContinuationScope contScope; volatile StackTraceElement ste;
    //   }
    // The Spring-Boot deduceMainApplicationClass path iterates
    // `Stream<StackFrame>` via the StackFrameTraverser Spliterator, which
    // calls `frame.getMethodName()`. The real-JDK StackFrameInfo
    // implementation reads `name` (slot for `name`); if null it invokes
    // the native `expandStackFrameInfo` (which we implement below, though it
    // only fills `type` — `name`/`bci` are written eagerly right here). And
    // `getDeclaringClass()` / `getClassName()` go through
    // `ClassFrameInfo.declaringClass()` → `JLIA.getDeclaringClass(
    // classOrMemberName)` which casts `classOrMemberName` to
    // `ResolvedMethodName` and crashes on anything else.
    //
    // Resolve fields by NAME so the right slot is hit on whichever real
    // class layout is present, and pre-fill them with the values our
    // own native overrides would also return.
    //
    // Do **not** reuse the old 6-field `StackWalker$StackFrame` indices here:
    // SF_FILENAME was written to slot 2, which is JDK `name`, so
    // `getMethodName()` (wired to `name`) returned null, Spring's
    // `deduceMainApplicationClass` never saw `"main"`, and
    // `StartupInfoLogger` NPE'd on `sourceClass.getPackage()`.
    let class_mirror_val = match (class_mirror, h_rmn) {
        (Some(m), Some((rmn, h))) => {
            let rmn = ctx.read_native_pin(h, rmn);
            ctx.set_field_by_name(rmn, "vmholder", Value::Object(Some(m)));
            Value::Object(Some(rmn))
        }
        (Some(m), None) => Value::Object(Some(m)),
        (None, _) => Value::Object(None),
    };
    // One memoized layout read instead of five name resolutions per frame; see
    // `sfi_layout`. `None` (synthetic-JDK, or the class not yet loaded) keeps
    // the by-name writes this replaced.
    sfi_set(
        ctx,
        sf,
        "classOrMemberName",
        layout.map(|l| l.class_or_member_name),
        class_mirror_val,
    );
    // `flags` mirrors ClassFrameInfo's: bit 27 is RETAIN_CLASS_REF (copied
    // from the walker); of the `Modifier` bits below, `Modifier.NATIVE`
    // (`0x100`) is what `isNativeMethod` / `getLineNumber` /
    // `getByteCodeIndex` read: a native frame (line `-2`) carries it, as
    // HotSpot's `ClassFrameInfo.flags` carries the method's modifiers
    // (interpreter round i2 wave 54, lane L3: `isNativeMethod()` answered
    // `false` for every frame of a `--jdk-only` walk). A lazy element keeps
    // the walk's class-redefinition count in the bits no Java reads
    // ([`sfi_flags_with_walk_count`]).
    let mut flags = if retain_class_ref {
        SF_FLAG_RETAIN_CLASS_REF
    } else {
        0
    };
    if entry.line_number == LINE_NUMBER_NATIVE {
        flags |= MODIFIER_NATIVE;
    }
    if lazy_element {
        flags = sfi_flags_with_walk_count(flags, cratonvm_classloading::class_redefinition_count());
    }
    sfi_set(ctx, sf, "flags", layout.map(|l| l.flags), Value::Int(flags));
    sfi_set(
        ctx,
        sf,
        "name",
        layout.map(|l| l.name),
        Value::Object(Some(meth_str)),
    );
    sfi_set(
        ctx,
        sf,
        "bci",
        layout.map(|l| l.bci),
        Value::Int(entry.byte_code_index),
    );
    // `type` (JDK 22+'s `Object type`): the descriptor String, the shape
    // HotSpot's expansion leaves there. Only with the memoized layout, which
    // exists exactly on a JDK whose carrier declares it.
    if let (Some(s), Some(type_slot)) = (desc_str, layout.and_then(|l| l.type_)) {
        ctx.set_field(sf, type_slot, Value::Object(Some(s)));
    }

    // Pre-cache `ste` so real JDK `toStackTraceElement()` / `getFileName()` /
    // `getLineNumber()` paths see a populated element without running
    // `StackTraceElement.of`.
    let file_val = file_str.map_or(Value::Object(None), |s| Value::Object(Some(s)));
    match ste_layout(ctx) {
        Some(l) => {
            ctx.set_field(ste, l.declaring_class, Value::Object(Some(cls_str)));
            ctx.set_field(ste, l.method_name, Value::Object(Some(meth_str)));
            ctx.set_field(ste, l.file_name, file_val);
            ctx.set_field(ste, l.line_number, Value::Int(entry.line_number));
            if let Some(m) = ste_mirror {
                ctx.set_field(ste, l.declaring_class_object, Value::Object(Some(m)));
            }
        }
        // Synthetic-stub layout: `[class, method, file, line]`, no mirror slot.
        None => {
            ctx.set_field(ste, 0, Value::Object(Some(cls_str)));
            ctx.set_field(ste, 1, Value::Object(Some(meth_str)));
            ctx.set_field(ste, 2, file_val);
            ctx.set_field(ste, 3, Value::Int(entry.line_number));
        }
    }
    if lazy_element {
        // Wave 54: `ste` stays null, so the JDK's `toStackTraceElement()`
        // builds the element at its first read (`StackTraceElement.of` ->
        // `initStackTraceElement`), which settles it against a redefinition
        // since the walk (`sfi_element_from_walk`). The walk's element rides
        // in the `contScope` slot (`SF_DECL_INTERNAL`) no Java reads, in place
        // of the internal name ([`sfi_decl_internal`] derives that from the
        // frame's class).
        ctx.set_field(sf, SF_DECL_INTERNAL, Value::Object(Some(ste)));
    } else {
        sfi_set(
            ctx,
            sf,
            "ste",
            layout.map(|l| l.ste),
            Value::Object(Some(ste)),
        );
        // `declaring_class_native` fast-path reads `SF_DECL_INTERNAL` — on the
        // real class this aliases `contScope` (slot 5); we stash the '/'-form
        // internal name there for class mirror lookup.
        ctx.set_field(sf, SF_DECL_INTERNAL, Value::Object(decl_internal));
    }

    ctx.unpin_native_roots(base);
    Ok(sf)
}

// ---------------------------------------------------------------------------
// A walk's lines (interpreter round i2 wave 55, lane L3)
// ---------------------------------------------------------------------------
//
// Proposal `docs/feature-designs/native-dispatch-architecture.md`.
// A lazy `StackFrameInfo` (wave 54) still paid, per frame, for the
// `StackTraceElement` it kept for its first read, and for interning the
// element's class name and file -- though the common reader (`findFirst`, a
// caller-class filter) reads the class and method name only. Under
// `CRATONVM_SFI_WALK_LINES` the native walk (`reflect_invoke::p59_sw_walk` /
// `p59_sw_forEach`) builds ONE `int[]` per walk instead
// ([`walk_lines_for`]): slots 0 and 1 the walk's class-redefinition count
// (low, high word), then per frame its line plus 2 in the low 24 bits and,
// in bit 24, "no file" (an old activation, or a class with no `SourceFile`).
// Each frame keeps that array in its `contScope` slot (no Java reads it) and
// its index in the bits of `flags` no Java reads (where a wave-54 frame keeps
// its walk count, so at most [`WALK_LINES_MAX_FRAMES`] frames per walk; the
// rest are built as before). Its first read builds the element from the
// frame (class, method name) and that entry ([`sfi_element_from_walk`]).

/// The most frames of one walk that keep their entry in the walk's lines:
/// the index rides in seven free bits of `flags`.
pub(crate) const WALK_LINES_MAX_FRAMES: usize = SF_WALK_COUNT_MODULUS as usize;
/// The walk's lines' header: its class-redefinition count, two words.
const WALK_LINES_HEADER: usize = 2;
/// A frame's entry: the line plus two in the low 24 bits...
const WALK_LINE_MASK: i32 = 0x00FF_FFFF;
/// ...and "no file" in bit 24.
const WALK_LINE_NO_FILE: i32 = 0x0100_0000;

/// The `int[]` contents of a walk's lines for `frames` (INNERMOST first, the
/// walk's order): the header, then the first [`WALK_LINES_MAX_FRAMES`] frames'
/// entries.
pub(crate) fn walk_lines_for(frames: &[StackTraceEntry]) -> Vec<i32> {
    walk_lines_chunk_for(frames, 0, cratonvm_classloading::class_redefinition_count())
}

/// WALK21-1 (`CRATONVM_STACKWALK_DEEP_WALK_LINES`, JIT round 21 wave 1, lane
/// walk): the `int[]` contents of chunk `chunk` of a walk's lines -- the
/// header (the walk's class-redefinition `count`, the same for every chunk of
/// one walk), then the entries of frames `chunk * 128 ..` (at most
/// [`WALK_LINES_MAX_FRAMES`]). Chunk 0 is exactly [`walk_lines_for`]. Empty
/// past the walk's last frame.
pub(crate) fn walk_lines_chunk_for(
    frames: &[StackTraceEntry],
    chunk: usize,
    count: u64,
) -> Vec<i32> {
    let start = chunk.saturating_mul(WALK_LINES_MAX_FRAMES);
    if chunk > 0 && start >= frames.len() {
        return Vec::new();
    }
    let n = frames.len().saturating_sub(start).min(WALK_LINES_MAX_FRAMES);
    let mut out = Vec::with_capacity(WALK_LINES_HEADER + n);
    // Casts: the count's two 32-bit halves, as bit patterns.
    out.push(count as u32 as i32);
    out.push((count >> 32) as u32 as i32);
    for entry in &frames[start..start + n] {
        out.push(walk_line_encode(entry));
    }
    out
}

/// One frame's entry in a walk's lines: its line plus two in the low 24 bits
/// and "no file" in bit 24 ([`walk_line_decode`] reads it back).
pub(crate) fn walk_line_encode(entry: &StackTraceEntry) -> i32 {
    // An entry with a class and no file has none to show (the walker's
    // capture names the class's file for every other such entry), as the
    // p59 carrier's `P59_SF_NO_FILE` reads it.
    let no_file = entry.class_id.is_some() && entry.source_file.is_none();
    let line = entry.line_number.saturating_add(2).clamp(0, WALK_LINE_MASK);
    line | if no_file { WALK_LINE_NO_FILE } else { 0 }
}

/// `CRATONVM_STACKWALK_DEFERRED_LINES=1` (WALK21-4, JIT round 21 wave 1, lane
/// walk; opt-in, default OFF): a batched pipeline walk
/// (`reflect_invoke::p59_sw_walk`, `--jdk-only`) takes the capture with its
/// lines DEFERRED (`NativeContext::capture_stack_walk_trace_deferred`,
/// WALK17-3's lean capture: the same entries, the `LineNumberTable` searches
/// left out), and a frame gets its line when its batch builds it
/// (`reflect_invoke::p59_sw_frame` resolves the entry, [`settle_walk_line`]
/// writes the frame's slot of the walk's lines). So a walk that consumes
/// `k` frames searches `k` line tables, not all of them, as HotSpot's
/// `fill_in_frames` fills only the batch it is asked for. Exact: the
/// deferred capture is taken only while no class was ever redefined (else it
/// is the eager capture), its resolution is the eager capture's own
/// ([`resolved_walk_entry`]), and a frame's line is read only from a built
/// frame, whose slot was written right after its build. Read once per
/// process.
pub(crate) fn stackwalk_deferred_lines_enabled() -> bool {
    // Read through the per-VM flag snapshot (`NativeFlags`), not a process
    // `OnceLock` (r21 review rev-native item 5).
    crate::nbflags().stackwalk_deferred_lines
}

/// WALK21-4: after a walk-lines frame `sf` was built from `entry` (its line
/// resolved now, [`resolved_walk_entry`]), write that line into the frame's
/// own slot of its walk's lines. A frame that keeps no lines (an eager element,
/// a `ClassFrameInfo`, the carrier) is left alone: its line is already in its
/// element. Allocates nothing; `sf` must be current.
pub(crate) fn settle_walk_line(
    ctx: &mut dyn NativeContext,
    sf: cratonvm_types::ObjectRef,
    entry: &StackTraceEntry,
) {
    let Value::Object(Some(lines)) = ctx.get_field(sf, SF_DECL_INTERNAL) else {
        return;
    };
    if !ctx.object_is_array(lines) {
        return;
    }
    let flags = match sfi_layout(ctx) {
        Some(layout) => ctx.get_field(sf, layout.flags),
        None => ctx.get_field_by_name(sf, "flags"),
    };
    let Value::Int(flags) = flags else {
        return;
    };
    // Cast: seven bits.
    let index = sfi_walk_count_of(flags) as usize;
    let _ = ctx.write_int_array_from(lines, WALK_LINES_HEADER + index, &[walk_line_encode(entry)]);
}

/// The class-redefinition count a walk's lines' header (`lines[0..2]`, as
/// [`walk_lines_chunk_for`] writes it) holds; `0` for a shorter slice.
pub(crate) fn walk_lines_header_count(lines: &[i32]) -> u64 {
    match lines {
        // Casts: the count's two 32-bit halves, as bit patterns.
        [low, high, ..] => u64::from(*low as u32) | (u64::from(*high as u32) << 32),
        _ => 0,
    }
}

/// A walk's lines' entry as `(line, no_file)`.
fn walk_line_decode(entry: i32) -> (i32, bool) {
    ((entry & WALK_LINE_MASK) - 2, entry & WALK_LINE_NO_FILE != 0)
}

/// `CRATONVM_STACKWALK_DEEP_WALK_LINES=1` (WALK21-1, JIT round 21 wave 1, lane
/// walk; opt-in, default OFF): a walk deeper than [`WALK_LINES_MAX_FRAMES`]
/// keeps the walk-lines build for EVERY frame, not only the first 128.
///
/// A frame keeps its walk index in seven free bits of `flags`, so one `int[]`
/// of lines serves 128 frames, and every frame past them took
/// [`populate_sfi`]: three interns (dotted class name, method, file), the
/// mirror, three allocations (the `StackFrameInfo`, a `StackTraceElement`
/// filled with its module / loader origin, a fresh `ResolvedMethodName`) and
/// by-name stores -- several times the lines build, on exactly the frames a
/// deep walk (a Spring / Tomcat request stack, a `depth 200` row) has most of.
/// With the switch on the walk builds one more `int[]` per further 128 frames
/// ([`walk_lines_chunk_for`], each with the same header), keeps them in one
/// pinned `Object[]` ([`WalkLines::deep`]), and frame `i` keeps chunk
/// `i / 128` in its `contScope` slot and `i % 128` in `flags`: the reader
/// ([`sfi_element_from_walk_lines`]) reads the frame's own array at the
/// frame's own index, so it is unchanged, and so is every answer (the lines
/// build and [`populate_sfi`] answer the same element, which the first 128
/// frames already rely on). WALK20-1's class kit then covers every frame too.
/// Read per call from the VM's flag snapshot.
pub(crate) fn stackwalk_deep_walk_lines_enabled() -> bool {
    // Read through the per-VM flag snapshot (`NativeFlags`), not a process
    // `OnceLock` (r21 review rev-native item 5).
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    crate::nbflags().stackwalk_deep_walk_lines
}

/// `CRATONVM_STACKWALK_PIPELINE_LAYOUT_ONCE=1` (WALK21-2, JIT round 21 wave 1,
/// lane walk; opt-in, default OFF): a pipeline walk
/// (`reflect_invoke::p59_sw_walk_jdk_pipeline`) resolves the
/// `StackFrameTraverser` class check and its three field slots, and the frame
/// buffer's six, once per VM and thread (`P59PipelineLayout`, keyed by
/// `vm_identity` and the class ids) instead of on every walk -- nine
/// class-manager read locks with a hierarchy scan each, and the traverser
/// class's name copied into a `String` to compare -- and each later batch
/// ([`pipeline_batch_fetch`]) stores the traverser's `anchor` by the slot
/// the walk resolved instead of a by-name class lookup per fetch. The same
/// slots, the same stores. A fixed per-walk cost: it moves short walks
/// (`findFirst`, `getCallerClass`-style filters) most. Read per call from the VM's flag snapshot.
pub(crate) fn stackwalk_pipeline_layout_once_enabled() -> bool {
    // Read through the per-VM flag snapshot (`NativeFlags`), not a process
    // `OnceLock` (r21 review rev-native item 5).
    crate::nbflags().stackwalk_pipeline_layout_once
}

/// `CRATONVM_STACKWALK_PIPELINE_INVOKE_BY_ID=1` (WALK21-6, JIT round 21 wave
/// 1, lane walk; opt-in, default OFF): a pipeline walk invokes the JDK's
/// `StackStreamFactory.makeStackTraverser` and
/// `AbstractStackWalker.doStackWalk` through `NativeContext::invoke_by_class_id`
/// with this VM's ids of the two owners (`reflect_invoke::p59_pipeline_invoke_owners`,
/// per thread and VM) instead of the by-name `invoke`, whose route resolves
/// the owner by name (a loader-aware class load) and asks the JNI caller
/// loader anchor on every walk before taking the same by-id tail. Read per call from the VM's
/// flag snapshot.
pub(crate) fn stackwalk_pipeline_invoke_by_id_enabled() -> bool {
    // Read through the per-VM flag snapshot (`NativeFlags`), not a process
    // `OnceLock` (r21 review rev-native item 5).
    crate::nbflags().stackwalk_pipeline_invoke_by_id
}

/// `CRATONVM_STACKWALK_WINDOW_CAPTURE=1` (WALK21-10, JIT round 21 wave 3, lane
/// walk; opt-in, default OFF): a batched pipeline walk (`--jdk-only`) captures
/// only a window of the innermost frames at its start
/// (`NativeContext::capture_stack_walk_trace_tail`, the first batch plus 16),
/// takes the anchor its call stands on (`NativeContext::stack_walk_anchor`),
/// and lists the rest only when a `fetchStackFrames` runs past the window
/// ([`pipeline_batch_fetch`] runs the walk's [`PipelineExtender`]: the
/// capture of the frames below the anchor, `capture_stack_walk_trace_anchored`,
/// screened, with its lines and kit kept in a holder the walk pinned at its
/// start). So `findFirst`, `dropWhile(..).findFirst()` and `limit(k)` walks
/// decode only the top of the stack, as HotSpot's first batch does. Exact:
/// the VM hands out a window only when it is the whole capture's innermost
/// suffix and anchors only while nothing would be spliced into the capture;
/// the screens are per frame, so the screened window is a prefix of the
/// screened whole list (checked at the extension:
/// a list that does not begin with the window, or a refused capture, lists
/// the walk from the whole capture at the fetch instead). Not taken for a
/// walker that shows hidden frames where the VM lists stand-in invoke frames,
/// nor for a continuation's walk. Read per call from the VM's flag snapshot.
pub(crate) fn stackwalk_window_capture_enabled() -> bool {
    // Read through the per-VM flag snapshot (`NativeFlags`), not a process
    // `OnceLock` (r21 review rev-native item 5).
    crate::nbflags().stackwalk_window_capture
}

/// A walk's lines as its frame builder reads them: the first chunk's `int[]`
/// and its pin (frames `0 .. 128`), and, for a deeper walk under WALK21-1
/// ([`stackwalk_deep_walk_lines_enabled`]), a pinned `Object[]` whose element
/// `k - 1` is chunk `k`'s `int[]` (frames `128 k ..`). Every reference is read
/// through its pin after the reader's last allocation; the walk's base pin
/// releases them.
///
/// WALK21-10 (`held`): the lines a walk built after its start (a window walk's
/// extension, inside a `fetchStackFrames`) are kept in slots of an `Object[]`
/// the walk pinned at its start, not under pins of their own: `pin` / `lines`
/// are then that holder, slot `s` holds chunk 0's `int[]` and slot `s + 1`
/// the deep chunks' `Object[]` (or null).
#[derive(Clone, Copy, Debug)]
pub(crate) struct WalkLines {
    pub(crate) pin: usize,
    pub(crate) lines: cratonvm_types::ObjectRef,
    pub(crate) deep: Option<(usize, cratonvm_types::ObjectRef)>,
    pub(crate) held: Option<usize>,
}

impl WalkLines {
    /// The deep chunks' `Object[]` at its current address, if any.
    fn deep_holder(&self, ctx: &mut dyn NativeContext) -> Option<cratonvm_types::ObjectRef> {
        match self.held {
            Some(slot) => {
                let holder = ctx.read_native_pin(self.pin, self.lines);
                match ctx.get_array_element(holder, slot + 1) {
                    Value::Object(Some(deep)) => Some(deep),
                    _ => None,
                }
            }
            None => self.deep.map(|(pin, deep)| ctx.read_native_pin(pin, deep)),
        }
    }

    /// May frame `index` take the lines build? Its chunk is held (chunk 0
    /// always). Allocates nothing.
    fn covers(&self, ctx: &mut dyn NativeContext, index: usize) -> bool {
        let chunk = index / WALK_LINES_MAX_FRAMES;
        if chunk == 0 {
            return true;
        }
        match self.deep_holder(ctx) {
            Some(holder) => ctx.array_length(holder) >= chunk,
            None => false,
        }
    }

    /// Frame `index`'s chunk `int[]` at its current address, or `None` (not
    /// held). Allocates nothing.
    fn chunk_of(
        &self,
        ctx: &mut dyn NativeContext,
        index: usize,
    ) -> Option<cratonvm_types::ObjectRef> {
        let chunk = index / WALK_LINES_MAX_FRAMES;
        if chunk == 0 {
            let lines = ctx.read_native_pin(self.pin, self.lines);
            return match self.held {
                Some(slot) => match ctx.get_array_element(lines, slot) {
                    Value::Object(Some(lines)) => Some(lines),
                    _ => None,
                },
                None => Some(lines),
            };
        }
        let holder = self.deep_holder(ctx)?;
        match ctx.get_array_element(holder, chunk - 1) {
            Value::Object(Some(lines)) => Some(lines),
            _ => None,
        }
    }

    /// WALK21-10: these lines moved into slots `slot`, `slot + 1` of the
    /// `Object[]` `holder` pinned at `holder_pin` (current addresses read
    /// through the pins; allocates nothing). The pins of the lines themselves
    /// may then be released.
    pub(crate) fn rehome(
        self,
        ctx: &mut dyn NativeContext,
        holder_pin: usize,
        holder: cratonvm_types::ObjectRef,
        slot: usize,
    ) -> Self {
        let first = ctx.read_native_pin(self.pin, self.lines);
        let deep = self.deep.map(|(pin, deep)| ctx.read_native_pin(pin, deep));
        let holder_now = ctx.read_native_pin(holder_pin, holder);
        ctx.set_array_element(holder_now, slot, Value::Object(Some(first)));
        ctx.set_array_element(holder_now, slot + 1, Value::Object(deep));
        Self {
            pin: holder_pin,
            lines: holder,
            deep: None,
            held: Some(slot),
        }
    }
}

// ---------------------------------------------------------------------------
// THROWS6-1: one layout resolution per walk (JIT round 19 wave 6, lane throws6)
// ---------------------------------------------------------------------------

/// `CRATONVM_STACKWALK_FRAME_LAYOUT_ONCE=1` (THROWS6-1, opt-in, default OFF):
/// a native `StackWalker.walk` / `forEach` resolves the `StackFrameInfo` and
/// `ResolvedMethodName` layouts ONCE per walk ([`WalkFrameAllocators`])
/// instead of once per frame and object. Every `try_alloc_concurrent_synthetic`
/// call resolves the class by name, checks its initialization, copies its name
/// into a `String`, reads its field count and runs the layout census
/// (`util_concurrent_ext::concurrent_synthetic_layout`); a walk frame made two
/// such calls. Interpreter round i1 wave 49 (lane L7) measured the same
/// resolution as a fifth of an element's cost in `Throwable.getStackTrace()`
/// and hoisted it there (`ConcurrentSyntheticAllocator`). The same objects,
/// the same layouts, the same frames; the allocation itself is the funnel's
/// non-collecting half, as before. Read once per process.
/// `CRATONVM_STACKWALK_LAZY_FRAMES=1` (THROWS5-3, JIT round 19 wave 7, lane
/// walk7; opt-in, default OFF): the native `StackWalker.walk` hands its
/// function a stream whose frames are built on first read
/// (`cratonvm_native_collections::push_lazy_walk_source`) instead of building
/// every frame before the function runs. Read once per process.
pub(crate) fn stackwalk_lazy_frames_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_STACKWALK_LAZY_FRAMES"))
}

/// `CRATONVM_STACKWALK_JDK_PIPELINE` (WALK7-1, JIT round 19 wave 8, lane
/// walk8; default ON since the round's close as a HotSpot-parity fix -- the
/// native walk's stream answered 30 `R19Walk8Rows` facts unlike HotSpot 25: an
/// escaped iterator walked instead of throwing `IllegalStateException`, the
/// spliterator shape and the per-stage call counts differed -- `=0` kills it): under `--jdk-only` the native
/// `StackWalker.walk` hands its function the JDK's own
/// `StreamSupport.stream(StackFrameTraverser, false)` over a frame buffer the
/// native walk filled (`phases_late::reflect_invoke::p59_sw_walk_jdk_pipeline`).
/// Read once per process.
pub(crate) fn stackwalk_jdk_pipeline_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_STACKWALK_JDK_PIPELINE")
    })
}

/// `CRATONVM_STACKWALK_JDK_PIPELINE_FOREACH=1` (WALK8-3, JIT round 19 wave 9,
/// lane walk9; opt-in, default OFF; only with [`stackwalk_jdk_pipeline_enabled`]):
/// the native `StackWalker.forEach` runs its action as HotSpot's `forEach`
/// does, `s.forEach(action)` on the JDK's own pipeline over the walk's
/// traverser (`phases_late::reflect_invoke::p59_sw_for_each`). Read once per
/// process.
pub(crate) fn stackwalk_jdk_pipeline_foreach_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_STACKWALK_JDK_PIPELINE_FOREACH")
    })
}

/// `CRATONVM_STACKWALK_JDK_PIPELINE_BATCHED` (WALK8-1, JIT round 19 wave 9,
/// lane walk9; default ON since the round's close -- short-circuit walks
/// first/skipfirst/filterfirst/limit/peekskip 3-5x faster than the eager
/// pipeline, collect rows level -- `=0` kills it; only with [`stackwalk_jdk_pipeline_enabled`]):
/// a pipeline walk deeper than HotSpot's first batch builds that batch only;
/// the JDK's `fetchStackFrames` asks for the rest, batch by batch, and
/// [`pipeline_batch_fetch`] builds them from the walk's screened list. The
/// registrar also registers [`native_stack_frame_buffer_fill`], a reviewed
/// `Intrinsic`, so the JDK's `resize` does not build the reflective
/// placeholder frames the fetch replaces. Read once per process (and once
/// per registry, at registration).
pub(crate) fn stackwalk_jdk_pipeline_batched_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_STACKWALK_JDK_PIPELINE_BATCHED")
    })
}

/// Builds frame `i` of a pipeline walk (WALK8-1): the same builder the walk's
/// first batch used.
pub(crate) type PipelineFrameBuilder = Box<
    dyn FnMut(&mut dyn NativeContext, usize) -> Result<cratonvm_types::ObjectRef, MethodCallFailed>,
>;

/// A pipeline walk (WALK8-1) whose later batches [`pipeline_batch_fetch`]
/// builds: its traverser and buffer (pinned by the walk, for the walk's whole
/// `doStackWalk`), the screened list's length and its frame builder (`None`
/// while a fetch is running it, so a nested walk can push its own source).
struct PipelineBatchSource {
    traverser_pin: usize,
    traverser: cratonvm_types::ObjectRef,
    buffer_pin: usize,
    buffer: cratonvm_types::ObjectRef,
    len: usize,
    builder: Option<PipelineFrameBuilder>,
    /// `CRATONVM_DBG_WALK_SECTIONS` (WALK20-1's census, lane xWalk): the
    /// nanoseconds the later batches spent building frames, and how many.
    batch_ns: u64,
    batch_frames: u64,
    /// WALK21-2 (`CRATONVM_STACKWALK_PIPELINE_LAYOUT_ONCE`): the traverser's
    /// `anchor` slot, resolved once by the walk; `None` resolves it by name
    /// per fetch, as before.
    anchor_slot: Option<usize>,
    /// WALK21-10 (`CRATONVM_STACKWALK_WINDOW_CAPTURE`): a window walk's
    /// extension -- the builder and length over the walk's whole list --
    /// taken by the first fetch that runs past the window.
    extend: Option<PipelineExtender>,
}

/// WALK21-10: what extends a window walk to its whole list: the new frame
/// builder and the list's length, or `None` (the window is all the walk
/// lists).
pub(crate) type PipelineExtender =
    Box<dyn FnOnce(&mut dyn NativeContext) -> Option<(PipelineFrameBuilder, usize)>>;

thread_local! {
    /// The pipeline walks in progress on this thread, innermost last
    /// (WALK8-1). Per thread and per walk: no process state.
    static PIPELINE_BATCH_SOURCES: std::cell::RefCell<Vec<PipelineBatchSource>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A registered [`PipelineBatchSource`]; dropping it (every exit of the
/// walk, a Java throw included) unregisters it.
pub(crate) struct PipelineBatchGuard {
    depth: usize,
}

impl PipelineBatchGuard {
    /// The walk's later batches so far under `CRATONVM_DBG_WALK_SECTIONS`:
    /// `(nanoseconds building frames, frames built)`.
    pub(crate) fn batch_census(&self) -> (u64, u64) {
        PIPELINE_BATCH_SOURCES.with(|s| {
            s.borrow()
                .get(self.depth)
                .map_or((0, 0), |src| (src.batch_ns, src.batch_frames))
        })
    }
}

impl Drop for PipelineBatchGuard {
    fn drop(&mut self) {
        PIPELINE_BATCH_SOURCES.with(|s| s.borrow_mut().truncate(self.depth));
    }
}

/// Register the pipeline walk whose traverser and buffer are pinned at
/// `traverser_pin` / `buffer_pin`, listing `len` frames that `builder` builds.
/// `anchor_slot` (WALK21-2): the traverser's `anchor` field index, when the
/// walk resolved it once for its fetches.
pub(crate) fn push_pipeline_batch_source(
    traverser_pin: usize,
    traverser: cratonvm_types::ObjectRef,
    buffer_pin: usize,
    buffer: cratonvm_types::ObjectRef,
    len: usize,
    builder: PipelineFrameBuilder,
    anchor_slot: Option<usize>,
    extend: Option<PipelineExtender>,
) -> PipelineBatchGuard {
    PIPELINE_BATCH_SOURCES.with(|s| {
        let mut s = s.borrow_mut();
        let depth = s.len();
        s.push(PipelineBatchSource {
            traverser_pin,
            traverser,
            buffer_pin,
            buffer,
            len,
            builder: Some(builder),
            batch_ns: 0,
            batch_frames: 0,
            anchor_slot,
            extend,
        });
        PipelineBatchGuard { depth }
    })
}

/// The index of the registered source whose traverser (`by_buffer == false`)
/// or buffer is `object`, at its current address. Allocates nothing.
fn pipeline_batch_source_of(
    ctx: &mut dyn NativeContext,
    object: cratonvm_types::ObjectRef,
    by_buffer: bool,
) -> Option<usize> {
    let pins: Vec<(usize, cratonvm_types::ObjectRef)> = PIPELINE_BATCH_SOURCES.with(|s| {
        s.borrow()
            .iter()
            .map(|src| {
                if by_buffer {
                    (src.buffer_pin, src.buffer)
                } else {
                    (src.traverser_pin, src.traverser)
                }
            })
            .collect()
    });
    pins.iter()
        .rposition(|&(pin, seed)| ctx.read_native_pin(pin, seed) == object)
}

/// WALK8-1: `AbstractStackWalker.fetchStackFrames` for a pipeline walk's
/// traverser (`args[0]`): frames `anchor ..` of the walk's screened list,
/// built by the walk's own builder into `frames[startIndex ..]`, at most
/// `batchSize - startIndex` of them (HotSpot's batch), and `anchor` moved past
/// them. `None` for any other receiver (the caller serves it as before).
fn pipeline_batch_fetch(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<MethodCallResult> {
    if !stackwalk_jdk_pipeline_batched_enabled() {
        return None;
    }
    let Some(Value::Object(Some(this))) = args.first() else {
        return None;
    };
    let index = pipeline_batch_source_of(ctx, *this, false)?;
    // JDK 25: (this, int mode, long anchor, int lastBatchFrameCount,
    // int batchSize, int startIndex, T[] frames), the long as one or two
    // slots (as `native_fetch_stack_frames` reads it).
    let (anchor, batch, start, frames) = if args.len() >= 8 {
        (
            value_as_i64_bits(args.get(2)),
            args.get(5),
            args.get(6),
            args.get(7),
        )
    } else {
        (
            value_as_i64_bits(args.get(2)),
            args.get(4),
            args.get(5),
            args.get(6),
        )
    };
    let (Some(Value::Int(batch)), Some(Value::Int(start)), Some(Value::Object(Some(frames)))) =
        (batch, start, frames)
    else {
        return None;
    };
    let (Ok(cursor), Ok(batch), Ok(start)) = (
        usize::try_from(anchor),
        usize::try_from(*batch),
        usize::try_from(*start),
    ) else {
        return None;
    };
    let (mut len, anchor_slot) = PIPELINE_BATCH_SOURCES.with(|s| {
        let s = s.borrow();
        (s[index].len, s[index].anchor_slot)
    });
    let room = batch.min(ctx.array_length(*frames)).saturating_sub(start);
    let mut builder = PIPELINE_BATCH_SOURCES.with(|s| s.borrow_mut()[index].builder.take())?;
    let this_pin = ctx.pin_native_root(*this);
    let frames_pin = ctx.pin_native_root(*frames);
    // WALK21-10: a window walk whose batch runs past its window lists the
    // rest now (the anchored capture; its pins sit above `this_pin` and are
    // released with it -- everything it keeps lives in the walk's holder).
    if cursor.saturating_add(room) > len {
        let extend = PIPELINE_BATCH_SOURCES.with(|s| s.borrow_mut()[index].extend.take());
        if let Some(extend) = extend {
            if let Some((extended, full)) = extend(ctx) {
                builder = extended;
                len = full;
                PIPELINE_BATCH_SOURCES.with(|s| {
                    if let Some(src) = s.borrow_mut().get_mut(index) {
                        src.len = full;
                    }
                });
            }
        }
    }
    let count = room.min(len.saturating_sub(cursor));
    let mut result = Ok(());
    let direct = stackwalk_pipeline_direct_store_enabled();
    let census_started = walk_sections_census_enabled().then(std::time::Instant::now);
    for k in 0..count {
        // The builder may run Java (a class initializer) and so a nested
        // walk: it pushes and pops its own source above this one.
        match builder(ctx, cursor + k) {
            // WALK9-2: the frame is current (nothing allocates from the
            // builder's answer to the store).
            Ok(frame) if direct => {
                let array = ctx.read_native_pin(frames_pin, *frames);
                ctx.set_array_element(array, start + k, Value::Object(Some(frame)));
            }
            Ok(frame) => {
                let frame_pin = ctx.pin_native_root(frame);
                let array = ctx.read_native_pin(frames_pin, *frames);
                let frame = ctx.read_native_pin(frame_pin, frame);
                ctx.set_array_element(array, start + k, Value::Object(Some(frame)));
                ctx.unpin_native_roots(frame_pin);
            }
            Err(error) => {
                result = Err(error);
                break;
            }
        }
    }
    PIPELINE_BATCH_SOURCES.with(|s| {
        if let Some(src) = s.borrow_mut().get_mut(index) {
            src.builder = Some(builder);
            if let Some(started) = census_started {
                // Cast: nanoseconds of one batch.
                src.batch_ns += started.elapsed().as_nanos() as u64;
                // Cast: a batch's frame count.
                src.batch_frames += count as u64;
            }
        }
    });
    let this = ctx.read_native_pin(this_pin, *this);
    ctx.unpin_native_roots(this_pin);
    if let Err(error) = result {
        return Some(Err(error));
    }
    // The cursor stays neither 0 (NEW) nor -1 (CLOSED): it is past the first
    // batch, which held at least one frame.
    // WALK21-2: the slot the walk resolved, else by name as before.
    if let Some(idx) = anchor_slot.or_else(|| {
        ctx.resolve_field_index("java/lang/StackStreamFactory$AbstractStackWalker", "anchor")
    }) {
        // Cast: a frame index of the walk's list.
        ctx.set_field(this, idx, Value::Long((cursor + count) as i64));
    }
    // Cast: at most a batch size (an `int`).
    Some(Ok(Some(Value::Int(count as i32))))
}

/// `StackStreamFactory$StackFrameBuffer.fill([Ljava/lang/ClassFrameInfo;II)[Ljava/lang/ClassFrameInfo;`,
/// a reviewed `NativeKind::Intrinsic` (WALK8-1,
/// `CRATONVM_STACKWALK_JDK_PIPELINE_BATCHED`, `--jdk-only` over a real JDK
/// only). The JDK body puts a fresh `ctor.newInstance(walker)` in every slot
/// `[startIndex, size)` so that HotSpot's `fetchStackFrames` can fill those
/// objects in place. For a pipeline walk's buffer the slots are written by
/// [`pipeline_batch_fetch`], which stores a freshly built frame in every slot
/// of the batch it reports and never reads a slot, and the JDK reads only
/// `[origin, fence)` of a batch, i.e. those slots: so the placeholders are
/// garbage the moment they are made, and this answers the array as is. Any
/// other buffer (a `LiveStackInfoTraverser`'s, a buffer under construction,
/// the JDK's own `callStackWalk` path) runs the JDK body unchanged
/// (`invoke_special_bytecode_only`). `ClassFrameBuffer` overrides `fill`
/// with its own bytecode and never reaches this triple.
pub(crate) fn native_stack_frame_buffer_fill(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let (Some(Value::Object(Some(this))), Some(array)) = (args.first(), args.get(1)) {
        if pipeline_batch_source_of(ctx, *this, true).is_some() {
            return Ok(Some(*array));
        }
    }
    ctx.invoke_special_bytecode_only(
        STACK_FRAME_BUFFER_CLASS,
        "fill",
        STACK_FRAME_BUFFER_FILL_DESC,
        args,
    )
}

/// `CRATONVM_STACKWALK_REFLECTION_FRAME_INTRINSIC=1` (WALK11-2's lever, JIT
/// round 19 wave 12, lane walk12; default ON since the wave 12 integration, `=0`
/// kill; walk-dropcount-deep 379-405 -> 311-326 us in 7/7 runs): the registrar puts
/// the reviewed `Intrinsic` [`native_stack_stream_is_reflection_frame`] over
/// the JDK's `StackStreamFactory.isReflectionFrame(Class)`, which every
/// frame of a pipeline walk runs (`peekFrame`) with two full-funnel
/// `Class.isAssignableFrom` calls. Read once per process (at registration).
pub(crate) fn stackwalk_reflection_frame_intrinsic_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_STACKWALK_REFLECTION_FRAME_INTRINSIC",
        )
    })
}

/// WALK12-1's leaf half (`docs/feature-designs/jit-ir-optimizations.md`;
/// JIT round 20 wave 11, lane pNat): `CRATONVM_STACKWALK_REFLECTION_FRAME_LEAF`
/// (default ON, `=0` kill) claims [`native_stack_stream_is_reflection_frame`]
/// LEAF, so its resolved answer skips the native-call funnel. Read at
/// registration (per registry, no cache).
pub(crate) fn stackwalk_reflection_frame_leaf_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_STACKWALK_REFLECTION_FRAME_LEAF")
}

/// `java/lang/StackStreamFactory.isReflectionFrame(Ljava/lang/Class;)Z`.
const IS_REFLECTION_FRAME_DESC: &str = "(Ljava/lang/Class;)Z";
/// `jdk.internal.reflect.MethodAccessor` / `ConstructorAccessor`, the two
/// interfaces `isReflectionFrame` tests.
const REFLECTION_ACCESSORS: [&str; 2] = [
    "jdk/internal/reflect/MethodAccessor",
    "jdk/internal/reflect/ConstructorAccessor",
];

/// What [`native_stack_stream_is_reflection_frame`] keeps on this thread
/// (2026-10-26, the stack-walker deep-walk page: the four by-name class
/// lookups were ~10% of a deep `count` walk, one set per frame).
#[derive(Clone, Copy, Default)]
struct ReflectionFrameRow {
    /// `(vm identity, [Method, Constructor, MethodAccessor,
    /// ConstructorAccessor])`, stored once all four resolved by name.
    ids: Option<(usize, [cratonvm_types::ClassId; 4])>,
    /// `(vm identity, c, answer)` of the last resolved-path answer: a deep
    /// walk asks about the same few classes frame after frame.
    answer: Option<(usize, cratonvm_types::ClassId, bool)>,
}

thread_local! {
    /// Per thread, keyed by VM identity (the `sfi_layout` rule): no process
    /// state. Exact: the four are boot classes, so the by-name answer is the
    /// class the uncached lookup finds, and class ids are never reused within
    /// a VM, so a class's answer (subclass and declared-interface relations
    /// fixed at its definition) is the one the resolved path recomputes.
    static REFLECTION_FRAME_TLS: std::cell::Cell<ReflectionFrameRow> =
        const { std::cell::Cell::new(ReflectionFrameRow { ids: None, answer: None }) };
}

/// The resolved path's answer for `cid` in VM `vm`, if it is the last one
/// asked on this thread.
fn reflection_frame_answer_cached(vm: usize, cid: cratonvm_types::ClassId) -> Option<bool> {
    match REFLECTION_FRAME_TLS.with(std::cell::Cell::get).answer {
        Some((v, c, b)) if v == vm && c == cid => Some(b),
        _ => None,
    }
}

/// `[Method, Constructor, MethodAccessor, ConstructorAccessor]` of VM `vm`,
/// by name once per thread and VM; `None` while any does not resolve
/// (nothing is stored then, so the next call asks again).
fn reflection_frame_ids(
    ctx: &mut dyn NativeContext,
    vm: usize,
) -> Option<[cratonvm_types::ClassId; 4]> {
    if let Some((v, ids)) = REFLECTION_FRAME_TLS.with(std::cell::Cell::get).ids {
        if v == vm {
            return Some(ids);
        }
    }
    let ids = [
        ctx.class_id_by_name("java/lang/reflect/Method")?,
        ctx.class_id_by_name("java/lang/reflect/Constructor")?,
        ctx.class_id_by_name(REFLECTION_ACCESSORS[0])?,
        ctx.class_id_by_name(REFLECTION_ACCESSORS[1])?,
    ];
    REFLECTION_FRAME_TLS.with(|c| {
        let mut row = c.get();
        row.ids = Some((vm, ids));
        c.set(row);
    });
    Some(ids)
}

/// `StackStreamFactory.isReflectionFrame(Class<?> c)`, a reviewed
/// `NativeKind::Intrinsic` (WALK11-2's lever,
/// [`stackwalk_reflection_frame_intrinsic_enabled`], over a real JDK, both
/// modes). The JDK 25 body is
///
/// ```java
/// return c == Method.class || c == Constructor.class ||
///        MethodAccessor.class.isAssignableFrom(c) ||
///        ConstructorAccessor.class.isAssignableFrom(c);
/// ```
///
/// and its two `isAssignableFrom` calls are this VM's
/// `lang_class::native_class_is_assignable_from`, each a full native funnel
/// that, for the usual `false`, also clones both class names. Review
/// argument: this answers the same expression with the same rule in one
/// native call. For a resolved, non-array `c` and resolved interfaces, that
/// native's answer is exactly `c == I || is_subclass(c, I) ||
/// loader_aware_reflect_assignable(c, I, name) ||
/// synthetic_implements_declared(c, name)` (its array, primitive and
/// unresolved-id arms do not apply; its depth guard cannot trip at a call
/// from Java), which is what [`reflection_accessor_assignable`] computes.
/// Every other case -- `c` unresolved or an array, an interface or
/// `Method` / `Constructor` not resolving by name (not yet loaded, or the
/// name ambiguous) -- evaluates the JDK expression through the native itself
/// on the real mirrors (`load_class`, as the body's `ldc` would load them).
/// `c == null` throws the message-less NPE the first `isAssignableFrom`
/// throws. No allocation on the resolved path. The four ids and the last
/// resolved answer are kept per thread and VM ([`ReflectionFrameRow`]).
///
/// WALK12-1's leaf half (JIT round 20 wave 11, lane pNat;
/// [`stackwalk_reflection_frame_leaf_enabled`]): the registrar may claim this
/// callback LEAF (`NativeMethodRegistry::set_leaf`). Only the resolved path
/// answers on the leaf path: it reads the mirror's class id, class names and
/// ids by name, and asks `is_subclass` / `loader_aware_reflect_assignable` /
/// `synthetic_implements_declared` -- class-store READ locks and atomic
/// census counters only, the same set the leaf `Class.isInstance` /
/// `Class.isArray` take (leaf rule 2), no Java allocation (rule 1), no
/// collection (rule 3), no pending exception (rule 4). Everything else --
/// a null `c` (its NPE keeps the funnel's native frame), a lambda-proxy `c`
/// (`is_subclass` asks `lambda_proxy_satisfies`, which may load the
/// functional interface), and the slow path (`load_class`, `pin_native_root`)
/// -- declines first (`NativeContext::decline_leaf_call`, before any side
/// effect) and runs again on the full funnel: the non-leaf twin is this same
/// body there, where `decline_leaf_call` answers `false`. Registered non-leaf
/// (`=0`), every `decline_leaf_call` answers `false` and this is the old body.
pub(crate) fn native_stack_stream_is_reflection_frame(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let c = match args.first() {
        Some(Value::Object(Some(c))) => *c,
        _ => {
            if ctx.decline_leaf_call() {
                return Ok(None);
            }
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: None,
            }
            .into());
        }
    };
    let answer = |b: bool| Ok(Some(Value::Int(i32::from(b))));
    // Resolved once, before the fast path, so a lambda-proxy class can decline
    // the leaf path before `is_subclass` reaches `lambda_proxy_satisfies`.
    let resolved = crate::lang_class::mirror_class_id(&*ctx, c);
    if resolved.is_some_and(|cid| cid.as_u32() >= 0x8000_0000) && ctx.decline_leaf_call() {
        return Ok(None);
    }
    let vm = ctx.vm_identity();
    let fast = (|| {
        let cid = resolved?;
        if let Some(b) = reflection_frame_answer_cached(vm, cid) {
            return Some(b);
        }
        if ctx.class_name_arc_of_id(cid)?.starts_with('[') {
            return None;
        }
        let [method, ctor, accessor_m, accessor_c] = reflection_frame_ids(ctx, vm)?;
        let accessors = [accessor_m, accessor_c];
        let b = cid == method
            || cid == ctor
            || accessors
                .iter()
                .zip(REFLECTION_ACCESSORS)
                .any(|(&target, name)| reflection_accessor_assignable(&*ctx, cid, target, name));
        REFLECTION_FRAME_TLS.with(|c| {
            let mut row = c.get();
            row.answer = Some((vm, cid, b));
            c.set(row);
        });
        Some(b)
    })();
    if let Some(b) = fast {
        return answer(b);
    }
    // WALK12-1 leaf half: the slow path loads classes and pins; not leaf-safe.
    if ctx.decline_leaf_call() {
        return Ok(None);
    }
    // The JDK expression on the real mirrors, through the same native.
    let c_pin = ctx.pin_native_root(c);
    for name in ["java/lang/reflect/Method", "java/lang/reflect/Constructor"] {
        let mirror = match ctx.load_class(name) {
            Ok(Some(Value::Object(Some(m)))) => m,
            Ok(_) => continue,
            Err(error) => {
                ctx.unpin_native_roots(c_pin);
                return Err(error);
            }
        };
        if mirror == ctx.read_native_pin(c_pin, c) {
            ctx.unpin_native_roots(c_pin);
            return answer(true);
        }
    }
    for name in REFLECTION_ACCESSORS {
        let accessor = match ctx.load_class(name) {
            Ok(Some(Value::Object(Some(m)))) => m,
            Ok(_) => continue,
            Err(error) => {
                ctx.unpin_native_roots(c_pin);
                return Err(error);
            }
        };
        let c = ctx.read_native_pin(c_pin, c);
        match crate::lang_class::native_class_is_assignable_from(
            ctx,
            &[Value::Object(Some(accessor)), Value::Object(Some(c))],
        ) {
            Ok(Some(Value::Int(0))) => {}
            Ok(_) => {
                ctx.unpin_native_roots(c_pin);
                return answer(true);
            }
            Err(error) => {
                ctx.unpin_native_roots(c_pin);
                return Err(error);
            }
        }
    }
    ctx.unpin_native_roots(c_pin);
    answer(false)
}

/// `native_class_is_assignable_from(I.class, c)` for a resolved, non-array
/// `c` (`source`) and a resolved interface `I` (`target`, internal name
/// `name`): that native's final expression, which is the only arm such a pair
/// reaches when its subclass hoist answers nothing.
fn reflection_accessor_assignable(
    ctx: &dyn NativeContext,
    source: cratonvm_types::ClassId,
    target: cratonvm_types::ClassId,
    name: &str,
) -> bool {
    source == target
        || ctx.is_subclass(source, target)
        || crate::lang_class::loader_aware_reflect_assignable(ctx, source, target, name)
        || ctx.synthetic_implements_declared(source, name)
}

/// The class [`native_stack_frame_buffer_fill`] is registered on.
const STACK_FRAME_BUFFER_CLASS: &str = "java/lang/StackStreamFactory$StackFrameBuffer";
/// `StackFrameBuffer.fill(T[], int, int)`, `T extends ClassFrameInfo` erased.
const STACK_FRAME_BUFFER_FILL_DESC: &str =
    "([Ljava/lang/ClassFrameInfo;II)[Ljava/lang/ClassFrameInfo;";

pub(crate) fn stackwalk_frame_layout_once_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_STACKWALK_FRAME_LAYOUT_ONCE")
    })
}

/// The per-walk allocators of THROWS6-1
/// ([`stackwalk_frame_layout_once_enabled`]): the `StackFrameInfo` layout and,
/// when it resolves, the `ResolvedMethodName` one.
pub(crate) struct WalkFrameAllocators {
    sfi: crate::util_concurrent_ext::ConcurrentSyntheticAllocator,
    /// `None` exactly where the per-frame `try_alloc_concurrent_synthetic(..)
    /// .ok()` answered nothing: the frame then holds the bare mirror.
    rmn: Option<crate::util_concurrent_ext::ConcurrentSyntheticAllocator>,
    /// WALK20-1 (`CRATONVM_STACKWALK_CLASS_KIT`): the walk's per-class kit.
    kit: Option<WalkClassKit>,
}

impl WalkFrameAllocators {
    /// The allocators of a walk over `frames` (screened, INNERMOST first):
    /// with WALK20-1 on ([`stackwalk_class_kit_enabled`]) the layouts are
    /// resolved once whatever THROWS6-1 says, plus the walk's
    /// [`WalkClassKit`]; otherwise exactly [`Self::for_walk`]. Allocates (the
    /// kit's array) and may run a `<clinit>`: the caller holds every reference
    /// it still needs pinned.
    pub(crate) fn for_walk_frames(
        ctx: &mut dyn NativeContext,
        frames: &[StackTraceEntry],
    ) -> Option<Self> {
        if !stackwalk_class_kit_enabled() {
            return Self::for_walk(ctx);
        }
        let mut allocs = Self::resolve(ctx)?;
        allocs.kit = WalkClassKit::for_walk(ctx, frames);
        Some(allocs)
    }

    /// The walk's allocators, or `None` (switch off, or the `StackFrameInfo`
    /// layout refused: every frame then takes the per-frame path, which
    /// answers that refusal itself). May run a `<clinit>` (Java): the caller
    /// holds every reference it still needs pinned, as for the per-frame
    /// funnel it replaces.
    pub(crate) fn for_walk(ctx: &mut dyn NativeContext) -> Option<Self> {
        if !stackwalk_frame_layout_once_enabled() {
            return None;
        }
        Self::resolve(ctx)
    }

    /// WALK21-10: move the kit's array into slot `slot` of the `Object[]`
    /// `holder` pinned at `holder_pin` (allocates nothing), so a kit built
    /// inside a `fetchStackFrames` outlives that native's pins.
    pub(crate) fn rehome_kit(
        &mut self,
        ctx: &mut dyn NativeContext,
        holder_pin: usize,
        holder: cratonvm_types::ObjectRef,
        slot: usize,
    ) {
        let Some(kit) = self.kit.as_mut() else {
            return;
        };
        let array = kit.array_now(ctx);
        let holder_now = ctx.read_native_pin(holder_pin, holder);
        ctx.set_array_element(holder_now, slot, Value::Object(Some(array)));
        kit.pin = holder_pin;
        kit.array = holder;
        kit.held_slot = Some(slot);
    }

    /// The two layouts, resolved now (THROWS6-1's body, shared with
    /// [`Self::for_walk_frames`]); no kit.
    fn resolve(ctx: &mut dyn NativeContext) -> Option<Self> {
        let sfi = crate::util_concurrent_ext::ConcurrentSyntheticAllocator::new(
            ctx,
            "java/lang/StackFrameInfo",
            STACK_FRAME_INFO_FIELDS,
        )
        .ok()?;
        let rmn = crate::util_concurrent_ext::ConcurrentSyntheticAllocator::new(
            ctx,
            "java/lang/invoke/ResolvedMethodName",
            1,
        )
        .ok();
        Some(Self {
            sfi,
            rmn,
            kit: None,
        })
    }
}

// ---------------------------------------------------------------------------
// WALK20-1: the per-walk class kit (JIT round 20 wave 18, lane xWalk)
// ---------------------------------------------------------------------------

/// `CRATONVM_STACKWALK_CLASS_KIT` (WALK20-1, JIT round 20 wave 18, lane xWalk;
/// default ON, `=0` restores the per-frame path): a native walk whose frames
/// go through [`populate_sfi_from_walk_lines`] resolves the `StackFrameInfo`
/// / `ResolvedMethodName` layouts once ([`WalkFrameAllocators`], THROWS6-1's
/// resolution, whatever that switch says), and keeps one [`WalkClassKit`]: per
/// distinct class of the walk, the class-store answer, the mirror and ONE
/// `ResolvedMethodName` that every frame of that class stores. Per frame that
/// takes away the second object allocation, the mirror lookup, the
/// class-store ask (and its `String`), the by-name `vmholder` store and the
/// two per-object layout resolutions (a by-name class lookup, an
/// initialization check and a `String` each). The walk's screen also takes
/// the capture by value instead of cloning every entry
/// (`reflect_invoke::walk_frames_screened_owned`). Read once per process.
///
/// Why sharing the `ResolvedMethodName` is exact: on this VM it holds one
/// field, `vmholder` (the class), written once at creation and only read
/// after (`StackFrameInfo.declaringClass()` -> `JLIA.getDeclaringClass`, and
/// the natives' `vmholder` reads); HotSpot itself interns one per `Method*`
/// (`ResolvedMethodTable`), so frames of one method already share it there;
/// `classOrMemberName` is private to `java.lang` and the class is
/// package-private to `java.lang.invoke`, so no program can tell two frames'
/// holders apart.
pub(crate) fn stackwalk_class_kit_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_STACKWALK_CLASS_KIT")
    })
}

/// A [`WalkClassKit`] class's state: not seen yet.
const KIT_UNSET: u8 = 0;
/// The class's mirror and holder are in the kit's array.
const KIT_FILLED: u8 = 1;
/// The class store does not hold the class (a lambda proxy's synthetic id):
/// every frame of it takes [`populate_sfi`], as without the kit.
const KIT_UNKNOWN_CLASS: u8 = 2;
/// Kit array slots per class: the mirror, the `ResolvedMethodName` (null when
/// the walk's frames hold the bare mirror).
const KIT_WIDTH: usize = 2;

/// `CRATONVM_STACKWALK_KIT_METHOD_NAMES` (WALKTHROW21-3, JIT round 21 lane
/// walkthrow2, ported onto dev 2026-10-26; default ON, `=0` interns per frame
/// as before): the WALK20-1 class kit also keeps one slot per distinct
/// method-name text of the walk's covered frames, holding the interned
/// `String` the first frame with that name interned, and every later frame
/// with that name stores it instead of interning the name again (a hash of
/// the name and a lookup in the global string table per frame). A deep
/// recursion interns once per distinct method per walk. (The round-21 design
/// kept the LAST name per class, which a recursion alternating methods of one
/// class replaced on every frame.) The same object: the intern of a text is
/// canonical, so every frame holds what it held before. Read once per walk
/// (with the kit).
pub(crate) fn stackwalk_kit_method_names_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_STACKWALK_KIT_METHOD_NAMES")
}

/// WALK20-1's per-walk class kit ([`stackwalk_class_kit_enabled`]): each
/// distinct class id of the walk's first [`WALK_LINES_MAX_FRAMES`] screened
/// frames (every frame under WALK21-1, [`stackwalk_deep_walk_lines_enabled`])
/// owns [`KIT_WIDTH`] slots of one pinned `Object[]`, filled by the
/// first frame of that class [`populate_sfi_from_walk_lines`] builds. Pinned
/// at the walk's set-up above the walk's base pin (released with it); the GC
/// traces and moves the slots as array elements. Per walk, per thread: no
/// process state.
pub(crate) struct WalkClassKit {
    pin: usize,
    array: cratonvm_types::ObjectRef,
    /// Per screened frame: its class's key, or `u32::MAX` (no class id).
    key_of: Vec<u32>,
    state: std::cell::RefCell<Vec<u8>>,
    /// `ResolvedMethodName.vmholder`, resolved once.
    vmholder: Option<usize>,
    /// WALK21-10: `Some(slot)` when `pin` / `array` are a walk's holder and
    /// the kit's array is its element `slot` ([`WalkFrameAllocators::rehome_kit`]).
    held_slot: Option<usize>,
    /// WALKTHROW21-3: per covered frame, its method name's slot key
    /// (`u32::MAX`: none); empty without the name slots.
    name_key_of: Vec<u32>,
    /// WALKTHROW21-3: the array index of name slot 0 (after the class slots).
    names_at: usize,
    /// WALKTHROW21-3: per name key, whether its slot holds the interned name.
    name_filled: std::cell::RefCell<Vec<bool>>,
}

impl WalkClassKit {
    /// The kit's array at its current address (allocates nothing).
    fn array_now(&self, ctx: &mut dyn NativeContext) -> cratonvm_types::ObjectRef {
        let array = ctx.read_native_pin(self.pin, self.array);
        match self.held_slot {
            Some(slot) => match ctx.get_array_element(array, slot) {
                Value::Object(Some(kit)) => kit,
                _ => array,
            },
            None => array,
        }
    }

    /// The kit over `frames`, or `None` (no frame with a class id, or the
    /// array refused: every frame then builds its own holder). Allocates.
    fn for_walk(ctx: &mut dyn NativeContext, frames: &[StackTraceEntry]) -> Option<Self> {
        // FxHash: a per-walk table of class ids (SipHash and its `RandomState`
        // were ~3% of a short walk).
        let mut keys: rustc_hash::FxHashMap<u32, u32> = rustc_hash::FxHashMap::default();
        // WALK21-1: a deep walk's frames past the first 128 take the lines
        // build too, so the kit covers them.
        let covered = if stackwalk_deep_walk_lines_enabled() {
            usize::MAX
        } else {
            WALK_LINES_MAX_FRAMES
        };
        let key_of: Vec<u32> = frames
            .iter()
            .take(covered)
            .map(|entry| match entry.class_id {
                Some(cid) => {
                    // Cast: at most one key per frame of a walk.
                    let next = keys.len() as u32;
                    *keys.entry(cid.as_u32()).or_insert(next)
                }
                None => u32::MAX,
            })
            .collect();
        let distinct = keys.len();
        if distinct == 0 {
            return None;
        }
        // WALKTHROW21-3: one name slot per distinct method-name text of the
        // frames the kit covers (frames without a class take no slot).
        let mut name_keys: rustc_hash::FxHashMap<&str, u32> = rustc_hash::FxHashMap::default();
        let name_key_of: Vec<u32> = if stackwalk_kit_method_names_enabled() {
            frames
                .iter()
                .zip(&key_of)
                .map(|(entry, &class_key)| {
                    if class_key == u32::MAX {
                        return u32::MAX;
                    }
                    // Cast: at most one key per frame of a walk.
                    let next = name_keys.len() as u32;
                    *name_keys.entry(&*entry.method_name).or_insert(next)
                })
                .collect()
        } else {
            Vec::new()
        };
        let names = name_keys.len();
        let names_at = distinct.checked_mul(KIT_WIDTH)?;
        let object = ctx.class_id_by_name("java/lang/Object")?;
        let array = ctx.try_new_ref_array(object, names_at.checked_add(names)?)?;
        let pin = ctx.pin_native_root(array);
        let vmholder = ctx.resolve_field_index("java/lang/invoke/ResolvedMethodName", "vmholder");
        Some(Self {
            pin,
            array,
            key_of,
            state: std::cell::RefCell::new(vec![KIT_UNSET; distinct]),
            vmholder,
            held_slot: None,
            name_key_of,
            names_at,
            name_filled: std::cell::RefCell::new(vec![false; names]),
        })
    }

    /// WALKTHROW21-3: frame `index`'s name key, if the kit keeps names.
    fn name_key(&self, index: usize) -> Option<usize> {
        match self.name_key_of.get(index) {
            // Cast: a `u32` key index.
            Some(&key) if key != u32::MAX => Some(key as usize),
            _ => None,
        }
    }

    /// WALKTHROW21-3: the interned method name of frame `index` at its
    /// current address, when an earlier frame with the same name stored it
    /// (allocates nothing); `None` otherwise. Keyed by the name's text: the
    /// intern of a text is canonical, whatever frame asked.
    fn load_method_name(
        &self,
        ctx: &mut dyn NativeContext,
        index: usize,
    ) -> Option<cratonvm_types::ObjectRef> {
        let key = self.name_key(index)?;
        if !self.name_filled.borrow().get(key).copied().unwrap_or(false) {
            return None;
        }
        let array = self.array_now(ctx);
        match ctx.get_array_element(array, self.names_at + key) {
            Value::Object(Some(name)) => Some(name),
            _ => None,
        }
    }

    /// WALKTHROW21-3: keep `name` (current address, the interned method name
    /// of frame `index`) in its name slot. Allocates nothing; a no-op without
    /// name slots, or when the slot is already filled.
    fn store_method_name(
        &self,
        ctx: &mut dyn NativeContext,
        index: usize,
        name: cratonvm_types::ObjectRef,
    ) {
        let Some(key) = self.name_key(index) else {
            return;
        };
        let mut filled = self.name_filled.borrow_mut();
        let Some(slot) = filled.get_mut(key) else {
            return;
        };
        if *slot {
            return;
        }
        let array = self.array_now(ctx);
        ctx.set_array_element(array, self.names_at + key, Value::Object(Some(name)));
        *slot = true;
    }

    /// Frame `index`'s class key and its state, or `None` (no class id, or
    /// past the kit's frames).
    fn key(&self, index: usize) -> Option<(usize, u8)> {
        let key = *self.key_of.get(index)?;
        if key == u32::MAX {
            return None;
        }
        // Cast: a `u32` key index.
        let key = key as usize;
        let state = self.state.borrow().get(key).copied()?;
        Some((key, state))
    }

    fn set_state(&self, key: usize, state: u8) {
        if let Some(s) = self.state.borrow_mut().get_mut(key) {
            *s = state;
        }
    }

    /// Store `key`'s mirror and holder (current addresses; allocates
    /// nothing).
    fn store(
        &self,
        ctx: &mut dyn NativeContext,
        key: usize,
        mirror: cratonvm_types::ObjectRef,
        rmn: Option<cratonvm_types::ObjectRef>,
    ) {
        let array = self.array_now(ctx);
        let at = key * KIT_WIDTH;
        ctx.set_array_element(array, at, Value::Object(Some(mirror)));
        ctx.set_array_element(array, at + 1, Value::Object(rmn));
        self.set_state(key, KIT_FILLED);
    }

    /// `key`'s mirror and holder at their current addresses (allocates
    /// nothing); `None` when the mirror slot is empty.
    fn load(
        &self,
        ctx: &mut dyn NativeContext,
        key: usize,
    ) -> Option<(cratonvm_types::ObjectRef, Option<cratonvm_types::ObjectRef>)> {
        let array = self.array_now(ctx);
        let at = key * KIT_WIDTH;
        // Both slots through one read region (one validation of the array,
        // nothing allocates between the two reads).
        let (mirror, rmn) = match ctx.read_cursor(array) {
            Some(cursor) => (
                ctx.cursor_array_element(&cursor, at),
                ctx.cursor_array_element(&cursor, at + 1),
            ),
            None => (
                ctx.get_array_element(array, at),
                ctx.get_array_element(array, at + 1),
            ),
        };
        let Value::Object(Some(mirror)) = mirror else {
            return None;
        };
        let Value::Object(rmn) = rmn else {
            return None;
        };
        Some((mirror, rmn))
    }
}

// ---------------------------------------------------------------------------
// WALK9-1 (walk-frame half): the per-walk name / mirror memo (JIT round 19
// wave 11, lane walk11)
// ---------------------------------------------------------------------------

/// `CRATONVM_STACKWALK_FRAME_MEMO=1` (WALK9-1, JIT round 19 wave 11, lane
/// walk11; opt-in, default OFF): a native walk's
/// [`populate_sfi_from_walk_lines`] takes each frame's method name, its
/// descriptor (a `RETAIN_CLASS_REFERENCE` walk) and its class mirror from a
/// per-walk [`WalkFrameMemo`] after the first frame of the same (class,
/// method, descriptor), instead of interning both names, materialising the
/// mirror and asking the class store per frame, and stores
/// `ResolvedMethodName.vmholder` by a resolved index instead of by name. The
/// interned strings and the mirror are canonical, so every frame holds the
/// same objects either way. Read once per process.
pub(crate) fn stackwalk_frame_memo_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_STACKWALK_FRAME_MEMO"))
}

/// `CRATONVM_STACKWALK_PIPELINE_DIRECT_STORE=1` (WALK9-2, JIT round 19 wave
/// 11, lane walk11; opt-in, default OFF): a pipeline walk's first batch
/// (`phases_late::reflect_invoke::p59_sw_walk_jdk_pipeline`) and its later
/// batches ([`pipeline_batch_fetch`]) store each frame the builder returns
/// straight into the pinned buffer array. The builder answers its frame read
/// back through its own pins after its last allocation, and nothing between
/// that answer and the store allocates, so the frame needs no pin of its own
/// (the default arm pins it, re-reads both and truncates per frame). Read
/// once per process.
pub(crate) fn stackwalk_pipeline_direct_store_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_STACKWALK_PIPELINE_DIRECT_STORE")
    })
}

/// A [`WalkFrameMemo`] key's state: nothing stored yet.
const MEMO_UNSET: u8 = 0;
/// The key's names and mirror are in the memo array.
const MEMO_FILLED: u8 = 1;
/// The key's class is not in the class store (a lambda proxy's synthetic id):
/// every frame of it takes [`populate_sfi`], as without the memo.
const MEMO_UNKNOWN_CLASS: u8 = 2;
/// Memo array slots per key: the method name, the descriptor (null when the
/// walk stores none), the class mirror.
const MEMO_WIDTH: usize = 3;

/// The per-walk memo of WALK9-1 ([`stackwalk_frame_memo_enabled`]): each
/// distinct (class id, method name, stored descriptor) of the walk's screened
/// frames owns [`MEMO_WIDTH`] slots of one pinned `Object[]`, filled by the
/// first frame of that key that [`populate_sfi_from_walk_lines`] builds. The
/// array is pinned at the walk's set-up, above the walk's base pin, so the GC
/// keeps (and moves) the memoized objects like any other array element; the
/// walk's base truncation releases it. Per walk, per thread: no process state.
pub(crate) struct WalkFrameMemo {
    pin: usize,
    array: cratonvm_types::ObjectRef,
    /// Per screened frame (at most [`WALK_LINES_MAX_FRAMES`]): its key, or
    /// `u32::MAX` for a frame with no class id.
    key_of: Vec<u32>,
    state: std::cell::RefCell<Vec<u8>>,
    /// `ResolvedMethodName.vmholder`, resolved once.
    vmholder: Option<usize>,
    /// WALK11-3 (`CRATONVM_STACKWALK_CLASS_FRAME_MEMO`): a `DROP_METHOD_INFO`
    /// walk's `ClassFrameInfo` layout, resolved once; such a memo is keyed by
    /// class only and uses the mirror slot.
    class_frame: Option<ClassFrameLayout>,
}

/// WALK11-3: the `ClassFrameInfo` layout a `DROP_METHOD_INFO` walk's memo
/// resolves once: its allocator and the two fields
/// [`populate_class_frame_info`] stores.
struct ClassFrameLayout {
    alloc: crate::util_concurrent_ext::ConcurrentSyntheticAllocator,
    class_or_member_name: usize,
    flags: usize,
}

/// `CRATONVM_STACKWALK_CLASS_FRAME_MEMO=1` (WALK11-3, JIT round 19 wave 12,
/// lane walk12; opt-in, default OFF): a native `DROP_METHOD_INFO` walk's
/// [`populate_class_frame_info`] takes each frame's mirror from a per-walk
/// [`WalkFrameMemo`] keyed by class (materialised once per class), allocates
/// the `ClassFrameInfo` through a layout resolved once per walk, and stores
/// its two fields by resolved index instead of by name. The same objects.
/// Read once per process.
pub(crate) fn stackwalk_class_frame_memo_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_STACKWALK_CLASS_FRAME_MEMO")
    })
}

impl WalkFrameMemo {
    /// The walk's memo, or `None` (switch off, no frame with a class id, or
    /// the array refused: every frame then takes the path without it). Only
    /// for a walk whose frames go through [`populate_sfi_from_walk_lines`]
    /// (`walk_lines`); allocates (the array), so the caller holds every
    /// reference it still needs pinned.
    pub(crate) fn for_walk(
        ctx: &mut dyn NativeContext,
        frames: &[StackTraceEntry],
        retain_class_ref: bool,
        walk_lines: bool,
    ) -> Option<Self> {
        if !walk_lines || !stackwalk_frame_memo_enabled() {
            return None;
        }
        let mut memo = Self::keyed(ctx, frames, WALK_LINES_MAX_FRAMES, |entry| {
            // The descriptor is part of the key exactly when the frame
            // stores one (`populate_sfi_from_walk_lines`).
            let desc = entry
                .method_descriptor
                .as_deref()
                .filter(|_| retain_class_ref);
            (&*entry.method_name, desc)
        })?;
        memo.vmholder = ctx.resolve_field_index("java/lang/invoke/ResolvedMethodName", "vmholder");
        Some(memo)
    }

    /// WALK11-3: a `DROP_METHOD_INFO` walk's memo (`drop_method_info`, its
    /// frames built by [`populate_class_frame_info`]), keyed by class; `None`
    /// with the switch off, no frame with a class id, or a layout refused.
    /// Allocates (the array) and may run a `<clinit>`: the caller holds every
    /// reference it still needs pinned.
    pub(crate) fn for_class_frame_walk(
        ctx: &mut dyn NativeContext,
        frames: &[StackTraceEntry],
        drop_method_info: bool,
    ) -> Option<Self> {
        if !drop_method_info || !stackwalk_class_frame_memo_enabled() {
            return None;
        }
        let alloc = crate::util_concurrent_ext::ConcurrentSyntheticAllocator::new(
            ctx,
            "java/lang/ClassFrameInfo",
            2,
        )
        .ok()?;
        let class_or_member_name =
            ctx.resolve_field_index("java/lang/ClassFrameInfo", "classOrMemberName")?;
        let flags = ctx.resolve_field_index("java/lang/ClassFrameInfo", "flags")?;
        let mut memo = Self::keyed(ctx, frames, frames.len(), |_| ("", None))?;
        memo.class_frame = Some(ClassFrameLayout {
            alloc,
            class_or_member_name,
            flags,
        });
        Some(memo)
    }

    /// A memo over the first `limit` of `frames`, keyed by class id and
    /// `extra(entry)`; its array pinned. `None` when no frame has a class id
    /// or the array is refused.
    fn keyed<'f>(
        ctx: &mut dyn NativeContext,
        frames: &'f [StackTraceEntry],
        limit: usize,
        extra: impl Fn(&'f StackTraceEntry) -> (&'f str, Option<&'f str>),
    ) -> Option<Self> {
        let mut keys: std::collections::HashMap<(u32, &str, Option<&str>), u32> =
            std::collections::HashMap::new();
        let key_of: Vec<u32> = frames
            .iter()
            .take(limit)
            .map(|entry| match entry.class_id {
                Some(cid) => {
                    let (method, desc) = extra(entry);
                    // Cast: at most one key per frame of a walk.
                    let next = keys.len() as u32;
                    *keys.entry((cid.as_u32(), method, desc)).or_insert(next)
                }
                None => u32::MAX,
            })
            .collect();
        let distinct = keys.len();
        if distinct == 0 {
            return None;
        }
        let object = ctx.class_id_by_name("java/lang/Object")?;
        let array = ctx.try_new_ref_array(object, distinct.checked_mul(MEMO_WIDTH)?)?;
        let pin = ctx.pin_native_root(array);
        Some(Self {
            pin,
            array,
            key_of,
            state: std::cell::RefCell::new(vec![MEMO_UNSET; distinct]),
            vmholder: None,
            class_frame: None,
        })
    }

    /// WALK11-3: store `key`'s mirror (current address; allocates nothing).
    fn store_mirror(
        &self,
        ctx: &mut dyn NativeContext,
        key: usize,
        mirror: cratonvm_types::ObjectRef,
    ) {
        let array = ctx.read_native_pin(self.pin, self.array);
        ctx.set_array_element(array, key * MEMO_WIDTH + 2, Value::Object(Some(mirror)));
        self.set_state(key, MEMO_FILLED);
    }

    /// WALK11-3: `key`'s mirror at its current address (allocates nothing).
    fn load_mirror(
        &self,
        ctx: &mut dyn NativeContext,
        key: usize,
    ) -> Option<cratonvm_types::ObjectRef> {
        let array = ctx.read_native_pin(self.pin, self.array);
        match ctx.get_array_element(array, key * MEMO_WIDTH + 2) {
            Value::Object(Some(mirror)) => Some(mirror),
            _ => None,
        }
    }

    /// Frame `index`'s key and its state, or `None` (no class id, or past
    /// the memo's frames).
    fn key(&self, index: usize) -> Option<(usize, u8)> {
        let key = *self.key_of.get(index)?;
        if key == u32::MAX {
            return None;
        }
        // Cast: a `u32` key index.
        let key = key as usize;
        let state = self.state.borrow().get(key).copied()?;
        Some((key, state))
    }

    fn set_state(&self, key: usize, state: u8) {
        if let Some(s) = self.state.borrow_mut().get_mut(key) {
            *s = state;
        }
    }

    /// Store `key`'s names and mirror (current addresses; allocates nothing).
    fn store(
        &self,
        ctx: &mut dyn NativeContext,
        key: usize,
        method: cratonvm_types::ObjectRef,
        desc: Option<cratonvm_types::ObjectRef>,
        mirror: cratonvm_types::ObjectRef,
    ) {
        let array = ctx.read_native_pin(self.pin, self.array);
        let at = key * MEMO_WIDTH;
        ctx.set_array_element(array, at, Value::Object(Some(method)));
        ctx.set_array_element(array, at + 1, Value::Object(desc));
        ctx.set_array_element(array, at + 2, Value::Object(Some(mirror)));
        self.set_state(key, MEMO_FILLED);
    }

    /// `key`'s names and mirror at their current addresses (allocates
    /// nothing); `None` when a slot that must hold one is empty.
    fn load(
        &self,
        ctx: &mut dyn NativeContext,
        key: usize,
    ) -> Option<(
        cratonvm_types::ObjectRef,
        Option<cratonvm_types::ObjectRef>,
        cratonvm_types::ObjectRef,
    )> {
        let array = ctx.read_native_pin(self.pin, self.array);
        let at = key * MEMO_WIDTH;
        let Value::Object(Some(method)) = ctx.get_array_element(array, at) else {
            return None;
        };
        let desc = match ctx.get_array_element(array, at + 1) {
            Value::Object(desc) => desc,
            _ => return None,
        };
        let Value::Object(Some(mirror)) = ctx.get_array_element(array, at + 2) else {
            return None;
        };
        Some((method, desc, mirror))
    }
}

// ---------------------------------------------------------------------------
// THROWS6-2: the walk / element section census (JIT round 19 wave 6)
// ---------------------------------------------------------------------------

/// `CRATONVM_DBG_WALK_SECTIONS=1` (THROWS6-2, diagnostic, default OFF): the
/// native `StackWalker.walk` and `StackTraceElement.initStackTraceElements`
/// time their sections and print per-thread averages to stderr after the
/// first call and every 512th (`[walk-sections]` / `[ste-sections]`). It
/// decomposes the rows `R19Throws4Compiled walk-first-deep`,
/// `get-stack-trace-deep` and `throwable-get-trace` (20-25x / 10x HotSpot)
/// into capture, screen, frame build and the function's own run, and tells
/// the element native's share of `getStackTrace()` from the JDK bytecode
/// around it (`StackTraceElement.of`, `computeFormat`). Nothing else changes.
/// WALK8-2 (wave 9, lane walk9): the `[ste-sections]` line also splits the
/// element build per element ([`SteSub`]). Read once per process.
pub(crate) fn walk_sections_census_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_WALK_SECTIONS"))
}

/// One thread's running totals for a [`walk_sections_census_enabled`] kind.
#[derive(Clone, Copy, Default)]
struct SectionTotals {
    calls: u64,
    items: u64,
    ns: [u64; 4],
    /// THROWS5-3: calls whose frames were built lazily, and the frames they
    /// built (`built/lazy-call` against `items/call`: materialised vs listed).
    lazy_calls: u64,
    built: u64,
    /// WALK20-1's census (lane xWalk): a batched pipeline walk's later
    /// batches, the nanoseconds spent building their frames (inside `apply`)
    /// and the frames they built.
    batch_ns: u64,
    batch_frames: u64,
}

/// A thread's totals before its first call.
const SECTION_TOTALS_ZERO: SectionTotals = SectionTotals {
    calls: 0,
    items: 0,
    ns: [0; 4],
    lazy_calls: 0,
    built: 0,
    batch_ns: 0,
    batch_frames: 0,
};

thread_local! {
    static WALK_SECTION_TOTALS: std::cell::Cell<SectionTotals> =
        const { std::cell::Cell::new(SECTION_TOTALS_ZERO) };
    static STE_SECTION_TOTALS: std::cell::Cell<SectionTotals> =
        const { std::cell::Cell::new(SECTION_TOTALS_ZERO) };
}

/// Which census a [`note_sections`] call adds to.
#[derive(Clone, Copy)]
pub(crate) enum SectionKind {
    /// `p59_sw_walk`: capture, screen (+ lines), frame build, the function.
    Walk,
    /// `initStackTraceElements`: the trace lookup, the element build, -, -.
    Elements,
}

/// Add one call's section times (`marks`: the instants at the call's start
/// and after each section; unused trailing sections repeat the last mark) and
/// its item count (frames / elements); print the thread's averages after the
/// first call and every 512th.
#[cold]
pub(crate) fn note_sections(kind: SectionKind, items: usize, marks: &[std::time::Instant; 5]) {
    note_sections_built(kind, items, None, marks);
}

/// [`note_sections`] for a walk whose frames were built lazily (THROWS5-3):
/// `built` frames of `items` were materialised by the time the function
/// returned. The `build` section is then only the array's allocation and the
/// frames' cost moved into `apply`.
#[cold]
pub(crate) fn note_sections_built(
    kind: SectionKind,
    items: usize,
    built: Option<usize>,
    marks: &[std::time::Instant; 5],
) {
    note_sections_with(kind, items, built, None, marks);
}

/// [`note_sections`] for a batched pipeline walk (WALK8-1): `batch` is
/// `(nanoseconds, frames)` its later batches spent building frames inside
/// `apply` ([`PipelineBatchGuard::batch_census`]). The line then adds
/// `batch-build=` (per call) and `batch-frame=` (per frame built), so `build
/// + batch-build` is every frame's native build and `apply - batch-build` the
/// JDK pipeline's own run. JIT round 20 wave 18, lane xWalk.
#[cold]
pub(crate) fn note_sections_batched(
    kind: SectionKind,
    items: usize,
    batch: Option<(u64, u64)>,
    marks: &[std::time::Instant; 5],
) {
    note_sections_with(kind, items, None, batch, marks);
}

#[cold]
fn note_sections_with(
    kind: SectionKind,
    items: usize,
    built: Option<usize>,
    batch: Option<(u64, u64)>,
    marks: &[std::time::Instant; 5],
) {
    let (cell, tag, labels) = match kind {
        SectionKind::Walk => (
            &WALK_SECTION_TOTALS,
            "walk-sections",
            ["capture", "screen", "build", "apply"],
        ),
        SectionKind::Elements => (
            &STE_SECTION_TOTALS,
            "ste-sections",
            ["lookup", "build", "-", "-"],
        ),
    };
    cell.with(|c| {
        let mut t = c.get();
        t.calls += 1;
        // Cast: a frame count.
        t.items += items as u64;
        if let Some(built) = built {
            t.lazy_calls += 1;
            // Cast: a frame count.
            t.built += built as u64;
        }
        if let Some((ns, frames)) = batch {
            t.batch_ns += ns;
            t.batch_frames += frames;
        }
        for (i, slot) in t.ns.iter_mut().enumerate() {
            // Cast: nanoseconds of one section of one call.
            *slot += marks[i + 1].saturating_duration_since(marks[i]).as_nanos() as u64;
        }
        c.set(t);
        if t.calls == 1 || t.calls % 512 == 0 {
            let per = |v: u64| v / t.calls.max(1);
            let lazy = if t.lazy_calls == 0 {
                String::new()
            } else {
                format!(
                    " lazy-calls={} built/lazy-call={}",
                    t.lazy_calls,
                    t.built / t.lazy_calls
                )
            };
            let sub = match kind {
                SectionKind::Elements => ste_sub_line(t.items),
                SectionKind::Walk if t.batch_frames > 0 => format!(
                    " batch-build={} ns batch-frame={} ns",
                    per(t.batch_ns),
                    t.batch_ns / t.batch_frames
                ),
                SectionKind::Walk => String::new(),
            };
            eprintln!(
                "[{tag}] calls={} items/call={} {}={} ns {}={} ns {}={} ns {}={} ns total={} ns{lazy}{sub}",
                t.calls,
                per(t.items),
                labels[0],
                per(t.ns[0]),
                labels[1],
                per(t.ns[1]),
                labels[2],
                per(t.ns[2]),
                labels[3],
                per(t.ns[3]),
                per(t.ns.iter().sum()),
            );
        }
    });
}

// ---------------------------------------------------------------------------
// WALK21-7: the per-frame split of a pipeline walk, per VM and depth (JIT
// round 21 wave 1, lane walk)
// ---------------------------------------------------------------------------

/// WALK21-7 (under `CRATONVM_DBG_WALK_SECTIONS=1`, diagnostic): one key's
/// running totals of batched pipeline walks -- every walk of one VM listing
/// the same number of frames (a probe row), so one run of a probe with rows
/// at several depths splits each row's cost, where `[walk-sections]` averages
/// every walk of the thread together. Per frame LISTED: capture and screen
/// (paid for every frame whatever the function reads); per frame BUILT: the
/// first batch's build and the later batches' (`fetchStackFrames`); per frame
/// CONSUMED (built): the rest of `apply`, i.e. the JDK pipeline's Java, the
/// function and the fetch round trips.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct WalkFrameSplit {
    calls: u64,
    listed: u64,
    first_built: u64,
    batch_built: u64,
    capture_ns: u64,
    screen_ns: u64,
    first_build_ns: u64,
    apply_ns: u64,
    batch_ns: u64,
}

impl WalkFrameSplit {
    /// Add one walk: `listed` frames, `first_built` built by its first batch,
    /// `(batch_ns, batch_built)` by the later ones, and its four section
    /// times `[capture, screen, first build, apply]` in nanoseconds.
    pub(crate) fn add(
        &mut self,
        listed: usize,
        first_built: usize,
        batch: Option<(u64, u64)>,
        sections: [u64; 4],
    ) {
        let (batch_ns, batch_built) = batch.unwrap_or((0, 0));
        self.calls += 1;
        // Casts: frame counts.
        self.listed += listed as u64;
        self.first_built += first_built as u64;
        self.batch_built += batch_built;
        self.capture_ns += sections[0];
        self.screen_ns += sections[1];
        self.first_build_ns += sections[2];
        self.apply_ns += sections[3];
        self.batch_ns += batch_ns;
    }

    /// The census line for VM `vm` and walks of `frames` frames.
    pub(crate) fn line(&self, vm: usize, frames: usize) -> String {
        let div = |v: u64, n: u64| v / n.max(1);
        let consumed = self.first_built + self.batch_built;
        let total = self.capture_ns + self.screen_ns + self.first_build_ns + self.apply_ns;
        format!(
            "[walk-frame-split] vm={vm:#x} frames={frames} calls={} built/call={} \
             capture/frame={} ns screen/frame={} ns first-build/frame={} ns \
             batch-build/frame={} ns java+function/frame={} ns per-walk={} ns",
            self.calls,
            div(consumed, self.calls),
            div(self.capture_ns, self.listed),
            div(self.screen_ns, self.listed),
            div(self.first_build_ns, self.first_built),
            div(self.batch_ns, self.batch_built),
            div(self.apply_ns.saturating_sub(self.batch_ns), consumed),
            div(total, self.calls),
        )
    }
}

/// The most (VM, depth) keys WALK21-7 keeps per thread; a key past them is
/// not counted (a census, not a profile).
const WALK_FRAME_SPLIT_KEYS: usize = 32;

thread_local! {
    /// WALK21-7's totals on this thread, per (VM identity, frames listed).
    /// Per thread, per VM: no process state.
    static WALK_FRAME_SPLITS: std::cell::RefCell<Vec<(usize, usize, WalkFrameSplit)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// WALK21-7: add one batched pipeline walk of VM `vm` (`frames` listed,
/// `first_built` built by its first batch, `batch` its later batches'
/// census, `marks` its section instants as [`note_sections_batched`] takes
/// them) and print its key's line after the key's first walk and every
/// 512th.
#[cold]
pub(crate) fn note_walk_frame_split(
    vm: usize,
    frames: usize,
    first_built: usize,
    batch: Option<(u64, u64)>,
    marks: &[std::time::Instant; 5],
) {
    let mut sections = [0u64; 4];
    for (i, slot) in sections.iter_mut().enumerate() {
        // Cast: nanoseconds of one section of one walk.
        *slot = marks[i + 1].saturating_duration_since(marks[i]).as_nanos() as u64;
    }
    let line = WALK_FRAME_SPLITS.with(|splits| {
        let mut splits = splits.borrow_mut();
        let at = match splits.iter().position(|(v, f, _)| *v == vm && *f == frames) {
            Some(at) => at,
            None if splits.len() < WALK_FRAME_SPLIT_KEYS => {
                splits.push((vm, frames, WalkFrameSplit::default()));
                splits.len() - 1
            }
            None => return None,
        };
        let split = &mut splits[at].2;
        split.add(frames, first_built, batch, sections);
        (split.calls == 1 || split.calls % 512 == 0).then(|| split.line(vm, frames))
    });
    if let Some(line) = line {
        eprintln!("{line}");
    }
}

// ---------------------------------------------------------------------------
// WALK8-2: the `[ste-sections] build` term, decomposed (JIT round 19 wave 9)
// ---------------------------------------------------------------------------

/// One part of an element's build in
/// `lang_misc::fill_stack_trace_elements_from_backtrace`, as
/// [`ste_sub_mark`] attributes the time since the previous mark.
#[derive(Clone, Copy)]
pub(crate) enum SteSub {
    /// The element's allocation (and its root), plus the array's allocator
    /// set-up on the first element.
    Alloc = 0,
    /// The `declaringClass` text (`captured_frame_dotted_name`).
    Name = 1,
    /// The per-VM `FrameOrigin` row: the lookup (lock + `Arc` clone) and,
    /// on a hit, its `resolve_global_root`s.
    Row = 2,
    /// The interns (method and file name; the class name off the row path).
    Intern = 3,
    /// `get_class_mirror` (and, off the row path, the frame class lookup).
    Mirror = 4,
    /// The field store(s) and the array store.
    Store = 5,
    /// Off the row path: the origin fields derived afresh (module, version,
    /// loader name, `format`) and the row recorded.
    Origin = 6,
}

/// [`SteSub`]'s labels, in its order.
const STE_SUB_LABELS: [&str; 7] = [
    "alloc", "name", "row", "intern", "mirror", "store", "origin",
];

/// A thread's running [`SteSub`] totals and the mark the next one is
/// measured from (`None`: no element build in progress; a mark is a no-op).
#[derive(Clone, Copy)]
struct SteSubTotals {
    ns: [u64; 7],
    /// Elements filled from a `FrameOrigin` row.
    row_hits: u64,
    last: Option<std::time::Instant>,
}

thread_local! {
    static STE_SUB_TOTALS: std::cell::Cell<SteSubTotals> = const {
        std::cell::Cell::new(SteSubTotals { ns: [0; 7], row_hits: 0, last: None })
    };
}

/// WALK8-2 (`CRATONVM_DBG_WALK_SECTIONS`): an element build in progress,
/// from [`SteSubBuild::begin`] to its drop (every exit, `?` included). A
/// nested build (through Java) restores the outer one's mark on its drop.
pub(crate) struct SteSubBuild {
    previous: Option<std::time::Instant>,
}

impl SteSubBuild {
    /// A build starting at `at`.
    #[cold]
    pub(crate) fn begin(at: std::time::Instant) -> Self {
        let previous = STE_SUB_TOTALS.with(|c| {
            let mut t = c.get();
            let previous = t.last.replace(at);
            c.set(t);
            previous
        });
        Self { previous }
    }
}

impl Drop for SteSubBuild {
    fn drop(&mut self) {
        let previous = self.previous;
        STE_SUB_TOTALS.with(|c| {
            let mut t = c.get();
            t.last = previous;
            c.set(t);
        });
    }
}

/// WALK8-2: attribute the time since the previous mark to `sub`. A no-op with
/// the census off or outside an element build (the fills other natives make).
#[inline]
pub(crate) fn ste_sub_mark(sub: SteSub) {
    if walk_sections_census_enabled() {
        ste_sub_mark_slow(sub, false);
    }
}

/// [`ste_sub_mark`] for the end of an element filled from a `FrameOrigin`
/// row (counted).
#[inline]
pub(crate) fn ste_sub_mark_row_hit(sub: SteSub) {
    if walk_sections_census_enabled() {
        ste_sub_mark_slow(sub, true);
    }
}

#[cold]
fn ste_sub_mark_slow(sub: SteSub, row_hit: bool) {
    STE_SUB_TOTALS.with(|c| {
        let mut t = c.get();
        let Some(last) = t.last else {
            return;
        };
        let now = std::time::Instant::now();
        // Cast: nanoseconds of one part of one element.
        t.ns[sub as usize] += now.saturating_duration_since(last).as_nanos() as u64;
        t.row_hits += u64::from(row_hit);
        t.last = Some(now);
        c.set(t);
    });
}

/// The `[ste-sections]` line's WALK8-2 tail: each [`SteSub`] per element over
/// the thread's `items` elements so far.
fn ste_sub_line(items: u64) -> String {
    let t = STE_SUB_TOTALS.with(std::cell::Cell::get);
    let per = |v: u64| v / items.max(1);
    let mut out = String::from(" per-element:");
    for (label, ns) in STE_SUB_LABELS.iter().zip(t.ns) {
        out.push_str(&format!(" {label}={} ns", per(ns)));
    }
    out.push_str(&format!(" row-hits={}/{}", t.row_hits, items));
    out
}

/// [`populate_sfi`] for frame `index` of a walk whose lines `lines` (pinned at
/// `lines_pin`) [`walk_lines_for`] built: no element, no class-name or file
/// String; the frame keeps `lines` in its `contScope` slot and `index` in
/// `flags`. Falls back to [`populate_sfi`] for a frame with no class, an
/// index past [`WALK_LINES_MAX_FRAMES`], or a VM whose `StackFrameInfo`
/// layout is unknown.
///
/// `allocs` (THROWS6-1, [`WalkFrameAllocators`]): the walk's resolved
/// `StackFrameInfo` / `ResolvedMethodName` layouts, in place of a resolution
/// per object; `None` resolves per object as before.
///
/// `memo` (WALK9-1, [`WalkFrameMemo`]): the walk's interned names and
/// mirrors per (class, method, descriptor); `None` interns and materialises
/// per frame as before.
///
/// `walk_lines` ([`WalkLines`]): WALK21-1 (`CRATONVM_STACKWALK_DEEP_WALK_LINES`)
/// gives a deep walk one `int[]` per 128 frames; frame `index` keeps the one
/// of chunk `index / 128` and its index in it, `index % 128`. A frame whose
/// chunk the walk does not hold (the switch off: every index past the first
/// 128) falls back as before.
pub(crate) fn populate_sfi_from_walk_lines(
    ctx: &mut dyn NativeContext,
    entry: &StackTraceEntry,
    retain_class_ref: bool,
    walk_lines: WalkLines,
    index: usize,
    allocs: Option<&WalkFrameAllocators>,
    memo: Option<&WalkFrameMemo>,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    let layout = sfi_layout(ctx);
    let (Some(layout), Some(cid)) = (layout, entry.class_id) else {
        return populate_sfi(ctx, entry, retain_class_ref);
    };
    if !walk_lines.covers(ctx, index) || !ctx.sfi_lazy_element() {
        return populate_sfi(ctx, entry, retain_class_ref);
    }
    // The frame's index in its chunk's lines (the whole index below 128).
    let line_index = index % WALK_LINES_MAX_FRAMES;
    // WALK9-1 (`CRATONVM_STACKWALK_FRAME_MEMO`): the frame's memo key, and
    // whether an earlier frame of the same key filled it.
    let memo_key = memo.and_then(|m| m.key(index).map(|(key, state)| (m, key, state)));
    if matches!(memo_key, Some((_, _, MEMO_UNKNOWN_CLASS))) {
        return populate_sfi(ctx, entry, retain_class_ref);
    }
    let hit = matches!(memo_key, Some((_, _, MEMO_FILLED)));
    // WALK20-1 (`CRATONVM_STACKWALK_CLASS_KIT`): the frame's class kit key,
    // and whether an earlier frame of the same class filled it.
    let kit = allocs.and_then(|a| a.kit.as_ref());
    let kit_key = kit.and_then(|k| k.key(index).map(|(key, state)| (k, key, state)));
    if matches!(kit_key, Some((_, _, KIT_UNKNOWN_CLASS))) {
        return populate_sfi(ctx, entry, retain_class_ref);
    }
    let kit_hit = matches!(kit_key, Some((_, _, KIT_FILLED)));
    // Interpreter round i2 wave 58 hand-back (`L3W50LambdaProxyFrames`): a
    // frame whose class the class store does not hold -- a lambda proxy's
    // synthetic id -- cannot rebuild its element from its class at the first
    // read (`sfi_element_from_walk_lines` answers nothing, and the accessors
    // then read the legacy carrier slots: `line=0 file=applyAsInt` where
    // HotSpot answers `-1` / `null`). It keeps the eager element, built from
    // the walk entry. (A memo or kit hit's class was found by its key's first
    // frame; with the kit the store is asked without copying the name.)
    let in_store = hit
        || kit_hit
        || if kit.is_some() {
            ctx.class_name_arc_of_id(cid).is_some()
        } else {
            ctx.class_name_of_id(cid).is_some()
        };
    if !in_store {
        if let Some((m, key, _)) = memo_key {
            m.set_state(key, MEMO_UNKNOWN_CLASS);
        }
        if let Some((k, key, _)) = kit_key {
            k.set_state(key, KIT_UNKNOWN_CLASS);
        }
        return populate_sfi(ctx, entry, retain_class_ref);
    }
    if crate::nbflags().debug_stackwalk {
        eprintln!(
            "[SW-DBG] walk lines: frame {index} {}.{} line={} (no element)",
            entry.class_name, entry.method_name, entry.line_number
        );
    }
    // The names and the mirror: interned / materialised and pinned, unless
    // the memo holds them (then read after the allocations below). A kit hit
    // reads its class's mirror from the kit, after the allocations too.
    let fresh: Option<FreshFrameNames> = if hit {
        None
    } else {
        // WALKTHROW21-3 (`CRATONVM_STACKWALK_KIT_METHOD_NAMES`): the name an
        // earlier frame of the same method interned, when the kit keeps it
        // (its current address, pinned below before anything allocates).
        let kept_name = match kit_key {
            Some((k, _, _)) => k.load_method_name(ctx, index),
            None => None,
        };
        let meth_str = match kept_name {
            Some(name) => name,
            None => ctx.intern_string(&entry.method_name),
        };
        let meth_pin = ctx.pin_native_root(meth_str);
        let desc_pinned = match entry.method_descriptor.as_deref() {
            Some(desc) if retain_class_ref => {
                let s = ctx.intern_string(desc);
                Some((s, ctx.pin_native_root(s)))
            }
            _ => None,
        };
        let mirror_pinned = if kit_hit {
            None
        } else {
            let mirror = ctx.get_class_mirror(cid);
            Some((mirror, ctx.pin_native_root(mirror)))
        };
        Some(FreshFrameNames {
            meth_str,
            meth_pin,
            desc_pinned,
            mirror_pinned,
        })
    };
    let mut sf = match allocs {
        Some(allocs) => allocs.sfi.alloc(ctx),
        None => try_alloc_concurrent_synthetic(
            ctx,
            "java/lang/StackFrameInfo",
            STACK_FRAME_INFO_FIELDS,
        )?,
    };
    let h_sf = ctx.pin_native_root(sf);
    // The lowest pin this frame pushed: released on every exit below.
    let base = fresh.as_ref().map_or(h_sf, |f| f.meth_pin);
    // As `populate_sfi`: the `ResolvedMethodName` HotSpot's walk stores, for
    // every frame. Interpreter round i2 wave 60, lane L3: this copy still
    // minted it under the pure-bytecode lever only, so every `--jdk-only`
    // walk under `CRATONVM_SFI_WALK_LINES` stored the bare `Class`, and the
    // JDK's own `StackFrameInfo.declaringClass()` (its native override
    // retired by SBR3 `refmisc`) failed its `ResolvedMethodName` cast:
    // `getClassName()` raised `ClassCastException` (`L3W43ReflectionFrames`
    // `walk-*` rows, `L3W60WalkDeclaringClass`). WALK20-1: a kit hit stores
    // its class's holder instead of minting one.
    let rmn = if kit_hit {
        None
    } else {
        match allocs {
            Some(allocs) => allocs.rmn.as_ref().map(|rmn| rmn.alloc(ctx)),
            None => {
                try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/ResolvedMethodName", 1).ok()
            }
        }
    };
    let h_rmn = rmn.map(|o| (o, ctx.pin_native_root(o)));
    // All allocations done: read every reference back through its pin.
    sf = ctx.read_native_pin(h_sf, sf);
    // WALK21-1: the frame's chunk of the lines (chunk 0 for every frame of
    // a walk of at most 128).
    let Some(lines) = walk_lines.chunk_of(ctx, index) else {
        // Unreachable (`covers` answered above, and a walk publishes its
        // chunks only when every one was stored), but answered.
        ctx.unpin_native_roots(base);
        return populate_sfi(ctx, entry, retain_class_ref);
    };
    // WALK20-1: a kit hit's mirror and holder (current addresses).
    let kit_loaded = match kit_key {
        Some((k, key, KIT_FILLED)) => match k.load(ctx, key) {
            Some(loaded) => Some(loaded),
            None => {
                // Unreachable (a filled class's mirror slot is never
                // cleared), but answered: the frame without the kit.
                ctx.unpin_native_roots(base);
                return populate_sfi(ctx, entry, retain_class_ref);
            }
        },
        _ => None,
    };
    let (meth_str, desc_str, mirror) = match fresh {
        Some(fresh) => {
            let meth_str = ctx.read_native_pin(fresh.meth_pin, fresh.meth_str);
            // WALKTHROW21-3: the next frame of this method takes it from the
            // kit (no allocation).
            if let Some((k, _, _)) = kit_key {
                k.store_method_name(ctx, index, meth_str);
            }
            let desc_str = fresh
                .desc_pinned
                .map(|(s, pin)| ctx.read_native_pin(pin, s));
            let mirror = match (fresh.mirror_pinned, kit_loaded) {
                (Some((mirror, pin)), _) => ctx.read_native_pin(pin, mirror),
                (None, Some((mirror, _))) => mirror,
                (None, None) => {
                    // Unreachable (the mirror is left out only on a kit hit,
                    // whose load answered above), but answered.
                    ctx.unpin_native_roots(base);
                    return populate_sfi(ctx, entry, retain_class_ref);
                }
            };
            if let Some((m, key, _)) = memo_key {
                m.store(ctx, key, meth_str, desc_str, mirror);
            }
            (meth_str, desc_str, mirror)
        }
        None => match memo_key.and_then(|(m, key, _)| m.load(ctx, key)) {
            Some(loaded) => loaded,
            None => {
                // Unreachable (a filled key's slots are never cleared), but
                // answered: the frame without the memo.
                ctx.unpin_native_roots(base);
                return populate_sfi(ctx, entry, retain_class_ref);
            }
        },
    };
    let holder = match (kit_loaded, h_rmn) {
        // WALK20-1: the class's holder, shared by every frame of it.
        (Some((_, held)), _) => held.unwrap_or(mirror),
        (None, Some((rmn, h))) => {
            let rmn = ctx.read_native_pin(h, rmn);
            let vmholder = kit
                .and_then(|k| k.vmholder)
                .or(memo.and_then(|m| m.vmholder));
            match vmholder {
                Some(slot) => ctx.set_field(rmn, slot, Value::Object(Some(mirror))),
                None => ctx.set_field_by_name(rmn, "vmholder", Value::Object(Some(mirror))),
            }
            rmn
        }
        (None, None) => mirror,
    };
    // WALK20-1: the first frame of a class fills the kit (no allocation).
    if let Some((k, key, KIT_UNSET)) = kit_key {
        k.store(ctx, key, mirror, h_rmn.map(|_| holder));
    }
    let mut flags = if retain_class_ref {
        SF_FLAG_RETAIN_CLASS_REF
    } else {
        0
    };
    if entry.line_number == LINE_NUMBER_NATIVE {
        flags |= MODIFIER_NATIVE;
    }
    // Cast: below `WALK_LINES_MAX_FRAMES` (an index in its chunk).
    flags = sfi_flags_with_walk_count(flags, line_index as u64);
    let type_store = match (desc_str, layout.type_) {
        (Some(s), Some(type_slot)) => Some((type_slot, s)),
        _ => None,
    };
    match sfi_store_descriptors(ctx, sf, &layout) {
        // One batched store of the frame's fields (one forwarding read and one
        // watch for the batch), with the declared descriptors.
        Some(d) => {
            let mut slots = [
                (layout.class_or_member_name, d[0]),
                (layout.flags, d[1]),
                (layout.name, d[2]),
                (layout.bci, d[3]),
                (SF_DECL_INTERNAL, d[4]),
                (0, 0),
            ];
            let mut values = [
                Value::Object(Some(holder)),
                Value::Int(flags),
                Value::Object(Some(meth_str)),
                Value::Int(entry.byte_code_index),
                Value::Object(Some(lines)),
                Value::Object(None),
            ];
            let mut n = 5;
            if let Some((type_slot, s)) = type_store {
                slots[5] = (type_slot, d[5]);
                values[5] = Value::Object(Some(s));
                n = 6;
            }
            ctx.set_fields_typed(sf, &slots[..n], &values[..n]);
        }
        None => {
            ctx.set_field(sf, layout.class_or_member_name, Value::Object(Some(holder)));
            ctx.set_field(sf, layout.flags, Value::Int(flags));
            ctx.set_field(sf, layout.name, Value::Object(Some(meth_str)));
            ctx.set_field(sf, layout.bci, Value::Int(entry.byte_code_index));
            if let Some((type_slot, s)) = type_store {
                ctx.set_field(sf, type_slot, Value::Object(Some(s)));
            }
            ctx.set_field(sf, SF_DECL_INTERNAL, Value::Object(Some(lines)));
        }
    }
    ctx.unpin_native_roots(base);
    Ok(sf)
}

thread_local! {
    /// [`sfi_store_descriptors`]'s answer on this thread: `(vm identity,
    /// frame class id, descriptors)`. Per thread, per VM: no process state.
    static SFI_STORE_DESCS_TLS: std::cell::Cell<Option<(usize, u32, [u8; 6])>> =
        const { std::cell::Cell::new(None) };
}

/// The declared descriptor bytes of the slots [`populate_sfi_from_walk_lines`]
/// stores into frame `sf` -- `[classOrMemberName, flags, name, bci, the walk
/// lines' slot, type]` (`type` `0` when the layout has none) -- read from the
/// VM's per-class cache (`NativeContext::instance_field_descriptor_byte`) once
/// per thread, VM and frame class; `None` when any is unknown (the stores then
/// go one by one, as before). 2026-10-26, the stack-walker deep-walk page:
/// six single-slot stores, each with its own forwarding read and watch, were
/// ~13% of a deep `count` walk. The descriptor is the class's own, so the
/// batched store coerces exactly as the single-slot door does.
fn sfi_store_descriptors(
    ctx: &mut dyn NativeContext,
    sf: cratonvm_types::ObjectRef,
    layout: &SfiLayout,
) -> Option<[u8; 6]> {
    let vm = ctx.vm_identity();
    let cid = ctx.class_id_of_object(sf).as_u32();
    if let Some((v, c, d)) = SFI_STORE_DESCS_TLS.with(std::cell::Cell::get) {
        if v == vm && c == cid {
            return Some(d);
        }
    }
    let byte = |slot: usize| ctx.instance_field_descriptor_byte(sf, slot);
    let d = [
        byte(layout.class_or_member_name)?,
        byte(layout.flags)?,
        byte(layout.name)?,
        byte(layout.bci)?,
        byte(SF_DECL_INTERNAL)?,
        match layout.type_ {
            Some(slot) => byte(slot)?,
            None => 0,
        },
    ];
    SFI_STORE_DESCS_TLS.with(|c| c.set(Some((vm, cid, d))));
    Some(d)
}

/// [`populate_sfi_from_walk_lines`]'s freshly interned names and mirror,
/// each with its pin (`meth_pin` is the lowest of them).
struct FreshFrameNames {
    meth_str: cratonvm_types::ObjectRef,
    meth_pin: usize,
    desc_pinned: Option<(cratonvm_types::ObjectRef, usize)>,
    /// The mirror and its pin; `None` on a WALK20-1 kit hit (the kit's).
    mirror_pinned: Option<(cratonvm_types::ObjectRef, usize)>,
}

/// The element of a frame [`populate_sfi_from_walk_lines`] built, from the
/// frame and its walk's lines `lines`, settled against a redefinition of its
/// class since the walk; `None` when the lines cannot be read.
fn sfi_element_from_walk_lines(
    ctx: &mut dyn NativeContext,
    sfi: cratonvm_types::ObjectRef,
    lines: cratonvm_types::ObjectRef,
) -> Option<SfiWalkElement> {
    let flags = match ctx.get_field_by_name(sfi, "flags") {
        Value::Int(f) => f,
        _ => return None,
    };
    // Cast: seven bits.
    let index = sfi_walk_count_of(flags) as usize;
    let mut head = [0i32; WALK_LINES_HEADER];
    if ctx.read_int_array_into(lines, 0, &mut head) != WALK_LINES_HEADER {
        return None;
    }
    let mut slot = [0i32; 1];
    if ctx.read_int_array_into(lines, WALK_LINES_HEADER + index, &mut slot) != 1 {
        return None;
    }
    // Casts: the two words of the count, as bit patterns.
    let walked_at = u64::from(head[0] as u32) | (u64::from(head[1] as u32) << 32);
    let (mut line, no_file) = walk_line_decode(slot[0]);
    let class_id = sfi_class_id(ctx, sfi)?;
    let internal = ctx.class_name_of_id(class_id)?;
    let dotted = crate::lang_class::dotted_class_name(ctx.vm_identity(), class_id, &internal);
    // Interpreter round i4 wave 91, lane L2 (default off): keep the cached
    // dotted name and the frame's interned `name` String, not copies of
    // their text (`SfiWalkElement::class_dotted_arc` / `method_ref`).
    let interned_names = ctx.stack_trace_fill_switches().walk_element_interned_names;
    let (class_dotted, class_dotted_arc) = if interned_names {
        (String::new(), Some(dotted))
    } else {
        (dotted.to_string(), None)
    };
    let (method, method_ref) = match ctx.get_field_by_name(sfi, "name") {
        Value::Object(Some(s)) if interned_names => (String::new(), Some(s)),
        Value::Object(Some(s)) => (ctx.read_string(s).unwrap_or_default(), None),
        _ => (String::new(), None),
    };
    let mut file = if no_file {
        None
    } else {
        ctx.class_source_file(class_id)
    };
    let now = cratonvm_classloading::class_redefinition_count();
    if now != walked_at {
        let mut redefined = false;
        let answered = vm_redefined_classes_between(&*ctx, walked_at, now, |c| {
            redefined |= c == class_id;
        });
        if answered && redefined {
            if line != LINE_NUMBER_NATIVE {
                line = -1;
            }
            file = None;
            if cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok() {
                eprintln!(
                    "[redefine] stack frame read after its class's redefinition: no line: class {} \
                     (walk lines) walked-at={walked_at} now={now}",
                    class_id.as_u32()
                );
            }
        }
    }
    Some(SfiWalkElement {
        class_id: Some(class_id),
        class_dotted,
        method,
        file,
        line,
        class_dotted_arc,
        method_ref,
    })
}

/// `StackStreamFactory.CLASS_INFO_ONLY`: the walker drops method information
/// (`StackWalker.Option.DROP_METHOD_INFO`), and its buffer holds the
/// `ClassFrameInfo`s the JDK allocated (`ClassFrameBuffer.fill`).
const STACK_WALK_MODE_CLASS_INFO_ONLY: i64 = 0x2;

/// Fill frame `index` of a `CLASS_INFO_ONLY` walk's buffer in place, as
/// HotSpot's `java_lang_ClassFrameInfo::init_class` does: the JDK's own
/// `ClassFrameInfo` there gets its class (`classOrMemberName`, a `Class`, which
/// `ClassFrameInfo.declaringClass()` casts to) and keeps its `flags` (its
/// constructor copied the walker's RETAIN_CLASS_REFERENCE). Interpreter round
/// i2 wave 54, lane L3: the walk stored a `StackFrameInfo` there instead,
/// whose method accessors answer where a `ClassFrameInfo`'s throw
/// `UnsupportedOperationException`, and whose `toString()` is the
/// element's (probe `L3W54StackFrameInfoShapes`). `false` when the slot holds
/// no `ClassFrameInfo` (the caller then builds a `StackFrameInfo`, as before).
fn fill_class_frame_info(
    ctx: &mut dyn NativeContext,
    buffer: cratonvm_types::ObjectRef,
    index: usize,
    entry: &cratonvm_native_api::StackTraceEntry,
) -> bool {
    let Value::Object(Some(frame)) = ctx.get_array_element(buffer, index) else {
        return false;
    };
    if ctx
        .class_name_of_id(ctx.class_id_of_object(frame))
        .as_deref()
        != Some("java/lang/ClassFrameInfo")
    {
        return false;
    }
    let Some(cid) = entry
        .class_id
        .or_else(|| ctx.class_id_by_name(&entry.class_name))
    else {
        return false;
    };
    let member_flags = class_frame_member_flags(ctx, std::slice::from_ref(entry))
        .first()
        .copied()
        .unwrap_or(0);
    let pin = ctx.pin_native_root(frame);
    let mirror = ctx.get_class_mirror(cid);
    let frame = ctx.read_native_pin(pin, frame);
    ctx.set_field_by_name(frame, "classOrMemberName", Value::Object(Some(mirror)));
    if member_flags != 0 {
        // `MemberName`'s `MN_HIDDEN_MEMBER` / `MN_CALLER_SENSITIVE`:
        // `toString()`'s ` hidden` / ` caller sensitive` tags
        // ([`class_frame_member_flags`]).
        if let Value::Int(flags) = ctx.get_field_by_name(frame, "flags") {
            ctx.set_field_by_name(frame, "flags", Value::Int(flags | member_flags));
        }
    }
    ctx.unpin_native_roots(pin);
    true
}

/// Can the native `walk` / `forEach` build the JDK's own frame classes
/// (`reflect_invoke::p59_sw_frame`)? `StackFrameInfo`'s layout resolves once
/// it is loaded; the first ask loads it. `false` on a VM without the JDK's
/// class (a synthetic-JDK unit test), whose walks keep the p59 carrier.
/// Interpreter round i2 wave 55, lane L3.
pub(crate) fn jdk_frame_classes_available(ctx: &mut dyn NativeContext) -> bool {
    if sfi_layout(ctx).is_some() {
        return true;
    }
    if ctx
        .ensure_class_initialized("java/lang/StackFrameInfo")
        .is_err()
    {
        return false;
    }
    sfi_layout(ctx).is_some()
}

/// Is `frame` an instance of the JDK's own `java.lang.ClassFrameInfo` (not a
/// `StackFrameInfo`): a `DROP_METHOD_INFO` walk's frame, whose method
/// accessors throw.
pub(crate) fn is_jdk_class_frame_info(
    ctx: &mut dyn NativeContext,
    frame: cratonvm_types::ObjectRef,
) -> bool {
    ctx.class_name_of_id(ctx.class_id_of_object(frame))
        .as_deref()
        == Some("java/lang/ClassFrameInfo")
}

/// The message-less `UnsupportedOperationException` the JDK's `StackFrame`
/// method bodies throw on a frame that has no such information (the
/// interface's default `getMethodType()` / `getDescriptor()`, and
/// `ClassFrameInfo`'s method accessors).
pub(crate) fn frame_lacks_method_info() -> MethodCallFailed {
    MethodCallFailed::from(RuntimeError::UnsupportedOperationException {
        message: String::new(),
    })
}

/// A native standing for `StackFrameInfo.getDescriptor()` refuses a frame of
/// a walker without RETAIN_CLASS_REFERENCE: HotSpot raises that in
/// `ClassFrameInfo.ensureRetainClassRefEnabled`, under
/// `StackFrameInfo.getMethodType`, under `getDescriptor` (measured, JDK
/// 25.0.3; probe `L3W55WalkFrameClasses`, row `descriptor`), two hops below
/// the served method, so the refusal notes that chain for the funnel's mark
/// (`NativeExceptionAccess::note_served_raise_chain`). Interpreter round i2
/// wave 55, lane L3.
#[cold]
#[inline(never)]
pub(crate) fn note_descriptor_retain_refusal_chain(ctx: &mut dyn NativeContext) {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const METHOD_TYPE: &str = "()Ljava/lang/invoke/MethodType;";
    ctx.note_served_raise_chain(ServedRaiseChain {
        throwable_class: "java/lang/UnsupportedOperationException",
        hops: vec![
            ServedRaiseHop {
                class: "java/lang/StackFrameInfo",
                name: "getDescriptor",
                descriptor: "()Ljava/lang/String;",
                site: ServedRaiseHopSite::LastCall {
                    name: "getMethodType",
                    descriptor: Some(METHOD_TYPE),
                },
            },
            ServedRaiseHop {
                class: "java/lang/StackFrameInfo",
                name: "getMethodType",
                descriptor: METHOD_TYPE,
                site: ServedRaiseHopSite::LastCall {
                    name: "ensureRetainClassRefEnabled",
                    descriptor: Some("()V"),
                },
            },
            ServedRaiseHop {
                class: "java/lang/ClassFrameInfo",
                name: "ensureRetainClassRefEnabled",
                descriptor: "()V",
                site: ServedRaiseHopSite::Raise,
            },
        ],
    });
}

/// A `DROP_METHOD_INFO` walk's frame for the native `walk` / `forEach`
/// (`reflect_invoke::p59_sw_frame`, both modes since wave 55): the JDK's own
/// `ClassFrameInfo`, its class in `classOrMemberName` and the walker's
/// RETAIN_CLASS_REFERENCE in `flags`, as `ClassFrameInfo(StackWalker)` and
/// HotSpot's `init_class` leave it (interpreter round i2 wave 54, lane L3;
/// probe `L3W54StackFrameInfoShapes`).
///
/// `member_flags`: the frame's `MemberName` bits ([`class_frame_member_flags`]),
/// ORed into `flags`.
pub(crate) fn populate_class_frame_info(
    ctx: &mut dyn NativeContext,
    entry: &cratonvm_native_api::StackTraceEntry,
    retain_class_ref: bool,
    member_flags: i32,
    index: usize,
    memo: Option<&WalkFrameMemo>,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    let retain = if retain_class_ref {
        SF_FLAG_RETAIN_CLASS_REF
    } else {
        0
    };
    // WALK11-3 (`CRATONVM_STACKWALK_CLASS_FRAME_MEMO`): the mirror from the
    // walk's per-class memo after the class's first frame, the frame through
    // the walk's resolved layout.
    if let (Some(m), Some(cid)) = (memo, entry.class_id) {
        if let (Some(layout), Some((key, state))) = (m.class_frame.as_ref(), m.key(index)) {
            let fresh = (state != MEMO_FILLED).then(|| {
                let mirror = ctx.get_class_mirror(cid);
                (mirror, ctx.pin_native_root(mirror))
            });
            let frame = layout.alloc.alloc(ctx);
            // No allocation from here on: `frame` stays current.
            let mirror = match fresh {
                Some((mirror, pin)) => {
                    let mirror = ctx.read_native_pin(pin, mirror);
                    ctx.unpin_native_roots(pin);
                    m.store_mirror(ctx, key, mirror);
                    Some(mirror)
                }
                None => m.load_mirror(ctx, key),
            };
            if let Some(mirror) = mirror {
                ctx.set_field(
                    frame,
                    layout.class_or_member_name,
                    Value::Object(Some(mirror)),
                );
                ctx.set_field(frame, layout.flags, Value::Int(retain | member_flags));
                return Ok(frame);
            }
            // Unreachable (a filled key's slot is never cleared): the frame
            // without the memo (the allocated one is dropped).
        }
    }
    let cid = entry
        .class_id
        .or_else(|| ctx.class_id_by_name(&entry.class_name));
    let (mirror, h_mirror) = match cid {
        Some(c) => {
            let m = ctx.get_class_mirror(c);
            (Some(m), Some(ctx.pin_native_root(m)))
        }
        None => (None, None),
    };
    let frame = try_alloc_concurrent_synthetic(ctx, "java/lang/ClassFrameInfo", 2);
    let mirror = match (mirror, h_mirror) {
        (Some(m), Some(h)) => {
            let m = ctx.read_native_pin(h, m);
            ctx.unpin_native_roots(h);
            Some(m)
        }
        _ => None,
    };
    let frame = frame?;
    ctx.set_field_by_name(frame, "classOrMemberName", Value::Object(mirror));
    ctx.set_field_by_name(frame, "flags", Value::Int(retain | member_flags));
    Ok(frame)
}

/// `java.lang.invoke.MethodHandleNatives.Constants.MN_HIDDEN_MEMBER`.
const MN_HIDDEN_MEMBER: i32 = 0x0040_0000;
/// `java.lang.invoke.MethodHandleNatives.Constants.MN_CALLER_SENSITIVE`.
const MN_CALLER_SENSITIVE: i32 = 0x0010_0000;

/// The `MemberName` bits HotSpot writes into a `DROP_METHOD_INFO` walk's
/// `ClassFrameInfo.flags` for each of `entries`, which its `toString()` tags:
/// `MN_HIDDEN_MEMBER` for a frame HotSpot's walk lists only with
/// `SHOW_HIDDEN_FRAMES` (`NativeContext::hidden_stack_entries`; measured, JDK
/// 25.0.3: `[java.lang.invoke.LambdaForm$MH/0x...  hidden]`; interpreter round
/// i2 wave 54, lane L3), and `MN_CALLER_SENSITIVE` for a `@CallerSensitive`
/// method's (`NativeContext::caller_sensitive_stack_entries`;
/// `[java.lang.reflect.Method  caller sensitive]`; wave 55, probe
/// `L3W55WalkFrameClasses`, row `caller-sensitive`).
pub(crate) fn class_frame_member_flags(
    ctx: &mut dyn NativeContext,
    entries: &[cratonvm_native_api::StackTraceEntry],
) -> Vec<i32> {
    // The member bit is the method's own (`Method::is_hidden`), whatever
    // `-XX:+ShowHiddenFrames` says: `CallerClassFinder` skips on it
    // (interpreter round i4 wave 92, lane L2).
    let hidden = ctx.hidden_member_entries(entries);
    let sensitive = ctx.caller_sensitive_stack_entries(entries);
    (0..entries.len())
        .map(|i| {
            let mut bits = 0;
            if hidden.get(i) == Some(&true) {
                bits |= MN_HIDDEN_MEMBER;
            }
            if sensitive.get(i) == Some(&true) {
                bits |= MN_CALLER_SENSITIVE;
            }
            bits
        })
        .collect()
}

fn stack_walk_skip_internals(class_name: &str, method_name: &str) -> bool {
    class_name == "java/lang/StackWalker"
        || class_name.starts_with("java/lang/StackWalker$")
        || class_name == "java/lang/StackStreamFactory"
        || class_name.starts_with("java/lang/StackStreamFactory$")
        || (class_name == "java/lang/Thread" && method_name == "getStackTrace")
        || class_name.starts_with("jdk/internal/reflect/")
        || class_name == "java/lang/reflect/Method"
        || class_name.starts_with("java/lang/invoke/MethodHandle")
        || class_name.starts_with("sun/reflect/")
}

/// Does the `StackWalker` `walker` show hidden frames
/// (`StackWalker.Option.SHOW_HIDDEN_FRAMES` in its `options`)? Once per walk.
/// FAILS OPEN: a walker whose options cannot be read shows every frame, as
/// before interpreter round i1 wave 35.
///
/// Read from the option set's fields first ([`enum_set_contains`]), which
/// runs no Java; only an option set of another shape is asked through its
/// `toString` (`[RETAIN_CLASS_REFERENCE, SHOW_HIDDEN_FRAMES]`). Wave 35 always
/// ran that `toString`, i.e. Java and a string allocation on every walk
/// through a hidden frame, which on a virtual thread (`Thread.runWith` is
/// `@Hidden`) is every walk (interpreter round i1 wave 37).
pub(crate) fn walker_shows_hidden_frames(
    ctx: &mut dyn NativeContext,
    walker: Option<cratonvm_types::ObjectRef>,
) -> bool {
    let Some(walker) = walker else {
        return true;
    };
    let options = match ctx.get_field_by_name(walker, "options") {
        Value::Object(Some(o)) => o,
        _ => return true,
    };
    if let Some(shows) = enum_set_contains(ctx, options, "SHOW_HIDDEN_FRAMES") {
        if crate::nbflags().debug_stackwalk {
            eprintln!("[SW-DBG] walker options read from fields: SHOW_HIDDEN_FRAMES={shows}");
        }
        // Wave 117, lane L4: see `note_hidden_frames_observed`.
        if shows {
            ctx.note_hidden_frames_observed();
        }
        return shows;
    }
    if crate::nbflags().debug_stackwalk {
        eprintln!("[SW-DBG] walker options read through toString");
    }
    match ctx.invoke_virtual(options, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx
            .read_string(s)
            .is_none_or(|text| text.contains("SHOW_HIDDEN_FRAMES")),
        _ => true,
    }
}

/// Does the `StackWalker.Option` set `set` hold the constant named
/// `constant`? The `RegularEnumSet` fields first ([`enum_set_contains`]), then
/// the set's `toString()` (the natives' own `HashSet`, a caller's `Set.of`);
/// `None` when neither answers. Runs Java on the second path, so the caller
/// keeps `set` pinned. Interpreter round i1 wave 42, lane L3.
/// JIT round 22 wave 2, lane runtime (flip review R1 / R3): a `StackWalker`
/// created with `SHOW_HIDDEN_FRAMES` (the served `getInstance` natives,
/// `stack_walker::alloc_walker`) ends the VM's "hidden frames never observed"
/// latch at creation, before its first walk, so the compiled reflective links
/// decline from then on. Field reads only (`enum_set_contains`); a set it
/// cannot read leaves the latch to the walk.
pub(crate) fn note_hidden_frames_if_options_show(
    ctx: &mut dyn NativeContext,
    set: cratonvm_types::ObjectRef,
) {
    if enum_set_contains(ctx, set, "SHOW_HIDDEN_FRAMES") == Some(true) {
        ctx.note_hidden_frames_observed();
    }
}

pub(crate) fn walker_options_contain(
    ctx: &mut dyn NativeContext,
    set: cratonvm_types::ObjectRef,
    constant: &str,
) -> Option<bool> {
    if let Some(found) = enum_set_contains(ctx, set, constant) {
        return Some(found);
    }
    match ctx.invoke_virtual(set, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).map(|text| text.contains(constant)),
        _ => None,
    }
}

/// Does the `java.util.RegularEnumSet` `set` contain the constant named
/// `constant`? Read from its fields, without running Java: `universe` (the
/// enum's constants in ordinal order, `EnumSet`'s) names the bit, `elements`
/// (the bit vector, `RegularEnumSet`'s) holds it. The ordinal is looked up by
/// NAME, so it does not depend on the JDK's order of `StackWalker.Option`
/// (JDK 22 inserted `DROP_METHOD_INFO`). `None` when `set` is not of that shape
/// (a `JumboEnumSet`, another `Set`), and the caller asks Java.
fn enum_set_contains(
    ctx: &mut dyn NativeContext,
    set: cratonvm_types::ObjectRef,
    constant: &str,
) -> Option<bool> {
    let Value::Long(elements) = ctx.get_field_by_name(set, "elements") else {
        return None;
    };
    let Value::Object(Some(universe)) = ctx.get_field_by_name(set, "universe") else {
        return None;
    };
    // WALK21-5 (`CRATONVM_STACKWALK_OPTION_ORDINALS_ONCE`): this VM's
    // ordinals of the walker options, read once per enum class and thread.
    if stackwalk_option_ordinals_once_enabled() {
        if let Some(answer) = enum_set_contains_by_ordinals(ctx, universe, elements, constant) {
            return Some(answer);
        }
    }
    let len = ctx.array_length(universe).min(64);
    for ordinal in 0..len {
        let Value::Object(Some(member)) = ctx.get_array_element(universe, ordinal) else {
            continue;
        };
        let Value::Object(Some(name)) = ctx.get_field_by_name(member, "name") else {
            continue;
        };
        if ctx.read_string(name).as_deref() == Some(constant) {
            return Some(elements & (1i64 << ordinal) != 0);
        }
    }
    None
}

/// `CRATONVM_STACKWALK_OPTION_ORDINALS_ONCE=1` (WALK21-5, JIT round 21 wave 1,
/// lane walk; opt-in, default OFF): [`enum_set_contains`] answers the four
/// `StackWalker.Option` names from the ordinals it read ONCE per VM, thread
/// and enum class ([`OptionOrdinals`]) instead of copying each constant's
/// `name` into a Rust `String` until one matches -- every walk asks
/// `DROP_METHOD_INFO` (four copies for a walker without it), and a walk
/// through a hidden or reflection frame asks again. Exact: an enum class's
/// constants, their names and ordinals never change, and class ids are
/// never reused within a VM; a name the class does not declare, or any other
/// constant, takes the name loop as before. Read per call from the VM's flag snapshot.
pub(crate) fn stackwalk_option_ordinals_once_enabled() -> bool {
    // Read through the per-VM flag snapshot (`NativeFlags`), not a process
    // `OnceLock` (r21 review rev-native item 5).
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    crate::nbflags().stackwalk_option_ordinals_once
}

/// The `StackWalker.Option` constants WALK21-5 remembers, in [`OptionOrdinals`]'s
/// slot order.
const WALKER_OPTION_NAMES: [&str; 4] = [
    "RETAIN_CLASS_REFERENCE",
    "SHOW_REFLECT_FRAMES",
    "SHOW_HIDDEN_FRAMES",
    "DROP_METHOD_INFO",
];
/// An [`OptionOrdinals`] slot whose name the enum class does not declare.
const OPTION_ORDINAL_ABSENT: u8 = u8::MAX;

/// WALK21-5's row: for VM `vm` and the enum class of a set's universe (its
/// first constant's class), the ordinal of each [`WALKER_OPTION_NAMES`] name.
#[derive(Clone, Copy)]
struct OptionOrdinals {
    vm: usize,
    enum_class: cratonvm_types::ClassId,
    ordinals: [u8; 4],
}

thread_local! {
    /// WALK21-5's row on this thread (one enum class: the walker options').
    /// Per thread, per VM: no process state.
    static OPTION_ORDINALS_TLS: std::cell::Cell<Option<OptionOrdinals>> =
        const { std::cell::Cell::new(None) };
}

/// The ordinals of [`WALKER_OPTION_NAMES`] in a universe whose constants'
/// names are `names` (ordinal order; `None` for an unreadable one).
fn option_ordinals_from_names(names: &[Option<String>]) -> [u8; 4] {
    let mut out = [OPTION_ORDINAL_ABSENT; 4];
    for (slot, wanted) in out.iter_mut().zip(WALKER_OPTION_NAMES) {
        if let Some(ordinal) = names.iter().position(|n| n.as_deref() == Some(wanted)) {
            // Cast: below 64 (the bit vector's width), checked by the caller.
            *slot = ordinal as u8;
        }
    }
    out
}

/// Is `constant` in the bit vector `elements`, by `ordinals`? `None` when
/// `constant` is not one of [`WALKER_OPTION_NAMES`] or the enum declares no
/// such constant (the caller's name loop decides then).
fn option_ordinal_answer(ordinals: &[u8; 4], elements: i64, constant: &str) -> Option<bool> {
    let slot = WALKER_OPTION_NAMES.iter().position(|n| *n == constant)?;
    let ordinal = ordinals[slot];
    if ordinal == OPTION_ORDINAL_ABSENT || ordinal >= 64 {
        return None;
    }
    Some(elements & (1i64 << ordinal) != 0)
}

/// WALK21-5: [`enum_set_contains`]'s answer from this thread's
/// [`OptionOrdinals`] row, filled now from `universe` when it is another VM's
/// or another enum's. No Java, no allocation on the heap.
fn enum_set_contains_by_ordinals(
    ctx: &mut dyn NativeContext,
    universe: cratonvm_types::ObjectRef,
    elements: i64,
    constant: &str,
) -> Option<bool> {
    WALKER_OPTION_NAMES.iter().position(|n| *n == constant)?;
    let len = ctx.array_length(universe);
    if len == 0 || len > 64 {
        return None;
    }
    let Value::Object(Some(first)) = ctx.get_array_element(universe, 0) else {
        return None;
    };
    let enum_class = ctx.class_id_of_object(first);
    let vm = ctx.vm_identity();
    let row = match OPTION_ORDINALS_TLS.with(std::cell::Cell::get) {
        Some(row) if row.vm == vm && row.enum_class == enum_class => row,
        _ => {
            let names: Vec<Option<String>> = (0..len)
                .map(|ordinal| match ctx.get_array_element(universe, ordinal) {
                    Value::Object(Some(member)) => match ctx.get_field_by_name(member, "name") {
                        Value::Object(Some(name)) => ctx.read_string(name),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            let row = OptionOrdinals {
                vm,
                enum_class,
                ordinals: option_ordinals_from_names(&names),
            };
            OPTION_ORDINALS_TLS.with(|c| c.set(Some(row)));
            row
        }
    };
    option_ordinal_answer(&row.ordinals, elements, constant)
}

/// Is the walk whose `AbstractStackWalker` is `this` a `getCallerClass`
/// walk (`java.lang.StackStreamFactory$CallerClassFinder`)? Asked only under
/// `-XX:+ShowHiddenFrames` (interpreter round i4 wave 92, lane L2).
fn walk_is_caller_class_finder(
    ctx: &mut dyn NativeContext,
    this: Option<cratonvm_types::ObjectRef>,
) -> bool {
    let Some(this) = this else {
        return false;
    };
    let class_id = ctx.class_id_of_object(this);
    ctx.class_name_of_id(class_id).as_deref()
        == Some("java/lang/StackStreamFactory$CallerClassFinder")
}

/// `java.lang.StackStreamFactory.SHOW_HIDDEN_FRAMES`, the `callStackWalk`
/// mode bit the JDK sets from `StackWalker.Option.SHOW_HIDDEN_FRAMES` (the
/// same value in JDK 17, 21 and 25).
const STACK_WALK_MODE_SHOW_HIDDEN_FRAMES: i64 = 0x20;

/// `frames` without the hidden ones (`NativeContext::hidden_stack_entries`)
/// unless the walker shows them: HotSpot's stack walk skips
/// `Method::is_hidden` frames without `SHOW_HIDDEN_FRAMES` (interpreter round
/// i1 wave 35).
///
/// `shows_hidden` is asked only when some frame IS hidden, so an ordinary walk
/// runs no Java for it. It may run Java (`walker_shows_hidden_frames`), so the
/// caller keeps every object it still needs pinned across it.
pub(crate) fn drop_hidden_frames(
    ctx: &mut dyn NativeContext,
    frames: Vec<StackTraceEntry>,
    shows_hidden: impl FnOnce(&mut dyn NativeContext) -> bool,
) -> Vec<StackTraceEntry> {
    let hidden = ctx.hidden_stack_entries(&frames);
    // Under `-XX:+ShowHiddenFrames` the listing answer above already keeps
    // every real method, and what it still marks (the VM's own bridge frame)
    // is dropped whatever the walker's options (interpreter round i4 wave 92,
    // lane L2).
    if !hidden.iter().any(|h| *h) || (shows_hidden(ctx) && !ctx.show_hidden_frames_flag()) {
        return frames;
    }
    frames
        .into_iter()
        .zip(hidden)
        .filter_map(|(frame, hidden)| (!hidden).then_some(frame))
        .collect()
}

/// `frames` (a walk's frames, INNERMOST first) with the hidden JDK frames the
/// VM's natives stand for (`NativeContext::hidden_standin_frames`: the
/// method-handle frames of a `MethodHandle.invokeExact` or `Method.invoke`
/// call) inserted where HotSpot runs them, when the VM lists them and the
/// walker shows hidden frames (`shows_hidden`, asked only when the VM lists
/// them). Answers the frames and the `shows_hidden` answer if it was asked, so
/// the caller's [`drop_hidden_frames`] need not ask again. Interpreter round i1
/// wave 48, lane L3 (`tools/probes/interp/L3/L3W46InvokeFramesInWalks.java`,
/// rows `*-hidden-shows-invoke-frames`).
pub(crate) fn add_hidden_standin_frames(
    ctx: &mut dyn NativeContext,
    frames: Vec<StackTraceEntry>,
    shows_hidden: impl FnOnce(&mut dyn NativeContext) -> bool,
) -> (Vec<StackTraceEntry>, Option<bool>) {
    if frames.len() < 2 || !ctx.lists_hidden_standin_frames() {
        return (frames, None);
    }
    if !shows_hidden(ctx) {
        return (frames, Some(false));
    }
    let insertions = ctx.hidden_standin_frames(&frames);
    (splice_hidden_standin_frames(frames, insertions), Some(true))
}

/// `frames` with each of `insertions` (`(index, entries)`, indices ascending)
/// inserted before `frames[index]`; an index past the end appends.
fn splice_hidden_standin_frames(
    frames: Vec<StackTraceEntry>,
    insertions: Vec<(usize, Vec<StackTraceEntry>)>,
) -> Vec<StackTraceEntry> {
    if insertions.is_empty() {
        return frames;
    }
    let added: usize = insertions.iter().map(|(_, entries)| entries.len()).sum();
    let mut out = Vec::with_capacity(frames.len() + added);
    let mut pending = insertions.into_iter().peekable();
    for (i, frame) in frames.into_iter().enumerate() {
        while let Some((_, entries)) = pending.next_if(|(at, _)| *at <= i) {
            out.extend(entries);
        }
        out.push(frame);
    }
    for (_, entries) in pending {
        out.extend(entries);
    }
    out
}

// ---------------------------------------------------------------------------
// The spun `LambdaForm$MH` frames of a served `invokeWithArguments`
// (interpreter round i6 wave 115, lane L3; `CRATONVM_STANDIN_SPUN_IWA_FRAMES`,
// default off; page `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`,
// probe `tools/probes/interp/L3/L3W115SpunIwaWalkFrames.java`).
//
// HotSpot runs JDK 25's `MethodHandle.invokeWithArguments(Object...)` body,
// `invocationType.invokers().spreadInvoker(0).invokeExact(asType(invocationType), arguments)`
// with `invocationType = genericMethodType(n)`. On a direct handle a
// `SHOW_HIDDEN_FRAMES` walk lists, INNERMOST first:
//
// ```text
// target
// DirectMethodHandle$Holder.invokeStatic          (pregenerated; the VM names it)
// LambdaForm$MH/0x...invoke(Object x (n+1))Object  the asType adapter, only when
//                                                  the handle's type is not
//                                                  genericMethodType(n)
// LambdaForm$MH/0x...invoke(Object,Object,Object)Object  the spread invoker
// Invokers$Holder.invokeExact_MT                  (pregenerated; the VM names it)
// MethodHandle.invokeWithArguments
// ```
//
// The VM serves the call natively (an `Intrinsic`; running the JDK body costs
// ~35 us a call, `i110-L4`), so the two spun classes do not take part in it.
// Their frames are not a function of the walk alone: they are methods of
// hidden classes the JDK spins per form. So the walk asks the JDK for them,
// as the body would: the spread invoker's form from
// `genericMethodType(n).invokers().spreadInvoker(0)`, and the adapter's from
// `asType(genericMethodType(n))` on a direct handle of the target the walk
// shows (`IMPL_LOOKUP.findStatic` / `findVirtual` / `findConstructor`; the
// adapter's form depends on the handle's type and basic type, not on its
// kind). Each frame is the form's `vmentry` method at its one `invokeBasic`
// call (`NativeContext::spun_lambda_form_frame`), so its class is a real
// hidden class (`getDeclaringClass()` under `RETAIN_CLASS_REFERENCE`) and its
// bci is the JDK generator's.
//
// Fail-closed: only where the VM already named the direct handle's holder
// frame and the call site's invoker frame under the array overload's
// `invokeWithArguments` frame, and only when every JDK call answers.
// Residuals (page): a handle that is not direct (bound, inserted, filtered:
// its own forms stand between, and the target's arity is not `n`); an adapter
// HotSpot has customized after 127 calls through the same handle (same
// names, other bytecode, so another bci); the `-XX:+ShowHiddenFrames`
// throwable, which this does not touch (no Java may run in a capture).
// ---------------------------------------------------------------------------

thread_local! {
    /// Set while [`add_spun_invoke_with_arguments_frames`] runs Java, so a
    /// walk made inside those calls lists no spun frames (no recursion).
    static SPUN_IWA_BUSY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The served `invokeWithArguments` target a walk shows: where its spun
/// frames go and what the JDK is asked for them.
struct SpunIwaSite {
    /// Insert before `frames[at]` (the `Invokers$Holder.invokeExact_MT` frame).
    at: usize,
    class_id: cratonvm_types::ClassId,
    name: std::sync::Arc<str>,
    descriptor: std::sync::Arc<str>,
    /// `invokeStatic`, `invokeSpecial` / `invokeVirtual`, `newInvokeSpecial`:
    /// the holder the VM named, which says how to look the target up.
    holder: std::sync::Arc<str>,
    /// `genericMethodType(n)`'s `n`: the holder's parameters but its handle.
    arity: usize,
    /// The served call's row in `NativeContext::invoke_with_arguments_adapters`
    /// (OUTERMOST first) whose array-overload frame is `frames[at + 1]`, when
    /// the walk could pair them ([`iwa_frame_rows`]) and the row noted an
    /// adapter. Interpreter round i7 wave 137, lane L3: the walk then reads
    /// the adapter the call ran ([`spun_iwa_record_entries`]) instead of
    /// rebuilding one from the target.
    row: Option<usize>,
    /// The VM named no `DirectMethodHandle$Holder` frame between the target
    /// and the invoker (`at` is then the target's index plus one): a shape the
    /// image does not pregenerate, e.g. a bound handle's
    /// `invokeVirtual(Object,Object,Object)Object`, which HotSpot runs as a
    /// SPUN `LambdaForm$DMH` method. Only the record path answers such a site
    /// (it lists that frame from the handle the call ran); `holder` and
    /// `arity` are unused. Interpreter round i7 wave 138, lane L3.
    holderless: bool,
}

/// What [`spun_iwa_entries`] lists for one site: the frames inserted before
/// the invoker frame (INNERMOST first), and, for a customized direct handle,
/// the spun `LambdaForm$DMH` frame that takes the place of the VM's
/// `DirectMethodHandle$Holder` stand-in (`frames[at - 1]`).
struct SpunIwaAnswer {
    inserted: Vec<StackTraceEntry>,
    holder: Option<StackTraceEntry>,
}

/// `frames` (a walk's, INNERMOST first) with the spun `LambdaForm$MH` frames
/// of each served `invokeWithArguments` call inserted (see the section
/// comment). Runs Java: the caller keeps every reference it holds pinned.
pub(crate) fn add_spun_invoke_with_arguments_frames(
    ctx: &mut dyn NativeContext,
    frames: Vec<StackTraceEntry>,
) -> Vec<StackTraceEntry> {
    let busy = SPUN_IWA_BUSY.with(|b| b.get());
    if crate::nbflags().debug_stackwalk {
        // Wave 117: one line per call, so a run that reaches the step says so.
        eprintln!(
            "[SW-DBG] spun-iwa called frames={} busy={busy}",
            frames.len()
        );
    }
    if busy {
        return frames;
    }
    // Interpreter round i7 wave 138, lane L3: served calls whose adapter ran
    // as a real activation, right under the array overload's frame.
    let adapter_sites = spun_iwa_real_adapter_sites(&frames);
    if !adapter_sites.is_empty() {
        SPUN_IWA_BUSY.with(|b| b.set(true));
        let mut insertions = Vec::with_capacity(adapter_sites.len());
        for &(at, n, adapter_class) in &adapter_sites {
            if let Some(entries) = spun_iwa_real_adapter_entries(ctx, n, adapter_class) {
                if crate::nbflags().debug_stackwalk {
                    for e in &entries {
                        eprintln!(
                            "[SW-DBG] spun-iwa real-adapter at={at} {}.{}{} bci={}",
                            e.class_name,
                            e.method_name,
                            e.method_descriptor.as_deref().unwrap_or(""),
                            e.byte_code_index
                        );
                    }
                }
                insertions.push((at, entries));
            }
        }
        SPUN_IWA_BUSY.with(|b| b.set(false));
        // A site of the other shapes never sits under a real adapter frame,
        // so the two passes touch different frames; the first runs on the
        // spliced walk, whose indices it recomputes.
        let frames = splice_hidden_standin_frames(frames, insertions);
        return add_spun_invoke_with_arguments_sites(ctx, frames);
    }
    add_spun_invoke_with_arguments_sites(ctx, frames)
}

/// [`add_spun_invoke_with_arguments_frames`] for the served chains the VM
/// named holder and invoker stand-ins for ([`spun_iwa_sites`]).
fn add_spun_invoke_with_arguments_sites(
    ctx: &mut dyn NativeContext,
    frames: Vec<StackTraceEntry>,
) -> Vec<StackTraceEntry> {
    let sites = spun_iwa_sites(&frames);
    if sites.is_empty() {
        // Wave 116: a walk through `invokeWithArguments` that found no served
        // chain names the frames it saw above the call.
        if crate::nbflags().debug_stackwalk {
            for (i, f) in frames.iter().enumerate() {
                if names_class(&f.class_name, "java/lang/invoke/MethodHandle")
                    && &*f.method_name == "invokeWithArguments"
                {
                    let above: Vec<String> = frames[i.saturating_sub(3)..i]
                        .iter()
                        .map(|e| {
                            format!(
                                "{}.{}{} id={:?}",
                                e.class_name,
                                e.method_name,
                                e.method_descriptor.as_deref().unwrap_or("<no descriptor>"),
                                e.class_id.map(|c| c.as_u32())
                            )
                        })
                        .collect();
                    eprintln!(
                        "[SW-DBG] spun-iwa decline: no served chain under {}{}: {}",
                        f.method_name,
                        f.method_descriptor.as_deref().unwrap_or("<no descriptor>"),
                        above.join(" | ")
                    );
                }
            }
        }
        return frames;
    }
    // Interpreter round i7 wave 137, lane L3: pair each site with the served
    // call's row, whose adapter (wave 136, lane L4's record) the walk reads.
    let mut sites = sites;
    let adapters = ctx.invoke_with_arguments_adapters();
    match iwa_frame_rows(&frames, adapters.len()) {
        Some(pairs) => {
            for site in &mut sites {
                site.row = pairs
                    .iter()
                    .find(|(frame, _)| *frame == site.at + 1)
                    .map(|(_, row)| *row)
                    .filter(|row| adapters.get(*row).is_some_and(Option::is_some));
            }
        }
        None => spun_iwa_decline(|| {
            format!(
                "record: {} served rows, walk lists another count of invokeWithArguments frames; rebuilding adapters",
                adapters.len()
            )
        }),
    }
    drop(adapters);
    SPUN_IWA_BUSY.with(|b| b.set(true));
    let mut insertions = Vec::with_capacity(sites.len());
    let mut holders = Vec::new();
    for site in &sites {
        if let Some(answer) = spun_iwa_entries(ctx, site) {
            insertions.push((site.at, answer.inserted));
            if let Some(holder) = answer.holder {
                holders.push((site.at - 1, holder));
            }
        }
    }
    SPUN_IWA_BUSY.with(|b| b.set(false));
    let mut frames = frames;
    for (index, holder) in holders {
        if crate::nbflags().debug_stackwalk {
            eprintln!(
                "[SW-DBG] spun-iwa holder at={index} {}.{}{} bci={}",
                holder.class_name,
                holder.method_name,
                holder.method_descriptor.as_deref().unwrap_or(""),
                holder.byte_code_index
            );
        }
        if let Some(slot) = frames.get_mut(index) {
            *slot = holder;
        }
    }
    if crate::nbflags().debug_stackwalk {
        for (at, entries) in &insertions {
            for e in entries {
                eprintln!(
                    "[SW-DBG] spun-iwa at={at} {}.{}{} bci={}",
                    e.class_name,
                    e.method_name,
                    e.method_descriptor.as_deref().unwrap_or(""),
                    e.byte_code_index
                );
            }
        }
    }
    splice_hidden_standin_frames(frames, insertions)
}

/// The spun `LambdaForm$MH` steps a `SHOW_HIDDEN_FRAMES` walk takes, each
/// under its own switch: the served `invokeWithArguments`'s
/// (`CRATONVM_STANDIN_SPUN_IWA_FRAMES`, [`add_spun_invoke_with_arguments_frames`]),
/// then the `invokeExact` linker's (`CRATONVM_STANDIN_SPUN_LINKER_FRAMES`,
/// [`add_spun_linker_frames`]). Interpreter round i6 wave 121, lane L3: the
/// one entry both walk routes call. Runs Java: the caller keeps every
/// reference it holds pinned.
pub(crate) fn add_spun_lambda_form_frames(
    ctx: &mut dyn NativeContext,
    frames: Vec<StackTraceEntry>,
) -> Vec<StackTraceEntry> {
    let frames = if crate::nbflags().standin_spun_iwa_frames {
        add_spun_invoke_with_arguments_frames(ctx, frames)
    } else {
        frames
    };
    if crate::nbflags().standin_spun_linker_frames {
        add_spun_linker_frames(ctx, frames)
    } else {
        frames
    }
}

/// Does either spun step run for a walk ([`add_spun_lambda_form_frames`])?
pub(crate) fn spun_lambda_form_steps_on() -> bool {
    crate::nbflags().standin_spun_iwa_frames || crate::nbflags().standin_spun_linker_frames
}

// ---------------------------------------------------------------------------
// The spun linker of an `invokeExact` call site whose shape `Invokers$Holder`
// does not pregenerate (interpreter round i6 wave 121, lane L3;
// `CRATONVM_STANDIN_SPUN_LINKER_FRAMES`, default off; residual 5 of page
// `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`, probe
// `tools/probes/interp/L3/L3W121SpunLinkerFrames.java`).
//
// HotSpot links `mh.invokeExact(..)` of type `T` to
// `Invokers.methodHandleInvokeLinkerMethod("invokeExact", T, ..)`, the
// `vmentry` of `invokeHandleForm(T, false, LF_EX_LINKER)`: the form of
// `T.basicType()`, resolved from `Invokers$Holder` when pregenerated and
// otherwise spun as `LambdaForm$MH/0x...invokeExact_MT`. A `SHOW_HIDDEN_FRAMES`
// walk lists that frame right above the calling one (measured on HotSpot 25:
// `(I)I` -> `LambdaForm$MH.invokeExact_MT(Object,int,Object)int@19`). The VM
// names the pregenerated one (`stackwalker::invoker_holder_entry`) and leaves
// the spun one out, since a capture may run no Java. A walk may, so this
// step asks the JDK for the form, as the link would (the form is cached on
// the basic type's `MethodTypeForm`, so the class is the one every such site
// shares), and lists its `vmentry` method at its one `invokeBasic` call.
//
// Fail-closed: only where the frame above the caller is the served call's
// holder frame or a `LambdaForm$` activation (not a linker already, and not
// the target itself: the VM names no holder for an unpregenerated
// `DirectMethodHandle` shape either, and a target directly above the caller
// cannot be told from, say, a class initializer the link ran), only for
// `invokeExact` (a generic `invoke` may stand an `asType` adapter between),
// and only when every JDK call answers.
// ---------------------------------------------------------------------------

/// `MethodTypeForm.LF_EX_LINKER` (JDK 25; what
/// `Invokers.methodHandleInvokeLinkerMethod` passes for `invokeExact`).
const LF_EX_LINKER: i32 = 10;

/// `MethodTypeForm.LF_GEN_LINKER` (JDK 25; what
/// `Invokers.methodHandleInvokeLinkerMethod` passes for a generic `invoke`).
const LF_GEN_LINKER: i32 = 12;

/// `frames` (a walk's, INNERMOST first) with the spun linker frame of each
/// served `invokeExact` call site whose shape is not pregenerated inserted
/// right above its caller (see the section comment). Runs Java.
pub(crate) fn add_spun_linker_frames(
    ctx: &mut dyn NativeContext,
    frames: Vec<StackTraceEntry>,
) -> Vec<StackTraceEntry> {
    if SPUN_IWA_BUSY.with(|b| b.get()) {
        return frames;
    }
    // `(insert before, site descriptor, generic)`.
    let mut sites: Vec<(usize, String, bool)> = Vec::new();
    for k in 0..frames.len().saturating_sub(1) {
        if !spun_linker_callee(&frames[k]) {
            continue;
        }
        if let Some(descriptor) = ctx.method_handle_invoke_exact_site(&frames[k + 1]) {
            sites.push((k + 1, descriptor, false));
            continue;
        }
        // Interpreter round i7 wave 142, lane L3 (`L3W141ReflectHunt` row
        // `walk-hidden method`): a GENERIC `invoke` site, fail-closed to the
        // one shape with no `asType` adapter between the linker and the
        // handle -- a static target's direct handle (`invokeStatic` holder
        // right above the caller) whose type, the target's own descriptor,
        // is the site's exactly; `checkGenericType` then answers the handle
        // itself and HotSpot lists the generic linker (`invoke_MT`) frame
        // directly above the holder.
        if let Some(descriptor) = ctx.method_handle_generic_invoke_site(&frames[k + 1]) {
            let holder = &frames[k];
            let exact_static_target = k >= 1
                && names_class(&holder.class_name, "java/lang/invoke/DirectMethodHandle$Holder")
                && &*holder.method_name == "invokeStatic"
                && !frames[k - 1].class_name.starts_with("java/lang/invoke/")
                && !frames[k - 1].class_name.starts_with("java.lang.invoke.")
                && frames[k - 1].method_descriptor.as_deref() == Some(descriptor.as_str());
            if exact_static_target {
                sites.push((k + 1, descriptor, true));
            } else {
                spun_iwa_decline(|| {
                    format!("linker: generic invoke{descriptor} not over an exact static target")
                });
            }
        }
    }
    if crate::nbflags().debug_stackwalk {
        eprintln!(
            "[SW-DBG] spun-linker called frames={} sites={}",
            frames.len(),
            sites.len()
        );
    }
    if sites.is_empty() {
        return frames;
    }
    SPUN_IWA_BUSY.with(|b| b.set(true));
    let mut memo: Vec<(String, bool, Option<StackTraceEntry>)> = Vec::new();
    let mut insertions = Vec::with_capacity(sites.len());
    for (at, descriptor, generic) in sites {
        let Some(basic) = basic_call_descriptor(&descriptor) else {
            spun_iwa_decline(|| format!("linker: site {descriptor} not basic-typed"));
            continue;
        };
        let entry = match memo.iter().find(|(d, g, _)| *d == basic && *g == generic) {
            Some((_, _, e)) => e.clone(),
            None => {
                let e = spun_linker_entry(ctx, &basic, generic);
                memo.push((basic, generic, e.clone()));
                e
            }
        };
        if let Some(e) = entry {
            insertions.push((at, vec![e]));
        }
    }
    SPUN_IWA_BUSY.with(|b| b.set(false));
    if crate::nbflags().debug_stackwalk {
        for (at, entries) in &insertions {
            for e in entries {
                eprintln!(
                    "[SW-DBG] spun-linker at={at} {}.{}{} bci={}",
                    e.class_name,
                    e.method_name,
                    e.method_descriptor.as_deref().unwrap_or(""),
                    e.byte_code_index
                );
            }
        }
    }
    splice_hidden_standin_frames(frames, insertions)
}

/// Is `callee` (the frame right above an `invokeExact` caller) one a served
/// call left there with no linker above it: a `DirectMethodHandle$Holder`
/// frame or a `LambdaForm$` activation that is not itself a linker?
fn spun_linker_callee(callee: &StackTraceEntry) -> bool {
    const DMH_HOLDER: &str = "java/lang/invoke/DirectMethodHandle$Holder";
    const INVOKERS_HOLDER: &str = "java/lang/invoke/Invokers$Holder";
    let name = &*callee.class_name;
    let lambda_form =
        name.starts_with("java/lang/invoke/LambdaForm$") || name.starts_with("java.lang.invoke.LambdaForm$");
    let linker_name = matches!(&*callee.method_name, "invokeExact_MT" | "invoke_MT");
    if linker_name && (lambda_form || names_class(name, INVOKERS_HOLDER)) {
        return false;
    }
    names_class(name, DMH_HOLDER) || lambda_form
}

/// `descriptor` (an `invokeExact` call site's type) as its basic type's
/// descriptor (`MethodType.basicType()`: references `Object`, subword
/// primitives `int`), or `None` when it does not parse or its parameters take
/// more slots than the uncustomized linker serves (`parameterSlotCount() >
/// 253`, where HotSpot links a customized form instead).
fn basic_call_descriptor(descriptor: &str) -> Option<String> {
    const OBJECT: &str = "Ljava/lang/Object;";
    let body = descriptor.strip_prefix('(')?;
    let (params, ret) = body.split_once(')')?;
    let basic = |bytes: &[u8], pos: &mut usize| -> Option<&'static str> {
        let tag = *bytes.get(*pos)?;
        *pos += 1;
        Some(match tag {
            b'Z' | b'B' | b'S' | b'C' | b'I' => "I",
            b'J' => "J",
            b'F' => "F",
            b'D' => "D",
            b'V' => "V",
            b'L' => {
                *pos += bytes[*pos..].iter().position(|b| *b == b';')? + 1;
                OBJECT
            }
            b'[' => {
                while bytes.get(*pos) == Some(&b'[') {
                    *pos += 1;
                }
                match *bytes.get(*pos)? {
                    b'L' => *pos += bytes[*pos..].iter().position(|b| *b == b';')? + 1,
                    b'Z' | b'B' | b'S' | b'C' | b'I' | b'J' | b'F' | b'D' => *pos += 1,
                    _ => return None,
                }
                OBJECT
            }
            _ => return None,
        })
    };
    let mut out = String::from("(");
    let mut slots = 0usize;
    let bytes = params.as_bytes();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let t = basic(bytes, &mut pos)?;
        if t == "V" {
            return None;
        }
        slots += if t == "J" || t == "D" { 2 } else { 1 };
        out.push_str(t);
    }
    if slots > 253 {
        return None;
    }
    out.push(')');
    let mut rpos = 0usize;
    let r = basic(ret.as_bytes(), &mut rpos)?;
    if rpos != ret.len() {
        return None;
    }
    out.push_str(r);
    Some(out)
}

/// The spun linker frame of the basic call type `basic`: the `vmentry` of
/// `Invokers.invokeHandleForm(fromMethodDescriptorString(basic), false,
/// LF_EX_LINKER)`, its `invokeExact_MT` at its one `invokeBasic` call. `None`
/// (with a decline line) when any step fails or the shape is pregenerated
/// after all (its entry is then `Invokers$Holder`, not a spun class).
fn spun_linker_entry(
    ctx: &mut dyn NativeContext,
    basic: &str,
    generic: bool,
) -> Option<StackTraceEntry> {
    // Wave 142: the generic linker (`LF_GEN_LINKER`, `invoke_MT`) too.
    let (which, linker_name) = if generic {
        (LF_GEN_LINKER, "invoke_MT")
    } else {
        (LF_EX_LINKER, "invokeExact_MT")
    };
    let text = ctx.create_string_uninterned(basic);
    let method_type = answered_object_at(
        "linker: MethodType.fromMethodDescriptorString",
        ctx.invoke_static(
            "java/lang/invoke/MethodType",
            "fromMethodDescriptorString",
            "(Ljava/lang/String;Ljava/lang/ClassLoader;)Ljava/lang/invoke/MethodType;",
            &[Value::Object(Some(text)), Value::Object(None)],
        ),
    )?;
    let form = answered_object_at(
        "linker: Invokers.invokeHandleForm",
        ctx.invoke_static(
            "java/lang/invoke/Invokers",
            "invokeHandleForm",
            "(Ljava/lang/invoke/MethodType;ZI)Ljava/lang/invoke/LambdaForm;",
            &[
                Value::Object(Some(method_type)),
                Value::Int(0),
                Value::Int(which),
            ],
        ),
    )?;
    let class_id = lambda_form_entry_class(ctx, form, "linker")?;
    // The linker's own descriptor: the handle, the site's basic parameters,
    // the appendix (the site's `MethodType`), the basic return.
    let (params, ret) = basic.strip_prefix('(')?.split_once(')')?;
    let descriptor = format!("(Ljava/lang/Object;{params}Ljava/lang/Object;){ret}");
    let frame = ctx.spun_lambda_form_frame(class_id, linker_name, &descriptor);
    if frame.is_none() {
        spun_iwa_decline(|| {
            format!(
                "linker: {} has no {linker_name}{descriptor} with one invokeBasic",
                ctx.class_name_of_id(class_id).unwrap_or_default()
            )
        });
    }
    frame
}

/// Interpreter round i7 wave 138, lane L3 (orchestrator's w137 run of
/// `L3W137IwaRecordedAdapterFrames`): the served calls whose `asType` /
/// bound adapter ran as a REAL activation of its spun `LambdaForm$MH` method
/// (the served lanes run a `BoundMethodHandle` adapter through its
/// `vmentry`), so the walk lists target | [direct handle frame] | adapter
/// frame | `invokeWithArguments`, and HotSpot's spread invoker and
/// `Invokers$Holder.invokeExact_MT` frames between the adapter and the array
/// overload are missing. `(insert before, n, adapter frame's class)` per such
/// site, `n` being the adapter method's parameters but its handle. Reads
/// only.
fn spun_iwa_real_adapter_sites(
    frames: &[StackTraceEntry],
) -> Vec<(usize, usize, Option<cratonvm_types::ClassId>)> {
    let mut out = Vec::new();
    for (k, pair) in frames.windows(2).enumerate() {
        let [adapter, iwa] = pair else {
            continue;
        };
        let spun = adapter.class_name.starts_with("java/lang/invoke/LambdaForm$MH")
            || adapter.class_name.starts_with("java.lang.invoke.LambdaForm$MH");
        if !spun || !is_iwa_array_frame(iwa) {
            continue;
        }
        let Some(params) = adapter
            .method_descriptor
            .as_deref()
            .and_then(descriptor_parameter_count)
        else {
            continue;
        };
        let Some(n) = params.checked_sub(1) else {
            continue;
        };
        out.push((k + 1, n, adapter.class_id));
    }
    out
}

/// The frames a real-adapter site ([`spun_iwa_real_adapter_sites`]) lacks,
/// INNERMOST first: the spread invoker of `genericMethodType(n)` and the
/// `Invokers$Holder.invokeExact_MT` linker of the body's
/// `spreadInvoker.invokeExact(adapter, arguments)` call. `None` when a step
/// declines, or when the frame under the call is the spread invoker itself
/// (`adapter_class` is its class: nothing is missing). Runs Java.
fn spun_iwa_real_adapter_entries(
    ctx: &mut dyn NativeContext,
    n: usize,
    adapter_class: Option<cratonvm_types::ClassId>,
) -> Option<Vec<StackTraceEntry>> {
    let arity = i32::try_from(n).ok()?;
    let generic = answered_object_at(
        "real adapter: MethodType.genericMethodType",
        ctx.invoke_static(
            "java/lang/invoke/MethodType",
            "genericMethodType",
            "(I)Ljava/lang/invoke/MethodType;",
            &[Value::Int(arity)],
        ),
    )?;
    let generic_pin = ctx.pin_native_root(generic);
    let spread = spun_iwa_spread_frame(ctx, generic_pin, generic);
    ctx.unpin_native_roots(generic_pin);
    let spread = spread?;
    if spread.class_id.is_some() && spread.class_id == adapter_class {
        spun_iwa_decline(|| "real adapter: the frame under the call is the spread invoker".to_string());
        return None;
    }
    let invoker = spun_iwa_invokers_holder_frame(ctx)?;
    Some(vec![spread, invoker])
}

/// `Invokers$Holder.invokeExact_MT(Object,Object,Object,Object)Object` at its
/// one `invokeBasic`: the linker of `invokeWithArguments`' call site
/// `spreadInvoker.invokeExact(MethodHandle, Object[])Object`, whatever the
/// target. Reads only.
fn spun_iwa_invokers_holder_frame(ctx: &mut dyn NativeContext) -> Option<StackTraceEntry> {
    let Some(class_id) = ctx.class_id_by_name("java/lang/invoke/Invokers$Holder") else {
        spun_iwa_decline(|| "real adapter: Invokers$Holder is not loaded".to_string());
        return None;
    };
    let descriptor = generic_form_descriptor(4);
    let frame = ctx.spun_lambda_form_frame(class_id, "invokeExact_MT", &descriptor);
    if frame.is_none() {
        spun_iwa_decline(|| format!("real adapter: Invokers$Holder has no invokeExact_MT{descriptor} with one invokeBasic"));
    }
    frame
}

/// The served `invokeWithArguments` targets of `frames` (INNERMOST first):
/// a target, the direct handle's `DirectMethodHandle$Holder` frame, an
/// `Invokers$Holder.invokeExact_MT` frame and the array overload's
/// `MethodHandle.invokeWithArguments` frame, in that order. Reads only.
fn spun_iwa_sites(frames: &[StackTraceEntry]) -> Vec<SpunIwaSite> {
    const DMH_HOLDER: &str = "java/lang/invoke/DirectMethodHandle$Holder";
    const INVOKERS_HOLDER: &str = "java/lang/invoke/Invokers$Holder";
    const IWA_ARRAY: &str = "([Ljava/lang/Object;)Ljava/lang/Object;";
    let mut out = Vec::new();
    for (k, window) in frames.windows(4).enumerate() {
        let [target, holder, invoker, iwa] = window else {
            continue;
        };
        // Interpreter round i6 wave 117, lane L3: names compared in either
        // spelling (some capture paths hand dotted names; the served
        // `invokeWithArguments` frame's own test, `is_invoke_with_arguments_frame`,
        // accepts both), and an `invokeWithArguments` frame with no descriptor
        // is the array overload when it stands directly under the invoker.
        if !names_class(&holder.class_name, DMH_HOLDER)
            || !matches!(
                &*holder.method_name,
                "invokeStatic" | "invokeSpecial" | "invokeVirtual" | "newInvokeSpecial"
            )
            || !names_class(&invoker.class_name, INVOKERS_HOLDER)
            || &*invoker.method_name != "invokeExact_MT"
            || !names_class(&iwa.class_name, "java/lang/invoke/MethodHandle")
            || &*iwa.method_name != "invokeWithArguments"
            || iwa
                .method_descriptor
                .as_deref()
                .is_some_and(|d| d != IWA_ARRAY)
            || target.class_name.starts_with("java/lang/invoke/")
            || target.class_name.starts_with("java.lang.invoke.")
        {
            if crate::nbflags().debug_stackwalk
                && names_class(&iwa.class_name, "java/lang/invoke/MethodHandle")
                && &*iwa.method_name == "invokeWithArguments"
            {
                eprintln!(
                    "[SW-DBG] spun-iwa decline: window under {}.{}{}: {} | {} | {}",
                    iwa.class_name,
                    iwa.method_name,
                    iwa.method_descriptor.as_deref().unwrap_or("<no descriptor>"),
                    target.class_name.to_string() + "." + &target.method_name,
                    holder.class_name.to_string() + "." + &holder.method_name,
                    invoker.class_name.to_string() + "." + &invoker.method_name
                );
            }
            continue;
        }
        if crate::nbflags().debug_stackwalk
            && (target.class_id.is_none() || target.method_descriptor.is_none())
        {
            eprintln!(
                "[SW-DBG] spun-iwa decline: target {}.{} class_id={:?} descriptor={:?}",
                target.class_name,
                target.method_name,
                target.class_id.map(|c| c.as_u32()),
                target.method_descriptor
            );
        }
        let (Some(class_id), Some(descriptor), Some(holder_descriptor)) = (
            target.class_id,
            target.method_descriptor.clone(),
            holder.method_descriptor.as_deref(),
        ) else {
            continue;
        };
        let Some(holder_params) = descriptor_parameter_count(holder_descriptor) else {
            continue;
        };
        let Some(arity) = holder_params.checked_sub(1) else {
            continue;
        };
        out.push(SpunIwaSite {
            at: k + 2,
            class_id,
            name: std::sync::Arc::clone(&target.method_name),
            descriptor,
            holder: std::sync::Arc::clone(&holder.method_name),
            arity,
            row: None,
            holderless: false,
        });
    }
    // Interpreter round i7 wave 138, lane L3: the same chain with no holder
    // frame named (`SpunIwaSite::holderless`). A window of the chain above
    // never matches here: its holder is a `java/lang/invoke` class, and its
    // target sits under the holder, not under the invoker.
    for (k, window) in frames.windows(3).enumerate() {
        let [target, invoker, iwa] = window else {
            continue;
        };
        if !names_class(&invoker.class_name, INVOKERS_HOLDER)
            || &*invoker.method_name != "invokeExact_MT"
            || !is_iwa_array_frame(iwa)
            || target.class_name.starts_with("java/lang/invoke/")
            || target.class_name.starts_with("java.lang.invoke.")
        {
            continue;
        }
        let (Some(class_id), Some(descriptor)) = (target.class_id, target.method_descriptor.clone())
        else {
            continue;
        };
        out.push(SpunIwaSite {
            at: k + 1,
            class_id,
            name: std::sync::Arc::clone(&target.method_name),
            descriptor,
            holder: std::sync::Arc::from(""),
            arity: 0,
            row: None,
            holderless: true,
        });
    }
    // `splice_hidden_standin_frames` takes the insertions in frame order.
    out.sort_by_key(|site| site.at);
    out
}

/// Is `frame` an array-overload `MethodHandle.invokeWithArguments` frame (or
/// one whose descriptor the capture did not record)?
fn is_iwa_array_frame(frame: &StackTraceEntry) -> bool {
    names_class(&frame.class_name, "java/lang/invoke/MethodHandle")
        && &*frame.method_name == "invokeWithArguments"
        && frame
            .method_descriptor
            .as_deref()
            .is_none_or(|d| d == "([Ljava/lang/Object;)Ljava/lang/Object;")
}

/// The pairs `(frame index, row)` of a walk's served `invokeWithArguments`
/// frames (`frames`, INNERMOST first) and the thread's served rows
/// (`NativeContext::invoke_with_arguments_adapters`, `rows` of them, OUTERMOST
/// first), or `None` when the counts disagree.
///
/// Interpreter round i7 wave 137, lane L3 (page
/// `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`, wave 136's
/// "What the walk side still needs"). Every served row lists exactly one
/// array-overload frame (the `List` overload's row lists its own frame and
/// the array one's), and every row is an ancestor of the walk, so with equal
/// counts the k-th frame from the outside is the k-th row. Fail-closed: a row
/// that pushed nothing (listed by no capture), a JDK-body activation
/// (`CRATONVM_MH_IWA_JDK_BODY`, a real frame with no row), or a truncated
/// walk makes the counts disagree, and the walk then rebuilds the adapters
/// as before.
fn iwa_frame_rows(frames: &[StackTraceEntry], rows: usize) -> Option<Vec<(usize, usize)>> {
    let listed: Vec<usize> = frames
        .iter()
        .enumerate()
        .filter(|(_, f)| is_iwa_array_frame(f))
        .map(|(i, _)| i)
        .collect();
    if listed.len() != rows {
        return None;
    }
    // `listed` is innermost first; rows are outermost first.
    Some(
        listed
            .iter()
            .enumerate()
            .map(|(k, frame)| (*frame, rows - 1 - k))
            .collect(),
    )
}

/// Does `name` (internal `a/b/C` or dotted `a.b.C`) name the internal
/// `internal` class? Wave 117.
fn names_class(name: &str, internal: &str) -> bool {
    name.len() == internal.len()
        && name
            .bytes()
            .zip(internal.bytes())
            .all(|(a, b)| a == b || (a == b'.' && b == b'/'))
}

/// The number of parameters of the method descriptor `descriptor`, or `None`
/// when it does not parse.
fn descriptor_parameter_count(descriptor: &str) -> Option<usize> {
    let body = descriptor.strip_prefix('(')?;
    let (params, _) = body.split_once(')')?;
    let bytes = params.as_bytes();
    let mut count = 0usize;
    let mut pos = 0usize;
    while pos < bytes.len() {
        while bytes.get(pos) == Some(&b'[') {
            pos += 1;
        }
        match *bytes.get(pos)? {
            b'L' => {
                let end = bytes[pos..].iter().position(|b| *b == b';')?;
                pos += end + 1;
            }
            b'Z' | b'B' | b'S' | b'C' | b'I' | b'J' | b'F' | b'D' => pos += 1,
            _ => return None,
        }
        count += 1;
    }
    Some(count)
}

/// Interpreter round i6 wave 116, lane L3 (w115 hand-back: the switch listed
/// nothing and said nothing): under `CRATONVM_DEBUG_STACKWALK=1`, one
/// `[SW-DBG] spun-iwa decline: <step>: <why>` line per step that gives up.
fn spun_iwa_decline(why: impl FnOnce() -> String) {
    if crate::nbflags().debug_stackwalk {
        eprintln!("[SW-DBG] spun-iwa decline: {}", why());
    }
}

/// The object a Java call answered, if it answered one; otherwise a decline
/// line naming `step` and what came back instead.
fn answered_object_at(step: &str, result: MethodCallResult) -> Option<cratonvm_types::ObjectRef> {
    match result {
        Ok(Some(Value::Object(Some(o)))) => Some(o),
        other => {
            spun_iwa_decline(|| {
                let what = match &other {
                    Err(e) => format!("raised {e:?}"),
                    Ok(v) => format!("answered {v:?}"),
                };
                format!("{step}: {}", what.chars().take(300).collect::<String>())
            });
            None
        }
    }
}

/// The internal name of `obj`'s class, for a decline line.
fn spun_iwa_class_of(ctx: &dyn NativeContext, obj: cratonvm_types::ObjectRef) -> String {
    ctx.class_name_of_id(ctx.class_id_of_object(obj))
        .unwrap_or_else(|| "?".to_string())
}

/// `form.vmentry`'s declaring class id and internal name, or why not.
fn form_vmentry_class(
    ctx: &dyn NativeContext,
    form: cratonvm_types::ObjectRef,
) -> Result<(cratonvm_types::ClassId, String), String> {
    let Value::Object(Some(entry)) = ctx.get_field_by_name(form, "vmentry") else {
        return Err("form.vmentry is null".to_string());
    };
    let Value::Object(Some(clazz)) = ctx.get_field_by_name(entry, "clazz") else {
        return Err(format!(
            "vmentry ({}) has no clazz",
            spun_iwa_class_of(ctx, entry)
        ));
    };
    let Some(class_id) = crate::lang_class::mirror_class_id(ctx, clazz) else {
        return Err("vmentry.clazz names no class id".to_string());
    };
    let name = ctx.class_name_of_id(class_id).unwrap_or_default();
    Ok((class_id, name))
}

/// The class of the `vmentry` of `handle`'s `form`: a spun `LambdaForm$MH`
/// class. `what` names the handle in decline lines.
///
/// Interpreter round i6 wave 116, lane L3: a form whose `vmentry` is absent,
/// or is this VM's fabricated entry (the C33 `InvokerBytecodeGenerator`
/// bypass hands `prepare()` a `MemberName` of `java/lang/invoke/LambdaForm`
/// itself), and that is not compiled yet, is compiled now by the JDK's own
/// `LambdaForm.compileToBytecode()`. That is what the handle's constructor
/// does on HotSpot (`prepare()` with `COMPILE_THRESHOLD == 0`), and what the
/// body this walk stands in for would have done at its first call; it spins
/// the form's `LambdaForm$MH` class (`CRATONVM_IBG_SPIN_ON_MISS`, default
/// on). A form compiled already keeps its entry. Runs Java.
fn form_entry_class(
    ctx: &mut dyn NativeContext,
    handle: cratonvm_types::ObjectRef,
    what: &str,
) -> Option<cratonvm_types::ClassId> {
    let Value::Object(Some(form)) = ctx.get_field_by_name(handle, "form") else {
        spun_iwa_decline(|| format!("{what}: {} has no form", spun_iwa_class_of(&*ctx, handle)));
        return None;
    };
    lambda_form_entry_class(ctx, form, what)
}

/// [`form_entry_class`] of the `LambdaForm` `form` itself. Interpreter round
/// i6 wave 121, lane L3: split out for the linker step
/// ([`add_spun_linker_frames`]), whose form comes from no handle. Runs Java.
fn lambda_form_entry_class(
    ctx: &mut dyn NativeContext,
    form: cratonvm_types::ObjectRef,
    what: &str,
) -> Option<cratonvm_types::ClassId> {
    lambda_form_entry_class_of(ctx, form, what, "java/lang/invoke/LambdaForm$MH")
}

/// [`lambda_form_entry_class`] for an entry class named with `prefix`
/// (`LambdaForm$MH`, or `LambdaForm$DMH` for a customized direct handle's
/// form; interpreter round i7 wave 137, lane L3). Runs Java.
fn lambda_form_entry_class_of(
    ctx: &mut dyn NativeContext,
    form: cratonvm_types::ObjectRef,
    what: &str,
    prefix: &str,
) -> Option<cratonvm_types::ClassId> {
    let first = form_vmentry_class(&*ctx, form);
    if let Ok((class_id, name)) = &first {
        if name.starts_with(prefix) {
            return Some(*class_id);
        }
    }
    let compiled = matches!(ctx.get_field_by_name(form, "isCompiled"), Value::Int(v) if v != 0);
    spun_iwa_decline(|| {
        format!(
            "{what}: form {} entry {} (isCompiled={compiled}){}",
            spun_iwa_class_of(&*ctx, form),
            match &first {
                Ok((_, name)) => name.clone(),
                Err(why) => why.clone(),
            },
            if compiled { "" } else { "; compiling it" }
        )
    });
    if compiled {
        return None;
    }
    let pin = ctx.pin_native_root(form);
    let ran = ctx.invoke_virtual(form, "compileToBytecode", "()V", &[]);
    let form = ctx.read_native_pin(pin, form);
    ctx.unpin_native_roots(pin);
    if let Err(e) = &ran {
        spun_iwa_decline(|| format!("{what}: compileToBytecode raised {e:?}"));
        return None;
    }
    match form_vmentry_class(&*ctx, form) {
        Ok((class_id, name)) if name.starts_with(prefix) => Some(class_id),
        Ok((_, name)) => {
            spun_iwa_decline(|| format!("{what}: compiled, entry still {name}"));
            None
        }
        Err(why) => {
            spun_iwa_decline(|| format!("{what}: compiled, {why}"));
            None
        }
    }
}

/// `ctx.spun_lambda_form_frame(class_id, "invoke", descriptor)`, or a decline
/// line listing the class's methods when it answers nothing.
fn spun_iwa_frame(
    ctx: &dyn NativeContext,
    what: &str,
    class_id: cratonvm_types::ClassId,
    descriptor: &str,
) -> Option<StackTraceEntry> {
    let frame = ctx.spun_lambda_form_frame(class_id, "invoke", descriptor);
    if frame.is_none() {
        spun_iwa_decline(|| {
            let methods: Vec<String> = ctx
                .declared_methods(class_id)
                .into_iter()
                .map(|m| format!("{}{}", m.name, m.descriptor))
                .collect();
            format!(
                "{what}: {} has no invoke{descriptor} with one invokeBasic (methods: {})",
                ctx.class_name_of_id(class_id).unwrap_or_default(),
                methods.join(" ")
            )
        });
    }
    frame
}

/// `(Object x count)Object`.
fn generic_form_descriptor(count: usize) -> String {
    format!("({})Ljava/lang/Object;", "Ljava/lang/Object;".repeat(count))
}

/// The spun frames of `site`, INNERMOST first (the adapter's, when there is
/// one, then the spread invoker's), or `None` when any step fails. A site
/// paired with its served row reads the adapter the call ran
/// ([`spun_iwa_record_entries`]); without one, or when that declines, the
/// adapter is rebuilt from the target as before (waves 115-121).
fn spun_iwa_entries(ctx: &mut dyn NativeContext, site: &SpunIwaSite) -> Option<SpunIwaAnswer> {
    if site.holderless {
        // Wave 138: nothing to rebuild from (no holder says how the target
        // was looked up, nor its arity).
        let Some(row) = site.row else {
            spun_iwa_decline(|| format!("holderless site {}: no served row", site.name));
            return None;
        };
        return spun_iwa_record_entries(ctx, row, site);
    }
    if let Some(row) = site.row {
        if let Some(answer) = spun_iwa_record_entries(ctx, row, site) {
            return Some(answer);
        }
        spun_iwa_decline(|| format!("record: row {row} declined; rebuilding the adapter"));
    }
    spun_iwa_rebuilt_entries(ctx, site).map(|inserted| SpunIwaAnswer {
        inserted,
        holder: None,
    })
}

/// The adapter row `row` of `NativeContext::invoke_with_arguments_adapters`
/// noted, where it is now (read through the served call's pin, so it is
/// current after any Java call; the served call, an ancestor of the walk,
/// holds the pin for the walk's whole length).
fn spun_iwa_recorded_adapter(
    ctx: &dyn NativeContext,
    row: usize,
) -> Option<cratonvm_types::ObjectRef> {
    ctx.invoke_with_arguments_adapters().get(row).copied().flatten()
}

/// The parameter count of `handle`'s `type` (`MethodType.ptypes`).
fn handle_parameter_count(ctx: &dyn NativeContext, handle: cratonvm_types::ObjectRef) -> Option<usize> {
    let Value::Object(Some(method_type)) = ctx.get_field_by_name(handle, "type") else {
        return None;
    };
    let Value::Object(Some(ptypes)) = ctx.get_field_by_name(method_type, "ptypes") else {
        return None;
    };
    Some(ctx.array_length(ptypes))
}

/// Interpreter round i7 wave 137, lane L3 (page
/// `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`, residuals 1 and
/// 2): the spun frames of a served call from the adapter it ran (lane L4's
/// wave-136 record), not from a handle rebuilt from the target. JDK 25 runs
/// `spreadInvoker(0).invokeExact(adapter, args)` with `adapter =
/// asType(genericMethodType(n))`, so:
///
/// * `n` is the adapter's parameter count (right for a bound handle too,
///   whose target's arity is not `n`);
/// * the adapter frame is `adapter.form.vmentry` at its one `invokeBasic`,
///   customized or not: a `BoundMethodHandle` adapter (the `asType` of a direct
///   handle, or a bound handle itself, whose `asType` merges the conversions
///   into its own form) lists its spun `LambdaForm$MH` method;
/// * a direct adapter (the handle itself, or a `viewAsType` copy) lists no
///   adapter frame, and when its form is CUSTOMIZED (128+ calls) the spun
///   `LambdaForm$DMH` method replaces the `DirectMethodHandle$Holder` stand-in,
///   as HotSpot's walk shows (`L4W132IwaCustomization` `walk customized`).
///
/// A holderless site (`SpunIwaSite::holderless`, wave 138) also lists the
/// `LambdaForm$DMH` frame of the direct handle that reaches the target, as
/// its innermost inserted frame: the adapter's own form's entry when the
/// adapter is direct, else (a `BoundMethodHandle`: `bindTo`, `asType`,
/// `insertArguments`) the entry of its `argL0`'s form when that is a direct
/// handle of the walk's target method.
///
/// `None` (the caller then rebuilds) when any step declines. Runs Java.
fn spun_iwa_record_entries(
    ctx: &mut dyn NativeContext,
    row: usize,
    site: &SpunIwaSite,
) -> Option<SpunIwaAnswer> {
    let adapter = spun_iwa_recorded_adapter(&*ctx, row)?;
    let n = handle_parameter_count(&*ctx, adapter)?;
    // Interpreter round i7 wave 139, lane L3 (page
    // `i138-L4-spun-iwa-walk-pairs-served-rows-by-count`): the pairing is by
    // count, so a row that belongs to another served call can be paired with
    // this site when an unlisted row and an unrecorded frame offset each
    // other. The row must prove it is this site's: for a site with a holder
    // frame, its arity and the direct handle the adapter reaches (the adapter
    // itself, or its `argL0`) naming the walk's target. Otherwise the caller
    // rebuilds.
    if !site.holderless && !spun_iwa_row_matches_site(&*ctx, adapter, n, site) {
        spun_iwa_decline(|| format!("record: row {row} is not the call of {}", site.name));
        return None;
    }
    let arity = i32::try_from(n).ok()?;
    let generic = answered_object_at(
        "record: MethodType.genericMethodType",
        ctx.invoke_static(
            "java/lang/invoke/MethodType",
            "genericMethodType",
            "(I)Ljava/lang/invoke/MethodType;",
            &[Value::Int(arity)],
        ),
    )?;
    let generic_pin = ctx.pin_native_root(generic);
    let spread = spun_iwa_spread_frame(ctx, generic_pin, generic);
    ctx.unpin_native_roots(generic_pin);
    let spread = spread?;
    let (adapter_frame, holder) = spun_iwa_recorded_adapter_frames(ctx, row, site)?;
    let mut inserted = Vec::with_capacity(3);
    if site.holderless {
        // The direct handle's spun frame goes innermost, under the adapter's.
        inserted.push(holder?);
        inserted.extend(adapter_frame);
        inserted.push(spread);
        return Some(SpunIwaAnswer {
            inserted,
            holder: None,
        });
    }
    inserted.extend(adapter_frame);
    inserted.push(spread);
    Some(SpunIwaAnswer { inserted, holder })
}

/// The `LambdaForm$DMH` class-name prefix of a spun direct-handle entry.
const SPUN_DMH_PREFIX: &str = "java/lang/invoke/LambdaForm$DMH";

/// Is `handle` a `DirectMethodHandle` (or one of its nested subclasses)?
fn spun_iwa_is_direct(ctx: &dyn NativeContext, handle: cratonvm_types::ObjectRef) -> bool {
    ctx.class_name_of_id(ctx.class_id_of_object(handle))
        .is_some_and(|name| name.starts_with("java/lang/invoke/DirectMethodHandle"))
}

/// [`spun_iwa_record_entries`]' adapter half: `(adapter frame, holder frame)`.
/// For a site with a holder frame the second is its replacement (a
/// customized direct handle's spun entry), for a holderless site the direct
/// handle's spun frame, which it needs. Runs Java (a form not compiled yet is
/// compiled, as [`lambda_form_entry_class`] does).
fn spun_iwa_recorded_adapter_frames(
    ctx: &mut dyn NativeContext,
    row: usize,
    site: &SpunIwaSite,
) -> Option<(Option<StackTraceEntry>, Option<StackTraceEntry>)> {
    let adapter = spun_iwa_recorded_adapter(&*ctx, row)?;
    let direct = spun_iwa_is_direct(&*ctx, adapter);
    let Value::Object(Some(form)) = ctx.get_field_by_name(adapter, "form") else {
        spun_iwa_decline(|| "record: adapter has no form".to_string());
        return None;
    };
    if direct {
        if site.holderless {
            if !spun_iwa_handle_targets(&*ctx, adapter, site) {
                spun_iwa_decline(|| format!("record: holderless direct adapter is not {}", site.name));
                return None;
            }
        } else if !matches!(ctx.get_field_by_name(form, "customized"), Value::Object(Some(_))) {
            // The VM's holder stand-in is the frame (no adapter frame).
            return Some((None, None));
        }
        let what = if site.holderless {
            "record: holderless direct"
        } else {
            "record: customized direct"
        };
        let class_id = lambda_form_entry_class_of(ctx, form, what, SPUN_DMH_PREFIX)?;
        let frame = spun_iwa_recorded_vmentry_frame(&*ctx, row, false, class_id, what)?;
        return Some((None, Some(frame)));
    }
    let class_id = lambda_form_entry_class(ctx, form, "record: adapter")?;
    let frame = spun_iwa_recorded_vmentry_frame(&*ctx, row, false, class_id, "record: adapter")?;
    if !site.holderless {
        return Some((Some(frame), None));
    }
    // Wave 138: the direct handle the bound adapter's form reinvokes.
    let Some(target) = spun_iwa_recorded_target(&*ctx, row) else {
        spun_iwa_decline(|| "record: bound adapter's argL0 is not a direct handle".to_string());
        return None;
    };
    if !spun_iwa_handle_targets(&*ctx, target, site) {
        spun_iwa_decline(|| format!("record: bound adapter's argL0 is not {}", site.name));
        return None;
    }
    let Value::Object(Some(target_form)) = ctx.get_field_by_name(target, "form") else {
        spun_iwa_decline(|| "record: bound target has no form".to_string());
        return None;
    };
    let class_id = lambda_form_entry_class_of(ctx, target_form, "record: bound target", SPUN_DMH_PREFIX)?;
    let dmh = spun_iwa_recorded_vmentry_frame(&*ctx, row, true, class_id, "record: bound target")?;
    Some((Some(frame), Some(dmh)))
}

/// The recorded adapter's `argL0` when it is a direct handle (a
/// `BoundMethodHandle` species whose first bound value is the handle its
/// form reinvokes: `bindTo`, `insertArguments`, an `asType` adapter). Reads
/// only; read through the record's pin, so current after any Java call.
fn spun_iwa_recorded_target(
    ctx: &dyn NativeContext,
    row: usize,
) -> Option<cratonvm_types::ObjectRef> {
    let adapter = spun_iwa_recorded_adapter(ctx, row)?;
    let Value::Object(Some(target)) = ctx.get_field_by_name(adapter, "argL0") else {
        return None;
    };
    spun_iwa_is_direct(ctx, target).then_some(target)
}

/// Is the recorded `adapter` (of `n` parameters) the served call of `site`
/// (a site with a holder frame)? Its arity is the site's, and the direct
/// handle it runs -- itself, or a `BoundMethodHandle`'s direct `argL0` --
/// names the walk's target ([`spun_iwa_handle_targets`]). Reads only.
/// Interpreter round i7 wave 139, lane L3.
fn spun_iwa_row_matches_site(
    ctx: &dyn NativeContext,
    adapter: cratonvm_types::ObjectRef,
    n: usize,
    site: &SpunIwaSite,
) -> bool {
    if n != site.arity {
        return false;
    }
    let direct = if spun_iwa_is_direct(ctx, adapter) {
        adapter
    } else {
        match ctx.get_field_by_name(adapter, "argL0") {
            Value::Object(Some(target)) if spun_iwa_is_direct(ctx, target) => target,
            _ => return false,
        }
    };
    spun_iwa_handle_targets(ctx, direct, site)
}

/// Does the direct handle `handle` name the walk's target method of `site`
/// (its `member`'s name and class)? A bound value that is a direct handle
/// but not the one running (a `foldArguments` combiner, say) then declines.
fn spun_iwa_handle_targets(
    ctx: &dyn NativeContext,
    handle: cratonvm_types::ObjectRef,
    site: &SpunIwaSite,
) -> bool {
    let Value::Object(Some(member)) = ctx.get_field_by_name(handle, "member") else {
        return false;
    };
    let name = match ctx.get_field_by_name(member, "name") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    };
    let class_id = match ctx.get_field_by_name(member, "clazz") {
        Value::Object(Some(mirror)) => crate::lang_class::mirror_class_id(ctx, mirror),
        _ => None,
    };
    name.as_deref() == Some(&*site.name) && class_id == Some(site.class_id)
}

/// The frame of the recorded adapter's `form.vmentry` method (by its
/// `MemberName.name`, the one method of that name in `class_id`), at its one
/// `invokeBasic` (or, in a `LambdaForm$DMH` class, its one `linkTo*`) call;
/// with `target`, of the adapter's direct `argL0`'s form instead
/// ([`spun_iwa_recorded_target`], wave 138). Reads only.
fn spun_iwa_recorded_vmentry_frame(
    ctx: &dyn NativeContext,
    row: usize,
    target: bool,
    class_id: cratonvm_types::ClassId,
    what: &str,
) -> Option<StackTraceEntry> {
    let adapter = if target {
        spun_iwa_recorded_target(ctx, row)?
    } else {
        spun_iwa_recorded_adapter(ctx, row)?
    };
    let Value::Object(Some(form)) = ctx.get_field_by_name(adapter, "form") else {
        return None;
    };
    let Value::Object(Some(entry)) = ctx.get_field_by_name(form, "vmentry") else {
        spun_iwa_decline(|| format!("{what}: form.vmentry is null after compiling"));
        return None;
    };
    let name = match ctx.get_field_by_name(entry, "name") {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => return None,
    };
    let named: Vec<String> = ctx
        .declared_methods(class_id)
        .into_iter()
        .filter(|m| m.name == name)
        .map(|m| m.descriptor)
        .collect();
    let [descriptor] = named.as_slice() else {
        spun_iwa_decline(|| {
            format!(
                "{what}: {} declares {} methods named {name}",
                ctx.class_name_of_id(class_id).unwrap_or_default(),
                named.len()
            )
        });
        return None;
    };
    let frame = ctx.spun_lambda_form_frame(class_id, &name, descriptor).or_else(|| {
        // Wave 139: a `LambdaForm$MH` adapter with bound filter handles (shape 2).
        spun_iwa_target_call_frame(ctx, form, class_id, &name, descriptor)
    });
    if frame.is_none() {
        spun_iwa_decline(|| {
            format!(
                "{what}: {}.{name}{descriptor} has no single invokeBasic/linkTo call",
                ctx.class_name_of_id(class_id).unwrap_or_default()
            )
        });
    }
    frame
}

/// The waves 115-121 answer for `site`: the adapter rebuilt from an
/// `IMPL_LOOKUP` handle of the target (right for an uncustomized direct
/// handle only).
fn spun_iwa_rebuilt_entries(
    ctx: &mut dyn NativeContext,
    site: &SpunIwaSite,
) -> Option<Vec<StackTraceEntry>> {
    let arity = i32::try_from(site.arity).ok()?;
    let generic = answered_object_at(
        "MethodType.genericMethodType",
        ctx.invoke_static(
            "java/lang/invoke/MethodType",
            "genericMethodType",
            "(I)Ljava/lang/invoke/MethodType;",
            &[Value::Int(arity)],
        ),
    )?;
    let generic_pin = ctx.pin_native_root(generic);
    // Both halves are asked even when one fails, so a decline names every
    // step that gave up (wave 116).
    let spread = spun_iwa_spread_frame(ctx, generic_pin, generic);
    let adapter = spun_iwa_adapter_frame(ctx, site, generic_pin, generic);
    ctx.unpin_native_roots(generic_pin);
    let (Some(spread), Some(adapter)) = (spread, adapter) else {
        spun_iwa_decline(|| {
            format!(
                "site {}.{}{} (n={}): nothing listed",
                ctx.class_name_of_id(site.class_id).unwrap_or_default(),
                site.name,
                site.descriptor,
                site.arity
            )
        });
        return None;
    };
    let mut out = Vec::with_capacity(2);
    out.extend(adapter);
    out.push(spread);
    Some(out)
}

/// The spread invoker's frame: JDK 25's
/// `genericMethodType(n).invokers().spreadInvoker(0)`, its form's entry.
fn spun_iwa_spread_frame(
    ctx: &mut dyn NativeContext,
    generic_pin: usize,
    generic: cratonvm_types::ObjectRef,
) -> Option<StackTraceEntry> {
    let generic_now = ctx.read_native_pin(generic_pin, generic);
    let invokers = answered_object_at(
        "spread: MethodType.invokers()",
        ctx.invoke_virtual(
            generic_now,
            "invokers",
            "()Ljava/lang/invoke/Invokers;",
            &[],
        ),
    )?;
    let spread = answered_object_at(
        "spread: Invokers.spreadInvoker(0)",
        ctx.invoke_virtual(
            invokers,
            "spreadInvoker",
            "(I)Ljava/lang/invoke/MethodHandle;",
            &[Value::Int(0)],
        ),
    )?;
    let class_id = form_entry_class(ctx, spread, "spread")?;
    spun_iwa_frame(&*ctx, "spread", class_id, &generic_form_descriptor(3))
}

/// The `asType` adapter's frame: `Some(None)` when the handle needs none
/// (`asType` answers the handle, or a `viewAsType` copy that runs the
/// handle's own form, for a type that only widens references).
fn spun_iwa_adapter_frame(
    ctx: &mut dyn NativeContext,
    site: &SpunIwaSite,
    generic_pin: usize,
    generic: cratonvm_types::ObjectRef,
) -> Option<Option<StackTraceEntry>> {
    let handle = spun_iwa_direct_handle(ctx, site)?;
    let handle_pin = ctx.pin_native_root(handle);
    let generic_now = ctx.read_native_pin(generic_pin, generic);
    let adapted = answered_object_at(
        "adapter: asType(genericMethodType(n))",
        ctx.invoke_virtual(
            handle,
            "asType",
            "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
            &[Value::Object(Some(generic_now))],
        ),
    );
    let handle = ctx.read_native_pin(handle_pin, handle);
    ctx.unpin_native_roots(handle_pin);
    let adapted = adapted?;
    let same_form = adapted == handle
        || matches!(
            (ctx.get_field_by_name(adapted, "form"), ctx.get_field_by_name(handle, "form")),
            (Value::Object(Some(a)), Value::Object(Some(b))) if a == b
        );
    if same_form {
        return Some(None);
    }
    // Pinned: `form_entry_class` may compile the form (Java).
    let adapted_pin = ctx.pin_native_root(adapted);
    let class_id = form_entry_class(ctx, adapted, "adapter");
    let adapted = ctx.read_native_pin(adapted_pin, adapted);
    ctx.unpin_native_roots(adapted_pin);
    let class_id = class_id?;
    let descriptor = generic_form_descriptor(site.arity + 1);
    if let Some(frame) = ctx.spun_lambda_form_frame(class_id, "invoke", &descriptor) {
        return Some(Some(frame));
    }
    // Wave 139: an adapter with bound filters makes several `invokeBasic`
    // calls; the frame stands at the one that calls the target.
    if let Value::Object(Some(form)) = ctx.get_field_by_name(adapted, "form") {
        if let Some(frame) = spun_iwa_target_call_frame(&*ctx, form, class_id, "invoke", &descriptor) {
            return Some(Some(frame));
        }
    }
    spun_iwa_frame(&*ctx, "adapter", class_id, &descriptor).map(Some)
}

/// Interpreter round i7 wave 139, lane L3 (page
/// `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`, shape 2): the
/// frame of the spun method `name descriptor` of `class_id` (the entry of
/// the `BoundMethodHandle` form `form`) at its call of the handle's `argL0`,
/// the target. `MethodHandleImpl.makePairwiseConvertByEditor` binds each
/// argument or return conversion as a further `L` value and calls it with
/// `invokeBasic` too, so an `(int)int` handle's `asType(genericMethodType(1))`
/// makes three such calls (unbox, target, box), where the single-call rule of
/// `spun_lambda_form_frame` answers nothing. The target call's position among
/// the form's `invokeBasic` names is its position among the method's calls
/// (`InvokerBytecodeGenerator` emits the names in order), which the VM checks
/// against the count. Reads only.
fn spun_iwa_target_call_frame(
    ctx: &dyn NativeContext,
    form: cratonvm_types::ObjectRef,
    class_id: cratonvm_types::ClassId,
    name: &str,
    descriptor: &str,
) -> Option<StackTraceEntry> {
    let (ordinal, count) = form_target_call_ordinal(ctx, form)?;
    let frame = ctx.spun_lambda_form_frame_at(class_id, name, descriptor, ordinal, count);
    if frame.is_none() {
        spun_iwa_decline(|| {
            format!(
                "target call: {}.{name}{descriptor} is not {count} invokeBasic calls (target #{ordinal})",
                ctx.class_name_of_id(class_id).unwrap_or_default()
            )
        });
    }
    frame
}

/// `(ordinal, count)`: the position of the call on the `BoundMethodHandle`'s
/// `argL0` among `form.names`' `MethodHandle.invokeBasic` names, and their
/// number; `None` when no such call, or several, are found. Reads only.
fn form_target_call_ordinal(
    ctx: &dyn NativeContext,
    form: cratonvm_types::ObjectRef,
) -> Option<(usize, usize)> {
    let Value::Object(Some(names)) = ctx.get_field_by_name(form, "names") else {
        return None;
    };
    let mut count = 0usize;
    let mut target = None;
    for i in 0..ctx.array_length(names) {
        let Value::Object(Some(name)) = ctx.get_array_element(names, i) else {
            continue;
        };
        if lambda_form_name_function(ctx, name).as_deref() != Some("invokeBasic") {
            continue;
        }
        let receiver = match ctx.get_field_by_name(name, "arguments") {
            Value::Object(Some(args)) if ctx.array_length(args) > 0 => {
                match ctx.get_array_element(args, 0) {
                    Value::Object(Some(r)) => Some(r),
                    _ => None,
                }
            }
            _ => None,
        };
        if receiver.is_some_and(|r| lambda_form_name_function(ctx, r).as_deref() == Some("argL0")) {
            if target.is_some() {
                return None;
            }
            target = Some(count);
        }
        count += 1;
    }
    target.map(|t| (t, count))
}

/// The name of a `LambdaForm.Name`'s function's member
/// (`name.function.member.name`): `invokeBasic` for a handle call, the field
/// name (`argL0`) for a bound-value getter. `None` for anything else (a
/// constant argument that is not a `Name` reads no such fields).
fn lambda_form_name_function(ctx: &dyn NativeContext, name: cratonvm_types::ObjectRef) -> Option<String> {
    let is_name = ctx
        .class_name_of_id(ctx.class_id_of_object(name))
        .is_some_and(|c| c == "java/lang/invoke/LambdaForm$Name");
    if !is_name {
        return None;
    }
    let Value::Object(Some(function)) = ctx.get_field_by_name(name, "function") else {
        return None;
    };
    let Value::Object(Some(member)) = ctx.get_field_by_name(function, "member") else {
        return None;
    };
    match ctx.get_field_by_name(member, "name") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// A direct handle of `site`'s target through the JDK's `IMPL_LOOKUP` (its
/// type is the type a handle the program looked the target up with has, for
/// every target but one reached through a subclass receiver).
fn spun_iwa_direct_handle(
    ctx: &mut dyn NativeContext,
    site: &SpunIwaSite,
) -> Option<cratonvm_types::ObjectRef> {
    let lookup = ctx
        .class_id_by_name("java/lang/invoke/MethodHandles$Lookup")
        .and_then(|class| {
            let slot = ctx.static_field_index_by_name(class, "IMPL_LOOKUP")?;
            match ctx.get_static_field(class, slot) {
                Value::Object(Some(lookup)) => Some(lookup),
                _ => None,
            }
        });
    let Some(lookup) = lookup else {
        spun_iwa_decline(|| "adapter: no MethodHandles$Lookup.IMPL_LOOKUP".to_string());
        return None;
    };
    let base = ctx.pin_native_root(lookup);
    let answer = spun_iwa_find(ctx, site, base, lookup);
    ctx.unpin_native_roots(base);
    answer
}

/// [`spun_iwa_direct_handle`] with the trusted lookup pinned at `base`; every
/// pin it takes goes with the caller's `unpin_native_roots(base)`.
fn spun_iwa_find(
    ctx: &mut dyn NativeContext,
    site: &SpunIwaSite,
    base: usize,
    lookup: cratonvm_types::ObjectRef,
) -> Option<cratonvm_types::ObjectRef> {
    const CLASS: &str = "Ljava/lang/Class;";
    const STRING: &str = "Ljava/lang/String;";
    const TYPE: &str = "Ljava/lang/invoke/MethodType;";
    const HANDLE: &str = "Ljava/lang/invoke/MethodHandle;";
    let mirror = ctx.get_class_mirror(site.class_id);
    let mirror_pin = ctx.pin_native_root(mirror);
    let descriptor = ctx.create_string_uninterned(&site.descriptor);
    let mirror = ctx.read_native_pin(mirror_pin, mirror);
    let loader = match ctx.get_field_by_name(mirror, "classLoader") {
        Value::Object(o) => Value::Object(o),
        _ => Value::Object(None),
    };
    let found = ctx.invoke_static(
        "java/lang/invoke/MethodType",
        "fromMethodDescriptorString",
        "(Ljava/lang/String;Ljava/lang/ClassLoader;)Ljava/lang/invoke/MethodType;",
        &[Value::Object(Some(descriptor)), loader],
    );
    let method_type = answered_object_at("adapter: MethodType.fromMethodDescriptorString", found)?;
    let type_pin = ctx.pin_native_root(method_type);
    if &*site.holder == "newInvokeSpecial" {
        let lookup = ctx.read_native_pin(base, lookup);
        let mirror = ctx.read_native_pin(mirror_pin, mirror);
        let method_type = ctx.read_native_pin(type_pin, method_type);
        let found = ctx.invoke_virtual(
            lookup,
            "findConstructor",
            &format!("({CLASS}{TYPE}){HANDLE}"),
            &[Value::Object(Some(mirror)), Value::Object(Some(method_type))],
        );
        return answered_object_at("adapter: IMPL_LOOKUP.findConstructor", found);
    }
    let finder = if &*site.holder == "invokeStatic" {
        "findStatic"
    } else {
        "findVirtual"
    };
    let name = ctx.intern_string(&site.name);
    let lookup = ctx.read_native_pin(base, lookup);
    let mirror = ctx.read_native_pin(mirror_pin, mirror);
    let method_type = ctx.read_native_pin(type_pin, method_type);
    let found = ctx.invoke_virtual(
        lookup,
        finder,
        &format!("({CLASS}{STRING}{TYPE}){HANDLE}"),
        &[
            Value::Object(Some(mirror)),
            Value::Object(Some(name)),
            Value::Object(Some(method_type)),
        ],
    );
    answered_object_at(finder, found)
}

/// Is a frame of `class_name` a reflection frame, which a `StackWalker`
/// without `SHOW_REFLECT_FRAMES` or `SHOW_HIDDEN_FRAMES` skips? JDK 25's
/// `StackStreamFactory.isReflectionFrame`: `Method`, `Constructor`, and a
/// `MethodAccessor` or `ConstructorAccessor` implementation, recognised here
/// by the names the JDK gives them in `jdk.internal.reflect`
/// (`DirectMethodHandleAccessor`, `NativeMethodAccessorImpl`,
/// `GeneratedMethodAccessor1`, ...). Interpreter round i1 wave 43, lane L3:
/// the VM's captures list a reflective call's JDK frames since this wave.
pub(crate) fn is_reflection_frame_class(class_name: &str) -> bool {
    if class_name == "java/lang/reflect/Method" || class_name == "java/lang/reflect/Constructor" {
        return true;
    }
    let Some(simple) = class_name.strip_prefix("jdk/internal/reflect/") else {
        return false;
    };
    // A nested class counts by its own name (`DirectMethodHandleAccessor$
    // NativeAccessor extends MethodAccessorImpl`).
    let own = simple.rsplit('$').next().unwrap_or(simple);
    !simple.contains('/')
        && (own.ends_with("Accessor")
            || own.ends_with("AccessorImpl")
            || own.starts_with("GeneratedMethodAccessor")
            || own.starts_with("GeneratedConstructorAccessor")
            || own.starts_with("GeneratedSerializationConstructorAccessor"))
        && simple != "MethodAccessor"
        && simple != "ConstructorAccessor"
}

/// Does the `StackWalker` `walker` show reflection frames
/// (`SHOW_REFLECT_FRAMES` or `SHOW_HIDDEN_FRAMES` in its options, JDK 25's
/// `AbstractStackWalker.skipReflectionFrames` negated)? Asked only when a
/// walk holds a reflection frame. Fails open, as
/// [`walker_shows_hidden_frames`] does. May run Java
/// ([`walker_options_contain`]); the caller keeps `walker` pinned.
pub(crate) fn walker_shows_reflection_frames(
    ctx: &mut dyn NativeContext,
    walker: Option<cratonvm_types::ObjectRef>,
) -> bool {
    let Some(walker) = walker else {
        return true;
    };
    let options = match ctx.get_field_by_name(walker, "options") {
        Value::Object(Some(o)) => o,
        _ => return true,
    };
    let pin = ctx.pin_native_root(options);
    let mut shows = false;
    for constant in ["SHOW_REFLECT_FRAMES", "SHOW_HIDDEN_FRAMES"] {
        let options = ctx.read_native_pin(pin, options);
        if walker_options_contain(ctx, options, constant).unwrap_or(true) {
            shows = true;
            break;
        }
    }
    ctx.unpin_native_roots(pin);
    shows
}

/// `CRATONVM_STACKWALK_REFLECTION_SCREEN_EXACT` (JIT round 21 wave 1, lane
/// walk; a HotSpot-parity fix, default ON, `=0` restores the name rule): the
/// native walks' reflection-frame screen ([`drop_reflection_frames`]) and the
/// `getCallerClass` finder rule (`stack_walker::caller_class_by_finder_rule`)
/// answer JDK 25's `StackStreamFactory.isReflectionFrame` itself for a frame
/// whose class the capture names by id: `Method`, `Constructor`, or a class
/// assignable to `MethodAccessor` / `ConstructorAccessor` -- the same
/// resolved-pair rule the reviewed `isReflectionFrame` Intrinsic answers
/// ([`reflection_accessor_assignable`]). The name rule
/// ([`is_reflection_frame_class`]) is a guess at that answer from the class's
/// simple name: it calls every `jdk.internal.reflect` class whose name ends in
/// `Accessor` / `AccessorImpl` a reflection frame (the field accessors,
/// `FieldAccessorImpl` and its `MethodHandle*FieldAccessorImpl` family, are
/// neither interface's) and misses an accessor named otherwise (an adapter
/// such as `CsMethodAccessorAdapter`, where the JDK has one). A frame with no
/// class id, a lambda proxy, or a JDK whose two interfaces do not resolve by
/// name keeps the name rule. Only `jdk.internal.reflect` classes are asked
/// (both interfaces live in that unexported package), so an ordinary walk
/// asks nothing more than before. Read per call from the VM's flag snapshot.
pub(crate) fn stackwalk_reflection_screen_exact_enabled() -> bool {
    // Read through the per-VM flag snapshot (`NativeFlags`), not a process
    // `OnceLock` (r21 review rev-native item 5).
    crate::nbflags().stackwalk_reflection_screen_exact
}

/// Is the walk entry `entry` a reflection frame, by JDK 25's
/// `isReflectionFrame` ([`stackwalk_reflection_screen_exact_enabled`]); the
/// name rule ([`is_reflection_frame_class`]) with the switch off or where the
/// exact rule cannot be asked. Allocates nothing, runs no Java, loads nothing.
pub(crate) fn is_reflection_frame_entry(ctx: &dyn NativeContext, entry: &StackTraceEntry) -> bool {
    let name: &str = &entry.class_name;
    if !stackwalk_reflection_screen_exact_enabled() {
        return is_reflection_frame_class(name);
    }
    if name == "java/lang/reflect/Method" || name == "java/lang/reflect/Constructor" {
        return true;
    }
    if !name.starts_with("jdk/internal/reflect/") {
        return false;
    }
    let Some(cid) = entry.class_id else {
        return is_reflection_frame_class(name);
    };
    // A lambda proxy's synthetic id: `is_subclass` may load its functional
    // interface. The name rule answers such a name (it holds `/0x`) `false`.
    if cid.as_u32() >= 0x8000_0000 {
        return is_reflection_frame_class(name);
    }
    let (Some(method_accessor), Some(ctor_accessor)) = (
        ctx.class_id_by_name(REFLECTION_ACCESSORS[0]),
        ctx.class_id_by_name(REFLECTION_ACCESSORS[1]),
    ) else {
        return is_reflection_frame_class(name);
    };
    reflection_accessor_assignable(ctx, cid, method_accessor, REFLECTION_ACCESSORS[0])
        || reflection_accessor_assignable(ctx, cid, ctor_accessor, REFLECTION_ACCESSORS[1])
}

/// `frames` without the reflection frames ([`is_reflection_frame_entry`])
/// unless the walker shows them (`shows`, asked only when there is one).
/// Interpreter round i1 wave 43, lane L3.
pub(crate) fn drop_reflection_frames(
    ctx: &mut dyn NativeContext,
    frames: Vec<StackTraceEntry>,
    shows: impl FnOnce(&mut dyn NativeContext) -> bool,
) -> Vec<StackTraceEntry> {
    if !frames
        .iter()
        .any(|frame| is_reflection_frame_entry(&*ctx, frame))
        || shows(ctx)
    {
        return frames;
    }
    let screen: &dyn NativeContext = &*ctx;
    frames
        .into_iter()
        .filter(|frame| !is_reflection_frame_entry(screen, frame))
        .collect()
}

/// `capture_stack_trace` order is outer→inner; StackWalker streams are
/// inner→outer with VM-internal walker frames stripped from the inner end.
/// `fetchStackFrames` reuses the same ordering with `anchor` as an index into
/// this vector (not into the raw reversed physical trace).
///
/// `pub(crate)` so `phases_late::p59_sw_walk`/`p59_sw_for_each` — the
/// primary, always-registered `StackWalker.walk`/`forEach` natives, which do
/// NOT route through `AbstractStackWalker.callStackWalk` — share this same
/// ordering instead of handing the caller the raw outer→inner
/// `capture_stack_trace` order.
pub(crate) fn ordered_stack_walk_frames(trace: &[StackTraceEntry]) -> Vec<StackTraceEntry> {
    let mut iter = trace.iter().rev().peekable();
    while let Some(e) = iter.peek() {
        if stack_walk_skip_internals(&e.class_name, &e.method_name) {
            iter.next();
        } else {
            break;
        }
    }
    iter.cloned().collect()
}

/// [`ordered_stack_walk_frames`] taking the trace by value: the same list,
/// with the entries moved instead of cloned (four `Arc` clones each). JIT
/// round 19 wave 2, lane throws2 (WALK17-3 step 1,
/// `CRATONVM_STACKWALK_LEAN_CAPTURE=1`).
pub(crate) fn ordered_stack_walk_frames_owned(trace: Vec<StackTraceEntry>) -> Vec<StackTraceEntry> {
    let mut iter = trace.into_iter().rev().peekable();
    while let Some(e) = iter.peek() {
        if stack_walk_skip_internals(&e.class_name, &e.method_name) {
            iter.next();
        } else {
            break;
        }
    }
    iter.collect()
}

/// `entry` with its deferred line resolved (WALK17-3 step 1): borrowed as it
/// is when nothing was deferred (every entry of an eager capture), else a
/// resolved copy for the one `StackFrameInfo` being written.
pub(crate) fn resolved_walk_entry<'e>(
    ctx: &dyn NativeContext,
    entry: &'e StackTraceEntry,
) -> std::borrow::Cow<'e, StackTraceEntry> {
    if entry.line_number == cratonvm_native_api::LINE_NUMBER_DEFERRED
        || entry.line_number == cratonvm_native_api::LINE_NUMBER_DEFERRED_AT_BCI_ZERO
    {
        let mut resolved = entry.clone();
        ctx.resolve_deferred_stack_walk_line(&mut resolved);
        std::borrow::Cow::Owned(resolved)
    } else {
        std::borrow::Cow::Borrowed(entry)
    }
}

/// `AbstractStackWalker.callStackWalk(long mode, int skip, int batch,
///                                    int startIndex,
///                                    Object[] frameBuffer,
///                                    Class<?>[] classBuffer)`.
///
/// Our interpretation:
/// * Capture the live call stack via `NativeContext::capture_stack_trace`.
/// * Skip `skip` frames (plus the two wrapper frames the JDK calling
///   convention synthesizes, which we approximate by skipping 2 extra
///   when `skip == 0`).
/// * Write up to `frameBuffer.length - startIndex` `StackFrameInfo`
///   entries into `frameBuffer` starting at `startIndex`.
/// * Return the number of frames written, cast to the anchor long. The
///   real JDK returns an anchor that doubles as a cursor; a positive
///   non-zero anchor indicates "more data available" which is exactly
///   what we want.
pub(crate) fn native_call_stack_walk(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_call_stack_walk_of(ctx, args, None)
}

/// The trace a walk by `walker` (a `java.lang.StackWalker`) reads, OUTERMOST
/// first as the capture: its continuation's frames when it was built on one
/// (`StackWalker`'s `continuation` field: `Continuation.getStackTrace`, which
/// `VirtualThread.getStackTrace` asks of an unmounted virtual thread;
/// `NativeContext::continuation_stack_trace`), else the calling thread's own.
pub(crate) fn walker_trace(
    ctx: &mut dyn NativeContext,
    walker: Option<cratonvm_types::ObjectRef>,
) -> Vec<StackTraceEntry> {
    if let Some(walker) = walker {
        if let Value::Object(Some(cont)) = ctx.get_field_by_name(walker, "continuation") {
            if let Some(trace) = ctx.continuation_stack_trace(cont) {
                return trace;
            }
        }
    }
    ctx.capture_stack_walk_trace()
}

/// [`walker_trace`] with the calling thread's lines deferred (WALK21-4,
/// [`stackwalk_deferred_lines_enabled`]): a continuation's trace is taken as
/// [`walker_trace`] takes it (eager), the thread's own through
/// `NativeContext::capture_stack_walk_trace_deferred`. Every entry must pass
/// through [`resolved_walk_entry`] before its line is read.
pub(crate) fn walker_trace_deferred(
    ctx: &mut dyn NativeContext,
    walker: Option<cratonvm_types::ObjectRef>,
) -> Vec<StackTraceEntry> {
    if let Some(walker) = walker {
        if let Value::Object(Some(_)) = ctx.get_field_by_name(walker, "continuation") {
            return walker_trace(ctx, Some(walker));
        }
    }
    ctx.capture_stack_walk_trace_deferred()
}

/// [`native_call_stack_walk`] over `trace` (OUTERMOST first, as the capture)
/// instead of the calling thread's own stack: a walk of an unmounted
/// continuation (`NativeContext::continuation_stack_trace`).
fn native_call_stack_walk_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    trace: Option<Vec<StackTraceEntry>>,
) -> MethodCallResult {
    // args[0] = this (AbstractStackWalker)
    // args[1] = mode (Long)
    // args[2] = skip (Int)
    // args[3] = batch (Int)
    // args[4] = startIndex (Int)
    // args[5] = frameBuffer (Object[])
    // args[6] = classBuffer (Class[] or null)
    let this = match args.first() {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let mode_long = match args.get(1) {
        Some(Value::Long(n)) => *n,
        Some(Value::Int(n)) => *n as i64,
        _ => 0,
    };
    let skip = match args.get(2) {
        Some(Value::Int(n)) => (*n).max(0) as usize,
        _ => 0,
    };
    let batch = match args.get(3) {
        Some(Value::Int(n)) => (*n).max(0) as usize,
        _ => 0,
    };
    let start_index = match args.get(4) {
        Some(Value::Int(n)) => (*n).max(0) as usize,
        _ => 0,
    };
    let frame_buffer = match args.get(5) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };

    // The walker's capture: an old activation has no line (interpreter round
    // i1 wave 39, lane L3). JIT round 19 wave 2 (lane throws2, WALK17-3 step
    // 1; `CRATONVM_STACKWALK_LEAN_CAPTURE=1`): with its lines deferred to the
    // entries actually written, and the ordered list built without a clone.
    let lean = ctx.stack_walk_lean_capture();
    let trace = match trace {
        Some(trace) => trace,
        None if lean => ctx.capture_stack_walk_trace_deferred(),
        None => ctx.capture_stack_walk_trace(),
    };
    if crate::nbflags().debug_stackwalk {
        eprintln!(
            "[SW-DBG] callStackWalk capture len={} skip={} batch={}",
            trace.len(),
            skip,
            batch
        );
        for (i, e) in trace.iter().enumerate() {
            eprintln!("  [{}] {}.{}", i, e.class_name, e.method_name);
        }
    }
    let buf_len = ctx.array_length(frame_buffer);
    let slack = buf_len.saturating_sub(start_index);
    let capacity = if batch == 0 { slack } else { slack.min(batch) };

    // Hidden frames (wave 35). The JDK has already folded the walker's
    // `SHOW_HIDDEN_FRAMES` option into `mode` (`StackStreamFactory.
    // toStackWalkMode`, bit `SHOW_HIDDEN_FRAMES = 0x20`), so no Java runs to
    // read it (interpreter round i1 wave 37; wave 35 ran the options set's
    // `toString`).
    //
    // Interpreter round i4 wave 92, lane L2: `-XX:+ShowHiddenFrames` shows
    // them in every mode (HotSpot's `StackWalk::fill_in_frames` skips a
    // hidden frame only `!ShowHiddenFrames`), the stand-ins included; a
    // `CallerClassFinder` walk still skips them in Java, on the member bit
    // `class_frame_member_flags` sets from the method's own hiddenness, so
    // for it the flag lists only the real hidden frames (what the listing
    // answer keeps under it), no stand-in: `getCallerClass` is unaffected.
    let shows_hidden = mode_long & STACK_WALK_MODE_SHOW_HIDDEN_FRAMES != 0
        || (ctx.show_hidden_frames_flag() && !walk_is_caller_class_finder(ctx, this));
    // Interpreter round i6 wave 117, lane L4: a walk that lists hidden frames
    // ends the method-handle lanes' "hidden frames never observed" latch for
    // this VM (`CRATONVM_MH_REAL_DIRECT_HIDDEN_LATCH`). One atomic load after
    // the first time.
    if shows_hidden {
        ctx.note_hidden_frames_observed();
    }
    // Wave 48, lane L3: the hidden method-handle frames a native stands for.
    let ordered = if lean {
        ordered_stack_walk_frames_owned(trace)
    } else {
        ordered_stack_walk_frames(&trace)
    };
    let (ordered, _) = add_hidden_standin_frames(ctx, ordered, |_| shows_hidden);
    // Interpreter round i6 wave 115, lane L3 (`CRATONVM_STANDIN_SPUN_IWA_FRAMES`,
    // default off): a served `invokeWithArguments`'s spun `LambdaForm$MH`
    // frames, asked of the JDK, so Java runs: the walker and the buffer are
    // pinned across it and read back.
    // Wave 117 (hand-back: the step never ran and said nothing): the gate's
    // three inputs, once per walk that shows hidden frames.
    if crate::nbflags().debug_stackwalk && shows_hidden {
        eprintln!(
            "[SW-DBG] spun-iwa gate shows_hidden=true switch={} lists_standins={} frames={}",
            crate::nbflags().standin_spun_iwa_frames,
            ctx.lists_hidden_standin_frames(),
            ordered.len()
        );
    }
    // Wave 121: either spun step (`add_spun_lambda_form_frames`).
    let (ordered, this, frame_buffer) = if shows_hidden
        && spun_lambda_form_steps_on()
        && ctx.lists_hidden_standin_frames()
    {
        let fb_pin = ctx.pin_native_root(frame_buffer);
        let this_pin = this.map(|t| ctx.pin_native_root(t));
        let ordered = add_spun_lambda_form_frames(ctx, ordered);
        let frame_buffer = ctx.read_native_pin(fb_pin, frame_buffer);
        let this = match (this, this_pin) {
            (Some(t), Some(p)) => Some(ctx.read_native_pin(p, t)),
            _ => None,
        };
        ctx.unpin_native_roots(fb_pin);
        (ordered, this, frame_buffer)
    } else {
        (ordered, this, frame_buffer)
    };
    let ordered = Rc::new(drop_hidden_frames(ctx, ordered, |_| shows_hidden));
    if crate::nbflags().debug_stackwalk {
        eprintln!("[SW-DBG] ordered len={}", ordered.len());
        for (i, e) in ordered.iter().enumerate() {
            eprintln!("  ord[{}] {}.{}", i, e.class_name, e.method_name);
        }
    }
    let mut written = 0usize;
    let mut pos = skip.min(ordered.len());
    // Read the walker's RETAIN_CLASS_REFERENCE option once, before the
    // allocating loop: `this` is the AbstractStackWalker and its `walker` field
    // is the only place the option is reachable from.
    let retain_class_ref = walker_retains_class_ref(ctx, this);
    // GC-SAFETY: `populate_sfi` allocates, so a moving GC can relocate the
    // `frame_buffer` array (and the SFI elements already stored in it — they
    // stay reachable through the pinned array). Pin it and re-read the
    // forwarded reference before every `set_array_element`.
    let fb_pin = ctx.pin_native_root(frame_buffer);
    // gc-common w19-g: the walker is pinned across the same loop and re-read
    // before it is handed to `doStackWalk` below as the receiver; it was
    // passed at its pre-loop address.
    let this_pin = this.map(|t| ctx.pin_native_root(t));
    let mut frame_buffer = frame_buffer;
    let class_info_only = mode_long & STACK_WALK_MODE_CLASS_INFO_ONLY != 0;
    while written < capacity && pos < ordered.len() {
        if class_info_only {
            frame_buffer = ctx.read_native_pin(fb_pin, frame_buffer);
            if fill_class_frame_info(ctx, frame_buffer, start_index + written, &ordered[pos]) {
                pos += 1;
                written += 1;
                continue;
            }
        }
        let entry = resolved_walk_entry(ctx, &ordered[pos]);
        let sfi = populate_sfi(ctx, &entry, retain_class_ref);
        frame_buffer = ctx.read_native_pin(fb_pin, frame_buffer);
        ctx.set_array_element(
            frame_buffer,
            start_index + written,
            Value::Object(Some(sfi?)),
        );
        pos += 1;
        written += 1;
    }
    let this = match (this, this_pin) {
        (Some(t), Some(p)) => Some(ctx.read_native_pin(p, t)),
        _ => None,
    };
    ctx.unpin_native_roots(fb_pin);
    let consumed = pos;

    // Real-JDK contract: `callStackWalk` is supposed to invoke
    // `this.doStackWalk(anchor, skip, batch, startIndex, endIndex)` and
    // return whatever doStackWalk returns. doStackWalk in turn binds the
    // FrameBuffer's batch range and calls `consumeFrames()`, which is the
    // abstract method the StackWalker subclass overrides to apply the
    // user-supplied `Function<Stream<StackFrame>, R>` to a Stream over the
    // frame buffer slice [startIndex, endIndex).
    //
    // Without this callback the user's lambda never runs and the native
    // returns a meaningless Long, which Spring's
    // `findFirst().map(...).orElse(null)` truncates to null — that's why
    // the SpringApplication banner never fires.
    let end_index = (start_index + written) as i32;
    if crate::nbflags().debug_stackwalk {
        eprintln!(
            "[SW-DBG] callStackWalk consumed={} written={} end_index={}",
            consumed, written, end_index
        );
    }
    if let Some(this_ref) = this {
        // Pass `endIndex = startIndex + written` so the FrameBuffer's
        // (origin, fence) range exactly covers the frames we populated.
        // Using `batch` here would set fence past the array length and
        // throw AIOOBE on the second iteration step.
        //
        // Encode the trace cursor in the anchor so a follow-up
        // `fetchStackFrames` invocation knows how many trace entries we
        // already consumed and can resume from the next frame.
        // Cache the clean ordered frame list for the duration of this walk so
        // `fetchStackFrames` (invoked while the lazy stream drains, with the
        // thread paused deeper inside `doStackWalk`) indexes it instead of
        // re-capturing a polluted live stack. Pushed/popped as a stack to
        // tolerate nested walks (walks can re-enter during frame resolution).
        SW_FRAME_CACHE.with(|c| c.borrow_mut().push(Rc::clone(&ordered)));
        let r = ctx.invoke(
            "java/lang/StackStreamFactory$AbstractStackWalker",
            "doStackWalk",
            "(JIIII)Ljava/lang/Object;",
            &[
                Value::Object(Some(this_ref)),
                Value::Long(consumed as i64),
                Value::Int(skip as i32),
                Value::Int(written as i32),
                Value::Int(start_index as i32),
                Value::Int(end_index),
            ],
        );
        SW_FRAME_CACHE.with(|c| {
            c.borrow_mut().pop();
        });
        return r;
    }
    Ok(Some(Value::Object(None)))
}

/// `AbstractStackWalker.fetchStackFrames(int mode, long anchor,
///                                       int numFrames, int batchSize,
///                                       int startIndex, T[] frameBuffer)`.
///
/// Called by the JDK's lazy Stream when additional frames are needed
/// after the initial batch. We use `anchor` as a cursor over the trace
/// captured at the original `callStackWalk` (the live thread is paused
/// in the native frame, so re-capturing produces the same trace) and
/// resume populating from there.
pub(crate) fn native_fetch_stack_frames(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // The JDK 25 signature is
    //   fetchStackFrames(int mode, long anchor, int numFrames,
    //                    int batchSize, int startIndex, T[] frames)
    // The legacy signatures used `(long mode, long anchor, ...)`. Probe
    // both shapes when extracting `anchor` so a single implementation
    // handles all registered overloads.
    // WALK8-1 (`CRATONVM_STACKWALK_JDK_PIPELINE_BATCHED`): a pipeline walk's
    // later batch, from the walk's own list and builder.
    if let Some(result) = pipeline_batch_fetch(ctx, args) {
        return result;
    }
    let mut anchor: i64 = 0;
    let mut start_index: i32 = 0;
    let mut frame_buffer = None;
    // JDK 25: (this, int mode, long anchor, int lastBatchFrameCount,
    //          int batchSize, int startIndex, T[] frames) — observed as
    //          `args.len() == 7` with `anchor` sometimes tagged as `Double`.
    if args.len() >= 8 {
        anchor = value_as_i64_bits(args.get(2));
        if let Some(Value::Int(s)) = args.get(6) {
            start_index = *s;
        }
        if let Some(Value::Object(Some(f))) = args.get(7) {
            frame_buffer = Some(*f);
        }
    } else if args.len() >= 7 {
        anchor = value_as_i64_bits(args.get(2));
        if let Some(Value::Int(s)) = args.get(5) {
            start_index = *s;
        }
        if let Some(Value::Object(Some(f))) = args.get(6) {
            frame_buffer = Some(*f);
        }
    }
    // Legacy layout: (this, long mode, long anchor, int batch, int startIndex, frames)
    if frame_buffer.is_none() {
        anchor = value_as_i64_bits(args.get(3));
        if let Some(Value::Int(s)) = args.get(5) {
            start_index = *s;
        }
        if let Some(Value::Object(Some(f))) = args.get(6) {
            frame_buffer = Some(*f);
        }
    }
    let buffer = match frame_buffer {
        Some(b) => b,
        None => return Ok(Some(Value::Int(0))),
    };

    if crate::nbflags().debug_stackwalk {
        eprintln!(
            "[SW-DBG] fetchStackFrames parsed anchor={} start_index={}",
            anchor, start_index
        );
    }

    let cursor = anchor.max(0) as usize;
    // Prefer the clean frame list cached by the enclosing `callStackWalk`.
    // Re-capturing here would observe a deeper, polluted stack (we are paused
    // inside `doStackWalk → <stream pipeline> → lambda`), shifting the cursor
    // onto `java.util.stream.*` / `StackStreamFactory$*` frames and feeding the
    // user function garbage — see `SW_FRAME_CACHE`. Fall back to a fresh
    // capture only when no walk is active (a stray/unmatched fetch).
    let cached = SW_FRAME_CACHE.with(|c| c.borrow().last().cloned());
    let from_cache = cached.is_some();
    let ordered = match cached {
        Some(o) => o,
        None => {
            let trace = ctx.capture_stack_walk_trace();
            Rc::new(ordered_stack_walk_frames(&trace))
        }
    };
    if crate::nbflags().debug_stackwalk {
        eprintln!(
            "[SW-DBG] fetchStackFrames anchor={} start_index={} ordered_len={} from_cache={}",
            cursor,
            start_index,
            ordered.len(),
            from_cache
        );
    }
    let buf_len = ctx.array_length(buffer);
    let start = start_index.max(0) as usize;
    let slack = buf_len.saturating_sub(start);
    let mut written = 0usize;
    let mut new_cursor = cursor;
    // Same RETAIN_CLASS_REFERENCE read as `native_call_stack_walk`: `args[0]`
    // is the AbstractStackWalker here too. Defaults to enabled when the
    // receiver/walker cannot be seen, so a stray fetch does not produce frames
    // that reject `ensureRetainClassRefEnabled()`.
    let retain_class_ref = walker_retains_class_ref(
        ctx,
        match args.first() {
            Some(Value::Object(Some(o))) => Some(*o),
            _ => None,
        },
    );
    // GC-SAFETY: see `native_call_stack_walk` — pin the buffer across the
    // allocating `populate_sfi` loop and re-read the forwarded reference.
    let buf_pin = ctx.pin_native_root(buffer);
    // `populate_sfi` allocates a StackFrameInfo per frame, so the receiver in
    // `args` is a pre-loop address by the time the anchor is written below.
    // gc-common w19-g: pinned HERE, before the loop. It used to be pinned
    // after the loop, which roots (and "re-reads") the already-stale
    // pre-loop address -- a pin is a fact about an object, not a variable.
    let this_pin = match args.first() {
        Some(Value::Object(Some(o))) => Some((ctx.pin_native_root(*o), *o)),
        _ => None,
    };
    let mut buffer = buffer;
    // `fetchStackFrames(int mode, ...)` (JDK 25) or `(long mode, ...)`.
    let class_info_only = match args.get(1) {
        Some(Value::Int(mode)) => i64::from(*mode),
        Some(Value::Long(mode)) => *mode,
        _ => 0,
    } & STACK_WALK_MODE_CLASS_INFO_ONLY
        != 0;
    for entry in ordered.iter().skip(cursor) {
        if written >= slack {
            break;
        }
        if class_info_only {
            buffer = ctx.read_native_pin(buf_pin, buffer);
            if fill_class_frame_info(ctx, buffer, start + written, entry) {
                written += 1;
                new_cursor += 1;
                continue;
            }
        }
        // WALK17-3 step 1: a cached lean walk's entry gets its line here.
        let entry = resolved_walk_entry(ctx, entry);
        let sfi = populate_sfi(ctx, &entry, retain_class_ref);
        buffer = ctx.read_native_pin(buf_pin, buffer);
        ctx.set_array_element(buffer, start + written, Value::Object(Some(sfi?)));
        written += 1;
        new_cursor += 1;
    }
    // Read BEFORE releasing `buf_pin`, which truncates the pin stack.
    let this_pinned = this_pin.map(|(p, o)| ctx.read_native_pin(p, o));
    ctx.unpin_native_roots(buf_pin);
    // Persist the new cursor back into `this.anchor` so a subsequent
    // `fetchStackFrames` call resumes from the next trace frame.
    if let Some(this_ref) = this_pinned.as_ref() {
        if let Some(idx) =
            ctx.resolve_field_index("java/lang/StackStreamFactory$AbstractStackWalker", "anchor")
        {
            ctx.set_field(*this_ref, idx, Value::Long(new_cursor as i64));
        }
    }
    Ok(Some(Value::Int(written as i32)))
}

/// Register the StackStreamFactory private natives.
pub fn register_lang_stackwalker(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    // java/lang/StackStreamFactory$AbstractStackWalker.callStackWalk
    let asw = "java/lang/StackStreamFactory$AbstractStackWalker";
    registry.register(
        asw,
        "callStackWalk",
        "(JIII[Ljava/lang/Object;[Ljava/lang/Class;)Ljava/lang/Object;",
        native_call_stack_walk,
    );
    // JDK 25 narrows `mode` to an int and inserts ContinuationScope +
    // Continuation references between mode/skip and
    // batch/startIndex/frameBuffer:
    //   callStackWalk(int mode, int skipframes, ContinuationScope,
    //                 Continuation, int bufferSize, int startIndex,
    //                 Object[] frames)
    // We collapse it onto `native_call_stack_walk` by reordering args into
    // the original (this, mode, skip, batch, startIndex, frameBuffer, _)
    // shape; the ContinuationScope / Continuation are dropped (we have no
    // virtual-thread continuation support so the user-visible behaviour
    // matches the platform-thread code path).
    //
    // The second int is `skipframes`, not a half of the mode (interpreter
    // round i1 wave 37): it used to be shifted into the mode's high word and
    // the skip passed as 0. JDK 25's only caller (`beginStackWalk`) passes 0,
    // so nothing observable changed; the mode's flag bits
    // (`SHOW_HIDDEN_FRAMES`) are read from its low word either way.
    registry.register_with_kind(
        asw,
        "callStackWalk",
        "(IILjdk/internal/vm/ContinuationScope;Ljdk/internal/vm/Continuation;II[Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            // args = (this, mode, skipframes, contScope, continuation,
            //         bufferSize, startIndex, frames)
            let mode = i64::from(args.get(1).and_then(|v| v.as_int()).unwrap_or(0));
            let skip = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            // A walker built on a continuation (`Continuation.stackWalker`,
            // `VirtualThread.getStackTrace` of an unmounted virtual thread)
            // walks that continuation's frames.
            let continuation_trace = match args.get(4) {
                Some(Value::Object(Some(cont))) => ctx.continuation_stack_trace(*cont),
                _ => None,
            };
            let reordered = [
                args.first().copied().unwrap_or(Value::Object(None)),
                Value::Long(mode),
                Value::Int(skip),
                args.get(5).copied().unwrap_or(Value::Int(0)),
                args.get(6).copied().unwrap_or(Value::Int(0)),
                args.get(7).copied().unwrap_or(Value::Object(None)),
                Value::Object(None),
            ];
            native_call_stack_walk_of(ctx, &reordered, continuation_trace)
        },
        NativeKind::Bridge,
    );
    // JDK 21's actual signature: single `long mode` (not split into two
    // ints like JDK 25) plus the ContinuationScope/Continuation params and
    // an `Object` return (not `int` like the older no-continuation
    // variant below). Missing this exact overload meant EVERY
    // `StackWalker.walk(...)` on JDK 21 threw `UnsatisfiedLinkError` —
    // including Mockito's `LocationImpl` (used by the inline mock maker on
    // every mocked-method invocation), so any suite mocking a JDK class
    // failed on the very first stubbed call.
    registry.register(
        asw,
        "callStackWalk",
        "(JILjdk/internal/vm/ContinuationScope;Ljdk/internal/vm/Continuation;II[Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            // args = (this, mode, skip, contScope, continuation, batch, startIndex, frameBuffer)
            // A walker built on a continuation walks its frames (see the
            // JDK 25 overload above).
            let continuation_trace = match args.get(4) {
                Some(Value::Object(Some(cont))) => ctx.continuation_stack_trace(*cont),
                _ => None,
            };
            let reordered = [
                args.first().copied().unwrap_or(Value::Object(None)),
                args.get(1).copied().unwrap_or(Value::Long(0)),
                args.get(2).copied().unwrap_or(Value::Int(0)),
                args.get(5).copied().unwrap_or(Value::Int(0)),
                args.get(6).copied().unwrap_or(Value::Int(0)),
                args.get(7).copied().unwrap_or(Value::Object(None)),
                Value::Object(None),
            ];
            native_call_stack_walk_of(ctx, &reordered, continuation_trace)
        },
    );
    // JDK 21+ changed the signature slightly (added ContinuationScope,
    // Continuation params) — register the old variant too so both paths
    // resolve.
    registry.register(
        asw,
        "callStackWalk",
        "(JIIII[Ljava/lang/Object;)I",
        |ctx, args| {
            // Re-fit args: (this, mode, skip, batchSize, startIndex, endIndex, frameBuffer).
            // We collapse this signature onto `callStackWalk` above by
            // passing the same frame buffer and reusing its logic.
            let reordered = [
                args.first().copied().unwrap_or(Value::Object(None)),
                args.get(1).copied().unwrap_or(Value::Long(0)),
                args.get(2).copied().unwrap_or(Value::Int(0)),
                args.get(3).copied().unwrap_or(Value::Int(0)),
                args.get(4).copied().unwrap_or(Value::Int(0)),
                args.get(6).copied().unwrap_or(Value::Object(None)),
                Value::Object(None),
            ];
            let res = native_call_stack_walk(ctx, &reordered)?;
            match res {
                Some(Value::Long(n)) => Ok(Some(Value::Int(n as i32))),
                other => Ok(other),
            }
        },
    );
    // fetchStackFrames — old and new signatures
    registry.register(
        asw,
        "fetchStackFrames",
        "(JJII[Ljava/lang/Object;)I",
        native_fetch_stack_frames,
    );
    registry.register(
        asw,
        "fetchStackFrames",
        "(JJII)I",
        native_fetch_stack_frames,
    );
    // JDK 25: fetchStackFrames(int mode, long anchor, int batchSize,
    //                           int startIndex, int endIndex, T[] frameBuffer)
    registry.register_with_kind(
        asw,
        "fetchStackFrames",
        "(IJIII[Ljava/lang/Object;)I",
        native_fetch_stack_frames,
        NativeKind::Bridge,
    );
    // WALK8-1 (`CRATONVM_STACKWALK_JDK_PIPELINE_BATCHED`, default ON, `=0` kill; JIT
    // round 19 wave 9, lane walk9): `--jdk-only` over a real JDK only, a
    // reviewed `Intrinsic`, stated here rather than inherited (AGENTS.md),
    // over the JDK's concrete `StackFrameBuffer.fill`: a pipeline walk's
    // buffer keeps its slots (the batch fetch writes every slot it reports);
    // any other buffer runs the JDK body (`native_stack_frame_buffer_fill`).
    if stackwalk_jdk_pipeline_batched_enabled()
        && registry.real_jdk()
        && registry.compatibility_mode() == cratonvm_types::compat::CompatibilityMode::JdkOnly
    {
        registry.register_with_kind(
            STACK_FRAME_BUFFER_CLASS,
            "fill",
            STACK_FRAME_BUFFER_FILL_DESC,
            native_stack_frame_buffer_fill,
            NativeKind::Intrinsic,
        );
    }
    // WALK11-2's lever (`CRATONVM_STACKWALK_REFLECTION_FRAME_INTRINSIC`, opt-in;
    // JIT round 19 wave 12, lane walk12): over a real JDK, both modes (the
    // pipeline walk runs in both), a reviewed `Intrinsic`, stated here, over
    // the JDK's concrete `StackStreamFactory.isReflectionFrame(Class)`: the
    // same expression through the same `isAssignableFrom` rule in one native
    // call (`native_stack_stream_is_reflection_frame`).
    //
    // WALK12-1's leaf half (`CRATONVM_STACKWALK_REFLECTION_FRAME_LEAF`, default
    // ON, `=0` registers it non-leaf as before; JIT round 20 wave 11, lane
    // pNat): the claim rides on this callback, whose resolved path is
    // leaf-safe and whose other paths decline to the full funnel (see
    // `native_stack_stream_is_reflection_frame`). Still a reviewed `Intrinsic`.
    if stackwalk_reflection_frame_intrinsic_enabled() && registry.real_jdk() {
        registry.with_leaf(stackwalk_reflection_frame_leaf_enabled(), |registry| {
            registry.register_with_kind(
                "java/lang/StackStreamFactory",
                "isReflectionFrame",
                IS_REFLECTION_FRAME_DESC,
                native_stack_stream_is_reflection_frame,
                NativeKind::Intrinsic,
            );
        });
    }

    // StackFrameInfo — native overrides run before bytecode (see
    // `try_stackless_invoke` / `invoke_or_native`).  Read real JDK fields
    // (`name`, `bci`, `flags`) and the eagerly-filled `ste` mirror.
    let sfi = "java/lang/StackFrameInfo";
    registry.register(sfi, "getClassName", "()Ljava/lang/String;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        // A lazy frame's walk element names the class too (wave 54); it is
        // read without building the frame's element, as the JDK's
        // `getClassName()` (`declaringClass().getName()`) builds none.
        let ste = match ctx.get_field_by_name(this, "ste") {
            Value::Object(Some(ste)) => Some(ste),
            _ => match ctx.get_field(this, SF_DECL_INTERNAL) {
                // Wave 55: a walk's lines (an `int[]`) name no class; the
                // frame's class does, below.
                Value::Object(Some(walk)) if ctx.object_is_array(walk) => None,
                Value::Object(Some(walk)) if ctx.read_string(walk).is_none() => Some(walk),
                _ => None,
            },
        };
        if ste.is_none() {
            // `declaringClass().getName()`, the JDK's body (wave 55: a frame
            // that keeps its walk's lines has neither an element nor a name
            // slot).
            if let Some(cid) = sfi_class_id(ctx, this) {
                if matches!(ctx.get_field(this, SF_DECL_INTERNAL), Value::Object(Some(w)) if ctx.object_is_array(w))
                {
                    let mirror = ctx.get_class_mirror(cid);
                    // Wave 79 (L2): `Class.getName()`'s own memo first.
                    if compat_sfi_class_name_direct(&*ctx) {
                        if let Some(name) = class_name_memo(ctx, mirror) {
                            return Ok(Some(Value::Object(Some(name))));
                        }
                    }
                    return ctx.invoke_virtual(mirror, "getName", "()Ljava/lang/String;", &[]);
                }
            }
        }
        if let Some(ste) = ste {
            // Layout-aware: slot 0 is `classLoaderName` on a real JDK 25
            // `StackTraceElement`, not the class name. See `SteLayout`.
            return Ok(Some(crate::lang_misc::ste_read_field(
                ctx,
                ste,
                "declaringClass",
                0,
            )));
        }
        Ok(Some(ctx.get_field(this, SF_CLASSNAME)))
    });
    registry.register(sfi, "getMethodName", "()Ljava/lang/String;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let by_name = ctx.get_field_by_name(this, "name");
        if matches!(by_name, Value::Object(Some(_))) {
            return Ok(Some(by_name));
        }
        Ok(Some(ctx.get_field(this, SF_METHODNAME)))
    });
    registry.register(sfi, "getFileName", "()Ljava/lang/String;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        if let Some(ste) = sfi_element(ctx, this)? {
            return Ok(Some(crate::lang_misc::ste_read_field(
                ctx, ste, "fileName", 2,
            )));
        }
        Ok(Some(ctx.get_field(this, SF_FILENAME)))
    });
    registry.register(sfi, "getLineNumber", "()I", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let flags = match ctx.get_field_by_name(this, "flags") {
            Value::Int(f) => f,
            _ => 0,
        };
        if (flags & 0x100) != 0 {
            return Ok(Some(Value::Int(-2)));
        }
        if let Some(ste) = sfi_element(ctx, this)? {
            return Ok(Some(crate::lang_misc::ste_read_field(
                ctx,
                ste,
                "lineNumber",
                3,
            )));
        }
        Ok(Some(ctx.get_field(this, SF_LINENUMBER)))
    });
    registry.register(sfi, "getByteCodeIndex", "()I", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let flags = match ctx.get_field_by_name(this, "flags") {
            Value::Int(f) => f,
            _ => 0,
        };
        if (flags & 0x100) != 0 {
            return Ok(Some(Value::Int(-1)));
        }
        match ctx.get_field_by_name(this, "bci") {
            Value::Int(bci) => Ok(Some(Value::Int(bci))),
            _ => Ok(Some(ctx.get_field(this, SF_BCI))),
        }
    });
    registry.register(
        sfi,
        "getDeclaringClass",
        "()Ljava/lang/Class;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // This method is specified to be available only when the walker
            // requested RETAIN_CLASS_REFERENCE.  `populate_sfi` records that
            // option on every frame, so do not let the native override bypass
            // the JDK guard that the bytecode normally executes first.
            if matches!(class_frame_retains_class_ref(ctx, this), Some(false)) {
                return Err(MethodCallFailed::from(
                    RuntimeError::UnsupportedOperationException {
                        message: "No access to RETAIN_CLASS_REFERENCE".to_string(),
                    },
                ));
            }
            // Same ordering rule as `declaring_class_native`: the mirror
            // `populate_sfi` resolved from the frame's own ClassId outranks a
            // fresh, loader-blind, ambiguity-strict by-name lookup.
            if let Value::Object(Some(m)) = ctx.get_field_by_name(this, "classOrMemberName") {
                if let Some(m) = class_or_member_mirror(ctx, m) {
                    return Ok(Some(Value::Object(Some(m))));
                }
            }
            let internal = sfi_decl_internal(ctx, this);
            if !internal.is_empty() {
                if let Some(cid) = ctx.class_id_by_name(&internal) {
                    return Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))));
                }
            }
            // Fall back to `classOrMemberName` (`populate_sfi` stores the Class
            // mirror there when its own ClassId resolution -- which prefers
            // the guaranteed-valid `StackTraceEntry::class_id` over a by-name
            // lookup -- succeeds). This rescues exactly the case a fresh
            // by-name lookup here can miss: a frame whose class is still
            // running its own `<clinit>` (`SpringFactoriesLoader`/
            // `EntityManagerFactoryUtils` calling `LogFactory.getLog()` from
            // their own static initializers, walked by log4j-api's
            // `StackLocator`).
            if let Some(idx) = ctx.resolve_field_index("java/lang/ClassFrameInfo", "classOrMemberName")
            {
                let v = ctx.get_field(this, idx);
                if let Value::Object(Some(_)) = v {
                    return Ok(Some(v));
                }
            }
            if crate::nbflags().sfi_null_trace {
                eprintln!("[SFI-NULL-TRACE getDeclaringClass] both internal={internal:?} lookup and classOrMemberName fallback failed");
            }
            Ok(Some(Value::Object(None)))
        },
    );
    registry.register(sfi, "isNativeMethod", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let flags = match ctx.get_field_by_name(this, "flags") {
            Value::Int(f) => f,
            _ => 0,
        };
        // java.lang.reflect.Modifier.NATIVE
        Ok(Some(Value::Int(if (flags & 0x100) != 0 { 1 } else { 0 })))
    });
    // `CRATONVM_COMPAT_SFI_ELEMENT_JDK_BODY` (default off; interpreter round
    // i3 wave 79, lane L2,
    // `i77-L2-proposal-compatible-walker-element-cost-20261110.md`): under
    // `--compatible` over a real JDK, the JDK's concrete body serves
    // `toStackTraceElement()` (its `synchronized (this)` memo, then
    // `StackTraceElement.of(StackFrameInfo)`, the reviewed `Intrinsic`
    // `lib.rs` registers under the same switch), as under `--jdk-only`,
    // where this `Bridge` is a retired shadow. Real bytecode wins by not
    // registering the bridge at all; no other registration changes.
    if !registry.compat_sfi_element_jdk_body() {
        registry.register(
            sfi,
            "toStackTraceElement",
            "()Ljava/lang/StackTraceElement;",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Object(None))),
                };
                if let Some(ste) = sfi_element(ctx, this)? {
                    return Ok(Some(Value::Object(Some(ste))));
                }
                // Decode SFI slots into the values `fill_stack_trace_element`
                // needs. `SF_CLASSNAME` holds the dotted class name; the
                // `/`-separated internal name lives in `SF_DECL_INTERNAL`.
                //
                // gc-common w19-g: every slot is read BEFORE the element's
                // allocation (they were read after it, off the pre-allocation
                // `this`), and the element is rooted across
                // `fill_stack_trace_element`, which allocates its strings -- so the
                // address it was handed is stale on return and must be re-read, as
                // its other callers do. (gc-common w19-e: the filler now also
                // returns the element's current address; the handle below reads
                // the same one.)
                let class_dotted = match ctx.get_field(this, SF_CLASSNAME) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => String::new(),
                };
                let class_slashed = match ctx.get_field(this, SF_DECL_INTERNAL) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => class_dotted.replace('.', "/"),
                };
                let method_name = match ctx.get_field(this, SF_METHODNAME) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => String::new(),
                };
                let file_name = match ctx.get_field(this, SF_FILENAME) {
                    Value::Object(Some(s)) => ctx.read_string(s),
                    _ => None,
                };
                let line = match ctx.get_field(this, SF_LINENUMBER) {
                    Value::Int(n) => n,
                    _ => -1,
                };
                let ste_new =
                    try_alloc_concurrent_synthetic(ctx, "java/lang/StackTraceElement", 4)?;
                let mut scope = NativeHandleScope::new(ctx);
                let ste_h = scope.root(ste_new);
                crate::lang_misc::fill_stack_trace_element(
                    &mut *scope,
                    ste_new,
                    &class_slashed,
                    &class_dotted,
                    &method_name,
                    file_name.as_deref(),
                    line,
                );
                let ste = scope.get(&ste_h);
                Ok(Some(Value::Object(Some(ste))))
            },
        );
    }

    // Round-7 HIGH-10 fix: StackFrame.getMethodType() previously returned
    // null, breaking ReflectionFactory / MethodHandle.asType reflection that
    // walks the stack and asks each frame for its method type (Spring
    // `AnnotationUtils.getDefaultValue` and Guice both do this).
    //
    // Strategy: read the declaring class + method name from the populated
    // SFI, find the first matching `declared_methods` entry, parse its JVM
    // descriptor, and delegate to `MethodType.fromMethodDescriptorString`
    // for the actual MethodType allocation. The JDK factory does the
    // descriptor → (Class returnType, Class[] paramTypes) decode for us
    // and applies caching downstream.
    //
    // Overload disambiguation: when multiple methods share a name, we pick
    // the first declared overload. The stack-trace entry does not carry the
    // descriptor, and computing the exact one would require pairing with the
    // call site's `invoke*` instruction, which is more than the JDK contract
    // requires for `StackFrame.getMethodType()` (the javadoc allows returning
    // any valid MethodType for the method).
    registry.register(
        sfi,
        "getMethodType",
        "()Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // JDK 25's `StackFrameInfo.getMethodType()` opens with
            // `ensureRetainClassRefEnabled()`, as `getDescriptor` (below) does
            // through it (interpreter round i2 wave 53, lane L3; probes
            // `L3W42StackWalkerApi` `frame-method-type-no-option`,
            // `L3W44StackWalkerEdges` `method-type-plain`).
            if class_frame_retains_class_ref(ctx, this) == Some(false) {
                return Err(MethodCallFailed::from(
                    RuntimeError::UnsupportedOperationException {
                        message: "No access to RETAIN_CLASS_REFERENCE".to_string(),
                    },
                ));
            }
            // The frame's own descriptor, recorded at the walk (wave 48).
            let recorded = frame_recorded_descriptor(ctx, this);
            let desc = match recorded {
                Some(desc) => desc,
                None => {
                    // Prefer the internal-name slot we populate at SFI-construct
                    // time so we avoid the dotted-class-name round-trip through
                    // class_id_by_name (which would NPE on synthetic frames).
                    let internal = sfi_decl_internal(ctx, this);
                    if internal.is_empty() {
                        return Ok(Some(Value::Object(None)));
                    }
                    let method_name = match ctx.get_field_by_name(this, "name") {
                        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                        _ => match ctx.get_field(this, SF_METHODNAME) {
                            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                            _ => String::new(),
                        },
                    };
                    if method_name.is_empty() {
                        return Ok(Some(Value::Object(None)));
                    }
                    let class_id = match ctx.class_id_by_name(&internal) {
                        Some(c) => c,
                        None => return Ok(Some(Value::Object(None))),
                    };
                    // Pick the first declared overload that matches the method
                    // name: a frame that recorded no descriptor. See the comment
                    // above on overload disambiguation.
                    let descriptor = ctx
                        .declared_methods(class_id)
                        .into_iter()
                        .find(|m| m.name == method_name)
                        .map(|m| m.descriptor);
                    let Some(desc) = descriptor else {
                        return Ok(Some(Value::Object(None)));
                    };
                    desc
                }
            };
            // Delegate to MethodType.fromMethodDescriptorString — the JDK
            // factory parses (paramTypes)returnType and allocates a real
            // MethodType -- with the declaring class's loader.
            frame_method_type_for_descriptor(ctx, this, &desc)
        },
    );

    // The real `StackFrameInfo.getDescriptor()` body goes through
    // `getMethodType()` and its HotSpot-populated hidden carrier.  CratonVM
    // records the declaring class and method name instead, so resolve the
    // descriptor directly from the class store.  This is intentionally
    // narrower than forcing `getMethodType`: callers asking for a MethodType
    // retain the existing construction path and its loader semantics.
    registry.register(sfi, "getDescriptor", "()Ljava/lang/String;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        // JDK 25's `getDescriptor()` is `getMethodType().descriptorString()`,
        // and `getMethodType()` refuses a walker without
        // RETAIN_CLASS_REFERENCE (`ensureRetainClassRefEnabled`); HotSpot
        // throws (interpreter round i1 wave 42, lane L3;
        // `tools/probes/interp/L3/L3W42StackWalkerApi.java`,
        // `frame-descriptor-no-option`), two hops below (wave 55).
        if class_frame_retains_class_ref(ctx, this) == Some(false) {
            note_descriptor_retain_refusal_chain(ctx);
            return Err(MethodCallFailed::from(
                RuntimeError::UnsupportedOperationException {
                    message: "No access to RETAIN_CLASS_REFERENCE".to_string(),
                },
            ));
        }
        let descriptor = frame_recorded_descriptor(ctx, this).or_else(|| {
            let internal = sfi_decl_internal(ctx, this);
            let method_name = match ctx.get_field_by_name(this, "name") {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            ctx.class_id_by_name(&internal)
                .map(|class_id| ctx.declared_methods(class_id))
                .and_then(|methods| {
                    methods
                        .into_iter()
                        .find(|method| method.name == method_name)
                        .map(|method| method.descriptor)
                })
        });
        match descriptor {
            Some(descriptor) => {
                let descriptor = ctx.create_string(&descriptor);
                Ok(Some(Value::Object(Some(descriptor))))
            }
            None => Err(MethodCallFailed::from(
                RuntimeError::UnsupportedOperationException {
                    message: "StackFrame descriptor metadata is unavailable".to_string(),
                },
            )),
        }
    });

    // SB3 deduceMainApplicationClass path: real-JDK
    // `StackFrameBuffer.at(int)` calls the package-private virtual
    // `ClassFrameInfo.declaringClass()` (overridden by `StackFrameInfo`)
    // for every populated frame as part of `setBatch()`. The default
    // `StackFrameInfo` override delegates to
    // `JavaLangInvokeAccess.getDeclaringClass(classOrMemberName)`, which
    // casts to `ResolvedMethodName` — a hidden type whose internals a
    // HotSpot `expandStackFrameInfo` intrinsic would populate and ours
    // cannot (we never mint a `ResolvedMethodName`). That cast is what
    // surfaces as the `ClassCastException` Spring catches, suppressing
    // the banner.
    //
    // Override the package-private `declaringClass()` to return the
    // Class mirror straight from our `SF_DECL_INTERNAL` slot. Same for
    // ClassFrameInfo proper (in case a frame ends up as a bare
    // ClassFrameInfo elsewhere). `expandStackFrameInfo` is registered
    // below so callers that get past our other overrides neither hit
    // UnsatisfiedLinkError nor read a null `type`.
    fn declaring_class_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        // NO RETAIN_CLASS_REFERENCE CHECK HERE, deliberately, and it is not an
        // oversight -- `java.lang.ClassFrameInfo` says so in a comment on the
        // declaration itself:
        //
        //     // package-private called by StackStreamFactory to skip
        //     // the capability check
        //     Class<?> declaringClass() { return (Class<?>) classOrMemberName; }
        //
        // and its own `getClassName()` is `declaringClass().getName()`. The
        // capability check belongs to the PUBLIC `getDeclaringClass()`
        // (`ensureRetainClassRefEnabled(); return declaringClass();`), which
        // `get_declaring_class_native` below still enforces.
        //
        // This guard used to be here, on the argument that "callers that arrive
        // through ClassFrameInfo must not bypass the contract either". The
        // caller it actually blocked is
        // `StackStreamFactory$StackFrameBuffer.at(int)` -- the same call site
        // the comment above this function names as the reason the override
        // exists at all -- which the JDK runs for EVERY populated frame inside
        // `setBatch()`. With the guard, a plain `StackWalker.getInstance()`
        // walk (no RETAIN_CLASS_REFERENCE) threw
        // `UnsupportedOperationException` out of its first batch, so nothing
        // driving the walk through the JDK's own `callStackWalk` /
        // `fetchStackFrames` path could complete. It went unnoticed only
        // because `p59_sw_walk` intercepted `walk` before the JDK bytecode
        // ever ran.
        // The mirror `populate_sfi` already resolved wins, and it is read
        // BY NAME off this carrier rather than through a `resolve_field_index`
        // on `java/lang/ClassFrameInfo` (which needs that class to be loaded
        // and unambiguous just to compute a slot).
        //
        // It used to be the other way round -- `SF_DECL_INTERNAL` +
        // `class_id_by_name` first, this only as a fallback -- and that is
        // unsound for exactly the reason `StackTraceEntry::class_id`'s doc
        // comment gives: `class_id_by_name` is `find_unique_class_by_name`, so
        // it is both loader-blind AND ambiguity-strict, answering `None` the
        // moment two loaders define the name. Under
        // `@CompileWithForkedClassLoader` that is the NORMAL state for every
        // non-JDK class, log4j-api's `StackLocator` included: its own frame's
        // `getDeclaringClass()` came back null, `LogFactory.getLog` NPE'd
        // inside `AbstractEnvironment`'s field initialiser, and the swallowed
        // failure left Spring running on a synthetic Environment whose
        // `logger` was null. `populate_sfi` prefers the frame's OWN ClassId
        // precisely so this never has to guess; it just was not being asked.
        if let Value::Object(Some(m)) = ctx.get_field_by_name(this, "classOrMemberName") {
            // Only a Class mirror. A real-JDK-built `ClassFrameInfo` can hold a
            // `ResolvedMethodName` here: its `vmholder` is the class.
            if let Some(m) = class_or_member_mirror(ctx, m) {
                return Ok(Some(Value::Object(Some(m))));
            }
        }
        // This native is registered for the p59 `StackWalker$StackFrame`
        // carrier as well (see `register_p59_stackwalker`), and that one is a
        // bare synthetic with UNNAMED slots -- its eagerly-resolved mirror
        // lives in slot 6, so the `classOrMemberName` read above cannot see
        // it. Without this arm the carrier fell all the way through to the
        // by-name lookup and answered null for every frame whose class two
        // loaders define.
        if ctx
            .class_name_of_id(ctx.class_id_of_object(this))
            .is_some_and(|n| n == P59_STACK_FRAME)
            && ctx.object_num_fields(this) > P59_SF_DECL_MIRROR
        {
            if let Value::Object(Some(m)) = ctx.get_field(this, P59_SF_DECL_MIRROR) {
                if is_class_mirror(ctx, m) {
                    return Ok(Some(Value::Object(Some(m))));
                }
            }
            // Slot 6 is LAZY since the carrier stopped resolving a mirror per
            // frame; slot 8 holds the same guaranteed-valid `ClassId` the eager
            // resolve used. Reading it here keeps this arm answering for a
            // carrier whose mirror has not been materialised yet, which is the
            // normal state for every frame a walk did not inspect.
            if let Some(cid) = crate::phases_late::reflect_invoke::p59_frame_class_id(ctx, this) {
                return Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))));
            }
        }
        // Only then the internal-name slot, resolved by name.
        if let Value::Object(Some(s)) = ctx.get_field(this, SF_DECL_INTERNAL) {
            let internal = ctx.read_string(s).unwrap_or_default();
            if !internal.is_empty() {
                if crate::nbflags().debug_sfi && internal.contains("InsuranceProjectApplication") {
                    eprintln!(
                        "[SFI-DBG] declaringClass internal={internal:?} class_id={:?}",
                        ctx.class_id_by_name(&internal)
                    );
                }
                if let Some(cid) = ctx.class_id_by_name(&internal) {
                    return Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))));
                }
            }
        }
        if let Some(idx) = ctx.resolve_field_index("java/lang/ClassFrameInfo", "classOrMemberName")
        {
            let v = ctx.get_field(this, idx);
            if let Value::Object(Some(_)) = v {
                return Ok(Some(v));
            }
        }
        if crate::nbflags().sfi_null_trace {
            let internal = match ctx.get_field(this, SF_DECL_INTERNAL) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::from("<no SF_DECL_INTERNAL>"),
            };
            eprintln!(
                "[SFI-NULL-TRACE] declaringClass() returning NULL for frame internal={internal:?} resolved_cid={:?}",
                if internal.is_empty() { None } else { ctx.class_id_by_name(&internal) }
            );
        }
        Ok(Some(Value::Object(None)))
    }
    /// The PUBLIC `StackFrame.getDeclaringClass()`, which is
    /// `ensureRetainClassRefEnabled(); return declaringClass();` in the JDK.
    /// Split out of `declaring_class_native` when that one stopped enforcing
    /// the capability -- see the long comment inside it for why the
    /// package-private bridge must not.
    fn get_declaring_class_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        if matches!(class_frame_retains_class_ref(ctx, this), Some(false)) {
            return Err(MethodCallFailed::from(
                RuntimeError::UnsupportedOperationException {
                    message: "No access to RETAIN_CLASS_REFERENCE".to_string(),
                },
            ));
        }
        declaring_class_native(ctx, args)
    }
    registry.register(
        sfi,
        "declaringClass",
        "()Ljava/lang/Class;",
        declaring_class_native,
    );
    registry.register(
        "java/lang/ClassFrameInfo",
        "declaringClass",
        "()Ljava/lang/Class;",
        declaring_class_native,
    );
    registry.register(
        "java/lang/ClassFrameInfo",
        "getDeclaringClass",
        "()Ljava/lang/Class;",
        get_declaring_class_native,
    );
    // Interpreter round i2 wave 55, lane L3: both read the old six-slot
    // synthetic layout by index, which on the JDK's own `ClassFrameInfo`
    // (`classOrMemberName`, `flags`) is the class MIRROR and the flags INT --
    // reachable once `--compatible`'s walk builds the JDK's frame classes
    // (`reflect_invoke::p59_sw_frame`). On a real `ClassFrameInfo` they now
    // answer as its JDK body does: `declaringClass().getName()`, and
    // `throw new UnsupportedOperationException()`.
    registry.register(
        "java/lang/ClassFrameInfo",
        "getClassName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            if let Value::Object(Some(m)) = ctx.get_field_by_name(this, "classOrMemberName") {
                if is_class_mirror(ctx, m) {
                    return ctx.invoke_virtual(m, "getName", "()Ljava/lang/String;", &[]);
                }
            }
            Ok(Some(ctx.get_field(this, SF_CLASSNAME)))
        },
    );
    registry.register(
        "java/lang/ClassFrameInfo",
        "getMethodName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            if is_jdk_class_frame_info(ctx, this) {
                return Err(MethodCallFailed::from(
                    RuntimeError::UnsupportedOperationException {
                        message: String::new(),
                    },
                ));
            }
            Ok(Some(ctx.get_field(this, SF_METHODNAME)))
        },
    );
    // IMPLEMENTED (was a PARTIAL no-op). `expandStackFrameInfo` is the lazy
    // filler for the private `StackFrameInfo` fields `name`, `type`, `bci`
    // (jdk-25 — `javap -c java.lang.StackFrameInfo` shows the call sites).
    // `populate_sfi` writes `name` and `bci` up front, so `getMethodName()` and
    // `getByteCodeIndex()` were already correct; it does NOT write `type`, so
    // `StackFrame.getDescriptor()` (which returns `type` directly when it is a
    // String) and any unshadowed `getMethodType()` found `type == null` after
    // the "expansion" and failed. This fills `type`.
    //
    // The old escalation asked for a `StackTraceEntry::descriptor` field filled
    // by `capture_stack_trace`. That is NOT needed and would be the wrong shape:
    //   * capture is hot (log4j's `StackLocator` walks on every logger lookup)
    //     and would pay a `String` per frame for a field almost nobody reads;
    //   * the carrier already has everything required — `SF_DECL_INTERNAL` holds
    //     the declaring class's internal name and `name` the method name, and
    //     `NativeContext::declared_methods` has the descriptor of every method
    //     the class declares.
    // That is exactly how the sibling accessors already answer this:
    // `StackFrameInfo.getMethodType()` above and
    // `phases_late::reflect_invoke::p59_sf_get_method_type` for the 7-slot
    // `StackWalker$StackFrame` carrier. Resolution stays lazy, here, per that
    // precedent.
    //
    // Overload disambiguation follows the same siblings: first declared overload
    // of that name. `StackTraceEntry` carries no descriptor and the
    // `getDescriptor()`/`getMethodType()` javadoc does not pin which overload of
    // an overloaded name is reported.
    //
    // `type` is left as the descriptor String, which is a shape the JDK itself
    // uses: `getDescriptor()` returns it as-is and `getMethodType()` inflates a
    // String `type` into a `MethodType` on demand.
    registry.register_with_kind(
        sfi,
        "expandStackFrameInfo",
        "()V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            // Already expanded — the real native is idempotent and re-entered on
            // every `getDescriptor()` call.
            if matches!(ctx.get_field_by_name(this, "type"), Value::Object(Some(_))) {
                return Ok(None);
            }
            // Only fill the slot when it can legally hold a String. HotSpot's own
            // `expandStackFrameInfo` stores the method's signature String in the
            // `Object type` field (the layout recorded in `populate_sfi`'s comment:
            // `String name; Object type; int bci;`) and lets `getMethodType()`
            // inflate it on demand, which is what this mirrors. If some JDK build
            // instead declares `type` as a `MethodType`, or the carrier has no such
            // field at all (our synthetic 6-slot layout), bail rather than writing a
            // value of the wrong type into a typed slot.
            let this_class = ctx.class_id_of_object(this);
            let type_field_desc = ctx
                .declared_fields(this_class)
                .into_iter()
                .find(|f| f.name == "type")
                .map(|f| f.descriptor);
            match type_field_desc.as_deref() {
                Some("Ljava/lang/Object;") | Some("Ljava/lang/String;") => {}
                _ => return Ok(None),
            }
            let internal = sfi_decl_internal(ctx, this);
            if internal.is_empty() {
                return Ok(None);
            }
            let method_name = match ctx.get_field_by_name(this, "name") {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => match ctx.get_field(this, SF_METHODNAME) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => String::new(),
                },
            };
            if method_name.is_empty() {
                return Ok(None);
            }
            let Some(class_id) = ctx.class_id_by_name(&internal) else {
                return Ok(None);
            };
            let descriptor = ctx
                .declared_methods(class_id)
                .into_iter()
                .find(|m| m.name == method_name)
                .map(|m| m.descriptor);
            let Some(desc) = descriptor else {
                return Ok(None);
            };
            // GC-SAFETY: `create_string` allocates and can relocate `this` under the
            // moving collector, so pin the frame and re-read it through the pin
            // before the (allocation-free) field write. Nothing allocates after the
            // string, so the string itself cannot move before it is stored.
            let pin = ctx.pin_native_root(this);
            let desc_str = ctx.create_string(&desc);
            let this = ctx.read_native_pin(pin, this);
            ctx.set_field_by_name(this, "type", Value::Object(Some(desc_str)));
            ctx.unpin_native_roots(pin);
            Ok(None)
        },
        NativeKind::Bridge,
    );
    // IMPLEMENTED (was an unconditional no-op). Real
    // `ClassFrameInfo.ensureRetainClassRefEnabled()` throws
    // UnsupportedOperationException unless the RETAIN_CLASS_REF bit is set in the
    // frame's `flags`, which `ClassFrameInfo(StackWalker)` copies from the
    // walker. The blocker was the CAUSE, not the check: `populate_sfi` wrote
    // `flags = 0` unconditionally, so a faithful throw fired on every frame.
    // `populate_sfi` now records the walker's real `retainClassRef`
    // ([`walker_retains_class_ref`], read off `args[0].walker` in
    // `native_call_stack_walk` / `native_fetch_stack_frames`), so the check below
    // is honest.
    //
    // FAILS OPEN on an unreadable `flags` (missing field → null object read):
    // frames minted by a path that never saw a walker keep the old permissive
    // behaviour rather than throwing.
    //
    // REACHABILITY (checked against jdk-25 bytecode): both callers —
    // `ClassFrameInfo.getDeclaringClass()` and `StackFrameInfo.getMethodType()` —
    // are themselves natively shadowed above, so in the default configuration
    // this native is still effectively unreachable and the throw is latent. It
    // becomes live for any real-JDK path that reaches `ClassFrameInfo` bytecode
    // we do not shadow (e.g. a subclass, or a shim removal). Note the two
    // shadowing accessors remain deliberately lax: `getDeclaringClass()` resolves
    // the Class mirror without consulting the option, matching the documented
    // choice in `phases_late::reflect_invoke` for the sibling carrier.
    registry.register(
        "java/lang/ClassFrameInfo",
        "ensureRetainClassRefEnabled",
        "()V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let Some(retains_class_ref) = class_frame_retains_class_ref(ctx, this) else {
                // No `flags` field on this carrier — fail open.
                return Ok(None);
            };
            if !retains_class_ref {
                return Err(MethodCallFailed::from(
                    RuntimeError::UnsupportedOperationException {
                        message: "No access to RETAIN_CLASS_REFERENCE".to_string(),
                    },
                ));
            }
            Ok(None)
        },
    );
    registry.set_category(__prev_cat);
}

#[cfg(test)]
mod tests {
    use super::*;
    use cratonvm_native_api::NativeMethodRegistry;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// JDK 25's `StackStreamFactory.isReflectionFrame` by name (interpreter
    /// round i1 wave 43, lane L3): the reflection classes and accessor
    /// implementations (a nested one included) are reflection frames; the
    /// accessor interfaces, the factory and `Reflection` are not.
    #[test]
    fn reflection_frames_are_the_jdk_25_reflection_and_accessor_classes() {
        for yes in [
            "java/lang/reflect/Method",
            "java/lang/reflect/Constructor",
            "jdk/internal/reflect/DirectMethodHandleAccessor",
            "jdk/internal/reflect/DirectMethodHandleAccessor$NativeAccessor",
            "jdk/internal/reflect/DirectConstructorHandleAccessor",
            "jdk/internal/reflect/NativeMethodAccessorImpl",
            "jdk/internal/reflect/GeneratedMethodAccessor12",
        ] {
            assert!(is_reflection_frame_class(yes), "{yes}");
        }
        for no in [
            "java/lang/reflect/Field",
            "jdk/internal/reflect/MethodAccessor",
            "jdk/internal/reflect/MethodHandleAccessorFactory",
            "jdk/internal/reflect/Reflection",
            "p/DirectMethodHandleAccessor",
        ] {
            assert!(!is_reflection_frame_class(no), "{no}");
        }
    }

    #[test]
    fn retain_class_reference_bit_matches_jdk_25_class_frame_info_layout() {
        assert_eq!(SF_FLAG_RETAIN_CLASS_REF, 1 << 27);
        assert_eq!(SF_FLAG_RETAIN_CLASS_REF & 0x00ff_ffff, 0);
    }

    /// Interpreter round i2 wave 54, lane L3: the walk count rides in the
    /// bits of `flags` no Java reads, and leaves every other bit alone.
    #[test]
    fn the_walk_count_keeps_out_of_the_bits_java_reads() {
        assert_eq!(SF_WALK_COUNT_LOW & 0x00ff_ffff, 0);
        assert_eq!(SF_WALK_COUNT_HIGH & 0x00ff_ffff, 0);
        assert_eq!(
            (SF_WALK_COUNT_LOW | SF_WALK_COUNT_HIGH) & SF_FLAG_RETAIN_CLASS_REF as u32,
            0
        );
        let base = SF_FLAG_RETAIN_CLASS_REF | MODIFIER_NATIVE;
        for count in [0u64, 1, 7, 8, 100, 127, 128, 129, 1_000_003] {
            let flags = sfi_flags_with_walk_count(base, count);
            assert_eq!(sfi_walk_count_of(flags), count % 128, "{count}");
            assert_eq!(flags & 0x00ff_ffff, MODIFIER_NATIVE, "{count}");
            assert_ne!(flags & SF_FLAG_RETAIN_CLASS_REF, 0, "{count}");
            // Rewriting replaces, never ORs into, the old count.
            assert_eq!(sfi_walk_count_of(sfi_flags_with_walk_count(flags, 5)), 5);
        }
    }

    /// Interpreter round i2 wave 55, lane L3: a walk's lines keep the count
    /// in two words, then each of the first 128 frames' line and no-file bit,
    /// and decode back to them.
    #[test]
    fn a_walks_lines_keep_each_frames_line_and_no_file_bit() {
        let entry = |line: i32, class: bool, file: bool| StackTraceEntry {
            class_name: std::sync::Arc::from("p/C"),
            method_name: std::sync::Arc::from("m"),
            method_descriptor: None,
            source_file: file.then(|| std::sync::Arc::from("C.java")),
            line_number: line,
            byte_code_index: 0,
            class_id: class.then(|| cratonvm_types::ClassId::new(9)),
            method_index: None,
        };
        let frames = vec![
            entry(12, true, true),
            entry(-1, true, false),
            entry(LINE_NUMBER_NATIVE, true, true),
            entry(65_535, false, false),
        ];
        let lines = walk_lines_for(&frames);
        assert_eq!(lines.len(), WALK_LINES_HEADER + frames.len());
        let decoded: Vec<(i32, bool)> = lines[WALK_LINES_HEADER..]
            .iter()
            .map(|&e| walk_line_decode(e))
            .collect();
        assert_eq!(
            decoded,
            vec![(12, false), (-1, true), (-2, false), (65_535, false)]
        );
        let many: Vec<StackTraceEntry> = (0..300).map(|i| entry(i, true, true)).collect();
        assert_eq!(
            walk_lines_for(&many).len(),
            WALK_LINES_HEADER + WALK_LINES_MAX_FRAMES
        );
        // The index rides where a wave-54 frame keeps its walk count.
        assert_eq!(sfi_walk_count_of(sfi_flags_with_walk_count(0, 127)), 127);
    }

    /// WALK21-1 (JIT round 21 wave 1, lane walk): a deep walk's lines are
    /// chunks of 128 frames, each with the walk's one header; chunk 0 is the
    /// single array of a shorter walk, and frame `i` sits in chunk `i / 128`
    /// at index `i % 128`.
    #[test]
    fn r21_walk_deep_walk_lines_chunks_cover_every_frame() {
        let entry = |line: i32| StackTraceEntry {
            class_name: std::sync::Arc::from("p/C"),
            method_name: std::sync::Arc::from("m"),
            method_descriptor: None,
            source_file: Some(std::sync::Arc::from("C.java")),
            line_number: line,
            byte_code_index: 0,
            class_id: Some(cratonvm_types::ClassId::new(9)),
            method_index: None,
        };
        let frames: Vec<StackTraceEntry> = (0..300).map(entry).collect();
        let count: u64 = 0x1_0000_0005;
        let c0 = walk_lines_chunk_for(&frames, 0, count);
        let c1 = walk_lines_chunk_for(&frames, 1, count);
        let c2 = walk_lines_chunk_for(&frames, 2, count);
        assert_eq!(c0.len(), WALK_LINES_HEADER + WALK_LINES_MAX_FRAMES);
        assert_eq!(c1.len(), WALK_LINES_HEADER + WALK_LINES_MAX_FRAMES);
        assert_eq!(c2.len(), WALK_LINES_HEADER + (300 - 2 * WALK_LINES_MAX_FRAMES));
        assert!(walk_lines_chunk_for(&frames, 3, count).is_empty());
        for chunk in [&c0, &c1, &c2] {
            assert_eq!(walk_lines_header_count(chunk), count);
        }
        let chunks = [&c0, &c1, &c2];
        for i in 0..300usize {
            let lines = chunks[i / WALK_LINES_MAX_FRAMES];
            let at = WALK_LINES_HEADER + i % WALK_LINES_MAX_FRAMES;
            // Cast: a test index below 300.
            assert_eq!(walk_line_decode(lines[at]), (i as i32, false), "{i}");
        }
        // Chunk 0 is exactly the single array of the walk.
        let single = walk_lines_for(&frames);
        assert_eq!(single[WALK_LINES_HEADER..], c0[WALK_LINES_HEADER..]);
        // A walk of at most 128 frames has one chunk.
        assert!(walk_lines_chunk_for(&frames[..128], 1, count).is_empty());
        assert_eq!(walk_lines_header_count(&[1]), 0);
    }

    /// WALK21-4: a frame's lines entry encodes its resolved line as the chunk
    /// builder does, so the slot `settle_walk_line` writes is the one an eager
    /// capture would have built; the deferred sentinels never decode to a line.
    #[test]
    fn r21_walk_deferred_line_slot_encodes_like_the_chunk() {
        let entry = |line: i32, file: bool| StackTraceEntry {
            class_name: std::sync::Arc::from("p/C"),
            method_name: std::sync::Arc::from("m"),
            method_descriptor: None,
            source_file: file.then(|| std::sync::Arc::from("C.java")),
            line_number: line,
            byte_code_index: 0,
            class_id: Some(cratonvm_types::ClassId::new(9)),
            method_index: None,
        };
        for (line, file) in [(1, true), (42, false), (-1, true), (LINE_NUMBER_NATIVE, true)] {
            let e = entry(line, file);
            let chunk = walk_lines_chunk_for(std::slice::from_ref(&e), 0, 0);
            assert_eq!(chunk[WALK_LINES_HEADER], walk_line_encode(&e));
            assert_eq!(walk_line_decode(walk_line_encode(&e)), (line, !file));
        }
        for sentinel in [
            cratonvm_native_api::LINE_NUMBER_DEFERRED,
            cratonvm_native_api::LINE_NUMBER_DEFERRED_AT_BCI_ZERO,
        ] {
            // Clamped to 0, i.e. decoded as -2: never a real line of a frame
            // that could be read before its build settles it.
            assert_eq!(walk_line_decode(walk_line_encode(&entry(sentinel, true))).0, -2);
        }
    }

    /// WALK21-5: the option ordinals are read by name, in the universe's own
    /// order (JDK 22 inserted `DROP_METHOD_INFO`), and answer the bit vector;
    /// an undeclared name or another constant leaves it to the name loop.
    #[test]
    fn r21_walk_option_ordinals_answer_the_bit_vector() {
        let names: Vec<Option<String>> = [
            "RETAIN_CLASS_REFERENCE",
            "SHOW_REFLECT_FRAMES",
            "SHOW_HIDDEN_FRAMES",
            "DROP_METHOD_INFO",
        ]
        .iter()
        .map(|n| Some(n.to_string()))
        .collect();
        let ordinals = option_ordinals_from_names(&names);
        assert_eq!(ordinals, [0, 1, 2, 3]);
        let elements: i64 = (1 << 0) | (1 << 3);
        assert_eq!(option_ordinal_answer(&ordinals, elements, "RETAIN_CLASS_REFERENCE"), Some(true));
        assert_eq!(option_ordinal_answer(&ordinals, elements, "SHOW_REFLECT_FRAMES"), Some(false));
        assert_eq!(option_ordinal_answer(&ordinals, elements, "DROP_METHOD_INFO"), Some(true));
        assert_eq!(option_ordinal_answer(&ordinals, elements, "OTHER"), None);
        // An older JDK's universe: no DROP_METHOD_INFO, another order.
        let old: Vec<Option<String>> = ["SHOW_HIDDEN_FRAMES", "RETAIN_CLASS_REFERENCE", "SHOW_REFLECT_FRAMES"]
            .iter()
            .map(|n| Some(n.to_string()))
            .collect();
        let old = option_ordinals_from_names(&old);
        assert_eq!(old, [1, 2, 0, OPTION_ORDINAL_ABSENT]);
        assert_eq!(option_ordinal_answer(&old, 1, "SHOW_HIDDEN_FRAMES"), Some(true));
        assert_eq!(option_ordinal_answer(&old, 1, "DROP_METHOD_INFO"), None);
    }

    /// WALK21-7: the per-frame split divides capture / screen by the frames
    /// listed, the builds by the frames each built, and the rest of `apply`
    /// by the frames consumed.
    #[test]
    fn r21_walk_frame_split_divides_by_the_right_frames() {
        let mut split = WalkFrameSplit::default();
        // 200 frames listed, 7 built by the first batch, 193 by later ones.
        split.add(200, 7, Some((19_300, 193)), [20_000, 4_000, 700, 59_300]);
        split.add(200, 7, Some((19_300, 193)), [20_000, 4_000, 700, 59_300]);
        let line = split.line(0x10, 200);
        assert!(line.contains("frames=200 calls=2 built/call=200"), "{line}");
        assert!(line.contains("capture/frame=100 ns"), "{line}");
        assert!(line.contains("screen/frame=20 ns"), "{line}");
        assert!(line.contains("first-build/frame=100 ns"), "{line}");
        assert!(line.contains("batch-build/frame=100 ns"), "{line}");
        assert!(line.contains("java+function/frame=200 ns"), "{line}");
        assert!(line.contains("per-walk=84000 ns"), "{line}");
        // A walk with no later batch divides nothing by zero.
        let mut short = WalkFrameSplit::default();
        short.add(3, 3, None, [1, 2, 3, 4]);
        assert!(short.line(0, 3).contains("batch-build/frame=0 ns"));
    }

    /// WALK21-10: lines moved into a walk's holder answer the same chunks
    /// (chunk 0 from its slot, chunk k from the deep holder's element k-1),
    /// and a held walk without deep chunks covers only chunk 0.
    #[test]
    fn r21_walk_held_lines_answer_the_same_chunks() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let object = ctx
            .ensure_class_initialized("java/lang/Object")
            .unwrap_or_else(|_| panic!("Object"));
        let first = ctx.new_array(cratonvm_types::ArrayElementType::Int, 4);
        let chunk1 = ctx.new_array(cratonvm_types::ArrayElementType::Int, 4);
        let deep = ctx.try_new_ref_array(object, 1).expect("deep holder");
        ctx.set_array_element(deep, 0, Value::Object(Some(chunk1)));
        let holder = ctx.try_new_ref_array(object, 3).expect("holder");
        let first_pin = ctx.pin_native_root(first);
        let deep_pin = ctx.pin_native_root(deep);
        let holder_pin = ctx.pin_native_root(holder);
        let lines = WalkLines {
            pin: first_pin,
            lines: first,
            deep: Some((deep_pin, deep)),
            held: None,
        };
        assert_eq!(lines.chunk_of(&mut ctx, 5), Some(first));
        assert_eq!(lines.chunk_of(&mut ctx, 130), Some(chunk1));
        let held = lines.rehome(&mut ctx, holder_pin, holder, 0);
        assert_eq!(held.held, Some(0));
        assert!(held.covers(&mut ctx, 127));
        assert!(held.covers(&mut ctx, 255));
        assert!(!held.covers(&mut ctx, 256));
        assert_eq!(held.chunk_of(&mut ctx, 0), Some(first));
        assert_eq!(held.chunk_of(&mut ctx, 200), Some(chunk1));
        let shallow = WalkLines {
            pin: first_pin,
            lines: first,
            deep: None,
            held: None,
        }
        .rehome(&mut ctx, holder_pin, holder, 0);
        assert!(shallow.covers(&mut ctx, 10));
        assert!(!shallow.covers(&mut ctx, 128));
    }

    /// WALK21-3 (`CRATONVM_STACKWALK_REFLECTION_SCREEN_EXACT`, default ON): a
    /// `jdk.internal.reflect` frame with a class id is a reflection frame
    /// exactly when its class is assignable to `MethodAccessor` /
    /// `ConstructorAccessor` (JDK 25 `isReflectionFrame`), whatever its name
    /// says; a frame with no id keeps the name rule.
    #[test]
    fn r21_walk_exact_reflection_screen_asks_the_accessor_interfaces() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let mut class = |name: &str| {
            ctx.ensure_class_initialized(name)
                .unwrap_or_else(|_| panic!("{name}"))
        };
        let method_accessor = class("jdk/internal/reflect/MethodAccessor");
        let ctor_accessor = class("jdk/internal/reflect/ConstructorAccessor");
        let accessor_impl = class("jdk/internal/reflect/MethodAccessorImpl");
        let direct = class("jdk/internal/reflect/DirectMethodHandleAccessor");
        let adapter = class("jdk/internal/reflect/CsMethodAccessorAdapter");
        let field = class("jdk/internal/reflect/MethodHandleObjectFieldAccessorImpl");
        let ctor = class("jdk/internal/reflect/DirectConstructorHandleAccessor");
        ctx.set_interfaces(accessor_impl, vec![method_accessor]);
        ctx.set_superclass(direct, accessor_impl);
        ctx.set_superclass(adapter, accessor_impl);
        ctx.set_interfaces(ctor, vec![ctor_accessor]);
        let entry = |name: &str, class_id: Option<cratonvm_types::ClassId>| StackTraceEntry {
            class_name: std::sync::Arc::from(name),
            method_name: std::sync::Arc::from("invoke"),
            method_descriptor: None,
            source_file: None,
            line_number: -1,
            byte_code_index: 0,
            class_id,
            method_index: None,
        };
        let exact = |name: &str, class_id| is_reflection_frame_entry(&ctx, &entry(name, class_id));
        assert!(exact("java/lang/reflect/Method", None));
        assert!(exact("java/lang/reflect/Constructor", None));
        assert!(exact("jdk/internal/reflect/DirectMethodHandleAccessor", Some(direct)));
        assert!(exact("jdk/internal/reflect/DirectConstructorHandleAccessor", Some(ctor)));
        // The two the name rule answered unlike the JDK.
        assert!(exact("jdk/internal/reflect/CsMethodAccessorAdapter", Some(adapter)));
        assert!(!exact("jdk/internal/reflect/MethodHandleObjectFieldAccessorImpl", Some(field)));
        assert!(!is_reflection_frame_class("jdk/internal/reflect/CsMethodAccessorAdapter"));
        assert!(is_reflection_frame_class(
            "jdk/internal/reflect/MethodHandleObjectFieldAccessorImpl"
        ));
        // No class id: the name rule. Outside the package: never.
        assert!(exact("jdk/internal/reflect/NativeMethodAccessorImpl", None));
        assert!(!exact("p/DirectMethodHandleAccessor", Some(direct)));
        assert!(!exact("java/lang/reflect/Field", None));
    }

    /// No redefinition since the walk (by the count's residue) answers
    /// `false` without asking the ring.
    #[test]
    fn a_walk_at_the_current_count_was_not_redefined_since() {
        let cid = cratonvm_types::ClassId::new(7);
        let ctx = crate::test_utils::MockNativeContext::new();
        assert!(!sfi_class_redefined_since(&ctx, cid, 5, 5));
        assert!(!sfi_class_redefined_since(&ctx, cid, 5, 133));
        // A residue above `now` with no full lap behind it cannot be.
        assert!(!sfi_class_redefined_since(&ctx, cid, 100, 3));
    }

    /// nio21 wave 6 L1: a context with no per-VM log (the trait default,
    /// the mock) answers `None`, and the reader asks the process ring; an
    /// empty span is answered with no visit either way.
    #[test]
    fn r21w6_redefinition_reader_falls_back_to_the_process_ring() {
        use cratonvm_native_api::NativeClassAccess;
        let ctx = crate::test_utils::MockNativeContext::new();
        let mut seen = 0;
        let mut count = |_c: cratonvm_types::ClassId| seen += 1;
        assert_eq!(ctx.redefined_classes_between(3, 3, &mut count), None);
        let now = cratonvm_classloading::class_redefinition_count();
        assert!(vm_redefined_classes_between(&ctx, now, now, |_| seen += 1));
        assert_eq!(seen, 0);
    }

    #[test]
    fn register_lang_stackwalker_adds_natives() {
        let mut r = NativeMethodRegistry::new();
        register_lang_stackwalker(&mut r);
        assert!(r
            .find(
                "java/lang/StackStreamFactory$AbstractStackWalker",
                "callStackWalk",
                "(JIII[Ljava/lang/Object;[Ljava/lang/Class;)Ljava/lang/Object;"
            )
            .is_some());
        assert!(r
            .find(
                "java/lang/StackStreamFactory$AbstractStackWalker",
                "fetchStackFrames",
                "(JJII[Ljava/lang/Object;)I"
            )
            .is_some());
        assert!(r
            .find("java/lang/StackFrameInfo", "getByteCodeIndex", "()I")
            .is_some());
        assert!(r
            .find(
                "java/lang/StackFrameInfo",
                "getDeclaringClass",
                "()Ljava/lang/Class;"
            )
            .is_some());
    }

    /// gc-common w19-g: `fetchStackFrames` now pins its receiver BEFORE the
    /// allocating frame loop (it pinned the stale post-loop address) and
    /// still releases every pin it takes. The mock never moves an object, so
    /// the relocation itself cannot be staged here; this holds the balance.
    #[test]
    fn w19g_fetch_stack_frames_releases_its_pins() {
        let mut ctx = crate::test_utils::mock_ctx();
        let this = ctx.fresh_object_ref();
        let frames = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 4);
        let before = ctx.native_pin_count_for_test();
        // JDK 25 shape: (this, int mode, long anchor, int lastBatchFrameCount,
        // int batchSize, int startIndex, T[] frames).
        let args = [
            Value::Object(Some(this)),
            Value::Int(0),
            Value::Long(0),
            Value::Int(0),
            Value::Int(4),
            Value::Int(0),
            Value::Object(Some(frames)),
        ];
        // No walk is active on this thread and the mock captures no frames,
        // so nothing is written.
        let r = native_fetch_stack_frames(&mut ctx, &args);
        assert!(matches!(r, Ok(Some(Value::Int(0)))), "got {r:?}");
        assert_eq!(ctx.native_pin_count_for_test(), before);
    }

    fn named(method: &str) -> cratonvm_native_api::StackTraceEntry {
        cratonvm_native_api::StackTraceEntry {
            class_name: std::sync::Arc::from("p/C"),
            method_name: std::sync::Arc::from(method),
            method_descriptor: None,
            source_file: None,
            line_number: -1,
            byte_code_index: -1,
            class_id: None,
            method_index: None,
        }
    }

    fn names(frames: &[cratonvm_native_api::StackTraceEntry]) -> Vec<String> {
        frames.iter().map(|f| f.method_name.to_string()).collect()
    }

    /// Interpreter round i1 wave 48, lane L3: the hidden stand-in frames go
    /// before the frame each insertion names (its caller, the walk being
    /// innermost first), in order, and one past the end appends.
    #[test]
    fn hidden_standin_frames_are_spliced_before_their_callers() {
        let frames = vec![named("target"), named("caller"), named("main")];
        let out = splice_hidden_standin_frames(
            frames,
            vec![
                (1, vec![named("holder"), named("invoker")]),
                (3, vec![named("tail")]),
            ],
        );
        assert_eq!(
            names(&out),
            ["target", "holder", "invoker", "caller", "main", "tail"]
        );
        let unchanged = splice_hidden_standin_frames(vec![named("a"), named("b")], Vec::new());
        assert_eq!(names(&unchanged), ["a", "b"]);
    }

    fn walk_entry(
        class: &str,
        method: &str,
        descriptor: &str,
        class_id: Option<u32>,
    ) -> cratonvm_native_api::StackTraceEntry {
        cratonvm_native_api::StackTraceEntry {
            class_name: std::sync::Arc::from(class),
            method_name: std::sync::Arc::from(method),
            method_descriptor: Some(std::sync::Arc::from(descriptor)),
            source_file: None,
            line_number: -1,
            byte_code_index: -1,
            class_id: class_id.map(cratonvm_types::ClassId::new),
            method_index: None,
        }
    }

    /// Interpreter round i6 wave 115, lane L3 (`CRATONVM_STANDIN_SPUN_IWA_FRAMES`):
    /// a served `invokeWithArguments` target is found only under its direct
    /// handle's holder, the call site's invoker and the array overload's
    /// frame; its spun frames go before the invoker, and `n` is the holder's
    /// parameters but the handle.
    #[test]
    fn i115_l3_spun_iwa_sites_need_the_whole_served_chain() {
        const OBJ: &str = "Ljava/lang/Object;";
        let frames = vec![
            walk_entry("p/C", "tInt2", "(II)I", Some(7)),
            walk_entry(
                "java/lang/invoke/DirectMethodHandle$Holder",
                "invokeStatic",
                &format!("({OBJ}II)I"),
                Some(1),
            ),
            walk_entry(
                "java/lang/invoke/Invokers$Holder",
                "invokeExact_MT",
                &format!("({OBJ}{OBJ}{OBJ}{OBJ}){OBJ}"),
                Some(2),
            ),
            walk_entry(
                "java/lang/invoke/MethodHandle",
                "invokeWithArguments",
                &format!("([{OBJ}){OBJ}"),
                Some(3),
            ),
            walk_entry("p/C", "main", "([Ljava/lang/String;)V", Some(7)),
        ];
        let sites = spun_iwa_sites(&frames);
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].at, 2);
        assert_eq!(sites[0].arity, 2);
        assert_eq!(&*sites[0].holder, "invokeStatic");
        assert_eq!(&*sites[0].name, "tInt2");
        // Not under invokeWithArguments (a plain invokeExact): nothing.
        let mut exact = frames.clone();
        exact.remove(3);
        assert!(spun_iwa_sites(&exact).is_empty());
        assert!(!sites[0].holderless);
        // No holder named (a shape the image does not pregenerate): since
        // wave 138 a holderless site, before the invoker, which only the
        // record path answers.
        let mut unnamed = frames.clone();
        unnamed.remove(1);
        let holderless = spun_iwa_sites(&unnamed);
        assert_eq!(holderless.len(), 1);
        assert!(holderless[0].holderless);
        assert_eq!(holderless[0].at, 1);
        assert_eq!(&*holderless[0].name, "tInt2");
        // ... and none under a non-array overload.
        let mut list = unnamed.clone();
        list[2].method_descriptor = Some(std::sync::Arc::from("(Ljava/util/List;)Ljava/lang/Object;"));
        assert!(spun_iwa_sites(&list).is_empty());
        // A target with no class id: nothing.
        let mut anonymous = frames;
        anonymous[0].class_id = None;
        assert!(spun_iwa_sites(&anonymous).is_empty());
    }

    /// Interpreter round i7 wave 137, lane L3: the walk's served
    /// `invokeWithArguments` frames (innermost first) pair with the served
    /// rows (outermost first) only when the counts agree; the `List`
    /// overload's own frame is not counted.
    #[test]
    fn i137_l3_iwa_frame_rows_pair_innermost_frame_with_innermost_row() {
        const ARRAY: &str = "([Ljava/lang/Object;)Ljava/lang/Object;";
        const LIST: &str = "(Ljava/util/List;)Ljava/lang/Object;";
        let mh = "java/lang/invoke/MethodHandle";
        let frames = vec![
            walk_entry("p/C", "inner", "()V", Some(7)),
            walk_entry(mh, "invokeWithArguments", ARRAY, Some(3)),
            walk_entry("p/C", "middle", "()V", Some(7)),
            walk_entry(mh, "invokeWithArguments", ARRAY, Some(3)),
            walk_entry(mh, "invokeWithArguments", LIST, Some(3)),
            walk_entry("p/C", "main", "([Ljava/lang/String;)V", Some(7)),
        ];
        assert_eq!(iwa_frame_rows(&frames, 2), Some(vec![(1, 1), (3, 0)]));
        assert_eq!(iwa_frame_rows(&frames, 1), None, "a row listed by no frame");
        assert_eq!(iwa_frame_rows(&frames, 3), None);
        assert_eq!(iwa_frame_rows(&frames[..1], 0), Some(Vec::new()));
        let mut dotted = frames;
        dotted[1].class_name = std::sync::Arc::from("java.lang.invoke.MethodHandle");
        assert_eq!(iwa_frame_rows(&dotted, 2), Some(vec![(1, 1), (3, 0)]));
    }

    #[test]
    fn i115_l3_descriptor_parameter_count_reads_every_kind() {
        assert_eq!(descriptor_parameter_count("()V"), Some(0));
        assert_eq!(
            descriptor_parameter_count("(Ljava/lang/Object;I[[JLp/C;D)V"),
            Some(5)
        );
        assert_eq!(descriptor_parameter_count("(Q)V"), None);
        assert_eq!(descriptor_parameter_count("V"), None);
        assert_eq!(
            generic_form_descriptor(2),
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
        );
    }

    /// Interpreter round i6 wave 121, lane L3: an `invokeExact` site's type
    /// as `MethodType.basicType()` spells it, and the frames the linker step
    /// may stand on.
    #[test]
    fn i121_l3_spun_linker_basic_type_and_callee() {
        const OBJ: &str = "Ljava/lang/Object;";
        assert_eq!(basic_call_descriptor("(I)I").as_deref(), Some("(I)I"));
        assert_eq!(
            basic_call_descriptor("(Ljava/lang/String;D)D").as_deref(),
            Some(format!("({OBJ}D)D").as_str())
        );
        assert_eq!(
            basic_call_descriptor("(ZBSC[I[[Lp/C;J)Z").as_deref(),
            Some(format!("(IIII{OBJ}{OBJ}J)I").as_str())
        );
        assert_eq!(basic_call_descriptor("()V").as_deref(), Some("()V"));
        assert_eq!(basic_call_descriptor("(V)V"), None);
        assert_eq!(basic_call_descriptor("(Q)V"), None);
        // 127 longs take 254 slots: the customized linker, not listed.
        assert_eq!(basic_call_descriptor(&format!("({})V", "J".repeat(127))), None);
        assert!(basic_call_descriptor(&format!("({})V", "J".repeat(126))).is_some());
        let at = |class: &str, method: &str| walk_entry(class, method, "()V", None);
        assert!(spun_linker_callee(&at(
            "java/lang/invoke/DirectMethodHandle$Holder",
            "invokeStatic"
        )));
        assert!(spun_linker_callee(&at("java/lang/invoke/LambdaForm$MH/0x1", "invoke")));
        assert!(spun_linker_callee(&at("java.lang.invoke.LambdaForm$DMH/0x2", "invokeStatic")));
        assert!(!spun_linker_callee(&at("java/lang/invoke/Invokers$Holder", "invokeExact_MT")));
        assert!(!spun_linker_callee(&at("java/lang/invoke/LambdaForm$MH/0x3", "invokeExact_MT")));
        assert!(!spun_linker_callee(&at("p/C", "target")));
    }

    /// A VM that lists no hidden stand-in frames (the mock's default) never
    /// asks the walker's options.
    #[test]
    fn a_vm_that_lists_no_hidden_standins_does_not_ask_the_walker() {
        let mut ctx = crate::test_utils::mock_ctx();
        let read = std::cell::Cell::new(false);
        let (frames, asked) =
            add_hidden_standin_frames(&mut ctx, vec![named("target"), named("caller")], |_| {
                read.set(true);
                true
            });
        assert!(!read.get(), "the walker's options were read");
        assert_eq!(asked, None);
        assert_eq!(names(&frames), ["target", "caller"]);
    }
}
