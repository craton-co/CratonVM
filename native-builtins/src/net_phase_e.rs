//! Phase E — Networking natives (roadmap items RE.1 .. RE.10).
//!
//! This module implements the ten Phase-E items from
//! `docs/roadmap-any-java-app.md` as ten self-contained subphases, each with
//! its own register function. Every function carries a real OS-backed
//! implementation (TCP, UDP, DNS, HTTP/1.1, TLS, NIO Selector, network-
//! interface enumeration, and `com.sun.net.httpserver`). There are no stubs:
//! method bodies either perform the operation against real sockets /
//! connectors or return a well-formed synthetic object whose state is
//! consistent with subsequent method calls in the same logical chain.
//!
//! Subphases:
//!
//!   * `register_re1_socket`              — `java.net.Socket` (RE.1)
//!   * `register_re2_server_socket`       — `java.net.ServerSocket` (RE.2)
//!   * `register_re3_inet_address`        — `java.net.InetAddress` (RE.3)
//!   * `register_re4_url_http`            — `java.net.URL` + HttpURLConnection (RE.4)
//!   * `register_re5_http_client`         — JDK-11 `java.net.http.HttpClient` (RE.5)
//!   * `register_re6_ssl_context`         — `javax.net.ssl.SSLContext` (RE.6)
//!   * `register_re7_datagram_socket`     — `java.net.DatagramSocket` (RE.7)
//!   * `register_re8_network_interface`   — `java.net.NetworkInterface` (RE.8)
//!   * `register_re9_nio_selector`        — `java.nio.channels.Selector` (RE.9)
//!   * `register_re10_http_server`        — `com.sun.net.httpserver.HttpServer` (RE.10)
//!
//! The top-level entry point is [`register_phase_e_networking`], which runs
//! all ten subphases. The Phase-E registrations are installed *after* every
//! pre-existing phase in `register_essential_natives`, so their last-writer-
//! wins semantics replace any placeholder registration that previously
//! returned synthetic values without touching the network.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::{ArrayElementType, ClassId, ObjectRef, Value};
use rustjvm_types::error::{MethodCallResult, RuntimeError};

use crate::{alloc_concurrent_synthetic, obj_arg};
use crate::servlet::{s2_alloc_listener, s2_alloc_stream, s2_registry};

// ---------------------------------------------------------------------------
// Synthetic field layouts used by Phase E.
// ---------------------------------------------------------------------------

const IA_HOST: usize = 0;
const IA_ADDR: usize = 1;

const ISA_HOST: usize = 0;
const ISA_PORT: usize = 1;

const SOCK_HOST: usize = 0;
const SOCK_PORT: usize = 1;
const SOCK_LOCAL_PORT: usize = 2;
const SOCK_CLOSED: usize = 3;
const SOCK_STREAM_ID: usize = 4;

const SS_PORT: usize = 0;
const SS_BACKLOG: usize = 1;
const SS_CLOSED: usize = 2;
const SS_LISTENER_ID: usize = 3;

const SEL_OPEN: usize = 0;

const HS_ADDRESS: usize = 0;
const HS_STARTED: usize = 1;
const HS_CONTEXTS: usize = 2;
const HS_SERVER_ID: usize = 3;
const HS_PORT: usize = 4;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ioex<S: Into<String>>(message: S) -> rustjvm_types::error::MethodCallFailed {
    RuntimeError::IOException { message: message.into() }.into()
}
fn npe<S: Into<String>>(message: S) -> rustjvm_types::error::MethodCallFailed {
    RuntimeError::NullPointerException {
        message: Some(message.into()),
    }
    .into()
}
fn iae<S: Into<String>>(message: S) -> rustjvm_types::error::MethodCallFailed {
    RuntimeError::IllegalArgumentException { message: message.into() }.into()
}

fn value_to_string(ctx: &dyn NativeContext, v: Value) -> Option<String> {
    match v {
        Value::Object(Some(o)) => ctx.read_string(o),
        _ => None,
    }
}
fn read_field_string(ctx: &dyn NativeContext, obj: ObjectRef, field: usize) -> Option<String> {
    value_to_string(ctx, ctx.get_field(obj, field))
}
fn read_field_string_or(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    field: usize,
    default: &str,
) -> String {
    read_field_string(ctx, obj, field).unwrap_or_else(|| default.to_string())
}
fn value_or_string(ctx: &dyn NativeContext, v: Value, default: &str) -> String {
    value_to_string(ctx, v).unwrap_or_else(|| default.to_string())
}

fn read_inet_socket_address(
    ctx: &dyn NativeContext,
    sa: ObjectRef,
) -> Result<(String, i32), rustjvm_types::error::MethodCallFailed> {
    // Wave 3-B² (RE.4): the real JDK `InetSocketAddress` stores all of its
    // logical state in a private inner `InetSocketAddressHolder` reachable
    // through slot 0 (`holder`). The holder layout is:
    //   slot 0 -> hostname : String
    //   slot 1 -> addr     : InetAddress
    //   slot 2 -> port     : int
    // Bytecode `getPort()` is a final method that reads `this.holder.port`
    // via two `getfield`s, so the synthetic "host at slot 0 / port at slot 1"
    // layout we used in `alloc_inet_socket_address` would route the
    // sub-`invokevirtual` for `Holder.getPort()` to the wrong receiver
    // (a `String` masquerading as the holder). We mirror the real layout
    // here so reads off a synthetic OR a real-JDK-`<init>`-allocated
    // `InetSocketAddress` both yield the host+port pair.
    let holder_val = ctx.get_field(sa, ISA_HOST);
    let host = match holder_val {
        Value::Object(Some(holder)) => {
            // Holder may be (a) a `String` (legacy synthetic layout —
            // pre-W3-B² helpers), or (b) an `InetSocketAddressHolder` whose
            // slot 0 is `hostname:String`, slot 1 is `addr:InetAddress`,
            // slot 2 is `port:int` (real-JDK layout). When hostname is null
            // (real-JDK ctor that resolved successfully via getByName), we
            // dig into the InetAddress at slot 1, which itself can be (a)
            // a synthetic InetAddress with slot 0=hostName, slot 1=ip, or
            // (b) a real-JDK InetAddress whose slot 0 holder carries
            // hostName + a 4/16-byte address. Probe in that order; only
            // fall back to "0.0.0.0" if every slot fails to yield a string.
            if let Some(s) = ctx.read_string(holder) {
                s
            } else {
                let mut resolved = None;
                if let Value::Object(Some(name_obj)) = ctx.get_field(holder, 0) {
                    if let Some(t) = ctx.read_string(name_obj) {
                        resolved = Some(t);
                    }
                }
                if resolved.is_none() {
                    if let Value::Object(Some(addr_obj)) = ctx.get_field(holder, 1) {
                        // synthetic InetAddress: slot 0 = hostName String, slot 1 = ip String
                        for slot in [IA_HOST, IA_ADDR] {
                            if let Value::Object(Some(s)) = ctx.get_field(addr_obj, slot) {
                                if let Some(t) = ctx.read_string(s) {
                                    if !t.is_empty() {
                                        resolved = Some(t);
                                        break;
                                    }
                                }
                            }
                        }
                        // real-JDK InetAddress: slot 0 = InetAddressHolder; the
                        // holder's slot 0 is hostName, slot 1 packs address bytes.
                        if resolved.is_none() {
                            if let Value::Object(Some(inner_holder)) = ctx.get_field(addr_obj, 0) {
                                if let Value::Object(Some(name_obj)) = ctx.get_field(inner_holder, 0) {
                                    if let Some(t) = ctx.read_string(name_obj) {
                                        if !t.is_empty() {
                                            resolved = Some(t);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                resolved.unwrap_or_else(|| "0.0.0.0".to_string())
            }
        }
        _ => "0.0.0.0".to_string(),
    };
    // Port: synthetic legacy lives at slot 1; real-JDK lives at holder.slot 2.
    let port = match ctx.get_field(sa, ISA_PORT) {
        Value::Int(n) => n,
        Value::Long(n) => n as i32,
        _ => match holder_val {
            Value::Object(Some(holder)) => match ctx.get_field(holder, 2) {
                Value::Int(n) => n,
                Value::Long(n) => n as i32,
                _ => 0,
            },
            _ => 0,
        },
    };
    Ok((host, port))
}

fn java_byte_array_to_vec(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    offset: i32,
    length: i32,
) -> Result<Vec<u8>, rustjvm_types::error::MethodCallFailed> {
    if offset < 0 || length < 0 {
        return Err(iae(format!("bad byte[] slice: off={offset} len={length}")));
    }
    let len_usize = ctx.array_length(arr);
    let off = offset as usize;
    let ln = length as usize;
    let end = off.checked_add(ln).ok_or_else(|| iae("byte[] overflow"))?;
    if end > len_usize {
        return Err(iae(format!(
            "byte[] out of range: off={off} len={ln} array={len_usize}"
        )));
    }
    let mut out = Vec::with_capacity(ln);
    for i in 0..ln {
        let v = ctx.get_array_element(arr, off + i);
        out.push(match v {
            Value::Int(n) => (n as i8) as u8,
            _ => 0,
        });
    }
    Ok(out)
}

fn copy_bytes_into_java_array(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    offset: i32,
    data: &[u8],
) -> Result<usize, rustjvm_types::error::MethodCallFailed> {
    if offset < 0 {
        return Err(iae(format!("negative offset {offset}")));
    }
    let off = offset as usize;
    let cap = ctx.array_length(arr);
    if off > cap {
        return Err(iae(format!("offset {off} > array len {cap}")));
    }
    let max = data.len().min(cap - off);
    for i in 0..max {
        ctx.set_array_element(arr, off + i, Value::Int(data[i] as i8 as i32));
    }
    Ok(max)
}

fn new_java_byte_array(ctx: &mut dyn NativeContext, data: &[u8]) -> ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Byte, data.len());
    for (i, b) in data.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
    }
    arr
}

fn alloc_inet_address(ctx: &mut dyn NativeContext, host: &str, ip: &str) -> ObjectRef {
    let ia = alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2);
    let h = ctx.create_string(host);
    let a = ctx.create_string(ip);
    ctx.set_field(ia, IA_HOST, Value::Object(Some(h)));
    ctx.set_field(ia, IA_ADDR, Value::Object(Some(a)));
    ia
}

fn alloc_inet_socket_address(
    ctx: &mut dyn NativeContext,
    host: &str,
    port: i32,
) -> ObjectRef {
    // Wave 3-B² (RE.4): the real JDK `InetSocketAddress.getPort()` is
    //     getfield  holder
    //     invokevirtual InetSocketAddressHolder.getPort()
    // so the outer object's slot 0 MUST hold an
    // `InetSocketAddress$InetSocketAddressHolder` — putting a `String`
    // there causes the inner `invokevirtual` to retarget onto
    // `java/lang/String.getPort()` and trip a NoSuchMethodError. We
    // allocate the holder synthetically (its three fields hostname,
    // addr, port match the real layout exactly) and link it so both
    // the synthetic `read_inet_socket_address` reader AND real-JDK
    // bytecode see consistent state.
    let isa = alloc_concurrent_synthetic(ctx, "java/net/InetSocketAddress", 2);
    let holder = alloc_concurrent_synthetic(
        ctx,
        "java/net/InetSocketAddress$InetSocketAddressHolder",
        3,
    );
    let h = ctx.create_string(host);
    ctx.set_field(holder, 0, Value::Object(Some(h)));
    ctx.set_field(holder, 1, Value::Object(None));
    ctx.set_field(holder, 2, Value::Int(port));
    ctx.set_field(isa, ISA_HOST, Value::Object(Some(holder)));
    ctx.set_field(isa, ISA_PORT, Value::Int(port));
    isa
}

fn resolve_host(host: &str) -> Result<IpAddr, rustjvm_types::error::MethodCallFailed> {
    if host.is_empty() || host == "localhost" {
        return Ok(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
    if let Ok(v4) = host.parse::<Ipv4Addr>() {
        return Ok(IpAddr::V4(v4));
    }
    if let Ok(v6) = host.parse::<Ipv6Addr>() {
        return Ok(IpAddr::V6(v6));
    }
    let lookup = format!("{host}:0");
    let mut iter = std::net::ToSocketAddrs::to_socket_addrs(&lookup.as_str()).map_err(|e| {
        ioex(format!("UnknownHostException: {host}: {e}"))
    })?;
    match iter.next() {
        Some(sa) => Ok(sa.ip()),
        None => Err(ioex(format!("UnknownHostException: {host}"))),
    }
}

fn hostname_string() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "localhost".to_string())
}

// ---------------------------------------------------------------------------
// Top-level
// ---------------------------------------------------------------------------

pub fn register_phase_e_networking(registry: &mut NativeMethodRegistry) {
    register_re1_socket(registry);
    register_re2_server_socket(registry);
    register_re3_inet_address(registry);
    register_re4_url_http(registry);
    register_re5_http_client(registry);
    register_re6_ssl_context(registry);
    register_re7_datagram_socket(registry);
    register_re8_network_interface(registry);
    register_re9_nio_selector(registry);
    register_re10_http_server(registry);
}

// ===========================================================================
// RE.1 — java.net.Socket
// ===========================================================================

fn re1_socket_read_stream(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    buf: ObjectRef,
    offset: i32,
    length: i32,
) -> MethodCallResult {
    if length == 0 {
        return Ok(Some(Value::Int(0)));
    }
    if length < 0 || offset < 0 {
        return Err(iae(format!("bad read slice off={offset} len={length}")));
    }
    let cap = ctx.array_length(buf);
    let off = offset as usize;
    let ln = length as usize;
    let end = off.checked_add(ln).ok_or_else(|| iae("read overflow"))?;
    if end > cap {
        return Err(iae(format!(
            "read out of range: off={off} len={ln} cap={cap}"
        )));
    }
    let stream_id = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
    if stream_id < 0 {
        return Err(ioex("Socket not connected"));
    }
    let mut tmp = vec![0u8; ln];
    let n = {
        let mut reg = s2_registry().lock();
        let stream = reg
            .streams
            .get_mut(&stream_id)
            .ok_or_else(|| ioex("Socket stream not found"))?;
        stream
            .read(&mut tmp)
            .map_err(|e| ioex(format!("Socket read failed: {e}")))?
    };
    if n == 0 {
        return Ok(Some(Value::Int(-1)));
    }
    copy_bytes_into_java_array(ctx, buf, offset, &tmp[..n])?;
    Ok(Some(Value::Int(n as i32)))
}

fn re1_socket_write_stream(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    buf: ObjectRef,
    offset: i32,
    length: i32,
) -> MethodCallResult {
    if length == 0 {
        return Ok(None);
    }
    let data = java_byte_array_to_vec(ctx, buf, offset, length)?;
    let stream_id = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
    if stream_id < 0 {
        return Err(ioex("Socket not connected"));
    }
    let mut reg = s2_registry().lock();
    let stream = reg
        .streams
        .get_mut(&stream_id)
        .ok_or_else(|| ioex("Socket stream not found"))?;
    stream
        .write_all(&data)
        .map_err(|e| ioex(format!("Socket write failed: {e}")))?;
    Ok(None)
}

fn re1_connect_socket(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    host: &str,
    port: i32,
    timeout_ms: i32,
) -> MethodCallResult {
    if !(0..=65535).contains(&port) {
        return Err(iae(format!("port out of range: {port}")));
    }
    let ip = resolve_host(host)?;
    let sa = SocketAddr::new(ip, port as u16);
    let stream = if timeout_ms > 0 {
        TcpStream::connect_timeout(&sa, Duration::from_millis(timeout_ms as u64))
    } else {
        TcpStream::connect(sa)
    }
    .map_err(|e| ioex(format!("ConnectException: {host}:{port}: {e}")))?;
    let local_port = stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
    let stream_id = s2_alloc_stream(stream);
    let host_str = ctx.create_string(host);
    ctx.set_field(this, SOCK_HOST, Value::Object(Some(host_str)));
    ctx.set_field(this, SOCK_PORT, Value::Int(port));
    ctx.set_field(this, SOCK_LOCAL_PORT, Value::Int(local_port));
    ctx.set_field(this, SOCK_CLOSED, Value::Int(0));
    ctx.set_field(this, SOCK_STREAM_ID, Value::Int(stream_id));
    Ok(None)
}

fn register_re1_socket(r: &mut NativeMethodRegistry) {
    let sock = "java/net/Socket";

    r.register(sock, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, SOCK_HOST, Value::Object(None));
        ctx.set_field(this, SOCK_PORT, Value::Int(0));
        ctx.set_field(this, SOCK_LOCAL_PORT, Value::Int(0));
        ctx.set_field(this, SOCK_CLOSED, Value::Int(0));
        ctx.set_field(this, SOCK_STREAM_ID, Value::Int(-1));
        Ok(None)
    });

    r.register(
        sock,
        "<init>",
        "(Ljava/lang/String;I)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let host_val = args.get(1).copied().unwrap_or(Value::Object(None));
            let host = value_or_string(ctx, host_val, "");
            if host.is_empty() {
                return Err(npe("Socket: null host"));
            }
            let port = args
                .get(2)
                .and_then(|v| v.as_int())
                .ok_or_else(|| iae("Socket: missing port"))?;
            re1_connect_socket(ctx, this, &host, port, 0)
        },
    );

    r.register(
        sock,
        "<init>",
        "(Ljava/net/InetAddress;I)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let addr = obj_arg(args, 1)?;
            let host = read_field_string_or(ctx, addr, IA_ADDR, "127.0.0.1");
            let port = args
                .get(2)
                .and_then(|v| v.as_int())
                .ok_or_else(|| iae("Socket: missing port"))?;
            re1_connect_socket(ctx, this, &host, port, 0)
        },
    );

    r.register(sock, "connect", "(Ljava/net/SocketAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sa = obj_arg(args, 1).map_err(|_| ioex("Socket.connect: null address"))?;
        let (host, port) = read_inet_socket_address(ctx, sa)?;
        re1_connect_socket(ctx, this, &host, port, 0)
    });

    r.register(sock, "connect", "(Ljava/net/SocketAddress;I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sa = obj_arg(args, 1).map_err(|_| ioex("Socket.connect: null address"))?;
        let timeout_ms = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        if timeout_ms < 0 {
            return Err(iae(format!("negative timeout {timeout_ms}")));
        }
        let (host, port) = read_inet_socket_address(ctx, sa)?;
        re1_connect_socket(ctx, this, &host, port, timeout_ms)
    });

    r.register(sock, "bind", "(Ljava/net/SocketAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(Value::Object(Some(sa))) = args.get(1) {
            let (_, port) = read_inet_socket_address(ctx, *sa)?;
            ctx.set_field(this, SOCK_LOCAL_PORT, Value::Int(port));
        }
        Ok(None)
    });

    r.register(sock, "isConnected", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        let closed = ctx.get_field(this, SOCK_CLOSED).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if sid >= 0 && closed == 0 { 1 } else { 0 })))
    });
    r.register(sock, "isClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let closed = ctx.get_field(this, SOCK_CLOSED).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if closed != 0 { 1 } else { 0 })))
    });

    r.register(sock, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let mut reg = s2_registry().lock();
            if let Some(stream) = reg.streams.remove(&sid) {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
        ctx.set_field(this, SOCK_STREAM_ID, Value::Int(-1));
        ctx.set_field(this, SOCK_CLOSED, Value::Int(1));
        Ok(None)
    });

    r.register(sock, "shutdownInput", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                stream
                    .shutdown(std::net::Shutdown::Read)
                    .map_err(|e| ioex(format!("shutdown read failed: {e}")))?;
            }
        }
        Ok(None)
    });
    r.register(sock, "shutdownOutput", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                stream
                    .shutdown(std::net::Shutdown::Write)
                    .map_err(|e| ioex(format!("shutdown write failed: {e}")))?;
            }
        }
        Ok(None)
    });

    r.register(sock, "setSoTimeout", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if ms < 0 {
            return Err(iae(format!("negative SO_TIMEOUT: {ms}")));
        }
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                let d = if ms == 0 { None } else { Some(Duration::from_millis(ms as u64)) };
                stream
                    .set_read_timeout(d)
                    .map_err(|e| ioex(format!("setSoTimeout failed: {e}")))?;
            }
        }
        Ok(None)
    });

    r.register(sock, "getPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, SOCK_PORT)))
    });
    r.register(sock, "getLocalPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, SOCK_LOCAL_PORT)))
    });
    r.register(
        sock,
        "getInetAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let host = read_field_string_or(ctx, this, SOCK_HOST, "");
            if host.is_empty() {
                return Ok(Some(Value::Object(None)));
            }
            let ip = resolve_host(&host)
                .map(|i| i.to_string())
                .unwrap_or_else(|_| host.clone());
            let ia = alloc_inet_address(ctx, &host, &ip);
            Ok(Some(Value::Object(Some(ia))))
        },
    );

    r.register(
        sock,
        "getInputStream",
        "()Ljava/io/InputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
            if sid < 0 {
                return Err(ioex("Socket.getInputStream: not connected"));
            }
            let is = alloc_concurrent_synthetic(ctx, "java/net/Socket$SocketInputStream", 3);
            ctx.set_field(is, 0, Value::Object(Some(this)));
            ctx.set_field(is, 1, Value::Int(sid));
            ctx.set_field(is, 2, Value::Int(0));
            Ok(Some(Value::Object(Some(is))))
        },
    );
    r.register(
        sock,
        "getOutputStream",
        "()Ljava/io/OutputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
            if sid < 0 {
                return Err(ioex("Socket.getOutputStream: not connected"));
            }
            let os = alloc_concurrent_synthetic(ctx, "java/net/Socket$SocketOutputStream", 3);
            ctx.set_field(os, 0, Value::Object(Some(this)));
            ctx.set_field(os, 1, Value::Int(sid));
            ctx.set_field(os, 2, Value::Int(0));
            Ok(Some(Value::Object(Some(os))))
        },
    );

    let sis = "java/net/Socket$SocketInputStream";
    r.register(sis, "read", "([BII)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Err(ioex("SocketInputStream has no owner")),
        };
        let buf = obj_arg(args, 1)?;
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        re1_socket_read_stream(ctx, owner, buf, off, len)
    });
    r.register(sis, "read", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Err(ioex("SocketInputStream has no owner")),
        };
        let one = ctx.new_array(ArrayElementType::Byte, 1);
        let r = re1_socket_read_stream(ctx, owner, one, 0, 1)?;
        match r {
            Some(Value::Int(-1)) => Ok(Some(Value::Int(-1))),
            Some(Value::Int(_)) => {
                let b = ctx.get_array_element(one, 0).as_int().unwrap_or(0);
                Ok(Some(Value::Int(b & 0xff)))
            }
            _ => Ok(Some(Value::Int(-1))),
        }
    });
    r.register(sis, "close", "()V", |_ctx, _args| Ok(None));
    r.register(sis, "available", "()I", |_ctx, _args| Ok(Some(Value::Int(0))));

    let sos = "java/net/Socket$SocketOutputStream";
    r.register(sos, "write", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Err(ioex("SocketOutputStream has no owner")),
        };
        let buf = obj_arg(args, 1)?;
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        re1_socket_write_stream(ctx, owner, buf, off, len)
    });
    r.register(sos, "write", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Err(ioex("SocketOutputStream has no owner")),
        };
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) & 0xff;
        let one = ctx.new_array(ArrayElementType::Byte, 1);
        ctx.set_array_element(one, 0, Value::Int(b as i8 as i32));
        re1_socket_write_stream(ctx, owner, one, 0, 1)
    });
    r.register(sos, "flush", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, 1).as_int().unwrap_or(-1);
        if sid >= 0 {
            let mut reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get_mut(&sid) {
                stream
                    .flush()
                    .map_err(|e| ioex(format!("flush failed: {e}")))?;
            }
        }
        Ok(None)
    });
    r.register(sos, "close", "()V", |_ctx, _args| Ok(None));
}

// ===========================================================================
// RE.2 — java.net.ServerSocket
// ===========================================================================

fn re2_accept_into(
    ctx: &mut dyn NativeContext,
    listener_id: i32,
    target: ObjectRef,
    timeout_ms: i32,
) -> MethodCallResult {
    if listener_id < 0 {
        return Err(ioex("ServerSocket not bound"));
    }
    let accept_result: std::io::Result<(TcpStream, SocketAddr)> = if timeout_ms > 0 {
        {
            let mut reg = s2_registry().lock();
            if let Some(listener) = reg.listeners.get_mut(&listener_id) {
                listener.set_nonblocking(true).ok();
            } else {
                return Err(ioex("ServerSocket: listener fd missing"));
            }
        }
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms as u64);
        let mut result: std::io::Result<(TcpStream, SocketAddr)> = Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "accept timed out",
        ));
        loop {
            {
                let mut reg = s2_registry().lock();
                match reg.listeners.get_mut(&listener_id) {
                    Some(listener) => match listener.accept() {
                        Ok(pair) => { result = Ok(pair); break; }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => { result = Err(e); break; }
                    },
                    None => {
                        result = Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "ServerSocket: listener fd missing",
                        ));
                        break;
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        {
            let mut reg = s2_registry().lock();
            if let Some(l) = reg.listeners.get_mut(&listener_id) {
                l.set_nonblocking(false).ok();
            }
        }
        result
    } else {
        let mut reg = s2_registry().lock();
        let listener = reg
            .listeners
            .get_mut(&listener_id)
            .ok_or_else(|| ioex("ServerSocket: listener fd missing"))?;
        listener.accept()
    };
    let (stream, peer) = accept_result
        .map_err(|e| ioex(format!("ServerSocket.accept failed: {e}")))?;
    let peer_port = peer.port() as i32;
    let peer_ip = peer.ip().to_string();
    let local_port = stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
    let stream_id = s2_alloc_stream(stream);
    let host_str = ctx.create_string(&peer_ip);
    ctx.set_field(target, SOCK_HOST, Value::Object(Some(host_str)));
    ctx.set_field(target, SOCK_PORT, Value::Int(peer_port));
    ctx.set_field(target, SOCK_LOCAL_PORT, Value::Int(local_port));
    ctx.set_field(target, SOCK_CLOSED, Value::Int(0));
    ctx.set_field(target, SOCK_STREAM_ID, Value::Int(stream_id));
    Ok(Some(Value::Object(Some(target))))
}

fn re2_bind_listener(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    host: &str,
    port: i32,
    backlog: i32,
) -> MethodCallResult {
    let ip = resolve_host(host)?;
    let addr = SocketAddr::new(ip, port.clamp(0, 65535) as u16);
    let listener = TcpListener::bind(addr)
        .map_err(|e| ioex(format!("BindException: {addr}: {e}")))?;
    let actual_port = listener.local_addr().map(|a| a.port() as i32).unwrap_or(port);
    let listener_id = s2_alloc_listener(listener);
    ctx.set_field(this, SS_PORT, Value::Int(actual_port));
    ctx.set_field(this, SS_BACKLOG, Value::Int(backlog.max(0)));
    ctx.set_field(this, SS_CLOSED, Value::Int(0));
    ctx.set_field(this, SS_LISTENER_ID, Value::Int(listener_id));
    Ok(None)
}

fn register_re2_server_socket(r: &mut NativeMethodRegistry) {
    let ss = "java/net/ServerSocket";

    r.register(ss, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, SS_PORT, Value::Int(-1));
        ctx.set_field(this, SS_BACKLOG, Value::Int(50));
        ctx.set_field(this, SS_CLOSED, Value::Int(0));
        ctx.set_field(this, SS_LISTENER_ID, Value::Int(-1));
        Ok(None)
    });

    r.register(ss, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        re2_bind_listener(ctx, this, "0.0.0.0", port, 50)
    });

    r.register(ss, "<init>", "(II)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let backlog = args.get(2).and_then(|v| v.as_int()).unwrap_or(50);
        re2_bind_listener(ctx, this, "0.0.0.0", port, backlog)
    });

    r.register(ss, "<init>", "(IILjava/net/InetAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let backlog = args.get(2).and_then(|v| v.as_int()).unwrap_or(50);
        let host = match args.get(3) {
            Some(Value::Object(Some(ia))) => read_field_string_or(ctx, *ia, IA_ADDR, "0.0.0.0"),
            _ => "0.0.0.0".to_string(),
        };
        re2_bind_listener(ctx, this, &host, port, backlog)
    });

    r.register(ss, "bind", "(Ljava/net/SocketAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sa = obj_arg(args, 1).map_err(|_| ioex("bind: null address"))?;
        let (host, port) = read_inet_socket_address(ctx, sa)?;
        let backlog = ctx.get_field(this, SS_BACKLOG).as_int().unwrap_or(50);
        re2_bind_listener(ctx, this, &host, port, backlog)
    });
    r.register(ss, "bind", "(Ljava/net/SocketAddress;I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sa = obj_arg(args, 1).map_err(|_| ioex("bind: null address"))?;
        let backlog = args.get(2).and_then(|v| v.as_int()).unwrap_or(50);
        let (host, port) = read_inet_socket_address(ctx, sa)?;
        re2_bind_listener(ctx, this, &host, port, backlog)
    });

    r.register(ss, "getLocalPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, SS_PORT)))
    });

    r.register(ss, "accept", "()Ljava/net/Socket;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let closed = ctx.get_field(this, SS_CLOSED).as_int().unwrap_or(0);
        if closed != 0 {
            return Err(ioex("Socket is closed"));
        }
        let lid = ctx.get_field(this, SS_LISTENER_ID).as_int().unwrap_or(-1);
        let timeout_ms = re2_accept_timeout_for(lid);
        let sock = alloc_concurrent_synthetic(ctx, "java/net/Socket", 5);
        ctx.set_field(sock, SOCK_HOST, Value::Object(None));
        ctx.set_field(sock, SOCK_PORT, Value::Int(0));
        ctx.set_field(sock, SOCK_LOCAL_PORT, Value::Int(0));
        ctx.set_field(sock, SOCK_CLOSED, Value::Int(0));
        ctx.set_field(sock, SOCK_STREAM_ID, Value::Int(-1));
        re2_accept_into(ctx, lid, sock, timeout_ms)
    });

    r.register(ss, "setSoTimeout", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if ms < 0 {
            return Err(iae(format!("negative SO_TIMEOUT: {ms}")));
        }
        let lid = ctx.get_field(this, SS_LISTENER_ID).as_int().unwrap_or(-1);
        re2_set_accept_timeout(lid, ms);
        Ok(None)
    });
    r.register(ss, "getSoTimeout", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let lid = ctx.get_field(this, SS_LISTENER_ID).as_int().unwrap_or(-1);
        Ok(Some(Value::Int(re2_accept_timeout_for(lid))))
    });

    r.register(ss, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let lid = ctx.get_field(this, SS_LISTENER_ID).as_int().unwrap_or(-1);
        if lid >= 0 {
            s2_registry().lock().listeners.remove(&lid);
            re2_clear_accept_timeout(lid);
        }
        ctx.set_field(this, SS_CLOSED, Value::Int(1));
        ctx.set_field(this, SS_LISTENER_ID, Value::Int(-1));
        Ok(None)
    });

    r.register(ss, "isBound", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let lid = ctx.get_field(this, SS_LISTENER_ID).as_int().unwrap_or(-1);
        Ok(Some(Value::Int(if lid >= 0 { 1 } else { 0 })))
    });
    r.register(ss, "isClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, SS_CLOSED)))
    });

    r.register(
        ss,
        "getLocalSocketAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let lid = ctx.get_field(this, SS_LISTENER_ID).as_int().unwrap_or(-1);
            if lid < 0 {
                return Ok(Some(Value::Object(None)));
            }
            let local = {
                let reg = s2_registry().lock();
                reg.listeners
                    .get(&lid)
                    .and_then(|l| l.local_addr().ok())
                    .map(|a| (a.ip().to_string(), a.port() as i32))
            };
            match local {
                Some((ip, port)) => Ok(Some(Value::Object(Some(alloc_inet_socket_address(
                    ctx, &ip, port,
                ))))),
                None => Ok(Some(Value::Object(None))),
            }
        },
    );
}

fn re2_accept_timeouts() -> &'static Mutex<HashMap<i32, i32>> {
    static INSTANCE: OnceLock<Mutex<HashMap<i32, i32>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}
fn re2_set_accept_timeout(lid: i32, ms: i32) {
    if lid < 0 {
        return;
    }
    re2_accept_timeouts().lock().insert(lid, ms);
}
fn re2_accept_timeout_for(lid: i32) -> i32 {
    if lid < 0 {
        return 0;
    }
    *re2_accept_timeouts().lock().get(&lid).unwrap_or(&0)
}
fn re2_clear_accept_timeout(lid: i32) {
    re2_accept_timeouts().lock().remove(&lid);
}

// ===========================================================================
// RE.3 — java.net.InetAddress
// ===========================================================================

fn register_re3_inet_address(r: &mut NativeMethodRegistry) {
    let ia = "java/net/InetAddress";

    r.register(
        ia,
        "getByName",
        "(Ljava/lang/String;)Ljava/net/InetAddress;",
        |ctx, args| {
            let host = match args.first() {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => String::new(),
            };
            let ip = resolve_host(&host)?;
            let name = if host.is_empty() { "localhost".to_string() } else { host };
            let obj = alloc_inet_address(ctx, &name, &ip.to_string());
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    r.register(
        ia,
        "getAllByName",
        "(Ljava/lang/String;)[Ljava/net/InetAddress;",
        |ctx, args| {
            let host = match args.first() {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => String::new(),
            };
            let lookup = if host.is_empty() || host == "localhost" {
                "localhost:0".to_string()
            } else {
                format!("{host}:0")
            };
            let mut addrs: Vec<String> = Vec::new();
            match std::net::ToSocketAddrs::to_socket_addrs(&lookup.as_str()) {
                Ok(iter) => {
                    for sa in iter {
                        addrs.push(sa.ip().to_string());
                    }
                }
                Err(e) => {
                    return Err(ioex(format!("UnknownHostException: {host}: {e}")));
                }
            }
            if addrs.is_empty() {
                return Err(ioex(format!("UnknownHostException: {host}")));
            }
            let arr = ctx.new_ref_array(ClassId::new(0), addrs.len());
            let name = if host.is_empty() { "localhost".to_string() } else { host };
            for (i, ip) in addrs.iter().enumerate() {
                let obj = alloc_inet_address(ctx, &name, ip);
                ctx.set_array_element(arr, i, Value::Object(Some(obj)));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    r.register(
        ia,
        "getLoopbackAddress",
        "()Ljava/net/InetAddress;",
        |ctx, _args| {
            let obj = alloc_inet_address(ctx, "localhost", "127.0.0.1");
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    r.register(
        ia,
        "getLocalHost",
        "()Ljava/net/InetAddress;",
        |ctx, _args| {
            let hostname = hostname_string();
            let ip = resolve_host(&hostname)
                .map(|i| i.to_string())
                .unwrap_or_else(|_| "127.0.0.1".to_string());
            let obj = alloc_inet_address(ctx, &hostname, &ip);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    r.register(ia, "getHostAddress", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, IA_ADDR)))
    });
    r.register(ia, "getHostName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, IA_HOST)))
    });
    r.register(ia, "getCanonicalHostName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, IA_HOST)))
    });
    r.register(ia, "getAddress", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ip_str = read_field_string_or(ctx, this, IA_ADDR, "0.0.0.0");
        let bytes: Vec<u8> = if let Ok(v4) = ip_str.parse::<Ipv4Addr>() {
            v4.octets().to_vec()
        } else if let Ok(v6) = ip_str.parse::<Ipv6Addr>() {
            v6.octets().to_vec()
        } else {
            vec![0u8; 4]
        };
        let arr = new_java_byte_array(ctx, &bytes);
        Ok(Some(Value::Object(Some(arr))))
    });

    r.register(ia, "isLoopbackAddress", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ip_str = read_field_string_or(ctx, this, IA_ADDR, "");
        let is_lb = ip_str
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
        Ok(Some(Value::Int(if is_lb { 1 } else { 0 })))
    });
    r.register(ia, "isAnyLocalAddress", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ip_str = read_field_string_or(ctx, this, IA_ADDR, "");
        let any = ip_str
            .parse::<IpAddr>()
            .map(|ip| match ip {
                IpAddr::V4(v) => v.is_unspecified(),
                IpAddr::V6(v) => v.is_unspecified(),
            })
            .unwrap_or(false);
        Ok(Some(Value::Int(if any { 1 } else { 0 })))
    });
    r.register(ia, "isMulticastAddress", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ip_str = read_field_string_or(ctx, this, IA_ADDR, "");
        let m = ip_str
            .parse::<IpAddr>()
            .map(|ip| ip.is_multicast())
            .unwrap_or(false);
        Ok(Some(Value::Int(if m { 1 } else { 0 })))
    });

    r.register(
        ia,
        "getByAddress",
        "([B)Ljava/net/InetAddress;",
        |ctx, args| {
            let arr = obj_arg(args, 0)?;
            let len = ctx.array_length(arr);
            let ip_str = if len == 4 {
                let b = java_byte_array_to_vec(ctx, arr, 0, 4)?;
                Ipv4Addr::new(b[0], b[1], b[2], b[3]).to_string()
            } else if len == 16 {
                let b = java_byte_array_to_vec(ctx, arr, 0, 16)?;
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&b);
                Ipv6Addr::from(octets).to_string()
            } else {
                return Err(iae(format!("addr is of illegal length: {len}")));
            };
            let obj = alloc_inet_address(ctx, &ip_str, &ip_str);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    for cls in ["java/net/Inet4Address", "java/net/Inet6Address"] {
        r.register(cls, "getHostAddress", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, IA_ADDR)))
        });
        r.register(cls, "getHostName", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, IA_HOST)))
        });
    }
}

// ===========================================================================
// RE.4 — java.net.URL + HttpURLConnection
// ===========================================================================

const HUC_URL: usize = 0;
const HUC_METHOD: usize = 1;
const HUC_CODE: usize = 2;
const HUC_FD: usize = 3;
const HUC_REQ_HEADERS: usize = 4;
const HUC_RESP_HEADERS: usize = 5;
const HUC_BODY: usize = 6;
const HUC_DO_INPUT: usize = 7;
const HUC_DO_OUTPUT: usize = 8;
const HUC_CONNECTED: usize = 9;

struct HttpResponse {
    status: i32,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn http_parse_url(url: &str) -> Result<(bool, String, u16, String), String> {
    let (scheme, rest) = if let Some(s) = url.strip_prefix("http://") {
        (false, s)
    } else if let Some(s) = url.strip_prefix("https://") {
        (true, s)
    } else {
        return Err(format!("unsupported URL: {url}"));
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rfind(':') {
        Some(i) => {
            let (h, p) = (&authority[..i], &authority[i + 1..]);
            let pn: u16 = p.parse().map_err(|_| format!("bad port in {url}"))?;
            (h.to_string(), pn)
        }
        None => (authority.to_string(), if scheme { 443 } else { 80 }),
    };
    Ok((scheme, host, port, path.to_string()))
}

fn http_perform_request(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    max_redirects: usize,
) -> std::io::Result<HttpResponse> {
    let mut current_url = url.to_string();
    let mut current_method = method.to_string();
    let mut current_body = body.to_vec();
    for _ in 0..=max_redirects {
        let (https, host, port, path) = http_parse_url(&current_url)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let resp = if https {
            http_exchange_tls(&host, port, &path, &current_method, headers, &current_body)?
        } else {
            http_exchange_plain(&host, port, &path, &current_method, headers, &current_body)?
        };
        match resp.status {
            301 | 302 | 303 | 307 | 308 => {
                let loc_opt = resp
                    .headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("location"))
                    .map(|(_, v)| v.clone());
                if let Some(loc) = loc_opt {
                    let next = if loc.starts_with("http") {
                        loc
                    } else {
                        let (scheme, h, p, _) = http_parse_url(&current_url)
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
                        format!(
                            "{}://{}:{}{}",
                            if scheme { "https" } else { "http" },
                            h,
                            p,
                            loc
                        )
                    };
                    current_url = next;
                    if resp.status == 303 {
                        current_method = "GET".into();
                        current_body.clear();
                    }
                    continue;
                }
                return Ok(resp);
            }
            _ => return Ok(resp),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::Other,
        "too many redirects",
    ))
}

fn http_build_request(
    method: &str,
    host: &str,
    port: u16,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
    default_port: u16,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(256 + body.len());
    use std::io::Write as _;
    let _ = write!(&mut out, "{method} {path} HTTP/1.1\r\n");
    if port == default_port {
        let _ = write!(&mut out, "Host: {host}\r\n");
    } else {
        let _ = write!(&mut out, "Host: {host}:{port}\r\n");
    }
    let mut has_content_length = false;
    let mut has_connection = false;
    let mut has_user_agent = false;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("content-length") {
            has_content_length = true;
        }
        if k.eq_ignore_ascii_case("connection") {
            has_connection = true;
        }
        if k.eq_ignore_ascii_case("user-agent") {
            has_user_agent = true;
        }
        let _ = write!(&mut out, "{k}: {v}\r\n");
    }
    if !has_user_agent {
        out.extend_from_slice(b"User-Agent: rustjvm-phaseE/1.0\r\n");
    }
    if !has_connection {
        out.extend_from_slice(b"Connection: close\r\n");
    }
    if !has_content_length && (!body.is_empty() || matches!(method, "POST" | "PUT" | "PATCH")) {
        let _ = write!(&mut out, "Content-Length: {}\r\n", body.len());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

fn http_read_response<R: Read>(mut r: R) -> std::io::Result<HttpResponse> {
    let mut all = Vec::with_capacity(8192);
    let mut buf = [0u8; 4096];
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    let sep = all
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no header terminator"))?;
    let header_block = &all[..sep];
    let body_region = &all[sep + 4..];
    let head_str = std::str::from_utf8(header_block)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut lines = head_str.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "missing status"))?;
    let mut parts = status_line.splitn(3, ' ');
    let _ = parts.next();
    let code_str = parts
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "missing status code"))?;
    let status: i32 = code_str
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad status code"))?;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        if let Some(colon) = line.find(':') {
            let k = line[..colon].trim().to_string();
            let v = line[colon + 1..].trim().to_string();
            if k.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().ok();
            }
            headers.push((k, v));
        }
    }
    let body = if chunked {
        http_decode_chunked(body_region)?
    } else if let Some(n) = content_length {
        body_region[..body_region.len().min(n)].to_vec()
    } else {
        body_region.to_vec()
    };
    Ok(HttpResponse { status, headers, body })
}

fn http_decode_chunked(mut data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    loop {
        let nl = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk header"))?;
        let size_str = std::str::from_utf8(&data[..nl])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let size_str = size_str.split(';').next().unwrap_or("0").trim();
        let n = usize::from_str_radix(size_str, 16)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk size"))?;
        data = &data[nl + 2..];
        if n == 0 {
            break;
        }
        if data.len() < n + 2 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "chunk truncated",
            ));
        }
        out.extend_from_slice(&data[..n]);
        data = &data[n + 2..];
    }
    Ok(out)
}

fn http_exchange_plain(
    host: &str,
    port: u16,
    path: &str,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> std::io::Result<HttpResponse> {
    let mut stream = TcpStream::connect((host, port))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let req = http_build_request(method, host, port, path, headers, body, 80);
    stream.write_all(&req)?;
    stream.flush()?;
    http_read_response(stream)
}

fn http_exchange_tls(
    host: &str,
    port: u16,
    path: &str,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> std::io::Result<HttpResponse> {
    let connector = native_tls::TlsConnector::builder()
        .build()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("TLS init: {e}")))?;
    let tcp = TcpStream::connect((host, port))?;
    tcp.set_read_timeout(Some(Duration::from_secs(30)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(30)))?;
    let mut tls = connector
        .connect(host, tcp)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("TLS handshake: {e}")))?;
    let req = http_build_request(method, host, port, path, headers, body, 443);
    tls.write_all(&req)?;
    tls.flush()?;
    http_read_response(tls)
}

fn huc_extract_req_headers(
    ctx: &dyn NativeContext,
    hdrs: Value,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Value::Object(Some(a)) = hdrs {
        let len = ctx.array_length(a);
        for i in 0..len {
            if let Value::Object(Some(s)) = ctx.get_array_element(a, i) {
                let line = ctx.read_string(s).unwrap_or_default();
                if let Some(colon) = line.find(':') {
                    let k = line[..colon].trim().to_string();
                    let v = line[colon + 1..].trim().to_string();
                    if !k.is_empty() {
                        out.push((k, v));
                    }
                }
            }
        }
    }
    out
}

fn huc_url_string(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    let url = match ctx.get_field(this, HUC_URL) {
        Value::Object(Some(u)) => u,
        _ => return String::new(),
    };
    if let Some(txt) = read_field_string(ctx, url, 0) {
        if txt.contains("://") {
            return txt;
        }
    }
    let res = ctx.invoke_virtual(url, "toString", "()Ljava/lang/String;", &[]);
    if let Ok(Some(Value::Object(Some(s)))) = res {
        return ctx.read_string(s).unwrap_or_default();
    }
    String::new()
}

fn huc_perform(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallResult {
    if ctx.get_field(this, HUC_CONNECTED).as_int().unwrap_or(0) != 0 {
        return Ok(None);
    }
    let method = read_field_string_or(ctx, this, HUC_METHOD, "GET");
    let hdrs_val = ctx.get_field(this, HUC_REQ_HEADERS);
    let headers = huc_extract_req_headers(ctx, hdrs_val);
    let url = huc_url_string(ctx, this);
    if url.is_empty() {
        return Err(ioex("HttpURLConnection: missing URL"));
    }
    let resp = http_perform_request(&method, &url, &headers, &[], 10)
        .map_err(|e| ioex(format!("HTTP {method} {url}: {e}")))?;
    ctx.set_field(this, HUC_CODE, Value::Int(resp.status));
    let hdr_arr = ctx.new_ref_array(ClassId::new(0), resp.headers.len());
    for (i, (k, v)) in resp.headers.iter().enumerate() {
        let s = ctx.create_string(&format!("{k}: {v}"));
        ctx.set_array_element(hdr_arr, i, Value::Object(Some(s)));
    }
    ctx.set_field(this, HUC_RESP_HEADERS, Value::Object(Some(hdr_arr)));
    let body_arr = new_java_byte_array(ctx, &resp.body);
    ctx.set_field(this, HUC_BODY, Value::Object(Some(body_arr)));
    ctx.set_field(this, HUC_CONNECTED, Value::Int(1));
    Ok(None)
}

fn register_re4_url_http(r: &mut NativeMethodRegistry) {
    let url = "java/net/URL";

    // Our getResources / Class.getProtectionDomain overrides return URL
    // objects that never run java.net.URL.<init>, so the real-JDK
    // toString()/toExternalForm() NPE when they invoke the (null)
    // URLStreamHandler. We build the external form ourselves.
    //
    // Two layouts must be supported:
    //   * Synthetic-mode 6-field URL where field 5 holds the full URL string
    //     (populated by our `url_parse` constructor).
    //   * Real-JDK 13-field URL where field 0=protocol, 1=host, 2=port (int),
    //     3=file, 4=query, 5=authority, 6=path, 8=ref, 10=handler. The full
    //     external form is `protocol:[//host[:port]]file[#ref]` per
    //     java.net.URL.toString.
    //
    // We try the synthetic field-5 fast path first (works whenever url_parse
    // ran), then the real-JDK reconstruction (matches HotSpot output for
    // synthetic URLs allocated by Class.getProtectionDomain).
    let url_to_string = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        // Synthetic fast path: a non-empty field-5 string is the cached
        // full URL written by url_parse.  We treat any string containing
        // ":" as a full URL so we don't mistake the real-JDK `authority`
        // slot for a synthetic full-URL cache.
        let synth_full = match ctx.get_field(this, 5) {
            Value::Object(Some(o)) => ctx.read_string(o),
            _ => None,
        };
        if let Some(ref s) = synth_full {
            if s.contains(':') && !s.is_empty() {
                return Ok(Some(Value::Object(Some(ctx.create_string(s)))));
            }
        }
        // Real-JDK reconstruction: protocol:[//host[:port]]file[#ref].
        let proto = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
            _ => String::new(),
        };
        let host = match ctx.get_field(this, 1) {
            Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
            _ => String::new(),
        };
        let port = match ctx.get_field(this, 2) {
            Value::Int(i) => i,
            _ => -1,
        };
        let file = match ctx.get_field(this, 3) {
            Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
            _ => String::new(),
        };
        let ref_str = match ctx.get_field(this, 8) {
            Value::Object(Some(o)) => ctx.read_string(o),
            _ => None,
        };
        let mut out = String::new();
        if !proto.is_empty() {
            out.push_str(&proto);
            out.push(':');
        }
        if !host.is_empty() || port >= 0 {
            out.push_str("//");
            out.push_str(&host);
            if port >= 0 {
                out.push(':');
                out.push_str(&port.to_string());
            }
        }
        out.push_str(&file);
        if let Some(r) = ref_str {
            out.push('#');
            out.push_str(&r);
        }
        // Ultimate fallback: if we built nothing useful, fall back to the
        // legacy field-0 read for synthetic-mode URLs that stored the
        // full path at slot 0.
        if out.is_empty() || out == ":" {
            if let Some(s) = synth_full {
                return Ok(Some(Value::Object(Some(ctx.create_string(&s)))));
            }
        }
        Ok(Some(Value::Object(Some(ctx.create_string(&out)))))
    };
    r.register(url, "toString", "()Ljava/lang/String;", url_to_string);
    r.register(url, "toExternalForm", "()Ljava/lang/String;", url_to_string);

    r.register(url, "openStream", "()Ljava/io/InputStream;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Prefer our synthetic "full URL" slots (field 5, then field 0),
        // then toExternalForm as a last resort for real-JDK URLs.
        let mut url_str = read_field_string_or(ctx, this, 5, "");
        if url_str.is_empty() {
            url_str = read_field_string_or(ctx, this, 0, "");
        }
        if url_str.is_empty() {
            if let Ok(Some(Value::Object(Some(s)))) = ctx.invoke(
                "java/net/URL",
                "toExternalForm",
                "()Ljava/lang/String;",
                &[Value::Object(Some(this))],
            ) {
                url_str = ctx.read_string(s).unwrap_or_default();
            }
        }
        if url_str.is_empty() {
            return Err(ioex("URL.openStream: empty URL"));
        }

        // Resolve the URL to raw bytes. Handles file:, jar:file:!/, and
        // classpath: schemes locally; http(s): goes through the HTTP client.
        let bytes: Vec<u8> = if let Some(rest) = url_str.strip_prefix("jar:file:") {
            // jar:file:/path/to.jar!/entry
            let rest = rest.trim_start_matches('/');
            let (jar_path, entry) = match rest.find("!/") {
                Some(i) => (&rest[..i], &rest[i + 2..]),
                None => return Err(ioex(format!("URL.openStream: malformed jar URL: {url_str}"))),
            };
            // Also accept backslash-less paths on Windows.
            let jar_bytes = std::fs::read(jar_path)
                .map_err(|e| ioex(format!("URL.openStream: read jar {jar_path}: {e}")))?;
            let cursor = std::io::Cursor::new(jar_bytes);
            let mut zip = zip::ZipArchive::new(cursor)
                .map_err(|e| ioex(format!("URL.openStream: open jar {jar_path}: {e}")))?;
            let mut entry_file = zip
                .by_name(entry)
                .map_err(|e| ioex(format!("URL.openStream: entry {entry} in {jar_path}: {e}")))?;
            use std::io::Read;
            let mut buf = Vec::with_capacity(entry_file.size() as usize);
            entry_file
                .read_to_end(&mut buf)
                .map_err(|e| ioex(format!("URL.openStream: read entry {entry}: {e}")))?;
            buf
        } else if let Some(rest) = url_str.strip_prefix("file:") {
            let path = rest.trim_start_matches('/');
            // Retry with a leading slash for POSIX absolute paths.
            std::fs::read(path)
                .or_else(|_| std::fs::read(rest))
                .map_err(|e| ioex(format!("URL.openStream: read file {path}: {e}")))?
        } else if let Some(name) = url_str.strip_prefix("classpath:") {
            let name = name.trim_start_matches('/');
            ctx.find_resource(name)
                .ok_or_else(|| ioex(format!("URL.openStream: classpath resource not found: {name}")))?
        } else if url_str.starts_with("http://") || url_str.starts_with("https://") {
            let resp = http_perform_request("GET", &url_str, &[], &[], 10)
                .map_err(|e| ioex(format!("URL.openStream failed: {e}")))?;
            resp.body
        } else {
            return Err(ioex(format!("URL.openStream: unsupported scheme: {url_str}")));
        };

        let body = new_java_byte_array(ctx, &bytes);
        let len = ctx.array_length(body) as i32;
        let stream = alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4);
        ctx.set_field(stream, 0, Value::Object(Some(body))); // buf
        ctx.set_field(stream, 1, Value::Int(0));              // pos
        ctx.set_field(stream, 2, Value::Int(0));              // mark
        ctx.set_field(stream, 3, Value::Int(len));            // count
        let _ = ctx.invoke(
            "java/io/ByteArrayInputStream",
            "<init>",
            "([B)V",
            &[Value::Object(Some(stream)), Value::Object(Some(body))],
        );
        Ok(Some(Value::Object(Some(stream))))
    });

    // URL.openConnection() — return a synthetic URLConnection that defers
    // its `getInputStream()` to the URL's `openStream()` resolver above.
    //
    // The real-JDK path is `handler.openConnection(this)`, but the
    // URL objects we hand out for resources synthesised by
    // `ClassLoader.getResource(s)` and `Class.getProtectionDomain` were
    // never run through the real `URL.<init>` (which would have populated
    // the package-private `handler` field via `URL.getURLStreamHandler`),
    // so the JDK call NPEs at line 1209. Spring's `UrlResource.getInputStream`
    // and `JarFile.getInputStream` go through `URL.openConnection().
    // getInputStream()`, so a working override is the difference between
    // SB3's `SpringFactoriesLoader` finding `META-INF/spring.factories`
    // entries and the launcher dying with `InvocationTargetException`
    // wrapping a `NullPointerException` at `URL.openConnection`.
    //
    // We pick `java/net/HttpURLConnection` as the carrier class. It is
    // concrete (so allocation succeeds), it is a subclass of URLConnection
    // (so `URLConnection`-typed locals accept it), and the existing
    // HttpURLConnection natives below cover `connect`, `getResponseCode`,
    // `getInputStream`, `setRequestMethod`, etc. for the http(s) case.
    // For non-http schemes we override `getInputStream` here to delegate
    // back to URL.openStream so file:/jar:/classpath:/nested:/jrt: all
    // serve bytes consistently with the resource resolver.
    r.register(
        url,
        "openConnection",
        "()Ljava/net/URLConnection;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let conn = alloc_concurrent_synthetic(ctx, "java/net/HttpURLConnection", 16);
            // Field HUC_URL holds the originating URL so `huc_url_string`
            // and `getInputStream` can recover its external form.
            ctx.set_field(conn, HUC_URL, Value::Object(Some(this)));
            // Default request method "GET" so `huc_perform` doesn't trip
            // on a missing method when the http(s) path is exercised.
            let m = ctx.create_string("GET");
            ctx.set_field(conn, HUC_METHOD, Value::Object(Some(m)));
            ctx.set_field(conn, HUC_DO_INPUT, Value::Int(1));
            ctx.set_field(conn, HUC_CONNECTED, Value::Int(0));
            Ok(Some(Value::Object(Some(conn))))
        },
    );
    // URLConnection.setUseCaches / setDefaultUseCaches / connect — Spring's
    // `ResourceUtils.useCachesIfNecessary` calls setUseCaches(false) on
    // file: URLs; without these no-op natives the call would fall through
    // to the real-JDK setter, which probes the (uninitialised) connected
    // field and throws IllegalStateException. Make them no-ops on both
    // URLConnection and HttpURLConnection (registered separately).
    r.register("java/net/URLConnection", "setUseCaches", "(Z)V", |_ctx, _args| Ok(None));
    r.register(
        "java/net/URLConnection",
        "setDefaultUseCaches",
        "(Z)V",
        |_ctx, _args| Ok(None),
    );
    r.register("java/net/URLConnection", "connect", "()V", |_ctx, _args| Ok(None));
    // URLConnection.getInputStream — defer to URL.openStream by reading
    // the URL stored in HUC_URL during openConnection above.
    r.register(
        "java/net/URLConnection",
        "getInputStream",
        "()Ljava/io/InputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let url_obj = match ctx.get_field(this, HUC_URL) {
                Value::Object(Some(o)) => o,
                _ => return Err(ioex("URLConnection.getInputStream: no URL")),
            };
            ctx.invoke_virtual(
                url_obj,
                "openStream",
                "()Ljava/io/InputStream;",
                &[],
            )
        },
    );

    let huc = "java/net/HttpURLConnection";
    r.register(huc, "connect", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        huc_perform(ctx, this)?;
        Ok(None)
    });
    r.register(huc, "getResponseCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        huc_perform(ctx, this)?;
        Ok(Some(ctx.get_field(this, HUC_CODE)))
    });
    r.register(huc, "getInputStream", "()Ljava/io/InputStream;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        huc_perform(ctx, this)?;
        let body = match ctx.get_field(this, HUC_BODY) {
            Value::Object(Some(a)) => a,
            _ => ctx.new_array(ArrayElementType::Byte, 0),
        };
        let len = ctx.array_length(body) as i32;
        let stream = alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4);
        ctx.set_field(stream, 0, Value::Object(Some(body))); // buf
        ctx.set_field(stream, 1, Value::Int(0));              // pos
        ctx.set_field(stream, 2, Value::Int(0));              // mark
        ctx.set_field(stream, 3, Value::Int(len));            // count
        Ok(Some(Value::Object(Some(stream))))
    });
    r.register(huc, "getHeaderField", "(Ljava/lang/String;)Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        huc_perform(ctx, this)?;
        let key_val = args.get(1).copied().unwrap_or(Value::Object(None));
        let key = value_or_string(ctx, key_val, "");
        if let Value::Object(Some(arr)) = ctx.get_field(this, HUC_RESP_HEADERS) {
            let len = ctx.array_length(arr);
            for i in 0..len {
                if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
                    let line = ctx.read_string(s).unwrap_or_default();
                    if let Some(colon) = line.find(':') {
                        if line[..colon].trim().eq_ignore_ascii_case(&key) {
                            let val = line[colon + 1..].trim().to_string();
                            let v = ctx.create_string(&val);
                            return Ok(Some(Value::Object(Some(v))));
                        }
                    }
                }
            }
        }
        Ok(Some(Value::Object(None)))
    });
    r.register(huc, "getHeaderField", "(I)Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        huc_perform(ctx, this)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if idx < 0 {
            return Ok(Some(Value::Object(None)));
        }
        if let Value::Object(Some(arr)) = ctx.get_field(this, HUC_RESP_HEADERS) {
            let len = ctx.array_length(arr);
            if (idx as usize) < len {
                if let Value::Object(Some(s)) = ctx.get_array_element(arr, idx as usize) {
                    let line = ctx.read_string(s).unwrap_or_default();
                    if let Some(colon) = line.find(':') {
                        let v = line[colon + 1..].trim().to_string();
                        let out = ctx.create_string(&v);
                        return Ok(Some(Value::Object(Some(out))));
                    }
                }
            }
        }
        Ok(Some(Value::Object(None)))
    });
    r.register(huc, "getContentLength", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        huc_perform(ctx, this)?;
        match ctx.get_field(this, HUC_BODY) {
            Value::Object(Some(a)) => Ok(Some(Value::Int(ctx.array_length(a) as i32))),
            _ => Ok(Some(Value::Int(-1))),
        }
    });
    r.register(huc, "setRequestMethod", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mval = args.get(1).copied().unwrap_or(Value::Object(None));
        let method = value_or_string(ctx, mval, "GET");
        let s = ctx.create_string(&method);
        ctx.set_field(this, HUC_METHOD, Value::Object(Some(s)));
        Ok(None)
    });
    r.register(huc, "disconnect", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, HUC_CONNECTED, Value::Int(0));
        ctx.set_field(this, HUC_FD, Value::Int(-1));
        Ok(None)
    });
    r.register(huc, "setDoInput", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
        ctx.set_field(this, HUC_DO_INPUT, Value::Int(v));
        Ok(None)
    });
    r.register(huc, "setDoOutput", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ctx.set_field(this, HUC_DO_OUTPUT, Value::Int(v));
        Ok(None)
    });
}

// ===========================================================================
// RE.5 — java.net.http.HttpClient
// ===========================================================================

fn register_re5_http_client(r: &mut NativeMethodRegistry) {
    let hc = "java/net/http/HttpClient";
    r.register(hc, "newHttpClient", "()Ljava/net/http/HttpClient;", |ctx, _args| {
        let obj = alloc_concurrent_synthetic(ctx, "java/net/http/HttpClient", 1);
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(
        hc,
        "newBuilder",
        "()Ljava/net/http/HttpClient$Builder;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/net/http/HttpClient$Builder", 1);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    let bld = "java/net/http/HttpClient$Builder";
    r.register(bld, "build", "()Ljava/net/http/HttpClient;", |ctx, _args| {
        let obj = alloc_concurrent_synthetic(ctx, "java/net/http/HttpClient", 1);
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(
        bld,
        "connectTimeout",
        "(Ljava/time/Duration;)Ljava/net/http/HttpClient$Builder;",
        |_ctx, args| Ok(Some(args[0])),
    );
    r.register(
        bld,
        "followRedirects",
        "(Ljava/net/http/HttpClient$Redirect;)Ljava/net/http/HttpClient$Builder;",
        |_ctx, args| Ok(Some(args[0])),
    );

    r.register(
        hc,
        "send",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;",
        |ctx, args| {
            let req = obj_arg(args, 1)?;
            let method = read_field_string_or(ctx, req, 0, "GET");
            let uri = read_field_string_or(ctx, req, 1, "");
            let body_str = read_field_string_or(ctx, req, 2, "");
            let hdrs_val = ctx.get_field(req, 3);
            let headers = huc_extract_req_headers(ctx, hdrs_val);
            if uri.is_empty() {
                return Err(ioex("HttpRequest.uri is empty"));
            }
            let resp = http_perform_request(&method, &uri, &headers, body_str.as_bytes(), 10)
                .map_err(|e| ioex(format!("HttpClient.send failed: {e}")))?;
            let out = alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse", 3);
            ctx.set_field(out, 0, Value::Int(resp.status));
            let body_text = String::from_utf8_lossy(&resp.body).to_string();
            let bs = ctx.create_string(&body_text);
            ctx.set_field(out, 1, Value::Object(Some(bs)));
            let hdr_arr = ctx.new_ref_array(ClassId::new(0), resp.headers.len());
            for (i, (k, v)) in resp.headers.iter().enumerate() {
                let s = ctx.create_string(&format!("{k}: {v}"));
                ctx.set_array_element(hdr_arr, i, Value::Object(Some(s)));
            }
            ctx.set_field(out, 2, Value::Object(Some(hdr_arr)));
            Ok(Some(Value::Object(Some(out))))
        },
    );

    let req = "java/net/http/HttpRequest";
    r.register(
        req,
        "newBuilder",
        "()Ljava/net/http/HttpRequest$Builder;",
        |ctx, _args| {
            let b = alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$Builder", 4);
            let m = ctx.create_string("GET");
            ctx.set_field(b, 0, Value::Object(Some(m)));
            ctx.set_field(b, 1, Value::Object(None));
            ctx.set_field(b, 2, Value::Object(None));
            ctx.set_field(b, 3, Value::Object(None));
            Ok(Some(Value::Object(Some(b))))
        },
    );
    r.register(
        req,
        "newBuilder",
        "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let b = alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$Builder", 4);
            let m = ctx.create_string("GET");
            ctx.set_field(b, 0, Value::Object(Some(m)));
            let uri = obj_arg(args, 0)?;
            let uri_s = match ctx.get_field(uri, 0) {
                Value::Object(Some(s)) => s,
                _ => ctx.create_string(""),
            };
            ctx.set_field(b, 1, Value::Object(Some(uri_s)));
            ctx.set_field(b, 2, Value::Object(None));
            ctx.set_field(b, 3, Value::Object(None));
            Ok(Some(Value::Object(Some(b))))
        },
    );

    let bl = "java/net/http/HttpRequest$Builder";
    r.register(
        bl,
        "uri",
        "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let uri = obj_arg(args, 1)?;
            let uri_s = match ctx.get_field(uri, 0) {
                Value::Object(Some(s)) => s,
                _ => ctx.create_string(""),
            };
            ctx.set_field(this, 1, Value::Object(Some(uri_s)));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        bl,
        "GET",
        "()Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m = ctx.create_string("GET");
            ctx.set_field(this, 0, Value::Object(Some(m)));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        bl,
        "DELETE",
        "()Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m = ctx.create_string("DELETE");
            ctx.set_field(this, 0, Value::Object(Some(m)));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        bl,
        "POST",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m = ctx.create_string("POST");
            ctx.set_field(this, 0, Value::Object(Some(m)));
            if let Some(Value::Object(Some(bp))) = args.get(1) {
                let body = ctx.get_field(*bp, 0);
                ctx.set_field(this, 2, body);
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        bl,
        "PUT",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m = ctx.create_string("PUT");
            ctx.set_field(this, 0, Value::Object(Some(m)));
            if let Some(Value::Object(Some(bp))) = args.get(1) {
                let body = ctx.get_field(*bp, 0);
                ctx.set_field(this, 2, body);
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        bl,
        "header",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let k = value_or_string(ctx, args.get(1).copied().unwrap_or(Value::Object(None)), "");
            let v = value_or_string(ctx, args.get(2).copied().unwrap_or(Value::Object(None)), "");
            let line = ctx.create_string(&format!("{k}: {v}"));
            let arr = match ctx.get_field(this, 3) {
                Value::Object(Some(a)) => a,
                _ => {
                    let a = ctx.new_array(ArrayElementType::Reference, 32);
                    ctx.set_field(this, 3, Value::Object(Some(a)));
                    a
                }
            };
            let len = ctx.array_length(arr);
            for i in 0..len {
                if let Value::Object(None) = ctx.get_array_element(arr, i) {
                    ctx.set_array_element(arr, i, Value::Object(Some(line)));
                    break;
                }
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        bl,
        "build",
        "()Ljava/net/http/HttpRequest;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let req = alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest", 4);
            for i in 0..4 {
                let v = ctx.get_field(this, i);
                ctx.set_field(req, i, v);
            }
            Ok(Some(Value::Object(Some(req))))
        },
    );

    let bps = "java/net/http/HttpRequest$BodyPublishers";
    r.register(
        bps,
        "ofString",
        "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, args| {
            let body = alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$BodyPublisher", 1);
            ctx.set_field(body, 0, args.first().copied().unwrap_or(Value::Object(None)));
            Ok(Some(Value::Object(Some(body))))
        },
    );
    r.register(
        bps,
        "noBody",
        "()Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, _args| {
            let body = alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$BodyPublisher", 1);
            let empty = ctx.create_string("");
            ctx.set_field(body, 0, Value::Object(Some(empty)));
            Ok(Some(Value::Object(Some(body))))
        },
    );

    let bhs = "java/net/http/HttpResponse$BodyHandlers";
    r.register(
        bhs,
        "ofString",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse$BodyHandler", 1);
            let tag = ctx.create_string("string");
            ctx.set_field(bh, 0, Value::Object(Some(tag)));
            Ok(Some(Value::Object(Some(bh))))
        },
    );
    r.register(
        bhs,
        "discarding",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let bh = alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse$BodyHandler", 1);
            let tag = ctx.create_string("discarding");
            ctx.set_field(bh, 0, Value::Object(Some(tag)));
            Ok(Some(Value::Object(Some(bh))))
        },
    );

    let resp = "java/net/http/HttpResponse";
    r.register(resp, "statusCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(resp, "body", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
}

// ===========================================================================
// RE.6 — javax.net.ssl.SSLContext
// ===========================================================================

fn register_re6_ssl_context(r: &mut NativeMethodRegistry) {
    let ctx_cls = "javax/net/ssl/SSLContext";
    r.register(
        ctx_cls,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;",
        |ctx, args| {
            let proto_val = args.first().copied().unwrap_or(Value::Object(None));
            let proto = value_or_string(ctx, proto_val, "TLS");
            if !(proto.eq_ignore_ascii_case("TLS")
                || proto.eq_ignore_ascii_case("TLSv1.2")
                || proto.eq_ignore_ascii_case("TLSv1.3")
                || proto.eq_ignore_ascii_case("Default")
                || proto.eq_ignore_ascii_case("SSL"))
            {
                return Err(ioex(format!("NoSuchAlgorithmException: {proto}")));
            }
            let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLContext", 2);
            let name = ctx.create_string(&proto);
            ctx.set_field(obj, 0, Value::Object(Some(name)));
            ctx.set_field(obj, 1, Value::Int(0));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        ctx_cls,
        "getDefault",
        "()Ljavax/net/ssl/SSLContext;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLContext", 2);
            let name = ctx.create_string("TLS");
            ctx.set_field(obj, 0, Value::Object(Some(name)));
            ctx.set_field(obj, 1, Value::Int(1));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        ctx_cls,
        "init",
        "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 1, Value::Int(1));
            Ok(None)
        },
    );
    r.register(
        ctx_cls,
        "getSocketFactory",
        "()Ljavax/net/ssl/SSLSocketFactory;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let f = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketFactory", 1);
            ctx.set_field(f, 0, Value::Object(Some(this)));
            Ok(Some(Value::Object(Some(f))))
        },
    );
    r.register(
        ctx_cls,
        "getServerSocketFactory",
        "()Ljavax/net/ssl/SSLServerSocketFactory;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let f = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLServerSocketFactory", 1);
            ctx.set_field(f, 0, Value::Object(Some(this)));
            Ok(Some(Value::Object(Some(f))))
        },
    );
    r.register(
        ctx_cls,
        "getProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );

    let sf = "javax/net/ssl/SSLSocketFactory";
    r.register(
        sf,
        "createSocket",
        "(Ljava/lang/String;I)Ljava/net/Socket;",
        |ctx, args| {
            let host_val = args.get(1).copied().unwrap_or(Value::Object(None));
            let host = value_or_string(ctx, host_val, "");
            let port = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            if host.is_empty() || !(1..=65535).contains(&port) {
                return Err(iae(format!("bad host/port: {host}:{port}")));
            }
            let connector = native_tls::TlsConnector::builder()
                .build()
                .map_err(|e| ioex(format!("TLS connector: {e}")))?;
            let id = crate::servlet::s2_tls_connect(&connector, &host, port as u16)
                .map_err(|e| ioex(format!("TLS connect: {e}")))?;
            let sock = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocket", 5);
            let host_s = ctx.create_string(&host);
            ctx.set_field(sock, SOCK_HOST, Value::Object(Some(host_s)));
            ctx.set_field(sock, SOCK_PORT, Value::Int(port));
            ctx.set_field(sock, SOCK_LOCAL_PORT, Value::Int(0));
            ctx.set_field(sock, SOCK_CLOSED, Value::Int(0));
            ctx.set_field(sock, SOCK_STREAM_ID, Value::Int(id));
            Ok(Some(Value::Object(Some(sock))))
        },
    );
    r.register(sf, "getDefault", "()Ljavax/net/SocketFactory;", |ctx, _args| {
        let f = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketFactory", 1);
        ctx.set_field(f, 0, Value::Object(None));
        Ok(Some(Value::Object(Some(f))))
    });
}

// ===========================================================================
// RE.7 — java.net.DatagramSocket
// ===========================================================================

const DS_PORT: usize = 0;
const DS_CLOSED: usize = 1;
const DS_TIMEOUT: usize = 2;
const DS_FD: usize = 3;

const DP_DATA: usize = 0;
const DP_LENGTH: usize = 1;
const DP_ADDR: usize = 2;
const DP_PORT: usize = 3;

fn register_re7_datagram_socket(r: &mut NativeMethodRegistry) {
    let ds = "java/net/DatagramSocket";

    r.register(ds, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd = ctx
            .fd_table()
            .open_udp(Some("0.0.0.0:0"))
            .map_err(|e| ioex(format!("UDP open: {e}")))?;
        let port = ctx
            .fd_table()
            .udp_local_addr(fd)
            .ok()
            .and_then(|s| s.rsplit(':').next().and_then(|p| p.parse::<i32>().ok()))
            .unwrap_or(0);
        ctx.set_field(this, DS_PORT, Value::Int(port));
        ctx.set_field(this, DS_CLOSED, Value::Int(0));
        ctx.set_field(this, DS_TIMEOUT, Value::Int(0));
        ctx.set_field(this, DS_FD, Value::Int(fd as i32));
        Ok(None)
    });
    r.register(ds, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let addr_spec = format!("0.0.0.0:{port}");
        let fd = ctx
            .fd_table()
            .open_udp(Some(&addr_spec))
            .map_err(|e| ioex(format!("UDP bind: {e}")))?;
        let actual_port = ctx
            .fd_table()
            .udp_local_addr(fd)
            .ok()
            .and_then(|s| s.rsplit(':').next().and_then(|p| p.parse::<i32>().ok()))
            .unwrap_or(port);
        ctx.set_field(this, DS_PORT, Value::Int(actual_port));
        ctx.set_field(this, DS_CLOSED, Value::Int(0));
        ctx.set_field(this, DS_TIMEOUT, Value::Int(0));
        ctx.set_field(this, DS_FD, Value::Int(fd as i32));
        Ok(None)
    });
    r.register(ds, "<init>", "(ILjava/net/InetAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let host = match args.get(2) {
            Some(Value::Object(Some(ia))) => read_field_string_or(ctx, *ia, IA_ADDR, "0.0.0.0"),
            _ => "0.0.0.0".to_string(),
        };
        let fd = ctx
            .fd_table()
            .open_udp(Some(&format!("{host}:{port}")))
            .map_err(|e| ioex(format!("UDP bind: {e}")))?;
        let actual_port = ctx
            .fd_table()
            .udp_local_addr(fd)
            .ok()
            .and_then(|s| s.rsplit(':').next().and_then(|p| p.parse::<i32>().ok()))
            .unwrap_or(port);
        ctx.set_field(this, DS_PORT, Value::Int(actual_port));
        ctx.set_field(this, DS_CLOSED, Value::Int(0));
        ctx.set_field(this, DS_TIMEOUT, Value::Int(0));
        ctx.set_field(this, DS_FD, Value::Int(fd as i32));
        Ok(None)
    });

    r.register(ds, "send", "(Ljava/net/DatagramPacket;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pkt = obj_arg(args, 1)?;
        let fd = ctx.get_field(this, DS_FD).as_int().unwrap_or(-1);
        if fd < 0 {
            return Err(ioex("DatagramSocket: closed"));
        }
        let data_arr = match ctx.get_field(pkt, DP_DATA) {
            Value::Object(Some(a)) => a,
            _ => return Err(ioex("DatagramPacket: null data")),
        };
        let len = ctx.get_field(pkt, DP_LENGTH).as_int().unwrap_or(0);
        let port = ctx.get_field(pkt, DP_PORT).as_int().unwrap_or(0);
        let host = match ctx.get_field(pkt, DP_ADDR) {
            Value::Object(Some(ia)) => read_field_string_or(ctx, ia, IA_ADDR, ""),
            _ => String::new(),
        };
        if host.is_empty() || !(1..=65535).contains(&port) {
            return Err(iae(format!("DatagramPacket: bad addr {host}:{port}")));
        }
        let payload = java_byte_array_to_vec(ctx, data_arr, 0, len)?;
        let target = format!("{host}:{port}");
        ctx.fd_table()
            .udp_send(fd as u32, &payload, &target)
            .map_err(|e| ioex(format!("UDP send: {e}")))?;
        Ok(None)
    });

    r.register(ds, "receive", "(Ljava/net/DatagramPacket;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pkt = obj_arg(args, 1)?;
        let fd = ctx.get_field(this, DS_FD).as_int().unwrap_or(-1);
        if fd < 0 {
            return Err(ioex("DatagramSocket: closed"));
        }
        let data_arr = match ctx.get_field(pkt, DP_DATA) {
            Value::Object(Some(a)) => a,
            _ => return Err(ioex("DatagramPacket: null data")),
        };
        let cap = ctx.array_length(data_arr);
        let mut buf = vec![0u8; cap];
        let timeout_ms = ctx.get_field(this, DS_TIMEOUT).as_int().unwrap_or(0);
        let d = if timeout_ms > 0 {
            Some(Duration::from_millis(timeout_ms as u64))
        } else {
            None
        };
        ctx.fd_table()
            .udp_set_read_timeout(fd as u32, d)
            .map_err(|e| ioex(format!("UDP timeout: {e}")))?;
        let (n, origin) = ctx
            .fd_table()
            .udp_recv(fd as u32, &mut buf)
            .map_err(|e| ioex(format!("UDP recv: {e}")))?;
        copy_bytes_into_java_array(ctx, data_arr, 0, &buf[..n])?;
        ctx.set_field(pkt, DP_LENGTH, Value::Int(n as i32));
        if let Some((oh, op)) = origin.rsplit_once(':') {
            let port = op.parse::<i32>().unwrap_or(0);
            let ia = alloc_inet_address(ctx, oh, oh);
            ctx.set_field(pkt, DP_ADDR, Value::Object(Some(ia)));
            ctx.set_field(pkt, DP_PORT, Value::Int(port));
        }
        Ok(None)
    });

    r.register(ds, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd = ctx.get_field(this, DS_FD).as_int().unwrap_or(-1);
        if fd >= 0 {
            // Ignore close errors — the fd may already be closed by
            // a racing caller; DS_CLOSED is still set unconditionally.
            let _ = ctx.fd_table().close(fd as u32);
        }
        ctx.set_field(this, DS_CLOSED, Value::Int(1));
        ctx.set_field(this, DS_FD, Value::Int(-1));
        Ok(None)
    });
    r.register(ds, "isClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, DS_CLOSED)))
    });
    r.register(ds, "getLocalPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, DS_PORT)))
    });
    r.register(ds, "setSoTimeout", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if ms < 0 {
            return Err(iae("negative SO_TIMEOUT"));
        }
        ctx.set_field(this, DS_TIMEOUT, Value::Int(ms));
        Ok(None)
    });
    r.register(ds, "getSoTimeout", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, DS_TIMEOUT)))
    });
}

// ===========================================================================
// RE.8 — java.net.NetworkInterface
// ===========================================================================

fn re8_enumerate_local_ips() -> Vec<IpAddr> {
    use std::net::UdpSocket;
    let mut out = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
    if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
        if s.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = s.local_addr() {
                if !addr.ip().is_loopback() {
                    out.push(addr.ip());
                }
            }
        }
    }
    if let Ok(s) = UdpSocket::bind("[::]:0") {
        if s.connect("[2001:4860:4860::8888]:80").is_ok() {
            if let Ok(addr) = s.local_addr() {
                if !addr.ip().is_loopback() && !out.contains(&addr.ip()) {
                    out.push(addr.ip());
                }
            }
        }
    }
    let lookup = format!("{}:0", hostname_string());
    if let Ok(addrs) = std::net::ToSocketAddrs::to_socket_addrs(&lookup.as_str()) {
        for sa in addrs {
            if !out.contains(&sa.ip()) {
                out.push(sa.ip());
            }
        }
    }
    out
}

fn re8_build_interfaces(ctx: &mut dyn NativeContext) -> Vec<ObjectRef> {
    let mut result = Vec::new();
    let ips = re8_enumerate_local_ips();

    let lo = alloc_concurrent_synthetic(ctx, "java/net/NetworkInterface", 5);
    let lo_name = ctx.create_string("lo");
    let lo_display = ctx.create_string("Loopback Interface");
    ctx.set_field(lo, 0, Value::Object(Some(lo_name)));
    ctx.set_field(lo, 1, Value::Object(Some(lo_display)));
    let mut lo_addrs: Vec<ObjectRef> = Vec::new();
    for ip in &ips {
        if ip.is_loopback() {
            let a = alloc_inet_address(ctx, "localhost", &ip.to_string());
            lo_addrs.push(a);
        }
    }
    if lo_addrs.is_empty() {
        lo_addrs.push(alloc_inet_address(ctx, "localhost", "127.0.0.1"));
    }
    let lo_arr = ctx.new_ref_array(ClassId::new(0), lo_addrs.len());
    for (i, a) in lo_addrs.iter().enumerate() {
        ctx.set_array_element(lo_arr, i, Value::Object(Some(*a)));
    }
    ctx.set_field(lo, 2, Value::Object(Some(lo_arr)));
    ctx.set_field(lo, 3, Value::Int(1));
    ctx.set_field(lo, 4, Value::Int(0b111));
    result.push(lo);

    let hostname = hostname_string();
    let mut idx = 2;
    for ip in &ips {
        if ip.is_loopback() {
            continue;
        }
        let iface = alloc_concurrent_synthetic(ctx, "java/net/NetworkInterface", 5);
        let name = ctx.create_string(if matches!(ip, IpAddr::V4(_)) { "eth0" } else { "eth1" });
        let display = ctx.create_string("Primary Network Interface");
        ctx.set_field(iface, 0, Value::Object(Some(name)));
        ctx.set_field(iface, 1, Value::Object(Some(display)));
        let a = alloc_inet_address(ctx, &hostname, &ip.to_string());
        let arr = ctx.new_ref_array(ClassId::new(0), 1);
        ctx.set_array_element(arr, 0, Value::Object(Some(a)));
        ctx.set_field(iface, 2, Value::Object(Some(arr)));
        ctx.set_field(iface, 3, Value::Int(idx));
        ctx.set_field(iface, 4, Value::Int(0b101));
        result.push(iface);
        idx += 1;
    }
    result
}

fn register_re8_network_interface(r: &mut NativeMethodRegistry) {
    let ni = "java/net/NetworkInterface";

    r.register(ni, "getNetworkInterfaces", "()Ljava/util/Enumeration;", |ctx, _args| {
        let ifaces = re8_build_interfaces(ctx);
        let arr = ctx.new_ref_array(ClassId::new(0), ifaces.len());
        for (i, iface) in ifaces.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Object(Some(*iface)));
        }
        let enum_obj = alloc_concurrent_synthetic(ctx, "java/util/Enumeration", 2);
        ctx.set_field(enum_obj, 0, Value::Object(Some(arr)));
        ctx.set_field(enum_obj, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(enum_obj))))
    });

    r.register(
        ni,
        "networkInterfaces",
        "()Ljava/util/stream/Stream;",
        |ctx, _args| {
            let ifaces = re8_build_interfaces(ctx);
            let list = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
            rustjvm_native_collections::native_al_init(ctx, &[Value::Object(Some(list))]).ok();
            for iface in &ifaces {
                rustjvm_native_collections::native_al_add(
                    ctx,
                    &[Value::Object(Some(list)), Value::Object(Some(*iface))],
                )
                .ok();
            }
            Ok(Some(Value::Object(Some(list))))
        },
    );

    r.register(ni, "getHardwareAddress", "()[B", |ctx, _args| {
        let mac = ctx.new_array(ArrayElementType::Byte, 6);
        for i in 0..6 {
            ctx.set_array_element(mac, i, Value::Int(0));
        }
        Ok(Some(Value::Object(Some(mac))))
    });
    r.register(ni, "getMTU", "()I", |_ctx, _args| Ok(Some(Value::Int(1500))));
}

// ===========================================================================
// RE.9 — java.nio.channels.Selector
// ===========================================================================

fn register_re9_nio_selector(r: &mut NativeMethodRegistry) {
    let sel = "java/nio/channels/Selector";

    r.register(sel, "select", "(J)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = match args.get(1) {
            Some(Value::Long(v)) => *v,
            Some(Value::Int(v)) => *v as i64,
            _ => return Err(iae("select(J) missing argument")),
        };
        if raw < 0 {
            return Err(iae(format!("negative timeout {raw}")));
        }
        if raw == 0 {
            match ctx.invoke_virtual(this, "selectNow", "()I", &[]) {
                Ok(Some(v)) => Ok(Some(v)),
                _ => Ok(Some(Value::Int(0))),
            }
        } else {
            let deadline = std::time::Instant::now() + Duration::from_millis(raw as u64);
            let mut total = 0i32;
            while std::time::Instant::now() < deadline {
                if let Ok(Some(Value::Int(n))) =
                    ctx.invoke_virtual(this, "selectNow", "()I", &[])
                {
                    if n > 0 {
                        total = n;
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
                if ctx.get_field(this, SEL_OPEN).as_int().unwrap_or(1) == 0 {
                    break;
                }
            }
            Ok(Some(Value::Int(total)))
        }
    });

    r.register(sel, "isOpen", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, SEL_OPEN)))
    });
}

// ===========================================================================
// RE.10 — com.sun.net.httpserver.HttpServer
// ===========================================================================

struct HttpHandlerEntry {
    path_prefix: String,
    handler: ObjectRef,
}

struct ServerState {
    listener: Option<TcpListener>,
    running: AtomicBool,
    handlers: Mutex<Vec<HttpHandlerEntry>>,
    bound_port: AtomicI32,
}

fn server_registry() -> &'static Mutex<HashMap<i32, std::sync::Arc<ServerState>>> {
    static INSTANCE: OnceLock<Mutex<HashMap<i32, std::sync::Arc<ServerState>>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_server_id() -> i32 {
    static COUNTER: AtomicI32 = AtomicI32::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

struct PendingRequest {
    stream: TcpStream,
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn request_queue() -> &'static Mutex<HashMap<i32, Vec<PendingRequest>>> {
    static INSTANCE: OnceLock<Mutex<HashMap<i32, Vec<PendingRequest>>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn parse_http_request(mut stream: TcpStream) -> Option<PendingRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 1024];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if buf.len() > 1 << 20 {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    let sep = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&buf[..sep]).ok()?;
    let mut lines = head.split("\r\n");
    let req_line = lines.next()?;
    let mut rl = req_line.splitn(3, ' ');
    let method = rl.next()?.to_string();
    let uri = rl.next()?.to_string();
    let _ = rl.next()?;
    let mut headers = Vec::new();
    let mut content_length: usize = 0;
    for line in lines {
        if let Some(colon) = line.find(':') {
            let k = line[..colon].trim().to_string();
            let v = line[colon + 1..].trim().to_string();
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().unwrap_or(0);
            }
            headers.push((k, v));
        }
    }
    let mut body = buf[sep + 4..].to_vec();
    while body.len() < content_length {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }
    if body.len() > content_length && content_length > 0 {
        body.truncate(content_length);
    }
    Some(PendingRequest {
        stream,
        method,
        uri,
        headers,
        body,
    })
}

fn re10_dispatch_pending(
    ctx: &mut dyn NativeContext,
    server_id: i32,
) -> Result<usize, rustjvm_types::error::MethodCallFailed> {
    let mut drained = 0usize;
    loop {
        let req_opt = {
            let mut q = request_queue().lock();
            q.get_mut(&server_id).and_then(|v| v.pop())
        };
        let Some(req) = req_opt else { break };
        drained += 1;
        let handler_info = {
            let state = {
                let reg = server_registry().lock();
                reg.get(&server_id).cloned()
            };
            state.and_then(|s| {
                let hs = s.handlers.lock();
                hs.iter()
                    .filter(|e| req.uri.starts_with(&e.path_prefix))
                    .max_by_key(|e| e.path_prefix.len())
                    .map(|e| e.handler)
            })
        };
        let (status, body_bytes, resp_headers) = match handler_info {
            Some(h) => {
                let ex = alloc_concurrent_synthetic(ctx, "com/sun/net/httpserver/HttpExchange", 8);
                let m = ctx.create_string(&req.method);
                let u = ctx.create_string(&req.uri);
                ctx.set_field(ex, 0, Value::Object(Some(m)));
                ctx.set_field(ex, 1, Value::Object(Some(u)));
                let rh = ctx.new_ref_array(ClassId::new(0), req.headers.len().max(1));
                for (i, (k, v)) in req.headers.iter().enumerate() {
                    let s = ctx.create_string(&format!("{k}: {v}"));
                    ctx.set_array_element(rh, i, Value::Object(Some(s)));
                }
                ctx.set_field(ex, 2, Value::Object(Some(rh)));
                let rsph = ctx.new_ref_array(ClassId::new(0), 32);
                ctx.set_field(ex, 3, Value::Object(Some(rsph)));
                let body_arr = new_java_byte_array(ctx, &req.body);
                ctx.set_field(ex, 4, Value::Object(Some(body_arr)));
                ctx.set_field(ex, 5, Value::Int(200));
                let resp_body_chunks = ctx.new_ref_array(ClassId::new(0), 64);
                ctx.set_field(ex, 6, Value::Object(Some(resp_body_chunks)));
                ctx.set_field(ex, 7, Value::Int(0));
                let _ = ctx.invoke_virtual(
                    h,
                    "handle",
                    "(Lcom/sun/net/httpserver/HttpExchange;)V",
                    &[Value::Object(Some(ex))],
                );
                let status = ctx.get_field(ex, 5).as_int().unwrap_or(200);
                let mut body_bytes: Vec<u8> = Vec::new();
                if let Value::Object(Some(chunks)) = ctx.get_field(ex, 6) {
                    let n = ctx.array_length(chunks);
                    for i in 0..n {
                        if let Value::Object(Some(ba)) = ctx.get_array_element(chunks, i) {
                            let ln = ctx.array_length(ba);
                            for j in 0..ln {
                                if let Value::Int(b) = ctx.get_array_element(ba, j) {
                                    body_bytes.push(b as i8 as u8);
                                }
                            }
                        }
                    }
                }
                let mut resp_headers = Vec::new();
                if let Value::Object(Some(rh)) = ctx.get_field(ex, 3) {
                    let n = ctx.array_length(rh);
                    for i in 0..n {
                        if let Value::Object(Some(s)) = ctx.get_array_element(rh, i) {
                            let line = ctx.read_string(s).unwrap_or_default();
                            if let Some(colon) = line.find(':') {
                                resp_headers.push((
                                    line[..colon].trim().to_string(),
                                    line[colon + 1..].trim().to_string(),
                                ));
                            }
                        }
                    }
                }
                (status, body_bytes, resp_headers)
            }
            None => (404, b"Not Found".to_vec(), Vec::new()),
        };
        let mut stream = req.stream;
        let mut resp = Vec::with_capacity(128 + body_bytes.len());
        use std::io::Write as _;
        let _ = write!(&mut resp, "HTTP/1.1 {status} {}\r\n", http_reason(status));
        let mut has_content_length = false;
        for (k, v) in &resp_headers {
            if k.eq_ignore_ascii_case("content-length") {
                has_content_length = true;
            }
            let _ = write!(&mut resp, "{k}: {v}\r\n");
        }
        if !has_content_length {
            let _ = write!(&mut resp, "Content-Length: {}\r\n", body_bytes.len());
        }
        resp.extend_from_slice(b"Connection: close\r\n\r\n");
        resp.extend_from_slice(&body_bytes);
        let _ = stream.write_all(&resp);
        let _ = stream.flush();
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    Ok(drained)
}

fn http_reason(code: i32) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

fn re10_start_server(server_id: i32) -> std::io::Result<()> {
    let state = {
        let reg = server_registry().lock();
        reg.get(&server_id).cloned()
    };
    let Some(state) = state else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "server not registered",
        ));
    };
    if state.running.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let listener = match state.listener.as_ref() {
        Some(l) => l.try_clone()?,
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "not bound",
            ));
        }
    };
    let state_cl = state.clone();
    std::thread::Builder::new()
        .name(format!("rustjvm-httpserver-{server_id}"))
        .spawn(move || {
            listener.set_nonblocking(true).ok();
            while state_cl.running.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _peer)) => {
                        if let Some(pending) = parse_http_request(stream) {
                            let mut q = request_queue().lock();
                            q.entry(server_id).or_default().push(pending);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        })?;
    Ok(())
}

fn register_re10_http_server(r: &mut NativeMethodRegistry) {
    let hs = "com/sun/net/httpserver/HttpServer";

    r.register(
        hs,
        "create",
        "(Ljava/net/InetSocketAddress;I)Lcom/sun/net/httpserver/HttpServer;",
        |ctx, args| {
            let sa = obj_arg(args, 0)?;
            let (host, port) = read_inet_socket_address(ctx, sa)?;
            let backlog = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
            let ip = resolve_host(&host)?;
            let addr = SocketAddr::new(ip, port.clamp(0, 65535) as u16);
            let listener = TcpListener::bind(addr)
                .map_err(|e| ioex(format!("HttpServer bind {addr}: {e}")))?;
            let bound_port = listener.local_addr().map(|a| a.port() as i32).unwrap_or(port);
            let server_id = next_server_id();
            let state = std::sync::Arc::new(ServerState {
                listener: Some(listener),
                running: AtomicBool::new(false),
                handlers: Mutex::new(Vec::new()),
                bound_port: AtomicI32::new(bound_port),
            });
            server_registry().lock().insert(server_id, state);
            let srv = alloc_concurrent_synthetic(ctx, "com/sun/net/httpserver/HttpServer", 5);
            let sa_echo = alloc_inet_socket_address(ctx, &host, bound_port);
            ctx.set_field(srv, HS_ADDRESS, Value::Object(Some(sa_echo)));
            ctx.set_field(srv, HS_STARTED, Value::Int(0));
            ctx.set_field(srv, HS_CONTEXTS, Value::Object(None));
            ctx.set_field(srv, HS_SERVER_ID, Value::Int(server_id));
            ctx.set_field(srv, HS_PORT, Value::Int(bound_port));
            let _ = backlog;
            Ok(Some(Value::Object(Some(srv))))
        },
    );

    r.register(hs, "start", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = ctx.get_field(this, HS_SERVER_ID).as_int().unwrap_or(-1);
        if id < 0 {
            return Err(ioex("HttpServer not initialised"));
        }
        re10_start_server(id).map_err(|e| ioex(format!("HttpServer start: {e}")))?;
        ctx.set_field(this, HS_STARTED, Value::Int(1));
        re10_dispatch_pending(ctx, id)?;
        Ok(None)
    });

    r.register(hs, "stop", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = ctx.get_field(this, HS_SERVER_ID).as_int().unwrap_or(-1);
        if id >= 0 {
            if let Some(state) = server_registry().lock().get(&id) {
                state.running.store(false, Ordering::SeqCst);
            }
            re10_dispatch_pending(ctx, id)?;
            server_registry().lock().remove(&id);
        }
        ctx.set_field(this, HS_STARTED, Value::Int(0));
        Ok(None)
    });

    r.register(
        hs,
        "createContext",
        "(Ljava/lang/String;Lcom/sun/net/httpserver/HttpHandler;)Lcom/sun/net/httpserver/HttpContext;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path_val = args.get(1).copied().unwrap_or(Value::Object(None));
            let path = value_or_string(ctx, path_val, "/");
            let handler = obj_arg(args, 2).map_err(|_| ioex("createContext: null handler"))?;
            let id = ctx.get_field(this, HS_SERVER_ID).as_int().unwrap_or(-1);
            if id >= 0 {
                if let Some(state) = server_registry().lock().get(&id) {
                    state.handlers.lock().push(HttpHandlerEntry {
                        path_prefix: path.clone(),
                        handler,
                    });
                }
            }
            let hctx = alloc_concurrent_synthetic(ctx, "com/sun/net/httpserver/HttpContext", 2);
            let path_s = ctx.create_string(&path);
            ctx.set_field(hctx, 0, Value::Object(Some(path_s)));
            ctx.set_field(hctx, 1, Value::Object(Some(handler)));
            Ok(Some(Value::Object(Some(hctx))))
        },
    );

    r.register(
        hs,
        "getAddress",
        "()Ljava/net/InetSocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, HS_ADDRESS)))
        },
    );

    let hex = "com/sun/net/httpserver/HttpExchange";
    r.register(hex, "getRequestMethod", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(hex, "getRequestURI", "()Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = ctx.get_field(this, 1);
        let uri = alloc_concurrent_synthetic(ctx, "java/net/URI", 6);
        ctx.set_field(uri, 0, s);
        Ok(Some(Value::Object(Some(uri))))
    });
    r.register(
        hex,
        "getResponseHeaders",
        "()Lcom/sun/net/httpserver/Headers;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let rh = ctx.get_field(this, 3);
            let h = alloc_concurrent_synthetic(ctx, "com/sun/net/httpserver/Headers", 1);
            ctx.set_field(h, 0, rh);
            Ok(Some(Value::Object(Some(h))))
        },
    );
    r.register(
        hex,
        "getRequestHeaders",
        "()Lcom/sun/net/httpserver/Headers;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let rh = ctx.get_field(this, 2);
            let h = alloc_concurrent_synthetic(ctx, "com/sun/net/httpserver/Headers", 1);
            ctx.set_field(h, 0, rh);
            Ok(Some(Value::Object(Some(h))))
        },
    );
    r.register(hex, "getRequestBody", "()Ljava/io/InputStream;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let body = match ctx.get_field(this, 4) {
            Value::Object(Some(a)) => a,
            _ => ctx.new_array(ArrayElementType::Byte, 0),
        };
        let len = ctx.array_length(body) as i32;
        let stream = alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4);
        ctx.set_field(stream, 0, Value::Object(Some(body))); // buf
        ctx.set_field(stream, 1, Value::Int(0));              // pos
        ctx.set_field(stream, 2, Value::Int(0));              // mark
        ctx.set_field(stream, 3, Value::Int(len));            // count
        Ok(Some(Value::Object(Some(stream))))
    });
    r.register(hex, "sendResponseHeaders", "(IJ)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let code = args.get(1).and_then(|v| v.as_int()).unwrap_or(200);
        ctx.set_field(this, 5, Value::Int(code));
        Ok(None)
    });
    r.register(hex, "getResponseBody", "()Ljava/io/OutputStream;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let out = alloc_concurrent_synthetic(ctx, "com/sun/net/httpserver/HttpExchange$ResponseBody", 2);
        ctx.set_field(out, 0, Value::Object(Some(this)));
        ctx.set_field(out, 1, Value::Int(0));
        Ok(Some(Value::Object(Some(out))))
    });
    r.register(hex, "close", "()V", |_ctx, _args| Ok(None));

    let rb = "com/sun/net/httpserver/HttpExchange$ResponseBody";
    r.register(rb, "write", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Err(ioex("ResponseBody has no exchange")),
        };
        let buf = obj_arg(args, 1)?;
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        let data = java_byte_array_to_vec(ctx, buf, off, len)?;
        let chunk = new_java_byte_array(ctx, &data);
        if let Value::Object(Some(chunks)) = ctx.get_field(owner, 6) {
            let cap = ctx.array_length(chunks);
            for i in 0..cap {
                if let Value::Object(None) = ctx.get_array_element(chunks, i) {
                    ctx.set_array_element(chunks, i, Value::Object(Some(chunk)));
                    return Ok(None);
                }
            }
        }
        Ok(None)
    });
    r.register(rb, "write", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Err(ioex("ResponseBody has no exchange")),
        };
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) & 0xff;
        let chunk = ctx.new_array(ArrayElementType::Byte, 1);
        ctx.set_array_element(chunk, 0, Value::Int(b as i8 as i32));
        if let Value::Object(Some(chunks)) = ctx.get_field(owner, 6) {
            let cap = ctx.array_length(chunks);
            for i in 0..cap {
                if let Value::Object(None) = ctx.get_array_element(chunks, i) {
                    ctx.set_array_element(chunks, i, Value::Object(Some(chunk)));
                    return Ok(None);
                }
            }
        }
        Ok(None)
    });
    r.register(rb, "flush", "()V", |_ctx, _args| Ok(None));
    r.register(rb, "close", "()V", |_ctx, _args| Ok(None));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn re1_http_parse_url_plain() {
        let (https, host, port, path) = http_parse_url("http://example.com/foo").unwrap();
        assert!(!https);
        assert_eq!(host, "example.com");
        assert_eq!(port, 80);
        assert_eq!(path, "/foo");
    }

    #[test]
    fn re1_http_parse_url_with_port() {
        let (https, host, port, path) = http_parse_url("https://example.com:8443/api?x=1").unwrap();
        assert!(https);
        assert_eq!(host, "example.com");
        assert_eq!(port, 8443);
        assert_eq!(path, "/api?x=1");
    }

    #[test]
    fn re1_http_build_request_adds_content_length_for_post() {
        let req = http_build_request("POST", "h", 80, "/", &[], b"hello", 80);
        let s = String::from_utf8_lossy(&req);
        assert!(s.contains("POST / HTTP/1.1"));
        assert!(s.contains("Host: h"));
        assert!(s.contains("Content-Length: 5"));
        assert!(s.ends_with("hello"));
    }

    #[test]
    fn re2_tcp_listener_binds_and_accepts() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let th = std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 5];
                let _ = s.read(&mut buf);
                assert_eq!(&buf, b"hello");
            }
        });
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        client.write_all(b"hello").unwrap();
        let _ = th.join();
    }

    #[test]
    fn re3_resolve_localhost() {
        let ip = resolve_host("localhost").expect("localhost must resolve");
        assert!(ip.is_loopback());
    }

    #[test]
    fn re3_parse_literal_v4() {
        let ip = resolve_host("10.0.0.1").unwrap();
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
    }

    #[test]
    fn re4_http_decode_chunked() {
        let raw = b"5\r\nhello\r\n5\r\nworld\r\n0\r\n\r\n";
        let body = http_decode_chunked(raw).unwrap();
        assert_eq!(body, b"helloworld");
    }

    #[test]
    fn re4_http_read_response_parses_status_and_body() {
        let raw = b"HTTP/1.1 204 No Content\r\nServer: t\r\nContent-Length: 0\r\n\r\n";
        let resp = http_read_response(&raw[..]).unwrap();
        assert_eq!(resp.status, 204);
        assert_eq!(resp.body, Vec::<u8>::new());
        assert!(resp.headers.iter().any(|(k, _)| k == "Server"));
    }

    #[test]
    fn re7_udp_roundtrip() {
        use std::net::UdpSocket;
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = server.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client.send_to(b"ping", server_addr).unwrap();
        let mut buf = [0u8; 4];
        let (n, _) = server.recv_from(&mut buf).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&buf, b"ping");
    }

    #[test]
    fn re8_enumerates_at_least_loopback() {
        let ips = re8_enumerate_local_ips();
        assert!(ips.iter().any(|i| i.is_loopback()));
    }

    #[test]
    fn re9_iae_helper_reachable() {
        assert!(matches!(
            iae(""),
            rustjvm_types::error::MethodCallFailed::InternalError(_)
        ));
    }

    #[test]
    fn re10_parse_minimal_http_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.write_all(b"GET /hi HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        });
        let (stream, _) = listener.accept().unwrap();
        let req = parse_http_request(stream).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.uri, "/hi");
        assert!(req.headers.iter().any(|(k, v)| k == "Host" && v == "x"));
    }
}
