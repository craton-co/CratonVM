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

use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry, StackTraceEntry};
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
    ctx.class_name_of_id(ctx.class_id_of_object(obj))
        .is_some_and(|n| n == "java/lang/Class")
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

fn populate_sfi(
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
        None => std::sync::Arc::from(entry.class_name.replace('/', ".")),
    };

    // ---- Allocate everything, pinning each object as it is created so the
    // next allocation cannot move/collect it. Pins return sequential handles;
    // `base` (the first) releases the whole batch. ----
    let mut cls_str = ctx.create_string(&dotted);
    let base = ctx.pin_native_root(cls_str);
    let mut meth_str = ctx.create_string(&entry.method_name);
    let h_meth = ctx.pin_native_root(meth_str);
    let (mut file_str, h_file) = match &entry.source_file {
        Some(f) => {
            let s = ctx.create_string(f);
            let h = ctx.pin_native_root(s);
            (Some(s), Some(h))
        }
        None => (None, None),
    };
    let mut decl_internal = ctx.create_string(&entry.class_name);
    let h_decl = ctx.pin_native_root(decl_internal);
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

    // ---- All allocations done. Read every reference back through its pin so
    // we use the *current* (post-GC, possibly forwarded) location. ----
    cls_str = ctx.read_native_pin(base, cls_str);
    meth_str = ctx.read_native_pin(h_meth, meth_str);
    if let (Some(s), Some(h)) = (file_str, h_file) {
        file_str = Some(ctx.read_native_pin(h, s));
    }
    decl_internal = ctx.read_native_pin(h_decl, decl_internal);
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
    let class_mirror_val = match class_mirror {
        Some(m) => Value::Object(Some(m)),
        None => Value::Object(None),
    };
    // One memoized layout read instead of five name resolutions per frame; see
    // `sfi_layout`. `None` (synthetic-JDK, or the class not yet loaded) keeps
    // the by-name writes this replaced.
    let layout = sfi_layout(ctx);
    sfi_set(
        ctx,
        sf,
        "classOrMemberName",
        layout.map(|l| l.class_or_member_name),
        class_mirror_val,
    );
    // `flags` mirrors ClassFrameInfo's: bit 0 is RETAIN_CLASS_REF (copied from
    // the walker), the `Modifier` bits above it are read by `isNativeMethod` /
    // `getLineNumber` / `getByteCodeIndex`. We do not (yet) know a frame's
    // method modifiers here, so only the retain bit is set.
    sfi_set(
        ctx,
        sf,
        "flags",
        layout.map(|l| l.flags),
        Value::Int(if retain_class_ref {
            SF_FLAG_RETAIN_CLASS_REF
        } else {
            0
        }),
    );
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
    ctx.set_field(sf, SF_DECL_INTERNAL, Value::Object(Some(decl_internal)));

    ctx.unpin_native_roots(base);
    Ok(sf)
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

    let trace = ctx.capture_stack_trace(0);
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

    let ordered = Rc::new(ordered_stack_walk_frames(&trace));
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
    let mut frame_buffer = frame_buffer;
    while written < capacity && pos < ordered.len() {
        let sfi = populate_sfi(ctx, &ordered[pos], retain_class_ref);
        frame_buffer = ctx.read_native_pin(fb_pin, frame_buffer);
        ctx.set_array_element(
            frame_buffer,
            start_index + written,
            Value::Object(Some(sfi?)),
        );
        pos += 1;
        written += 1;
    }
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
    let _ = mode_long;
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
            let trace = ctx.capture_stack_trace(0);
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
    let mut buffer = buffer;
    for entry in ordered.iter().skip(cursor) {
        if written >= slack {
            break;
        }
        let sfi = populate_sfi(ctx, entry, retain_class_ref);
        buffer = ctx.read_native_pin(buf_pin, buffer);
        ctx.set_array_element(buffer, start + written, Value::Object(Some(sfi?)));
        written += 1;
        new_cursor += 1;
    }
    // `populate_sfi` allocates a StackFrameInfo per frame, so the receiver
    // read out of `args` below is a pre-loop address. `buffer` was already
    // pinned for the same reason; `args` was not. Read it BEFORE releasing
    // `buf_pin`, which truncates the pin stack.
    let this_pinned = match args.first() {
        Some(Value::Object(Some(o))) => {
            let p = ctx.pin_native_root(*o);
            let refreshed = ctx.read_native_pin(p, *o);
            Some(refreshed)
        }
        _ => None,
    };
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
    // JDK 25 splits the long `mode` into two ints and inserts
    // ContinuationScope + Continuation references between mode/skip and
    // batch/startIndex/frameBuffer:
    //   callStackWalk(int mode, int flags, ContinuationScope,
    //                 Continuation, int batch, int startIndex,
    //                 Object[] frameBuffer)
    // We collapse it onto `native_call_stack_walk` by reordering args into
    // the original (this, mode, skip, batch, startIndex, frameBuffer, _)
    // shape. The two int "mode/flags" are merged into the long mode slot;
    // the ContinuationScope / Continuation are dropped (we have no
    // virtual-thread continuation support so the user-visible behaviour
    // matches the platform-thread code path).
    registry.register_with_kind(
        asw,
        "callStackWalk",
        "(IILjdk/internal/vm/ContinuationScope;Ljdk/internal/vm/Continuation;II[Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            // args = (this, mode, flags, contScope, continuation,
            //         batch, startIndex, frameBuffer)
            let mode_lo = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as i64;
            let mode_hi = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as i64;
            let mode = (mode_hi << 32) | (mode_lo & 0xFFFF_FFFF);
            let reordered = [
                args.first().copied().unwrap_or(Value::Object(None)),
                Value::Long(mode),
                Value::Int(0), // skip — JDK 25 absorbs this into mode/flags
                args.get(5).copied().unwrap_or(Value::Int(0)),
                args.get(6).copied().unwrap_or(Value::Int(0)),
                args.get(7).copied().unwrap_or(Value::Object(None)),
                Value::Object(None),
            ];
            native_call_stack_walk(ctx, &reordered)
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
            let reordered = [
                args.first().copied().unwrap_or(Value::Object(None)),
                args.get(1).copied().unwrap_or(Value::Long(0)),
                args.get(2).copied().unwrap_or(Value::Int(0)),
                args.get(5).copied().unwrap_or(Value::Int(0)),
                args.get(6).copied().unwrap_or(Value::Int(0)),
                args.get(7).copied().unwrap_or(Value::Object(None)),
                Value::Object(None),
            ];
            native_call_stack_walk(ctx, &reordered)
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

    // StackFrameInfo — native overrides run before bytecode (see
    // `try_stackless_invoke` / `invoke_or_native`).  Read real JDK fields
    // (`name`, `bci`, `flags`) and the eagerly-filled `ste` mirror.
    let sfi = "java/lang/StackFrameInfo";
    registry.register(sfi, "getClassName", "()Ljava/lang/String;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        if let Value::Object(Some(ste)) = ctx.get_field_by_name(this, "ste") {
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
        if let Value::Object(Some(ste)) = ctx.get_field_by_name(this, "ste") {
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
        if let Value::Object(Some(ste)) = ctx.get_field_by_name(this, "ste") {
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
                if is_class_mirror(ctx, m) {
                    return Ok(Some(Value::Object(Some(m))));
                }
            }
            let internal = match ctx.get_field(this, SF_DECL_INTERNAL) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            if !internal.is_empty() {
                if let Some(cid) = ctx.class_id_by_name(&internal) {
                    let mirror = ctx.get_class_mirror(cid);
                    return Ok(Some(Value::Object(Some(mirror))));
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
    registry.register(
        sfi,
        "toStackTraceElement",
        "()Ljava/lang/StackTraceElement;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            if let Value::Object(Some(ste)) = ctx.get_field_by_name(this, "ste") {
                return Ok(Some(Value::Object(Some(ste))));
            }
            let ste = try_alloc_concurrent_synthetic(ctx, "java/lang/StackTraceElement", 4)?;
            // Decode SFI slots into the values `fill_stack_trace_element`
            // needs. `SF_CLASSNAME` holds the dotted class name; the
            // `/`-separated internal name lives in `SF_DECL_INTERNAL`.
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
            crate::lang_misc::fill_stack_trace_element(
                ctx,
                ste,
                &class_slashed,
                &class_dotted,
                &method_name,
                file_name.as_deref(),
                line,
            );
            Ok(Some(Value::Object(Some(ste))))
        },
    );

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
            // Prefer the internal-name slot we populate at SFI-construct
            // time so we avoid the dotted-class-name round-trip through
            // class_id_by_name (which would NPE on synthetic frames).
            let internal = match ctx.get_field(this, SF_DECL_INTERNAL) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
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
            // Pick the first declared overload that matches the method name.
            // See the comment above on overload disambiguation.
            let descriptor = ctx
                .declared_methods(class_id)
                .into_iter()
                .find(|m| m.name == method_name)
                .map(|m| m.descriptor);
            let Some(desc) = descriptor else {
                return Ok(Some(Value::Object(None)));
            };
            // Delegate to MethodType.fromMethodDescriptorString — the JDK
            // factory parses (paramTypes)returnType and allocates a real
            // MethodType. We pass `null` for the ClassLoader to use the
            // system loader (matches what the JDK's own StackFrameInfo
            // wiring does in expandStackFrameInfo).
            let desc_str = ctx.create_string(&desc);
            ctx.invoke(
                "java/lang/invoke/MethodType",
                "fromMethodDescriptorString",
                "(Ljava/lang/String;Ljava/lang/ClassLoader;)Ljava/lang/invoke/MethodType;",
                &[Value::Object(Some(desc_str)), Value::Object(None)],
            )
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
        let internal = match ctx.get_field(this, SF_DECL_INTERNAL) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let method_name = match ctx.get_field_by_name(this, "name") {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let descriptor = ctx
            .class_id_by_name(&internal)
            .map(|class_id| ctx.declared_methods(class_id))
            .and_then(|methods| {
                methods
                    .into_iter()
                    .find(|method| method.name == method_name)
                    .map(|method| method.descriptor)
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
            // `ResolvedMethodName` here, which is not what this returns.
            if is_class_mirror(ctx, m) {
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
    registry.register(
        "java/lang/ClassFrameInfo",
        "getClassName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
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
            let internal = match ctx.get_field(this, SF_DECL_INTERNAL) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
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

    #[test]
    fn retain_class_reference_bit_matches_jdk_25_class_frame_info_layout() {
        assert_eq!(SF_FLAG_RETAIN_CLASS_REF, 1 << 27);
        assert_eq!(SF_FLAG_RETAIN_CLASS_REF & 0x00ff_ffff, 0);
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
}
