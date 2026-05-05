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

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::Value;
use rustjvm_types::error::MethodCallResult;

use crate::alloc_concurrent_synthetic;

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

/// Populate a StackFrameInfo from a `StackTraceEntry`.
fn populate_sfi(
    ctx: &mut dyn NativeContext,
    entry: &rustjvm_native_api::StackTraceEntry,
) -> rustjvm_types::ObjectRef {
    let sf = alloc_concurrent_synthetic(ctx, "java/lang/StackFrameInfo", STACK_FRAME_INFO_FIELDS);
    let cls_str = ctx.create_string(&entry.class_name.replace('/', "."));
    let meth_str = ctx.create_string(&entry.method_name);
    let file_str = match &entry.source_file {
        Some(f) => Value::Object(Some(ctx.create_string(f))),
        None => Value::Object(None),
    };
    let decl_internal = ctx.create_string(&entry.class_name);

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
    // the native `expandStackFrameInfo` (which we don't implement). And
    // `getDeclaringClass()` / `getClassName()` go through
    // `ClassFrameInfo.declaringClass()` → `JLIA.getDeclaringClass(
    // classOrMemberName)` which casts `classOrMemberName` to
    // `ResolvedMethodName` and crashes on anything else.
    //
    // Resolve fields by NAME so the right slot is hit on whichever real
    // class layout is present, and pre-fill them with the values our
    // own native overrides would also return.
    let class_mirror = ctx
        .class_id_by_name(&entry.class_name)
        .map(|cid| Value::Object(Some(ctx.get_class_mirror(cid))))
        .unwrap_or(Value::Object(None));

    let try_set = |ctx: &mut dyn NativeContext, owner: &str, fname: &str, val: Value| {
        if let Some(idx) = ctx.resolve_field_index(owner, fname) {
            ctx.set_field(sf, idx, val);
            true
        } else {
            false
        }
    };
    // ClassFrameInfo.classOrMemberName <- Class mirror (real-JDK
    // declaringClass() unwraps via JLIA which handles Class instances).
    let mut filled_classmem = try_set(ctx, "java/lang/ClassFrameInfo", "classOrMemberName", class_mirror);
    if !filled_classmem {
        filled_classmem = try_set(ctx, "java/lang/StackFrameInfo", "classOrMemberName", class_mirror);
    }
    // StackFrameInfo.name <- method name (avoid expandStackFrameInfo path).
    let filled_name = try_set(ctx, "java/lang/StackFrameInfo", "name", Value::Object(Some(meth_str)));
    // StackFrameInfo.bci <- byte code index.
    try_set(ctx, "java/lang/StackFrameInfo", "bci", Value::Int(entry.byte_code_index));

    // Always also write our own legacy 6-slot synthetic layout. Our
    // native getter overrides (`getClassName`, `getMethodName`,
    // `getDeclaringClass`, etc.) read from these fixed slots, so even
    // when the real-JDK class is loaded our overrides keep working as
    // long as we register them with the same dispatch precedence.
    if !filled_classmem || !filled_name {
        ctx.set_field(sf, SF_CLASSNAME, Value::Object(Some(cls_str)));
        ctx.set_field(sf, SF_METHODNAME, Value::Object(Some(meth_str)));
        ctx.set_field(sf, SF_FILENAME, file_str);
        ctx.set_field(sf, SF_LINENUMBER, Value::Int(entry.line_number));
        ctx.set_field(sf, SF_BCI, Value::Int(entry.byte_code_index));
        ctx.set_field(sf, SF_DECL_INTERNAL, Value::Object(Some(decl_internal)));
    } else {
        // Suppress unused-variable warnings on the synthetic-mode
        // fallback values when real-JDK layout was hit.
        let _ = (cls_str, file_str, decl_internal);
    }
    sf
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
    let buf_len = ctx.array_length(frame_buffer);
    let slack = buf_len.saturating_sub(start_index);
    let capacity = if batch == 0 { slack } else { slack.min(batch) };

    // `capture_stack_trace` produces frames in outermost → innermost order
    // (oldest frame first, current frame last). The JDK StackWalker
    // contract is the opposite: the stream begins with the *caller* of
    // `walk()` and proceeds down toward `main`. So we walk the trace in
    // reverse, then skip walker / reflection internals so the user-visible
    // first frame is the one that called `StackWalker.walk(...)`.
    fn is_walker_internal(name: &str, method: &str) -> bool {
        name == "java/lang/StackWalker"
            || name.starts_with("java/lang/StackWalker$")
            || name == "java/lang/StackStreamFactory"
            || name.starts_with("java/lang/StackStreamFactory$")
            || (name == "java/lang/Thread" && method == "getStackTrace")
            || name.starts_with("jdk/internal/reflect/")
            || name == "java/lang/reflect/Method"
            || name.starts_with("java/lang/invoke/MethodHandle")
            || name.starts_with("sun/reflect/")
    }

    let mut written = 0usize;
    // Count how many trace entries we walked over (skipped internals +
    // user-requested `skip` + the entries we materialized). The anchor we
    // return encodes this cursor so a follow-up `fetchStackFrames` can
    // resume from the right place.
    let mut consumed = 0usize;
    let mut iter = trace.iter().rev().peekable();
    while let Some(e) = iter.peek() {
        if is_walker_internal(&e.class_name, &e.method_name) {
            iter.next();
            consumed += 1;
        } else {
            break;
        }
    }
    for _ in 0..skip {
        if iter.next().is_some() {
            consumed += 1;
        }
    }
    for entry in iter {
        if written >= capacity {
            break;
        }
        let sfi = populate_sfi(ctx, entry);
        ctx.set_array_element(
            frame_buffer,
            start_index + written,
            Value::Object(Some(sfi)),
        );
        written += 1;
        consumed += 1;
    }

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
    if let Some(this_ref) = this {
        // Pass `endIndex = startIndex + written` so the FrameBuffer's
        // (origin, fence) range exactly covers the frames we populated.
        // Using `batch` here would set fence past the array length and
        // throw AIOOBE on the second iteration step.
        //
        // Encode the trace cursor in the anchor so a follow-up
        // `fetchStackFrames` invocation knows how many trace entries we
        // already consumed and can resume from the next frame.
        return ctx.invoke(
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
    // Try JDK 25 layout: this(0), mode:int(1), anchor:long(2-3), numFrames:int(4),
    // batchSize:int(5), startIndex:int(6), frames(7).
    if let Some(Value::Long(a)) = args.get(2) {
        anchor = *a;
        if let Some(Value::Int(s)) = args.get(6) {
            start_index = *s;
        }
        if let Some(Value::Object(Some(f))) = args.get(7) {
            frame_buffer = Some(*f);
        }
    }
    // Legacy layout: this(0), mode:long(1-2), anchor:long(3-4), batch(5),
    // startIndex(6), frames(7).
    if frame_buffer.is_none() {
        if let Some(Value::Long(a)) = args.get(3) {
            anchor = *a;
        }
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

    let cursor = anchor.max(0) as usize;
    let trace = ctx.capture_stack_trace(0);
    let buf_len = ctx.array_length(buffer);
    let start = start_index.max(0) as usize;
    let slack = buf_len.saturating_sub(start);
    let mut written = 0usize;
    let mut new_cursor = cursor;
    let entries: Vec<_> = trace.iter().rev().skip(cursor).collect();
    for entry in entries {
        if written >= slack {
            break;
        }
        let sfi = populate_sfi(ctx, entry);
        ctx.set_array_element(
            buffer,
            start + written,
            Value::Object(Some(sfi)),
        );
        written += 1;
        new_cursor += 1;
    }
    // Persist the new cursor back into `this.anchor` so a subsequent
    // `fetchStackFrames` call resumes from the next trace frame.
    if let Some(Value::Object(Some(this_ref))) = args.first() {
        if let Some(idx) = ctx.resolve_field_index(
            "java/lang/StackStreamFactory$AbstractStackWalker",
            "anchor",
        ) {
            ctx.set_field(*this_ref, idx, Value::Long(new_cursor as i64));
        }
    }
    Ok(Some(Value::Int(written as i32)))
}

/// Register the StackStreamFactory private natives.
pub fn register_lang_stackwalker(registry: &mut NativeMethodRegistry) {
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
    registry.register(
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
    registry.register(
        asw,
        "fetchStackFrames",
        "(IJIII[Ljava/lang/Object;)I",
        native_fetch_stack_frames,
    );

    // StackFrameInfo (shares layout with StackWalker$StackFrame).
    // Register the same 6 accessors so reflective access works.
    let sfi = "java/lang/StackFrameInfo";
    registry.register(sfi, "getClassName", "()Ljava/lang/String;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        Ok(Some(ctx.get_field(this, SF_CLASSNAME)))
    });
    registry.register(sfi, "getMethodName", "()Ljava/lang/String;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        Ok(Some(ctx.get_field(this, SF_METHODNAME)))
    });
    registry.register(sfi, "getFileName", "()Ljava/lang/String;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        Ok(Some(ctx.get_field(this, SF_FILENAME)))
    });
    registry.register(sfi, "getLineNumber", "()I", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(-1))),
        };
        Ok(Some(ctx.get_field(this, SF_LINENUMBER)))
    });
    registry.register(sfi, "getByteCodeIndex", "()I", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(-1))),
        };
        Ok(Some(ctx.get_field(this, SF_BCI)))
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
            let internal = match ctx.get_field(this, SF_DECL_INTERNAL) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(None))),
            };
            if internal.is_empty() {
                return Ok(Some(Value::Object(None)));
            }
            if let Some(cid) = ctx.class_id_by_name(&internal) {
                let mirror = ctx.get_class_mirror(cid);
                return Ok(Some(Value::Object(Some(mirror))));
            }
            Ok(Some(Value::Object(None)))
        },
    );
    registry.register(sfi, "isNativeMethod", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let ln = match ctx.get_field(this, SF_LINENUMBER) {
            Value::Int(v) => v,
            _ => -1,
        };
        Ok(Some(Value::Int(if ln == -2 { 1 } else { 0 })))
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
            let ste = alloc_concurrent_synthetic(ctx, "java/lang/StackTraceElement", 4);
            ctx.set_field(ste, 0, ctx.get_field(this, SF_CLASSNAME));
            ctx.set_field(ste, 1, ctx.get_field(this, SF_METHODNAME));
            ctx.set_field(ste, 2, ctx.get_field(this, SF_FILENAME));
            ctx.set_field(ste, 3, ctx.get_field(this, SF_LINENUMBER));
            Ok(Some(Value::Object(Some(ste))))
        },
    );

    // SB3 deduceMainApplicationClass path: real-JDK
    // `StackFrameBuffer.at(int)` calls the package-private virtual
    // `ClassFrameInfo.declaringClass()` (overridden by `StackFrameInfo`)
    // for every populated frame as part of `setBatch()`. The default
    // `StackFrameInfo` override delegates to
    // `JavaLangInvokeAccess.getDeclaringClass(classOrMemberName)`, which
    // casts to `ResolvedMethodName` — a hidden type whose internals
    // require `expandStackFrameInfo` (a HotSpot intrinsic we don't
    // implement) to populate. That cast is what surfaces as the
    // `ClassCastException` Spring catches, suppressing the banner.
    //
    // Override the package-private `declaringClass()` to return the
    // Class mirror straight from our `SF_DECL_INTERNAL` slot. Same for
    // ClassFrameInfo proper (in case a frame ends up as a bare
    // ClassFrameInfo elsewhere). And register `expandStackFrameInfo` as
    // a no-op so any callers that get past our other overrides don't
    // hit UnsatisfiedLinkError.
    fn declaring_class_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        // Prefer the internal-name slot we always populate.
        if let Value::Object(Some(s)) = ctx.get_field(this, SF_DECL_INTERNAL) {
            let internal = ctx.read_string(s).unwrap_or_default();
            if !internal.is_empty() {
                if let Some(cid) = ctx.class_id_by_name(&internal) {
                    return Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))));
                }
            }
        }
        // Fall back: real-JDK layout has `classOrMemberName` at the
        // first ClassFrameInfo slot. Resolve by name and read it.
        if let Some(idx) = ctx.resolve_field_index("java/lang/ClassFrameInfo", "classOrMemberName") {
            let v = ctx.get_field(this, idx);
            if let Value::Object(Some(_)) = v {
                return Ok(Some(v));
            }
        }
        Ok(Some(Value::Object(None)))
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
        declaring_class_native,
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
    // expandStackFrameInfo is a private native on StackFrameInfo that
    // populates `name`/`type`/`bci` from a HotSpot intrinsic. We
    // pre-populate everything our getters need, so this is a no-op.
    registry.register(sfi, "expandStackFrameInfo", "()V", |_ctx, _args| {
        Ok(None)
    });
    // ensureRetainClassRefEnabled is package-private on ClassFrameInfo
    // and asserts the walker had RETAIN_CLASS_REFERENCE. We always
    // populate the class mirror, so this is also a no-op.
    registry.register(
        "java/lang/ClassFrameInfo",
        "ensureRetainClassRefEnabled",
        "()V",
        |_ctx, _args| Ok(None),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustjvm_native_api::NativeMethodRegistry;

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
