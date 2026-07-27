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

    // GZIPOutputStream — accumulates data in field 0/1, compresses on finish
    let go = "java/util/zip/GZIPOutputStream";
    r.register(go, "<init>", "(Ljava/io/OutputStream;)V", p58_gzip_out_init);
    r.register(go, "write", "(I)V", p58_gzip_out_write);
    r.register(go, "write", "([BII)V", p58_gzip_out_write_bytes);
    r.register(go, "finish", "()V", p58_gzip_out_finish);
    r.register(go, "flush", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Flush underlying stream
        if let Value::Object(Some(underlying)) = ctx.get_field(this, 2) {
            let _ = ctx.invoke_virtual(underlying, "flush", "()V", &[]);
        }
        Ok(None)
    });
    r.register(go, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Finish compression if not already done (count >= 0 means not finished)
        let count = ctx.get_field(this, 1).as_int().unwrap_or(0);
        if count >= 0 {
            p58_gzip_out_finish(ctx, args)?;
        }
        // Close underlying stream
        if let Value::Object(Some(underlying)) = ctx.get_field(this, 2) {
            let _ = ctx.invoke_virtual(underlying, "close", "()V", &[]);
        }
        Ok(None)
    });

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
            let ze = alloc_concurrent_synthetic(ctx, "java/util/zip/ZipEntry", 2);
            let name_val = read_pinned_object_value(ctx, name_pin, name_val);
            let this = ctx.read_native_pin(this_pin, this);
            if let Some((h, _)) = name_pin {
                ctx.unpin_native_roots(h);
            } else {
                ctx.unpin_native_roots(this_pin);
            }
            ctx.set_field(ze, 0, name_val); // name
                                            // Set size from data array
            if let Value::Object(Some(data_arr)) = ctx.get_field(this, 2) {
                if let Value::Object(Some(entry_data)) =
                    ctx.get_array_element(data_arr, idx as usize)
                {
                    let size = ctx.array_length(entry_data);
                    ctx.set_field(ze, 1, Value::Int(size as i32));
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
        if let Value::Object(Some(underlying)) = ctx.get_field(this, 0) {
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
            // Read entry name from ZipEntry (field 0)
            let entry_name = match args.get(1) {
                Some(Value::Object(Some(ze))) => ctx.get_field(*ze, 0),
                _ => Value::Object(None),
            };
            ctx.set_field(this, 3, entry_name);
            Ok(None)
        },
    );
    // Internal accumulator for current entry data.
    r.register(zo, "write", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
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
            // Accumulate in a Rust-side buffer via a temporary byte array
            // Read existing accumulated bytes for this entry, append new ones
            zo_append_entry_data(ctx, this, &bytes);
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
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u8;
        zo_append_entry_data(ctx, this, &[b]);
        Ok(None)
    });
    r.register(zo, "write", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(Value::Object(Some(src))) = args.get(1) {
            let len = ctx.array_length(*src);
            let mut bytes = Vec::with_capacity(len);
            for i in 0..len {
                if let Value::Int(b) = ctx.get_array_element(*src, i) {
                    bytes.push(b as u8);
                }
            }
            zo_append_entry_data(ctx, this, &bytes);
        }
        Ok(None)
    });
    r.register(zo, "closeEntry", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        zo_finalize_current_entry(ctx, this);
        Ok(None)
    });
    r.register(zo, "finish", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Finalize any open entry
        zo_finalize_current_entry(ctx, this);
        // Build zip and write to underlying stream
        zo_write_zip(ctx, this)?;
        Ok(None)
    });
    r.register(zo, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        zo_finalize_current_entry(ctx, this);
        zo_write_zip(ctx, this)?;
        if let Value::Object(Some(underlying)) = ctx.get_field(this, 0) {
            let _ = ctx.invoke_virtual(underlying, "close", "()V", &[]);
        }
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
        "read",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(-1))),
    );
    r.register(
        "java/util/zip/InflaterInputStream",
        "read",
        "([BII)I",
        |_ctx, _args| Ok(Some(Value::Int(-1))),
    );
    r.register(
        "java/util/zip/InflaterInputStream",
        "available",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(0))),
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
            eprintln!("[IIS_CLOSE_DBG] native InflaterInputStream.close invoked");
            let this = obj_arg(args, 0)?;
            if matches!(ctx.get_field_by_name(this, "closed"), Value::Int(1)) {
                return Ok(None);
            }
            if matches!(
                ctx.get_field_by_name(this, "usesDefaultInflater"),
                Value::Int(1)
            ) {
                if let Value::Object(Some(inf)) = ctx.get_field_by_name(this, "inf") {
                    let _ = ctx.invoke_virtual(inf, "end", "()V", &[]);
                }
            }
            if let Value::Object(Some(underlying)) = ctx.get_field_by_name(this, "in") {
                let _ = ctx.invoke_virtual(underlying, "close", "()V", &[]);
            }
            ctx.set_field_by_name(this, "closed", Value::Int(1));
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

pub(crate) fn zo_write_zip(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<(), MethodCallFailed> {
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

    // Write zip bytes to underlying OutputStream
    let zip_bytes = zip_buf.into_inner();
    if let Value::Object(Some(underlying)) = ctx.get_field(this, 0) {
        for &b in &zip_bytes {
            let _ = ctx.invoke_virtual(underlying, "write", "(I)V", &[Value::Int(b as i32)]);
        }
    }

    // Reset count to prevent double-write
    ctx.set_field(this, 4, Value::Int(0));
    Ok(())
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

#[cfg(unix)]
pub(crate) fn p58_zlib_deflate(data: &[u8]) -> Option<Vec<u8>> {
    use std::ffi::c_void;
    use std::os::raw::{c_char, c_int, c_ulong};

    type CompressBound = unsafe extern "C" fn(c_ulong) -> c_ulong;
    type Compress2 =
        unsafe extern "C" fn(*mut u8, *mut c_ulong, *const u8, c_ulong, c_int) -> c_int;

    unsafe fn sym<T>(handle: *mut c_void, name: &'static [u8]) -> Option<T> {
        let ptr = libc::dlsym(handle, name.as_ptr() as *const c_char);
        if ptr.is_null() {
            None
        } else {
            Some(std::mem::transmute_copy(&ptr))
        }
    }

    let mut handle = std::ptr::null_mut();
    for name in [b"libz.so.1\0".as_slice(), b"libz.so\0".as_slice()] {
        handle = unsafe { libc::dlopen(name.as_ptr() as *const c_char, libc::RTLD_LAZY) };
        if !handle.is_null() {
            break;
        }
    }
    if handle.is_null() {
        return None;
    }

    let compress_bound: CompressBound = unsafe { sym(handle, b"compressBound\0")? };
    let compress2: Compress2 = unsafe { sym(handle, b"compress2\0")? };

    let source_len = data.len() as c_ulong;
    let mut bound = unsafe { compress_bound(source_len) } as usize;
    if bound == 0 {
        bound = data.len().saturating_add(64);
    }
    let mut z = vec![0u8; bound];
    let mut z_len = bound as c_ulong;
    let rc = unsafe { compress2(z.as_mut_ptr(), &mut z_len, data.as_ptr(), source_len, 6) };
    if rc != 0 || z_len < 6 {
        return None;
    }
    z.truncate(z_len as usize);
    // zlib wrapper = 2-byte header + raw deflate + 4-byte Adler-32 trailer.
    Some(z[2..z.len() - 4].to_vec())
}

#[cfg(not(unix))]
pub(crate) fn p58_zlib_deflate(_data: &[u8]) -> Option<Vec<u8>> {
    None
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
    r.register(
        zf,
        "getEntry",
        "(Ljava/lang/String;)Ljava/util/zip/ZipEntry;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(zf, "entries", "()Ljava/util/Enumeration;", |ctx, _args| {
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
        let itr = alloc_concurrent_synthetic(ctx, "java/util/zip/ZipFile$Itr", 2);
        ctx.set_field(itr, 0, Value::Object(Some(arr)));
        ctx.set_field(itr, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(itr))))
    });
    r.register(zf, "size", "()I", |_ctx, _args| Ok(Some(Value::Int(0))));
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
    r.register(cis, "read", "()I", |_ctx, _args| Ok(Some(Value::Int(-1))));
    r.register(cis, "read", "([BII)I", |_ctx, _args| {
        Ok(Some(Value::Int(-1)))
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
