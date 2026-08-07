// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Real-JDK-mode native implementations for `java.util.zip.Inflater` and
//! `java.util.zip.Deflater`, backed by the `flate2` crate.
//!
//! Context: the JDK 25 pure-Java ZIP parser (`java.util.zip.ZipFile$Source`,
//! `Inflater`) compiles into bytecode that calls natives such as
//! `Inflater.inflateBytesBytes(J[BII[BII)J`. If the native returns 0 or is
//! missing, the Java wrapper dereferences a zero `jzstream` handle and the
//! process SIGSEGVs. This module wires those natives up to real DEFLATE
//! state machines.
//!
//! Bit packing of the returned `long` from `inflateBytesBytes` (confirmed by
//! disassembling `java.util.zip.Inflater.inflate([BII)I` in JDK 25):
//!   bits  0..30  = inputConsumed   (mask 0x7FFFFFFF)
//!   bits 31..61  = outputConsumed  (packed >> 31) & 0x7FFFFFFF
//!   bit    62    = finished        (packed >> 62) & 1
//!   bit    63    = needDict        (packed >> 63) & 1
//!
//! Only runs in real-JDK mode. Synthetic mode registers Java-level overrides
//! in `phases_late.rs::register_phase71_natives` against a 4-field synthetic
//! Inflater layout; those run AFTER us (essential -> synthetic in
//! `register_builtins`), so last-writer-wins makes synthetic take over.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;

use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};
use flate2::{Compress, Decompress, FlushCompress, FlushDecompress};

// ---------------------------------------------------------------------------
// Handle tables
// ---------------------------------------------------------------------------

struct InflaterState {
    decomp: Decompress,
    // zlib_header: true means the stream has a zlib header (nowrap == false).
    // Tracked here so that `reset(long)` can recreate the stream in the same
    // mode, since flate2's `Decompress::reset` takes the header flag as an arg.
    zlib_header: bool,
    /// Running ADLER-32 of the UNCOMPRESSED bytes produced so far — what
    /// `Inflater.getAdler()` is documented to return. `flate2::Decompress`
    /// does not expose zlib's internal `strm->adler`, and it is only
    /// maintained at all for zlib-wrapped streams, so we accumulate it
    /// ourselves over every output chunk. Seeded with the zlib initial value
    /// (1), reset back to 1 by `reset(long)`.
    adler: u32,
}

struct DeflaterState {
    compress: Compress,
    // zlib_header: tracked so `reset(long)`/level changes can recreate the
    // stream in the same mode, mirroring InflaterState.
    zlib_header: bool,
    // Real JDK `Deflater.deflate()` is idempotent once FINISH has produced
    // `Z_STREAM_END`: further calls are a documented no-op (0 bytes
    // consumed/produced, finished stays true) until `reset()`. Calling
    // `Compress::compress` again on an already-finished zlib stream is
    // undefined by zlib's own contract (typically `Z_STREAM_ERROR`, but not
    // guaranteed) — track completion explicitly and short-circuit instead
    // of re-entering zlib once finished.
    finished: bool,
    /// Running ADLER-32 of the UNCOMPRESSED input consumed so far — what
    /// `Deflater.getAdler()` is documented to return. See the matching field
    /// on `InflaterState` for why it is accumulated here rather than read out
    /// of `flate2::Compress`.
    adler: u32,
}

/// Rolling ADLER-32 (RFC 1950 §9). `adler` starts at 1; feeding successive
/// slices is equivalent to hashing their concatenation, which is what lets the
/// Deflater/Inflater states keep a running value across many native calls.
fn adler32_update(adler: u32, data: &[u8]) -> u32 {
    const MOD_ADLER: u32 = 65521;
    let mut s1 = adler & 0xFFFF;
    let mut s2 = (adler >> 16) & 0xFFFF;
    // Chunk so the u32 accumulators cannot overflow before the reduction
    // (5552 is the classic zlib NMAX for 8-bit input).
    for chunk in data.chunks(5552) {
        for &b in chunk {
            s1 += b as u32;
            s2 += s1;
        }
        s1 %= MOD_ADLER;
        s2 %= MOD_ADLER;
    }
    (s2 << 16) | s1
}

fn inflater_table() -> &'static Mutex<HashMap<i64, InflaterState>> {
    static T: OnceLock<Mutex<HashMap<i64, InflaterState>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn deflater_table() -> &'static Mutex<HashMap<i64, DeflaterState>> {
    static T: OnceLock<Mutex<HashMap<i64, DeflaterState>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_handle() -> i64 {
    static COUNTER: AtomicI64 = AtomicI64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Argument / value helpers
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

fn arg_bool(args: &[Value], idx: usize) -> bool {
    match args.get(idx) {
        Some(Value::Int(0)) => false,
        Some(Value::Int(_)) => true,
        _ => false,
    }
}

fn arg_obj(args: &[Value], idx: usize) -> Option<ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(o)) => *o,
        _ => None,
    }
}

fn read_byte_array(ctx: &dyn NativeContext, arr: ObjectRef, off: usize, len: usize) -> Vec<u8> {
    let arr_len = ctx.array_length(arr);
    let end = (off + len).min(arr_len);
    let start = off.min(arr_len);
    let mut out = vec![0u8; end.saturating_sub(start)];
    let copied = ctx.read_byte_array_into(arr, start, &mut out);
    out.truncate(copied);
    out
}

fn write_byte_array(ctx: &mut dyn NativeContext, arr: ObjectRef, off: usize, data: &[u8]) -> usize {
    let arr_len = ctx.array_length(arr);
    let start = off.min(arr_len);
    let len = data.len().min(arr_len.saturating_sub(start));
    ctx.write_byte_array_from(arr, start, &data[..len]);
    len
}

fn defl_effective_level(level_raw: i32) -> i32 {
    if (0..=9).contains(&level_raw) {
        level_raw
    } else {
        6
    }
}

// Only used by the golden-byte-vector test below now that
// `defl_deflate_bytes_bytes` streams through `Compress` directly instead of
// buffering the whole body for a single one-shot compress at FINISH.
#[cfg(test)]
fn defl_zlib_compress(data: &[u8], level: i32, zlib_header: bool) -> Option<Vec<u8>> {
    let source_len = data.len() as libz_sys::uLong;
    let mut bound = unsafe { libz_sys::compressBound(source_len) } as usize;
    if bound == 0 {
        bound = data.len().saturating_add(64);
    }
    let mut z = vec![0u8; bound];
    let mut z_len = bound as libz_sys::uLong;
    let rc = unsafe {
        libz_sys::compress2(
            z.as_mut_ptr(),
            &mut z_len,
            data.as_ptr(),
            source_len,
            defl_effective_level(level),
        )
    };
    if rc != 0 || z_len < 6 {
        return None;
    }
    z.truncate(z_len as usize);
    if zlib_header {
        Some(z)
    } else {
        Some(z[2..z.len() - 4].to_vec())
    }
}

#[cfg(test)]
fn defl_compress_finished(data: &[u8], level: i32, zlib_header: bool) -> std::io::Result<Vec<u8>> {
    if let Some(out) = defl_zlib_compress(data, level, zlib_header) {
        return Ok(out);
    }

    use std::io::Write;
    let level = flate2::Compression::new(defl_effective_level(level) as u32);
    if zlib_header {
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), level);
        encoder.write_all(data)?;
        encoder.finish()
    } else {
        let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), level);
        encoder.write_all(data)?;
        encoder.finish()
    }
}

// ---------------------------------------------------------------------------
// Pack result
// ---------------------------------------------------------------------------

fn pack_inflate_result(
    input_consumed: u32,
    output_consumed: u32,
    finished: bool,
    need_dict: bool,
) -> i64 {
    let mut p: u64 = 0;
    p |= (input_consumed as u64) & 0x7FFF_FFFF;
    p |= ((output_consumed as u64) & 0x7FFF_FFFF) << 31;
    if finished {
        p |= 1u64 << 62;
    }
    if need_dict {
        p |= 1u64 << 63;
    }
    p as i64
}

fn pack_deflate_result(input_consumed: u32, output_consumed: u32, finished: bool) -> i64 {
    // Deflater uses the same low-bits layout; we leave dict/other flag bits at 0.
    let mut p: u64 = 0;
    p |= (input_consumed as u64) & 0x7FFF_FFFF;
    p |= ((output_consumed as u64) & 0x7FFF_FFFF) << 31;
    if finished {
        p |= 1u64 << 62;
    }
    p as i64
}

// ---------------------------------------------------------------------------
// Inflater natives
// ---------------------------------------------------------------------------

fn infl_init_ids(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

fn infl_init(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // static init(boolean nowrap) -> long
    let nowrap = arg_bool(args, 0);
    let zlib_header = !nowrap;
    let state = InflaterState {
        decomp: Decompress::new(zlib_header),
        zlib_header,
        adler: 1,
    };
    let handle = next_handle();
    inflater_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(handle, state);
    Ok(Some(Value::Long(handle)))
}

fn infl_set_dictionary(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // static setDictionary(long, byte[], int off, int len)
    // flate2's `Decompress::set_dictionary` is gated behind a zlib backend
    // feature we don't enable; preset dictionaries are not used by any
    // JAR/ZIP entry encountered on the bootstrap path, so we no-op here.
    Ok(None)
}

fn infl_set_dictionary_buffer(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Direct buffer setDictionary — not essential for bootstrap; no-op.
    Ok(None)
}

fn infl_inflate_bytes_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // this, long addr, byte[] in, int inOff, int inLen, byte[] out, int outOff, int outLen
    // args[0] = this (receiver)
    let addr = arg_long(args, 1);
    let input_arr = arg_obj(args, 2);
    let in_off = arg_int(args, 3).max(0) as usize;
    let in_len = arg_int(args, 4).max(0) as usize;
    let output_arr = arg_obj(args, 5);
    let out_off = arg_int(args, 6).max(0) as usize;
    let out_len = arg_int(args, 7).max(0) as usize;

    let input_data = match input_arr {
        Some(a) => read_byte_array(ctx, a, in_off, in_len),
        None => Vec::new(),
    };
    let mut output_buf = vec![0u8; out_len];

    let mut tbl = inflater_table().lock().unwrap_or_else(|e| e.into_inner());
    let st = match tbl.get_mut(&addr) {
        Some(s) => s,
        None => {
            // Zero handle / closed stream. Return all-zero so Java sees no progress.
            return Ok(Some(Value::Long(0)));
        }
    };

    let total_in_before = st.decomp.total_in();
    let total_out_before = st.decomp.total_out();
    let status = st
        .decomp
        .decompress(&input_data, &mut output_buf, FlushDecompress::None);
    let input_consumed = (st.decomp.total_in() - total_in_before) as u32;
    let output_consumed = (st.decomp.total_out() - total_out_before) as u32;

    let (finished, need_dict) = match status {
        Ok(flate2::Status::StreamEnd) => (true, false),
        Ok(flate2::Status::Ok) => (false, false),
        Ok(flate2::Status::BufError) => (false, false),
        Err(e) => {
            // A "needs dictionary" error from flate2 carries an Adler-32 on
            // `DecompressError`; surface it via the needDict bit so Java
            // throws the right exception.
            let needs = e.needs_dictionary().is_some();
            (false, needs)
        }
    };
    // `Inflater.getAdler()` reports the ADLER-32 of the uncompressed data.
    // `.min(len)` is belt-and-braces: a slice panic here would abort the VM.
    let produced = (output_consumed as usize).min(output_buf.len());
    st.adler = adler32_update(st.adler, &output_buf[..produced]);

    drop(tbl)?;

    if let Some(a) = output_arr {
        if output_consumed > 0 {
            write_byte_array(ctx, a, out_off, &output_buf[..output_consumed as usize]);
        }
    }

    Ok(Some(Value::Long(pack_inflate_result(
        input_consumed,
        output_consumed,
        finished,
        need_dict,
    ))))
}

// audit-round6 (LOW, direct-ByteBuffer inflate no-op):
//
// These three natives cover the inflate combinations where at least one
// side is a *direct* `ByteBuffer`, addressed by a raw `long` rather than a
// `byte[]`. This VM has no raw-memory view into direct buffers here, so we
// genuinely cannot make progress on them.
//
// The previous implementation returned a packed `Long(0)` ("no progress")
// and claimed the JDK would "loop around to an array-backed path". That is
// NOT correct: the JDK's `Inflater` dispatches to the native that matches
/// Shared decompression core for all four `Inflater.inflate*` overloads.
fn infl_do_decompress(
    addr: i64,
    input_data: &[u8],
    output_buf: &mut [u8],
) -> (u32, u32, bool, bool) {
    let mut tbl = inflater_table().lock().unwrap_or_else(|e| e.into_inner());
    let Some(st) = tbl.get_mut(&addr) else {
        return (0, 0, false, false);
    };
    let total_in_before = st.decomp.total_in();
    let total_out_before = st.decomp.total_out();
    let status = st
        .decomp
        .decompress(input_data, output_buf, FlushDecompress::None);
    let input_consumed = (st.decomp.total_in() - total_in_before) as u32;
    let output_consumed = (st.decomp.total_out() - total_out_before) as u32;
    let (finished, need_dict) = match status {
        Ok(flate2::Status::StreamEnd) => (true, false),
        Ok(flate2::Status::Ok | flate2::Status::BufError) => (false, false),
        Err(e) => (false, e.needs_dictionary().is_some()),
    };
    // Same running ADLER-32 over the uncompressed output as the bytes-bytes
    // overload above — `getAdler()` must agree no matter which overload the
    // JDK wrapper picked. `.min(len)` guards against a slice panic.
    let produced = (output_consumed as usize).min(output_buf.len());
    st.adler = adler32_update(st.adler, &output_buf[..produced]);
    (input_consumed, output_consumed, finished, need_dict)
}

fn infl_inflate_bytes_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // input: byte[], output: direct ByteBuffer (long addr). Cannot write the
    // direct output buffer — no array fallback exists for a direct output.
    let addr = arg_long(args, 1);
    let input_arr = arg_obj(args, 2);
    let in_off = arg_int(args, 3).max(0) as usize;
    let in_len = arg_int(args, 4).max(0) as usize;
    let output_addr = arg_long(args, 5);
    let out_len = arg_int(args, 6).max(0) as usize;
    let input_data = input_arr
        .map(|a| read_byte_array(ctx, a, in_off, in_len))
        .unwrap_or_default();
    let mut output_buf = vec![0u8; out_len];
    let (input_consumed, output_consumed, finished, need_dict) =
        infl_do_decompress(addr, &input_data, &mut output_buf);
    if output_consumed > 0
        && !ctx.copy_to_native_memory(output_addr, &output_buf[..output_consumed as usize])
    {
        return Err(RuntimeError::IOException {
            message: format!("inflateBytesBuffer: invalid output buffer address {output_addr:#x}"),
        }
        .into());
    }
    Ok(Some(Value::Long(pack_inflate_result(
        input_consumed,
        output_consumed,
        finished,
        need_dict,
    ))))
}

fn infl_inflate_buffer_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // input: direct ByteBuffer (long addr), output: byte[]. Cannot read the
    // direct input bytes, so no progress is possible.
    let addr = arg_long(args, 1);
    let input_addr = arg_long(args, 2);
    let in_len = arg_int(args, 3).max(0) as usize;
    let output_arr = arg_obj(args, 4);
    let out_off = arg_int(args, 5).max(0) as usize;
    let out_len = arg_int(args, 6).max(0) as usize;
    let mut input_data = vec![0u8; in_len];
    if in_len > 0 && !ctx.copy_from_native_memory(input_addr, &mut input_data) {
        return Err(RuntimeError::IOException {
            message: format!("inflateBufferBytes: invalid input buffer address {input_addr:#x}"),
        }
        .into());
    }
    let mut output_buf = vec![0u8; out_len];
    let (input_consumed, output_consumed, finished, need_dict) =
        infl_do_decompress(addr, &input_data, &mut output_buf);
    if let Some(a) = output_arr {
        if output_consumed > 0 {
            write_byte_array(ctx, a, out_off, &output_buf[..output_consumed as usize]);
        }
    }
    Ok(Some(Value::Long(pack_inflate_result(
        input_consumed,
        output_consumed,
        finished,
        need_dict,
    ))))
}

fn infl_inflate_buffer_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Both sides are direct ByteBuffers — neither readable nor writable here.
    let addr = arg_long(args, 1);
    let input_addr = arg_long(args, 2);
    let in_len = arg_int(args, 3).max(0) as usize;
    let output_addr = arg_long(args, 4);
    let out_len = arg_int(args, 5).max(0) as usize;
    let mut input_data = vec![0u8; in_len];
    if in_len > 0 && !ctx.copy_from_native_memory(input_addr, &mut input_data) {
        return Err(RuntimeError::IOException {
            message: format!("inflateBufferBuffer: invalid input buffer address {input_addr:#x}"),
        }
        .into());
    }
    let mut output_buf = vec![0u8; out_len];
    let (input_consumed, output_consumed, finished, need_dict) =
        infl_do_decompress(addr, &input_data, &mut output_buf);
    if output_consumed > 0
        && !ctx.copy_to_native_memory(output_addr, &output_buf[..output_consumed as usize])
    {
        return Err(RuntimeError::IOException {
            message: format!("inflateBufferBuffer: invalid output buffer address {output_addr:#x}"),
        }
        .into());
    }
    Ok(Some(Value::Long(pack_inflate_result(
        input_consumed,
        output_consumed,
        finished,
        need_dict,
    ))))
}

fn infl_get_adler(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = arg_long(args, 0);
    let tbl = inflater_table().lock().unwrap_or_else(|e| e.into_inner());
    // Was an unconditional `1` (the zlib seed), i.e. the checksum never
    // changed no matter how much data was inflated — a caller validating a
    // zlib-wrapped stream by comparing `getAdler()` against the trailer would
    // always see a mismatch (or, worse, silently "verify" an empty stream).
    // The real ADLER-32 of the uncompressed output is now accumulated by
    // `infl_do_decompress`/`infl_inflate_bytes_bytes`.
    let adler = tbl.get(&addr).map_or(1, |st| st.adler);
    Ok(Some(Value::Int(adler as i32)))
}

fn infl_reset(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = arg_long(args, 0);
    let mut tbl = inflater_table().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(st) = tbl.get_mut(&addr) {
        st.decomp.reset(st.zlib_header);
        // `Inflater.reset()` restarts the checksum at the zlib seed too.
        st.adler = 1;
    }
    Ok(None)
}

fn infl_end(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = arg_long(args, 0);
    inflater_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&addr);
    Ok(None)
}

// ---------------------------------------------------------------------------
// Deflater natives
// ---------------------------------------------------------------------------

fn defl_init_ids(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

fn defl_init(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // static init(int level, int strategy, boolean nowrap) -> long
    let level_raw = arg_int(args, 0);
    let _strategy = arg_int(args, 1);
    let nowrap = arg_bool(args, 2);
    let zlib_header = !nowrap;
    let level = flate2::Compression::new(defl_effective_level(level_raw) as u32);
    let state = DeflaterState {
        compress: Compress::new(level, zlib_header),
        zlib_header,
        finished: false,
        adler: 1,
    };
    let handle = next_handle();
    deflater_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(handle, state);
    Ok(Some(Value::Long(handle)))
}

fn defl_set_dictionary(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Preset dictionaries not needed for the bootstrap JAR path; see
    // infl_set_dictionary for rationale.
    Ok(None)
}

fn defl_set_dictionary_buffer(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Shared compression core for all 4 `Deflater.deflate*` native overloads
/// (bytes-bytes, bytes-buffer, buffer-bytes, buffer-buffer). Looks up the
/// live `flate2::Compress` by `addr`, applies any pending level/strategy
/// params, and compresses `input_data` into `output_buf`. Returns
/// `(input_consumed, output_consumed, finished)` — `(0, 0, false)` when
/// `addr` isn't registered (matches the pre-existing bytes-bytes behavior:
/// a silent no-progress result rather than an error, since a caller that
/// retries on 0 progress would otherwise spin).
///
/// JDK `Deflater` flush codes: 0=NO_FLUSH, 1=SYNC_FLUSH, 2=FULL_FLUSH,
/// 4=FINISH (see `java.util.zip.Deflater.{NO,SYNC,FULL}_FLUSH` constants).
/// Real streaming producers (e.g. Jetty's `GzipHttpOutputInterceptor`)
/// depend on SYNC_FLUSH/FULL_FLUSH actually producing output mid-stream —
/// previously the bytes-bytes native only ever emitted bytes on FINISH,
/// buffering the whole body and never satisfying a caller that blocks
/// waiting for a flush to make progress before it hands over more input.
fn defl_do_compress(
    addr: i64,
    params: i32,
    flush_code: i32,
    input_data: &[u8],
    output_buf: &mut [u8],
    which: &str,
) -> Result<(u32, u32, bool), MethodCallFailed> {
    let flush = match flush_code {
        1 => FlushCompress::Sync,
        2 => FlushCompress::Full,
        4 => FlushCompress::Finish,
        _ => FlushCompress::None,
    };

    let mut tbl = deflater_table().lock().unwrap_or_else(|e| e.into_inner());
    let st = match tbl.get_mut(&addr) {
        Some(s) => s,
        None => {
            if crate::nbflags().dbg_deflate {
                eprintln!(
                    "[DBG-DEFLATER] {which} addr={addr:#x} NOT FOUND in deflater_table (silent 0/0 return)"
                );
            }
            return Ok((0, 0, false));
        }
    };

    if st.finished {
        // Matches real JDK: once finished, deflate() is a no-op until reset().
        return Ok((0, 0, true));
    }
    if params != 0 {
        // JDK packs params as: bit0=set, bits1..2=strategy, bits3..=level.
        let level = flate2::Compression::new(defl_effective_level(params >> 3) as u32);
        let _ = st.compress.set_level(level);
    }

    let total_in_before = st.compress.total_in();
    let total_out_before = st.compress.total_out();
    let status = st
        .compress
        .compress(input_data, output_buf, flush)
        .map_err(|e| RuntimeError::IOException {
            message: format!("Deflater compression failed: {:?}", e),
        })?;
    let input_consumed = (st.compress.total_in() - total_in_before) as u32;
    let output_consumed = (st.compress.total_out() - total_out_before) as u32;
    // `Deflater.getAdler()` reports the ADLER-32 of the uncompressed input.
    // `.min(len)` is belt-and-braces: a slice panic here would abort the VM.
    let consumed = (input_consumed as usize).min(input_data.len());
    st.adler = adler32_update(st.adler, &input_data[..consumed]);
    let finished = matches!(status, flate2::Status::StreamEnd);
    if finished {
        st.finished = true;
    }
    Ok((input_consumed, output_consumed, finished))
}

fn defl_deflate_bytes_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // this, long addr, byte[] in, int inOff, int inLen, byte[] out, int outOff, int outLen,
    // int flush, int params
    let addr = arg_long(args, 1);
    let input_arr = arg_obj(args, 2);
    let in_off = arg_int(args, 3).max(0) as usize;
    let in_len = arg_int(args, 4).max(0) as usize;
    let output_arr = arg_obj(args, 5);
    let out_off = arg_int(args, 6).max(0) as usize;
    let out_len = arg_int(args, 7).max(0) as usize;
    let flush_code = arg_int(args, 8);
    let params = arg_int(args, 9);

    let input_data = match input_arr {
        Some(a) => read_byte_array(ctx, a, in_off, in_len),
        None => Vec::new(),
    };
    let mut output_buf = vec![0u8; out_len];
    if crate::nbflags().dbg_deflate {
        eprintln!(
            "[DBG-DEFLATER] deflateBytesBytes addr={addr:#x} in_len={in_len} out_len={out_len} flush_code={flush_code} params={params}"
        );
    }

    let (input_consumed, output_consumed, finished) = defl_do_compress(
        addr,
        params,
        flush_code,
        &input_data,
        &mut output_buf,
        "deflateBytesBytes",
    )?;

    if let Some(a) = output_arr {
        if output_consumed > 0 {
            write_byte_array(ctx, a, out_off, &output_buf[..output_consumed as usize]);
        }
    }

    Ok(Some(Value::Long(pack_deflate_result(
        input_consumed,
        output_consumed,
        finished,
    ))))
}

/// `Deflater.deflateBytesBuffer(long addr, byte[] in, int inOff, int inLen,
/// long outputAddr, int outLen, int flush, int params) -> long`. Input is a
/// heap `byte[]`; output is a direct `ByteBuffer` — the JDK bytecode wrapper
/// already resolved it to its native address before calling this native
/// (`Buffer.address`, read the same way other native-buffer call sites in
/// this codebase do via `NativeContext::copy_to_native_memory`). Was
/// previously "not supported" (threw `NotImplemented`), which silently
/// truncated every gzip/deflate response written through a direct output
/// buffer to just its 10-byte gzip header — see
/// fixed-suite-bugs/springboot/jetty-webserver-factory-poststartup-timeout-and-reflective-supertype-residuals-FIXED.md
/// (the `compressionOfResponseToGetRequest` residual: Jetty's
/// `GzipHttpOutputInterceptor` calls exactly this overload).
fn defl_deflate_bytes_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = arg_long(args, 1);
    let input_arr = arg_obj(args, 2);
    let in_off = arg_int(args, 3).max(0) as usize;
    let in_len = arg_int(args, 4).max(0) as usize;
    let output_addr = arg_long(args, 5);
    let out_len = arg_int(args, 6).max(0) as usize;
    let flush_code = arg_int(args, 7);
    let params = arg_int(args, 8);

    let input_data = match input_arr {
        Some(a) => read_byte_array(ctx, a, in_off, in_len),
        None => Vec::new(),
    };
    let mut output_buf = vec![0u8; out_len];
    if crate::nbflags().dbg_deflate {
        eprintln!(
            "[DBG-DEFLATER] deflateBytesBuffer addr={addr:#x} in_len={in_len} out_len={out_len} flush_code={flush_code} params={params}"
        );
    }

    let (input_consumed, output_consumed, finished) = defl_do_compress(
        addr,
        params,
        flush_code,
        &input_data,
        &mut output_buf,
        "deflateBytesBuffer",
    )?;

    if output_consumed > 0
        && !ctx.copy_to_native_memory(output_addr, &output_buf[..output_consumed as usize])
    {
        return Err(RuntimeError::IOException {
            message: format!("deflateBytesBuffer: invalid output buffer address {output_addr:#x}"),
        }
        .into());
    }

    Ok(Some(Value::Long(pack_deflate_result(
        input_consumed,
        output_consumed,
        finished,
    ))))
}

/// `Deflater.deflateBufferBytes(long addr, long inputAddr, int inLen, byte[]
/// out, int outOff, int outLen, int flush, int params) -> long`. Mirror of
/// `deflateBytesBuffer` with input/output swapped: input is a direct
/// `ByteBuffer` (already resolved to its native address), output a heap
/// `byte[]`.
fn defl_deflate_buffer_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = arg_long(args, 1);
    let input_addr = arg_long(args, 2);
    let in_len = arg_int(args, 3).max(0) as usize;
    let output_arr = arg_obj(args, 4);
    let out_off = arg_int(args, 5).max(0) as usize;
    let out_len = arg_int(args, 6).max(0) as usize;
    let flush_code = arg_int(args, 7);
    let params = arg_int(args, 8);

    let mut input_data = vec![0u8; in_len];
    if in_len > 0 && !ctx.copy_from_native_memory(input_addr, &mut input_data) {
        return Err(RuntimeError::IOException {
            message: format!("deflateBufferBytes: invalid input buffer address {input_addr:#x}"),
        }
        .into());
    }
    let mut output_buf = vec![0u8; out_len];
    if crate::nbflags().dbg_deflate {
        eprintln!(
            "[DBG-DEFLATER] deflateBufferBytes addr={addr:#x} in_len={in_len} out_len={out_len} flush_code={flush_code} params={params}"
        );
    }

    let (input_consumed, output_consumed, finished) = defl_do_compress(
        addr,
        params,
        flush_code,
        &input_data,
        &mut output_buf,
        "deflateBufferBytes",
    )?;

    if let Some(a) = output_arr {
        if output_consumed > 0 {
            write_byte_array(ctx, a, out_off, &output_buf[..output_consumed as usize]);
        }
    }

    Ok(Some(Value::Long(pack_deflate_result(
        input_consumed,
        output_consumed,
        finished,
    ))))
}

/// `Deflater.deflateBufferBuffer(long addr, long inputAddr, int inLen, long
/// outputAddr, int outLen, int flush, int params) -> long`. Both input and
/// output are direct `ByteBuffer`s (already resolved to native addresses) —
/// the exact overload Jetty's `GzipHttpOutputInterceptor` calls when both
/// its scratch input and the pooled network output buffer are direct (the
/// common case for a real NIO connector). See `defl_deflate_bytes_buffer`'s
/// doc comment for the bug this fixes.
fn defl_deflate_buffer_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = arg_long(args, 1);
    let input_addr = arg_long(args, 2);
    let in_len = arg_int(args, 3).max(0) as usize;
    let output_addr = arg_long(args, 4);
    let out_len = arg_int(args, 5).max(0) as usize;
    let flush_code = arg_int(args, 6);
    let params = arg_int(args, 7);

    let mut input_data = vec![0u8; in_len];
    if in_len > 0 && !ctx.copy_from_native_memory(input_addr, &mut input_data) {
        return Err(RuntimeError::IOException {
            message: format!("deflateBufferBuffer: invalid input buffer address {input_addr:#x}"),
        }
        .into());
    }
    let mut output_buf = vec![0u8; out_len];
    if crate::nbflags().dbg_deflate {
        eprintln!(
            "[DBG-DEFLATER] deflateBufferBuffer addr={addr:#x} in_len={in_len} out_len={out_len} flush_code={flush_code} params={params}"
        );
    }

    let (input_consumed, output_consumed, finished) = defl_do_compress(
        addr,
        params,
        flush_code,
        &input_data,
        &mut output_buf,
        "deflateBufferBuffer",
    )?;

    if output_consumed > 0
        && !ctx.copy_to_native_memory(output_addr, &output_buf[..output_consumed as usize])
    {
        return Err(RuntimeError::IOException {
            message: format!("deflateBufferBuffer: invalid output buffer address {output_addr:#x}"),
        }
        .into());
    }

    Ok(Some(Value::Long(pack_deflate_result(
        input_consumed,
        output_consumed,
        finished,
    ))))
}

fn defl_get_adler(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = arg_long(args, 0);
    let tbl = deflater_table().lock().unwrap_or_else(|e| e.into_inner());
    // Was an unconditional `1` regardless of how much data had been fed in,
    // so `Deflater.getAdler()` could never be used to checksum the payload
    // (nor to cross-check a matching `Inflater.getAdler()`). Now returns the
    // real running ADLER-32 of the uncompressed input, maintained by
    // `defl_do_compress`.
    let adler = tbl.get(&addr).map_or(1, |st| st.adler);
    Ok(Some(Value::Int(adler as i32)))
}

fn defl_reset(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = arg_long(args, 0);
    let mut tbl = deflater_table().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(st) = tbl.get_mut(&addr) {
        st.compress.reset();
        st.finished = false;
        // `Deflater.reset()` restarts the checksum at the zlib seed too.
        st.adler = 1;
    }
    Ok(None)
}

fn defl_end(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = arg_long(args, 0);
    deflater_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&addr);
    Ok(None)
}

// audit-round6: `RuntimeError` is now used by the direct-buffer inflate
// natives (`infl_direct_buffer_unsupported`), so the former
// `_unused_runtime_error` import-silencing shim has been removed.
#[allow(dead_code)]
fn _unused_ret() -> ObjectRef {
    unreachable!()
}
#[allow(dead_code)]
fn _unused_aet() -> ArrayElementType {
    ArrayElementType::Byte
}

// ---------------------------------------------------------------------------
// CRC32 natives (real-JDK mode)
// ---------------------------------------------------------------------------
//
// The pure-Java `java.util.zip.CRC32` in JDK 25 delegates its hot loop to
// `private static native int updateBytes0(int crc, byte[] b, int off, int len)`
// (and `updateByteBuffer0` for direct buffers, plus `update(int crc, int b)`).
// Without intercepts, JAR-loading code that verifies entry CRCs trips an
// UnsatisfiedLinkError. We implement these in software using the classical
// IEEE 802.3 (reflected) polynomial 0xEDB88320 — the same polynomial used by
// the JDK and zlib. The Java side passes `~crc` to the native and complements
// the return value, so on entry/exit we operate on the *running* CRC value
// rather than the externally-visible "value" (which is the bit-complement).
//
// Reference: java.util.zip.CRC32.updateBytes (JDK 25) — calls
// `updateBytes0(crc, b, off, len)` where `crc` is the *public* CRC value
// (initial 0, never complemented in the Java wrapper) and the native is
// expected to return the new public CRC value. zlib's `crc32` implements
// this contract by complementing the running state on entry and exit; the
// inner reflected-shift loop runs on the complemented form. We mirror that
// exactly here — getting this wrong by treating `crc` as the *running*
// state breaks `ZipInputStream` (CRC of inflated entry mismatches the entry
// header's stored CRC) on every benchmark that extracts data from a ZIP,
// including DaCapo's `Benchmark.unpackZipStream` (avrora).

/// Inner reflected CRC-32/IEEE shift (poly 0xEDB88320). Takes the running
/// state (complemented form — i.e. `~public_crc`), returns the updated
/// running state. Callers must complement on entry/exit; see
/// `crc32_update_public` for the boundary-correct wrapper.
fn crc32_step(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    crc
}

/// CRC-32/IEEE update matching the JDK `CRC32` contract: `crc` is the
/// public value (initial 0), output is the new public value.
fn crc32_update_public(public_crc: u32, data: &[u8]) -> u32 {
    // `libz-sys` is already our deterministic zlib implementation for the
    // Deflater/GZIP bridges. Its `crc32` API uses Java's public CRC
    // representation (fresh state is zero) and its vectorized implementation
    // avoids the old eight-shifts-per-byte path. The loader ZIP64 fixture
    // checksums seven GiB of data, so that scalar loop consumed the complete
    // 300-second class budget before the archive could be reopened.
    //
    // `data.len()` originates from a Java `int`, and is therefore within
    // zlib's `uInt` range on every supported host.
    unsafe { libz_sys::crc32(public_crc as _, data.as_ptr(), data.len() as libz_sys::uInt) as u32 }
}

fn crc32_update(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // static native int update(int crc, int b)
    let crc = arg_int(args, 0) as u32;
    let b = (arg_int(args, 1) & 0xFF) as u8;
    Ok(Some(Value::Int(crc32_update_public(crc, &[b]) as i32)))
}

fn crc32_update_bytes_0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // private static native int updateBytes0(int crc, byte[] b, int off, int len)
    let crc = arg_int(args, 0) as u32;
    let arr = arg_obj(args, 1);
    let off = arg_int(args, 2).max(0) as usize;
    let len = arg_int(args, 3).max(0) as usize;
    let bytes = match arr {
        Some(a) => read_byte_array(ctx, a, off, len),
        None => Vec::new(),
    };
    let new_crc = crc32_update_public(crc, &bytes);
    Ok(Some(Value::Int(new_crc as i32)))
}

fn crc32_update_byte_buffer_0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // private static native int updateByteBuffer0(int crc, long addr, int off, int len)
    //
    // `addr` is the direct buffer's resolved native base address (the caller
    // already extracted it via ((DirectBuffer) buf).address()); `off`/`len`
    // select the region to checksum, matching the byte[] overload's contract.
    let crc = arg_int(args, 0) as u32;
    let addr = arg_long(args, 1);
    let off = arg_int(args, 2) as i64;
    let len = arg_int(args, 3).max(0) as usize;
    let mut bytes = vec![0u8; len];
    if len > 0 && !ctx.copy_from_native_memory(addr + off, &mut bytes) {
        return Err(RuntimeError::IOException {
            message: "CRC32.updateByteBuffer0: failed to read native buffer memory".to_string(),
        }
        .into());
    }
    let new_crc = crc32_update_public(crc, &bytes);
    Ok(Some(Value::Int(new_crc as i32)))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register_zip_real_natives(r: &mut NativeMethodRegistry) {
    // Inflater
    let il = "java/util/zip/Inflater";
    r.register_with_kind(il, "initIDs", "()V", infl_init_ids, NativeKind::Bridge);
    r.register_with_kind(il, "init", "(Z)J", infl_init, NativeKind::Bridge);
    r.register_with_kind(il, "setDictionary", "(J[BII)V", infl_set_dictionary, NativeKind::Bridge);
    r.register_with_kind(
        il,
        "setDictionaryBuffer",
        "(JJI)V",
        infl_set_dictionary_buffer,
        NativeKind::Bridge,
    );
    r.register_with_kind(
        il,
        "inflateBytesBytes",
        "(J[BII[BII)J",
        infl_inflate_bytes_bytes,
        NativeKind::Bridge,
    );
    r.register_with_kind(
        il,
        "inflateBytesBuffer",
        "(J[BIIJI)J",
        infl_inflate_bytes_buffer,
        NativeKind::Bridge,
    );
    r.register_with_kind(
        il,
        "inflateBufferBytes",
        "(JJI[BII)J",
        infl_inflate_buffer_bytes,
        NativeKind::Bridge,
    );
    r.register_with_kind(
        il,
        "inflateBufferBuffer",
        "(JJIJI)J",
        infl_inflate_buffer_buffer,
        NativeKind::Bridge,
    );
    r.register_with_kind(il, "getAdler", "(J)I", infl_get_adler, NativeKind::Bridge);
    r.register_with_kind(il, "reset", "(J)V", infl_reset, NativeKind::Bridge);
    r.register_with_kind(il, "end", "(J)V", infl_end, NativeKind::Bridge);

    // Deflater
    let dl = "java/util/zip/Deflater";
    r.register(dl, "initIDs", "()V", defl_init_ids);
    r.register_with_kind(dl, "init", "(IIZ)J", defl_init, NativeKind::Bridge);
    r.register_with_kind(dl, "setDictionary", "(J[BII)V", defl_set_dictionary, NativeKind::Bridge);
    r.register_with_kind(
        dl,
        "setDictionaryBuffer",
        "(JJI)V",
        defl_set_dictionary_buffer,
        NativeKind::Bridge,
    );
    r.register_with_kind(
        dl,
        "deflateBytesBytes",
        "(J[BII[BIIII)J",
        defl_deflate_bytes_bytes,
        NativeKind::Bridge,
    );
    r.register_with_kind(
        dl,
        "deflateBytesBuffer",
        "(J[BIIJIII)J",
        defl_deflate_bytes_buffer,
        NativeKind::Bridge,
    );
    r.register_with_kind(
        dl,
        "deflateBufferBytes",
        "(JJI[BIIII)J",
        defl_deflate_buffer_bytes,
        NativeKind::Bridge,
    );
    r.register_with_kind(
        dl,
        "deflateBufferBuffer",
        "(JJIJIII)J",
        defl_deflate_buffer_buffer,
        NativeKind::Bridge,
    );
    r.register_with_kind(dl, "getAdler", "(J)I", defl_get_adler, NativeKind::Bridge);
    r.register_with_kind(dl, "reset", "(J)V", defl_reset, NativeKind::Bridge);
    r.register_with_kind(dl, "end", "(J)V", defl_end, NativeKind::Bridge);

    // Note: java.util.zip.ZipFile and ZipFile$Source have NO native methods in
    // JDK 25 — the central-directory parser is pure Java backed by
    // RandomAccessFile. Nothing to register here.

    // CRC32 — covers the JDK 25 `java.util.zip.CRC32` natives. The
    // phases_early synthetic registers Java-level `update`/`getValue` against
    // a synthetic 1-field layout; here we additionally cover the *real-JDK*
    // private natives that the actual JDK class file delegates to. Mindustry
    // and any app reading JARs trips `updateBytes0` during entry verification.
    let crc = "java/util/zip/CRC32";
    r.register_with_kind(crc, "update", "(II)I", crc32_update, NativeKind::Bridge);
    // `updateBytes` is concrete real-JDK bytecode that only checks its range
    // then delegates to updateBytes0. Force its registered implementation in
    // real-JDK mode so archive writers never compile a second, incompatible
    // CRC-state transition around the native boundary.
    r.register(crc, "updateBytes", "(I[BII)I", crc32_update_bytes_0);
    r.register_with_kind(crc, "updateBytes0", "(I[BII)I", crc32_update_bytes_0, NativeKind::Bridge);
    r.register_with_kind(
        crc,
        "updateByteBuffer0",
        "(IJII)I",
        crc32_update_byte_buffer_0,
        NativeKind::Bridge,
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use flate2::{write::DeflateEncoder, Compression};
    use std::io::Write;

    /// Unpacks a `Deflater.deflate*` result long into
    /// `(inputConsumed, outputConsumed, finished)`.
    fn unpack_deflate_result(packed: i64) -> (usize, usize, bool) {
        let p = packed as u64;
        (
            (p & 0x7FFF_FFFF) as usize,
            ((p >> 31) & 0x7FFF_FFFF) as usize,
            (p >> 62) & 1 == 1,
        )
    }

    fn assert_decompresses_to(compressed: &[u8], original: &[u8]) {
        let mut decomp = Decompress::new(false);
        let mut out = vec![0u8; 1024];
        let status = decomp
            .decompress(compressed, &mut out, FlushDecompress::Finish)
            .expect("decompress ok");
        let produced = decomp.total_out() as usize;
        assert_eq!(&out[..produced], original);
        assert!(matches!(status, flate2::Status::StreamEnd));
    }

    fn new_deflater(ctx: &mut dyn NativeContext) -> i64 {
        match defl_init(ctx, &[Value::Int(6), Value::Int(0), Value::Int(1)])
            .unwrap()
            .unwrap()
        {
            Value::Long(a) => a,
            other => panic!("expected Long handle, got {other:?}"),
        }
    }

    /// Regression guard for the direct-ByteBuffer `Deflater.deflate*`
    /// overloads (`deflateBytesBuffer`/`deflateBufferBytes`/
    /// `deflateBufferBuffer`): these used to throw `NotImplemented`, then
    /// were implemented for real (see `defl_deflate_bytes_buffer`'s doc
    /// comment for the Jetty `GzipHttpOutputInterceptor` bug this fixed).
    /// Exercise all three overloads end-to-end and confirm the compressed
    /// output actually decompresses back to the original input, rather than
    /// just checking that *some* non-error long comes back.
    #[test]
    fn direct_buffer_deflate_paths_produce_correct_output() {
        let mut ctx = mock_ctx();
        let original = b"hello hello hello hello hello world world world";

        // deflateBytesBuffer: heap byte[] input, direct ByteBuffer output.
        let addr = new_deflater(&mut ctx);
        let input_arr = ctx.new_array(ArrayElementType::Byte, original.len());
        for (i, b) in original.iter().enumerate() {
            ctx.set_array_element(input_arr, i, Value::Int(*b as i32));
        }
        let mut output_buf = vec![0u8; 1024];
        let output_addr = output_buf.as_mut_ptr() as i64;
        let packed = match defl_deflate_bytes_buffer(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Object(Some(input_arr)),
                Value::Int(0),
                Value::Int(original.len() as i32),
                Value::Long(output_addr),
                Value::Int(1024),
                Value::Int(4), // FINISH
                Value::Int(0),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Long(p) => p,
            other => panic!("expected Long, got {other:?}"),
        };
        let (input_consumed, output_consumed, finished) = unpack_deflate_result(packed);
        assert_eq!(input_consumed, original.len());
        assert!(output_consumed > 0, "deflateBytesBuffer produced no output");
        assert!(
            finished,
            "deflateBytesBuffer must report finished on FINISH"
        );
        assert_decompresses_to(&output_buf[..output_consumed], original);

        // deflateBufferBytes: direct ByteBuffer input, heap byte[] output.
        let addr = new_deflater(&mut ctx);
        let mut input_buf = original.to_vec();
        let input_addr = input_buf.as_mut_ptr() as i64;
        let output_arr = ctx.new_array(ArrayElementType::Byte, 1024);
        let packed = match defl_deflate_buffer_bytes(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Long(input_addr),
                Value::Int(original.len() as i32),
                Value::Object(Some(output_arr)),
                Value::Int(0),
                Value::Int(1024),
                Value::Int(4), // FINISH
                Value::Int(0),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Long(p) => p,
            other => panic!("expected Long, got {other:?}"),
        };
        let (input_consumed, output_consumed, finished) = unpack_deflate_result(packed);
        assert_eq!(input_consumed, original.len());
        assert!(output_consumed > 0, "deflateBufferBytes produced no output");
        assert!(
            finished,
            "deflateBufferBytes must report finished on FINISH"
        );
        let compressed = read_byte_array(&ctx, output_arr, 0, output_consumed);
        assert_decompresses_to(&compressed, original);

        // deflateBufferBuffer: both input and output are direct ByteBuffers.
        let addr = new_deflater(&mut ctx);
        let mut input_buf = original.to_vec();
        let input_addr = input_buf.as_mut_ptr() as i64;
        let mut output_buf = vec![0u8; 1024];
        let output_addr = output_buf.as_mut_ptr() as i64;
        let packed = match defl_deflate_buffer_buffer(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Long(input_addr),
                Value::Int(original.len() as i32),
                Value::Long(output_addr),
                Value::Int(1024),
                Value::Int(4), // FINISH
                Value::Int(0),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Long(p) => p,
            other => panic!("expected Long, got {other:?}"),
        };
        let (input_consumed, output_consumed, finished) = unpack_deflate_result(packed);
        assert_eq!(input_consumed, original.len());
        assert!(
            output_consumed > 0,
            "deflateBufferBuffer produced no output"
        );
        assert!(
            finished,
            "deflateBufferBuffer must report finished on FINISH"
        );
        assert_decompresses_to(&output_buf[..output_consumed], original);
    }

    /// Regression guard for the three direct-ByteBuffer `Inflater` overloads.
    #[test]
    fn direct_buffer_inflate_paths_produce_correct_output() {
        let original = b"hello hello hello hello hello world world world";
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(original).unwrap();
        let compressed = enc.finish().unwrap();

        fn unpack(packed: i64) -> (usize, usize, bool, bool) {
            let p = packed as u64;
            (
                (p & 0x7FFF_FFFF) as usize,
                ((p >> 31) & 0x7FFF_FFFF) as usize,
                (p >> 62) & 1 == 1,
                (p >> 63) & 1 == 1,
            )
        }
        fn new_inflater(ctx: &mut dyn NativeContext) -> i64 {
            match infl_init(ctx, &[Value::Int(1)]).unwrap().unwrap() {
                Value::Long(addr) => addr,
                other => panic!("expected Long handle, got {other:?}"),
            }
        }
        fn packed(result: MethodCallResult) -> i64 {
            match result.unwrap().unwrap() {
                Value::Long(value) => value,
                other => panic!("expected Long result, got {other:?}"),
            }
        }

        let mut ctx = mock_ctx();
        let addr = new_inflater(&mut ctx);
        let input_arr = ctx.new_array(ArrayElementType::Byte, compressed.len());
        for (i, b) in compressed.iter().enumerate() {
            ctx.set_array_element(input_arr, i, Value::Int(*b as i8 as i32));
        }
        let mut output = vec![0u8; 1024];
        let (used_in, used_out, finished, need_dict) = unpack(packed(infl_inflate_bytes_buffer(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Object(Some(input_arr)),
                Value::Int(0),
                Value::Int(compressed.len() as i32),
                Value::Long(output.as_mut_ptr() as i64),
                Value::Int(output.len() as i32),
            ],
        )));
        assert_eq!(used_in, compressed.len());
        assert_eq!(&output[..used_out], original);
        assert!(finished && !need_dict);

        let addr = new_inflater(&mut ctx);
        let mut input = compressed.clone();
        let output_arr = ctx.new_array(ArrayElementType::Byte, 1024);
        let (used_in, used_out, finished, need_dict) = unpack(packed(infl_inflate_buffer_bytes(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Long(input.as_mut_ptr() as i64),
                Value::Int(input.len() as i32),
                Value::Object(Some(output_arr)),
                Value::Int(0),
                Value::Int(1024),
            ],
        )));
        assert_eq!(used_in, compressed.len());
        assert_eq!(read_byte_array(&ctx, output_arr, 0, used_out), original);
        assert!(finished && !need_dict);

        let addr = new_inflater(&mut ctx);
        let mut input = compressed.clone();
        let mut output = vec![0u8; 1024];
        let (used_in, used_out, finished, need_dict) = unpack(packed(infl_inflate_buffer_buffer(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Long(input.as_mut_ptr() as i64),
                Value::Int(input.len() as i32),
                Value::Long(output.as_mut_ptr() as i64),
                Value::Int(output.len() as i32),
            ],
        )));
        assert_eq!(used_in, compressed.len());
        assert_eq!(&output[..used_out], original);
        assert!(finished && !need_dict);
    }

    #[test]
    fn round_trip_inflate_raw() {
        let original = b"hello hello hello hello hello world world world";

        // Produce raw DEFLATE (no zlib header) of the payload.
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(original).unwrap();
        let compressed = enc.finish().unwrap();

        // init with nowrap=true (raw deflate).
        let mut decomp = Decompress::new(false);
        let mut out = vec![0u8; 1024];
        let status = decomp
            .decompress(&compressed, &mut out, FlushDecompress::Finish)
            .expect("decompress ok");
        let produced = decomp.total_out() as usize;
        assert_eq!(&out[..produced], original);
        assert!(matches!(status, flate2::Status::StreamEnd));
    }

    #[test]
    fn crc32_matches_known_vectors() {
        // Empty input — running state starts at 0xFFFFFFFF (the inner step
        // takes the complemented form), no bytes processed, result equals
        // the input.
        assert_eq!(crc32_step(0xFFFF_FFFF, b""), 0xFFFF_FFFF);

        // "123456789" — classic CRC-32/IEEE check vector is 0xCBF43926
        // (final, complemented). Internally the running state at the end is
        // !0xCBF43926 = 0x340BC6D9.
        let running = crc32_step(0xFFFF_FFFF, b"123456789");
        assert_eq!(!running, 0xCBF4_3926);

        // "abc" — CRC-32/IEEE = 0x352441C2.
        let running = crc32_step(0xFFFF_FFFF, b"abc");
        assert_eq!(!running, 0x3524_41C2);
    }

    /// `crc32_update_public` matches the JDK `CRC32.update*` contract:
    /// public CRC in, public CRC out (initial state 0, no caller-side
    /// complementation). Regression guard for the DaCapo `avrora`
    /// "invalid entry CRC" crash where the native was treating the
    /// JDK-public `int crc` field as the complemented running state.
    #[test]
    fn crc32_public_contract_matches_jdk() {
        // Fresh CRC32 starts at 0; getValue() must return 0 (matches real JDK).
        assert_eq!(crc32_update_public(0, b""), 0);

        // "123456789" → 0xCBF43926 (canonical IEEE check vector, also what
        // real JDK 25 java.util.zip.CRC32 returns).
        assert_eq!(crc32_update_public(0, b"123456789"), 0xCBF4_3926);

        // "abc" → 0x352441C2.
        assert_eq!(crc32_update_public(0, b"abc"), 0x3524_41C2);

        // Chained call must equal one-shot.
        let half = crc32_update_public(0, b"1234");
        assert_eq!(crc32_update_public(half, b"56789"), 0xCBF4_3926);
    }

    #[test]
    fn pack_layout_matches_jdk_unpacking() {
        // JDK unpacks:
        //   inputConsumed  = (packed & 0x7FFFFFFF)
        //   outputConsumed = ((packed >>> 31) & 0x7FFFFFFF)
        //   finished       = ((packed >>> 62) & 1) != 0
        //   needDict       = ((packed >>> 63) & 1) != 0
        let p = pack_inflate_result(42, 1000, true, false) as u64;
        assert_eq!(p & 0x7FFF_FFFF, 42);
        assert_eq!((p >> 31) & 0x7FFF_FFFF, 1000);
        assert_eq!((p >> 62) & 1, 1);
        assert_eq!((p >> 63) & 1, 0);

        let p2 = pack_inflate_result(0x7FFF_FFFF, 0x7FFF_FFFF, false, true) as u64;
        assert_eq!(p2 & 0x7FFF_FFFF, 0x7FFF_FFFF);
        assert_eq!((p2 >> 31) & 0x7FFF_FFFF, 0x7FFF_FFFF);
        assert_eq!((p2 >> 62) & 1, 0);
        assert_eq!((p2 >> 63) & 1, 1);
    }

    #[test]
    fn deflater_matches_hotspot_for_a_large_json_string() {
        let mut body = Vec::with_capacity(10_002);
        body.push(b'[');
        body.extend(std::iter::repeat_n(b'a', 10_000));
        body.push(b']');

        let actual = defl_compress_finished(&body, 6, false)
            .expect("raw deflate compression should succeed");
        let expected = [
            0xed, 0xc1, 0x31, 0x0d, 0x00, 0x00, 0x0c, 0x03, 0x20, 0xa1, 0x4b, 0x8f, 0xf9, 0x37,
            0x51, 0x1f, 0x0d, 0x70, 0x0f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x03, 0x52,
        ];
        assert_eq!(actual, expected);
    }

    /// Regression guard for the Jetty `compressionOfResponseToGetRequest`
    /// hang (`jetty-webserver-factory-poststartup-timeout-and-reflective-
    /// supertype-residuals.md`): a mid-stream `SYNC_FLUSH` (JDK flush code
    /// 1) must produce real compressed bytes immediately instead of being
    /// silently buffered until `FINISH`. A streaming gzip writer that waits
    /// for a flush to make progress before handing over more input would
    /// hang forever against the old buffer-until-FINISH implementation.
    #[test]
    fn deflate_sync_flush_produces_output_without_finish() {
        let mut ctx = mock_ctx();

        // nowrap=true (raw deflate, no zlib header) to match `Decompress::new(false)` below.
        let addr = match defl_init(&mut ctx, &[Value::Int(6), Value::Int(0), Value::Int(1)])
            .unwrap()
            .unwrap()
        {
            Value::Long(a) => a,
            other => panic!("expected Long handle, got {other:?}"),
        };

        let original = b"hello hello hello hello hello world world world";
        let input_arr = ctx.new_array(ArrayElementType::Byte, original.len());
        for (i, b) in original.iter().enumerate() {
            ctx.set_array_element(input_arr, i, Value::Int(*b as i32));
        }
        let output_arr = ctx.new_array(ArrayElementType::Byte, 1024);

        let packed = match defl_deflate_bytes_bytes(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Object(Some(input_arr)),
                Value::Int(0),
                Value::Int(original.len() as i32),
                Value::Object(Some(output_arr)),
                Value::Int(0),
                Value::Int(1024),
                Value::Int(1), // SYNC_FLUSH
                Value::Int(0),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Long(p) => p as u64,
            other => panic!("expected Long, got {other:?}"),
        };
        let input_consumed = (packed & 0x7FFF_FFFF) as usize;
        let output_consumed = ((packed >> 31) & 0x7FFF_FFFF) as usize;
        assert_eq!(input_consumed, original.len());
        assert!(
            output_consumed > 0,
            "SYNC_FLUSH must flush compressed bytes immediately, not defer to FINISH"
        );

        let mut compressed = Vec::new();
        for i in 0..output_consumed {
            match ctx.get_array_element(output_arr, i) {
                Value::Int(v) => compressed.push(v as u8),
                other => panic!("expected byte, got {other:?}"),
            }
        }

        // Finish the stream with no further input and collect the tail.
        let empty_arr = ctx.new_array(ArrayElementType::Byte, 0);
        let output_arr2 = ctx.new_array(ArrayElementType::Byte, 1024);
        let packed2 = match defl_deflate_bytes_bytes(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Object(Some(empty_arr)),
                Value::Int(0),
                Value::Int(0),
                Value::Object(Some(output_arr2)),
                Value::Int(0),
                Value::Int(1024),
                Value::Int(4), // FINISH
                Value::Int(0),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Long(p) => p as u64,
            other => panic!("expected Long, got {other:?}"),
        };
        let output_consumed2 = ((packed2 >> 31) & 0x7FFF_FFFF) as usize;
        let finished2 = (packed2 >> 62) & 1 == 1;
        assert!(finished2, "FINISH flush must report stream completion");
        for i in 0..output_consumed2 {
            match ctx.get_array_element(output_arr2, i) {
                Value::Int(v) => compressed.push(v as u8),
                other => panic!("expected byte, got {other:?}"),
            }
        }

        let mut decomp = Decompress::new(false);
        let mut out = vec![0u8; 1024];
        let status = decomp
            .decompress(&compressed, &mut out, FlushDecompress::Finish)
            .expect("decompress ok");
        let produced = decomp.total_out() as usize;
        assert_eq!(&out[..produced], &original[..]);
        assert!(matches!(status, flate2::Status::StreamEnd));
    }

    /// Real `Deflater.deflate()` is documented as a no-op once FINISH has
    /// produced `Z_STREAM_END`: callers may call it again before `reset()`
    /// and must see 0 bytes consumed/produced with `finished` still true.
    /// Re-entering zlib past `Z_STREAM_END` is not part of its contract;
    /// guard against it explicitly rather than relying on the backend's
    /// behavior for an out-of-contract call.
    #[test]
    fn deflate_after_finish_is_a_no_op_not_a_reentry() {
        let mut ctx = mock_ctx();
        let addr = match defl_init(&mut ctx, &[Value::Int(6), Value::Int(0), Value::Int(1)])
            .unwrap()
            .unwrap()
        {
            Value::Long(a) => a,
            other => panic!("expected Long handle, got {other:?}"),
        };

        let input_arr = ctx.new_array(ArrayElementType::Byte, 0);
        let output_arr = ctx.new_array(ArrayElementType::Byte, 64);
        let finish_args = |input, output| {
            vec![
                Value::Object(None),
                Value::Long(addr),
                Value::Object(Some(input)),
                Value::Int(0),
                Value::Int(0),
                Value::Object(Some(output)),
                Value::Int(0),
                Value::Int(64),
                Value::Int(4), // FINISH
                Value::Int(0),
            ]
        };

        let first = match defl_deflate_bytes_bytes(&mut ctx, &finish_args(input_arr, output_arr))
            .unwrap()
            .unwrap()
        {
            Value::Long(p) => p as u64,
            other => panic!("expected Long, got {other:?}"),
        };
        assert_eq!(
            (first >> 62) & 1,
            1,
            "first FINISH call must report finished"
        );

        // Calling deflate() again after finished must stay a clean no-op —
        // not an error, not a re-entry into zlib.
        let output_arr2 = ctx.new_array(ArrayElementType::Byte, 64);
        let second = match defl_deflate_bytes_bytes(&mut ctx, &finish_args(input_arr, output_arr2))
            .unwrap()
            .unwrap()
        {
            Value::Long(p) => p as u64,
            other => panic!("expected Long, got {other:?}"),
        };
        assert_eq!(
            second & 0x7FFF_FFFF,
            0,
            "post-finish call must consume no input"
        );
        assert_eq!(
            (second >> 31) & 0x7FFF_FFFF,
            0,
            "post-finish call must produce no output"
        );
        assert_eq!(
            (second >> 62) & 1,
            1,
            "post-finish call must still report finished"
        );
    }
}
