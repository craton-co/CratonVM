// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP5.6 — legacy `sun.net.www.protocol.http(s).HttpURLConnection` natives.
//!
//! The JDK's HTTP/1.1 stack predates `java.net.http.HttpClient` and exposes
//! a more stateful, blocking API where each connection object holds its own
//! socket, request buffer, and parsed response. Real apps still use it (e.g.
//! `URL.openConnection().getInputStream()` in WildFly's bootstrap), so we
//! wire it through to the same TCP+TLS plumbing as `http_client.rs` but with
//! the older semantics:
//!
//!   * a per-instance native registry keyed by `conn_id` (stored in field 0
//!     of the Java HttpURLConnection synthetic object);
//!   * `connect()` opens the socket, drives TLS for the `https` variant,
//!     sends the request line, and parses the response head;
//!   * `getInputStream()` returns a `ByteArrayInputStream` over the response
//!     body — this matches what the JDK actually does once the body is fully
//!     buffered (it's not a streaming abstraction in our world);
//!   * `getOutputStream()` returns a synthetic `ByteArrayOutputStream` that
//!     `connect()` will pick up the body from on the first read.
//!
//! This module owns ONLY the connection-level natives. URL stream-handler
//! dispatch (`URL.openConnection`) lives in `net_phase_e::register_re4_url_http`
//! and is unchanged.
//!
//! No stubs: every code path performs real I/O against the network or rejects
//! the call with a typed `IOException`. We never fabricate canned 200s.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

thread_local! {
    /// Reason phrase from the most recently parsed HTTP response status line
    /// on this thread. httparse gives us the exact bytes the server sent
    /// (`resp.reason`), but `read_response`/`read_response_with_prefix`'s
    /// return type is the canonical `(status, headers, body)` triple used
    /// pervasively throughout this module's pooling/retry/redirect call
    /// chains -- threading a 4th field through every one of those signatures
    /// is a large, risky refactor for what only `huc_get_response_message`
    /// needs. A thread-local side channel (mirroring this file's existing
    /// identity-keyed side-table pattern, e.g. `real_results()`) is far
    /// smaller in scope: each blocking HTTP call is performed serially on
    /// the calling Java thread, so "most recently parsed" always means "the
    /// response this thread just read" -- correctly reflecting the final
    /// (post-redirect-follow) response by the time `huc_real_perform` reads
    /// it back out into `RealResult::reason`.
    static LAST_REASON_PHRASE: RefCell<String> = RefCell::new(String::new());

    /// Whether the response body most recently read on this thread was
    /// TRUNCATED — the peer closed the connection part-way through a chunked
    /// body/chunk header, or short of the advertised `Content-Length`.
    ///
    /// This used to be reported as a hard `Err` from `read_chunked`, which
    /// **discarded every byte already received**. Real JDK semantics are the
    /// opposite: `getResponseCode()` succeeds (the head arrived intact),
    /// `getInputStream()` hands out the bytes that DID arrive, and only the
    /// read that runs past the truncation point throws `IOException`
    /// ("Premature EOF"). Tomcat's `TestGenerator.testBug56581` depends on
    /// exactly that: `bug56581.jsp` writes 1000 lines, commits the response,
    /// then throws, so `ErrorReportValve` aborts the connection mid-body — the
    /// test asserts on the 1000 lines the client did receive AND on the
    /// resulting `IOException`. Discarding the body made `ByteChunk.toString()`
    /// return `null` and the assertion NPE.
    ///
    /// Same rationale as `LAST_REASON_PHRASE` above for why this is a
    /// thread-local side channel rather than a 4th tuple element: every
    /// blocking HTTP call is performed serially on the calling Java thread, so
    /// "most recently read on this thread" is exactly "the response this
    /// thread just read".
    static LAST_RESPONSE_TRUNCATED: Cell<bool> = const { Cell::new(false) };
}

/// Mark the response currently being read on this thread as truncated.
fn mark_response_truncated() {
    LAST_RESPONSE_TRUNCATED.with(|t| t.set(true));
}

/// Read and clear the truncation flag for the response just read.
fn take_response_truncated() -> bool {
    LAST_RESPONSE_TRUNCATED.with(|t| t.replace(false))
}

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

use cratonvm_native_api::{
    install_baos_event_hook, BaosEvent, NativeContext, NativeMethodRegistry,
};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

use cratonvm_native_io::eintr::{retry_eintr, EintrIo};

use crate::{obj_arg, try_alloc_concurrent_synthetic};

// ---------------------------------------------------------------------------
// Synthetic field layout for sun.net.www.protocol.http.HttpURLConnection
// ---------------------------------------------------------------------------

const HUC_CONN_ID: usize = 0; // i32 native registry key, -1 = unconnected
const HUC_URL_STR: usize = 1; // String form of the URL
const HUC_METHOD: usize = 2; // String, default "GET"
const HUC_REQ_HEADERS: usize = 3; // String[] of "key: value" lines
const HUC_REQ_BODY_STREAM: usize = 4; // ByteArrayOutputStream object or null
const HUC_DO_INPUT: usize = 5;
const HUC_DO_OUTPUT: usize = 6;
const HUC_CONNECTED: usize = 7;
const HUC_DISCONNECTED: usize = 8;
const HUC_INSTANCE_FOLLOW_REDIRECTS: usize = 9;
const HUC_CONNECT_TIMEOUT: usize = 10;
const HUC_READ_TIMEOUT: usize = 11;

const MAX_RESPONSE_BODY: usize = 16 * 1024 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Native connection registry
// ---------------------------------------------------------------------------

struct ConnState {
    status: i32,
    response_body: Vec<u8>,
    response_headers: Vec<(String, String)>,
    /// Once `getInputStream` has been called and the bytes drained, we keep
    /// the body here so subsequent `read()` calls return the same data.
    body_consumed: bool,
    /// The peer aborted before the body was complete — see
    /// `LAST_RESPONSE_TRUNCATED` and `make_response_input_stream`.
    truncated: bool,
}

struct ConnRegistry {
    next_id: i32,
    conns: HashMap<i32, ConnState>,
}

impl ConnRegistry {
    fn new() -> Self {
        Self {
            next_id: 1,
            conns: HashMap::new(),
        }
    }
    fn allocate(&mut self, state: ConnState) -> i32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id <= 0 {
            self.next_id = 1; // wrap back into positive id space
        }
        self.conns.insert(id, state);
        id
    }
    fn get(&self, id: i32) -> Option<&ConnState> {
        self.conns.get(&id)
    }
    fn remove(&mut self, id: i32) {
        self.conns.remove(&id);
    }
}

fn registry() -> &'static Mutex<ConnRegistry> {
    static R: OnceLock<Mutex<ConnRegistry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(ConnRegistry::new()))
}

/// What the TLS handshake of an `https` exchange learned about the peer, kept
/// for the `HttpsURLConnection` accessors that report it.
struct HttpsPeerInfo {
    /// Peer certificate chain, DER, leaf first — exactly what rustls handed
    /// back, so `getServerCertificates()` reports the real chain.
    chain_der: Vec<Vec<u8>>,
    /// JSSE spelling (see `t27_tls`'s `suite_to_java_cipher_name`), not
    /// rustls's `Debug` spelling.
    cipher: String,
    /// The CONNECTION-level view of this exchange has been torn down — see
    /// [`https_recycle_carrier`]. The entry is kept rather than removed for one
    /// load-bearing reason: [`https_ensure_exchanged`] treats "no entry" as
    /// "this connection has never handshaked" and drives a fresh exchange, so
    /// removing the entry would make the very next accessor re-issue the HTTPS
    /// request over the network and repopulate the table — the accessor would
    /// answer again, and it would have made a second request to do it.
    ///
    /// A `record_https_peer_info` for the same carrier clears it: a connection
    /// that was recycled and then genuinely re-handshaked is open again.
    recycled: bool,
}

/// Peer info per connection object, keyed by identity hash.
///
/// Keyed by object identity rather than a `HUC_*` slot for the reason the
/// `RealReq` table above documents: a real-JDK
/// `sun.net.www.protocol.https.HttpsURLConnectionImpl` carries the JDK's own
/// instance layout, and writing a synthetic slot into it corrupts a real field.
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0), after the three sites that
/// evaluated `ctx.identity_hash_code` inside the lock expression (or held the
/// guard across `throw_jca_exc`) were restructured to compute the key, or clone
/// the row out, first. The fourth site already did: a `get(..).map(..)` whose
/// result is matched after the guard has dropped.
fn https_peer_info(
) -> &'static cratonvm_types::lock_order::OrderedMutex<HashMap<u64, HttpsPeerInfo>> {
    static R: OnceLock<cratonvm_types::lock_order::OrderedMutex<HashMap<u64, HttpsPeerInfo>>> =
        OnceLock::new();
    R.get_or_init(|| {
        cratonvm_types::lock_order::OrderedMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

fn record_https_peer_info(
    ctx: &dyn NativeContext,
    connection: Option<ObjectRef>,
    chain_der: &[Vec<u8>],
    cipher: &str,
) {
    let Some(conn) = connection else { return };
    if chain_der.is_empty() {
        return;
    }
    // Key before the guard: `ctx.identity_hash_code` is a re-entry into the
    // VM, and this table's `LockLevel` claims it is never held across one.
    let key = ctx.identity_hash_code(conn) as u32 as u64;
    https_peer_info().lock().unwrap().insert(
        key,
        HttpsPeerInfo {
            chain_der: chain_der.to_vec(),
            cipher: cipher.to_string(),
            recycled: false,
        },
    );
}

/// Tear down the CONNECTION-level view of a completed `https:` exchange, the
/// way HotSpot does when the connection leaves the application's hands.
///
/// MEASURED, HotSpot 25.0.3+9-LTS (`scratchpad/g7/TlsProbe.java`, transcribed
/// in `G7-1` §1d): after `disconnect()` all six `HttpsURLConnection` session
/// accessors throw `IllegalStateException: connection not yet open` again — the
/// same exception, with the same message, that a never-handshaked connection
/// throws. The message describes the STATE and not the call order, which is why
/// the pre-connect and post-disconnect rows are identical. CratonVM kept
/// answering for the life of the carrier object.
///
/// Both tables are torn down, because the six accessors read from two of them
/// (`G7-1` §5.1): the five this file owns read [`https_peer_info`], and
/// `getSSLSession` — the one name this file deliberately does not register —
/// reads `net_phase_e`'s `https_carrier_sessions`. Recycling one and not the
/// other would leave the six disagreeing about whether the connection is open,
/// which is the split that table comment warns about made real.
///
/// `forget_https_carrier_session` is called UNCONDITIONALLY, not only when this
/// file's table has an entry. The two populators do not agree on when they
/// fire: `record_https_peer_info` early-returns for an empty peer chain, while
/// `record_https_carrier_session` runs on every completed handshake, so an
/// anonymous-suite exchange has a carrier session and no peer info. Gating the
/// release on this table would leak exactly those.
///
/// It is also the first call site `net_phase_e::forget_https_carrier_session`
/// has ever had, and therefore the first time anything is removed from
/// `https_carrier_sessions` — which, since `aed6a3b73`, also holds a global
/// root on one `SSLSession` per entry. Until now that table grew by one entry
/// per HTTPS carrier for the life of the process.
fn https_recycle_carrier(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let carrier_key = crate::net_phase_e::native_obj_key(&*ctx, this);
    https_recycle_carrier_by_key(ctx, carrier_key);
}

/// The response streams currently outstanding for an `https` carrier, mapping
/// the stream's identity to the carrier's key.
///
/// WHY A TABLE AND NOT A FIELD ON THE STREAM. The object handed to Java is a
/// `java/io/ByteArrayInputStream` with the JDK's own four-field layout
/// (`buf`, `pos`, `mark`, `count`); there is no spare slot, and writing one
/// would corrupt a real field — the same rule `HttpsPeerInfo`'s own comment
/// states for the carrier. The observer is handed the stream and nothing
/// else, so the association has to live somewhere it can be looked up by
/// stream identity.
///
/// **Bounded.** A row is inserted only for a carrier that already has an
/// `https_peer_info` entry (i.e. a real TLS exchange), and is removed by the
/// first `Eof` or `Close` the stream produces. A stream that is neither
/// drained nor closed leaves one two-integer row — strictly less than what
/// this whole mechanism removes, since an unrecycled carrier holds a GC root
/// on an `SSLSession` for the life of the process.
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Both acquisitions are a
/// single map operation on integers, with the `ctx.identity_hash_code` that
/// produces the key evaluated into a local BEFORE the guard is taken — see
/// `note_response_stream`, which is where it used to sit inside the
/// `table.insert(..)` argument list.
fn https_response_streams(
) -> &'static cratonvm_types::lock_order::OrderedMutex<HashMap<u64, crate::net_phase_e::NativeObjKey>>
{
    static R: OnceLock<
        cratonvm_types::lock_order::OrderedMutex<HashMap<u64, crate::net_phase_e::NativeObjKey>>,
    > = OnceLock::new();
    R.get_or_init(|| {
        cratonvm_types::lock_order::OrderedMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Remember that `stream` is the response body of `carrier`, so draining it
/// recycles the connection the way HotSpot's `KeepAliveCache` does.
///
/// A no-op unless the carrier has a recorded TLS exchange: a plain `http:`
/// connection has no session state to tear down, and registering one would
/// grow the table for every non-TLS request in the process for no effect.
fn note_response_stream(ctx: &dyn NativeContext, stream: ObjectRef, carrier: Option<ObjectRef>) {
    let Some(carrier) = carrier else { return };
    let carrier_key = crate::net_phase_e::native_obj_key(ctx, carrier);
    if !https_peer_info()
        .lock()
        .map(|t| t.contains_key(&(carrier_key.identity as u32 as u64)))
        .unwrap_or(false)
    {
        return;
    }
    // The key is evaluated BEFORE the guard: `identity_hash_code` is a call
    // back into the VM, and the lock-discipline level stamped on this table
    // claims no such call happens under it. Idempotent, so hoisting it is
    // behaviour-preserving.
    let stream_key = ctx.identity_hash_code(stream) as u32 as u64;
    if let Ok(mut table) = https_response_streams().lock() {
        table.insert(stream_key, carrier_key);
    }
}

/// The `BaisEvent` observer: HotSpot's drain instant, made observable.
///
/// MEASURED contract (G44-1 N2, `RSslLiveSession`'s `drainTrap` family): once
/// the response body is fully drained the connection returns to the
/// `KeepAliveCache` and every CONNECTION-level accessor throws
/// `IllegalStateException: connection not yet open` again — the same exception
/// a never-handshaked connection throws — while **the `SSLSession` object the
/// application already holds stays valid**. That second half is the row that
/// separates "recycled" from "destroyed", and it is why this recycles the
/// CARRIER's view and never touches the session object.
///
/// Both events are handled and the row is removed on the first of them, so
/// the `Eof`-then-`Close` sequence a drained-and-closed stream produces
/// recycles once. `BaisEvent::Eof` fires on every exhausted read rather than
/// on the transition (its doc explains why the transition is not observable),
/// so idempotence here is required, not defensive.
fn huc_live_bais_event(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
    _event: cratonvm_native_api::registry::BaisEvent,
) -> Result<(), MethodCallFailed> {
    let stream_key = ctx.identity_hash_code(stream) as u32 as u64;
    // Scoped so the lock is released before the call below, which takes two
    // more process-global locks and can release a GC root. Same rule, and the
    // same reason, as the scoped guard in `https_recycle_carrier_by_key`.
    let carrier_key = {
        let Ok(mut table) = https_response_streams().lock() else {
            return Ok(());
        };
        table.remove(&stream_key)
    };
    if let Some(key) = carrier_key {
        https_recycle_carrier_by_key(ctx, key);
    }
    Ok(())
}

/// [`https_recycle_carrier`] for a caller holding the carrier's KEY rather
/// than the object — see [`huc_live_bais_event`], which is handed the response
/// stream and has no way back to the carrier except this key.
fn https_recycle_carrier_by_key(
    ctx: &mut dyn NativeContext,
    carrier_key: crate::net_phase_e::NativeObjKey,
) {
    let key = carrier_key.identity as u32 as u64;
    // Scoped, and NOT written as `if let Some(..) = https_peer_info().lock()
    // ...`: under Rust 2021's drop rules the guard produced in an `if let`
    // scrutinee lives to the end of the block, so the process-global lock would
    // still be held across the `forget_https_carrier_session` call below —
    // which takes another process-global lock and can release a GC root. That
    // is the "native holds a lock across a call that re-enters the VM" cycle
    // this workspace has already paid for once.
    {
        let mut table = https_peer_info().lock().unwrap();
        if let Some(info) = table.get_mut(&key) {
            info.recycled = true;
        }
    }
    crate::net_phase_e::forget_https_carrier_session_by_key(ctx, carrier_key);
}

/// Pins `this` across [`https_ensure_exchanged_body`] and hands the refreshed reference back.
///
/// The receiver is `&mut` on purpose. The body ALLOCATES and returns no
/// reference, so a moving collector could relocate `this` inside the call and
/// every caller was left holding a pre-move address -- the shape
/// `WORKER-5-NOTE-10` traced `TreeMap.size()` returning 0 to. `&mut` makes
/// forgetting the refresh a COMPILE ERROR instead of an audit finding.
fn https_ensure_exchanged(ctx: &mut dyn NativeContext, this: &mut ObjectRef) {
    let w5_pin = ctx.pin_native_root(*this);
    let w5_out = https_ensure_exchanged_body(ctx, *this);
    *this = ctx.read_native_pin(w5_pin, *this);
    ctx.unpin_native_roots(w5_pin);
    w5_out
}

/// Make sure the exchange that produces the handshake info has actually run.
///
/// `HttpsURLConnection.getServerCertificates()` and friends are defined to
/// report the session of a connected connection, and the JDK's
/// `HttpsURLConnectionImpl` connects on demand. CratonVM's `huc_connect` is a
/// deliberate NO-OP for a real-JDK carrier — HotSpot's `connect()` opens the
/// socket but sends nothing, so the request is deferred to
/// `getResponseCode`/`getInputStream` — which means an app that does
/// `connect(); getServerCertificates();` (netty's `OcspClientTest` does exactly
/// that) had no handshake behind it at all. Drive the same lazy exchange the
/// response getters drive, then read what it recorded.
///
/// Errors are swallowed: the accessor's own contract is
/// `SSLPeerUnverifiedException`, and the caller below raises that when the
/// table is still empty. A connect failure surfaces properly on the next
/// `getResponseCode`/`getInputStream`.
///
/// The early return below is `contains_key`, deliberately NOT
/// "contains a live entry": a RECYCLED carrier must not re-issue its request.
/// That is why [`https_recycle_carrier`] flips a flag instead of removing the
/// row — remove it and this function would drive a second HTTPS exchange on the
/// next accessor call, repopulate the table, and answer as if the connection
/// had never been torn down.
fn https_ensure_exchanged_body(ctx: &mut dyn NativeContext, this: ObjectRef) {
    // Key before the guard — see `record_https_peer_info`.
    let key = ctx.identity_hash_code(this) as u32 as u64;
    if https_peer_info().lock().unwrap().contains_key(&key) {
        return;
    }
    if let Some(url_str) = huc_real_object_url(ctx, this) {
        if url_str.starts_with("https://") {
            let _ = huc_real_perform(ctx, this, &url_str);
        }
        return;
    }
    let _ = ensure_connected(ctx, this);
}

/// `IllegalStateException: connection not yet open` — HotSpot's answer for a
/// session accessor called on a connection that has never handshaked.
///
/// **G7 — this is a DIFFERENT refusal from `SSLPeerUnverifiedException`, and
/// the two were conflated here.** MEASURED, HotSpot 25.0.3+9-LTS,
/// `scratchpad/g7/TlsProbe.java`, against a real loopback HTTPS server:
///
/// ```text
///   before connect()   getCipherSuite         -> IllegalStateException: connection not yet open
///                      getServerCertificates  -> IllegalStateException: connection not yet open
///                      getLocalCertificates   -> IllegalStateException: connection not yet open
///                      getPeerPrincipal       -> IllegalStateException: connection not yet open
///                      getLocalPrincipal      -> IllegalStateException: connection not yet open
///                      getSSLSession          -> IllegalStateException: connection not yet open
///   after  connect()   real values; getSSLSession isPresent = true
///   after  disconnect() IllegalStateException: connection not yet open   (again)
/// ```
///
/// All six, one message, and the post-`disconnect()` row shows the message is
/// about the state and not about the call order. `SSLPeerUnverifiedException`
/// is the answer to a DIFFERENT question — the connection is open and the peer
/// did not authenticate — and it is the one `getServerCertificates` and
/// `getPeerPrincipal` declare in their throws clause.
///
/// SOURCE-VERIFIED, `javap -p javax.net.ssl.HttpsURLConnection` on the oracle:
///
/// ```text
///   public abstract java.lang.String getCipherSuite();
///   public abstract java.security.cert.Certificate[] getLocalCertificates();
///   public abstract java.security.cert.Certificate[] getServerCertificates()
///           throws javax.net.ssl.SSLPeerUnverifiedException;
///   public java.security.Principal getPeerPrincipal()
///           throws javax.net.ssl.SSLPeerUnverifiedException;
///   public java.security.Principal getLocalPrincipal();
/// ```
///
/// So `getCipherSuite` and the two "local" accessors have NO checked exception
/// in their signature at all: raising `SSLPeerUnverifiedException` (an
/// `IOException` subclass) out of them delivered an undeclared checked
/// exception through a `throws`-free method, which no `catch` written against
/// this API can name.
fn https_not_yet_open(ctx: &mut dyn NativeContext) -> MethodCallFailed {
    crate::phases_early::throw_jca_exc(
        ctx,
        "java/lang/IllegalStateException",
        "connection not yet open",
    )
}

/// Has a handshake been recorded against `this` at all?
///
/// The discriminator between the two refusals above: no entry means the
/// exchange never completed (`connection not yet open`); an entry with an empty
/// chain means it did and the peer presented nothing
/// (`peer not authenticated`).
///
/// A RECYCLED entry answers `false`, not `true`. It records that a handshake
/// once happened, which is not the question — the question is whether this
/// CONNECTION is open now, and after [`https_recycle_carrier`] it is not. See
/// that function for HotSpot's measured post-`disconnect()` transcript.
fn https_has_session(ctx: &mut dyn NativeContext, mut this: ObjectRef) -> bool {
    https_ensure_exchanged(ctx, &mut this);
    let key = ctx.identity_hash_code(this) as u32 as u64;
    https_peer_info()
        .lock()
        .unwrap()
        .get(&key)
        .is_some_and(|info| !info.recycled)
}

/// The peer chain recorded for `this`, or the measured refusal for the state
/// it is in: `IllegalStateException` when no handshake was ever recorded,
/// `SSLPeerUnverifiedException` when one was and it carried no chain.
fn https_peer_chain_or_throw(
    ctx: &mut dyn NativeContext,
    mut this: ObjectRef,
) -> Result<Vec<Vec<u8>>, MethodCallFailed> {
    https_ensure_exchanged(ctx, &mut this);
    let key = ctx.identity_hash_code(this) as u32 as u64;
    // A recycled entry is filtered out here rather than matched below, so it
    // lands on the `None` arm — `IllegalStateException: connection not yet
    // open`, which is HotSpot's measured post-`disconnect()` answer, and NOT
    // `SSLPeerUnverifiedException`, which would claim the connection is open
    // and the peer anonymous.
    let found = https_peer_info()
        .lock()
        .unwrap()
        .get(&key)
        .filter(|info| !info.recycled)
        .map(|info| info.chain_der.clone());
    match found {
        Some(chain) if !chain.is_empty() => Ok(chain),
        // An entry exists, so the handshake happened; it just produced no
        // chain. That is the state `SSLPeerUnverifiedException` names, and it
        // is the exception both of this helper's callers declare.
        Some(_) => Err(crate::phases_early::throw_jca_exc(
            ctx,
            "javax/net/ssl/SSLPeerUnverifiedException",
            "peer not authenticated",
        )),
        // RESIDUAL, stated because it makes this arm reachable more often than
        // it should be: `record_https_peer_info` early-returns when the chain
        // is empty, so an anonymous-suite handshake leaves NO entry and lands
        // here rather than one line above. Fixing that means recording the
        // entry unconditionally, which is `record_https_peer_info`'s call
        // contract and is left alone here — see G7-1 §5.
        None => Err(https_not_yet_open(ctx)),
    }
}

/// The `javax.net.ssl.HttpsURLConnection` accessors that report the handshake.
///
/// These are ABSTRACT on `javax.net.ssl.HttpsURLConnection` — in the real JDK
/// they are implemented by `HttpsURLConnectionImpl`, which forwards to a
/// `DelegateHttpsURLConnection` holding the live `SSLSession`. CratonVM's
/// carrier performs the exchange itself (see `perform`) and has no such
/// delegate, so without these registrations a call landed on the abstract
/// declaration and threw
/// `AbstractMethodError: javax/net/ssl/HttpsURLConnection.getServerCertificates()
/// has no Code attribute` — an abstract declaration reached because no
/// override exists, not a bad dispatch, and the failure behind
/// `OcspClientTest`'s `[1] https://apple.com` case in the retired
/// `ssl-cert-validation-residuals` write-up.
///
/// Registered for the whole family, not just the one method the OCSP test
/// needed: an app that reads the chain almost always reads the cipher suite
/// beside it, and leaving the siblings abstract just moves the same
/// `AbstractMethodError` one line down.
///
/// ## G7 — THIS registrar wins, and the other one's comment says it cannot
///
/// `net_phase_e.rs` has a function of the SAME NAME,
/// `register_https_session_accessors`, registering the same six names on the
/// same two classes. Its doc comment reasons about which copy is live and
/// concludes: *"None of the six names below appear in `register_one`, so none
/// of them can be overwritten by it. If a later change adds any of them there,
/// THAT copy wins and this one goes silently dead."*
///
/// The premise is true of `register_one` and the conclusion is false, because
/// the names were added to THIS function instead — a second registrar in the
/// same file, reached from the same `register_http_url_connection_real`:
///
/// ```text
///   lib.rs:18688  net_phase_e::register_phase_e_networking
///                   -> register_re4_url_http -> register_https_session_accessors  (6 names)
///   lib.rs:18805  http_url_connection::register_http_url_connection_real
///                   -> register_https_session_accessors(r, "sun/net/www/protocol/https/HttpsURLConnectionImpl")
///                   -> register_https_session_accessors(r, "javax/net/ssl/HttpsURLConnection")   (5 names)
/// ```
///
/// Both calls are inside `register_essential_natives_with_shims`, 18805 after
/// 18688, and registration is last-write-wins. So on the real-JDK path the
/// bodies below own `getServerCertificates`, `getLocalCertificates`,
/// `getCipherSuite`, `getPeerPrincipal` and `getLocalPrincipal`, and
/// net_phase_e's five copies are dead. `getSSLSession` is the ONE name this
/// function does not register, so net_phase_e's survives for it alone.
///
/// **The consequence is that the six accessors answer from TWO DIFFERENT
/// TABLES.** These five read `https_peer_info()` (this file, populated by
/// `record_https_peer_info` on the handshake path); the surviving
/// `getSSLSession` reads net_phase_e's `https_carrier_session` (populated by
/// `record_https_carrier_session`, called from `huc_verify_hostname` STEP 0).
/// Both populators run on the same successful exchange, so the split is not
/// currently observable — but it is one deleted call away from being so, and
/// it is why the refusal wording had drifted apart between the two halves
/// (net_phase_e's `https_not_yet_open` was already right; this file's
/// `SSLPeerUnverifiedException` was not). NOMINATED in G7-1: collapse the two
/// registrars and the two tables into one.
///
/// Established by reading the two call sites in `lib.rs`, not from either
/// comment — this file's own history (C6-3) is that a comment about which body
/// runs is the least reliable thing in the tree. It has NOT been confirmed
/// against a `--dump-native-registry` dump, because no binary carrying this
/// change exists yet; that check is listed for the orchestrator in G7-1 §7.
fn register_https_session_accessors(r: &mut NativeMethodRegistry, cls: &str) {
    r.register(
        cls,
        "getServerCertificates",
        "()[Ljava/security/cert/Certificate;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let chain = https_peer_chain_or_throw(ctx, this)?;
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), chain.len());
            for (i, der) in chain.iter().enumerate() {
                let mirror = crate::keystore::make_x509_mirror(ctx, "peer", der)?;
                ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );
    // No client certificate is ever sent by `perform` (it builds its rustls
    // client config without one), so this is `null` — the JDK's own answer for
    // a connection that did not authenticate itself, not a stand-in. MEASURED
    // and confirmed: a completed client connection answers `null` here, and so
    // does `getLocalPrincipal` below.
    //
    // G7: but only ONCE THE CONNECTION IS OPEN. This body used to answer `null`
    // unconditionally, including before any handshake, where HotSpot throws
    // `IllegalStateException: connection not yet open` (measured — see
    // `https_not_yet_open`). A `null` there is the silent-lie shape this VM
    // removes elsewhere: it tells a caller "no local certificate was sent" for
    // a connection that has not been opened, which is an answer to a question
    // that has no answer yet.
    r.register(
        cls,
        "getLocalCertificates",
        "()[Ljava/security/cert/Certificate;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if !https_has_session(ctx, this) {
                return Err(https_not_yet_open(ctx));
            }
            Ok(Some(Value::Object(None)))
        },
    );
    r.register(
        cls,
        "getCipherSuite",
        "()Ljava/lang/String;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            https_ensure_exchanged(ctx, &mut this);
            let key = ctx.identity_hash_code(this) as u32 as u64;
            // `filter` before `map`: a recycled connection has no cipher suite to
            // report, and falls through to the refusal below. See
            // `https_recycle_carrier`.
            let cipher = https_peer_info()
                .lock()
                .unwrap()
                .get(&key)
                .filter(|i| !i.recycled)
                .map(|i| i.cipher.clone());
            match cipher {
                Some(c) if !c.is_empty() => Ok(Some(Value::Object(Some(ctx.create_string(&c))))),
                // G7: `IllegalStateException`, not `SSLPeerUnverifiedException`.
                // `getCipherSuite()` is declared `public abstract String
                // getCipherSuite();` with NO throws clause (SOURCE-VERIFIED by
                // `javap` — see `https_not_yet_open`), so the old refusal was an
                // undeclared checked exception out of a method whose signature
                // cannot name it. HotSpot's measured refusal in this state is
                // `IllegalStateException: connection not yet open`.
                _ => Err(https_not_yet_open(ctx)),
            }
        },
    );
    r.register(
        cls,
        "getPeerPrincipal",
        "()Ljava/security/Principal;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let chain = https_peer_chain_or_throw(ctx, this)?;
            // JSSE's own fallback: the peer principal is the leaf certificate's
            // subject when the session carries no separate principal.
            let leaf = crate::keystore::make_x509_mirror(ctx, "peer", &chain[0])?;
            let pin = ctx.pin_native_root(leaf);
            let leaf = ctx.read_native_pin(pin, leaf);
            let principal = ctx.invoke_virtual(
                leaf,
                "getSubjectX500Principal",
                "()Ljavax/security/auth/x500/X500Principal;",
                &[],
            );
            ctx.unpin_native_roots(pin);
            match principal {
                Ok(Some(v @ Value::Object(Some(_)))) => Ok(Some(v)),
                Err(e) => Err(e),
                _ => Ok(Some(Value::Object(None))),
            }
        },
    );
    // Same split as `getLocalCertificates` above: `null` once the connection is
    // open (MEASURED — a client that sent no certificate has no local
    // principal), the "not yet open" refusal before that.
    r.register(
        cls,
        "getLocalPrincipal",
        "()Ljava/security/Principal;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if !https_has_session(ctx, this) {
                return Err(https_not_yet_open(ctx));
            }
            Ok(Some(Value::Object(None)))
        },
    );
}

// ---------------------------------------------------------------------------
// Real-JDK HttpURLConnection support
// ---------------------------------------------------------------------------
//
// When `URL.openConnection()` runs GENUINE JDK bytecode (a real `http(s)://`
// URL whose stream handler is wired — e.g. `URI.create(...).toURL()` in
// `TomcatBaseTest.getUrl`), it returns a real
// `sun.net.www.protocol.http.HttpURLConnection`, NOT our synthetic carrier.
// That object's instance layout is the JDK's (field 0 = `URLConnection.url`, a
// `java/net/URL`) and does NOT match our `HUC_*` slots — so the synthetic
// `getResponseCode`/`getInputStream` natives below misread it (`HUC_CONNECTED`
// lands on an unrelated real field that reads 1 → `ensure_connected`
// early-returns making NO request → `-1`; confirmed by tracing, see
// 10-pagecontext-npe-contains-null-FAIL.md). Detect that
// case via the URL object at field 0, perform the request from the *real* URL,
// and cache the result keyed by the connection object's identity hash so a
// follow-up `getInputStream` returns the same body. The synthetic resource-URL
// path (`file:`/`jar:`/`classpath:` → `URL.openStream`) is left untouched.

struct RealResult {
    status: i32,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// The real reason phrase read off the wire (see `LAST_REASON_PHRASE`).
    /// Empty for the synthetic timeout-sentinel result, in which case
    /// `huc_get_response_message` falls back to the hardcoded table.
    reason: String,
    /// The peer aborted before the body was complete (see
    /// `LAST_RESPONSE_TRUNCATED`). `body` holds the bytes that DID arrive;
    /// the stream handed to Java replays them and then throws `IOException`.
    truncated: bool,
}

fn real_results() -> &'static Mutex<HashMap<i32, RealResult>> {
    static R: OnceLock<Mutex<HashMap<i32, RealResult>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Per-real-connection REQUEST state, keyed by `identity_hash_code(this)`.
///
/// A real-JDK `sun.net.www...HttpURLConnection` (handed out by the genuine
/// `URL.openConnection()` machinery) carries the JDK's own instance layout, so
/// the synthetic `HUC_*` slots do NOT apply — writing them corrupts unrelated
/// real fields and reading them yields garbage (this is the root cause of the
/// dropped-`Authorization`-header / `cannot write after connect` /
/// empty-`getHeaderFields` cluster). We therefore keep every piece of request
/// state a real carrier needs (method, request headers, doOutput) in this
/// identity-keyed side-table, mirroring the established
/// `net_phase_e::stream_owner_table` precedent for real-JDK carrier objects.
#[derive(Clone)]
struct RealReq {
    method: String,                 // empty => "GET"
    headers: Vec<(String, String)>, // ordered; setRequestProperty replaces, addRequestProperty appends
    do_output: bool,
    // Caller-configured timeouts in ms (Java semantics: 0 = infinite, unset =
    // None → fall back to a sane default). The real-JDK carrier cannot park
    // these in synthetic slots, so they live here keyed by object identity.
    connect_timeout_ms: Option<i32>,
    read_timeout_ms: Option<i32>,
    follow_redirects: bool,
    streaming: StreamingMode,
}

/// Only fixed-length streaming has a wire-level implementation today.  It is
/// deliberately recorded separately from a user-supplied Content-Length: the
/// JDK's streaming setter changes *when* bytes are sent, not just which header
/// appears on the request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum StreamingMode {
    #[default]
    Buffered,
    Fixed(u64),
    Chunked(i32),
}

impl Default for RealReq {
    fn default() -> Self {
        Self {
            method: String::new(),
            headers: Vec::new(),
            do_output: false,
            connect_timeout_ms: None,
            read_timeout_ms: None,
            follow_redirects: true,
            streaming: StreamingMode::Buffered,
        }
    }
}

fn real_reqs() -> &'static Mutex<HashMap<i32, RealReq>> {
    static R: OnceLock<Mutex<HashMap<i32, RealReq>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Real-carrier buffered request-body stream (the `ByteArrayOutputStream`
/// returned by `getOutputStream`), keyed by `identity_hash_code(this)`. We
/// cannot park it in a synthetic slot on a real object, so we track the object
/// by identity — same rationale and lifetime as `stream_owner_table`: the
/// harness keeps the stream live on its Java stack across the
/// write→`getResponseCode` window, so the bytes are read back at perform time.
///
/// GC note (gc-followups-20260706): the Java stack keeps the BAOS ALIVE, but
/// this raw ref is never REMAPPED — a moving GC in the
/// write→`getResponseCode` window leaves a stale address behind and the
/// response-perform path then reads the body from the old location.
/// Follow-up: store `(identity_key, ObjectRef)` var-handle-root pairs
/// (ASYNC_POOL pattern) and re-read at perform time.
/// GC note (cce0079 follow-up): values are `(identity_key, last_addr)`
/// VarHandle-root pairs — registered at insert so the BAOS stays alive and
/// registry-remapped across moving GCs; readers resolve the CURRENT address
/// via `ctx.read_var_handle_root(identity_key)` with the stored address as
/// fallback. (Keys were already GC-stable identity hashes.)
fn real_body_streams() -> &'static Mutex<HashMap<i32, (i32, ObjectRef)>> {
    static R: OnceLock<Mutex<HashMap<i32, (i32, ObjectRef)>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// An active HTTP/1.1 fixed-length request.  The Java-facing output stream is
/// still a ByteArrayOutputStream-shaped object, but native-io offers its
/// writes to the hook below before it buffers them.  This lets the real
/// HttpURLConnection carrier preserve the JDK's immediate-upload semantics
/// without teaching the I/O crate about HTTP.
struct LiveFixedStream {
    tcp: TcpStream,
    expected: u64,
    written: u64,
    closed: bool,
}

fn live_fixed_streams() -> &'static Mutex<HashMap<i32, LiveFixedStream>> {
    static R: OnceLock<Mutex<HashMap<i32, LiveFixedStream>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Maps the identity of the returned BAOS to its owning real connection.
fn live_fixed_owners() -> &'static Mutex<HashMap<i32, i32>> {
    static R: OnceLock<Mutex<HashMap<i32, i32>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// True if `this` is a real-JDK URLConnection carrier (field 0 holds the
/// `java/net/URL` object) rather than our synthetic carrier (field 0 = i32
/// conn-id). The real JDK constructor uses a different descriptor than our
/// `<init>(Ljava/net/URL;)V` native, so a genuine carrier never runs `huc_init`
/// and keeps the JDK layout (field 0 = `URLConnection.url`).
fn is_real_carrier(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    matches!(ctx.get_field(this, HUC_CONN_ID), Value::Object(Some(_)))
}

/// Mutate this real carrier's `RealReq` entry (creating it on first use).
fn with_real_req<R>(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    f: impl FnOnce(&mut RealReq) -> R,
) -> R {
    let key = ctx.identity_hash_code(this);
    let mut t = real_reqs().lock().expect("real_reqs poisoned");
    f(t.entry(key).or_default())
}

/// If `this` is a real-JDK URLConnection (field 0 is a `java/net/URL` object,
/// not our synthetic int conn-id), return its full external-form URL string via
/// `URL.toExternalForm()` (robust for both synthetic and real URL layouts).
fn huc_real_object_url(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<String> {
    let url_obj = match ctx.get_field(this, HUC_CONN_ID) {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    match ctx.invoke_virtual(url_obj, "toExternalForm", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    }
}

/// Read the buffered request body for a real-JDK carrier from the
/// `ByteArrayOutputStream` recorded by `getOutputStream` (synthetic BAOS layout:
/// field 0 = byte[] buf, field 1 = count). Empty if no body was written.
fn real_body_bytes(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<u8> {
    let key = ctx.identity_hash_code(this);
    let baos = match real_body_streams()
        .lock()
        .ok()
        .and_then(|t| t.get(&key).copied())
    {
        // Resolve the CURRENT address through the VarHandle-root registry
        // (the stored copy is stale after any moving GC).
        Some((vkey, stored)) => ctx.read_var_handle_root(vkey).unwrap_or(stored),
        None => return Vec::new(),
    };
    if let Value::Object(Some(arr)) = ctx.get_field(baos, 0) {
        let mut buf = read_byte_array(ctx, arr);
        if let Value::Int(count) = ctx.get_field(baos, 1) {
            if (count as usize) < buf.len() {
                buf.truncate(count as usize);
            }
        }
        return buf;
    }
    Vec::new()
}

/// Native-io calls this before it mutates an ordinary BAOS.  Returning `true`
/// consumes the event, so only BAOS instances registered by
/// `start_live_fixed_stream` bypass heap buffering.
fn huc_live_baos_event(
    ctx: &mut dyn NativeContext,
    baos: ObjectRef,
    event: BaosEvent,
) -> Result<bool, MethodCallFailed> {
    let baos_key = ctx.identity_hash_code(baos);
    let Some(conn_key) = live_fixed_owners()
        .lock()
        .ok()
        .and_then(|owners| owners.get(&baos_key).copied())
    else {
        return Ok(false);
    };

    let bytes = match event {
        BaosEvent::WriteByte(b) => Some(vec![b]),
        BaosEvent::WriteArray { array, offset, len } => {
            let mut out = vec![0u8; len];
            ctx.read_byte_array_into(array, offset, &mut out);
            Some(out)
        }
        BaosEvent::Flush | BaosEvent::Close => None,
    };
    let mut streams = live_fixed_streams()
        .lock()
        .map_err(|_| ioex("HttpURLConnection fixed-length stream registry poisoned"))?;
    let stream = streams
        .get_mut(&conn_key)
        .ok_or_else(|| ioex("HttpURLConnection fixed-length stream is no longer available"))?;

    if let Some(bytes) = bytes {
        if stream.closed {
            return Err(ioex("HttpURLConnection fixed-length stream is closed"));
        }
        let next = stream
            .written
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| ioex("HttpURLConnection fixed-length stream overflow"))?;
        if next > stream.expected {
            return Err(ioex(format!(
                "HttpURLConnection fixed-length stream wrote {next} bytes; expected {}",
                stream.expected
            )));
        }
        ctx.begin_blocking_region();
        let write_result = stream
            .tcp
            .write_all(&bytes)
            .and_then(|_| stream.tcp.flush());
        ctx.end_blocking_region();
        write_result.map_err(|e| ioex(format!("HttpURLConnection streaming write failed: {e}")))?;
        stream.written = next;
        return Ok(true);
    }

    match event {
        BaosEvent::Flush => {
            ctx.begin_blocking_region();
            let result = stream.tcp.flush();
            ctx.end_blocking_region();
            result.map_err(|e| ioex(format!("HttpURLConnection streaming flush failed: {e}")))?;
        }
        BaosEvent::Close => {
            if !stream.closed {
                if stream.written != stream.expected {
                    return Err(ioex(format!(
                        "HttpURLConnection fixed-length stream closed after {} bytes; expected {}",
                        stream.written, stream.expected
                    )));
                }
                ctx.begin_blocking_region();
                let result = stream.tcp.shutdown(Shutdown::Write);
                ctx.end_blocking_region();
                result
                    .map_err(|e| ioex(format!("HttpURLConnection streaming close failed: {e}")))?;
                stream.closed = true;
            }
        }
        BaosEvent::WriteByte(_) | BaosEvent::WriteArray { .. } => unreachable!(),
    }
    Ok(true)
}

fn take_live_fixed_stream(conn_key: i32) -> Result<Option<LiveFixedStream>, MethodCallFailed> {
    let stream = live_fixed_streams()
        .lock()
        .map_err(|_| ioex("HttpURLConnection fixed-length stream registry poisoned"))?
        .remove(&conn_key);
    if stream.is_some() {
        if let Ok(mut owners) = live_fixed_owners().lock() {
            owners.retain(|_, owner| *owner != conn_key);
        }
    }
    Ok(stream)
}

fn forget_live_fixed_stream(conn_key: i32) {
    if let Ok(mut streams) = live_fixed_streams().lock() {
        streams.remove(&conn_key);
    }
    if let Ok(mut owners) = live_fixed_owners().lock() {
        owners.retain(|_, owner| *owner != conn_key);
    }
}

/// Open a plain HTTP connection and write the request head now.  HTTPS keeps
/// the established buffered path because its rustls stream may re-enter Java
/// for key-manager callbacks; moving that stateful handshake into a write hook
/// would require a separate TLS stream owner.  The Tomcat regression and the
/// JDK fixed-length contract exercised here are plain HTTP.
fn start_live_fixed_stream(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    baos: ObjectRef,
    expected: u64,
) -> Result<bool, MethodCallFailed> {
    let Some(url_str) = huc_real_object_url(ctx, this) else {
        return Ok(false);
    };
    let parsed = parse_url(&url_str).map_err(ioex)?;
    if parsed.scheme != "http" {
        return Ok(false);
    }
    let conn_key = ctx.identity_hash_code(this);
    if live_fixed_streams()
        .lock()
        .ok()
        .is_some_and(|streams| streams.contains_key(&conn_key))
    {
        return Ok(true);
    }
    let req = real_reqs()
        .lock()
        .ok()
        .and_then(|reqs| reqs.get(&conn_key).cloned())
        .unwrap_or_default();
    let method = if req.method.is_empty() {
        "POST"
    } else {
        req.method.as_str()
    };
    let connect_timeout = match req.connect_timeout_ms {
        Some(v) if v > 0 => Duration::from_millis(v as u64),
        _ => Duration::from_secs(30),
    };
    let read_timeout = match req.read_timeout_ms {
        Some(v) if v > 0 => Duration::from_millis(v as u64),
        _ => Duration::from_secs(60),
    };
    // The streaming setter owns Content-Length.  Do not let an earlier manual
    // header make the protocol head disagree with the exact byte count that
    // the write hook enforces.
    let mut headers: Vec<(String, String)> = req
        .headers
        .into_iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("content-length"))
        .collect();
    headers.push(("Content-Length".to_string(), expected.to_string()));
    let head = build_request_head(method, &parsed, &headers, expected, true);

    let addr = format!("{}:{}", parsed.host, parsed.port);
    let mut addrs: Vec<std::net::SocketAddr> =
        std::net::ToSocketAddrs::to_socket_addrs(&addr.as_str())
            .map_err(|e| ioex(format!("HttpURLConnection resolve {addr}: {e}")))?
            .collect();
    addrs.sort_by_key(|sa| u8::from(sa.is_ipv6()));
    ctx.begin_blocking_region();
    let tcp = addrs
        .into_iter()
        .find_map(|sa| TcpStream::connect_timeout(&sa, connect_timeout).ok());
    ctx.end_blocking_region();
    let mut tcp = tcp.ok_or_else(|| ioex(format!("HttpURLConnection connect failed: {addr}")))?;
    let _ = tcp.set_read_timeout(Some(read_timeout));
    let _ = tcp.set_write_timeout(Some(read_timeout));
    let _ = tcp.set_nodelay(true);
    ctx.begin_blocking_region();
    let write_result = tcp.write_all(&head).and_then(|_| tcp.flush());
    ctx.end_blocking_region();
    write_result.map_err(|e| {
        ioex(format!(
            "HttpURLConnection streaming header write failed: {e}"
        ))
    })?;

    live_fixed_streams()
        .lock()
        .map_err(|_| ioex("HttpURLConnection fixed-length stream registry poisoned"))?
        .insert(
            conn_key,
            LiveFixedStream {
                tcp,
                expected,
                written: 0,
                closed: false,
            },
        );
    live_fixed_owners()
        .lock()
        .map_err(|_| ioex("HttpURLConnection fixed-length owner registry poisoned"))?
        .insert(ctx.identity_hash_code(baos), conn_key);
    Ok(true)
}

/// Perform (idempotently) the request for a real-JDK http(s) connection and
/// cache `(status, headers, body)` by object identity. Returns the HTTP status
/// (-1 on parse/IO failure). The request honors the method, headers, and body
/// the caller staged via `setRequestMethod` / `setRequestProperty` / the output
/// stream — all tracked in the identity-keyed `real_reqs` / `real_body_streams`
/// side-tables, since a real carrier's synthetic slots are unusable. Wrapped in
/// a GC-safe blocking region so the blocking I/O does not stall an in-process
/// CratonVM server's worker threads.
/// Cached status sentinel marking a connection whose request timed out, so
/// repeat getters re-raise `SocketTimeoutException` without blocking again.
const HUC_TIMEOUT_STATUS: i32 = -2;

fn huc_real_perform(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    url_str: &str,
) -> Result<i32, MethodCallFailed> {
    let key = ctx.identity_hash_code(this);
    if let Some(st) = real_results()
        .lock()
        .ok()
        .and_then(|t| t.get(&key).map(|r| r.status))
    {
        if st == HUC_TIMEOUT_STATUS {
            return Err(socket_timeout_ex("Read timed out"));
        }
        return Ok(st);
    }
    // Fixed-length streaming has already sent the request head and each body
    // write on this same TCP connection.  Read its response instead of calling
    // `perform` (which would open a second connection and replay an empty
    // buffered BAOS).  Redirect replay is intentionally not attempted here:
    // the JDK likewise cannot transparently replay a streamed request body.
    if let Some(mut live) = take_live_fixed_stream(key)? {
        if live.written != live.expected {
            return Err(ioex(format!(
                "HttpURLConnection fixed-length stream has {} bytes; expected {} before reading the response",
                live.written, live.expected
            )));
        }
        let method = real_reqs()
            .lock()
            .ok()
            .and_then(|reqs| reqs.get(&key).map(|req| req.method.clone()))
            .unwrap_or_default();
        ctx.begin_blocking_region();
        let response = read_response(&mut live.tcp, method.eq_ignore_ascii_case("HEAD"));
        ctx.end_blocking_region();
        return match response {
            Ok((status, headers, body)) => {
                let reason = LAST_REASON_PHRASE.with(|r| r.borrow().clone());
                if let Ok(mut results) = real_results().lock() {
                    results.insert(
                        key,
                        RealResult {
                            status,
                            headers,
                            body,
                            reason,
                            truncated: take_response_truncated(),
                        },
                    );
                }
                Ok(status)
            }
            Err(ref e) if e == READ_TIMEOUT_SENTINEL => {
                if let Ok(mut results) = real_results().lock() {
                    results.insert(
                        key,
                        RealResult {
                            status: HUC_TIMEOUT_STATUS,
                            headers: Vec::new(),
                            body: Vec::new(),
                            reason: String::new(),
                            truncated: false,
                        },
                    );
                }
                Err(socket_timeout_ex("Read timed out"))
            }
            // A fixed-length streaming request has already committed its head
            // and body to this connection.  If the peer aborts before a
            // response can be parsed, surface that transport failure as the
            // IOException HotSpot exposes to `postUrl`, rather than treating it
            // like a malformed buffered response and returning -1.
            Err(e) => Err(ioex(format!(
                "HttpURLConnection streaming response failed: {e}"
            ))),
        };
    }
    let req = real_reqs()
        .lock()
        .ok()
        .and_then(|t| t.get(&key).cloned())
        .unwrap_or_default();
    let mut method = if req.method.is_empty() {
        "GET".to_string()
    } else {
        req.method.clone()
    };
    // Honor the caller's setConnectTimeout/setReadTimeout (ms). Java treats 0
    // as "infinite"; an unbounded blocking read would hang a worker forever,
    // so 0/unset keeps the historical 30s/60s sane defaults while an explicit
    // positive value (e.g. TomcatBaseTest's 1000ms) is honored exactly.
    let connect_to = match req.connect_timeout_ms {
        Some(v) if v > 0 => Duration::from_millis(v as u64),
        _ => Duration::from_secs(30),
    };
    let read_to = match req.read_timeout_ms {
        Some(v) if v > 0 => Duration::from_millis(v as u64),
        _ => Duration::from_secs(60),
    };
    let original_body = real_body_bytes(ctx, this);
    let mut body = original_body.clone();
    let mut current_url = url_str.to_string();
    let mut final_resp = None;
    // `perform` parks this thread in a GC-blocking region for every socket
    // wait (see its STW-TAKEOVER-FIX note), so a moving collection can relocate
    // `this` mid-exchange. The raw native local would then name a stale slot on
    // the next redirect hop, or when `huc_client_tls_restrictions`/`perform`
    // reads the connection's own fields. Pin it and re-read the forwarded
    // address at the top of each hop — the pin also covers
    // `huc_client_tls_restrictions`, which allocates and runs bytecode.
    let this_pin = ctx.pin_native_root(this);
    for redirect_count in 0..=20 {
        let this = ctx.read_native_pin(this_pin, this);
        let parsed = match parse_url(&current_url) {
            Ok(p) => p,
            Err(_) => {
                ctx.unpin_native_roots(this_pin);
                return Ok(-1);
            }
        };
        let tls_restrictions = if parsed.scheme == "https" {
            match huc_client_tls_restrictions(ctx, Some(this), &parsed.host, parsed.port) {
                Ok(r) => r,
                Err(e) => {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            }
        } else {
            None
        };
        let this = ctx.read_native_pin(this_pin, this);
        let resp = perform_with_retry(
            ctx,
            Some(this),
            &parsed,
            &method,
            &req.headers,
            &body,
            connect_to,
            read_to,
            tls_restrictions.as_ref(),
        );
        match resp {
            Ok((status, headers, resp_body))
                if req.follow_redirects && is_redirect_status(status) && redirect_count < 20 =>
            {
                if let Some(location) = header_value(&headers, "Location") {
                    current_url = resolve_redirect_url(&parsed, &location);
                    if status == 303
                        || ((status == 301 || status == 302) && method != "GET" && method != "HEAD")
                    {
                        method = "GET".to_string();
                        body.clear();
                    }
                    continue;
                }
                final_resp = Some(Ok((status, headers, resp_body)));
                break;
            }
            other => {
                final_resp = Some(other);
                break;
            }
        }
    }
    ctx.unpin_native_roots(this_pin);
    let resp = final_resp.unwrap_or_else(|| Ok((310, Vec::new(), Vec::new())));
    match resp {
        Ok((status, headers, body)) => {
            let reason = LAST_REASON_PHRASE.with(|r| r.borrow().clone());
            if let Ok(mut t) = real_results().lock() {
                t.insert(
                    key,
                    RealResult {
                        status,
                        headers,
                        body,
                        reason,
                        truncated: take_response_truncated(),
                    },
                );
            }
            Ok(status)
        }
        // A read timeout maps to java.net.SocketTimeoutException (real-JDK
        // behaviour) — code such as TestConnector.testStop catches it
        // specifically. Cache the sentinel so later getters re-raise without
        // blocking for another full timeout.
        Err(ref e) if e == READ_TIMEOUT_SENTINEL => {
            if let Ok(mut t) = real_results().lock() {
                t.insert(
                    key,
                    RealResult {
                        status: HUC_TIMEOUT_STATUS,
                        headers: Vec::new(),
                        body: Vec::new(),
                        reason: String::new(),
                        truncated: false,
                    },
                );
            }
            Err(socket_timeout_ex("Read timed out"))
        }
        // A TLS handshake-phase failure (see `TLS_HANDSHAKE_FAILURE_SENTINEL`'s
        // doc) must reach Java as `SSLHandshakeException` — real JSSE never
        // silently reports "-1" for a rejected/aborted handshake, and several
        // Tomcat tests specifically assert on catching that exception type
        // (e.g. a required client certificate that was not presented).
        Err(ref e) if e.starts_with(TLS_HANDSHAKE_FAILURE_SENTINEL) => {
            Err(crate::phases_early::throw_jca_exc(
                ctx,
                "javax/net/ssl/SSLHandshakeException",
                e.trim_start_matches(TLS_HANDSHAKE_FAILURE_SENTINEL),
            ))
        }
        // A caller-installed `HostnameVerifier` rejected the peer. Real JSSE
        // raises `SSLPeerUnverifiedException` here (from
        // `HttpsClient.checkURLSpoofing`), not `SSLHandshakeException` — the
        // handshake itself succeeded; it is the identity that was refused.
        // Both extend `SSLException`/`IOException`, so a caller catching
        // either supertype is unaffected, but code that catches the precise
        // type (the shape a pinning test asserts on) needs this distinction.
        Err(ref e) if e.starts_with(TLS_PEER_UNVERIFIED_SENTINEL) => {
            Err(crate::phases_early::throw_jca_exc(
                ctx,
                "javax/net/ssl/SSLPeerUnverifiedException",
                e.trim_start_matches(TLS_PEER_UNVERIFIED_SENTINEL),
            ))
        }
        // An application `HostnameVerifier` was consulted and answered `false`.
        // MEASURED: a plain `java.io.IOException`, NOT the
        // `SSLPeerUnverifiedException` above — see
        // `TLS_HOSTNAME_REFUSED_SENTINEL` for the transcript and the JDK source
        // line.
        Err(ref e) if e.starts_with(TLS_HOSTNAME_REFUSED_SENTINEL) => Err(ioex(
            e.trim_start_matches(TLS_HOSTNAME_REFUSED_SENTINEL)
                .to_string(),
        )),
        // A refused TCP connect (see `CONNECT_REFUSED_SENTINEL`'s doc) must
        // reach Java as `ConnectException`, not a generic IOException — real
        // code catches it specifically (see the type's own doc).
        Err(ref e) if e.starts_with(CONNECT_REFUSED_SENTINEL) => {
            Err(RuntimeError::ConnectException {
                message: e.trim_start_matches(CONNECT_REFUSED_SENTINEL).to_string(),
            }
            .into())
        }
        // A transport failure before a response is available is an IOException.
        Err(e) => Err(ioex(format!("HttpURLConnection response failed: {e}"))),
    }
}

/// Cached response body for a real-JDK connection (empty if not performed).
fn huc_real_body(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<u8> {
    let key = ctx.identity_hash_code(this);
    real_results()
        .lock()
        .ok()
        .and_then(|t| t.get(&key).map(|r| r.body.clone()))
        .unwrap_or_default()
}

/// Whether the cached response for a real-JDK connection was truncated by the
/// peer (see `LAST_RESPONSE_TRUNCATED`).
fn huc_real_truncated(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let key = ctx.identity_hash_code(this);
    real_results()
        .lock()
        .ok()
        .and_then(|t| t.get(&key).map(|r| r.truncated))
        .unwrap_or(false)
}

/// Build the `InputStream` handed to Java for a response body.
///
/// For a complete response this is just a `ByteArrayInputStream` over `body`.
/// For a TRUNCATED response (peer aborted mid-body) it is a
/// `SequenceInputStream(ByteArrayInputStream(body), <a stream whose first read
/// throws IOException>)`, so a Java reader sees exactly what HotSpot shows it:
/// the bytes that arrived, followed by an `IOException` at the point the data
/// stopped — rather than an empty body (what discarding the partial body gave)
/// or a silent clean EOF (what returning it without the error would give).
///
/// The trailing "always throws" stream is an **unconnected**
/// `java.io.PipedInputStream`: a real JDK class, constructed with no side
/// effects, whose every `read` overload immediately throws
/// `IOException("Pipe not connected")`. Using a real `InputStream` subclass
/// (rather than a CratonVM synthetic) matters because callers wrap the result
/// in `BufferedInputStream`/`InputStreamReader`, which are typed against
/// `java.io.InputStream`.
fn make_response_input_stream(
    ctx: &mut dyn NativeContext,
    body: &[u8],
    truncated: bool,
    carrier: Option<ObjectRef>,
) -> MethodCallResult {
    let head = make_byte_array_input_stream(ctx, body);
    // Associate the BAIS — never the `SequenceInputStream` wrapper — with the
    // carrier: the BAIS is what `native-io` observes, and on the truncated
    // path the wrapper produces no `BaisEvent` of its own. The truncated case
    // is registered too, deliberately: its EOF still means the application is
    // done with the bytes that arrived, and the error tail that follows is a
    // read failure, not a reason to keep the connection's view open.
    if let Ok(Value::Object(Some(head_ref))) = head {
        note_response_stream(&*ctx, head_ref, carrier);
    }
    if !truncated {
        return Ok(Some(head?));
    }
    let Ok(Value::Object(Some(head_ref))) = head else {
        return Ok(Some(head?));
    };
    // `head` must survive the two constructor up-calls below, both of which can
    // allocate and therefore relocate it under the moving collector — pin it and
    // read the forwarded reference back afterwards.
    let pin = ctx.pin_native_root(head_ref);
    let tail = ctx.new_object_initialized("java/io/PipedInputStream", "()V", &[]);
    let out = match tail {
        Ok(Some(Value::Object(Some(tail_ref)))) => {
            // BOTH operands have to survive the `SequenceInputStream`
            // allocation, so pin the tail too before re-reading either.
            let tail_pin = ctx.pin_native_root(tail_ref);
            let head_now = Value::Object(Some(ctx.read_native_pin(pin, head_ref)));
            let tail_now = Value::Object(Some(ctx.read_native_pin(tail_pin, tail_ref)));
            match ctx.new_object_initialized(
                "java/io/SequenceInputStream",
                "(Ljava/io/InputStream;Ljava/io/InputStream;)V",
                &[head_now, tail_now],
            ) {
                Ok(Some(seq @ Value::Object(Some(_)))) => Ok(Some(seq)),
                // Could not wrap — hand back the partial body on its own rather
                // than losing it.
                _ => Ok(Some(Value::Object(Some(
                    ctx.read_native_pin(pin, head_ref),
                )))),
            }
        }
        _ => Ok(Some(Value::Object(Some(
            ctx.read_native_pin(pin, head_ref),
        )))),
    };
    ctx.unpin_native_roots(pin);
    out
}

/// Cached response headers for a real-JDK connection (empty if not performed).
fn huc_real_headers(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<(String, String)> {
    let key = ctx.identity_hash_code(this);
    real_results()
        .lock()
        .ok()
        .and_then(|t| t.get(&key).map(|r| r.headers.clone()))
        .unwrap_or_default()
}

/// Drop all identity-keyed side-table state for a real carrier (on disconnect).
fn real_forget(ctx: &dyn NativeContext, this: ObjectRef) {
    let key = ctx.identity_hash_code(this);
    if let Ok(mut t) = real_results().lock() {
        t.remove(&key);
    }
    if let Ok(mut t) = real_reqs().lock() {
        t.remove(&key);
    }
    if let Ok(mut t) = real_body_streams().lock() {
        t.remove(&key);
    }
    forget_live_fixed_stream(key);
}

// ---------------------------------------------------------------------------
// rustls config — system trust store, no ALPN (legacy HTTP/1.1 only)
// ---------------------------------------------------------------------------

fn shared_legacy_config() -> Arc<ClientConfig> {
    static C: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    C.get_or_init(|| {
        let mut roots = RootCertStore::empty();
        let result = rustls_native_certs::load_native_certs();
        for c in result.certs {
            let _ = roots.add(c);
        }
        let mut cfg = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        Arc::new(cfg)
    })
    .clone()
}

// ---------------------------------------------------------------------------
// Errors / helpers
// ---------------------------------------------------------------------------

fn ioex<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::IOException {
        message: message.into(),
    }
    .into()
}

fn socket_timeout_ex<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::SocketTimeoutException {
        message: message.into(),
    }
    .into()
}

fn iae<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: message.into(),
    }
    .into()
}

fn protocol_ex<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::ProtocolException {
        message: message.into(),
    }
    .into()
}

fn read_str_field(ctx: &dyn NativeContext, obj: ObjectRef, idx: usize) -> Option<String> {
    match ctx.get_field(obj, idx) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

fn read_byte_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(b) = ctx.get_array_element(arr, i) {
            out.push((b & 0xff) as u8);
        }
    }
    out
}

fn new_byte_array(ctx: &mut dyn NativeContext, bytes: &[u8]) -> ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i32));
    }
    arr
}

/// Build a `java/io/ByteArrayInputStream` over `body` (4-field synthetic:
/// buf=0, pos=1, mark=2, count=3).
fn make_byte_array_input_stream(
    ctx: &mut dyn NativeContext,
    body: &[u8],
) -> Result<Value, MethodCallFailed> {
    let body_arr = new_byte_array(ctx, body);
    let stream = try_alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4)?;
    ctx.set_field(stream, 0, Value::Object(Some(body_arr)));
    ctx.set_field(stream, 1, Value::Int(0));
    ctx.set_field(stream, 2, Value::Int(0));
    ctx.set_field(stream, 3, Value::Int(body.len() as i32));
    Ok(Value::Object(Some(stream)))
}

/// Build a real `java.util.Map<String, java.util.List<String>>` from response
/// headers, grouping duplicate header names (case-insensitively, first-seen
/// order) into a per-name `List`. This mirrors what the JDK's
/// `URLConnection.getHeaderFields()` returns and is what `TomcatBaseTest`'s
/// `resHead` out-parameter is filled from. Every `create_string`/`invoke` can
/// allocate and move objects, so the map/list/key refs are pinned and re-read
/// across each re-entrant call (see the manifest-builder idiom in phases_late).
fn build_header_map(
    ctx: &mut dyn NativeContext,
    headers: &[(String, String)],
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for (k, v) in headers {
        if let Some(g) = groups.iter_mut().find(|(gk, _)| gk.eq_ignore_ascii_case(k)) {
            g.1.push(v.clone());
        } else {
            groups.push((k.clone(), vec![v.clone()]));
        }
    }
    let map = match ctx.new_object_initialized("java/util/HashMap", "()V", &[])? {
        Some(Value::Object(Some(o))) => o,
        _ => return Err(ioex("getHeaderFields: could not allocate HashMap")),
    };
    let map_pin = ctx.pin_native_root(map);
    for (k, vals) in &groups {
        let list = match ctx.new_object_initialized("java/util/ArrayList", "()V", &[])? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                ctx.unpin_native_roots(map_pin);
                return Err(ioex("getHeaderFields: could not allocate ArrayList"));
            }
        };
        let list_pin = ctx.pin_native_root(list);
        for v in vals {
            let vs = ctx.create_string(v);
            let vs_pin = ctx.pin_native_root(vs);
            let list = ctx.read_native_pin(list_pin, list);
            let vs = ctx.read_native_pin(vs_pin, vs);
            ctx.invoke(
                "java/util/ArrayList",
                "add",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(list)), Value::Object(Some(vs))],
            )?;
        }
        let ks = ctx.create_string(k);
        let ks_pin = ctx.pin_native_root(ks);
        let map = ctx.read_native_pin(map_pin, map);
        let list = ctx.read_native_pin(list_pin, list);
        let ks = ctx.read_native_pin(ks_pin, ks);
        ctx.invoke(
            "java/util/HashMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(map)),
                Value::Object(Some(ks)),
                Value::Object(Some(list)),
            ],
        )?;
    }
    let map = ctx.read_native_pin(map_pin, map);
    ctx.unpin_native_roots(map_pin);
    Ok(map)
}

#[derive(Debug, Clone)]
struct Url1 {
    scheme: String,
    host: String,
    port: u16,
    path: String,
    /// RFC 3986 user-info (`alice:secret` in `http://alice:secret@host/…`),
    /// stripped from the connect target / Host header. `build_request` turns
    /// it into preemptive `Authorization: Basic` credentials — the real-JDK
    /// `HttpURLConnection` behaviour (from `url.getUserInfo()`) that Spring's
    /// `ResourceTests.useUserInfoToSetBasicAuth` asserts on.
    userinfo: Option<String>,
}

fn is_redirect_status(status: i32) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

fn resolve_redirect_url(base: &Url1, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    let default_port: u16 = if base.scheme == "https" { 443 } else { 80 };
    let prefix = if base.port == default_port {
        format!("{}://{}", base.scheme, base.host)
    } else {
        format!("{}://{}:{}", base.scheme, base.host, base.port)
    };
    if location.starts_with('/') {
        return format!("{prefix}{location}");
    }
    let base_dir = match base.path.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &base.path[..=i],
    };
    format!("{prefix}{base_dir}{location}")
}

fn parse_url(url: &str) -> Result<Url1, String> {
    let (scheme, rest) = if let Some(r) = url.strip_prefix("https://") {
        ("https".to_string(), r)
    } else if let Some(r) = url.strip_prefix("http://") {
        ("http".to_string(), r)
    } else {
        return Err(format!("unsupported scheme in {url}"));
    };
    // A query-only target has no explicit path, but HTTP origin-form still
    // needs `/?query`; fragments never belong in an HTTP request target.
    let (authority, path) = match rest.find(|c| matches!(c, '/' | '?' | '#')) {
        Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
        Some(i) if rest.as_bytes()[i] == b'?' => (&rest[..i], format!("/{}", &rest[i..])),
        Some(i) => (&rest[..i], "/".to_string()),
        None => (rest, "/".to_string()),
    };
    // RFC 3986: authority = [ userinfo "@" ] host [ ":" port ]. Split at the
    // LAST '@' (userinfo may itself contain an encoded/raw '@').
    let (userinfo, authority) = match authority.rfind('@') {
        Some(i) => (Some(authority[..i].to_string()), &authority[i + 1..]),
        None => (None, authority),
    };
    let default_port: u16 = if scheme == "https" { 443 } else { 80 };
    let (host, port) = match authority.rfind(':') {
        Some(idx) if !authority[idx..].contains(']') => {
            let h = &authority[..idx];
            let p_str = &authority[idx + 1..];
            let p = p_str
                .parse::<u16>()
                .map_err(|_| format!("invalid port in {url}"))?;
            (h.to_string(), p)
        }
        _ => (authority.to_string(), default_port),
    };
    if host.is_empty() {
        return Err(format!("empty host in {url}"));
    }
    Ok(Url1 {
        scheme,
        host,
        port,
        path,
        userinfo,
    })
}

fn build_request(
    method: &str,
    parsed: &Url1,
    headers: &[(String, String)],
    body: &[u8],
) -> Vec<u8> {
    let mut out = build_request_head(method, parsed, headers, body.len() as u64, !body.is_empty());
    out.extend_from_slice(body);
    out
}

fn ise<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

/// Build an HTTP/1.1 request head without materialising the body.  The live
/// fixed-length path uses this to put headers on the wire before Java writes
/// its first body byte.
fn build_request_head(
    method: &str,
    parsed: &Url1,
    headers: &[(String, String)],
    body_len: u64,
    has_output: bool,
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(256);
    let _ = write!(&mut out, "{method} {} HTTP/1.1\r\n", parsed.path);
    let default_port: u16 = if parsed.scheme == "https" { 443 } else { 80 };
    if parsed.port == default_port {
        let _ = write!(&mut out, "Host: {}\r\n", parsed.host);
    } else {
        let _ = write!(&mut out, "Host: {}:{}\r\n", parsed.host, parsed.port);
    }
    let mut has_user_agent = false;
    let mut has_connection = false;
    let mut has_content_length = false;
    let mut has_content_type = false;
    let mut has_authorization = false;
    for (k, v) in headers {
        let lk = k.to_ascii_lowercase();
        if lk == "user-agent" {
            has_user_agent = true;
        }
        if lk == "connection" {
            has_connection = true;
        }
        if lk == "content-length" {
            has_content_length = true;
        }
        if lk == "content-type" {
            has_content_type = true;
        }
        if lk == "authorization" {
            has_authorization = true;
        }
        let _ = write!(&mut out, "{k}: {v}\r\n");
    }
    // JDK parity: a URL carrying user-info sends preemptive
    // `Authorization: Basic base64(userinfo)` unless the caller staged an
    // explicit Authorization header (sun.net.www.protocol.http
    // .HttpURLConnection does this from `url.getUserInfo()`; asserted by
    // Spring's ResourceTests.useUserInfoToSetBasicAuth).
    if !has_authorization {
        if let Some(ui) = &parsed.userinfo {
            let b64 =
                String::from_utf8(crate::b64_encode(ui.as_bytes(), 0, false)).unwrap_or_default();
            let _ = write!(&mut out, "Authorization: Basic {b64}\r\n");
        }
    }
    if !has_user_agent {
        out.extend_from_slice(b"User-Agent: Java/CratonVM\r\n");
    }
    // The legacy JDK HttpURLConnection client keeps HTTP/1.1 connections
    // alive by default and sends the explicit compatibility header. Tomcat
    // exposes that choice in its response header set, including when it drops
    // an invalid response header before committing the response.
    if !has_connection {
        out.extend_from_slice(b"Connection: keep-alive\r\n");
    }
    let is_output_method = has_output || matches!(method, "POST" | "PUT" | "PATCH");
    if !has_content_length && is_output_method {
        let _ = write!(&mut out, "Content-Length: {body_len}\r\n");
    }
    // Real-JDK `HttpURLConnection` defaults the request Content-Type to
    // `application/x-www-form-urlencoded` when the application opened an
    // output stream (POST/PUT/PATCH) without setting one. Servers rely on
    // this to parse a form body into request parameters — e.g. Tomcat's
    // `request.getParameter(...)` (TestRestCsrfPreventionFilter2's
    // request-param nonce path returned 403 without it because the body was
    // never parsed as parameters).
    if !has_content_type && is_output_method {
        out.extend_from_slice(b"Content-Type: application/x-www-form-urlencoded\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Sentinel error string returned by [`read_response`] when a socket read
/// exceeds the configured read timeout. `huc_real_perform` recognises it and
/// raises `java.net.SocketTimeoutException` (real-JDK behaviour) rather than
/// folding it into the generic `-1`/IOException path.
const READ_TIMEOUT_SENTINEL: &str = "__cratonvm_read_timeout__";

/// Prefix on an error string returned by [`perform`]'s HTTPS branch when the
/// failure happened during the TLS handshake itself, or the connection closed
/// with zero response bytes immediately after the client's side of a TLS 1.3
/// handshake completed optimistically (see the doc at the `read_response`
/// call in `perform`). `huc_real_perform` recognises this prefix and raises
/// `javax.net.ssl.SSLHandshakeException` (real-JDK/JSSE behaviour) instead of
/// folding it into the generic "-1" contract used for other connection
/// failures (e.g. a malformed-but-present HTTP response).
const TLS_HANDSHAKE_FAILURE_SENTINEL: &str = "__cratonvm_tls_handshake_failure__: ";

/// Prefix on an error string returned by [`perform`] when a caller-installed
/// `HostnameVerifier` REJECTED the peer (returned `false`, threw, or returned
/// a non-boolean). Deliberately distinct from
/// [`TLS_HANDSHAKE_FAILURE_SENTINEL`] for two reasons: the exception type
/// differs (real JSSE's `HttpsClient.checkURLSpoofing` raises
/// `SSLPeerUnverifiedException`, not `SSLHandshakeException`), and
/// `perform_with_retry` must NOT retry this — a verifier's verdict on the same
/// certificate is deterministic, so retrying would just run the app's verifier
/// a second time and fail identically.
const TLS_PEER_UNVERIFIED_SENTINEL: &str = "__cratonvm_tls_peer_unverified__: ";

/// Prefix on an error string returned by [`perform`] when an application
/// `HostnameVerifier` WAS consulted and answered `false`.
///
/// **A separate sentinel from [`TLS_PEER_UNVERIFIED_SENTINEL`] because HotSpot
/// answers it with a different exception CLASS, which is the opposite of what
/// this file assumed.** [`huc_unverified_peer_message`]'s doc used to argue
/// that the three ways to reach a failed endpoint identification "cannot
/// describe the same outcome three different ways" and gave all three one
/// message. MEASURED 2026-08-17 on HotSpot 25.0.3+9-LTS over a live loopback
/// TLS 1.3 handshake (`scratchpad/g31/HvCase.java`, one case per process so a
/// refusal cannot poison the next row), they are three different outcomes:
///
/// ```text
///   case      installed verifier      HotSpot getResponseCode()
///   -----     --------------------    -----------------------------------------
///   true      returns true            200
///   false     returns false           java.io.IOException
///                                       Wrong HTTPS hostname: should be <127.0.0.1>
///   throws    throws ISE              java.lang.RuntimeException
///                                       java.lang.IllegalStateException: verifier exploded
///   none      none installed          javax.net.ssl.SSLHandshakeException
///                                       (certificate_unknown) No subject alternative
///                                       names matching IP address 127.0.0.1 found
/// ```
///
/// The `false` row is the one this sentinel carries, and it is a PLAIN
/// `java.io.IOException` — `sun.net.www.protocol.https.HttpsClient
/// .checkURLSpoofing` ends with `throw new IOException(formatMsg("Wrong HTTPS
/// hostname%s", ...))` (SOURCE-VERIFIED against `$JAVA_HOME/lib/src.zip`),
/// having already closed the socket and invalidated the session. It is NOT an
/// `SSLPeerUnverifiedException`: that type is what `checkURLSpoofing` CATCHES
/// and swallows on its way to consulting the verifier, not what it throws
/// afterwards. Code that catches `IOException` is unaffected either way; code
/// that switches on the type — which is the shape a pinning test has — was
/// being told the peer could not be authenticated when what actually happened
/// is that its own verifier said no.
///
/// The `none` and `throws` rows are NOT served by this sentinel and remain
/// divergent; both are NOMINATED in G31-1 because neither can be fixed in this
/// file (one is a rustls-layer handshake message, the other needs `perform`'s
/// `Result<_, String>` to carry a pending Java exception).
///
/// `perform_with_retry` must not retry this, for the same reason it must not
/// retry [`TLS_PEER_UNVERIFIED_SENTINEL`]: a verifier's verdict on the same
/// certificate is deterministic. It does not, because its retry arm matches two
/// specific strings and this is neither.
const TLS_HOSTNAME_REFUSED_SENTINEL: &str = "__cratonvm_tls_hostname_refused__: ";

/// Prefix on an error string returned by [`perform`] when its TCP connect
/// phase failed with `ConnectionRefused` specifically. `huc_real_perform`
/// recognises this and raises `java.net.ConnectException` (real-JDK
/// behaviour — see the typed `RuntimeError::ConnectException` variant's doc),
/// matching every other native connect path in this codebase (plain
/// `Socket`/`SocketChannel`), instead of folding a refused connection into
/// the generic IOException used for other connect failures (DNS failure,
/// timeout).
const CONNECT_REFUSED_SENTINEL: &str = "__cratonvm_connect_refused__: ";

/// Map a socket-read `io::Error` to an error string, flagging a timeout via
/// [`READ_TIMEOUT_SENTINEL`]. A blocking `read` that hits `SO_RCVTIMEO`
/// surfaces as `WouldBlock` (Unix) or `TimedOut` (Windows).
fn read_io_err(prefix: &str, e: std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
            READ_TIMEOUT_SENTINEL.to_string()
        }
        _ => format!("{prefix}: {e}"),
    }
}

/// Read that tolerates an unclean TLS close: many HTTP servers close the TCP
/// connection at end-of-response without sending a TLS `close_notify`, which
/// rustls surfaces as `UnexpectedEof` ("peer closed connection without sending
/// TLS close_notify"). For an HTTP client that is a normal end-of-stream, so map
/// it to `Ok(0)` (EOF) rather than a hard error.
///
/// `pub(crate)` — also reused by `t27_tls::rustls_stream_read`'s CLIENT-side
/// path (a plain `javax.net.ssl.SSLSocket.getInputStream().read()`, not just
/// this file's own HTTP client bridge, hits the identical "server did
/// Connection: Close without a TLS close_notify" pattern — see
/// `TestSsl.testSni`). Deliberately NOT applied to the server-side read path
/// in that same function: a client that goes silent mid-REQUEST is a more
/// security-relevant truncation than a server closing after a fully-framed
/// response, so server reads keep surfacing the hard error.
pub(crate) fn read_eof_tolerant<S: Read>(stream: &mut S, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        match stream.read(buf) {
            Ok(n) => return Ok(n),
            // EINTR is a transient interruption, not a peer disconnect.
            Err(e)
                if e.kind() == std::io::ErrorKind::Interrupted || e.raw_os_error() == Some(4) =>
            {
                continue
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::UnexpectedEof
                    || e.to_string().contains("close_notify") =>
            {
                return Ok(0);
            }
            Err(e) => return Err(e),
        }
    }
}

fn read_response<S: Read>(
    stream: &mut S,
    head: bool,
) -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
    read_response_with_prefix(stream, head, Vec::new())
}

/// Same as [`read_response`], but starts from `prefix` bytes already read off
/// the wire (e.g. the pooled connection liveness-probe's first read) instead
/// of assuming nothing has been consumed yet.
fn read_response_with_prefix<S: Read>(
    stream: &mut S,
    head: bool,
    prefix: Vec<u8>,
) -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
    // Fresh response: clear any truncation flag left by an earlier one on this
    // thread (e.g. the first leg of a redirect chain).
    LAST_RESPONSE_TRUNCATED.with(|t| t.set(false));
    let mut buf = prefix;
    let mut tmp = [0u8; 8192];
    let head_end = match find_subslice(&buf, b"\r\n\r\n") {
        Some(pos) => pos + 4,
        None => loop {
            let n =
                read_eof_tolerant(stream, &mut tmp).map_err(|e| read_io_err("response read", e))?;
            if n == 0 {
                return Err("connection closed before response head".into());
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                break pos + 4;
            }
            if buf.len() > 64 * 1024 {
                return Err("response head exceeded 64 KiB".into());
            }
        },
    };

    let mut headers_storage = [httparse::EMPTY_HEADER; 64];
    let mut resp = httparse::Response::new(&mut headers_storage);
    let parse_status = resp
        .parse(&buf[..head_end])
        .map_err(|e| format!("httparse: {e}"))?;
    if parse_status.is_partial() {
        return Err("incomplete response head".into());
    }
    let status = resp.code.ok_or("no status code")? as i32;
    LAST_REASON_PHRASE.with(|r| *r.borrow_mut() = resp.reason.unwrap_or("").to_string());
    let mut headers: Vec<(String, String)> = Vec::with_capacity(resp.headers.len());
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for h in resp.headers.iter() {
        let name = h.name.to_string();
        // HTTP header fields are byte-oriented. `HttpURLConnection` exposes
        // those bytes as ISO-8859-1 code points rather than interpreting them
        // as UTF-8. In particular, Tomcat may emit a UTF-8 cookie value; its
        // client test retrieves the raw header bytes with ISO-8859-1 and then
        // decodes them as UTF-8. Requiring UTF-8 here both violates that
        // contract and either rejects or corrupts valid obs-text bytes.
        let value: String = h.value.iter().map(|&byte| char::from(byte)).collect();
        let lname = name.to_ascii_lowercase();
        if lname == "content-length" {
            content_length = value.trim().parse::<usize>().ok();
        }
        if lname == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
        headers.push((name, value));
    }

    // Responses that carry NO message body regardless of their
    // `Content-Length`/`Transfer-Encoding` headers (RFC 9110 §6.4.1):
    //   * any response to a HEAD request,
    //   * 1xx (informational), 204 (No Content), 304 (Not Modified).
    // Return as soon as the head is parsed — otherwise the body loop below
    // blocks. The 304/204/1xx case is the dangerous one: such a response
    // legitimately omits `Content-Length`, so without this guard the
    // no-content-length `else` branch reads "until EOF", which never comes on a
    // keep-alive connection (the server holds it open) -> `getResponseCode`
    // hangs indefinitely instead of returning the status. Surfaced by Tomcat
    // `TestExpiresFilter` (`testExcludedResponseStatusCode`, `testBug63909`),
    // whose conditional-GET / explicit `setStatus(304)` servlets return a
    // bodiless 304 that previously hung the client (`expected:<304> but
    // was:<-1>`).
    let bodiless = head || status == 204 || status == 304 || (100..200).contains(&status);
    if bodiless {
        return Ok((status, headers, Vec::new()));
    }

    let mut body_buf: Vec<u8> = Vec::new();
    if buf.len() > head_end {
        body_buf.extend_from_slice(&buf[head_end..]);
    }
    if chunked {
        let body = read_chunked(&mut body_buf, stream)?;
        return Ok((status, headers, body));
    }
    if let Some(target) = content_length {
        let target = target.min(MAX_RESPONSE_BODY);
        while body_buf.len() < target {
            let n = read_eof_tolerant(stream, &mut tmp).map_err(|e| format!("body read: {e}"))?;
            if n == 0 {
                // Peer closed before delivering the advertised Content-Length.
                // Keep what arrived; the caller surfaces the shortfall as an
                // IOException at the END of the stream, as HotSpot does.
                mark_response_truncated();
                break;
            }
            body_buf.extend_from_slice(&tmp[..n]);
        }
        body_buf.truncate(target);
    } else {
        loop {
            let n = read_eof_tolerant(stream, &mut tmp).map_err(|e| format!("body read: {e}"))?;
            if n == 0 {
                break;
            }
            body_buf.extend_from_slice(&tmp[..n]);
            if body_buf.len() >= MAX_RESPONSE_BODY {
                body_buf.truncate(MAX_RESPONSE_BODY);
                break;
            }
        }
    }

    Ok((status, headers, body_buf))
}

fn read_chunked<S: Read>(prefix: &mut Vec<u8>, stream: &mut S) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let line_end = loop {
            if let Some(pos) = find_subslice(prefix, b"\r\n") {
                break pos;
            }
            let n =
                read_eof_tolerant(stream, &mut tmp).map_err(|e| read_io_err("chunked size", e))?;
            if n == 0 {
                // Connection aborted between chunks. Return the chunks that
                // DID arrive (see `LAST_RESPONSE_TRUNCATED`) instead of
                // discarding the whole body — the caller replays them to the
                // Java reader and then throws, matching HotSpot.
                mark_response_truncated();
                return Ok(out);
            }
            prefix.extend_from_slice(&tmp[..n]);
        };
        let size_line = std::str::from_utf8(&prefix[..line_end])
            .map_err(|e| format!("chunked size utf8: {e}"))?
            .to_string();
        let size_str = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| format!("bad chunk size: {size_line:?}"))?;
        prefix.drain(..line_end + 2);
        if size == 0 {
            return Ok(out);
        }
        while prefix.len() < size + 2 {
            let n =
                read_eof_tolerant(stream, &mut tmp).map_err(|e| read_io_err("chunked body", e))?;
            if n == 0 {
                // Aborted part-way through a chunk: keep the complete chunks
                // already decoded plus whatever of this one arrived.
                mark_response_truncated();
                out.extend_from_slice(&prefix[..prefix.len().min(size)]);
                return Ok(out);
            }
            prefix.extend_from_slice(&tmp[..n]);
        }
        out.extend_from_slice(&prefix[..size]);
        if out.len() > MAX_RESPONSE_BODY {
            return Err("body exceeded MAX_RESPONSE_BODY".into());
        }
        prefix.drain(..size + 2);
    }
}

/// The client-side TLS policy a caller-installed `SSLSocketFactory` would
/// impose on this request: `(enabled cipher suites, enabled protocols)`,
/// each empty when that dimension is unrestricted.
pub(crate) type ClientTlsRestrictions = (Vec<String>, Vec<String>);

/// FIX (tls-handshake-enforcement-gap, doc 21): discover the TLS restrictions
/// the currently-installed default `SSLSocketFactory` applies, by up-calling
/// its real `createSocket(host, port)` in PROBE mode (see
/// `net_phase_e::set_huc_factory_probe_mode`).
///
/// Why a probe rather than using the up-called socket directly: real JSSE
/// runs every `HttpsURLConnection` request over the installed factory's
/// socket, but CratonVM's native `HttpURLConnection` owns its own rustls
/// connection, and only THAT connection has the full client feature set —
/// synchronous Java `KeyManager.chooseClientAlias` consultation for mTLS,
/// the captured per-`SSLContext` trust roots, and the shared TLS-ticket
/// cache. Handing `perform` a socket built by `SSLSocketFactory.createSocket`
/// (which has none of those) silently traded a correct connection for an
/// enforced restriction. Probing gets both: the factory's own Java code runs
/// for real — so `setEnabledCipherSuites`/`setEnabledProtocols` overrides
/// like Tomcat's `TesterSupport.ClientSSLSocketFactory` really are observed —
/// while the single actual connection stays `perform`'s own.
///
/// It also removes the reason the previous up-call had to be narrowed to
/// almost nothing: in probe mode `createSocket` performs NO TCP connect and
/// NO handshake, so the re-entrant `invoke_virtual` can no longer reach the
/// class-loading/vtable-install lock-ordering deadlock that a nested
/// blocking connect once exposed (see this function's history in
/// `tls-ocsp-clientcert-validation-not-enforced-FIXED.md`).
/// The old gate — "only up-call when the factory has a private `ciphers`
/// field holding at least one rustls-mappable suite name" — was both
/// test-helper-specific and, since the factory was never published to
/// `HttpsURLConnection.defaultSSLSocketFactory` at all (see
/// `t27_tls::publish_default_ssl_socket_factory`), unreachable in practice.
///
/// Returns `Ok(None)` — meaning `perform` connects exactly as before — when
/// no factory is installed, when the installed factory is CratonVM's own
/// synthetic placeholder (`SSLContext.getSocketFactory()`'s bare return
/// value, which has no Java code to run), or when the probe came back with
/// no restriction at all.
fn huc_client_tls_restrictions(
    ctx: &mut dyn NativeContext,
    connection: Option<ObjectRef>,
    host: &str,
    port: u16,
) -> Result<Option<ClientTlsRestrictions>, MethodCallFailed> {
    let dbg = crate::nbflags().dbg_tls_auth_ok;
    // FIX (huc-per-connection-ssf-readback): prefer THIS connection's own
    // factory, installed by `HttpsURLConnection.setSSLSocketFactory`, over the
    // process default — the JDK's precedence. Until that setter started
    // storing the factory (see `t27_tls`'s registration) an instance-scoped
    // factory's cipher/protocol restrictions were unreachable here, so an
    // instance `setSSLSocketFactory` silently connected unrestricted while an
    // otherwise identical `setDefaultSSLSocketFactory` was honoured.
    //
    // Read before anything below allocates or runs bytecode: `connection` is
    // the caller's already-pin-refreshed reference, and a moving collection
    // during the probe up-call would strand it.
    let instance_factory =
        connection.and_then(|c| match ctx.get_field_by_name(c, "sslSocketFactory") {
            Value::Object(Some(f)) => Some(f),
            _ => None,
        });
    // The process default lives in a GC-rooted native slot, NOT in the real
    // JDK static field: writing that field does not stick on this VM
    // (measured — see `t27_tls::huc_default_factory_slot`), which silently
    // disabled this whole mechanism.
    let Some(factory) = instance_factory.or_else(crate::t27_tls::huc_default_ssl_socket_factory)
    else {
        if dbg {
            eprintln!("[dbg-tls-auth] huc_client_tls_restrictions: no default factory installed");
        }
        return Ok(None);
    };
    if dbg && instance_factory.is_some() {
        eprintln!(
            "[dbg-tls-auth] huc_client_tls_restrictions: using this connection's own factory"
        );
    }
    // Our own placeholder carrier has no overriding Java bytecode to run, so
    // probing it can only ever come back empty. Compare by `ClassId` rather
    // than by name: `alloc_concurrent_synthetic` documents that
    // `class_name_of_id` can misreport an interface-like synthetic class
    // (`SSLSocketFactory` is abstract) as `java/lang/Object`.
    let placeholder_cid = ctx.class_id_by_name("javax/net/ssl/SSLSocketFactory");
    let factory_cid = ctx.class_id_of_object(factory);
    if dbg {
        eprintln!(
            "[dbg-tls-auth] huc_client_tls_restrictions: factory class={:?} placeholder={}",
            ctx.class_name_of_id(factory_cid),
            placeholder_cid == Some(factory_cid)
        );
    }
    if placeholder_cid == Some(factory_cid) {
        return Ok(None);
    }
    if ctx
        .class_name_of_id(factory_cid)
        .is_none_or(|n| n == "java/lang/Object" || n == "javax/net/ssl/SSLSocketFactory")
    {
        return Ok(None);
    }
    let host_obj = ctx.create_string(host);
    crate::net_phase_e::set_huc_factory_probe_mode(true);
    let created = ctx.invoke_virtual(
        factory,
        "createSocket",
        "(Ljava/lang/String;I)Ljava/net/Socket;",
        &[Value::Object(Some(host_obj)), Value::Int(port as i32)],
    );
    crate::net_phase_e::set_huc_factory_probe_mode(false);
    let socket = match created? {
        Some(Value::Object(Some(s))) => s,
        _ => return Ok(None),
    };
    let Some((ciphers, protocols)) = crate::net_phase_e::take_probe_restrictions(ctx, socket)
    else {
        return Ok(None);
    };
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] huc_client_tls_restrictions host={host} -> ciphers={ciphers:?} \
             protocols={protocols:?}"
        );
    }
    if ciphers.is_empty() && protocols.is_empty() {
        return Ok(None);
    }
    Ok(Some((ciphers, protocols)))
}

// ---------------------------------------------------------------------------
// Plain-HTTP keep-alive connection pool
// ---------------------------------------------------------------------------
//
// bug-h2-httpurlconnection-no-keepalive-pooling-FIXED.md
// — real JDK's `sun.net.www.http.HttpClient` pools/reuses a TCP connection to
// the same `(host, port)` across separate `HttpURLConnection` instances once
// a response is fully drained; `perform` previously always opened a brand
// new `TcpStream` per call. Plain-HTTP only (mirrors the request builder's
// own `Connection: keep-alive` default above) — HTTPS is out of scope, same
// as the reverted first attempt at this feature.
//
// A first implementation attempt (2026-07-22, see the doc above) was
// reverted after a confirmed ~28-30s regression on `org.h2.test.server.
// TestWeb`. Root-caused (this session, via `WebThread`/`WebServer`/`TestWeb`
// source inspection) to: `TestWeb.test()` runs ~8 independent `Server`
// instances *sequentially in one process*, ALL bound to the same fixed port
// (8182) — each one's `finally { server.shutdown(); }` synchronously force-
// closes any still-open kept-alive sockets (`WebServer.stop()` iterates
// `running` `WebThread`s and calls `stopNow()`, i.e. `socket.close()`, on
// each). A `(host,port)`-keyed pool inevitably hands the next test method's
// first request a connection left over from the *previous, now-dead* server
// instance — that is the dominant source of "staleness", not some inherent
// property of the H2 wire protocol. The prior attempt's fix compounded this:
// it used a full non-blocking peek plus a 2s probe-read timeout per stale
// hit, AND (implicitly) could pay that cost once per pooled candidate if the
// freshest one happened to look alive at peek time but still failed later.
//
// This attempt bounds cost differently:
//   * A non-blocking `peek()` before handing out a pooled connection catches
//     the common case — the peer's `socket.close()` sent a FIN well before
//     the next request arrives (test methods do real work in between) — for
//     effectively zero added latency, no timeout wait at all.
//   * The residual race (peek looks alive, but the peer tears down before or
//     during our write/first-read) is bounded by a SHORT, fixed probe
//     timeout (`POOL_PROBE_TIMEOUT`, not the caller's full read timeout,
//     which can be 60s) applied ONLY to the wait for the first response
//     byte. Once real bytes arrive the connection is proven alive and the
//     caller's normal `read_timeout` takes back over for the rest of the
//     body — so a slow-but-alive server response is never misclassified as
//     dead.
//   * On ANY failure using a pooled connection (peek, write, or the bounded
//     first-read), `perform` treats it exactly like a pool miss — silently
//     discards it (and drains the rest of that key's bucket, since one dead
//     connection strongly implies the others from the same server
//     generation are equally dead — see `WebServer.stop()` above) and falls
//     straight through to the ordinary fresh-connect path below. No new
//     error sentinel, no interaction with `perform_with_retry`'s existing
//     retry-on-immediately-closed-fresh-connection logic.
//   * `POOL_IDLE_WINDOW` is a secondary/backstop cleanup (bounds memory and
//     stale-fd lifetime for a key that's never queried again), not the
//     primary staleness defense — the peek+probe above are.
struct PooledConn {
    stream: TcpStream,
    returned_at: Instant,
}

const POOL_MAX_PER_KEY: usize = 4;
const POOL_IDLE_WINDOW: Duration = Duration::from_secs(2);
const POOL_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

fn conn_pool() -> &'static Mutex<HashMap<(String, u16), Vec<PooledConn>>> {
    static P: OnceLock<Mutex<HashMap<(String, u16), Vec<PooledConn>>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Pop the freshest still-plausibly-alive pooled connection for `(host,
/// port)`, or `None` on a miss. Entries are stored oldest-first, so the
/// freshest is the last one; if IT is already past `POOL_IDLE_WINDOW`, every
/// other entry (returned earlier) is too, so the whole bucket is dropped in
/// one step rather than age-checking each entry individually.
fn pool_take(host: &str, port: u16) -> Option<TcpStream> {
    let mut guard = conn_pool().lock().ok()?;
    let key = (host.to_string(), port);
    let bucket = guard.get_mut(&key)?;
    let mut entry = bucket.pop()?;
    if entry.returned_at.elapsed() > POOL_IDLE_WINDOW {
        bucket.clear();
        return None;
    }
    // Non-blocking peek: a peer that already sent FIN/RST shows up as Ok(0)
    // or a hard error here; WouldBlock (nothing pending) is the only signal
    // that means "still looks alive" for an idle keep-alive connection —
    // HTTP request/response is strictly synchronous, so any other outcome
    // (including unexpected already-buffered bytes) is treated as untrusted.
    let _ = entry.stream.set_nonblocking(true);
    let mut probe = [0u8; 1];
    let peek_result = entry.stream.peek(&mut probe);
    let _ = entry.stream.set_nonblocking(false);
    match peek_result {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Some(entry.stream),
        _ => {
            bucket.clear();
            None
        }
    }
}

/// Drop every pooled connection for `(host, port)` — called after a pooled
/// candidate fails post-peek (write or bounded first-read), since that
/// almost always means the whole server generation behind this key is gone.
fn pool_clear(host: &str, port: u16) {
    if let Ok(mut guard) = conn_pool().lock() {
        guard.remove(&(host.to_string(), port));
    }
}

/// Is the idle-connection reaper thread currently alive? See [`pool_put`].
static POOL_REAPER_RUNNING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// How often the reaper sweeps. Well under [`POOL_IDLE_WINDOW`], so a
/// connection is closed within roughly that window of becoming unusable.
const POOL_REAPER_TICK: Duration = Duration::from_millis(400);

/// Close pooled connections that are past [`POOL_IDLE_WINDOW`], and report
/// whether anything is still pooled.
///
/// Dropping a [`PooledConn`] drops its `TcpStream`, which closes the socket.
fn pool_sweep_idle() -> bool {
    let Ok(mut guard) = conn_pool().lock() else {
        return false;
    };
    guard.retain(|_, bucket| {
        bucket.retain(|entry| entry.returned_at.elapsed() <= POOL_IDLE_WINDOW);
        !bucket.is_empty()
    });
    !guard.is_empty()
}

/// Start the idle-connection reaper if it is not already running.
///
/// **Why this exists.** Without it a pooled socket stays open for the entire
/// life of the VM: `pool_take` only discards stale entries when someone asks
/// for that exact `(host, port)` again, so a client that makes ONE request and
/// then stops leaves the connection established forever. The real JDK does not
/// behave that way — `sun.net.www.http.KeepAliveCache` runs a "Keep-Alive-Timer"
/// daemon thread that closes idle connections (measured on HotSpot 25:
/// closed 5.004s after the response was consumed).
///
/// The difference is observable from the *server* side, which is how it
/// surfaced: `MockWebServer.close()` closes its listening socket and then waits
/// up to 5s for each per-connection task to finish, and that task is parked in
/// a read on a connection our client never closed — so teardown failed with
/// `AssertionError: Gave up waiting for queue to shut down` (6 of 21 in
/// `Saml2RelyingPartyAutoConfigurationTests`, whose SAML metadata fetch goes
/// through `UrlResource` → `HttpURLConnection`). Any test or application that
/// waits for its peer to hang up saw the same leak.
///
/// The thread exits once the pool drains, so an idle VM carries no extra
/// thread, and `pool_put` restarts it on the next pooled connection.
fn pool_start_reaper() {
    use std::sync::atomic::Ordering;
    if POOL_REAPER_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("cratonvm-huc-keepalive-reaper".to_string())
        .spawn(|| {
            loop {
                std::thread::sleep(POOL_REAPER_TICK);
                if pool_sweep_idle() {
                    continue;
                }
                // The pool looks empty, so this thread wants to exit. Publish
                // "not running" BEFORE the final check, not after: a `pool_put`
                // that raced us in between would find the latch still set,
                // decline to start a reaper, and leave its connection open for
                // the life of the VM — the exact leak this thread exists to
                // prevent. Having published, re-check; if something did arrive,
                // re-acquire the latch and keep going (or exit, if that racing
                // `pool_put` already started a replacement).
                POOL_REAPER_RUNNING.store(false, Ordering::Release);
                if !pool_sweep_idle() {
                    break;
                }
                if POOL_REAPER_RUNNING
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    break;
                }
            }
        });
    if spawned.is_err() {
        // Could not spawn (thread limit): clear the latch so a later
        // `pool_put` retries rather than leaving the pool unreaped forever.
        POOL_REAPER_RUNNING.store(false, Ordering::Release);
    }
}

fn pool_put(host: &str, port: u16, stream: TcpStream) {
    if let Ok(mut guard) = conn_pool().lock() {
        let bucket = guard.entry((host.to_string(), port)).or_default();
        if bucket.len() >= POOL_MAX_PER_KEY {
            bucket.remove(0);
        }
        bucket.push(PooledConn {
            stream,
            returned_at: Instant::now(),
        });
    }
    pool_start_reaper();
}

/// Whether a just-parsed response leaves the connection in a cleanly
/// reusable state: unambiguous framing (explicit `Content-Length`, or a
/// status/method that RFC 9110 §6.4.1 guarantees carries no body) and no
/// `Connection: close` from the peer. Chunked responses are excluded —
/// narrower, already-validated scope matching the reverted first attempt,
/// not a framing-safety requirement (a fully-consumed chunked body does
/// leave the stream at a clean boundary).
fn is_poolable_response(status: i32, head: bool, headers: &[(String, String)]) -> bool {
    let bodiless = head || status == 204 || status == 304 || (100..200).contains(&status);
    let mut chunked = false;
    let mut has_content_length = false;
    let mut close = false;
    for (k, v) in headers {
        let lk = k.to_ascii_lowercase();
        if lk == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
        if lk == "content-length" {
            has_content_length = true;
        }
        if lk == "connection" && v.to_ascii_lowercase().contains("close") {
            close = true;
        }
    }
    !chunked && !close && (has_content_length || bodiless)
}

/// Send `req` over an already-connected pooled `stream` and read the
/// response, bounding the wait for the first response byte to
/// `POOL_PROBE_TIMEOUT` rather than the caller's full `read_timeout` (see the
/// pool's module doc for why). Once at least one byte has arrived the
/// connection is proven alive and `read_timeout` applies normally for the
/// rest of the response.
fn try_pooled_request(
    stream: &mut TcpStream,
    req: &[u8],
    head: bool,
    read_timeout: Duration,
) -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
    let _ = stream.set_write_timeout(Some(POOL_PROBE_TIMEOUT));
    stream
        .write_all(req)
        .map_err(|e| format!("pooled write: {e}"))?;
    stream.flush().map_err(|e| format!("pooled flush: {e}"))?;
    let _ = stream.set_read_timeout(Some(POOL_PROBE_TIMEOUT));
    let mut probe = [0u8; 4096];
    let n = EintrIo::new(stream)
        .read(&mut probe)
        .map_err(|e| format!("pooled probe read: {e}"))?;
    if n == 0 {
        return Err("pooled connection closed before response head".into());
    }
    let _ = stream.set_read_timeout(Some(read_timeout));
    read_response_with_prefix(stream, head, probe[..n].to_vec())
}

/// The `HostnameVerifier` in force for `connection`: its own instance field
/// first, then the process-wide `HttpsURLConnection.defaultHostnameVerifier`.
/// This mirrors real JSSE's precedence exactly, and reads the same two REAL
/// JDK fields `t27_tls`'s `set{,Default}HostnameVerifier` natives write, so
/// the setter and this call site cannot drift apart.
fn huc_hostname_verifier(
    ctx: &mut dyn NativeContext,
    connection: Option<ObjectRef>,
) -> Option<ObjectRef> {
    if let Some(connection) = connection {
        if let Value::Object(Some(v)) = ctx.get_field_by_name(connection, "hostnameVerifier") {
            return Some(v);
        }
    }
    let cid = ctx.class_id_by_name("javax/net/ssl/HttpsURLConnection")?;
    let idx = ctx.static_field_index_by_name(cid, "defaultHostnameVerifier")?;
    match ctx.get_static_field(cid, idx) {
        Value::Object(Some(v)) => Some(v),
        _ => None,
    }
}

/// Class names that mean "no application `HostnameVerifier` is installed".
///
/// Two shapes reach `huc_verify_hostname`, and BOTH are non-checks:
///
///   * `javax/net/ssl/HostnameVerifier` — this VM's own stand-in, allocated as
///     a bare-interface instance by `t27_tls`'s `get{,Default}HostnameVerifier`
///     when nothing was ever installed (the interface-level `verify` native in
///     that file short-circuits `true` for exactly this shape).
///   * `javax/net/ssl/HttpsURLConnection$DefaultHostnameVerifier` — the REAL
///     JDK's default, installed by `HttpsURLConnection.<clinit>` and copied
///     into every instance's `hostnameVerifier` field by its constructor. Its
///     entire body is `iconst_0; ireturn` — it always answers `false`. That is
///     not a policy: real JSSE recognises it BY NAME (`HttpsClient
///     .afterConnect`, `defaultHVCanonicalName`) and, when it is the one
///     installed, performs endpoint identification inside the handshake
///     (`setEndpointIdentificationAlgorithm("HTTPS")`) and never calls the
///     verifier at all. A VM that instead calls it and reads its `false` as a
///     rejection fails EVERY https request — which is precisely what happened
///     once `f6028ba50` started honouring the stored verifier, and is the
///     defect this predicate exists to prevent.
///
/// **`None` IS NOT ONE OF THEM, AND THAT WAS THE BUG.** This predicate used to
/// fold `None` in, on the reasoning that *"with no identifiable verifier there
/// is nothing to consult, and real JSSE treats a null verifier as the default
/// for the same reason"*. The premise is true of a NULL verifier and false of
/// the `None` this argument actually carries: the caller has already handled a
/// null with its own `let ... else`, so by the time `None` reaches here a
/// verifier object EXISTS and it is only its CLASS NAME that could not be
/// resolved. Those are different facts, and the second one is a statement about
/// this VM, not about the application.
///
/// MEASURED 2026-08-17 against HotSpot 25.0.3+9-LTS on a live TLS 1.3 loopback
/// handshake (`scratchpad/g31/HvFamily.java`, `HvCase.java`, `HvLambda.java`;
/// certificate carries a `localhost` dNSName SAN and deliberately no iPAddress
/// SAN, so the URL `https://127.0.0.1:<port>/` fails the built-in check and the
/// verifier is reached). Two verifiers, same connection shape, same request:
///
/// ```text
///                                     HotSpot      CratonVM (before)
///   named class  HvFamily$Rec          calls=1      calls=1   verifier=Some("HvFamily$Rec")
///   lambda       (h,s) -> true         calls=1      calls=0   verifier=None
/// ```
///
/// A lambda's runtime class is a hidden class; `class_name_of_id` answers
/// `None` for it, this predicate read that as "the JDK's own default is
/// installed", and the request was refused with `SSLPeerUnverifiedException`.
/// Every `HostnameVerifier` written the way applications actually write them —
/// and the one `RSslLiveSession.verifier` installs — took that path.
///
/// The alternative diagnosis, that the per-connection field read simply fails
/// for a lambda, is RULED OUT rather than argued away. `HvLambda decide`
/// installs a NAMED verifier process-wide via `setDefaultHostnameVerifier` AND
/// a lambda on the connection: if the instance read had returned nothing,
/// [`huc_hostname_verifier`]'s static fallback would have found the named one
/// and called it. MEASURED on CratonVM: `namedDefault.calls = 0`. The instance
/// read returned the lambda; only the naming failed.
///
/// So the two entries that remain are the only two that are genuinely "no
/// application verifier is installed", and both are named by a class this VM
/// can always resolve.
fn is_default_hostname_verifier(name: Option<&str>) -> bool {
    matches!(
        name,
        Some("javax/net/ssl/HostnameVerifier")
            | Some("javax/net/ssl/HttpsURLConnection$DefaultHostnameVerifier")
    )
}

/// RFC 2818 §3.1 endpoint identification against the presented leaf — the same
/// check `sun.security.util.HostnameChecker` (`TYPE_TLS`) performs for JSSE,
/// and the FIRST thing `HttpsClient.checkURLSpoofing` does.
///
/// Shares `x509_manager::verify_hostname` with the `X509TrustManager`
/// validation path so the two can never drift into disagreeing about what
/// `localhost` matches.
///
/// This is deliberately re-derived here rather than assumed from the
/// handshake. rustls only performs endpoint identification when it owns the
/// server-certificate policy; when the application supplied Java
/// `TrustManager`s, `t27_tls::PassthroughServerCertVerifier` is installed
/// instead and ignores the server name entirely, with
/// `run_client_trust_check_for_chain` (a plain `checkServerTrusted(chain,
/// authType)`) doing chain policy only. On that path nothing else in the VM
/// checks the hostname, so skipping this would leave a real hole.
fn huc_builtin_endpoint_identification(host: &str, chain: &[Vec<u8>]) -> Result<(), String> {
    let Some(leaf_der) = chain.first() else {
        return Err("no peer certificate was presented".to_string());
    };
    let leaf = crate::x509_manager::parse_certificate(leaf_der)
        .map_err(|e| format!("peer certificate could not be parsed: {e}"))?;
    crate::x509_manager::verify_hostname(&leaf, host).map_err(|e| e.to_string())
}

/// rustls's own spelling of a negotiated suite, translated to the name JSSE
/// reports — which is what `SSLSession.getCipherSuite()` and
/// `HttpsURLConnection.getCipherSuite()` are contracted to return.
///
/// The two agree on every TLS 1.2 suite (both use the IANA registry name, e.g.
/// `TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256`) and disagree on every TLS 1.3 one:
/// rustls's `CipherSuite` enum spells them `TLS13_AES_256_GCM_SHA384`
/// (`rustls/src/enums.rs`, the `enum_builder!` variant names, which is what
/// `format!("{:?}", cs.suite())` prints), while the registry — and therefore
/// JSSE — spells the same suite `TLS_AES_256_GCM_SHA384`. Measured on this host
/// rather than assumed (`scratchpad/c12/C12Probe.java`, HotSpot 25.0.3+9-LTS):
///
/// ```text
/// JSSE supports TLS_AES_256_GCM_SHA384 = true
/// JSSE has any TLS13_* name            = false
/// ```
///
/// TLS 1.3 is this client's default, so without this every `getCipherSuite()`
/// answer on the ordinary path would carry a name no JSSE program has ever seen
/// — and `t27_tls::java_cipher_name_to_suite`, the VM's own reverse mapping,
/// only accepts the JSSE spelling, so the round trip did not close either.
///
/// Prefix-only, deliberately: it is exactly the five `TLS13_*` variants, and a
/// name that does not carry the prefix is already the registry's.
///
/// `pub(crate)` because `t27_tls.rs` has the other producers of a rustls suite
/// name and reaches this rewrite through `suite_to_java_cipher_name`'s
/// fall-through arm — that typed helper is the one entry point both files call,
/// and `suite_to_java_cipher_name_pub` is how THIS file calls it (see the https
/// branch below)
/// — see `docs/known-issues/jdk-only/E3-1-the-cipher-name-helper-and-its-real-denominator.md`.
pub(crate) fn jsse_cipher_suite_name(rustls_name: &str) -> String {
    match rustls_name.strip_prefix("TLS13_") {
        Some(rest) => format!("TLS_{rest}"),
        None => rustls_name.to_string(),
    }
}

/// Endpoint identification for a completed client handshake, run immediately
/// after the `TrustManager` check and before a single request byte is written
/// — the same point, and in the same order, real JSSE runs it.
///
/// SHAPE, and why it is not "additionally run the app's verifier". Real JSSE's
/// `HostnameVerifier` is a FALLBACK, never an extra gate
/// (`sun.net.www.protocol.https.HttpsClient.checkURLSpoofing`): it first runs
/// `HostnameChecker.match(host, peerCert)` and, **if that passes, returns
/// without ever calling the verifier**. Only a FAILED built-in check consults
/// it, and a `true` answer there rescues the connection. So a verifier can
/// only ever WIDEN what is accepted, never narrow it — an app "pinning" with a
/// `HostnameVerifier` on `HttpsURLConnection` is not consulted at all while the
/// name matches, on HotSpot exactly as here.
///
/// The previous shape inverted that: it treated the installed verifier as an
/// additional gate every connection had to pass. Because the real JDK's
/// default verifier is a hardcoded `return false` (see
/// `is_default_hostname_verifier`), that made every https request fail with
/// `SSLPeerUnverifiedException` — across the Spring Boot
/// `SimpleClientHttpRequestFactoryBuilderTests` and seven Tomcat TLS classes.
///
/// FAIL CLOSED, still. Once the built-in check has failed, a verifier that
/// throws — or answers anything that is not boolean `true` — leaves the peer
/// unverified. `perform` returns `Result<_, String>` and cannot carry a
/// pending Java exception, so a throw cannot be propagated verbatim;
/// swallowing it and proceeding would turn a failed identity check into a
/// silent pass. Same choice, and the same reasoning, as
/// `run_client_trust_check_for_chain`'s `map_err` at the TrustManager gate a
/// few lines above the call site.
fn huc_verify_hostname(
    ctx: &mut dyn NativeContext,
    connection: Option<ObjectRef>,
    host: &str,
    port: u16,
    protocol: &str,
    cipher: &str,
    peer_chain_der: Vec<Vec<u8>>,
) -> Result<(), String> {
    // STEP 0 — record the negotiated session against the carrier, BEFORE
    // anything that can return.
    //
    // `HttpsURLConnection.getCipherSuite()` / `getServerCertificates()` /
    // `getLocalCertificates()` / `getPeerPrincipal()` / `getLocalPrincipal()` /
    // `getSSLSession()` (registered in `net_phase_e::register_https_session_accessors`)
    // answer from this table and from nothing else; with no entry they answer
    // `IllegalStateException: connection not yet open`, which is HotSpot's own
    // answer for an unhandshaken connection — a missing ANSWER, never a wrong
    // one. This function is the only place in the VM holding the carrier, the
    // protocol, the cipher suite and the peer chain at the same instant.
    //
    // THE PLACEMENT IS THE WHOLE POINT, not a stylistic choice. STEP 1 below
    // ends in `if builtin.is_ok() { return Ok(()); }`, and that early return is
    // the path EVERY SUCCESSFUL REQUEST TAKES — the endpoint-identification
    // check passing is the normal case. A capture written anywhere after it
    // would record a session only for connections whose built-in name check
    // FAILED: green under any probe that deliberately breaks verification, and
    // dead in production. Do not move this below STEP 1.
    //
    // Recorded even when identification later fails, exactly as the real JDK
    // does: the session exists once the handshake completes, and whether the
    // peer is ACCEPTED is a separate question, answered by this function's
    // `Err` and the exception the caller raises from it. A caller that catches
    // that exception and then asks what was negotiated gets the same answer
    // HotSpot gives.
    if let Some(conn) = connection {
        crate::net_phase_e::record_https_carrier_session(
            ctx,
            conn,
            protocol,
            cipher,
            &peer_chain_der,
            host,
            port,
        );
    }

    // STEP 1 — the built-in check, always first and always on its own.
    let builtin = huc_builtin_endpoint_identification(host, &peer_chain_der);
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] huc_verify_hostname host={host:?} chain_len={} builtin={:?}",
            peer_chain_der.len(),
            builtin
        );
    }
    if builtin.is_ok() {
        return Ok(());
    }

    // STEP 2 — the built-in check failed. Consult the installed verifier, if
    // one that is not a JDK/VM default stand-in was actually installed.
    let verifier0 = huc_hostname_verifier(ctx, connection);
    let verifier_class = match verifier0 {
        Some(v) => {
            let cid = ctx.class_id_of_object(v);
            ctx.class_name_of_id(cid)
        }
        None => None,
    };
    if crate::nbflags().dbg_tls_auth_ok {
        // The two `None`s are printed DIFFERENTLY, and that is not cosmetic.
        // This line used to render `verifier_class` alone, so "no verifier
        // object was found" and "a verifier object was found whose class this
        // VM cannot name" both printed `verifier=None` — and the second is the
        // lambda defect [`is_default_hostname_verifier`] documents. A debug
        // line that cannot separate a missing thing from an unnameable one is
        // how that defect stayed hidden behind a trace that was already on.
        let shown = match (verifier0, verifier_class.as_deref()) {
            (None, _) => "<none installed>".to_string(),
            (Some(_), Some(n)) => n.to_string(),
            (Some(_), None) => "<installed, class name unresolvable>".to_string(),
        };
        eprintln!("[dbg-tls-auth] huc_verify_hostname verifier={shown}");
    }
    let Some(verifier0) = verifier0 else {
        return Err(huc_unverified_peer_message(host));
    };
    if is_default_hostname_verifier(verifier_class.as_deref()) {
        return Err(huc_unverified_peer_message(host));
    }

    // GC: `verifier0` is a raw ObjectRef that must survive four allocations
    // and then an arbitrary-Java call, and each allocated argument must
    // survive the allocations that follow it. Pin every one and re-read the
    // forwarded address before each use. A single `unpin_native_roots` of the
    // FIRST base releases the whole batch (it truncates the pin stack), so
    // every exit below goes through it.
    let verifier_pin = ctx.pin_native_root(verifier0);
    let host_s0 = ctx.create_string(host);
    let host_pin = ctx.pin_native_root(host_s0);

    // G44 N1 — HAND THE VERIFIER **THE** SESSION, NOT A SECOND ONE.
    //
    // MEASURED, `RSslLiveSession` on `9ae371468`:
    //
    // ```text
    // CK RSslLiveSession verifier.sameObjectAsGetSSLSession = false  WANT true
    // ```
    //
    // HotSpot hands `HostnameVerifier.verify` the same `SSLSession` object
    // that `HttpsURLConnection.getSSLSession()` returns afterwards. This call
    // site used to mint its OWN — the same four `set_field` calls that
    // `net_phase_e::https_session_object` makes, deliberately sharing
    // `HTTPS_CLIENT_SESSION_MARKER` so the two could not drift — and two
    // minters cannot produce one object however identical their writes are.
    //
    // `https_carrier_session_object` is that one minter behind its one
    // per-carrier cache. STEP 0 above ran `record_https_carrier_session` on
    // this very connection BEFORE the built-in check's early return, so by the
    // time control reaches here the entry always exists and this is the fast
    // path; the local mint below survives only for the two states in which it
    // does not:
    //
    //   * `None`      — no carrier at all (`connection` is `None`, which is how
    //                   `perform` calls this for a request with no
    //                   `HttpsURLConnection` object behind it), or no recorded
    //                   handshake. NOT an error; see the exposed function's
    //                   own doc.
    //   * `Some(Err)` — the `javax/net/ssl/SSLSession` allocation was refused.
    //
    // Keeping the fallback rather than propagating is deliberate: an installed
    // verifier that is not consulted is a SECURITY change, and this lane is
    // fixing an identity row, not the decision the verifier makes.
    let carrier_session =
        match connection.and_then(|c| crate::net_phase_e::https_carrier_session_object(ctx, c)) {
            Some(Ok(session)) => Some(session),
            _ => None,
        };
    let session0 = match carrier_session {
        Some(session) => session,
        None => {
            match huc_mint_verifier_session(ctx, protocol, cipher, peer_chain_der, host, port) {
                Ok(session) => session,
                // The pins taken above are released on THIS exit too. The
                // `map_err(..)?` this replaces returned straight out of the
                // function with `verifier_pin` and `host_pin` still on the pin
                // stack — a leak on the one path that already had nothing to
                // hand back.
                Err(message) => {
                    ctx.unpin_native_roots(verifier_pin);
                    return Err(message);
                }
            }
        }
    };
    let session_pin = ctx.pin_native_root(session0);

    let verifier = ctx.read_native_pin(verifier_pin, verifier0);
    let host_s = ctx.read_native_pin(host_pin, host_s0);
    let session = ctx.read_native_pin(session_pin, session0);
    let outcome = ctx.invoke_virtual(
        verifier,
        "verify",
        "(Ljava/lang/String;Ljavax/net/ssl/SSLSession;)Z",
        &[Value::Object(Some(host_s)), Value::Object(Some(session))],
    );
    ctx.unpin_native_roots(verifier_pin);

    match outcome {
        Ok(Some(v)) if v.as_int().unwrap_or(0) != 0 => Ok(()),
        // The verifier was consulted and DECLINED. MEASURED on HotSpot: a plain
        // `java.io.IOException` carrying `Wrong HTTPS hostname: should be
        // <host>` — a different class and a different sentence from the
        // "nothing was installed" exit below, which this arm used to share. See
        // [`TLS_HOSTNAME_REFUSED_SENTINEL`] for the transcript.
        Ok(_) => Err(huc_verifier_declined_message(host)),
        Err(_) => Err(format!(
            "{TLS_PEER_UNVERIFIED_SENTINEL}the installed HostnameVerifier threw while \
             verifying <{host}>; treating the peer as unverified"
        )),
    }
}

/// The private `SSLSession` [`huc_verify_hostname`] used to mint on EVERY
/// verified connection, kept as the fallback for the two states in which the
/// one per-carrier session is not available (see the G44 N1 comment at that
/// call site).
///
/// The four writes are unchanged and deliberately still the same four
/// `net_phase_e::https_session_object` makes, sharing
/// [`net_phase_e::HTTPS_CLIENT_SESSION_MARKER`] so the two shapes cannot drift.
/// What changed is how OFTEN this runs: it is now the exception rather than the
/// rule, and a session minted here is by construction NOT the one
/// `getSSLSession()` will answer with — which is exactly the row N1 closes, and
/// is why every state that can reach the carrier's session must reach it
/// instead of coming here.
///
/// Slot 2 must not carry the "never negotiated" sentinel `-1`: this session
/// comes from the far side of a handshake that COMPLETED — `verify()` decides
/// whether to ACCEPT the peer, a separate question from whether anything was
/// negotiated — and `t27_tls::session_has_negotiated` reads exactly this slot,
/// so a verifier that asks `session.isValid()` or `session.getId()` would
/// otherwise be told the handshake it was invoked to vet had not happened.
///
/// GC: every allocated value has to survive the allocations that follow it, so
/// each is pinned and re-read. The batch is released from THIS function's own
/// base before returning, which truncates only the pins taken here — the
/// caller's `verifier_pin`/`host_pin` sit below it and are untouched. There is
/// no allocation between the final `read_native_pin` and the `return`, so the
/// address handed back is current.
fn huc_mint_verifier_session(
    ctx: &mut dyn NativeContext,
    protocol: &str,
    cipher: &str,
    peer_chain_der: Vec<Vec<u8>>,
    peer_host: &str,
    peer_port: u16,
) -> Result<ObjectRef, String> {
    let proto_s0 = ctx.create_string(protocol);
    let proto_pin = ctx.pin_native_root(proto_s0);
    let cipher_s0 = ctx.create_string(cipher);
    let cipher_pin = ctx.pin_native_root(cipher_s0);
    // The client `SSLSession` shape (`new13_alloc_ssl_session`'s — 4 fields
    // since E42; the width is `NEW13_SSL_SESS_FIELDS` and must stay that
    // constant, because `t27_tls`'s slot rules are keyed on it), so the
    // layout-aware real-mode accessors in `t27_tls::register_ssl_session_real`
    // read it correctly: `getProtocol`/`getCipherSuite` from these slots,
    // `getPeerCertificates` from the side table populated just below. A
    // pinning verifier calls exactly that pair.
    let session0 = match try_alloc_concurrent_synthetic(
        ctx,
        "javax/net/ssl/SSLSession",
        crate::phases_late::ssl_security::NEW13_SSL_SESS_FIELDS,
    ) {
        Ok(session) => session,
        Err(_) => {
            ctx.unpin_native_roots(proto_pin);
            return Err("--jdk-only refused javax/net/ssl/SSLSession".to_string());
        }
    };
    let session_pin = ctx.pin_native_root(session0);

    let proto_s = ctx.read_native_pin(proto_pin, proto_s0);
    let session = ctx.read_native_pin(session_pin, session0);
    ctx.set_field(
        session,
        crate::phases_late::ssl_security::NEW13_SESS_PROTO,
        Value::Object(Some(proto_s)),
    );
    let cipher_s = ctx.read_native_pin(cipher_pin, cipher_s0);
    let session = ctx.read_native_pin(session_pin, session0);
    ctx.set_field(
        session,
        crate::phases_late::ssl_security::NEW13_SESS_CIPHER,
        Value::Object(Some(cipher_s)),
    );
    ctx.set_field(
        session,
        crate::phases_late::ssl_security::NEW13_SESS_TLSID,
        Value::Int(crate::net_phase_e::HTTPS_CLIENT_SESSION_MARKER),
    );
    let session = ctx.read_native_pin(session_pin, session0);
    crate::t27_tls::record_client_peer_chain(ctx, session, peer_chain_der);
    // G51-1 N1, the fallback half. This session is by construction NOT the one
    // `getSSLSession()` answers with, so no row on `RSslLiveSession` reaches
    // it — but a verifier that asks the session it was handed where the peer
    // is must not get `null`/`-1` just because the carrier was unavailable.
    // Same source as the carrier path: the host and port the URL NAMED.
    let session = ctx.read_native_pin(session_pin, session0);
    crate::t27_tls::record_session_peer_endpoint(ctx, session, peer_host, i32::from(peer_port));
    let session = ctx.read_native_pin(session_pin, session0);
    ctx.unpin_native_roots(proto_pin);
    Ok(session)
}

/// HotSpot's refusal when an application `HostnameVerifier` was consulted and
/// answered `false`, transcribed rather than composed.
///
/// The wording is `HttpsClient.checkURLSpoofing`'s own — `formatMsg("Wrong
/// HTTPS hostname%s", filterNonSocketInfo(url.getHost()).prefixWith(": should
/// be <").suffixWith(">"))`, which renders as
/// `Wrong HTTPS hostname: should be <127.0.0.1>` (MEASURED on this host, and
/// the source line is in `$JAVA_HOME/lib/src.zip`). Note it names ONLY the
/// host: no certificate, no subject alternative names, no mention of the
/// verifier. That is the whole message, and the difference from
/// [`huc_unverified_peer_message`] is deliberate on HotSpot's part — one says
/// the certificate did not match, the other says the application refused it.
fn huc_verifier_declined_message(host: &str) -> String {
    format!("{TLS_HOSTNAME_REFUSED_SENTINEL}Wrong HTTPS hostname: should be <{host}>")
}

/// The rejection message for a failed endpoint identification with NO
/// application verifier to fall back on — either none was installed, or the one
/// installed is a JDK/VM default stand-in.
///
/// **It is no longer shared with the "an app verifier declined" exit, and the
/// note that used to justify sharing it was wrong.** That note said the three
/// ways to reach a failed identification "cannot describe the same outcome
/// three different ways". MEASURED (see [`TLS_HOSTNAME_REFUSED_SENTINEL`]),
/// HotSpot describes them as three DIFFERENT outcomes with three different
/// exception classes, because they are three different facts: the certificate
/// did not match; the application refused it; the application's verifier blew
/// up. Collapsing them was a decision about tidiness taken where a measurement
/// was available and had not been made.
///
/// This exit's own HotSpot answer is still divergent and deliberately left so:
/// on HotSpot nothing reaches here at all, because with only the default
/// verifier installed JSSE performs endpoint identification INSIDE the
/// handshake (`setEndpointIdentificationAlgorithm("HTTPS")`) and the connection
/// fails as `SSLHandshakeException: (certificate_unknown) No subject
/// alternative names matching IP address 127.0.0.1 found`. CratonVM cannot
/// raise that here — it is a rustls-layer handshake abort, and this VM
/// deliberately re-derives endpoint identification AFTER the handshake because
/// rustls skips it whenever the application supplied Java `TrustManager`s (see
/// [`huc_builtin_endpoint_identification`]). NOMINATED in G31-1 against the TLS
/// layer rather than papered over here: the message can be copied, the
/// TIMING — failing before any application code sees a connected socket —
/// cannot.
fn huc_unverified_peer_message(host: &str) -> String {
    format!(
        "{TLS_PEER_UNVERIFIED_SENTINEL}Certificate for <{host}> does not match any of the \
         subject alternative names or the common name: HTTPS hostname wrong, should be <{host}>"
    )
}

/// The part of an https exchange that happens once the TLS handshake — and
/// every Java-facing gate hanging off it — is complete: write the request,
/// read the response, drain any post-handshake control records.
///
/// Extracted from [`perform`] so its caller can run the whole thing inside one
/// `begin_blocking_region()`/`end_blocking_region()` bracket without an early
/// `return` ever escaping between the two (a thread that leaves a blocking
/// region unclosed stays permanently marked GC-blocked — the mirror image of
/// the deadlock the bracket exists to prevent; the `SSLSocketOutputStream`
/// drain loop in `phases_late/ssl_security.rs` carries the same warning).
/// Nothing here touches the Java heap or takes a `NativeContext`, which is
/// what makes parking across it sound — see the STW-TAKEOVER-FIX note at the
/// call site.
fn https_post_handshake_exchange(
    stream: &mut StreamOwned<ClientConnection, TcpStream>,
    req: &[u8],
    head: bool,
) -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
    // FIX (tls-handshake-enforcement-gap, doc 21): a TLS 1.3 client finishes
    // its own side of the handshake before the server has accepted it, so a
    // server that rejects (e.g. a REQUIRED client certificate that was not
    // presented) sends its alert and closes while we are already past
    // `is_handshaking()`. That surfaces here as a failing FIRST write/flush —
    // before a single request byte has been acknowledged, let alone a response
    // byte read — and was reported as a bare `IOException`. Real JSSE raises
    // `SSLHandshakeException`; `TestSslHandshakeFailure
    // .testMissingClientCertificate` asserts exactly that type. Only this
    // first write is reclassified: any later write failure happens on a
    // connection the server already accepted and is a genuine transport error.
    stream.write_all(req).map_err(|e| {
        format!(
            "{TLS_HANDSHAKE_FAILURE_SENTINEL}connection failed immediately after the \
             TLS handshake, before the request could be sent — the peer likely \
             rejected the handshake: write: {e}"
        )
    })?;
    retry_eintr(|| stream.flush()).map_err(|e| {
        format!(
            "{TLS_HANDSHAKE_FAILURE_SENTINEL}connection failed immediately after the \
             TLS handshake, before the request could be sent — the peer likely \
             rejected the handshake: flush: {e}"
        )
    })?;
    // A TLS 1.3 client considers ITS side of the handshake finished (and so
    // `is_handshaking()` above already flipped false) as soon as it has sent
    // its own Finished — the server can still reject afterwards (e.g. a
    // required-but-missing client certificate: `NoCertificatesPresented`)
    // and close the connection without ever sending an HTTP response. Real
    // JSSE surfaces that as `SSLHandshakeException`/`SSLException`, not a
    // silent empty/malformed response, so a connection that closes before
    // a single response byte arrives — immediately after our own optimistic
    // handshake completion — is classified the same way here rather than
    // falling through to `huc_real_perform`'s generic "-1" contract (which
    // is correct for a genuinely malformed-but-present HTTP response, not
    // for zero bytes at all).
    let response = read_response(stream, head).map_err(|e| {
        if e == "connection closed before response head" {
            format!(
                "{TLS_HANDSHAKE_FAILURE_SENTINEL}connection closed immediately after the \
                 TLS handshake with no response — the peer likely rejected the handshake \
                 (e.g. a required client certificate was not presented): {e}"
            )
        } else if e.contains("received fatal alert") {
            // FIX (tls-handshake-enforcement-gap, doc 21): the peer rejected
            // the connection with a TLS alert instead of closing silently —
            // e.g. `CertificateRequired` from a
            // `certificateVerification="required"` connector when the client
            // presented none (`TestSslHandshakeFailure
            // .testMissingClientCertificate`). No response byte has been read
            // at this point, so this is a rejected handshake, not a mid-stream
            // transport error, and real JSSE raises `SSLHandshakeException`
            // for it. Without this it fell through to the generic
            // `IOException` wrapper — the exact type mismatch that test
            // asserts on.
            format!("{TLS_HANDSHAKE_FAILURE_SENTINEL}{e}")
        } else {
            e
        }
    })?;
    // TLS 1.3 tickets are post-handshake messages. The response body may
    // finish before the server's NewSessionTicket has been read; consume any
    // immediately available control records so the shared ClientConfig retains
    // the ticket for the next URL connection.
    let old_timeout = stream.sock.read_timeout().ok().flatten();
    let _ = stream
        .sock
        .set_read_timeout(Some(Duration::from_millis(100)));
    let mut drain_err: Option<String> = None;
    while stream.conn.wants_read() {
        match stream.conn.read_tls(&mut EintrIo::new(&mut stream.sock)) {
            Ok(0) => break,
            Ok(_) => {
                if let Err(e) = stream.conn.process_new_packets() {
                    drain_err = Some(format!(
                        "{TLS_HANDSHAKE_FAILURE_SENTINEL}post-handshake TLS: {e}"
                    ));
                    break;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                break;
            }
            Err(e) => {
                drain_err = Some(format!("post-handshake TLS read: {e}"));
                break;
            }
        }
    }
    let _ = stream.sock.set_read_timeout(old_timeout);
    match drain_err {
        Some(e) => Err(e),
        None => Ok(response),
    }
}

fn perform(
    ctx: &mut dyn NativeContext,
    connection: Option<ObjectRef>,
    parsed: &Url1,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
    connect_timeout: Duration,
    read_timeout: Duration,
    tls_restrictions: Option<&ClientTlsRestrictions>,
) -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
    let head = method.eq_ignore_ascii_case("HEAD");
    let req = build_request(method, parsed, headers, body);

    // Plain-HTTP keep-alive pool (see the module doc above `perform`): try a
    // pooled connection before ever touching the network. Any failure here —
    // peek, write, or the bounded first-read — is treated exactly like a
    // pool miss, falling straight through to the ordinary fresh-connect path
    // below with no error surfaced to the caller.
    let poolable_key = (parsed.scheme == "http").then(|| (parsed.host.clone(), parsed.port));
    if let Some((host, port)) = &poolable_key {
        if let Some(mut pooled) = pool_take(host, *port) {
            ctx.begin_blocking_region();
            let attempt = try_pooled_request(&mut pooled, &req, head, read_timeout);
            ctx.end_blocking_region();
            match attempt {
                Ok((status, resp_headers, resp_body)) => {
                    if is_poolable_response(status, head, &resp_headers) {
                        pool_put(host, *port, pooled);
                    }
                    return Ok((status, resp_headers, resp_body));
                }
                Err(_) => pool_clear(host, *port),
            }
        }
    }

    let addr = format!("{}:{}", parsed.host, parsed.port);
    let mut last_err: Option<String> = None;
    let mut tcp: Option<TcpStream> = None;
    // `normalize_connect_addr` folds an IPv4-mapped destination
    // (`::ffff:a.b.c.d`) to plain IPv4. A URL carries its host as TEXT, so this
    // path never passes through `InetAddress` — which is where real JDK, and
    // CratonVM's own mirror of it, collapses that literal to an
    // `Inet4Address`. Without the fold we build an AF_INET6 socket, and on
    // Windows `IPV6_V6ONLY` defaults to 1, so `connect` cannot reach a mapped
    // destination: `TestStartupIPv6Connectors.testIPv6MappedIPv4` reported
    // exactly "connect [::ffff:127.0.0.1]:<port>: ... (os error 10049)" from
    // the `last_err` line below. The fold also makes the IPv4-first sort do
    // what it says: a mapped address is a v4 destination wearing a v6
    // sockaddr, so it used to sort LAST despite being the loopback we want
    // tried first.
    let mut addrs: Vec<std::net::SocketAddr> =
        std::net::ToSocketAddrs::to_socket_addrs(&addr.as_str())
            .map_err(|e| format!("resolve {addr}: {e}"))?
            .map(cratonvm_native_io::outbound_policy::normalize_connect_addr)
            .collect();
    // preferIPv4Stack semantics: try IPv4 candidates before IPv6. On Windows a
    // "localhost" lookup returns `[::1, 127.0.0.1]` (IPv6 first), but an
    // embedded/test Tomcat started with -Djava.net.preferIPv4Stack=true listens
    // on 127.0.0.1 only — so the `[::1]:port` attempt is silently dropped and
    // `connect_timeout`'s select() stalls ~1s per request before falling back to
    // IPv4. Re-ordering IPv4 first makes the common loopback case connect
    // immediately while still trying IPv6 for genuinely IPv6-only hosts. A
    // literal IP resolves to a single candidate, so the sort is a no-op. Mirrors
    // native-api fd_table::connect_prefer_ipv4 and the merged WS-connect fix.
    addrs.sort_by_key(|sa| u8::from(sa.is_ipv6()));
    // Blocking region: pure OS-level TCP connect, no Java interaction at all —
    // safe to mark this thread GC-parked for however long it takes.
    let mut last_err_refused = false;
    ctx.begin_blocking_region();
    for sa in addrs {
        match TcpStream::connect_timeout(&sa, connect_timeout) {
            Ok(s) => {
                tcp = Some(s);
                break;
            }
            Err(e) => {
                last_err_refused = e.kind() == std::io::ErrorKind::ConnectionRefused;
                last_err = Some(format!("connect {sa}: {e}"));
            }
        }
    }
    ctx.end_blocking_region();
    let tcp = match tcp {
        Some(t) => t,
        None => {
            let msg =
                last_err.unwrap_or_else(|| format!("could not resolve any address for {addr}"));
            return Err(if last_err_refused {
                format!("{CONNECT_REFUSED_SENTINEL}{msg}")
            } else {
                msg
            });
        }
    };
    let _ = tcp.set_read_timeout(Some(read_timeout));
    let _ = tcp.set_write_timeout(Some(read_timeout));
    let _ = tcp.set_nodelay(true);

    if parsed.scheme == "https" {
        // Prewarm the classes `JavaKeyManagerResolver` allocates
        // (`X500Principal`/`Principal`/`String`) here, in this normal
        // top-level native-call context. If any of these is genuinely
        // loaded/linked for the first time from deep inside the nested
        // reflective-invocation + rustls callback context the resolver
        // actually runs in (JUnit's `Method.invoke` -> ... -> `resolve()` ->
        // `ensure_class_initialized`), defining it there can self-deadlock:
        // reproduced via gdb — the interpreter's class-manager vtable-install
        // write lock blocks in `lock_exclusive_slow` with no other thread
        // holding it, i.e. this same thread re-entering class definition
        // (to link a not-yet-loaded supertype/interface while installing the
        // outer class's vtable) before releasing that lock. Touching these
        // classes here — an ordinary, already-exercised call shape elsewhere
        // in this codebase — sidesteps the hazard entirely regardless of its
        // exact internal mechanism: by the time the resolver runs, they're
        // already defined, so `ensure_class_initialized`/`alloc_concurrent_
        // synthetic` are cache hits, no fresh class definition needed. Also
        // force each array TYPE (`[Ljava/security/Principal;` etc.) to be
        // defined here — a Java array type is its own `Class` object,
        // defined separately from its component type, and `new_ref_array`
        // (which the resolver also calls, for the `Principal[]`/`String[]`
        // arguments to `chooseClientAlias`) would otherwise trigger that
        // definition for the first time from the same risky nested context.
        for cls in [
            "javax/security/auth/x500/X500Principal",
            "java/security/Principal",
            "java/lang/String",
        ] {
            if let Ok(cid) = ctx.ensure_class_initialized(cls) {
                let _ = ctx.new_ref_array(cid, 0);
            }
        }
        // Reuse the ClientConfig captured from SSLContext.getSocketFactory().
        // It contains the configured trust roots/client identity and owns the
        // TLS ticket cache required for a following connection to resume.
        // If no custom SSLContext was captured, use cached system roots.
        let cfg = connection
            .and_then(|connection| {
                crate::t27_tls::huc_client_config_for_connection(ctx, connection)
            })
            .or_else(crate::t27_tls::huc_default_client_config)
            .unwrap_or_else(shared_legacy_config);
        // FIX (tls-handshake-enforcement-gap, doc 21): apply the cipher/
        // protocol policy the installed `SSLSocketFactory` imposes (probed in
        // `huc_client_tls_restrictions`). Rebuilt from the SAME captured
        // ingredients the cached config was built from — client identity,
        // `KeyManager` context key, trust-manager context key — so narrowing
        // the handshake never costs mTLS or custom-trust behaviour. Only the
        // TLS-ticket cache is given up (a fresh `ClientConfig` owns a fresh
        // session store), which is why this is done ONLY when a restriction
        // is actually in force.
        let cfg = match tls_restrictions {
            Some((ciphers, protocols)) => {
                match crate::t27_tls::build_engine_client_config_with_identity_ciphers(
                    &["http/1.1"],
                    crate::t27_tls::huc_default_client_identity()
                        .as_ref()
                        .map(|(c, k)| (c.as_str(), k.as_str())),
                    crate::t27_tls::huc_default_key_managers_ctx_key(),
                    crate::t27_tls::huc_default_trust_managers_ctx_key(),
                    ciphers,
                    protocols,
                ) {
                    Ok(restricted) => restricted,
                    // Falling back to the unrestricted config here silently
                    // turns "this handshake must fail" into "this handshake
                    // succeeded", so make the reason visible rather than
                    // leaving a mystery pass.
                    Err(e) => {
                        if crate::nbflags().dbg_tls_auth_ok {
                            eprintln!(
                                "[dbg-tls-auth] perform: restricted client config FAILED \
                                 (ciphers={ciphers:?} protocols={protocols:?}): {e} — falling \
                                 back to the unrestricted config"
                            );
                        }
                        cfg
                    }
                }
            }
            None => cfg,
        };
        // Match JSSE's SNI policy for this host — see
        // `t27_tls::jsse_would_send_sni`.
        let cfg = crate::t27_tls::client_config_for_host(cfg, &parsed.host);
        let server_name = ServerName::try_from(parsed.host.clone())
            .map_err(|e| format!("bad server name {}: {e}", parsed.host))?;
        let conn = ClientConnection::new(cfg, server_name)
            .map_err(|e| format!("rustls ClientConnection::new: {e}"))?;
        let mut stream: StreamOwned<ClientConnection, TcpStream> = StreamOwned::new(conn, tcp);
        let deadline = std::time::Instant::now() + HANDSHAKE_TIMEOUT;
        // STW-TAKEOVER-FIX (2026-08-01, doc
        // `tomcat/testsslhostconfigcompat-testhostec-read-timeout`): EVERY
        // blocking socket wait below is bracketed by
        // `begin_blocking_region()`/`end_blocking_region{,_refs}()`, and the
        // active native-context window is closed as soon as the handshake is
        // over. Both corrections replace a premise this block used to assert
        // and that is measurably false.
        //
        // What this used to say: the whole https exchange ran with NO blocking
        // region at all, reasoning that "a loopback exchange is fast, so a GC
        // during these few milliseconds simply waits for this thread, like any
        // other ordinary native call". It is not fast when the peer is another
        // thread in THIS SAME VM — an embedded Tomcat under test is exactly
        // that. A stop-the-world cross-thread JIT takeover requested by any
        // other thread parks every mutator except this one; this one is parked
        // in `recv()` and so can never reach a safepoint, can never be forcibly
        // taken over (it is not in JIT code either), and the server thread that
        // owes us the next TLS flight — or the HTTP response — is itself parked
        // at that same barrier. Neither side can move until the socket's
        // `SO_RCVTIMEO` fires, and `TomcatBaseTest` sets that to 300 s.
        // Reproduced as `TestSSLHostConfigCompat` failing ~40% of runs with a
        // 300 440 ms `testHostEC[JSSE-KEYSTORE]` and exactly one stderr line
        // `STW cross-thread JIT takeover is still waiting for cooperative
        // mutators rounds=64 pending=1 taken=0`. The plain-HTTP branch at the
        // bottom of this same function already carried this fix, with this
        // rationale, since 2026-07-14; only the https branch was exempted.
        //
        // Why bracketing is sound here when an earlier attempt deadlocked: the
        // regions cover ONLY socket syscalls — `read_tls`/`write_tls` during
        // the handshake, and the request write plus `read_response` after it.
        // They never cover `process_new_packets()`, which is the one place
        // rustls can call back into Java (`JavaKeyManagerResolver::resolve` ->
        // `KeyManager.chooseClientAlias`/`getPrivateKey`). Running bytecode
        // while marked GC-parked is precisely what `set_active_native_context`'s
        // doc forbids, and the earlier attempt that deadlocked the
        // class-loading/vtable-install locks had put the region around
        // `process_new_packets` itself — i.e. exactly backwards.
        //
        // Why the active-context window can close after the handshake: the old
        // comment held it open across `read_response` for a server-triggered
        // mid-connection renegotiation (Tomcat's `SSLAuthenticator` learns a
        // request needs `CLIENT-CERT` only after parsing the request line).
        // rustls 0.23 categorically REFUSES renegotiation on both sides — a
        // post-handshake `HelloRequest` is answered with a `no_renegotiation`
        // alert and never processed (`rustls/src/common_state.rs::process_msg`;
        // root-caused from the dependency's own source in `
        // tls-ocsp-clientcert-validation-not-enforced-FIXED.md`,
        // "Residual #2 follow-up"). So no Java callback can fire from inside
        // `read_response`; keeping the window open there would buy nothing and
        // would be the one thing that makes the post-handshake region unsound.
        //
        // `connection` is an `ObjectRef` still used after the handshake (by
        // `huc_verify_hostname`), so it rides through `end_blocking_region_refs`
        // to pick up any relocation from a moving GC that ran while parked —
        // same pattern as `phases_late::net_channels`' blocking `SocketChannel`
        // reads.
        let mut connection = connection;
        let outcome = (|| -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
            let active_ctx_guard = crate::t27_tls::set_active_native_context(ctx);
            while stream.conn.is_handshaking() {
                if std::time::Instant::now() > deadline {
                    return Err(format!(
                        "{TLS_HANDSHAKE_FAILURE_SENTINEL}TLS handshake timed out"
                    ));
                }
                if stream.conn.wants_write() {
                    let mut blocked_refs = [Value::Object(connection)];
                    ctx.begin_blocking_region();
                    let written = stream.conn.write_tls(&mut EintrIo::new(&mut stream.sock));
                    ctx.end_blocking_region_refs(&mut blocked_refs);
                    if let Value::Object(o) = blocked_refs[0] {
                        connection = o;
                    }
                    written.map_err(|e| {
                        format!("{TLS_HANDSHAKE_FAILURE_SENTINEL}handshake write: {e}")
                    })?;
                }
                if stream.conn.wants_read() {
                    // FIX (client-cipher-restriction): `read_tls` returns `Ok(0)`
                    // (not an `Err`) once the peer closes the connection — rustls's
                    // documented contract. Left unchecked, a server that rejects the
                    // handshake and closes makes every subsequent `read_tls` return
                    // `Ok(0)` instantly, busy-spinning until `HANDSHAKE_TIMEOUT`
                    // instead of failing immediately. The deadline check above still
                    // bounds this loop, so this was never an outright hang here —
                    // just a wasted spin — but the sibling `rustls_client_connect`
                    // (t27_tls.rs), which has no such deadline, live-locked forever
                    // on exactly this gap once a real cipher restriction could
                    // actually cause a server to reject and close.
                    let mut blocked_refs = [Value::Object(connection)];
                    ctx.begin_blocking_region();
                    let read = stream.conn.read_tls(&mut EintrIo::new(&mut stream.sock));
                    ctx.end_blocking_region_refs(&mut blocked_refs);
                    if let Value::Object(o) = blocked_refs[0] {
                        connection = o;
                    }
                    let n = read.map_err(|e| {
                        format!("{TLS_HANDSHAKE_FAILURE_SENTINEL}handshake read: {e}")
                    })?;
                    if n == 0 {
                        return Err(format!(
                            "{TLS_HANDSHAKE_FAILURE_SENTINEL}connection closed by peer during \
                             handshake"
                        ));
                    }
                    stream.conn.process_new_packets().map_err(|e| {
                        format!("{TLS_HANDSHAKE_FAILURE_SENTINEL}handshake process: {e}")
                    })?;
                }
            }
            if let Some(tm_ctx_key) = crate::t27_tls::huc_default_trust_managers_ctx_key() {
                let peer_chain: Vec<Vec<u8>> = stream
                    .conn
                    .peer_certificates()
                    .map(|certs| certs.iter().map(|cert| cert.as_ref().to_vec()).collect())
                    .unwrap_or_default();
                if peer_chain.is_empty() {
                    return Err(format!(
                        "{TLS_HANDSHAKE_FAILURE_SENTINEL}no peer certificate available for \
                         TrustManager verification"
                    ));
                }
                crate::t27_tls::run_client_trust_check_for_chain(ctx, tm_ctx_key, peer_chain)
                    .map_err(|_| {
                        // Keep the reason. This `Result<_, String>` cannot carry
                        // the Java exception the check actually raised, so the
                        // check records it on a thread-local for exactly this
                        // hand-off — without it every rejection reads the same,
                        // whether the TrustManager genuinely refused the chain
                        // or the VM faulted underneath it.
                        let detail = crate::t27_tls::take_last_trust_rejection_detail()
                            .unwrap_or_else(|| "no exception detail available".to_string());
                        format!(
                            "{TLS_HANDSHAKE_FAILURE_SENTINEL}TrustManager rejected the peer \
                             certificate chain: {detail}"
                        )
                    })?;
            }
            // Endpoint identification, at real JSSE's ordering: after the
            // trust check, before the request is written. See
            // `huc_verify_hostname` for why an installed `HostnameVerifier` is
            // a FALLBACK for a failed built-in check and never an extra gate.
            //
            // The chain is re-read from the connection rather than reusing the
            // `peer_chain` above: that binding only exists inside the
            // TrustManager branch, which is skipped entirely when no custom
            // TrustManager is configured — and the built-in name check below
            // has to run on both paths, since the TrustManager branch is
            // exactly the one where rustls skipped the name check itself.
            {
                let peer_chain_der: Vec<Vec<u8>> = stream
                    .conn
                    .peer_certificates()
                    .map(|certs| certs.iter().map(|cert| cert.as_ref().to_vec()).collect())
                    .unwrap_or_default();
                let protocol = match stream.conn.protocol_version() {
                    Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2",
                    _ => "TLSv1.3",
                };
                let cipher = stream
                    .conn
                    .negotiated_cipher_suite()
                    .map(|cs| crate::t27_tls::suite_to_java_cipher_name_pub(cs.suite()))
                    .unwrap_or_else(|| "TLS_AES_256_GCM_SHA384".to_string());
                // Record BEFORE the hostname check: the accessors below report
                // what the handshake produced, and a peer that fails endpoint
                // identification still produced a chain the caller may want to
                // inspect from the exception path.
                record_https_peer_info(ctx, connection, &peer_chain_der, &cipher);
                huc_verify_hostname(
                    ctx,
                    connection,
                    &parsed.host,
                    // The RESOLVED port — `parse_url` fills the scheme default
                    // when the URL named none, so a plain `https://h/p` records
                    // 443, which is the port this connection dialled. G51-1 N1.
                    parsed.port,
                    protocol,
                    &cipher,
                    peer_chain_der,
                )?;
            }
            // The handshake is over and every Java-facing gate above it (the
            // `TrustManager` consultation and endpoint identification) has
            // run, so no bytecode can execute for the rest of this exchange —
            // close the active native-context window and park properly for the
            // request write and the response read. See this branch's
            // STW-TAKEOVER-FIX note above for why both halves of that are
            // required, and why `read_response` in particular can no longer
            // call back into Java.
            drop(active_ctx_guard);
            ctx.begin_blocking_region();
            let exchange = https_post_handshake_exchange(&mut stream, &req, head);
            ctx.end_blocking_region();
            exchange
        })();
        outcome
    } else {
        let mut s = tcp;
        // Plain-HTTP request write + response read is a real blocking OS
        // recv() with no Java-heap interaction anywhere in the closure
        // (`read_response` is generic over `Read`, takes no `ctx`) — bracket
        // it like the TCP connect above, unlike the HTTPS branch which uses
        // `set_active_native_context` instead for its own documented reason.
        // Without this, a cross-thread STW pause requested while this thread
        // is parked in `read_response` can never be satisfied: the thread is
        // neither in JIT code (can't be forcibly taken over) nor at a
        // safepoint (can't cooperate), so the takeover loop in
        // `stw_take_over_and_wait` spins until the external harness timeout.
        ctx.begin_blocking_region();
        let result = (|| -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
            s.write_all(&req).map_err(|e| format!("write: {e}"))?;
            s.flush().map_err(|e| format!("flush: {e}"))?;
            read_response(&mut s, head)
        })();
        ctx.end_blocking_region();
        if let (Ok((status, resp_headers, _)), Some((host, port))) = (&result, &poolable_key) {
            if is_poolable_response(*status, head, resp_headers) {
                pool_put(host, *port, s);
            }
        }
        result
    }
}

/// Wraps [`perform`] with HotSpot's transparent retry-once-on-dead-connection
/// behaviour: `sun.net.www.protocol.http.HttpURLConnection` silently retries
/// a request over a brand-new TCP connection when the first attempt's
/// connection is closed by the peer before any response bytes arrive (its
/// legacy recovery heuristic for a stale/dead pooled keep-alive connection —
/// which also covers a genuinely brand-new connection the peer tears down
/// mid-request). Confirmed against real JDK 21 and 25 with a minimal
/// standalone repro mirroring H2 `WebServer`'s self-shutdown-on-logout
/// pattern (`bug-h2-testweb-logout-connectexception-mismatch-FIXED.md`): the server reads
/// the `logout.do` request in full, then — synchronously, on that same
/// request-handling thread — closes its own just-accepted socket as part of
/// tearing itself down, before ever writing a response. That is NOT a
/// CratonVM-specific race (a standalone repro of exactly this shape fails
/// identically on real JDK), but real JDK's client-side retry then hits a
/// listening socket that has, by that point, already been closed by the same
/// shutdown — `ConnectException` — which is what the H2 test's
/// `catch (ConnectException e)` actually expects. Without this retry,
/// CratonVM's single-attempt `perform` surfaces the first attempt's raw
/// "connection closed before response head" as a generic `IOException`
/// instead. Skipped for the caller-supplied-socket (custom `SSLSocketFactory`)
/// path: that connection isn't ours to reopen.
fn perform_with_retry(
    ctx: &mut dyn NativeContext,
    connection: Option<ObjectRef>,
    parsed: &Url1,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
    connect_timeout: Duration,
    read_timeout: Duration,
    tls_restrictions: Option<&ClientTlsRestrictions>,
) -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
    let resp = perform(
        ctx,
        connection,
        parsed,
        method,
        headers,
        body,
        connect_timeout,
        read_timeout,
        tls_restrictions,
    );
    match resp {
        // FIX (tls-handshake-enforcement-gap, doc 21): the second condition
        // is the "Tomcat wanted to renegotiate for a client certificate and
        // couldn't" case — see `t27_tls::deferred_client_auth_contexts`. The
        // server has, by the time it closed this connection, armed itself to
        // request the certificate on the NEXT handshake, so retrying once on
        // a fresh connection is what completes the exchange. A server that
        // rejects for any other reason simply fails the retry the same way
        // (one extra loopback handshake), and the caller still receives
        // `SSLHandshakeException`.
        Err(ref e)
            if e == "connection closed before response head"
                || (e.starts_with(TLS_HANDSHAKE_FAILURE_SENTINEL)
                    && e.contains("connection closed immediately after the TLS handshake")) =>
        {
            perform(
                ctx,
                connection,
                parsed,
                method,
                headers,
                body,
                connect_timeout,
                read_timeout,
                tls_restrictions,
            )
        }
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Plain-HTTP keep-alive connection pool
//
// Real JDK's `sun.net.www.http.HttpClient`/`KeepAliveCache` pools/reuses a
// TCP connection across *separate* `HttpURLConnection` instances to the same
// `(host, port)` whenever the previous response was fully drained and
// neither side sent `Connection: close`. This codebase's `perform()` never
// did that — every call opened, used, and implicitly dropped a brand-new
// `TcpStream`. That's not just a performance gap: some servers key
// connection-scoped state off the TCP connection itself (H2's `WebServer`
// per-`WebThread` session-locale persistence is one confirmed case — see
// `bug-h2-httpurlconnection-no-keepalive-pooling-FIXED.md`
// for the full root-cause writeup with a `tcpdump`-confirmed repro).
//
// Deliberately scoped conservative for this first implementation:
//   - plain HTTP only (no TLS session reuse to get right, and no
//     interaction with the already-hardened HTTPS/custom-`SSLSocketFactory`
//     branches of `perform`/`perform_with_retry`, which this pool never
//     touches at all).
//   - never pools a chunked response (sidesteps getting the exact
//     "0\r\n\r\n" trailer consumption right — `read_chunked` doesn't
//     currently guarantee it hasn't left the trailing CRLF unread on the
//     wire, which would corrupt the next reused request's response parse;
//     simplest safe answer here is to just never reuse that connection).
//   - a pooled connection is liveness-checked with a non-blocking `peek()`
//     before being handed out (catches the common "peer already closed"
//     case cheaply), AND *every* use — pooled or freshly connected — falls
//     back to one fresh-connection retry on any transport failure, so a
//     `peek()` TOCTOU race (connection dies between the peek and our write)
//     can never do worse than one wasted reconnect. This retry-of-last-resort
//     is a superset of `perform_with_retry`'s HotSpot-parity retry (that one
//     only retries a *fresh* connection's specific "closed before response
//     head" failure; this one also retries a *reused* connection on any
//     failure, since staleness can surface as a write error too).
//   - small per-key cap and a short idle timeout, evaluated lazily on
//     access (no background reaper thread, unlike real JDK's actual
//     `KeepAliveCache`) — bounds memory/fd growth for the common case (the
//     same handful of hosts:ports hit repeatedly within one test run)
//     without the complexity of proactive cross-key eviction. A host:port
//     that's contacted once and never again leaks its pooled entries for
//     the life of the process; acceptable for now given the alternative
//     (a background reaper) adds its own correctness surface.
// ---------------------------------------------------------------------------

const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const LEGACY_POOL_MAX_PER_KEY: usize = 4;
/// Read timeout used ONLY for a reused pooled connection's first response —
/// deliberately much shorter than the caller's configured read timeout
/// (which defaults to 60s and can be set much higher). The liveness `peek()`
/// in `pool_take_live` only catches a peer that has already sent a FIN; it
/// cannot catch a peer that accepts our write into a half-dead connection
/// (e.g. it already closed its read side, or closed between the peek and
/// our write — a TOCTOU race) and then never responds. Without this, that
/// case stalls for the *full* read timeout before the fallback-to-fresh
/// retry ever kicks in — observed directly: an early version of this pool
/// using the caller's full read timeout for reused connections made
/// `org.h2.test.server.TestWeb` intermittently take 25s+ (a stale
/// connection or two hit per run, each stalling most of the way to a 60s
/// default) instead of the sub-second baseline. This bounds that worst case
/// to one short stall per stale hit instead of one long one; a genuinely
/// alive reused connection replying at all (even slowly) is expected to
/// beat this easily since it's on a warm connection with no fresh TCP
/// handshake to pay for.
const POOL_REUSE_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

type PoolKey = (String, u16);

fn legacy_conn_pool() -> &'static Mutex<HashMap<PoolKey, Vec<(TcpStream, Instant)>>> {
    static POOL: OnceLock<Mutex<HashMap<PoolKey, Vec<(TcpStream, Instant)>>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Pop a still-live pooled connection for `key`, discarding (not returning)
/// any entry that's aged out or whose peer has already closed. A
/// non-blocking `peek()` distinguishes "idle and healthy" (`WouldBlock`, no
/// data waiting) from "peer closed" (`Ok(0)`) or "peer sent something
/// unsolicited" (`Ok(n>0)` — also treated as unusable; a healthy idle
/// keep-alive connection has nothing to say until we write a request).
fn legacy_pool_take_live(key: &PoolKey) -> Option<TcpStream> {
    loop {
        let candidate = {
            let mut pool = legacy_conn_pool().lock().ok()?;
            let list = pool.get_mut(key)?;
            list.pop()
        };
        let (stream, inserted_at) = candidate?;
        if inserted_at.elapsed() >= POOL_IDLE_TIMEOUT {
            continue;
        }
        let _ = stream.set_nonblocking(true);
        let mut probe = [0u8; 1];
        let live = matches!(
            stream.peek(&mut probe),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
        );
        let _ = stream.set_nonblocking(false);
        if live {
            return Some(stream);
        }
    }
}

/// Return a connection to the pool for reuse, dropping it instead if the
/// per-key cap is already full (closing an idle connection is harmless —
/// it just means the next request to this host:port pays for a fresh
/// connect, same as before this pool existed).
fn legacy_pool_put(key: PoolKey, stream: TcpStream) {
    if let Ok(mut pool) = legacy_conn_pool().lock() {
        let list = pool.entry(key).or_default();
        if list.len() < LEGACY_POOL_MAX_PER_KEY {
            list.push((stream, Instant::now()));
        }
    }
}

/// Whether a just-completed request/response on this connection is safe to
/// hand back to the pool: neither side asked for `Connection: close`, and
/// the response wasn't chunked (see the module doc above for why chunked
/// responses are excluded).
fn is_poolable(resp_headers: &[(String, String)], req_headers: &[(String, String)]) -> bool {
    let has_close = |hs: &[(String, String)]| {
        hs.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("connection")
                && v.split(',')
                    .any(|tok| tok.trim().eq_ignore_ascii_case("close"))
        })
    };
    let is_chunked = resp_headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked")
    });
    !is_chunked && !has_close(resp_headers) && !has_close(req_headers)
}

/// Plain-TCP connect loop shared by [`perform`]'s http/https-agnostic connect
/// stage and this pool's fresh-connection path. Tags a refused connection
/// with [`CONNECT_REFUSED_SENTINEL`], exactly like `perform`'s own copy of
/// this loop (kept as a separate, small, duplicated function rather than
/// factored into `perform` itself — `perform` is already verified/merged
/// for the non-pooled path and this avoids touching it again for what's a
/// ~15-line loop).
fn connect_plain(parsed: &Url1, connect_timeout: Duration) -> Result<TcpStream, String> {
    let addr = format!("{}:{}", parsed.host, parsed.port);
    // Same IPv4-mapped fold as `perform`'s loop above — see the comment there.
    // This function is a deliberate duplicate of that loop, so a fix to one is
    // only half a fix.
    let mut addrs: Vec<std::net::SocketAddr> =
        std::net::ToSocketAddrs::to_socket_addrs(&addr.as_str())
            .map_err(|e| format!("resolve {addr}: {e}"))?
            .map(cratonvm_native_io::outbound_policy::normalize_connect_addr)
            .collect();
    addrs.sort_by_key(|sa| u8::from(sa.is_ipv6()));
    let mut last_err: Option<String> = None;
    let mut last_err_refused = false;
    for sa in addrs {
        match TcpStream::connect_timeout(&sa, connect_timeout) {
            Ok(s) => return Ok(s),
            Err(e) => {
                last_err_refused = e.kind() == std::io::ErrorKind::ConnectionRefused;
                last_err = Some(format!("connect {sa}: {e}"));
            }
        }
    }
    let msg = last_err.unwrap_or_else(|| format!("could not resolve any address for {addr}"));
    Err(if last_err_refused {
        format!("{CONNECT_REFUSED_SENTINEL}{msg}")
    } else {
        msg
    })
}

/// Configure a connection (pooled or fresh) identically before use.
fn configure_stream(stream: &TcpStream, read_timeout: Duration) {
    let _ = stream.set_read_timeout(Some(read_timeout));
    let _ = stream.set_write_timeout(Some(read_timeout));
    let _ = stream.set_nodelay(true);
}

/// Write `req` and read one response over `stream`, bracketed in a blocking
/// region exactly like `perform`'s own plain-HTTP write+read (pure OS I/O,
/// no Java-heap touch inside the closure).
fn attempt_plain(
    ctx: &mut dyn NativeContext,
    stream: &mut TcpStream,
    req: &[u8],
    head: bool,
) -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
    ctx.begin_blocking_region();
    let result = (|| -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
        stream.write_all(req).map_err(|e| format!("write: {e}"))?;
        stream.flush().map_err(|e| format!("flush: {e}"))?;
        read_response(stream, head)
    })();
    ctx.end_blocking_region();
    result
}

/// Plain-HTTP entry point used in place of [`perform_with_retry`] whenever
/// `parsed.scheme == "http"`: tries a pooled connection first, falls back to
/// (and, on success, pools) a fresh one. See the module doc above for the
/// pooling contract and why this is a separate, self-contained function
/// rather than a modification of `perform`. Each of the two branches below
/// makes at most one retry attempt, matching `perform_with_retry`'s
/// exactly-once bound — a pooled-connection failure retries once fresh; a
/// pool-miss's fresh connection retries once more only on the specific
/// HotSpot-parity "closed before response head" shape (`perform_with_retry`'s
/// own condition).
fn perform_pooled(
    ctx: &mut dyn NativeContext,
    parsed: &Url1,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
    let head = method.eq_ignore_ascii_case("HEAD");
    let req = build_request(method, parsed, headers, body);
    let key: PoolKey = (parsed.host.clone(), parsed.port);

    if let Some(mut stream) = legacy_pool_take_live(&key) {
        // Short probe timeout, not the caller's full `read_timeout` — see
        // `POOL_REUSE_PROBE_TIMEOUT`'s doc.
        configure_stream(&stream, read_timeout.min(POOL_REUSE_PROBE_TIMEOUT));
        let result = attempt_plain(ctx, &mut stream, &req, head);
        return match result {
            Ok((status, resp_headers, resp_body)) => {
                if is_poolable(&resp_headers, headers) {
                    legacy_pool_put(key, stream);
                }
                Ok((status, resp_headers, resp_body))
            }
            Err(_) => {
                // Stale despite the liveness peek (TOCTOU, or the peer
                // closed at this exact moment) — exactly one fresh retry.
                let mut fresh = connect_plain(parsed, connect_timeout)?;
                configure_stream(&fresh, read_timeout);
                let result2 = attempt_plain(ctx, &mut fresh, &req, head);
                if let Ok((_, ref resp_headers, _)) = result2 {
                    if is_poolable(resp_headers, headers) {
                        legacy_pool_put(key, fresh);
                    }
                }
                result2
            }
        };
    }

    // Pool miss: normal fresh-connect path, preserving `perform_with_retry`'s
    // original HotSpot-parity single retry (e.g. H2 WebServer's
    // self-shutdown-on-logout — see the sibling FIXED doc).
    let mut stream = connect_plain(parsed, connect_timeout)?;
    configure_stream(&stream, read_timeout);
    let result = attempt_plain(ctx, &mut stream, &req, head);
    match result {
        Err(ref e) if e == "connection closed before response head" => {
            let mut fresh = connect_plain(parsed, connect_timeout)?;
            configure_stream(&fresh, read_timeout);
            let result2 = attempt_plain(ctx, &mut fresh, &req, head);
            if let Ok((_, ref resp_headers, _)) = result2 {
                if is_poolable(resp_headers, headers) {
                    legacy_pool_put(key, fresh);
                }
            }
            result2
        }
        Ok((status, resp_headers, resp_body)) => {
            if is_poolable(&resp_headers, headers) {
                legacy_pool_put(key, stream);
            }
            Ok((status, resp_headers, resp_body))
        }
        other => other,
    }
}

/// Wraps [`perform`] with HotSpot's transparent retry-once-on-dead-connection
/// behaviour: `sun.net.www.protocol.http.HttpURLConnection` silently retries
/// a request over a brand-new TCP connection when the first attempt's
/// connection is closed by the peer before any response bytes arrive (its
/// legacy recovery heuristic for a stale/dead pooled keep-alive connection —
/// which also covers a genuinely brand-new connection the peer tears down
/// mid-request). Confirmed against real JDK 21 and 25 with a minimal
/// standalone repro mirroring H2 `WebServer`'s self-shutdown-on-logout
/// pattern (`bug-h2-testweb-logout-connectexception-mismatch-FIXED.md`): the server reads
/// the `logout.do` request in full, then — synchronously, on that same
/// request-handling thread — closes its own just-accepted socket as part of
/// tearing itself down, before ever writing a response. That is NOT a
/// CratonVM-specific race (a standalone repro of exactly this shape fails
/// identically on real JDK), but real JDK's client-side retry then hits a
/// listening socket that has, by that point, already been closed by the same
/// shutdown — `ConnectException` — which is what the H2 test's
/// `catch (ConnectException e)` actually expects. Without this retry,
/// CratonVM's single-attempt `perform` surfaces the first attempt's raw
/// "connection closed before response head" as a generic `IOException`
/// instead. Skipped for the caller-supplied-socket (custom `SSLSocketFactory`)
/// path: that connection isn't ours to reopen.
// ---------------------------------------------------------------------------
#[allow(dead_code)]
const PERFORM_RETRY_SEMANTICS: () = ();

// Java-side helpers
// ---------------------------------------------------------------------------

fn extract_headers(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if let Value::Object(Some(arr)) = ctx.get_field(this, HUC_REQ_HEADERS) {
        let len = ctx.array_length(arr);
        for i in 0..len {
            if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
                if let Some(line) = ctx.read_string(s) {
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
    }
    out
}

fn extract_body(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<u8> {
    // ByteArrayOutputStream synthetic field 0 = byte[] buf, field 1 = count.
    if let Value::Object(Some(baos)) = ctx.get_field(this, HUC_REQ_BODY_STREAM) {
        if let Value::Object(Some(arr)) = ctx.get_field(baos, 0) {
            let mut buf = read_byte_array(ctx, arr);
            if let Value::Int(count) = ctx.get_field(baos, 1) {
                if (count as usize) < buf.len() {
                    buf.truncate(count as usize);
                }
            }
            return buf;
        }
    }
    Vec::new()
}

fn ensure_connected(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallResult {
    if matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        return Ok(None);
    }
    if matches!(ctx.get_field(this, HUC_DISCONNECTED), Value::Int(1)) {
        return Err(ioex("HttpURLConnection: connection already closed"));
    }
    let url_str = read_str_field(ctx, this, HUC_URL_STR)
        .ok_or_else(|| iae("HttpURLConnection: URL not set"))?;
    let parsed = parse_url(&url_str).map_err(ioex)?;
    let method = read_str_field(ctx, this, HUC_METHOD).unwrap_or_else(|| "GET".to_string());
    let headers = extract_headers(ctx, this);
    let body = extract_body(ctx, this);
    let connect_to = match ctx.get_field(this, HUC_CONNECT_TIMEOUT) {
        Value::Int(n) if n > 0 => Duration::from_millis(n as u64),
        _ => Duration::from_secs(30),
    };
    let read_to = match ctx.get_field(this, HUC_READ_TIMEOUT) {
        Value::Int(n) if n > 0 => Duration::from_millis(n as u64),
        _ => Duration::from_secs(60),
    };

    // FIX (client-cipher-restriction): resolve any real caller-installed
    // SSLSocketFactory BEFORE calling perform — the probe up-call needs `ctx`.
    let tls_restrictions = if parsed.scheme == "https" {
        huc_client_tls_restrictions(ctx, Some(this), &parsed.host, parsed.port)?
    } else {
        None
    };
    // `perform` manages its own (fine-grained) blocking regions internally —
    // see its doc — so this caller must not wrap the whole call in one.
    // Goes through `perform_with_retry` (like `huc_real_perform` already did)
    // so `HttpURLConnection.connect()` gets the same one-shot retry on a
    // connection the peer closed before responding — including the
    // deferred-client-auth case (doc 21).
    let (status, headers, body_bytes) = match perform_with_retry(
        ctx,
        Some(this),
        &parsed,
        &method,
        &headers,
        &body,
        connect_to,
        read_to,
        tls_restrictions.as_ref(),
    ) {
        Ok(v) => v,
        // FIX (client-cipher-restriction): mirror `huc_real_perform`'s
        // sentinel handling — a handshake-phase failure (e.g. no cipher/cert
        // in common, now correctly and promptly surfaced instead of busy-
        // spinning per the `read_tls` `Ok(0)` fix above) must reach Java as
        // `SSLHandshakeException`, not a bare `IOException`. This path
        // (`ensure_connected`, reached via `HttpURLConnection.connect()`)
        // previously fell straight to the generic `ioex` branch below for
        // every error including handshake failures — a pre-existing
        // inconsistency with `huc_real_perform` that the old, slow,
        // eventually-timing-out-anyway busy-spin never surfaced clearly
        // enough to notice (`TestSSLHostConfigCompat`'s EC/RSA
        // cert-mismatch cases, which expect `SSLHandshakeException`
        // specifically, exposed it once handshake failures started
        // resolving promptly).
        Err(ref e) if e.starts_with(TLS_HANDSHAKE_FAILURE_SENTINEL) => {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "javax/net/ssl/SSLHandshakeException",
                e.trim_start_matches(TLS_HANDSHAKE_FAILURE_SENTINEL),
            ));
        }
        // See the matching arm in `huc_real_perform`: a `HostnameVerifier`
        // rejection is `SSLPeerUnverifiedException`, not a handshake failure.
        // Registered on this path too so `HttpURLConnection.connect()` and
        // `getInputStream()`/`getResponseCode()` agree on the exception type.
        Err(ref e) if e.starts_with(TLS_PEER_UNVERIFIED_SENTINEL) => {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "javax/net/ssl/SSLPeerUnverifiedException",
                e.trim_start_matches(TLS_PEER_UNVERIFIED_SENTINEL),
            ));
        }
        // See the matching arm in `huc_real_perform`: a verifier that answered
        // `false` is a plain `IOException`. Registered on this path too so
        // `connect()` and `getResponseCode()` cannot disagree about the type.
        Err(ref e) if e.starts_with(TLS_HOSTNAME_REFUSED_SENTINEL) => {
            return Err(ioex(
                e.trim_start_matches(TLS_HOSTNAME_REFUSED_SENTINEL)
                    .to_string(),
            ));
        }
        Err(e) => return Err(ioex(format!("HttpURLConnection.connect failed: {e}"))),
    };

    let id = registry()
        .lock()
        .map_err(|_| ioex("connection registry poisoned"))?
        .allocate(ConnState {
            status,
            response_body: body_bytes,
            response_headers: headers,
            body_consumed: false,
            truncated: take_response_truncated(),
        });
    ctx.set_field(this, HUC_CONN_ID, Value::Int(id));
    ctx.set_field(this, HUC_CONNECTED, Value::Int(1));
    Ok(None)
}

fn with_state<R>(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    f: impl FnOnce(&ConnState) -> R,
) -> Option<R> {
    let id = match ctx.get_field(this, HUC_CONN_ID) {
        Value::Int(i) if i > 0 => i,
        _ => return None,
    };
    let reg = registry().lock().ok()?;
    reg.get(id).map(f)
}

// ---------------------------------------------------------------------------
// Native callbacks
// ---------------------------------------------------------------------------

fn huc_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // `<init>(Ljava/net/URL;)V` is also the REAL descriptor of the abstract
    // `java.net.HttpURLConnection(URL u)` protected constructor, so a user
    // subclass that calls `super(u)` directly (bypassing `URL.openConnection()`)
    // hits this native too — not just our own synthetic-carrier allocation
    // path. Detect that case by asking the URL argument for its real
    // `toExternalForm()`: a genuine `java.net.URL` answers with a proper
    // "scheme://..." string; treat it as a real carrier (field 0 keeps the
    // URL object itself, matching `is_real_carrier`/`huc_real_object_url`)
    // instead of clobbering field 0 with the synthetic Int(-1) conn-id, which
    // corrupted the real inherited `URLConnection.url`/`doOutput`/... fields
    // and broke `getURL()` + every `is_real_carrier` check downstream
    // (`ensure_connected` then misread the never-populated HUC_URL_STR slot
    // and threw "HttpURLConnection: URL not set").
    if let Some(Value::Object(Some(url_obj))) = args.get(1) {
        let url_obj = *url_obj;
        // `toExternalForm()` is real Java: it allocates a String and can move
        // both `this` and `url_obj`, each of which is written through below.
        let this_pin = ctx.pin_native_root(this);
        let url_pin = ctx.pin_native_root(url_obj);
        let call = ctx.invoke_virtual(url_obj, "toExternalForm", "()Ljava/lang/String;", &[]);
        let this = ctx.read_native_pin(this_pin, this);
        let url_obj = ctx.read_native_pin(url_pin, url_obj);
        if let Ok(Some(Value::Object(Some(s)))) = call {
            if let Some(full) = ctx.read_string(s) {
                if full.contains("://") {
                    ctx.set_field(this, HUC_CONN_ID, Value::Object(Some(url_obj)));
                    return Ok(None);
                }
            }
        }
    }
    ctx.set_field(this, HUC_CONN_ID, Value::Int(-1));
    let m = ctx.create_string("GET");
    ctx.set_field(this, HUC_METHOD, Value::Object(Some(m)));
    ctx.set_field(this, HUC_REQ_HEADERS, Value::Object(None));
    ctx.set_field(this, HUC_REQ_BODY_STREAM, Value::Object(None));
    ctx.set_field(this, HUC_DO_INPUT, Value::Int(1));
    ctx.set_field(this, HUC_DO_OUTPUT, Value::Int(0));
    ctx.set_field(this, HUC_CONNECTED, Value::Int(0));
    ctx.set_field(this, HUC_DISCONNECTED, Value::Int(0));
    ctx.set_field(this, HUC_INSTANCE_FOLLOW_REDIRECTS, Value::Int(1));
    ctx.set_field(this, HUC_CONNECT_TIMEOUT, Value::Int(0));
    ctx.set_field(this, HUC_READ_TIMEOUT, Value::Int(0));
    // If args[1] is a URL object with field 0 = full URL string, capture it.
    if let Some(Value::Object(Some(url_obj))) = args.get(1) {
        let s_val = ctx.get_field(*url_obj, 0);
        if let Value::Object(Some(_)) = s_val {
            ctx.set_field(this, HUC_URL_STR, s_val);
        }
    }
    Ok(None)
}

fn huc_connect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Real-JDK carrier: `connect()` only opens the socket on HotSpot and the
    // request is sent lazily by `getResponseCode`/`getInputStream`/the output
    // stream. We mirror that by deferring the actual `perform` — running it here
    // would (a) misread the synthetic slots `ensure_connected` consults and
    // (b) prematurely fix the request before the body/headers are fully staged.
    if is_real_carrier(ctx, this) {
        return Ok(None);
    }
    ensure_connected(ctx, this)
}

fn huc_get_response_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Real-JDK sun.net.www HttpURLConnection (field 0 is the real URL object):
    // perform from the real URL rather than misreading our synthetic HUC_* slots.
    if let Some(url_str) = huc_real_object_url(ctx, this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            return Ok(Some(Value::Int(huc_real_perform(ctx, this, &url_str)?)));
        }
    }
    ensure_connected(ctx, this)?;
    let code = with_state(ctx, this, |s| s.status).unwrap_or(-1);
    Ok(Some(Value::Int(code)))
}

fn status_reason(status: i32) -> String {
    match status {
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
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => return format!("Status {status}"),
    }
    .to_string()
}

fn huc_get_response_message(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Real-JDK carrier: derive from the cached perform result. Prefer the
    // reason phrase actually read off the wire (real servers often deviate
    // from the RFC's canonical phrase, e.g. OkHttp MockWebServer's default
    // "Server Error" for 500 vs. the RFC's "Internal Server Error" — real
    // HttpURLConnection.getResponseMessage() always returns exactly what the
    // server sent) — the hardcoded `status_reason` table is only a fallback
    // for when no reason was captured (e.g. the synthetic timeout result).
    if let Some(url_str) = huc_real_object_url(ctx, this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            let status = huc_real_perform(ctx, this, &url_str)?;
            let key = ctx.identity_hash_code(this);
            let reason = real_results()
                .lock()
                .ok()
                .and_then(|t| t.get(&key).map(|r| r.reason.clone()))
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| status_reason(status));
            let s = ctx.create_string(&reason);
            return Ok(Some(Value::Object(Some(s))));
        }
    }
    ensure_connected(ctx, this)?;
    let msg = with_state(ctx, this, |s| match s.status {
        200 => "OK".to_string(),
        201 => "Created".to_string(),
        204 => "No Content".to_string(),
        301 => "Moved Permanently".to_string(),
        302 => "Found".to_string(),
        304 => "Not Modified".to_string(),
        400 => "Bad Request".to_string(),
        401 => "Unauthorized".to_string(),
        403 => "Forbidden".to_string(),
        404 => "Not Found".to_string(),
        500 => "Internal Server Error".to_string(),
        503 => "Service Unavailable".to_string(),
        c => format!("Status {c}"),
    })
    .unwrap_or_else(|| "".to_string());
    let s = ctx.create_string(&msg);
    Ok(Some(Value::Object(Some(s))))
}

fn huc_get_input_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;

    // Compatibility: `java.net.URL.openConnection()` (registered in
    // `net_phase_e::register_re4_url_http` and in `lib.rs::register_net_natives`)
    // allocates `java/net/HttpURLConnection` synthetics with a DIFFERENT field
    // layout — field 0 holds the originating URL object, not an i32 conn_id.
    // For non-http(s) URLs (file:, jar:, classpath:, jrt:, nested:), the right
    // behaviour is to delegate to `URL.openStream()` so Logback / Spring /
    // Cassandra can read XML configs through `URLConnection.getInputStream()`.
    // Without this branch, the HTTP-only `ensure_connected` path below treats
    // the URL object as a conn_id, fails `parse_url`, and returns a 0-byte
    // ByteArrayInputStream — which surfaces as `SAXParseException: Premature
    // end of file` in Logback's Joran parser (Cassandra NodeTool boot).
    if let Value::Object(Some(maybe_url)) = ctx.get_field(this, HUC_CONN_ID) {
        // Recover the URL through its public external form first.  This works
        // for both real-JDK URLs and Craton's synthetic resource URLs, whereas
        // probing individual URL fields confuses a real `file:` URL's authority
        // or path with the complete URL.  `URLClassLoader.getResourceAsStream`
        // uses `openConnection().getInputStream()`, so treating a non-HTTP URL
        // as the HTTP carrier's empty response body makes inherited resources
        // appear as zero-byte streams (Hazelcast's filtered-loader XML config).
        if let Some(full) = huc_real_object_url(ctx, this) {
            if full.starts_with("http://") || full.starts_with("https://") {
                huc_real_perform(ctx, this, &full)?;
                let body = huc_real_body(ctx, this);
                let truncated = huc_real_truncated(ctx, this);
                return make_response_input_stream(ctx, &body, truncated, Some(this));
            }
            return ctx.invoke_virtual(maybe_url, "openStream", "()Ljava/io/InputStream;", &[]);
        }
        // Peek at the external form via the URL's full-URL string field
        // (field 5 in our URL synthetic), falling back to field 0.
        let url_str = match ctx.get_field(maybe_url, 5) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => match ctx.get_field(maybe_url, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            },
        };
        if !url_str.is_empty()
            && !url_str.starts_with("http://")
            && !url_str.starts_with("https://")
        {
            return ctx.invoke_virtual(maybe_url, "openStream", "()Ljava/io/InputStream;", &[]);
        }
    }

    ensure_connected(ctx, this)?;
    let (body_bytes, truncated) =
        with_state(ctx, this, |s| (s.response_body.clone(), s.truncated)).unwrap_or_default();
    // Mark consumed so a follow-up read doesn't double-pull.
    if let Value::Int(id) = ctx.get_field(this, HUC_CONN_ID) {
        if let Ok(mut reg) = registry().lock() {
            if let Some(state) = reg.conns.get_mut(&id) {
                state.body_consumed = true;
            }
        }
    }
    make_response_input_stream(ctx, &body_bytes, truncated, Some(this))
}

fn huc_get_error_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Real-JDK carrier: serve the cached body when the response was an error.
    if let Some(url_str) = huc_real_object_url(ctx, this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            let status = huc_real_perform(ctx, this, &url_str)?;
            if status < 400 {
                return Ok(Some(Value::Object(None)));
            }
            let body = huc_real_body(ctx, this);
            let truncated = huc_real_truncated(ctx, this);
            return make_response_input_stream(ctx, &body, truncated, Some(this));
        }
    }
    if !matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        return Ok(Some(Value::Object(None)));
    }
    let (status, body_bytes) =
        with_state(ctx, this, |s| (s.status, s.response_body.clone())).unwrap_or((-1, Vec::new()));
    if status < 400 {
        return Ok(Some(Value::Object(None)));
    }
    let body_arr = new_byte_array(ctx, &body_bytes);
    let len = body_bytes.len() as i32;
    let stream = try_alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4)?;
    ctx.set_field(stream, 0, Value::Object(Some(body_arr)));
    ctx.set_field(stream, 1, Value::Int(0));
    ctx.set_field(stream, 2, Value::Int(0));
    ctx.set_field(stream, 3, Value::Int(len));
    Ok(Some(Value::Object(Some(stream))))
}

fn huc_get_output_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Real-JDK carrier: the synthetic `HUC_DO_OUTPUT`/`HUC_CONNECTED` slots land
    // on unrelated real fields (one reads 1 → the old code wrongly threw "cannot
    // write after connect"). Use the identity-keyed `RealReq.do_output` and a
    // buffered BAOS tracked by identity; a write-after-`connect()` is legal here
    // exactly as on HotSpot (the body is sent lazily by `getResponseCode`).
    if is_real_carrier(ctx, this) {
        // `doOutput` may have been staged either through our own `setDoOutput`
        // native (identity-keyed side-table) or via the `java/net/HttpURLConnection`
        // setter natives that write the real `URLConnection.doOutput` field
        // directly (whichever won registration). Consult BOTH so the gate never
        // spuriously reports false and refuses a legitimate write.
        let do_output = with_real_req(ctx, this, |r| r.do_output)
            || matches!(ctx.get_field_by_name(this, "doOutput"), Value::Int(1));
        if !do_output {
            return Err(ioex("HttpURLConnection.getOutputStream: doOutput=false"));
        }
        // JDK semantics: opening the output stream promotes a still-default GET
        // to POST (see sun.net.www...HttpURLConnection.getOutputStream:
        // `if (method.equals("GET")) method = "POST"`). The harness's `postUrl`
        // relies on this — it sets only `setDoOutput(true)`, never the method —
        // so without this promotion the body is sent as a GET and the servlet
        // replies 405 Method Not Allowed.
        with_real_req(ctx, this, |r| {
            if r.method.is_empty() || r.method == "GET" {
                r.method = "POST".to_string();
            }
        });
        let key = ctx.identity_hash_code(this);
        if let Some((vkey, stored)) = real_body_streams()
            .lock()
            .ok()
            .and_then(|t| t.get(&key).copied())
        {
            let existing = ctx.read_var_handle_root(vkey).unwrap_or(stored);
            return Ok(Some(Value::Object(Some(existing))));
        }
        let baos = try_alloc_concurrent_synthetic(ctx, "java/io/ByteArrayOutputStream", 2)?;
        // Family-1 fix (cce0079): `new_array` below can move the
        // still-unrooted `baos` — pin and refresh it before the field
        // stores and the registry insert.
        let baos_pin = ctx.pin_native_root(baos);
        let backing = ctx.new_array(ArrayElementType::Byte, 0);
        let baos = ctx.read_native_pin(baos_pin, baos);
        ctx.unpin_native_roots(baos_pin);
        ctx.set_field(baos, 0, Value::Object(Some(backing)));
        ctx.set_field(baos, 1, Value::Int(0));
        ctx.register_var_handle_root(baos);
        let vkey = ctx.identity_hash_code(baos);
        if let Ok(mut t) = real_body_streams().lock() {
            t.insert(key, (vkey, baos));
        }
        let streaming = real_reqs()
            .lock()
            .ok()
            .and_then(|reqs| reqs.get(&key).map(|req| req.streaming))
            .unwrap_or_default();
        if let StreamingMode::Fixed(expected) = streaming {
            start_live_fixed_stream(ctx, this, baos, expected)?;
        }
        return Ok(Some(Value::Object(Some(baos))));
    }
    if !matches!(ctx.get_field(this, HUC_DO_OUTPUT), Value::Int(1)) {
        return Err(ioex("HttpURLConnection.getOutputStream: doOutput=false"));
    }
    if matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        return Err(ioex(
            "HttpURLConnection.getOutputStream: cannot write after connect",
        ));
    }
    // Lazily allocate a ByteArrayOutputStream-backed body buffer.
    if let Value::Object(Some(existing)) = ctx.get_field(this, HUC_REQ_BODY_STREAM) {
        return Ok(Some(Value::Object(Some(existing))));
    }
    let baos = try_alloc_concurrent_synthetic(ctx, "java/io/ByteArrayOutputStream", 2)?;
    let backing = ctx.new_array(ArrayElementType::Byte, 0);
    ctx.set_field(baos, 0, Value::Object(Some(backing)));
    ctx.set_field(baos, 1, Value::Int(0));
    ctx.set_field(this, HUC_REQ_BODY_STREAM, Value::Object(Some(baos)));
    Ok(Some(Value::Object(Some(baos))))
}

pub(crate) fn huc_get_header_field_named(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    // Real-JDK carrier: read from the cached perform result, not synthetic slots.
    // LAST duplicate wins, mirroring `sun.net.www.MessageHeader.findValue`'s
    // backwards iteration (see `content_length_of`'s doc for the MockWebServer
    // duplicate-Content-Length case that exposed this).
    if let Some(url_str) = huc_real_object_url(ctx, this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            huc_real_perform(ctx, this, &url_str)?;
            let v = huc_real_headers(ctx, this)
                .into_iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case(&name))
                .last()
                .map(|(_, v)| v);
            return Ok(Some(match v {
                Some(s) => Value::Object(Some(ctx.create_string(&s))),
                None => Value::Object(None),
            }));
        }
    }
    if !matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        ensure_connected(ctx, this)?;
    }
    let v = with_state(ctx, this, |s| {
        s.response_headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(&name))
            .last()
            .map(|(_, v)| v.clone())
    })
    .flatten();
    match v {
        Some(s) => {
            let js = ctx.create_string(&s);
            Ok(Some(Value::Object(Some(js))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn huc_get_header_field_indexed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(-1);
    if idx < 0 {
        return Ok(Some(Value::Object(None)));
    }
    if let Some(url_str) = huc_real_object_url(ctx, this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            huc_real_perform(ctx, this, &url_str)?;
            let v = huc_real_headers(ctx, this).get(idx as usize).cloned();
            return Ok(Some(match v {
                Some((_k, val)) => Value::Object(Some(ctx.create_string(&val))),
                None => Value::Object(None),
            }));
        }
    }
    ensure_connected(ctx, this)?;
    let v = with_state(ctx, this, |s| s.response_headers.get(idx as usize).cloned()).flatten();
    match v {
        Some((_k, val)) => {
            let js = ctx.create_string(&val);
            Ok(Some(Value::Object(Some(js))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn huc_get_header_field_key_indexed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(-1);
    if idx < 0 {
        return Ok(Some(Value::Object(None)));
    }
    if let Some(url_str) = huc_real_object_url(ctx, this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            huc_real_perform(ctx, this, &url_str)?;
            let v = huc_real_headers(ctx, this).get(idx as usize).cloned();
            return Ok(Some(match v {
                Some((k, _)) => Value::Object(Some(ctx.create_string(&k))),
                None => Value::Object(None),
            }));
        }
    }
    ensure_connected(ctx, this)?;
    let v = with_state(ctx, this, |s| s.response_headers.get(idx as usize).cloned()).flatten();
    match v {
        Some((k, _)) => {
            let js = ctx.create_string(&k);
            Ok(Some(Value::Object(Some(js))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// `getHeaderFields()` — the `Map<String,List<String>>` accessor the JDK's
/// `URLConnection` exposes and `TomcatBaseTest.methodUrl` reads `resHead` from.
/// Registered nowhere before this fix, so it fell through to real-JDK bytecode
/// that reads response state our shim never populated → empty map.
fn huc_get_header_fields(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // URL lookup can enter real-JDK code and collect. Keep the carrier rooted
    // until the subsequent perform/header operations have consumed it.
    let this_pin = ctx.pin_native_root(this);
    let result = (|| -> MethodCallResult {
        let this = ctx.read_native_pin(this_pin, this);
        let headers = if let Some(url_str) = huc_real_object_url(ctx, this) {
            let this = ctx.read_native_pin(this_pin, this);
            if url_str.starts_with("http://") || url_str.starts_with("https://") {
                huc_real_perform(ctx, this, &url_str)?;
                let this = ctx.read_native_pin(this_pin, this);
                huc_real_headers(ctx, this)
            } else {
                ensure_connected(ctx, this)?;
                let this = ctx.read_native_pin(this_pin, this);
                with_state(ctx, this, |s| s.response_headers.clone()).unwrap_or_default()
            }
        } else {
            ensure_connected(ctx, this)?;
            let this = ctx.read_native_pin(this_pin, this);
            with_state(ctx, this, |s| s.response_headers.clone()).unwrap_or_default()
        };
        let map = build_header_map(ctx, &headers)?;
        Ok(Some(Value::Object(Some(map))))
    })();
    ctx.unpin_native_roots(this_pin);
    result
}

/// Content length per the real `URLConnection.getContentLengthLong()` contract:
/// the `Content-Length` RESPONSE HEADER when present, else the buffered body
/// size (legacy behaviour, kept for chunked/close-delimited responses). The
/// distinction matters for HEAD responses, which advertise the entity size in
/// the header but carry NO body — reporting the (empty) body made Spring's
/// `AbstractFileResolvingResource.isReadable()/contentLength()` see 0 after a
/// HEAD 200 and call the resource empty/unreadable
/// (ResourceTests.remoteResourceExists: `exists()` true but `isReadable()`
/// false, `contentLength()` 0 instead of 6).
///
/// LAST duplicate wins: `sun.net.www.MessageHeader.findValue` iterates
/// BACKWARDS (`for (int i = nkeys; --i >= 0;)`), so on real JDK a later
/// duplicate header overrides an earlier one. MockWebServer actually emits
/// `Content-Length: 0` (its bodiless default) FOLLOWED BY the test's
/// `addHeader("Content-Length", "6")` on both HotSpot and CratonVM — HotSpot
/// reads 6, so must we.
fn content_length_of(headers: &[(String, String)], body_len: usize) -> i64 {
    headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .last()
        .and_then(|(_, v)| v.trim().parse::<i64>().ok())
        .unwrap_or(body_len as i64)
}

pub(crate) fn huc_get_content_length(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(url_str) = huc_real_object_url(ctx, this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            huc_real_perform(ctx, this, &url_str)?;
            let n = content_length_of(&huc_real_headers(ctx, this), huc_real_body(ctx, this).len());
            return Ok(Some(Value::Int(n as i32)));
        }
    }
    if !matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        ensure_connected(ctx, this)?;
    }
    let n = with_state(ctx, this, |s| {
        content_length_of(&s.response_headers, s.response_body.len()) as i32
    })
    .unwrap_or(-1);
    Ok(Some(Value::Int(n)))
}

pub(crate) fn huc_get_content_length_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(url_str) = huc_real_object_url(ctx, this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            huc_real_perform(ctx, this, &url_str)?;
            let n = content_length_of(&huc_real_headers(ctx, this), huc_real_body(ctx, this).len());
            return Ok(Some(Value::Long(n)));
        }
    }
    if !matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        ensure_connected(ctx, this)?;
    }
    let n = with_state(ctx, this, |s| {
        content_length_of(&s.response_headers, s.response_body.len())
    })
    .unwrap_or(-1);
    Ok(Some(Value::Long(n)))
}

fn huc_disconnect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // MEASURED, HotSpot (`G7-1` §1d): after `disconnect()` every one of the six
    // `HttpsURLConnection` session accessors throws `IllegalStateException:
    // connection not yet open` again. Before both carrier shapes are handled
    // below, because the session tables are keyed on the carrier's identity and
    // are the same tables for both.
    https_recycle_carrier(ctx, this);
    // Real carrier: clear identity-keyed side-table state, never write synthetic
    // slots (they alias real fields on a real-JDK object).
    if is_real_carrier(ctx, this) {
        real_forget(ctx, this);
        return Ok(None);
    }
    if let Value::Int(id) = ctx.get_field(this, HUC_CONN_ID) {
        if id > 0 {
            if let Ok(mut reg) = registry().lock() {
                reg.remove(id);
            }
        }
    }
    ctx.set_field(this, HUC_CONN_ID, Value::Int(-1));
    ctx.set_field(this, HUC_CONNECTED, Value::Int(0));
    ctx.set_field(this, HUC_DISCONNECTED, Value::Int(1));
    Ok(None)
}

fn huc_set_request_method(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let m = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Err(iae("setRequestMethod: null method")),
    };
    let normalized = m.to_ascii_uppercase();
    // Real JDK's `sun.net.www.protocol.http.HttpURLConnection.setRequestMethod`
    // whitelist is {GET, POST, HEAD, OPTIONS, PUT, DELETE, TRACE} — notably NOT
    // PATCH, which is why Spring recommends a different `ClientHttpRequestFactory`
    // for PATCH and its own test suite (`SimpleClientHttpRequestFactoryTests
    // .httpMethods()`) asserts `ProtocolException` for it. Rejecting it here
    // (as `ProtocolException`, matching the real JDK exception type) mirrors
    // that restriction instead of silently accepting it.
    if !matches!(
        normalized.as_str(),
        "GET" | "HEAD" | "POST" | "PUT" | "DELETE" | "OPTIONS" | "TRACE" | "CONNECT"
    ) {
        return Err(protocol_ex(format!("Invalid HTTP method: {m}")));
    }
    if is_real_carrier(ctx, this) {
        with_real_req(ctx, this, |r| r.method = normalized);
        return Ok(None);
    }
    let s = ctx.create_string(&normalized);
    ctx.set_field(this, HUC_METHOD, Value::Object(Some(s)));
    Ok(None)
}

fn huc_get_request_method(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if is_real_carrier(ctx, this) {
        let m = with_real_req(ctx, this, |r| {
            if r.method.is_empty() {
                "GET".to_string()
            } else {
                r.method.clone()
            }
        });
        let s = ctx.create_string(&m);
        return Ok(Some(Value::Object(Some(s))));
    }
    let m = match ctx.get_field(this, HUC_METHOD) {
        Value::Object(Some(s)) => s,
        _ => ctx.create_string("GET"),
    };
    Ok(Some(Value::Object(Some(m))))
}

fn huc_set_request_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Err(iae("setRequestProperty: null key")),
    };
    let value = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    // Real-JDK carrier: store into the identity-keyed header list (synthetic
    // slot 3 lands on an unrelated real field → header silently dropped, which
    // is the dropped-`Authorization`/`Origin` 401/403 bug). setRequestProperty
    // replaces any existing values for the key.
    if is_real_carrier(ctx, this) {
        with_real_req(ctx, this, |r| {
            r.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(&key));
            r.headers.push((key.clone(), value.clone()));
        });
        return Ok(None);
    }
    let line = ctx.create_string(&format!("{key}: {value}"));
    let arr = match ctx.get_field(this, HUC_REQ_HEADERS) {
        Value::Object(Some(a)) => a,
        _ => {
            let a = ctx.new_array(ArrayElementType::Reference, 32);
            ctx.set_field(this, HUC_REQ_HEADERS, Value::Object(Some(a)));
            a
        }
    };
    let len = ctx.array_length(arr);
    // Replace if the key already exists, else add to first empty slot.
    for i in 0..len {
        if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
            let existing = ctx.read_string(s).unwrap_or_default();
            if let Some(colon) = existing.find(':') {
                if existing[..colon].trim().eq_ignore_ascii_case(&key) {
                    ctx.set_array_element(arr, i, Value::Object(Some(line)));
                    return Ok(None);
                }
            }
        }
    }
    for i in 0..len {
        if matches!(ctx.get_array_element(arr, i), Value::Object(None)) {
            ctx.set_array_element(arr, i, Value::Object(Some(line)));
            return Ok(None);
        }
    }
    Ok(None)
}

fn huc_add_request_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Err(iae("addRequestProperty: null key")),
    };
    let value = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    if is_real_carrier(ctx, this) {
        with_real_req(ctx, this, |r| r.headers.push((key.clone(), value.clone())));
        return Ok(None);
    }
    let line = ctx.create_string(&format!("{key}: {value}"));
    let arr = match ctx.get_field(this, HUC_REQ_HEADERS) {
        Value::Object(Some(a)) => a,
        _ => {
            let a = ctx.new_array(ArrayElementType::Reference, 32);
            ctx.set_field(this, HUC_REQ_HEADERS, Value::Object(Some(a)));
            a
        }
    };
    let len = ctx.array_length(arr);
    for i in 0..len {
        if matches!(ctx.get_array_element(arr, i), Value::Object(None)) {
            ctx.set_array_element(arr, i, Value::Object(Some(line)));
            return Ok(None);
        }
    }
    Ok(None)
}

/// `getRequestProperty(String)` — read back a request header. The JDK joins
/// multiple `addRequestProperty` values with ", ". Registered as a native so a
/// real-JDK carrier reads from our identity-keyed header list rather than the
/// real `requests` map our setter never populated (which returned null).
fn huc_get_request_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let joined: Option<String> = if is_real_carrier(ctx, this) {
        with_real_req(ctx, this, |r| {
            // The LAST matching value, not a join. MEASURED on HotSpot
            // 25.0.3+9 (`probes/HucAccessors.java`): after
            // `setRequestProperty("X-A","2"); addRequestProperty("X-A","3")`,
            // `getRequestProperty("X-A")` answers `"3"`, while
            // `getRequestProperties()` answers `[2, 3]`. The comma-joined form
            // belongs to the PLURAL accessor and to response headers; the
            // singular one is `MessageHeader.findValue`, which yields one value.
            //
            // The synthetic arm below already did this -- it overwrites `found`
            // as it scans, so it keeps the last. Only this arm joined, so the
            // two halves of one function disagreed and the half that runs on a
            // REAL JDK image was the wrong one.
            r.headers
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case(&key))
                .map(|(_, v)| v.clone())
                .next_back()
        })
    } else if let Value::Object(Some(arr)) = ctx.get_field(this, HUC_REQ_HEADERS) {
        let len = ctx.array_length(arr);
        let mut found: Option<String> = None;
        for i in 0..len {
            if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
                let line = ctx.read_string(s).unwrap_or_default();
                if let Some(colon) = line.find(':') {
                    if line[..colon].trim().eq_ignore_ascii_case(&key) {
                        found = Some(line[colon + 1..].trim().to_string());
                    }
                }
            }
        }
        found
    } else {
        None
    };
    Ok(Some(match joined {
        Some(s) => Value::Object(Some(ctx.create_string(&s))),
        None => Value::Object(None),
    }))
}

/// `URLConnection.getRequestProperties()` — the REQUEST headers set on this
/// carrier so far, grouped like `getHeaderFields()` groups the response ones.
///
/// The real JDK reads its `sun.net.www.MessageHeader requests` field. Neither
/// CratonVM carrier populates that field — the synthetic one keeps its request
/// headers in `HUC_REQ_HEADERS`, the real one in the identity-keyed `RealReq`
/// table — so the inherited bytecode hits its own `requests == null` arm and
/// answers `Collections.emptyMap()` for a connection that has headers.
/// Silent, and wrong in the direction that reads as "no headers were set".
fn huc_get_request_properties(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let headers: Vec<(String, String)> = if is_real_carrier(ctx, this) {
        with_real_req(ctx, this, |r| r.headers.clone())
    } else if let Value::Object(Some(arr)) = ctx.get_field(this, HUC_REQ_HEADERS) {
        let len = ctx.array_length(arr);
        let mut out = Vec::new();
        for i in 0..len {
            if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
                let line = ctx.read_string(s).unwrap_or_default();
                if let Some(colon) = line.find(':') {
                    out.push((
                        line[..colon].trim().to_string(),
                        line[colon + 1..].trim().to_string(),
                    ));
                }
            }
        }
        out
    } else {
        Vec::new()
    };
    let map = build_header_map(ctx, &headers)?;
    Ok(Some(Value::Object(Some(map))))
}

fn huc_set_do_input(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
    if is_real_carrier(ctx, this) {
        // Mirror onto the real `URLConnection.doInput` field, for exactly the
        // reason `huc_set_do_output` below gives for `doOutput`: `getDoInput()`
        // is declared on `URLConnection`, is not overridden here, and is one
        // `getfield` -- so it answers from the REAL field and never consults a
        // per-class native of ours. Dropping the write left
        // `setDoInput(false); getDoInput()` answering `true` forever (MEASURED:
        // `probes/HucAccessors.java`, wrong in BOTH modes, right on HotSpot).
        //
        // The comment this replaces said "never write a synthetic slot on a
        // real object (it corrupts a real field)". That rule is right and is
        // not what this does: `set_field_by_name` resolves the REAL `doInput`
        // slot in the receiver's own hierarchy. Writing HUC_DO_INPUT --- a
        // synthetic INDEX --- is what would corrupt one, which is why that
        // write stays in the synthetic arm below.
        //
        // No `RealReq` entry: `perform` genuinely does not consult doInput, so
        // the field is the whole fix. `getInputStream()`'s own
        // `ProtocolException("Cannot read from URLConnection if doInput=false")`
        // guard reads that field, and could never fire while it was stale.
        ctx.set_field_by_name(this, "doInput", Value::Int(v));
        return Ok(None);
    }
    ctx.set_field(this, HUC_DO_INPUT, Value::Int(v));
    Ok(None)
}

fn huc_set_do_output(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    if is_real_carrier(ctx, this) {
        with_real_req(ctx, this, |r| r.do_output = v != 0);
        // Also mirror onto the real `URLConnection.doOutput` field so the
        // inherited `getDoOutput()` bytecode (declared on URLConnection, not
        // overridden here → our per-class native is never consulted for it)
        // returns the caller's value. Spring's SimpleClientHttpRequest gates the
        // request-body write on `getDoOutput()`; a stale `false` drops the body
        // and the server blocks on the promised Content-Length → "Read timed out".
        ctx.set_field_by_name(this, "doOutput", Value::Int(v));
        return Ok(None);
    }
    ctx.set_field(this, HUC_DO_OUTPUT, Value::Int(v));
    Ok(None)
}

fn huc_set_connect_timeout(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    if v < 0 {
        return Err(iae("setConnectTimeout: negative"));
    }
    if is_real_carrier(ctx, this) {
        let key = ctx.identity_hash_code(this);
        if let Ok(mut t) = real_reqs().lock() {
            t.entry(key).or_default().connect_timeout_ms = Some(v);
        }
        // Mirror onto the real `URLConnection.connectTimeout` field, for the
        // same reason `setDoOutput` mirrors `doOutput`: the inherited
        // `getConnectTimeout()` is one `getfield` and answers from THERE. The
        // side table above is what `perform` reads; the field is what the JDK's
        // own accessor reads, and a carrier that reports 0 (infinite) for a
        // timeout the caller just set is a silent lie.
        ctx.set_field_by_name(this, "connectTimeout", Value::Int(v));
        return Ok(None);
    }
    ctx.set_field(this, HUC_CONNECT_TIMEOUT, Value::Int(v));
    Ok(None)
}

fn huc_set_read_timeout(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    if v < 0 {
        return Err(iae("setReadTimeout: negative"));
    }
    if is_real_carrier(ctx, this) {
        let key = ctx.identity_hash_code(this);
        if let Ok(mut t) = real_reqs().lock() {
            t.entry(key).or_default().read_timeout_ms = Some(v);
        }
        // See `huc_set_connect_timeout` — the inherited `getReadTimeout()` is
        // one `getfield` and must not contradict the setter.
        ctx.set_field_by_name(this, "readTimeout", Value::Int(v));
        return Ok(None);
    }
    ctx.set_field(this, HUC_READ_TIMEOUT, Value::Int(v));
    Ok(None)
}

fn huc_set_fixed_length_streaming_mode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let length = match args.get(1) {
        Some(Value::Int(v)) => *v as i64,
        Some(Value::Long(v)) => *v,
        _ => return Err(iae("setFixedLengthStreamingMode: invalid length")),
    };
    if length < 0 {
        return Err(iae("setFixedLengthStreamingMode: negative length"));
    }
    if !is_real_carrier(ctx, this) {
        // Synthetic carriers retain their historical buffered implementation.
        return Ok(None);
    }
    let key = ctx.identity_hash_code(this);
    if live_fixed_streams()
        .lock()
        .ok()
        .is_some_and(|streams| streams.contains_key(&key))
    {
        return Err(ise("setFixedLengthStreamingMode: already connected"));
    }
    with_real_req(ctx, this, |req| match req.streaming {
        StreamingMode::Chunked(_) => Err(ise("Chunked encoding streaming mode set")),
        _ => {
            req.streaming = StreamingMode::Fixed(length as u64);
            Ok(())
        }
    })?;
    Ok(None)
}

fn huc_set_chunked_streaming_mode(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let chunk_length = args.get(1).and_then(Value::as_int).unwrap_or(0);
    if !is_real_carrier(ctx, this) {
        return Ok(None);
    }
    let key = ctx.identity_hash_code(this);
    if live_fixed_streams()
        .lock()
        .ok()
        .is_some_and(|streams| streams.contains_key(&key))
    {
        return Err(ise("setChunkedStreamingMode: already connected"));
    }
    with_real_req(ctx, this, |req| match req.streaming {
        StreamingMode::Fixed(_) => Err(ise("Fixed length streaming mode set")),
        _ => {
            req.streaming = StreamingMode::Chunked(chunk_length);
            Ok(())
        }
    })?;
    Ok(None)
}

fn huc_set_instance_follow_redirects(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
    if is_real_carrier(ctx, this) {
        with_real_req(ctx, this, |req| {
            req.follow_redirects = v != 0;
        });
        return Ok(None);
    }
    ctx.set_field(this, HUC_INSTANCE_FOLLOW_REDIRECTS, Value::Int(v));
    Ok(None)
}

fn huc_get_instance_follow_redirects(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if is_real_carrier(ctx, this) {
        let follow = real_reqs()
            .lock()
            .ok()
            .and_then(|t| {
                t.get(&ctx.identity_hash_code(this))
                    .map(|req| req.follow_redirects)
            })
            .unwrap_or(true);
        return Ok(Some(Value::Int(if follow { 1 } else { 0 })));
    }
    Ok(Some(ctx.get_field(this, HUC_INSTANCE_FOLLOW_REDIRECTS)))
}

fn huc_using_proxy(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // KEEP the constant `false`. This is not a stub standing in for an
    // unimplemented lookup: `perform`/`connect_plain` dial the origin host
    // directly and consult no `ProxySelector`, so "this connection is going
    // through a proxy" is genuinely false for every connection this class
    // makes. Returning true — or consulting `proxy_selector.rs` and reporting
    // what a proxy-aware client WOULD have done — would be the lie, and
    // callers branch on this to decide whether to send an absolute-form
    // request line or `Proxy-Authorization`. If the legacy path ever learns
    // to honour proxies, this must start reading the connection's own state.
    Ok(Some(Value::Int(0)))
}

// ---------------------------------------------------------------------------
// Public registration entry point
// ---------------------------------------------------------------------------

fn register_one(r: &mut NativeMethodRegistry, cls: &str) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    r.register(cls, "<init>", "(Ljava/net/URL;)V", huc_init);
    r.register(cls, "<init>", "()V", huc_init);
    r.register(cls, "connect", "()V", huc_connect);
    r.register(cls, "getResponseCode", "()I", huc_get_response_code);
    r.register(
        cls,
        "getResponseMessage",
        "()Ljava/lang/String;",
        huc_get_response_message,
    );
    r.register(
        cls,
        "getInputStream",
        "()Ljava/io/InputStream;",
        huc_get_input_stream,
    );
    r.register(
        cls,
        "getErrorStream",
        "()Ljava/io/InputStream;",
        huc_get_error_stream,
    );
    r.register(
        cls,
        "getOutputStream",
        "()Ljava/io/OutputStream;",
        huc_get_output_stream,
    );
    r.register(
        cls,
        "getHeaderField",
        "(Ljava/lang/String;)Ljava/lang/String;",
        huc_get_header_field_named,
    );
    r.register(
        cls,
        "getHeaderField",
        "(I)Ljava/lang/String;",
        huc_get_header_field_indexed,
    );
    r.register(
        cls,
        "getHeaderFieldKey",
        "(I)Ljava/lang/String;",
        huc_get_header_field_key_indexed,
    );
    r.register(
        cls,
        "getHeaderFields",
        "()Ljava/util/Map;",
        huc_get_header_fields,
    );
    r.register(cls, "getContentLength", "()I", huc_get_content_length);
    r.register(
        cls,
        "getContentLengthLong",
        "()J",
        huc_get_content_length_long,
    );
    r.register(cls, "disconnect", "()V", huc_disconnect);
    r.register(
        cls,
        "setRequestMethod",
        "(Ljava/lang/String;)V",
        huc_set_request_method,
    );
    r.register(
        cls,
        "getRequestMethod",
        "()Ljava/lang/String;",
        huc_get_request_method,
    );
    r.register(
        cls,
        "setRequestProperty",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        huc_set_request_property,
    );
    r.register(
        cls,
        "addRequestProperty",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        huc_add_request_property,
    );
    r.register(
        cls,
        "getRequestProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        huc_get_request_property,
    );
    r.register(cls, "setDoInput", "(Z)V", huc_set_do_input);
    r.register(cls, "setDoOutput", "(Z)V", huc_set_do_output);
    r.register(cls, "setConnectTimeout", "(I)V", huc_set_connect_timeout);
    r.register(cls, "setReadTimeout", "(I)V", huc_set_read_timeout);
    // Streaming-mode setters are no-ops: our `perform` buffers the request body
    // (via the overridden `getOutputStream` BAOS) and derives `Content-Length`
    // from the body / the converter's own header, so the JDK's fixed-length /
    // chunked streaming machinery is bypassed. The real setters guard on
    // `chunkLength != -1` / `fixedContentLengthLong != -1`, but our synthetically
    // constructed carrier never runs URLConnection's field initializers, so those
    // fields are 0 (not -1) and `setFixedLengthStreamingMode` would throw
    // `IllegalStateException("Chunked encoding streaming mode set")`. Spring's
    // `SimpleClientHttpRequest.executeInternal` calls this once `getDoOutput()` is
    // true — so it only surfaced after the doOutput fix let the body path run.
    r.register(
        cls,
        "setFixedLengthStreamingMode",
        "(I)V",
        huc_set_fixed_length_streaming_mode,
    );
    r.register(
        cls,
        "setFixedLengthStreamingMode",
        "(J)V",
        huc_set_fixed_length_streaming_mode,
    );
    r.register(
        cls,
        "setChunkedStreamingMode",
        "(I)V",
        huc_set_chunked_streaming_mode,
    );
    r.register(
        cls,
        "setInstanceFollowRedirects",
        "(Z)V",
        huc_set_instance_follow_redirects,
    );
    r.register(
        cls,
        "getInstanceFollowRedirects",
        "()Z",
        huc_get_instance_follow_redirects,
    );
    r.register(cls, "usingProxy", "()Z", huc_using_proxy);
    r.register(
        cls,
        "getRequestProperties",
        "()Ljava/util/Map;",
        huc_get_request_properties,
    );
    r.set_category(__prev_cat);
}

/// Run `super_class`'s OWN bytecode body for `name`/`desc` against `this`.
///
/// See [`register_https_delegate_forwarders`] for why. Uses
/// `invoke_special_bytecode_only`, which resolves statically on `super_class`'s
/// hierarchy and — the part that matters here — skips the native-registry
/// lookup, so a native registered for the same triple cannot re-enter itself.
/// Virtual calls the super body makes (`getHeaderField`, `checkConnected`, …)
/// still dispatch normally and therefore still land on CratonVM's registered
/// natives, which is what makes the derived getters answer from CratonVM's
/// state rather than from the JDK's unused fields.
fn https_forward_to_super(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    super_class: &str,
    name: &str,
    desc: &str,
) -> MethodCallResult {
    // A null receiver would have thrown at the call site; keep the same
    // diagnostic rather than passing it through to the interpreter.
    let _ = obj_arg(args, 0)?;
    ctx.invoke_special_bytecode_only(super_class, name, desc, args)
}

macro_rules! https_super_forwarders {
    ($r:expr, $cls:expr, [ $( ($fname:ident, $sup:expr, $name:expr, $desc:expr) ),+ $(,)? ]) => {
        $(
            fn $fname(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                https_forward_to_super(ctx, args, $sup, $name, $desc)
            }
            $r.register($cls, $name, $desc, $fname);
        )+
    };
}

/// `HttpsURLConnectionImpl`'s delegate-forwarding overrides, re-pointed at the
/// superclass bodies they exist to bypass.
///
/// **The problem.** `URL.openConnection()` on an `https:` URL hands back a
/// `sun.net.www.protocol.https.HttpsURLConnectionImpl` that CratonVM
/// ALLOCATES rather than CONSTRUCTS (see `net_phase_e.rs`'s carrier choice —
/// the concrete class is required so `instanceof HttpsURLConnection` and the
/// abstract session accessors both work). Its constructor is what creates the
/// `DelegateHttpsURLConnection` and stores it in `delegate`, so on this carrier
/// `delegate` is null — and essentially every method the class declares is
/// `getfield delegate; invokevirtual …`. `TomcatBaseTest.methodUrl` opens a
/// connection and calls `setUseCaches(false)` on it before anything else, which
/// is the whole SSL/TLS + OCSP portion of the Tomcat suite failing on
///
/// ```text
/// java.lang.NullPointerException: Cannot invoke
///   "sun.net.www.protocol.https.DelegateHttpsURLConnection.setUseCaches(boolean)"
///   because "this.delegate" is null
/// ```
///
/// **Why the plain-`http` carrier does not have this.** That carrier is
/// `java/net/HttpURLConnection`, which declares none of these — the calls land
/// on `java.net.URLConnection`'s own bytecode, which reads and writes the
/// object's OWN fields. That is exactly the behaviour CratonVM wants, because
/// `perform` reads those same fields. The https carrier differs from it in one
/// way only: the Impl overrides them to forward to a delegate that does not
/// exist here.
///
/// **So the fix is to make the override transparent**, not to re-implement 20
/// JDK methods. Each entry below runs the superclass body the Impl overrides —
/// `java/net/URLConnection` or `java/net/HttpURLConnection`, the two classes
/// actually in this carrier's chain (`sun.net.www.protocol.http.HttpURLConnection`
/// is NOT: `HttpsURLConnectionImpl extends javax.net.ssl.HttpsURLConnection
/// extends java.net.HttpURLConnection`). Writing bodies by hand instead would
/// be twenty chances to guess a JDK semantic wrong; this way `setAuthenticator`
/// throws the JDK's own `UnsupportedOperationException`, `getHeaderFieldDate`
/// applies the JDK's own `GMT`-suffix repair, and `getDefaultUseCaches` reads
/// the JDK's own static — none of which is written here.
///
/// **What is deliberately NOT registered**, and why the NPE is the right answer
/// for it: `setNewClient`/`setProxiedClient` (both arities). Those are the
/// JDK's internal plumbing for driving its own `sun.net.www` HTTP client, which
/// CratonVM's `perform` replaces wholesale. There is no superclass body to run
/// — they are declared on the Impl alone — and inventing a no-op would claim a
/// client was reconfigured when nothing happened. `getSSLSession` is also
/// absent, and that one is not a gap: `net_phase_e`'s registrar owns it (see
/// `register_https_session_accessors`' G7 note).
///
/// `getConnectTimeout`/`getReadTimeout` reach the inherited one-`getfield`
/// bodies, which is why `huc_set_connect_timeout`/`huc_set_read_timeout` now
/// mirror their values onto the real fields as well as into `RealReq`.
fn register_https_delegate_forwarders(r: &mut NativeMethodRegistry, cls: &str) {
    const UC: &str = "java/net/URLConnection";
    const HUC: &str = "java/net/HttpURLConnection";
    https_super_forwarders!(
        r,
        cls,
        [
            // --- URLConnection state the object owns ---
            (fwd_set_use_caches, UC, "setUseCaches", "(Z)V"),
            (fwd_get_use_caches, UC, "getUseCaches", "()Z"),
            (fwd_get_do_input, UC, "getDoInput", "()Z"),
            (fwd_get_do_output, UC, "getDoOutput", "()Z"),
            (
                fwd_set_allow_user_interaction,
                UC,
                "setAllowUserInteraction",
                "(Z)V"
            ),
            (
                fwd_get_allow_user_interaction,
                UC,
                "getAllowUserInteraction",
                "()Z"
            ),
            (fwd_set_if_modified_since, UC, "setIfModifiedSince", "(J)V"),
            (fwd_get_if_modified_since, UC, "getIfModifiedSince", "()J"),
            (fwd_get_default_use_caches, UC, "getDefaultUseCaches", "()Z"),
            (
                fwd_set_default_use_caches,
                UC,
                "setDefaultUseCaches",
                "(Z)V"
            ),
            (fwd_get_connect_timeout, UC, "getConnectTimeout", "()I"),
            (fwd_get_read_timeout, UC, "getReadTimeout", "()I"),
            (fwd_get_url, UC, "getURL", "()Ljava/net/URL;"),
            (fwd_to_string, UC, "toString", "()Ljava/lang/String;"),
            // --- derived from the response headers; each of these calls
            //     `getHeaderField` virtually, which lands on CratonVM's own
            //     registered native ---
            (
                fwd_get_content_type,
                UC,
                "getContentType",
                "()Ljava/lang/String;"
            ),
            (
                fwd_get_content_encoding,
                UC,
                "getContentEncoding",
                "()Ljava/lang/String;"
            ),
            (fwd_get_expiration, UC, "getExpiration", "()J"),
            (fwd_get_date, UC, "getDate", "()J"),
            (fwd_get_last_modified, UC, "getLastModified", "()J"),
            (
                fwd_get_header_field_int,
                UC,
                "getHeaderFieldInt",
                "(Ljava/lang/String;I)I"
            ),
            (
                fwd_get_header_field_long,
                UC,
                "getHeaderFieldLong",
                "(Ljava/lang/String;J)J"
            ),
            (fwd_get_content, UC, "getContent", "()Ljava/lang/Object;"),
            (
                fwd_get_content_typed,
                UC,
                "getContent",
                "([Ljava/lang/Class;)Ljava/lang/Object;"
            ),
            // --- HttpURLConnection's own concrete bodies ---
            (
                fwd_get_header_field_date,
                HUC,
                "getHeaderFieldDate",
                "(Ljava/lang/String;J)J"
            ),
            (
                fwd_get_permission,
                HUC,
                "getPermission",
                "()Ljava/security/Permission;"
            ),
            (
                fwd_set_authenticator,
                HUC,
                "setAuthenticator",
                "(Ljava/net/Authenticator;)V"
            ),
        ]
    );

    // The four below have NO superclass body to forward to — `isConnected` and
    // `setConnected` are declared on the Impl alone (the JDK's own hook for the
    // delegate to report its state back), and `equals`/`hashCode` would reach
    // `java.lang.Object`, whose identity semantics are what `URLConnection`
    // leaves in place anyway. Written against the object's own state instead,
    // which is the same answer the plain-`http` carrier gives.
    r.register(cls, "isConnected", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let connected = matches!(
            ctx.get_field_by_name(this, "connected"),
            Value::Int(v) if v != 0
        );
        Ok(Some(Value::Int(i32::from(connected))))
    });
    r.register(cls, "setConnected", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ctx.set_field_by_name(this, "connected", Value::Int(v));
        Ok(None)
    });
    r.register(cls, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ctx.identity_hash_code(this))))
    });
    r.register(cls, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let same = match args.get(1) {
            Some(Value::Object(Some(other))) => {
                ctx.identity_hash_code(*other) == ctx.identity_hash_code(this)
            }
            _ => false,
        };
        Ok(Some(Value::Int(i32::from(same))))
    });
}

pub fn register_http_url_connection_real(r: &mut NativeMethodRegistry) {
    install_baos_event_hook(huc_live_baos_event);
    // The input-side mirror. `native-io` has dispatched `BaisEvent` since
    // a1cfdb122 and nothing consumed it; this is the consumer that closes the
    // four `drain.conn.*` rows. Installing a hook is not a native
    // registration, so `bridge-ratchet.sh` and the baselines under `scripts/`
    // do not move — see `BaisEvent`'s "This adds no registration" note for the
    // designs that were rejected because they would have.
    cratonvm_native_api::registry::install_bais_event_hook(huc_live_bais_event);
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // The legacy `sun.net.www.protocol.http.HttpURLConnection` is the bulk of
    // the surface; the `https` variant subclasses it and overrides only TLS-
    // specific accessors. Both classes get the same native registrations so
    // an Https instance dispatches into the same connect() path with TLS.
    register_one(r, "sun/net/www/protocol/http/HttpURLConnection");
    register_one(r, "sun/net/www/protocol/https/HttpsURLConnectionImpl");
    // Some apps use the abstract base class directly via reflection.
    register_one(r, "java/net/HttpURLConnection");
    register_one(r, "javax/net/ssl/HttpsURLConnection");
    // The TLS-specific accessors, on the https classes only — `java.net`'s
    // plain `HttpURLConnection` does not declare them.
    register_https_session_accessors(r, "sun/net/www/protocol/https/HttpsURLConnectionImpl");
    register_https_session_accessors(r, "javax/net/ssl/HttpsURLConnection");
    // The Impl ALONE — `javax/net/ssl/HttpsURLConnection` declares none of
    // these and reaches the same superclass bodies by ordinary inheritance, so
    // registering there would insert a hop that changes nothing.
    register_https_delegate_forwarders(r, "sun/net/www/protocol/https/HttpsURLConnectionImpl");
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod http_url_connection_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// **Which of the two `register_https_session_accessors` functions owns
    /// each of the six names.**
    ///
    /// G7. There are two functions with that name — this file's and
    /// `net_phase_e`'s — registering the same six triples on the same two
    /// classes, and `lib.rs` calls net_phase_e's first (18688) and this file's
    /// second (18805), both inside `register_essential_natives_with_shims`.
    /// Registration is last-write-wins, so this file's five bodies are live and
    /// net_phase_e's five are dead, while `getSSLSession` — the one name this
    /// file does not register — stays net_phase_e's.
    ///
    /// net_phase_e's own comment reasons the opposite way and rules the
    /// overwrite out by checking only `register_one`. That is exactly the trap
    /// HANDOFF-20260814 §5 names: a correct body silently shadowed by a later
    /// registrar. This test makes the SPLIT itself executable, so that:
    ///
    ///   * adding `getSSLSession` here fails, instead of silently killing
    ///     net_phase_e's copy (the only one with a body for it); and
    ///   * removing any of the five here fails, instead of silently reviving
    ///     net_phase_e's — which reads a different table.
    ///
    /// Asserted on `register_http_url_connection_real` ALONE, which is what
    /// makes it a statement about this file rather than about a call order it
    /// cannot see.
    #[test]
    fn this_files_registrar_owns_five_of_the_six_https_session_accessors() {
        use cratonvm_native_api::NativeMethodRegistry;
        let mut r = NativeMethodRegistry::new();
        super::register_http_url_connection_real(&mut r);

        for cls in [
            "sun/net/www/protocol/https/HttpsURLConnectionImpl",
            "javax/net/ssl/HttpsURLConnection",
        ] {
            for (name, desc) in [
                (
                    "getServerCertificates",
                    "()[Ljava/security/cert/Certificate;",
                ),
                (
                    "getLocalCertificates",
                    "()[Ljava/security/cert/Certificate;",
                ),
                ("getCipherSuite", "()Ljava/lang/String;"),
                ("getPeerPrincipal", "()Ljava/security/Principal;"),
                ("getLocalPrincipal", "()Ljava/security/Principal;"),
            ] {
                assert!(
                    r.find(cls, name, desc).is_some(),
                    "{cls}.{name}{desc} must be registered by THIS file. \
                     lib.rs runs this registrar after net_phase_e's, so \
                     dropping it here does not restore the abstract \
                     declaration — it silently hands the door back to \
                     net_phase_e's copy, which answers from a different table \
                     (`https_carrier_session`, not `https_peer_info`)."
                );
            }
            assert!(
                r.find(cls, "getSSLSession", "()Ljava/util/Optional;")
                    .is_none(),
                "{cls}.getSSLSession()Ljava/util/Optional; must NOT be \
                 registered here. net_phase_e owns it precisely because this \
                 file leaves it alone; registering it here would run last and \
                 make net_phase_e's the dead copy. If you need to serve it \
                 from this file, move the whole family — do not split it \
                 further. See G7-1."
            );
        }
    }

    /// Every method `sun.net.www.protocol.https.HttpsURLConnectionImpl`
    /// declares must be answered by CratonVM, or be on the short list of
    /// methods that are deliberately left to NPE.
    ///
    /// The carrier `URL.openConnection()` hands back for an `https:` URL is
    /// ALLOCATED, not constructed, so its `delegate` field is null — and every
    /// method the class declares is `getfield delegate; invokevirtual …`. A
    /// declared method with no CratonVM registration is therefore not "falls
    /// back to the JDK", it is a guaranteed
    /// `NullPointerException: … because "this.delegate" is null`, which is how
    /// the entire SSL/TLS + OCSP portion of the Tomcat suite failed on
    /// `TomcatBaseTest.methodUrl`'s opening `setUseCaches(false)`.
    ///
    /// The list is `javap -p --module java.base
    /// sun.net.www.protocol.https.HttpsURLConnectionImpl` on Temurin JDK 25,
    /// minus `<init>` and the static `checkURL`. It is a constant because this
    /// test cannot read the JDK image; a JDK that adds a forwarder will not
    /// fail here, it will fail in the suite — which is exactly why the list
    /// carries the command that regenerates it.
    #[test]
    fn every_declared_https_impl_method_is_answered_or_explicitly_refused() {
        use cratonvm_native_api::NativeMethodRegistry;
        const CLS: &str = "sun/net/www/protocol/https/HttpsURLConnectionImpl";

        // Declared on the Impl and left UNREGISTERED on purpose.
        //
        // `setNewClient`/`setProxiedClient` drive the JDK's own `sun.net.www`
        // HTTP client, which `perform` replaces wholesale: there is no
        // superclass body to forward to (the Impl declares them alone) and a
        // no-op would claim a client was reconfigured when nothing happened.
        // The NPE is the honest answer — this path is not implemented.
        const DELIBERATELY_UNREGISTERED: &[(&str, &str)] = &[
            ("setNewClient", "(Ljava/net/URL;)V"),
            ("setNewClient", "(Ljava/net/URL;Z)V"),
            ("setProxiedClient", "(Ljava/net/URL;Ljava/lang/String;I)V"),
            ("setProxiedClient", "(Ljava/net/URL;Ljava/lang/String;IZ)V"),
        ];

        // Answered by `net_phase_e`'s registrar, not this file's — see the G7
        // note on `register_https_session_accessors`.
        const OWNED_BY_NET_PHASE_E: &[(&str, &str)] =
            &[("getSSLSession", "()Ljava/util/Optional;")];

        const DECLARED: &[(&str, &str)] = &[
            ("setNewClient", "(Ljava/net/URL;)V"),
            ("setNewClient", "(Ljava/net/URL;Z)V"),
            ("setProxiedClient", "(Ljava/net/URL;Ljava/lang/String;I)V"),
            ("setProxiedClient", "(Ljava/net/URL;Ljava/lang/String;IZ)V"),
            ("connect", "()V"),
            ("isConnected", "()Z"),
            ("setConnected", "(Z)V"),
            ("getCipherSuite", "()Ljava/lang/String;"),
            (
                "getLocalCertificates",
                "()[Ljava/security/cert/Certificate;",
            ),
            (
                "getServerCertificates",
                "()[Ljava/security/cert/Certificate;",
            ),
            ("getPeerPrincipal", "()Ljava/security/Principal;"),
            ("getLocalPrincipal", "()Ljava/security/Principal;"),
            ("getOutputStream", "()Ljava/io/OutputStream;"),
            ("getInputStream", "()Ljava/io/InputStream;"),
            ("getErrorStream", "()Ljava/io/InputStream;"),
            ("disconnect", "()V"),
            ("usingProxy", "()Z"),
            ("getHeaderFields", "()Ljava/util/Map;"),
            ("getHeaderField", "(Ljava/lang/String;)Ljava/lang/String;"),
            ("getHeaderField", "(I)Ljava/lang/String;"),
            ("getHeaderFieldKey", "(I)Ljava/lang/String;"),
            (
                "setRequestProperty",
                "(Ljava/lang/String;Ljava/lang/String;)V",
            ),
            (
                "addRequestProperty",
                "(Ljava/lang/String;Ljava/lang/String;)V",
            ),
            ("getResponseCode", "()I"),
            (
                "getRequestProperty",
                "(Ljava/lang/String;)Ljava/lang/String;",
            ),
            ("getRequestProperties", "()Ljava/util/Map;"),
            ("setInstanceFollowRedirects", "(Z)V"),
            ("getInstanceFollowRedirects", "()Z"),
            ("setRequestMethod", "(Ljava/lang/String;)V"),
            ("getRequestMethod", "()Ljava/lang/String;"),
            ("getResponseMessage", "()Ljava/lang/String;"),
            ("getHeaderFieldDate", "(Ljava/lang/String;J)J"),
            ("getPermission", "()Ljava/security/Permission;"),
            ("getURL", "()Ljava/net/URL;"),
            ("getContentLength", "()I"),
            ("getContentLengthLong", "()J"),
            ("getContentType", "()Ljava/lang/String;"),
            ("getContentEncoding", "()Ljava/lang/String;"),
            ("getExpiration", "()J"),
            ("getDate", "()J"),
            ("getLastModified", "()J"),
            ("getHeaderFieldInt", "(Ljava/lang/String;I)I"),
            ("getHeaderFieldLong", "(Ljava/lang/String;J)J"),
            ("getContent", "()Ljava/lang/Object;"),
            ("getContent", "([Ljava/lang/Class;)Ljava/lang/Object;"),
            ("toString", "()Ljava/lang/String;"),
            ("setDoInput", "(Z)V"),
            ("getDoInput", "()Z"),
            ("setDoOutput", "(Z)V"),
            ("getDoOutput", "()Z"),
            ("setAllowUserInteraction", "(Z)V"),
            ("getAllowUserInteraction", "()Z"),
            ("setUseCaches", "(Z)V"),
            ("getUseCaches", "()Z"),
            ("setIfModifiedSince", "(J)V"),
            ("getIfModifiedSince", "()J"),
            ("getDefaultUseCaches", "()Z"),
            ("setDefaultUseCaches", "(Z)V"),
            ("equals", "(Ljava/lang/Object;)Z"),
            ("hashCode", "()I"),
            ("setConnectTimeout", "(I)V"),
            ("getConnectTimeout", "()I"),
            ("setReadTimeout", "(I)V"),
            ("getReadTimeout", "()I"),
            ("setFixedLengthStreamingMode", "(I)V"),
            ("setFixedLengthStreamingMode", "(J)V"),
            ("setChunkedStreamingMode", "(I)V"),
            ("setAuthenticator", "(Ljava/net/Authenticator;)V"),
            ("getSSLSession", "()Ljava/util/Optional;"),
        ];

        let mut r = NativeMethodRegistry::new();
        super::register_http_url_connection_real(&mut r);

        let mut missing: Vec<String> = Vec::new();
        for &(name, desc) in DECLARED {
            if DELIBERATELY_UNREGISTERED.contains(&(name, desc))
                || OWNED_BY_NET_PHASE_E.contains(&(name, desc))
            {
                assert!(
                    r.find(CLS, name, desc).is_none(),
                    "{CLS}.{name}{desc} is on an exemption list but IS registered \
                     here — move it out of the list, or out of this registrar."
                );
                continue;
            }
            if r.find(CLS, name, desc).is_none() {
                missing.push(format!("{name}{desc}"));
            }
        }
        assert!(
            missing.is_empty(),
            "{} declared method(s) of {CLS} reach the JDK's own \
             `getfield delegate; invokevirtual …` body on a carrier whose \
             `delegate` is null, i.e. throw NullPointerException: {missing:?}. \
             Add them to `register_https_delegate_forwarders` (forwarding to \
             the superclass body the Impl overrides), or state why the NPE is \
             the right answer and list them in DELIBERATELY_UNREGISTERED.",
            missing.len()
        );
    }

    /// rustls's TLS 1.3 spelling is not JSSE's, and `getCipherSuite()` is
    /// contracted to answer JSSE's. Both directions asserted: the five
    /// `TLS13_*` variants are rewritten, and a TLS 1.2 name — where the two
    /// already agree — must pass through untouched. Oracle for the expected
    /// strings: `scratchpad/c12/C12Probe.java` §B on HotSpot 25.
    #[test]
    fn tls13_suite_names_are_reported_with_jsse_spelling() {
        assert_eq!(
            jsse_cipher_suite_name("TLS13_AES_256_GCM_SHA384"),
            "TLS_AES_256_GCM_SHA384"
        );
        assert_eq!(
            jsse_cipher_suite_name("TLS13_AES_128_GCM_SHA256"),
            "TLS_AES_128_GCM_SHA256"
        );
        assert_eq!(
            jsse_cipher_suite_name("TLS13_CHACHA20_POLY1305_SHA256"),
            "TLS_CHACHA20_POLY1305_SHA256"
        );
        assert_eq!(
            jsse_cipher_suite_name("TLS13_AES_128_CCM_8_SHA256"),
            "TLS_AES_128_CCM_8_SHA256"
        );
        // TLS 1.2: already the registry name on both sides.
        assert_eq!(
            jsse_cipher_suite_name("TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"),
            "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"
        );
        // Not a blanket "TLS" rewrite: only the prefix, and only when present.
        assert_eq!(jsse_cipher_suite_name("UNKNOWN"), "UNKNOWN");
    }

    /// SOURCE WITNESS — the session capture must stay ABOVE the early return.
    ///
    /// `huc_verify_hostname` ends STEP 1 with `if builtin.is_ok() { return
    /// Ok(()); }`, and that return is the path EVERY SUCCESSFUL REQUEST TAKES.
    /// A `record_https_carrier_session` call below it would record a session
    /// only for connections whose built-in hostname check FAILED — i.e. it
    /// would pass any probe that deliberately breaks verification and capture
    /// nothing in production, leaving all six `HttpsURLConnection` session
    /// accessors answering `IllegalStateException: connection not yet open`
    /// forever. No behavioural test can see that difference without a live TLS
    /// peer, so the ordering is asserted against the source.
    ///
    /// Reads the WORKING TREE rather than an `include_str!` snapshot, so it
    /// tracks the file someone is editing, and skips rather than fails if the
    /// source is not on disk (a packaged build). Line endings are normalised
    /// because this repository is edited from both Windows and Linux.
    #[test]
    fn the_session_capture_precedes_the_success_path_early_return() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("http_url_connection.rs");
        let Ok(src) = std::fs::read_to_string(&path) else {
            println!("http_url_connection.rs not on disk at {path:?}; witness skipped");
            return;
        };
        let lines: Vec<&str> = src.lines().map(|l| l.trim_end_matches('\r')).collect();

        let fn_start = lines
            .iter()
            .position(|l| l.starts_with("fn huc_verify_hostname("))
            .expect("huc_verify_hostname must still exist");
        // The function body ends at the next top-level `}`.
        let fn_end = lines
            .iter()
            .enumerate()
            .skip(fn_start)
            .find(|(_, l)| **l == "}")
            .map(|(i, _)| i)
            .expect("huc_verify_hostname must be terminated");
        let body = &lines[fn_start..fn_end];

        let capture = body
            .iter()
            .position(|l| l.contains("record_https_carrier_session("))
            .expect(
                "huc_verify_hostname must record the negotiated session; without it every \
                 HttpsURLConnection session accessor answers \"connection not yet open\"",
            );
        let early_return = body
            .iter()
            .position(|l| l.trim() == "if builtin.is_ok() {")
            .expect("STEP 1's success-path early return must still be recognisable");

        assert!(
            capture < early_return,
            "record_https_carrier_session is at body line {capture}, BELOW the \
             `if builtin.is_ok()` early return at body line {early_return} — that is the \
             path every successful request takes, so the capture would only ever fire for \
             connections whose hostname check FAILED. Move it back above STEP 1."
        );
    }

    /// SOURCE WITNESS — G44 N1: the verifier gets THE carrier's session, and
    /// the local mint is only ever the fallback.
    ///
    /// MEASURED, `RSslLiveSession` on `9ae371468`:
    /// `verifier.sameObjectAsGetSSLSession = false  WANT true`. HotSpot hands
    /// `HostnameVerifier.verify` the same object `getSSLSession()` returns
    /// afterwards, and two minters cannot satisfy that however identical their
    /// field writes are — which is why the fix is a lookup and not a fifth copy
    /// of the same four `set_field` calls.
    ///
    /// Asserted against the source for the same reason the witness above is:
    /// the difference between one object and two is only observable with a live
    /// TLS peer, so no `MockNativeContext` test can see it. What IS checkable
    /// here is the shape — that `huc_verify_hostname` consults
    /// `https_carrier_session_object` and that the only remaining
    /// `try_alloc_concurrent_synthetic` of a session in this file sits inside
    /// the fallback helper, not in the verifier path itself.
    #[test]
    fn the_verifier_is_handed_the_carriers_session_not_a_second_one() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("http_url_connection.rs");
        let Ok(src) = std::fs::read_to_string(&path) else {
            println!("http_url_connection.rs not on disk at {path:?}; witness skipped");
            return;
        };
        let lines: Vec<&str> = src.lines().map(|l| l.trim_end_matches('\r')).collect();
        let fn_start = lines
            .iter()
            .position(|l| l.starts_with("fn huc_verify_hostname("))
            .expect("huc_verify_hostname must still exist");
        let fn_end = lines
            .iter()
            .enumerate()
            .skip(fn_start)
            .find(|(_, l)| **l == "}")
            .map(|(i, _)| i)
            .expect("huc_verify_hostname must be terminated");
        // CODE lines only. This function's body is more comment than code, and
        // both assertions below would otherwise be satisfied — or broken — by
        // prose that merely names the function.
        let body: Vec<&str> = lines[fn_start..fn_end]
            .iter()
            .copied()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect();

        assert!(
            body.iter()
                .any(|l| l.contains("https_carrier_session_object(")),
            "huc_verify_hostname must ask net_phase_e for THE session this carrier already \
             handed out. Minting a private one here is what made \
             `verifier.sameObjectAsGetSSLSession` answer false."
        );
        assert!(
            !body
                .iter()
                .any(|l| l.contains("try_alloc_concurrent_synthetic(")),
            "huc_verify_hostname must not allocate an SSLSession itself — the fallback lives \
             in huc_mint_verifier_session, so that the carrier's session is the DEFAULT and \
             the private mint is the exception."
        );
    }

    /// `disconnect()` tears down the connection-level view of BOTH https
    /// session tables — and leaves this file's row in place rather than
    /// deleting it.
    ///
    /// MEASURED, HotSpot (`G7-1` §1d): after `disconnect()` all six accessors
    /// throw `IllegalStateException: connection not yet open` again, the same
    /// exception a never-handshaked connection throws. Two halves are asserted
    /// here because each is a separate way to get this wrong:
    ///
    ///   * the row must SURVIVE, flagged. Removing it would put
    ///     `https_ensure_exchanged` back on its "never handshaked" path, and
    ///     the very next accessor would re-issue the HTTPS request over the
    ///     network and answer from the fresh entry — a wrong answer AND a
    ///     second request;
    ///   * `net_phase_e`'s carrier session must go, because `getSSLSession` is
    ///     the one of the six that reads THAT table (`G7-1` §5.1). Recycling
    ///     one table and not the other leaves the six disagreeing about whether
    ///     the connection is open.
    /// G51-1 N2 — draining the response body recycles the connection's view,
    /// the way HotSpot's `KeepAliveCache` does at the same instant.
    ///
    /// Drives the observer directly rather than through `native-io`: the
    /// dispatch sites are that crate's and already have their own tests
    /// (`native-io/src/lib.rs`, the `BaisEvent` recorder). What is this file's
    /// to prove is that the observer maps a stream back to its carrier, that
    /// it recycles exactly once, and that an unregistered stream is inert.
    #[test]
    fn draining_the_response_body_recycles_the_carrier() {
        use cratonvm_native_api::registry::BaisEvent;
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let carrier = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        let stream = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        let key = ctx.identity_hash_code(carrier) as u32 as u64;
        let chain = vec![vec![0x30u8, 0x01, 0x02]];

        record_https_peer_info(&ctx, Some(carrier), &chain, "TLS_AES_256_GCM_SHA384");
        note_response_stream(&ctx, stream, Some(carrier));
        assert!(
            https_peer_info()
                .lock()
                .unwrap()
                .get(&key)
                .is_some_and(|i| !i.recycled),
            "a completed exchange starts OPEN"
        );

        huc_live_bais_event(&mut ctx, stream, BaisEvent::Eof).expect("the observer must not fail");
        assert!(
            https_peer_info()
                .lock()
                .unwrap()
                .get(&key)
                .is_some_and(|i| i.recycled),
            "at body EOF the CONNECTION-level view is torn down — every accessor throws              IllegalStateException: connection not yet open again"
        );
        assert!(
            https_peer_info().lock().unwrap().contains_key(&key),
            "the ROW must survive: https_ensure_exchanged reads a missing entry as              \"never handshaked\" and would re-issue the request over the network"
        );

        // Eof fires on EVERY exhausted read and a closed stream produces Close
        // as well, so the second and third events must find nothing to do.
        assert!(
            https_response_streams()
                .lock()
                .unwrap()
                .get(&(ctx.identity_hash_code(stream) as u32 as u64))
                .is_none(),
            "the association is consumed by the first event"
        );
        huc_live_bais_event(&mut ctx, stream, BaisEvent::Close).expect("idempotent");
    }

    /// A stream that was never associated with an `https` carrier must be
    /// inert. Every `ByteArrayInputStream` in the process reaches this
    /// observer — a plain `http:` body, an application's own buffer, a
    /// resource read through `URLClassLoader` — and recycling anything for
    /// those would tear down state they have nothing to do with.
    #[test]
    fn an_unassociated_stream_recycles_nothing() {
        use cratonvm_native_api::registry::BaisEvent;
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let carrier = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        let stranger = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        let key = ctx.identity_hash_code(carrier) as u32 as u64;

        record_https_peer_info(
            &ctx,
            Some(carrier),
            &[vec![0x30u8]],
            "TLS_AES_128_GCM_SHA256",
        );
        // Deliberately NOT noted, and noted with no carrier — both are the
        // shapes an ordinary BAIS arrives in.
        note_response_stream(&ctx, stranger, None);
        huc_live_bais_event(&mut ctx, stranger, BaisEvent::Eof).expect("inert");
        assert!(
            https_peer_info()
                .lock()
                .unwrap()
                .get(&key)
                .is_some_and(|i| !i.recycled),
            "an unrelated stream's EOF must not recycle a live connection"
        );
    }

    #[test]
    fn disconnect_recycles_both_https_session_tables() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let carrier = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        let key = ctx.identity_hash_code(carrier) as u32 as u64;
        let chain = vec![vec![0x30u8, 0x01, 0x02]];

        record_https_peer_info(&ctx, Some(carrier), &chain, "TLS_AES_256_GCM_SHA384");
        crate::net_phase_e::record_https_carrier_session(
            &mut ctx,
            carrier,
            "TLSv1.3",
            "TLS_AES_256_GCM_SHA384",
            &chain,
            "example.test",
            443,
        );
        assert!(
            https_peer_info()
                .lock()
                .unwrap()
                .get(&key)
                .is_some_and(|i| !i.recycled),
            "a completed exchange starts OPEN"
        );

        https_recycle_carrier(&mut ctx, carrier);

        let table = https_peer_info().lock().unwrap();
        let info = table
            .get(&key)
            .expect("the row must survive the recycle — see this test's doc comment");
        assert!(info.recycled, "the row must be flagged, not merely present");
        assert_eq!(
            info.cipher, "TLS_AES_256_GCM_SHA384",
            "recycling reports the connection closed; it does not forge the handshake's data"
        );
        drop(table);
        assert!(
            crate::net_phase_e::https_carrier_session_object(&mut ctx, carrier).is_none(),
            "getSSLSession's table must have been evicted too, or five accessors report \
             `connection not yet open` while the sixth still hands out a session"
        );

        // Idempotent: `disconnect()` is documented as callable twice, and the
        // second call must not release a global root a second time.
        https_recycle_carrier(&mut ctx, carrier);

        // A connection that genuinely re-handshakes is OPEN again — the flag is
        // cleared by the recorder, not sticky on the carrier's identity.
        record_https_peer_info(&ctx, Some(carrier), &chain, "TLS_AES_128_GCM_SHA256");
        assert!(
            https_peer_info()
                .lock()
                .unwrap()
                .get(&key)
                .is_some_and(|i| !i.recycled),
            "re-recording a handshake must clear the recycled flag"
        );
        https_peer_info().lock().unwrap().remove(&key);
    }

    /// SOURCE WITNESS — `https_ensure_exchanged`'s early return must stay a
    /// presence test, not a liveness test.
    ///
    /// It is the one line that keeps a recycled carrier from re-issuing its
    /// HTTPS request: "no entry" means "never handshaked, go and handshake". A
    /// later change that made this read `.get(..).is_some_and(|i| !i.recycled)`
    /// — which is exactly the shape the three ACCESSORS were just given, so it
    /// looks like consistency — would drive a second network request from the
    /// next accessor call and repopulate the table with a live entry.
    #[test]
    fn the_lazy_exchange_guard_is_a_presence_test_not_a_liveness_test() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("http_url_connection.rs");
        let Ok(src) = std::fs::read_to_string(&path) else {
            println!("http_url_connection.rs not on disk at {path:?}; witness skipped");
            return;
        };
        let lines: Vec<&str> = src.lines().map(|l| l.trim_end_matches('\r')).collect();
        let fn_start = lines
            .iter()
            // `_body` since `WORKER-5-NOTE-13`: `https_ensure_exchanged` is now
            // the `&mut ObjectRef` wrapper and the guard this witness is about
            // lives in the body half. `expect` keeps a further rename loud.
            .position(|l| l.starts_with("fn https_ensure_exchanged_body("))
            .expect("https_ensure_exchanged_body must still exist");
        let fn_end = lines
            .iter()
            .enumerate()
            .skip(fn_start)
            .find(|(_, l)| **l == "}")
            .map(|(i, _)| i)
            .expect("https_ensure_exchanged_body must be terminated");
        let body = &lines[fn_start..fn_end];

        assert!(
            body.iter().any(|l| l.contains("contains_key(")),
            "https_ensure_exchanged's guard must be a plain presence test"
        );
        assert!(
            !body.iter().any(|l| l.contains("recycled")),
            "https_ensure_exchanged must NOT skip recycled rows — treating a recycled row as \
             absent makes the next accessor re-issue the HTTPS request and answer from the \
             fresh entry. The flag is read by the accessors, never by this guard."
        );
    }

    /// A pooled keep-alive connection must actually be CLOSED once it is past
    /// `POOL_IDLE_WINDOW`, not merely become ineligible for reuse.
    ///
    /// Before the reaper existed, `pool_take` was the only thing that dropped a
    /// stale entry, so a client that made one request and stopped held its
    /// socket open for the life of the VM — visible to the peer, and what broke
    /// `MockWebServer.close()` (see `pool_start_reaper`). This asserts from the
    /// server side, which is the side that noticed.
    #[test]
    fn pooled_connection_is_closed_once_idle() {
        use std::io::Read;
        use std::net::{TcpListener, TcpStream as Stream};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let client = Stream::connect(("127.0.0.1", port)).expect("connect");
        let (mut accepted, _) = listener.accept().expect("accept");

        pool_put("127.0.0.1", port, client);

        // The peer must observe EOF within a bounded window: the reaper only
        // discards entries older than POOL_IDLE_WINDOW, so allow that plus
        // several sweep ticks of slack.
        accepted
            .set_read_timeout(Some(POOL_IDLE_WINDOW + Duration::from_secs(5)))
            .expect("set_read_timeout");
        let mut buf = [0u8; 1];
        let observed = accepted.read(&mut buf);
        assert!(
            matches!(observed, Ok(0)),
            "peer should see EOF after the pooled connection goes idle, got {observed:?}"
        );
    }

    /// A `localhost` leaf with `SAN dNSName=localhost` (plus `foo.test`,
    /// `bar.test`, `IP 127.0.0.1`) — the same fixture the rustls loopback
    /// self-test serves, and the same shape every Spring Boot / Tomcat TLS
    /// test's keystore presents.
    fn localhost_leaf_der() -> Vec<u8> {
        let pem = include_str!("t27_certs/server.crt");
        crate::t27_tls::parse_cert_chain_pem(pem)
            .expect("t27_certs/server.crt must parse")
            .remove(0)
            .as_ref()
            .to_vec()
    }

    /// The regression this whole change exists for.
    ///
    /// The real JDK's `HttpsURLConnection.<clinit>` installs
    /// `HttpsURLConnection$DefaultHostnameVerifier`, whose entire body is
    /// `iconst_0; ireturn`, and every `HttpsURLConnection` instance inherits it
    /// through the constructor. Failing to recognise that class as a default
    /// stand-in — as the first version of this code did, which knew only about
    /// the VM's own bare-interface synthetic — turns its unconditional `false`
    /// into a rejection of every single https request.
    ///
    /// Deliberately spelled out as literals rather than derived: this is a
    /// name-matching contract with the JDK (real JSSE matches the same class by
    /// canonical name in `HttpsClient.afterConnect`), so a rename must break a
    /// test rather than silently fall through to "app verifier".
    #[test]
    fn jdk_default_hostname_verifier_is_recognised_as_a_non_check() {
        assert!(is_default_hostname_verifier(Some(
            "javax/net/ssl/HttpsURLConnection$DefaultHostnameVerifier"
        )));
        assert!(is_default_hostname_verifier(Some(
            "javax/net/ssl/HostnameVerifier"
        )));
        // NOT `None`. By the time a name reaches this predicate the caller has
        // already returned for "no verifier installed at all"
        // (`let Some(verifier0) = verifier0 else { ... }`), so `None` here means
        // "a verifier object exists whose class this VM could not name" — a
        // lambda, which is an APPLICATION verifier and must be consulted.
        // `an_unnameable_verifier_is_not_a_default_stand_in` is the regression
        // guard for exactly that, with the HotSpot measurement behind it; this
        // line used to assert the opposite and the two directly contradicted
        // each other.
        //
        // Anything else is a genuine application verifier and must be
        // consulted (as a fallback) rather than skipped.
        assert!(!is_default_hostname_verifier(Some("com/example/PinningHV")));
        assert!(!is_default_hostname_verifier(Some(
            "org/apache/http/conn/ssl/NoopHostnameVerifier"
        )));
    }

    /// The built-in RFC 2818 check must ACCEPT the ordinary loopback case on
    /// its own, before any verifier is consulted — that is what makes the
    /// verifier a fallback rather than a gate. If this ever returns `Err`, the
    /// JDK default verifier's hardcoded `false` becomes the deciding answer
    /// again and every https request fails.
    #[test]
    fn builtin_endpoint_identification_accepts_a_localhost_leaf() {
        let chain = vec![localhost_leaf_der()];
        assert_eq!(
            huc_builtin_endpoint_identification("localhost", &chain),
            Ok(())
        );
        // Case-insensitive, per RFC 6125.
        assert_eq!(
            huc_builtin_endpoint_identification("LOCALHOST", &chain),
            Ok(())
        );
        // The same leaf's IP SAN.
        assert_eq!(
            huc_builtin_endpoint_identification("127.0.0.1", &chain),
            Ok(())
        );
    }

    /// ...and must REJECT a host the leaf does not assert, so the fallback to
    /// the installed verifier is actually reachable. A check that always
    /// passed would silently disable app-supplied verifiers altogether.
    #[test]
    fn builtin_endpoint_identification_rejects_a_foreign_host_and_an_empty_chain() {
        let chain = vec![localhost_leaf_der()];
        assert!(huc_builtin_endpoint_identification("evil.example.com", &chain).is_err());
        assert!(huc_builtin_endpoint_identification("localhost", &[]).is_err());
        assert!(huc_builtin_endpoint_identification("localhost", &[vec![0u8; 8]]).is_err());
    }

    #[test]
    fn test_register_http_url_connection_init() {
        let mut r = NativeMethodRegistry::new();
        register_http_url_connection_real(&mut r);
        assert!(r
            .find(
                "sun/net/www/protocol/http/HttpURLConnection",
                "<init>",
                "(Ljava/net/URL;)V"
            )
            .is_some());
    }

    #[test]
    fn test_register_https_url_connection_impl_init() {
        let mut r = NativeMethodRegistry::new();
        register_http_url_connection_real(&mut r);
        assert!(r
            .find(
                "sun/net/www/protocol/https/HttpsURLConnectionImpl",
                "<init>",
                "(Ljava/net/URL;)V"
            )
            .is_some());
    }

    #[test]
    fn test_register_huc_connect_methods() {
        let mut r = NativeMethodRegistry::new();
        register_http_url_connection_real(&mut r);
        let cls = "sun/net/www/protocol/http/HttpURLConnection";
        assert!(r.find(cls, "connect", "()V").is_some());
        assert!(r.find(cls, "getResponseCode", "()I").is_some());
        assert!(r
            .find(cls, "getInputStream", "()Ljava/io/InputStream;")
            .is_some());
        assert!(r
            .find(cls, "getOutputStream", "()Ljava/io/OutputStream;")
            .is_some());
        assert!(r.find(cls, "disconnect", "()V").is_some());
    }

    #[test]
    fn test_register_huc_header_methods() {
        let mut r = NativeMethodRegistry::new();
        register_http_url_connection_real(&mut r);
        let cls = "sun/net/www/protocol/http/HttpURLConnection";
        assert!(r
            .find(
                cls,
                "getHeaderField",
                "(Ljava/lang/String;)Ljava/lang/String;"
            )
            .is_some());
        assert!(r
            .find(cls, "getHeaderField", "(I)Ljava/lang/String;")
            .is_some());
        assert!(r
            .find(cls, "getHeaderFieldKey", "(I)Ljava/lang/String;")
            .is_some());
        assert!(r.find(cls, "getContentLength", "()I").is_some());
        assert!(r.find(cls, "getContentLengthLong", "()J").is_some());
    }

    #[test]
    fn test_register_huc_request_property_methods() {
        let mut r = NativeMethodRegistry::new();
        register_http_url_connection_real(&mut r);
        let cls = "sun/net/www/protocol/http/HttpURLConnection";
        assert!(r
            .find(
                cls,
                "setRequestProperty",
                "(Ljava/lang/String;Ljava/lang/String;)V"
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "addRequestProperty",
                "(Ljava/lang/String;Ljava/lang/String;)V"
            )
            .is_some());
        assert!(r
            .find(cls, "setRequestMethod", "(Ljava/lang/String;)V")
            .is_some());
        assert!(r
            .find(cls, "getRequestMethod", "()Ljava/lang/String;")
            .is_some());
    }

    #[test]
    fn test_parse_url_http() {
        let p = parse_url("http://example.com/foo").unwrap();
        assert_eq!(p.scheme, "http");
        assert_eq!(p.host, "example.com");
        assert_eq!(p.port, 80);
        assert_eq!(p.path, "/foo");
    }

    #[test]
    fn test_parse_url_https_with_port() {
        let p = parse_url("https://example.com:8443").unwrap();
        assert_eq!(p.scheme, "https");
        assert_eq!(p.host, "example.com");
        assert_eq!(p.port, 8443);
        assert_eq!(p.path, "/");
    }

    #[test]
    fn test_parse_url_query_only_and_fragment() {
        let p = parse_url("http://localhost:8080?trace=false&message=false").unwrap();
        assert_eq!(p.host, "localhost");
        assert_eq!(p.port, 8080);
        assert_eq!(p.path, "/?trace=false&message=false");

        let p = parse_url("https://example.com:8443#client-only").unwrap();
        assert_eq!(p.host, "example.com");
        assert_eq!(p.port, 8443);
        assert_eq!(p.path, "/");
    }

    #[test]
    fn test_parse_url_rejects_unknown_scheme() {
        assert!(parse_url("ftp://example.com").is_err());
    }

    #[test]
    fn test_parse_url_rejects_empty_host() {
        assert!(parse_url("http:///foo").is_err());
    }

    #[test]
    fn test_build_request_get() {
        let p = parse_url("http://example.com/foo").unwrap();
        let req = build_request("GET", &p, &[], &[]);
        let s = String::from_utf8(req).unwrap();
        assert!(s.starts_with("GET /foo HTTP/1.1\r\n"));
        assert!(s.contains("Host: example.com\r\n"));
        assert!(s.contains("User-Agent: Java/CratonVM\r\n"));
        assert!(s.contains("Connection: keep-alive\r\n"));
    }

    #[test]
    fn test_build_request_post_body() {
        let p = parse_url("https://example.com:8443/api").unwrap();
        let req = build_request(
            "POST",
            &p,
            &[("Content-Type".to_string(), "application/json".to_string())],
            b"{}",
        );
        let s = String::from_utf8(req).unwrap();
        assert!(s.contains("POST /api HTTP/1.1\r\n"));
        assert!(s.contains("Host: example.com:8443\r\n"));
        assert!(s.contains("Content-Length: 2\r\n"));
        assert!(s.contains("Content-Type: application/json\r\n"));
        assert!(s.ends_with("{}"));
    }

    #[test]
    fn test_parse_url_userinfo_stripped() {
        // `http://alice:secret@localhost:8080/resource` — the user-info must be
        // stripped from host/port (connect target, Host header) and surfaced in
        // `userinfo` (ResourceTests.useUserInfoToSetBasicAuth).
        let p = parse_url("http://alice:secret@localhost:8080/resource").unwrap();
        assert_eq!(p.scheme, "http");
        assert_eq!(p.host, "localhost");
        assert_eq!(p.port, 8080);
        assert_eq!(p.path, "/resource");
        assert_eq!(p.userinfo.as_deref(), Some("alice:secret"));
        // No user-info → None.
        assert_eq!(parse_url("http://example.com/x").unwrap().userinfo, None);
    }

    #[test]
    fn test_build_request_preemptive_basic_auth_from_userinfo() {
        let p = parse_url("http://alice:secret@localhost:8080/resource").unwrap();
        let req = build_request("GET", &p, &[], b"");
        let s = String::from_utf8(req).unwrap();
        // Host header must NOT carry the user-info.
        assert!(s.contains("Host: localhost:8080\r\n"));
        // base64("alice:secret") == "YWxpY2U6c2VjcmV0" (preemptive Basic auth).
        assert!(s.contains("Authorization: Basic YWxpY2U6c2VjcmV0\r\n"));

        // An explicitly staged Authorization header wins over the user-info.
        let req2 = build_request(
            "GET",
            &p,
            &[("Authorization".to_string(), "Bearer tok".to_string())],
            b"",
        );
        let s2 = String::from_utf8(req2).unwrap();
        assert!(s2.contains("Authorization: Bearer tok\r\n"));
        assert!(!s2.contains("Basic YWxpY2U6c2VjcmV0"));
    }

    #[test]
    fn test_content_length_prefers_header_over_body() {
        // HEAD responses advertise the entity size in Content-Length but carry
        // no body — the header must win (ResourceTests.remoteResourceExists).
        let headers = vec![("Content-Length".to_string(), "6".to_string())];
        assert_eq!(content_length_of(&headers, 0), 6);
        // No header → fall back to the buffered body size.
        assert_eq!(content_length_of(&[], 5), 5);
        // Duplicate headers: the LAST wins (MessageHeader.findValue iterates
        // backwards). MockWebServer emits its bodiless "Content-Length: 0"
        // default before the test's addHeader("Content-Length", "6").
        let dup = vec![
            ("Content-Length".to_string(), "0".to_string()),
            ("Content-Type".to_string(), "text/plain".to_string()),
            ("Content-Length".to_string(), "6".to_string()),
        ];
        assert_eq!(content_length_of(&dup, 0), 6);
    }

    #[test]
    fn test_find_subslice() {
        assert_eq!(find_subslice(b"hello world", b"world"), Some(6));
        assert_eq!(find_subslice(b"abc", b""), None);
    }

    #[test]
    fn test_read_chunked_simple() {
        let mut data: Vec<u8> = Vec::new();
        // Chunk sizes are hex; "in \r\nchunks." is 12 bytes = 0xc.
        // Embedded CRLF inside the chunk data is preserved per RFC 7230 §4.1.
        data.extend_from_slice(b"4\r\nWiki\r\n");
        data.extend_from_slice(b"6\r\npedia \r\n");
        data.extend_from_slice(b"c\r\nin \r\nchunks.\r\n");
        data.extend_from_slice(b"0\r\n\r\n");
        let mut empty: &[u8] = &[];
        let body = read_chunked(&mut data, &mut empty).unwrap();
        assert_eq!(body, b"Wikipedia in \r\nchunks.");
    }

    #[test]
    fn test_read_chunked_retries_raw_eintr() {
        struct InterruptOnce<'a> {
            interrupted: bool,
            remaining: &'a [u8],
        }

        impl Read for InterruptOnce<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(std::io::Error::from_raw_os_error(4));
                }
                self.remaining.read(buf)
            }
        }

        let mut prefix = b"4\r\n".to_vec();
        let mut stream = InterruptOnce {
            interrupted: false,
            remaining: b"Wiki\r\n0\r\n\r\n",
        };
        assert_eq!(read_chunked(&mut prefix, &mut stream).unwrap(), b"Wiki");
    }

    #[test]
    fn test_read_response_304_is_bodiless() {
        // RFC 9110 §6.4.1: a 304 has no message body even when it carries a
        // (bogus) Content-Length. read_response must return immediately with an
        // empty body and NOT consume the trailing bytes — otherwise a keep-alive
        // 304 (no real body coming) hangs the client. The trailing "HELLO" here
        // stands in for "bytes that are not ours to read".
        let mut data: &[u8] =
            b"HTTP/1.1 304 Not Modified\r\nETag: W/\"x\"\r\nContent-Length: 5\r\n\r\nHELLO";
        let (status, headers, body) = read_response(&mut data, false).unwrap();
        assert_eq!(status, 304);
        assert!(body.is_empty(), "304 must be bodiless");
        assert!(headers
            .iter()
            .any(|(n, v)| n.eq_ignore_ascii_case("etag") && v == "W/\"x\""));
    }

    #[test]
    fn test_read_response_204_is_bodiless() {
        let mut data: &[u8] = b"HTTP/1.1 204 No Content\r\nContent-Length: 3\r\n\r\nabc";
        let (status, _h, body) = read_response(&mut data, false).unwrap();
        assert_eq!(status, 204);
        assert!(body.is_empty(), "204 must be bodiless");
    }

    #[test]
    fn test_read_response_200_reads_body() {
        // Guard against over-broadening the bodiless rule: a normal 200 with a
        // Content-Length must still return its body.
        let mut data: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nHELLO";
        let (status, _h, body) = read_response(&mut data, false).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"HELLO");
    }

    #[test]
    fn test_read_response_preserves_non_utf8_header_bytes_as_latin1() {
        // Tomcat's RFC6265 cookie test emits U+0120 as UTF-8 (C4 A0) then
        // uses String.getBytes(ISO_8859_1) to recover the wire bytes from the
        // response header. The HTTP client must therefore retain C4 A0 as
        // Latin-1 code points, not decode or replace them as UTF-8.
        let mut data: &[u8] =
            b"HTTP/1.1 200 OK\r\nSet-Cookie: Test=\xC4\xA0\r\nContent-Length: 0\r\n\r\n";
        let (status, headers, body) = read_response(&mut data, false).unwrap();
        assert_eq!(status, 200);
        assert!(body.is_empty());
        assert!(headers
            .iter()
            .any(|(name, value)| name == "Set-Cookie" && value == "Test=\u{00c4}\u{00a0}"));
    }

    #[test]
    fn test_registry_allocates_unique_ids() {
        let mut reg = ConnRegistry::new();
        let id1 = reg.allocate(ConnState {
            status: 200,
            response_body: b"a".to_vec(),
            response_headers: vec![],
            body_consumed: false,
            truncated: false,
        });
        let id2 = reg.allocate(ConnState {
            status: 404,
            response_body: b"b".to_vec(),
            response_headers: vec![],
            body_consumed: false,
            truncated: false,
        });
        assert_ne!(id1, id2);
        assert_eq!(reg.get(id1).unwrap().status, 200);
        assert_eq!(reg.get(id2).unwrap().status, 404);
    }

    #[test]
    fn test_registry_remove_clears_state() {
        let mut reg = ConnRegistry::new();
        let id = reg.allocate(ConnState {
            status: 200,
            response_body: vec![],
            response_headers: vec![],
            body_consumed: false,
            truncated: false,
        });
        assert!(reg.get(id).is_some());
        reg.remove(id);
        assert!(reg.get(id).is_none());
    }

    #[test]
    fn test_huc_init_sets_defaults() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 12);
        let url_obj = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
        let url_str = ctx.create_string("http://example.com/");
        ctx.set_field(url_obj, 0, Value::Object(Some(url_str)));
        let _ = huc_init(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(url_obj))],
        );
        assert_eq!(ctx.get_field(this, HUC_CONN_ID), Value::Int(-1));
        assert_eq!(ctx.get_field(this, HUC_DO_INPUT), Value::Int(1));
        assert_eq!(ctx.get_field(this, HUC_DO_OUTPUT), Value::Int(0));
    }

    #[test]
    fn test_huc_set_request_method_normalizes() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 12);
        let _ = huc_init(&mut ctx, &[Value::Object(Some(this))]);
        let post = ctx.create_string("post");
        let _ = huc_set_request_method(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(post))],
        );
        let m = match ctx.get_field(this, HUC_METHOD) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap(),
            _ => panic!("expected method string"),
        };
        assert_eq!(m, "POST");
    }

    #[test]
    fn test_huc_set_request_method_rejects_bogus() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 12);
        let _ = huc_init(&mut ctx, &[Value::Object(Some(this))]);
        let bad = ctx.create_string("FROBNICATE");
        let result = huc_set_request_method(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(bad))],
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_huc_disconnect_clears_state() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 12);
        let _ = huc_init(&mut ctx, &[Value::Object(Some(this))]);
        let _ = huc_disconnect(&mut ctx, &[Value::Object(Some(this))]);
        assert_eq!(ctx.get_field(this, HUC_CONN_ID), Value::Int(-1));
        assert_eq!(ctx.get_field(this, HUC_DISCONNECTED), Value::Int(1));
    }

    #[test]
    fn test_huc_get_response_message_known_codes() {
        // Build a fake state and run get_response_message via the pipeline.
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 12);
        let _ = huc_init(&mut ctx, &[Value::Object(Some(this))]);
        // Cannot run ensure_connected without a real network — just exercise
        // the code-name table directly.
        let id = registry().lock().unwrap().allocate(ConnState {
            status: 404,
            response_body: vec![],
            response_headers: vec![],
            body_consumed: false,
            truncated: false,
        });
        ctx.set_field(this, HUC_CONN_ID, Value::Int(id));
        ctx.set_field(this, HUC_CONNECTED, Value::Int(1));
        let res = huc_get_response_message(&mut ctx, &[Value::Object(Some(this))]).unwrap();
        let s = match res {
            Some(Value::Object(Some(o))) => ctx.read_string(o).unwrap(),
            _ => panic!("expected string"),
        };
        assert_eq!(s, "Not Found");
    }

    #[test]
    fn test_huc_set_connect_timeout_negative_rejected() {
        let mut ctx = crate::test_utils::MockNativeContext::new();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 12);
        let _ = huc_init(&mut ctx, &[Value::Object(Some(this))]);
        let res = huc_set_connect_timeout(&mut ctx, &[Value::Object(Some(this)), Value::Int(-1)]);
        assert!(res.is_err());
    }

    #[test]
    fn test_anchor_pub_function_exists() {
        // Anchor-grep guard: enforce the exported API name keeps existing.
        let mut r = NativeMethodRegistry::new();
        register_http_url_connection_real(&mut r);
        assert!(!r.is_empty());
    }

    // -----------------------------------------------------------------
    // Plain-HTTP keep-alive pool
    // -----------------------------------------------------------------

    #[test]
    fn test_is_poolable_response_content_length() {
        let headers = vec![("Content-Length".to_string(), "5".to_string())];
        assert!(is_poolable_response(200, false, &headers));
    }

    #[test]
    fn test_is_poolable_response_chunked_excluded() {
        let headers = vec![("Transfer-Encoding".to_string(), "chunked".to_string())];
        assert!(!is_poolable_response(200, false, &headers));
    }

    #[test]
    fn test_is_poolable_response_connection_close_excluded() {
        let headers = vec![
            ("Content-Length".to_string(), "5".to_string()),
            ("Connection".to_string(), "close".to_string()),
        ];
        assert!(!is_poolable_response(200, false, &headers));
    }

    #[test]
    fn test_is_poolable_response_bodiless_without_content_length() {
        // HEAD / 204 / 304 / 1xx carry no body and no Content-Length, but the
        // framing is still unambiguous — poolable.
        assert!(is_poolable_response(200, true, &[]));
        assert!(is_poolable_response(204, false, &[]));
        assert!(is_poolable_response(304, false, &[]));
        assert!(is_poolable_response(100, false, &[]));
    }

    #[test]
    fn test_is_poolable_response_ambiguous_framing_excluded() {
        // A normal 200 with neither Content-Length nor chunked has no
        // reliable end-of-body marker other than connection close — not safe
        // to hand back to the pool.
        assert!(!is_poolable_response(200, false, &[]));
    }

    /// A loopback `TcpStream` pair for exercising `pool_take`/`pool_put`
    /// against real sockets (the pool stores `TcpStream` directly, not a
    /// generic `Read`, so a slice-backed fake won't do here).
    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    #[test]
    fn test_pool_put_then_take_roundtrip() {
        let (client, _server) = loopback_pair();
        let host = format!("test-roundtrip-{}", client.local_addr().unwrap().port());
        let port = 1;
        pool_put(&host, port, client);
        assert!(pool_take(&host, port).is_some());
        // The bucket is empty now — a second take is a plain miss.
        assert!(pool_take(&host, port).is_none());
    }

    #[test]
    fn test_pool_take_evicts_closed_peer() {
        let (client, server) = loopback_pair();
        let host = format!("test-closed-peer-{}", client.local_addr().unwrap().port());
        let port = 1;
        pool_put(&host, port, client);
        drop(server); // peer close -> FIN visible to a peek on `client`
                      // Give the FIN a moment to actually land in the kernel buffer.
        std::thread::sleep(Duration::from_millis(50));
        assert!(pool_take(&host, port).is_none());
    }

    #[test]
    fn test_pool_take_respects_idle_window() {
        let (client, _server) = loopback_pair();
        let host = format!("test-idle-window-{}", client.local_addr().unwrap().port());
        let port = 1;
        if let Ok(mut guard) = conn_pool().lock() {
            guard
                .entry((host.clone(), port))
                .or_default()
                .push(PooledConn {
                    stream: client,
                    returned_at: Instant::now() - POOL_IDLE_WINDOW - Duration::from_millis(1),
                });
        }
        assert!(pool_take(&host, port).is_none());
    }

    #[test]
    fn test_pool_put_caps_bucket_at_max_per_key() {
        let (first, _first_server) = loopback_pair();
        let host = format!("test-cap-{}", first.local_addr().unwrap().port());
        let port = 2;
        pool_put(&host, port, first);
        for _ in 0..(POOL_MAX_PER_KEY + 1) {
            let (client, _server) = loopback_pair();
            pool_put(&host, port, client);
        }
        let len = conn_pool()
            .lock()
            .unwrap()
            .get(&(host, port))
            .map(|b| b.len())
            .unwrap_or(0);
        assert_eq!(len, POOL_MAX_PER_KEY);
    }

    // -----------------------------------------------------------------------
    // G31 — the per-connection `HostnameVerifier` that was never asked
    //
    // Rows measured 2026-08-17 against HotSpot 25.0.3+9-LTS over a live
    // loopback TLS 1.3 handshake (`scratchpad/g31/HvFamily.java`,
    // `HvCase.java`, `HvLambda.java`), one case per process.
    // -----------------------------------------------------------------------

    /// THE REGRESSION GUARD. `None` means "a verifier object exists whose class
    /// this VM could not name" by the time it reaches this predicate — the
    /// caller has already dealt with "no verifier at all" — and a lambda is
    /// exactly that. Reading it as "the JDK default is installed" refused every
    /// connection whose application verifier was written as a lambda.
    ///
    /// MEASURED, same connection shape, same request, HotSpot vs CratonVM:
    /// a named-class verifier was called on both (`calls=1`); a lambda was
    /// called on HotSpot and NOT on CratonVM (`calls=0`).
    #[test]
    fn an_unnameable_verifier_is_not_a_default_stand_in() {
        assert!(
            !is_default_hostname_verifier(None),
            "a verifier whose class name could not be resolved is an APPLICATION \
             verifier (a lambda), not a JDK default — MEASURED: HotSpot calls it"
        );
    }

    /// The two entries that are genuinely "nothing application-specific is
    /// installed", and a sample of the shapes that are not. Both survivors are
    /// named by classes this VM always resolves, which is why the predicate can
    /// afford to be a name match at all.
    #[test]
    fn the_two_default_stand_ins_are_the_only_ones() {
        assert!(is_default_hostname_verifier(Some(
            "javax/net/ssl/HttpsURLConnection$DefaultHostnameVerifier"
        )));
        assert!(is_default_hostname_verifier(Some(
            "javax/net/ssl/HostnameVerifier"
        )));
        // MEASURED: `HvFamily$Rec` is consulted on both VMs.
        assert!(!is_default_hostname_verifier(Some("HvFamily$Rec")));
        // A third-party permissive verifier must still be consulted.
        assert!(!is_default_hostname_verifier(Some(
            "org/apache/http/conn/ssl/NoopHostnameVerifier"
        )));
    }

    /// A verifier that DECLINED and a certificate that never matched are two
    /// different facts, and HotSpot reports them with two different exception
    /// classes and two different sentences. The messages are transcribed, so
    /// they are asserted character for character.
    #[test]
    fn a_declined_verifier_and_an_unmatched_certificate_read_differently() {
        let declined = huc_verifier_declined_message("127.0.0.1");
        // MEASURED: java.io.IOException | Wrong HTTPS hostname: should be <127.0.0.1>
        assert_eq!(
            declined.trim_start_matches(TLS_HOSTNAME_REFUSED_SENTINEL),
            "Wrong HTTPS hostname: should be <127.0.0.1>"
        );
        assert!(
            declined.starts_with(TLS_HOSTNAME_REFUSED_SENTINEL),
            "the declined exit must route to the plain-IOException sentinel, not the \
             SSLPeerUnverifiedException one"
        );
        let unmatched = huc_unverified_peer_message("127.0.0.1");
        assert!(unmatched.starts_with(TLS_PEER_UNVERIFIED_SENTINEL));
        assert_ne!(
            declined.trim_start_matches(TLS_HOSTNAME_REFUSED_SENTINEL),
            unmatched.trim_start_matches(TLS_PEER_UNVERIFIED_SENTINEL),
            "MEASURED: HotSpot gives these two exits different messages"
        );
    }

    /// The two sentinels must stay distinguishable by prefix, or the arm that
    /// picks the exception class would match the wrong one. They share no
    /// prefix relation in either direction.
    #[test]
    fn the_tls_sentinels_do_not_shadow_each_other() {
        for (a, b) in [
            (TLS_HOSTNAME_REFUSED_SENTINEL, TLS_PEER_UNVERIFIED_SENTINEL),
            (
                TLS_HOSTNAME_REFUSED_SENTINEL,
                TLS_HANDSHAKE_FAILURE_SENTINEL,
            ),
            (TLS_HOSTNAME_REFUSED_SENTINEL, CONNECT_REFUSED_SENTINEL),
        ] {
            assert!(!a.starts_with(b), "{a} must not start with {b}");
            assert!(!b.starts_with(a), "{b} must not start with {a}");
        }
    }
}
