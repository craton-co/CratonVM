// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! HTTP/2 Java client native method implementations.
//!
//! Provides java.net.http.HttpClient, HttpRequest, HttpResponse,
//! HttpHeaders, HttpRequest$BodyPublisher, HttpResponse$BodyHandlers,
//! and WebSocket support for the CratonVM native layer.
//!
//! Implements Java HTTP Client API (java.net.http) introduced in Java 11,
//! with HTTP/2 (RFC 7540) and HPACK header compression (RFC 7541) stubs.

use crate::{obj_arg, try_alloc_concurrent_synthetic};
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::RuntimeError;
use cratonvm_types::{ObjectRef, Value};

use cratonvm_types::error::MethodCallFailed;
use std::io::Write;
use std::net::TcpStream;

// ---------------------------------------------------------------------------
// HTTP/2 frame types per RFC 7540
// ---------------------------------------------------------------------------

/// HTTP/2 frame types per RFC 7540
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum Http2FrameType {
    Data = 0x0,
    Headers = 0x1,
    Priority = 0x2,
    RstStream = 0x3,
    Settings = 0x4,
    PushPromise = 0x5,
    Ping = 0x6,
    Goaway = 0x7,
    WindowUpdate = 0x8,
    Continuation = 0x9,
}

/// HTTP/2 error codes per RFC 7540
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u32)]
pub enum Http2ErrorCode {
    NoError = 0x0,
    ProtocolError = 0x1,
    InternalError = 0x2,
    FlowControlError = 0x3,
    SettingsTimeout = 0x4,
    StreamClosed = 0x5,
    FrameSizeError = 0x6,
    RefusedStream = 0x7,
    Cancel = 0x8,
    CompressionError = 0x9,
    ConnectError = 0xa,
    EnhanceYourCalm = 0xb,
    InadequateSecurity = 0xc,
    Http11Required = 0xd,
}

// ---------------------------------------------------------------------------
// HPACK static table (RFC 7541 Appendix A)
// ---------------------------------------------------------------------------

/// HPACK static table entry (name, value).
/// First 61 entries are the static table defined in RFC 7541 Appendix A.
pub struct HpackStaticTable;

impl HpackStaticTable {
    pub const ENTRIES: &'static [(&'static str, &'static str)] = &[
        (":authority", ""),
        (":method", "GET"),
        (":method", "POST"),
        (":path", "/"),
        (":path", "/index.html"),
        (":scheme", "http"),
        (":scheme", "https"),
        (":status", "200"),
        (":status", "204"),
        (":status", "206"),
        (":status", "304"),
        (":status", "400"),
        (":status", "404"),
        (":status", "500"),
        ("accept-charset", ""),
        ("accept-encoding", "gzip, deflate"),
        ("accept-language", ""),
        ("accept-ranges", ""),
        ("accept", ""),
        ("access-control-allow-origin", ""),
        ("age", ""),
        ("allow", ""),
        ("authorization", ""),
        ("cache-control", ""),
        ("content-disposition", ""),
        ("content-encoding", ""),
        ("content-language", ""),
        ("content-length", ""),
        ("content-location", ""),
        ("content-range", ""),
        ("content-type", ""),
        ("cookie", ""),
        ("date", ""),
        ("etag", ""),
        ("expect", ""),
        ("expires", ""),
        ("from", ""),
        ("host", ""),
        ("if-match", ""),
        ("if-modified-since", ""),
        ("if-none-match", ""),
        ("if-range", ""),
        ("if-unmodified-since", ""),
        ("last-modified", ""),
        ("link", ""),
        ("location", ""),
        ("max-forwards", ""),
        ("proxy-authenticate", ""),
        ("proxy-authorization", ""),
        ("range", ""),
        ("referer", ""),
        ("refresh", ""),
        ("retry-after", ""),
        ("server", ""),
        ("set-cookie", ""),
        ("strict-transport-security", ""),
        ("transfer-encoding", ""),
        ("user-agent", ""),
        ("vary", ""),
        ("via", ""),
        ("www-authenticate", ""),
    ];

    /// Find a static table index by header name (and optional value).
    /// Returns 1-based index per the HPACK spec, or None if not found.
    pub fn find(name: &str, value: Option<&str>) -> Option<usize> {
        for (i, &(n, v)) in Self::ENTRIES.iter().enumerate() {
            if n == name {
                if let Some(val) = value {
                    if v == val {
                        return Some(i + 1);
                    }
                } else {
                    return Some(i + 1);
                }
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// HPACK Huffman decoding (RFC 7541 Appendix B)
// ---------------------------------------------------------------------------

/// RFC 7541 Appendix B Huffman code table. One entry per symbol (0..=255 then
/// the EOS at index 256). Each entry is `(code, bit_length)` where `code` is
/// right-aligned in a `u32`. This is the canonical static table from the RFC;
/// it is read-only and used only on the response decode path.
const HPACK_HUFFMAN_TABLE: &[(u32, u8)] = &[
    (0x1ff8, 13),
    (0x7fffd8, 23),
    (0xfffffe2, 28),
    (0xfffffe3, 28),
    (0xfffffe4, 28),
    (0xfffffe5, 28),
    (0xfffffe6, 28),
    (0xfffffe7, 28),
    (0xfffffe8, 28),
    (0xffffea, 24),
    (0x3ffffffc, 30),
    (0xfffffe9, 28),
    (0xfffffea, 28),
    (0x3ffffffd, 30),
    (0xfffffeb, 28),
    (0xfffffec, 28),
    (0xfffffed, 28),
    (0xfffffee, 28),
    (0xfffffef, 28),
    (0xffffff0, 28),
    (0xffffff1, 28),
    (0xffffff2, 28),
    (0x3ffffffe, 30),
    (0xffffff3, 28),
    (0xffffff4, 28),
    (0xffffff5, 28),
    (0xffffff6, 28),
    (0xffffff7, 28),
    (0xffffff8, 28),
    (0xffffff9, 28),
    (0xffffffa, 28),
    (0xffffffb, 28),
    (0x14, 6),
    (0x3f8, 10),
    (0x3f9, 10),
    (0xffa, 12),
    (0x1ff9, 13),
    (0x15, 6),
    (0xf8, 8),
    (0x7fa, 11),
    (0x3fa, 10),
    (0x3fb, 10),
    (0xf9, 8),
    (0x7fb, 11),
    (0xfa, 8),
    (0x16, 6),
    (0x17, 6),
    (0x18, 6),
    (0x0, 5),
    (0x1, 5),
    (0x2, 5),
    (0x19, 6),
    (0x1a, 6),
    (0x1b, 6),
    (0x1c, 6),
    (0x1d, 6),
    (0x1e, 6),
    (0x1f, 6),
    (0x5c, 7),
    (0xfb, 8),
    (0x7ffc, 15),
    (0x20, 6),
    (0xffb, 12),
    (0x3fc, 10),
    (0x1ffa, 13),
    (0x21, 6),
    (0x5d, 7),
    (0x5e, 7),
    (0x5f, 7),
    (0x60, 7),
    (0x61, 7),
    (0x62, 7),
    (0x63, 7),
    (0x64, 7),
    (0x65, 7),
    (0x66, 7),
    (0x67, 7),
    (0x68, 7),
    (0x69, 7),
    (0x6a, 7),
    (0x6b, 7),
    (0x6c, 7),
    (0x6d, 7),
    (0x6e, 7),
    (0x6f, 7),
    (0x70, 7),
    (0x71, 7),
    (0x72, 7),
    (0xfc, 8),
    (0x73, 7),
    (0xfd, 8),
    (0x1ffb, 13),
    (0x7fff0, 19),
    (0x1ffc, 13),
    (0x3ffc, 14),
    (0x22, 6),
    (0x7ffd, 15),
    (0x3, 5),
    (0x23, 6),
    (0x4, 5),
    (0x24, 6),
    (0x5, 5),
    (0x25, 6),
    (0x26, 6),
    (0x27, 6),
    (0x6, 5),
    (0x74, 7),
    (0x75, 7),
    (0x28, 6),
    (0x29, 6),
    (0x2a, 6),
    (0x7, 5),
    (0x2b, 6),
    (0x76, 7),
    (0x2c, 6),
    (0x8, 5),
    (0x9, 5),
    (0x2d, 6),
    (0x77, 7),
    (0x78, 7),
    (0x79, 7),
    (0x7a, 7),
    (0x7b, 7),
    (0x7ffe, 15),
    (0x7fc, 11),
    (0x3ffd, 14),
    (0x1ffd, 13),
    (0xffffffc, 28),
    (0xfffe6, 20),
    (0x3fffd2, 22),
    (0xfffe7, 20),
    (0xfffe8, 20),
    (0x3fffd3, 22),
    (0x3fffd4, 22),
    (0x3fffd5, 22),
    (0x7fffd9, 23),
    (0x3fffd6, 22),
    (0x7fffda, 23),
    (0x7fffdb, 23),
    (0x7fffdc, 23),
    (0x7fffdd, 23),
    (0x7fffde, 23),
    (0xffffeb, 24),
    (0x7fffdf, 23),
    (0xffffec, 24),
    (0xffffed, 24),
    (0x3fffd7, 22),
    (0x7fffe0, 23),
    (0xffffee, 24),
    (0x7fffe1, 23),
    (0x7fffe2, 23),
    (0x7fffe3, 23),
    (0x7fffe4, 23),
    (0x1fffdc, 21),
    (0x3fffd8, 22),
    (0x7fffe5, 23),
    (0x3fffd9, 22),
    (0x7fffe6, 23),
    (0x7fffe7, 23),
    (0xffffef, 24),
    (0x3fffda, 22),
    (0x1fffdd, 21),
    (0xfffe9, 20),
    (0x3fffdb, 22),
    (0x3fffdc, 22),
    (0x7fffe8, 23),
    (0x7fffe9, 23),
    (0x1fffde, 21),
    (0x7fffea, 23),
    (0x3fffdd, 22),
    (0x3fffde, 22),
    (0xfffff0, 24),
    (0x1fffdf, 21),
    (0x3fffdf, 22),
    (0x7fffeb, 23),
    (0x7fffec, 23),
    (0x1fffe0, 21),
    (0x1fffe1, 21),
    (0x3fffe0, 22),
    (0x1fffe2, 21),
    (0x7fffed, 23),
    (0x3fffe1, 22),
    (0x7fffee, 23),
    (0x7fffef, 23),
    (0xfffea, 20),
    (0x3fffe2, 22),
    (0x3fffe3, 22),
    (0x3fffe4, 22),
    (0x7ffff0, 23),
    (0x3fffe5, 22),
    (0x3fffe6, 22),
    (0x7ffff1, 23),
    (0x3ffffe0, 26),
    (0x3ffffe1, 26),
    (0xfffeb, 20),
    (0x7fff1, 19),
    (0x3fffe7, 22),
    (0x7ffff2, 23),
    (0x3fffe8, 22),
    (0x1ffffec, 25),
    (0x3ffffe2, 26),
    (0x3ffffe3, 26),
    (0x3ffffe4, 26),
    (0x7ffffde, 27),
    (0x7ffffdf, 27),
    (0x3ffffe5, 26),
    (0xfffff1, 24),
    (0x1ffffed, 25),
    (0x7fff2, 19),
    (0x1fffe3, 21),
    (0x3ffffe6, 26),
    (0x7ffffe0, 27),
    (0x7ffffe1, 27),
    (0x3ffffe7, 26),
    (0x7ffffe2, 27),
    (0xfffff2, 24),
    (0x1fffe4, 21),
    (0x1fffe5, 21),
    (0x3ffffe8, 26),
    (0x3ffffe9, 26),
    (0xffffffd, 28),
    (0x7ffffe3, 27),
    (0x7ffffe4, 27),
    (0x7ffffe5, 27),
    (0xfffec, 20),
    (0xfffff3, 24),
    (0xfffed, 20),
    (0x1fffe6, 21),
    (0x3fffe9, 22),
    (0x1fffe7, 21),
    (0x1fffe8, 21),
    (0x7ffff3, 23),
    (0x3fffea, 22),
    (0x3fffeb, 22),
    (0x1ffffee, 25),
    (0x1ffffef, 25),
    (0xfffff4, 24),
    (0xfffff5, 24),
    (0x3ffffea, 26),
    (0x7ffff4, 23),
    (0x3ffffeb, 26),
    (0x7ffffe6, 27),
    (0x3ffffec, 26),
    (0x3ffffed, 26),
    (0x7ffffe7, 27),
    (0x7ffffe8, 27),
    (0x7ffffe9, 27),
    (0x7ffffea, 27),
    (0x7ffffeb, 27),
    (0xffffffe, 28),
    (0x7ffffec, 27),
    (0x7ffffed, 27),
    (0x7ffffee, 27),
    (0x7ffffef, 27),
    (0x7fffff0, 27),
    (0x3ffffee, 26),
    (0x3fffffff, 30), // EOS (index 256)
];

/// Decode an HPACK Huffman-coded byte slice (RFC 7541 §5.2 / Appendix B).
///
/// Fails closed: returns `Err` for an over-long final padding (more than 7
/// bits, or padding not all-ones), for an explicit EOS symbol in the stream,
/// or for any code that cannot be matched — never silently returns wrong or
/// partial data.
pub fn hpack_huffman_decode(input: &[u8]) -> Result<Vec<u8>, String> {
    // Bound the output: each input byte is at least the 5-bit minimum code, so
    // the decoded length never exceeds input_len * 8 / 5 + 1. This keeps the
    // allocation proportional to the (already frame-bounded) input.
    let mut out = Vec::with_capacity(input.len().saturating_mul(8) / 5 + 1);
    let mut acc: u64 = 0; // right-aligned bit accumulator
    let mut nbits: u32 = 0; // number of valid bits currently in `acc`
    for &byte in input {
        acc = (acc << 8) | byte as u64;
        nbits += 8;
        // Greedily match the shortest code at the front of `acc`.
        while nbits >= 5 {
            let mut matched = false;
            // Codes are 5..=30 bits; try increasing lengths.
            let max_len = nbits.min(30);
            for len in 5..=max_len {
                let code = ((acc >> (nbits - len)) & ((1u64 << len) - 1)) as u32;
                if let Some(sym) = huffman_lookup(code, len as u8) {
                    if sym == 256 {
                        return Err("hpack huffman: EOS symbol in stream".into());
                    }
                    out.push(sym as u8);
                    nbits -= len;
                    matched = true;
                    break;
                }
            }
            if !matched {
                break;
            }
        }
    }
    // Per RFC 7541 §5.2, any leftover bits must be the most-significant bits of
    // the EOS code, i.e. fewer than 8 bits, all set to 1.
    if nbits >= 8 {
        return Err("hpack huffman: incomplete trailing code".into());
    }
    if nbits > 0 {
        let pad = (acc & ((1u64 << nbits) - 1)) as u32;
        let all_ones = (1u32 << nbits) - 1;
        if pad != all_ones {
            return Err("hpack huffman: invalid padding".into());
        }
    }
    Ok(out)
}

/// Linear lookup of a `len`-bit code against the Huffman table. Linear scan is
/// acceptable: the table is 257 entries and only header strings flow through it.
fn huffman_lookup(code: u32, len: u8) -> Option<u16> {
    for (sym, &(c, l)) in HPACK_HUFFMAN_TABLE.iter().enumerate() {
        if l == len && c == code {
            return Some(sym as u16);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Version / redirect policy constants
// ---------------------------------------------------------------------------

// These are ENUM ORDINALS, not private encodings: `version()` and
// `followRedirects()` hand back real `HttpClient$Version` / `HttpClient$Redirect`
// objects built by `p57_alloc_enum`, and the `HTTP_1_1` / `NEVER` / ... statics
// registered below mint the same ones. Declaration order in the JDK is
// `HTTP_1_1, HTTP_2` and `NEVER, ALWAYS, NORMAL`; NEVER, ALWAYS and NORMAL used
// to be 0/2/1 here, which disagreed with the `phases_late::net_channels`
// registrar that owns those statics — so `client.followRedirects() ==
// HttpClient.Redirect.ALWAYS` compared two different ordinals.
const HTTP_VERSION_1_1: i32 = 0;
const HTTP_VERSION_2: i32 = 1;

const REDIRECT_NEVER: i32 = 0;
const REDIRECT_ALWAYS: i32 = 1;
const REDIRECT_NORMAL: i32 = 2;

// Default connection pool size
const DEFAULT_POOL_SIZE: i32 = 20;

// HTTP method indices
const METHOD_GET: i32 = 0;
const METHOD_POST: i32 = 1;
const METHOD_PUT: i32 = 2;
const METHOD_DELETE: i32 = 3;
const METHOD_HEAD: i32 = 4;
const METHOD_PATCH: i32 = 5;
const METHOD_OPTIONS: i32 = 6;

// WebSocket states
const WS_OPEN: i32 = 0;
const WS_CLOSING: i32 = 1;
const WS_CLOSED: i32 = 2;

// ---------------------------------------------------------------------------
// Field-index constants
// ---------------------------------------------------------------------------

// HttpClient fields
const CLIENT_VERSION: usize = 0;
const CLIENT_REDIRECT: usize = 1;
const CLIENT_CONNECT_TIMEOUT: usize = 2;
const CLIENT_HAS_SSL: usize = 3;
const CLIENT_HAS_EXECUTOR: usize = 4;
const CLIENT_HAS_PROXY: usize = 5;
const CLIENT_HAS_AUTH: usize = 6;
const CLIENT_HAS_COOKIE: usize = 7;
const CLIENT_FOLLOW_REDIR: usize = 8;
const CLIENT_POOL_SIZE: usize = 9;

// HttpRequest fields
const REQ_METHOD: usize = 0;
const REQ_URI: usize = 1;
const REQ_HAS_BODY: usize = 2;
const REQ_TIMEOUT: usize = 3;
const REQ_VERSION: usize = 4;
const REQ_EXPECT_100: usize = 5;
const REQ_HDR_COUNT: usize = 6;
const REQ_BODY_LEN: usize = 7;

// HttpResponse fields
const RESP_STATUS: usize = 0;
const RESP_VERSION: usize = 1;
const RESP_BODY_LEN: usize = 2;
const RESP_HAS_PREV: usize = 3;
const RESP_METHOD: usize = 4;
const RESP_HAS_SSL: usize = 5;
const RESP_BODY_OBJ: usize = 6;

// HttpHeaders fields
const HDR_COUNT: usize = 0;
const HDR_HAS_CT: usize = 1;
const HDR_HAS_CL: usize = 2;

// WebSocket fields
const WS_STATE: usize = 0;
const WS_SUBPROTOCOL: usize = 1;
const WS_OUTPUT_CLOSED: usize = 2;
const WS_INPUT_CLOSED: usize = 3;

// ---------------------------------------------------------------------------
// Allocation helpers
// ---------------------------------------------------------------------------

// Each `alloc_*` below is paired with an `init_*_fields` that establishes the
// object's starting state. The split exists because these classes are also
// reachable through their registered `<init>` native, and a receiver that
// arrives there has every slot at its untyped default — not the typed zero the
// accessors expect. Both entry points must produce the same object, so they
// share one initializer.

/// True when `obj`'s class IS `class_name` — i.e. it is the CratonVM synthetic
/// this file's slot indices describe, not a real JDK type that merely inherits
/// from it.
///
/// Every `<init>` native below has to ask this first. Native dispatch is keyed
/// on the DECLARING class of the resolved method, and `HttpClient` /
/// `HttpRequest` are abstract CLASSES in the real JDK — so a real
/// `jdk.internal.net.http.HttpClientImpl` running `super()` resolves to
/// `HttpClient.<init>()V` and lands here too, carrying its own field layout.
/// The indices these initializers write are meaningful only for the synthetic
/// shape; writing them into a real instance would clobber that instance's
/// fields. For the real types the JDK constructor body is empty anyway
/// (`protected HttpClient() {}`), so declining to write anything is exactly
/// the faithful behaviour.
fn is_synthetic_shape(ctx: &dyn NativeContext, obj: ObjectRef, class_name: &str) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(obj))
        .as_deref()
        == Some(class_name)
}

/// Is this `java/net/http/HttpHeaders` receiver one THIS file minted?
///
/// `is_synthetic_shape` cannot answer that question for this class, because
/// every candidate answers to the same class NAME. THREE different objects are
/// stamped `java.net.http.HttpHeaders` in a running CratonVM and they do not
/// share a layout:
///
/// | minted by | slots | slot 0 holds |
/// |---|---|---|
/// | `alloc_http_headers` (this file) | 3 | `Value::Int` — `HDR_COUNT` |
/// | `net_phase_e::re5_make_http_headers` | 1 | `Value::Object` — a `String[]` of `"k: v"` |
/// | the real JDK's `HttpHeaders.of(Map, BiPredicate)` | the real class's | `Value::Object` — a real `Map` |
///
/// So the discriminator is the KIND of slot 0, not the class name: only the
/// counter layout puts a primitive there. That is a property of the three
/// minters and not of a name list, which is what makes it hold when a fourth
/// minter appears.
///
/// This matters because `NativeMethodRegistry::register` is last-write-wins
/// with no unregister API. `net_phase_e::register_phase_e_networking` and
/// [`register_http2_natives`] both claim
/// `HttpHeaders.{map,firstValue,allValues,firstValueAsLong}`, and in a
/// `synthetic-jdk` build BOTH run (`lib.rs`: `register_essential_natives`
/// reaches phase E first, then `register_synthetic_overrides` reaches this
/// file), so this file's four bodies win the slots while `net_phase_e`'s
/// minter keeps producing `String[]`-shaped receivers for them. Without this
/// guard those bodies read `HDR_HAS_CT`/`HDR_HAS_CL` — slots 1 and 2 — off an
/// object that has ONE slot, and answer a fabricated `content-type` from
/// whatever they find. With it they decline, and a receiver this file did not
/// mint gets the absent answer instead of another object's memory.
///
/// MEASURED (2026-08-17, `C:/craton/target-rel2/release/cratonvm.exe`): under
/// `--real-jdk` this file's registrar does not run at all, so the guard is
/// inert there — every `java/net/http/HttpHeaders` row in
/// `--dump-native-registry` is `net_phase_e`'s with `overwrote=null`. The
/// guard is for the boot order this file IS on, and for the one it is one
/// call-site move away from.
fn http_headers_is_counter_shape(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    matches!(ctx.get_field(obj, HDR_COUNT), Value::Int(_))
}

fn alloc_http_client(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpClient", 10)?;
    init_http_client_fields(ctx, obj);
    Ok(obj)
}

fn init_http_client_fields(ctx: &mut dyn NativeContext, obj: ObjectRef) {
    ctx.set_field(obj, CLIENT_VERSION, Value::Int(HTTP_VERSION_2));
    ctx.set_field(obj, CLIENT_REDIRECT, Value::Int(REDIRECT_NEVER));
    ctx.set_field(obj, CLIENT_CONNECT_TIMEOUT, Value::Long(0));
    ctx.set_field(obj, CLIENT_HAS_SSL, Value::Int(0));
    ctx.set_field(obj, CLIENT_HAS_EXECUTOR, Value::Int(0));
    ctx.set_field(obj, CLIENT_HAS_PROXY, Value::Int(0));
    ctx.set_field(obj, CLIENT_HAS_AUTH, Value::Int(0));
    ctx.set_field(obj, CLIENT_HAS_COOKIE, Value::Int(0));
    ctx.set_field(obj, CLIENT_FOLLOW_REDIR, Value::Int(0));
    ctx.set_field(obj, CLIENT_POOL_SIZE, Value::Int(DEFAULT_POOL_SIZE));
}

fn alloc_http_client_builder(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpClient$Builder", 8)?;
    init_http_client_builder_fields(ctx, obj);
    Ok(obj)
}

/// A `Builder` accumulates into the same 8 slots `build()` later copies out
/// wholesale, so every one of them has to start at a defined value — a slot
/// the caller never touched is still read and copied into the client.
fn init_http_client_builder_fields(ctx: &mut dyn NativeContext, obj: ObjectRef) {
    ctx.set_field(obj, 0, Value::Int(HTTP_VERSION_2));
    ctx.set_field(obj, 1, Value::Int(REDIRECT_NEVER));
    ctx.set_field(obj, 2, Value::Long(0));
    ctx.set_field(obj, 3, Value::Int(0));
    ctx.set_field(obj, 4, Value::Int(0));
    ctx.set_field(obj, 5, Value::Int(0));
    ctx.set_field(obj, 6, Value::Int(0));
    ctx.set_field(obj, 7, Value::Int(0));
}

fn alloc_http_request(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest", 8)?;
    init_http_request_fields(ctx, obj);
    Ok(obj)
}

/// Shared by `HttpRequest` and `HttpRequest$Builder`: both carry the same 8
/// slots (the builder's `build()` copies them across one-for-one), so an
/// unset `REQ_TIMEOUT` / `REQ_BODY_LEN` would be copied into the request as a
/// non-`Long` and read back through the `()J` accessors.
fn init_http_request_fields(ctx: &mut dyn NativeContext, obj: ObjectRef) {
    ctx.set_field(obj, REQ_METHOD, Value::Int(METHOD_GET));
    ctx.set_field(obj, REQ_URI, Value::Object(None));
    ctx.set_field(obj, REQ_HAS_BODY, Value::Int(0));
    ctx.set_field(obj, REQ_TIMEOUT, Value::Long(0));
    ctx.set_field(obj, REQ_VERSION, Value::Int(0));
    ctx.set_field(obj, REQ_EXPECT_100, Value::Int(0));
    ctx.set_field(obj, REQ_HDR_COUNT, Value::Int(0));
    ctx.set_field(obj, REQ_BODY_LEN, Value::Long(0));
}

fn alloc_http_request_builder(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$Builder", 8)?;
    init_http_request_fields(ctx, obj);
    Ok(obj)
}

fn alloc_http_response(
    ctx: &mut dyn NativeContext,
    status: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse", 7)?;
    init_http_response_fields(ctx, obj, status);
    Ok(obj)
}

/// A response with `status` whose body String is `body`.
///
/// gc-common w16-f: the String first, rooted across the response's
/// allocation. Both `send` bodies allocated the response, then the String, and
/// stored the String through the response's pre-allocation address.
fn alloc_http_response_with_body(
    ctx: &mut dyn NativeContext,
    status: i32,
    body: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let body_str = ctx.create_string(body);
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let body_h = scope.root(body_str);
    let resp = alloc_http_response(&mut *scope, status)?;
    let body_str = scope.get(&body_h);
    scope.set_field(resp, RESP_BODY_OBJ, Value::Object(Some(body_str)));
    Ok(resp)
}

/// The synthetic completed `CompletableFuture` (slot 0 = value, slot 1 =
/// done) over `value`.
///
/// gc-common w16-f: `value` is rooted across the future's allocation. Every
/// copy of this sequence in this file -- the four in `http2_send_async`, the
/// five `WebSocket` sends -- stored the value through its address from before
/// the allocation.
fn completed_future_of(
    ctx: &mut dyn NativeContext,
    value: ObjectRef,
) -> cratonvm_types::error::MethodCallResult {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let value_h = scope.root(value);
    let cf: ObjectRef =
        try_alloc_concurrent_synthetic(&mut *scope, "java/util/concurrent/CompletableFuture", 2)?;
    let value = scope.get(&value_h);
    scope.set_field(cf, 0, Value::Object(Some(value)));
    scope.set_field(cf, 1, Value::Int(1)); // completed
    Ok(Some(Value::Object(Some(cf))))
}

/// A `java.util.Optional` over `value` (empty for `None`). Slot 0 is the
/// reference `value` -- see docs/feature-designs/native-dispatch-architecture.md.
///
/// gc-common w16-f: callers build the payload FIRST and this roots it across
/// the Optional's allocation. The sites it replaces allocated the Optional
/// first and stored a payload allocated after it through the Optional's
/// pre-allocation address.
fn optional_of(
    ctx: &mut dyn NativeContext,
    value: Option<ObjectRef>,
) -> Result<ObjectRef, MethodCallFailed> {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let value_h = value.map(|v| scope.root(v));
    let opt: ObjectRef = try_alloc_concurrent_synthetic(&mut *scope, "java/util/Optional", 1)?;
    let value = value_h.as_ref().map(|h| scope.get(h));
    scope.set_field(opt, 0, Value::Object(value));
    Ok(opt)
}

fn init_http_response_fields(ctx: &mut dyn NativeContext, obj: ObjectRef, status: i32) {
    ctx.set_field(obj, RESP_STATUS, Value::Int(status));
    ctx.set_field(obj, RESP_VERSION, Value::Int(HTTP_VERSION_2));
    ctx.set_field(obj, RESP_BODY_LEN, Value::Long(0));
    ctx.set_field(obj, RESP_HAS_PREV, Value::Int(0));
    ctx.set_field(obj, RESP_METHOD, Value::Int(METHOD_GET));
    ctx.set_field(obj, RESP_HAS_SSL, Value::Int(0));
    ctx.set_field(obj, RESP_BODY_OBJ, Value::Object(None));
}

fn alloc_http_headers(
    ctx: &mut dyn NativeContext,
    count: i32,
    has_ct: i32,
    has_cl: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpHeaders", 3)?;
    init_http_headers_fields(ctx, obj, count, has_ct, has_cl);
    Ok(obj)
}

fn init_http_headers_fields(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    count: i32,
    has_ct: i32,
    has_cl: i32,
) {
    ctx.set_field(obj, HDR_COUNT, Value::Int(count));
    ctx.set_field(obj, HDR_HAS_CT, Value::Int(has_ct));
    ctx.set_field(obj, HDR_HAS_CL, Value::Int(has_cl));
}

fn alloc_websocket(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/net/http/WebSocket", 4)?;
    init_websocket_fields(ctx, obj);
    Ok(obj)
}

fn init_websocket_fields(ctx: &mut dyn NativeContext, obj: ObjectRef) {
    ctx.set_field(obj, WS_STATE, Value::Int(WS_OPEN));
    ctx.set_field(obj, WS_SUBPROTOCOL, Value::Int(0));
    ctx.set_field(obj, WS_OUTPUT_CLOSED, Value::Int(0));
    ctx.set_field(obj, WS_INPUT_CLOSED, Value::Int(0));
}

// ---------------------------------------------------------------------------
// Real HTTP/1.1 request helper
// ---------------------------------------------------------------------------

/// Extract host, port, and path from a URI object.
///
/// JDK-ONLY-LAYOUT: converted from raw slot indices to
/// [`crate::net_phase_e::uri_components`]. This function used to carry its own
/// copy of the fabricated `scheme=0, host=1, port=2, path=3` URI model — the
/// second of two identical copies, the other in `servlet.rs` — and on a real
/// `java.net.URI` those indices are `fragment`, `authority` and `userInfo`. It
/// read a well-typed `String` every time and it was the wrong one, so an HTTP
/// request built from a real URI would dial the fragment as its host.
fn extract_uri_parts(ctx: &dyn NativeContext, uri: ObjectRef) -> Option<(String, u16, String)> {
    let parts = crate::net_phase_e::uri_components(ctx, uri);
    let host = parts.host?;
    let port = if parts.port > 0 {
        parts.port as u16
    } else if parts.scheme.as_deref() == Some("https") {
        443
    } else {
        80
    };
    let path = if parts.path.is_empty() {
        "/".to_string()
    } else {
        parts.path
    };
    Some((host, port, path))
}

/// Does this URI's scheme call for TLS? Falls back to the port when the URI
/// carries no scheme, which is what both call sites did through the raw slot.
fn uri_wants_tls(ctx: &dyn NativeContext, uri: ObjectRef, port: u16) -> bool {
    match crate::net_phase_e::uri_components(ctx, uri).scheme {
        Some(s) => s == "https",
        None => port == 443,
    }
}

/// Maximum HTTP response body size (10 MB).
const MAX_RESPONSE_BODY: usize = 10 * 1024 * 1024;

/// [VULN fix nb-http2 (2)] Reject any CR, LF, or NUL byte in a value that is
/// interpolated into the HTTP/1.1 request line or a header field.
///
/// `http11_request_impl` builds the request line by directly interpolating the
/// method, request-target (path) and `Host` value. Without this check an
/// attacker-controlled value containing `\r\n` could inject extra request lines
/// or headers (HTTP request splitting / smuggling). The JDK's
/// `java.net.http`/`HeaderName`/field-value validation rejects these characters
/// with `IllegalArgumentException`; we mirror that here.
fn validate_no_crlf(kind: &str, value: &str) -> Result<(), RuntimeError> {
    for &b in value.as_bytes() {
        if b == b'\r' || b == b'\n' || b == 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!(
                    "illegal character in HTTP request {kind}: control byte 0x{b:02x} (CR/LF/NUL not permitted)"
                ),
            });
        }
    }
    Ok(())
}

/// [HIGH fix nb-http2 (1)] Refuse to silently send a request whose headers or
/// body would be dropped.
///
/// The synthetic `HttpRequest` only retains a header *count* (`REQ_HDR_COUNT`)
/// and a `REQ_HAS_BODY` flag — the builder discards the actual header
/// name/value strings and the `BodyPublisher` content, and `BodyPublisher`
/// itself only stores a length, never the bytes. Therefore the request line
/// emitted by `http11_request_impl` cannot reproduce caller-supplied headers or
/// a request body. Rather than transmit a corrupted (header/body-less) request
/// and pretend it succeeded, we throw `UnsupportedOperationException` so the
/// caller sees an honest failure. A plain request with no headers and no body
/// loses nothing and is allowed to proceed.
///
/// Full fidelity requires the builder (`HttpRequest$Builder.header`/`POST`/...)
/// and `BodyPublishers.*` to persist the real header strings and body bytes on
/// the object — a cross-file change outside this module's scope. See the
/// cross-file follow-up in the task report.
fn ensure_no_dropped_payload(ctx: &dyn NativeContext, req: ObjectRef) -> Result<(), RuntimeError> {
    let hdr_count = match ctx.get_field(req, REQ_HDR_COUNT) {
        Value::Int(n) => n,
        _ => 0,
    };
    let has_body = match ctx.get_field(req, REQ_HAS_BODY) {
        Value::Int(n) => n,
        _ => 0,
    };
    if hdr_count > 0 || has_body != 0 {
        return Err(RuntimeError::UnsupportedOperationException {
            message: format!(
                "CratonVM HttpClient cannot send requests with custom headers ({hdr_count}) \
                 or a request body (has_body={has_body}): the synthetic request layout does not \
                 retain header values or body bytes (would be silently dropped)"
            ),
        });
    }
    Ok(())
}

/// Perform a real HTTP/1.1 request with optional TLS and return (status_code, response_body).
fn http11_request(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
) -> Result<(i32, String), String> {
    http11_request_impl(host, port, method, path, false)
}

/// Perform a real HTTPS/1.1 request with TLS and return (status_code, response_body).
fn https_request(host: &str, port: u16, method: &str, path: &str) -> Result<(i32, String), String> {
    http11_request_impl(host, port, method, path, true)
}

fn http11_request_impl(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    tls: bool,
) -> Result<(i32, String), String> {
    // [VULN fix nb-http2 (2)] Defense-in-depth: never write a request line whose
    // interpolated method/path/host carries CR, LF, or NUL — that would allow
    // request splitting/smuggling. The native send/sendAsync handlers also
    // validate (and surface IllegalArgumentException), but guarding here keeps
    // the actual socket-write path safe for any caller.
    let has_ctl = |s: &str| {
        s.as_bytes()
            .iter()
            .any(|&b| b == b'\r' || b == b'\n' || b == 0)
    };
    if has_ctl(method) || has_ctl(path) || has_ctl(host) {
        return Err("illegal CR/LF/NUL in request line (request-splitting guard)".to_string());
    }

    let addr = format!("{}:{}", host, port);
    // Fold IPv4-mapped destinations (`::ffff:a.b.c.d`) to plain IPv4 before
    // dialling: on Windows an AF_INET6 socket cannot reach one (`IPV6_V6ONLY`
    // defaults to 1 → WSAEADDRNOTAVAIL), and a URL host arrives here as text,
    // never through `InetAddress`. Same fold the h1 client applies.
    let tcp_stream = cratonvm_native_io::outbound_policy::connect_str_normalized(&addr)
        .map_err(|e| format!("connect: {e}"))?;
    tcp_stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .ok();
    tcp_stream
        .set_write_timeout(Some(std::time::Duration::from_secs(10)))
        .ok();

    let request_line = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nAccept: */*\r\nUser-Agent: CratonVM/1.0\r\n\r\n"
    );

    let response_bytes = if tls {
        // TLS connection via native-tls
        let connector = native_tls::TlsConnector::new().map_err(|e| format!("TLS init: {e}"))?;
        let mut tls_stream = connector
            .connect(host, tcp_stream)
            .map_err(|e| format!("TLS handshake: {e}"))?;
        tls_stream
            .write_all(request_line.as_bytes())
            .map_err(|e| format!("TLS write: {e}"))?;
        tls_stream.flush().map_err(|e| format!("TLS flush: {e}"))?;
        let mut buf = Vec::new();
        read_limited(&mut tls_stream, &mut buf, MAX_RESPONSE_BODY)
            .map_err(|e| format!("TLS read: {e}"))?;
        let _ = tls_stream.shutdown();
        buf
    } else {
        let mut stream = tcp_stream;
        stream
            .write_all(request_line.as_bytes())
            .map_err(|e| format!("write: {e}"))?;
        stream.flush().map_err(|e| format!("flush: {e}"))?;
        let mut buf = Vec::new();
        read_limited(&mut stream, &mut buf, MAX_RESPONSE_BODY).map_err(|e| format!("read: {e}"))?;
        let _ = stream.shutdown(std::net::Shutdown::Both);
        buf
    };

    let response_str = String::from_utf8_lossy(&response_bytes);

    // Parse HTTP/1.1 status line: "HTTP/1.x NNN reason\r\n"
    let status_line = response_str.lines().next().unwrap_or("");
    let status_code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(0);

    // Split headers from body at "\r\n\r\n"
    let (headers_section, raw_body) = if let Some(pos) = response_str.find("\r\n\r\n") {
        (&response_str[..pos], &response_str[pos + 4..])
    } else if let Some(pos) = response_str.find("\n\n") {
        (&response_str[..pos], &response_str[pos + 2..])
    } else {
        (response_str.as_ref(), "")
    };

    // Handle chunked transfer-encoding
    let is_chunked = headers_section
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let body = if is_chunked {
        decode_chunked(raw_body)
    } else {
        raw_body.to_string()
    };

    Ok((status_code, body))
}

/// Read from a stream with a size limit to prevent unbounded memory growth.
fn read_limited(
    reader: &mut dyn std::io::Read,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<()> {
    let mut tmp = [0u8; 8192];
    loop {
        match reader.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() + n > max {
                    let remaining = max - buf.len();
                    buf.extend_from_slice(&tmp[..remaining]);
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Decode a chunked transfer-encoding body.
fn decode_chunked(input: &str) -> String {
    let mut result = String::new();
    let mut remaining = input;
    loop {
        // Find the chunk size line
        let line_end = match remaining.find("\r\n") {
            Some(pos) => pos,
            None => break,
        };
        let size_str = remaining[..line_end].trim();
        if size_str.is_empty() {
            break;
        }
        let chunk_size = match usize::from_str_radix(size_str, 16) {
            Ok(s) => s,
            Err(_) => break,
        };
        if chunk_size == 0 {
            break;
        } // Terminal chunk
        let data_start = line_end + 2;
        let data_end = data_start + chunk_size;
        if data_end > remaining.len() {
            // Partial chunk — take what's available
            result.push_str(&remaining[data_start..]);
            break;
        }
        result.push_str(&remaining[data_start..data_end]);
        remaining = &remaining[data_end..];
        // Skip trailing \r\n after chunk data
        if remaining.starts_with("\r\n") {
            remaining = &remaining[2..];
        }
    }
    result
}

fn alloc_body_publisher(
    ctx: &mut dyn NativeContext,
    len: i64,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$BodyPublisher", 2)?;
    init_body_publisher_fields(ctx, obj, len);
    Ok(obj)
}

fn init_body_publisher_fields(ctx: &mut dyn NativeContext, obj: ObjectRef, len: i64) {
    ctx.set_field(obj, 0, Value::Long(len));
    ctx.set_field(obj, 1, Value::Int(0)); // type idx
}

fn alloc_body_handler(
    ctx: &mut dyn NativeContext,
    kind: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse$BodyHandler", 1)?;
    ctx.set_field(obj, 0, Value::Int(kind));
    Ok(obj)
}

// ---------------------------------------------------------------------------
// 1. java.net.http.HttpClient
// ---------------------------------------------------------------------------

/// `HttpClient.sendAsync` — one body shared by the two-argument and the
/// three-argument (push-promise) overloads, so neither can fall through to a
/// different registrar's carrier layout.
fn http2_send_async(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let req = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let resp = alloc_http_response(ctx, 0)?;
            return completed_future_of(ctx, resp);
        }
    };
    let method_idx = match ctx.get_field(req, REQ_METHOD) {
        Value::Int(n) => n,
        _ => METHOD_GET,
    };
    let method_str = method_idx_to_name(method_idx);
    let uri_obj = match ctx.get_field(req, REQ_URI) {
        Value::Object(Some(u)) => u,
        _ => {
            let resp = alloc_http_response(ctx, 0)?;
            return completed_future_of(ctx, resp);
        }
    };
    let (host, port, path) = match extract_uri_parts(ctx, uri_obj) {
        Some(parts) => parts,
        None => {
            let resp = alloc_http_response(ctx, 0)?;
            return completed_future_of(ctx, resp);
        }
    };
    let use_tls = uri_wants_tls(ctx, uri_obj, port);
    // [HIGH fix nb-http2 (1)] Don't silently drop caller headers/body.
    ensure_no_dropped_payload(ctx, req)?;
    // [VULN fix nb-http2 (2)] Reject CR/LF/NUL in request-line values.
    validate_no_crlf("method", method_str)?;
    validate_no_crlf("request-target", &path)?;
    validate_no_crlf("Host header", &host)?;
    let result = blocking_http_exchange(ctx, use_tls, &host, port, method_str, &path, false);
    let resp = match result {
        Ok((status, body)) => alloc_http_response_with_body(ctx, status, &body)?,
        Err(_) => alloc_http_response(ctx, 0)?,
    };
    completed_future_of(ctx, resp)
}

/// The synthetic client's network exchange, inside a GC-blocking region.
///
/// gc-common w16-f: `https_request` / `http11_request` connect, write and
/// read a socket (30 s timeouts) with no context and no heap access, and they
/// ran as an ordinary native call: a stop-the-world collection requested by
/// another thread waited for this one until the peer answered. The callers
/// hold no object reference across the call.
///
/// `sync` (interpreter round i3 wave 82, lane L6): the caller stands for
/// `HttpClient.send`, which JDK 25 runs as `sendAsync(..).get()` -- the
/// HotSpot thread is WAITING (parking) in `CompletableFuture.get`, so the
/// region says `Parked`. `!sync` is `sendAsync`, which returns at once on
/// HotSpot with the thread RUNNABLE: a VM wait (`begin_vm_wait_region`).
fn blocking_http_exchange(
    ctx: &mut dyn NativeContext,
    use_tls: bool,
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    sync: bool,
) -> Result<(i32, String), String> {
    if sync {
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::Parked);
    } else {
        ctx.begin_vm_wait_region();
    }
    let result = if use_tls {
        https_request(host, port, method, path)
    } else {
        http11_request(host, port, method, path)
    };
    ctx.end_blocking_region();
    result
}

/// `HttpClient.Version` for an ordinal, as a real enum object.
///
/// Shares `p57_alloc_enum` with the `phases_late::net_channels` registrar that
/// owns the `HTTP_1_1` / `HTTP_2` statics, so an object from either side has the
/// same (name, ordinal) shape and the two compare equal.
fn version_enum(
    ctx: &mut dyn NativeContext,
    ordinal: i32,
) -> cratonvm_types::error::MethodCallResult {
    let name = if ordinal == HTTP_VERSION_1_1 {
        "HTTP_1_1"
    } else {
        "HTTP_2"
    };
    crate::phases_late::p57_alloc_enum(ctx, "java/net/http/HttpClient$Version", name, ordinal)
}

/// `HttpClient.Redirect` for an ordinal. See [`version_enum`].
fn redirect_enum(
    ctx: &mut dyn NativeContext,
    ordinal: i32,
) -> cratonvm_types::error::MethodCallResult {
    let name = match ordinal {
        REDIRECT_ALWAYS => "ALWAYS",
        REDIRECT_NORMAL => "NORMAL",
        _ => "NEVER",
    };
    crate::phases_late::p57_alloc_enum(ctx, "java/net/http/HttpClient$Redirect", name, ordinal)
}

// ---------------------------------------------------------------------------
// ARGUMENT DECODING — read the DESCRIPTOR, not the neighbouring idiom.
//
// Six builder natives in this file matched `Some(Value::Int(n))` against an
// argument whose descriptor is a REFERENCE (`Ljava/time/Duration;`,
// `Ljava/net/http/HttpClient$Version;`, `Ljava/net/http/HttpClient$Redirect;`,
// `[B`). That arm can never fire, so the `_ =>` default ALWAYS won and every
// such setter silently discarded what the caller passed. The helpers below
// decode what the descriptor actually says arrives.
//
// This is NOT "every `Value::Int` match in a builder is wrong":
// `HttpRequest$Builder.expectContinue(Z)` uses the identical idiom and is
// CORRECT, because its descriptor really is a primitive `boolean`. Same
// structure as `HttpHeaders.firstValueAsLong`, whose 2-slot
// `(isPresent, value)` `OptionalLong` is right where the reference `Optional`'s
// was wrong. Both are pinned by negative-control tests below. Read the
// descriptor before changing an arm.
//
// See docs/feature-designs/native-dispatch-architecture.md
// ---------------------------------------------------------------------------

/// Instance slots of a `java.time.Duration`: `seconds` (`long`) then `nanos`
/// (`int`).
///
/// `javap -p java.time.Duration` (JDK 25) declares exactly those two instance
/// fields in that order, and `util_time::alloc_duration` mints the synthetic
/// with the same pair — so one decode serves both shapes.
const DUR_SLOT_SECONDS: usize = 0;
const DUR_SLOT_NANOS: usize = 1;

/// Instance slots of a `java.lang.Enum`: `name` (a `String`) then `ordinal`
/// (`int`) — the order `java.lang.Enum` declares them and the order
/// `phases_late::p57_alloc_enum` writes them.
const ENUM_SLOT_NAME: usize = 0;
const ENUM_SLOT_ORDINAL: usize = 1;

/// `HttpClient.Version` constants, name -> the ordinal THIS file uses.
/// Verified against `HttpClient.Version.values()` on HotSpot 25: `HTTP_1_1`=0,
/// `HTTP_2`=1 — the JDK ordinals and this file's constants agree.
const VERSION_NAMES: &[(&str, i32)] = &[("HTTP_1_1", HTTP_VERSION_1_1), ("HTTP_2", HTTP_VERSION_2)];

/// `HttpClient.Redirect` constants. Verified against
/// `HttpClient.Redirect.values()` on HotSpot 25: `NEVER`=0, `ALWAYS`=1,
/// `NORMAL`=2 — again matching this file's constants.
const REDIRECT_NAMES: &[(&str, i32)] = &[
    ("NEVER", REDIRECT_NEVER),
    ("ALWAYS", REDIRECT_ALWAYS),
    ("NORMAL", REDIRECT_NORMAL),
];

/// Decode a `java.time.Duration` ARGUMENT to whole milliseconds.
///
/// Both encodings reconcile here (the "one concept, two encodings" shape) via
/// the CLASS-SIDE witness: resolve the NAME to a slot against the receiver's
/// own class, and fall back to the synthetic layout only when the class does
/// not declare it. That is deliberately not a `get_field_by_name` VALUE read —
/// `test_utils::MockNativeContext::get_field_by_name` answers `Value::Int(0)`
/// for an unresolvable name where production answers `Value::Object(None)`
/// (its own doc comment records the divergence and two bugs already paid for
/// by it), so a value-side fallback would be steered by the mock rather than
/// by the VM. `resolve_field_index_by_class_id` is the read the mock answers
/// faithfully.
///
/// `Duration` is floor-normalised (`ofMillis(-1500)` is `seconds=-2,
/// nanos=500_000_000`), so `seconds * 1000 + nanos / 1_000_000` is correct for
/// negative durations too.
///
/// The raw `Long` / `Int` arms are retained as a fallback for any caller that
/// hands over bare millis rather than a `Duration`.
fn duration_arg_millis(ctx: &mut dyn NativeContext, arg: Option<&Value>) -> i64 {
    match arg {
        Some(Value::Object(Some(d))) => {
            let d = *d;
            let cid = ctx.class_id_of_object(d);
            let sec_slot = ctx
                .resolve_field_index_by_class_id(cid, "seconds")
                .unwrap_or(DUR_SLOT_SECONDS);
            let nano_slot = ctx
                .resolve_field_index_by_class_id(cid, "nanos")
                .unwrap_or(DUR_SLOT_NANOS);
            let secs = match ctx.get_field(d, sec_slot) {
                Value::Long(n) => n,
                Value::Int(n) => i64::from(n),
                _ => 0,
            };
            let nanos = match ctx.get_field(d, nano_slot) {
                Value::Int(n) => n,
                Value::Long(n) => n as i32,
                _ => 0,
            };
            secs.saturating_mul(1_000)
                .saturating_add(i64::from(nanos) / 1_000_000)
        }
        // Bare millis — not what any descriptor in this file says, but cheap to
        // honour and the shape the pre-fix code was (unreachably) written for.
        Some(Value::Long(n)) => *n,
        Some(Value::Int(n)) => i64::from(*n),
        _ => 0,
    }
}

/// Decode an ENUM ARGUMENT to its ordinal, or `None` when it carries neither
/// encoding.
///
/// `None` rather than `0` on purpose: a decoder that silently decays to ordinal
/// zero is how `TimeUnit` turned `SECONDS.toNanos(1)` into `1`
/// (`phases_early::tu_ordinal`). Each caller keeps its OWN documented default.
///
/// NAME first, ordinal second. The name is the encoding-independent identity of
/// an enum constant, it is what `p57_alloc_enum` writes into slot 0, and it
/// survives an ordinal renumbering in a future JDK. Both fields are located by
/// the CLASS-SIDE witness rather than a `get_field_by_name` value read — see
/// [`duration_arg_millis`] for why. `java.lang.Enum` declares `name` and
/// `ordinal` on the SUPERCLASS, and `resolve_field_index_by_class_id` walks the
/// hierarchy, so a real enum resolves both.
///
/// Neither step calls `invoke_virtual`, unlike
/// `phases_early::enum_value_ordinal`: no native in this file re-enters the
/// interpreter, and an argument decoder is the wrong place to start.
fn enum_arg_ordinal(
    ctx: &mut dyn NativeContext,
    arg: Option<&Value>,
    names: &[(&str, i32)],
) -> Option<i32> {
    let obj = match arg {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    let cid = ctx.class_id_of_object(obj);
    let name_slot = ctx
        .resolve_field_index_by_class_id(cid, "name")
        .unwrap_or(ENUM_SLOT_NAME);
    let ordinal_slot = ctx
        .resolve_field_index_by_class_id(cid, "ordinal")
        .unwrap_or(ENUM_SLOT_ORDINAL);
    if let Value::Object(Some(s)) = ctx.get_field(obj, name_slot) {
        if let Some(text) = ctx.read_string(s) {
            if let Some((_, ord)) = names.iter().find(|(k, _)| *k == text) {
                return Some(*ord);
            }
        }
    }
    let ordinal = match ctx.get_field(obj, ordinal_slot) {
        Value::Int(n) => Some(n),
        _ => None,
    };
    // An ordinal outside the constants this file models is not a usable answer;
    // hand back `None` so the caller's default applies rather than storing a
    // number `version_enum` / `redirect_enum` would have to invent a name for.
    ordinal.filter(|n| names.iter().any(|(_, o)| o == n))
}

/// The declared content length of a `BodyPublisher` ARGUMENT (slot 0, a
/// `Long`; `-1` is the publishers' "unknown length"), or `None` when the
/// argument is not a publisher.
///
/// Found by grepping the SHAPE rather than the `Value::Int` idiom: `POST`,
/// `PUT` and `method(String, BodyPublisher)` set `REQ_HAS_BODY` from their
/// publisher argument and then throw the publisher away, so `REQ_BODY_LEN`
/// stayed 0 and `bodyPublisher().get().contentLength()` answered 0 for every
/// body. Same family as the six `Value::Int` sites — a builder that does not
/// read the reference it was handed — with a quieter tell.
fn body_publisher_arg_len(ctx: &mut dyn NativeContext, arg: Option<&Value>) -> Option<i64> {
    let bp = match arg {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    match ctx.get_field(bp, 0) {
        Value::Long(n) => Some(n),
        Value::Int(n) => Some(i64::from(n)),
        _ => None,
    }
}

fn register_http_client(r: &mut NativeMethodRegistry) {
            let cls = "java/net/http/HttpClient";
    // `HttpClient` is an ABSTRACT class in the real JDK, whose protected
    // no-arg constructor really is empty — but CratonVM's `HttpClient` is a
    // 10-slot synthetic that `send()` reads by index, and a receiver reaching
    // here has none of those slots written. Establish the same defaults the
    // factory (`newHttpClient()`) produces. See `is_synthetic_shape` for why
    // the real-subclass case must be left alone.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if is_synthetic_shape(ctx, this, "java/net/http/HttpClient") {
            init_http_client_fields(ctx, this);
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // static newHttpClient() -> HttpClient
    r.register(
        cls,
        "newHttpClient",
        "()Ljava/net/http/HttpClient;",
        |ctx, _args| {
            let obj = alloc_http_client(ctx)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // static newBuilder() -> HttpClient$Builder
    r.register(
        cls,
        "newBuilder",
        "()Ljava/net/http/HttpClient$Builder;",
        |ctx, _args| {
            let bld = alloc_http_client_builder(ctx)?;
            Ok(Some(Value::Object(Some(bld))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // send(HttpRequest, BodyHandler) -> HttpResponse
    r.register(
        cls,
        "send",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;",
        |ctx, args| {
            // Extract the HttpRequest from args[1] (args[0] is 'this' HttpClient)
            let req = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => {
                    let resp = alloc_http_response(ctx, 0)?;
                    return Ok(Some(Value::Object(Some(resp))));
                }
            };

            // Get HTTP method
            let method_idx = match ctx.get_field(req, REQ_METHOD) {
                Value::Int(n) => n,
                _ => METHOD_GET,
            };
            let method_str = method_idx_to_name(method_idx);

            // Get URI object from the request
            let uri_obj = match ctx.get_field(req, REQ_URI) {
                Value::Object(Some(u)) => u,
                _ => {
                    // No URI — return a synthetic 0-status response
                    let resp = alloc_http_response(ctx, 0)?;
                    return Ok(Some(Value::Object(Some(resp))));
                }
            };

            // Extract host/port/path from URI
            let (host, port, path) = match extract_uri_parts(ctx, uri_obj) {
                Some(parts) => parts,
                None => {
                    let resp = alloc_http_response(ctx, 0)?;
                    return Ok(Some(Value::Object(Some(resp))));
                }
            };

            // Determine if TLS is needed based on URI scheme or port
            let use_tls = uri_wants_tls(ctx, uri_obj, port);

            // [HIGH fix nb-http2 (1)] Don't silently drop caller headers/body —
            // throw UnsupportedOperationException if the request carries either.
            ensure_no_dropped_payload(ctx, req)?;

            // [VULN fix nb-http2 (2)] Reject CR/LF/NUL in the values that are
            // interpolated into the request line (request-splitting guard).
            validate_no_crlf("method", method_str)?;
            validate_no_crlf("request-target", &path)?;
            validate_no_crlf("Host header", &host)?;

            // Perform real HTTP request (with TLS for HTTPS), GC-blocking
            // (gc-common w16-f).
            let result =
                blocking_http_exchange(ctx, use_tls, &host, port, method_str, &path, true);
            match result {
                Ok((status, body)) => {
                    // The body String rides on `RESP_BODY_OBJ`, stored through
                    // the response's current address (gc-common w16-f).
                    let resp = alloc_http_response_with_body(ctx, status, &body)?;
                    // NOTE [nb-http2 (1)]: response headers are not parsed/attached here.
                    // The synthetic `HttpHeaders` layout (3 ints) cannot hold real
                    // name/value pairs, so `HttpResponse.headers()` still fabricates a
                    // fixed header set. Faithful response-header parsing requires
                    // extending the HttpHeaders representation (cross-method follow-up).
                    Ok(Some(Value::Object(Some(resp))))
                }
                Err(_e) => {
                    // Connection failed — return status 0 to signal error
                    let resp = alloc_http_response(ctx, 0)?;
                    return Ok(Some(Value::Object(Some(resp))));
                }
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // sendAsync(HttpRequest, BodyHandler, PushPromiseHandler) — the same request
    // with the push-promise handler ignored, which is exactly what the two-arg
    // overload's contract already is (a client that never receives a server push
    // never invokes the handler).
    //
    // Registered HERE, on this carrier, because `net_phase_e`'s RE5 registrar
    // owned this key alone. In a `--synthetic-jdk` build this file registers
    // LAST, so every other `HttpClient` key resolved to the 10-slot carrier
    // while this one fell through to RE5's body — which reads the 9-slot RE5
    // layout (`RE5_CLIENT_SSL_CONTEXT` at slot 3, `RE5_CLIENT_PROXY` at 5) off
    // an object whose slots 3 and 5 are this file's `has-SSL` / `has-proxy`
    // booleans. That cross-shape read is the concrete form of the
    // three-carriers hazard; covering the key closes it without deleting a
    // registrar either build depends on.
    r.register(
        cls,
        "sendAsync",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;Ljava/net/http/HttpResponse$PushPromiseHandler;)Ljava/util/concurrent/CompletableFuture;",
        http2_send_async, cratonvm_native_api::NativeKind::Bridge,
    );

    // sendAsync(HttpRequest, BodyHandler) -> CompletableFuture<HttpResponse>
    // Performs the same real HTTP request as send(), then wraps result in a completed CF.
    r.register(
        cls,
        "sendAsync",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/util/concurrent/CompletableFuture;",
        http2_send_async, cratonvm_native_api::NativeKind::Bridge,
    );

    // version() -> HttpClient$Version
    r.register(
        cls,
        "version",
        "()Ljava/net/http/HttpClient$Version;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let v = match ctx.get_field(this, CLIENT_VERSION) {
                Value::Int(n) => n,
                _ => HTTP_VERSION_2,
            };
            // Declared to return `HttpClient$Version`; returning the raw slot
            // int handed an Int where every caller dereferences an object.
            version_enum(ctx, v)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // connectTimeout() -> Optional<Duration>
    r.register(
        cls,
        "connectTimeout",
        "()Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ms = match ctx.get_field(this, CLIENT_CONNECT_TIMEOUT) {
                Value::Long(n) => n,
                _ => 0,
            };
            // `java.util.Optional` has ONE instance field and it is a
            // REFERENCE (`javap -p java.util.Optional`, JDK 25): `isPresent()`
            // is `value != null` and `get()` returns `value` itself. This VM's
            // own synthetic `Optional` natives model it identically
            // (`phases_early::register_core_stdlib_extras`). A presence flag in
            // slot 0 therefore IS the value, and `Value::Int(0)` is not null to
            // either reader -- so an EMPTY Optional reported PRESENT and
            // `get()` handed back the flag. The (flag, payload) layout is the
            // real layout of `OptionalInt`/`OptionalLong`/`OptionalDouble`, not
            // of this class -- see the `firstValueAsLong` site below, which is
            // CORRECT for exactly that reason. See
            // docs/feature-designs/native-dispatch-architecture.md
            // Declared `Optional<Duration>`; the same body as
            // `http_client.rs:1632`'s `connectTimeout`.
            //
            // The Duration first, then the Optional (`optional_of`, gc-common
            // w16-f): it used to be stored through the Optional's address from
            // before the Duration's allocation.
            let dur = if ms > 0 {
                let nanos = ((ms % 1000) * 1_000_000) as i32;
                // `crate::util_time` is gated behind `#[cfg(feature = "synthetic-jdk")]`,
                // but this file compiles in EVERY configuration. The crate root
                // carries an ungated twin with the same job (and normalisation
                // the gated one lacks), so use it rather than gating the caller.
                Some(crate::lib_parts::matcher_captures::alloc_duration(ctx, ms / 1000, nanos)?)
            } else {
                None
            };
            let opt = optional_of(ctx, dur)?;
            Ok(Some(Value::Object(Some(opt))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // followRedirects() -> HttpClient$Redirect
    r.register(
        cls,
        "followRedirects",
        "()Ljava/net/http/HttpClient$Redirect;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let r = match ctx.get_field(this, CLIENT_REDIRECT) {
                Value::Int(n) => n,
                _ => REDIRECT_NEVER,
            };
            // Declared to return `HttpClient$Redirect` — see `version()` above.
            redirect_enum(ctx, r)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // executor() -> Optional<Executor>
    r.register(cls, "executor", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Slot 0 of a `java.util.Optional` is `value`, a REFERENCE -- an `Int`
        // there reads as PRESENT and `get()` returns the flag. The arity was
        // already right here and the TYPE was not, which is what shows the
        // defect is the flag rather than the slot count. See
        // docs/feature-designs/native-dispatch-architecture.md.
        // The synthetic client stores a presence FLAG and never the `Executor`
        // itself (`CLIENT_HAS_EXECUTOR` is only ever written `Int(0)` by
        // `alloc_http_client`, then copied by `Builder.build()`), so `empty()`
        // is the only answer this layout can give truthfully: a MISSING answer
        // where there was a wrong one.
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        ctx.set_field(opt, 0, Value::Object(None));
        Ok(Some(Value::Object(Some(opt))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // cookieHandler() -> Optional<CookieHandler>
    r.register(
        cls,
        "cookieHandler",
        "()Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // See `executor()` above and
            // docs/feature-designs/native-dispatch-architecture.md:
            // slot 0 is the reference `value`, and this layout holds a flag
            // rather than the `CookieHandler`, so `empty()` is the only
            // truthful answer.
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            ctx.set_field(opt, 0, Value::Object(None));
            Ok(Some(Value::Object(Some(opt))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // proxy() -> Optional<ProxySelector>
    r.register(cls, "proxy", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // See `executor()` above: slot 0 is the reference `value`, and this
        // layout holds a flag rather than the `ProxySelector`.
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        ctx.set_field(opt, 0, Value::Object(None));
        Ok(Some(Value::Object(Some(opt))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // authenticator() -> Optional<Authenticator>
    r.register(
        cls,
        "authenticator",
        "()Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // See `executor()` above: slot 0 is the reference `value`, and
            // this layout holds a flag rather than the `Authenticator`.
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            ctx.set_field(opt, 0, Value::Object(None));
            Ok(Some(Value::Object(Some(opt))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // sslContext() -> SSLContext
    r.register(
        cls,
        "sslContext",
        "()Ljavax/net/ssl/SSLContext;",
        |ctx, _args| {
            let ssl = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLContext", 4)?;
            ctx.set_field(ssl, 0, Value::Int(2)); // TLSv1.3
            ctx.set_field(ssl, 1, Value::Int(1)); // initialized
            Ok(Some(Value::Object(Some(ssl))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // sslParameters() -> SSLParameters
    r.register(
        cls,
        "sslParameters",
        "()Ljavax/net/ssl/SSLParameters;",
        |ctx, _args| {
            let params = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLParameters", 4)?;
            ctx.set_field(params, 0, Value::Int(0));
            ctx.set_field(params, 1, Value::Int(0));
            ctx.set_field(params, 2, Value::Int(0));
            ctx.set_field(params, 3, Value::Int(0));
            let params = crate::tls::ssl_parameters_constructor_defaults(ctx, params);
            Ok(Some(Value::Object(Some(params))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // newWebSocketBuilder() -> WebSocket$Builder
    r.register(
        cls,
        "newWebSocketBuilder",
        "()Ljava/net/http/WebSocket$Builder;",
        |ctx, _args| {
            let bld = try_alloc_concurrent_synthetic(ctx, "java/net/http/WebSocket$Builder", 3)?;
            ctx.set_field(bld, 0, Value::Int(0)); // subprotocols set
            ctx.set_field(bld, 1, Value::Long(0)); // connect timeout
            ctx.set_field(bld, 2, Value::Int(0)); // header count
            Ok(Some(Value::Object(Some(bld))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

// ---------------------------------------------------------------------------
// 2. java.net.http.HttpClient$Builder
// ---------------------------------------------------------------------------

fn register_http_client_builder(r: &mut NativeMethodRegistry) {
            let cls = "java/net/http/HttpClient$Builder";
    // `HttpClient.Builder` is an interface in the real JDK, so this runs only
    // for a CratonVM synthetic receiver — and for a builder the all-defaults
    // state is definitely NOT correct: `build()` below copies all 8 slots into
    // the client unconditionally, so any slot the caller did not configure
    // (the common case — nobody calls all nine setters) would arrive at the
    // client as an untyped default rather than as `HTTP_2` / `NEVER` / a
    // `Long` connect timeout.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if is_synthetic_shape(ctx, this, "java/net/http/HttpClient$Builder") {
            init_http_client_builder_fields(ctx, this);
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // version(HttpClient$Version) -> Builder
    r.register(
        cls,
        "version",
        "(Ljava/net/http/HttpClient$Version;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // The parameter is an ENUM REFERENCE. The `Some(Value::Int(n))`
            // this used to match could never fire, so `version(HTTP_1_1)` built
            // an HTTP_2 client — the default silently overwrote the caller.
            let v = enum_arg_ordinal(ctx, args.get(1), VERSION_NAMES).unwrap_or(HTTP_VERSION_2);
            ctx.set_field(this, 0, Value::Int(v));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // connectTimeout(Duration) -> Builder
    r.register(
        cls,
        "connectTimeout",
        "(Ljava/time/Duration;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // The parameter is a `java.time.Duration` REFERENCE, so neither the
            // `Long` nor the `Int` arm ever fired and `CLIENT_CONNECT_TIMEOUT`
            // could never be non-zero — which is why `connectTimeout()` had no
            // reachable present arm at all.
            let ms = duration_arg_millis(ctx, args.get(1));
            ctx.set_field(this, 2, Value::Long(ms));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // followRedirects(HttpClient$Redirect) -> Builder
    r.register(
        cls,
        "followRedirects",
        "(Ljava/net/http/HttpClient$Redirect;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Enum REFERENCE parameter: `followRedirects(ALWAYS)` used to leave
            // the policy at NEVER.
            let policy =
                enum_arg_ordinal(ctx, args.get(1), REDIRECT_NAMES).unwrap_or(REDIRECT_NEVER);
            ctx.set_field(this, 1, Value::Int(policy));
            // A SLOT COLLISION this fix would otherwise have activated: this
            // used to also write `Int(policy != NEVER)` into BUILDER slot 7 —
            // which `build()` copies to `CLIENT_HAS_COOKIE`, not to
            // `CLIENT_FOLLOW_REDIR` (slot 8, which `build()` does not copy at
            // all). It was inert only because `policy` could never be anything
            // but `NEVER`, so it always wrote the same 0 the initialiser had
            // just written; the moment the decode above works it starts
            // clobbering the cookie-handler flag with a redirect answer.
            // `build()` derives `CLIENT_FOLLOW_REDIR` from the policy instead.
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // executor(Executor) -> Builder
    r.register(
        cls,
        "executor",
        "(Ljava/util/concurrent/Executor;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let has = match args.get(1) {
                Some(Value::Object(Some(_))) => 1,
                _ => 0,
            };
            ctx.set_field(this, 4, Value::Int(has));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // cookieHandler(CookieHandler) -> Builder
    r.register(
        cls,
        "cookieHandler",
        "(Ljava/net/CookieHandler;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let has = match args.get(1) {
                Some(Value::Object(Some(_))) => 1,
                _ => 0,
            };
            ctx.set_field(this, 7, Value::Int(has));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // proxy(ProxySelector) -> Builder
    r.register(
        cls,
        "proxy",
        "(Ljava/net/ProxySelector;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let has = match args.get(1) {
                Some(Value::Object(Some(_))) => 1,
                _ => 0,
            };
            ctx.set_field(this, 5, Value::Int(has));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // authenticator(Authenticator) -> Builder
    r.register(
        cls,
        "authenticator",
        "(Ljava/net/Authenticator;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let has = match args.get(1) {
                Some(Value::Object(Some(_))) => 1,
                _ => 0,
            };
            ctx.set_field(this, 6, Value::Int(has));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // sslContext(SSLContext) -> Builder
    r.register(
        cls,
        "sslContext",
        "(Ljavax/net/ssl/SSLContext;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let has = match args.get(1) {
                Some(Value::Object(Some(_))) => 1,
                _ => 0,
            };
            ctx.set_field(this, 3, Value::Int(has));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // sslParameters(SSLParameters) -> Builder
    r.register(
        cls,
        "sslParameters",
        "(Ljavax/net/ssl/SSLParameters;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // sslParameters presence implies ssl is configured
            let has = match args.get(1) {
                Some(Value::Object(Some(_))) => 1,
                _ => 0,
            };
            ctx.set_field(this, 3, Value::Int(has));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // build() -> HttpClient
    r.register(cls, "build", "()Ljava/net/http/HttpClient;", |ctx, args| {
        // gc-common w19-d: the builder is rooted across the client's
        // allocation and read through its current address (it was read raw
        // after the allocation).
        let this = obj_arg(args, 0)?;
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        let client = alloc_http_client(&mut *scope)?;
        let this = scope.get(&this_h);
        let ctx = &mut *scope;
        // Copy builder fields into client
        for i in 0..8usize {
            let field_val = ctx.get_field(this, i);
            let dest = match i {
                0 => CLIENT_VERSION,
                1 => CLIENT_REDIRECT,
                2 => CLIENT_CONNECT_TIMEOUT, // Long field
                3 => CLIENT_HAS_SSL,
                4 => CLIENT_HAS_EXECUTOR,
                5 => CLIENT_HAS_PROXY,
                6 => CLIENT_HAS_AUTH,
                7 => CLIENT_HAS_COOKIE,
                _ => unreachable!(),
            };
            ctx.set_field(client, dest, field_val);
        }
        // `CLIENT_FOLLOW_REDIR` is slot 8 and the copy loop only reaches slot
        // 7, so it can only be derived here. See `followRedirects` above for
        // the slot collision this replaces.
        let policy = match ctx.get_field(client, CLIENT_REDIRECT) {
            Value::Int(n) => n,
            _ => REDIRECT_NEVER,
        };
        ctx.set_field(
            client,
            CLIENT_FOLLOW_REDIR,
            Value::Int(i32::from(policy != REDIRECT_NEVER)),
        );
        ctx.set_field(client, CLIENT_POOL_SIZE, Value::Int(DEFAULT_POOL_SIZE));
        Ok(Some(Value::Object(Some(client))))
    }, cratonvm_native_api::NativeKind::Bridge);
    }

// ---------------------------------------------------------------------------
// 3. java.net.http.HttpRequest
// ---------------------------------------------------------------------------

fn register_http_request(r: &mut NativeMethodRegistry) {
            let cls = "java/net/http/HttpRequest";
    // `HttpRequest` is an ABSTRACT class whose protected no-arg constructor is
    // empty in the real JDK; CratonVM's is an 8-slot synthetic that
    // `HttpClient.send()` reads by index (method, URI, header count, body
    // length). Seed it the way `newBuilder().build()` would — but only for the
    // synthetic shape; see `is_synthetic_shape`.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if is_synthetic_shape(ctx, this, "java/net/http/HttpRequest") {
            init_http_request_fields(ctx, this);
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // static newBuilder() -> HttpRequest$Builder
    r.register(
        cls,
        "newBuilder",
        "()Ljava/net/http/HttpRequest$Builder;",
        |ctx, _args| {
            let bld = alloc_http_request_builder(ctx)?;
            Ok(Some(Value::Object(Some(bld))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // static newBuilder(URI) -> HttpRequest$Builder
    r.register(
        cls,
        "newBuilder",
        "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            // gc-common w19-d: the URI argument is rooted across the
            // builder's allocation (it was read from `args`, a copy no
            // collection updates, after it).
            let uri = match args.first() {
                Some(Value::Object(Some(u))) => Some(*u),
                _ => None,
            };
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let uri_h = uri.map(|u| scope.root(u));
            let bld = alloc_http_request_builder(&mut *scope)?;
            let uri_now = uri_h.as_ref().map(|h| scope.get(h));
            scope.set_field(bld, REQ_URI, Value::Object(uri_now));
            Ok(Some(Value::Object(Some(bld))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // uri() -> URI
    r.register(cls, "uri", "()Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, REQ_URI)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // method() -> String
    r.register(cls, "method", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let m = match ctx.get_field(this, REQ_METHOD) {
            Value::Int(n) => n,
            _ => METHOD_GET,
        };
        let name = method_idx_to_name(m);
        let sv = ctx.create_string(name);
        Ok(Some(Value::Object(Some(sv))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // headers() -> HttpHeaders
    r.register(
        cls,
        "headers",
        "()Ljava/net/http/HttpHeaders;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let count = match ctx.get_field(this, REQ_HDR_COUNT) {
                Value::Int(n) => n,
                _ => 0,
            };
            let hdrs = alloc_http_headers(ctx, count, 0, 0)?;
            Ok(Some(Value::Object(Some(hdrs))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // bodyPublisher() -> Optional<BodyPublisher>
    r.register(
        cls,
        "bodyPublisher",
        "()Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let has_body = match ctx.get_field(this, REQ_HAS_BODY) {
                Value::Int(n) => n,
                _ => 0,
            };
            // Slot 0 of a `java.util.Optional` is `value`, a REFERENCE. The
            // publisher belongs THERE; parked at slot 1 nothing ever read it.
            // See
            // docs/feature-designs/native-dispatch-architecture.md
            //
            // gc-common w16-f: the length is read and the publisher built
            // BEFORE the Optional. The length used to be read off the request
            // after the Optional's allocation, and the publisher stored
            // through the Optional's address from before its own.
            let bp = if has_body == 1 {
                let len = match ctx.get_field(this, REQ_BODY_LEN) {
                    Value::Long(n) => n,
                    _ => 0,
                };
                Some(alloc_body_publisher(ctx, len)?)
            } else {
                None
            };
            let opt = optional_of(ctx, bp)?;
            Ok(Some(Value::Object(Some(opt))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // timeout() -> Optional<Duration>
    r.register(cls, "timeout", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = match ctx.get_field(this, REQ_TIMEOUT) {
            Value::Long(n) => n,
            _ => 0,
        };
        // Declared `Optional<Duration>`; slot 0 is the reference `value`. See
        // docs/feature-designs/native-dispatch-architecture.md
        //
        // The Duration first, then the Optional (gc-common w16-f).
        let dur = if ms > 0 {
            let nanos = ((ms % 1000) * 1_000_000) as i32;
            // `crate::util_time` is gated behind `#[cfg(feature = "synthetic-jdk")]`,
            // but this file compiles in EVERY configuration. The crate root
            // carries an ungated twin with the same job (and normalisation
            // the gated one lacks), so use it rather than gating the caller.
            Some(crate::lib_parts::matcher_captures::alloc_duration(ctx, ms / 1000, nanos)?)
        } else {
            None
        };
        let opt = optional_of(ctx, dur)?;
        Ok(Some(Value::Object(Some(opt))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // expectContinue() -> boolean
    r.register(cls, "expectContinue", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match ctx.get_field(this, REQ_EXPECT_100) {
            Value::Int(n) => n,
            _ => 0,
        };
        Ok(Some(Value::Int(v)))    }, cratonvm_native_api::NativeKind::Bridge);

    // version() -> Optional<HttpClient$Version>
    r.register(cls, "version", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ver = match ctx.get_field(this, REQ_VERSION) {
            Value::Int(n) => n,
            _ => 0,
        };
        // Slot 0 of a `java.util.Optional` is `value`, a REFERENCE, and this
        // is declared `Optional<HttpClient$Version>` -- so slot 0 must hold an
        // ENUM MIRROR or null, never an ordinal and never a flag. See
        // docs/feature-designs/native-dispatch-architecture.md.
        //
        // E2-1 left this unconditionally EMPTY, correctly: `REQ_VERSION` could
        // never be non-zero, because its only writer
        // (`HttpRequest$Builder.version(HttpClient$Version)`) matched
        // `Some(Value::Int(n))` against a reference argument and always took the
        // `_ => 0` arm. That builder is FIXED in this same file now, so
        // `REQ_VERSION` carries 1 = HTTP_1_1 / 2 = HTTP_2 and the present arm is
        // live. The `Optional` must be PINNED across `version_enum`'s
        // allocation: a moving young GC there relocates `opt` and the write
        // would land on a stale address (the native-stale-local family). See
        // docs/feature-designs/native-dispatch-architecture.md
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        if ver > 0 {
            let opt_pin = ctx.pin_native_root(opt);
            let mirror = version_enum(ctx, ver - 1);
            let opt = ctx.read_native_pin(opt_pin, opt);
            ctx.unpin_native_roots(opt_pin);
            match mirror? {
                Some(v @ Value::Object(Some(_))) => ctx.set_field(opt, 0, v),
                // `version_enum` could not mint the constant; an honestly-empty
                // Optional beats an undereferenceable slot 0.
                _ => ctx.set_field(opt, 0, Value::Object(None)),
            }
            return Ok(Some(Value::Object(Some(opt))));
        }
        ctx.set_field(opt, 0, Value::Object(None));
        Ok(Some(Value::Object(Some(opt))))
    }, cratonvm_native_api::NativeKind::Bridge);
    }

// ---------------------------------------------------------------------------
// 4. java.net.http.HttpRequest$Builder
// ---------------------------------------------------------------------------

fn register_http_request_builder(r: &mut NativeMethodRegistry) {
            let cls = "java/net/http/HttpRequest$Builder";
    // Interface in the real JDK; synthetic receivers only. Same reasoning as
    // `HttpClient$Builder`: `build()` copies slots 0..7 verbatim, and the
    // header-count accumulator (`header()`, `setHeader()`) increments whatever
    // it reads, so an unseeded `REQ_HDR_COUNT` never starts counting.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if is_synthetic_shape(ctx, this, "java/net/http/HttpRequest$Builder") {
            init_http_request_fields(ctx, this);
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // uri(URI) -> Builder
    r.register(
        cls,
        "uri",
        "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let uri_val = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field(this, REQ_URI, uri_val);
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // method(String, BodyPublisher) -> Builder
    r.register(
        cls,
        "method",
        "(Ljava/lang/String;Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m_idx = match args.get(1) {
                Some(Value::Object(Some(s))) => {
                    match ctx.read_string(*s).as_deref() {
                        Some("GET")     => METHOD_GET,
                        Some("POST")    => METHOD_POST,
                        Some("PUT")     => METHOD_PUT,
                        Some("DELETE")  => METHOD_DELETE,
                        Some("HEAD")    => METHOD_HEAD,
                        Some("PATCH")   => METHOD_PATCH,
                        Some("OPTIONS") => METHOD_OPTIONS,
                        _ => METHOD_GET,                    }                }
                _ => METHOD_GET,
            };
            ctx.set_field(this, REQ_METHOD, Value::Int(m_idx));
            let has_body = match args.get(2) {
                Some(Value::Object(Some(_))) => 1,
                _ => 0,
            };
            ctx.set_field(this, REQ_HAS_BODY, Value::Int(has_body));
            // Same as `POST` — carry the publisher's declared length instead of
            // recording only that a body exists.
            let body_len = body_publisher_arg_len(ctx, args.get(2));
            if let Some(len) = body_len {
                ctx.set_field(this, REQ_BODY_LEN, Value::Long(len));            }
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // GET() -> Builder
    r.register(
        cls,
        "GET",
        "()Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, REQ_METHOD, Value::Int(METHOD_GET));
            ctx.set_field(this, REQ_HAS_BODY, Value::Int(0));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // POST(BodyPublisher) -> Builder
    r.register(
        cls,
        "POST",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, REQ_METHOD, Value::Int(METHOD_POST));
            ctx.set_field(this, REQ_HAS_BODY, Value::Int(1));
            // The publisher's declared length used to be dropped on the floor,
            // so `bodyPublisher().get().contentLength()` answered 0 for every
            // body. `-1` (the streaming publishers' "unknown") is a legitimate
            // answer and is carried through unchanged.
            let body_len = body_publisher_arg_len(ctx, args.get(1));
            if let Some(len) = body_len {
                ctx.set_field(this, REQ_BODY_LEN, Value::Long(len));            }
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // PUT(BodyPublisher) -> Builder
    r.register(
        cls,
        "PUT",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, REQ_METHOD, Value::Int(METHOD_PUT));
            ctx.set_field(this, REQ_HAS_BODY, Value::Int(1));
            // Same as `POST` — see there.
            let body_len = body_publisher_arg_len(ctx, args.get(1));
            if let Some(len) = body_len {
                ctx.set_field(this, REQ_BODY_LEN, Value::Long(len));            }
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // DELETE() -> Builder
    r.register(
        cls,
        "DELETE",
        "()Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, REQ_METHOD, Value::Int(METHOD_DELETE));
            ctx.set_field(this, REQ_HAS_BODY, Value::Int(0));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // header(String, String) -> Builder
    r.register(
        cls,
        "header",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let prev = match ctx.get_field(this, REQ_HDR_COUNT) {
                Value::Int(n) => n,
                _ => 0,
            };
            ctx.set_field(this, REQ_HDR_COUNT, Value::Int(prev + 1));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // headers(String...) -> Builder  (varargs flattened)
    r.register(
        cls,
        "headers",
        "([Ljava/lang/String;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Each header is a name/value pair so count = args.len() / 2 extra
            let extra = ((args.len().saturating_sub(1)) / 2) as i32;
            let prev = match ctx.get_field(this, REQ_HDR_COUNT) {
                Value::Int(n) => n,
                _ => 0,
            };
            ctx.set_field(this, REQ_HDR_COUNT, Value::Int(prev + extra));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // setHeader(String, String) -> Builder
    r.register(
        cls,
        "setHeader",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let prev = match ctx.get_field(this, REQ_HDR_COUNT) {
                Value::Int(n) => n,
                _ => 0,
            };
            ctx.set_field(this, REQ_HDR_COUNT, Value::Int(prev + 1));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // timeout(Duration) -> Builder
    r.register(
        cls,
        "timeout",
        "(Ljava/time/Duration;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // `java.time.Duration` REFERENCE parameter — see the
            // `HttpClient$Builder.connectTimeout` twin above.
            let ms = duration_arg_millis(ctx, args.get(1));
            ctx.set_field(this, REQ_TIMEOUT, Value::Long(ms));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // expectContinue(boolean) -> Builder
    r.register(
        cls,
        "expectContinue",
        "(Z)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // NEGATIVE CONTROL — DO NOT "fix" this to match its neighbours.
            // The descriptor is `(Z)`: a primitive `boolean`, which really does
            // arrive as `Value::Int`. This is the one builder in the file for
            // which the `Some(Value::Int(n))` idiom is CORRECT, and
            // `e13_expect_continue_still_reads_a_primitive_boolean` fails by
            // name if anyone sweeps it up with the six that were wrong.
            let flag = match args.get(1) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            ctx.set_field(this, REQ_EXPECT_100, Value::Int(flag));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // version(HttpClient$Version) -> Builder
    r.register(
        cls,
        "version",
        "(Ljava/net/http/HttpClient$Version;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Enum REFERENCE parameter. `REQ_VERSION` encodes 0 = "no
            // override", 1 = HTTP_1_1, 2 = HTTP_2, so a decoded ordinal is
            // stored as `ordinal + 1`; an undecodable argument keeps 0, which
            // is what `HttpRequest.version()` reports as an empty `Optional`.
            let ver = match enum_arg_ordinal(ctx, args.get(1), VERSION_NAMES) {
                Some(ord) => ord + 1,
                None => 0,
            };
            ctx.set_field(this, REQ_VERSION, Value::Int(ver));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // build() -> HttpRequest
    r.register(
        cls,
        "build",
        "()Ljava/net/http/HttpRequest;",
        |ctx, args| {
            // gc-common w19-d: the builder (whose slot `REQ_URI` is a
            // reference) is rooted across the request's allocation and copied
            // from its current address.
            let this = obj_arg(args, 0)?;
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let req = alloc_http_request(&mut *scope)?;
            let this = scope.get(&this_h);
            let ctx = &mut *scope;
            for i in 0..8usize {
                let v = ctx.get_field(this, i);
                ctx.set_field(req, i, v);
            }
            Ok(Some(Value::Object(Some(req))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

// ---------------------------------------------------------------------------
// 5. java.net.http.HttpResponse
// ---------------------------------------------------------------------------

fn register_http_response(r: &mut NativeMethodRegistry) {
            let cls = "java/net/http/HttpResponse";
    // Interface in the real JDK; synthetic receivers only. Status 0 is the
    // same "no exchange happened" value the error paths in
    // `HttpClient.send()` construct, so a directly-constructed response is
    // indistinguishable from one that never reached the wire — rather than
    // one whose `statusCode()` silently falls through to the hard-coded 200
    // fallback below.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if is_synthetic_shape(ctx, this, "java/net/http/HttpResponse") {
            init_http_response_fields(ctx, this, 0);
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // statusCode() -> int
    r.register(cls, "statusCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sc = match ctx.get_field(this, RESP_STATUS) {
            Value::Int(n) => n,
            _ => 200,
        };
        Ok(Some(Value::Int(sc)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // body() -> T (returned as a String object)
    r.register(cls, "body", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let body = ctx.get_field(this, RESP_BODY_OBJ);
        match body {
            Value::Object(Some(_)) => Ok(Some(body)),
            _ => {
                // Fallback: return empty string
                let sv = ctx.create_string("");
                Ok(Some(Value::Object(Some(sv))))
            }
        }
    }, cratonvm_native_api::NativeKind::Bridge);

    // headers() -> HttpHeaders
    r.register(
        cls,
        "headers",
        "()Ljava/net/http/HttpHeaders;",
        |ctx, _args| {
            let hdrs = alloc_http_headers(ctx, 2, 1, 1)?;
            Ok(Some(Value::Object(Some(hdrs))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // uri() -> URI
    r.register(cls, "uri", "()Ljava/net/URI;", |ctx, _args| {
        let uri = try_alloc_concurrent_synthetic(ctx, "java/net/URI", 2)?;
        // JDK-ONLY-LAYOUT: this placeholder wrote `Int(0)` into slots 0 and 1,
        // which on a real `java.net.URI` are `scheme` and `fragment` — two
        // reference fields taking a primitive. The object is a placeholder
        // either way, so on a real layout it is simply left empty.
        if crate::net_phase_e::uri_has_synthetic_layout(ctx, uri) {
            ctx.set_field(uri, 0, Value::Int(0));
            ctx.set_field(uri, 1, Value::Int(0));
        } else {
            // `getPort()` is the JDK's own bytecode now (retired): `-1` is
            // "no port", and the zero a carrier starts with reads as port 0.
            ctx.set_field_by_name(uri, "port", Value::Int(-1));
        }
        Ok(Some(Value::Object(Some(uri))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // version() -> HttpClient$Version
    r.register(
        cls,
        "version",
        "()Ljava/net/http/HttpClient$Version;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let v = match ctx.get_field(this, RESP_VERSION) {
                Value::Int(n) => n,
                _ => HTTP_VERSION_2,
            };
            Ok(Some(Value::Int(v)))        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // request() -> HttpRequest
    r.register(
        cls,
        "request",
        "()Ljava/net/http/HttpRequest;",
        |ctx, args| {
            // gc-common w19-d: the method is read before the allocation, not
            // through the response's pre-allocation address after it.
            let this = obj_arg(args, 0)?;
            let m = match ctx.get_field(this, RESP_METHOD) {
                Value::Int(n) => n,
                _ => METHOD_GET,
            };
            let req = alloc_http_request(ctx)?;
            ctx.set_field(req, REQ_METHOD, Value::Int(m));
            Ok(Some(Value::Object(Some(req))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // previousResponse() -> Optional<HttpResponse>
    r.register(
        cls,
        "previousResponse",
        "()Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // The arity was already right and the TYPE was not: slot 0 of a
            // `java.util.Optional` is the reference `value`, so an `Int` there
            // reads as PRESENT. This is the row that shows the defect is the
            // flag and not the slot count. `RESP_HAS_PREV` is only ever written
            // `Int(0)` and no previous response is stored anywhere, so
            // `empty()` is the truthful answer. See
            // docs/feature-designs/native-dispatch-architecture.md
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            ctx.set_field(opt, 0, Value::Object(None));
            Ok(Some(Value::Object(Some(opt))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // sslSession() -> Optional<SSLSession>
    r.register(cls, "sslSession", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let has = match ctx.get_field(this, RESP_HAS_SSL) {
            Value::Int(n) => n,
            _ => 0,
        };
        // Slot 0 of a `java.util.Optional` is the reference `value`; the
        // session belongs THERE. See
        // docs/feature-designs/native-dispatch-architecture.md
        let mut opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        if has == 1 {
            // `opt` is held across the second allocation, which can move it.
            let opt_pin = ctx.pin_native_root(opt);
            let ssl = crate::t27_tls::alloc_session_carrier(ctx, 6);
            opt = ctx.read_native_pin(opt_pin, opt);
            ctx.unpin_native_roots(opt_pin);
            let ssl = ssl?;
            // gc-common w11-a: see `t27_tls::forget_session_rows`.
            crate::t27_tls::forget_session_rows(ctx, ssl);
            ctx.set_field(opt, 0, Value::Object(Some(ssl)));        } else {
            ctx.set_field(opt, 0, Value::Object(None));        }
        Ok(Some(Value::Object(Some(opt))))
    }, cratonvm_native_api::NativeKind::Bridge);
    }

// ---------------------------------------------------------------------------
// 6. java.net.http.HttpHeaders
// ---------------------------------------------------------------------------

fn register_http_headers(r: &mut NativeMethodRegistry) {
            let cls = "java/net/http/HttpHeaders";
    // `HttpHeaders` is a FINAL class in the real JDK (constructed only via
    // `HttpHeaders.of(...)`), so this too runs only for synthetic receivers.
    // Its backing state here is three counters rather than a map, and
    // `allValues`/`firstValue`/`map` all read them expecting an `Int`; seed
    // them to the empty-headers shape (no headers, no content-type, no
    // content-length) so those reads are answered by real state instead of by
    // each accessor's private `_ => 0` fallback.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if is_synthetic_shape(ctx, this, "java/net/http/HttpHeaders") {
            init_http_headers_fields(ctx, this, 0, 0, 0);
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // allValues(String name) -> List<String>
    r.register(
        cls,
        "allValues",
        "(Ljava/lang/String;)Ljava/util/List;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Shape guard — see `http_headers_is_counter_shape`. A receiver
            // this file did not mint keeps something OTHER than a counter in
            // slot 0, and slots 1/2 may not exist at all; reading them would
            // answer a fabricated `content-type` out of another minter's
            // memory. HotSpot's `allValues` on an absent name is `[]`, never
            // null, so declining is also the oracle-correct answer.
            // The guard is tested BEFORE the slot reads, not folded into a
            // match arm after them: `net_phase_e`'s receiver has exactly ONE
            // slot, so reading `HDR_HAS_CT`/`HDR_HAS_CL` off it is an
            // out-of-range field access, and a guard that fires afterwards has
            // already taken it.
            let (has_ct, has_cl) = if http_headers_is_counter_shape(ctx, this) {
                (
                    match ctx.get_field(this, HDR_HAS_CT) {
                        Value::Int(n) => n,
                        _ => 0,
                    },
                    match ctx.get_field(this, HDR_HAS_CL) {
                        Value::Int(n) => n,
                        _ => 0,
                    },
                )            } else {
                (0, 0)            };
            let queried = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let match_name = queried.to_lowercase();
            let value = if match_name == "content-type" && has_ct == 1 {
                Some("application/json")
            } else if match_name == "content-length" && has_cl == 1 {
                Some("20")
            } else {
                None
            };
            // The crate-wide `java/util/ArrayList` convention is "allocate,
            // then let native-collections establish the layout" — every
            // populated list in `phases_early.rs`, `logging_shims.rs` and
            // `jmx.rs` does it that way. The previous code hand-wrote
            // `Int(count)` into slot 0, which is where `native_al_init` puts
            // the backing `Object[]`: a type-punned slot that every registered
            // `native_al_*` reader then misreads, so the list reported a size
            // it could not produce an element for.
            //
            // `native_al_init` allocates the backing `Object[]` and
            // `create_string` allocates a `String`, so `list` is a bare Rust
            // local across two GC points. PIN it and read it back through the
            // pin, the way `HttpRequest$Builder.version` above already does —
            // a moving young GC otherwise relocates the list and every write
            // after the first lands on a stale address. The pin is released
            // BEFORE any `?`, so an error from either collections call cannot
            // leak it.
            let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
            let list_pin = ctx.pin_native_root(list);
            let inited =
                cratonvm_native_collections::native_al_init(ctx, &[Value::Object(Some(list))]);
            let list = ctx.read_native_pin(list_pin, list);
            let added = match value {
                Some(v) if inited.is_ok() => {
                    let sv = ctx.create_string(v);
                    let list = ctx.read_native_pin(list_pin, list);
                    // gc-common w11-c: the reclaiming door; `list` is re-read
                    // through its pin below.
                    cratonvm_native_collections::native_al_add_entry_pub(
                        ctx,
                        &[Value::Object(Some(list)), Value::Object(Some(sv))],
                    )
                }
                _ => Ok(None),
            };
            let list = ctx.read_native_pin(list_pin, list);
            ctx.unpin_native_roots(list_pin);
            // `Option<Value>` is `#[must_use]`; bind it away rather than
            // leaving a bare `expr?;` statement.
            let _ = inited?;
            let _ = added?;
            Ok(Some(Value::Object(Some(list))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // firstValue(String name) -> Optional<String>
    r.register(
        cls,
        "firstValue",
        "(Ljava/lang/String;)Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Shape guard — see `http_headers_is_counter_shape`. Declining for
            // a receiver this file did not mint yields `Optional.empty`, which
            // is HotSpot's answer for a name the headers do not carry; reading
            // slots 1/2 off a one-slot `String[]`-shaped object instead
            // invents a `content-type` that is nowhere in the request.
            // The guard is tested BEFORE the slot reads, not folded into a
            // match arm after them: `net_phase_e`'s receiver has exactly ONE
            // slot, so reading `HDR_HAS_CT`/`HDR_HAS_CL` off it is an
            // out-of-range field access, and a guard that fires afterwards has
            // already taken it.
            let (has_ct, has_cl) = if http_headers_is_counter_shape(ctx, this) {
                (
                    match ctx.get_field(this, HDR_HAS_CT) {
                        Value::Int(n) => n,
                        _ => 0,
                    },
                    match ctx.get_field(this, HDR_HAS_CL) {
                        Value::Int(n) => n,
                        _ => 0,
                    },
                )            } else {
                (0, 0)            };
            let queried = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            // Slot 0 of a `java.util.Optional` is the reference `value`; the
            // header string belongs THERE. Contrast `firstValueAsLong` just
            // below, which returns a `java.util.OptionalLong` -- whose REAL
            // layout IS `(boolean isPresent, long value)`, so its 2-slot
            // `Int`-at-0 form is correct and must not be swept up in this fix.
            // See
            // docs/feature-designs/native-dispatch-architecture.md
            //
            // gc-common w16-f: the String first, then the Optional
            // (`optional_of`); the String used to be stored through the
            // Optional's address from before the String's allocation.
            let lower = queried.to_lowercase();
            let value = if lower == "content-type" && has_ct == 1 {
                Some(ctx.create_string("application/json"))
            } else if lower == "content-length" && has_cl == 1 {
                Some(ctx.create_string("20"))
            } else {
                None
            };
            let opt = optional_of(ctx, value)?;
            Ok(Some(Value::Object(Some(opt))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // firstValueAsLong(String name) -> OptionalLong
    r.register(
        cls,
        "firstValueAsLong",
        "(Ljava/lang/String;)Ljava/util/OptionalLong;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Shape guard, tested BEFORE the slot read — see
            // `http_headers_is_counter_shape` and the note in `allValues`.
            let has_cl = if http_headers_is_counter_shape(ctx, this) {
                match ctx.get_field(this, HDR_HAS_CL) {
                    Value::Int(n) => n,
                    _ => 0,
                }
            } else {
                0
            };
            let queried = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/OptionalLong", 2)?;
            if queried.to_lowercase() == "content-length" && has_cl == 1 {
                ctx.set_field(opt, 0, Value::Int(1));
                ctx.set_field(opt, 1, Value::Long(20));
            } else {
                ctx.set_field(opt, 0, Value::Int(0));
                ctx.set_field(opt, 1, Value::Long(0));            }
            Ok(Some(Value::Object(Some(opt))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // map() -> Map<String, List<String>>
    r.register(cls, "map", "()Ljava/util/Map;", |ctx, args| {
        // Kept for the arity/NPE check the other four accessors also get from
        // it. `HDR_COUNT` is only a header COUNT on a receiver this file
        // minted; on `net_phase_e`'s it is the `String[]` of headers and on a
        // real `HttpHeaders.of(...)` it is the real `Map`. This body no longer
        // reads that slot at all — see below — which is the strongest form of
        // the guard the other three get from `http_headers_is_counter_shape`.
        let _this = obj_arg(args, 0)?;
        // The crate-wide `java/util/HashMap` convention is 3 slots plus
        // `native_map_init` (`phases_early.rs:393`, `logging_shims.rs:3516`,
        // `phases_late.rs:1550`, `reflect_annotations.rs:997`, and three
        // more — this file was the ONLY 2-slot allocation of the class in the
        // crate). The previous body hand-wrote `Int(count)` into slot 0 and
        // `Int(0)` into slot 1, so the object it returned could not be read by
        // any registered `java/util/HashMap` native: it claimed a size in a
        // slot the layout does not keep a size in, over entries it never had.
        // An honestly-empty, well-formed map beats a size no `get` can honour
        // — the counters carry no header NAMES, so there is nothing to put.
        let map = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
        let _ = cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(map))])?;
        Ok(Some(Value::Object(Some(map))))
    }, cratonvm_native_api::NativeKind::Bridge);
    }

// ---------------------------------------------------------------------------
// 7. java.net.http.HttpRequest$BodyPublisher
// ---------------------------------------------------------------------------

fn register_body_publisher(r: &mut NativeMethodRegistry) {
            let cls = "java/net/http/HttpRequest$BodyPublisher";
    let bps = "java/net/http/HttpRequest$BodyPublishers";
    // `BodyPublisher` is an interface in the real JDK; synthetic receivers
    // only. Slot 0 is the content length, read back through `contentLength()`
    // (`()J`), so it must start as a `Long`. Zero is the correct default: a
    // publisher with nothing pushed into it is `BodyPublishers.noBody()`,
    // whose contract is a known length of 0 (NOT the -1 "unknown length" the
    // streaming publishers use).
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if is_synthetic_shape(ctx, this, "java/net/http/HttpRequest$BodyPublisher") {
            init_body_publisher_fields(ctx, this, 0);
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // BodyPublishers.ofString(String) -> BodyPublisher
    r.register(
        bps,
        "ofString",
        "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, args| {
            let len = match args.get(0) {
                Some(Value::Object(Some(s))) => {
                    ctx.read_string(*s).map(|s| s.len() as i64).unwrap_or(0)                }
                _ => 0,
            };
            let bp = alloc_body_publisher(ctx, len)?;
            Ok(Some(Value::Object(Some(bp))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // BodyPublishers.ofByteArray(byte[]) -> BodyPublisher
    r.register(
        bps,
        "ofByteArray",
        "([B)Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, args| {
            // STATIC, so `args[0]` is the array itself — a REFERENCE, never the
            // `Value::Int` this used to match, so every `ofByteArray` publisher
            // reported `contentLength() == 0`. The `ofString` sibling directly
            // above always read its reference argument; this one did not.
            let len = match args.first() {
                Some(Value::Object(Some(arr))) => ctx.array_length(*arr) as i64,
                _ => 0,
            };
            let bp = alloc_body_publisher(ctx, len)?;
            Ok(Some(Value::Object(Some(bp))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // BodyPublishers.ofFile(Path) -> BodyPublisher
    r.register(
        bps,
        "ofFile",
        "(Ljava/nio/file/Path;)Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, _args| {
            let bp = alloc_body_publisher(ctx, -1)?; // unknown length
            Ok(Some(Value::Object(Some(bp))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // BodyPublishers.ofInputStream(Supplier) -> BodyPublisher
    r.register(
        bps,
        "ofInputStream",
        "(Ljava/util/function/Supplier;)Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, _args| {
            let bp = alloc_body_publisher(ctx, -1)?;
            Ok(Some(Value::Object(Some(bp))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // BodyPublishers.noBody() -> BodyPublisher
    r.register(
        bps,
        "noBody",
        "()Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, _args| {
            let bp = alloc_body_publisher(ctx, 0)?;
            Ok(Some(Value::Object(Some(bp))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // contentLength() -> long  (instance method on BodyPublisher)
    r.register(cls, "contentLength", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let len = match ctx.get_field(this, 0) {
            Value::Long(n) => n,
            _ => 0,
        };
        Ok(Some(Value::Long(len)))    }, cratonvm_native_api::NativeKind::Bridge);

    // subscribe(Flow.Subscriber) -> void — accepted and dropped.
    //
    // gc-common w16-f: this used to record the subscriber in a PROCESS-WIDE
    // map keyed by the BodySubscriber's bare identity hash (no VM, so two
    // VMs' subscribers could share a row), and to make the subscriber a
    // PERMANENT root (`register_var_handle_root`, which has no release) on
    // every call -- including a re-subscribe, whose predecessor stayed rooted.
    // Nothing ever read the map. So every `Flow.Subscriber` handed to a
    // synthetic `BodySubscriber`, and everything it reaches, lived for the
    // life of the VM for no effect. The map and the root are gone; the call
    // is still a no-op to its caller, exactly as it was observably before.
    r.register(
        cls,
        "subscribe",
        "(Ljava/util/concurrent/Flow$Subscriber;)V",
        |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::Bridge,
    );
    }

// ---------------------------------------------------------------------------
// 8. java.net.http.HttpResponse$BodyHandlers
// ---------------------------------------------------------------------------

fn register_body_handlers(r: &mut NativeMethodRegistry) {
            let bhs = "java/net/http/HttpResponse$BodyHandlers";

    // ofString() -> BodyHandler<String>
    r.register(
        bhs,
        "ofString",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_body_handler(ctx, 0)?;
            Ok(Some(Value::Object(Some(bh))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // ofByteArray() -> BodyHandler<byte[]>
    r.register(
        bhs,
        "ofByteArray",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_body_handler(ctx, 1)?;
            Ok(Some(Value::Object(Some(bh))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // ofFile(Path) -> BodyHandler<Path>
    r.register(
        bhs,
        "ofFile",
        "(Ljava/nio/file/Path;)Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_body_handler(ctx, 2)?;
            Ok(Some(Value::Object(Some(bh))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // ofLines() -> BodyHandler<Stream<String>>
    r.register(
        bhs,
        "ofLines",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_body_handler(ctx, 3)?;
            Ok(Some(Value::Object(Some(bh))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // discarding() -> BodyHandler<Void>
    r.register(
        bhs,
        "discarding",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_body_handler(ctx, 4)?;
            Ok(Some(Value::Object(Some(bh))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // replacing(Object) -> BodyHandler<T>
    r.register(
        bhs,
        "replacing",
        "(Ljava/lang/Object;)Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_body_handler(ctx, 5)?;
            Ok(Some(Value::Object(Some(bh))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

// ---------------------------------------------------------------------------
// 9. java.net.http.WebSocket
// ---------------------------------------------------------------------------

fn register_websocket(r: &mut NativeMethodRegistry) {
    // SyntheticStub: these send*/abort/request handlers fake success (return a
    // pre-completed CompletableFuture, flip local close flags) without doing
    // any real WebSocket framing or socket I/O; subprotocol() returns "unknown".
            let cls = "java/net/http/WebSocket";
    // Interface in the real JDK; synthetic receivers only. All-defaults is
    // wrong here in a way that inverts behaviour: `WS_STATE` starts at the
    // untyped default rather than at `WS_OPEN`, and the close-flag reads that
    // gate `sendText`/`sendClose`/`isInputClosed` would answer from it.
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if is_synthetic_shape(ctx, this, "java/net/http/WebSocket") {
            init_websocket_fields(ctx, this);
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // sendText(CharSequence, boolean last) -> CompletableFuture<WebSocket>
    r.register(
        cls,
        "sendText",
        "(Ljava/lang/CharSequence;Z)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // The socket rooted across the future's allocation (w16-f).
            completed_future_of(ctx, this)
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // sendBinary(ByteBuffer, boolean last) -> CompletableFuture<WebSocket>
    r.register(
        cls,
        "sendBinary",
        "(Ljava/nio/ByteBuffer;Z)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // The socket rooted across the future's allocation (w16-f).
            completed_future_of(ctx, this)
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // sendPing(ByteBuffer) -> CompletableFuture<WebSocket>
    r.register(
        cls,
        "sendPing",
        "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // The socket rooted across the future's allocation (w16-f).
            completed_future_of(ctx, this)
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // sendPong(ByteBuffer) -> CompletableFuture<WebSocket>
    r.register(
        cls,
        "sendPong",
        "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // The socket rooted across the future's allocation (w16-f).
            completed_future_of(ctx, this)
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // sendClose(int statusCode, String reason) -> CompletableFuture<WebSocket>
    r.register(
        cls,
        "sendClose",
        "(ILjava/lang/String;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Mark both directions closed
            ctx.set_field(this, WS_STATE, Value::Int(WS_CLOSING));
            ctx.set_field(this, WS_OUTPUT_CLOSED, Value::Int(1));
            // The socket rooted across the future's allocation (w16-f).
            completed_future_of(ctx, this)
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // request(long n) -> void — flow-control demand, accepted and dropped.
    //
    // gc-common w16-f: the demand used to accumulate in a PROCESS-WIDE map
    // keyed by the WebSocket's RAW ADDRESS -- stale after the first moving
    // collection, inherited by whatever object was later allocated at a dead
    // socket's address, one row per address for the life of the process, and
    // never read by anything. This stub delivers no frames whatever the
    // demand, so dropping the count changes nothing a caller can observe.
    r.register(cls, "request", "(J)V", |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::SyntheticStub);

    // subprotocol() -> String
    r.register(cls, "subprotocol", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = match ctx.get_field(this, WS_SUBPROTOCOL) {
            Value::Int(n) => n,
            _ => 0,
        };
        let proto = if idx == 0 { "" } else { "unknown" };
        let sv = ctx.create_string(proto);
        Ok(Some(Value::Object(Some(sv))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // isInputClosed() -> boolean
    r.register(cls, "isInputClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match ctx.get_field(this, WS_INPUT_CLOSED) {
            Value::Int(n) => n,
            _ => 0,
        };
        Ok(Some(Value::Int(v)))    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // isOutputClosed() -> boolean
    r.register(cls, "isOutputClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match ctx.get_field(this, WS_OUTPUT_CLOSED) {
            Value::Int(n) => n,
            _ => 0,
        };
        Ok(Some(Value::Int(v)))    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // abort() -> void
    r.register(cls, "abort", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, WS_STATE, Value::Int(WS_CLOSED));
        ctx.set_field(this, WS_OUTPUT_CLOSED, Value::Int(1));
        ctx.set_field(this, WS_INPUT_CLOSED, Value::Int(1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    }

// ---------------------------------------------------------------------------
// Helper: HTTP method index to string
// ---------------------------------------------------------------------------

fn method_idx_to_name(idx: i32) -> &'static str {
    match idx {
        METHOD_GET => "GET",
        METHOD_POST => "POST",
        METHOD_PUT => "PUT",
        METHOD_DELETE => "DELETE",
        METHOD_HEAD => "HEAD",
        METHOD_PATCH => "PATCH",
        METHOD_OPTIONS => "OPTIONS",
        _ => "GET",
    }
}

// ---------------------------------------------------------------------------
// Public registration entry point
// ---------------------------------------------------------------------------

/// Register all HTTP/2 client native methods into the given registry.
pub fn register_http2_natives(r: &mut NativeMethodRegistry) {
            register_http_client(r);
    register_http_client_builder(r);
    register_http_request(r);
    register_http_request_builder(r);
    register_http_response(r);
    register_http_headers(r);
    register_body_publisher(r);
    register_body_handlers(r);
    register_websocket(r);
    }

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod http2_tests;
