#[allow(unused_imports)]
use super::*;

/// Perform a real HTTP/1.1 request for HttpURLConnection.
/// Connects via fd_table TCP, sends request, reads and parses the full response.
pub fn p54_huc_do_request_body(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<(), MethodCallFailed> {
    // Mark connected
    ctx.set_field(this, 9, Value::Int(1));

    // Extract URL fields (URL = 5-field: protocol=0, host=1, port=2, path=3, query=4)
    let url_obj = match ctx.get_field(this, 0) {
        Value::Object(Some(u)) => u,
        _ => {
            return Err(RuntimeError::IOException {
                message: "No URL set".into(),
            }
            .into())
        }
    };
    let protocol = match ctx.get_field(url_obj, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "http".into()),
        _ => "http".into(),
    };
    let host = match ctx.get_field(url_obj, 1) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "localhost".into()),
        _ => "localhost".into(),
    };
    let port = ctx.get_field(url_obj, 2).as_int().unwrap_or(-1);
    let effective_port = if port > 0 {
        port
    } else if protocol == "https" {
        443
    } else {
        80
    };
    let path = match ctx.get_field(url_obj, 3) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "/".into()),
        _ => "/".into(),
    };
    let query = match ctx.get_field(url_obj, 4) {
        Value::Object(Some(s)) => {
            let q = ctx.read_string(s).unwrap_or_default();
            if q.is_empty() {
                String::new()            } else {
                format!("?{}", q)            }        }
        _ => String::new(),
    };

    let method = match ctx.get_field(this, 1) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "GET".into()),
        _ => "GET".into(),
    };

    // Collect request headers
    let mut extra_headers = Vec::new();
    if let Value::Object(Some(hdr_arr)) = ctx.get_field(this, 4) {
        let len = ctx.array_length(hdr_arr);
        for i in 0..len {
            if let Value::Object(Some(s)) = ctx.get_array_element(hdr_arr, i) {
                if let Some(h) = ctx.read_string(s) {
                    extra_headers.push(h);
                }
            }
        }
    }

    // Connect via fd_table
    let addr_str = format!("{}:{}", host, effective_port);
    let fd_id = match ctx.fd_table().open_tcp_connect(&addr_str) {
        Ok(fd) => fd,
        Err(e) => {
            return Err(RuntimeError::IOException {
                message: format!("Connection failed: {}", e),
            }
            .into())
        }
    };
    ctx.set_field(this, 3, Value::Int(fd_id as i32));

    // Honour `setReadTimeout(ms)`. Without it the response loop below blocks
    // until the peer closes, which is what made the setter look harmless.
    // 0 (the JDK default, and an unconfigured slot) still means "no timeout".
    // The connect timeout can only be recorded for now — `FileDescriptorTable`
    // has no connect-with-timeout entry point, so `open_tcp_connect` above
    // still uses the OS default. FOLLOW-UP: add one and pass it through here.
    let read_timeout_ms = p54_huc_get_slot(ctx, this, P54_HUC_READ_TIMEOUT)
        .as_int()
        .unwrap_or(0);
    if read_timeout_ms > 0 {
        let _ = ctx.fd_table().tcp_set_read_timeout(
            fd_id,
            Some(std::time::Duration::from_millis(read_timeout_ms as u64)),
        );
    }

    // Build HTTP/1.1 request
    let request_line = format!("{} {}{} HTTP/1.1\r\n", method, path, query);
    let mut request = request_line;
    request.push_str(&format!("Host: {}\r\n", host));

    // Add user headers (skip Host if user already set it)
    let mut has_connection = false;
    for h in &extra_headers {
        request.push_str(h);
        request.push_str("\r\n");
        if h.to_lowercase().starts_with("connection:") {
            has_connection = true;
        }
    }
    if !has_connection {
        request.push_str("Connection: close\r\n");
    }
    request.push_str("\r\n");

    // Write request
    let req_bytes = request.as_bytes();
    let mut written = 0;
    while written < req_bytes.len() {
        match ctx.fd_table().tcp_write(fd_id, &req_bytes[written..]) {
            Ok(n) => written += n,
            Err(e) => {
                let _ = ctx.fd_table().close(fd_id);
                // JIT r23 w4 review: forget the id with the socket -- under
                // `CRATONVM_FD_TABLE_OS_KEYS` a later `disconnect()` would
                // otherwise close whatever reused the OS value.
                ctx.set_field(this, 3, Value::Int(-1));
                return Err(RuntimeError::IOException {
                    message: format!("Write failed: {}", e),
                }
                .into());
            }
        }
    }

    // Read full response
    let mut response_buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 4096];
    loop {
        match ctx.fd_table().tcp_read(fd_id, &mut chunk) {
            Ok(0) => break,
            Ok(n) => response_buf.extend_from_slice(&chunk[..n]),
            // An expired read timeout surfaces as WouldBlock (POSIX) or
            // TimedOut (Windows). Neither can occur on the blocking socket we
            // get when no timeout was configured, so the historical
            // "treat as end of response" behaviour is preserved for that case
            // and only an explicitly configured timeout throws — silently
            // truncating the body would be worse than the original no-op.
            Err(ref e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if read_timeout_ms > 0 {
                    let _ = ctx.fd_table().close(fd_id);
                    ctx.set_field(this, 3, Value::Int(-1));
                    return Err(RuntimeError::SocketTimeoutException {
                        message: "Read timed out".to_string(),
                    }
                    .into());
                }
                break;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
            Err(e) => {
                let _ = ctx.fd_table().close(fd_id);
                // JIT r23 w4 review: see the write-failure arm above.
                ctx.set_field(this, 3, Value::Int(-1));
                return Err(RuntimeError::IOException {
                    message: format!("Read failed: {}", e),
                }
                .into());
            }
        }
        // Safety limit: 64 MB
        if response_buf.len() > 64 * 1024 * 1024 {
            break;
        }
    }

    // Close the connection (we sent Connection: close)
    let _ = ctx.fd_table().close(fd_id);
    ctx.set_field(this, 3, Value::Int(-1));

    // Parse response: find header/body boundary
    let resp_str = String::from_utf8_lossy(&response_buf);
    let header_end = resp_str.find("\r\n\r\n").unwrap_or(resp_str.len());
    let header_section = &resp_str[..header_end];
    let body_start = if header_end + 4 <= response_buf.len() {
        header_end + 4
    } else {
        response_buf.len()
    };

    // Parse status line: "HTTP/1.1 200 OK"
    let mut lines = header_section.lines();
    let status_code = if let Some(status_line) = lines.next() {
        let parts: Vec<&str> = status_line.splitn(3, ' ').collect();
        if parts.len() >= 2 {
            parts[1].parse::<i32>().unwrap_or(0)
        } else {
            0
        }
    } else {
        0
    };
    ctx.set_field(this, 2, Value::Int(status_code));

    // Parse response headers
    let resp_headers: Vec<String> = lines.map(|l| l.to_string()).collect();

    // Check for chunked transfer-encoding and handle body
    let is_chunked = resp_headers.iter().any(|h| {
        h.to_lowercase().starts_with("transfer-encoding:") && h.to_lowercase().contains("chunked")
    });

    let body_bytes = if is_chunked {
        p54_decode_chunked(&response_buf[body_start..])
    } else {
        response_buf[body_start..].to_vec()
    };

    // gc-common w20-b: `this` (the caller's pin does not update this local)
    // and the header array were held raw across the header strings' and the
    // body array's allocations. `this` is pinned here and re-read before
    // each store; the header array is built rooted.
    let this_pin = ctx.pin_native_root(this);
    let hdr_arr = crate::util_concurrent_ext::build_rooted_ref_array(
        ctx,
        cratonvm_types::ClassId::new(0),
        resp_headers.len(),
        |ctx, i| Ok(ctx.create_string(&resp_headers[i])),
    );
    let this = ctx.read_native_pin(this_pin, this);
    let hdr_arr: ObjectRef = match hdr_arr {
        Ok(arr) => arr,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    };
    ctx.set_field(this, 5, Value::Object(Some(hdr_arr)));

    // Store body as byte array (one bulk copy; it can be tens of megabytes)
    let body_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, body_bytes.len());
    ctx.write_byte_array_from(body_arr, 0, &body_bytes);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field(this, 6, Value::Object(Some(body_arr)));

    Ok(())
}

/// Decode chunked transfer-encoding body.
pub fn p54_decode_chunked(data: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    let mut pos = 0;
    loop {
        // Find end of chunk size line
        let line_end = match data[pos..].windows(2).position(|w| w == b"\r\n") {
            Some(p) => pos + p,
            None => break,
        };
        let size_str = std::str::from_utf8(&data[pos..line_end])
            .unwrap_or("0")
            .trim();
        let chunk_size = usize::from_str_radix(size_str, 16).unwrap_or(0);
        if chunk_size == 0 {
            break;
        }
        let chunk_start = line_end + 2;
        let chunk_end = chunk_start + chunk_size;
        if chunk_end > data.len() {
            // Partial chunk — take what we have
            result.extend_from_slice(&data[chunk_start..]);
            break;
        }
        result.extend_from_slice(&data[chunk_start..chunk_end]);
        // Skip trailing \r\n after chunk data
        pos = if chunk_end + 2 <= data.len() {
            chunk_end + 2
        } else {
            data.len()
        };
    }
    result
}

// ---------------------------------------------------------------------------
// java.util.zip stubs (ZipEntry, ZipInputStream, ZipOutputStream basics)
// ---------------------------------------------------------------------------
pub fn register_phase54_zip_stubs(r: &mut NativeMethodRegistry) {
            // --- ZipEntry (4-field: name=0, size=1, compressedSize=2, method=3) ---
    let ze = "java/util/zip/ZipEntry";
    r.register(ze, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args[1]);
        ctx.set_field(this, 1, Value::Long(-1));
        ctx.set_field(this, 2, Value::Long(-1));
        ctx.set_field(this, 3, Value::Int(-1));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ze, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ze, "getSize", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ze, "setSize", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, args[1]);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ze, "getCompressedSize", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ze, "getMethod", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 3)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ze, "setMethod", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 3, args[1]);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ze, "isDirectory", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(n)) = ctx.get_field(this, 0) {
            let name = ctx.read_string(n).unwrap_or_default();
            Ok(Some(Value::Int(if name.ends_with('/') { 1 } else { 0 })))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ze, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // ZipEntry constants — KEEP. `static final int` FIELD reads; 0/8 are the
    // PKZIP compression-method numbers `java.util.zip.ZipEntry` declares.
    r.register(ze, "STORED", "I", |_ctx, _args| Ok(Some(Value::Int(0))), cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ze, "DEFLATED", "I", |_ctx, _args| Ok(Some(Value::Int(8))), cratonvm_native_api::NativeKind::Intrinsic);

    // --- java.util.zip.CRC32 (1-field: crc=0 as Long) ---
    let crc = "java/util/zip/CRC32";
    r.register(crc, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Long(0));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(crc, "update", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let crc_val = ctx.get_field(this, 0).as_long().unwrap_or(0) as u32;
        let b = args[1].as_int().unwrap_or(0) as u8;
        let new_crc = p54_crc32_update(crc_val, &[b]);
        ctx.set_field(this, 0, Value::Long(new_crc as i64));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(crc, "update", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let crc_val = ctx.get_field(this, 0).as_long().unwrap_or(0) as u32;
        let arr = obj_arg(args, 1)?;
        let len = ctx.array_length(arr);
        let mut bytes = Vec::with_capacity(len);
        for i in 0..len {
            bytes.push(ctx.get_array_element(arr, i).as_int().unwrap_or(0) as u8);
        }
        let new_crc = p54_crc32_update(crc_val, &bytes);
        ctx.set_field(this, 0, Value::Long(new_crc as i64));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(crc, "update", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let crc_val = ctx.get_field(this, 0).as_long().unwrap_or(0) as u32;
        let arr = obj_arg(args, 1)?;
        let off = args[2].as_int().unwrap_or(0) as usize;
        let len = args[3].as_int().unwrap_or(0) as usize;
        let mut bytes = Vec::with_capacity(len);
        for i in off..off + len {
            bytes.push(ctx.get_array_element(arr, i).as_int().unwrap_or(0) as u8);
        }
        let new_crc = p54_crc32_update(crc_val, &bytes);
        ctx.set_field(this, 0, Value::Long(new_crc as i64));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(crc, "getValue", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(crc, "reset", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Long(0));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
}

/// CRC-32 update (IEEE 802.3 polynomial)
pub fn p54_crc32_update(crc: u32, data: &[u8]) -> u32 {
    let mut c = !crc;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            if c & 1 != 0 {
                c = (c >> 1) ^ 0xEDB8_8320;
            } else {
                c >>= 1;
            }
        }
    }
    !c
}

// ===========================================================================
// T2.3.1 — `jdk.internal.util.ArraysSupport` intrinsics
// ===========================================================================
//
// OpenJDK's `java.util.Arrays`, `java.lang.String`, and the collection
// hashCode/equals fast paths all delegate to the two native intrinsics
// below:
//
//   int  vectorizedHashCode(Object array, int fromIndex, int length,
//                           int initialValue, int basicType);
//   int  vectorizedMismatch(Object a, long aOffset,
//                           Object b, long bOffset,
//                           int length, int log2ArrayIndexScale);
//
// In HotSpot these are SIMD intrinsics produced by C2. For a correctness-
// first interpreter the fast path is a scalar loop — the native only
// needs to match the *semantics* the callers expect, not the throughput
// of AVX-512.
//
// `vectorizedHashCode(arr, from, len, init, T_BYTE|T_CHAR|T_INT)` folds
// `len` elements of `arr` starting at `from` into a rolling hash using
// the JDK's canonical formula `31 * acc + element`, returning the final
// hash. `basicType` selects how each element is decoded; we handle the
// four types `String.hashCode`, `Arrays.hashCode(byte[])`,
// `Arrays.hashCode(char[])`, and `Arrays.hashCode(int[])` use in
// practice, matching the HotSpot T_* enum values.
//
// `vectorizedMismatch(a, aOff, b, bOff, len, log2Scale)` returns the
// byte-index of the first element that differs, or `-1` if all `len`
// elements are equal. `log2Scale` is 0 for byte, 1 for short/char, 2
// for int/float, 3 for long/double; the offsets are in BYTES from the
// object header per HotSpot ABI, but we already have the base array
// ref as args[0]/args[2] so we translate `off >> log2Scale` to an
// element index. Callers only pass offsets pointing at the start of
// the element array, so this translation is always exact.

// ---------------------------------------------------------------------------
// StringLatin1 — native overrides for methods whose bytecode triggers AIOOBE
// when the interpreter's loop / getChar interaction goes wrong.
// ---------------------------------------------------------------------------

/// nio21 wave 2, lane L5: where `StringLatin1.inflate([BI[CII)V`'s bytecode
/// first fails, for the given operand state. JDK 25's body is
///
/// ```text
/// for (int i = 0; i < len; i++) dst[dstOff++] = (char)(src[srcOff++] & 0xff);
/// ```
///
/// so iteration `i` loads `src[srcOff + i]` (`baload`: null, then range)
/// BEFORE it stores `dst[dstOff + i]` (`castore`: null, then range), and every
/// earlier iteration's char is already stored. A `len <= 0` touches neither
/// array, null or not. `None` for a null array. Returns how many chars are
/// copied, and the fault the copy then stops at, if any.
pub fn latin1_inflate_fault(
    src_len: Option<i32>,
    src_off: i32,
    dst_len: Option<i32>,
    dst_off: i32,
    len: i32,
) -> (i32, Option<Latin1InflateFault>) {
    if len <= 0 {
        return (0, None);
    }
    // The first iteration whose access fails (`len` when none does). An
    // in-range start reaches the array's end before `off + i` can wrap.
    let first_bad = |arr_len: Option<i32>, off: i32| -> i32 {
        match arr_len {
            None => 0,
            Some(n) if off < 0 || off >= n => 0,
            Some(n) => (n - off).min(len),
        }
    };
    let src_bad = first_bad(src_len, src_off);
    let dst_bad = first_bad(dst_len, dst_off);
    if src_bad >= len && dst_bad >= len {
        return (len, None);
    }
    // On a tie the `baload` of that iteration runs first.
    if src_bad <= dst_bad {
        let fault = match src_len {
            None => Latin1InflateFault::SrcNull,
            Some(n) => Latin1InflateFault::Range(src_off.wrapping_add(src_bad), n),
        };
        (src_bad, Some(fault))
    } else {
        let fault = match dst_len {
            None => Latin1InflateFault::DstNull,
            Some(n) => Latin1InflateFault::Range(dst_off.wrapping_add(dst_bad), n),
        };
        (dst_bad, Some(fault))
    }
}

/// The exception [`latin1_inflate_fault`] stops at: the interpreter's own
/// answer at that bytecode (JEP 358 text from the method's
/// `LocalVariableTable`, `src` / `dst`; `Index i out of bounds for length n`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Latin1InflateFault {
    SrcNull,
    DstNull,
    Range(i32, i32),
}

impl Latin1InflateFault {
    pub fn into_error(self) -> RuntimeError {
        match self {
            // nio21 wave 6 L1: HotSpot words `baload` as `byte/boolean` (it
            // cannot tell the two element types apart from the opcode);
            // measured on 25.0.3. This said `byte array` before.
            Self::SrcNull => RuntimeError::NullPointerException {
                message: Some(
                    "Cannot load from byte/boolean array because \"src\" is null".to_string(),
                ),
            },
            Self::DstNull => RuntimeError::NullPointerException {
                message: Some("Cannot store to char array because \"dst\" is null".to_string()),
            },
            Self::Range(index, length) => RuntimeError::aioobe(index, length),
        }
    }
}

pub fn native_string_latin1_inflate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let src = match args.first() {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let src_off = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let dst = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let dst_off = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if len <= 0 {
        return Ok(None);
    }
    // nio21 wave 2, lane L5: the bytecode's failure point and the partial
    // copy before it. This native used to check everything up front, throw
    // `NullPointerException` for a null array even with `len <= 0`, and word
    // its exceptions in its own text.
    // Cast: a Java array length fits an `i32`.
    let src_len = src.map(|a| ctx.array_length(a) as i32);
    let dst_len = dst.map(|a| ctx.array_length(a) as i32);
    let (copied, fault) = latin1_inflate_fault(src_len, src_off, dst_len, dst_off, len);
    // r21 (2026-10-13): a faulting call is the JDK body's, which copies what
    // precedes the fault and throws from its own frame.
    if fault.is_some() {
        return latin1_fault_from_body(ctx, "inflate", "([BI[CII)V", args);
    }

    // Chunked through stack buffers: this native is the whole cost of a short
    // `new String(bytes, ISO_8859_1)` / `Charset.decode` (the common Latin-1 to
    // UTF-16 widening), and two heap `Vec`s per call were a large part of it.
    if let (Some(src), Some(dst)) = (src, dst) {
        // `copied > 0` only when both offsets are in range, so both are >= 0.
        let count = usize::try_from(copied).unwrap_or(0);
        let src_at = usize::try_from(src_off).unwrap_or(0);
        let dst_at = usize::try_from(dst_off).unwrap_or(0);
        let mut bytes = [0u8; 256];
        let mut chars = [0u16; 256];
        let mut done = 0usize;
        while done < count {
            let n = (count - done).min(256);
            let read = ctx.read_byte_array_into(src, src_at + done, &mut bytes[..n]);
            if read != n {
                // Unreachable after the screen above; the bytecode's answer.
                // Cast: `done + read <= copied`, an `i32`.
                let index = src_off.wrapping_add((done + read) as i32);
                return Err(RuntimeError::aioobe(index, src_len.unwrap_or(0)).into());
            }
            for (c, b) in chars[..n].iter_mut().zip(&bytes[..n]) {
                *c = u16::from(*b);
            }
            if !ctx.write_char_array_from(dst, dst_at + done, &chars[..n]) {
                // Unreachable after the screen above.
                // Cast: `done < copied`, an `i32`.
                let index = dst_off.wrapping_add(done as i32);
                return Err(RuntimeError::aioobe(index, dst_len.unwrap_or(0)).into());
            }
            done += n;
        }
    }
    match fault {
        Some(fault) => Err(fault.into_error().into()),
        None => Ok(None),
    }
}


// ---------------------------------------------------------------------------
// nio21 wave 6 L1: the KC26 `StringLatin1` natives answer as the bytecode
// does on every input (`docs/feature-designs/native-dispatch-architecture.md`).
// Measured on HotSpot 25.0.3 through reflection: a null array is the JEP 358
// NPE named after the bytecode's local (`value` / `other`, and in
// `ArraysSupport.mismatch` `a` / `b`), a bad index `Index i out of bounds for
// length n`. The screen only decides that a call faults; the JDK body then
// throws it (`latin1_fault_from_body`), so the trace has HotSpot's frames.
// `CRATONVM_LATIN1_HOTSPOT_FAULTS=0` restores the old answers (`0` for a null
// array, `lim` clamped to the array lengths). The switch is read on fault
// paths only.
// ---------------------------------------------------------------------------



pub fn latin1_obj_arg(args: &[Value], i: usize) -> Option<ObjectRef> {
    match args.get(i) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

/// A faulting `baload`: on the null local named, or out of range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Latin1LoadFault {
    Null(&'static str),
    Range(i32, i32),
}

impl Latin1LoadFault {
    pub fn into_error(self) -> RuntimeError {
        match self {
            Self::Null(name) => RuntimeError::NullPointerException {
                message: Some(format!(
                    "Cannot load from byte/boolean array because \"{name}\" is null"
                )),
            },
            Self::Range(index, length) => RuntimeError::aioobe(index, length),
        }
    }
}

/// `baload` of `local[index]` over an array of length `len` (`None`: null).
pub fn latin1_element_fault(
    len: Option<i32>,
    index: i32,
    local: &'static str,
) -> Option<Latin1LoadFault> {
    match len {
        None => Some(Latin1LoadFault::Null(local)),
        Some(n) if index < 0 || index >= n => Some(Latin1LoadFault::Range(index, n)),
        Some(_) => None,
    }
}

/// `StringLatin1.compareTo(value, other, len1, len2)`'s bytecode over arrays
/// of lengths `a_len` / `b_len` (`None`: null): `lim = min(len1, len2)`, the
/// `ArraysSupport.mismatch(a, b, lim)` loop (`a[i]` loads before `b[i]`),
/// then `len1 - len2` or the two unsigned bytes' difference. `byte_at(first,
/// i)` reads `a[i]` (`first`) or `b[i]`, unsigned, and is only called for an
/// index the screen admitted. For `lim > 7` the JDK's `mismatch` first
/// compares `a[0]` / `b[0]` and then runs `vectorizedMismatch`, whose answer
/// on an in-bounds pair is the loop's; an out-of-bounds `lim` there reads past
/// the array on HotSpot (no check), and this answers the loop's AIOOBE.
pub fn latin1_compare_to(
    a_len: Option<i32>,
    b_len: Option<i32>,
    len1: i32,
    len2: i32,
    mut byte_at: impl FnMut(bool, i32) -> i32,
) -> Result<i32, Latin1LoadFault> {
    let lim = len1.min(len2);
    for i in 0..lim.max(0) {
        if let Some(fault) = latin1_element_fault(a_len, i, "a") {
            return Err(fault);
        }
        let va = byte_at(true, i);
        if let Some(fault) = latin1_element_fault(b_len, i, "b") {
            return Err(fault);
        }
        let vb = byte_at(false, i);
        if va != vb {
            return Ok(va - vb);
        }
    }
    Ok(len1.wrapping_sub(len2))
}

/// The pre-wave-6 answer (`CRATONVM_LATIN1_HOTSPOT_FAULTS=0`): `0` for a null
/// array; negative lengths read as `0`; `lim` clamped to the array lengths.
pub fn latin1_compare_to_legacy(
    a_len: Option<i32>,
    b_len: Option<i32>,
    len1: i32,
    len2: i32,
    mut byte_at: impl FnMut(bool, i32) -> i32,
) -> i32 {
    let (Some(a_len), Some(b_len)) = (a_len, b_len) else {
        return 0;
    };
    let (len1, len2) = (len1.max(0), len2.max(0));
    let lim = len1.min(len2).min(a_len).min(b_len);
    for i in 0..lim {
        let (va, vb) = (byte_at(true, i), byte_at(false, i));
        if va != vb {
            return va - vb;
        }
    }
    len1 - len2
}

/// Re-enter `StringLatin1.<method><descriptor>`'s JDK body with the
/// native's own arguments: the screen decided the call faults, and the body
/// throws that fault itself, from its own frames (`StringLatin1.compareTo`,
/// and for the 4-argument one `ArraysSupport.mismatch` above it, or
/// `StringLatin1.inflate`), as HotSpot's trace shows it. A leaf may not
/// re-enter Java, so on the leaf path the call declines first (nothing has
/// been written yet) and runs again on the full funnel, which gets here.
/// A native exception without the method's frame is not only a trace
/// difference: `DirectMethodHandleAccessor` reads a top frame of its own as
/// "the arguments were bad" and wraps a reflective call's NPE in
/// `IllegalArgumentException`.
pub fn latin1_fault_from_body(
    ctx: &mut dyn NativeContext,
    method: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    if ctx.decline_leaf_call() {
        return Ok(None);
    }
    ctx.invoke_special_bytecode_only("java/lang/StringLatin1", method, descriptor, args)
}

pub fn latin1_compare_to_answer(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    descriptor: &str,
    a: Option<ObjectRef>,
    b: Option<ObjectRef>,
    len1: i32,
    len2: i32,
) -> MethodCallResult {
    // Cast: a Java array length fits an `i32`.
    let a_len = a.map(|o| ctx.array_length(o) as i32);
    let b_len = b.map(|o| ctx.array_length(o) as i32);
    let answer = {
        let ctx = &*ctx;
        let byte_at = |first: bool, i: i32| -> i32 {
            let arr = if first { a } else { b };
            // Cast: callers pass an admitted index, `0 <= i < len`.
            match arr.map(|o| ctx.get_array_element(o, i as usize)) {
                Some(Value::Int(v)) => v & 0xff,
                _ => 0,
            }
        };
        match latin1_compare_to(a_len, b_len, len1, len2, byte_at) {
            Err(_) if false => {
                Ok(latin1_compare_to_legacy(a_len, b_len, len1, len2, byte_at))
            }
            other => other,
        }
    };
    match answer {
        Ok(v) => Ok(Some(Value::Int(v))),
        Err(_) => latin1_fault_from_body(ctx, "compareTo", descriptor, args),
    }
}


/// W5L4-6 / W6L1-P2 (JIT round 20 wave 11, lane pNat;
/// `docs/feature-designs/native-dispatch-architecture.md`
/// item 1): `CRATONVM_STRING_LATIN1_KC26_RETIRED` (default ON, `=0` keeps
/// them): over a real JDK, the KC26 `StringLatin1.compareTo` x2 / `getChar`
/// `Intrinsic`s are not registered, so the JDK's own bytecode -- which every
/// JDK this VM boots (17, 21, 25) declares with a body, `getChar` and both
/// `compareTo`s unchanged since JDK 9 -- runs in the interpreter and compiles
/// like any other method. Their registration's premise ("bypass bytecode loop
/// bugs in the interpreter's baload/if_icmpge interaction") is an interpreter
/// defect from an early round the current interpreter does not have, and the
/// bytecode's failures are HotSpot's by construction (the page's residual (a):
/// a 4-argument `compareTo` fault inside `ArraysSupport.mismatch` gets that
/// frame). The synthetic JDK (no `StringLatin1` bytecode) keeps them. Read
/// per registration (per VM).
pub fn string_latin1_kc26_retired(r: &NativeMethodRegistry) -> bool {
    r.real_jdk()
}

/// The same retirement for `StringLatin1.toLowerCase(String, byte[], Locale)`
/// (`CRATONVM_STRING_LATIN1_LOWER_RETIRED`, default ON, `=0` keeps it), its own
/// switch because it was registered for speed ("the generic bytecode path
/// allocates a transient String on each lookup"), not for the interpreter bug,
/// and because the JIT's single-pass `toLowerCase` ladder binds a thin helper
/// to the same triple: `cratonvm_jit` reads this switch there and binds nothing
/// while it is on (`jit/src/lib.rs`, the `StringLatin1.toLowerCase` ladder), so
/// the registry and the ladder cannot disagree. Real JDK only, as above.
pub fn string_latin1_lower_retired(r: &NativeMethodRegistry) -> bool {
    r.real_jdk()
        && true}

/// The same retirement for the LEAF `StringLatin1.inflate([BI[CII)V`
/// (`CRATONVM_STRING_LATIN1_INFLATE_RETIRED`, default ON, `=0` keeps it): its
/// own switch because a leaf skips the funnel, so only a measurement can say
/// whether the JDK loop (interpreted: one `baload`/`castore` per char;
/// compiled: the optimizing tier's `IrStringSearchKind::Inflate` prefix, which
/// answers the in-range case inline and declines to the call) beats it.
pub fn string_latin1_inflate_retired(r: &NativeMethodRegistry) -> bool {
    r.real_jdk()
}

pub fn register_string_latin1_natives(r: &mut NativeMethodRegistry) {
            let c = "java/lang/StringLatin1";
    let kc26_retired = string_latin1_kc26_retired(r);
    let lower_retired = string_latin1_lower_retired(r);
    let inflate_retired = string_latin1_inflate_retired(r);

    // Merge of JIT round 20 into dev: over a real JDK the round-20 retirement
    // switches below decide whether these rows are registered at all
    // (`CRATONVM_STRING_LATIN1_KC26_RETIRED` / `_LOWER_RETIRED` /
    // `_INFLATE_RETIRED`, default ON; JIT A/B in the round record: lower and
    // inflate won, compareTo tied). Their `=0` arm registers the forms below.
    if !kc26_retired {
        // The two `compareTo`s stay native because they measure faster than the
        // bytecode in the interpreter: natural `String` order (`TreeMap<String, _>`,
        // `Arrays.sort`) took about 0.55x the bytecode's time under `--nojit`. With
        // the JIT the two arms tie. They are leaves, like `inflate`: an in-range
        // call allocates nothing and does not re-enter Java, and a faulting one
        // declines to the full funnel and re-enters the JDK body there
        // (`latin1_fault_from_body`). `getChar` used to be registered here too, and
        // the bytecode beat it (about 0.75x on `String.CASE_INSENSITIVE_ORDER`), so
        // it is plain JDK bytecode now
        // (`docs/feature-designs/native-dispatch-architecture.md`).

        // static int compareTo(byte[] value, byte[] other)
        //
        // nio21 wave 6 L1: the bytecode's answers on every input.
        // `value.length` then `other.length` (a null is the `arraylength` NPE,
        // `value` first), then the 4-argument body over the real lengths, which
        // cannot fault. It answered `0` for a null array before.
        r.with_leaf(true, |r| {
            r.register(c, "compareTo", "([B[B)I", |ctx, args| {
                let a = latin1_obj_arg(args, 0);
                let b = latin1_obj_arg(args, 1);
                let (a, b) = match (a, b) {
                    (Some(a), Some(b)) => (a, b),
                    _ if false => return Ok(Some(Value::Int(0))),
                    _ => return latin1_fault_from_body(ctx, "compareTo", "([B[B)I", args),
                };
                // Cast: a Java array length fits an `i32`.
                let len1 = ctx.array_length(a) as i32;
                let len2 = ctx.array_length(b) as i32;
                latin1_compare_to_answer(ctx, args, "([B[B)I", Some(a), Some(b), len1, len2)
            }, cratonvm_native_api::NativeKind::Intrinsic);
        });

        // static int compareTo(byte[] value, byte[] other, int len1, int len2)
        //
        // nio21 wave 6 L1: the bytecode is `lim = Math.min(len1, len2)`, then
        // `ArraysSupport.mismatch(value, other, lim)`, then `len1 - len2` or the
        // two `getChar`s. So `lim <= 0` touches neither array (null or not) and
        // answers `len1 - len2`; otherwise the first faulting element load of the
        // mismatch loop is the answer (`a[i]` before `b[i]`). It used to answer
        // `0` for a null array and clamp `lim` to the array lengths.
        r.with_leaf(true, |r| {
            r.register(c, "compareTo", "([B[BII)I", |ctx, args| {
                let a = latin1_obj_arg(args, 0);
                let b = latin1_obj_arg(args, 1);
                let len1 = match args.get(2) {
                    Some(Value::Int(v)) => *v,
                    _ => 0,
                };
                let len2 = match args.get(3) {
                    Some(Value::Int(v)) => *v,
                    _ => 0,
                };
                latin1_compare_to_answer(ctx, args, "([B[BII)I", a, b, len1, len2)            }, cratonvm_native_api::NativeKind::Intrinsic);        });

        // `String.toLowerCase(Locale)` delegates here for compact Latin-1
        // Strings. The generic bytecode path allocates a transient String on each
        // lookup and is disproportionately expensive under the moving collector.
        // Reuse the String-level implementation, which preserves the unchanged
        // receiver and alternates distinct cached results for changed ASCII input.
        //
        // The static signature is `(String this, byte[] value, Locale locale)`, so
        // the receiver is `args[0]` and the locale `args[2]`; both are forwarded —
        // passing only `args[..1]` made this (JIT-reachable) path root-locale-only.
    }

    if !lower_retired {
        r.register(
            c,
            "toLowerCase",
            "(Ljava/lang/String;[BLjava/util/Locale;)Ljava/lang/String;",
            native_string_latin1_to_lower_case, cratonvm_native_api::NativeKind::Intrinsic,
        );

        // Leaf: no Java-heap allocation, no safepoint, no re-entry into Java (a
        // faulting call declines to the full funnel first). It skips the native
        // funnel (~200-300 ns a call).
    }
    if !inflate_retired {
        r.with_leaf(true, |r| {
            r.register(c, "inflate", "([BI[CII)V", native_string_latin1_inflate, cratonvm_native_api::NativeKind::Intrinsic);
        });
    }
    }

pub fn register_arrays_support_natives(r: &mut NativeMethodRegistry) {
            let c = "jdk/internal/util/ArraysSupport";
    r.register(
        c,
        "vectorizedHashCode",
        "(Ljava/lang/Object;IIII)I",
        native_arrays_support_vectorized_hash_code, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        c,
        "vectorizedMismatch",
        "(Ljava/lang/Object;JLjava/lang/Object;JII)I",
        native_arrays_support_vectorized_mismatch, cratonvm_native_api::NativeKind::Intrinsic,
    );
    // Direct native for mismatch(byte[], byte[], int) — bypasses JDK bytecode
    // that can be OSR-compiled incorrectly. Its answers on every input are
    // the JDK body's (`arrays_support_mismatch_answer`).
    //
    // The four `mismatch` overloads are LEAVES (they skip the ~200-300 ns native
    // funnel): an in-range call reads the arrays through the heap reader and
    // allocates nothing on the Java heap, and a call that faults and must run the
    // JDK body (`invoke_special_bytecode_only`) declines to the full funnel first
    // (`arrays_support_mismatch_answer`). A `jar:nested:` URL parse and every
    // `String.startsWith` / `Arrays.equals` pay this crossing.
    //
    // The two `byte[]` overloads register only in a VM WITHOUT a JIT
    // (`interpreted_intrinsic_registers_row`): with one the compiled JDK body
    // wins. `String.startsWith("jar:")` on the Azure host, one binary: 200 ns
    // through this native (a generic field-read path per call), a few ns as
    // bytecode; four of these per `jar:nested:` `new URL(context, spec)`.
    // `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS=arrays-mismatch-byte` keeps them
    // in a JIT VM, marked body-compilable. The `char[]` overloads stay (ECJ).
    const MISMATCH_BYTE_ROW: &str = "arrays-mismatch-byte";
    let byte_mismatch_native = r.interpreted_intrinsic_registers_row(MISMATCH_BYTE_ROW);
    if byte_mismatch_native {
        for desc in ["([B[BI)I", "([BI[BII)I"] {
            r.mark_body_compilable_intrinsic_row(
                MISMATCH_BYTE_ROW,
                "jdk/internal/util/ArraysSupport",
                "mismatch",
                desc,
            );
        }
    }
    r.with_leaf(true, |r| {
        if byte_mismatch_native {
            r.register(c, "mismatch", "([B[BI)I", |ctx, args| {
                arrays_support_mismatch_answer(ctx, args, false, 1)
            }, cratonvm_native_api::NativeKind::Intrinsic);
            // Direct native for mismatch(byte[], int, byte[], int, int)
            r.register(c, "mismatch", "([BI[BII)I", |ctx, args| {
                arrays_support_mismatch_answer(ctx, args, true, 1)
            }, cratonvm_native_api::NativeKind::Intrinsic);
        }
        // Direct native for mismatch(char[], char[], int). ECJ's
        // CharOperation.equals delegates to Arrays.equals(char[]), which reaches
        // this helper for class-file attribute names such as ModuleHashes. Running
        // the JDK bytecode here depends on Unsafe.getLongUnaligned over char[] and
        // can incorrectly report long equal ranges.
        r.register(c, "mismatch", "([C[CI)I", |ctx, args| {
            arrays_support_mismatch_answer(ctx, args, false, 2)
        }, cratonvm_native_api::NativeKind::Intrinsic);
        // Direct native for mismatch(char[], int, char[], int, int), same rationale
        // as the whole-array char[] overload above.
        r.register(c, "mismatch", "([CI[CII)I", |ctx, args| {
            arrays_support_mismatch_answer(ctx, args, true, 2)
        }, cratonvm_native_api::NativeKind::Intrinsic);
    });
    }

// ---------------------------------------------------------------------------
// The registered `ArraysSupport.mismatch` natives answer as the JDK body does
// (`docs/feature-designs/native-dispatch-architecture.md`).
// HotSpot 25.0.3, measured by calling the four overloads directly: `length <=
// 0` answers `-1` and touches neither array (null included); otherwise the
// loop loads `a[aFromIndex + i]` then `b[bFromIndex + i]`, so a difference
// before the first faulting load is the answer, and the fault is a null
// array's JEP 358 NPE named after the JDK's locals (`a` / `b`; `byte/boolean`
// or `char`) or `Index i out of bounds for length n`. The `length > 7` (`> 3`
// for `char[]`) prelude loads `a[aFromIndex]` / `b[bFromIndex]` in the same
// order; `vectorizedMismatch` over a range past an array's end reads raw
// memory on HotSpot (undefined), and this answers the loop's AIOOBE there.
// Every other fault is handed to the JDK body (`invoke_special_bytecode_only`),
// so its trace has HotSpot's `ArraysSupport.mismatch` frame and line; the
// screen only decides that it faults, so the in-range calls stay native.
// `CRATONVM_LATIN1_HOTSPOT_FAULTS=0` restores the old answers (`-1` for a
// null array, negative arguments read as `0`, the range clamped to the
// arrays); the switch is read on fault paths only.
// ---------------------------------------------------------------------------

/// What [`arrays_support_mismatch_answer`] compares: `n` elements from
/// `a_from` / `b_from`, then `fault` if no difference turned up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArraysMismatchPlan {
    pub a_from: i32,
    pub b_from: i32,
    pub n: i32,
    pub fault: Option<Latin1LoadFault>,
}

/// `ArraysSupport.mismatch`'s loop over arrays of lengths `a_len` / `b_len`
/// (`None`: null). `hotspot` false is the pre-fix answer.
pub fn arrays_mismatch_plan(
    a_len: Option<i32>,
    a_from: i32,
    b_len: Option<i32>,
    b_from: i32,
    length: i32,
    hotspot: bool,
) -> ArraysMismatchPlan {
    // Iterations whose load of `local[from + i]` is in range.
    let clean = |len: Option<i32>, from: i32| match len {
        Some(n) if from >= 0 && from < n => n - from,
        _ => 0,
    };
    let (ka, kb) = (clean(a_len, a_from), clean(b_len, b_from));
    let k = ka.min(kb);
    if length <= k {
        return ArraysMismatchPlan {
            a_from,
            b_from,
            n: length.max(0),
            fault: None,
        };
    }
    if !hotspot {
        let (Some(a_len), Some(b_len)) = (a_len, b_len) else {
            return ArraysMismatchPlan {
                a_from: 0,
                b_from: 0,
                n: 0,
                fault: None,
            };
        };
        let (a_from, b_from) = (a_from.max(0), b_from.max(0));
        let n = length
            .min((a_len - a_from).max(0))
            .min((b_len - b_from).max(0));
        return ArraysMismatchPlan {
            a_from,
            b_from,
            n,
            fault: None,
        };
    }
    // `a[aFromIndex + k]` loads before `b[bFromIndex + k]`; `from + k` cannot
    // overflow (`k` is `0` or `len - from`).
    let fault = if ka <= kb {
        latin1_element_fault(a_len, a_from + k, "a")
    } else {
        latin1_element_fault(b_len, b_from + k, "b")
    };
    ArraysMismatchPlan {
        a_from,
        b_from,
        n: k,
        fault,
    }
}

pub fn arrays_mismatch_fault_error(fault: Latin1LoadFault, width: usize) -> RuntimeError {
    match fault {
        Latin1LoadFault::Null(name) if width == 2 => RuntimeError::NullPointerException {
            message: Some(format!(
                "Cannot load from char array because \"{name}\" is null"
            )),
        },
        other => other.into_error(),
    }
}

/// `ArraysSupport.mismatch(a, b, length)` (`offsets` false) or
/// `mismatch(a, aFromIndex, b, bFromIndex, length)` over `width`-byte
/// elements (1: `byte[]`, 2: `char[]`).
pub fn arrays_support_mismatch_answer(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    offsets: bool,
    width: usize,
) -> MethodCallResult {
    let int_at = |i: usize| match args.get(i) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (a, a_from, b, b_from, length) = if offsets {
        (
            latin1_obj_arg(args, 0),
            int_at(1),
            latin1_obj_arg(args, 2),
            int_at(3),
            int_at(4),
        )
    } else {
        (
            latin1_obj_arg(args, 0),
            0,
            latin1_obj_arg(args, 1),
            0,
            int_at(2),
        )
    };
    // Cast: a Java array length fits an `i32`.
    let a_len = a.map(|o| ctx.array_length(o) as i32);
    let b_len = b.map(|o| ctx.array_length(o) as i32);
    let mut plan = arrays_mismatch_plan(a_len, a_from, b_len, b_from, length, true);
    if plan.fault.is_some() && false {
        plan = arrays_mismatch_plan(a_len, a_from, b_len, b_from, length, false);
    }
    if let (Some(a), Some(b), true) = (a, b, plan.n > 0) {
        // Casts: with `n > 0` both starts are in range, so non-negative.
        let (af, bf, n) = (plan.a_from as usize, plan.b_from as usize, plan.n as usize);
        match mismatch_bulk(ctx, a, af, b, bf, n, width) {
            Some(-1) => {}
            Some(answer) => return Ok(Some(Value::Int(answer))),
            None => {
                for i in 0..n {
                    if ctx.get_array_element(a, af + i) != ctx.get_array_element(b, bf + i) {
                        // Cast: `i < n <= i32::MAX`.
                        return Ok(Some(Value::Int(i as i32)));
                    }
                }
            }
        }
    }
    match plan.fault {
        // The JDK body throws it itself, from its own frame at the faulting
        // load's line, as HotSpot's trace shows it.
        Some(_) if arrays_mismatch_fault_is_the_bodys(length, plan.n, width) => {
            let descriptor = match (offsets, width) {
                (false, 1) => "([B[BI)I",
                (false, _) => "([C[CI)I",
                (true, 1) => "([BI[BII)I",
                (true, _) => "([CI[CII)I",
            };
            // A leaf may not re-enter Java: decline first (nothing has been
            // written yet) and run again on the full funnel.
            if ctx.decline_leaf_call() {
                return Ok(None);
            }
            ctx.invoke_special_bytecode_only(
                "jdk/internal/util/ArraysSupport",
                "mismatch",
                descriptor,
                args,
            )
        }
        Some(fault) => Err(arrays_mismatch_fault_error(fault, width).into()),
        None => Ok(Some(Value::Int(-1))),
    }
}

/// Does the JDK body raise this fault itself (after `clean` in-range
/// iterations of `length`)? Yes when its plain loop runs (`length <= 7`, `<=
/// 3` for `char[]`) or the prelude's index-0 load faults (`clean == 0`).
/// Otherwise `vectorizedMismatch` would read past an array (undefined on
/// HotSpot) and the native answers the loop's AIOOBE.
pub fn arrays_mismatch_fault_is_the_bodys(length: i32, clean: i32, width: usize) -> bool {
    clean == 0 || length <= if width == 2 { 3 } else { 7 }
}


/// HotSpot `BasicType` enum values used by `vectorizedHashCode`'s
/// `basicType` argument. These are stable ABI constants and must not
/// change — the JDK's `jdk.internal.util.ArraysSupport` hardcodes them.
pub const HOTSPOT_T_BOOLEAN: i32 = 4;
pub const HOTSPOT_T_BYTE: i32 = 8;
pub const HOTSPOT_T_SHORT: i32 = 9;
pub const HOTSPOT_T_CHAR: i32 = 5;
pub const HOTSPOT_T_INT: i32 = 10;

pub fn native_arrays_support_vectorized_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let initial_value = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    // hash21: a missing array answers the CALLER'S initial value (the JDK body
    // returns it for an empty range), not a hard-coded 1: `StringLatin1` /
    // `StringUTF16` seed 0, `Arrays.hashCode` seeds 1.
    let arr = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(initial_value))),
    };
    let from_index = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let length = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let basic_type = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => HOTSPOT_T_BYTE,
    };

    if length <= 0 || from_index < 0 {
        return Ok(Some(Value::Int(initial_value)));
    }
    let arr_len = ctx.array_length(arr);

    // T_CHAR over a byte[] is the one case whose ELEMENT is not one array slot.
    //
    // `StringUTF16.hashCode(byte[] value)` calls
    // `vectorizedHashCode(value, 0, value.length >> 1, 0, T_CHAR)`: the array
    // is a `byte[]`, `length` counts CHARS, and each char is a PAIR of bytes.
    // Every other BasicType here has one element per slot.
    //
    // This loop used to read one slot per iteration for T_CHAR as well, so a
    // 3-char string hashed bytes 0..3 instead of pairing 0/1, 2/3, 4/5 — and
    // `(elem as u16)` on a signed byte sign-extended each one. Measured:
    // `"ΣΟΣ".hashCode()` returned 62956255 where the JLS (and
    // HotSpot) say 924359, while `charAt`, `length` and `equals` on the same
    // object were all correct. `StringUTF16.getChar` and `StringUTF16.length`
    // were correct too when invoked directly — which is what localised the
    // fault here rather than in the class.
    //
    // Latin-1 was unaffected and that is why this survived: `StringLatin1`
    // reaches this function through `hashCodeOfUnsigned` with T_BOOLEAN, one
    // byte per element, masked — correct as written. Only a `String`
    // containing a code unit > 0xFF takes the T_CHAR path at all.
    //
    // Little-endian, to match `native_string_utf16_is_big_endian` returning
    // false and every Rust accessor in `lang_string.rs`. The comment below
    // said "big-endian u16 pairs", which was the OpenJDK-on-a-BE-host
    // description and never this VM's layout.
    // T_CHAR arrives with TWO different array types and they are NOT the same
    // read. The discriminator is the array, not the `basicType`:
    //
    //   * `Arrays.hashCode(char[] a)` -> `vectorizedHashCode(a, 0, a.length, 1,
    //     T_CHAR)`. A `char[]`: one slot per element, zero-extended.
    //   * `StringUTF16.hashCode(byte[] v)` -> `vectorizedHashCode(v, 0,
    //     v.length >> 1, 0, T_CHAR)`. A `byte[]` of UTF-16 pairs: TWO slots per
    //     element, and `length` counts chars.
    //
    // Keying on `basic_type` alone gets one of them wrong whichever way it is
    // written. `t2_arrays_support_hash_code_char_zero_extends` covers the
    // `char[]` side and caught exactly that.
    let char_pairs = basic_type == HOTSPOT_T_CHAR
        && matches!(
            ctx.heap_element_type_of(arr),
            cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean
        );
    let (start_slot, end_slot) = if char_pairs {
        let start = (from_index as usize).saturating_mul(2);
        (
            start,
            start.saturating_add((length as usize).saturating_mul(2)),
        )
    } else {
        let start = from_index as usize;
        (start, start.saturating_add(length as usize))
    };
    // Guard against out-of-range — the JDK contract says the caller is
    // responsible, but we refuse to read past the end defensively.
    if end_slot > arr_len {
        return Ok(Some(Value::Int(initial_value)));
    }

    // Strings round (`str16`): fold over the payload read in bulk, a chunk at
    // a time into a stack buffer, instead of two validated heap trips and a
    // boxed `Value` per element. Compiled `String.hashCode()` reaches here on
    // every fresh string whose call site is not the inline intrinsic
    // (`StringLatin1.hashCode` -> `ArraysSupport.hashCodeOfUnsigned`), where
    // the element loop below cost ~1 us for a 14-char string. `None` = the
    // context has no raw read for this array (a mock, a reference array):
    // the loop runs as before.
    if let Some(acc) = vectorized_hash_code_bulk(
        ctx,
        arr,
        start_slot,
        end_slot,
        initial_value,
        basic_type,
        char_pairs,
    ) {
        return Ok(Some(Value::Int(acc)));
    }

    let mut acc = initial_value;
    if char_pairs {
        let mut slot = start_slot;
        while slot < end_slot {
            let lo = match ctx.get_array_element(arr, slot) {
                Value::Int(v) => (v as u8) as u16,
                Value::Long(v) => (v as u8) as u16,
                _ => 0,
            };
            let hi = match ctx.get_array_element(arr, slot + 1) {
                Value::Int(v) => (v as u8) as u16,
                Value::Long(v) => (v as u8) as u16,
                _ => 0,
            };
            let unit = ((hi << 8) | lo) as i32;
            acc = acc.wrapping_mul(31).wrapping_add(unit);
            slot += 2;
        }
        return Ok(Some(Value::Int(acc)));
    }
    for i in start_slot..end_slot {
        let elem = match ctx.get_array_element(arr, i) {
            Value::Int(v) => v,
            Value::Long(v) => v as i32,
            _ => 0,
        };
        // Normalize the per-element contribution by BasicType so the
        // numeric result matches `String.hashCode` / `Arrays.hashCode`.
        //
        // The JDK uses HotSpot's BasicType enum here (NOT a literal type-
        // descriptor). In particular, `ArraysSupport.hashCodeOfUnsigned`
        // (the entry point used by `StringLatin1.hashCode(byte[])`) passes
        // T_BOOLEAN(=4) as a sentinel meaning "unsigned byte hash" — the
        // intrinsic's contract is `(byte[i] & 0xff)`, NOT `byte[i] & 1`.
        // OpenJDK 25 `ArraysSupport.vectorizedHashCode` switches on:
        //   T_BOOLEAN → unsignedHashCode(byte[]) → `(a[i] & 0xff)`
        //   T_BYTE    → hashCode(byte[])         → sign-extend `byte`
        //   T_SHORT   → hashCode(short[])        → sign-extend `short`
        //   T_CHAR    → utf16hashCode(byte[])    → u16 PAIRS, handled above
        //   T_INT     → hashCode(int[])          → raw 32-bit
        let contribution = match basic_type {
            HOTSPOT_T_BOOLEAN => elem & 0xff,
            HOTSPOT_T_BYTE => elem as i8 as i32,
            HOTSPOT_T_SHORT => elem as i16 as i32,
            HOTSPOT_T_CHAR => (elem as u16) as i32,
            HOTSPOT_T_INT => elem,
            _ => elem,
        };
        acc = acc.wrapping_mul(31).wrapping_add(contribution);
    }
    Ok(Some(Value::Int(acc)))
}

/// One chunk of [`vectorized_hash_code_bulk`]: `payload` is whole elements of
/// a primitive array of `width` bytes each (`is_short`: a `short[]`, whose
/// default decode sign-extends), decoded exactly as the slot loop of
/// [`native_arrays_support_vectorized_hash_code`] decodes the `Value` it
/// reads, and folded `acc * 31 + element`.
pub fn fold_hash_payload(
    mut acc: i32,
    payload: &[u8],
    width: usize,
    is_short: bool,
    basic_type: i32,
    char_pairs: bool,
) -> i32 {
    let step = if char_pairs { 2 } else { width };
    for chunk in payload.chunks_exact(step) {
        let contribution = if char_pairs {
            i32::from(u16::from_le_bytes([chunk[0], chunk[1]]))
        } else {
            let raw = match width {
                1 => i32::from(chunk[0]),
                2 => i32::from(u16::from_le_bytes([chunk[0], chunk[1]])),
                _ => i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
            };
            // As the slot loop normalises a slot value by `basic_type`.
            match basic_type {
                HOTSPOT_T_BOOLEAN => raw & 0xff,
                HOTSPOT_T_BYTE => raw as i8 as i32,
                HOTSPOT_T_SHORT => raw as i16 as i32,
                HOTSPOT_T_CHAR => raw & 0xffff,
                HOTSPOT_T_INT => raw,
                // The slot loop's `_ => elem`: the slot's own value,
                // sign-extended as its `Value::Int` was.
                _ => match width {
                    1 => raw as i8 as i32,
                    2 if is_short => raw as i16 as i32,
                    _ => raw,
                },
            }
        };
        acc = acc.wrapping_mul(31).wrapping_add(contribution);
    }
    acc
}

/// [`native_arrays_support_vectorized_hash_code`]'s fold over the array's raw
/// payload (strings round, `str16`): slots `[start_slot, end_slot)` read with
/// [`NativeContext::read_primitive_array_bytes`] in chunks of a stack buffer,
/// each element decoded exactly as the slot loop decodes it (`char_pairs`: a
/// `byte[]` of little-endian UTF-16 pairs; otherwise one element per slot,
/// normalised by `basic_type`). `None` when the array is not a primitive
/// array this fold knows, or the context answers short (nothing to trust).
pub fn vectorized_hash_code_bulk(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    start_slot: usize,
    end_slot: usize,
    initial_value: i32,
    basic_type: i32,
    char_pairs: bool,
) -> Option<i32> {
    use cratonvm_types::ArrayElementType as E;
    let elem = ctx.heap_element_type_of(arr);
    if !matches!(elem, E::Byte | E::Boolean | E::Char | E::Short | E::Int) {
        return None;
    }
    // The heap's own payload width, which the raw read is offset in.
    let width = cratonvm_types::element_byte_size(elem);
    if !matches!(width, 1 | 2 | 4) {
        return None;
    }
    // One decoded element per `step` bytes of payload.
    let step = if char_pairs { 2 } else { width };
    if char_pairs && width != 1 {
        return None;
    }
    let mut acc = initial_value;
    let mut buf = [0u8; 1024];
    let mut byte_off = start_slot.checked_mul(width)?;
    let end_byte = end_slot.checked_mul(width)?;
    while byte_off < end_byte {
        // A whole number of elements per chunk.
        let want = (end_byte - byte_off).min(buf.len() / step * step);
        let got = ctx.read_primitive_array_bytes(arr, byte_off, &mut buf[..want]);
        if got != want {
            return None;
        }
        acc = fold_hash_payload(
            acc,
            &buf[..want],
            width,
            elem == E::Short,
            basic_type,
            char_pairs,
        );
        byte_off += want;
    }
    Some(acc)
}

pub fn native_arrays_support_vectorized_mismatch(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(answer) = vectorized_mismatch_raw_bytes(ctx, args) {
        return Ok(Some(Value::Int(answer)));
    }
    let length = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // JIT round 17 wave 6, lane vecmis: every refusal below answers `!length`
    // ("nothing compared; the caller's scalar tail checks all `length`
    // elements"), the value the JDK's own Java body returns when it compares no
    // word, instead of `-1` ("all equal"), which made the caller skip its tail
    // and report the ranges equal. For `length == 0` both are `-1`; for a
    // negative length `!length` is exactly what the Java body answers.
    // `CRATONVM_VECTORIZED_MISMATCH_STRICT=0` restores `-1`.
    let refuse = || -> MethodCallResult {
        let answer = if vectorized_mismatch_flags().strict {
            !length
        } else {
            -1
        };
        Ok(Some(Value::Int(answer)))
    };
    let a = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return refuse(),
    };
    let a_offset_bytes = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let b = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return refuse(),
    };
    let b_offset_bytes = match args.get(3) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let log2_scale = match args.get(5) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    if length <= 0 || log2_scale < 0 || log2_scale > 3 {
        return refuse();
    }
    // The offsets are *Unsafe* byte offsets from the object header, so for a
    // real array they START at the array base offset (ABASE = 16), not 0:
    // OpenJDK's `Arrays.equals(char[],…)` calls
    // `vectorizedMismatch(a, ARRAY_CHAR_BASE_OFFSET, …)`. `get_array_element`
    // is 0-based, so we must subtract ABASE before translating to an element
    // index. The previous code divided the raw offset by `scale`, giving
    // `16/2 = 8` for `char[]` (and `16/8 = 2` for `long[]`): the
    // out-of-range guard below then tripped and the native reported "equal"
    // for ANY two char[]/long[] arrays — breaking `Arrays.equals(char[])`
    // (e.g. ecj's `CharOperation.equals("Signature","Synthetic")` → every
    // generic method tagged `ACC_SYNTHETIC` → dropped). Offsets below ABASE
    // (the synthetic unit tests pass 0) are treated as already
    // element-byte-relative for backwards compatibility.
    const ABASE: i64 = 16;
    let scale = 1i64 << log2_scale;
    let rel = |off: i64| -> i64 {
        if off >= ABASE {
            off - ABASE
        } else {
            off.max(0)
        }
    };
    // A negative offset, or one that is not a whole number of elements past
    // the base, names bytes this element-index path cannot address (the old
    // code clamped / truncated it to a different element); no JDK caller
    // passes one, so the scalar tail decides.
    let strict_offset = |off: i64| off < 0 || rel(off) % scale != 0;
    if (strict_offset(a_offset_bytes) || strict_offset(b_offset_bytes))
        && vectorized_mismatch_flags().strict
    {
        return refuse();
    }
    let a_idx = (rel(a_offset_bytes) / scale) as usize;
    let b_idx = (rel(b_offset_bytes) / scale) as usize;

    let a_len = ctx.array_length(a);
    let b_len = ctx.array_length(b);
    let n = length as usize;
    if a_idx.saturating_add(n) > a_len || b_idx.saturating_add(n) > b_len {
        return refuse();
    }

    // HotSpot compares the two ranges as memory, so a `float`/`double` pair
    // differs exactly when its BITS differ: `0.0` vs `-0.0` is a mismatch
    // (`Arrays.equals(new float[]{1, 0}, new float[]{1, -0f})` is false) and
    // two identical NaNs are not. `Value`'s derived `==` is IEEE equality, the
    // opposite on both. (The JDK callers re-check a NaN hit themselves, so
    // only the signed-zero case changed a public answer.)
    let strict = vectorized_mismatch_flags().strict;
    // `CRATONVM_ARRAYS_MISMATCH_BULK`: the same first differing element, read
    // as payload bytes. Only under `strict`, whose bit compare of float /
    // double elements IS the byte compare (the IEEE `!=` of the old answer is
    // not).
    let bulk = if strict {
        mismatch_bulk(ctx, a, a_idx, b, b_idx, n, scale as usize)
    } else {
        None
    };
    if let Some(answer) = bulk {
        return Ok(Some(Value::Int(answer)));
    }
    for i in 0..n {
        let va = ctx.get_array_element(a, a_idx + i);
        let vb = ctx.get_array_element(b, b_idx + i);
        let differs = if strict {
            vectorized_mismatch_bits_differ(va, vb)
        } else {
            va != vb
        };
        if differs {
            return Ok(Some(Value::Int(i as i32)));
        }
    }
    // JDK contract (jdk.internal.util.ArraysSupport.vectorizedMismatch):
    // returns the index of the first differing element, or -1 when the
    // two ranges are byte-wise equal over `length` elements.
    Ok(Some(Value::Int(-1)))
}

/// The `vectorizedMismatch` switches, read once together (one cache, not one
/// static per switch). Kill switches for correctness fixes and one opt-in perf
/// switch; none of them is compatibility state.
#[derive(Clone, Copy)]
pub struct VectorizedMismatchFlags {
    /// `CRATONVM_VECTORIZED_MISMATCH_RAW_BYTES` (default on, `=0` off): see
    /// [`vectorized_mismatch_raw_bytes`].
    pub raw_bytes: bool,
    /// `CRATONVM_VECTORIZED_MISMATCH_STRICT` (default on, `=0` off): the
    /// element-index path refuses with `!length`, not `-1`, and compares
    /// float/double elements by bits (lane vecmis, round 17 wave 6).
    pub strict: bool,
    /// `CRATONVM_VECTORIZED_MISMATCH_CHUNKED` (default on since nio20, `=0` off): the
    /// raw-bytes path compares [`VECTORIZED_MISMATCH_WINDOW`]-byte windows and
    /// stops at the first differing one (BUFCMP17-2).
    pub chunked: bool,
}

pub fn vectorized_mismatch_flags() -> VectorizedMismatchFlags {
    static FLAGS: std::sync::OnceLock<VectorizedMismatchFlags> = std::sync::OnceLock::new();
    *FLAGS.get_or_init(|| VectorizedMismatchFlags {
        raw_bytes: true,
        strict: true,
        chunked: true,
    })
}

pub fn vectorized_mismatch_raw_bytes_enabled() -> bool {
    vectorized_mismatch_flags().raw_bytes
}

/// Whether two array elements differ as MEMORY (HotSpot's compare): float and
/// double by their raw bits, everything else by value.
pub fn vectorized_mismatch_bits_differ(va: Value, vb: Value) -> bool {
    match (va, vb) {
        (Value::Float(x), Value::Float(y)) => x.to_bits() != y.to_bits(),
        (Value::Double(x), Value::Double(y)) => x.to_bits() != y.to_bits(),
        _ => va != vb,
    }
}

/// Window size (bytes per side) of the chunked raw-bytes compare.
pub const VECTORIZED_MISMATCH_WINDOW: usize = 4096;

/// Window (bytes per side, on the stack) of [`mismatch_bulk_with`]. A multiple
/// of every element width (1, 2, 4, 8), so a window never splits an element.
pub const MISMATCH_BULK_WINDOW: usize = 2048;

/// `CRATONVM_ARRAYS_MISMATCH_BULK` (interpreter round i3 wave 74, lane L5):
/// the first differing element of `a[a_idx..a_idx + n]` and
/// `b[b_idx..b_idx + n]`, two primitive arrays of `width`-byte elements,
/// compared as payload bytes copied out in [`MISMATCH_BULK_WINDOW`]-byte
/// windows -- instead of the two `get_array_element` calls (each a heap
/// validation and a `Value` decode) per element the `ArraysSupport.mismatch`
/// intrinsics and `vectorizedMismatch`'s element-index path make. A 4 KB
/// `Arrays.equals(byte[], byte[])` was 8000 such calls.
///
/// Elements are equal exactly when their bytes are (an integral or `char`
/// element's value is its bytes; the float / double caller only comes here
/// under the bitwise `strict` compare), so the answer -- the index of the
/// first differing element, `-1` when none -- is the per-element loop's.
///
/// `None` (the caller runs its per-element loop, from the start) when the
/// switch is off, a side is not a primitive array of `width`-byte elements,
/// or a read comes back short (a G1 humongous array with no flat payload, the
/// trait's default `read_primitive_array_bytes`). The caller has already
/// clamped `n` to both arrays.
pub fn mismatch_bulk(
    ctx: &mut dyn NativeContext,
    a: ObjectRef,
    a_idx: usize,
    b: ObjectRef,
    b_idx: usize,
    n: usize,
    width: usize,
) -> Option<i32> {
    if !ctx.arrays_mismatch_bulk() {
        return None;
    }
    let of_width = |ctx: &mut dyn NativeContext, o: ObjectRef| {
        ctx.heap_kind_of(o) == cratonvm_types::ObjectKind::Array
            && ctx.heap_element_type_of(o) != cratonvm_types::ArrayElementType::Reference
            && crate::array_elem_byte_width(ctx.heap_element_type_of(o)) == width
    };
    if !of_width(ctx, a) || !of_width(ctx, b) {
        return None;
    }
    let a_byte = a_idx.checked_mul(width)?;
    let b_byte = b_idx.checked_mul(width)?;
    let reader: &dyn NativeContext = ctx;
    mismatch_bulk_with(
        &mut |side_b: bool, off: usize, dst: &mut [u8]| -> usize {
            reader.read_primitive_array_bytes(if side_b { b } else { a }, off, dst)
        },
        a_byte,
        b_byte,
        n,
        width,
    )
}

/// The window loop of [`mismatch_bulk`] over a byte reader
/// (`read(side_b, byte_off, dst)` answers the bytes copied; `side_b` false
/// for `a`), so the unit tests can drive it without a heap. `Some(index)` of
/// the first differing `width`-byte element, `Some(-1)` when the `n`
/// elements agree, `None` when a read comes back short.
pub fn mismatch_bulk_with(
    read: &mut dyn FnMut(bool, usize, &mut [u8]) -> usize,
    a_byte: usize,
    b_byte: usize,
    n: usize,
    width: usize,
) -> Option<i32> {
    if width == 0 || MISMATCH_BULK_WINDOW % width != 0 {
        return None;
    }
    let total = n.checked_mul(width)?;
    let mut xa = [0u8; MISMATCH_BULK_WINDOW];
    let mut xb = [0u8; MISMATCH_BULK_WINDOW];
    let mut done = 0usize;
    while done < total {
        let win = (total - done).min(MISMATCH_BULK_WINDOW);
        if read(false, a_byte.checked_add(done)?, &mut xa[..win]) != win
            || read(true, b_byte.checked_add(done)?, &mut xb[..win]) != win
        {
            return None;
        }
        if xa[..win] != xb[..win] {
            let at = xa[..win]
                .iter()
                .zip(&xb[..win])
                .position(|(x, y)| x != y)
                .unwrap_or(0);
            return i32::try_from((done + at) / width).ok();
        }
        done += win;
    }
    Some(-1)
}

/// Largest range (bytes per side) [`vectorized_mismatch_raw_bytes`] buffers.
pub const VECTORIZED_MISMATCH_MAX_BYTES: usize = 256 * 1024 * 1024;

/// JIT round 17 wave 4, lane bufcmp: the `vectorizedMismatch` shapes the
/// element-index path above cannot answer, compared as RAW BYTES the way
/// HotSpot's (and the JDK's own Java body) does.
///
/// * a NULL base -- an off-heap address (a direct buffer, an FFM segment).
///   `BufferMismatch.mismatch` passes `a.base()` / `a.address`, which for a
///   direct `IntBuffer` view is `null` + a real pointer; the path above
///   answered `-1` ("all equal") for any null base, so
///   `directLeView.mismatch(IntBuffer.wrap({1,2,9,-4}))` over `{1,2,3,-4}`
///   was `-1` where HotSpot answers `2` (MEASURED, R17Nio2Typed `cmp` field 8);
/// * an array whose element width is not `1 << log2Scale` -- a typed view over
///   a `byte[]` (heap `ByteBuffer.order(nativeOrder()).asIntBuffer()`), which
///   the element-index path read as one BYTE per int.
///
/// `Some(answer)`: the first differing element index, `-1` when equal, or
/// `!length` ("nothing compared; the caller's scalar tail checks all
/// `length` elements" -- the contract's bitwise-complement-of-the-remainder
/// answer) when a side cannot be read. `None`: a shape the element-index path
/// owns (two arrays of the scaled width), or the switch is off.
pub fn vectorized_mismatch_raw_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<i32> {
    let obj = |pos: usize| match args.get(pos) {
        Some(Value::Object(o)) => Some(*o),
        _ => None,
    };
    let long = |pos: usize| match args.get(pos) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => i64::from(*v),
        _ => 0,
    };
    let (a, b) = (obj(0)?, obj(2)?);
    let (a_off, b_off) = (long(1), long(3));
    let length = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => return None,
    };
    let log2_scale = match args.get(5) {
        Some(Value::Int(v)) => *v,
        _ => return None,
    };
    if length <= 0 || !(0..=3).contains(&log2_scale) {
        return None;
    }
    let scale = 1usize << log2_scale;
    let scaled_array = |ctx: &mut dyn NativeContext, o: Option<cratonvm_types::ObjectRef>| match o {
        Some(o) => {
            ctx.heap_kind_of(o) == cratonvm_types::ObjectKind::Array
                && ctx.heap_element_type_of(o) != cratonvm_types::ArrayElementType::Reference
                && crate::array_elem_byte_width(ctx.heap_element_type_of(o)) == scale
        }
        None => false,
    };
    if scaled_array(ctx, a) && scaled_array(ctx, b) {
        return None;
    }
    if !vectorized_mismatch_raw_bytes_enabled() {
        return None;
    }
    let Some(n_bytes) = (length as usize).checked_mul(scale) else {
        return Some(!length);
    };
    if n_bytes > VECTORIZED_MISMATCH_MAX_BYTES {
        return Some(!length);
    }
    let window = if vectorized_mismatch_flags().chunked {
        VECTORIZED_MISMATCH_WINDOW
    } else {
        n_bytes
    };
    Some(vectorized_mismatch_compare_windows(
        ctx, a, a_off, b, b_off, n_bytes, scale, length, window,
    ))
}

/// Reads `len` raw bytes of one `vectorizedMismatch` side: off-heap memory for
/// a null base, the array's bytes (Unsafe offset, base included) otherwise.
pub fn vectorized_mismatch_read(
    ctx: &mut dyn NativeContext,
    o: Option<cratonvm_types::ObjectRef>,
    off: i64,
    len: usize,
) -> Option<Vec<u8>> {
    match o {
        None => {
            let mut v = vec![0u8; len];
            ctx.copy_from_native_memory(off, &mut v).then_some(v)
        }
        Some(arr) if off >= 0 => crate::unsafe_array_read_bytes(ctx, arr, off as usize, len),
        Some(_) => None,
    }
}

/// Compares `n_bytes` per side in `window`-byte steps and stops at the first
/// differing window. `window == n_bytes` is the wave-4 whole-range compare;
/// `CRATONVM_VECTORIZED_MISMATCH_CHUNKED` passes
/// [`VECTORIZED_MISMATCH_WINDOW`] (BUFCMP17-2, lane vecmis), so a difference
/// early in a large direct buffer no longer copies both whole ranges. `window`
/// must be a multiple of `scale` (every power-of-two window >= 8 is), so a
/// window never splits an element. A side that cannot be read answers
/// `!length`, as the whole-range compare does.
#[allow(clippy::too_many_arguments)]
pub fn vectorized_mismatch_compare_windows(
    ctx: &mut dyn NativeContext,
    a: Option<cratonvm_types::ObjectRef>,
    a_off: i64,
    b: Option<cratonvm_types::ObjectRef>,
    b_off: i64,
    n_bytes: usize,
    scale: usize,
    length: i32,
    window: usize,
) -> i32 {
    debug_assert!(window >= scale && window % scale == 0);
    let window = window.max(scale);
    let mut done = 0usize;
    while done < n_bytes {
        let win = (n_bytes - done).min(window);
        // `done < n_bytes <= VECTORIZED_MISMATCH_MAX_BYTES`: the cast and the
        // add cannot overflow for an array offset; an off-heap address wraps
        // the way the hardware address would.
        let step = done as i64;
        let (Some(x), Some(y)) = (
            vectorized_mismatch_read(ctx, a, a_off.wrapping_add(step), win),
            vectorized_mismatch_read(ctx, b, b_off.wrapping_add(step), win),
        ) else {
            return !length;
        };
        for (i, (ex, ey)) in x.chunks_exact(scale).zip(y.chunks_exact(scale)).enumerate() {
            if ex != ey {
                return (done / scale + i) as i32;
            }
        }
        done += win;
    }
    -1
}

// ===========================================================================
// T2.3 completion — items 2/3/6/8/12 of the java.util.* roadmap.
// ===========================================================================
//
// Scope per `roadmap-100.md`:
//   T2.3.2 — `ConcurrentHashMap.tabAt` / `casTabAt` / `setTabAt`
//   T2.3.3 — `ArrayList.elementData(int)` package-private accessor
//   T2.3.6 — `Arrays.parallelSort` for `[I`, `[J`, `[D`, `[Ljava/lang/Object;`
//   T2.3.8 — `Spliterator$OfInt/OfLong/OfDouble` primitive specializations
//            (`tryAdvance(IntConsumer)` / `forEachRemaining(IntConsumer)` etc.)
//   T2.3.12 — `Scanner.findWithinHorizon(Pattern, long)` regex integration
//
// All other T2.3 items are already covered elsewhere:
//   T2.3.1  `ArraysSupport.vectorizedHashCode/Mismatch` — see above.
//   T2.3.4  `Collections.shuffle(List)` — `phases_early.rs` line 1292.
//   T2.3.5  `Random.nextLong/nextDouble`                — `rng.rs`.
//   T2.3.7  `Arrays.stream`                             — `phases_early.rs:655`.
//   T2.3.9  `IntSummaryStatistics` + siblings           — `phases_late.rs:2161`.
//   T2.3.10 `EnumSet/EnumMap`                           — registered in 50.
//   T2.3.11 `BitSet.toLongArray`                        — registered in 50.
//   T2.3.13 `StringTokenizer.countTokens`               — see t2_tests below.
//   T2.3.14 `Optional.orElseThrow`                      — `native-collections`.
//   T2.3.15/16/17 Stream collect/flatMap/iterate/generate
//                                                      — `native-collections`,
//                                                        `phases_late.rs:1260`.
//   T2.3.18 `Collectors.groupingBy(..., Supplier, Collector)`
//                                                      — `native-collections`.
//   T2.3.19 `Collectors.partitioningBy(Predicate, Collector)`
//                                                      — `native-collections`.
//   T2.3.20 `IntStream.range(Closed)`                   — `native-collections`.

pub fn register_t2_3_completion_natives(r: &mut NativeMethodRegistry) {
            register_chm_tab_natives(r);
    register_arraylist_element_data(r);
    register_arrays_parallel_sort(r);
    register_spliterator_primitive_natives(r);
    }

// ---------------------------------------------------------------------------
// T2.3.2 — ConcurrentHashMap.tabAt / casTabAt / setTabAt
// ---------------------------------------------------------------------------
//
// These are package-private static helpers in OpenJDK's CHM that operate
// directly on the internal `Node[] tab` array using `Unsafe`'s
// acquire/release/CAS variants. In our JVM the real backing store for CHM
// is the per-segment map in `native-collections`, so these helpers are
// called only from CHM Java bytecode paths that we do not fully override
// (such as `ConcurrentHashMap.initTable`, `helpTransfer`, and reflective
// callers). Registering them ensures correct semantics if any of those
// paths is exercised at runtime.
//
// We implement them as real array operations guarded by the array's
// monitor: the CAS is not a hardware CAS, but the array is never observed
// by concurrent threads without going through these wrappers, so
// monitor-wrapped load/store/CAS is equivalent to the acquire/release
// semantics `Unsafe.compareAndSetObject` provides in this context.
//
// The Java-level signatures are:
//   static <K,V> Node<K,V> tabAt(Node<K,V>[] tab, int i)
//   static <K,V> boolean   casTabAt(Node<K,V>[] tab, int i, Node expect, Node val)
//   static <K,V> void      setTabAt(Node<K,V>[] tab, int i, Node v)
//
// which JVM-erases to the plain-Object signatures below.

pub const CHM_CLASS: &str = "java/util/concurrent/ConcurrentHashMap";

pub fn register_chm_tab_natives(r: &mut NativeMethodRegistry) {
    r.register(
        CHM_CLASS,
        "tabAt",
        "([Ljava/util/concurrent/ConcurrentHashMap$Node;I)Ljava/util/concurrent/ConcurrentHashMap$Node;",
        native_chm_tab_at, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CHM_CLASS,
        "setTabAt",
        "([Ljava/util/concurrent/ConcurrentHashMap$Node;ILjava/util/concurrent/ConcurrentHashMap$Node;)V",
        native_chm_set_tab_at, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CHM_CLASS,
        "casTabAt",
        "([Ljava/util/concurrent/ConcurrentHashMap$Node;ILjava/util/concurrent/ConcurrentHashMap$Node;Ljava/util/concurrent/ConcurrentHashMap$Node;)Z",
        native_chm_cas_tab_at, cratonvm_native_api::NativeKind::Bridge,
    );
}

/// Validate that `tab` is a non-null reference array and that `i` is a
/// legal index. On failure we return `None` for tabAt/setTabAt and `false`
/// for casTabAt — mirroring the behavior of `Unsafe.getObjectAcquire` on
/// an out-of-range offset, which is undefined in HotSpot and treated as a
/// soft error in our runtime rather than a panic.
pub fn chm_tab_index(
    ctx: &dyn NativeContext,
    args: &[Value],
) -> Option<(cratonvm_types::ObjectRef, usize)> {
    let tab = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return None,
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => return None,
    };
    if idx >= ctx.array_length(tab) {
        return None;
    }
    Some((tab, idx))
}

pub fn native_chm_tab_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (tab, i) = match chm_tab_index(ctx, args) {
        Some(v) => v,
        None => return Ok(Some(Value::Object(None))),
    };
    ctx.monitor_enter(tab);
    let v = ctx.get_array_element(tab, i);
    ctx.monitor_exit(tab);
    Ok(Some(v))
}

pub fn native_chm_set_tab_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (tab, i) = match chm_tab_index(ctx, args) {
        Some(v) => v,
        None => return Ok(None),
    };
    let new_val = args.get(2).copied().unwrap_or(Value::Object(None));
    ctx.monitor_enter(tab);
    ctx.set_array_element(tab, i, new_val);
    ctx.monitor_exit(tab);
    Ok(None)
}

pub fn native_chm_cas_tab_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (tab, i) = match chm_tab_index(ctx, args) {
        Some(v) => v,
        None => return Ok(Some(Value::Int(0))),
    };
    let expect = args.get(2).copied().unwrap_or(Value::Object(None));
    let new_val = args.get(3).copied().unwrap_or(Value::Object(None));
    ctx.monitor_enter(tab);
    let current = ctx.get_array_element(tab, i);
    let ok = values_ref_equal(&current, &expect);
    if ok {
        ctx.set_array_element(tab, i, new_val);
    }
    ctx.monitor_exit(tab);
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

/// Pointer-equality compare for Value::Object — CAS must match by
/// identity, not by equals(). Primitive values are compared bitwise.
fn values_ref_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(l), Value::Object(r)) => l == r,
        _ => a == b,
    }
}

// ---------------------------------------------------------------------------
// T2.3.3 — ArrayList.elementData(int)
// ---------------------------------------------------------------------------
//
// `elementData(int)` is a package-private accessor OpenJDK uses internally
// from `ArrayList.Itr.next()`, `Spliterator` paths, and `List.copyOf`. The
// method returns the backing array element without a range check (callers
// promise they've already validated the index). We still bounds-check
// defensively — an out-of-range index would otherwise read undefined heap
// slots.
//
// ArrayList layout (see class_manager.rs): field 0 = `Object[] elementData`,
// field 1 = `int size`.

pub const AL_FIELD_DATA: usize = 0;

pub fn register_arraylist_element_data(r: &mut NativeMethodRegistry) {
    r.register(
        "java/util/ArrayList",
        "elementData",
        "(I)Ljava/lang/Object;",
        native_arraylist_element_data, cratonvm_native_api::NativeKind::Bridge,
    );
}

pub fn native_arraylist_element_data(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let data = match ctx.get_field(this, AL_FIELD_DATA) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => {
            let bad = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => -1,
            };
            return Err(cratonvm_types::error::RuntimeError::aioobe_index_only(bad).into());
        }
    };
    // `elementData[index]` (`aaload`): the array access's text (wave 100 L7).
    let data_len = ctx.array_length(data);
    if idx >= data_len {
        return Err(cratonvm_types::error::RuntimeError::aioobe(
            i32::try_from(idx).unwrap_or(i32::MAX),
            i32::try_from(data_len).unwrap_or(i32::MAX),
        )
        .into());
    }
    Ok(Some(ctx.get_array_element(data, idx)))}

// ---------------------------------------------------------------------------
// T2.3.6 — Arrays.parallelSort
// ---------------------------------------------------------------------------
//
// OpenJDK specifies `Arrays.parallelSort` as "The sorting algorithm is a
// parallel sort-merge that breaks the array into sub-arrays [...] When
// the sub-array length reaches a minimum granularity, the sub-array is
// sorted using the appropriate Arrays.sort method." It does not mandate
// that the work actually run in parallel — a single-threaded
// implementation is spec-compliant, and indeed faster for the small
// array sizes typical of our workloads.
//
// We sort in-place using Rust's stable sort for object arrays (to
// preserve the documented *stable* ordering guarantee) and
// `sort_unstable` for primitive arrays (which is both correct, since
// primitive equality is total, and faster).
//
// Error semantics:
//   - Null array  → NullPointerException
//   - Negative / OOB fromIndex, toIndex → IllegalArgumentException or
//     ArrayIndexOutOfBoundsException to match `Arrays.sort`.

pub fn register_arrays_parallel_sort(r: &mut NativeMethodRegistry) {
    let c = "java/util/Arrays";
    r.register(c, "parallelSort", "([I)V", native_parallel_sort_int, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "parallelSort", "([III)V", native_parallel_sort_int_range, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(c, "parallelSort", "([J)V", native_parallel_sort_long, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        c,
        "parallelSort",
        "([JII)V",
        native_parallel_sort_long_range, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(c, "parallelSort", "([D)V", native_parallel_sort_double, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        c,
        "parallelSort",
        "([DII)V",
        native_parallel_sort_double_range, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "parallelSort",
        "([Ljava/lang/Comparable;)V",
        native_parallel_sort_objects, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        c,
        "parallelSort",
        "([Ljava/lang/Comparable;II)V",
        native_parallel_sort_objects_range, cratonvm_native_api::NativeKind::Bridge,
    );
}

pub fn null_arr_npe() -> cratonvm_types::error::MethodCallFailed {
    cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into()
}

pub fn range_bounds(
    ctx: &dyn NativeContext,
    arr: cratonvm_types::ObjectRef,
    from: i32,
    to: i32,
) -> Result<(usize, usize), cratonvm_types::error::MethodCallFailed> {
    if from > to {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: format!("fromIndex({from}) > toIndex({to})"),
            }
            .into(),
        );
    }
    let len = ctx.array_length(arr);
    // `Arrays.rangeCheck`: `new ArrayIndexOutOfBoundsException(fromIndex)`
    // for a negative start, else `(toIndex)` for an end past the length (wave
    // 100 L7: the end case named `from`).
    if from < 0 {
        return Err(cratonvm_types::error::RuntimeError::aioobe_index_only(from).into());
    }
    if (to as usize) > len {
        return Err(cratonvm_types::error::RuntimeError::aioobe_index_only(to).into());
    }
    Ok((from as usize, to as usize))
}

pub fn native_parallel_sort_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Err(null_arr_npe()),
    };
    let len = ctx.array_length(arr);
    parallel_sort_int_slice(ctx, arr, 0, len);
    Ok(None)
}

pub fn native_parallel_sort_int_range(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Err(null_arr_npe()),
    };
    let from = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let to = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (from, to) = range_bounds(ctx, arr, from, to)?;
    parallel_sort_int_slice(ctx, arr, from, to);
    Ok(None)
}

pub fn parallel_sort_int_slice(
    ctx: &mut dyn NativeContext,
    arr: cratonvm_types::ObjectRef,
    from: usize,
    to: usize,
) {
    let mut buf: Vec<i32> = (from..to)
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Int(v) => v,
            _ => 0,
        })
        .collect();
    buf.sort_unstable();
    for (k, v) in buf.into_iter().enumerate() {
        ctx.set_array_element(arr, from + k, Value::Int(v));
    }
}

pub fn native_parallel_sort_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Err(null_arr_npe()),
    };
    let len = ctx.array_length(arr);
    parallel_sort_long_slice(ctx, arr, 0, len);
    Ok(None)
}

pub fn native_parallel_sort_long_range(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Err(null_arr_npe()),
    };
    let from = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let to = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (from, to) = range_bounds(ctx, arr, from, to)?;
    parallel_sort_long_slice(ctx, arr, from, to);
    Ok(None)
}

pub fn parallel_sort_long_slice(
    ctx: &mut dyn NativeContext,
    arr: cratonvm_types::ObjectRef,
    from: usize,
    to: usize,
) {
    let mut buf: Vec<i64> = (from..to)
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Long(v) => v,
            _ => 0,
        })
        .collect();
    buf.sort_unstable();
    for (k, v) in buf.into_iter().enumerate() {
        ctx.set_array_element(arr, from + k, Value::Long(v));
    }
}

pub fn native_parallel_sort_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Err(null_arr_npe()),
    };
    let len = ctx.array_length(arr);
    parallel_sort_double_slice(ctx, arr, 0, len);
    Ok(None)
}

pub fn native_parallel_sort_double_range(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Err(null_arr_npe()),
    };
    let from = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let to = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (from, to) = range_bounds(ctx, arr, from, to)?;
    parallel_sort_double_slice(ctx, arr, from, to);
    Ok(None)
}

pub fn parallel_sort_double_slice(
    ctx: &mut dyn NativeContext,
    arr: cratonvm_types::ObjectRef,
    from: usize,
    to: usize,
) {
    let mut buf: Vec<f64> = (from..to)
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Double(v) => v,
            _ => 0.0,
        })
        .collect();
    // `Arrays.sort` for doubles uses `Double.compare`, which gives NaN > +inf
    // (all NaNs sort to the end) and distinguishes +0 / -0. `f64::total_cmp`
    // does NOT implement that ordering, whatever this comment used to claim: it
    // is IEEE totalOrder, which sorts a negatively-signed NaN BELOW -infinity.
    // `Math.sqrt(-1.0)` is `0xfff8…` on x86, so an array holding one came back
    // with that element at the FRONT instead of the end.
    buf.sort_by(|a, b| cratonvm_types::jfp::double_ordering(*a, *b));
    for (k, v) in buf.into_iter().enumerate() {
        ctx.set_array_element(arr, from + k, Value::Double(v));
    }
}

pub fn native_parallel_sort_objects(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Err(null_arr_npe()),
    };
    let len = ctx.array_length(arr);
    parallel_sort_objects_slice(ctx, arr, 0, len)?;
    Ok(None)
}

pub fn native_parallel_sort_objects_range(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Err(null_arr_npe()),
    };
    let from = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let to = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let (from, to) = range_bounds(ctx, arr, from, to)?;
    parallel_sort_objects_slice(ctx, arr, from, to)?;
    Ok(None)
}

/// Stable sort over the object sub-slice using `Comparable.compareTo`.
/// The callback invokes the Java method via the NativeContext — so
/// this is a real polymorphic dispatch, not just pointer comparison.
pub fn parallel_sort_objects_slice(
    ctx: &mut dyn NativeContext,
    arr: cratonvm_types::ObjectRef,
    from: usize,
    to: usize,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    if to <= from + 1 {
        return Ok(());
    }
    // Snapshot references out of the heap array into a local Vec so we
    // can sort without aliasing the heap mid-compare. We preserve nulls
    // and sort them to the front — mirroring OpenJDK's behavior, which
    // throws NPE if any null is encountered. We replicate that: any
    // null element in the slice is an error.
    //
    // gc-common w20-b: the snapshot is of HANDLES. It held raw addresses
    // across every `compareTo` (Java), so after a moving collection the sort
    // compared -- and finally stored back into the array -- stale copies;
    // the array itself was written at its entry address.
    let mut scope = NativeHandleScope::new(ctx);
    let arr_h = scope.root(arr);
    let mut buf: Vec<NativeHandle> = Vec::with_capacity(to - from);
    for i in from..to {
        match scope.get_array_element(arr, i) {
            Value::Object(Some(r)) => buf.push(scope.root(r)),
            _ => {
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: Some("parallelSort: null element".to_string()),
                }
                .into());
            }
        }
    }

    // Insertion sort — O(n²) but *stable* and trivially correct. For
    // our workload typical array sizes are <256 elements, so the big-O
    // difference vs. merge sort is negligible and the simplicity is
    // worth it. OpenJDK's Arrays.sort itself falls back to insertion
    // sort below 47 elements.
    for i in 1..buf.len() {
        let mut j = i;
        while j > 0 {
            let a = scope.get(&buf[j - 1]);
            let b = scope.get(&buf[j]);
            let cmp = scope
                .invoke_virtual(
                    a,
                    "compareTo",
                    "(Ljava/lang/Object;)I",
                    &[Value::Object(Some(b))],
                )?
                .unwrap_or(Value::Int(0));
            let c = match cmp {
                Value::Int(v) => v,
                _ => 0,
            };
            if c > 0 {
                buf.swap(j - 1, j);
                j -= 1;
            } else {
                break;
            }
        }
    }
    let arr = scope.get(&arr_h);
    for (k, h) in buf.iter().enumerate() {
        let r = scope.get(h);
        scope.set_array_element(arr, from + k, Value::Object(Some(r)));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// T2.3.8 — Spliterator$OfInt / OfLong / OfDouble
// ---------------------------------------------------------------------------
//
// Spliterator primitive specializations over an int[]/long[]/double[] backing
// array. Field layout matches `class_manager.rs` additions:
//   field 0 = backing array (Object[] | int[] | long[] | double[])
//   field 1 = cursor Int (next index to emit)
//
// We register the 5 user-observable methods per specialization:
//   tryAdvance(consumer) — consume one element, return true iff advanced
//   forEachRemaining(consumer) — drain to end
//   estimateSize() — length - cursor
//   characteristics() — ORDERED | SIZED | SUBSIZED | NONNULL | IMMUTABLE
//   trySplit() — returns null (we always return the sequential root);
//                that is spec-compliant per `Spliterator.trySplit` javadoc.

pub const SPL_FIELD_DATA: usize = 0;
pub const SPL_FIELD_CURSOR: usize = 1;
/// ORDERED | SIZED | SUBSIZED | NONNULL | IMMUTABLE for primitive array
/// spliterators — matches `Spliterators.IntArraySpliterator`.
pub const SPL_PRIM_CHARS: i32 = 0x10 | 0x40 | 0x4000 | 0x100 | 0x400;

pub fn register_spliterator_primitive_natives(r: &mut NativeMethodRegistry) {
    register_spliterator_primitive_one(
        r,
        "java/util/Spliterator$OfInt",
        "(Ljava/util/function/IntConsumer;)Z",
        "(Ljava/util/function/IntConsumer;)V",
        "()Ljava/util/Spliterator$OfInt;",
        native_spl_int_try_advance,
        native_spl_int_for_each,
        native_spl_int_try_split,
    );
    register_spliterator_primitive_one(
        r,
        "java/util/Spliterator$OfLong",
        "(Ljava/util/function/LongConsumer;)Z",
        "(Ljava/util/function/LongConsumer;)V",
        "()Ljava/util/Spliterator$OfLong;",
        native_spl_long_try_advance,
        native_spl_long_for_each,
        native_spl_long_try_split,
    );
    register_spliterator_primitive_one(
        r,
        "java/util/Spliterator$OfDouble",
        "(Ljava/util/function/DoubleConsumer;)Z",
        "(Ljava/util/function/DoubleConsumer;)V",
        "()Ljava/util/Spliterator$OfDouble;",
        native_spl_double_try_advance,
        native_spl_double_for_each,
        native_spl_double_try_split,
    );
}

#[allow(clippy::too_many_arguments)]
pub fn register_spliterator_primitive_one(
    r: &mut NativeMethodRegistry,
    class: &str,
    try_advance_desc: &str,
    for_each_desc: &str,
    try_split_desc: &str,
    try_advance: cratonvm_native_api::NativeCallback,
    for_each: cratonvm_native_api::NativeCallback,
    try_split: cratonvm_native_api::NativeCallback,
) {
    r.register(class, "tryAdvance", try_advance_desc, try_advance, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(class, "forEachRemaining", for_each_desc, for_each, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(class, "trySplit", try_split_desc, try_split, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(class, "estimateSize", "()J", native_spl_estimate_size, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        class,
        "getExactSizeIfKnown",
        "()J",
        native_spl_estimate_size, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(class, "characteristics", "()I", native_spl_characteristics, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        class,
        "hasCharacteristics",
        "(I)Z",
        native_spl_has_characteristics, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        class,
        "getComparator",
        "()Ljava/util/Comparator;",
        native_spl_get_comparator_null, cratonvm_native_api::NativeKind::SyntheticStub,
    );
}

pub fn native_spl_estimate_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Long(spl_remaining(ctx, this) as i64)))
}

pub fn native_spl_characteristics(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(SPL_PRIM_CHARS)))
}

pub fn native_spl_has_characteristics(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mask = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    Ok(Some(Value::Int(if (SPL_PRIM_CHARS & mask) == mask {
        1
    } else {
        0
    })))
}

/// Primitive spliterators are not SORTED — spec says `getComparator`
/// must throw IllegalStateException. We return null as a graceful
/// degradation consistent with our other Spliterator natives.
pub fn native_spl_get_comparator_null(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

// trySplit — null return is always spec-compliant per
// `Spliterator.trySplit` javadoc.
pub fn native_spl_int_try_split(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}
fn native_spl_long_try_split(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}
fn native_spl_double_try_split(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

pub fn native_spl_int_try_advance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    spl_prim_try_advance(ctx, args, SplPrim::Int, "(I)V")
}
fn native_spl_int_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    spl_prim_for_each_remaining(ctx, args, SplPrim::Int, "(I)V")
}
fn native_spl_long_try_advance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    spl_prim_try_advance(ctx, args, SplPrim::Long, "(J)V")
}
fn native_spl_long_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    spl_prim_for_each_remaining(ctx, args, SplPrim::Long, "(J)V")
}
fn native_spl_double_try_advance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    spl_prim_try_advance(ctx, args, SplPrim::Double, "(D)V")
}
fn native_spl_double_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    spl_prim_for_each_remaining(ctx, args, SplPrim::Double, "(D)V")
}

#[derive(Copy, Clone)]
pub enum SplPrim {
    Int,
    Long,
    Double,
}

pub fn spl_remaining(ctx: &dyn NativeContext, this: cratonvm_types::ObjectRef) -> usize {
    let cursor = match ctx.get_field(this, SPL_FIELD_CURSOR) {
        Value::Int(v) => v.max(0) as usize,
        _ => 0,
    };
    let data = match ctx.get_field(this, SPL_FIELD_DATA) {
        Value::Object(Some(a)) => a,
        _ => return 0,
    };
    ctx.array_length(data).saturating_sub(cursor)
}

pub fn spl_prim_element_value(raw: Value, prim: SplPrim) -> Value {
    // Normalize whatever is in the backing array into the typed value
    // the primitive consumer expects. Our synthetic arrays may store
    // ints in a reference array (legacy paths) or in a typed primitive
    // array. Both must project to the same widened value.
    match prim {
        SplPrim::Int => match raw {
            Value::Int(v) => Value::Int(v),
            Value::Long(v) => Value::Int(v as i32),
            Value::Double(v) => Value::Int(v as i32),
            _ => Value::Int(0),
        },
        SplPrim::Long => match raw {
            Value::Long(v) => Value::Long(v),
            Value::Int(v) => Value::Long(v as i64),
            Value::Double(v) => Value::Long(v as i64),
            _ => Value::Long(0),
        },
        SplPrim::Double => match raw {
            Value::Double(v) => Value::Double(v),
            Value::Int(v) => Value::Double(v as f64),
            Value::Long(v) => Value::Double(v as f64),
            _ => Value::Double(0.0),
        },
    }
}

pub fn spl_prim_try_advance(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    prim: SplPrim,
    accept_desc: &str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let consumer = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(Some(Value::Int(0))),
    };
    let data = match ctx.get_field(this, SPL_FIELD_DATA) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cursor = match ctx.get_field(this, SPL_FIELD_CURSOR) {
        Value::Int(v) => v.max(0) as usize,
        _ => 0,
    };
    if cursor >= ctx.array_length(data) {
        return Ok(Some(Value::Int(0)));
    }
    let raw = ctx.get_array_element(data, cursor);
    let val = spl_prim_element_value(raw, prim);
    ctx.set_field(this, SPL_FIELD_CURSOR, Value::Int((cursor + 1) as i32));
    ctx.invoke_virtual(consumer, "accept", accept_desc, &[val])?;
    Ok(Some(Value::Int(1)))
}

pub fn spl_prim_for_each_remaining(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    prim: SplPrim,
    accept_desc: &str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let consumer = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(None),
    };
    let data = match ctx.get_field(this, SPL_FIELD_DATA) {
        Value::Object(Some(a)) => a,
        _ => return Ok(None),
    };
    let cursor = match ctx.get_field(this, SPL_FIELD_CURSOR) {
        Value::Int(v) => v.max(0) as usize,
        _ => 0,
    };
    let len = ctx.array_length(data);
    // gc-common w20-b: the data array was read, and the cursor stored into
    // `this`, at their entry addresses after every `accept` (Java). Both are
    // pinned with the consumer now (one unpin from `this_pin` drops all
    // three), and the pins are released on the exception path too.
    let this_pin = ctx.pin_native_root(this);
    let data_pin = ctx.pin_native_root(data);
    let consumer_pin = ctx.pin_native_root(consumer);
    for i in cursor..len {
        let consumer = ctx.read_native_pin(consumer_pin, consumer);
        let data = ctx.read_native_pin(data_pin, data);
        let raw = ctx.get_array_element(data, i);
        let val = spl_prim_element_value(raw, prim);
        if let Err(e) = ctx.invoke_virtual(consumer, "accept", accept_desc, &[val]) {
            ctx.unpin_native_roots(this_pin);
            return Err(e);
        }
    }
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    ctx.set_field(this, SPL_FIELD_CURSOR, Value::Int(len as i32));
    Ok(None)
}

