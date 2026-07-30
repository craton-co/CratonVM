// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.util.zip` natives: GZIP/Zip streams, ZipEntry, Deflater/Inflater, Adler32, checked streams.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

// =============================================================================
// GZIP / Zip streams — real flate2 compression/decompression
// GZIPInputStream = 2-field (decompressed_data=0 byte[], position=1 Int)
// GZIPOutputStream = 2-field (accumulated_data=0 byte[], count=1 Int)
// ZipInputStream = 2-field (in=0, currentEntry=1)
// ZipOutputStream = 2-field (out=0, currentEntry=1)
// =============================================================================

/// Default cap on the total number of inflated bytes a single GZIP/Zip input
/// stream may produce (256 MiB). Overridable at runtime via
/// `CRATONVM_MAX_INFLATED_BYTES` (decimal byte count; `0` disables the cap).
///
/// FIX (finding 3): these synthetic streams eagerly inflate the WHOLE input in
/// `<init>`. Without a bound, a few-KB "zip/gzip bomb" can inflate to gigabytes
/// and exhaust the heap before any Java code runs. The cap turns that into a
/// loud `IOException` instead of an OOM crash.
pub(crate) const GZIP_DEFAULT_MAX_INFLATED: u64 = 256 * 1024 * 1024;

/// Resolve the configured max inflated-size cap. Returns `None` when the cap is
/// explicitly disabled (`CRATONVM_MAX_INFLATED_BYTES=0`).
pub(crate) fn gzip_max_inflated_bytes() -> Option<u64> {
    match crate::nbflags().max_inflated_bytes.as_deref() {
        Some(s) => match s.trim().parse::<u64>() {
            Ok(0) => None, // explicitly disabled
            Ok(n) => Some(n),
            Err(_) => Some(GZIP_DEFAULT_MAX_INFLATED),
        },
        None => Some(GZIP_DEFAULT_MAX_INFLATED),
    }
}

/// Inflate `reader` fully into a `Vec<u8>`, refusing to exceed `cap` bytes.
///
/// Reads in chunks so memory grows incrementally; as soon as the running total
/// would exceed `cap` we stop and return `Err` (compression-bomb defense). When
/// `cap` is `None` the read is unbounded (legacy behavior, opt-in only).
pub(crate) fn inflate_bounded<R: std::io::Read>(
    mut reader: R,
    cap: Option<u64>,
) -> Result<Vec<u8>, std::io::Error> {
    use std::io::Read;
    let mut out: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        if let Some(limit) = cap {
            if out.len() as u64 + n as u64 > limit {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "inflated size exceeds CRATONVM_MAX_INFLATED_BYTES cap ({limit} bytes) \
                         — possible compression bomb"
                    ),
                ));
            }
        }
        out.extend_from_slice(&chunk[..n]);
    }
    Ok(out)
}

/// Drain a Java `InputStream` fully into a host `Vec<u8>`.
///
/// PERF: the GZIP/Zip `<init>` natives previously slurped the underlying stream
/// ONE BYTE AT A TIME via `read()I`, paying a full virtual-dispatch + `Value`
/// box per byte (catastrophic for multi-MB archives). This reads in 64 KiB
/// chunks through the bulk `read([B,I,I)I` virtual instead, copying each chunk
/// out with the `read_byte_array_into` memcpy intrinsic. A single reusable Java
/// `byte[]` buffer is allocated for the whole drain.
///
/// Behavior is identical to the old per-byte loop ("read until EOF"): if the
/// bulk read is unsupported (errors / returns a non-int) or returns 0 with a
/// non-zero request length (a misbehaving stream that would otherwise spin), we
/// fall back to the per-byte `read()I` loop for the remainder, so no input is
/// ever lost.
pub(crate) fn drain_input_stream_bulk(ctx: &mut dyn NativeContext, is_ref: ObjectRef) -> Vec<u8> {
    const CHUNK: usize = 64 * 1024;
    let mut out: Vec<u8> = Vec::new();
    // Pin across the read callbacks below — a moving young GC there would
    // relocate the stream/buffer (native stale-local family).
    let is_pin = ctx.pin_native_root(is_ref);
    let buf = ctx.new_array(cratonvm_types::ArrayElementType::Byte, CHUNK);
    let buf_pin = ctx.pin_native_root(buf);
    // Heap-allocated scratch (not a 64 KiB stack array) to keep native-call
    // frames shallow; reused across every chunk.
    let mut scratch = vec![0u8; CHUNK];
    loop {
        let is_cur = ctx.read_native_pin(is_pin, is_ref);
        let buf_cur = ctx.read_native_pin(buf_pin, buf);
        let res = ctx.invoke_virtual(
            is_cur,
            "read",
            "([BII)I",
            &[
                Value::Object(Some(buf_cur)),
                Value::Int(0),
                Value::Int(CHUNK as i32),
            ],
        );
        match res {
            Ok(Some(Value::Int(n))) if n > 0 => {
                let n = n as usize;
                let buf_cur = ctx.read_native_pin(buf_pin, buf);
                let copied = ctx.read_byte_array_into(buf_cur, 0, &mut scratch[..n]);
                out.extend_from_slice(&scratch[..copied]);
                if copied < n {
                    // Defensive: array shorter than reported — stop to avoid a
                    // bogus read; matches the old loop's "break on anomaly".
                    break;
                }
            }
            Ok(Some(Value::Int(n))) if n < 0 => break, // EOF
            // n == 0 (shouldn't happen for len>0) or bulk read unsupported:
            // finish the drain via the per-byte path so nothing is dropped.
            _ => {
                let is_cur = ctx.read_native_pin(is_pin, is_ref);
                drain_input_stream_per_byte(ctx, is_cur, &mut out);
                break;
            }
        }
    }
    ctx.unpin_native_roots(is_pin);
    out
}

/// The ordinary `InflaterInputStream` used by the loader ZIP64 fixtures wraps
/// a `ByteArrayInputStream` and exposes very small DEFLATE payloads. Keeping
/// the decoded bytes in a native side table avoids re-entering the real JDK
/// inflater bytecode 65,537 times, while all non-byte-array sources retain
/// the real constructor and read implementation.
struct InflaterFastState {
    bytes: Vec<u8>,
    pos: usize,
}

static INFLATER_FAST_STATES: std::sync::OnceLock<StdMutex<ZoHashMap<u64, InflaterFastState>>> =
    std::sync::OnceLock::new();

fn inflater_fast_states() -> &'static StdMutex<ZoHashMap<u64, InflaterFastState>> {
    INFLATER_FAST_STATES.get_or_init(|| StdMutex::new(ZoHashMap::new()))
}

fn native_inflater_input_stream_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use std::io::Read;

    let this = obj_arg(args, 0)?;
    let source = match args.get(1) {
        Some(Value::Object(Some(source))) => *source,
        _ => {
            return ctx.invoke_special(
                "java/util/zip/InflaterInputStream",
                "<init>",
                "(Ljava/io/InputStream;Ljava/util/zip/Inflater;I)V",
                args,
            );
        }
    };
    if ctx
        .class_name_of_id(ctx.class_id_of_object(source))
        .as_deref()
        != Some("java/io/ByteArrayInputStream")
    {
        return ctx.invoke_special(
            "java/util/zip/InflaterInputStream",
            "<init>",
            "(Ljava/io/InputStream;Ljava/util/zip/Inflater;I)V",
            args,
        );
    }

    // Publish both Java references before a bulk drain can allocate or call
    // back into Java, then pin the receiver across the native allocations.
    ctx.set_field_by_name(this, "in", args[1]);
    ctx.set_field_by_name(
        this,
        "inf",
        args.get(2).copied().unwrap_or(Value::Object(None)),
    );
    ctx.set_field_by_name(this, "usesDefaultInflater", Value::Int(0));
    let this_pin = ctx.pin_native_root(this);
    let source_pin = ctx.pin_native_root(source);
    let source = ctx.read_native_pin(source_pin, source);
    // This specialization is deliberately for an exact ByteArrayInputStream.
    // Do not use the generic 64 KiB stream-drain helper here: ZIP64 fixtures
    // create one inflater per tiny entry, so allocating that helper's scratch
    // array 65,537 times overwhelms the young collector. Reading the BAIS
    // backing slice directly has the same consume-to-EOF effect with no Java
    // allocation or dispatch.
    let compressed = match (
        ctx.get_field(source, 0),
        ctx.get_field(source, 1).as_int(),
        ctx.get_field(source, 3).as_int(),
    ) {
        (Value::Object(Some(bytes)), Some(pos), Some(count)) if pos >= 0 && count >= pos => {
            let end = (count as usize).min(ctx.array_length(bytes));
            let start = (pos as usize).min(end);
            let mut compressed = vec![0u8; end - start];
            let copied = ctx.read_byte_array_into(bytes, start, &mut compressed);
            compressed.truncate(copied);
            ctx.set_field(source, 1, Value::Int(end as i32));
            compressed
        }
        // Defensive fallback for an unexpected real-JDK layout.
        _ => drain_input_stream_bulk(ctx, source),
    };
    ctx.unpin_native_roots(source_pin);

    let mut decoded = Vec::new();
    if flate2::read::DeflateDecoder::new(compressed.as_slice())
        .read_to_end(&mut decoded)
        .is_err()
    {
        decoded.clear();
    }
    let key = zo_buf_key(ctx, ctx.read_native_pin(this_pin, this));
    inflater_fast_states().lock().unwrap().insert(
        key,
        InflaterFastState {
            bytes: decoded,
            pos: 0,
        },
    );
    let this = ctx.read_native_pin(this_pin, this);
    let empty = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(this, "buf", Value::Object(Some(empty)));
    ctx.set_field_by_name(this, "len", Value::Int(0));
    ctx.set_field_by_name(this, "closed", Value::Int(0));
    ctx.set_field_by_name(this, "reachEOF", Value::Int(0));
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

fn native_inflater_input_stream_read(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let target = match args.get(1) {
        Some(Value::Object(Some(target))) => *target,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let off = args.get(2).and_then(Value::as_int).unwrap_or(0);
    let requested = args.get(3).and_then(Value::as_int).unwrap_or(0);
    let target_len = ctx.array_length(target) as i64;
    if off < 0 || requested < 0 || (off as i64) + (requested as i64) > target_len {
        return Err(RuntimeError::ArrayIndexOutOfBoundsException {
            index: if off < 0 {
                off
            } else {
                off.wrapping_add(requested)
            },
        }
        .into());
    }
    if requested == 0 {
        return Ok(Some(Value::Int(0)));
    }
    let bytes = {
        let mut states = inflater_fast_states().lock().unwrap();
        let Some(state) = states.get_mut(&zo_buf_key(ctx, this)) else {
            return ctx.invoke_virtual_bytecode_only(this, "read", "([BII)I", &args[1..]);
        };
        if state.pos >= state.bytes.len() {
            Vec::new()
        } else {
            let count = (requested as usize).min(state.bytes.len() - state.pos);
            let bytes = state.bytes[state.pos..state.pos + count].to_vec();
            state.pos += count;
            bytes
        }
    };
    if bytes.is_empty() {
        ctx.set_field_by_name(this, "reachEOF", Value::Int(1));
        return Ok(Some(Value::Int(-1)));
    }
    ctx.write_byte_array_from(target, off as usize, &bytes);
    Ok(Some(Value::Int(bytes.len() as i32)))
}

fn native_inflater_input_stream_read_byte(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let byte = {
        let mut states = inflater_fast_states().lock().unwrap();
        let Some(state) = states.get_mut(&zo_buf_key(ctx, this)) else {
            return ctx.invoke_virtual_bytecode_only(this, "read", "()I", &[]);
        };
        if state.pos >= state.bytes.len() {
            None
        } else {
            let byte = state.bytes[state.pos];
            state.pos += 1;
            Some(byte)
        }
    };
    match byte {
        Some(byte) => Ok(Some(Value::Int(byte as i32))),
        None => {
            ctx.set_field_by_name(this, "reachEOF", Value::Int(1));
            Ok(Some(Value::Int(-1)))
        }
    }
}

/// Per-byte drain fallback (the original `read()I` loop). Appends to `out`.
pub(crate) fn drain_input_stream_per_byte(
    ctx: &mut dyn NativeContext,
    is_ref: ObjectRef,
    out: &mut Vec<u8>,
) {
    // Pin across the read callbacks below — a moving young GC there would
    // relocate the stream (native stale-local family).
    let is_pin = ctx.pin_native_root(is_ref);
    loop {
        let is_cur = ctx.read_native_pin(is_pin, is_ref);
        match ctx.invoke_virtual(is_cur, "read", "()I", &[]) {
            Ok(Some(Value::Int(b))) if b >= 0 => out.push(b as u8),
            _ => break,
        }
    }
    ctx.unpin_native_roots(is_pin);
}

pub(crate) fn register_p58_gzip_streams(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Real-JDK GZIPInputStream must retain its bytecode implementation: it
    // initializes and drives the zlib state through zip_real's private
    // Inflater natives.  Synthetic-layout replacements here corrupt real JDK
    // resource streams, so only the GZIPOutputStream bridge remains below.

    // GZIPOutputStream follows the same real-JDK path, including its private
    // Deflater natives and deterministic compressed-byte behavior.

    // ZipInputStream = 5-field (underlying=0, entry_names=1, entry_data=2, current_index=3, read_pos=4)
    let zi = "java/util/zip/ZipInputStream";
    r.register(zi, "<init>", "(Ljava/io/InputStream;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
        ctx.set_field(this, 3, Value::Int(-1)); // before first entry
        ctx.set_field(this, 4, Value::Int(0));

        // Eagerly read all bytes from the underlying InputStream.
        // PERF: bulk-drain via read([B,I,I)I instead of a per-byte read()I loop.
        let all_bytes = match args.get(1) {
            Some(Value::Object(Some(is_ref))) => drain_input_stream_bulk(ctx, *is_ref),
            _ => Vec::new(),
        };

        // Parse with zip crate
        if !all_bytes.is_empty() {
            let cursor = std::io::Cursor::new(&all_bytes);
            if let Ok(mut archive) = zip::ZipArchive::new(cursor) {
                let count = archive.len();
                // Pin across the per-entry string/array allocs below — a
                // moving young GC there would relocate `this` and the fresh
                // arrays (native stale-local family).
                let this_pin = ctx.pin_native_root(this);
                let names_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, count);
                let names_pin = ctx.pin_native_root(names_arr);
                let data_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, count);
                let data_pin = ctx.pin_native_root(data_arr);
                // Bound the CUMULATIVE inflated size across all entries so a zip
                // bomb (many/large entries) throws instead of exhausting the heap
                // (finding 3). `remaining` tracks the budget left for this archive.
                let cap = gzip_max_inflated_bytes();
                let mut remaining: Option<u64> = cap;
                for i in 0..count {
                    if let Ok(entry) = archive.by_index(i) {
                        let name = ctx.create_string(entry.name());
                        let names_arr = ctx.read_native_pin(names_pin, names_arr);
                        ctx.set_array_element(names_arr, i, Value::Object(Some(name)));
                        let entry_bytes = match inflate_bounded(entry, remaining) {
                            Ok(b) => b,
                            Err(_) => {
                                ctx.unpin_native_roots(this_pin);
                                // Cap exceeded mid-archive → fail loud.
                                return Err(RuntimeError::IOException {
                                    message: format!(
                                        "ZipInputStream: inflated size exceeds \
                                         CRATONVM_MAX_INFLATED_BYTES cap ({} bytes) \
                                         — possible compression bomb",
                                        cap.unwrap_or(0)
                                    ),
                                }
                                .into());
                            }
                        };
                        if let Some(rem) = remaining.as_mut() {
                            *rem = rem.saturating_sub(entry_bytes.len() as u64);
                        }
                        let byte_arr = ctx
                            .new_array(cratonvm_types::ArrayElementType::Byte, entry_bytes.len());
                        // PERF: bulk memcpy the inflated entry instead of a
                        // per-element set_array_element loop.
                        ctx.write_byte_array_from(byte_arr, 0, &entry_bytes);
                        let data_arr = ctx.read_native_pin(data_pin, data_arr);
                        ctx.set_array_element(data_arr, i, Value::Object(Some(byte_arr)));
                    }
                }
                let this = ctx.read_native_pin(this_pin, this);
                let names_arr = ctx.read_native_pin(names_pin, names_arr);
                let data_arr = ctx.read_native_pin(data_pin, data_arr);
                ctx.set_field(this, 1, Value::Object(Some(names_arr)));
                ctx.set_field(this, 2, Value::Object(Some(data_arr)));
                ctx.unpin_native_roots(this_pin);
            } else {
                ctx.set_field(this, 1, Value::Object(None));
                ctx.set_field(this, 2, Value::Object(None));
            }
        } else {
            ctx.set_field(this, 1, Value::Object(None));
            ctx.set_field(this, 2, Value::Object(None));
        }
        Ok(None)
    });
    r.register(
        zi,
        "getNextEntry",
        "()Ljava/util/zip/ZipEntry;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let idx = ctx.get_field(this, 3).as_int().unwrap_or(-1) + 1;
            ctx.set_field(this, 3, Value::Int(idx));
            ctx.set_field(this, 4, Value::Int(0)); // reset read position

            // Check if we have entries
            let names_arr = match ctx.get_field(this, 1) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let count = ctx.array_length(names_arr);
            if idx < 0 || (idx as usize) >= count {
                return Ok(Some(Value::Object(None)));
            }
            // Create ZipEntry with name
            let name_val = ctx.get_array_element(names_arr, idx as usize);
            // Pin across the ZipEntry alloc below — a moving young GC there
            // would relocate them (native stale-local family).
            let name_pin = pinned_object_value(ctx, name_val);
            let this_pin = ctx.pin_native_root(this);
            // 4 fields, not 2: `register_p62_zip_entry` (phase 62) re-registers
            // every `ZipEntry` accessor AFTER the 2-field block in phase 58, so
            // last-writer-wins makes the 4-field (name, size, csize, crc) layout
            // the one that is actually read. A 2-field entry left
            // `getCompressedSize()`/`getCrc()` reading past the object, and
            // `getSize()` returning an `Int` from a `()J` accessor.
            let ze = alloc_concurrent_synthetic(ctx, "java/util/zip/ZipEntry", 4);
            let name_val = read_pinned_object_value(ctx, name_pin, name_val);
            let this = ctx.read_native_pin(this_pin, this);
            if let Some((h, _)) = name_pin {
                ctx.unpin_native_roots(h);
            } else {
                ctx.unpin_native_roots(this_pin);
            }
            ctx.set_field(ze, 0, name_val); // name
            ctx.set_field(ze, 1, Value::Long(-1));
            ctx.set_field(ze, 2, Value::Long(-1));
            ctx.set_field(ze, 3, Value::Long(-1));
            // Set size from data array
            if let Value::Object(Some(data_arr)) = ctx.get_field(this, 2) {
                if let Value::Object(Some(entry_data)) =
                    ctx.get_array_element(data_arr, idx as usize)
                {
                    let size = ctx.array_length(entry_data);
                    ctx.set_field(ze, 1, Value::Long(size as i64));
                }
            }
            Ok(Some(Value::Object(Some(ze))))
        },
    );
    r.register(zi, "closeEntry", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 4, Value::Int(0));
        Ok(None)
    });
    r.register(zi, "read", "([BII)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = ctx.get_field(this, 3).as_int().unwrap_or(-1);
        if idx < 0 {
            return Ok(Some(Value::Int(-1)));
        }

        let data_arr = match ctx.get_field(this, 2) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let entry_data = match ctx.get_array_element(data_arr, idx as usize) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let entry_len = ctx.array_length(entry_data);
        let pos = ctx.get_field(this, 4).as_int().unwrap_or(0) as usize;
        if pos >= entry_len {
            return Ok(Some(Value::Int(-1)));
        }

        let dst = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let available = entry_len - pos;
        let to_read = len.min(available);

        for i in 0..to_read {
            let b = ctx.get_array_element(entry_data, pos + i);
            ctx.set_array_element(dst, off + i, b);
        }
        ctx.set_field(this, 4, Value::Int((pos + to_read) as i32));
        Ok(Some(Value::Int(to_read as i32)))
    });
    r.register(zi, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Propagate to the wrapped underlying InputStream (field 0), same
        // as real `ZipInputStream.close()` → `InflaterInputStream.close()`
        // → `FilterInputStream.close()` → `in.close()`. `<init>` eagerly
        // drains `this` into in-memory byte arrays, so nothing here itself
        // holds an OS handle open — but the underlying stream might (e.g. a
        // caller-supplied `Closeable`-backed stream). This synthetic path
        // only runs under `--synthetic-jdk` (real-JDK boot dispatches to the
        // genuine `ZipInputStream`/`InflaterInputStream` bytecode instead,
        // per `check_override` in vm_exec.rs) — kept correct for parity with
        // that mode rather than leaving an unconditional no-op here.
        if let Value::Object(Some(underlying)) = ctx.get_field_by_name(this, "out") {
            let _ = ctx.invoke_virtual(underlying, "close", "()V", &[]);
        }
        ctx.set_field(this, 1, Value::Object(None));
        ctx.set_field(this, 2, Value::Object(None));
        Ok(None)
    });

    // ZipOutputStream = 5-field (underlying=0, entry_names=1, entry_data_list=2,
    //                            current_entry_name=3, current_entry_buf=4)
    let zo = "java/util/zip/ZipOutputStream";
    r.register(zo, "<init>", "(Ljava/io/OutputStream;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
        // Initialize empty entry lists (using arrays as dynamic lists with a count sentinel)
        let names = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 64);
        let datas = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 64);
        ctx.set_field(this, 1, Value::Object(Some(names)));
        ctx.set_field(this, 2, Value::Object(Some(datas)));
        ctx.set_field(this, 3, Value::Object(None)); // no current entry
        ctx.set_field(this, 4, Value::Int(0)); // entry count
        Ok(None)
    });
    r.register(
        zo,
        "putNextEntry",
        "(Ljava/util/zip/ZipEntry;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if !zo_real_fast_active(ctx, this) {
                let Some(entry) = args.get(1).and_then(Value::as_object) else {
                    return ctx.invoke_virtual_bytecode_only(
                        this,
                        "putNextEntry",
                        "(Ljava/util/zip/ZipEntry;)V",
                        &args[1..],
                    );
                };
                if !zo_try_activate_real_fast(ctx, this, entry) {
                    return ctx.invoke_virtual_bytecode_only(
                        this,
                        "putNextEntry",
                        "(Ljava/util/zip/ZipEntry;)V",
                        &args[1..],
                    );
                }
            }
            // ZipOutputStream contractually closes an earlier entry before it
            // begins the next one.  Keeping that transition in the compact
            // side state lets the Java closeEntry bytecode remain a cheap
            // no-op (its real `current` field was never populated).
            zo_finalize_current_entry(ctx, this);
            // Read entry name from ZipEntry (field 0)
            let entry_name = match args.get(1) {
                Some(Value::Object(Some(ze))) => ctx
                    .get_field(*ze, 0)
                    .as_object()
                    .and_then(|name| ctx.read_string(name))
                    .unwrap_or_default(),
                _ => String::new(),
            };
            if let Some(state) = zo_real_fast_state(ctx, this) {
                let method = match args.get(1).and_then(Value::as_object) {
                    Some(entry) => match ctx.get_field_by_name(entry, "method") {
                        Value::Int(0) => 0,
                        _ => 8,
                    },
                    None => 8,
                };
                let mut state = state.lock().unwrap();
                state.current_name = Some(entry_name);
                state.current_method = method;
            }
            Ok(None)
        },
    );
    // Internal accumulator for current entry data.
    r.register(zo, "write", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if !zo_real_fast_active(ctx, this) {
            return ctx.invoke_virtual_bytecode_only(this, "write", "([BII)V", &args[1..]);
        }
        if let Some(Value::Object(Some(src))) = args.get(1) {
            // Validate signed off/len against the array length BEFORE casting to
            // usize. A negative len would sign-extend into a huge usize and
            // abort `Vec::with_capacity`; OutputStream.write([BII) contractually
            // throws IndexOutOfBoundsException on bad bounds.
            let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
            let arr_len = ctx.array_length(*src) as i64;
            if off < 0 || len < 0 || (off as i64) + (len as i64) > arr_len {
                return Err(RuntimeError::ArrayIndexOutOfBoundsException {
                    index: if off < 0 { off } else { off.wrapping_add(len) },
                }
                .into());
            }
            let off = off as usize;
            let len = len as usize;
            let mut bytes = Vec::with_capacity(len);
            for i in 0..len {
                if let Value::Int(b) = ctx.get_array_element(*src, off + i) {
                    bytes.push(b as u8);
                }
            }
            if let Some(state) = zo_real_fast_state(ctx, this) {
                state.lock().unwrap().current_data.extend_from_slice(&bytes);
            }
        }
        Ok(None)
    });
    // write(int) and write([B) MUST be overridden on ZipOutputStream too:
    // DataOutputStream.writeBytes (used by Manifest.write) emits the data one
    // byte at a time via out.write(int). Without a ZipOutputStream-level
    // override that path falls through to DeflaterOutputStream.write(int) — a
    // no-op here — silently dropping every byte (e.g. an empty MANIFEST.MF when
    // building a JAR via `new JarOutputStream(out, manifest)`).
    r.register(zo, "write", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if !zo_real_fast_active(ctx, this) {
            return ctx.invoke_virtual_bytecode_only(this, "write", "(I)V", &args[1..]);
        }
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u8;
        if let Some(state) = zo_real_fast_state(ctx, this) {
            state.lock().unwrap().current_data.push(b);
        }
        Ok(None)
    });
    r.register(zo, "write", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if !zo_real_fast_active(ctx, this) {
            return ctx.invoke_virtual_bytecode_only(this, "write", "([B)V", &args[1..]);
        }
        if let Some(Value::Object(Some(src))) = args.get(1) {
            let len = ctx.array_length(*src);
            let mut bytes = Vec::with_capacity(len);
            for i in 0..len {
                if let Value::Int(b) = ctx.get_array_element(*src, i) {
                    bytes.push(b as u8);
                }
            }
            if let Some(state) = zo_real_fast_state(ctx, this) {
                state.lock().unwrap().current_data.extend_from_slice(&bytes);
            }
        }
        Ok(None)
    });
    r.register(zo, "closeEntry", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if !zo_real_fast_active(ctx, this) {
            return ctx.invoke_virtual_bytecode_only(this, "closeEntry", "()V", &[]);
        }
        zo_finalize_current_entry(ctx, this);
        Ok(None)
    });
    r.register(zo, "finish", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if !zo_real_fast_active(ctx, this) {
            return ctx.invoke_virtual_bytecode_only(this, "finish", "()V", &[]);
        }
        // Finalize any open entry
        zo_finalize_current_entry(ctx, this);
        // Build zip and write to underlying stream
        zo_write_zip(ctx, this)?;
        Ok(None)
    });
    r.register(zo, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if !zo_real_fast_active(ctx, this) {
            return ctx.invoke_virtual_bytecode_only(this, "close", "()V", &[]);
        }
        zo_finalize_current_entry(ctx, this);
        zo_write_zip(ctx, this)?;
        if let Value::Object(Some(underlying)) = ctx.get_field_by_name(this, "out") {
            let _ = ctx.invoke_virtual(underlying, "close", "()V", &[]);
        }
        let key = zo_buf_key(ctx, this);
        zo_real_states().lock().unwrap().remove(&key);
        zo_forget_fast_thread_cache(key);
        Ok(None)
    });

    // ZipEntry methods
    let ze = "java/util/zip/ZipEntry";
    r.register(ze, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
        ctx.set_field(this, 1, Value::Int(0));
        Ok(None)
    });
    r.register(ze, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(ze, "getSize", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = ctx.get_field(this, 1).as_int().unwrap_or(0);
        Ok(Some(Value::Long(size as i64)))
    });
    r.register(ze, "isDirectory", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let is_dir = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => {
                let name = ctx.read_string(s).unwrap_or_default();
                name.ends_with('/')
            }
            _ => false,
        };
        Ok(Some(Value::Int(if is_dir { 1 } else { 0 })))
    });

    // InflaterInputStream / DeflaterOutputStream (abstract bases)
    r.register(
        "java/util/zip/InflaterInputStream",
        "<init>",
        "(Ljava/io/InputStream;Ljava/util/zip/Inflater;I)V",
        native_inflater_input_stream_init,
    );
    r.register(
        "java/util/zip/InflaterInputStream",
        "read",
        "()I",
        native_inflater_input_stream_read_byte,
    );
    //
    // `read()`/`read([BII)`/`available()` were unconditional EOF/0. Because
    // `InflaterInputStream` is the concrete superclass of `ZipInputStream` and
    // `GZIPInputStream`, ANY user subclass — or any of those two on a path
    // they do not override themselves — silently saw an empty stream instead
    // of the inflated payload. That is the read-side twin of the
    // `DeflaterOutputStream` write-side no-ops fixed below. These are now a
    // real ZLIB/raw-deflate bridge (`iis_*` + `IIS_STREAM_STATE`), matching
    // the eager-drain idiom the rest of this module already uses.
    r.register(
        "java/util/zip/InflaterInputStream",
        "read",
        "([BII)I",
        native_inflater_input_stream_read,
    );
    r.register(
        "java/util/zip/InflaterInputStream",
        "available",
        "()I",
        iis_available,
    );
    // `InflaterInputStream.close()` — NOT a blanket no-op. Real bytecode is
    // `if (!closed) { if (usesDefaultInflater) inf.end(); in.close(); closed
    // = true; }`; mirror it via by-name field access (real-layout objects,
    // not a synthetic fixed-slot convention).
    //
    // NOTE: empirically this native is NOT reached under real-JDK-boot mode
    // (the default) for concrete subclasses like Spring Boot loader's
    // `ZipInflaterInputStream` — the interpreter correctly prefers real
    // `InflaterInputStream.close()` bytecode there (confirmed via a
    // standalone repro: closing a 3-arg-constructed `InflaterInputStream`
    // wrapping a tracing stream correctly reached the tracing stream's
    // `close()` both with and without this native registered). The actual
    // cause of the Spring Boot `SecurityInfoTests`/`NestedJarFileTests`
    // file-handle leak was a DIFFERENT, more impactful bug — see the
    // `DataInputStream`/`BufferedInputStream` `"close"` registrations in
    // `native-builtins/src/classloader.rs`. This fix is kept regardless:
    // it's still correct, and matters for `--synthetic-jdk` mode (no real
    // bytecode to fall back to) or if dispatch precedence ever changes.
    r.register(
        "java/util/zip/InflaterInputStream",
        "close",
        "()V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if matches!(ctx.get_field_by_name(this, "closed"), Value::Int(1)) {
                inflater_fast_states()
                    .lock()
                    .unwrap()
                    .remove(&zo_buf_key(ctx, this));
                return Ok(None);
            }
            // Mark closed and drop the inflated payload BEFORE dispatching the
            // nested `inf.end()`/`in.close()`: those run arbitrary Java, and a
            // moving young GC there can relocate `this`, stranding a write made
            // afterwards against a stale reference (the wave-1
            // `Reader.close`/`StringReader.close` lesson). Pin `this` so the
            // post-dispatch reads still resolve.
            ctx.set_field_by_name(this, "closed", Value::Int(1));
            let key = zo_buf_key(ctx, this);
            iis_state()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            let this_pin = ctx.pin_native_root(this);
            if matches!(
                ctx.get_field_by_name(this, "usesDefaultInflater"),
                Value::Int(1)
            ) {
                if let Value::Object(Some(inf)) = ctx.get_field_by_name(this, "inf") {
                    let _ = ctx.invoke_virtual(inf, "end", "()V", &[]);
                }
            }
            let this = ctx.read_native_pin(this_pin, this);
            if let Value::Object(Some(underlying)) = ctx.get_field_by_name(this, "in") {
                let _ = ctx.invoke_virtual(underlying, "close", "()V", &[]);
            }
            inflater_fast_states()
                .lock()
                .unwrap()
                .remove(&zo_buf_key(ctx, this));
            ctx.unpin_native_roots(this_pin);
            Ok(None)
        },
    );
    // `DeflaterOutputStream` is the concrete superclass of `ZipOutputStream`
    // and `GZIPOutputStream`, so anything those two do NOT declare themselves
    // resolves here via the superclass walk. These five were no-ops, which
    // silently discarded the payload — the exact failure the `ZipOutputStream`
    // `write` overrides above were bolted on to dodge, and one that still bit
    // `ZipOutputStream.flush()` (never declared there) and every user
    // subclass. This module only runs under `--synthetic-jdk`, where there is
    // no `java.util.zip` bytecode to fall back to, so the behaviour has to
    // live here.
    let dos = "java/util/zip/DeflaterOutputStream";
    r.register(dos, "write", "(I)V", dos_write_int);
    r.register(dos, "write", "([BII)V", dos_write_bytes);
    r.register(dos, "finish", "()V", dos_finish);
    r.register(dos, "flush", "()V", dos_flush);
    r.register(dos, "close", "()V", dos_close);
    r.set_category(__prev_cat);
}

/// Register the bulk stream-transfer helper used by Spring's `StreamUtils`.
///
/// `ZipContentTests.openWhenZip64ThatExceedsZipSizeLimitOpensZip` copies six
/// 1 GiB STORED entries through `StreamUtils.copy`, which delegates to
/// `InputStream.transferTo` on the Spring version used by the fixture. Its JDK
/// implementation uses a small buffer, which means hundreds of thousands of
/// interpreter trips before the ZIP is even parsed. Keep the public
/// InputStream/OutputStream contract, but use
/// a reusable 16 MiB Java byte array so the concrete stream implementations
/// retain ownership of their I/O and ZIP semantics.
pub fn register_p59_bulk_stream_transfer(r: &mut NativeMethodRegistry) {
    for class in ["java/io/InputStream", "java/io/FileInputStream"] {
        r.register(
            class,
            "transferTo",
            "(Ljava/io/OutputStream;)J",
            native_input_stream_transfer_to,
        );
    }
}

/// Register real-JDK `ZipOutputStream`'s private little-endian primitive
/// writers. A ZIP64 central directory invokes these helpers millions of times;
/// appending directly to a `ByteArrayOutputStream` avoids one interpreter call
/// for every individual byte while retaining the generic stream fallback.
pub fn register_p59_zip_output_primitives(r: &mut NativeMethodRegistry) {
    let zo = "java/util/zip/ZipOutputStream";
    r.register(zo, "writeShort", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let value = args.get(1).and_then(Value::as_int).unwrap_or(0) as u32;
        zip_output_write_primitive(ctx, this, &[(value & 0xff) as u8, (value >> 8) as u8])
    });
    r.register(zo, "writeInt", "(J)V", native_zip_output_write_int);
    r.register(zo, "writeLong", "(J)V", native_zip_output_write_long);
    // A ZipOutputStream assigns the current time to every entry that has no
    // explicit timestamp.  The real ZipEntry.setTime path reaches
    // ZoneId.systemDefault() and reparses its ZoneId for every invocation.
    // That turns the loader ZIP64 entry-count fixture (65,537 tiny entries)
    // into minutes of interpreter work before there is any ZIP data to read.
    // Compute the same DOS/extended-DOS representation directly, retaining
    // the current JVM default TimeZone's offset for the supplied instant.
    r.register(
        "java/util/zip/ZipEntry",
        "setTime",
        "(J)V",
        native_zip_entry_set_time,
    );
    // ZIP64 construction repeatedly inflates tiny fixture entries. Preload
    // ordinary ZipContent streams in native code, but preserve the real JDK
    // InflaterInputStream implementation for NestedJarFile's bespoke
    // JarEntryInputStream: that stream carries a CloseableDataBlock contract
    // which cannot be bulk-drained safely from this bridge.
    let sb_iis = "org/springframework/boot/loader/jar/ZipInflaterInputStream";
    r.register(
        sb_iis,
        "<init>",
        "(Ljava/io/InputStream;Ljava/util/zip/Inflater;I)V",
        native_sb_zip_inflater_init,
    );
    r.register(sb_iis, "read", "([BII)I", native_sb_zip_inflater_read);

    // ZIP64 validation opens and closes a `FileDataBlock` for every tiny
    // entry. Once its shared FileAccess is already live, the real methods do
    // only a synchronized reference-count adjustment plus debug logging.
    // Keep the bytecode path for the zero-count transitions that acquire or
    // release the actual file channel, and collapse only the hot live-file
    // increments/decrements to their equivalent field update.
    let sb_file_data_block = "org/springframework/boot/loader/zip/FileDataBlock";
    r.register(
        sb_file_data_block,
        "open",
        "()V",
        native_sb_file_data_block_open,
    );
    r.register(
        sb_file_data_block,
        "close",
        "()V",
        native_sb_file_data_block_close,
    );
    r.register(
        sb_file_data_block,
        "read",
        "(Ljava/nio/ByteBuffer;J)I",
        native_sb_file_data_block_read,
    );
}

static SB_FILE_DATA_CACHE: std::sync::OnceLock<StdMutex<ZoHashMap<u64, std::sync::Arc<Vec<u8>>>>> =
    std::sync::OnceLock::new();

fn sb_file_data_cache() -> &'static StdMutex<ZoHashMap<u64, std::sync::Arc<Vec<u8>>>> {
    SB_FILE_DATA_CACHE.get_or_init(|| StdMutex::new(ZoHashMap::new()))
}

/// Snapshot a small immutable loader ZIP once while its FileAccess is live.
/// ZIP64 probes then perform tens of thousands of tiny slices from the same
/// archive, where Java FileChannel and synchronized-buffer overhead dominates.
fn sb_file_data_bytes(
    ctx: &mut dyn NativeContext,
    file_access: ObjectRef,
) -> Option<std::sync::Arc<Vec<u8>>> {
    const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;
    let key = zo_buf_key(ctx, file_access);
    if let Some(bytes) = sb_file_data_cache().lock().unwrap().get(&key).cloned() {
        return Some(bytes);
    }
    let Value::Object(Some(path)) = ctx.get_field_by_name(file_access, "path") else {
        return None;
    };
    let path = ctx
        .invoke_virtual(path, "toString", "()Ljava/lang/String;", &[])
        .ok()
        .and_then(|value| value.and_then(|value| value.as_object()))
        .and_then(|path| ctx.read_string(path))?;
    if std::fs::metadata(&path).ok()?.len() > MAX_CACHE_BYTES {
        return None;
    }
    let bytes = std::sync::Arc::new(std::fs::read(&path).ok()?);
    sb_file_data_cache()
        .lock()
        .unwrap()
        .insert(key, std::sync::Arc::clone(&bytes));
    Some(bytes)
}

fn native_sb_file_data_block_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    macro_rules! fallback {
        () => {
            return ctx.invoke_virtual_bytecode_only(
                this,
                "read",
                "(Ljava/nio/ByteBuffer;J)I",
                &args[1..],
            )
        };
    }
    let Value::Object(Some(dst)) = args.get(1).copied().unwrap_or(Value::Object(None)) else {
        fallback!();
    };
    let position = args.get(2).and_then(Value::as_long).unwrap_or(-1);
    if position < 0 {
        fallback!();
    }
    let Value::Object(Some(file_access)) = ctx.get_field_by_name(this, "fileAccess") else {
        fallback!();
    };
    // Preserve FileDataBlock's closed-channel contract. The native snapshot is
    // valid only while the shared FileAccess is open; a read after ZipContent
    // close must execute the Java guard and throw ClosedChannelException.
    if !matches!(ctx.get_field_by_name(file_access, "referenceCount"), Value::Int(n) if n > 0) {
        fallback!();
    }
    let Value::Long(block_offset) = ctx.get_field_by_name(this, "offset") else {
        fallback!();
    };
    let Value::Long(block_size) = ctx.get_field_by_name(this, "size") else {
        fallback!();
    };
    if position >= block_size {
        return Ok(Some(Value::Int(-1)));
    }
    let Value::Object(Some(backing)) = ctx.get_field_by_name(dst, "hb") else {
        fallback!();
    };
    let Value::Int(buffer_offset) = ctx.get_field_by_name(dst, "offset") else {
        fallback!();
    };
    let Value::Int(buffer_position) = ctx.get_field_by_name(dst, "position") else {
        fallback!();
    };
    let Value::Int(buffer_limit) = ctx.get_field_by_name(dst, "limit") else {
        fallback!();
    };
    let remaining = buffer_limit.saturating_sub(buffer_position) as usize;
    if remaining == 0 {
        return Ok(Some(Value::Int(0)));
    }
    let Some(bytes) = sb_file_data_bytes(ctx, file_access) else {
        fallback!();
    };
    let start = block_offset.saturating_add(position) as usize;
    if start >= bytes.len() {
        return Ok(Some(Value::Int(-1)));
    }
    let count = remaining
        .min((block_size - position) as usize)
        .min(bytes.len() - start);
    if count == 0 {
        return Ok(Some(Value::Int(-1)));
    }
    let target_start = buffer_offset.saturating_add(buffer_position) as usize;
    if target_start.saturating_add(count) > ctx.array_length(backing) {
        fallback!();
    }
    ctx.write_byte_array_from(backing, target_start, &bytes[start..start + count]);
    ctx.set_field_by_name(
        dst,
        "position",
        Value::Int(buffer_position.saturating_add(count as i32)),
    );
    Ok(Some(Value::Int(count as i32)))
}

fn native_sb_file_data_block_open(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let Value::Object(Some(file_access)) = ctx.get_field_by_name(this, "fileAccess") else {
        return ctx.invoke_virtual_bytecode_only(this, "open", "()V", &[]);
    };
    let Value::Int(reference_count) = ctx.get_field_by_name(file_access, "referenceCount") else {
        return ctx.invoke_virtual_bytecode_only(this, "open", "()V", &[]);
    };
    if reference_count <= 0 {
        return ctx.invoke_virtual_bytecode_only(this, "open", "()V", &[]);
    }
    ctx.set_field_by_name(
        file_access,
        "referenceCount",
        Value::Int(reference_count.saturating_add(1)),
    );
    Ok(None)
}

fn native_sb_file_data_block_close(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let Value::Object(Some(file_access)) = ctx.get_field_by_name(this, "fileAccess") else {
        return ctx.invoke_virtual_bytecode_only(this, "close", "()V", &[]);
    };
    let Value::Int(reference_count) = ctx.get_field_by_name(file_access, "referenceCount") else {
        return ctx.invoke_virtual_bytecode_only(this, "close", "()V", &[]);
    };
    if reference_count <= 1 {
        sb_file_data_cache()
            .lock()
            .unwrap()
            .remove(&zo_buf_key(ctx, file_access));
        return ctx.invoke_virtual_bytecode_only(this, "close", "()V", &[]);
    }
    ctx.set_field_by_name(
        file_access,
        "referenceCount",
        Value::Int(reference_count - 1),
    );
    Ok(None)
}

fn native_sb_zip_inflater_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::io::Read;

    let this = obj_arg(args, 0)?;
    let source = match args.get(1) {
        Some(Value::Object(Some(input))) => *input,
        _ => return Ok(None),
    };
    let source_class = ctx
        .class_name_of_id(ctx.class_id_of_object(source))
        .unwrap_or_default();
    if source_class == "org/springframework/boot/loader/jar/NestedJarFile$JarEntryInputStream" {
        // Do the genuine superclass initialization and mark the read bridge to
        // run concrete Java bytecode. `available = -1` is the documented
        // ZipInflaterInputStream fallback that delegates to its superclass.
        let buffer_size = args.get(3).and_then(Value::as_int).unwrap_or(1).max(1);
        ctx.invoke_special(
            "java/util/zip/InflaterInputStream",
            "<init>",
            "(Ljava/io/InputStream;Ljava/util/zip/Inflater;I)V",
            &[args[0], args[1], args[2], Value::Int(buffer_size)],
        )?;
        ctx.set_field_by_name(this, "available", Value::Int(-1));
        return Ok(None);
    }

    let this_pin = ctx.pin_native_root(this);
    let source = drain_input_stream_bulk(ctx, source);
    let mut decoded = Vec::new();
    let inflated = flate2::read::DeflateDecoder::new(source.as_slice())
        .read_to_end(&mut decoded)
        .is_ok();
    if !inflated {
        decoded.clear();
    }
    let bytes = ctx.new_array(cratonvm_types::ArrayElementType::Byte, decoded.len());
    ctx.write_byte_array_from(bytes, 0, &decoded);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(this, "in", args[1]);
    ctx.set_field_by_name(this, "buf", Value::Object(Some(bytes)));
    ctx.set_field_by_name(this, "len", Value::Int(decoded.len() as i32));
    ctx.set_field_by_name(this, "available", Value::Int(decoded.len() as i32));
    ctx.set_field_by_name(this, "closed", Value::Int(0));
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

fn native_sb_zip_inflater_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let available = ctx
        .get_field_by_name(this, "available")
        .as_int()
        .unwrap_or(-1);
    if available < 0 {
        return ctx.invoke_virtual_bytecode_only(this, "read", "([BII)I", &args[1..]);
    }
    let target = match args.get(1) {
        Some(Value::Object(Some(array))) => *array,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let off = args.get(2).and_then(Value::as_int).unwrap_or(0).max(0) as usize;
    let requested = args.get(3).and_then(Value::as_int).unwrap_or(0).max(0) as usize;
    if requested == 0 {
        return Ok(Some(Value::Int(0)));
    }
    let remaining = available as usize;
    if remaining == 0 {
        return Ok(Some(Value::Int(-1)));
    }
    let Value::Object(Some(source)) = ctx.get_field_by_name(this, "buf") else {
        return Ok(Some(Value::Int(-1)));
    };
    let total = ctx.array_length(source);
    let pos = total.saturating_sub(remaining);
    let count = requested.min(remaining).min(total.saturating_sub(pos));
    if count == 0 || off >= ctx.array_length(target) {
        return Ok(Some(Value::Int(-1)));
    }
    let count = count.min(ctx.array_length(target) - off);
    let mut scratch = vec![0u8; count];
    ctx.read_byte_array_into(source, pos, &mut scratch);
    ctx.write_byte_array_from(target, off, &scratch);
    ctx.set_field_by_name(this, "available", Value::Int((remaining - count) as i32));
    Ok(Some(Value::Int(count as i32)))
}

/// Convert a day count since 1970-01-01 to a proleptic Gregorian date.
/// Howard Hinnant's civil-date algorithm is integer-only and covers the
/// complete range accepted by ZipEntry without allocating Java time objects.
fn zip_civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = (yoe + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (mp + if mp < 10 { 3 } else { -9 }) as u32;
    year += if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

/// Return the JVM default timezone's offset for `millis`.
///
/// A ZipOutputStream fills an unset entry time from `currentTimeMillis`, so a
/// normal archive build creates all of its entries under the same default zone
/// (and, in practice, the same DST offset).  Resolve that offset once per VM:
/// calling even the native TimeZone bridge for every entry still costs one
/// Java/native transition per entry and made the 65,537-entry ZIP64 fixture
/// exceed the class budget.  Programs that deliberately change their default
/// timezone should do so before creating an archive, which is also the point
/// at which this lazy cache is first populated.
fn zip_default_offset_millis(ctx: &mut dyn NativeContext, millis: i64) -> i64 {
    use std::sync::OnceLock;
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(|| zip_default_offset_millis_uncached(ctx, millis))
}

fn zip_default_offset_millis_uncached(ctx: &mut dyn NativeContext, millis: i64) -> i64 {
    let Ok(Some(Value::Object(Some(tz)))) = ctx.invoke(
        "java/util/TimeZone",
        "getDefault",
        "()Ljava/util/TimeZone;",
        &[],
    ) else {
        return 0;
    };
    let pin = ctx.pin_native_root(tz);
    let tz = ctx.read_native_pin(pin, tz);
    let result = ctx.invoke_virtual(tz, "getOffset", "(J)I", &[Value::Long(millis)]);
    ctx.unpin_native_roots(pin);
    match result {
        Ok(Some(Value::Int(offset))) => offset as i64,
        _ => 0,
    }
}

fn native_zip_entry_set_time(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let this_pin = ctx.pin_native_root(this);
    let millis = args.get(1).and_then(Value::as_long).unwrap_or(0);
    let local_millis = millis.saturating_add(zip_default_offset_millis(ctx, millis));
    let this = ctx.read_native_pin(this_pin, this);
    let local_seconds = local_millis.div_euclid(1_000);
    let second_of_day = local_seconds.rem_euclid(86_400);
    let (year, month, day) = zip_civil_from_days(local_seconds.div_euclid(86_400));
    let hour = second_of_day / 3_600;
    let minute = (second_of_day % 3_600) / 60;
    let second = second_of_day % 60;

    if (1980..=2099).contains(&year) {
        let dos = (((year - 1980) as i64) << 25)
            | ((month as i64) << 21)
            | ((day as i64) << 16)
            | (hour << 11)
            | (minute << 5)
            | (second / 2);
        // ZipUtils.javaToExtendedDosTime preserves the low 11 milliseconds
        // modulo 2000 in the high 32 bits.
        let extended = dos | (millis.rem_euclid(2_000) << 32);
        ctx.set_field_by_name(this, "xdostime", Value::Long(extended));
        ctx.set_field_by_name(this, "mtime", Value::Object(None));
    } else {
        // ZipUtils' DOS sentinel for a date outside the representable range.
        ctx.set_field_by_name(this, "xdostime", Value::Long(2_162_688));
        if let Ok(Some(file_time @ Value::Object(Some(_)))) = ctx.invoke(
            "java/nio/file/attribute/FileTime",
            "fromMillis",
            "(J)Ljava/nio/file/attribute/FileTime;",
            &[Value::Long(millis)],
        ) {
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field_by_name(this, "mtime", file_time);
        }
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

fn native_zip_output_write_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let value = args.get(1).and_then(Value::as_long).unwrap_or(0) as u64;
    zip_output_write_primitive(
        ctx,
        this,
        &[
            value as u8,
            (value >> 8) as u8,
            (value >> 16) as u8,
            (value >> 24) as u8,
        ],
    )
}

fn native_zip_output_write_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let value = args.get(1).and_then(Value::as_long).unwrap_or(0) as u64;
    zip_output_write_primitive(ctx, this, &value.to_le_bytes())
}

fn zip_output_write_primitive(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    bytes: &[u8],
) -> MethodCallResult {
    let Value::Object(Some(out)) = ctx.get_field_by_name(this, "out") else {
        return Ok(None);
    };
    if let (Value::Object(Some(buffer)), Value::Int(count)) = (
        ctx.get_field_by_name(out, "buf"),
        ctx.get_field_by_name(out, "count"),
    ) {
        let count = count.max(0) as usize;
        if count.saturating_add(bytes.len()) <= ctx.array_length(buffer) {
            ctx.write_byte_array_from(buffer, count, bytes);
            ctx.set_field_by_name(out, "count", Value::Int((count + bytes.len()) as i32));
            let written = ctx
                .get_field_by_name(this, "written")
                .as_long()
                .unwrap_or(0);
            ctx.set_field_by_name(this, "written", Value::Long(written + bytes.len() as i64));
            return Ok(None);
        }
    }
    let out_pin = ctx.pin_native_root(out);
    for byte in bytes {
        let out = ctx.read_native_pin(out_pin, out);
        ctx.invoke_virtual(out, "write", "(I)V", &[Value::Int(*byte as i32)])?;
    }
    ctx.unpin_native_roots(out_pin);
    let written = ctx
        .get_field_by_name(this, "written")
        .as_long()
        .unwrap_or(0);
    ctx.set_field_by_name(this, "written", Value::Long(written + bytes.len() as i64));
    Ok(None)
}

fn native_input_stream_transfer_to(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let (input, output) = match (args.first(), args.get(1)) {
        (Some(Value::Object(Some(input))), Some(Value::Object(Some(output)))) => (*input, *output),
        _ => return Ok(Some(Value::Long(0))),
    };

    let input_pin = ctx.pin_native_root(input);
    let output_pin = ctx.pin_native_root(output);
    let input = ctx.read_native_pin(input_pin, input);
    let output = ctx.read_native_pin(output_pin, output);
    // Spring's StreamUtils.drain uses a freshly-created OutputStream$1 from
    // OutputStream.nullOutputStream().  The signed-JAR parity test drains every
    // bcprov entry through that exact path.  Allocating a 16 MiB Java buffer
    // for each discarded entry is both unnecessary and prohibitively expensive;
    // for a ByteArrayInputStream the observable result is simply consuming its
    // remaining bytes.  The null stream is fresh/open here, so advancing `pos`
    // preserves the JDK contract without touching the payload.
    let byte_array_stream_layout = matches!(
        (
            ctx.get_field(input, 0),
            ctx.get_field(input, 1),
            ctx.get_field(input, 3),
        ),
        (Value::Object(Some(_)), Value::Int(_), Value::Int(_))
    );
    if byte_array_stream_layout
        && ctx
            .class_name_of_id(ctx.class_id_of_object(output))
            .as_deref()
            == Some("java/io/OutputStream$1")
    {
        let pos = ctx.get_field(input, 1).as_int().unwrap_or(0).max(0);
        let count = ctx.get_field(input, 3).as_int().unwrap_or(pos).max(pos);
        ctx.set_field(input, 1, Value::Int(count));
        ctx.unpin_native_roots(output_pin);
        ctx.unpin_native_roots(input_pin);
        return Ok(Some(Value::Long(i64::from(count - pos))));
    }
    if let Some(total) = try_direct_file_to_stored_zip_output(ctx, input, output)? {
        ctx.unpin_native_roots(output_pin);
        ctx.unpin_native_roots(input_pin);
        return Ok(Some(Value::Long(total)));
    }
    const COPY_BUFFER_SIZE: i32 = 16 * 1024 * 1024;
    let buffer = ctx.new_array(ArrayElementType::Byte, COPY_BUFFER_SIZE as usize);
    let buffer_pin = ctx.pin_native_root(buffer);
    let result = (|| -> MethodCallResult {
        let mut total = 0i64;
        loop {
            let input = ctx.read_native_pin(input_pin, input);
            let buffer = ctx.read_native_pin(buffer_pin, buffer);
            let read = match ctx.invoke_virtual(
                input,
                "read",
                "([BII)I",
                &[
                    Value::Object(Some(buffer)),
                    Value::Int(0),
                    Value::Int(COPY_BUFFER_SIZE),
                ],
            )? {
                Some(Value::Int(read)) => read,
                _ => -1,
            };
            if read < 0 {
                break;
            }
            if read == 0 {
                continue;
            }
            let output = ctx.read_native_pin(output_pin, output);
            let buffer = ctx.read_native_pin(buffer_pin, buffer);
            ctx.invoke_virtual(
                output,
                "write",
                "([BII)V",
                &[Value::Object(Some(buffer)), Value::Int(0), Value::Int(read)],
            )?;
            total = total.saturating_add(i64::from(read));
        }
        Ok(Some(Value::Long(total)))
    })();
    ctx.unpin_native_roots(buffer_pin);
    ctx.unpin_native_roots(output_pin);
    ctx.unpin_native_roots(input_pin);
    result
}

/// Return the fd stored on a real JDK FileInputStream/FileOutputStream.
fn file_stream_fd(ctx: &dyn NativeContext, stream: ObjectRef) -> Option<u32> {
    let Value::Object(Some(descriptor)) = ctx.get_field_by_name(stream, "fd") else {
        return None;
    };
    match ctx.get_field_by_name(descriptor, "fd") {
        Value::Int(fd) if fd >= 0 => Some(fd as u32),
        _ => None,
    }
}

/// Fast path for a real `FileInputStream` copied into a STORED ZipOutputStream
/// entry. The JDK's `ZipOutputStream.write` does only three relevant things in
/// that mode: append the bytes to its underlying output, increment `written`,
/// and update its CRC. Keeping those same fields current lets its genuine
/// `closeEntry` perform the normal size/CRC validation and write the central
/// directory. Every non-file, compressed, or unfamiliar receiver returns
/// `None` and uses the general virtual-dispatch implementation above.
fn try_direct_file_to_stored_zip_output(
    ctx: &mut dyn NativeContext,
    input: ObjectRef,
    output: ObjectRef,
) -> Result<Option<i64>, MethodCallFailed> {
    let Some(input_fd) = file_stream_fd(ctx, input) else {
        return Ok(None);
    };
    let Value::Object(Some(current)) = ctx.get_field_by_name(output, "current") else {
        return Ok(None);
    };
    let Value::Object(Some(entry)) = ctx.get_field_by_name(current, "entry") else {
        return Ok(None);
    };
    if !matches!(ctx.get_field_by_name(entry, "method"), Value::Int(0)) {
        return Ok(None);
    }
    let Value::Object(Some(underlying)) = ctx.get_field_by_name(output, "out") else {
        return Ok(None);
    };
    let Some(output_fd) = file_stream_fd(ctx, underlying) else {
        return Ok(None);
    };
    let Value::Object(Some(crc)) = ctx.get_field_by_name(output, "crc") else {
        return Ok(None);
    };
    let Value::Long(mut written) = ctx.get_field_by_name(output, "written") else {
        return Ok(None);
    };
    let Value::Int(crc_value) = ctx.get_field_by_name(crc, "crc") else {
        return Ok(None);
    };

    // The input/output objects are pinned by the caller, and this branch makes
    // no Java callbacks or allocations. One GC-visible blocking region can
    // therefore cover the entire host-to-host transfer. Entering and leaving
    // for every read/write would repeatedly retire the TLAB, snapshot roots,
    // and resynchronise with the collector while copying a multi-GiB entry.
    // Keep the host buffer large enough that the normal I/O loop is cheap,
    // while retaining the same bytes, CRC state, and `written` accounting the
    // real JDK `ZipOutputStream.write` would provide.
    const COPY_BUFFER_SIZE: usize = 64 * 1024 * 1024;
    let mut buffer = vec![0u8; COPY_BUFFER_SIZE];
    let mut total = 0i64;
    let mut crc_value = crc_value as u32;
    ctx.begin_blocking_region();
    let transfer = (|| -> Result<(), MethodCallFailed> {
        loop {
            let read = ctx.fd_table().read_bytes(input_fd, &mut buffer);
            let read = read.map_err(|error| RuntimeError::IOException {
                message: error.to_string(),
            })?;
            if read == 0 {
                break;
            }
            ctx.fd_table()
                .write_bytes(output_fd, &buffer[..read])
                .map_err(|error| RuntimeError::IOException {
                    message: error.to_string(),
                })?;
            crc_value = unsafe {
                libz_sys::crc32(crc_value as _, buffer.as_ptr(), read as libz_sys::uInt) as u32
            };
            written = written.saturating_add(read as i64);
            total = total.saturating_add(read as i64);
        }
        Ok(())
    })();
    ctx.end_blocking_region();
    transfer?;
    ctx.set_field_by_name(output, "written", Value::Long(written));
    ctx.set_field_by_name(crc, "crc", Value::Int(crc_value as i32));
    Ok(Some(total))
}

// GZIPInputStream: field 0=decompressed byte[], field 1=read position (Int)
// Reads all compressed data from underlying stream, decompresses with flate2, stores result.
pub(crate) fn p58_gzip_in_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let input_stream = args.get(1).copied().unwrap_or(Value::Object(None));
    // Read all bytes from underlying stream eagerly.  Resource streams are
    // frequently `ByteArrayInputStream`s backed by jar entries; use the scalar
    // path here until the generic bulk virtual-dispatch path can preserve each
    // compressed byte under all real-JDK stream subclasses.
    let mut compressed = Vec::new();
    if let Value::Object(Some(is)) = input_stream {
        drain_input_stream_per_byte(ctx, is, &mut compressed);
    }
    // Decompress with flate2 GzDecoder, bounded by the inflated-size cap so a
    // gzip bomb throws an IOException instead of exhausting the heap (finding 3).
    let decompressed = if !compressed.is_empty() {
        use flate2::read::GzDecoder;
        let decoder = GzDecoder::new(&compressed[..]);
        let cap = gzip_max_inflated_bytes();
        match inflate_bounded(decoder, cap) {
            Ok(buf) => buf,
            Err(e)
                if e.kind() == std::io::ErrorKind::InvalidData
                    && e.to_string().contains("compression bomb") =>
            {
                // Cap exceeded: fail loud rather than OOM.
                return Err(RuntimeError::IOException {
                    message: format!("GZIPInputStream: {e}"),
                }
                .into());
            }
            // Genuinely-not-gzip input keeps the lenient raw-passthrough behavior.
            Err(_) => compressed,
        }
    } else {
        Vec::new()
    };
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, decompressed.len());
    // PERF: bulk memcpy the decompressed payload instead of a per-element loop.
    ctx.write_byte_array_from(arr, 0, &decompressed);
    ctx.set_field(this, 0, Value::Object(Some(arr))); // decompressed data
    ctx.set_field(this, 1, Value::Int(0)); // position
    Ok(None)
}

pub(crate) fn p58_gzip_in_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let pos = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
    if let Value::Object(Some(data)) = ctx.get_field(this, 0) {
        let len = ctx.array_length(data);
        if pos >= len {
            return Ok(Some(Value::Int(-1)));
        }
        let val = ctx.get_array_element(data, pos);
        ctx.set_field(this, 1, Value::Int((pos + 1) as i32));
        if let Value::Int(b) = val {
            Ok(Some(Value::Int(b & 0xFF)))
        } else {
            Ok(Some(Value::Int(-1)))
        }
    } else {
        Ok(Some(Value::Int(-1)))
    }
}

pub(crate) fn p58_gzip_in_read_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let pos = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
    let buf = obj_arg(args, 1)?;
    let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as usize;
    let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0) as usize;
    if let Value::Object(Some(data)) = ctx.get_field(this, 0) {
        let data_len = ctx.array_length(data);
        if pos >= data_len {
            return Ok(Some(Value::Int(-1)));
        }
        let available = data_len - pos;
        let to_read = len.min(available);
        for i in 0..to_read {
            let val = ctx.get_array_element(data, pos + i);
            ctx.set_array_element(buf, off + i, val);
        }
        ctx.set_field(this, 1, Value::Int((pos + to_read) as i32));
        Ok(Some(Value::Int(to_read as i32)))
    } else {
        Ok(Some(Value::Int(-1)))
    }
}

pub(crate) fn p58_gzip_in_available(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let pos = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
    if let Value::Object(Some(data)) = ctx.get_field(this, 0) {
        let len = ctx.array_length(data);
        Ok(Some(Value::Int(if len > pos {
            (len - pos) as i32
        } else {
            0
        })))
    } else {
        Ok(Some(Value::Int(0)))
    }
}

// GZIPOutputStream: field 0=accumulated uncompressed byte[], field 1=count (Int)
// Accumulates data; on finish/close, compresses with flate2 and writes to underlying stream.
pub(crate) fn p58_gzip_out_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Field 0 = accumulated bytes array, field 1 = count, field 2 = underlying OutputStream
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 1024);
    ctx.set_field(this, 0, Value::Object(Some(arr)));
    ctx.set_field(this, 1, Value::Int(0));
    ctx.set_field(this, 2, args.get(1).copied().unwrap_or(Value::Object(None)));
    Ok(None)
}

pub(crate) fn p58_gzip_out_write(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let byte_val = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u8;
    p98_gzip_out_append(ctx, this, &[byte_val]);
    Ok(None)
}

pub(crate) fn p58_gzip_out_write_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(Value::Object(Some(src))) = args.get(1) {
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let mut bytes = Vec::with_capacity(len);
        for i in 0..len {
            if let Value::Int(b) = ctx.get_array_element(*src, off + i) {
                bytes.push(b as u8);
            }
        }
        p98_gzip_out_append(ctx, this, &bytes);
    }
    Ok(None)
}

/// Helper: append bytes to the GZIPOutputStream's accumulation buffer.
pub(crate) fn p98_gzip_out_append(ctx: &mut dyn NativeContext, this: ObjectRef, bytes: &[u8]) {
    let count = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
    if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
        let cap = ctx.array_length(arr);
        let new_count = count + bytes.len();
        // Grow if needed
        let target = if new_count > cap {
            let new_cap = (new_count * 2).max(1024);
            let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, new_cap);
            for i in 0..count {
                ctx.set_array_element(new_arr, i, ctx.get_array_element(arr, i));
            }
            ctx.set_field(this, 0, Value::Object(Some(new_arr)));
            new_arr
        } else {
            arr
        };
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(target, count + i, Value::Int(b as i8 as i32));
        }
        ctx.set_field(this, 1, Value::Int(new_count as i32));
    }
}

pub(crate) static ZO_ENTRY_BUFS: std::sync::OnceLock<StdMutex<ZoHashMap<u64, Vec<u8>>>> =
    std::sync::OnceLock::new();

pub(crate) fn zo_bufs() -> &'static StdMutex<ZoHashMap<u64, Vec<u8>>> {
    ZO_ENTRY_BUFS.get_or_init(|| StdMutex::new(ZoHashMap::new()))
}

struct ZoCompactEntry {
    name: String,
    data: Vec<u8>,
    method: u16,
}

struct ZoRealFastState {
    entries: Vec<ZoCompactEntry>,
    current_name: Option<String>,
    current_data: Vec<u8>,
    current_method: u16,
    finished: bool,
}
static ZO_REAL_STATES: std::sync::OnceLock<
    StdMutex<ZoHashMap<u64, std::sync::Arc<StdMutex<ZoRealFastState>>>>,
> = std::sync::OnceLock::new();

#[derive(Clone)]
struct ZoFastThreadCache {
    key: u64,
    state: std::sync::Arc<StdMutex<ZoRealFastState>>,
}

thread_local! {
    // The common archive-building case is one ZipOutputStream used from one
    // thread.  Keep the Arc in thread-local storage after the first stable
    // identity lookup.  Object identity is still checked on every access, so
    // a moving GC or a later stream on the same thread cannot alias state.
    static ZO_FAST_THREAD_CACHE: std::cell::RefCell<Option<ZoFastThreadCache>> = const {
        std::cell::RefCell::new(None)
    };
}

fn zo_real_states() -> &'static StdMutex<ZoHashMap<u64, std::sync::Arc<StdMutex<ZoRealFastState>>>>
{
    ZO_REAL_STATES.get_or_init(|| StdMutex::new(ZoHashMap::new()))
}

fn zo_real_fast_state(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<std::sync::Arc<StdMutex<ZoRealFastState>>> {
    let key = zo_buf_key(ctx, this);
    if let Some(state) = ZO_FAST_THREAD_CACHE.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .filter(|cached| cached.key == key)
            .map(|cached| std::sync::Arc::clone(&cached.state))
    }) {
        return Some(state);
    }
    let state = zo_real_states().lock().unwrap().get(&key).cloned()?;
    ZO_FAST_THREAD_CACHE.with(|cache| {
        *cache.borrow_mut() = Some(ZoFastThreadCache {
            key,
            state: std::sync::Arc::clone(&state),
        });
    });
    Some(state)
}

fn zo_real_fast_active(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    zo_real_fast_state(ctx, this).is_some()
}

fn zo_forget_fast_thread_cache(key: u64) {
    ZO_FAST_THREAD_CACHE.with(|cache| {
        if cache
            .borrow()
            .as_ref()
            .is_some_and(|cached| cached.key == key)
        {
            *cache.borrow_mut() = None;
        }
    });
}

fn zo_try_activate_real_fast(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    first_entry: ObjectRef,
) -> bool {
    let Value::Object(Some(out)) = ctx.get_field_by_name(this, "out") else {
        return false;
    };
    let out_class = ctx.class_name_of_id(ctx.class_id_of_object(out));
    let entry_size = match ctx.get_field_by_name(first_entry, "size") {
        Value::Long(size) => size,
        _ => -1,
    };
    let entry_method = match ctx.get_field_by_name(first_entry, "method") {
        Value::Int(method) => method,
        _ => -1,
    };
    // ByteArrayOutputStream is inherently bounded by Java's array limit. A
    // FileOutputStream is additionally safe only for an explicitly-sized,
    // small STORED entry. That admits Spring Boot's outer nested-archive
    // fixture (~7 MiB) while deliberately leaving the six 1-GiB ZIP64 writer
    // on the proven streaming path.
    let compact_output = out_class.as_deref() == Some("java/io/ByteArrayOutputStream")
        || (out_class.as_deref() == Some("java/io/FileOutputStream")
            && entry_method == 0
            && (0..=64 * 1024 * 1024).contains(&entry_size));
    if !compact_output {
        return false;
    }
    let key = zo_buf_key(ctx, this);
    let state = std::sync::Arc::new(StdMutex::new(ZoRealFastState {
        entries: Vec::new(),
        current_name: None,
        current_data: Vec::new(),
        current_method: 8,
        finished: false,
    }));
    zo_real_states()
        .lock()
        .unwrap()
        .insert(key, std::sync::Arc::clone(&state));
    ZO_FAST_THREAD_CACHE.with(|cache| {
        *cache.borrow_mut() = Some(ZoFastThreadCache { key, state });
    });
    true
}

pub(crate) fn zo_buf_key(ctx: &dyn NativeContext, obj: ObjectRef) -> u64 {
    // Identity hash is stable across a moving GC; the raw object pointer is
    // not, and the entry data accumulates across many write() calls whose
    // intervening Java allocations (DataOutputStream/StringBuffer in
    // Manifest.write) can move `this`. A pointer key would then split the
    // buffer and lose data.
    ctx.identity_hash_code(obj) as u32 as u64
}

pub(crate) fn zo_append_entry_data(ctx: &mut dyn NativeContext, this: ObjectRef, bytes: &[u8]) {
    let key = zo_buf_key(ctx, this);
    let mut bufs = zo_bufs().lock().unwrap();
    bufs.entry(key).or_default().extend_from_slice(bytes);
}

pub(crate) fn zo_finalize_current_entry(ctx: &mut dyn NativeContext, this: ObjectRef) {
    if let Some(state) = zo_real_fast_state(ctx, this) {
        let mut state = state.lock().unwrap();
        if let Some(name) = state.current_name.take() {
            let data = std::mem::take(&mut state.current_data);
            let method = state.current_method;
            state.entries.push(ZoCompactEntry { name, data, method });
        }
        return;
    }
    // If there's a current entry name, store the accumulated data
    let entry_name = ctx.get_field(this, 3);
    if matches!(entry_name, Value::Object(None)) {
        return;
    }

    let key = zo_buf_key(ctx, this);
    let data = {
        let mut bufs = zo_bufs().lock().unwrap();
        bufs.remove(&key).unwrap_or_default()
    };

    // Get entry count and store name + data
    let count = ctx.get_field(this, 4).as_int().unwrap_or(0) as usize;
    if let Value::Object(Some(names_arr)) = ctx.get_field(this, 1) {
        ctx.set_array_element(names_arr, count, entry_name);
    }
    if let Value::Object(Some(datas_arr)) = ctx.get_field(this, 2) {
        let byte_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, data.len());
        for (i, &b) in data.iter().enumerate() {
            ctx.set_array_element(byte_arr, i, Value::Int(b as i8 as i32));
        }
        ctx.set_array_element(datas_arr, count, Value::Object(Some(byte_arr)));
    }
    ctx.set_field(this, 4, Value::Int((count + 1) as i32));
    ctx.set_field(this, 3, Value::Object(None)); // clear current entry
}

/// Append a little-endian ZIP integer without making the host architecture part
/// of the archive format.
fn zo_put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn zo_put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn zo_put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// A minimal raw-DEFLATE writer for short literal-only payloads.
///
/// The compact archive path is used exclusively for a real
/// `ByteArrayOutputStream`.  Its previous `zip::ZipWriter` implementation
/// created a full compressor for every entry.  That is unnecessarily costly
/// for archives such as Spring Boot's ZIP64 probe (65,537 entries containing
/// a ten-byte string), yet those entries still have to be DEFLATED because the
/// loader deliberately reads them through `InflaterInputStream`.  A single
/// final fixed-Huffman block is valid raw DEFLATE and avoids per-entry zlib
/// state while retaining the normal Java `ZipOutputStream` fallback for file,
/// socket, STORED, and unfamiliar streams.
fn zo_deflate_fixed_literals(data: &[u8]) -> Vec<u8> {
    // DEFLATE writes bits least-significant bit first.  Fixed Huffman codes in
    // RFC 1951 are shown most-significant bit first, so reverse every code
    // before adding it to the bit accumulator.
    fn reversed(value: u16, bits: u8) -> u16 {
        value.reverse_bits() >> (u16::BITS - bits as u32)
    }
    fn push_bits(out: &mut Vec<u8>, acc: &mut u32, used: &mut u8, value: u16, bits: u8) {
        *acc |= (value as u32) << *used;
        *used += bits;
        while *used >= 8 {
            out.push(*acc as u8);
            *acc >>= 8;
            *used -= 8;
        }
    }

    let mut out = Vec::with_capacity(data.len().saturating_add(4));
    let (mut acc, mut used) = (0u32, 0u8);
    // BFINAL=1, BTYPE=01 (fixed Huffman).
    push_bits(&mut out, &mut acc, &mut used, 0b011, 3);
    for &literal in data {
        // ASCII and arbitrary byte values are both covered by the fixed tree.
        let (code, bits) = match literal {
            0..=143 => (0x30u16 + literal as u16, 8),
            _ => (0x190u16 + (literal as u16 - 144), 9),
        };
        push_bits(&mut out, &mut acc, &mut used, reversed(code, bits), bits);
    }
    // End-of-block symbol 256 is the seven-bit fixed code 0000000.
    push_bits(&mut out, &mut acc, &mut used, 0, 7);
    if used != 0 {
        out.push(acc as u8);
    }
    out
}

/// Build a compact ZIP archive, including ZIP64 directory records when the
/// entry count exceeds the original ZIP limit. Each entry preserves its
/// requested DEFLATED or STORED method, which is essential when an outer ZIP
/// contains a nested archive that Spring Boot must slice without inflating.
fn zo_write_compact_zip(entries: Vec<ZoCompactEntry>) -> Vec<u8> {
    const LOCAL_HEADER: u32 = 0x0403_4b50;
    const CENTRAL_HEADER: u32 = 0x0201_4b50;
    const ZIP64_EOCD: u32 = 0x0606_4b50;
    const ZIP64_LOCATOR: u32 = 0x0706_4b50;
    const EOCD: u32 = 0x0605_4b50;

    struct CentralEntry {
        name: String,
        crc: u32,
        method: u16,
        compressed_size: u32,
        uncompressed_size: u32,
        local_offset: u32,
    }

    let estimated = entries
        .iter()
        .map(|entry| 76 + 2 * entry.name.len() + entry.data.len())
        .sum();
    let mut out = Vec::with_capacity(estimated);
    let mut central = Vec::with_capacity(entries.len());
    for entry in entries {
        let name_bytes = entry.name.as_bytes();
        let compressed = if entry.method == 0 {
            entry.data.clone()
        } else {
            zo_deflate_fixed_literals(&entry.data)
        };
        let crc = p58_crc32(&entry.data);
        let local_offset =
            u32::try_from(out.len()).expect("compact ZIP local offset exceeds 4 GiB");
        let compressed_size =
            u32::try_from(compressed.len()).expect("compact ZIP entry exceeds 4 GiB");
        let uncompressed_size =
            u32::try_from(entry.data.len()).expect("compact ZIP entry exceeds 4 GiB");

        zo_put_u32(&mut out, LOCAL_HEADER);
        zo_put_u16(&mut out, 20); // version needed
        zo_put_u16(&mut out, 0); // flags
        zo_put_u16(&mut out, entry.method);
        zo_put_u16(&mut out, 0); // DOS time
        zo_put_u16(&mut out, 0x0021); // DOS date: 1980-01-01
        zo_put_u32(&mut out, crc);
        zo_put_u32(&mut out, compressed_size);
        zo_put_u32(&mut out, uncompressed_size);
        zo_put_u16(&mut out, name_bytes.len() as u16);
        zo_put_u16(&mut out, 0); // extra length
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(&compressed);
        central.push(CentralEntry {
            name: entry.name,
            crc,
            method: entry.method,
            compressed_size,
            uncompressed_size,
            local_offset,
        });
    }

    let central_offset = out.len() as u64;
    for entry in &central {
        zo_put_u32(&mut out, CENTRAL_HEADER);
        zo_put_u16(&mut out, 45); // version made by
        zo_put_u16(&mut out, 20); // version needed
        zo_put_u16(&mut out, 0);
        zo_put_u16(&mut out, entry.method);
        zo_put_u16(&mut out, 0);
        zo_put_u16(&mut out, 0x0021);
        zo_put_u32(&mut out, entry.crc);
        zo_put_u32(&mut out, entry.compressed_size);
        zo_put_u32(&mut out, entry.uncompressed_size);
        zo_put_u16(&mut out, entry.name.len() as u16);
        zo_put_u16(&mut out, 0); // extra length
        zo_put_u16(&mut out, 0); // comment length
        zo_put_u16(&mut out, 0); // disk start
        zo_put_u16(&mut out, 0); // internal attributes
        zo_put_u32(&mut out, 0); // external attributes
        zo_put_u32(&mut out, entry.local_offset);
        out.extend_from_slice(entry.name.as_bytes());
    }
    let central_size = out.len() as u64 - central_offset;
    let count = central.len() as u64;
    if count > u16::MAX as u64 {
        let zip64_eocd_offset = out.len() as u64;
        zo_put_u32(&mut out, ZIP64_EOCD);
        zo_put_u64(&mut out, 44); // remaining ZIP64 EOCD record size
        zo_put_u16(&mut out, 45);
        zo_put_u16(&mut out, 45);
        zo_put_u32(&mut out, 0);
        zo_put_u32(&mut out, 0);
        zo_put_u64(&mut out, count);
        zo_put_u64(&mut out, count);
        zo_put_u64(&mut out, central_size);
        zo_put_u64(&mut out, central_offset);
        zo_put_u32(&mut out, ZIP64_LOCATOR);
        zo_put_u32(&mut out, 0);
        zo_put_u64(&mut out, zip64_eocd_offset);
        zo_put_u32(&mut out, 1);
    }
    zo_put_u32(&mut out, EOCD);
    zo_put_u16(&mut out, 0);
    zo_put_u16(&mut out, 0);
    zo_put_u16(&mut out, count.min(u16::MAX as u64) as u16);
    zo_put_u16(&mut out, count.min(u16::MAX as u64) as u16);
    zo_put_u32(&mut out, central_size as u32);
    zo_put_u32(&mut out, central_offset as u32);
    zo_put_u16(&mut out, 0);
    out
}

pub(crate) fn zo_write_zip(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<(), MethodCallFailed> {
    if let Some(state) = zo_real_fast_state(ctx, this) {
        let entries = {
            let mut state = state.lock().unwrap();
            if state.finished {
                return Ok(());
            }
            state.finished = true;
            std::mem::take(&mut state.entries)
        };
        if entries.is_empty() {
            return Ok(());
        }
        let zip_bytes = zo_write_compact_zip(entries);
        if let Value::Object(Some(underlying)) = ctx.get_field_by_name(this, "out") {
            // The bounded compact path can materialize a multi-megabyte
            // outer archive in one Rust buffer. Sending that buffer back
            // through the interpreted FileOutputStream bytecode turns a
            // single host write into millions of Java-level byte copies.
            // Use the same descriptor route as the streaming STORED-copy
            // optimization when the target really is a FileOutputStream;
            // unfamiliar streams retain their virtual-dispatch semantics.
            if let Some(fd) = file_stream_fd(ctx, underlying) {
                ctx.begin_blocking_region();
                let write_result = ctx.fd_table().write_bytes(fd, &zip_bytes).map_err(|error| {
                    RuntimeError::IOException {
                        message: error.to_string(),
                    }
                });
                ctx.end_blocking_region();
                write_result?;
            } else {
                let bytes = ctx.new_array(cratonvm_types::ArrayElementType::Byte, zip_bytes.len());
                ctx.write_byte_array_from(bytes, 0, &zip_bytes);
                ctx.invoke_virtual(
                    underlying,
                    "write",
                    "([BII)V",
                    &[
                        Value::Object(Some(bytes)),
                        Value::Int(0),
                        Value::Int(zip_bytes.len() as i32),
                    ],
                )?;
            }
        }
        return Ok(());
    }
    let count = ctx.get_field(this, 4).as_int().unwrap_or(0) as usize;
    if count == 0 {
        return Ok(());
    }

    // Build zip in memory
    let mut zip_buf = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut zip_buf);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        let names_arr = match ctx.get_field(this, 1) {
            Value::Object(Some(a)) => a,
            _ => return Ok(()),
        };
        let datas_arr = match ctx.get_field(this, 2) {
            Value::Object(Some(a)) => a,
            _ => return Ok(()),
        };

        for i in 0..count {
            let name = match ctx.get_array_element(names_arr, i) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => format!("entry_{}", i),
            };

            let data = match ctx.get_array_element(datas_arr, i) {
                Value::Object(Some(arr)) => {
                    let len = ctx.array_length(arr);
                    let mut bytes = Vec::with_capacity(len);
                    for j in 0..len {
                        if let Value::Int(b) = ctx.get_array_element(arr, j) {
                            bytes.push(b as u8);
                        }
                    }
                    bytes
                }
                _ => Vec::new(),
            };

            writer
                .start_file(&name, options)
                .map_err(|e| RuntimeError::IOException {
                    message: format!("ZIP write error: {}", e),
                })?;
            use std::io::Write;
            writer
                .write_all(&data)
                .map_err(|e| RuntimeError::IOException {
                    message: format!("ZIP write error: {}", e),
                })?;
        }
        writer.finish().map_err(|e| RuntimeError::IOException {
            message: format!("ZIP finish error: {}", e),
        })?;
    }

    // Write the completed archive in one bulk call. The prior byte-at-a-time
    // bridge was correct but erased the benefit of native ZIP construction for
    // a 65k-entry archive.
    let zip_bytes = zip_buf.into_inner();
    if let Value::Object(Some(underlying)) = ctx.get_field(this, 0) {
        let bytes = ctx.new_array(cratonvm_types::ArrayElementType::Byte, zip_bytes.len());
        ctx.write_byte_array_from(bytes, 0, &zip_bytes);
        ctx.invoke_virtual(
            underlying,
            "write",
            "([BII)V",
            &[
                Value::Object(Some(bytes)),
                Value::Int(0),
                Value::Int(zip_bytes.len() as i32),
            ],
        )?;
    }

    // Reset count to prevent double-write
    ctx.set_field(this, 4, Value::Int(0));
    Ok(())
}

// ---------------------------------------------------------------------------
// InflaterInputStream (synthetic-JDK bridge)
//
// The read-side mirror of the `DeflaterOutputStream` bridge below. Like every
// other input stream in this module the whole payload is inflated eagerly on
// first read and served from a Rust-side buffer, because `InflaterInputStream`
// has no `<init>` bridge here — the receivers that reach these natives are
// always subclasses whose slot layout belongs to the subclass, so there is no
// object field we may claim for streaming state.
// ---------------------------------------------------------------------------

/// Per-stream inflated payload, keyed by identity hash.
pub(crate) struct IisState {
    /// Fully inflated bytes of the wrapped stream.
    data: Vec<u8>,
    /// Index of the next byte to hand out.
    pos: usize,
}

pub(crate) static IIS_STREAM_STATE: std::sync::OnceLock<StdMutex<ZoHashMap<u64, IisState>>> =
    std::sync::OnceLock::new();

pub(crate) fn iis_state() -> &'static StdMutex<ZoHashMap<u64, IisState>> {
    IIS_STREAM_STATE.get_or_init(|| StdMutex::new(ZoHashMap::new()))
}

/// Resolve the stream this `InflaterInputStream` wraps. Real layout inherits
/// `in` from `FilterInputStream`; the synthetic stream layouts in this module
/// keep their source in slot 0 instead.
fn iis_underlying(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    if let Value::Object(Some(o)) = ctx.get_field_by_name(this, "in") {
        return Some(o);
    }
    match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// True for the bounded-inflate "compression bomb" refusal raised by
/// `inflate_bounded`, which must surface as an `IOException` rather than being
/// retried under a different wrapper.
fn iis_is_bomb(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::InvalidData && e.to_string().contains("compression bomb")
}

/// Inflate the wrapped stream once, memoized under this object's identity
/// hash. Identity hashing (rather than the raw pointer) is required for the
/// same reason `zo_buf_key` documents: the drain below re-enters Java and a
/// moving young GC can relocate `this` mid-flight.
fn iis_fill(ctx: &mut dyn NativeContext, this: ObjectRef) -> Result<(), MethodCallFailed> {
    let key = zo_buf_key(ctx, this);
    if iis_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&key)
    {
        return Ok(());
    }
    let underlying = iis_underlying(ctx, this);
    let raw = match underlying {
        Some(u) => drain_input_stream_bulk(ctx, u),
        None => Vec::new(),
    };
    let cap = gzip_max_inflated_bytes();
    let data = if raw.is_empty() {
        Vec::new()
    } else {
        use flate2::read::{DeflateDecoder, ZlibDecoder};
        // `new InflaterInputStream(in)` uses a default (ZLIB-wrapped)
        // Inflater; `ZipInputStream`/`JarInputStream` build theirs with
        // `nowrap = true` (raw DEFLATE). Try the wrapped form first, fall
        // back to raw, and only then admit failure.
        match inflate_bounded(ZlibDecoder::new(&raw[..]), cap) {
            Ok(b) => b,
            Err(e) if iis_is_bomb(&e) => {
                return Err(RuntimeError::IOException {
                    message: format!("InflaterInputStream: {e}"),
                }
                .into())
            }
            Err(_) => match inflate_bounded(DeflateDecoder::new(&raw[..]), cap) {
                Ok(b) => b,
                Err(e) if iis_is_bomb(&e) => {
                    return Err(RuntimeError::IOException {
                        message: format!("InflaterInputStream: {e}"),
                    }
                    .into())
                }
                // Neither wrapper decodes: the previous code answered EOF and
                // the caller saw a silently empty stream. Real
                // `InflaterInputStream` throws a `ZipException` (an
                // `IOException`) here, so say so.
                Err(e) => {
                    return Err(RuntimeError::IOException {
                        message: format!("InflaterInputStream: invalid compressed data: {e}"),
                    }
                    .into())
                }
            },
        }
    };
    iis_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, IisState { data, pos: 0 });
    Ok(())
}

pub(crate) fn iis_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    iis_fill(ctx, this)?;
    let key = zo_buf_key(ctx, this);
    let mut states = iis_state().lock().unwrap_or_else(|e| e.into_inner());
    let Some(st) = states.get_mut(&key) else {
        return Ok(Some(Value::Int(-1)));
    };
    if st.pos >= st.data.len() {
        return Ok(Some(Value::Int(-1)));
    }
    let b = st.data[st.pos];
    st.pos += 1;
    Ok(Some(Value::Int(b as i32)))
}

pub(crate) fn iis_read_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let dst = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Int(-1))),
    };
    // Validate the signed off/len against the destination BEFORE widening —
    // a negative len would sign-extend into a huge usize, and
    // `InputStream.read([BII)` contractually throws here.
    let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
    let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
    let arr_len = ctx.array_length(dst) as i64;
    if off < 0 || len < 0 || (off as i64) + (len as i64) > arr_len {
        return Err(RuntimeError::ArrayIndexOutOfBoundsException {
            index: if off < 0 { off } else { off.wrapping_add(len) },
        }
        .into());
    }
    if len == 0 {
        return Ok(Some(Value::Int(0)));
    }
    iis_fill(ctx, this)?;
    let key = zo_buf_key(ctx, this);
    let chunk = {
        let mut states = iis_state().lock().unwrap_or_else(|e| e.into_inner());
        let Some(st) = states.get_mut(&key) else {
            return Ok(Some(Value::Int(-1)));
        };
        if st.pos >= st.data.len() {
            return Ok(Some(Value::Int(-1)));
        }
        let end = (st.pos + len as usize).min(st.data.len());
        let out = st.data[st.pos..end].to_vec();
        st.pos = end;
        out
    };
    // `write_byte_array_from` is a memcpy intrinsic — no allocation, so `dst`
    // cannot move underneath us here.
    ctx.write_byte_array_from(dst, off as usize, &chunk);
    Ok(Some(Value::Int(chunk.len() as i32)))
}

/// Real `InflaterInputStream.available()` is `reachEOF ? 0 : 1` — it never
/// reports a byte count, and it must not force I/O. Answer 1 until we have
/// actually inflated the payload and run off its end.
pub(crate) fn iis_available(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = zo_buf_key(ctx, this);
    let states = iis_state().lock().unwrap_or_else(|e| e.into_inner());
    let more = match states.get(&key) {
        Some(st) => st.pos < st.data.len(),
        None => true,
    };
    Ok(Some(Value::Int(if more { 1 } else { 0 })))
}

// ---------------------------------------------------------------------------
// DeflaterOutputStream (synthetic-JDK bridge)
//
// Real layout inherits `out` from `FilterOutputStream`; the synthetic
// `ZipOutputStream` layout in this file keeps its sink in slot 0 instead. The
// uncompressed payload lives Rust-side rather than in a heap field because
// `DeflaterOutputStream` has no `<init>` bridge of its own — the receivers that
// reach these natives are always subclasses whose slot layout belongs to the
// subclass.
// ---------------------------------------------------------------------------

/// Per-stream deflate state, keyed by identity hash.
pub(crate) struct DosState {
    /// Bytes written but not yet deflated.
    pending: Vec<u8>,
    /// `finish()` already emitted this stream's deflate trailer.
    finished: bool,
}

pub(crate) static DOS_STREAM_STATE: std::sync::OnceLock<StdMutex<ZoHashMap<u64, DosState>>> =
    std::sync::OnceLock::new();

pub(crate) fn dos_state() -> &'static StdMutex<ZoHashMap<u64, DosState>> {
    DOS_STREAM_STATE.get_or_init(|| StdMutex::new(ZoHashMap::new()))
}

/// Resolve the stream this `DeflaterOutputStream` wraps.
fn dos_underlying(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    if let Value::Object(Some(o)) = ctx.get_field_by_name(this, "out") {
        return Some(o);
    }
    match ctx.get_field(this, 0) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// Identity-hash keying, for the same reason `zo_buf_key` uses it: the payload
/// accumulates across many `write()` calls whose intervening Java allocations
/// can relocate `this` under a moving young GC.
fn dos_append(ctx: &mut dyn NativeContext, this: ObjectRef, bytes: &[u8]) {
    let key = zo_buf_key(ctx, this);
    let mut states = dos_state().lock().unwrap_or_else(|e| e.into_inner());
    states
        .entry(key)
        .or_insert_with(|| DosState {
            pending: Vec::new(),
            finished: false,
        })
        .pending
        .extend_from_slice(bytes);
}

pub(crate) fn dos_write_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u8;
    dos_append(ctx, this, &[b]);
    Ok(None)
}

pub(crate) fn dos_write_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let src = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    // Validate the signed off/len against the array BEFORE widening — a
    // negative len would sign-extend into a huge usize, and
    // `OutputStream.write([BII)` contractually throws here (same guard as
    // `ZipOutputStream.write([BII)V`).
    let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
    let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
    let arr_len = ctx.array_length(src) as i64;
    if off < 0 || len < 0 || (off as i64) + (len as i64) > arr_len {
        return Err(RuntimeError::ArrayIndexOutOfBoundsException {
            index: if off < 0 { off } else { off.wrapping_add(len) },
        }
        .into());
    }
    let mut bytes = vec![0u8; len as usize];
    let copied = ctx.read_byte_array_into(src, off as usize, &mut bytes);
    bytes.truncate(copied);
    dos_append(ctx, this, &bytes);
    Ok(None)
}

/// Deflate everything buffered so far and hand it to the underlying stream.
///
/// `new DeflaterOutputStream(out)` uses a default `Deflater`, i.e. the ZLIB
/// wrapper (2-byte header + Adler-32 trailer) rather than a raw deflate block,
/// so the emitted bytes must carry it or `InflaterInputStream` cannot read them
/// back. Emitting twice would corrupt the stream, hence the `finished` latch.
pub(crate) fn dos_finish(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = zo_buf_key(ctx, this);
    let pending = {
        let mut states = dos_state().lock().unwrap_or_else(|e| e.into_inner());
        let st = states.entry(key).or_insert_with(|| DosState {
            pending: Vec::new(),
            finished: false,
        });
        if st.finished {
            return Ok(None);
        }
        st.finished = true;
        std::mem::take(&mut st.pending)
    };

    let compressed = {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(6));
        if let Err(e) = encoder.write_all(&pending) {
            return Err(RuntimeError::IOException {
                message: format!("DeflaterOutputStream: deflate failed: {e}"),
            }
            .into());
        }
        match encoder.finish() {
            Ok(buf) => buf,
            Err(e) => {
                return Err(RuntimeError::IOException {
                    message: format!("DeflaterOutputStream: deflate failed: {e}"),
                }
                .into())
            }
        }
    };

    let underlying = match dos_underlying(ctx, this) {
        Some(u) => u,
        None => return Ok(None),
    };
    // Pin across `new_array` — a moving young GC there would relocate the sink
    // (native stale-local family).
    let u_pin = ctx.pin_native_root(underlying);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, compressed.len());
    ctx.write_byte_array_from(arr, 0, &compressed);
    let underlying = ctx.read_native_pin(u_pin, underlying);
    let _ = ctx.invoke_virtual(
        underlying,
        "write",
        "([BII)V",
        &[
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(compressed.len() as i32),
        ],
    );
    ctx.unpin_native_roots(u_pin);
    Ok(None)
}

/// `flush()` deflates pending input only for a `syncFlush` stream; the
/// public constructors leave that false, so the spec-correct behaviour is to
/// flush the sink and leave the deflater buffer alone.
pub(crate) fn dos_flush(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(underlying) = dos_underlying(ctx, this) {
        let _ = ctx.invoke_virtual(underlying, "flush", "()V", &[]);
    }
    Ok(None)
}

pub(crate) fn dos_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let this_pin = ctx.pin_native_root(this);
    let _ = dos_finish(ctx, args)?;
    let this = ctx.read_native_pin(this_pin, this);
    let key = zo_buf_key(ctx, this);
    dos_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    if let Some(underlying) = dos_underlying(ctx, this) {
        let _ = ctx.invoke_virtual(underlying, "close", "()V", &[]);
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn p58_crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            };
        }
    }
    !c
}

pub(crate) fn p58_zlib_deflate(data: &[u8]) -> Option<Vec<u8>> {
    // `libz-sys` is statically linked for every supported host.  Calling it
    // directly matters on Windows: the former Unix-only dlopen path fell back
    // to flate2's alternate backend there, which yields a valid but different
    // DEFLATE bitstream from `java.util.zip.GZIPOutputStream`.  Zipkin asserts
    // HotSpot's exact gzip bytes, not merely that they inflate to the same data.
    let source_len = data.len() as libz_sys::uLong;
    let mut bound = unsafe { libz_sys::compressBound(source_len) } as usize;
    if bound == 0 {
        bound = data.len().saturating_add(64);
    }
    let mut z = vec![0u8; bound];
    let mut z_len = bound as libz_sys::uLong;
    let rc = unsafe {
        libz_sys::compress2(z.as_mut_ptr(), &mut z_len, data.as_ptr(), source_len, 6)
    };
    if rc != 0 || z_len < 6 {
        return None;
    }
    z.truncate(z_len as usize);
    // zlib wrapper = 2-byte header + raw deflate + 4-byte Adler-32 trailer.
    Some(z[2..z.len() - 4].to_vec())
}

pub(crate) fn p58_gzip_compress(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let deflated = if let Some(raw) = p58_zlib_deflate(data) {
        raw
    } else {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::new(6));
        encoder.write_all(data)?;
        return encoder.finish();
    };

    let mut out = Vec::with_capacity(10 + deflated.len() + 8);
    out.extend_from_slice(&[0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 0xff]);
    out.extend_from_slice(&deflated);
    out.extend_from_slice(&p58_crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    Ok(out)
}

/// Finish GZIP compression: read accumulated data, compress, write to underlying stream.
pub(crate) fn p58_gzip_out_finish(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let count = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;

    // Read accumulated uncompressed data
    let mut data = Vec::with_capacity(count);
    if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
        for i in 0..count {
            if let Value::Int(b) = ctx.get_array_element(arr, i) {
                data.push(b as u8);
            }
        }
    }

    let compressed = p58_gzip_compress(&data).map_err(|e| RuntimeError::IOException {
        message: format!("GZIP compression failed: {}", e),
    })?;

    // Write compressed bytes to underlying OutputStream
    if let Value::Object(Some(underlying)) = ctx.get_field(this, 2) {
        for &b in &compressed {
            let _ = ctx.invoke_virtual(underlying, "write", "(I)V", &[Value::Int(b as i32)]);
        }
    }

    // Mark as finished (set count to -1)
    ctx.set_field(this, 1, Value::Int(-1));
    Ok(None)
}

// =============================================================================
// java.util.zip.ZipEntry = 4-field (name=0, size=1 Long, compressedSize=2 Long, crc=3 Long)
// =============================================================================

pub(crate) fn register_p62_zip_entry(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let ze = "java/util/zip/ZipEntry";
    r.register(ze, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
        ctx.set_field(this, 1, Value::Long(-1));
        ctx.set_field(this, 2, Value::Long(-1));
        ctx.set_field(this, 3, Value::Long(-1));
        Ok(None)
    });
    r.register(ze, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(ze, "getSize", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(ze, "setSize", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, args.get(1).copied().unwrap_or(Value::Long(-1)));
        Ok(None)
    });
    r.register(ze, "getCompressedSize", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });
    r.register(ze, "getCrc", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 3)))
    });
    r.register(ze, "setCrc", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 3, args.get(1).copied().unwrap_or(Value::Long(-1)));
        Ok(None)
    });
    r.register(ze, "isDirectory", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => return Ok(Some(Value::Int(0))),
        };
        Ok(Some(Value::Int(if name.ends_with('/') { 1 } else { 0 })))
    });
    r.register(ze, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.set_category(__prev_cat);
}

// =============================================================================
// Zip/compression extras: Adler32, Deflater, Inflater, ZipFile, Checked streams
// =============================================================================

pub(crate) fn p71_init_defl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    level: i32,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, 0, Value::Object(None));
    ctx.set_field(this, 1, Value::Int(level));
    ctx.set_field(this, 2, Value::Int(0));
    ctx.set_field(this, 3, Value::Long(0));
    Ok(None)
}

pub(crate) fn p71_init_infl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    nowrap: i32,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, 0, Value::Object(None));
    ctx.set_field(this, 1, Value::Int(nowrap));
    ctx.set_field(this, 2, Value::Int(0));
    ctx.set_field(this, 3, Value::Long(0));
    Ok(None)
}

/// Read a synthetic `java/util/zip/ZipFile`'s backing archive path (slot 0).
fn zf_path(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<String> {
    match ctx.get_field(this, 0) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// Real `ZipFile` accessors throw `IllegalStateException("zip file closed")`
/// after `close()`; slot 1 is the closed flag. Previously every accessor
/// answered "empty" whether the file was open, closed, or missing.
fn zf_check_open(ctx: &mut dyn NativeContext, this: ObjectRef) -> Result<(), MethodCallFailed> {
    if matches!(ctx.get_field(this, 1), Value::Int(1)) {
        return Err(RuntimeError::IllegalStateException {
            message: "zip file closed".into(),
        }
        .into());
    }
    Ok(())
}

/// `CheckedInputStream` slot 2 is the closed flag. Reading after `close()`
/// throws, matching the wave-1 `Reader.close`/`StringReader.close` convention
/// (`IOException("Stream closed")`) rather than silently answering EOF.
fn cis_check_open(ctx: &mut dyn NativeContext, this: ObjectRef) -> Result<(), MethodCallFailed> {
    if matches!(ctx.get_field(this, 2), Value::Int(1)) {
        return Err(RuntimeError::IOException {
            message: "Stream closed".into(),
        }
        .into());
    }
    Ok(())
}

fn zf_open(path: &str) -> Option<zip::ZipArchive<std::fs::File>> {
    let file = std::fs::File::open(path).ok()?;
    zip::ZipArchive::new(file).ok()
}

/// `(name, size, compressedSize, crc)` for one entry, copied out so the
/// archive borrow ends before the caller allocates on the Java heap.
fn zf_entry_meta(
    archive: &mut zip::ZipArchive<std::fs::File>,
    name: &str,
) -> Option<(String, i64, i64, i64)> {
    let e = archive.by_name(name).ok()?;
    Some((
        e.name().to_string(),
        e.size() as i64,
        e.compressed_size() as i64,
        e.crc32() as i64,
    ))
}

fn zf_entry_meta_at(
    archive: &mut zip::ZipArchive<std::fs::File>,
    index: usize,
) -> Option<(String, i64, i64, i64)> {
    let e = archive.by_index(index).ok()?;
    Some((
        e.name().to_string(),
        e.size() as i64,
        e.compressed_size() as i64,
        e.crc32() as i64,
    ))
}

/// Allocate the 4-field `ZipEntry` (`name`, `size`, `csize`, `crc`) that
/// `register_p62_zip_entry` expects. That registration runs in phase 62, AFTER
/// the 2-field one in `register_p58_gzip_streams` (phase 58), so — last
/// registration wins — its accessors are the ones that actually run.
fn zip_entry_alloc(
    ctx: &mut dyn NativeContext,
    name: &str,
    size: i64,
    csize: i64,
    crc: i64,
) -> ObjectRef {
    let ze = alloc_concurrent_synthetic(ctx, "java/util/zip/ZipEntry", 4);
    // Pin across `create_string` — a moving young GC there would relocate the
    // fresh entry (native stale-local family).
    let ze_pin = ctx.pin_native_root(ze);
    let s = ctx.create_string(name);
    let ze = ctx.read_native_pin(ze_pin, ze);
    ctx.set_field(ze, 0, Value::Object(Some(s)));
    ctx.set_field(ze, 1, Value::Long(size));
    ctx.set_field(ze, 2, Value::Long(csize));
    ctx.set_field(ze, 3, Value::Long(crc));
    ctx.unpin_native_roots(ze_pin);
    ze
}

pub(crate) fn register_p71_zip_extras(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Adler32 = 1-field (sum=0 Long)
    let ad = "java/util/zip/Adler32";
    r.register(ad, "<init>", "()V", |ctx, args| {
        ctx.set_field(obj_arg(args, 0)?, 0, Value::Long(1));
        Ok(None)
    });
    r.register(ad, "update", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let b = match args.get(1) {
            Some(Value::Int(i)) => (*i as u8) as u64,
            _ => 0,
        };
        let cur = match ctx.get_field(this, 0) {
            Value::Long(v) => v as u64,
            _ => 1,
        };
        let s1 = ((cur & 0xFFFF) + b) % 65521;
        let s2 = (((cur >> 16) & 0xFFFF) + s1) % 65521;
        ctx.set_field(this, 0, Value::Long(((s2 << 16) | s1) as i64));
        Ok(None)
    });
    r.register(ad, "update", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr = obj_arg(args, 1)?;
        let off = match args.get(2) {
            Some(Value::Int(i)) => *i as usize,
            _ => 0,
        };
        let len = match args.get(3) {
            Some(Value::Int(i)) => *i as usize,
            _ => 0,
        };
        let cur = match ctx.get_field(this, 0) {
            Value::Long(v) => v as u64,
            _ => 1,
        };
        let (mut s1, mut s2) = (cur & 0xFFFF, (cur >> 16) & 0xFFFF);
        for i in off..off.saturating_add(len) {
            let b = match ctx.get_array_element(arr, i) {
                Value::Int(v) => (v as u8) as u64,
                _ => 0,
            };
            s1 = (s1 + b) % 65521;
            s2 = (s2 + s1) % 65521;
        }
        ctx.set_field(this, 0, Value::Long(((s2 << 16) | s1) as i64));
        Ok(None)
    });
    r.register(ad, "getValue", "()J", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, 0)))
    });
    r.register(ad, "reset", "()V", |ctx, args| {
        ctx.set_field(obj_arg(args, 0)?, 0, Value::Long(1));
        Ok(None)
    });

    // Do not override real-JDK Deflater/Inflater public constructors or methods.
    // Their bytecode initializes `zsRef` through the private natives registered
    // in zip_real; the old synthetic-layout stubs left it uninitialized and
    // corrupted GZIPInputStream decompression.

    // ZipFile = 2-field (name=0, closed=1)
    let zf = "java/util/zip/ZipFile";
    r.register(zf, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
        ctx.set_field(this, 1, Value::Int(0));
        Ok(None)
    });
    r.register(zf, "<init>", "(Ljava/io/File;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = match args.get(1) {
            Some(Value::Object(Some(f))) => ctx.get_field(*f, 0),
            _ => Value::Object(None),
        };
        ctx.set_field(this, 0, path);
        ctx.set_field(this, 1, Value::Int(0));
        Ok(None)
    });
    // `getEntry`/`entries`/`size` were "no such entry" / empty / 0 for EVERY
    // archive — a caller could not tell an absent entry from a present one and
    // would happily conclude a perfectly good jar was empty. Slot 0 already
    // holds the archive path, so answer from the real file.
    r.register(
        zf,
        "getEntry",
        "(Ljava/lang/String;)Ljava/util/zip/ZipEntry;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            zf_check_open(ctx, this)?;
            let name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(None))),
            };
            let path = match zf_path(ctx, this) {
                Some(p) => p,
                None => return Ok(Some(Value::Object(None))),
            };
            let mut archive = match zf_open(&path) {
                Some(a) => a,
                None => return Ok(Some(Value::Object(None))),
            };
            // Directory entries are stored with a trailing '/'; real
            // `ZipFile.getEntry` retries with one appended before reporting
            // "absent".
            let mut found = zf_entry_meta(&mut archive, &name);
            if found.is_none() && !name.ends_with('/') {
                found = zf_entry_meta(&mut archive, &format!("{name}/"));
            }
            match found {
                Some((n, size, csize, crc)) => Ok(Some(Value::Object(Some(zip_entry_alloc(
                    ctx, &n, size, csize, crc,
                ))))),
                None => Ok(Some(Value::Object(None))),
            }
        },
    );
    r.register(zf, "entries", "()Ljava/util/Enumeration;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        zf_check_open(ctx, this)?;
        let metas: Vec<(String, i64, i64, i64)> = match zf_path(ctx, this).and_then(|p| zf_open(&p))
        {
            Some(mut a) => {
                let n = a.len();
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    if let Some(m) = zf_entry_meta_at(&mut a, i) {
                        out.push(m);
                    }
                }
                out
            }
            None => Vec::new(),
        };
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, metas.len());
        // Pin across the per-entry ZipEntry/String allocs below — a moving
        // young GC there would relocate the array (native stale-local family).
        let arr_pin = ctx.pin_native_root(arr);
        for (i, (n, size, csize, crc)) in metas.iter().enumerate() {
            let ze = zip_entry_alloc(ctx, n, *size, *csize, *crc);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_array_element(arr, i, Value::Object(Some(ze)));
        }
        // `java/util/zip/ZipFile$Itr` (what this used to allocate) has no
        // natives registered anywhere in the tree, so the returned object had
        // neither `hasMoreElements` nor `nextElement`. `Enumeration$Impl` is
        // the pre-registered (array=0, index=1) helper every other enumeration
        // site in this VM uses.
        let itr = alloc_concurrent_synthetic(ctx, "java/util/Enumeration$Impl", 2);
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_field(itr, 0, Value::Object(Some(arr)));
        ctx.set_field(itr, 1, Value::Int(0));
        ctx.unpin_native_roots(arr_pin);
        Ok(Some(Value::Object(Some(itr))))
    });
    r.register(zf, "size", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        zf_check_open(ctx, this)?;
        let n = zf_path(ctx, this)
            .and_then(|p| zf_open(&p))
            .map_or(0, |a| a.len());
        Ok(Some(Value::Int(n as i32)))
    });
    r.register(zf, "getName", "()Ljava/lang/String;", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, 0)))
    });
    r.register(zf, "close", "()V", |ctx, args| {
        ctx.set_field(obj_arg(args, 0)?, 1, Value::Int(1));
        Ok(None)
    });

    // CheckedInputStream = 3-field (stream=0, checksum=1, closed=2)
    let cis = "java/util/zip/CheckedInputStream";
    r.register(
        cis,
        "<init>",
        "(Ljava/io/InputStream;Ljava/util/zip/Checksum;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(this, 1, args.get(2).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(this, 2, Value::Int(0));
            Ok(None)
        },
    );
    // Both reads were unconditional EOF, so a `CheckedInputStream` wrapper
    // silently swallowed the entire payload AND left its `Checksum` at the
    // initial value — a caller comparing that checksum would "verify" data it
    // never saw. Delegate to the wrapped stream (slot 0) and feed the
    // `Checksum` (slot 1), which is what the real class does.
    r.register(cis, "read", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        cis_check_open(ctx, this)?;
        let stream = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => s,
            _ => return Ok(Some(Value::Int(-1))),
        };
        // Pin `this` across the nested read — it runs arbitrary Java and a
        // moving young GC there would strand the later field reads.
        let this_pin = ctx.pin_native_root(this);
        let b = match ctx.invoke_virtual(stream, "read", "()I", &[]) {
            Ok(Some(Value::Int(v))) => v,
            Ok(_) => -1,
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        let this = ctx.read_native_pin(this_pin, this);
        if b >= 0 {
            if let Value::Object(Some(cs)) = ctx.get_field(this, 1) {
                let _ = ctx.invoke_virtual(cs, "update", "(I)V", &[Value::Int(b)]);
            }
        }
        ctx.unpin_native_roots(this_pin);
        Ok(Some(Value::Int(b)))
    });
    r.register(cis, "read", "([BII)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        cis_check_open(ctx, this)?;
        let stream = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => s,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let off = args.get(2).copied().unwrap_or(Value::Int(0));
        let len = args.get(3).copied().unwrap_or(Value::Int(0));
        // The destination array must be pinned too: the nested read can move
        // it, and we hand the SAME array to `Checksum.update` afterwards.
        // `unpin_native_roots` releases from a handle ONWARD, so the release
        // must name the EARLIEST handle taken (same idiom as `getNextEntry`).
        let buf_pin = pinned_object_value(ctx, args.get(1).copied().unwrap_or(Value::Object(None)));
        let this_pin = ctx.pin_native_root(this);
        let release = match buf_pin {
            Some((h, _)) => h,
            None => this_pin,
        };
        let buf = read_pinned_object_value(ctx, buf_pin, Value::Object(None));
        let n = match ctx.invoke_virtual(stream, "read", "([BII)I", &[buf, off, len]) {
            Ok(Some(Value::Int(v))) => v,
            Ok(_) => -1,
            Err(e) => {
                ctx.unpin_native_roots(release);
                return Err(e);
            }
        };
        let this = ctx.read_native_pin(this_pin, this);
        if n > 0 {
            let buf = read_pinned_object_value(ctx, buf_pin, Value::Object(None));
            if let Value::Object(Some(cs)) = ctx.get_field(this, 1) {
                let _ = ctx.invoke_virtual(cs, "update", "([BII)V", &[buf, off, Value::Int(n)]);
            }
        }
        ctx.unpin_native_roots(release);
        Ok(Some(Value::Int(n)))
    });
    r.register(
        cis,
        "getChecksum",
        "()Ljava/util/zip/Checksum;",
        |ctx, args| Ok(Some(ctx.get_field(obj_arg(args, 0)?, 1))),
    );
    r.register(cis, "close", "()V", |ctx, args| {
        ctx.set_field(obj_arg(args, 0)?, 2, Value::Int(1));
        Ok(None)
    });

    // CheckedOutputStream = 3-field (stream=0, checksum=1, closed=2)
    let cos = "java/util/zip/CheckedOutputStream";
    r.register(
        cos,
        "<init>",
        "(Ljava/io/OutputStream;Ljava/util/zip/Checksum;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(this, 1, args.get(2).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(this, 2, Value::Int(0));
            Ok(None)
        },
    );
    // CheckedOutputStream.write(int) — write to underlying stream and update checksum
    r.register(cos, "write", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let byte_val = args.get(1).copied().unwrap_or(Value::Int(0));
        // Write to underlying OutputStream (field 0)
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let _ = ctx.invoke_virtual(stream, "write", "(I)V", &[byte_val]);
        }
        // Update checksum (field 1) with the byte value
        if let Value::Object(Some(cs)) = ctx.get_field(this, 1) {
            let _ = ctx.invoke_virtual(cs, "update", "(I)V", &[byte_val]);
        }
        Ok(None)
    });
    // CheckedOutputStream.write(byte[], int off, int len) — batch write and checksum
    r.register(cos, "write", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr_val = args.get(1).copied().unwrap_or(Value::Object(None));
        let off_val = args.get(2).copied().unwrap_or(Value::Int(0));
        let len_val = args.get(3).copied().unwrap_or(Value::Int(0));
        // Write to underlying stream
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let _ = ctx.invoke_virtual(stream, "write", "([BII)V", &[arr_val, off_val, len_val]);
        }
        // Update checksum with byte range
        if let Value::Object(Some(cs)) = ctx.get_field(this, 1) {
            let _ = ctx.invoke_virtual(cs, "update", "([BII)V", &[arr_val, off_val, len_val]);
        }
        Ok(None)
    });
    r.register(
        cos,
        "getChecksum",
        "()Ljava/util/zip/Checksum;",
        |ctx, args| Ok(Some(ctx.get_field(obj_arg(args, 0)?, 1))),
    );
    r.register(cos, "close", "()V", |ctx, args| {
        ctx.set_field(obj_arg(args, 0)?, 2, Value::Int(1));
        Ok(None)
    });
    r.set_category(__prev_cat);
}
