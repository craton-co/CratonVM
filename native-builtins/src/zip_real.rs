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

use cratonvm_native_api::{NativeCallback, NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};
#[cfg(test)]
use flate2::{Compress, FlushCompress};
use flate2::{Decompress, FlushDecompress};

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
    /// (1), reset back to 1 by `reset(long)`. Stays 0 (and is never
    /// updated) when `track_adler` is false.
    adler: u32,
    /// Whether `adler` is maintained at all. Always for a zlib-wrapped
    /// stream; for a raw (`nowrap`) one only while
    /// `CRATONVM_INFLATER_RAW_ADLER_ZERO` is off (read once, when the stream
    /// is created). zlib keeps `strm->adler` only for a wrapped stream
    /// (`inflate()`'s `state->wrap & 4` update, `inflateResetKeep`'s
    /// `if (state->wrap)` seed) and the JDK `calloc`s the `z_stream`, so
    /// HotSpot's raw inflater answers `getAdler()` = 0 before, during and
    /// after a stream, and after `reset()`; with the switch this one does
    /// too, and skips the per-byte checksum over everything a `ZipFile`
    /// entry read inflates (interpreter round i3 wave 74, lane L5;
    /// `docs/internal/fixed-bugs/interpreter-i73-L5-inflater-getadler-differs-from-zlib-FIXED-20261111.md`,
    /// case 1).
    track_adler: bool,
    /// `CRATONVM_INFLATER_REUSE_BUFFERS` (interpreter round i3 wave 73, lane
    /// L5): the input copy and the inflate target of `inflateBytesBytes`,
    /// kept with the stream so a `ZipFile` entry read (three or four calls of
    /// up to 16 KiB each, on an `Inflater` the `ZipFile` caches and `reset`s)
    /// allocates and zeroes nothing per call. Grown on demand up to
    /// [`INFLATER_SCRATCH_CAP`]; a larger call uses one-off buffers. Empty
    /// (no allocation) while the switch is off.
    scratch_in: Vec<u8>,
    scratch_out: Vec<u8>,
    /// A preset dictionary was accepted since the last step (wave 79, L5; i73
    /// page case 3). zlib answers `Z_NEED_DICT` with `strm->adler` = the
    /// stream's DICTID (what `Inflater.getAdler()` must then report, so the
    /// caller can pick the dictionary), keeps it through
    /// `inflateSetDictionary`, and re-seeds it to 1 in the next `inflate`
    /// call's `DICT` state before any output: [`InflaterState::before_step`].
    dict_resume: bool,
}

impl InflaterState {
    /// Run before every zlib step: the `DICT`-state re-seed of `strm->adler`
    /// after a dictionary was set (see `dict_resume`).
    fn before_step(&mut self) {
        if std::mem::take(&mut self.dict_resume) && self.track_adler {
            self.adler = 1;
        }
    }
}

/// The largest scratch buffer an [`InflaterState`] keeps between calls, each
/// way. `InputStream.readNBytes` asks for 16 KiB at a time and a `ZipFile`'s
/// inflater input buffer is at most 64 KiB, so this covers the entry reads the
/// switch is for without letting one `inflate(new byte[1 << 30])` pin a
/// gigabyte to a cached `Inflater`.
const INFLATER_SCRATCH_CAP: usize = 64 * 1024;

/// One `java.util.zip.Deflater` stream: zlib's own `z_stream`, owned the way
/// the JDK's `Deflater.c` owns it (interpreter round i3 wave 79, lane L5).
///
/// It used to be a `flate2::Compress`, which cannot pass a strategy:
/// `Compress::set_level` is `deflateParams(strm, level, Z_DEFAULT_STRATEGY)`,
/// so `setStrategy(HUFFMAN_ONLY | FILTERED)` was dropped, a `Z_BUF_ERROR` from
/// it was ignored (the change lost), and the same call then compressed --
/// where HotSpot's `doDeflate` runs ONLY `deflateParams` in a parameter call
/// and reports bit 63 to keep `setParams` on `Z_BUF_ERROR`
/// (`docs/internal/fixed-bugs/interpreter-i75-L5-deflater-params-ignore-the-strategy-and-deflate-in-the-same-call-FIXED-20261111.md`).
/// With the raw stream every call is HotSpot's: `deflateInit2(level,
/// Z_DEFLATED, nowrap ? -15 : 15, 8, strategy)`, `deflateParams` or
/// `deflate`, `deflateSetDictionary`, `deflateReset`, and `getAdler()` is
/// `strm->adler` (1 forever for a raw stream, the dictionary's id after
/// `setDictionary`; i73 page case 2).
///
/// The `z_stream` is boxed because zlib's internal state points back at it
/// (`deflateStateCheck`'s `s->strm != strm`); the box never moves.
struct DeflaterState {
    strm: Box<libz_sys::z_stream>,
    // Real JDK `Deflater.deflate()` is idempotent once FINISH has produced
    // `Z_STREAM_END`: further calls are a documented no-op (0 bytes
    // consumed/produced, finished stays true) until `reset()`. Tracked
    // explicitly and short-circuited instead of re-entering zlib.
    finished: bool,
}

// SAFETY: the `z_stream` and the zlib state it owns are reached only through
// `deflater_table()`'s mutex; its `next_in` / `next_out` pointers are set
// before, and cleared after, each zlib call made under that lock.
unsafe impl Send for DeflaterState {}

impl Drop for DeflaterState {
    fn drop(&mut self) {
        // SAFETY: `strm` was initialised by `deflateInit2_` (a state that
        // failed to initialise is never constructed).
        unsafe {
            libz_sys::deflateEnd(&mut *self.strm);
        }
    }
}

/// zlib's allocator hooks for [`DeflaterState`] (`z_stream.zalloc` / `zfree`
/// are non-nullable function pointers in `libz-sys`, so zlib's own default
/// cannot be selected with a null). `calloc` / `free`, as zlib's `zcalloc`.
extern "C" fn defl_zalloc(
    _opaque: libz_sys::voidpf,
    items: libz_sys::uInt,
    size: libz_sys::uInt,
) -> libz_sys::voidpf {
    // SAFETY: plain C allocation; zlib checks the result for null.
    unsafe { libc::calloc(items as libc::size_t, size as libc::size_t) as libz_sys::voidpf }
}

extern "C" fn defl_zfree(_opaque: libz_sys::voidpf, address: libz_sys::voidpf) {
    // SAFETY: `address` came from `defl_zalloc`.
    unsafe { libc::free(address as *mut libc::c_void) }
}

impl DeflaterState {
    /// `Deflater.c`'s `init`: `deflateInit2(strm, level, Z_DEFLATED, nowrap ?
    /// -MAX_WBITS : MAX_WBITS, DEF_MEM_LEVEL, strategy)`. `None` when zlib
    /// refuses (out of memory, or a level / strategy it rejects).
    fn new(level: i32, strategy: i32, nowrap: bool) -> Option<Self> {
        let mut strm = Box::new(libz_sys::z_stream {
            next_in: std::ptr::null_mut(),
            avail_in: 0,
            total_in: 0,
            next_out: std::ptr::null_mut(),
            avail_out: 0,
            total_out: 0,
            msg: std::ptr::null_mut(),
            state: std::ptr::null_mut(),
            zalloc: defl_zalloc,
            zfree: defl_zfree,
            opaque: std::ptr::null_mut(),
            data_type: 0,
            adler: 0,
            reserved: 0,
        });
        let window_bits = if nowrap { -15 } else { 15 };
        // SAFETY: `strm` is a fresh, boxed stream with valid allocator hooks.
        let rc = unsafe {
            libz_sys::deflateInit2_(
                &mut *strm,
                level,
                libz_sys::Z_DEFLATED,
                window_bits,
                8,
                strategy,
                libz_sys::zlibVersion(),
                std::mem::size_of::<libz_sys::z_stream>() as std::os::raw::c_int,
            )
        };
        if rc != libz_sys::Z_OK {
            return None;
        }
        Some(Self {
            strm,
            finished: false,
        })
    }

    /// `Deflater.c`'s `doDeflate` + `checkDeflateStatus`: with `params & 1`
    /// ONLY `deflateParams(strm, params >> 3, (params >> 1) & 3)`, reporting
    /// what it consumed and produced and `params_pending` (bit 63) on
    /// `Z_BUF_ERROR`; otherwise `deflate(strm, flush)`. Returns `(input used,
    /// output used, finished, params_pending)`, or zlib's verdict for an
    /// error (`Err(message)`, HotSpot's `InternalError`).
    fn step(
        &mut self,
        params: i32,
        flush: i32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(u32, u32, bool, bool), String> {
        let in_len = input.len().min(u32::MAX as usize) as u32;
        let out_len = output.len().min(u32::MAX as usize) as u32;
        let strm = &mut *self.strm;
        // zlib never writes through `next_in` (it is `z_const` in zlib.h).
        strm.next_in = input.as_ptr() as *mut u8;
        strm.avail_in = in_len;
        strm.next_out = output.as_mut_ptr();
        strm.avail_out = out_len;
        strm.msg = std::ptr::null_mut();
        let set_params = params & 1 != 0;
        // SAFETY: the stream is initialised; `next_in` / `next_out` cover
        // `avail_in` / `avail_out` bytes of live slices for the call.
        let raw: *mut libz_sys::z_stream = &mut *strm;
        let rc = unsafe {
            if set_params {
                libz_sys::deflateParams(raw, params >> 3, (params >> 1) & 3)
            } else {
                libz_sys::deflate(raw, flush)
            }
        };
        let used_in = in_len - strm.avail_in;
        let used_out = out_len - strm.avail_out;
        let msg = if strm.msg.is_null() {
            None
        } else {
            // SAFETY: zlib's messages are static NUL-terminated strings.
            Some(
                unsafe { std::ffi::CStr::from_ptr(strm.msg) }
                    .to_string_lossy()
                    .into_owned(),
            )
        };
        strm.next_in = std::ptr::null_mut();
        strm.avail_in = 0;
        strm.next_out = std::ptr::null_mut();
        strm.avail_out = 0;
        if set_params {
            match rc {
                libz_sys::Z_OK => Ok((used_in, used_out, false, false)),
                libz_sys::Z_BUF_ERROR => Ok((used_in, used_out, false, true)),
                _ => Err("deflateParams failed".to_string()),
            }
        } else {
            match rc {
                libz_sys::Z_STREAM_END => {
                    self.finished = true;
                    Ok((used_in, used_out, true, false))
                }
                libz_sys::Z_OK => Ok((used_in, used_out, false, false)),
                libz_sys::Z_BUF_ERROR => Ok((0, 0, false, false)),
                _ => Err(msg.unwrap_or_else(|| format!("deflate failed ({rc})"))),
            }
        }
    }

    /// `deflateSetDictionary`; zlib's return code.
    fn set_dictionary(&mut self, dict: &[u8]) -> i32 {
        // SAFETY: initialised stream; zlib copies the dictionary.
        unsafe {
            libz_sys::deflateSetDictionary(
                &mut *self.strm,
                dict.as_ptr(),
                dict.len().min(u32::MAX as usize) as libz_sys::uInt,
            )
        }
    }

    /// `deflateReset`.
    fn reset(&mut self) {
        // SAFETY: initialised stream.
        unsafe {
            libz_sys::deflateReset(&mut *self.strm);
        }
        self.finished = false;
    }

    /// `Deflater.getAdler()`: zlib's `strm->adler`, as `Deflater.c` returns it.
    fn adler(&self) -> u32 {
        self.strm.adler as u32
    }
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

/// The number of shards of the inflater handle table. Shard 0 holds every
/// handle minted with `CRATONVM_INFLATER_SHARDED_TABLE` off -- exactly the
/// one table, and the one lock, those VMs always had -- and shards
/// `1..INFLATER_TABLE_SHARDS` hold the handles a VM with the switch on mints
/// ([`inflater_handle_shard`]).
const INFLATER_TABLE_SHARDS: usize = 64;

/// Set in a handle minted under `CRATONVM_INFLATER_SHARDED_TABLE`, so every
/// native finds the handle's shard from the handle alone, without asking the
/// VM (and a VM with the switch off keeps shard 0 to itself). The JDK only
/// ever tests the handle against 0 (`Inflater.ensureOpen`), and the counter
/// never reaches this bit.
const SHARDED_INFLATER_HANDLE_BIT: i64 = 1 << 48;

/// One shard of the inflater handle table.
type InflaterShard = Mutex<HashMap<i64, InflaterState>>;

/// The shard that holds inflater `handle`: 0 for a handle minted with the
/// switch off, else `1 + handle % (INFLATER_TABLE_SHARDS - 1)` (consecutive
/// handles land in different shards; the counter is monotonic).
fn inflater_handle_shard(handle: i64) -> usize {
    if handle & SHARDED_INFLATER_HANDLE_BIT == 0 {
        return 0;
    }
    1 + (handle.unsigned_abs() % (INFLATER_TABLE_SHARDS as u64 - 1)) as usize
}

/// The table shard holding inflater `handle` (zlib state per stream). A
/// single process-wide table with one lock, held across the zlib step, until
/// interpreter round i5 wave 100, lane L5: with
/// `CRATONVM_INFLATER_SHARDED_TABLE` on, threads inflating different streams
/// (parallel class loading out of jars) take different locks, as HotSpot's
/// per-stream `z_stream` has no shared lock at all
/// (`interpreter-i73-L5-proposal-a-sharded-inflater-table-DONE-20261019.md`). Process-wide
/// still, because the handles are plain `long`s Java holds; no compatibility
/// state lives here.
fn inflater_table(handle: i64) -> &'static InflaterShard {
    static T: OnceLock<Box<[InflaterShard]>> = OnceLock::new();
    let shards = T.get_or_init(|| {
        (0..INFLATER_TABLE_SHARDS)
            .map(|_| Mutex::new(HashMap::new()))
            .collect()
    });
    &shards[inflater_handle_shard(handle)]
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

/// `Deflater.c`'s `checkDeflateStatus` packing: bit 63 is `setParams` still
/// pending (a `deflateParams` that answered `Z_BUF_ERROR`), which
/// `Deflater.deflate` reads to keep its `setParams` and retry.
fn pack_deflate_result_params(
    input_consumed: u32,
    output_consumed: u32,
    finished: bool,
    params_pending: bool,
) -> i64 {
    let mut p: u64 = 0;
    p |= (input_consumed as u64) & 0x7FFF_FFFF;
    p |= ((output_consumed as u64) & 0x7FFF_FFFF) << 31;
    if finished {
        p |= 1u64 << 62;
    }
    if params_pending {
        p |= 1u64 << 63;
    }
    p as i64
}

// ---------------------------------------------------------------------------
// Inflater natives
// ---------------------------------------------------------------------------

fn infl_init_ids(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

fn infl_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // static init(boolean nowrap) -> long
    let nowrap = arg_bool(args, 0);
    let handle = inflater_init_in(
        nowrap,
        ctx.inflater_raw_adler_zero(),
        ctx.inflater_sharded_table(),
    );
    Ok(Some(Value::Long(handle)))
}

/// A new stream's handle: `Inflater.init(nowrap)` with the
/// `CRATONVM_INFLATER_RAW_ADLER_ZERO` choice passed in, so the unit tests can
/// create both kinds on a mock context. Unsharded (shard 0).
fn inflater_init(nowrap: bool, raw_adler_zero: bool) -> i64 {
    inflater_init_in(nowrap, raw_adler_zero, false)
}

/// [`inflater_init`], the handle minted for a sharded table slot when
/// `sharded` (`CRATONVM_INFLATER_SHARDED_TABLE`; [`inflater_table`]).
fn inflater_init_in(nowrap: bool, raw_adler_zero: bool, sharded: bool) -> i64 {
    let zlib_header = !nowrap;
    let track_adler = zlib_header || !raw_adler_zero;
    let state = InflaterState {
        decomp: Decompress::new(zlib_header),
        zlib_header,
        adler: inflater_adler_seed(track_adler),
        track_adler,
        scratch_in: Vec::new(),
        scratch_out: Vec::new(),
        dict_resume: false,
    };
    let handle = if sharded {
        next_handle() | SHARDED_INFLATER_HANDLE_BIT
    } else {
        next_handle()
    };
    inflater_table(handle)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(handle, state);
    handle
}

/// What `getAdler()` answers for a fresh or `reset()` stream: zlib's seed 1
/// when the stream keeps a checksum, the `calloc`'d 0 of a raw stream's
/// `strm->adler` when it does not (see `InflaterState::track_adler`).
fn inflater_adler_seed(track_adler: bool) -> u32 {
    u32::from(track_adler)
}

/// `Inflater.setDictionary(long addr, byte[] b, int off, int len)`.
///
/// A zlib PRESET DICTIONARY seeds the LZ77 window before decoding. The stream
/// asks for one by setting `FDICT` in the header's second byte and following it
/// with the dictionary's ADLER-32; zlib then reports `Z_NEED_DICT`, which the
/// JDK surfaces as `Inflater.needsDictionary()`, and the caller is expected to
/// supply the dictionary and resume.
///
/// This used to be a no-op, on the stated grounds that "flate2's
/// `Decompress::set_dictionary` is gated behind a zlib backend feature we don't
/// enable". That was not true: `native-builtins/Cargo.toml` has always built
/// flate2 with `features = ["zlib"]`, which sets flate2's `any_zlib` and
/// compiles `set_dictionary` in. The no-op cost SPDY every compressed header
/// block — `SpdyHeaderBlockZlibDecoder` hands the dictionary over exactly as
/// above, got silence, inflated 0 bytes and reported `Invalid Header Block`.
///
/// A refusal is thrown as HotSpot's `Inflater.c` `checkSetDictionaryResult`
/// throws it: zlib's `Z_STREAM_ERROR` (a zlib-wrapped stream not at its DICT
/// point, e.g. a fresh inflater) and `Z_DATA_ERROR` (a dictionary whose
/// Adler-32 is not the stream's DICTID) are `IllegalArgumentException(strm->msg)`,
/// a `null` message for both (zlib sets none). These refusals used to be
/// swallowed (interpreter round i6 wave 115, lane L5; probe
/// `L5W115ZipDictionaryAndDataErrors`): the JDK's `setDictionary` then cleared
/// `needDict` as if the dictionary had been taken.
fn infl_set_dictionary(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // static setDictionary(long, byte[], int off, int len)
    let addr = arg_long(args, 0);
    let Some(arr) = arg_obj(args, 1) else {
        return Ok(None);
    };
    let off = arg_int(args, 2).max(0) as usize;
    let len = arg_int(args, 3).max(0) as usize;
    let dict = read_byte_array(ctx, arr, off, len);
    let refusal = infl_apply_dictionary(addr, &dict);
    dictionary_refusal(ctx, refusal)
}

/// Hand `dict` to the inflater at `addr` (the table's lock held only for the
/// zlib call). `Some(message)` when zlib refused it (`message` is zlib's,
/// usually `None`); `None` when it was taken or `addr` names no stream.
fn infl_apply_dictionary(addr: i64, dict: &[u8]) -> Option<Option<String>> {
    let mut tbl = inflater_table(addr).lock().unwrap_or_else(|e| e.into_inner());
    let st = tbl.get_mut(&addr)?;
    match st.decomp.set_dictionary(dict) {
        Ok(_) => {
            st.dict_resume = true;
            None
        }
        Err(e) => Some(e.message().map(str::to_owned)),
    }
}

/// `checkSetDictionaryResult`'s throw (`Inflater.c`, `Deflater.c`): a zlib
/// refusal of a preset dictionary is `IllegalArgumentException` with zlib's
/// message, or a `null` one. Run with no table lock held (it runs `<init>`).
fn dictionary_refusal(
    ctx: &mut dyn NativeContext,
    refusal: Option<Option<String>>,
) -> MethodCallResult {
    let Some(message) = refusal else {
        return Ok(None);
    };
    let detail = message.map(|m| ctx.create_string(&m));
    let built = match detail {
        Some(s) => ctx.new_object_initialized(
            "java/lang/IllegalArgumentException",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(s))],
        ),
        None => ctx.new_object_initialized("java/lang/IllegalArgumentException", "()V", &[]),
    };
    match built {
        Ok(Some(Value::Object(Some(exc)))) => Err(MethodCallFailed::ExceptionThrown(exc)),
        Err(e) => Err(e),
        _ => Err(RuntimeError::IllegalArgumentException {
            message: String::new(),
        }
        .into()),
    }
}

/// Read a preset dictionary out of a direct `ByteBuffer`'s native memory.
///
/// Shared by both `setDictionaryBuffer` natives. A buffer address that cannot
/// be read is reported as `IllegalArgumentException`, which is UNCHECKED and is
/// the exception `setDictionary` already documents — the sibling `inflate*`
/// natives raise `IOException` for the same fault, but they are declared
/// `throws DataFormatException` and `setDictionary*` is declared to throw
/// nothing, so a checked throwable out of this frame would escape every
/// caller's `catch`.
fn read_direct_dictionary(
    ctx: &mut dyn NativeContext,
    buf_addr: i64,
    len: usize,
) -> Result<Vec<u8>, MethodCallFailed> {
    let mut dict = vec![0u8; len];
    if len > 0 && !ctx.copy_from_native_memory(buf_addr, &mut dict) {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!(
                "setDictionaryBuffer: invalid dictionary buffer address {buf_addr:#x}"
            ),
        }
        .into());
    }
    Ok(dict)
}

/// `Inflater.setDictionaryBuffer(long addr, long bufferAddress, int len)` — the
/// direct-`ByteBuffer` flavour of [`infl_set_dictionary`], reached from
/// `Inflater.setDictionary(ByteBuffer)` when the buffer is direct.
///
/// This was a no-op for a stated reason that had gone stale in the same way the
/// heap overload's had: "the direct-buffer inflate natives are themselves
/// unsupported (`infl_direct_buffer_unsupported`), so a caller cannot get far
/// enough to need this". There is no `infl_direct_buffer_unsupported` — all
/// four `inflate*` overloads (and all four `deflate*` overloads) read and write
/// direct-buffer memory for real. So a caller CAN get there, and did: an app
/// that inflates a preset-dictionary stream into a direct `ByteBuffer` hit
/// exactly the SPDY failure the heap overload was fixed for, with the
/// justification for the gap pointing at a function that does not exist.
///
/// Verify the premise, not the comment: `deflate_dictDirectBuffer_len` is 18
/// bytes against HotSpot's 18 and the no-dictionary encoder's 53.
fn infl_set_dictionary_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // static setDictionaryBuffer(long addr, long bufferAddress, int len)
    let addr = arg_long(args, 0);
    let buf_addr = arg_long(args, 1);
    let len = arg_int(args, 2).max(0) as usize;
    let dict = read_direct_dictionary(ctx, buf_addr, len)?;
    // A refusal is thrown as the heap overload throws it.
    let refusal = infl_apply_dictionary(addr, &dict);
    dictionary_refusal(ctx, refusal)
}

// ---------------------------------------------------------------------------
// Inflate error reporting
// ---------------------------------------------------------------------------
//
// All four `Inflater.inflate*` natives are declared
// `throws java.util.zip.DataFormatException` (`javap -p java.util.zip.Inflater`
// on JDK 25 shows the `throws` clause on every one of them). That is not
// decoration: libzip's JNI entry point maps zlib's `Z_DATA_ERROR` to
// `JNU_ThrowByName(env, "java/util/zip/DataFormatException", strm->msg)`, and
// `Inflater.inflate(byte[],int,int)` has NO Java-level check that turns a
// zero-progress packed result into an exception — its bytecode stores the
// packed long and returns (disassembled to confirm). The ONLY channel for
// "this stream is corrupt" is a throw out of the native frame.
//
// So the pre-existing `Err(e) => (false, e.needs_dictionary().is_some())` arm
// was a fabricated success where the spec mandates a failure: a hard decode
// error became `(0, 0, false, false)`, `inflate()` returned 0, and the caller
// could not tell corrupt input from "needs more input".
// `RJdkJni.zipNatives:165` measures exactly that:
//
//     bad.setInput(new byte[] { 1, 2, 3, 4, 5, 6, 7, 8 });
//     bad.inflate(new byte[64]);            // must throw DataFormatException
//
// (`{1,2,...}` is not a zlib stream: CMF=0x01 carries CM=1 rather than 8, and
// the two-byte header check `(0x01 << 8 | 0x02) % 31 != 0`, so zlib fails on
// the header before producing a byte.)
//
// `Z_NEED_DICT` is deliberately NOT an error: the JDK reports it through the
// `needDict` bit and `Inflater.needsDictionary()`. flate2 signals it as an
// `Err` whose `needs_dictionary()` is `Some(adler)`, which is why the two are
// split apart below rather than both being treated as failures.

/// The outcome of one `flate2::Decompress::decompress` step, shared by all
/// four `Inflater.inflate*` overloads.
struct InflateStep {
    input_consumed: u32,
    output_consumed: u32,
    finished: bool,
    need_dict: bool,
    /// `Some(zlib message)` when zlib reported a hard stream/data error — the
    /// `Z_DATA_ERROR` case the JDK turns into `DataFormatException`. `None`
    /// for every non-failing status, including `Z_NEED_DICT`.
    data_error: Option<String>,
}

/// Construct and throw a real, catchable `java.util.zip.DataFormatException`.
///
/// It is a CHECKED exception on `Inflater.inflate`, so a Java `catch
/// (DataFormatException)` is the intended handler and nothing else will do —
/// an internal/uncatchable error here would escape the caller's `catch` and
/// abort the whole call chain instead. If the class cannot be constructed we
/// still raise a catchable throwable (an `IOException` naming the fault)
/// rather than reporting success.
fn throw_data_format(ctx: &mut dyn NativeContext, msg: &str) -> MethodCallFailed {
    let detail = ctx.create_string(msg);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        "java/util/zip/DataFormatException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    RuntimeError::IOException {
        message: format!("DataFormatException: {msg}"),
    }
    .into()
}

/// Stash partial progress in the receiver's `inputConsumed`/`outputConsumed`
/// fields before throwing, the way the JDK's JNI code does: the exception
/// handler compiled into `Inflater.inflate` reads them back to advance
/// `inputPos` and `bytesRead`. On the non-throwing path the packed return
/// value carries the same numbers, so this is only needed here. A no-op when
/// the fields do not resolve (a synthetic `Inflater` layout has neither).
fn store_progress_fields(
    ctx: &mut dyn NativeContext,
    this: Option<ObjectRef>,
    input_consumed: u32,
    output_consumed: u32,
) {
    let Some(this) = this else { return };
    let cid = ctx.class_id_of_object(this);
    let slots = ctx.object_num_fields(this);
    // Fail closed on an out-of-range index rather than writing past a
    // stand-in allocated with fewer slots than the real class declares.
    for (name, value) in [
        ("inputConsumed", input_consumed),
        ("outputConsumed", output_consumed),
    ] {
        if let Some(idx) = ctx.resolve_field_index_by_class_id(cid, name) {
            if idx < slots {
                ctx.set_field(this, idx, Value::Int(value as i32));
            }
        }
    }
}

/// Turn a completed [`InflateStep`] into the native's return value: either the
/// packed progress long, or a thrown `DataFormatException`. Callers must have
/// already flushed whatever output bytes were produced — the JDK writes them
/// straight into the caller's buffer inside its critical section, so they are
/// visible on the throwing path too.
fn finish_inflate(
    ctx: &mut dyn NativeContext,
    this: Option<ObjectRef>,
    step: InflateStep,
) -> MethodCallResult {
    // `CRATONVM_DBG=zip-stream-census` (interpreter round i6 wave 116, lane
    // L5): every overload's call, by stream kind and consumer frame.
    crate::zip_stream_census::note_inflate(ctx, step.output_consumed);
    if let Some(msg) = step.data_error {
        store_progress_fields(ctx, this, step.input_consumed, step.output_consumed);
        return Err(throw_data_format(ctx, &msg));
    }
    Ok(Some(Value::Long(pack_inflate_result(
        step.input_consumed,
        step.output_consumed,
        step.finished,
        step.need_dict,
    ))))
}

/// Shared decompression core for all four `Inflater.inflate*` overloads.
///
/// Holds the handle-table mutex for the duration of the zlib step ONLY: the
/// guard must be dropped before any caller re-enters Java (`finish_inflate`
/// allocates and runs `DataFormatException.<init>`), so nothing here returns
/// while still holding it.
fn infl_do_decompress(addr: i64, input_data: &[u8], output_buf: &mut [u8]) -> InflateStep {
    let mut tbl = inflater_table(addr).lock().unwrap_or_else(|e| e.into_inner());
    let Some(st) = tbl.get_mut(&addr) else {
        return InflateStep::no_progress();
    };
    st.before_step();
    decompress_step(
        &mut st.decomp,
        st.track_adler.then_some(&mut st.adler),
        input_data,
        output_buf,
    )
}

impl InflateStep {
    /// Zero handle / already-ended stream. Report no progress, matching the
    /// long-standing behaviour: the Java wrapper's `ensureOpen()` is what
    /// rejects a closed Inflater, and a caller that legitimately races here
    /// must not be handed a decode error it cannot act on.
    fn no_progress() -> Self {
        InflateStep {
            input_consumed: 0,
            output_consumed: 0,
            finished: false,
            need_dict: false,
            data_error: None,
        }
    }
}

/// One zlib step of a stream's state, and the running ADLER-32 of what it
/// produced: the body [`infl_do_decompress`] and the scratch-buffer path
/// ([`inflate_bytes_bytes_reusing`]) share.
fn decompress_step(
    decomp: &mut Decompress,
    adler: Option<&mut u32>,
    input_data: &[u8],
    output_buf: &mut [u8],
) -> InflateStep {
    let total_in_before = decomp.total_in();
    let total_out_before = decomp.total_out();
    let status = decomp.decompress(input_data, output_buf, FlushDecompress::None);
    let input_consumed = (decomp.total_in() - total_in_before) as u32;
    let output_consumed = (decomp.total_out() - total_out_before) as u32;
    let (finished, dict_id, data_error) = match status {
        Ok(flate2::Status::StreamEnd) => (true, None, None),
        Ok(flate2::Status::Ok | flate2::Status::BufError) => (false, None, None),
        Err(e) => match e.needs_dictionary() {
            // A "needs dictionary" error carries the dictionary's Adler-32
            // (the stream's DICTID); the JDK signals that through the
            // needDict bit, NOT an exception.
            Some(id) => (false, Some(id), None),
            // Everything else is zlib's Z_DATA_ERROR / Z_STREAM_ERROR: the
            // stream is corrupt and `DataFormatException` is the spec'd answer.
            // Its message is zlib's own `strm->msg` ("incorrect header check"),
            // which `Inflater.c` hands to `DataFormatException` verbatim; flate2's
            // `Display` prefixes "deflate decompression error: " to it, so read
            // the bare message (interpreter round i6 wave 115, lane L5; probe
            // `L5W115ZipDictionaryAndDataErrors`). `Display` only when zlib left
            // no message.
            None => (
                false,
                None,
                Some(e.message().map_or_else(|| format!("{e}"), str::to_owned)),
            ),
        },
    };
    let need_dict = dict_id.is_some();
    // `Inflater.getAdler()` reports the ADLER-32 of the uncompressed data, and
    // must agree no matter which overload the JDK wrapper picked. `.min(len)`
    // is belt-and-braces: a slice panic here would abort the VM.
    // `None`: a raw stream under `CRATONVM_INFLATER_RAW_ADLER_ZERO`, which
    // keeps no checksum (zlib's raw `strm->adler`).
    if let Some(adler) = adler {
        let produced = (output_consumed as usize).min(output_buf.len());
        *adler = adler32_update(*adler, &output_buf[..produced]);
        // zlib's `Z_NEED_DICT`: `strm->adler` is the DICTID, which
        // `Inflater.getAdler()` reports so the caller can choose the
        // dictionary (wave 79, L5; i73 page case 3).
        if let Some(id) = dict_id {
            *adler = id;
        }
    }
    InflateStep {
        input_consumed,
        output_consumed,
        finished,
        need_dict,
        data_error,
    }
}

/// `inflateBytesBytes` over the stream's own scratch buffers
/// (`CRATONVM_INFLATER_REUSE_BUFFERS`; interpreter round i3 wave 73, lane L5).
///
/// The same copy in, the same zlib step and the same copy out as the
/// allocating path, so the packed result, the bytes written into `output_arr`,
/// the stream's totals and its ADLER-32 are identical; only where the two
/// intermediate buffers live differs. zlib only writes its output buffer, so
/// stale bytes past what a call produces are never read or copied out. The
/// caller keeps calls larger than [`INFLATER_SCRATCH_CAP`] on the allocating
/// path.
///
/// The handle-table mutex is held for the zlib step only, as in
/// [`infl_do_decompress`]: the buffers are taken out of the state for the two
/// array copies (which go through the context) and put back afterwards, so no
/// context call ever runs under the process-wide table lock. `Inflater`'s
/// Java methods hold the stream's own lock across the native, so no other call
/// on this handle runs in between; one that `end()`ed it meanwhile simply
/// finds no state to step or to return the buffers to.
#[allow(clippy::too_many_arguments)]
fn inflate_bytes_bytes_reusing(
    ctx: &mut dyn NativeContext,
    addr: i64,
    input_arr: Option<ObjectRef>,
    in_off: usize,
    in_len: usize,
    output_arr: Option<ObjectRef>,
    out_off: usize,
    out_len: usize,
) -> InflateStep {
    let (mut scratch_in, mut scratch_out) = {
        let mut tbl = inflater_table(addr).lock().unwrap_or_else(|e| e.into_inner());
        match tbl.get_mut(&addr) {
            Some(st) => (
                std::mem::take(&mut st.scratch_in),
                std::mem::take(&mut st.scratch_out),
            ),
            None => return InflateStep::no_progress(),
        }
    };
    // `read_byte_array`'s clamping, into the kept buffer.
    let input_len = match input_arr {
        Some(a) => {
            let arr_len = ctx.array_length(a);
            let start = in_off.min(arr_len);
            let end = in_off.saturating_add(in_len).min(arr_len);
            let n = end.saturating_sub(start);
            if scratch_in.len() < n {
                scratch_in.resize(n, 0);
            }
            ctx.read_byte_array_into(a, start, &mut scratch_in[..n])
        }
        None => 0,
    };
    if scratch_out.len() < out_len {
        scratch_out.resize(out_len, 0);
    }
    let step = {
        let mut tbl = inflater_table(addr).lock().unwrap_or_else(|e| e.into_inner());
        match tbl.get_mut(&addr) {
            Some(st) => {
                st.before_step();
                decompress_step(
                    &mut st.decomp,
                    st.track_adler.then_some(&mut st.adler),
                    &scratch_in[..input_len],
                    &mut scratch_out[..out_len],
                )
            }
            None => InflateStep::no_progress(),
        }
    };
    if let Some(a) = output_arr {
        let produced = (step.output_consumed as usize).min(out_len);
        if produced > 0 {
            write_byte_array(ctx, a, out_off, &scratch_out[..produced]);
        }
    }
    let mut tbl = inflater_table(addr).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(st) = tbl.get_mut(&addr) {
        st.scratch_in = scratch_in;
        st.scratch_out = scratch_out;
    }
    step
}

/// `Inflater.inflateBytesBytes`'s door: the body, timed for
/// `CRATONVM_DBG=zip-stream-census` (interpreter round i6 wave 117, lane L5).
fn infl_inflate_bytes_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let clock = crate::zip_stream_census::start();
    let result = infl_inflate_bytes_bytes_body(ctx, args);
    crate::zip_stream_census::note_inflate_time(clock);
    result
}

/// `Inflater.inflateBytesBuffer`'s door: the body, timed for
/// `CRATONVM_DBG=zip-stream-census` (interpreter round i6 wave 117, lane L5).
fn infl_inflate_bytes_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let clock = crate::zip_stream_census::start();
    let result = infl_inflate_bytes_buffer_body(ctx, args);
    crate::zip_stream_census::note_inflate_time(clock);
    result
}

/// `Inflater.inflateBufferBytes`'s door: the body, timed for
/// `CRATONVM_DBG=zip-stream-census` (interpreter round i6 wave 117, lane L5).
fn infl_inflate_buffer_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let clock = crate::zip_stream_census::start();
    let result = infl_inflate_buffer_bytes_body(ctx, args);
    crate::zip_stream_census::note_inflate_time(clock);
    result
}

/// `Inflater.inflateBufferBuffer`'s door: the body, timed for
/// `CRATONVM_DBG=zip-stream-census` (interpreter round i6 wave 117, lane L5).
fn infl_inflate_buffer_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let clock = crate::zip_stream_census::start();
    let result = infl_inflate_buffer_buffer_body(ctx, args);
    crate::zip_stream_census::note_inflate_time(clock);
    result
}

fn infl_inflate_bytes_bytes_body(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let reuse = ctx.inflater_reuse_buffers();
    let window = ctx.inflater_input_window();
    inflate_bytes_bytes_with(ctx, args, reuse, window)
}

/// `Inflater.inflateBytesBytes`, with the scratch-buffer choice passed in so
/// the unit tests can run both paths on a mock context (the input window
/// off).
fn inflate_bytes_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    reuse_buffers: bool,
) -> MethodCallResult {
    inflate_bytes_bytes_with(ctx, args, reuse_buffers, false)
}

/// The smallest input window [`inflate_bytes_windowed`] hands zlib per step.
const INFLATER_INPUT_WINDOW_MIN: usize = 512;

/// How much of the caller's `in_len` input bytes one windowed step copies:
/// the output window's size (a 1:1 guess; most streams expand, so one step
/// usually covers the call), at least [`INFLATER_INPUT_WINDOW_MIN`], at most
/// all of it.
fn inflater_input_window(in_len: usize, out_len: usize) -> usize {
    in_len.min(out_len.max(INFLATER_INPUT_WINDOW_MIN))
}

/// `inflateBytesBytes` over bounded input windows
/// (`CRATONVM_INFLATER_INPUT_WINDOW`; interpreter round i3 wave 75, lane L5;
/// `docs/internal/fixed-bugs/interpreter-i74-L5-proposal-inflate-without-copying-the-whole-input-20261107-DONE-20261108.md`).
///
/// The allocating path copies `input[in_off .. in_off + in_len]` -- all the
/// input `setInput` handed over that zlib has not consumed yet -- on every
/// call, then lets zlib take what it needs: O(remaining input) per call, so
/// `InflaterInputStream.read()` (one byte out per call) over a 64 KiB buffer
/// copies 64 KiB per byte. HotSpot points zlib at the array in place.
///
/// Here zlib gets [`inflater_input_window`] bytes at a time, and the next
/// window only while the previous step consumed ALL of its window and the
/// stream neither finished, failed nor asked for a dictionary -- also when
/// the output is already full, because the one-call path would have gone on
/// consuming too (zlib decodes the next symbol, block header or table before
/// it notices it has no room to write, and that input counts as consumed).
/// It stops at the first step that leaves input in its window, or when the
/// input runs out. zlib's `inflate()` is resumable at any input boundary:
/// its state after a run over a split input is its state after the same
/// run over the whole input, and the point a call stops at (no output room,
/// end of stream, an error, a dictionary) does not depend on how the input
/// arrived, because a stop for want of input is never the last step here
/// unless the input really ran out. So the packed result (input consumed,
/// output produced, finished, needDict), the bytes written, the stream's
/// totals and its ADLER-32 are the one-call path's. The unit test
/// `i75_windowed_inflate_matches_the_whole_input_call` drives one stream each
/// way over the same calls and compares everything per call.
#[allow(clippy::too_many_arguments)]
fn inflate_bytes_windowed(
    ctx: &mut dyn NativeContext,
    addr: i64,
    input_arr: ObjectRef,
    in_off: usize,
    in_len: usize,
    output_arr: Option<ObjectRef>,
    out_off: usize,
    out_len: usize,
) -> InflateStep {
    // `read_byte_array`'s clamping to the array.
    let arr_len = ctx.array_length(input_arr);
    let start = in_off.min(arr_len);
    let mut in_end = in_off.saturating_add(in_len).min(arr_len);
    let window = inflater_input_window(in_end.saturating_sub(start), out_len);
    let mut input = vec![0u8; window];
    let mut output = vec![0u8; out_len];
    let mut total = InflateStep::no_progress();
    let mut fed = start;
    loop {
        let want = window.min(in_end.saturating_sub(fed));
        let n = ctx.read_byte_array_into(input_arr, fed, &mut input[..want]);
        if n < want {
            // A short read (not a `byte[]` after all): what was read is all
            // the input there is.
            in_end = fed + n;
        }
        let produced = total.output_consumed as usize;
        let step = infl_do_decompress(addr, &input[..n], &mut output[produced..]);
        total.input_consumed += step.input_consumed;
        total.output_consumed += step.output_consumed;
        total.finished = step.finished;
        total.need_dict = step.need_dict;
        total.data_error = step.data_error;
        if total.finished || total.need_dict || total.data_error.is_some() {
            break;
        }
        if step.input_consumed as usize != n || n == 0 {
            break;
        }
        fed += n;
        if fed >= in_end {
            break;
        }
    }
    if let Some(a) = output_arr {
        let produced = (total.output_consumed as usize).min(out_len);
        if produced > 0 {
            write_byte_array(ctx, a, out_off, &output[..produced]);
        }
    }
    total
}

/// `Inflater.inflateBytesBytes` with both buffer choices passed in.
fn inflate_bytes_bytes_with(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    reuse_buffers: bool,
    input_window: bool,
) -> MethodCallResult {
    // this, long addr, byte[] in, int inOff, int inLen, byte[] out, int outOff, int outLen
    // args[0] = this (receiver)
    let this = arg_obj(args, 0);
    let addr = arg_long(args, 1);
    let input_arr = arg_obj(args, 2);
    let in_off = arg_int(args, 3).max(0) as usize;
    let in_len = arg_int(args, 4).max(0) as usize;
    let output_arr = arg_obj(args, 5);
    let out_off = arg_int(args, 6).max(0) as usize;
    let out_len = arg_int(args, 7).max(0) as usize;

    // `CRATONVM_INFLATER_INPUT_WINDOW`: only when the input is larger than
    // one window (a class read's input fits one, so it keeps the paths
    // below unchanged).
    if input_window && in_len > inflater_input_window(in_len, out_len) {
        if let Some(a) = input_arr {
            let step =
                inflate_bytes_windowed(ctx, addr, a, in_off, in_len, output_arr, out_off, out_len);
            return finish_inflate(ctx, this, step);
        }
    }

    if reuse_buffers && in_len <= INFLATER_SCRATCH_CAP && out_len <= INFLATER_SCRATCH_CAP {
        let step = inflate_bytes_bytes_reusing(
            ctx, addr, input_arr, in_off, in_len, output_arr, out_off, out_len,
        );
        return finish_inflate(ctx, this, step);
    }

    let input_data = match input_arr {
        Some(a) => read_byte_array(ctx, a, in_off, in_len),
        None => Vec::new(),
    };
    let mut output_buf = vec![0u8; out_len];

    let step = infl_do_decompress(addr, &input_data, &mut output_buf);

    if let Some(a) = output_arr {
        if step.output_consumed > 0 {
            write_byte_array(
                ctx,
                a,
                out_off,
                &output_buf[..step.output_consumed as usize],
            );
        }
    }

    finish_inflate(ctx, this, step)
}

fn infl_inflate_bytes_buffer_body(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // input: byte[], output: direct ByteBuffer (long addr).
    let this = arg_obj(args, 0);
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
    let step = infl_do_decompress(addr, &input_data, &mut output_buf);
    if step.output_consumed > 0
        && !ctx.copy_to_native_memory(output_addr, &output_buf[..step.output_consumed as usize])
    {
        return Err(RuntimeError::IOException {
            message: format!("inflateBytesBuffer: invalid output buffer address {output_addr:#x}"),
        }
        .into());
    }
    finish_inflate(ctx, this, step)
}

fn infl_inflate_buffer_bytes_body(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // input: direct ByteBuffer (long addr), output: byte[].
    let this = arg_obj(args, 0);
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
    let step = infl_do_decompress(addr, &input_data, &mut output_buf);
    if let Some(a) = output_arr {
        if step.output_consumed > 0 {
            write_byte_array(
                ctx,
                a,
                out_off,
                &output_buf[..step.output_consumed as usize],
            );
        }
    }
    finish_inflate(ctx, this, step)
}

fn infl_inflate_buffer_buffer_body(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Both sides are direct ByteBuffers (already resolved to native addresses).
    let this = arg_obj(args, 0);
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
    let step = infl_do_decompress(addr, &input_data, &mut output_buf);
    if step.output_consumed > 0
        && !ctx.copy_to_native_memory(output_addr, &output_buf[..step.output_consumed as usize])
    {
        return Err(RuntimeError::IOException {
            message: format!("inflateBufferBuffer: invalid output buffer address {output_addr:#x}"),
        }
        .into());
    }
    finish_inflate(ctx, this, step)
}

fn infl_get_adler(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = arg_long(args, 0);
    let tbl = inflater_table(addr).lock().unwrap_or_else(|e| e.into_inner());
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
    let mut tbl = inflater_table(addr).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(st) = tbl.get_mut(&addr) {
        st.decomp.reset(st.zlib_header);
        // `Inflater.reset()` restarts the checksum at the zlib seed too (a raw
        // stream that keeps none stays at 0).
        st.adler = inflater_adler_seed(st.track_adler);
        st.dict_resume = false;
    }
    Ok(None)
}

fn infl_end(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = arg_long(args, 0);
    inflater_table(addr)
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
    // `Deflater` validates both (level -1..=9, strategy 0..=2) before this
    // native; out-of-range values fall back to zlib's defaults here.
    let strategy = match arg_int(args, 1) {
        s @ 0..=4 => s,
        _ => libz_sys::Z_DEFAULT_STRATEGY,
    };
    let nowrap = arg_bool(args, 2);
    let Some(state) = DeflaterState::new(defl_effective_level(level_raw), strategy, nowrap) else {
        // `Deflater.c`: `Z_MEM_ERROR` -> `OutOfMemoryError`; anything else is
        // an `InternalError`.
        return Err(RuntimeError::OutOfMemoryError {
            message: "Deflater init failed".into(),
        }
        .into());
    };
    let handle = next_handle();
    deflater_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(handle, state);
    Ok(Some(Value::Long(handle)))
}

/// `Deflater.setDictionary(long addr, byte[] b, int off, int len)` — the
/// compressing half of the preset-dictionary contract; see
/// [`infl_set_dictionary`] for why this was a no-op and why that was wrong.
///
/// Without it the encoder silently produced a stream with `FDICT` clear, so a
/// round trip through CratonVM's own Deflater+Inflater still "worked" — both
/// halves ignored the dictionary — while the output was rejected by any real
/// zlib peer and any stream from one was rejected here. That mutual blindness
/// is why a probe has to compare the COMPRESSED LENGTH against HotSpot (57 vs
/// 75 bytes for the same input) rather than just asserting the round trip.
fn defl_set_dictionary(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = arg_long(args, 0);
    let Some(arr) = arg_obj(args, 1) else {
        return Ok(None);
    };
    let off = arg_int(args, 2).max(0) as usize;
    let len = arg_int(args, 3).max(0) as usize;
    let dict = read_byte_array(ctx, arr, off, len);
    let refusal = defl_apply_dictionary(addr, &dict);
    dictionary_refusal(ctx, refusal)
}

/// `deflateSetDictionary` on the deflater at `addr`. `Deflater.c`'s
/// `checkSetDictionaryResult` throws `IllegalArgumentException` (no message)
/// for `Z_STREAM_ERROR` -- a zlib-wrapped stream past its header, or a raw one
/// with input still buffered -- which used to be swallowed (interpreter round i6
/// wave 115, lane L5; probe `L5W115ZipDictionaryAndDataErrors`). `None` when
/// taken or `addr` names no stream.
fn defl_apply_dictionary(addr: i64, dict: &[u8]) -> Option<Option<String>> {
    let mut tbl = deflater_table().lock().unwrap_or_else(|e| e.into_inner());
    let st = tbl.get_mut(&addr)?;
    match st.set_dictionary(dict) {
        libz_sys::Z_OK => None,
        // `JNU_ThrowIllegalArgumentException(env, 0)`; any other code is
        // `InternalError` there, which zlib's `deflateSetDictionary` never returns.
        _ => Some(None),
    }
}

/// `Deflater.setDictionaryBuffer(long addr, long bufferAddress, int len)` — the
/// direct-`ByteBuffer` flavour of [`defl_set_dictionary`]; see
/// [`infl_set_dictionary_buffer`] for why this pair was a no-op and why the
/// reason given was not true.
fn defl_set_dictionary_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = arg_long(args, 0);
    let buf_addr = arg_long(args, 1);
    let len = arg_int(args, 2).max(0) as usize;
    let dict = read_direct_dictionary(ctx, buf_addr, len)?;
    let refusal = defl_apply_dictionary(addr, &dict);
    dictionary_refusal(ctx, refusal)
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
/// The flush codes are zlib's, which is what `java.util.zip.Deflater` passes down:
/// 0=NO_FLUSH, 1=PARTIAL_FLUSH, 2=SYNC_FLUSH, 3=FULL_FLUSH, 4=FINISH (the JDK
/// constants are `NO_FLUSH` 0, `SYNC_FLUSH` 2, `FULL_FLUSH` 3). SYNC_FLUSH keeps the
/// dictionary; FULL_FLUSH clears it (the two used to be swapped, so a repeating
/// sync-flushed stream compressed at stored size).
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
) -> Result<(u32, u32, bool, bool), MethodCallFailed> {
    let mut tbl = deflater_table().lock().unwrap_or_else(|e| e.into_inner());
    let st = match tbl.get_mut(&addr) {
        Some(s) => s,
        None => {
            if crate::nbflags().dbg_deflate {
                eprintln!(
                    "[DBG-DEFLATER] {which} addr={addr:#x} NOT FOUND in deflater_table (silent 0/0 return)"
                );
            }
            return Ok((0, 0, false, false));
        }
    };

    if st.finished && params & 1 == 0 {
        // Matches real JDK: once finished, deflate() is a no-op until reset().
        return Ok((0, 0, true, false));
    }
    // JDK packs params as: bit0=set, bits1..2=strategy, bits3..=level, and
    // `Deflater.c`'s `doDeflate` runs ONLY `deflateParams` in such a call
    // (wave 79, L5: the strategy used to be dropped and the same call
    // compressed; see [`DeflaterState`]). Flush codes are zlib's
    // (0=NO_FLUSH, 2=SYNC_FLUSH, 3=FULL_FLUSH, 4=FINISH), passed through.
    st.step(params, flush_code, input_data, output_buf)
        .map_err(|message| RuntimeError::InternalError { message }.into())
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

    let (input_consumed, output_consumed, finished, params_pending) = defl_do_compress(
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

    Ok(Some(Value::Long(pack_deflate_result_params(
        input_consumed,
        output_consumed,
        finished,
        params_pending,
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
/// jetty-webserver-factory-poststartup-timeout-and-reflective-supertype-residuals-FIXED.md
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

    let (input_consumed, output_consumed, finished, params_pending) = defl_do_compress(
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

    Ok(Some(Value::Long(pack_deflate_result_params(
        input_consumed,
        output_consumed,
        finished,
        params_pending,
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

    let (input_consumed, output_consumed, finished, params_pending) = defl_do_compress(
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

    Ok(Some(Value::Long(pack_deflate_result_params(
        input_consumed,
        output_consumed,
        finished,
        params_pending,
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

    let (input_consumed, output_consumed, finished, params_pending) = defl_do_compress(
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

    Ok(Some(Value::Long(pack_deflate_result_params(
        input_consumed,
        output_consumed,
        finished,
        params_pending,
    ))))
}

fn defl_get_adler(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = arg_long(args, 0);
    let tbl = deflater_table().lock().unwrap_or_else(|e| e.into_inner());
    // zlib's `strm->adler`, as `Deflater.c` returns it: the running ADLER-32
    // of the input for a zlib-wrapped stream, the dictionary's id right after
    // `setDictionary`, and 1 forever for a raw (`nowrap`) stream (wave 79, L5;
    // i73 page case 2 -- this VM used to keep its own checksum for raw
    // streams too).
    let adler = tbl.get(&addr).map_or(1, |st| st.adler());
    Ok(Some(Value::Int(adler as i32)))
}

fn defl_reset(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = arg_long(args, 0);
    let mut tbl = deflater_table().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(st) = tbl.get_mut(&addr) {
        // `deflateReset`: zlib re-seeds `strm->adler` itself.
        st.reset();
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
// natives (`infl_inflate_bytes_buffer` and friends, which report an unreadable
// buffer address rather than fabricating a successful step), so the former
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
// Adler32 natives (real-JDK mode)
// ---------------------------------------------------------------------------
//
// `java.util.zip.Adler32` in JDK 25 is pure Java holding a single `int adler`
// field (seeded to 1 by the field initialiser) and delegating every mutation
// to a *static* native that takes the current value and returns the new one:
//
//     private static native int update(int adler, int b);
//     private static native int updateBytes(int adler, byte[] b, int off, int len);
//     private static native int updateByteBuffer(int adler, long addr, int off, int len);
//
// Unlike `CRC32`, there is no concrete Java `updateBytes` wrapper in front of
// an `updateBytes0` native — the range check lives in the *public*
// `update(byte[],int,int)` body and `updateBytes` itself is the JNI entry
// point. So the names above are exactly what has to be registered; missing
// them is a hard `UnsatisfiedLinkError` from `Adler32.update` (seen as
// `Missing native method in real-JDK mode
// method=java/util/zip/Adler32.updateBytes(I[BII)I` in the regression suite's
// `RJdkJni.zipNatives`).
//
// Also unlike CRC32, there is NO complement dance at the boundary: RFC 1950's
// Adler-32 state *is* the externally visible value. `getValue()` is just
// `(long) adler & 0xffffffffL`, and a fresh checksum is 1 (s1 = 1, s2 = 0),
// not 0. `adler32_update` above already implements the rolling RFC 1950 §9
// update in exactly that public representation — the same helper the
// Deflater/Inflater `getAdler` bridges accumulate with — so these natives are
// thin wrappers over it and can never drift from the zlib-stream checksums.
//
// Hand-verified vectors (see the unit tests below):
//   Adler32("abc")       = 0x024D0127   (s1 = 1+97+98+99 = 295 = 0x127,
//                                        s2 = 98+196+295 = 589 = 0x24D)
//   Adler32("123456789") = 0x091E01DE   (the value RJdkJni.java:119 asserts)
//
// These are Bridge, not SyntheticStub: they implement a method that genuinely
// has no Java body in the real JDK, so `--jdk-only` must keep them or the
// real JDK class file cannot run at all.

/// `private static native int update(int adler, int b)`.
fn adler32_update_int(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let adler = arg_int(args, 0) as u32;
    let b = (arg_int(args, 1) & 0xFF) as u8;
    Ok(Some(Value::Int(adler32_update(adler, &[b]) as i32)))
}

/// `private static native int updateBytes(int adler, byte[] b, int off, int len)`.
fn adler32_update_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let adler = arg_int(args, 0) as u32;
    let arr = arg_obj(args, 1);
    let off = arg_int(args, 2).max(0) as usize;
    let len = arg_int(args, 3).max(0) as usize;
    let bytes = match arr {
        Some(a) => read_byte_array(ctx, a, off, len),
        None => Vec::new(),
    };
    Ok(Some(Value::Int(adler32_update(adler, &bytes) as i32)))
}

/// `private static native int updateByteBuffer(int adler, long addr, int off, int len)`.
///
/// `addr` is the direct buffer's resolved native base address (the Java side
/// already extracted it via `((DirectBuffer) buffer).address()`); `off`/`len`
/// are the buffer's `position()`/remaining, mirroring the byte[] overload.
fn adler32_update_byte_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let adler = arg_int(args, 0) as u32;
    let addr = arg_long(args, 1);
    let off = arg_int(args, 2) as i64;
    let len = arg_int(args, 3).max(0) as usize;
    let mut bytes = vec![0u8; len];
    if len > 0 && !ctx.copy_from_native_memory(addr + off, &mut bytes) {
        return Err(RuntimeError::IOException {
            message: "Adler32.updateByteBuffer: failed to read native buffer memory".to_string(),
        }
        .into());
    }
    Ok(Some(Value::Int(adler32_update(adler, &bytes) as i32)))
}

// ---------------------------------------------------------------------------
// ZipUtils field reads (CRATONVM_ZIP_UTILS_INTRINSICS)
// ---------------------------------------------------------------------------

const ZIP_UTILS: &str = "java/util/zip/ZipUtils";

/// The unsigned little-endian value of `b[off..off + width]` (`width` 2 or 4),
/// or `None` when the JDK body must answer: a null array, a negative `off`, a
/// range past the end, a non-`byte[]` (the read comes back short).
fn zip_utils_read_le(ctx: &dyn NativeContext, args: &[Value], width: usize) -> Option<u32> {
    zip_le_at(ctx, arg_obj(args, 0), arg_int(args, 1), width)
}

/// The unsigned little-endian value of `arr[off..off + width]` (`width` 1, 2
/// or 4), or `None` for a null array, a negative `off` or a range past the
/// end (and a short read: not a `byte[]`). The shared body of the `get16` /
/// `get32` intrinsics and the CEN / LOC accessor intrinsics.
fn zip_le_at(
    ctx: &dyn NativeContext,
    arr: Option<ObjectRef>,
    off: i32,
    width: usize,
) -> Option<u32> {
    let arr = arr?;
    let off = usize::try_from(off).ok()?;
    if off.checked_add(width)? > ctx.array_length(arr) {
        return None;
    }
    let mut buf = [0u8; 4];
    if ctx.read_byte_array_into(arr, off, &mut buf[..width]) != width {
        return None;
    }
    Some(u32::from_le_bytes(buf))
}

/// `ZipUtils.get16(byte[] b, int off)` -> `int`, as an `Intrinsic`
/// (`CRATONVM_ZIP_UTILS_INTRINSICS`; interpreter round i3 wave 74, lane L5).
///
/// The JDK 25 body is two `Preconditions.checkIndex` calls and
/// `Short.toUnsignedInt(UNSAFE.getShortUnaligned(b, ARRAY_BYTE_BASE_OFFSET +
/// off, false))` -- five or six interpreted frames and an `Unsafe` native
/// door for every CEN field `ZipFile.getEntry` reads (about ten per hit).
/// In range, the answer is the same two bytes, little-endian, zero-extended.
/// Out of range or null, the JDK body runs (`invoke_special_bytecode_only`
/// executes the bytecode, not this native), so the exception, its message
/// and its `Preconditions` frames are the JDK's own.
fn zip_utils_get16(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match zip_utils_read_le(ctx, args, 2) {
        Some(v) => Ok(Some(Value::Int(v as i32))),
        None => ctx.invoke_special_bytecode_only(ZIP_UTILS, "get16", "([BI)I", args),
    }
}

/// `ZipUtils.get32(byte[] b, int off)` -> `long` (the unsigned 32-bit value),
/// as [`zip_utils_get16`].
fn zip_utils_get32(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match zip_utils_read_le(ctx, args, 4) {
        Some(v) => Ok(Some(Value::Long(i64::from(v)))),
        None => ctx.invoke_special_bytecode_only(ZIP_UTILS, "get32", "([BI)J", args),
    }
}

// ---------------------------------------------------------------------------
// ZipUtils CEN / LOC header accessors (CRATONVM_ZIP_CEN_ACCESSOR_INTRINSICS)
// ---------------------------------------------------------------------------

/// One `java.util.zip.ZipUtils` header accessor answered natively: its name,
/// its descriptor, the field's offset (from `pos` for a CEN accessor,
/// `([BI)?`; from 0 for a LOC accessor, `([B)?`) and its width in bytes.
///
/// The table is the JDK 25 source (`java.base/java/util/zip/ZipUtils.java`
/// of the 25.0.3 `src.zip`; confirm against `javap -c -p
/// java.util.zip.ZipUtils` of the 25.0.4 image on the host before a flip):
///
/// ```text
/// static final long CENSIG(byte[] b, int pos) { return get32(b, pos + 0); }
/// static final int  CENVEM(byte[] b, int pos) { return get16(b, pos + 4); }
/// static final int  CENVEM_FA(byte[] b, int pos) { return Byte.toUnsignedInt(b[pos + 5]); }
/// static final int  CENVER(byte[] b, int pos) { return get16(b, pos + 6); }
/// static final int  CENFLG(byte[] b, int pos) { return get16(b, pos + 8); }
/// static final int  CENHOW(byte[] b, int pos) { return get16(b, pos + 10);}
/// static final long CENTIM(byte[] b, int pos) { return get32(b, pos + 12);}
/// static final long CENCRC(byte[] b, int pos) { return get32(b, pos + 16);}
/// static final long CENSIZ(byte[] b, int pos) { return get32(b, pos + 20);}
/// static final long CENLEN(byte[] b, int pos) { return get32(b, pos + 24);}
/// static final int  CENNAM(byte[] b, int pos) { return get16(b, pos + 28);}
/// static final int  CENEXT(byte[] b, int pos) { return get16(b, pos + 30);}
/// static final int  CENCOM(byte[] b, int pos) { return get16(b, pos + 32);}
/// static final int  CENDSK(byte[] b, int pos) { return get16(b, pos + 34);}
/// static final int  CENATT(byte[] b, int pos) { return get16(b, pos + 36);}
/// static final long CENATX(byte[] b, int pos) { return get32(b, pos + 38);}
/// static final int  CENATX_PERMS(byte[] b, int pos) { return get16(b, pos + 40);}
/// static final long CENOFF(byte[] b, int pos) { return get32(b, pos + 42);}
/// static final long LOCSIG(byte[] b) { return get32(b, 0); }
/// static final int  LOCNAM(byte[] b) { return get16(b, 26);}
/// static final int  LOCEXT(byte[] b) { return get16(b, 28);}
/// ```
///
/// `pos + k` is Java `int` arithmetic (it wraps), and so is it here; a
/// wrapped, negative or past-the-end offset, or a null array, runs the JDK
/// body, which throws its own exception with its own message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ZipHeaderField {
    name: &'static str,
    descriptor: &'static str,
    field: i32,
    width: usize,
}

impl ZipHeaderField {
    /// A CEN accessor (`(byte[] b, int pos)`): the field sits at `pos + field`.
    fn takes_pos(&self) -> bool {
        self.descriptor.starts_with("([BI)")
    }

    /// The accessor answers a `long` (`get32`), else an `int`.
    fn returns_long(&self) -> bool {
        self.descriptor.ends_with('J')
    }

    /// The JDK body's answer when it is a plain in-range read, else `None`.
    fn answer(&self, ctx: &dyn NativeContext, args: &[Value]) -> Option<Value> {
        let off = if self.takes_pos() {
            arg_int(args, 1).wrapping_add(self.field)
        } else {
            self.field
        };
        let v = zip_le_at(ctx, arg_obj(args, 0), off, self.width)?;
        Some(if self.returns_long() {
            Value::Long(i64::from(v))
        } else {
            Value::Int(v as i32)
        })
    }
}

/// The body every header accessor intrinsic shares: the field in range, the
/// JDK bytecode otherwise (`invoke_special_bytecode_only` runs the bytecode,
/// not this native).
fn zip_header_accessor_call(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    f: ZipHeaderField,
) -> MethodCallResult {
    match f.answer(ctx, args) {
        Some(v) => Ok(Some(v)),
        None => ctx.invoke_special_bytecode_only(ZIP_UTILS, f.name, f.descriptor, args),
    }
}

/// One callback per accessor (a `NativeCallback` is a plain `fn`), and the
/// table that registers them and that the unit test pins.
macro_rules! zip_header_accessors {
    ($( $fn_name:ident => $name:literal, $desc:literal, $field:literal, $width:literal; )*) => {
        $(
            fn $fn_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                zip_header_accessor_call(
                    ctx,
                    args,
                    ZipHeaderField { name: $name, descriptor: $desc, field: $field, width: $width },
                )
            }
        )*
        const ZIP_HEADER_ACCESSORS: &[(ZipHeaderField, NativeCallback)] = &[
            $(
                (
                    ZipHeaderField { name: $name, descriptor: $desc, field: $field, width: $width },
                    $fn_name as NativeCallback,
                ),
            )*
        ];
    };
}

zip_header_accessors! {
    zip_cen_sig => "CENSIG", "([BI)J", 0, 4;
    zip_cen_vem => "CENVEM", "([BI)I", 4, 2;
    zip_cen_vem_fa => "CENVEM_FA", "([BI)I", 5, 1;
    zip_cen_ver => "CENVER", "([BI)I", 6, 2;
    zip_cen_flg => "CENFLG", "([BI)I", 8, 2;
    zip_cen_how => "CENHOW", "([BI)I", 10, 2;
    zip_cen_tim => "CENTIM", "([BI)J", 12, 4;
    zip_cen_crc => "CENCRC", "([BI)J", 16, 4;
    zip_cen_siz => "CENSIZ", "([BI)J", 20, 4;
    zip_cen_len => "CENLEN", "([BI)J", 24, 4;
    zip_cen_nam => "CENNAM", "([BI)I", 28, 2;
    zip_cen_ext => "CENEXT", "([BI)I", 30, 2;
    zip_cen_com => "CENCOM", "([BI)I", 32, 2;
    zip_cen_dsk => "CENDSK", "([BI)I", 34, 2;
    zip_cen_att => "CENATT", "([BI)I", 36, 2;
    zip_cen_atx => "CENATX", "([BI)J", 38, 4;
    zip_cen_atx_perms => "CENATX_PERMS", "([BI)I", 40, 2;
    zip_cen_off => "CENOFF", "([BI)J", 42, 4;
    zip_loc_sig => "LOCSIG", "([B)J", 0, 4;
    zip_loc_nam => "LOCNAM", "([B)I", 26, 2;
    zip_loc_ext => "LOCEXT", "([B)I", 28, 2;
}

// ---------------------------------------------------------------------------
// ZipCoder$UTF8ZipCoder.compare (CRATONVM_ZIP_CODER_COMPARE_INTRINSIC)
// ---------------------------------------------------------------------------

const ZIP_CODER_UTF8: &str = "java/util/zip/ZipCoder$UTF8ZipCoder";
const ZIP_CODER_COMPARE_DESC: &str = "(Ljava/lang/String;[BIIZ)B";
/// `ZipCoder.EXACT_MATCH`, `DIRECTORY_MATCH`, `NO_MATCH` (JDK 25:
/// `static final byte EXACT_MATCH = 0, DIRECTORY_MATCH = 1, NO_MATCH = 2;`).
const ZIP_CODER_EXACT_MATCH: i32 = 0;
const ZIP_CODER_DIRECTORY_MATCH: i32 = 1;
const ZIP_CODER_NO_MATCH: i32 = 2;

/// `UTF8ZipCoder.compare(String str, byte[] b, int off, int len, boolean
/// matchDirectory)`'s answer for an all-ASCII `str`, or `None` when the JDK
/// body must answer.
///
/// The JDK 25 body:
///
/// ```text
/// byte[] encoded = JLA.uncheckedGetBytesNoRepl(str, UTF_8.INSTANCE);
/// int mismatch = Arrays.mismatch(encoded, 0, encoded.length, b, off, off+len);
/// if (mismatch == -1) return EXACT_MATCH;
/// else if (matchDirectory && len == mismatch + 1 && hasTrailingSlash(b, off + len)) return DIRECTORY_MATCH;
/// else return NO_MATCH;
/// // hasTrailingSlash(a, end): end > 0 && a[end - 1] == '/'
/// ```
///
/// For a `String` whose every char is below 0x80 the UTF-8 encoding is the
/// chars themselves, byte for byte, so the compare runs on the chars and the
/// CEN range in place: no encode, no `byte[]`. `Arrays.mismatch` answers the
/// first differing index below the shorter length, else -1 for equal lengths
/// and the shorter length otherwise; that is computed here the same way
/// (including the JDK's own quirk that a last-byte difference against a
/// trailing `/` of the same length reads as a directory match). Declined, so
/// the JDK body runs: a null `str` or `b`, a non-ASCII char (the encode and its
/// `CharacterCodingException` are the JDK's), and every range
/// `Arrays.mismatch`'s check would reject (`off < 0`, `len < 0`, `off + len`
/// past the end or overflowing).
/// Whether the booted library is the JDK whose `java.util.zip` internals the
/// intrinsics in this section were written against: the JDK 25 `ZipCoder`
/// outcome codes (`EXACT_MATCH = 0, DIRECTORY_MATCH = 1, NO_MATCH = 2`), its
/// `getEntryPos` walk and its hash. Unknown (synthetic JDK, mock contexts):
/// yes, as before. Interpreter round i5 wave 101, lane L5: over a JDK 21 image
/// a class-path jar resource answered `null` under `--jdk-only` while HotSpot
/// served it (`L5W100MultiReleaseFeature`, `resource=null`), the shape of an
/// outcome code the booted `ZipCoder` reads differently; any other release now
/// runs its own bytecode, which is what HotSpot runs.
fn zip_internals_are_jdk25(ctx: &dyn NativeContext) -> bool {
    ctx.jdk_feature_version().is_none_or(|feature| feature == 25)
}

fn zip_coder_compare_answer(ctx: &dyn NativeContext, args: &[Value]) -> Option<i32> {
    if !zip_internals_are_jdk25(ctx) {
        return None;
    }
    let s = arg_obj(args, 1)?;
    let b = arg_obj(args, 2)?;
    let off = arg_int(args, 3);
    let len = arg_int(args, 4);
    let match_directory = arg_bool(args, 5);
    let end = off.checked_add(len)?;
    if off < 0 || len < 0 || end as usize > ctx.array_length(b) {
        return None;
    }
    let units = ctx.read_string_units(s)?;
    if units.iter().any(|&u| u >= 0x80) {
        return None;
    }
    let (off, len) = (off as usize, len as usize);
    let mut stack = [0u8; 256];
    let mut heap = Vec::new();
    let cen: &mut [u8] = if len <= stack.len() {
        &mut stack[..len]
    } else {
        heap.resize(len, 0u8);
        &mut heap[..]
    };
    if ctx.read_byte_array_into(b, off, cen) != len {
        return None;
    }
    let n = units.len();
    let common = n.min(len);
    let mismatch: i64 = match (0..common).find(|&i| units[i] as u8 != cen[i]) {
        Some(i) => i as i64,
        None if n == len => -1,
        None => common as i64,
    };
    Some(if mismatch == -1 {
        ZIP_CODER_EXACT_MATCH
    } else if match_directory && len as i64 == mismatch + 1 && cen[len - 1] == b'/' {
        // `len == mismatch + 1` with `mismatch >= 0` means `len >= 1`, so
        // `b[off + len - 1]` is `cen[len - 1]`.
        ZIP_CODER_DIRECTORY_MATCH
    } else {
        ZIP_CODER_NO_MATCH
    })
}

/// `ZipCoder$UTF8ZipCoder.compare` as an `Intrinsic`
/// (`CRATONVM_ZIP_CODER_COMPARE_INTRINSIC`; interpreter round i3 wave 75,
/// lane L5): the entry-name compare every `ZipFile.getEntry` /
/// `getInputStream` hash hit runs. The JDK body otherwise.
fn zip_coder_utf8_compare(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match zip_coder_compare_answer(ctx, args) {
        Some(v) => Ok(Some(Value::Int(v))),
        None => ctx.invoke_special_bytecode_only(
            ZIP_CODER_UTF8,
            "compare",
            ZIP_CODER_COMPARE_DESC,
            args,
        ),
    }
}

// ---------------------------------------------------------------------------
// ZipFile$Source.getEntryPos miss (CRATONVM_ZIP_ENTRY_MISS_INTRINSIC)
// ---------------------------------------------------------------------------

const ZIP_SOURCE: &str = "java/util/zip/ZipFile$Source";
const ZIP_SOURCE_GET_ENTRY_POS_DESC: &str =
    "(Ljava/lang/String;ZLjava/util/zip/ZipCoder;)Ljava/util/zip/ZipFile$EntryPos;";
/// `ZipFile$Source.ZIP_ENDCHAIN` (JDK 25: `private static final int
/// ZIP_ENDCHAIN = -1;`).
const ZIP_ENDCHAIN: i32 = -1;

/// The four `ZipFile$Source` instance fields the hash walk reads, resolved
/// once per VM (`NativeContext::zip_source_slot_cache`) for the class id they
/// were resolved against. The wave-64 `Resource.getBytes` intrinsics resolved
/// their fields by name on every call and measured slower than the bytecode
/// they replaced; this one pays the by-name lookup once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ZipSourceSlots {
    class_id: ClassId,
    /// `private int total;`
    total: usize,
    /// `private int[] table;`
    table: usize,
    /// `private int tablelen;`
    tablelen: usize,
    /// `private int[] entries;`
    entries: usize,
}

impl ZipSourceSlots {
    fn resolve(ctx: &dyn NativeContext, class_id: ClassId) -> Option<Self> {
        Some(Self {
            class_id,
            total: ctx.resolve_field_index_by_class_id(class_id, "total")?,
            table: ctx.resolve_field_index_by_class_id(class_id, "table")?,
            tablelen: ctx.resolve_field_index_by_class_id(class_id, "tablelen")?,
            entries: ctx.resolve_field_index_by_class_id(class_id, "entries")?,
        })
    }
}

/// What the hit answer (`CRATONVM_ZIP_ENTRY_HIT_INTRINSIC`; interpreter round
/// i3 wave 77, lane L5) needs beyond the walk's four fields, resolved once
/// per VM at the first call that finds all of it (a failed resolution is not
/// kept: `ZipFile$EntryPos` is loaded and initialised by the first JDK-body
/// hit, which may come after the first walk).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ZipHitLayout {
    /// `ZipFile$Source.cen` (`private byte[] cen;`).
    cen: usize,
    /// `java/util/zip/ZipCoder$UTF8ZipCoder` (a `final` class whose
    /// `isUTF8()` is `true`, so `zipCoderFor` hands it back unread).
    utf8_coder: ClassId,
    /// `java/util/zip/ZipFile$EntryPos` (`record EntryPos(String name, int pos)`).
    entry_pos: ClassId,
    /// The record's instance field count.
    entry_pos_fields: usize,
    /// `EntryPos.name`'s slot.
    name: usize,
    /// `EntryPos.pos`'s slot.
    pos: usize,
}

const ZIP_ENTRY_POS: &str = "java/util/zip/ZipFile$EntryPos";

impl ZipHitLayout {
    fn resolve(ctx: &dyn NativeContext, source: ClassId) -> Option<Self> {
        let utf8_coder = ctx.class_id_by_name(ZIP_CODER_UTF8)?;
        let entry_pos = ctx.class_id_by_name(ZIP_ENTRY_POS)?;
        // A native allocation runs no `<clinit>`; answer only once the JDK
        // body's own `new EntryPos` has initialised the class.
        if !ctx.is_class_initialized(entry_pos) {
            return None;
        }
        let name = ctx.resolve_field_index_by_class_id(entry_pos, "name")?;
        let pos = ctx.resolve_field_index_by_class_id(entry_pos, "pos")?;
        Some(Self {
            cen: ctx.resolve_field_index_by_class_id(source, "cen")?,
            utf8_coder,
            entry_pos,
            entry_pos_fields: ctx.class_num_total_fields(entry_pos).max(name.max(pos) + 1),
            name,
            pos,
        })
    }
}

/// The per-VM box behind `NativeContext::zip_source_slot_cache`: the walk's
/// slots (kept whatever they resolved to) and the hit layout (kept once
/// resolved).
struct ZipSourceCache {
    slots: Option<ZipSourceSlots>,
    hit: OnceLock<ZipHitLayout>,
}

/// THIS VM's cache box, created at the first call (`None`: a context with no
/// per-VM slot, i.e. the mock contexts).
fn zip_source_cache(ctx: &dyn NativeContext, class_id: ClassId) -> Option<&ZipSourceCache> {
    let slot = ctx.zip_source_slot_cache()?;
    let boxed = slot.get_or_init(|| -> Box<dyn std::any::Any + Send + Sync> {
        Box::new(ZipSourceCache {
            slots: ZipSourceSlots::resolve(ctx, class_id),
            hit: OnceLock::new(),
        })
    });
    let any: &(dyn std::any::Any + Send + Sync) = &**boxed;
    any.downcast_ref::<ZipSourceCache>()
}

/// THIS VM's `ZipFile$Source` field slots for a receiver of `class_id`:
/// resolved at the first call and kept in the per-VM slot (a refused
/// resolution is kept too, so a layout without these fields declines for the
/// VM's life without asking again). A receiver of another class id than the
/// one resolved declines. A context with no per-VM slot (mock contexts)
/// resolves per call.
fn zip_source_slots(ctx: &dyn NativeContext, class_id: ClassId) -> Option<ZipSourceSlots> {
    if ctx.zip_source_slot_cache().is_none() {
        return ZipSourceSlots::resolve(ctx, class_id);
    }
    match zip_source_cache(ctx, class_id)?.slots {
        Some(slots) if slots.class_id == class_id => Some(slots),
        _ => None,
    }
}

/// THIS VM's [`ZipHitLayout`] for the `Source` class `slots` were resolved
/// against: kept once resolved, asked again while it does not resolve. A
/// context with no per-VM slot resolves per call.
fn zip_hit_layout(ctx: &dyn NativeContext, slots: &ZipSourceSlots) -> Option<ZipHitLayout> {
    let Some(cache) = zip_source_cache(ctx, slots.class_id) else {
        return ZipHitLayout::resolve(ctx, slots.class_id);
    };
    if let Some(layout) = cache.hit.get() {
        return Some(*layout);
    }
    let layout = ZipHitLayout::resolve(ctx, slots.class_id)?;
    let _ = cache.hit.set(layout);
    Some(layout)
}

/// JDK 25 `ZipCoder.hash(String name)` over the name's UTF-16 units:
///
/// ```text
/// int hsh = name.hashCode();
/// int len = name.length();
/// if (len > 0 && name.charAt(len - 1) != '/') {
///     hsh = hsh * 31 + '/';
/// }
/// return hsh;
/// ```
fn zip_coder_hash(units: &[u16]) -> i32 {
    let hash = units
        .iter()
        .fold(0i32, |h, &u| h.wrapping_mul(31).wrapping_add(i32::from(u)));
    match units.last() {
        Some(&last) if last != u16::from(b'/') => {
            hash.wrapping_mul(31).wrapping_add(i32::from(b'/'))
        }
        _ => hash,
    }
}

/// `Some(())` when `ZipFile$Source.getEntryPos(name, addSlash, zipCoder)`
/// would return `null` having run nothing but its hash walk, else `None` (the
/// JDK body must answer).
///
/// The JDK 25 body (`java.base/java/util/zip/ZipFile.java` of the 25.0.3
/// `src.zip`):
///
/// ```text
/// if (total == 0) { return null; }
/// int hsh = ZipCoder.hash(name);
/// int idx = table[(hsh & 0x7fffffff) % tablelen];
/// int dirPos = -1;
/// while (idx != ZIP_ENDCHAIN) {
///     if (getEntryHash(idx) == hsh) { ... compare, maybe return new EntryPos(..) ... }
///     idx = getEntryNext(idx);            // entries[idx + 1]
/// }
/// if (dirPos != -1) { return new EntryPos(name + "/", dirPos); }
/// return null;
/// ```
///
/// `dirPos` is set only under a hash match, so a chain with no
/// `entries[idx] == hsh` returns `null` without a compare, a `CEN` read or an
/// allocation: that is the only case answered here. `total == 0` answers
/// `null` before the name is touched (a null name too, as the body does).
/// Declined, so the JDK body runs from its first line: a hash match anywhere
/// on the chain, a null name with entries, a closed source (`table` /
/// `entries` null: the body's NPE), `tablelen <= 0` (the body's
/// `ArithmeticException` for 0; never negative in a well-formed source), a
/// chain index out of range or negative (the body's
/// `ArrayIndexOutOfBoundsException`), a chain longer than the entry count
/// (a cycle: the body loops), a field that does not read as declared.
///
/// The one difference from the body on the answered path: `String.hashCode`
/// caches the hash in the `String`'s private `hash` / `hashIsZero` fields,
/// which this leaves as they were; no public API observes it.
fn zip_entry_pos_miss(ctx: &dyn NativeContext, args: &[Value]) -> Option<()> {
    matches!(zip_entry_pos_walk(ctx, args, false), EntryPosWalk::Null).then_some(())
}

/// What [`zip_entry_pos_walk`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryPosWalk {
    /// The body would return `null` (no exact and no directory match).
    Null,
    /// The body would return `new EntryPos(name, pos)`: an exact match of the
    /// `UTF8ZipCoder` compare at CEN position `pos`, with no directory match
    /// before it on the chain.
    Exact(i32),
    /// The JDK body must answer.
    Decline,
}

/// The walk of [`zip_entry_pos_miss`]'s doc, and with `hits` (the
/// `CRATONVM_ZIP_ENTRY_HIT_INTRINSIC` layout resolved) its hash matches too
/// (interpreter round i3 wave 77, lane L5). Under a hash match the body runs:
///
/// ```text
/// int pos = getEntryPos(idx);                 // entries[idx + 2]
/// int noff = pos + CENHDR;                    // CENHDR = 46
/// int nlen = CENNAM(cen, pos);                // get16(cen, pos + 28)
/// final ZipCoder zc = zipCoderFor(cen, pos, zipCoder);
/// switch (zc.compare(name, cen, noff, nlen, addSlash)) {
///     case EXACT_MATCH:     return new EntryPos(name, pos);
///     case DIRECTORY_MATCH: dirPos = pos; break;
///     case NO_MATCH:        // continue
/// }
/// ```
///
/// Answered: a `zipCoder` of class `UTF8ZipCoder` (`zipCoderFor` returns it
/// without reading `CENFLG`, its `isUTF8()` being `true`); `CENNAM` in range;
/// the wave-75 compare answer ([`zip_coder_compare_answer`]: an all-ASCII name
/// and a range `Arrays.mismatch` accepts). `EXACT_MATCH` is
/// [`EntryPosWalk::Exact`]; `NO_MATCH` continues the walk, natively.
/// Declined (the body runs from its first line, having observed nothing):
/// any other coder (the default `ZipFile` / `JarFile` coder is the UTF-8
/// one; a `ZipFile(file, charset)` with another charset is not), a null
/// `cen`, a `CENNAM` or `entries[idx + 2]` out of range (the body's
/// exception), a compare the answer refuses (non-ASCII name: the JDK
/// encodes), and every `DIRECTORY_MATCH` (its answer is `name + "/"`, a new
/// `String`; rare on a class path).
fn zip_entry_pos_walk(ctx: &dyn NativeContext, args: &[Value], hits: bool) -> EntryPosWalk {
    zip_entry_pos_walk_opt(ctx, args, hits).unwrap_or(EntryPosWalk::Decline)
}

fn zip_entry_pos_walk_opt(
    ctx: &dyn NativeContext,
    args: &[Value],
    hits: bool,
) -> Option<EntryPosWalk> {
    let this = arg_obj(args, 0)?;
    let slots = zip_source_slots(ctx, ctx.class_id_of_object(this))?;
    let mut f = [Value::Object(None); 4];
    ctx.get_fields_typed(
        this,
        &[
            (slots.total, b'I'),
            (slots.table, b'['),
            (slots.tablelen, b'I'),
            (slots.entries, b'['),
        ],
        &mut f,
    );
    let Value::Int(total) = f[0] else {
        return None;
    };
    if total == 0 {
        return Some(EntryPosWalk::Null);
    }
    let (Value::Object(Some(table)), Value::Int(tablelen), Value::Object(Some(entries))) =
        (f[1], f[2], f[3])
    else {
        return None;
    };
    if tablelen <= 0 {
        return None;
    }
    let name = arg_obj(args, 1)?;
    let units = ctx.read_string_units(name)?;
    let hsh = zip_coder_hash(&units);
    // `(hsh & 0x7fffffff) % tablelen`: both operands non-negative here, so
    // Rust's `%` is Java's.
    let head = ((hsh & 0x7fff_ffff) % tablelen) as usize;
    let mut first = [0i32; 1];
    if ctx.read_int_array_into(table, head, &mut first) != 1 {
        return None;
    }
    let mut idx = first[0];
    // A well-formed chain visits each entry at most once (`entries` holds
    // three ints per entry); more links than that is a cycle.
    let mut budget = ctx.array_length(entries) / 3 + 1;
    // The hit answer's inputs, read at the first hash match only.
    let mut hit: Option<(ObjectRef, Value, Value)> = None;
    while idx != ZIP_ENDCHAIN {
        if idx < 0 || budget == 0 {
            return None;
        }
        budget -= 1;
        let mut link = [0i32; 2];
        if ctx.read_int_array_into(entries, idx as usize, &mut link) != 2 {
            return None;
        }
        if link[0] == hsh {
            if !hits {
                return None;
            }
            let (cen, coder, add_slash) = match hit {
                Some(h) => h,
                None => {
                    let layout = zip_hit_layout(ctx, &slots)?;
                    let coder = arg_obj(args, 3)?;
                    if ctx.class_id_of_object(coder) != layout.utf8_coder {
                        return None;
                    }
                    let Value::Object(Some(cen)) = ctx.get_field(this, layout.cen) else {
                        return None;
                    };
                    let h = (cen, Value::Object(Some(coder)), args.get(2).copied()?);
                    hit = Some(h);
                    h
                }
            };
            let mut pos = [0i32; 1];
            if ctx.read_int_array_into(entries, idx as usize + 2, &mut pos) != 1 {
                return None;
            }
            let pos = pos[0];
            let nlen = zip_le_at(ctx, Some(cen), pos.wrapping_add(28), 2)? as i32;
            let compare_args = [
                coder,
                Value::Object(Some(name)),
                Value::Object(Some(cen)),
                Value::Int(pos.wrapping_add(46)),
                Value::Int(nlen),
                add_slash,
            ];
            match zip_coder_compare_answer(ctx, &compare_args)? {
                ZIP_CODER_EXACT_MATCH => return Some(EntryPosWalk::Exact(pos)),
                ZIP_CODER_NO_MATCH => {}
                _ => return None,
            }
        }
        idx = link[1];
    }
    Some(EntryPosWalk::Null)
}

/// `ZipFile$Source.getEntryPos(String, boolean, ZipCoder)` as an `Intrinsic`
/// (`CRATONVM_ZIP_ENTRY_MISS_INTRINSIC`; interpreter round i3 wave 76, lane
/// L5): the entry miss every class-path jar before a class's own jar pays,
/// answered without the ~15-20 interpreted frames of the walk
/// (`ZipCoder.hash`, `String.hashCode`, `getEntryHash` / `getEntryNext` per
/// link). Everything else runs the JDK body (`invoke_special_bytecode_only`
/// executes the bytecode, not this native), so a hit, its compare and its
/// `EntryPos` are the JDK's own.
fn zip_source_get_entry_pos(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if zip_internals_are_jdk25(&*ctx) && zip_entry_pos_miss(ctx, args).is_some() {
        return Ok(Some(Value::Object(None)));
    }
    ctx.invoke_special_bytecode_only(
        ZIP_SOURCE,
        "getEntryPos",
        ZIP_SOURCE_GET_ENTRY_POS_DESC,
        args,
    )
}

/// `new EntryPos(name, pos)` for the caller's own `name` reference (the JDK
/// body passes `name` itself, not a copy), or `None` when the record cannot
/// be built here (no layout). The canonical constructor stores exactly the
/// two fields, so the object is the body's. `name` is pinned across the
/// allocation (a collection point) and re-read after it; nothing allocates
/// between the allocation and the two stores.
fn zip_entry_pos_record(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    pos: i32,
) -> Option<ObjectRef> {
    let this = arg_obj(args, 0)?;
    let name = arg_obj(args, 1)?;
    let slots = zip_source_slots(&*ctx, ctx.class_id_of_object(this))?;
    let layout = zip_hit_layout(&*ctx, &slots)?;
    let pin = ctx.pin_native_root(name);
    let record = ctx.alloc_object(layout.entry_pos, layout.entry_pos_fields);
    let name = ctx.read_native_pin(pin, name);
    ctx.unpin_native_roots(pin);
    ctx.set_field(record, layout.name, Value::Object(Some(name)));
    ctx.set_field(record, layout.pos, Value::Int(pos));
    Some(record)
}

/// The same intrinsic with `CRATONVM_ZIP_ENTRY_HIT_INTRINSIC` (interpreter
/// round i3 wave 77, lane L5): a hit is answered too (see
/// [`zip_entry_pos_walk`]), so the chain is walked once -- the miss
/// intrinsic alone walks a hit natively up to its hash match and then the
/// JDK body walks it again from the head, which is what made `zip-get-entry`
/// slower with it on (wave 76). What it does not answer, the JDK body does,
/// from its first line.
fn zip_source_get_entry_pos_hits(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let walk = if zip_internals_are_jdk25(&*ctx) {
        zip_entry_pos_walk(&*ctx, args, true)
    } else {
        EntryPosWalk::Decline
    };
    match walk {
        EntryPosWalk::Null => return Ok(Some(Value::Object(None))),
        EntryPosWalk::Exact(pos) => {
            if let Some(record) = zip_entry_pos_record(ctx, args, pos) {
                return Ok(Some(Value::Object(Some(record))));
            }
        }
        EntryPosWalk::Decline => {}
    }
    ctx.invoke_special_bytecode_only(
        ZIP_SOURCE,
        "getEntryPos",
        ZIP_SOURCE_GET_ENTRY_POS_DESC,
        args,
    )
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register_zip_real_natives(r: &mut NativeMethodRegistry) {
    // `CRATONVM_ZIP_UTILS_INTRINSICS` (default on since the wave-74 triage;
    // read once per registry): the two little-endian field readers every
    // `ZipFile` CEN / LOC parse goes through. `Intrinsic`, stated: a reviewed
    // fast path over the JDK's own bytecode, which it calls back for every
    // case it does not answer.
    //
    // `CRATONVM_JIT_ZIP_HEADER_LOADS` (default ON since the round's close, `=0` kill; JIT round 19 proposal
    // wave 10, lane zip10, i75-L5): in a VM with a JIT, neither this pair nor
    // the CEN / LOC accessors below register, and the JDK's own `ZipUtils`
    // bytecode runs -- HotSpot's arrangement. A registered native on the
    // triple makes the JIT refuse to splice or bind any site that reaches it
    // (`native-shadow`), so a compiled `getZipEntry` paid a native-door
    // crossing per CEN field; the bytecode (`Preconditions.checkIndex` twice
    // and `Unsafe.getShortUnaligned` / `getIntUnaligned`) is what the
    // optimizing tier answers inline. Nothing changes without a JIT.
    let header_reads_yield = r.zip_header_reads_yield_to_jit();
    if r.zip_utils_intrinsics() && !header_reads_yield {
        r.register_with_kind(
            ZIP_UTILS,
            "get16",
            "([BI)I",
            zip_utils_get16,
            NativeKind::Intrinsic,
        );
        r.register_with_kind(
            ZIP_UTILS,
            "get32",
            "([BI)J",
            zip_utils_get32,
            NativeKind::Intrinsic,
        );
    }
    // `CRATONVM_ZIP_CEN_ACCESSOR_INTRINSICS` (default off; read once per
    // registry; interpreter round i3 wave 75, lane L5): the `ZipUtils` CEN /
    // LOC header accessors themselves (`CENFLG`, `CENNAM`, ..., `LOCEXT`), so
    // a `ZipFile.getEntry` hit's ~10 field reads and a `getInputStream`'s LOC
    // read run no interpreted accessor frame. `Intrinsic`, stated, as above;
    // 21 rows when on (a flip raises `BASELINE_INTRINSICS` by 21).
    // Not registered under `CRATONVM_JIT_ZIP_HEADER_LOADS` with a JIT (above).
    if r.zip_cen_accessor_intrinsics() && !header_reads_yield {
        for (f, callback) in ZIP_HEADER_ACCESSORS {
            r.register_with_kind(
                ZIP_UTILS,
                f.name,
                f.descriptor,
                *callback,
                NativeKind::Intrinsic,
            );
        }
    }
    // `CRATONVM_ZIP_CODER_COMPARE_INTRINSIC` (default on since the wave-75
    // triage; read once per registry; same wave): the UTF-8 entry-name
    // compare of every CEN hash hit, without the encode. `Intrinsic`,
    // stated; 1 row when on.
    if r.zip_coder_compare_intrinsic() {
        r.register_with_kind(
            ZIP_CODER_UTF8,
            "compare",
            ZIP_CODER_COMPARE_DESC,
            zip_coder_utf8_compare,
            NativeKind::Intrinsic,
        );
    }
    // `CRATONVM_ZIP_ENTRY_MISS_INTRINSIC` (default off; read once per
    // registry; interpreter round i3 wave 76, lane L5): the no-hash-match
    // miss of `ZipFile$Source.getEntryPos`, answered without the Java walk;
    // the JDK body for everything else. `CRATONVM_ZIP_ENTRY_HIT_INTRINSIC`
    // (default off; wave 77, lane L5) registers the same row with the body
    // that answers a hit too (it wins over the miss switch: it answers every
    // miss the miss body does). `Intrinsic`, stated; 1 row when either is on
    // (a flip raises `BASELINE_INTRINSICS` by 1). The hit body is default on
    // since the wave-77 triage and registered only in a VM without a JIT:
    // `--nojit` zip-get-entry 10322 -> 7819 ns, zip-miss-collide 4667 ->
    // 1961; with the JIT the compiled walk wins (zip-miss-entry 88 -> 409).
    //
    // `CRATONVM_INTRINSIC_BODY_COMPILES` (default off; interpreter round i3
    // wave 79, lane L5): the hit body registers with a JIT too, marked
    // body-compilable, so the JIT still compiles `getEntryPos` and binds
    // compiled callers to it while interpreted calls take the intrinsic.
    if r.zip_entry_hit_intrinsic() && r.interpreted_intrinsic_registers() {
        r.register_with_kind(
            ZIP_SOURCE,
            "getEntryPos",
            ZIP_SOURCE_GET_ENTRY_POS_DESC,
            zip_source_get_entry_pos_hits,
            NativeKind::Intrinsic,
        );
        r.mark_body_compilable_intrinsic(ZIP_SOURCE, "getEntryPos", ZIP_SOURCE_GET_ENTRY_POS_DESC);
    } else if r.zip_entry_miss_intrinsic() {
        r.register_with_kind(
            ZIP_SOURCE,
            "getEntryPos",
            ZIP_SOURCE_GET_ENTRY_POS_DESC,
            zip_source_get_entry_pos,
            NativeKind::Intrinsic,
        );
    }
    // Inflater
    let il = "java/util/zip/Inflater";
    r.register_with_kind(il, "initIDs", "()V", infl_init_ids, NativeKind::Bridge);
    r.register_with_kind(il, "init", "(Z)J", infl_init, NativeKind::Bridge);
    r.register_with_kind(
        il,
        "setDictionary",
        "(J[BII)V",
        infl_set_dictionary,
        NativeKind::Bridge,
    );
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
    r.register_with_kind(dl, "init", "(IIZ)J", defl_init, NativeKind::Bridge);
    r.register_with_kind(
        dl,
        "setDictionary",
        "(J[BII)V",
        defl_set_dictionary,
        NativeKind::Bridge,
    );
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
    r.register_with_kind(
        crc,
        "updateBytes0",
        "(I[BII)I",
        crc32_update_bytes_0,
        NativeKind::Bridge,
    );
    r.register_with_kind(
        crc,
        "updateByteBuffer0",
        "(IJII)I",
        crc32_update_byte_buffer_0,
        NativeKind::Bridge,
    );

    // Adler32 — the JDK 25 `java.util.zip.Adler32` natives. All three are
    // static, take the running (== public) checksum and return the new one.
    // The phases_late synthetic registers instance-level
    // `<init>`/`update(I)V`/`update([BII)V`/`getValue`/`reset` against a
    // synthetic 1-field Long layout; those descriptors are disjoint from the
    // static ones below, so both sets coexist and each mode uses its own.
    let ad = "java/util/zip/Adler32";
    r.register_with_kind(
        ad,
        "update",
        "(II)I",
        adler32_update_int,
        NativeKind::Bridge,
    );
    r.register_with_kind(
        ad,
        "updateBytes",
        "(I[BII)I",
        adler32_update_bytes,
        NativeKind::Bridge,
    );
    r.register_with_kind(
        ad,
        "updateByteBuffer",
        "(IJII)I",
        adler32_update_byte_buffer,
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

    /// Interpreter round i3 wave 73, lane L5 (`CRATONVM_INFLATER_REUSE_BUFFERS`):
    /// a raw stream inflated in uneven slices (700-byte input windows, 3000-byte
    /// output windows at moving offsets, so the kept buffers hold stale bytes
    /// from the previous call) answers the same packed results, the same output
    /// bytes and the same `getAdler()` through the scratch buffers as through
    /// fresh ones; and a call larger than the cap (one 70 000-byte window)
    /// takes the allocating path with the switch on and still agrees.
    #[test]
    fn i73_reused_inflater_buffers_inflate_exactly_as_fresh_ones() {
        let original: Vec<u8> = (0..20_000u32)
            .map(|i| ((i * 7) % 251) as u8 ^ (i / 300) as u8)
            .collect();
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&original).unwrap();
        let compressed = enc.finish().unwrap();

        fn run(
            reuse: bool,
            compressed: &[u8],
            room: usize,
            in_step: usize,
            out_step: usize,
        ) -> (Vec<i64>, Vec<u8>, i32) {
            let mut ctx = mock_ctx();
            let addr = match infl_init(&mut ctx, &[Value::Int(1)]).unwrap().unwrap() {
                Value::Long(a) => a,
                other => panic!("expected Long handle, got {other:?}"),
            };
            let input = ctx.new_array(ArrayElementType::Byte, compressed.len());
            for (i, b) in compressed.iter().enumerate() {
                ctx.set_array_element(input, i, Value::Int(*b as i8 as i32));
            }
            let out = ctx.new_array(ArrayElementType::Byte, room);
            let mut packs = Vec::new();
            let mut in_pos = 0usize;
            let mut out_pos = 0usize;
            for _ in 0..10_000 {
                let in_len = (compressed.len() - in_pos).min(in_step);
                let out_len = (room - out_pos).min(out_step);
                let packed = match inflate_bytes_bytes(
                    &mut ctx,
                    &[
                        Value::Object(None),
                        Value::Long(addr),
                        Value::Object(Some(input)),
                        Value::Int(in_pos as i32),
                        Value::Int(in_len as i32),
                        Value::Object(Some(out)),
                        Value::Int(out_pos as i32),
                        Value::Int(out_len as i32),
                    ],
                    reuse,
                )
                .unwrap()
                .unwrap()
                {
                    Value::Long(p) => p,
                    other => panic!("expected Long result, got {other:?}"),
                };
                packs.push(packed);
                let p = packed as u64;
                in_pos += (p & 0x7FFF_FFFF) as usize;
                out_pos += ((p >> 31) & 0x7FFF_FFFF) as usize;
                if (p >> 62) & 1 == 1 {
                    break;
                }
            }
            let adler = match infl_get_adler(&mut ctx, &[Value::Long(addr)])
                .unwrap()
                .unwrap()
            {
                Value::Int(a) => a,
                other => panic!("expected Int adler, got {other:?}"),
            };
            let bytes = read_byte_array(&ctx, out, 0, out_pos);
            infl_end(&mut ctx, &[Value::Long(addr)]).unwrap();
            (packs, bytes, adler)
        }

        let room = original.len() + 64;
        let fresh = run(false, &compressed, room, 700, 3000);
        assert_eq!(fresh.1, original);
        assert!(fresh.0.len() > 2, "the slices make several calls");
        assert_eq!(run(true, &compressed, room, 700, 3000), fresh);

        let big_room = 70_000;
        assert!(big_room > INFLATER_SCRATCH_CAP);
        let fresh_big = run(false, &compressed, big_room, compressed.len(), big_room);
        assert_eq!(fresh_big.1, original);
        assert_eq!(
            run(true, &compressed, big_room, compressed.len(), big_room),
            fresh_big
        );
    }

    /// What one stream driven the way `Inflater` drives it answers: the packed
    /// result of every call, the bytes produced, `getAdler()` at the end, and
    /// the index of the call that threw (if one did).
    type Driven = (Vec<i64>, Vec<u8>, i32, Option<usize>);

    /// Drives one stream through `inflateBytesBytes` as `Inflater.inflate`
    /// does -- every call hands over ALL the input not consumed yet (the
    /// `setInput` buffer from `inputPos` to `inputLim`) and asks for the
    /// next `out_steps` window -- with `CRATONVM_INFLATER_INPUT_WINDOW`'s
    /// choice passed in; answers a needDict with `dict`.
    fn drive_inflate(
        window: bool,
        nowrap: bool,
        compressed: &[u8],
        dict: Option<&[u8]>,
        out_steps: &[usize],
        room: usize,
    ) -> Driven {
        let mut ctx = mock_ctx();
        let addr = match infl_init(&mut ctx, &[Value::Int(i32::from(nowrap))])
            .unwrap()
            .unwrap()
        {
            Value::Long(a) => a,
            other => panic!("expected Long handle, got {other:?}"),
        };
        let input = byte_array(&mut ctx, compressed);
        let out = ctx.new_array(ArrayElementType::Byte, room);
        let mut packs = Vec::new();
        let mut thrown = None;
        let (mut in_pos, mut out_pos) = (0usize, 0usize);
        for call in 0..200_000usize {
            let out_len = (room - out_pos).min(out_steps[call % out_steps.len()]);
            let result = inflate_bytes_bytes_with(
                &mut ctx,
                &[
                    Value::Object(None),
                    Value::Long(addr),
                    Value::Object(Some(input)),
                    Value::Int(in_pos as i32),
                    Value::Int((compressed.len() - in_pos) as i32),
                    Value::Object(Some(out)),
                    Value::Int(out_pos as i32),
                    Value::Int(out_len as i32),
                ],
                false,
                window,
            );
            let packed = match result {
                Ok(Some(Value::Long(p))) => p,
                Ok(other) => panic!("expected Long result, got {other:?}"),
                Err(_) => {
                    thrown = Some(call);
                    break;
                }
            };
            packs.push(packed);
            let p = packed as u64;
            in_pos += (p & 0x7FFF_FFFF) as usize;
            out_pos += ((p >> 31) & 0x7FFF_FFFF) as usize;
            if (p >> 62) & 1 == 1 {
                break;
            }
            if (p >> 63) & 1 == 1 {
                let d = dict.expect("a needDict without a dictionary");
                let d_arr = byte_array(&mut ctx, d);
                infl_set_dictionary(
                    &mut ctx,
                    &[
                        Value::Long(addr),
                        Value::Object(Some(d_arr)),
                        Value::Int(0),
                        Value::Int(d.len() as i32),
                    ],
                )
                .unwrap();
            } else if packed == 0 {
                // No progress at all: the input ran out (or the room did).
                break;
            }
        }
        let adler = match infl_get_adler(&mut ctx, &[Value::Long(addr)])
            .unwrap()
            .unwrap()
        {
            Value::Int(a) => a,
            other => panic!("expected Int adler, got {other:?}"),
        };
        // On a throw the bytes the failing call wrote are in the array too:
        // compare the whole room then.
        let shown = if thrown.is_some() { room } else { out_pos };
        let bytes = read_byte_array(&ctx, out, 0, shown);
        infl_end(&mut ctx, &[Value::Long(addr)]).unwrap();
        (packs, bytes, adler, thrown)
    }

    /// Interpreter round i3 wave 75, lane L5 (`CRATONVM_INFLATER_INPUT_WINDOW`):
    /// a stream inflated through bounded input windows answers, call for
    /// call, exactly what the whole-input call answers -- the packed result
    /// (input consumed, output produced, finished, needDict), the bytes, and
    /// `getAdler()` -- over 1-byte and odd-sized output calls with the whole
    /// remaining input handed over each time (`InflaterInputStream.read()`'s
    /// shape), for a raw stream, a zlib-wrapped one, a stored (level 0) one
    /// whose output needs more input than one window, a preset-dictionary
    /// one, and a corrupt one (the same call throws, with the same bytes
    /// written).
    #[test]
    fn i75_windowed_inflate_matches_the_whole_input_call() {
        assert_eq!(inflater_input_window(20_000, 1), INFLATER_INPUT_WINDOW_MIN);
        assert_eq!(inflater_input_window(20_000, 4096), 4096);
        assert_eq!(inflater_input_window(300, 4096), 300);

        let original: Vec<u8> = (0..20_000u32)
            .map(|i| ((i * 7) % 251) as u8 ^ (i / 300) as u8 ^ ((i * i) % 7) as u8)
            .collect();
        let room = original.len() + 64;
        let steps: &[&[usize]] = &[
            &[1],
            &[1, 1, 7, 1, 300, 1, 4096, 2],
            &[3, 1000, 1, 513],
            &[600],
            &[65_536],
        ];
        let check = |name: &str, nowrap: bool, compressed: &[u8], dict: Option<&[u8]>| {
            assert!(
                compressed.len() > 2 * INFLATER_INPUT_WINDOW_MIN,
                "{name}: too short to window"
            );
            for out_steps in steps {
                let whole = drive_inflate(false, nowrap, compressed, dict, out_steps, room);
                let windowed = drive_inflate(true, nowrap, compressed, dict, out_steps, room);
                assert_eq!(windowed, whole, "{name} {out_steps:?}");
                assert!(
                    whole.0.len() > 1 || out_steps[0] >= original.len(),
                    "{name}: several calls"
                );
            }
        };

        let mut raw = DeflateEncoder::new(Vec::new(), Compression::default());
        raw.write_all(&original).unwrap();
        let raw = raw.finish().unwrap();
        check("raw", true, &raw, None);
        assert_eq!(
            drive_inflate(true, true, &raw, None, &[1, 5], room).1,
            original
        );

        let mut wrapped = flate2::write::ZlibEncoder::new(Vec::new(), Compression::default());
        wrapped.write_all(&original).unwrap();
        let wrapped = wrapped.finish().unwrap();
        check("wrapped", false, &wrapped, None);

        let mut stored = DeflateEncoder::new(Vec::new(), Compression::none());
        stored.write_all(&original).unwrap();
        let stored = stored.finish().unwrap();
        check("stored", true, &stored, None);
        assert_eq!(
            drive_inflate(true, true, &stored, None, &[5000], room).1,
            original
        );

        let dict: Vec<u8> = (0..300u32).map(|i| ((i * 7) % 251) as u8).collect();
        let mut c = Compress::new(Compression::default(), true);
        c.set_dictionary(&dict).unwrap();
        let mut with_dict = Vec::with_capacity(original.len() * 2 + 1024);
        let status = c
            .compress_vec(&original, &mut with_dict, FlushCompress::Finish)
            .unwrap();
        assert_eq!(status, flate2::Status::StreamEnd);
        check("dictionary", false, &with_dict, Some(dict.as_slice()));
        assert_eq!(
            drive_inflate(
                true,
                false,
                &with_dict,
                Some(dict.as_slice()),
                &[1, 9],
                room
            )
            .1,
            original
        );

        let mut corrupt = raw.clone();
        let mid = corrupt.len() / 2;
        for b in &mut corrupt[mid..mid + 16] {
            *b ^= 0xA5;
        }
        for out_steps in steps {
            let whole = drive_inflate(false, true, &corrupt, None, out_steps, room);
            let windowed = drive_inflate(true, true, &corrupt, None, out_steps, room);
            assert_eq!(windowed, whole, "corrupt {out_steps:?}");
        }
    }

    /// Interpreter round i3 wave 74, lane L5 (`CRATONVM_INFLATER_RAW_ADLER_ZERO`):
    /// with the switch a raw (`nowrap`) stream answers `getAdler()` = 0 fresh,
    /// after inflating (through both buffer paths) and after `reset()` --
    /// zlib's raw `strm->adler` -- while the packed results and the bytes are
    /// the same as without it; a zlib-wrapped stream keeps its running
    /// checksum (the trailer's ADLER-32) either way; and without the switch a
    /// raw stream still answers the running checksum (seed 1).
    #[test]
    fn i74_raw_inflater_keeps_no_adler_with_the_switch() {
        let original: Vec<u8> = (0..9_000u32).map(|i| ((i * 13) % 241) as u8).collect();
        let mut raw = DeflateEncoder::new(Vec::new(), Compression::default());
        raw.write_all(&original).unwrap();
        let raw = raw.finish().unwrap();
        let mut wrapped = flate2::write::ZlibEncoder::new(Vec::new(), Compression::default());
        wrapped.write_all(&original).unwrap();
        let wrapped = wrapped.finish().unwrap();
        let expected_adler = adler32_update(1, &original) as i32;

        fn adler_of(ctx: &mut crate::test_utils::MockNativeContext, addr: i64) -> i32 {
            match infl_get_adler(ctx, &[Value::Long(addr)]).unwrap().unwrap() {
                Value::Int(a) => a,
                other => panic!("expected Int adler, got {other:?}"),
            }
        }

        /// (fresh adler, packed results, bytes, adler after, adler after reset)
        fn run(
            nowrap: bool,
            raw_adler_zero: bool,
            reuse: bool,
            stream: &[u8],
            n: usize,
        ) -> (i32, Vec<i64>, Vec<u8>, i32, i32) {
            let mut ctx = mock_ctx();
            let addr = inflater_init(nowrap, raw_adler_zero);
            let fresh = adler_of(&mut ctx, addr);
            let input = ctx.new_array(ArrayElementType::Byte, stream.len());
            for (i, b) in stream.iter().enumerate() {
                ctx.set_array_element(input, i, Value::Int(*b as i8 as i32));
            }
            let out = ctx.new_array(ArrayElementType::Byte, n + 16);
            let (mut in_pos, mut out_pos, mut packs) = (0usize, 0usize, Vec::new());
            for _ in 0..1_000 {
                let packed = match inflate_bytes_bytes(
                    &mut ctx,
                    &[
                        Value::Object(None),
                        Value::Long(addr),
                        Value::Object(Some(input)),
                        Value::Int(in_pos as i32),
                        Value::Int((stream.len() - in_pos).min(512) as i32),
                        Value::Object(Some(out)),
                        Value::Int(out_pos as i32),
                        Value::Int((n + 16 - out_pos).min(2048) as i32),
                    ],
                    reuse,
                )
                .unwrap()
                .unwrap()
                {
                    Value::Long(p) => p,
                    other => panic!("expected Long result, got {other:?}"),
                };
                packs.push(packed);
                let p = packed as u64;
                in_pos += (p & 0x7FFF_FFFF) as usize;
                out_pos += ((p >> 31) & 0x7FFF_FFFF) as usize;
                if (p >> 62) & 1 == 1 {
                    break;
                }
            }
            let after = adler_of(&mut ctx, addr);
            let bytes = read_byte_array(&ctx, out, 0, out_pos);
            infl_reset(&mut ctx, &[Value::Long(addr)]).unwrap();
            let after_reset = adler_of(&mut ctx, addr);
            infl_end(&mut ctx, &[Value::Long(addr)]).unwrap();
            (fresh, packs, bytes, after, after_reset)
        }

        let n = original.len();
        for reuse in [false, true] {
            let off = run(true, false, reuse, &raw, n);
            assert_eq!(off.2, original);
            assert_eq!((off.0, off.3, off.4), (1, expected_adler, 1));
            let on = run(true, true, reuse, &raw, n);
            assert_eq!((on.0, on.3, on.4), (0, 0, 0));
            assert_eq!((&on.1, &on.2), (&off.1, &off.2));

            for raw_adler_zero in [false, true] {
                let w = run(false, raw_adler_zero, reuse, &wrapped, n);
                assert_eq!(w.2, original);
                assert_eq!((w.0, w.3, w.4), (1, expected_adler, 1));
            }
        }
    }

    /// Interpreter round i3 wave 74, lane L5 (`CRATONVM_ZIP_UTILS_INTRINSICS`):
    /// `get16` / `get32` answer the little-endian unsigned field in range (at
    /// the first and the last possible offset, with high bits set so a sign
    /// extension would show), and decline everything the JDK body must answer
    /// (negative offset, a range past the end by one, a null array).
    #[test]
    fn i74_zip_utils_reads_answer_the_little_endian_field() {
        let mut ctx = mock_ctx();
        let bytes: [u8; 6] = [0xFE, 0xCA, 0x81, 0x92, 0xA3, 0xF4];
        let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
        for (i, b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
        }
        let at = |off: i32| [Value::Object(Some(arr)), Value::Int(off)];
        assert_eq!(zip_utils_read_le(&ctx, &at(0), 2), Some(0xCAFE));
        assert_eq!(zip_utils_read_le(&ctx, &at(4), 2), Some(0xF4A3));
        assert_eq!(zip_utils_read_le(&ctx, &at(0), 4), Some(0x9281_CAFE));
        assert_eq!(zip_utils_read_le(&ctx, &at(2), 4), Some(0xF4A3_9281));
        assert_eq!(
            zip_utils_get16(&mut ctx, &at(4)).unwrap(),
            Some(Value::Int(0xF4A3))
        );
        assert_eq!(
            zip_utils_get32(&mut ctx, &at(2)).unwrap(),
            Some(Value::Long(0xF4A3_9281))
        );
        assert_eq!(zip_utils_read_le(&ctx, &at(5), 2), None);
        assert_eq!(zip_utils_read_le(&ctx, &at(3), 4), None);
        assert_eq!(zip_utils_read_le(&ctx, &at(-1), 2), None);
        assert_eq!(
            zip_utils_read_le(&ctx, &[Value::Object(None), Value::Int(0)], 2),
            None
        );
    }

    /// The `ZipUtils` intrinsics are registered only with the switch on (set
    /// explicitly both ways: the default flipped on in the wave-74 triage).
    #[test]
    fn i74_zip_utils_intrinsics_register_only_with_the_switch() {
        for (word, want) in [(Some("0"), false), (Some("1"), true)] {
            let found = cratonvm_types::flags::with_thread_overrides(
                // The registry defaults to a JIT; under the default-ON
                // `CRATONVM_JIT_ZIP_HEADER_LOADS` `get16` / `get32` yield to
                // the JDK bytecode, so this test of their own switch turns it off.
                &[
                    ("CRATONVM_ZIP_UTILS_INTRINSICS", word),
                    ("CRATONVM_JIT_ZIP_HEADER_LOADS", Some("0")),
                ],
                || {
                    let mut r = NativeMethodRegistry::new();
                    register_zip_real_natives(&mut r);
                    (
                        r.find(ZIP_UTILS, "get16", "([BI)I").is_some(),
                        r.find(ZIP_UTILS, "get32", "([BI)J").is_some(),
                    )
                },
            );
            assert_eq!(found, (want, want), "switch {word:?}");
        }
    }

    /// JIT round 19 proposal wave 10, lane zip10 (i75-L5,
    /// `CRATONVM_JIT_ZIP_HEADER_LOADS`): with the switch on and a JIT, neither
    /// `get16` / `get32` nor any CEN / LOC accessor is registered (the JDK
    /// bytecode runs); without a JIT, or with the switch off, both families
    /// register exactly as their own switches say. Every word set explicitly.
    #[test]
    fn r19_zip10_header_loads_yield_the_field_reads_only_under_a_jit() {
        for (word, jit, want) in [
            ("0", true, true),
            ("0", false, true),
            ("1", false, true),
            ("1", true, false),
        ] {
            let edits = [
                ("CRATONVM_JIT_ZIP_HEADER_LOADS", Some(word)),
                ("CRATONVM_ZIP_UTILS_INTRINSICS", Some("1")),
                ("CRATONVM_ZIP_CEN_ACCESSOR_INTRINSICS", Some("1")),
                ("CRATONVM_ZIP_CODER_COMPARE_INTRINSIC", Some("1")),
            ];
            let (utils, accessors, compare) =
                cratonvm_types::flags::with_thread_overrides(&edits, || {
                    let mut r = NativeMethodRegistry::new();
                    r.set_jit_enabled(jit);
                    assert_eq!(r.jit_zip_header_loads(), word == "1");
                    assert_eq!(r.zip_header_reads_yield_to_jit(), word == "1" && jit);
                    register_zip_real_natives(&mut r);
                    let utils = [("get16", "([BI)I"), ("get32", "([BI)J")]
                        .iter()
                        .map(|(m, d)| r.kind_of(ZIP_UTILS, m, d))
                        .collect::<Vec<_>>();
                    let accessors = ZIP_HEADER_ACCESSORS
                        .iter()
                        .map(|(f, _)| r.kind_of(ZIP_UTILS, f.name, f.descriptor))
                        .collect::<Vec<_>>();
                    let compare = r.kind_of(ZIP_CODER_UTF8, "compare", ZIP_CODER_COMPARE_DESC);
                    (utils, accessors, compare)
                });
            let k = want.then_some(NativeKind::Intrinsic);
            assert!(
                utils.iter().all(|x| *x == k),
                "word {word} jit {jit}: {utils:?}"
            );
            assert!(
                accessors.iter().all(|x| *x == k),
                "word {word} jit {jit}: {accessors:?}"
            );
            // The name compare is not a field read: it stays registered.
            assert_eq!(
                compare,
                Some(NativeKind::Intrinsic),
                "word {word} jit {jit}"
            );
        }
    }

    /// Interpreter round i3 wave 75, lane L5
    /// (`CRATONVM_ZIP_CEN_ACCESSOR_INTRINSICS`): every accessor in the table
    /// reads its own field. The headers are built field by field in the
    /// APPNOTE 4.3.7 / 4.3.12 order (no offsets written here: each field's
    /// offset is wherever the fields before it end), with a distinct value per
    /// field whose high bits are set, so a wrong offset, width or sign
    /// extension shows. The CEN header sits at a non-zero `pos`.
    #[test]
    fn i75_zip_header_accessors_read_their_own_field() {
        // (name, width, value) in header order.
        let cen_fields: [(&str, usize, u32); 16] = [
            ("CENSIG", 4, 0x0201_4B50),
            ("CENVEM", 2, 0x83A1),
            ("CENVER", 2, 0x8414),
            ("CENFLG", 2, 0x8808),
            ("CENHOW", 2, 0x8C08),
            ("CENTIM", 4, 0x9A5B_C3D7),
            ("CENCRC", 4, 0xDEAD_BEEF),
            ("CENSIZ", 4, 0xF100_0601),
            ("CENLEN", 4, 0xF200_0FDD),
            ("CENNAM", 2, 0x9017),
            ("CENEXT", 2, 0x9124),
            ("CENCOM", 2, 0x9235),
            ("CENDSK", 2, 0x9346),
            ("CENATT", 2, 0x9457),
            ("CENATX", 4, 0xA1ED_C0DE),
            ("CENOFF", 4, 0xF3AB_CDEF),
        ];
        let loc_fields: [(&str, usize, u32); 10] = [
            ("LOCSIG", 4, 0x0403_4B50),
            ("LOCVER", 2, 0x8014),
            ("LOCFLG", 2, 0x8108),
            ("LOCHOW", 2, 0x8208),
            ("LOCTIM", 4, 0x9A5B_C3D7),
            ("LOCCRC", 4, 0xCAFE_F00D),
            ("LOCSIZ", 4, 0xF400_0601),
            ("LOCLEN", 4, 0xF500_0FDD),
            ("LOCNAM", 2, 0x9617),
            ("LOCEXT", 2, 0x9724),
        ];
        let lay_out = |prefix: usize, fields: &[(&str, usize, u32)]| {
            let mut bytes = vec![0x5Au8; prefix];
            let mut at = std::collections::HashMap::new();
            for (name, width, value) in fields {
                at.insert(name.to_string(), (bytes.len() - prefix, *width, *value));
                bytes.extend_from_slice(&value.to_le_bytes()[..*width]);
            }
            bytes.extend_from_slice(&[0xA5; 3]);
            (bytes, at)
        };
        const POS: i32 = 7;
        let (cen, cen_at) = lay_out(POS as usize, &cen_fields);
        assert_eq!(
            cen.len(),
            POS as usize + 46 + 3,
            "a CEN header is CENHDR = 46 bytes"
        );
        let (loc, loc_at) = lay_out(0, &loc_fields);
        assert_eq!(loc.len(), 30 + 3, "a LOC header is LOCHDR = 30 bytes");

        let mut ctx = mock_ctx();
        let cen_arr = byte_array(&mut ctx, &cen);
        let loc_arr = byte_array(&mut ctx, &loc);
        assert_eq!(ZIP_HEADER_ACCESSORS.len(), 21);
        for (f, callback) in ZIP_HEADER_ACCESSORS {
            let expected = match f.name {
                // `Byte.toUnsignedInt(b[pos + 5])`: the high byte of CENVEM.
                "CENVEM_FA" => cen_at["CENVEM"].2 >> 8,
                // `get16(b, pos + 40)`: the high half of CENATX.
                "CENATX_PERMS" => cen_at["CENATX"].2 >> 16,
                name if f.takes_pos() => cen_at[name].2,
                name => loc_at[name].2,
            };
            let width = match f.name {
                "CENVEM_FA" => 1,
                "CENATX_PERMS" => 2,
                name if f.takes_pos() => cen_at[name].1,
                name => loc_at[name].1,
            };
            assert_eq!(f.width, width, "{} width", f.name);
            let args = if f.takes_pos() {
                vec![Value::Object(Some(cen_arr)), Value::Int(POS)]
            } else {
                vec![Value::Object(Some(loc_arr))]
            };
            let want = if f.returns_long() {
                Value::Long(i64::from(expected))
            } else {
                Value::Int(expected as i32)
            };
            assert_eq!(f.answer(&ctx, &args), Some(want), "{}", f.name);
            assert_eq!(
                callback(&mut ctx, &args).unwrap(),
                Some(want),
                "{} callback",
                f.name
            );
            // The descriptor's return type is the JDK's: get32 fields are
            // `long`, get16 / byte fields `int`.
            assert_eq!(f.returns_long(), width == 4, "{} return type", f.name);
        }
    }

    /// The header accessors decline (the JDK body answers) exactly where the
    /// JDK body would throw: a null array, a negative `pos + k`, a `pos + k`
    /// that wraps past `Integer.MAX_VALUE`, a field past the end by one byte.
    #[test]
    fn i75_zip_header_accessors_decline_what_the_jdk_body_throws() {
        let mut ctx = mock_ctx();
        let cen = byte_array(&mut ctx, &[0x11; 46]);
        let field = |name: &str| {
            ZIP_HEADER_ACCESSORS
                .iter()
                .find(|(f, _)| f.name == name)
                .map(|(f, _)| *f)
                .unwrap()
        };
        let at = |pos: i32| vec![Value::Object(Some(cen)), Value::Int(pos)];
        let off = field("CENOFF");
        assert_eq!(off.answer(&ctx, &at(0)), Some(Value::Long(0x1111_1111)));
        assert_eq!(off.answer(&ctx, &at(1)), None, "42 + 4 > 46 - 1");
        assert_eq!(off.answer(&ctx, &at(-42)), Some(Value::Long(0x1111_1111)));
        assert_eq!(off.answer(&ctx, &at(-43)), None, "pos + 42 < 0");
        assert_eq!(
            off.answer(&ctx, &at(i32::MAX)),
            None,
            "pos + 42 wraps negative"
        );
        let fa = field("CENVEM_FA");
        assert_eq!(fa.answer(&ctx, &at(40)), Some(Value::Int(0x11)));
        assert_eq!(fa.answer(&ctx, &at(41)), None, "b[46] is past the end");
        assert_eq!(
            off.answer(&ctx, &[Value::Object(None), Value::Int(0)]),
            None,
            "null array"
        );
        let short_loc = byte_array(&mut ctx, &[0x22; 29]);
        assert_eq!(
            field("LOCEXT").answer(&ctx, &[Value::Object(Some(short_loc))]),
            None,
            "LOCEXT reads bytes 28..30 of a 29-byte array"
        );
        assert_eq!(
            field("LOCNAM").answer(&ctx, &[Value::Object(Some(short_loc))]),
            Some(Value::Int(0x2222))
        );
    }

    /// `CRATONVM_ZIP_CEN_ACCESSOR_INTRINSICS` and
    /// `CRATONVM_ZIP_CODER_COMPARE_INTRINSIC` register their rows, as
    /// `Intrinsic`, only when on; each independent of the other and of
    /// `CRATONVM_ZIP_UTILS_INTRINSICS`.
    #[test]
    fn i75_zip_lookup_intrinsics_register_only_with_their_switch() {
        for (cen, coder) in [("0", "0"), ("1", "0"), ("0", "1"), ("1", "1")] {
            let (rows, compare) = cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_ZIP_CEN_ACCESSOR_INTRINSICS", Some(cen)),
                    ("CRATONVM_ZIP_CODER_COMPARE_INTRINSIC", Some(coder)),
                    ("CRATONVM_ZIP_UTILS_INTRINSICS", Some("0")),
                    // The registry defaults to a JIT; under the default-ON
                    // `CRATONVM_JIT_ZIP_HEADER_LOADS` the accessors yield to the
                    // JDK bytecode, so this test of their own switch turns it off.
                    ("CRATONVM_JIT_ZIP_HEADER_LOADS", Some("0")),
                ],
                || {
                    let mut r = NativeMethodRegistry::new();
                    register_zip_real_natives(&mut r);
                    let rows: Vec<Option<NativeKind>> = ZIP_HEADER_ACCESSORS
                        .iter()
                        .map(|(f, _)| r.kind_of(ZIP_UTILS, f.name, f.descriptor))
                        .collect();
                    (
                        rows,
                        r.kind_of(ZIP_CODER_UTF8, "compare", ZIP_CODER_COMPARE_DESC),
                    )
                },
            );
            let want_rows = (cen == "1").then_some(NativeKind::Intrinsic);
            assert!(rows.iter().all(|k| *k == want_rows), "cen={cen}: {rows:?}");
            assert_eq!(
                compare,
                (coder == "1").then_some(NativeKind::Intrinsic),
                "coder={coder}"
            );
        }
    }

    /// Interpreter round i3 wave 75, lane L5
    /// (`CRATONVM_ZIP_CODER_COMPARE_INTRINSIC`): `UTF8ZipCoder.compare`'s
    /// answer for ASCII names, worked through the JDK body's
    /// `Arrays.mismatch` / `hasTrailingSlash` rule by hand, and the cases left
    /// to the JDK body.
    #[test]
    fn i75_zip_coder_compare_answers_the_jdk_bodys_byte() {
        let mut ctx = mock_ctx();
        // A "CEN" with the encoded names at offsets 3 ("a/b"), 6 ("a/b/"),
        // 10 ("ab/").
        let cen = byte_array(&mut ctx, b"xyza/ba/b/ab/");
        let compare = |ctx: &mut crate::test_utils::MockNativeContext,
                       name: &str,
                       off: i32,
                       len: i32,
                       dir: bool| {
            let s = ctx.create_string(name);
            let args = [
                Value::Object(None), // the coder; unused
                Value::Object(Some(s)),
                Value::Object(Some(cen)),
                Value::Int(off),
                Value::Int(len),
                Value::Int(i32::from(dir)),
            ];
            zip_coder_compare_answer(&*ctx, &args)
        };
        let (exact, directory, none) = (
            Some(ZIP_CODER_EXACT_MATCH),
            Some(ZIP_CODER_DIRECTORY_MATCH),
            Some(ZIP_CODER_NO_MATCH),
        );
        assert_eq!(compare(&mut ctx, "a/b", 3, 3, true), exact);
        assert_eq!(compare(&mut ctx, "a/b", 3, 3, false), exact);
        // "a/b" against "a/b/": mismatch = 3 = len - 1, trailing '/'.
        assert_eq!(compare(&mut ctx, "a/b", 6, 4, true), directory);
        assert_eq!(compare(&mut ctx, "a/b", 6, 4, false), none);
        // "a/b/" against "a/b": a proper prefix the other way round.
        assert_eq!(compare(&mut ctx, "a/b/", 3, 3, true), none);
        // The JDK quirk: "abc" against "ab/" -- same length, the last byte
        // differs (mismatch = 2 = len - 1) and it is '/'.
        assert_eq!(compare(&mut ctx, "abc", 10, 3, true), directory);
        assert_eq!(compare(&mut ctx, "abc", 10, 3, false), none);
        assert_eq!(compare(&mut ctx, "abd", 3, 3, true), none);
        // Empty against empty: equal lengths, no difference.
        assert_eq!(compare(&mut ctx, "", 2, 0, true), exact);
        assert_eq!(
            compare(&mut ctx, "", 3, 1, true),
            none,
            "\"\" vs \"a\": mismatch 0, len 1, no '/'"
        );
        // Left to the JDK body.
        assert_eq!(compare(&mut ctx, "a/\u{e9}", 3, 3, true), None, "non-ASCII");
        assert_eq!(compare(&mut ctx, "a/b", -1, 3, true), None, "off < 0");
        assert_eq!(compare(&mut ctx, "a/b", 3, -1, true), None, "len < 0");
        assert_eq!(compare(&mut ctx, "a/b", 11, 3, true), None, "past the end");
        assert_eq!(
            compare(&mut ctx, "a/b", i32::MAX, 3, true),
            None,
            "off + len overflows"
        );
        let null_name = [
            Value::Object(None),
            Value::Object(None),
            Value::Object(Some(cen)),
            Value::Int(3),
            Value::Int(3),
            Value::Int(1),
        ];
        assert_eq!(
            zip_coder_compare_answer(&ctx, &null_name),
            None,
            "null name"
        );

        // A name longer than the on-stack window.
        let long: String = (0..300).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        let mut bytes = long.clone().into_bytes();
        bytes.push(b'/');
        let long_cen = byte_array(&mut ctx, &bytes);
        let s = ctx.create_string(&long);
        let args = |len: i32, dir: bool| {
            [
                Value::Object(None),
                Value::Object(Some(s)),
                Value::Object(Some(long_cen)),
                Value::Int(0),
                Value::Int(len),
                Value::Int(i32::from(dir)),
            ]
        };
        assert_eq!(zip_coder_compare_answer(&ctx, &args(300, true)), exact);
        assert_eq!(zip_coder_compare_answer(&ctx, &args(301, true)), directory);
        assert_eq!(zip_coder_compare_answer(&ctx, &args(301, false)), none);
    }

    fn int_array(ctx: &mut dyn NativeContext, values: &[i32]) -> ObjectRef {
        let arr = ctx.new_array(ArrayElementType::Int, values.len());
        for (i, v) in values.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*v));
        }
        arr
    }

    /// A `ZipFile$Source` stand-in on the mock: the class with its four walk
    /// fields declared at slots 0..4 (the JDK descriptors), and `table` /
    /// `entries` filled the way JDK 25's `initCEN` fills them: `tablelen =
    /// (total / 2) | 1`, entry `i` at `3 * i` as (hash, next, pos), each entry
    /// pushed at the head of its chain. Answers (receiver, table, entries).
    fn zip_source(
        ctx: &mut crate::test_utils::MockNativeContext,
        names: &[&str],
    ) -> (ObjectRef, ObjectRef, ObjectRef) {
        let cid = ctx
            .ensure_class_initialized(ZIP_SOURCE)
            .expect("Source class");
        let field =
            |name: &str, descriptor: &str, slot_index: usize| cratonvm_native_api::FieldMetadata {
                name: name.to_string(),
                descriptor: descriptor.to_string(),
                access_flags: 0x0002,
                slot_index,
                declaring_class_id: cid,
                is_static: false,
            };
        ctx.set_declared_fields(
            cid,
            vec![
                field("total", "I", 0),
                field("table", "[I", 1),
                field("tablelen", "I", 2),
                field("entries", "[I", 3),
            ],
        );
        let total = names.len();
        let tablelen = (total / 2) | 1;
        let mut table = vec![ZIP_ENDCHAIN; tablelen];
        let mut entries = vec![0i32; 3 * total];
        for (i, name) in names.iter().enumerate() {
            let units: Vec<u16> = name.encode_utf16().collect();
            let hash = zip_coder_hash(&units);
            let head = ((hash & 0x7fff_ffff) as usize) % tablelen;
            entries[3 * i] = hash;
            entries[3 * i + 1] = table[head];
            entries[3 * i + 2] = (46 * i) as i32;
            table[head] = (3 * i) as i32;
        }
        let table = int_array(ctx, &table);
        let entries = int_array(ctx, &entries);
        let this = ctx.alloc_object(cid, 4);
        ctx.set_field(this, 0, Value::Int(total as i32));
        ctx.set_field(this, 1, Value::Object(Some(table)));
        ctx.set_field(this, 2, Value::Int(tablelen as i32));
        ctx.set_field(this, 3, Value::Object(Some(entries)));
        (this, table, entries)
    }

    /// `getEntryPos(name, true, coder)`'s arguments for `this` and `name`.
    fn entry_pos_args(
        ctx: &mut crate::test_utils::MockNativeContext,
        this: ObjectRef,
        name: Option<&str>,
    ) -> [Value; 4] {
        let name = name.map(|n| ctx.create_string_uninterned(n));
        [
            Value::Object(Some(this)),
            Value::Object(name),
            Value::Int(1),
            Value::Object(None),
        ]
    }

    /// Interpreter round i3 wave 76, lane L5
    /// (`CRATONVM_ZIP_ENTRY_MISS_INTRINSIC`): `ZipCoder.hash` is
    /// `String.hashCode` plus a trailing `/` when the name has none, so a name
    /// and its directory form hash alike (the JDK's own design: `getEntry("a")`
    /// finds `a/` through the same chain).
    #[test]
    fn i76_zip_coder_hash_is_the_jdks() {
        let hash = |s: &str| zip_coder_hash(&s.encode_utf16().collect::<Vec<u16>>());
        assert_eq!(hash(""), 0);
        // "a/".hashCode() = 97 * 31 + 47.
        assert_eq!(hash("a/"), 3054);
        assert_eq!(hash("a"), 3054);
        // "Aa" and "BB" share a String.hashCode, and so every name built
        // around them in the same place.
        assert_eq!(hash("q/AaX.class"), hash("q/BBX.class"));
        // A long name wraps as Java's int arithmetic does.
        let long: String = (0..200).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        let java = long
            .encode_utf16()
            .fold(0i32, |h, u| h.wrapping_mul(31).wrapping_add(i32::from(u)));
        assert_eq!(hash(&long), java.wrapping_mul(31).wrapping_add(47));
        // Non-ASCII units hash as UTF-16 units.
        assert_eq!(hash("\u{e9}/"), 0xe9 * 31 + 47);
    }

    /// The intrinsic answers `null` exactly for the names whose hash no
    /// stored entry shares (a model check: a chain holds every entry of its
    /// hash, so "no match on the chain" is "no stored name hashes alike"), and
    /// declines every hash hit -- an exact name, the directory form `q0` of
    /// `q0/`, a String.hashCode collision -- for the JDK body to compare.
    #[test]
    fn i76_zip_entry_miss_answers_only_the_no_hash_match_miss() {
        let mut ctx = mock_ctx();
        let stored = [
            "META-INF/MANIFEST.MF",
            "q0/",
            "q0/P1.class",
            "q0/AaX.class",
            "q0/P2.class",
            "dir/",
            "dir/x.txt",
            "\u{e9}t\u{e9}.txt",
        ];
        let (this, _, _) = zip_source(&mut ctx, &stored);
        let hash = |s: &str| zip_coder_hash(&s.encode_utf16().collect::<Vec<u16>>());
        for name in stored {
            let args = entry_pos_args(&mut ctx, this, Some(name));
            assert_eq!(zip_entry_pos_miss(&ctx, &args), None, "stored {name:?}");
        }
        for hit in ["q0", "dir", "q0/BBX.class"] {
            let args = entry_pos_args(&mut ctx, this, Some(hit));
            assert_eq!(zip_entry_pos_miss(&ctx, &args), None, "hash hit {hit:?}");
        }
        let probes = [
            "q0/P3.class",
            "q1/P1.class",
            "q0/P1.clas",
            "META-INF/",
            "META-INF/INDEX.LIST",
            "dir/y.txt",
            "",
            "/",
            "\u{e9}t\u{e9}",
            "java/lang/Object.class",
        ];
        let mut answered = 0;
        for name in probes {
            let model_miss = !stored.iter().any(|s| hash(*s) == hash(name));
            let args = entry_pos_args(&mut ctx, this, Some(name));
            let got = zip_entry_pos_miss(&ctx, &args).is_some();
            assert_eq!(got, model_miss, "probe {name:?}");
            answered += usize::from(got);
        }
        assert!(
            answered >= 5,
            "the probes must exercise the answered path: {answered}"
        );

        // The callback answers `null` itself on a miss.
        let args = entry_pos_args(&mut ctx, this, Some("q0/P3.class"));
        assert_eq!(
            zip_source_get_entry_pos(&mut ctx, &args).unwrap(),
            Some(Value::Object(None))
        );

        // A null name with entries: the JDK body's NPE.
        let args = entry_pos_args(&mut ctx, this, None);
        assert_eq!(zip_entry_pos_miss(&ctx, &args), None, "null name");
    }

    /// `total == 0` answers `null` before the name is read (a null name too);
    /// a closed source, `tablelen == 0`, an out-of-range chain index and a
    /// chain cycle all run the JDK body.
    #[test]
    fn i76_zip_entry_miss_declines_what_the_body_must_answer() {
        let mut ctx = mock_ctx();
        let (empty, _, _) = zip_source(&mut ctx, &[]);
        let args = entry_pos_args(&mut ctx, empty, None);
        assert_eq!(
            zip_entry_pos_miss(&ctx, &args),
            Some(()),
            "empty, null name"
        );
        let args = entry_pos_args(&mut ctx, empty, Some("a"));
        assert_eq!(zip_entry_pos_miss(&ctx, &args), Some(()), "empty");

        // One entry: tablelen 1, so every name walks the one chain.
        let (one, table, entries) = zip_source(&mut ctx, &["a/b"]);
        let args = entry_pos_args(&mut ctx, one, Some("c/d"));
        assert_eq!(
            zip_entry_pos_miss(&ctx, &args),
            Some(()),
            "well-formed miss"
        );

        // A chain cycle: the entry's `next` names itself.
        ctx.set_array_element(entries, 1, Value::Int(0));
        assert_eq!(zip_entry_pos_miss(&ctx, &args), None, "cycle");
        ctx.set_array_element(entries, 1, Value::Int(ZIP_ENDCHAIN));
        // A next index past the end, and a negative one.
        ctx.set_array_element(entries, 1, Value::Int(3));
        assert_eq!(zip_entry_pos_miss(&ctx, &args), None, "next past the end");
        ctx.set_array_element(entries, 1, Value::Int(-7));
        assert_eq!(zip_entry_pos_miss(&ctx, &args), None, "negative next");
        ctx.set_array_element(entries, 1, Value::Int(ZIP_ENDCHAIN));
        // A head index past the end.
        ctx.set_array_element(table, 0, Value::Int(30));
        assert_eq!(zip_entry_pos_miss(&ctx, &args), None, "head past the end");
        ctx.set_array_element(table, 0, Value::Int(0));
        assert_eq!(zip_entry_pos_miss(&ctx, &args), Some(()), "restored");

        // `tablelen == 0`: the body's ArithmeticException.
        ctx.set_field(one, 2, Value::Int(0));
        assert_eq!(zip_entry_pos_miss(&ctx, &args), None, "tablelen 0");
        ctx.set_field(one, 2, Value::Int(1));
        // Closed (`Source.close` nulls `table` and `entries`).
        ctx.set_field(one, 1, Value::Object(None));
        assert_eq!(zip_entry_pos_miss(&ctx, &args), None, "closed");
    }

    /// The miss intrinsic is registered, as an `Intrinsic`, only with its
    /// switch (set explicitly both ways).
    #[test]
    fn i76_zip_entry_miss_registers_only_with_the_switch() {
        for word in ["0", "1"] {
            let kind = cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_ZIP_ENTRY_MISS_INTRINSIC", Some(word)),
                    ("CRATONVM_ZIP_ENTRY_HIT_INTRINSIC", Some("0")),
                ],
                || {
                    let mut r = NativeMethodRegistry::new();
                    register_zip_real_natives(&mut r);
                    r.kind_of(ZIP_SOURCE, "getEntryPos", ZIP_SOURCE_GET_ENTRY_POS_DESC)
                },
            );
            assert_eq!(
                kind,
                (word == "1").then_some(NativeKind::Intrinsic),
                "switch {word}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // CRATONVM_ZIP_ENTRY_HIT_INTRINSIC (interpreter round i3 wave 77, lane L5)
    // -----------------------------------------------------------------------

    /// A `ZipFile$Source` with a real CEN: entry `i` is a 46-byte header
    /// (`CENNAM` at +28) followed by its name's bytes, `entries[3i + 2]` its
    /// CEN position; the class declares `cen` at slot 4 besides the walk's
    /// four fields. Also the `UTF8ZipCoder` class (and one coder object) and
    /// the `ZipFile$EntryPos` record class (`name` slot 0, `pos` slot 1).
    /// Answers (receiver, coder, CEN positions).
    fn zip_source_with_cen(
        ctx: &mut crate::test_utils::MockNativeContext,
        names: &[&str],
    ) -> (ObjectRef, ObjectRef, Vec<i32>) {
        let (this, _, entries) = zip_source(ctx, names);
        let cid = ctx.class_id_of_object(this);
        let field = |name: &str, descriptor: &str, slot_index: usize, owner: ClassId| {
            cratonvm_native_api::FieldMetadata {
                name: name.to_string(),
                descriptor: descriptor.to_string(),
                access_flags: 0x0002,
                slot_index,
                declaring_class_id: owner,
                is_static: false,
            }
        };
        ctx.set_declared_fields(
            cid,
            vec![
                field("total", "I", 0, cid),
                field("table", "[I", 1, cid),
                field("tablelen", "I", 2, cid),
                field("entries", "[I", 3, cid),
                field("cen", "[B", 4, cid),
            ],
        );
        let mut cen = Vec::new();
        let mut positions = Vec::new();
        for (i, name) in names.iter().enumerate() {
            let pos = cen.len();
            positions.push(pos as i32);
            let mut header = [0u8; 46];
            header[28..30].copy_from_slice(&(name.len() as u16).to_le_bytes());
            cen.extend_from_slice(&header);
            cen.extend_from_slice(name.as_bytes());
            ctx.set_array_element(entries, 3 * i + 2, Value::Int(pos as i32));
        }
        let cen = byte_array(ctx, &cen);
        // `zip_source` allocated four slots; a fifth for `cen`.
        let wide = ctx.alloc_object(cid, 5);
        for slot in 0..4 {
            let v = ctx.get_field(this, slot);
            ctx.set_field(wide, slot, v);
        }
        ctx.set_field(wide, 4, Value::Object(Some(cen)));
        let coder_cid = ctx
            .ensure_class_initialized(ZIP_CODER_UTF8)
            .expect("coder class");
        let coder = ctx.alloc_object(coder_cid, 0);
        let pos_cid = ctx
            .ensure_class_initialized(ZIP_ENTRY_POS)
            .expect("EntryPos class");
        ctx.set_declared_fields(
            pos_cid,
            vec![
                field("name", "Ljava/lang/String;", 0, pos_cid),
                field("pos", "I", 1, pos_cid),
            ],
        );
        (wide, coder, positions)
    }

    fn hit_args(
        ctx: &mut crate::test_utils::MockNativeContext,
        this: ObjectRef,
        name: &str,
        add_slash: bool,
        coder: Option<ObjectRef>,
    ) -> [Value; 4] {
        let name = ctx.create_string_uninterned(name);
        [
            Value::Object(Some(this)),
            Value::Object(Some(name)),
            Value::Int(i32::from(add_slash)),
            Value::Object(coder),
        ]
    }

    /// The hit answer: an exact ASCII name is `Exact(its CEN position)`, a
    /// `String.hashCode` collision continues the walk (to the exact entry, or
    /// to `null`), a miss is `null`; the directory form, a non-ASCII name and
    /// a coder of another class decline. Without `hits` every hash match
    /// declines, as the wave-76 miss intrinsic does.
    #[test]
    fn i77_the_hit_walk_answers_exact_matches_and_collisions() {
        let mut ctx = mock_ctx();
        let stored = [
            "META-INF/MANIFEST.MF",
            "q0/",
            "q0/P1.class",
            "q0/AaX.class",
            "q0/BBX.class",
            "\u{e9}.txt",
        ];
        let (this, coder, positions) = zip_source_with_cen(&mut ctx, &stored);
        for (i, name) in stored.iter().enumerate().take(5) {
            let args = hit_args(&mut ctx, this, name, true, Some(coder));
            assert_eq!(
                zip_entry_pos_walk(&ctx, &args, true),
                EntryPosWalk::Exact(positions[i]),
                "{name:?}"
            );
            assert_eq!(
                zip_entry_pos_walk(&ctx, &args, false),
                EntryPosWalk::Decline,
                "{name:?} no hits"
            );
        }
        // "q0/AaX.class" and "q0/BBX.class" share a hash, so one chain: the
        // loop above found each, the one behind the other by continuing past
        // a NO_MATCH. With only one of them stored, the other walks past it to
        // `null`.
        let hash = |s: &str| zip_coder_hash(&s.encode_utf16().collect::<Vec<u16>>());
        assert_eq!(hash("q0/AaX.class"), hash("q0/BBX.class"));
        let (lone, lone_coder, _) = zip_source_with_cen(&mut ctx, &["q0/AaX.class", "q0/P1.class"]);
        let args = hit_args(&mut ctx, lone, "q0/BBX.class", true, Some(lone_coder));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Null,
            "collision, not stored"
        );
        let args = hit_args(&mut ctx, this, "q0/BBX.clas", true, Some(coder));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Null,
            "plain miss"
        );
        // `getEntry("q0")` finds the directory "q0/": DIRECTORY_MATCH declines.
        let args = hit_args(&mut ctx, this, "q0", true, Some(coder));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Decline,
            "directory"
        );
        // ... and without `addSlash` (getInputStream's lookup) it is no match.
        let args = hit_args(&mut ctx, this, "q0", false, Some(coder));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Null,
            "no addSlash"
        );
        // A non-ASCII name: the JDK encodes it.
        let args = hit_args(&mut ctx, this, "\u{e9}.txt", true, Some(coder));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Decline,
            "non-ASCII"
        );
        // Another coder class, or none.
        let other_cid = ctx
            .ensure_class_initialized("java/util/zip/ZipCoder")
            .expect("ZipCoder");
        let other = ctx.alloc_object(other_cid, 0);
        let args = hit_args(&mut ctx, this, "q0/P1.class", true, Some(other));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Decline,
            "other coder"
        );
        let args = hit_args(&mut ctx, this, "q0/P1.class", true, None);
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Decline,
            "null coder"
        );
    }

    /// A corrupt CEN (`CENNAM` past the end, `entries[idx + 2]` past the end)
    /// and a closed `cen` decline; the callback builds the record with the
    /// caller's own `name` and the position.
    #[test]
    fn i77_the_hit_callback_builds_the_record_and_declines_corrupt_input() {
        let mut ctx = mock_ctx();
        let (this, coder, positions) = zip_source_with_cen(&mut ctx, &["a/B.class", "a/C.class"]);
        let args = hit_args(&mut ctx, this, "a/C.class", true, Some(coder));
        let got = zip_source_get_entry_pos_hits(&mut ctx, &args).expect("answered");
        let Some(Value::Object(Some(record))) = got else {
            panic!("expected an EntryPos, got {got:?}");
        };
        let pos_cid = ctx.class_id_by_name(ZIP_ENTRY_POS).expect("EntryPos");
        assert_eq!(ctx.class_id_of_object(record), pos_cid);
        assert_eq!(ctx.get_field(record, 0), args[1], "the caller's own name");
        assert_eq!(ctx.get_field(record, 1), Value::Int(positions[1]));
        let miss = hit_args(&mut ctx, this, "a/D.class", true, Some(coder));
        assert_eq!(
            zip_source_get_entry_pos_hits(&mut ctx, &miss).expect("answered"),
            Some(Value::Object(None))
        );

        // `entries[idx + 2]` (the CEN position) pointing past the CEN.
        let Value::Object(Some(entries)) = ctx.get_field(this, 3) else {
            panic!("entries");
        };
        ctx.set_array_element(entries, 5, Value::Int(10_000));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Decline,
            "CENNAM out of range"
        );
        ctx.set_array_element(entries, 5, Value::Int(-30));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Decline,
            "negative position"
        );
        ctx.set_array_element(entries, 5, Value::Int(positions[1]));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Exact(positions[1]),
            "restored"
        );
        // A closed source's `cen`.
        ctx.set_field(this, 4, Value::Object(None));
        assert_eq!(
            zip_entry_pos_walk(&ctx, &args, true),
            EntryPosWalk::Decline,
            "null cen"
        );
    }

    /// The row is registered, as an `Intrinsic`, when either switch is on; the
    /// hit switch installs the hit body (every word set explicitly).
    #[test]
    fn i77_zip_entry_hit_registers_with_its_switch() {
        let hits: NativeCallback = zip_source_get_entry_pos_hits;
        let miss: NativeCallback = zip_source_get_entry_pos;
        for (miss_word, hit_word, want) in [
            ("0", "0", None),
            ("1", "0", Some(miss)),
            ("0", "1", Some(hits)),
            ("1", "1", Some(hits)),
        ] {
            let (kind, callback) = cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_ZIP_ENTRY_MISS_INTRINSIC", Some(miss_word)),
                    ("CRATONVM_ZIP_ENTRY_HIT_INTRINSIC", Some(hit_word)),
                ],
                || {
                    let mut r = NativeMethodRegistry::new();
                    // The hit body registers only without a JIT (wave-77 triage).
                    r.set_jit_enabled(false);
                    register_zip_real_natives(&mut r);
                    (
                        r.kind_of(ZIP_SOURCE, "getEntryPos", ZIP_SOURCE_GET_ENTRY_POS_DESC),
                        r.find(ZIP_SOURCE, "getEntryPos", ZIP_SOURCE_GET_ENTRY_POS_DESC),
                    )
                },
            );
            assert_eq!(
                kind,
                want.map(|_| NativeKind::Intrinsic),
                "miss {miss_word} hit {hit_word}"
            );
            assert_eq!(
                callback.map(|c| c as usize),
                want.map(|c| c as usize),
                "miss {miss_word} hit {hit_word}"
            );
        }
    }

    /// Corrupt deflate input must FAIL, not answer "no progress".
    ///
    /// All four `Inflater.inflate*` natives are declared `throws
    /// DataFormatException` and `Inflater.inflate` has no Java-level check
    /// that converts a zero-progress packed result into an exception, so a
    /// swallowed decode error reaches the caller as `inflate() == 0` — a
    /// fabricated success. `RJdkJni.zipNatives:165` asserts the throw.
    ///
    /// The mock context cannot construct a real
    /// `java.util.zip.DataFormatException`, so what is asserted here is the
    /// half that is testable off-VM: the call must be `Err`, never `Ok`. The
    /// exception CLASS is chosen in `throw_data_format`.
    #[test]
    fn corrupt_input_fails_instead_of_reporting_no_progress() {
        let mut ctx = mock_ctx();
        // nowrap = 0 → a zlib-wrapped stream is expected. {1,2,...} has
        // CM = 1 (not 8) and a bad two-byte header check, so zlib rejects it
        // before producing a single byte.
        let addr = match infl_init(&mut ctx, &[Value::Int(0)]).unwrap().unwrap() {
            Value::Long(addr) => addr,
            other => panic!("expected Long handle, got {other:?}"),
        };
        let corrupt: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
        let input_arr = ctx.new_array(ArrayElementType::Byte, corrupt.len());
        for (i, b) in corrupt.iter().enumerate() {
            ctx.set_array_element(input_arr, i, Value::Int(*b as i32));
        }
        let output_arr = ctx.new_array(ArrayElementType::Byte, 64);
        let result = infl_inflate_bytes_bytes(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Object(Some(input_arr)),
                Value::Int(0),
                Value::Int(corrupt.len() as i32),
                Value::Object(Some(output_arr)),
                Value::Int(0),
                Value::Int(64),
            ],
        );
        assert!(
            result.is_err(),
            "corrupt deflate input must raise, not answer no-progress"
        );

        // A VALID stream through the same path must still succeed — the guard
        // has to reject corrupt input without rejecting good input.
        let original = b"hello hello hello hello world world";
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(original).unwrap();
        let good = enc.finish().unwrap();
        let addr = match infl_init(&mut ctx, &[Value::Int(0)]).unwrap().unwrap() {
            Value::Long(addr) => addr,
            other => panic!("expected Long handle, got {other:?}"),
        };
        let input_arr = ctx.new_array(ArrayElementType::Byte, good.len());
        for (i, b) in good.iter().enumerate() {
            ctx.set_array_element(input_arr, i, Value::Int(*b as i8 as i32));
        }
        let output_arr = ctx.new_array(ArrayElementType::Byte, 256);
        let packed = match infl_inflate_bytes_bytes(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Object(Some(input_arr)),
                Value::Int(0),
                Value::Int(good.len() as i32),
                Value::Object(Some(output_arr)),
                Value::Int(0),
                Value::Int(256),
            ],
        )
        .expect("valid zlib stream must inflate")
        .expect("inflate returns a long")
        {
            Value::Long(p) => p as u64,
            other => panic!("expected Long result, got {other:?}"),
        };
        let produced = ((packed >> 31) & 0x7FFF_FFFF) as usize;
        assert_eq!(read_byte_array(&ctx, output_arr, 0, produced), original);
        assert!((packed >> 62) & 1 == 1, "valid stream must report finished");
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
    /// hang (`jetty-webserver-factory-poststartup-timeout-and-reflective-supertype-residuals-FIXED.md`): a mid-stream `SYNC_FLUSH` (zlib flush code
    /// 2) must produce real compressed bytes immediately instead of being
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
                Value::Int(2), // SYNC_FLUSH
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

    /// RFC 1950 §9 known-answer vectors for the rolling helper the Adler32
    /// natives (and the Deflater/Inflater `getAdler` bridges) share.
    #[test]
    fn adler32_known_vectors() {
        // Fresh state is 1 (s1 = 1, s2 = 0), never 0 — unlike CRC32.
        assert_eq!(adler32_update(1, b""), 1);
        // "abc": s1 = 1+97+98+99 = 295 = 0x127;
        //        s2 = 98+196+295 = 589 = 0x24D.
        assert_eq!(adler32_update(1, b"abc"), 0x024D_0127);
        // "123456789": the vector RJdkJni.zipNatives asserts.
        assert_eq!(adler32_update(1, b"123456789"), 0x091E_01DE);
        // Streaming in pieces must equal hashing the concatenation — this is
        // the property `Adler32.update` relies on across native calls.
        let split = adler32_update(adler32_update(1, b"1234"), b"56789");
        assert_eq!(split, 0x091E_01DE);
    }

    /// The three registered natives must reproduce those vectors through the
    /// `Value` boundary, byte-array reads included.
    #[test]
    fn adler32_natives_match_vectors() {
        let mut ctx = mock_ctx();

        // update(int adler, int b): 'a' from the seed => s1 = 98, s2 = 98.
        let one = adler32_update_int(&mut ctx, &[Value::Int(1), Value::Int(b'a' as i32)])
            .unwrap()
            .unwrap();
        assert_eq!(one, Value::Int(0x0062_0062));

        // Only the low 8 bits of `b` participate (JNI jbyte truncation).
        let masked = adler32_update_int(&mut ctx, &[Value::Int(1), Value::Int(0x1_0061)])
            .unwrap()
            .unwrap();
        assert_eq!(masked, Value::Int(0x0062_0062));

        // updateBytes(int adler, byte[] b, int off, int len) over "123456789"
        // embedded in a larger array, to exercise the off/len slice.
        let data = b"XX123456789XX";
        let arr = ctx.new_array(ArrayElementType::Byte, data.len());
        for (i, b) in data.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
        }
        let bytes = adler32_update_bytes(
            &mut ctx,
            &[
                Value::Int(1),
                Value::Object(Some(arr)),
                Value::Int(2),
                Value::Int(9),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(bytes, Value::Int(0x091E_01DE));

        // A null array is a no-op rather than a panic.
        let null_arr = adler32_update_bytes(
            &mut ctx,
            &[
                Value::Int(1),
                Value::Object(None),
                Value::Int(0),
                Value::Int(9),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(null_arr, Value::Int(1));
    }

    /// Copy `bytes` into a fresh mock `byte[]`.
    fn byte_array(ctx: &mut dyn NativeContext, bytes: &[u8]) -> ObjectRef {
        let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
        for (i, b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
        }
        arr
    }

    /// A zlib PRESET DICTIONARY must actually reach zlib, in both directions.
    ///
    /// Both `setDictionary` natives used to be no-ops, on the stated grounds
    /// that flate2's `set_dictionary` was "gated behind a zlib backend feature
    /// we don't enable" — untrue, `native-builtins/Cargo.toml` has always used
    /// `features = ["zlib"]`. SPDY compresses every header block with a preset
    /// dictionary, so `SpdyHeaderBlockZlibDecoder` handed the dictionary over,
    /// got silence, inflated 0 bytes and reported `Invalid Header Block`.
    ///
    /// Asserting a round trip through our OWN Deflater+Inflater would not have
    /// caught it: with both halves ignoring the dictionary the round trip is
    /// green and the bytes on the wire are still wrong. So the compressing
    /// half is pinned on the header's `FDICT` bit and the decompressing half on
    /// the `needDict` result bit — both of which are only reachable when the
    /// dictionary genuinely reaches zlib.
    #[test]
    fn set_dictionary_reaches_zlib_on_both_halves() {
        const DICT: &[u8] = b"optionsgetheadpostputdeletetraceacceptaccept-charset";
        let payload = b"accept-charset: utf-8 accept: text/html options get head post";

        let mut ctx = mock_ctx();

        // --- compressing half: the emitted zlib header must set FDICT -------
        let daddr = match defl_init(&mut ctx, &[Value::Int(6), Value::Int(0), Value::Int(0)])
            .unwrap()
            .unwrap()
        {
            Value::Long(a) => a,
            other => panic!("expected Long handle, got {other:?}"),
        };
        let dict_arr = byte_array(&mut ctx, DICT);
        defl_set_dictionary(
            &mut ctx,
            &[
                Value::Long(daddr),
                Value::Object(Some(dict_arr)),
                Value::Int(0),
                Value::Int(DICT.len() as i32),
            ],
        )
        .unwrap();

        let in_arr = byte_array(&mut ctx, payload);
        let out_arr = ctx.new_array(ArrayElementType::Byte, 512);
        let packed = match defl_deflate_bytes_bytes(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(daddr),
                Value::Object(Some(in_arr)),
                Value::Int(0),
                Value::Int(payload.len() as i32),
                Value::Object(Some(out_arr)),
                Value::Int(0),
                Value::Int(512),
                Value::Int(4), // FINISH
                Value::Int(0),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Long(v) => v,
            other => panic!("expected Long, got {other:?}"),
        };
        let (_, produced, _) = unpack_deflate_result(packed);
        assert!(produced > 2, "deflate produced nothing");
        let compressed = read_byte_array(&ctx, out_arr, 0, produced);

        // zlib header: CMF, FLG. FDICT is bit 5 of FLG, and a DICTID follows.
        assert_eq!(compressed[0] & 0x0f, 8, "expected deflate method in CMF");
        assert_ne!(
            compressed[1] & 0x20,
            0,
            "FDICT must be set — the dictionary never reached the compressor \
             (this is the bug: the stream is then unreadable by any peer that \
             expects the preset dictionary)"
        );

        // --- decompressing half: needDict, then progress after setDictionary -
        let iaddr = match infl_init(&mut ctx, &[Value::Int(0)]).unwrap().unwrap() {
            Value::Long(a) => a,
            other => panic!("expected Long handle, got {other:?}"),
        };
        let cin = byte_array(&mut ctx, &compressed);
        let cout = ctx.new_array(ArrayElementType::Byte, 512);
        // `Inflater` keeps its own input cursor: the header bytes the first
        // call consumes must NOT be handed to the second one, or zlib sees a
        // second header mid-stream and raises DataFormatException.
        let inflate = |ctx: &mut dyn NativeContext, off: usize| -> (usize, usize, bool) {
            let p = match infl_inflate_bytes_bytes(
                ctx,
                &[
                    Value::Object(None),
                    Value::Long(iaddr),
                    Value::Object(Some(cin)),
                    Value::Int(off as i32),
                    Value::Int((compressed.len() - off) as i32),
                    Value::Object(Some(cout)),
                    Value::Int(0),
                    Value::Int(512),
                ],
            )
            .unwrap()
            .unwrap()
            {
                Value::Long(v) => v as u64,
                other => panic!("expected Long, got {other:?}"),
            };
            (
                (p & 0x7FFF_FFFF) as usize,
                ((p >> 31) & 0x7FFF_FFFF) as usize,
                (p >> 63) & 1 == 1,
            )
        };

        let (consumed_first, produced_first, need_dict) = inflate(&mut ctx, 0);
        assert_eq!(
            produced_first, 0,
            "no output is possible before the dictionary"
        );
        assert!(
            need_dict,
            "inflate must report needDict for an FDICT stream"
        );

        let dict_arr2 = byte_array(&mut ctx, DICT);
        infl_set_dictionary(
            &mut ctx,
            &[
                Value::Long(iaddr),
                Value::Object(Some(dict_arr2)),
                Value::Int(0),
                Value::Int(DICT.len() as i32),
            ],
        )
        .unwrap();

        let (_, produced_second, _) = inflate(&mut ctx, consumed_first);
        assert_eq!(
            produced_second,
            payload.len(),
            "after setDictionary the stream must decode — a no-op setDictionary \
             leaves this at 0, which is what made SPDY report Invalid Header Block"
        );
        assert_eq!(read_byte_array(&ctx, cout, 0, produced_second), payload);
    }

    /// Deflate `payload` with `dict_addr` as a DIRECT-buffer preset dictionary
    /// (or with no dictionary at all when `dict_addr` is `None`), returning the
    /// compressed bytes.
    fn deflate_with_buffer_dictionary(
        ctx: &mut dyn NativeContext,
        payload: &[u8],
        dict: Option<(i64, usize)>,
    ) -> Vec<u8> {
        let addr = match defl_init(ctx, &[Value::Int(6), Value::Int(0), Value::Int(0)])
            .unwrap()
            .unwrap()
        {
            Value::Long(a) => a,
            other => panic!("expected Long handle, got {other:?}"),
        };
        if let Some((dict_addr, dict_len)) = dict {
            defl_set_dictionary_buffer(
                ctx,
                &[
                    Value::Long(addr),
                    Value::Long(dict_addr),
                    Value::Int(dict_len as i32),
                ],
            )
            .unwrap();
        }
        let in_arr = byte_array(ctx, payload);
        let out_arr = ctx.new_array(ArrayElementType::Byte, 512);
        let packed = match defl_deflate_bytes_bytes(
            ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Object(Some(in_arr)),
                Value::Int(0),
                Value::Int(payload.len() as i32),
                Value::Object(Some(out_arr)),
                Value::Int(0),
                Value::Int(512),
                Value::Int(4), // FINISH
                Value::Int(0),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Long(v) => v,
            other => panic!("expected Long, got {other:?}"),
        };
        let (_, produced, _) = unpack_deflate_result(packed);
        read_byte_array(ctx, out_arr, 0, produced)
    }

    /// The DIRECT-`ByteBuffer` `setDictionary` overloads must reach zlib too.
    ///
    /// These two stayed no-ops when the heap overloads were fixed, on the
    /// stated grounds that "the direct-buffer inflate natives are themselves
    /// unsupported (`infl_direct_buffer_unsupported`), so a caller cannot get
    /// far enough to need this". No such function exists: all four `inflate*`
    /// and all four `deflate*` overloads read and write direct-buffer memory
    /// for real, so the gap was reachable — the same wrong-on-the-wire stream
    /// SPDY tripped over, reached through `setDictionary(ByteBuffer)`.
    ///
    /// Pinned on FDICT and on the compressed LENGTH against a no-dictionary
    /// control, for the same reason as the heap test: a round trip through our
    /// own encoder is green either way.
    #[test]
    fn set_dictionary_buffer_reaches_zlib_on_both_halves() {
        const DICT: &[u8] = b"optionsgetheadpostputdeletetraceacceptaccept-charset";
        let payload = b"accept-charset: utf-8 accept: text/html options get head post";

        let mut ctx = mock_ctx();

        // A direct ByteBuffer's payload is plain native memory as far as the
        // native is concerned; a `Vec`'s backing store is the same thing here,
        // and `MockNativeContext` copies from real pointers.
        let dict_mem = DICT.to_vec();
        let dict_addr = dict_mem.as_ptr() as i64;

        let plain = deflate_with_buffer_dictionary(&mut ctx, payload, None);
        let with_dict =
            deflate_with_buffer_dictionary(&mut ctx, payload, Some((dict_addr, DICT.len())));

        assert_ne!(
            with_dict[1] & 0x20,
            0,
            "FDICT must be set — the direct-buffer dictionary never reached the \
             compressor"
        );
        assert!(
            with_dict.len() < plain.len(),
            "a preset dictionary must shrink this payload: {} bytes with the \
             dictionary vs {} without (equal lengths mean the no-op is still \
             there)",
            with_dict.len(),
            plain.len()
        );

        // --- decompressing half -------------------------------------------
        let iaddr = match infl_init(&mut ctx, &[Value::Int(0)]).unwrap().unwrap() {
            Value::Long(a) => a,
            other => panic!("expected Long handle, got {other:?}"),
        };
        let cin = byte_array(&mut ctx, &with_dict);
        let cout = ctx.new_array(ArrayElementType::Byte, 512);
        let mut inflate = |ctx: &mut dyn NativeContext, off: usize| -> (usize, usize, bool) {
            let p = match infl_inflate_bytes_bytes(
                ctx,
                &[
                    Value::Object(None),
                    Value::Long(iaddr),
                    Value::Object(Some(cin)),
                    Value::Int(off as i32),
                    Value::Int((with_dict.len() - off) as i32),
                    Value::Object(Some(cout)),
                    Value::Int(0),
                    Value::Int(512),
                ],
            )
            .unwrap()
            .unwrap()
            {
                Value::Long(v) => v as u64,
                other => panic!("expected Long, got {other:?}"),
            };
            (
                (p & 0x7FFF_FFFF) as usize,
                ((p >> 31) & 0x7FFF_FFFF) as usize,
                (p >> 63) & 1 == 1,
            )
        };

        let (consumed_first, produced_first, need_dict) = inflate(&mut ctx, 0);
        assert_eq!(
            produced_first, 0,
            "no output is possible before the dictionary"
        );
        assert!(
            need_dict,
            "inflate must report needDict for an FDICT stream"
        );

        infl_set_dictionary_buffer(
            &mut ctx,
            &[
                Value::Long(iaddr),
                Value::Long(dict_addr),
                Value::Int(DICT.len() as i32),
            ],
        )
        .unwrap();

        let (_, produced_second, _) = inflate(&mut ctx, consumed_first);
        assert_eq!(
            produced_second,
            payload.len(),
            "after setDictionary(ByteBuffer) the stream must decode — the no-op \
             left this at 0"
        );
        assert_eq!(read_byte_array(&ctx, cout, 0, produced_second), payload);

        // Keep the dictionary alive until every native has copied out of it.
        drop(dict_mem);
    }

    /// An unreadable dictionary address is reported, not silently dropped —
    /// and `IllegalArgumentException` is chosen because it is UNCHECKED:
    /// `setDictionary` declares no checked exception, so a `DataFormatException`
    /// (what the sibling `inflate*` natives raise for a bad address) would
    /// escape every caller's `catch`.
    #[test]
    fn set_dictionary_buffer_reports_an_unreadable_address() {
        let mut ctx = mock_ctx();
        let addr = match defl_init(&mut ctx, &[Value::Int(6), Value::Int(0), Value::Int(0)])
            .unwrap()
            .unwrap()
        {
            Value::Long(a) => a,
            other => panic!("expected Long handle, got {other:?}"),
        };

        assert!(
            defl_set_dictionary_buffer(
                &mut ctx,
                &[Value::Long(addr), Value::Long(0), Value::Int(8)]
            )
            .is_err(),
            "a null buffer address with a non-zero length must be reported"
        );
        // A zero-length dictionary reads nothing, so the address is irrelevant.
        assert!(defl_set_dictionary_buffer(
            &mut ctx,
            &[Value::Long(addr), Value::Long(0), Value::Int(0)]
        )
        .is_ok());
    }
}

// ---------------------------------------------------------------------------
// Deflater on zlib's own stream (interpreter round i3 wave 79, lane L5)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod i79_deflater_params_tests {
    use super::*;

    const HUFFMAN_ONLY: i32 = 2;

    fn params(level: i32, strategy: i32) -> i32 {
        1 | (strategy << 1) | (level << 3)
    }

    /// Deflate `input` to the end on `st` (FINISH), with a roomy buffer.
    fn finish(st: &mut DeflaterState, input: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; input.len() + 1024];
        let (used_in, used_out, finished, pending) =
            st.step(0, 4, input, &mut out).expect("deflate");
        assert_eq!(used_in as usize, input.len());
        assert!(finished && !pending);
        out.truncate(used_out as usize);
        out
    }

    fn inflate_raw(compressed: &[u8], len: usize) -> Vec<u8> {
        let mut d = Decompress::new(false);
        let mut out = vec![0u8; len + 16];
        d.decompress(compressed, &mut out, FlushDecompress::Finish)
            .expect("inflate");
        out.truncate(d.total_out() as usize);
        out
    }

    /// `setStrategy(HUFFMAN_ONLY)` reaches zlib: a parameter call on a fresh
    /// stream consumes and produces nothing (`deflateParams` only), and the
    /// stream it leaves cannot use LZ77 matches, so 10 000 `'a'` bytes cost
    /// about a bit each where the default strategy needs a few dozen bytes.
    /// Before wave 79 the strategy bits were dropped (`set_level` only).
    #[test]
    fn i79_a_strategy_change_reaches_zlib_and_does_not_compress_in_that_call() {
        let input = vec![b'a'; 10_000];
        let mut huff = DeflaterState::new(6, 0, true).expect("init");
        let mut out = vec![0u8; 64];
        let got = huff
            .step(params(6, HUFFMAN_ONLY), 0, &input, &mut out)
            .expect("params");
        assert_eq!(got, (0, 0, false, false), "a parameter call only switches");
        let huff_bytes = finish(&mut huff, &input);

        let mut plain = DeflaterState::new(6, 0, true).expect("init");
        let plain_bytes = finish(&mut plain, &input);

        assert!(
            huff_bytes.len() > 1000,
            "Huffman-only: {} bytes",
            huff_bytes.len()
        );
        assert!(
            plain_bytes.len() < 100,
            "default: {} bytes",
            plain_bytes.len()
        );
        assert_eq!(inflate_raw(&huff_bytes, input.len()), input);
        assert_eq!(inflate_raw(&plain_bytes, input.len()), input);
    }

    /// `deflateParams` answering `Z_BUF_ERROR` (pending output larger than the
    /// buffer, input not taken) reports what it wrote and bit 63, so
    /// `Deflater.deflate` keeps `setParams` and retries -- `Deflater.c`'s
    /// `checkDeflateStatus`. The level change was lost before wave 79.
    #[test]
    fn i79_a_params_buf_error_keeps_the_change_pending() {
        let mut seed = 0x1234_5678u32;
        let input: Vec<u8> = (0..4096)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (seed >> 24) as u8
            })
            .collect();
        let mut st = DeflaterState::new(6, 0, true).expect("init");
        let mut small = [0u8; 16];
        // SYNC_FLUSH into 16 bytes: all input taken, a block left pending.
        let (used_in, used_out, finished, pending) =
            st.step(0, 2, &input, &mut small).expect("deflate");
        assert_eq!(
            (used_in, used_out, finished, pending),
            (4096, 16, false, false)
        );
        // A level change now cannot flush: Z_BUF_ERROR, 16 bytes out, none in.
        let got = st
            .step(params(1, 0), 0, &input, &mut small)
            .expect("params");
        assert_eq!(got, (0, 16, false, true));
        assert_eq!(
            pack_deflate_result_params(got.0, got.1, got.2, got.3) as u64 >> 63,
            1,
            "bit 63 = params still pending"
        );
    }

    /// `getAdler()` is zlib's `strm->adler`: 1 forever for a raw stream
    /// (zlib updates it only when `wrap == 1`), the running ADLER-32 for a
    /// wrapped one, and the seed again after `reset()`
    /// (i73-L5-inflater-getadler-differs-from-zlib, case 2).
    #[test]
    fn i79_deflater_adler_is_zlibs() {
        let input = b"hello hello hello world";
        let mut raw = DeflaterState::new(6, 0, true).expect("init");
        assert_eq!(raw.adler(), 1);
        let _ = finish(&mut raw, input);
        assert_eq!(raw.adler(), 1, "raw stream keeps the seed");

        let mut wrapped = DeflaterState::new(6, 0, false).expect("init");
        let _ = finish(&mut wrapped, input);
        assert_eq!(wrapped.adler(), adler32_update(1, input));
        wrapped.reset();
        assert_eq!(wrapped.adler(), 1);
    }

    /// i73 page case 3: at `Z_NEED_DICT` the inflater's `getAdler()` is the
    /// stream's DICTID (the dictionary's ADLER-32), it stays so across
    /// `setDictionary`, and the next inflate re-seeds it before producing the
    /// output (zlib's `DICT` state), ending at the output's ADLER-32.
    #[test]
    fn i79_inflater_reports_the_dictid_at_need_dict() {
        use crate::test_utils::mock_ctx;
        #[allow(unused_imports)]
        use cratonvm_native_api::{NativeHeapAccess, NativeSystemAccess};
        const DICT: &[u8] = b"optionsgetheadpostputdelete";
        let payload: &[u8] = b"options get head post put delete options get";
        let mut c = Compress::new(flate2::Compression::default(), true);
        c.set_dictionary(DICT).expect("dict");
        let mut compressed = Vec::with_capacity(256);
        c.compress_vec(payload, &mut compressed, FlushCompress::Finish)
            .expect("compress");

        let mut ctx = mock_ctx();
        let addr = inflater_init(false, true);
        fn adler_of(ctx: &mut dyn NativeContext, addr: i64) -> u32 {
            match infl_get_adler(ctx, &[Value::Long(addr)]).unwrap().unwrap() {
                Value::Int(a) => a as u32,
                other => panic!("expected Int, got {other:?}"),
            }
        }
        let mut out = vec![0u8; 256];
        let first = infl_do_decompress(addr, &compressed, &mut out);
        assert!(first.need_dict && first.output_consumed == 0);
        assert_eq!(
            adler_of(&mut ctx, addr),
            adler32_update(1, DICT),
            "DICTID at needsDictionary()"
        );

        let dict_arr = ctx.new_array(ArrayElementType::Byte, DICT.len());
        for (i, b) in DICT.iter().enumerate() {
            ctx.set_array_element(dict_arr, i, Value::Int(*b as i8 as i32));
        }
        infl_set_dictionary(
            &mut ctx,
            &[
                Value::Long(addr),
                Value::Object(Some(dict_arr)),
                Value::Int(0),
                Value::Int(DICT.len() as i32),
            ],
        )
        .unwrap();
        assert_eq!(
            adler_of(&mut ctx, addr),
            adler32_update(1, DICT),
            "kept across setDictionary"
        );

        let rest = &compressed[first.input_consumed as usize..];
        let second = infl_do_decompress(addr, rest, &mut out);
        assert!(second.finished);
        assert_eq!(&out[..second.output_consumed as usize], payload);
        assert_eq!(
            adler_of(&mut ctx, addr),
            adler32_update(1, payload),
            "the output's ADLER-32"
        );
        infl_end(&mut ctx, &[Value::Long(addr)]).unwrap();
    }
}

/// Interpreter round i5 wave 100, lane L5: `CRATONVM_INFLATER_SHARDED_TABLE`.
#[cfg(test)]
mod i100_inflater_sharded_table_tests {
    use super::*;
    use crate::test_utils::mock_ctx;

    /// An unsharded handle lives in shard 0 (the one table every VM with the
    /// switch off shares, as before); a sharded one never does, and two
    /// consecutive sharded handles land in different shards.
    #[test]
    fn i100_handles_find_their_shard_from_the_handle_alone() {
        let plain = inflater_init_in(true, true, false);
        let first = inflater_init_in(true, true, true);
        let second = inflater_init_in(true, true, true);
        assert_eq!(inflater_handle_shard(plain), 0);
        assert_eq!(plain & SHARDED_INFLATER_HANDLE_BIT, 0);
        for h in [first, second] {
            assert_ne!(h & SHARDED_INFLATER_HANDLE_BIT, 0);
            let shard = inflater_handle_shard(h);
            assert!((1..INFLATER_TABLE_SHARDS).contains(&shard), "{shard}");
            assert!(inflater_table(h).lock().unwrap().contains_key(&h));
        }
        // Other tests mint handles concurrently; adjacent ones always differ.
        if second - first < INFLATER_TABLE_SHARDS as i64 - 1 {
            assert_ne!(inflater_handle_shard(first), inflater_handle_shard(second));
        }
        assert!(inflater_table(plain).lock().unwrap().contains_key(&plain));
        let mut ctx = mock_ctx();
        for h in [plain, first, second] {
            infl_end(&mut ctx, &[Value::Long(h)]).unwrap();
            assert!(!inflater_table(h).lock().unwrap().contains_key(&h));
        }
    }

    /// A sharded stream inflates exactly as an unsharded one.
    #[test]
    fn i100_a_sharded_stream_inflates() {
        let payload: &[u8] = b"sharded sharded sharded inflater table table table";
        let mut c = Compress::new(flate2::Compression::default(), false);
        let mut compressed = Vec::with_capacity(256);
        c.compress_vec(payload, &mut compressed, FlushCompress::Finish)
            .expect("compress");
        let addr = inflater_init_in(true, true, true);
        let mut out = vec![0u8; 256];
        let step = infl_do_decompress(addr, &compressed, &mut out);
        assert!(step.finished);
        assert_eq!(&out[..step.output_consumed as usize], payload);
        let mut ctx = mock_ctx();
        infl_end(&mut ctx, &[Value::Long(addr)]).unwrap();
    }
}

#[cfg(test)]
mod i115_zip_refusal_tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    use cratonvm_native_api::NativeHeapAccess;

    /// `Inflater.c` hands zlib's own `strm->msg` to `DataFormatException`;
    /// flate2's `Display` would prefix "deflate decompression error: ".
    #[test]
    fn i115_a_data_error_carries_zlibs_bare_message() {
        let addr = inflater_init(false, false);
        let mut out = vec![0u8; 64];
        let step = infl_do_decompress(addr, &[1, 2, 3, 4, 5, 6, 7, 8], &mut out);
        assert_eq!(step.data_error.as_deref(), Some("incorrect header check"));
        let step = {
            let addr = inflater_init(true, false);
            infl_do_decompress(addr, &[0x07, 0, 0, 0], &mut out)
        };
        assert_eq!(step.data_error.as_deref(), Some("invalid block type"));
    }

    /// `checkSetDictionaryResult`: zlib refuses a dictionary on a fresh
    /// zlib-wrapped inflater (`Z_STREAM_ERROR`) and on a deflater whose header
    /// is out; a raw inflater and a fresh deflater take it. A refusal has no
    /// message, and the native throws.
    #[test]
    fn i115_a_refused_dictionary_is_reported() {
        assert_eq!(infl_apply_dictionary(inflater_init(false, false), b"abc"), Some(None));
        assert_eq!(infl_apply_dictionary(inflater_init(true, false), b"abc"), None);

        let mut ctx = mock_ctx();
        let daddr = match defl_init(&mut ctx, &[Value::Int(6), Value::Int(0), Value::Int(0)])
            .unwrap()
            .unwrap()
        {
            Value::Long(a) => a,
            other => panic!("expected Long handle, got {other:?}"),
        };
        assert_eq!(defl_apply_dictionary(daddr, b"abc"), None, "before the header");
        let payload = b"some payload some payload";
        let in_arr = ctx.new_array(ArrayElementType::Byte, payload.len());
        for (i, b) in payload.iter().enumerate() {
            ctx.set_array_element(in_arr, i, Value::Int(*b as i8 as i32));
        }
        let out_arr = ctx.new_array(ArrayElementType::Byte, 256);
        defl_deflate_bytes_bytes(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Long(daddr),
                Value::Object(Some(in_arr)),
                Value::Int(0),
                Value::Int(payload.len() as i32),
                Value::Object(Some(out_arr)),
                Value::Int(0),
                Value::Int(256),
                Value::Int(2), // SYNC_FLUSH
                Value::Int(0),
            ],
        )
        .unwrap();
        assert_eq!(defl_apply_dictionary(daddr, b"abc"), Some(None), "past the header");

        let iaddr = inflater_init(false, false);
        let dict = ctx.new_array(ArrayElementType::Byte, 3);
        assert!(infl_set_dictionary(
            &mut ctx,
            &[Value::Long(iaddr), Value::Object(Some(dict)), Value::Int(0), Value::Int(3)],
        )
        .is_err());
    }
}
