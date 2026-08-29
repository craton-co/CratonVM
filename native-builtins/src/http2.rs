// SPDX-License-Identifier: Apache-2.0
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
/// `--jdk-only` this file's registrar does not run at all, so the guard is
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
            let cf =
                try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2)?;
            ctx.set_field(cf, 0, Value::Object(Some(resp)));
            ctx.set_field(cf, 1, Value::Int(1));
            return Ok(Some(Value::Object(Some(cf))));
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
            let cf =
                try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2)?;
            ctx.set_field(cf, 0, Value::Object(Some(resp)));
            ctx.set_field(cf, 1, Value::Int(1));
            return Ok(Some(Value::Object(Some(cf))));
        }
    };
    let (host, port, path) = match extract_uri_parts(ctx, uri_obj) {
        Some(parts) => parts,
        None => {
            let resp = alloc_http_response(ctx, 0)?;
            let cf =
                try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2)?;
            ctx.set_field(cf, 0, Value::Object(Some(resp)));
            ctx.set_field(cf, 1, Value::Int(1));
            return Ok(Some(Value::Object(Some(cf))));
        }
    };
    let use_tls = uri_wants_tls(ctx, uri_obj, port);
    // [HIGH fix nb-http2 (1)] Don't silently drop caller headers/body.
    ensure_no_dropped_payload(ctx, req)?;
    // [VULN fix nb-http2 (2)] Reject CR/LF/NUL in request-line values.
    validate_no_crlf("method", method_str)?;
    validate_no_crlf("request-target", &path)?;
    validate_no_crlf("Host header", &host)?;
    let result = if use_tls {
        https_request(&host, port, method_str, &path)
    } else {
        http11_request(&host, port, method_str, &path)
    };
    let resp = match result {
        Ok((status, body)) => {
            let r = alloc_http_response(ctx, status)?;
            let body_str = ctx.create_string(&body);
            ctx.set_field(r, RESP_BODY_OBJ, Value::Object(Some(body_str)));
            r
        }
        Err(_) => alloc_http_response(ctx, 0)?,
    };
    let cf = try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2)?;
    ctx.set_field(cf, 0, Value::Object(Some(resp)));
    ctx.set_field(cf, 1, Value::Int(1)); // completed
    Ok(Some(Value::Object(Some(cf))))
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
// See docs/known-issues/jdk-only/E13-1-the-six-builders-decode-a-reference-as-an-int.md
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
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
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
    });

    // static newHttpClient() -> HttpClient
    r.register(
        cls,
        "newHttpClient",
        "()Ljava/net/http/HttpClient;",
        |ctx, _args| {
            let obj = alloc_http_client(ctx)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // static newBuilder() -> HttpClient$Builder
    r.register(
        cls,
        "newBuilder",
        "()Ljava/net/http/HttpClient$Builder;",
        |ctx, _args| {
            let bld = alloc_http_client_builder(ctx)?;
            Ok(Some(Value::Object(Some(bld))))
        },
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

            // Perform real HTTP request (with TLS for HTTPS)
            let result = if use_tls {
                https_request(&host, port, method_str, &path)
            } else {
                http11_request(&host, port, method_str, &path)
            };
            match result {
                Ok((status, body)) => {
                    let resp = alloc_http_response(ctx, status)?;
                    let body_str = ctx.create_string(&body);
                    // Store body string on a dedicated field — we use RESP_HAS_PREV (3)
                    // as body_obj since it's unused for real responses
                    ctx.set_field(resp, RESP_BODY_OBJ, Value::Object(Some(body_str)));
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
        },
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
        http2_send_async,
    );

    // sendAsync(HttpRequest, BodyHandler) -> CompletableFuture<HttpResponse>
    // Performs the same real HTTP request as send(), then wraps result in a completed CF.
    r.register(
        cls,
        "sendAsync",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/util/concurrent/CompletableFuture;",
        http2_send_async,
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
        },
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
            // docs/known-issues/jdk-only/D3-2-http2-optional-reference-layout.md
            // Declared `Optional<Duration>`; the same body as
            // `http_client.rs:1632`'s `connectTimeout`.
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            if ms > 0 {
                let nanos = ((ms % 1000) * 1_000_000) as i32;
                // `crate::util_time` is gated behind `#[cfg(feature = "synthetic-jdk")]`,
                // but this file compiles in EVERY configuration. The crate root
                // carries an ungated twin with the same job (and normalisation
                // the gated one lacks), so use it rather than gating the caller.
                let dur = crate::alloc_duration(ctx, ms / 1000, nanos)?;
                ctx.set_field(opt, 0, Value::Object(Some(dur)));
            } else {
                ctx.set_field(opt, 0, Value::Object(None));
            }
            Ok(Some(Value::Object(Some(opt))))
        },
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
        },
    );

    // executor() -> Optional<Executor>
    r.register(cls, "executor", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Slot 0 of a `java.util.Optional` is `value`, a REFERENCE -- an `Int`
        // there reads as PRESENT and `get()` returns the flag. The arity was
        // already right here and the TYPE was not, which is what shows the
        // defect is the flag rather than the slot count. See
        // docs/known-issues/jdk-only/D3-2-http2-optional-reference-layout.md.
        // The synthetic client stores a presence FLAG and never the `Executor`
        // itself (`CLIENT_HAS_EXECUTOR` is only ever written `Int(0)` by
        // `alloc_http_client`, then copied by `Builder.build()`), so `empty()`
        // is the only answer this layout can give truthfully: a MISSING answer
        // where there was a wrong one.
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        ctx.set_field(opt, 0, Value::Object(None));
        Ok(Some(Value::Object(Some(opt))))
    });

    // cookieHandler() -> Optional<CookieHandler>
    r.register(
        cls,
        "cookieHandler",
        "()Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // See `executor()` above and
            // docs/known-issues/jdk-only/D3-2-http2-optional-reference-layout.md:
            // slot 0 is the reference `value`, and this layout holds a flag
            // rather than the `CookieHandler`, so `empty()` is the only
            // truthful answer.
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            ctx.set_field(opt, 0, Value::Object(None));
            Ok(Some(Value::Object(Some(opt))))
        },
    );

    // proxy() -> Optional<ProxySelector>
    r.register(cls, "proxy", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // See `executor()` above: slot 0 is the reference `value`, and this
        // layout holds a flag rather than the `ProxySelector`.
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        ctx.set_field(opt, 0, Value::Object(None));
        Ok(Some(Value::Object(Some(opt))))
    });

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
        },
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
        },
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
            Ok(Some(Value::Object(Some(params))))
        },
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
        },
    );
    r.set_category(__prev_cat);
    ()
}

// ---------------------------------------------------------------------------
// 2. java.net.http.HttpClient$Builder
// ---------------------------------------------------------------------------

fn register_http_client_builder(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
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
    });

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
        },
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
        },
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
        },
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
        },
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
        },
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
        },
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
        },
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
        },
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
        },
    );

    // build() -> HttpClient
    r.register(cls, "build", "()Ljava/net/http/HttpClient;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let client = alloc_http_client(ctx)?;
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
    });
    r.set_category(__prev_cat);
    ()
}

// ---------------------------------------------------------------------------
// 3. java.net.http.HttpRequest
// ---------------------------------------------------------------------------

fn register_http_request(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
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
    });

    // static newBuilder() -> HttpRequest$Builder
    r.register(
        cls,
        "newBuilder",
        "()Ljava/net/http/HttpRequest$Builder;",
        |ctx, _args| {
            let bld = alloc_http_request_builder(ctx)?;
            Ok(Some(Value::Object(Some(bld))))
        },
    );

    // static newBuilder(URI) -> HttpRequest$Builder
    r.register(
        cls,
        "newBuilder",
        "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let bld = alloc_http_request_builder(ctx)?;
            let uri_val = args.get(0).copied().unwrap_or(Value::Object(None));
            ctx.set_field(bld, REQ_URI, uri_val);
            Ok(Some(Value::Object(Some(bld))))
        },
    );

    // uri() -> URI
    r.register(cls, "uri", "()Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, REQ_URI)))
    });

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
    });

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
        },
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
            // docs/known-issues/jdk-only/D3-2-http2-optional-reference-layout.md
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            if has_body == 1 {
                let len = match ctx.get_field(this, REQ_BODY_LEN) {
                    Value::Long(n) => n,
                    _ => 0,
                };
                let bp = alloc_body_publisher(ctx, len)?;
                ctx.set_field(opt, 0, Value::Object(Some(bp)));
            } else {
                ctx.set_field(opt, 0, Value::Object(None));
            }
            Ok(Some(Value::Object(Some(opt))))
        },
    );

    // timeout() -> Optional<Duration>
    r.register(cls, "timeout", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = match ctx.get_field(this, REQ_TIMEOUT) {
            Value::Long(n) => n,
            _ => 0,
        };
        // Declared `Optional<Duration>`; slot 0 is the reference `value`. See
        // docs/known-issues/jdk-only/D3-2-http2-optional-reference-layout.md
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        if ms > 0 {
            let nanos = ((ms % 1000) * 1_000_000) as i32;
            // `crate::util_time` is gated behind `#[cfg(feature = "synthetic-jdk")]`,
            // but this file compiles in EVERY configuration. The crate root
            // carries an ungated twin with the same job (and normalisation
            // the gated one lacks), so use it rather than gating the caller.
            let dur = crate::alloc_duration(ctx, ms / 1000, nanos)?;
            ctx.set_field(opt, 0, Value::Object(Some(dur)));
        } else {
            ctx.set_field(opt, 0, Value::Object(None));
        }
        Ok(Some(Value::Object(Some(opt))))
    });

    // expectContinue() -> boolean
    r.register(cls, "expectContinue", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match ctx.get_field(this, REQ_EXPECT_100) {
            Value::Int(n) => n,
            _ => 0,
        };
        Ok(Some(Value::Int(v)))
    });

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
        // docs/known-issues/jdk-only/D3-2-http2-optional-reference-layout.md.
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
        // docs/known-issues/jdk-only/E13-1-the-six-builders-decode-a-reference-as-an-int.md
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
    });
    r.set_category(__prev_cat);
    ()
}

// ---------------------------------------------------------------------------
// 4. java.net.http.HttpRequest$Builder
// ---------------------------------------------------------------------------

fn register_http_request_builder(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
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
    });

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
        },
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
                        _ => METHOD_GET,
                    }
                }
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
                ctx.set_field(this, REQ_BODY_LEN, Value::Long(len));
            }
            Ok(Some(Value::Object(Some(this))))
        },
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
        },
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
                ctx.set_field(this, REQ_BODY_LEN, Value::Long(len));
            }
            Ok(Some(Value::Object(Some(this))))
        },
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
                ctx.set_field(this, REQ_BODY_LEN, Value::Long(len));
            }
            Ok(Some(Value::Object(Some(this))))
        },
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
        },
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
        },
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
        },
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
        },
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
        },
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
        },
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
        },
    );

    // build() -> HttpRequest
    r.register(
        cls,
        "build",
        "()Ljava/net/http/HttpRequest;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let req = alloc_http_request(ctx)?;
            for i in 0..8usize {
                let v = ctx.get_field(this, i);
                ctx.set_field(req, i, v);
            }
            Ok(Some(Value::Object(Some(req))))
        },
    );
    r.set_category(__prev_cat);
    ()
}

// ---------------------------------------------------------------------------
// 5. java.net.http.HttpResponse
// ---------------------------------------------------------------------------

fn register_http_response(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
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
    });

    // statusCode() -> int
    r.register(cls, "statusCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sc = match ctx.get_field(this, RESP_STATUS) {
            Value::Int(n) => n,
            _ => 200,
        };
        Ok(Some(Value::Int(sc)))
    });

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
    });

    // headers() -> HttpHeaders
    r.register(
        cls,
        "headers",
        "()Ljava/net/http/HttpHeaders;",
        |ctx, _args| {
            let hdrs = alloc_http_headers(ctx, 2, 1, 1)?;
            Ok(Some(Value::Object(Some(hdrs))))
        },
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
        }
        Ok(Some(Value::Object(Some(uri))))
    });

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
            Ok(Some(Value::Int(v)))
        },
    );

    // request() -> HttpRequest
    r.register(
        cls,
        "request",
        "()Ljava/net/http/HttpRequest;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let req = alloc_http_request(ctx)?;
            let m = match ctx.get_field(this, RESP_METHOD) {
                Value::Int(n) => n,
                _ => METHOD_GET,
            };
            ctx.set_field(req, REQ_METHOD, Value::Int(m));
            Ok(Some(Value::Object(Some(req))))
        },
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
            // docs/known-issues/jdk-only/D3-2-http2-optional-reference-layout.md
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            ctx.set_field(opt, 0, Value::Object(None));
            Ok(Some(Value::Object(Some(opt))))
        },
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
        // docs/known-issues/jdk-only/D3-2-http2-optional-reference-layout.md
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        if has == 1 {
            let ssl = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSession", 6)?;
            ctx.set_field(opt, 0, Value::Object(Some(ssl)));
        } else {
            ctx.set_field(opt, 0, Value::Object(None));
        }
        Ok(Some(Value::Object(Some(opt))))
    });
    r.set_category(__prev_cat);
    ()
}

// ---------------------------------------------------------------------------
// 6. java.net.http.HttpHeaders
// ---------------------------------------------------------------------------

fn register_http_headers(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
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
    });

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
                )
            } else {
                (0, 0)
            };
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
                    cratonvm_native_collections::native_al_add(
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
        },
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
                )
            } else {
                (0, 0)
            };
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
            // docs/known-issues/jdk-only/D3-2-http2-optional-reference-layout.md
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            let lower = queried.to_lowercase();
            if lower == "content-type" && has_ct == 1 {
                let sv = ctx.create_string("application/json");
                ctx.set_field(opt, 0, Value::Object(Some(sv)));
            } else if lower == "content-length" && has_cl == 1 {
                let sv = ctx.create_string("20");
                ctx.set_field(opt, 0, Value::Object(Some(sv)));
            } else {
                ctx.set_field(opt, 0, Value::Object(None));
            }
            Ok(Some(Value::Object(Some(opt))))
        },
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
                ctx.set_field(opt, 1, Value::Long(0));
            }
            Ok(Some(Value::Object(Some(opt))))
        },
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
    });
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// 7. java.net.http.HttpRequest$BodyPublisher
// ---------------------------------------------------------------------------

fn register_body_publisher(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
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
    });

    // BodyPublishers.ofString(String) -> BodyPublisher
    r.register(
        bps,
        "ofString",
        "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, args| {
            let len = match args.get(0) {
                Some(Value::Object(Some(s))) => {
                    ctx.read_string(*s).map(|s| s.len() as i64).unwrap_or(0)
                }
                _ => 0,
            };
            let bp = alloc_body_publisher(ctx, len)?;
            Ok(Some(Value::Object(Some(bp))))
        },
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
        },
    );

    // BodyPublishers.ofFile(Path) -> BodyPublisher
    r.register(
        bps,
        "ofFile",
        "(Ljava/nio/file/Path;)Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, _args| {
            let bp = alloc_body_publisher(ctx, -1)?; // unknown length
            Ok(Some(Value::Object(Some(bp))))
        },
    );

    // BodyPublishers.ofInputStream(Supplier) -> BodyPublisher
    r.register(
        bps,
        "ofInputStream",
        "(Ljava/util/function/Supplier;)Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, _args| {
            let bp = alloc_body_publisher(ctx, -1)?;
            Ok(Some(Value::Object(Some(bp))))
        },
    );

    // BodyPublishers.noBody() -> BodyPublisher
    r.register(
        bps,
        "noBody",
        "()Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, _args| {
            let bp = alloc_body_publisher(ctx, 0)?;
            Ok(Some(Value::Object(Some(bp))))
        },
    );

    // contentLength() -> long  (instance method on BodyPublisher)
    r.register(cls, "contentLength", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let len = match ctx.get_field(this, 0) {
            Value::Long(n) => n,
            _ => 0,
        };
        Ok(Some(Value::Long(len)))
    });

    // subscribe(Flow.Subscriber) -> void — record the subscriber in a global map.
    r.register(
        cls,
        "subscribe",
        "(Ljava/util/concurrent/Flow$Subscriber;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let subscriber = match args.get(1) {
                Some(Value::Object(Some(s))) => Some(*s),
                _ => None,
            };
            // GC-stable keying + rooted value (cce0079 follow-up): the raw
            // address key went stale after any moving GC and the subscriber
            // value was neither rooted nor remapped. Key by identity hash;
            // keep the value alive + remapped via the VarHandle-root
            // registry; readers must resolve through
            // `read_var_handle_root(skey)` for the CURRENT address.
            let key = ctx.identity_hash_code(this) as u64;
            if let Some(s) = subscriber {
                ctx.register_var_handle_root(s);
                let skey = ctx.identity_hash_code(s);
                body_subscriber_subscribers().lock().insert(key, (skey, s));
            } else {
                body_subscriber_subscribers().lock().remove(&key);
            }
            Ok(None)
        },
    );
    r.set_category(__prev_cat);
    ()
}

/// Process-wide map from BodySubscriber identity → downstream Flow.Subscriber.
/// Populated by `BodySubscriber.subscribe(Flow$Subscriber)` and queried by code
/// that wants to forward data into the reactive pipeline.
///
/// GC note (RESOLVED, cce0079 follow-up): keyed by the BodySubscriber's
/// identity hash; the subscriber value is stored as a
/// `(identity_key, last_addr)` VarHandle-root pair — registered via
/// `ctx.register_var_handle_root` at subscribe, so it stays alive and
/// registry-remapped across moving GCs. Readers must resolve the CURRENT
/// address via `ctx.read_var_handle_root(identity_key)`, falling back to
/// the stored address only when the registry has no entry.
fn body_subscriber_subscribers(
) -> &'static parking_lot::Mutex<std::collections::HashMap<u64, (i32, ObjectRef)>> {
    use std::sync::OnceLock;
    static MAP: OnceLock<parking_lot::Mutex<std::collections::HashMap<u64, (i32, ObjectRef)>>> =
        OnceLock::new();
    MAP.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// Process-wide map from BodySubscriber identity → outstanding demand (i64 saturating).
fn body_subscriber_demand() -> &'static parking_lot::Mutex<std::collections::HashMap<u64, i64>> {
    use std::sync::OnceLock;
    static MAP: OnceLock<parking_lot::Mutex<std::collections::HashMap<u64, i64>>> = OnceLock::new();
    MAP.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

// ---------------------------------------------------------------------------
// 8. java.net.http.HttpResponse$BodyHandlers
// ---------------------------------------------------------------------------

fn register_body_handlers(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let bhs = "java/net/http/HttpResponse$BodyHandlers";

    // ofString() -> BodyHandler<String>
    r.register(
        bhs,
        "ofString",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_body_handler(ctx, 0)?;
            Ok(Some(Value::Object(Some(bh))))
        },
    );

    // ofByteArray() -> BodyHandler<byte[]>
    r.register(
        bhs,
        "ofByteArray",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_body_handler(ctx, 1)?;
            Ok(Some(Value::Object(Some(bh))))
        },
    );

    // ofFile(Path) -> BodyHandler<Path>
    r.register(
        bhs,
        "ofFile",
        "(Ljava/nio/file/Path;)Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_body_handler(ctx, 2)?;
            Ok(Some(Value::Object(Some(bh))))
        },
    );

    // ofLines() -> BodyHandler<Stream<String>>
    r.register(
        bhs,
        "ofLines",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_body_handler(ctx, 3)?;
            Ok(Some(Value::Object(Some(bh))))
        },
    );

    // discarding() -> BodyHandler<Void>
    r.register(
        bhs,
        "discarding",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_body_handler(ctx, 4)?;
            Ok(Some(Value::Object(Some(bh))))
        },
    );

    // replacing(Object) -> BodyHandler<T>
    r.register(
        bhs,
        "replacing",
        "(Ljava/lang/Object;)Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_body_handler(ctx, 5)?;
            Ok(Some(Value::Object(Some(bh))))
        },
    );
    r.set_category(__prev_cat);
    ()
}

// ---------------------------------------------------------------------------
// 9. java.net.http.WebSocket
// ---------------------------------------------------------------------------

fn register_websocket(r: &mut NativeMethodRegistry) {
    // SyntheticStub: these send*/abort/request handlers fake success (return a
    // pre-completed CompletableFuture, flip local close flags) without doing
    // any real WebSocket framing or socket I/O; subprotocol() returns "unknown".
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
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
    });

    // sendText(CharSequence, boolean last) -> CompletableFuture<WebSocket>
    r.register(
        cls,
        "sendText",
        "(Ljava/lang/CharSequence;Z)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cf =
                try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2)?;
            ctx.set_field(cf, 0, Value::Object(Some(this)));
            ctx.set_field(cf, 1, Value::Int(1));
            Ok(Some(Value::Object(Some(cf))))
        },
    );

    // sendBinary(ByteBuffer, boolean last) -> CompletableFuture<WebSocket>
    r.register(
        cls,
        "sendBinary",
        "(Ljava/nio/ByteBuffer;Z)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cf =
                try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2)?;
            ctx.set_field(cf, 0, Value::Object(Some(this)));
            ctx.set_field(cf, 1, Value::Int(1));
            Ok(Some(Value::Object(Some(cf))))
        },
    );

    // sendPing(ByteBuffer) -> CompletableFuture<WebSocket>
    r.register(
        cls,
        "sendPing",
        "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cf =
                try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2)?;
            ctx.set_field(cf, 0, Value::Object(Some(this)));
            ctx.set_field(cf, 1, Value::Int(1));
            Ok(Some(Value::Object(Some(cf))))
        },
    );

    // sendPong(ByteBuffer) -> CompletableFuture<WebSocket>
    r.register(
        cls,
        "sendPong",
        "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cf =
                try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2)?;
            ctx.set_field(cf, 0, Value::Object(Some(this)));
            ctx.set_field(cf, 1, Value::Int(1));
            Ok(Some(Value::Object(Some(cf))))
        },
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
            let cf =
                try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/CompletableFuture", 2)?;
            ctx.set_field(cf, 0, Value::Object(Some(this)));
            ctx.set_field(cf, 1, Value::Int(1));
            Ok(Some(Value::Object(Some(cf))))
        },
    );

    // request(long n) -> void  (flow control — accumulate demand in process-wide map)
    r.register(cls, "request", "(J)V", |_ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        let n = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        if n < 0 {
            return Ok(None); // ignore negative demand silently in the http2.rs path
        }
        let key = this.as_ptr() as u64;
        let mut map = body_subscriber_demand().lock();
        let current = *map.get(&key).unwrap_or(&0);
        map.insert(key, current.saturating_add(n));
        Ok(None)
    });

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
    });

    // isInputClosed() -> boolean
    r.register(cls, "isInputClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match ctx.get_field(this, WS_INPUT_CLOSED) {
            Value::Int(n) => n,
            _ => 0,
        };
        Ok(Some(Value::Int(v)))
    });

    // isOutputClosed() -> boolean
    r.register(cls, "isOutputClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match ctx.get_field(this, WS_OUTPUT_CLOSED) {
            Value::Int(n) => n,
            _ => 0,
        };
        Ok(Some(Value::Int(v)))
    });

    // abort() -> void
    r.register(cls, "abort", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, WS_STATE, Value::Int(WS_CLOSED));
        ctx.set_field(this, WS_OUTPUT_CLOSED, Value::Int(1));
        ctx.set_field(this, WS_INPUT_CLOSED, Value::Int(1));
        Ok(None)
    });
    r.set_category(__prev_cat);
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
pub(crate) fn register_http2_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    register_http_client(r);
    register_http_client_builder(r);
    register_http_request(r);
    register_http_request_builder(r);
    register_http_response(r);
    register_http_headers(r);
    register_body_publisher(r);
    register_body_handlers(r);
    register_websocket(r);
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod http2_tests {
    use super::*;
    use cratonvm_native_api::NativeMethodRegistry;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // --- Registration tests ------------------------------------------------

    #[test]
    fn test_http_client_init_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find("java/net/http/HttpClient", "<init>", "()V")
            .is_some());
    }

    #[test]
    fn test_http_client_new_builder_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpClient",
                "newBuilder",
                "()Ljava/net/http/HttpClient$Builder;"
            )
            .is_some());
    }

    #[test]
    fn test_http_client_new_http_client_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpClient",
                "newHttpClient",
                "()Ljava/net/http/HttpClient;"
            )
            .is_some());
    }

    #[test]
    fn test_http_client_send_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpClient",
            "send",
            "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;"
        ).is_some());
    }

    #[test]
    fn test_http_client_send_async_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find(
            "java/net/http/HttpClient",
            "sendAsync",
            "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/util/concurrent/CompletableFuture;"
        ).is_some());
    }

    #[test]
    fn test_http_client_version_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpClient",
                "version",
                "()Ljava/net/http/HttpClient$Version;"
            )
            .is_some());
    }

    #[test]
    fn test_http_client_follow_redirects_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpClient",
                "followRedirects",
                "()Ljava/net/http/HttpClient$Redirect;"
            )
            .is_some());
    }

    #[test]
    fn test_http_client_connect_timeout_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpClient",
                "connectTimeout",
                "()Ljava/util/Optional;"
            )
            .is_some());
    }

    #[test]
    fn test_http_client_ssl_context_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpClient",
                "sslContext",
                "()Ljavax/net/ssl/SSLContext;"
            )
            .is_some());
    }

    #[test]
    fn test_http_client_new_websocket_builder_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpClient",
                "newWebSocketBuilder",
                "()Ljava/net/http/WebSocket$Builder;"
            )
            .is_some());
    }

    #[test]
    fn test_http_client_builder_build_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpClient$Builder",
                "build",
                "()Ljava/net/http/HttpClient;"
            )
            .is_some());
    }

    #[test]
    fn test_http_client_builder_version_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpClient$Builder",
                "version",
                "(Ljava/net/http/HttpClient$Version;)Ljava/net/http/HttpClient$Builder;"
            )
            .is_some());
    }

    #[test]
    fn test_http_client_builder_follow_redirects_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpClient$Builder",
                "followRedirects",
                "(Ljava/net/http/HttpClient$Redirect;)Ljava/net/http/HttpClient$Builder;"
            )
            .is_some());
    }

    #[test]
    fn test_http_request_new_builder_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpRequest",
                "newBuilder",
                "()Ljava/net/http/HttpRequest$Builder;"
            )
            .is_some());
    }

    #[test]
    fn test_http_request_new_builder_uri_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpRequest",
                "newBuilder",
                "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;"
            )
            .is_some());
    }

    #[test]
    fn test_http_request_method_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpRequest",
                "method",
                "()Ljava/lang/String;"
            )
            .is_some());
    }

    #[test]
    fn test_http_request_builder_get_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpRequest$Builder",
                "GET",
                "()Ljava/net/http/HttpRequest$Builder;"
            )
            .is_some());
    }

    #[test]
    fn test_http_request_builder_post_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpRequest$Builder",
                "POST",
                "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;"
            )
            .is_some());
    }

    #[test]
    fn test_http_request_builder_put_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpRequest$Builder",
                "PUT",
                "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;"
            )
            .is_some());
    }

    #[test]
    fn test_http_request_builder_delete_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpRequest$Builder",
                "DELETE",
                "()Ljava/net/http/HttpRequest$Builder;"
            )
            .is_some());
    }

    #[test]
    fn test_http_request_builder_header_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpRequest$Builder",
                "header",
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/http/HttpRequest$Builder;"
            )
            .is_some());
    }

    #[test]
    fn test_http_request_builder_build_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpRequest$Builder",
                "build",
                "()Ljava/net/http/HttpRequest;"
            )
            .is_some());
    }

    #[test]
    fn test_http_response_status_code_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find("java/net/http/HttpResponse", "statusCode", "()I")
            .is_some());
    }

    #[test]
    fn test_http_response_body_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find("java/net/http/HttpResponse", "body", "()Ljava/lang/Object;")
            .is_some());
    }

    #[test]
    fn test_http_response_headers_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpResponse",
                "headers",
                "()Ljava/net/http/HttpHeaders;"
            )
            .is_some());
    }

    #[test]
    fn test_http_response_version_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpResponse",
                "version",
                "()Ljava/net/http/HttpClient$Version;"
            )
            .is_some());
    }

    #[test]
    fn test_http_response_previous_response_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpResponse",
                "previousResponse",
                "()Ljava/util/Optional;"
            )
            .is_some());
    }

    #[test]
    fn test_http_headers_all_values_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpHeaders",
                "allValues",
                "(Ljava/lang/String;)Ljava/util/List;"
            )
            .is_some());
    }

    #[test]
    fn test_http_headers_first_value_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpHeaders",
                "firstValue",
                "(Ljava/lang/String;)Ljava/util/Optional;"
            )
            .is_some());
    }

    #[test]
    fn test_http_headers_first_value_as_long_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpHeaders",
                "firstValueAsLong",
                "(Ljava/lang/String;)Ljava/util/OptionalLong;"
            )
            .is_some());
    }

    #[test]
    fn test_http_headers_map_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find("java/net/http/HttpHeaders", "map", "()Ljava/util/Map;")
            .is_some());
    }

    #[test]
    fn test_body_publishers_of_string_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpRequest$BodyPublishers",
                "ofString",
                "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;"
            )
            .is_some());
    }

    #[test]
    fn test_body_publishers_no_body_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpRequest$BodyPublishers",
                "noBody",
                "()Ljava/net/http/HttpRequest$BodyPublisher;"
            )
            .is_some());
    }

    #[test]
    fn test_body_handlers_of_string_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpResponse$BodyHandlers",
                "ofString",
                "()Ljava/net/http/HttpResponse$BodyHandler;"
            )
            .is_some());
    }

    #[test]
    fn test_body_handlers_discarding_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/HttpResponse$BodyHandlers",
                "discarding",
                "()Ljava/net/http/HttpResponse$BodyHandler;"
            )
            .is_some());
    }

    #[test]
    fn test_websocket_send_text_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/WebSocket",
                "sendText",
                "(Ljava/lang/CharSequence;Z)Ljava/util/concurrent/CompletableFuture;"
            )
            .is_some());
    }

    #[test]
    fn test_websocket_send_close_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find(
                "java/net/http/WebSocket",
                "sendClose",
                "(ILjava/lang/String;)Ljava/util/concurrent/CompletableFuture;"
            )
            .is_some());
    }

    #[test]
    fn test_websocket_abort_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r.find("java/net/http/WebSocket", "abort", "()V").is_some());
    }

    #[test]
    fn test_websocket_is_output_closed_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find("java/net/http/WebSocket", "isOutputClosed", "()Z")
            .is_some());
    }

    #[test]
    fn test_websocket_is_input_closed_registered() {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        assert!(r
            .find("java/net/http/WebSocket", "isInputClosed", "()Z")
            .is_some());
    }

    // --- Logic / unit tests ------------------------------------------------

    #[test]
    fn test_method_idx_to_name_all_variants() {
        assert_eq!(method_idx_to_name(METHOD_GET), "GET");
        assert_eq!(method_idx_to_name(METHOD_POST), "POST");
        assert_eq!(method_idx_to_name(METHOD_PUT), "PUT");
        assert_eq!(method_idx_to_name(METHOD_DELETE), "DELETE");
        assert_eq!(method_idx_to_name(METHOD_HEAD), "HEAD");
        assert_eq!(method_idx_to_name(METHOD_PATCH), "PATCH");
        assert_eq!(method_idx_to_name(METHOD_OPTIONS), "OPTIONS");
        assert_eq!(method_idx_to_name(99), "GET"); // unknown defaults to GET
    }

    // [VULN fix nb-http2 (2)] CR/LF/NUL request-splitting guard.
    #[test]
    fn test_validate_no_crlf_accepts_clean_values() {
        assert!(validate_no_crlf("method", "GET").is_ok());
        assert!(validate_no_crlf("request-target", "/path?a=b&c=d").is_ok());
        assert!(validate_no_crlf("Host header", "example.com:8443").is_ok());
        assert!(validate_no_crlf("request-target", "").is_ok());
    }

    #[test]
    fn test_validate_no_crlf_rejects_cr_lf_nul() {
        // A request-target carrying CRLF + an injected header must be rejected.
        let split = "/path\r\nX-Injected: evil";
        let err = validate_no_crlf("request-target", split).unwrap_err();
        assert!(matches!(err, RuntimeError::IllegalArgumentException { .. }));

        assert!(validate_no_crlf("Host header", "evil\rhost").is_err());
        assert!(validate_no_crlf("Host header", "evil\nhost").is_err());
        assert!(validate_no_crlf("method", "GET\0").is_err());
        // Bare LF (header smuggling against lenient parsers) is rejected too.
        assert!(validate_no_crlf("method", "GE\nT").is_err());
    }

    #[test]
    fn test_http_version_constants() {
        assert_eq!(HTTP_VERSION_1_1, 0);
        assert_eq!(HTTP_VERSION_2, 1);
    }

    #[test]
    fn test_redirect_constants() {
        // These are `java.net.http.HttpClient.Redirect` ORDINALS, in the JDK's
        // declaration order — NEVER, ALWAYS, NORMAL. They used to be 0/2/1,
        // which disagreed with the `NEVER`/`ALWAYS`/`NORMAL` statics owned by
        // `phases_late::net_channels`, so `client.followRedirects() ==
        // HttpClient.Redirect.ALWAYS` compared two different numbers.
        assert_eq!(REDIRECT_NEVER, 0);
        assert_eq!(REDIRECT_ALWAYS, 1);
        assert_eq!(REDIRECT_NORMAL, 2);
    }

    #[test]
    fn test_websocket_state_constants() {
        assert_eq!(WS_OPEN, 0);
        assert_eq!(WS_CLOSING, 1);
        assert_eq!(WS_CLOSED, 2);
    }

    #[test]
    fn test_hpack_static_table_length() {
        // RFC 7541 defines exactly 61 static entries (index 1..=61)
        assert_eq!(HpackStaticTable::ENTRIES.len(), 61);
    }

    #[test]
    fn test_hpack_find_method_get() {
        let idx = HpackStaticTable::find(":method", Some("GET"));
        assert_eq!(idx, Some(2)); // 1-based: entry 2 is ":method GET"
    }

    #[test]
    fn test_hpack_find_method_post() {
        let idx = HpackStaticTable::find(":method", Some("POST"));
        assert_eq!(idx, Some(3));
    }

    #[test]
    fn test_hpack_find_scheme_https() {
        let idx = HpackStaticTable::find(":scheme", Some("https"));
        assert_eq!(idx, Some(7));
    }

    #[test]
    fn test_hpack_find_status_200() {
        let idx = HpackStaticTable::find(":status", Some("200"));
        assert_eq!(idx, Some(8));
    }

    #[test]
    fn test_hpack_find_no_value_match() {
        // Searching by name only returns first occurrence
        let idx = HpackStaticTable::find(":method", None);
        assert!(idx.is_some());
        assert!(idx.unwrap() >= 1);
    }

    #[test]
    fn test_hpack_find_nonexistent() {
        let idx = HpackStaticTable::find("x-custom-header", Some("value"));
        assert_eq!(idx, None);
    }

    #[test]
    fn test_hpack_huffman_decode_known_vectors() {
        // RFC 7541 Appendix C.4.1: "www.example.com" Huffman-encoded.
        let encoded = [
            0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90, 0xf4, 0xff,
        ];
        let decoded = hpack_huffman_decode(&encoded).expect("decode www.example.com");
        assert_eq!(&decoded, b"www.example.com");

        // RFC 7541 Appendix C.4.2: "no-cache".
        let encoded2 = [0xa8, 0xeb, 0x10, 0x64, 0x9c, 0xbf];
        let decoded2 = hpack_huffman_decode(&encoded2).expect("decode no-cache");
        assert_eq!(&decoded2, b"no-cache");

        // RFC 7541 Appendix C.4.3: "custom-value".
        let encoded3 = [0x25, 0xa8, 0x49, 0xe9, 0x5b, 0xb8, 0xe8, 0xb4, 0xbf];
        let decoded3 = hpack_huffman_decode(&encoded3).expect("decode custom-value");
        assert_eq!(&decoded3, b"custom-value");
    }

    #[test]
    fn test_hpack_huffman_decode_empty() {
        assert_eq!(hpack_huffman_decode(&[]).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn test_hpack_huffman_decode_rejects_bad_padding() {
        // Trailing bits that are not all-ones must be rejected (fail-closed).
        // 0x00 alone: first 5 bits 00000 decode to '0', leaving 3 zero pad bits
        // which are NOT all-ones -> error.
        assert!(hpack_huffman_decode(&[0x00]).is_err());
    }

    #[test]
    fn test_http2_frame_type_values() {
        assert_eq!(Http2FrameType::Data as u8, 0x0);
        assert_eq!(Http2FrameType::Headers as u8, 0x1);
        assert_eq!(Http2FrameType::Settings as u8, 0x4);
        assert_eq!(Http2FrameType::Ping as u8, 0x6);
        assert_eq!(Http2FrameType::Goaway as u8, 0x7);
        assert_eq!(Http2FrameType::WindowUpdate as u8, 0x8);
        assert_eq!(Http2FrameType::Continuation as u8, 0x9);
    }

    #[test]
    fn test_http2_error_code_values() {
        assert_eq!(Http2ErrorCode::NoError as u32, 0x0);
        assert_eq!(Http2ErrorCode::ProtocolError as u32, 0x1);
        assert_eq!(Http2ErrorCode::Cancel as u32, 0x8);
        assert_eq!(Http2ErrorCode::InadequateSecurity as u32, 0xc);
        assert_eq!(Http2ErrorCode::Http11Required as u32, 0xd);
    }

    #[test]
    fn test_default_pool_size() {
        assert_eq!(DEFAULT_POOL_SIZE, 20);
    }

    #[test]
    fn test_all_http2_natives_count() {
        // Smoke-test: ensure registration completes without panic
        // and that a representative cross-section of methods are present.
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        // HttpClient
        assert!(r
            .find(
                "java/net/http/HttpClient",
                "sslParameters",
                "()Ljavax/net/ssl/SSLParameters;"
            )
            .is_some());
        // HttpRequest
        assert!(r
            .find("java/net/http/HttpRequest", "expectContinue", "()Z")
            .is_some());
        // HttpResponse
        assert!(r
            .find(
                "java/net/http/HttpResponse",
                "sslSession",
                "()Ljava/util/Optional;"
            )
            .is_some());
        // WebSocket
        assert!(r
            .find(
                "java/net/http/WebSocket",
                "subprotocol",
                "()Ljava/lang/String;"
            )
            .is_some());
        // BodyHandlers
        assert!(r
            .find(
                "java/net/http/HttpResponse$BodyHandlers",
                "ofLines",
                "()Ljava/net/http/HttpResponse$BodyHandler;"
            )
            .is_some());
    }

    // --- M14: Real HTTP client/server tests ---------------------------------

    #[test]
    fn m14_http11_request_to_localhost_server() {
        // Spin up a real TCP server that responds with HTTP/1.1 200 OK.
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("M14: bind failed");
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("M14: accept failed");
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).expect("M14: read failed");
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(
                request.starts_with("GET /hello"),
                "M14: expected GET /hello, got: {}",
                request
            );

            let body = "Hello from CratonVM!";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            stream
                .write_all(response.as_bytes())
                .expect("M14: write failed");
            stream.flush().ok();
        });

        let (status, body) =
            http11_request("127.0.0.1", port, "GET", "/hello").expect("M14: http11_request failed");

        assert_eq!(status, 200, "M14: expected status 200, got {status}");
        assert_eq!(body, "Hello from CratonVM!", "M14: body mismatch");
        server.join().expect("M14: server thread panicked");
    }

    #[test]
    fn m14_http11_post_request() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("M14: bind failed");
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("M14: accept failed");
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).expect("M14: read failed");
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(
                request.starts_with("POST /api"),
                "M14: expected POST /api, got: {}",
                request
            );

            let body = "{\"status\":\"created\"}";
            let response = format!(
                "HTTP/1.1 201 Created\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            stream
                .write_all(response.as_bytes())
                .expect("M14: write failed");
        });

        let (status, body) =
            http11_request("127.0.0.1", port, "POST", "/api").expect("M14: http11_request failed");

        assert_eq!(status, 201, "M14: expected status 201");
        assert_eq!(body, "{\"status\":\"created\"}");
        server.join().expect("M14: server thread panicked");
    }

    #[test]
    fn m14_http11_connection_refused() {
        // Connecting to a port with no listener should fail gracefully.
        let result = http11_request("127.0.0.1", 1, "GET", "/");
        assert!(result.is_err(), "M14: expected connection error for port 1");
    }

    #[test]
    fn m14_http11_404_response() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("M14: bind failed");
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("M14: accept failed");
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf);
            let response = "HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\nNot Found";
            stream.write_all(response.as_bytes()).ok();
        });

        let (status, body) = http11_request("127.0.0.1", port, "GET", "/missing")
            .expect("M14: http11_request failed");

        assert_eq!(status, 404, "M14: expected 404");
        assert_eq!(body, "Not Found");
        server.join().expect("M14: server thread panicked");
    }

    // --- E2: `java.util.Optional` is (reference-or-null), never (flag, payload)
    //
    // `javap -p --module java.base java.util.Optional` (JDK 25) declares ONE
    // instance field, `private final T value`, and it is a REFERENCE. The three
    // PRIMITIVE Optionals genuinely do declare `boolean isPresent` at slot 0 and
    // `value` at slot 1 -- which is why the (flag, payload) idiom looked right
    // and why `firstValueAsLong` below must keep it. `ref_operand_is_null`
    // (`vm/src/runtime/interpreter.rs:8246`) counts only `Object(None)`,
    // `Uninitialized` and `Long(0)` as null, so an `Int(0)` parked in slot 0
    // made an EMPTY Optional answer `isPresent() == true` and made `get()`
    // return the flag. These tests INVOKE the natives rather than merely
    // asserting they are registered -- the 64 tests above are registration-only
    // and every one of them stayed green through the whole defect.
    //
    // See docs/known-issues/jdk-only/E2-1-optional-reference-layout-landed.md

    /// Every reference-`Optional` producer in this file, invoked for real.
    /// Returns slot 0 of the `Optional` the native actually built.
    fn opt_slot0(
        cb: cratonvm_native_api::NativeCallback,
        ctx: &mut crate::test_utils::MockNativeContext,
        args: &[Value],
    ) -> Value {
        match cb(ctx, args).expect("native must not fail") {
            Some(Value::Object(Some(opt))) => {
                assert_eq!(
                    ctx.object_num_fields(opt),
                    1,
                    "java.util.Optional has exactly ONE instance field"
                );
                ctx.get_field(opt, 0)
            }
            other => panic!("expected an Optional object, got {other:?}"),
        }
    }

    fn find_cb(class: &str, name: &str, desc: &str) -> cratonvm_native_api::NativeCallback {
        let mut r = NativeMethodRegistry::new();
        register_http2_natives(&mut r);
        r.find(class, name, desc)
            .unwrap_or_else(|| panic!("{class}.{name}{desc} must be registered"))
    }

    fn alloc_of(
        ctx: &mut crate::test_utils::MockNativeContext,
        class: &str,
        n: usize,
    ) -> ObjectRef {
        let cid = ctx.ensure_class_initialized(class).unwrap();
        ctx.alloc_object(cid, n)
    }

    /// The assertion that names the defect. A bare `Value::Int` in slot 0 is
    /// what `isPresent()` reads as the reference `value`.
    fn assert_not_a_flag(site: &str, slot0: &Value) {
        if let Value::Int(n) = slot0 {
            panic!(
                "{site}: slot 0 of a java.util.Optional is the REFERENCE `value`, \
                 but it holds Value::Int({n}). isPresent() reads this as PRESENT \
                 (Int(0) is not null to ref_operand_is_null) and get() returns the flag."
            );
        }
    }

    #[test]
    fn e2_http_client_presence_accessors_answer_empty_not_a_flag() {
        // executor/proxy/authenticator/cookieHandler store a presence FLAG and
        // never the object itself, so `empty()` is the only truthful answer.
        // These four have the CORRECT arity (1) and had the WRONG type, which
        // is the shape the layout-alias instrument (a slot COUNT comparison) is
        // structurally blind to.
        for (name, flag_slot) in [
            ("executor", CLIENT_HAS_EXECUTOR),
            ("proxy", CLIENT_HAS_PROXY),
            ("authenticator", CLIENT_HAS_AUTH),
            ("cookieHandler", CLIENT_HAS_COOKIE),
        ] {
            let mut ctx = crate::test_utils::MockNativeContext::new();
            let this = alloc_of(&mut ctx, "java/net/http/HttpClient", 10);
            // Set the flag to 1 -- the case that used to produce Int(1), which
            // `get()` would have handed back where an object is declared.
            ctx.set_field(this, flag_slot, Value::Int(1));
            let cb = find_cb("java/net/http/HttpClient", name, "()Ljava/util/Optional;");
            let slot0 = opt_slot0(cb, &mut ctx, &[Value::Object(Some(this))]);
            assert_not_a_flag(name, &slot0);
            assert_eq!(
                slot0,
                Value::Object(None),
                "{name}: no object is stored in this layout, so empty() is the only truthful answer"
            );
        }
    }

    #[test]
    fn e2_connect_timeout_absent_is_null_and_present_is_a_duration() {
        let cb = || {
            find_cb(
                "java/net/http/HttpClient",
                "connectTimeout",
                "()Ljava/util/Optional;",
            )
        };

        // Absent: this is the row that used to report isPresent() == true.
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, "java/net/http/HttpClient", 10);
        ctx.set_field(this, CLIENT_CONNECT_TIMEOUT, Value::Long(0));
        let slot0 = opt_slot0(cb(), &mut ctx, &[Value::Object(Some(this))]);
        assert_not_a_flag("connectTimeout.absent", &slot0);
        assert_eq!(slot0, Value::Object(None));

        // Present: declared Optional<Duration>, so slot 0 owes a Duration
        // object -- not a Long and certainly not the flag. 1500ms == 1s + 500ms.
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, "java/net/http/HttpClient", 10);
        ctx.set_field(this, CLIENT_CONNECT_TIMEOUT, Value::Long(1500));
        let slot0 = opt_slot0(cb(), &mut ctx, &[Value::Object(Some(this))]);
        assert_not_a_flag("connectTimeout.present", &slot0);
        match slot0 {
            Value::Object(Some(dur)) => {
                assert_eq!(ctx.get_field(dur, 0), Value::Long(1), "Duration seconds");
                assert_eq!(
                    ctx.get_field(dur, 1),
                    Value::Int(500_000_000),
                    "Duration nanos"
                );
            }
            other => panic!("connectTimeout present must hold a Duration, got {other:?}"),
        }
    }

    #[test]
    fn e2_request_timeout_absent_is_null_and_present_is_a_duration() {
        let cb = || {
            find_cb(
                "java/net/http/HttpRequest",
                "timeout",
                "()Ljava/util/Optional;",
            )
        };

        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, "java/net/http/HttpRequest", 8);
        ctx.set_field(this, REQ_TIMEOUT, Value::Long(0));
        let slot0 = opt_slot0(cb(), &mut ctx, &[Value::Object(Some(this))]);
        assert_not_a_flag("timeout.absent", &slot0);
        assert_eq!(slot0, Value::Object(None));

        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, "java/net/http/HttpRequest", 8);
        ctx.set_field(this, REQ_TIMEOUT, Value::Long(7000));
        let slot0 = opt_slot0(cb(), &mut ctx, &[Value::Object(Some(this))]);
        assert_not_a_flag("timeout.present", &slot0);
        match slot0 {
            Value::Object(Some(dur)) => {
                assert_eq!(ctx.get_field(dur, 0), Value::Long(7), "Duration seconds");
                assert_eq!(ctx.get_field(dur, 1), Value::Int(0), "Duration nanos");
            }
            other => panic!("timeout present must hold a Duration, got {other:?}"),
        }
    }

    #[test]
    fn e2_body_publisher_moves_the_publisher_into_slot_zero() {
        let cb = || {
            find_cb(
                "java/net/http/HttpRequest",
                "bodyPublisher",
                "()Ljava/util/Optional;",
            )
        };

        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, "java/net/http/HttpRequest", 8);
        ctx.set_field(this, REQ_HAS_BODY, Value::Int(0));
        let slot0 = opt_slot0(cb(), &mut ctx, &[Value::Object(Some(this))]);
        assert_not_a_flag("bodyPublisher.absent", &slot0);
        assert_eq!(slot0, Value::Object(None));

        // Present: the publisher used to be parked at slot 1, where no real
        // bytecode ever read it.
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, "java/net/http/HttpRequest", 8);
        ctx.set_field(this, REQ_HAS_BODY, Value::Int(1));
        ctx.set_field(this, REQ_BODY_LEN, Value::Long(2));
        let slot0 = opt_slot0(cb(), &mut ctx, &[Value::Object(Some(this))]);
        assert_not_a_flag("bodyPublisher.present", &slot0);
        assert!(
            matches!(slot0, Value::Object(Some(_))),
            "bodyPublisher present must hold the publisher itself, got {slot0:?}"
        );
    }

    /// Declared `Optional<HttpClient$Version>`, so slot 0 owes an enum mirror
    /// or null -- **never an ordinal**, which is the law this test states and
    /// which has not changed.
    ///
    /// AMENDED by E13. E2 wrote this asserting `Value::Object(None)` for BOTH
    /// `REQ_VERSION == 0` and `REQ_VERSION == 2`, because `REQ_VERSION` could
    /// not then be non-zero: its only writer,
    /// `HttpRequest$Builder.version(HttpClient$Version)`, matched
    /// `Some(Value::Int(n))` against a reference argument. E13 fixed that
    /// builder, so `2` now means an explicit HTTP_2 override and the present
    /// arm is live. The `Int(2)` row moved from "empty" to "an enum mirror";
    /// `assert_not_a_flag` -- the actual C12-3 assertion -- still holds on
    /// both, and `e13_request_version_optional_now_holds_the_enum_mirror`
    /// drives the same site through the real builder.
    #[test]
    fn e2_request_version_is_never_an_ordinal() {
        for (stored, expect_present) in [(Value::Int(0), false), (Value::Int(2), true)] {
            let mut ctx = crate::test_utils::MockNativeContext::new();
            let this = alloc_of(&mut ctx, "java/net/http/HttpRequest", 8);
            ctx.set_field(this, REQ_VERSION, stored);
            let cb = find_cb(
                "java/net/http/HttpRequest",
                "version",
                "()Ljava/util/Optional;",
            );
            let slot0 = opt_slot0(cb, &mut ctx, &[Value::Object(Some(this))]);
            assert_not_a_flag("version", &slot0);
            if expect_present {
                match slot0 {
                    Value::Object(Some(mirror)) => assert_eq!(
                        ctx.get_field(mirror, ENUM_SLOT_ORDINAL),
                        Value::Int(HTTP_VERSION_2),
                        "an ENUM MIRROR, not the raw ordinal, belongs in slot 0"
                    ),
                    other => panic!("an explicit version override must be PRESENT, got {other:?}"),
                }
            } else {
                assert_eq!(slot0, Value::Object(None));
            }
        }
    }

    /// The row C12-3 calls "the one that settles what this is": the arity was
    /// already correct (1 slot) and the TYPE was not. It is covered by NO Java
    /// fixture -- `RJdkOptionalShape` cannot reach it without a loopback
    /// `HttpServer` -- and it is invisible to the layout-alias instrument,
    /// which compares slot COUNTS. This test is its only coverage.
    #[test]
    fn e2_previous_response_is_empty_not_a_flag() {
        for flag in [0, 1] {
            let mut ctx = crate::test_utils::MockNativeContext::new();
            let this = alloc_of(&mut ctx, "java/net/http/HttpResponse", 7);
            ctx.set_field(this, RESP_HAS_PREV, Value::Int(flag));
            let cb = find_cb(
                "java/net/http/HttpResponse",
                "previousResponse",
                "()Ljava/util/Optional;",
            );
            let slot0 = opt_slot0(cb, &mut ctx, &[Value::Object(Some(this))]);
            assert_not_a_flag("previousResponse", &slot0);
            assert_eq!(
                slot0,
                Value::Object(None),
                "no previous response is stored anywhere in this layout"
            );
        }
    }

    #[test]
    fn e2_ssl_session_moves_the_session_into_slot_zero() {
        let cb = || {
            find_cb(
                "java/net/http/HttpResponse",
                "sslSession",
                "()Ljava/util/Optional;",
            )
        };

        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, "java/net/http/HttpResponse", 7);
        ctx.set_field(this, RESP_HAS_SSL, Value::Int(0));
        let slot0 = opt_slot0(cb(), &mut ctx, &[Value::Object(Some(this))]);
        assert_not_a_flag("sslSession.absent", &slot0);
        assert_eq!(slot0, Value::Object(None));

        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, "java/net/http/HttpResponse", 7);
        ctx.set_field(this, RESP_HAS_SSL, Value::Int(1));
        let slot0 = opt_slot0(cb(), &mut ctx, &[Value::Object(Some(this))]);
        assert_not_a_flag("sslSession.present", &slot0);
        assert!(
            matches!(slot0, Value::Object(Some(_))),
            "sslSession present must hold the SSLSession itself, got {slot0:?}"
        );
    }

    #[test]
    fn e2_first_value_moves_the_string_into_slot_zero() {
        let cb = || {
            find_cb(
                "java/net/http/HttpHeaders",
                "firstValue",
                "(Ljava/lang/String;)Ljava/util/Optional;",
            )
        };

        // Present: the header string used to sit at slot 1.
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 3);
        ctx.set_field(this, HDR_HAS_CT, Value::Int(1));
        let key = ctx.create_string("Content-Type");
        let slot0 = opt_slot0(
            cb(),
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(key))],
        );
        assert_not_a_flag("firstValue.present", &slot0);
        match slot0 {
            Value::Object(Some(s)) => {
                assert_eq!(ctx.read_string(s).as_deref(), Some("application/json"));
            }
            other => panic!("firstValue present must hold the String, got {other:?}"),
        }

        // Absent: an unknown header name.
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 3);
        ctx.set_field(this, HDR_HAS_CT, Value::Int(1));
        let key = ctx.create_string("X-Absent");
        let slot0 = opt_slot0(
            cb(),
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(key))],
        );
        assert_not_a_flag("firstValue.absent", &slot0);
        assert_eq!(slot0, Value::Object(None));
    }

    /// NEGATIVE CONTROL -- the test that must FAIL if anyone "simplifies" the
    /// fix above into a blanket one.
    ///
    /// `java.util.OptionalLong` really does declare `boolean isPresent` at slot
    /// 0 and `long value` at slot 1 (`javap -p --module java.base`, JDK 25), so
    /// `firstValueAsLong`'s 2-slot (flag, payload) form is CORRECT and sits
    /// twelve lines below the last site that was wrong. That adjacency is the
    /// best available explanation of how the defect happened: the idiom is
    /// right for the class one method away. Applying the reference-Optional
    /// fix here would BREAK a working accessor.
    #[test]
    fn e2_optional_long_keeps_the_primitive_flag_payload_layout() {
        let cb = || {
            find_cb(
                "java/net/http/HttpHeaders",
                "firstValueAsLong",
                "(Ljava/lang/String;)Ljava/util/OptionalLong;",
            )
        };

        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 3);
        ctx.set_field(this, HDR_HAS_CL, Value::Int(1));
        let key = ctx.create_string("Content-Length");
        let opt = match cb()(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(key))],
        )
        .expect("native must not fail")
        {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected an OptionalLong, got {other:?}"),
        };
        assert_eq!(
            ctx.object_num_fields(opt),
            2,
            "OptionalLong declares TWO fields -- do not collapse it to one"
        );
        assert_eq!(
            ctx.get_field(opt, 0),
            Value::Int(1),
            "OptionalLong slot 0 IS `boolean isPresent` -- a flag here is CORRECT"
        );
        assert_eq!(
            ctx.get_field(opt, 1),
            Value::Long(20),
            "OptionalLong slot 1 IS `long value`"
        );
    }

    // =====================================================================
    // E13 -- THE SIX BUILDERS THAT DECODED A REFERENCE AS AN `Int`.
    //
    // Behavioural, like E2's: each drives the native through the registry and
    // asserts the value it actually STORED. `http2.rs` had 64 tests before E2
    // and every one was registration-only, so all 64 stayed green through both
    // of these defect families -- a native existing says nothing about what it
    // writes.
    //
    // The setups build their arguments the way the VM mints them
    // (`p57_alloc_enum`: name at slot 0, ordinal at slot 1;
    // `util_time::alloc_duration`: seconds at slot 0, nanos at slot 1) rather
    // than by name, so the class-side-witness fallback in the decoders is the
    // path under test. `test_utils::MockNativeContext::get_field_by_name`
    // answers `Value::Int(0)` for an unresolvable name where production answers
    // `Value::Object(None)`; the decoders deliberately never take a value-side
    // by-name read, so that divergence cannot steer these results.
    //
    // See docs/known-issues/jdk-only/E13-1-the-six-builders-decode-a-reference-as-an-int.md
    // =====================================================================

    const CLS_CLIENT_BUILDER: &str = "java/net/http/HttpClient$Builder";
    const CLS_REQUEST_BUILDER: &str = "java/net/http/HttpRequest$Builder";
    const DESC_CLIENT_VERSION: &str =
        "(Ljava/net/http/HttpClient$Version;)Ljava/net/http/HttpClient$Builder;";
    const DESC_REQUEST_VERSION: &str =
        "(Ljava/net/http/HttpClient$Version;)Ljava/net/http/HttpRequest$Builder;";

    /// An enum constant shaped the way `phases_late::p57_alloc_enum` mints one.
    fn alloc_enum_const(
        ctx: &mut crate::test_utils::MockNativeContext,
        class: &str,
        name: &str,
        ordinal: i32,
    ) -> ObjectRef {
        let obj = alloc_of(ctx, class, 2);
        let n = ctx.create_string(name);
        ctx.set_field(obj, ENUM_SLOT_NAME, Value::Object(Some(n)));
        ctx.set_field(obj, ENUM_SLOT_ORDINAL, Value::Int(ordinal));
        obj
    }

    /// A `java.time.Duration` shaped the way `util_time::alloc_duration` mints
    /// one: floor-normalised `seconds` (`Long`) and `nanos` (`Int`).
    fn alloc_dur(
        ctx: &mut crate::test_utils::MockNativeContext,
        seconds: i64,
        nanos: i32,
    ) -> ObjectRef {
        let obj = alloc_of(ctx, "java/time/Duration", 2);
        ctx.set_field(obj, DUR_SLOT_SECONDS, Value::Long(seconds));
        ctx.set_field(obj, DUR_SLOT_NANOS, Value::Int(nanos));
        obj
    }

    /// Drive a fluent setter and check it hands the receiver back -- a builder
    /// that returns anything else breaks the chain the caller wrote.
    fn call_setter(
        cb: cratonvm_native_api::NativeCallback,
        ctx: &mut crate::test_utils::MockNativeContext,
        this: ObjectRef,
        arg: Value,
    ) {
        match cb(ctx, &[Value::Object(Some(this)), arg]).expect("native must not fail") {
            Some(Value::Object(Some(ret))) => assert_eq!(
                ret.as_ptr(),
                this.as_ptr(),
                "a fluent setter must return its own receiver"
            ),
            other => panic!("expected the builder back, got {other:?}"),
        }
    }

    /// THE DISCRIMINATING CASE. `HTTP_1_1` is the constant the pre-fix
    /// `_ => HTTP_VERSION_2` fallback swallowed: a caller who asked for
    /// HTTP/1.1 got an HTTP/2 client, and asking for HTTP_2 looked "right" only
    /// because the default happened to agree. Both are asserted so a future
    /// regression cannot hide behind the agreeing one.
    #[test]
    fn e13_client_builder_version_stores_the_constant_the_caller_passed() {
        for (name, ordinal, expected) in [
            ("HTTP_1_1", 0, HTTP_VERSION_1_1),
            ("HTTP_2", 1, HTTP_VERSION_2),
        ] {
            let mut ctx = crate::test_utils::MockNativeContext::new();
            let this = alloc_of(&mut ctx, CLS_CLIENT_BUILDER, 8);
            let v = alloc_enum_const(&mut ctx, "java/net/http/HttpClient$Version", name, ordinal);
            let cb = find_cb(CLS_CLIENT_BUILDER, "version", DESC_CLIENT_VERSION);
            call_setter(cb, &mut ctx, this, Value::Object(Some(v)));
            assert_eq!(
                ctx.get_field(this, 0),
                Value::Int(expected),
                "version({name}) must store ordinal {expected}; the descriptor's parameter is a \
                 REFERENCE, so a `Some(Value::Int(n))` match here never fires and the default wins"
            );
        }
    }

    /// The two encodings of one concept, separated. Each arm carries ONLY its
    /// own encoding and the other slot is left at the mock's `Int(0)` default,
    /// so each is a real single-path test:
    ///
    /// * NAME-only `HTTP_2` -- slot 1 reads `Int(0)`. Answering `HTTP_2` proves
    ///   the name was consulted FIRST; a decoder that only read the ordinal
    ///   would answer `HTTP_1_1` here.
    /// * ORDINAL-only `1` -- slot 0 holds no string, so the name lookup misses
    ///   and the ordinal path must carry it.
    #[test]
    fn e13_enum_decode_reads_the_name_first_and_the_ordinal_as_a_fallback() {
        let cb = || find_cb(CLS_CLIENT_BUILDER, "version", DESC_CLIENT_VERSION);

        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, CLS_CLIENT_BUILDER, 8);
        let by_name = alloc_of(&mut ctx, "java/net/http/HttpClient$Version", 2);
        let n = ctx.create_string("HTTP_2");
        ctx.set_field(by_name, ENUM_SLOT_NAME, Value::Object(Some(n)));
        call_setter(cb(), &mut ctx, this, Value::Object(Some(by_name)));
        assert_eq!(
            ctx.get_field(this, 0),
            Value::Int(HTTP_VERSION_2),
            "the NAME must be consulted before the ordinal slot"
        );

        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, CLS_CLIENT_BUILDER, 8);
        let by_ordinal = alloc_of(&mut ctx, "java/net/http/HttpClient$Version", 2);
        ctx.set_field(by_ordinal, ENUM_SLOT_ORDINAL, Value::Int(HTTP_VERSION_2));
        call_setter(cb(), &mut ctx, this, Value::Object(Some(by_ordinal)));
        assert_eq!(
            ctx.get_field(this, 0),
            Value::Int(HTTP_VERSION_2),
            "a constant with no readable name must still decode through its ordinal"
        );
    }

    #[test]
    fn e13_client_builder_connect_timeout_decodes_a_duration() {
        let cb = || {
            find_cb(
                CLS_CLIENT_BUILDER,
                "connectTimeout",
                "(Ljava/time/Duration;)Ljava/net/http/HttpClient$Builder;",
            )
        };

        // 1500 ms == Duration.ofMillis(1500) == seconds 1, nanos 500_000_000.
        // This is the value RJdkOptionalShape's `mint-connectTimeout-present-millis`
        // asks for, and it was unreachable: CLIENT_CONNECT_TIMEOUT could never
        // be non-zero, so `connectTimeout()` had no present arm to take.
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, CLS_CLIENT_BUILDER, 8);
        let d = alloc_dur(&mut ctx, 1, 500_000_000);
        call_setter(cb(), &mut ctx, this, Value::Object(Some(d)));
        assert_eq!(ctx.get_field(this, 2), Value::Long(1500));

        // Duration is FLOOR-normalised, so ofMillis(-1500) is seconds -2 with
        // nanos +500_000_000. A decoder that truncated toward zero would say
        // -1000 here.
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, CLS_CLIENT_BUILDER, 8);
        let d = alloc_dur(&mut ctx, -2, 500_000_000);
        call_setter(cb(), &mut ctx, this, Value::Object(Some(d)));
        assert_eq!(ctx.get_field(this, 2), Value::Long(-1500));

        // A null Duration keeps the pre-fix answer rather than inventing one.
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, CLS_CLIENT_BUILDER, 8);
        call_setter(cb(), &mut ctx, this, Value::Object(None));
        assert_eq!(ctx.get_field(this, 2), Value::Long(0));
    }

    #[test]
    fn e13_request_builder_timeout_decodes_a_duration() {
        let cb = find_cb(
            CLS_REQUEST_BUILDER,
            "timeout",
            "(Ljava/time/Duration;)Ljava/net/http/HttpRequest$Builder;",
        );
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, CLS_REQUEST_BUILDER, 8);
        let d = alloc_dur(&mut ctx, 7, 0);
        call_setter(cb, &mut ctx, this, Value::Object(Some(d)));
        assert_eq!(
            ctx.get_field(this, REQ_TIMEOUT),
            Value::Long(7000),
            "the 7000 ms `mint-timeout-present-millis` asks for"
        );
    }

    /// Covers the enum decode AND the slot collision the decode uncovered:
    /// `followRedirects` used to also write `Int(policy != NEVER)` into BUILDER
    /// slot 7, which `build()` copies to `CLIENT_HAS_COOKIE`. That was inert
    /// only while `policy` was stuck at `NEVER`.
    #[test]
    fn e13_follow_redirects_decodes_the_enum_and_leaves_the_cookie_flag_alone() {
        for (name, ordinal, follow) in [
            ("NEVER", REDIRECT_NEVER, 0),
            ("ALWAYS", REDIRECT_ALWAYS, 1),
            ("NORMAL", REDIRECT_NORMAL, 1),
        ] {
            let mut ctx = crate::test_utils::MockNativeContext::new();
            let bld = alloc_of(&mut ctx, CLS_CLIENT_BUILDER, 8);
            init_http_client_builder_fields(&mut ctx, bld);
            // A cookie handler IS configured -- the flag the stray write to
            // builder slot 7 would have overwritten with a redirect answer.
            let cookie_cb = find_cb(
                CLS_CLIENT_BUILDER,
                "cookieHandler",
                "(Ljava/net/CookieHandler;)Ljava/net/http/HttpClient$Builder;",
            );
            let handler = alloc_of(&mut ctx, "java/net/CookieHandler", 1);
            call_setter(cookie_cb, &mut ctx, bld, Value::Object(Some(handler)));

            let e = alloc_enum_const(&mut ctx, "java/net/http/HttpClient$Redirect", name, ordinal);
            let cb = find_cb(
                CLS_CLIENT_BUILDER,
                "followRedirects",
                "(Ljava/net/http/HttpClient$Redirect;)Ljava/net/http/HttpClient$Builder;",
            );
            call_setter(cb, &mut ctx, bld, Value::Object(Some(e)));
            assert_eq!(
                ctx.get_field(bld, 1),
                Value::Int(ordinal),
                "followRedirects({name}) must store the policy it was given"
            );
            assert_eq!(
                ctx.get_field(bld, 7),
                Value::Int(1),
                "builder slot 7 is the COOKIE flag; followRedirects must not touch it"
            );

            let build = find_cb(CLS_CLIENT_BUILDER, "build", "()Ljava/net/http/HttpClient;");
            let client =
                match build(&mut ctx, &[Value::Object(Some(bld))]).expect("build must not fail") {
                    Some(Value::Object(Some(c))) => c,
                    other => panic!("build() must return an HttpClient, got {other:?}"),
                };
            assert_eq!(ctx.get_field(client, CLIENT_REDIRECT), Value::Int(ordinal));
            assert_eq!(
                ctx.get_field(client, CLIENT_HAS_COOKIE),
                Value::Int(1),
                "the cookie flag must survive the redirect policy"
            );
            assert_eq!(
                ctx.get_field(client, CLIENT_FOLLOW_REDIR),
                Value::Int(follow),
                "CLIENT_FOLLOW_REDIR is slot 8 and the copy loop stops at 7, so build() must \
                 derive it"
            );
        }
    }

    #[test]
    fn e13_request_builder_version_stores_the_ordinal_plus_one() {
        let cb = || find_cb(CLS_REQUEST_BUILDER, "version", DESC_REQUEST_VERSION);
        for (name, ordinal, stored) in [("HTTP_1_1", 0, 1), ("HTTP_2", 1, 2)] {
            let mut ctx = crate::test_utils::MockNativeContext::new();
            let this = alloc_of(&mut ctx, CLS_REQUEST_BUILDER, 8);
            let v = alloc_enum_const(&mut ctx, "java/net/http/HttpClient$Version", name, ordinal);
            call_setter(cb(), &mut ctx, this, Value::Object(Some(v)));
            assert_eq!(
                ctx.get_field(this, REQ_VERSION),
                Value::Int(stored),
                "REQ_VERSION encodes 0 = no override, so {name} is stored as {stored}"
            );
        }
    }

    /// An argument the decoder cannot read must leave each site's OWN default
    /// standing -- never a silent decay to ordinal 0. That decay is what turned
    /// `TimeUnit.SECONDS.toNanos(1)` into `1` (`phases_early::tu_ordinal`), and
    /// here it would make `version(<unreadable>)` mean HTTP_1_1 on the client
    /// and an explicit HTTP_1_1 override on the request.
    #[test]
    fn e13_an_undecodable_enum_argument_keeps_each_sites_own_default() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, CLS_CLIENT_BUILDER, 8);
        let cb = find_cb(CLS_CLIENT_BUILDER, "version", DESC_CLIENT_VERSION);
        call_setter(cb, &mut ctx, this, Value::Object(None));
        assert_eq!(
            ctx.get_field(this, 0),
            Value::Int(HTTP_VERSION_2),
            "HttpClient$Builder.version's documented default is HTTP_2, not ordinal 0"
        );

        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, CLS_REQUEST_BUILDER, 8);
        let cb = find_cb(CLS_REQUEST_BUILDER, "version", DESC_REQUEST_VERSION);
        call_setter(cb, &mut ctx, this, Value::Object(None));
        assert_eq!(
            ctx.get_field(this, REQ_VERSION),
            Value::Int(0),
            "HttpRequest$Builder.version's default is 0 == NO override"
        );

        // An ordinal outside the modelled constants is not a usable answer
        // either -- `version_enum` would have to invent a name for it.
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = alloc_of(&mut ctx, CLS_REQUEST_BUILDER, 8);
        let bogus = alloc_of(&mut ctx, "java/net/http/HttpClient$Version", 2);
        ctx.set_field(bogus, ENUM_SLOT_ORDINAL, Value::Int(97));
        let cb = find_cb(CLS_REQUEST_BUILDER, "version", DESC_REQUEST_VERSION);
        call_setter(cb, &mut ctx, this, Value::Object(Some(bogus)));
        assert_eq!(ctx.get_field(this, REQ_VERSION), Value::Int(0));
    }

    /// `BodyPublishers.ofByteArray` is STATIC, so `args[0]` is the `[B` itself.
    /// Every publisher it minted reported `contentLength() == 0`, while the
    /// `ofString` sibling directly above it always read its reference argument.
    #[test]
    fn e13_of_byte_array_reads_the_array_length() {
        let cb = find_cb(
            "java/net/http/HttpRequest$BodyPublishers",
            "ofByteArray",
            "([B)Ljava/net/http/HttpRequest$BodyPublisher;",
        );
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 5);
        let bp = match cb(&mut ctx, &[Value::Object(Some(arr))]).expect("native must not fail") {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected a BodyPublisher, got {other:?}"),
        };
        assert_eq!(
            ctx.get_field(bp, 0),
            Value::Long(5),
            "contentLength() reads slot 0 as a Long"
        );
    }

    /// Found by grepping the SHAPE ("a builder that does not read the reference
    /// it was handed") rather than the `Value::Int` idiom: `POST`, `PUT` and
    /// `method(String, BodyPublisher)` recorded only THAT a body existed, so
    /// `bodyPublisher().get().contentLength()` answered 0 for every body --
    /// which is `mint-bodyPublisher-present-len`.
    #[test]
    fn e13_post_put_and_method_carry_the_publishers_content_length() {
        let bp_cb = find_cb(
            "java/net/http/HttpRequest$BodyPublishers",
            "ofString",
            "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;",
        );
        for (method, desc, extra) in [
            (
                "POST",
                "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
                0usize,
            ),
            (
                "PUT",
                "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
                0,
            ),
            (
                "method",
                "(Ljava/lang/String;Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
                1,
            ),
        ] {
            let mut ctx = crate::test_utils::MockNativeContext::new();
            let this = alloc_of(&mut ctx, CLS_REQUEST_BUILDER, 8);
            let body = ctx.create_string("hi");
            let bp = match bp_cb(&mut ctx, &[Value::Object(Some(body))])
                .expect("ofString must not fail")
            {
                Some(Value::Object(Some(o))) => o,
                other => panic!("expected a BodyPublisher, got {other:?}"),
            };
            let cb = find_cb(CLS_REQUEST_BUILDER, method, desc);
            let mut args = vec![Value::Object(Some(this))];
            if extra == 1 {
                let name = ctx.create_string("POST");
                args.push(Value::Object(Some(name)));
            }
            args.push(Value::Object(Some(bp)));
            cb(&mut ctx, &args).expect("native must not fail");
            assert_eq!(ctx.get_field(this, REQ_HAS_BODY), Value::Int(1));
            assert_eq!(
                ctx.get_field(this, REQ_BODY_LEN),
                Value::Long(2),
                "{method} must carry the publisher's declared length, not just a has-body flag"
            );
        }
    }

    /// NEGATIVE CONTROL -- the test that must FAIL if anyone "simplifies" the
    /// six fixes above into "no builder matches `Value::Int`".
    ///
    /// `expectContinue`'s descriptor is `(Z)`: a primitive `boolean`, which
    /// really does arrive as a `Value::Int`. The idiom is CORRECT here and
    /// wrong twelve lines away, exactly as `firstValueAsLong`'s
    /// `(isPresent, value)` layout is correct where the reference `Optional`'s
    /// was wrong. Read the DESCRIPTOR, not the neighbouring line.
    #[test]
    fn e13_expect_continue_still_reads_a_primitive_boolean() {
        let cb = || {
            find_cb(
                CLS_REQUEST_BUILDER,
                "expectContinue",
                "(Z)Ljava/net/http/HttpRequest$Builder;",
            )
        };
        for flag in [0, 1] {
            let mut ctx = crate::test_utils::MockNativeContext::new();
            let this = alloc_of(&mut ctx, CLS_REQUEST_BUILDER, 8);
            call_setter(cb(), &mut ctx, this, Value::Int(flag));
            assert_eq!(
                ctx.get_field(this, REQ_EXPECT_100),
                Value::Int(flag),
                "a primitive boolean parameter DOES arrive as Value::Int -- this arm is correct"
            );
        }
    }

    /// END TO END through the pair E2 could not join up: builder -> build() ->
    /// accessor. E2 pinned `HttpRequest.version()` at unconditionally EMPTY and
    /// said so in a comment, because `REQ_VERSION` could not be non-zero. This
    /// patch makes it non-zero, so slot 0 of the `Optional` must now hold an
    /// ENUM MIRROR -- still never an ordinal and never a flag.
    #[test]
    fn e13_request_version_optional_now_holds_the_enum_mirror() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let bld = alloc_of(&mut ctx, CLS_REQUEST_BUILDER, 8);
        init_http_request_fields(&mut ctx, bld);
        let v = alloc_enum_const(&mut ctx, "java/net/http/HttpClient$Version", "HTTP_2", 1);
        let set = find_cb(CLS_REQUEST_BUILDER, "version", DESC_REQUEST_VERSION);
        call_setter(set, &mut ctx, bld, Value::Object(Some(v)));
        let build = find_cb(
            CLS_REQUEST_BUILDER,
            "build",
            "()Ljava/net/http/HttpRequest;",
        );
        let req = match build(&mut ctx, &[Value::Object(Some(bld))]).expect("build must not fail") {
            Some(Value::Object(Some(r))) => r,
            other => panic!("build() must return an HttpRequest, got {other:?}"),
        };

        let cb = find_cb(
            "java/net/http/HttpRequest",
            "version",
            "()Ljava/util/Optional;",
        );
        let slot0 = opt_slot0(cb, &mut ctx, &[Value::Object(Some(req))]);
        assert_not_a_flag("version.present", &slot0);
        match slot0 {
            Value::Object(Some(mirror)) => {
                let name = match ctx.get_field(mirror, ENUM_SLOT_NAME) {
                    Value::Object(Some(s)) => ctx.read_string(s),
                    other => panic!("an enum mirror's slot 0 is its name, got {other:?}"),
                };
                assert_eq!(name.as_deref(), Some("HTTP_2"));
                assert_eq!(
                    ctx.get_field(mirror, ENUM_SLOT_ORDINAL),
                    Value::Int(HTTP_VERSION_2)
                );
            }
            other => panic!("version() present must hold an HttpClient$Version, got {other:?}"),
        }

        // The absent arm still answers empty -- no override was requested.
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let req = alloc_of(&mut ctx, "java/net/http/HttpRequest", 8);
        init_http_request_fields(&mut ctx, req);
        let cb = find_cb(
            "java/net/http/HttpRequest",
            "version",
            "()Ljava/util/Optional;",
        );
        let slot0 = opt_slot0(cb, &mut ctx, &[Value::Object(Some(req))]);
        assert_eq!(slot0, Value::Object(None));
    }

    /// The other two `Optional` present arms E2 could only reach by writing the
    /// backing field by hand, now driven through the BUILDER the way Java does.
    /// This is what `mint-connectTimeout-present-millis` and
    /// `mint-timeout-present-millis` execute.
    #[test]
    fn e13_the_duration_optionals_are_reachable_through_the_builder_now() {
        // HttpClient: connectTimeout(Duration.ofMillis(1500)).build().connectTimeout()
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let bld = alloc_of(&mut ctx, CLS_CLIENT_BUILDER, 8);
        init_http_client_builder_fields(&mut ctx, bld);
        let d = alloc_dur(&mut ctx, 1, 500_000_000);
        let set = find_cb(
            CLS_CLIENT_BUILDER,
            "connectTimeout",
            "(Ljava/time/Duration;)Ljava/net/http/HttpClient$Builder;",
        );
        call_setter(set, &mut ctx, bld, Value::Object(Some(d)));
        let build = find_cb(CLS_CLIENT_BUILDER, "build", "()Ljava/net/http/HttpClient;");
        let client =
            match build(&mut ctx, &[Value::Object(Some(bld))]).expect("build must not fail") {
                Some(Value::Object(Some(c))) => c,
                other => panic!("build() must return an HttpClient, got {other:?}"),
            };
        let cb = find_cb(
            "java/net/http/HttpClient",
            "connectTimeout",
            "()Ljava/util/Optional;",
        );
        let slot0 = opt_slot0(cb, &mut ctx, &[Value::Object(Some(client))]);
        assert_not_a_flag("connectTimeout.present", &slot0);
        match slot0 {
            Value::Object(Some(dur)) => {
                assert_eq!(ctx.get_field(dur, DUR_SLOT_SECONDS), Value::Long(1));
                assert_eq!(
                    ctx.get_field(dur, DUR_SLOT_NANOS),
                    Value::Int(500_000_000),
                    "1500 ms must round-trip through the builder as 1 s + 500 ms"
                );
            }
            other => panic!("connectTimeout present must hold a Duration, got {other:?}"),
        }

        // HttpRequest: POST(ofString("hi")).build().bodyPublisher().contentLength()
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let bld = alloc_of(&mut ctx, CLS_REQUEST_BUILDER, 8);
        init_http_request_fields(&mut ctx, bld);
        let body = ctx.create_string("hi");
        let bp_cb = find_cb(
            "java/net/http/HttpRequest$BodyPublishers",
            "ofString",
            "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;",
        );
        let bp =
            match bp_cb(&mut ctx, &[Value::Object(Some(body))]).expect("ofString must not fail") {
                Some(Value::Object(Some(o))) => o,
                other => panic!("expected a BodyPublisher, got {other:?}"),
            };
        let post = find_cb(
            CLS_REQUEST_BUILDER,
            "POST",
            "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        );
        call_setter(post, &mut ctx, bld, Value::Object(Some(bp)));
        let build = find_cb(
            CLS_REQUEST_BUILDER,
            "build",
            "()Ljava/net/http/HttpRequest;",
        );
        let req = match build(&mut ctx, &[Value::Object(Some(bld))]).expect("build must not fail") {
            Some(Value::Object(Some(r))) => r,
            other => panic!("build() must return an HttpRequest, got {other:?}"),
        };
        let cb = find_cb(
            "java/net/http/HttpRequest",
            "bodyPublisher",
            "()Ljava/util/Optional;",
        );
        let slot0 = opt_slot0(cb, &mut ctx, &[Value::Object(Some(req))]);
        assert_not_a_flag("bodyPublisher.present", &slot0);
        match slot0 {
            Value::Object(Some(pub_obj)) => assert_eq!(
                ctx.get_field(pub_obj, 0),
                Value::Long(2),
                "the body \"hi\" is 2 bytes; this is `mint-bodyPublisher-present-len`"
            ),
            other => panic!("bodyPublisher present must hold the publisher, got {other:?}"),
        }
    }

    // --- G34-1: the three shapes of java.net.http.HttpHeaders ---------------
    //
    // `NativeMethodRegistry::register` is last-write-wins with no unregister
    // API, and TWO registrars claim
    // `HttpHeaders.{map,firstValue,allValues,firstValueAsLong}`: this file's
    // `register_http_headers` and `net_phase_e`'s. Whichever runs last owns
    // the slot for every receiver in the VM, including the ones the OTHER
    // minter produced. These tests pin the discriminator that makes that
    // survivable, because the boot order is not something either file can see.

    /// The three minters, and the one property that separates them.
    ///
    /// MEASURED (`--dump-native-registry`, `target-rel2`, 2026-08-17): under
    /// `--jdk-only` only `net_phase_e`'s rows exist, all `overwrote=null`. In a
    /// `synthetic-jdk` build both registrars run and this file's bodies own the
    /// four slots. The guard has to hold in both.
    #[test]
    fn http_headers_counter_shape_separates_the_three_minters() {
        let mut ctx = crate::test_utils::MockNativeContext::new();

        // (a) This file's minter: 3 slots, `Value::Int` counters.
        let mine = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 3);
        init_http_headers_fields(&mut ctx, mine, 2, 1, 0);
        assert!(
            http_headers_is_counter_shape(&ctx, mine),
            "alloc_http_headers puts Value::Int in HDR_COUNT; that IS the counter shape"
        );

        // (b) `net_phase_e::re5_make_http_headers`: 1 slot holding a String[].
        //     A reference in slot 0 is the whole difference.
        let theirs = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 1);
        let arr_stand_in = ctx.create_string("Accept: text/plain");
        ctx.set_field(theirs, 0, Value::Object(Some(arr_stand_in)));
        assert!(
            !http_headers_is_counter_shape(&ctx, theirs),
            "net_phase_e keeps a String[] in slot 0; decoding it as HDR_COUNT is the hazard"
        );

        // (c) The real JDK's `HttpHeaders.of(Map, BiPredicate)`: slot 0 is the
        //     real `Map` field. MEASURED on HotSpot 25.0.3+9-LTS,
        //     `map().getClass()` is `java.util.Collections$UnmodifiableMap`.
        let real = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 1);
        ctx.set_field(real, 0, Value::Object(None));
        assert!(
            !http_headers_is_counter_shape(&ctx, real),
            "a real JDK HttpHeaders holds a Map reference in slot 0, never a counter"
        );
    }

    /// `firstValue` on a receiver this file did not mint must answer ABSENT,
    /// not invent a `content-type` out of another minter's slots.
    ///
    /// MEASURED on HotSpot: `firstValue` of a name the headers do not carry is
    /// `Optional.empty`, so declining is also the oracle-correct answer.
    #[test]
    fn http_headers_first_value_declines_a_foreign_receiver() {
        let cb = find_cb(
            "java/net/http/HttpHeaders",
            "firstValue",
            "(Ljava/lang/String;)Ljava/util/Optional;",
        );
        let mut ctx = crate::test_utils::MockNativeContext::new();

        // This file's own receiver, content-type present: still answered.
        let mine = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 3);
        init_http_headers_fields(&mut ctx, mine, 1, 1, 0);
        let name = ctx.create_string("Content-Type");
        let slot0 = opt_slot0(
            cb,
            &mut ctx,
            &[Value::Object(Some(mine)), Value::Object(Some(name))],
        );
        assert_not_a_flag("firstValue.own", &slot0);
        match slot0 {
            Value::Object(Some(sv)) => assert_eq!(
                ctx.read_string(sv).as_deref(),
                Some("application/json"),
                "this file's own receiver must keep answering exactly as before the guard"
            ),
            other => panic!("expected the content-type string, got {other:?}"),
        }

        // A `net_phase_e`-shaped receiver: ONE slot, a reference in it. The
        // guard must fire BEFORE HDR_HAS_CT/HDR_HAS_CL are read, because slots
        // 1 and 2 do not exist on this object at all.
        let theirs = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 1);
        let arr_stand_in = ctx.create_string("Content-Type: text/plain");
        ctx.set_field(theirs, 0, Value::Object(Some(arr_stand_in)));
        let name2 = ctx.create_string("Content-Type");
        let slot0 = opt_slot0(
            cb,
            &mut ctx,
            &[Value::Object(Some(theirs)), Value::Object(Some(name2))],
        );
        assert_eq!(
            slot0,
            Value::Object(None),
            "a receiver this file did not mint must get Optional.empty, not a \
             content-type fabricated from a slot that belongs to another layout"
        );
    }

    /// The same for `firstValueAsLong`, whose `OptionalLong` really does carry
    /// a `(boolean isPresent, long value)` pair — so the absent answer has to
    /// be `Int(0)` in slot 0, not merely a null reference.
    #[test]
    fn http_headers_first_value_as_long_declines_a_foreign_receiver() {
        let cb = find_cb(
            "java/net/http/HttpHeaders",
            "firstValueAsLong",
            "(Ljava/lang/String;)Ljava/util/OptionalLong;",
        );
        let mut ctx = crate::test_utils::MockNativeContext::new();

        let mine = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 3);
        init_http_headers_fields(&mut ctx, mine, 1, 0, 1);
        let name = ctx.create_string("Content-Length");
        let got = match cb(
            &mut ctx,
            &[Value::Object(Some(mine)), Value::Object(Some(name))],
        )
        .expect("native must not fail")
        {
            Some(Value::Object(Some(o))) => (ctx.get_field(o, 0), ctx.get_field(o, 1)),
            other => panic!("expected an OptionalLong, got {other:?}"),
        };
        assert_eq!(
            got,
            (Value::Int(1), Value::Long(20)),
            "this file's own receiver must keep answering exactly as before the guard"
        );

        let theirs = alloc_of(&mut ctx, "java/net/http/HttpHeaders", 1);
        let arr_stand_in = ctx.create_string("Content-Length: 20");
        ctx.set_field(theirs, 0, Value::Object(Some(arr_stand_in)));
        let name2 = ctx.create_string("Content-Length");
        let got = match cb(
            &mut ctx,
            &[Value::Object(Some(theirs)), Value::Object(Some(name2))],
        )
        .expect("native must not fail")
        {
            Some(Value::Object(Some(o))) => (ctx.get_field(o, 0), ctx.get_field(o, 1)),
            other => panic!("expected an OptionalLong, got {other:?}"),
        };
        assert_eq!(
            got,
            (Value::Int(0), Value::Long(0)),
            "a foreign receiver must get OptionalLong.empty, not a fabricated 20"
        );
    }

    /// The ratchet. Every accessor `register_http_headers` registers reads a
    /// slot map that only ONE of the three minters produces, so every one of
    /// them must consult the discriminator — or, for `map`, read no counter at
    /// all. A guard dropped in a later edit is silent at runtime (it answers a
    /// plausible wrong header) and this is what makes it loud instead.
    ///
    /// Source-level on purpose: the failure being ratcheted is "someone edits
    /// this function and forgets", which no amount of behaviour on today's
    /// receivers can catch.
    #[test]
    fn every_http_headers_accessor_is_shape_guarded() {
        let src = include_str!("http2.rs");
        let start = src
            .find("fn register_http_headers(")
            .expect("register_http_headers must exist");
        let end = src[start..]
            .find("\nfn register_body_publisher(")
            .map(|o| start + o)
            .expect("register_body_publisher must follow register_http_headers");
        let body = &src[start..end];

        let guards = body
            .matches("http_headers_is_counter_shape(ctx, this)")
            .count();
        assert_eq!(
            guards, 3,
            "allValues / firstValue / firstValueAsLong must each test \
             http_headers_is_counter_shape before reading HDR_HAS_CT/HDR_HAS_CL. \
             `map` is the fourth accessor and is guarded more strongly — it \
             reads no counter slot at all."
        );

        // `map` must not have regained a counter read.
        let map_start = body
            .find("r.register(cls, \"map\", \"()Ljava/util/Map;\"")
            .expect("map must still be registered");
        assert!(
            !body[map_start..].contains("get_field(this, HDR_COUNT)"),
            "map() must not read HDR_COUNT: the slot is a counter only on this \
             file's own receiver, and the returned map carries no header names \
             to put in it either way"
        );
    }

    /// The `java/util/HashMap` and `java/util/ArrayList` this file hands back
    /// must be shaped the way the rest of the crate shapes them, because it is
    /// `native-collections`' registered natives — not this file — that answer
    /// `size()`/`get()` on them afterwards.
    ///
    /// MEASURED by sweep (2026-08-17): before this change `http2.rs` was the
    /// ONLY `java/util/HashMap` allocation in `native-builtins` that asked for
    /// 2 slots; seven other files ask for 3 and pair it with `native_map_init`.
    #[test]
    fn collection_returns_follow_the_crate_wide_slot_convention() {
        let src = include_str!("http2.rs");
        assert!(
            !src.contains("try_alloc_concurrent_synthetic(ctx, \"java/util/HashMap\", 2)"),
            "a 2-slot java/util/HashMap cannot be read by any registered \
             native_map_* body; the crate convention is 3 slots + native_map_init"
        );
        assert!(
            src.contains("cratonvm_native_collections::native_map_init"),
            "the HashMap this file returns must be initialised by the same \
             helper every other allocator in the crate uses"
        );
        assert!(
            src.contains("cratonvm_native_collections::native_al_init"),
            "the ArrayList this file returns must be initialised by \
             native_al_init; hand-writing Int into slot 0 type-puns the slot \
             native_al_init keeps the backing Object[] in"
        );
    }
}
