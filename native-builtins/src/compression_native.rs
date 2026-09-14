// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Native shims for the third-party compression JNI used by Apache Kafka's
//! record codecs.
//!
//! Kafka's `org.apache.kafka.common.compress` codecs delegate to two native
//! libraries that the VM does not otherwise back:
//!
//!  * **snappy-java** (`org.xerial.snappy.SnappyNative`) — the canonical Snappy
//!    *block* format. `SnappyInputStream`/`SnappyOutputStream` do their stream
//!    framing in pure Java and call the native layer only for per-block
//!    `rawCompress`/`rawUncompress`/`maxCompressedLength`/`uncompressedLength`.
//!    We back those with the pure-Rust `snap` crate, whose `snap::raw` format is
//!    the same canonical Snappy block format, so the Java framing round-trips.
//!
//!  * **zstd-jni** (`com.github.luben.zstd.*`) — the zstd *streaming* format.
//!    `ZstdOutputStreamNoFinalizer`/`ZstdInputStreamNoFinalizer` keep a single
//!    `CStream`/`DStream` context across many `write`/`read` calls and drive it
//!    through `compressStream`/`flushStream`/`endStream`/`decompressStream`.
//!    The JNI contract is a direct mirror of libzstd's `ZSTD_compressStream2`/
//!    `ZSTD_decompressStream`: each native reads the `srcPos`/`dstPos` instance
//!    fields as the in/out buffer cursors, advances the contexts, and writes the
//!    fields back. We replicate exactly that against `zstd_safe::CCtx`/`DCtx`
//!    (bundled libzstd — byte-compatible with zstd-jni's libzstd, so a frame
//!    produced by one decodes with the other). Because the contract tracks
//!    libzstd rather than any zstd-jni internal, it is version-stable.
//!
//! Native libraries load as no-ops in this VM (`System.load` succeeds without a
//! real .so/.dll), so the only thing missing is the native method bodies; the
//! Java loader/stream classes run unchanged once these are registered. Mirrors
//! the `flate2`-backed Inflater/Deflater approach in [`crate::zip_real`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use zstd::zstd_safe::zstd_sys::ZSTD_EndDirective;
use zstd::zstd_safe::{
    self, CCtx, CParameter, DCtx, DParameter, InBuffer, OutBuffer, ResetDirective,
};

// ---------------------------------------------------------------------------
// Argument / value / field helpers
// ---------------------------------------------------------------------------

fn arg_long(args: &[Value], idx: usize) -> i64 {
    match args.get(idx) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    }
}

fn arg_int(args: &[Value], idx: usize) -> i32 {
    match args.get(idx) {
        Some(Value::Int(v)) => *v,
        Some(Value::Long(v)) => *v as i32,
        _ => 0,
    }
}

fn arg_obj(args: &[Value], idx: usize) -> Option<ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(o)) => *o,
        _ => None,
    }
}

/// Read `byte[]`-array bytes in `[off, off+len)`, clamped to the array bounds.
fn read_bytes(ctx: &dyn NativeContext, arr: Option<ObjectRef>, off: usize, len: usize) -> Vec<u8> {
    match arr {
        Some(a) => {
            let total = ctx.array_length(a);
            let begin = off.min(total);
            let end = off.saturating_add(len).min(total);
            let mut buf = vec![0u8; end - begin];
            if !buf.is_empty() {
                ctx.read_byte_array_into(a, begin, &mut buf);
            }
            buf
        }
        None => Vec::new(),
    }
}

fn get_long_field(ctx: &dyn NativeContext, obj: ObjectRef, name: &str) -> i64 {
    match ctx.get_field_by_name(obj, name) {
        Value::Long(v) => v,
        Value::Int(v) => v as i64,
        _ => 0,
    }
}

fn set_long_field(ctx: &dyn NativeContext, obj: ObjectRef, name: &str, v: i64) {
    ctx.set_field_by_name(obj, name, Value::Long(v));
}

fn next_handle() -> i64 {
    static COUNTER: AtomicI64 = AtomicI64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ===========================================================================
// snappy-java — org.xerial.snappy.SnappyNative (block format)
// ===========================================================================
//
// SnappyNative is an *instance* implementation of SnappyApi, so every method
// receives the receiver as args[0]; the declared parameters start at args[1].

fn snappy_native_library_version(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // The loader compares this against the expected version; any well-formed
    // "x.y.z" satisfies the check.
    Ok(Some(Value::Object(Some(ctx.create_string("1.1.10")))))
}

fn snappy_max_compressed_length(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let n = arg_int(args, 1).max(0) as usize;
    Ok(Some(Value::Int(snap::raw::max_compress_len(n) as i32)))
}

fn snappy_uncompressed_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // int uncompressedLength(Object input, int offset, int length)
    let off = arg_int(args, 2).max(0) as usize;
    let len = arg_int(args, 3).max(0) as usize;
    let input = read_bytes(ctx, arg_obj(args, 1), off, len);
    match snap::raw::decompress_len(&input) {
        Ok(n) => Ok(Some(Value::Int(n as i32))),
        Err(e) => Err(RuntimeError::IOException {
            message: format!("snappy uncompressedLength: {e}"),
        }
        .into()),
    }
}

fn snappy_raw_compress(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // int rawCompress(Object input, int inOff, int inLen, Object output, int outOff)
    let in_off = arg_int(args, 2).max(0) as usize;
    let in_len = arg_int(args, 3).max(0) as usize;
    let out_off = arg_int(args, 5).max(0) as usize;
    let input = read_bytes(ctx, arg_obj(args, 1), in_off, in_len);

    let mut out = vec![0u8; snap::raw::max_compress_len(input.len())];
    let n = match snap::raw::Encoder::new().compress(&input, &mut out) {
        Ok(n) => n,
        Err(e) => {
            return Err(RuntimeError::IOException {
                message: format!("snappy rawCompress: {e}"),
            }
            .into());
        }
    };
    if let Some(o) = arg_obj(args, 4) {
        ctx.write_byte_array_from(o, out_off, &out[..n]);
    }
    Ok(Some(Value::Int(n as i32)))
}

fn snappy_raw_uncompress(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // int rawUncompress(Object input, int inOff, int inLen, Object output, int outOff)
    let in_off = arg_int(args, 2).max(0) as usize;
    let in_len = arg_int(args, 3).max(0) as usize;
    let out_off = arg_int(args, 5).max(0) as usize;
    let input = read_bytes(ctx, arg_obj(args, 1), in_off, in_len);

    let dlen = match snap::raw::decompress_len(&input) {
        Ok(n) => n,
        Err(e) => {
            return Err(RuntimeError::IOException {
                message: format!("snappy rawUncompress (len): {e}"),
            }
            .into());
        }
    };
    let mut out = vec![0u8; dlen];
    let n = match snap::raw::Decoder::new().decompress(&input, &mut out) {
        Ok(n) => n,
        Err(e) => {
            return Err(RuntimeError::IOException {
                message: format!("snappy rawUncompress: {e}"),
            }
            .into());
        }
    };
    if let Some(o) = arg_obj(args, 4) {
        ctx.write_byte_array_from(o, out_off, &out[..n]);
    }
    Ok(Some(Value::Int(n as i32)))
}

fn snappy_is_valid_compressed_buffer(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // boolean isValidCompressedBuffer(Object input, int offset, int length)
    let off = arg_int(args, 2).max(0) as usize;
    let len = arg_int(args, 3).max(0) as usize;
    let input = read_bytes(ctx, arg_obj(args, 1), off, len);
    let valid = snap::raw::decompress_len(&input).is_ok();
    Ok(Some(Value::Int(i32::from(valid))))
}

fn snappy_array_copy(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // void arrayCopy(Object src, int offset, int byteLength, Object dest, int dOffset)
    // The snappy-java contract: offsets/length are in *bytes* regardless of the
    // backing array's element type. Kafka only ever passes byte[]s here.
    let off = arg_int(args, 2).max(0) as usize;
    let len = arg_int(args, 3).max(0) as usize;
    let doff = arg_int(args, 5).max(0) as usize;
    let bytes = read_bytes(ctx, arg_obj(args, 1), off, len);
    if let Some(dest) = arg_obj(args, 4) {
        ctx.write_byte_array_from(dest, doff, &bytes);
    }
    Ok(None)
}

// ===========================================================================
// zstd-jni — com.github.luben.zstd.* (streaming format)
// ===========================================================================
//
// CCtx/DCtx are only ever touched under the global table mutex (one call at a
// time per handle), so asserting Send for the boxed contexts is sound even
// though a raw libzstd context is not itself thread-safe to share.

struct CCtxBox(CCtx<'static>);
// SAFETY: serialized through `cctx_table()`'s Mutex; never aliased across threads.
unsafe impl Send for CCtxBox {}

struct DCtxBox(DCtx<'static>);
// SAFETY: serialized through `dctx_table()`'s Mutex; never aliased across threads.
unsafe impl Send for DCtxBox {}

fn cctx_table() -> &'static Mutex<HashMap<i64, CCtxBox>> {
    static T: OnceLock<Mutex<HashMap<i64, CCtxBox>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn dctx_table() -> &'static Mutex<HashMap<i64, DCtxBox>> {
    static T: OnceLock<Mutex<HashMap<i64, DCtxBox>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn zstd_err_name(code: usize) -> String {
    unsafe {
        let p = zstd_safe::zstd_sys::ZSTD_getErrorName(code);
        if p.is_null() {
            return "zstd error".to_string();
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// Clamp a libzstd "bytes remaining" hint into the `int` the Java side reads.
fn hint_as_int(remaining: usize) -> i32 {
    remaining.min(i32::MAX as usize) as i32
}

// -- compression context lifecycle (createCStream/freeCStream/resetCStream) --

fn zstd_create_cstream(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    match CCtx::try_create() {
        Some(c) => {
            let h = next_handle();
            cctx_table()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(h, CCtxBox(c));
            Ok(Some(Value::Long(h)))
        }
        // 0 → the Java side throws (errMemoryAllocation), the faithful behaviour.
        None => Ok(Some(Value::Long(0))),
    }
}

fn zstd_free_cstream(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let h = arg_long(args, 0);
    cctx_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&h);
    Ok(Some(Value::Int(0)))
}

fn zstd_reset_cstream(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // instance: args[0]=this, args[1]=ctx handle
    let h = arg_long(args, 1);
    if let Some(b) = cctx_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&h)
    {
        let _ = b.0.reset(ResetDirective::SessionOnly);
    }
    Ok(Some(Value::Int(0)))
}

/// Shared body for `compressStream`/`flushStream`/`endStream`. `has_input`
/// distinguishes the data-bearing compress call from the empty-input drain
/// calls; `end_op` selects continue/flush/end.
fn zstd_compress_common(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    has_input: bool,
    end_op: ZSTD_EndDirective,
) -> MethodCallResult {
    let this = match arg_obj(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    let h = arg_long(args, 1);
    let dst = arg_obj(args, 2);
    let dst_size = arg_int(args, 3).max(0) as usize;

    let dst_pos = get_long_field(ctx, this, "dstPos").max(0) as usize;

    // Compress reads src[srcPos..srcSize]; flush/end feed an empty input.
    let (input_bytes, src_pos) = if has_input {
        let src = arg_obj(args, 4);
        let src_size = arg_int(args, 5).max(0) as usize;
        let src_pos = get_long_field(ctx, this, "srcPos").max(0) as usize;
        (
            read_bytes(ctx, src, src_pos, src_size.saturating_sub(src_pos)),
            src_pos,
        )
    } else {
        (Vec::new(), 0)
    };

    let mut outbuf = vec![0u8; dst_size.saturating_sub(dst_pos)];

    let (consumed, produced, remaining) = {
        let mut tbl = cctx_table().lock().unwrap_or_else(|e| e.into_inner());
        let cbox = match tbl.get_mut(&h) {
            Some(c) => c,
            None => return Ok(Some(Value::Int(0))),
        };
        let mut inb = InBuffer::around(&input_bytes);
        let mut outb = OutBuffer::around(&mut outbuf[..]);
        let remaining = match cbox.0.compress_stream2(&mut outb, &mut inb, end_op) {
            Ok(rem) => rem,
            Err(code) => {
                return Err(RuntimeError::IOException {
                    message: format!("zstd compressStream: {}", zstd_err_name(code)),
                }
                .into());
            }
        };
        (inb.pos, outb.pos(), remaining)
    };

    if produced > 0 {
        if let Some(d) = dst {
            ctx.write_byte_array_from(d, dst_pos, &outbuf[..produced]);
        }
    }
    if has_input {
        set_long_field(ctx, this, "srcPos", (src_pos + consumed) as i64);
    }
    set_long_field(ctx, this, "dstPos", (dst_pos + produced) as i64);
    Ok(Some(Value::Int(hint_as_int(remaining))))
}

fn zstd_compress_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    zstd_compress_common(ctx, args, true, ZSTD_EndDirective::ZSTD_e_continue)
}

fn zstd_flush_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    zstd_compress_common(ctx, args, false, ZSTD_EndDirective::ZSTD_e_flush)
}

fn zstd_end_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    zstd_compress_common(ctx, args, false, ZSTD_EndDirective::ZSTD_e_end)
}

// -- decompression context lifecycle + stream ------------------------------

fn zstd_create_dstream(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    match DCtx::try_create() {
        Some(d) => {
            let h = next_handle();
            dctx_table()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(h, DCtxBox(d));
            Ok(Some(Value::Long(h)))
        }
        None => Ok(Some(Value::Long(0))),
    }
}

fn zstd_free_dstream(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let h = arg_long(args, 0);
    dctx_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&h);
    Ok(Some(Value::Int(0)))
}

fn zstd_init_dstream(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // instance: args[0]=this, args[1]=ctx handle. ZSTD_initDStream == a
    // session-only reset of the context.
    let h = arg_long(args, 1);
    if let Some(b) = dctx_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&h)
    {
        let _ = b.0.reset(ResetDirective::SessionOnly);
    }
    // Real initDStream returns the recommended next input size hint (>0).
    Ok(Some(Value::Int(hint_as_int(unsafe {
        zstd_safe::zstd_sys::ZSTD_DStreamInSize()
    }))))
}

fn zstd_decompress_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // instance: args[0]=this, args[1]=ctx, args[2]=dst, args[3]=dstSize,
    //           args[4]=src, args[5]=srcSize
    let this = match arg_obj(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    let h = arg_long(args, 1);
    let dst = arg_obj(args, 2);
    let dst_size = arg_int(args, 3).max(0) as usize;
    let src = arg_obj(args, 4);
    let src_size = arg_int(args, 5).max(0) as usize;

    let src_pos = get_long_field(ctx, this, "srcPos").max(0) as usize;
    let dst_pos = get_long_field(ctx, this, "dstPos").max(0) as usize;

    let input_bytes = read_bytes(ctx, src, src_pos, src_size.saturating_sub(src_pos));
    let mut outbuf = vec![0u8; dst_size.saturating_sub(dst_pos)];

    let (consumed, produced, remaining) = {
        let mut tbl = dctx_table().lock().unwrap_or_else(|e| e.into_inner());
        let dbox = match tbl.get_mut(&h) {
            Some(d) => d,
            None => return Ok(Some(Value::Int(0))),
        };
        let mut inb = InBuffer::around(&input_bytes);
        let mut outb = OutBuffer::around(&mut outbuf[..]);
        let remaining = match dbox.0.decompress_stream(&mut outb, &mut inb) {
            Ok(rem) => rem,
            Err(code) => {
                return Err(RuntimeError::IOException {
                    message: format!("zstd decompressStream: {}", zstd_err_name(code)),
                }
                .into());
            }
        };
        (inb.pos, outb.pos(), remaining)
    };

    if produced > 0 {
        if let Some(d) = dst {
            ctx.write_byte_array_from(d, dst_pos, &outbuf[..produced]);
        }
    }
    set_long_field(ctx, this, "srcPos", (src_pos + consumed) as i64);
    set_long_field(ctx, this, "dstPos", (dst_pos + produced) as i64);
    Ok(Some(Value::Int(hint_as_int(remaining))))
}

// -- Zstd static helpers ---------------------------------------------------

fn zstd_is_error(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let code = arg_long(args, 0) as usize;
    let is_err = unsafe { zstd_safe::zstd_sys::ZSTD_isError(code) } != 0;
    Ok(Some(Value::Int(i32::from(is_err))))
}

fn zstd_get_error_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let code = arg_long(args, 0) as usize;
    Ok(Some(Value::Object(Some(
        ctx.create_string(&zstd_err_name(code)),
    ))))
}

fn zstd_get_error_code(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let code = arg_long(args, 0) as usize;
    let ec = unsafe { zstd_safe::zstd_sys::ZSTD_getErrorCode(code) };
    Ok(Some(Value::Long(ec as i64)))
}

fn zstd_compress_bound(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let n = arg_long(args, 0).max(0) as usize;
    let bound = unsafe { zstd_safe::zstd_sys::ZSTD_compressBound(n) };
    Ok(Some(Value::Long(bound as i64)))
}

/// `Zstd.setCompressionLevel(long stream, int level)` — apply to the context so
/// the produced frame honours the requested level; returns 0 (success).
fn zstd_set_compression_level(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let h = arg_long(args, 0);
    let level = arg_int(args, 1);
    if let Some(b) = cctx_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&h)
    {
        let _ = b.0.set_parameter(CParameter::CompressionLevel(level));
    }
    Ok(Some(Value::Int(0)))
}

/// `(size_t)(-ZSTD_error_parameter_unsupported)` — the canonical libzstd code
/// for "this build cannot honour that parameter". Matches the value
/// `register_err_const` publishes as `Zstd.errParameterUnsupported()`, which is
/// what zstd-jni compares an error return against.
const ZSTD_ERR_PARAMETER_UNSUPPORTED: i32 = -40;

/// `(size_t)(-ZSTD_error_parameter_outOfBound)`; see
/// `ZSTD_ERR_PARAMETER_UNSUPPORTED`.
const ZSTD_ERR_PARAMETER_OUT_OF_BOUND: i32 = -42;

/// Apply one compression parameter to the live `CCtx` behind a zstd-jni stream
/// handle.
///
/// Returns 0 on success and libzstd's own error code otherwise — exactly what
/// the caller feeds to `Zstd.isError`/`Zstd.getErrorName`. An unknown handle is
/// reported as success: zstd-jni's setters are also called on a stream that has
/// not created its context yet, and that is not an error there either.
fn zstd_apply_cparam(handle: i64, param: CParameter) -> MethodCallResult {
    let mut table = cctx_table().lock().unwrap_or_else(|e| e.into_inner());
    let Some(b) = table.get_mut(&handle) else {
        return Ok(Some(Value::Int(0)));
    };
    match b.0.set_parameter(param) {
        Ok(_) => Ok(Some(Value::Int(0))),
        Err(code) => Ok(Some(Value::Int(code as i32))),
    }
}

/// `zstd_apply_cparam`'s decompression twin (`DCtx` / `DParameter`).
fn zstd_apply_dparam(handle: i64, param: DParameter) -> MethodCallResult {
    let mut table = dctx_table().lock().unwrap_or_else(|e| e.into_inner());
    let Some(b) = table.get_mut(&handle) else {
        return Ok(Some(Value::Int(0)));
    };
    match b.0.set_parameter(param) {
        Ok(_) => Ok(Some(Value::Int(0))),
        Err(code) => Ok(Some(Value::Int(code as i32))),
    }
}

/// Define one `Zstd.setXxx(long, int|boolean)I` native that maps its second
/// argument onto a libzstd parameter and applies it. `$build` receives the raw
/// `int` (a `boolean` arrives as 0/1).
///
/// A macro rather than a loop because the registry takes a bare `fn` pointer,
/// so each name needs its own non-capturing function.
macro_rules! zstd_param_setter {
    ($fn_name:ident, $apply:ident, $param_ty:ty, $build:expr) => {
        fn $fn_name(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
            let build: fn(i32) -> $param_ty = $build;
            $apply(arg_long(args, 0), build(arg_int(args, 1)))
        }
    };
}

zstd_param_setter!(zstd_set_checksums, zstd_apply_cparam, CParameter, |v| {
    CParameter::ChecksumFlag(v != 0)
});
zstd_param_setter!(zstd_set_workers, zstd_apply_cparam, CParameter, |v| {
    CParameter::NbWorkers(v.max(0) as u32)
});
zstd_param_setter!(zstd_set_overlap_log, zstd_apply_cparam, CParameter, |v| {
    CParameter::OverlapSizeLog(v.max(0) as u32)
});
zstd_param_setter!(zstd_set_job_size, zstd_apply_cparam, CParameter, |v| {
    CParameter::JobSize(v.max(0) as u32)
});
zstd_param_setter!(zstd_set_target_length, zstd_apply_cparam, CParameter, |v| {
    CParameter::TargetLength(v.max(0) as u32)
});
zstd_param_setter!(zstd_set_min_match, zstd_apply_cparam, CParameter, |v| {
    CParameter::MinMatch(v.max(0) as u32)
});
zstd_param_setter!(zstd_set_search_log, zstd_apply_cparam, CParameter, |v| {
    CParameter::SearchLog(v.max(0) as u32)
});
zstd_param_setter!(zstd_set_chain_log, zstd_apply_cparam, CParameter, |v| {
    CParameter::ChainLog(v.max(0) as u32)
});
zstd_param_setter!(zstd_set_hash_log, zstd_apply_cparam, CParameter, |v| {
    CParameter::HashLog(v.max(0) as u32)
});
zstd_param_setter!(zstd_set_window_log, zstd_apply_cparam, CParameter, |v| {
    CParameter::WindowLog(v.max(0) as u32)
});
zstd_param_setter!(zstd_set_ldm, zstd_apply_cparam, CParameter, |v| {
    CParameter::EnableLongDistanceMatching(v != 0)
});
zstd_param_setter!(
    zstd_set_decompression_long_max,
    zstd_apply_dparam,
    DParameter,
    |v| { DParameter::WindowLogMax(v.max(0) as u32) }
);

/// `Zstd.setCompressionStrategy(long, int)`.
///
/// zstd-jni passes libzstd's `ZSTD_strategy` enum value (1..=9); `zstd_safe`
/// models it as a Rust enum, so map the number back. Anything outside the
/// documented range is refused rather than silently coerced.
fn zstd_set_strategy(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let strategy = match arg_int(args, 1) {
        1 => zstd_safe::Strategy::ZSTD_fast,
        2 => zstd_safe::Strategy::ZSTD_dfast,
        3 => zstd_safe::Strategy::ZSTD_greedy,
        4 => zstd_safe::Strategy::ZSTD_lazy,
        5 => zstd_safe::Strategy::ZSTD_lazy2,
        6 => zstd_safe::Strategy::ZSTD_btlazy2,
        7 => zstd_safe::Strategy::ZSTD_btopt,
        8 => zstd_safe::Strategy::ZSTD_btultra,
        9 => zstd_safe::Strategy::ZSTD_btultra2,
        _ => return Ok(Some(Value::Int(ZSTD_ERR_PARAMETER_OUT_OF_BOUND))),
    };
    zstd_apply_cparam(arg_long(args, 0), CParameter::Strategy(strategy))
}

/// `Zstd.setCompressionLong(long, int windowLog)` — zstd-jni's shorthand for
/// "enable long-distance matching over this window", and "disable it" when the
/// window is below libzstd's `ZSTD_WINDOWLOG_MIN` (10).
fn zstd_set_compression_long(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let handle = arg_long(args, 0);
    let window_log = arg_int(args, 1);
    if window_log < 10 {
        return zstd_apply_cparam(handle, CParameter::EnableLongDistanceMatching(false));
    }
    match zstd_apply_cparam(handle, CParameter::EnableLongDistanceMatching(true))? {
        Some(Value::Int(0)) => {}
        other => return Ok(other),
    }
    zstd_apply_cparam(handle, CParameter::WindowLog(window_log as u32))
}

/// Parameters that exist only behind libzstd's *experimental* API, which the
/// bundled `zstd-safe` is not built with. Reporting "unsupported" is the honest
/// answer; returning 0 told the caller a knob had been applied when nothing
/// had changed. Same treatment as `zstd_set_magicless` below.
fn zstd_set_experimental_param(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(ZSTD_ERR_PARAMETER_UNSUPPORTED)))
}

/// `Zstd.setCompressionMagicless` / `setDecompressionMagicless`.
fn zstd_set_magicless(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Requesting magicless == false leaves us in the state we are already in.
    if arg_int(args, 1) == 0 {
        return Ok(Some(Value::Int(0)));
    }
    Ok(Some(Value::Int(ZSTD_ERR_PARAMETER_UNSUPPORTED)))
}

/// `Zstd.loadDictCompress(long, byte[], int)` / `loadDictDecompress(...)`.
fn zstd_load_dict_bytes(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let has_dict = matches!(args.get(1), Some(Value::Object(Some(_)))) && arg_int(args, 2) > 0;
    if has_dict {
        return Ok(Some(Value::Int(ZSTD_ERR_PARAMETER_UNSUPPORTED)));
    }
    Ok(Some(Value::Int(0)))
}

/// `Zstd.loadFastDictCompress(long, ZstdDictCompress)` / the decompress twin.
fn zstd_load_dict_object(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if matches!(args.get(1), Some(Value::Object(Some(_)))) {
        return Ok(Some(Value::Int(ZSTD_ERR_PARAMETER_UNSUPPORTED)));
    }
    Ok(Some(Value::Int(0)))
}

// ===========================================================================
// lz4-java — net.jpountz.lz4.LZ4JNI (canonical LZ4 block format)
// ===========================================================================
//
// All LZ4JNI methods are *static*, so args start at index 0 (no receiver). Each
// (byte[] arr, ByteBuffer buf) pair is "heap array XOR direct buffer" — lz4-java
// takes the array branch when both sides `hasArray()` and the ByteBuffer branch
// otherwise, and passes `null` for the half it is not using.
//
// This shim was originally written for Kafka's codec, which only ever takes the
// heap-array branch, and read `args[0]`/`args[4]` unconditionally — so on the
// ByteBuffer branch it read an EMPTY input and wrote its output NOWHERE. That is
// not a crash but a wrong answer: `compress` of nothing returns 1, `decompress`
// of nothing fails with "expected another byte, found none", and a decompress
// that writes nothing leaves the destination zeroed, which surfaces one frame
// later as netty's "stream corrupted: mismatching checksum". Netty's
// `Lz4FrameDecoder` takes the ByteBuffer branch for every pooled or direct
// `ByteBuf` — `CompressionUtil.safeNioBuffer` / `internalNioBuffer` — so it hit
// all three shapes. Both halves are backed now; see `lz4_read` / `lz4_write`.

/// One side of an lz4-java `(byte[] arr, ByteBuffer buf)` pair, resolved to
/// something this shim can actually read and write.
enum Lz4Side {
    /// A `byte[]`, either passed directly or reached through a heap
    /// `ByteBuffer`'s backing array. `base` is the index in that array that
    /// the Java-side offset 0 refers to.
    Array(ObjectRef, usize),
    /// A direct `ByteBuffer`, as its native base address. On this VM that is an
    /// arena handle rather than an OS pointer, which is why the transfer goes
    /// through `copy_from_native_memory` / `copy_to_native_memory` — the same
    /// bridge `Inflater`'s direct-buffer natives use — instead of a raw deref.
    Direct(i64),
}

/// Resolve whichever half of the pair is non-null.
///
/// Returns `None` only when neither half is usable. Callers turn that into an
/// error rather than an empty buffer: answering "0 bytes" for an unresolvable
/// argument is precisely the silent-wrong-answer mode this replaced.
fn lz4_side(
    ctx: &dyn NativeContext,
    arr: Option<ObjectRef>,
    buf: Option<ObjectRef>,
) -> Option<Lz4Side> {
    if let Some(a) = arr {
        return Some(Lz4Side::Array(a, 0));
    }
    let b = buf?;
    // A direct buffer carries its base in `Buffer.address`. A slice/duplicate
    // has already folded its own offset into that field, so the Java-side
    // offset is added on top of it unmodified — which is exactly what
    // lz4-java's C does with `GetDirectBufferAddress`.
    match ctx.get_field_by_name(b, "address") {
        Value::Long(v) if v != 0 => return Some(Lz4Side::Direct(v)),
        Value::Int(v) if v != 0 => return Some(Lz4Side::Direct(v as i64)),
        _ => {}
    }
    // A heap `ByteBuffer` reaches here only when lz4-java could not call
    // `hasArray()` on it (a read-only view). `ByteBuffer.hb` is still the
    // backing array and `Buffer.offset` its base index.
    if let Value::Object(Some(hb)) = ctx.get_field_by_name(b, "hb") {
        let base = match ctx.get_field_by_name(b, "offset") {
            Value::Int(v) => v.max(0) as usize,
            Value::Long(v) => v.max(0) as usize,
            _ => 0,
        };
        return Some(Lz4Side::Array(hb, base));
    }
    None
}

/// Read `len` bytes starting at Java-side offset `off` from either half.
fn lz4_read(
    ctx: &mut dyn NativeContext,
    arr: Option<ObjectRef>,
    buf: Option<ObjectRef>,
    off: usize,
    len: usize,
) -> Option<Vec<u8>> {
    // Resolved into a local first: the reborrow `lz4_side` takes is shared, and
    // the `Direct` arm below needs `ctx` mutably.
    let side = lz4_side(&*ctx, arr, buf)?;
    match side {
        Lz4Side::Array(a, base) => Some(read_bytes(&*ctx, Some(a), base + off, len)),
        Lz4Side::Direct(addr) => {
            let mut out = vec![0u8; len];
            if out.is_empty() {
                return Some(out);
            }
            ctx.copy_from_native_memory(addr.wrapping_add(off as i64), &mut out)
                .then_some(out)
        }
    }
}

/// Write `data` at Java-side offset `off` into either half.
fn lz4_write(
    ctx: &mut dyn NativeContext,
    arr: Option<ObjectRef>,
    buf: Option<ObjectRef>,
    off: usize,
    data: &[u8],
) -> bool {
    let side = lz4_side(&*ctx, arr, buf);
    match side {
        Some(Lz4Side::Array(a, base)) => {
            ctx.write_byte_array_from(a, base + off, data);
            true
        }
        Some(Lz4Side::Direct(addr)) => {
            data.is_empty() || ctx.copy_to_native_memory(addr.wrapping_add(off as i64), data)
        }
        None => false,
    }
}

fn lz4_bad_buffer(which: &str) -> MethodCallFailed {
    RuntimeError::IOException {
        message: format!(
            "lz4: {which} buffer is neither a byte[] nor a readable direct ByteBuffer"
        ),
    }
    .into()
}

/// `LZ4_compressBound(int)`.
///
/// liblz4's macro exactly: `n + n/255 + 16`. It used to answer
/// `lz4_flex::block::get_maximum_output_size`, which is `20 + n * 1.1` — a
/// different, larger number (4525 vs 4128 for a 4 KiB block). Callers size a
/// destination buffer from this and lz4-java exposes it as
/// `LZ4Compressor.maxCompressedLength`, so the value is observable; the larger
/// bound was safe but diverged from HotSpot for every caller that prints or
/// asserts on it. The internal scratch buffer in `lz4_compress_into_dst` still
/// uses lz4_flex's own (more conservative) bound, so nothing here depends on
/// this number being the one lz4_flex wants.
fn lz4_compress_bound(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let n = arg_int(args, 0).max(0) as i64;
    Ok(Some(Value::Int((n + n / 255 + 16) as i32)))
}

/// Decode one LZ4 block, reporting BOTH how many input bytes were consumed and
/// how many output bytes were produced.
///
/// `LZ4_decompress_fast` is not given the compressed length — it is told the
/// *uncompressed* length, decodes until the output is full, and returns the
/// number of source bytes it read. `lz4_flex` has no such entry point: its
/// `decompress_into` wants the input to be exactly one block, so handing it the
/// caller's whole source array (whose tail is unwritten zeroes) made it fail,
/// and `LZ4_decompress_fast` answered -1 for every call.
///
/// The block format is small enough to walk directly, so this walks it: token,
/// literal run, 16-bit little-endian match offset, match run, repeat, stopping
/// the moment the output is full. Every length is bounds-checked against both
/// buffers before use, so malformed input returns `None` rather than reading or
/// writing out of range.
fn lz4_block_decode(input: &[u8], output: &mut [u8]) -> Option<(usize, usize)> {
    let mut ip = 0usize;
    let mut op = 0usize;
    loop {
        // Input exhausted exactly at a sequence boundary is the clean end of a
        // block. `LZ4_decompress_safe` is given the exact compressed length and
        // an output bound that may be LARGER than the block produces, so this —
        // not "output full" — is how that call normally terminates.
        if ip == input.len() {
            return Some((ip, op));
        }
        let token = *input.get(ip)?;
        ip += 1;

        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            loop {
                let b = *input.get(ip)?;
                ip += 1;
                lit += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        let lit_end = ip.checked_add(lit)?;
        let op_end = op.checked_add(lit)?;
        if lit_end > input.len() || op_end > output.len() {
            return None;
        }
        output[op..op_end].copy_from_slice(&input[ip..lit_end]);
        ip = lit_end;
        op = op_end;
        // A block's final sequence is literals only, so BOTH of these end it,
        // and which one fires depends on the call:
        //   * output full — `LZ4_decompress_fast`, whose caller knows the
        //     uncompressed size but not where the block ends, so the input it
        //     hands over runs on past the block;
        //   * input exhausted — `LZ4_decompress_safe`, whose caller knows the
        //     compressed size exactly but bounds the output generously.
        if op == output.len() || ip == input.len() {
            return Some((ip, op));
        }

        // Anything else at the end of the input is a partial match offset.
        if ip + 2 > input.len() {
            return None;
        }
        let offset = u16::from_le_bytes([input[ip], input[ip + 1]]) as usize;
        ip += 2;
        // Offset 0 is invalid, and an offset past what has been produced would
        // copy from uninitialised output.
        if offset == 0 || offset > op {
            return None;
        }

        let mut mlen = (token & 0x0F) as usize;
        if mlen == 15 {
            loop {
                let b = *input.get(ip)?;
                ip += 1;
                mlen += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        mlen += 4; // MINMATCH
        if op.checked_add(mlen)? > output.len() {
            return None;
        }
        // Byte-at-a-time on purpose: LZ4 matches may overlap their own output
        // (offset < mlen is how runs are encoded), so this cannot be a
        // `copy_within`.
        let mut src = op - offset;
        for _ in 0..mlen {
            output[op] = output[src];
            op += 1;
            src += 1;
        }
        if op == output.len() {
            return Some((ip, op));
        }
    }
}

/// Shared body for `LZ4_compress_limitedOutput` / `LZ4_compressHC`. lz4_flex
/// emits the same raw LZ4 block regardless of "HC", so both route here; the
/// HC level only affects ratio, not format/correctness.
#[allow(clippy::too_many_arguments)]
fn lz4_compress_into_dst(
    ctx: &mut dyn NativeContext,
    src_arr: Option<ObjectRef>,
    src_buf: Option<ObjectRef>,
    src_off: usize,
    src_len: usize,
    dst_arr: Option<ObjectRef>,
    dst_buf: Option<ObjectRef>,
    dst_off: usize,
    max_dst_len: usize,
) -> MethodCallResult {
    let Some(input) = lz4_read(ctx, src_arr, src_buf, src_off, src_len) else {
        return Err(lz4_bad_buffer("compress source"));
    };
    let mut tmp = vec![0u8; lz4_flex::block::get_maximum_output_size(input.len())];
    let clen = match lz4_flex::block::compress_into(&input, &mut tmp) {
        Ok(n) => n,
        Err(e) => {
            return Err(RuntimeError::IOException {
                message: format!("lz4 compress: {e}"),
            }
            .into());
        }
    };
    // liblz4 LZ4_compress_limitedOutput contract: 0 when the result does not fit.
    if clen > max_dst_len {
        return Ok(Some(Value::Int(0)));
    }
    if !lz4_write(ctx, dst_arr, dst_buf, dst_off, &tmp[..clen]) {
        return Err(lz4_bad_buffer("compress destination"));
    }
    Ok(Some(Value::Int(clen as i32)))
}

fn lz4_compress_limited_output(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // (byte[] src, ByteBuffer, int srcOff, int srcLen, byte[] dst, ByteBuffer, int dstOff, int maxDstLen)
    lz4_compress_into_dst(
        ctx,
        arg_obj(args, 0),
        arg_obj(args, 1),
        arg_int(args, 2).max(0) as usize,
        arg_int(args, 3).max(0) as usize,
        arg_obj(args, 4),
        arg_obj(args, 5),
        arg_int(args, 6).max(0) as usize,
        arg_int(args, 7).max(0) as usize,
    )
}

fn lz4_compress_hc(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // same as limitedOutput plus a trailing compression-level int (args[8], ignored)
    lz4_compress_into_dst(
        ctx,
        arg_obj(args, 0),
        arg_obj(args, 1),
        arg_int(args, 2).max(0) as usize,
        arg_int(args, 3).max(0) as usize,
        arg_obj(args, 4),
        arg_obj(args, 5),
        arg_int(args, 6).max(0) as usize,
        arg_int(args, 7).max(0) as usize,
    )
}

fn lz4_decompress_safe(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // (byte[] src, ByteBuffer, int srcOff, int srcLen, byte[] dst, ByteBuffer, int dstOff, int maxDstLen)
    let src_off = arg_int(args, 2).max(0) as usize;
    let src_len = arg_int(args, 3).max(0) as usize;
    let dst_off = arg_int(args, 6).max(0) as usize;
    let max_dst_len = arg_int(args, 7).max(0) as usize;
    let Some(input) = lz4_read(ctx, arg_obj(args, 0), arg_obj(args, 1), src_off, src_len) else {
        return Err(lz4_bad_buffer("decompress source"));
    };

    // liblz4's contract for a block it cannot decode is a NEGATIVE return, not
    // an exception — lz4-java turns that into `LZ4Exception`, which is what
    // callers catch (netty's `Lz4FrameDecoder` has an explicit
    // `catch (LZ4Exception)` arm that converts it to `DecompressionException`).
    // Throwing `IOException` from here bypassed that arm and surfaced as a raw
    // `DecoderException` instead.
    let mut out = vec![0u8; max_dst_len];
    let Some((_consumed, produced)) = lz4_block_decode(&input, &mut out) else {
        return Ok(Some(Value::Int(-1)));
    };
    if !lz4_write(
        ctx,
        arg_obj(args, 4),
        arg_obj(args, 5),
        dst_off,
        &out[..produced],
    ) {
        return Err(lz4_bad_buffer("decompress destination"));
    }
    Ok(Some(Value::Int(produced as i32)))
}

fn lz4_decompress_fast(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // (byte[] src, ByteBuffer, int srcOff, byte[] dst, ByteBuffer, int dstOff, int destLen)
    // The "fast" decompressor is told the output size up front and returns the
    // number of *source* bytes consumed, so the source length is unknown here
    // by construction: everything from `srcOff` to the end of the source is
    // offered and `lz4_block_decode` reports where the block actually ended.
    let src_off = arg_int(args, 2).max(0) as usize;
    let dst_off = arg_int(args, 5).max(0) as usize;
    let dest_len = arg_int(args, 6).max(0) as usize;

    let src_arr = arg_obj(args, 0);
    let src_buf = arg_obj(args, 1);
    let side = lz4_side(&*ctx, src_arr, src_buf);
    let src_avail = match side {
        Some(Lz4Side::Array(a, base)) => ctx.array_length(a).saturating_sub(base + src_off),
        // A direct buffer has no reachable "rest of the array"; its capacity is
        // the bound. `capacity` rather than `limit` because the caller's own
        // limit may have been narrowed to the uncompressed view.
        Some(Lz4Side::Direct(_)) => match ctx.get_field_by_name(src_buf.unwrap(), "capacity") {
            Value::Int(v) => (v.max(0) as usize).saturating_sub(src_off),
            Value::Long(v) => (v.max(0) as usize).saturating_sub(src_off),
            _ => 0,
        },
        None => return Err(lz4_bad_buffer("decompress source")),
    };
    let Some(input) = lz4_read(ctx, src_arr, src_buf, src_off, src_avail) else {
        return Err(lz4_bad_buffer("decompress source"));
    };

    let mut out = vec![0u8; dest_len];
    let Some((consumed, produced)) = lz4_block_decode(&input, &mut out) else {
        return Ok(Some(Value::Int(-1)));
    };
    if produced != dest_len {
        return Ok(Some(Value::Int(-1)));
    }
    if !lz4_write(ctx, arg_obj(args, 3), arg_obj(args, 4), dst_off, &out) {
        return Err(lz4_bad_buffer("decompress destination"));
    }
    Ok(Some(Value::Int(consumed as i32)))
}

// ===========================================================================
// lz4-java — net.jpountz.xxhash.XXHashJNI (xxHash32/64 for LZ4 frame checksums)
// ===========================================================================
//
// All static. `twox-hash` is reference-xxHash-compatible, so the block
// checksums it produces match what a real liblz4/xxhash decoder verifies.

fn xxh32_table() -> &'static Mutex<HashMap<i64, twox_hash::XxHash32>> {
    static T: OnceLock<Mutex<HashMap<i64, twox_hash::XxHash32>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn xxh64_table() -> &'static Mutex<HashMap<i64, twox_hash::XxHash64>> {
    static T: OnceLock<Mutex<HashMap<i64, twox_hash::XxHash64>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn xxh32_oneshot(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::hash::Hasher;
    // XXH32(byte[] buf, int off, int len, int seed)
    let off = arg_int(args, 1).max(0) as usize;
    let len = arg_int(args, 2).max(0) as usize;
    let seed = arg_int(args, 3) as u32;
    let data = read_bytes(ctx, arg_obj(args, 0), off, len);
    let mut h = twox_hash::XxHash32::with_seed(seed);
    h.write(&data);
    Ok(Some(Value::Int(h.finish() as u32 as i32)))
}

fn xxh32_init(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let seed = arg_int(args, 0) as u32;
    let handle = next_handle();
    xxh32_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(handle, twox_hash::XxHash32::with_seed(seed));
    Ok(Some(Value::Long(handle)))
}

fn xxh32_update(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::hash::Hasher;
    // XXH32_update(long state, byte[] buf, int off, int len)
    let h = arg_long(args, 0);
    let off = arg_int(args, 2).max(0) as usize;
    let len = arg_int(args, 3).max(0) as usize;
    let data = read_bytes(ctx, arg_obj(args, 1), off, len);
    if let Some(st) = xxh32_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&h)
    {
        st.write(&data);
    }
    Ok(None)
}

fn xxh32_digest(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::hash::Hasher;
    let h = arg_long(args, 0);
    let v = xxh32_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&h)
        .map(|s| s.finish() as u32 as i32)
        .unwrap_or(0);
    Ok(Some(Value::Int(v)))
}

fn xxh32_free(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let h = arg_long(args, 0);
    xxh32_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&h);
    Ok(None)
}

fn xxh64_oneshot(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::hash::Hasher;
    // XXH64(byte[] buf, int off, int len, long seed)
    let off = arg_int(args, 1).max(0) as usize;
    let len = arg_int(args, 2).max(0) as usize;
    let seed = arg_long(args, 3) as u64;
    let data = read_bytes(ctx, arg_obj(args, 0), off, len);
    let mut h = twox_hash::XxHash64::with_seed(seed);
    h.write(&data);
    Ok(Some(Value::Long(h.finish() as i64)))
}

fn xxh64_init(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let seed = arg_long(args, 0) as u64;
    let handle = next_handle();
    xxh64_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(handle, twox_hash::XxHash64::with_seed(seed));
    Ok(Some(Value::Long(handle)))
}

fn xxh64_update(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::hash::Hasher;
    let h = arg_long(args, 0);
    let off = arg_int(args, 2).max(0) as usize;
    let len = arg_int(args, 3).max(0) as usize;
    let data = read_bytes(ctx, arg_obj(args, 1), off, len);
    if let Some(st) = xxh64_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&h)
    {
        st.write(&data);
    }
    Ok(None)
}

fn xxh64_digest(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::hash::Hasher;
    let h = arg_long(args, 0);
    let v = xxh64_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&h)
        .map(|s| s.finish() as i64)
        .unwrap_or(0);
    Ok(Some(Value::Long(v)))
}

fn xxh64_free(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let h = arg_long(args, 0);
    xxh64_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&h);
    Ok(None)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register_compression_natives(r: &mut NativeMethodRegistry) {
    // --- snappy-java: org.xerial.snappy.SnappyNative (instance methods) ---
    let sn = "org/xerial/snappy/SnappyNative";
    r.register(
        sn,
        "nativeLibraryVersion",
        "()Ljava/lang/String;",
        snappy_native_library_version,
    );
    r.register(
        sn,
        "maxCompressedLength",
        "(I)I",
        snappy_max_compressed_length,
    );
    r.register(
        sn,
        "uncompressedLength",
        "(Ljava/lang/Object;II)I",
        snappy_uncompressed_length,
    );
    r.register(
        sn,
        "rawCompress",
        "(Ljava/lang/Object;IILjava/lang/Object;I)I",
        snappy_raw_compress,
    );
    r.register(
        sn,
        "rawUncompress",
        "(Ljava/lang/Object;IILjava/lang/Object;I)I",
        snappy_raw_uncompress,
    );
    r.register(
        sn,
        "isValidCompressedBuffer",
        "(Ljava/lang/Object;II)Z",
        snappy_is_valid_compressed_buffer,
    );
    r.register(
        sn,
        "arrayCopy",
        "(Ljava/lang/Object;IILjava/lang/Object;I)V",
        snappy_array_copy,
    );

    // --- lz4-java: net.jpountz.lz4.LZ4JNI (static methods, block format) ---
    let lz = "net/jpountz/lz4/LZ4JNI";
    // KEEP: `LZ4JNI.init()` exists solely to force the JNI library load
    // (`System.loadLibrary`) before the first codec call. Every LZ4 entry
    // point is already bound to a Rust implementation below, so there is no
    // library to load and an empty body is the correct implementation.
    r.register(lz, "init", "()V", |_c, _a| Ok(None));
    r.register(lz, "LZ4_compressBound", "(I)I", lz4_compress_bound);
    r.register(
        lz,
        "LZ4_compress_limitedOutput",
        "([BLjava/nio/ByteBuffer;II[BLjava/nio/ByteBuffer;II)I",
        lz4_compress_limited_output,
    );
    r.register(
        lz,
        "LZ4_compressHC",
        "([BLjava/nio/ByteBuffer;II[BLjava/nio/ByteBuffer;III)I",
        lz4_compress_hc,
    );
    r.register(
        lz,
        "LZ4_decompress_safe",
        "([BLjava/nio/ByteBuffer;II[BLjava/nio/ByteBuffer;II)I",
        lz4_decompress_safe,
    );
    r.register(
        lz,
        "LZ4_decompress_fast",
        "([BLjava/nio/ByteBuffer;I[BLjava/nio/ByteBuffer;II)I",
        lz4_decompress_fast,
    );

    // --- lz4-java: net.jpountz.xxhash.XXHashJNI (static, LZ4 frame checksums) ---
    let xx = "net/jpountz/xxhash/XXHashJNI";
    // KEEP: same as `LZ4JNI.init` — a JNI library-load trigger with nothing
    // to load here.
    r.register(xx, "init", "()V", |_c, _a| Ok(None));
    r.register(xx, "XXH32", "([BIII)I", xxh32_oneshot);
    r.register(xx, "XXH32_init", "(I)J", xxh32_init);
    r.register(xx, "XXH32_update", "(J[BII)V", xxh32_update);
    r.register(xx, "XXH32_digest", "(J)I", xxh32_digest);
    r.register(xx, "XXH32_free", "(J)V", xxh32_free);
    r.register(xx, "XXH64", "([BIIJ)J", xxh64_oneshot);
    r.register(xx, "XXH64_init", "(J)J", xxh64_init);
    r.register(xx, "XXH64_update", "(J[BII)V", xxh64_update);
    r.register(xx, "XXH64_digest", "(J)J", xxh64_digest);
    r.register(xx, "XXH64_free", "(J)V", xxh64_free);

    // --- zstd-jni: streaming compress (ZstdOutputStreamNoFinalizer) ---
    let zo = "com/github/luben/zstd/ZstdOutputStreamNoFinalizer";
    r.register(zo, "recommendedCOutSize", "()J", |_c, _a| {
        Ok(Some(Value::Long(
            unsafe { zstd_safe::zstd_sys::ZSTD_CStreamOutSize() } as i64,
        )))
    });
    r.register(zo, "createCStream", "()J", zstd_create_cstream);
    r.register(zo, "freeCStream", "(J)I", zstd_free_cstream);
    r.register(zo, "resetCStream", "(J)I", zstd_reset_cstream);
    r.register(zo, "compressStream", "(J[BI[BI)I", zstd_compress_stream);
    r.register(zo, "flushStream", "(J[BI)I", zstd_flush_stream);
    r.register(zo, "endStream", "(J[BI)I", zstd_end_stream);

    // --- zstd-jni: streaming decompress (ZstdInputStreamNoFinalizer) ---
    let zi = "com/github/luben/zstd/ZstdInputStreamNoFinalizer";
    r.register(zi, "recommendedDInSize", "()J", |_c, _a| {
        Ok(Some(Value::Long(
            unsafe { zstd_safe::zstd_sys::ZSTD_DStreamInSize() } as i64,
        )))
    });
    r.register(zi, "recommendedDOutSize", "()J", |_c, _a| {
        Ok(Some(Value::Long(
            unsafe { zstd_safe::zstd_sys::ZSTD_DStreamOutSize() } as i64,
        )))
    });
    r.register(zi, "createDStream", "()J", zstd_create_dstream);
    r.register(zi, "freeDStream", "(J)I", zstd_free_dstream);
    r.register(zi, "initDStream", "(J)I", zstd_init_dstream);
    r.register(zi, "decompressStream", "(J[BI[BI)I", zstd_decompress_stream);

    // --- zstd-jni: com.github.luben.zstd.Zstd static helpers ---
    let z = "com/github/luben/zstd/Zstd";
    r.register(z, "isError", "(J)Z", zstd_is_error);
    r.register(
        z,
        "getErrorName",
        "(J)Ljava/lang/String;",
        zstd_get_error_name,
    );
    r.register(z, "getErrorCode", "(J)J", zstd_get_error_code);
    r.register(z, "compressBound", "(J)J", zstd_compress_bound);
    // Ask the linked libzstd, exactly as the min/max accessors below do. (The
    // previous hard-coded `3` justified itself by claiming
    // `ZSTD_defaultCLevel` sits behind zstd-sys's `experimental` bindings
    // gate. It does not: it is declared in `bindings_zstd.rs` three lines
    // after `ZSTD_minCLevel`/`ZSTD_maxCLevel`.)
    r.register(z, "defaultCompressionLevel", "()I", |_c, _a| {
        Ok(Some(Value::Int(unsafe {
            zstd_safe::zstd_sys::ZSTD_defaultCLevel()
        })))
    });
    r.register(z, "minCompressionLevel", "()I", |_c, _a| {
        Ok(Some(Value::Int(unsafe {
            zstd_safe::zstd_sys::ZSTD_minCLevel()
        })))
    });
    r.register(z, "maxCompressionLevel", "()I", |_c, _a| {
        Ok(Some(Value::Int(unsafe {
            zstd_safe::zstd_sys::ZSTD_maxCLevel()
        })))
    });

    // Parameter setters: apply each knob to the real context and REFUSE the
    // ones libzstd cannot honour in this build. Each returns an int the caller
    // feeds to `Zstd.isError`.
    r.register(
        z,
        "setCompressionLevel",
        "(JI)I",
        zstd_set_compression_level,
    );
    // Each of these now reaches libzstd's real `ZSTD_CCtx_setParameter` /
    // `ZSTD_DCtx_setParameter` on the context behind the handle and returns
    // libzstd's own status code. They used to return a blanket 0 while
    // discarding the request: `setCompressionChecksums(true)` produced a frame
    // with no checksum, and every window/strategy knob was silently inert
    // while reporting success.
    //
    // The `(JI)I` / `(JZ)I` pair is registered for each name because a
    // `boolean` second argument is `Z` and an `int` is `I` at the descriptor
    // level; both shapes route to the same handler, which reads 0/1 either way.
    let param_setters: [(&str, cratonvm_native_api::NativeCallback); 18] = [
        ("setCompressionChecksums", zstd_set_checksums),
        ("setCompressionLong", zstd_set_compression_long),
        ("setCompressionWorkers", zstd_set_workers),
        ("setCompressionOverlapLog", zstd_set_overlap_log),
        ("setCompressionJobSize", zstd_set_job_size),
        ("setCompressionTargetLength", zstd_set_target_length),
        ("setCompressionMinMatch", zstd_set_min_match),
        ("setCompressionSearchLog", zstd_set_search_log),
        ("setCompressionChainLog", zstd_set_chain_log),
        ("setCompressionHashLog", zstd_set_hash_log),
        ("setCompressionWindowLog", zstd_set_window_log),
        ("setCompressionStrategy", zstd_set_strategy),
        ("setEnableLongDistanceMatching", zstd_set_ldm),
        ("setDecompressionLongMax", zstd_set_decompression_long_max),
        // libzstd exposes these four only through its experimental API, which
        // the bundled zstd-safe is not built with, so they report
        // ZSTD_error_parameter_unsupported instead of a fake success.
        ("setRefMultipleDDicts", zstd_set_experimental_param),
        ("setValidateSequences", zstd_set_experimental_param),
        ("setSequenceProducerFallback", zstd_set_experimental_param),
        ("setSearchForExternalRepcodes", zstd_set_experimental_param),
    ];
    for (name, cb) in param_setters {
        r.register(z, name, "(JI)I", cb);
        r.register(z, name, "(JZ)I", cb);
    }
    // "Magicless" frames omit the 4-byte ZSTD magic number, so a frame written
    // (or expected) magicless is NOT interchangeable with a normal one.
    // Silently accepting the request produced a stream neither side could
    // read, with a "success" return to say all was well. Accept a request to
    // DISABLE it (that is the state we are already in) and report
    // ZSTD_error_parameter_unsupported otherwise.
    for name in ["setCompressionMagicless", "setDecompressionMagicless"] {
        r.register(z, name, "(JI)I", zstd_set_magicless);
        r.register(z, name, "(JZ)I", zstd_set_magicless);
    }
    // Dictionary loaders. These used to report 0 ("dictionary loaded") while
    // discarding it: a compressor would then emit dictionary-less frames a
    // dictionary-expecting peer rejects, and a decompressor would fail later
    // on dictionary-compressed input with an unrelated corruption error. A
    // null/empty dictionary is genuinely a no-op and still returns 0; a real
    // dictionary now reports ZSTD_error_parameter_unsupported at the point of
    // the request.
    r.register(z, "loadDictCompress", "(J[BI)I", zstd_load_dict_bytes);
    r.register(z, "loadDictDecompress", "(J[BI)I", zstd_load_dict_bytes);
    r.register(
        z,
        "loadFastDictCompress",
        "(JLcom/github/luben/zstd/ZstdDictCompress;)I",
        zstd_load_dict_object,
    );
    r.register(
        z,
        "loadFastDictDecompress",
        "(JLcom/github/luben/zstd/ZstdDictDecompress;)I",
        zstd_load_dict_object,
    );

    // Canonical libzstd error-code accessors. These are only consulted on the
    // error path (which Kafka's round-trip never reaches), but registering them
    // keeps a corruption-handling test from tripping an UnsatisfiedLinkError.
    // ZSTD error codes are `(size_t)(-ZSTD_error_xxx)`; as a signed long that is
    // the negated enum value. The constant for each name lives in
    // `register_err_const` (the registry takes a `fn` pointer, not a closure
    // that could capture the value).
    for name in [
        "errNoError",
        "errGeneric",
        "errPrefixUnknown",
        "errVersionUnsupported",
        "errFrameParameterUnsupported",
        "errFrameParameterWindowTooLarge",
        "errCorruptionDetected",
        "errChecksumWrong",
        "errDictionaryCorrupted",
        "errDictionaryWrong",
        "errDictionaryCreationFailed",
        "errParameterUnsupported",
        "errParameterOutOfBound",
        "errTableLogTooLarge",
        "errMaxSymbolValueTooLarge",
        "errMaxSymbolValueTooSmall",
        "errStageWrong",
        "errInitMissing",
        "errMemoryAllocation",
        "errWorkSpaceTooSmall",
        "errDstSizeTooSmall",
        "errSrcSizeWrong",
        "errDstBufferNull",
    ] {
        register_err_const(r, z, name);
    }
}

/// Register one `Zstd.errXxx()J` accessor returning the canonical negated
/// libzstd error enum value. Kept as an explicit match so each name maps to a
/// `fn` pointer (the registry does not accept capturing closures).
fn register_err_const(r: &mut NativeMethodRegistry, class: &str, name: &str) {
    let cb: cratonvm_native_api::NativeCallback = match name {
        "errNoError" => |_c, _a| Ok(Some(Value::Long(0))),
        "errGeneric" => |_c, _a| Ok(Some(Value::Long(-1))),
        "errPrefixUnknown" => |_c, _a| Ok(Some(Value::Long(-10))),
        "errVersionUnsupported" => |_c, _a| Ok(Some(Value::Long(-12))),
        "errFrameParameterUnsupported" => |_c, _a| Ok(Some(Value::Long(-14))),
        "errFrameParameterWindowTooLarge" => |_c, _a| Ok(Some(Value::Long(-16))),
        "errCorruptionDetected" => |_c, _a| Ok(Some(Value::Long(-20))),
        "errChecksumWrong" => |_c, _a| Ok(Some(Value::Long(-22))),
        "errDictionaryCorrupted" => |_c, _a| Ok(Some(Value::Long(-30))),
        "errDictionaryWrong" => |_c, _a| Ok(Some(Value::Long(-32))),
        "errDictionaryCreationFailed" => |_c, _a| Ok(Some(Value::Long(-34))),
        "errParameterUnsupported" => |_c, _a| Ok(Some(Value::Long(-40))),
        "errParameterOutOfBound" => |_c, _a| Ok(Some(Value::Long(-42))),
        "errTableLogTooLarge" => |_c, _a| Ok(Some(Value::Long(-44))),
        "errMaxSymbolValueTooLarge" => |_c, _a| Ok(Some(Value::Long(-46))),
        "errMaxSymbolValueTooSmall" => |_c, _a| Ok(Some(Value::Long(-48))),
        "errStageWrong" => |_c, _a| Ok(Some(Value::Long(-60))),
        "errInitMissing" => |_c, _a| Ok(Some(Value::Long(-62))),
        "errMemoryAllocation" => |_c, _a| Ok(Some(Value::Long(-64))),
        "errWorkSpaceTooSmall" => |_c, _a| Ok(Some(Value::Long(-66))),
        "errDstSizeTooSmall" => |_c, _a| Ok(Some(Value::Long(-70))),
        "errSrcSizeWrong" => |_c, _a| Ok(Some(Value::Long(-72))),
        "errDstBufferNull" => |_c, _a| Ok(Some(Value::Long(-74))),
        _ => |_c, _a| Ok(Some(Value::Long(-1))),
    };
    r.register(class, name, "()J", cb);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// Shapes chosen to reach every branch of the block grammar: an empty
    /// block, one shorter than the 15-byte literal-length escape, one long
    /// enough to force multi-byte literal *and* match lengths, a run that
    /// encodes as an overlapping match (offset 1), and near-incompressible
    /// data that is almost all literals.
    fn lz4_decode_shapes() -> Vec<Vec<u8>> {
        vec![
            Vec::new(),
            b"abc".to_vec(),
            b"the quick brown fox jumps over the lazy dog".to_vec(),
            vec![0x5Au8; 70_000],
            (0..40_000u32).map(|i| (i / 3) as u8).collect(),
            (0..30_000u32)
                .map(|i| i.wrapping_mul(2654435761).to_le_bytes()[0])
                .collect(),
        ]
    }

    #[test]
    fn lz4_block_decode_round_trips_and_reports_the_source_length() {
        for original in lz4_decode_shapes() {
            let mut comp = vec![0u8; lz4_flex::block::get_maximum_output_size(original.len()) + 16];
            let clen = lz4_flex::block::compress_into(&original, &mut comp).expect("compress");

            // The source is deliberately longer than the block — this is the
            // `LZ4_decompress_fast` shape, where the caller cannot say where the
            // block ends and the tail is whatever was already in the buffer.
            for tail in [0u8, 0xFF] {
                let mut src = comp[..clen].to_vec();
                src.extend(std::iter::repeat_n(tail, 512));

                let mut out = vec![0u8; original.len()];
                let (consumed, produced) =
                    lz4_block_decode(&src, &mut out).expect("decode a well-formed block");
                assert_eq!(out, original, "round trip (tail {tail:#x})");
                assert_eq!(produced, original.len());
                assert_eq!(
                    consumed, clen,
                    "consumed must be the block length, not the buffer length (tail {tail:#x})"
                );
            }
        }
    }

    #[test]
    fn lz4_block_decode_refuses_malformed_input_instead_of_panicking() {
        let original = b"the quick brown fox jumps over the lazy dog".to_vec();
        let mut comp = vec![0u8; lz4_flex::block::get_maximum_output_size(original.len()) + 16];
        let clen = lz4_flex::block::compress_into(&original, &mut comp).expect("compress");

        // Truncated: every prefix must refuse rather than index out of range.
        // (A prefix can legitimately decode when the output happens to fill
        // first, so this asserts termination and bounds, not failure.)
        for cut in 0..clen {
            let mut out = vec![0u8; original.len()];
            let _ = lz4_block_decode(&comp[..cut], &mut out);
        }

        // An output buffer smaller than the block produces is a refusal, not a
        // truncated write — `LZ4_decompress_safe` reports that as a negative.
        let mut small = vec![0u8; original.len() - 1];
        assert!(lz4_block_decode(&comp[..clen], &mut small).is_none());

        // The other direction: an output bound LARGER than the block produces
        // must succeed and report the real produced length, because that is how
        // `LZ4_decompress_safe` is normally called.
        let mut roomy = vec![0u8; original.len() + 4096];
        let (consumed, produced) =
            lz4_block_decode(&comp[..clen], &mut roomy).expect("oversized output is not an error");
        assert_eq!(consumed, clen);
        assert_eq!(produced, original.len());
        assert_eq!(&roomy[..produced], &original[..]);

        // A match offset of 0 is invalid, and one reaching before the start of
        // the output would copy uninitialised bytes. Token 0x00 = no literals,
        // then a zero offset.
        let mut out = vec![0u8; 8];
        assert!(lz4_block_decode(&[0x00, 0x00, 0x00], &mut out).is_none());
        assert!(lz4_block_decode(&[0x00, 0x04, 0x00], &mut out).is_none());
    }

    #[test]
    fn lz4_compress_bound_matches_liblz4() {
        // liblz4's LZ4_COMPRESSBOUND(n) == n + n/255 + 16, which is what HotSpot
        // answers through the real .so. The previous body used lz4_flex's own
        // (larger) estimate and diverged for every input.
        let mut ctx = crate::test_utils::mock_ctx();
        for n in [0i64, 1, 255, 256, 4096, 65_536, 1_000_000] {
            let got = lz4_compress_bound(&mut ctx, &[Value::Int(n as i32)])
                .unwrap()
                .unwrap();
            assert_eq!(got, Value::Int((n + n / 255 + 16) as i32), "bound for {n}");
        }
    }

    #[test]
    fn snappy_block_round_trip() {
        let original = b"the quick brown fox jumps over the lazy dog, repeatedly.....";
        let mut comp = vec![0u8; snap::raw::max_compress_len(original.len())];
        let clen = snap::raw::Encoder::new()
            .compress(original, &mut comp)
            .unwrap();
        let dlen = snap::raw::decompress_len(&comp[..clen]).unwrap();
        assert_eq!(dlen, original.len());
        let mut out = vec![0u8; dlen];
        let n = snap::raw::Decoder::new()
            .decompress(&comp[..clen], &mut out)
            .unwrap();
        assert_eq!(&out[..n], original);
    }

    #[test]
    fn lz4_block_round_trip() {
        let original: Vec<u8> = (0..3000u32).map(|i| (i % 97) as u8).collect();
        let mut comp = vec![0u8; lz4_flex::block::get_maximum_output_size(original.len())];
        let clen = lz4_flex::block::compress_into(&original, &mut comp).unwrap();
        assert!(clen > 0 && clen <= comp.len());
        // safe-decompress path (the one Kafka uses), bounded by max output size.
        let out = lz4_flex::block::decompress(&comp[..clen], original.len()).unwrap();
        assert_eq!(out, original);
    }

    #[test]
    fn xxhash32_oneshot_matches_streaming_and_reference() {
        use std::hash::Hasher;
        // Reference xxHash32 of the empty input with seed 0 is 0x02CC5D05.
        let mut empty = twox_hash::XxHash32::with_seed(0);
        empty.write(&[]);
        assert_eq!(empty.finish() as u32, 0x02CC_5D05);

        // One-shot over "hello world" must equal the streamed "hello"+" world".
        let mut one = twox_hash::XxHash32::with_seed(0);
        one.write(b"hello world");
        let mut stream = twox_hash::XxHash32::with_seed(0);
        stream.write(b"hello");
        stream.write(b" world");
        assert_eq!(one.finish(), stream.finish());
    }

    #[test]
    fn zstd_streaming_round_trip() {
        // Mirror the JNI contract end-to-end with raw zstd_safe buffers: a
        // multi-call compress (continue + end) then a single decompress.
        let original: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();

        let mut cctx = CCtx::try_create().unwrap();
        let mut framed = Vec::new();
        // continue
        {
            let mut inb = InBuffer::around(&original);
            let mut tmp = vec![0u8; unsafe { zstd_safe::zstd_sys::ZSTD_CStreamOutSize() }];
            let mut outb = OutBuffer::around(&mut tmp[..]);
            cctx.compress_stream2(&mut outb, &mut inb, ZSTD_EndDirective::ZSTD_e_continue)
                .unwrap();
            framed.extend_from_slice(outb.as_slice());
            assert_eq!(inb.pos, original.len());
        }
        // end (drain)
        loop {
            let empty: [u8; 0] = [];
            let mut inb = InBuffer::around(&empty);
            let mut tmp = vec![0u8; unsafe { zstd_safe::zstd_sys::ZSTD_CStreamOutSize() }];
            let mut outb = OutBuffer::around(&mut tmp[..]);
            let rem = cctx
                .compress_stream2(&mut outb, &mut inb, ZSTD_EndDirective::ZSTD_e_end)
                .unwrap();
            framed.extend_from_slice(outb.as_slice());
            if rem == 0 {
                break;
            }
        }

        let mut dctx = DCtx::try_create().unwrap();
        let mut restored = Vec::new();
        let mut inb = InBuffer::around(&framed);
        while inb.pos < framed.len() {
            let mut tmp = vec![0u8; unsafe { zstd_safe::zstd_sys::ZSTD_DStreamOutSize() }];
            let mut outb = OutBuffer::around(&mut tmp[..]);
            dctx.decompress_stream(&mut outb, &mut inb).unwrap();
            restored.extend_from_slice(outb.as_slice());
        }
        assert_eq!(restored, original);
    }
}
