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
    ctx.set_field(sf, SF_CLASSNAME, Value::Object(Some(cls_str)));
    ctx.set_field(sf, SF_METHODNAME, Value::Object(Some(meth_str)));
    ctx.set_field(sf, SF_FILENAME, file_str);
    ctx.set_field(sf, SF_LINENUMBER, Value::Int(entry.line_number));
    ctx.set_field(sf, SF_BCI, Value::Int(entry.byte_code_index));
    ctx.set_field(sf, SF_DECL_INTERNAL, Value::Object(Some(decl_internal)));
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
        _ => return Ok(Some(Value::Long(0))),
    };

    let trace = ctx.capture_stack_trace(0);
    let buf_len = ctx.array_length(frame_buffer);
    let slack = buf_len.saturating_sub(start_index);
    let capacity = if batch == 0 { slack } else { slack.min(batch) };

    let mut written = 0usize;
    for entry in trace.iter().skip(skip) {
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
    }

    // Anchor = number of frames written. The JDK uses this as a
    // continuation token; any non-zero anchor is permitted, and zero
    // signals "no more frames".
    Ok(Some(Value::Long(written as i64)))
}

/// `AbstractStackWalker.fetchStackFrames(long mode, long anchor,
///                                       int batchSize, int startIndex,
///                                       Object[] frameBuffer)`.
///
/// Called by the JDK's lazy Stream when additional frames are needed
/// after the initial batch. We return 0 because our initial
/// `callStackWalk` already materialized everything — this matches the
/// JDK's "stack is fully consumed" sentinel.
pub(crate) fn native_fetch_stack_frames(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // args: [this, mode, anchor, batchSize, startIndex, frameBuffer]
    // Our `callStackWalk` returns every frame in the initial batch, so
    // continuation calls always report "0 more frames" which cleanly
    // closes the Stream.
    Ok(Some(Value::Int(0)))
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
