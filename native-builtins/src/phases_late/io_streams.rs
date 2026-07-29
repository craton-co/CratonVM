// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.io` stream natives: Pushback streams/readers and Object{Input,Output}Stream stubs.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

// =============================================================================
// PushbackInputStream = 3-field (in=0, buf=1 byte[], pos=2)
// PushbackReader = 3-field (in=0, buf=1 char[], pos=2)
// =============================================================================

pub(crate) fn register_p58_pushback(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let pi = "java/io/PushbackInputStream";
    r.register(
        pi,
        "<init>",
        "(Ljava/io/InputStream;)V",
        p58_pushback_in_init,
    );
    r.register(
        pi,
        "<init>",
        "(Ljava/io/InputStream;I)V",
        p58_pushback_in_init_size,
    );
    r.register(pi, "read", "()I", p58_pushback_in_read);
    r.register(pi, "unread", "(I)V", p58_pushback_in_unread);
    r.register(pi, "available", "()I", p58_pushback_in_available);
    r.register(pi, "close", "()V", |ctx, args| {
        // Close by delegating to the underlying stream (field 0)
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(underlying)) = ctx.get_field(this, 0) {
            let _ = ctx.invoke_virtual(underlying, "close", "()V", &[]);
        }
        Ok(None)
    });

    let pr = "java/io/PushbackReader";
    r.register(
        pr,
        "<init>",
        "(Ljava/io/Reader;)V",
        p58_pushback_reader_init,
    );
    r.register(
        pr,
        "<init>",
        "(Ljava/io/Reader;I)V",
        p58_pushback_reader_init_size,
    );
    r.register(pr, "read", "()I", p58_pushback_reader_read);
    r.register(pr, "unread", "(I)V", p58_pushback_reader_unread);
    r.register(pr, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(underlying)) = ctx.get_field(this, 0) {
            let _ = ctx.invoke_virtual(underlying, "close", "()V", &[]);
        }
        Ok(None)
    });
    r.set_category(__prev_cat);
}

pub(crate) fn p58_pushback_in_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
    // Pin across the buffer alloc below — a moving young GC there would
    // relocate `this` (native stale-local family).
    let this_pin = ctx.pin_native_root(this);
    let buf = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 1);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, 1, Value::Object(Some(buf)));
    ctx.set_field(this, 2, Value::Int(1)); // pos = buf.length means buffer empty
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn p58_pushback_in_init_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
    let size = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 1,
    };
    // Pin across the buffer alloc below — a moving young GC there would
    // relocate `this` (native stale-local family).
    let this_pin = ctx.pin_native_root(this);
    let buf = ctx.new_array(cratonvm_types::ArrayElementType::Byte, size);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, 1, Value::Object(Some(buf)));
    ctx.set_field(this, 2, Value::Int(size as i32)); // pos = size means buffer empty
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn p58_pushback_in_read(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let pos = match ctx.get_field(this, 2) {
        Value::Int(v) => v,
        _ => 0,
    };
    if let Value::Object(Some(buf)) = ctx.get_field(this, 1) {
        let buf_len = ctx.array_length(buf) as i32;
        if pos < buf_len {
            // Read from pushback buffer
            let b = ctx.get_array_element(buf, pos as usize);
            ctx.set_field(this, 2, Value::Int(pos + 1));
            return Ok(Some(b));
        }
    }
    // Read from underlying stream
    if let Value::Object(Some(is)) = ctx.get_field(this, 0) {
        let b = ctx.invoke_virtual(is, "read", "()I", &[])?;
        Ok(b.or(Some(Value::Int(-1))))
    } else {
        Ok(Some(Value::Int(-1)))
    }
}

pub(crate) fn p58_pushback_in_unread(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let byte_val = args.get(1).copied().unwrap_or(Value::Int(0));
    let pos = match ctx.get_field(this, 2) {
        Value::Int(v) => v,
        _ => 0,
    };
    if pos > 0 {
        let new_pos = pos - 1;
        if let Value::Object(Some(buf)) = ctx.get_field(this, 1) {
            ctx.set_array_element(buf, new_pos as usize, byte_val);
        }
        ctx.set_field(this, 2, Value::Int(new_pos));
    }
    Ok(None)
}

pub(crate) fn p58_pushback_in_available(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let pos = match ctx.get_field(this, 2) {
        Value::Int(v) => v,
        _ => 0,
    };
    let buf_avail = if let Value::Object(Some(buf)) = ctx.get_field(this, 1) {
        ctx.array_length(buf) as i32 - pos
    } else {
        0
    };
    let stream_avail = if let Value::Object(Some(is)) = ctx.get_field(this, 0) {
        match ctx.invoke_virtual(is, "available", "()I", &[])? {
            Some(Value::Int(v)) => v,
            _ => 0,
        }
    } else {
        0
    };
    Ok(Some(Value::Int(buf_avail + stream_avail)))
}

pub(crate) fn p58_pushback_reader_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
    // Pin across the buffer alloc below — a moving young GC there would
    // relocate `this` (native stale-local family).
    let this_pin = ctx.pin_native_root(this);
    let buf = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, 1, Value::Object(Some(buf)));
    ctx.set_field(this, 2, Value::Int(1));
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn p58_pushback_reader_init_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
    let size = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => 1,
    };
    // Pin across the buffer alloc below — a moving young GC there would
    // relocate `this` (native stale-local family).
    let this_pin = ctx.pin_native_root(this);
    let buf = ctx.new_array(cratonvm_types::ArrayElementType::Reference, size);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, 1, Value::Object(Some(buf)));
    ctx.set_field(this, 2, Value::Int(size as i32));
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn p58_pushback_reader_read(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let pos = match ctx.get_field(this, 2) {
        Value::Int(v) => v,
        _ => 0,
    };
    if let Value::Object(Some(buf)) = ctx.get_field(this, 1) {
        let buf_len = ctx.array_length(buf) as i32;
        if pos < buf_len {
            let ch = ctx.get_array_element(buf, pos as usize);
            ctx.set_field(this, 2, Value::Int(pos + 1));
            return Ok(Some(ch));
        }
    }
    if let Value::Object(Some(reader)) = ctx.get_field(this, 0) {
        let ch = ctx.invoke_virtual(reader, "read", "()I", &[])?;
        Ok(ch.or(Some(Value::Int(-1))))
    } else {
        Ok(Some(Value::Int(-1)))
    }
}

pub(crate) fn p58_pushback_reader_unread(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let ch_val = args.get(1).copied().unwrap_or(Value::Int(0));
    let pos = match ctx.get_field(this, 2) {
        Value::Int(v) => v,
        _ => 0,
    };
    if pos > 0 {
        let new_pos = pos - 1;
        if let Value::Object(Some(buf)) = ctx.get_field(this, 1) {
            ctx.set_array_element(buf, new_pos as usize, ch_val);
        }
        ctx.set_field(this, 2, Value::Int(new_pos));
    }
    Ok(None)
}

// =============================================================================
// PushbackReader = 3-field (reader=0, buf=1 char[], pos=2)
// =============================================================================

pub(crate) fn register_p66_pushback_reader(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let pr = "java/io/PushbackReader";
    r.register(pr, "<init>", "(Ljava/io/Reader;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
        let buf = ctx.new_array(cratonvm_types::ArrayElementType::Char, 1);
        ctx.set_field(this, 1, Value::Object(Some(buf)));
        ctx.set_field(this, 2, Value::Int(1)); // pos = buf.length means no pushback data
        Ok(None)
    });
    r.register(pr, "<init>", "(Ljava/io/Reader;I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
        let size = match args.get(2) {
            Some(Value::Int(v)) => (*v).max(1) as usize,
            _ => 1,
        };
        let buf = ctx.new_array(cratonvm_types::ArrayElementType::Char, size);
        ctx.set_field(this, 1, Value::Object(Some(buf)));
        ctx.set_field(this, 2, Value::Int(size as i32));
        Ok(None)
    });
    r.register(pr, "read", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = match ctx.get_field(this, 2) {
            Value::Int(v) => v as usize,
            _ => 0,
        };
        let buf = match ctx.get_field(this, 1) {
            Value::Object(Some(b)) => b,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let buf_len = ctx.array_length(buf);
        if pos < buf_len {
            // Read from pushback buffer
            let ch = ctx.get_array_element(buf, pos);
            ctx.set_field(this, 2, Value::Int((pos + 1) as i32));
            return Ok(Some(ch));
        }
        // Otherwise return -1 (simplified — real impl would delegate to inner reader)
        Ok(Some(Value::Int(-1)))
    });
    r.register(pr, "unread", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ch = args.get(1).copied().unwrap_or(Value::Int(0));
        let pos = match ctx.get_field(this, 2) {
            Value::Int(v) => v as usize,
            _ => 0,
        };
        if pos == 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "Pushback buffer overflow".into(),
            }
            .into());
        }
        let buf = match ctx.get_field(this, 1) {
            Value::Object(Some(b)) => b,
            _ => return Ok(None),
        };
        ctx.set_array_element(buf, pos - 1, ch);
        ctx.set_field(this, 2, Value::Int((pos - 1) as i32));
        Ok(None)
    });
    r.register(pr, "ready", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = match ctx.get_field(this, 2) {
            Value::Int(v) => v as usize,
            _ => 0,
        };
        let buf = match ctx.get_field(this, 1) {
            Value::Object(Some(b)) => b,
            _ => return Ok(Some(Value::Int(0))),
        };
        let buf_len = ctx.array_length(buf);
        Ok(Some(Value::Int(if pos < buf_len { 1 } else { 0 })))
    });
    r.register(pr, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Delegate close() to the underlying Reader (field 0) if present, then clear our buffer.
        if let Value::Object(Some(inner)) = ctx.get_field(this, 0) {
            let _ = ctx.invoke_virtual(inner, "close", "()V", &[]);
        }
        ctx.set_field(this, 0, Value::Object(None));
        ctx.set_field(this, 1, Value::Object(None));
        Ok(None)
    });
    r.set_category(__prev_cat);
}

/// Read a single byte from an InputStream. Returns -1 on EOF.
pub(crate) fn ois_read_byte(ctx: &mut dyn NativeContext, stream: ObjectRef) -> i32 {
    match ctx.invoke_virtual(stream, "read", "()I", &[]) {
        Ok(Some(Value::Int(b))) => b,
        _ => -1,
    }
}

/// Read exactly n bytes from an InputStream. Returns empty vec on EOF.
pub(crate) fn ois_read_n(ctx: &mut dyn NativeContext, stream: ObjectRef, n: usize) -> Vec<u8> {
    // Pin across the read callbacks below — a moving young GC there would
    // relocate the stream (native stale-local family).
    let stream_pin = ctx.pin_native_root(stream);
    let mut result = Vec::with_capacity(n);
    for _ in 0..n {
        let stream = ctx.read_native_pin(stream_pin, stream);
        let b = ois_read_byte(ctx, stream);
        if b < 0 {
            break;
        }
        result.push(b as u8);
    }
    ctx.unpin_native_roots(stream_pin);
    result
}

#[cfg(not(feature = "experimental-serialization"))]
pub(crate) fn register_p70_object_streams(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // ObjectOutputStream — field 0 = underlying OutputStream
    let oos = "java/io/ObjectOutputStream";
    r.register(oos, "<init>", "(Ljava/io/OutputStream;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
        // Write Java serialization magic + version header
        if let Value::Object(Some(stream)) = args.get(1).copied().unwrap_or(Value::Object(None)) {
            // Magic: 0xACED, Version: 0x0005
            oos_write_bytes(ctx, stream, &[0xAC, 0xED, 0x00, 0x05]);
        }
        Ok(None)
    });
    r.register(oos, "writeObject", "(Ljava/lang/Object;)V", |ctx, args| {
        // Simplified: write TC_NULL (0x70) for null, or a marker for non-null
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            match args.get(1) {
                Some(Value::Object(None)) | None => {
                    oos_write_bytes(ctx, stream, &[0x70]); // TC_NULL
                }
                Some(Value::Object(Some(obj))) => {
                    // TC_STRING (0x74) for String objects, TC_OBJECT (0x73) for others
                    if let Some(s) = ctx.read_string(*obj) {
                        let bytes = s.as_bytes();
                        oos_write_bytes(ctx, stream, &[0x74]); // TC_STRING
                        oos_write_bytes(ctx, stream, &(bytes.len() as u16).to_be_bytes());
                        oos_write_bytes(ctx, stream, bytes);
                    } else {
                        oos_write_bytes(ctx, stream, &[0x73]); // TC_OBJECT (simplified)
                    }
                }
                _ => {}
            }
        }
        Ok(None)
    });
    r.register(oos, "writeInt", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, &val.to_be_bytes());
        }
        Ok(None)
    });
    r.register(oos, "writeLong", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, &val.to_be_bytes());
        }
        Ok(None)
    });
    r.register(oos, "writeUTF", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = match args.get(1) {
            Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
            _ => String::new(),
        };
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let bytes = s.as_bytes();
            oos_write_bytes(ctx, stream, &(bytes.len() as u16).to_be_bytes());
            oos_write_bytes(ctx, stream, bytes);
        }
        Ok(None)
    });
    r.register(oos, "writeBoolean", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, &[if val != 0 { 1 } else { 0 }]);
        }
        Ok(None)
    });
    r.register(oos, "writeDouble", "(D)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = match args.get(1) {
            Some(Value::Double(v)) => *v,
            _ => 0.0,
        };
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, &val.to_be_bytes());
        }
        Ok(None)
    });
    r.register(oos, "writeFloat", "(F)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = match args.get(1) {
            Some(Value::Float(v)) => *v,
            _ => 0.0,
        };
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, &val.to_be_bytes());
        }
        Ok(None)
    });
    r.register(oos, "writeByte", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u8;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, &[val]);
        }
        Ok(None)
    });
    r.register(oos, "writeChar", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u16;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, &val.to_be_bytes());
        }
        Ok(None)
    });
    r.register(oos, "writeShort", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as i16;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, &val.to_be_bytes());
        }
        Ok(None)
    });
    r.register(oos, "writeBytes", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = match args.get(1) {
            Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
            _ => String::new(),
        };
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, s.as_bytes());
        }
        Ok(None)
    });
    r.register(oos, "flush", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let _ = ctx.invoke_virtual(stream, "flush", "()V", &[]);
        }
        Ok(None)
    });
    r.register(oos, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let _ = ctx.invoke_virtual(stream, "flush", "()V", &[]);
            let _ = ctx.invoke_virtual(stream, "close", "()V", &[]);
        }
        Ok(None)
    });
    r.register(oos, "defaultWriteObject", "()V", |_ctx, _args| {
        // `defaultWriteObject()` writes the calling class's default-serializable
        // fields. This fallback OOS has no class-descriptor machinery to walk
        // them with, and it never enters a per-class `writeObject` callback, so
        // a no-op here silently DROPPED every field of every class that used the
        // normal `writeObject`/`defaultWriteObject` idiom. `NotActiveException`
        // is not a substitute for the real thing — it is what the real JDK
        // throws in exactly this state ("not in call to writeObject"), so the
        // caller learns the truth instead of shipping an empty object.
        Err(RuntimeError::IOException {
            message: "java.io.NotActiveException: not in call to writeObject".into(),
        }
        .into())
    });
    r.register(oos, "reset", "()V", |ctx, args| {
        // `reset()` is a WIRE-FORMAT operation, not just a bookkeeping one: the
        // JDK emits TC_RESET so the paired reader drops its back-reference
        // table at the same point in the byte stream. Skipping the byte left the
        // two sides disagreeing about stream position. (The matching TC_RESET
        // skip is in `ObjectInputStream.readObject` below.)
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            oos_write_bytes(ctx, stream, &[0x79]); // TC_RESET
        }
        Ok(None)
    });

    // ObjectInputStream — field 0 = underlying InputStream
    let ois = "java/io/ObjectInputStream";
    r.register(ois, "<init>", "(Ljava/io/InputStream;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
        // Read and validate magic + version header
        if let Value::Object(Some(stream)) = args.get(1).copied().unwrap_or(Value::Object(None)) {
            let header = ois_read_n(ctx, stream, 4);
            if header.len() == 4 && (header[0] != 0xAC || header[1] != 0xED) {
                // Not a valid serialization stream — but don't error, just proceed
            }
        }
        Ok(None)
    });
    r.register(ois, "readObject", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            // TC_RESET carries no payload — skip it and read the next item, or
            // `ObjectOutputStream.reset()`'s marker would be mis-read as an
            // unknown type code and answered with a bogus null.
            let mut tc = ois_read_byte(ctx, stream);
            while tc == 0x79 {
                tc = ois_read_byte(ctx, stream);
            }
            match tc {
                0x70 => Ok(Some(Value::Object(None))), // TC_NULL
                0x74 => {
                    // TC_STRING: read 2-byte length + UTF-8 bytes
                    let len_bytes = ois_read_n(ctx, stream, 2);
                    if len_bytes.len() == 2 {
                        let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
                        let str_bytes = ois_read_n(ctx, stream, len);
                        let s = String::from_utf8_lossy(&str_bytes).to_string();
                        let obj = ctx.create_string(&s);
                        Ok(Some(Value::Object(Some(obj))))
                    } else {
                        Ok(Some(Value::Object(None)))
                    }
                }
                _ => Ok(Some(Value::Object(None))),
            }
        } else {
            Ok(Some(Value::Object(None)))
        }
    });
    r.register(ois, "readInt", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let bytes = ois_read_n(ctx, stream, 4);
            if bytes.len() == 4 {
                Ok(Some(Value::Int(i32::from_be_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3],
                ]))))
            } else {
                Ok(Some(Value::Int(0)))
            }
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(ois, "readLong", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let bytes = ois_read_n(ctx, stream, 8);
            if bytes.len() == 8 {
                Ok(Some(Value::Long(i64::from_be_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ]))))
            } else {
                Ok(Some(Value::Long(0)))
            }
        } else {
            Ok(Some(Value::Long(0)))
        }
    });
    r.register(ois, "readUTF", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let len_bytes = ois_read_n(ctx, stream, 2);
            if len_bytes.len() == 2 {
                let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
                let str_bytes = ois_read_n(ctx, stream, len);
                let s = String::from_utf8_lossy(&str_bytes).to_string();
                let obj = ctx.create_string(&s);
                Ok(Some(Value::Object(Some(obj))))
            } else {
                let obj = ctx.create_string("");
                Ok(Some(Value::Object(Some(obj))))
            }
        } else {
            let obj = ctx.create_string("");
            Ok(Some(Value::Object(Some(obj))))
        }
    });
    r.register(ois, "readBoolean", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let b = ois_read_byte(ctx, stream);
            Ok(Some(Value::Int(if b != 0 { 1 } else { 0 })))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(ois, "readDouble", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let bytes = ois_read_n(ctx, stream, 8);
            if bytes.len() == 8 {
                Ok(Some(Value::Double(f64::from_be_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ]))))
            } else {
                Ok(Some(Value::Double(0.0)))
            }
        } else {
            Ok(Some(Value::Double(0.0)))
        }
    });
    r.register(ois, "readFloat", "()F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let bytes = ois_read_n(ctx, stream, 4);
            if bytes.len() == 4 {
                Ok(Some(Value::Float(f32::from_be_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3],
                ]))))
            } else {
                Ok(Some(Value::Float(0.0)))
            }
        } else {
            Ok(Some(Value::Float(0.0)))
        }
    });
    r.register(ois, "readByte", "()B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let b = ois_read_byte(ctx, stream);
            Ok(Some(Value::Int(b as i8 as i32)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(ois, "readChar", "()C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let bytes = ois_read_n(ctx, stream, 2);
            if bytes.len() == 2 {
                Ok(Some(Value::Int(
                    u16::from_be_bytes([bytes[0], bytes[1]]) as i32
                )))
            } else {
                Ok(Some(Value::Int(0)))
            }
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(ois, "readShort", "()S", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let bytes = ois_read_n(ctx, stream, 2);
            if bytes.len() == 2 {
                Ok(Some(Value::Int(
                    i16::from_be_bytes([bytes[0], bytes[1]]) as i32
                )))
            } else {
                Ok(Some(Value::Int(0)))
            }
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(ois, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let _ = ctx.invoke_virtual(stream, "close", "()V", &[]);
        }
        Ok(None)
    });
    r.register(ois, "defaultReadObject", "()V", |_ctx, _args| {
        // Mirror of `ObjectOutputStream.defaultWriteObject` above: a no-op here
        // left every default-serializable field at its default value while the
        // caller believed it had been restored. This fallback OIS has no class
        // descriptor to read them from and is never inside a per-class
        // `readObject` callback, which is precisely the state in which the real
        // JDK throws `NotActiveException`.
        Err(RuntimeError::IOException {
            message: "java.io.NotActiveException: not in call to readObject".into(),
        }
        .into())
    });
    // Delegate to the underlying stream, as `close()` above does; a constant 0
    // told every caller the stream was exhausted while bytes were still queued.
    r.register(ois, "available", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            match ctx.invoke_virtual(stream, "available", "()I", &[])? {
                Some(Value::Int(v)) => Ok(Some(Value::Int(v))),
                _ => Ok(Some(Value::Int(0))),
            }
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.set_category(__prev_cat);
}
