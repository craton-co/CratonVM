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

/// One synthetic carrier's completed exchange, and the carrier it belongs to.
struct ConnRow {
    /// The weak lock key of the carrier the row was filed for
    /// ([`huc_obj_key`]), so the lock-key sweep that frees it -- the carrier
    /// died -- finds the row ([`forget_huc_obj_keys`]). The carrier's identity
    /// hash until gc-common w27-b, which two live carriers could share.
    owner_key: usize,
    state: ConnState,
}

/// ONE VM's synthetic-carrier connection rows (a row of [`CONN_REGISTRY`]).
///
/// gc-common w17-e (`common-w16f-synthetic-huc-connection-registry-holds-
/// response-bodies`): this was one process-wide `Mutex<ConnRegistry>` whose
/// only removal was `huc_disconnect`, so every response body a synthetic
/// carrier read without `disconnect()` -- the ordinary pattern -- stayed in
/// native memory for the life of the process, and a torn-down VM's rows with
/// it. Rows are now per VM (dropped at teardown by
/// [`forget_vm_http_url_connection_state`]) and each is filed under its
/// carrier, whose death drops it ([`ConnRegistry::forget_owners`], reached
/// from the lock-key sweep through [`forget_huc_obj_keys`] since gc-common
/// w27-b; from `t27_tls::gc_sweep_tls_rows` before that).
///
/// Keyed by connection id and by the carrier's weak lock key, never by an
/// `ObjectRef`: not an address-keyed table (`vm/src/memory/addr_keyed.rs`).
struct ConnRegistry {
    next_id: i32,
    conns: HashMap<i32, ConnRow>,
    /// Carrier weak lock key -> the ids filed for it. Usually one id (a
    /// carrier connects once); a key no other live carrier shares (gc-common
    /// w27-b: two live carriers with one identity hash used to share a row
    /// here, and their ids went together).
    by_owner: HashMap<usize, Vec<i32>>,
}

impl Default for ConnRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnRegistry {
    fn new() -> Self {
        Self {
            next_id: 1,
            conns: HashMap::new(),
            by_owner: HashMap::new(),
        }
    }
    fn allocate(&mut self, owner_key: usize, state: ConnState) -> i32 {
        if self.next_id <= 0 {
            self.next_id = 1; // wrap back into positive id space
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id <= 0 {
            self.next_id = 1;
        }
        if let Some(old) = self.conns.insert(id, ConnRow { owner_key, state }) {
            // An id reused after a full wrap: the displaced row's index entry
            // must not outlive it.
            self.unlink_owner(old.owner_key, id);
        }
        self.by_owner.entry(owner_key).or_default().push(id);
        id
    }
    fn get(&self, id: i32) -> Option<&ConnState> {
        self.conns.get(&id).map(|row| &row.state)
    }
    fn get_mut(&mut self, id: i32) -> Option<&mut ConnState> {
        self.conns.get_mut(&id).map(|row| &mut row.state)
    }
    fn remove(&mut self, id: i32) {
        if let Some(row) = self.conns.remove(&id) {
            self.unlink_owner(row.owner_key, id);
        }
    }
    fn unlink_owner(&mut self, owner_key: usize, id: i32) {
        if let Some(ids) = self.by_owner.get_mut(&owner_key) {
            ids.retain(|&i| i != id);
            if ids.is_empty() {
                self.by_owner.remove(&owner_key);
            }
        }
    }
    /// Drop every row filed for a carrier whose key is in `freed` (the
    /// carriers died). The rows are handed back so the caller drops the
    /// response bodies after releasing the table lock.
    fn forget_owners(&mut self, freed: &mut FreedHucKeys<'_>) -> Vec<ConnRow> {
        let mut rows = Vec::new();
        for ids in take_huc_rows(&mut self.by_owner, freed) {
            for id in ids {
                if let Some(row) = self.conns.remove(&id) {
                    rows.push(row);
                }
            }
        }
        rows
    }
}

/// The synthetic-carrier connection rows, one [`ConnRegistry`] per VM.
/// Lock discipline is `VmScoped`'s: nothing allocates, dispatches Java or
/// takes another lock while holding it -- the post-collection lock-key sweep
/// takes it ([`forget_huc_obj_keys`]).
static CONN_REGISTRY: cratonvm_native_api::vm_scoped::VmScoped<ConnRegistry> =
    cratonvm_native_api::vm_scoped::VmScoped::new();

/// Run `f` against VM `vm`'s connection rows if it has any, without creating
/// a row for a VM that never connected a synthetic carrier.
fn conn_registry_mut<R>(vm: usize, f: impl FnOnce(&mut ConnRegistry) -> R) -> Option<R> {
    if !CONN_REGISTRY.has_row(vm) {
        return None;
    }
    Some(CONN_REGISTRY.with(vm, f))
}

/// File a synthetic carrier's completed exchange and answer its connection id.
///
/// `this` must be the carrier's CURRENT address (nothing may allocate between
/// the caller's last pin read and this call): the row is filed under its weak
/// lock key ([`huc_obj_key`]), which the carrier alone owns and which the
/// lock-key sweep frees -- dropping the row -- once it dies.
fn register_synthetic_conn(ctx: &dyn NativeContext, this: ObjectRef, state: ConnState) -> i32 {
    let vm = ctx.vm_identity();
    let owner_key = huc_obj_key(ctx, this);
    CONN_REGISTRY.with(vm, |reg| reg.allocate(owner_key, state))
}

// ---------------------------------------------------------------------------
// Per-object row keys (gc-common w27-b)
// ---------------------------------------------------------------------------
//
// Every per-object side table of this file -- the handshake records, the
// response-stream associations, the synthetic connection rows, and the four
// real-carrier tables plus the request-body streams -- is keyed by the
// object's WEAK LOCK KEY (`crate::gc_stable_weak_lock_key`), not by its
// identity hash (`common-w26b-tls-sibling-tables-keyed-by-vm-folded-identity-hash`).
// The identity hash is 32 bits from one per-heap counter that wraps, so two
// LIVE carriers of one VM could share a row: the second one read the first
// one's cached response, request headers, method or timeouts, found its
// handshake record and never ran its own (and reported the first one's peer
// chain and cipher), and a mint of the second wiped the first's in-flight
// request. A weak lock key is per VM and per live object (matched by the
// object's current address, which the stop-the-world epilogue re-addresses
// after a move), stable for the object's life, never minted again, and freed
// by the first lock-key sweep that finds the object dead; that sweep then
// drops every row filed under it ([`forget_huc_obj_keys`], called from
// `lib.rs::sweep_lock_keys` and `forget_vm_lock_keys`). It replaces the
// `t27_tls` `HucCarrier` weak owner, which was keyed by the folded hash.
//
// Writers mint ([`huc_obj_key`]); readers never do
// ([`huc_existing_obj_key`]): an object never keyed has no row, and a lookup
// must not leave a registry slot behind for every stream or carrier asked.
//
// Rows READ AFTER CLOSE: a recycled `https_peer_info` row (after
// `disconnect()` or the response drain) must outlive the connection's socket
// and session but never its carrier object -- the accessors that read it are
// called on the carrier. A cached real-carrier result is read after the
// socket closed, by the carrier's own getters. Both are exactly "until the
// carrier dies", which is what a weak key gives.

/// The key `obj`'s rows are filed under in this file's per-object tables: its
/// weak lock key, MINTED. Call it where a row is written. `obj` must be the
/// CURRENT address. Takes the lock-key registry (a leaf) and
/// [`huc_keyed_vms`]: compute it BEFORE any table guard.
fn huc_obj_key(ctx: &dyn NativeContext, obj: ObjectRef) -> usize {
    let key = crate::gc_stable_weak_lock_key(ctx, obj)
        .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"));
    note_huc_keyed_vm(ctx.vm_identity());
    key
}

/// The key [`huc_obj_key`] would answer for `obj`, or `None` when the calling
/// VM never keyed it -- in which case no table of this file has a row for it.
/// Never mints.
#[inline]
fn huc_existing_obj_key(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<usize> {
    crate::existing_weak_lock_key(ctx, obj)
}

/// The VMs that filed a row under a weak lock key in one of this file's
/// PER-VM tables ([`CONN_REGISTRY`], [`REAL_CARRIERS`], [`REAL_BODY_STREAMS`]):
/// [`forget_huc_obj_keys`] is handed freed keys alone, and a freed key no
/// longer names its VM. Registered by [`huc_obj_key`], dropped at teardown
/// ([`forget_vm_http_url_connection_state`]). One entry per VM, so a sweep
/// visits the few VMs that ever used a carrier. `LockLevel::Scratch`: one
/// `Vec` operation per acquisition, no VM call and no other lock under it.
fn huc_keyed_vms() -> &'static cratonvm_types::lock_order::OrderedPlMutex<Vec<usize>> {
    static T: OnceLock<cratonvm_types::lock_order::OrderedPlMutex<Vec<usize>>> = OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            Vec::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

fn note_huc_keyed_vm(vm: usize) {
    let mut vms = huc_keyed_vms().lock();
    if !vms.contains(&vm) {
        vms.push(vm);
    }
}

/// The keys one lock-key sweep freed, with a set of them built on first need
/// (only when a table is smaller than the key list, so walking the table is
/// the cheaper side).
struct FreedHucKeys<'a> {
    keys: &'a [usize],
    set: Option<std::collections::HashSet<usize>>,
}

impl<'a> FreedHucKeys<'a> {
    fn new(keys: &'a [usize]) -> Self {
        Self { keys, set: None }
    }

    fn set(&mut self) -> &std::collections::HashSet<usize> {
        let keys = self.keys;
        self.set.get_or_insert_with(|| keys.iter().copied().collect())
    }

    fn contains(&mut self, key: usize) -> bool {
        self.set().contains(&key)
    }
}

/// Remove the rows of `map` filed under a freed key, walking whichever side is
/// smaller, and hand them back (so the caller drops them -- response bodies,
/// sockets -- after releasing the table's lock).
fn take_huc_rows<V>(map: &mut HashMap<usize, V>, freed: &mut FreedHucKeys<'_>) -> Vec<V> {
    if map.is_empty() {
        return Vec::new();
    }
    if map.len() <= freed.keys.len() {
        let set = freed.set();
        let hit: Vec<usize> = map.keys().filter(|k| set.contains(*k)).copied().collect();
        hit.into_iter().filter_map(|k| map.remove(&k)).collect()
    } else {
        freed.keys.iter().filter_map(|k| map.remove(k)).collect()
    }
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
    /// The VM whose carrier this is (teardown selects a VM's rows by it).
    vm: usize,
}

/// Peer info per connection object, keyed by the carrier's weak lock key
/// ([`huc_obj_key`]; its identity hash folded with the VM until gc-common
/// w27-b). A row is read after close (a recycled one answers "not yet open")
/// and goes with its carrier ([`forget_huc_obj_keys`]).
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
) -> &'static cratonvm_types::lock_order::OrderedMutex<HashMap<usize, HttpsPeerInfo>> {
    static R: OnceLock<cratonvm_types::lock_order::OrderedMutex<HashMap<usize, HttpsPeerInfo>>> =
        OnceLock::new();
    R.get_or_init(|| {
        cratonvm_types::lock_order::OrderedMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

// gc-common w16-f: [`https_peer_info`] and [`https_response_streams`] are
// process-wide and were keyed by the bare identity hash, so two VMs' carriers
// with equal hashes shared one row (VM B's `https_ensure_exchanged` found VM
// A's handshake). w16-f folded the VM into the key; gc-common w27-b keys them
// by the object's weak lock key ([`huc_obj_key`]), which also tells apart two
// LIVE carriers of ONE VM. A dead carrier's rows go with its key
// ([`forget_huc_obj_keys`]); a carrier built by `huc_init` drops any row
// already filed under its key; a torn-down VM's rows go in
// [`forget_vm_http_url_connection_state`].

/// Drop the `https_peer_info` row filed under carrier key `key`; answer
/// whether one went. Takes only that table's lock.
fn forget_https_peer_row(key: usize) -> bool {
    https_peer_info()
        .lock()
        .map(|mut t| t.remove(&key).is_some())
        .unwrap_or(false)
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
    // Key before the guard: it re-enters the VM (the identity hash) and takes
    // the lock-key registry, and this table's `LockLevel` claims neither
    // happens under it. `conn` is current: the one production caller re-reads
    // it from its pin (`PinnedCarrier::current`) just before this call.
    let key = huc_obj_key(ctx, conn);
    let vm = ctx.vm_identity();
    https_peer_info().lock().unwrap().insert(
        key,
        HttpsPeerInfo {
            chain_der: chain_der.to_vec(),
            cipher: cipher.to_string(),
            recycled: false,
            vm,
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
    let carrier = https_carrier_keys(&*ctx, this);
    https_recycle_carrier_by_key(ctx, carrier);
}

/// The two keys an `https` carrier's session state is filed under: its row in
/// [`https_peer_info`] (`None` when the carrier was never keyed, so it has no
/// row) and its `net_phase_e` `https_carrier_sessions` key. Plain integers,
/// safe to hold across Java execution (gc-common w27-b; the one
/// `NativeObjKey` used to address both tables through its identity hash).
/// Since gc-common w28-a both are the carrier's weak lock key, and
/// `session_key` is `None` for a carrier `net_phase_e` never keyed.
#[derive(Clone, Copy, Debug)]
struct HttpsCarrierKeys {
    peer_key: Option<usize>,
    session_key: Option<crate::net_phase_e::NativeObjKey>,
}

/// One [`https_response_streams`] row: the carrier the stream belongs to, and
/// the VM (teardown selects a VM's rows by it).
#[derive(Clone, Copy, Debug)]
struct ResponseStreamRow {
    vm: usize,
    /// The carrier's [`https_peer_info`] key. Always a key that HAD a row
    /// when the stream was noted.
    peer_key: usize,
    session_key: Option<crate::net_phase_e::NativeObjKey>,
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
/// on an `SSLSession` for the life of the process. Keyed by the stream's weak
/// lock key since gc-common w27-b, and a row goes when either the stream or
/// its carrier dies ([`forget_huc_obj_keys`]).
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Every acquisition is a
/// single map operation on integers, with the key that addresses it
/// evaluated into a local BEFORE the guard is taken — see
/// `note_response_stream`, which is where it used to sit inside the
/// `table.insert(..)` argument list.
fn https_response_streams(
) -> &'static cratonvm_types::lock_order::OrderedMutex<HashMap<usize, ResponseStreamRow>> {
    static R: OnceLock<
        cratonvm_types::lock_order::OrderedMutex<HashMap<usize, ResponseStreamRow>>,
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
///
/// Test-only since gc-common w16-f: the one production caller keys the
/// carrier itself, before it allocates (see [`note_response_stream_keyed`]).
#[cfg(test)]
fn note_response_stream(ctx: &dyn NativeContext, stream: ObjectRef, carrier: Option<ObjectRef>) {
    let carrier_keys = carrier.map(|c| https_carrier_keys(ctx, c));
    note_response_stream_keyed(ctx, stream, carrier_keys);
}

/// The keys of `carrier`'s session state ([`HttpsCarrierKeys`]), taken while
/// `carrier` is current. Never mints.
fn https_carrier_keys(ctx: &dyn NativeContext, carrier: ObjectRef) -> HttpsCarrierKeys {
    HttpsCarrierKeys {
        peer_key: huc_existing_obj_key(ctx, carrier),
        session_key: crate::net_phase_e::existing_native_obj_key(ctx, carrier),
    }
}

/// `note_response_stream` for a carrier whose keys were taken BEFORE the
/// stream was allocated (gc-common w16-f): `make_response_input_stream`
/// allocates the stream after the caller handed it the carrier, so reading the
/// carrier's identity hash afterwards read the mark word at an address the
/// allocation may have vacated. `stream` is current (just allocated).
fn note_response_stream_keyed(
    ctx: &dyn NativeContext,
    stream: ObjectRef,
    carrier: Option<HttpsCarrierKeys>,
) {
    let Some(HttpsCarrierKeys {
        peer_key: Some(peer_key),
        session_key,
    }) = carrier
    else {
        // No carrier, or one never keyed: it has no recorded TLS exchange.
        return;
    };
    if !https_peer_info()
        .lock()
        .map(|t| t.contains_key(&peer_key))
        .unwrap_or(false)
    {
        return;
    }
    // The key is evaluated BEFORE the guard: it calls back into the VM and
    // takes the lock-key registry, and the lock-discipline level stamped on
    // this table claims neither happens under it. Idempotent.
    let stream_key = huc_obj_key(ctx, stream);
    let row = ResponseStreamRow {
        vm: ctx.vm_identity(),
        peer_key,
        session_key,
    };
    if let Ok(mut table) = https_response_streams().lock() {
        table.insert(stream_key, row);
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
    // Every `ByteArrayInputStream` EOF in the process lands here. With no
    // `https` response stream outstanding -- the common case -- answer at
    // once, without the identity hash or the lock-key registry (gc-common
    // w27-b: the stream's key lookup takes that process-wide mutex).
    let outstanding = https_response_streams()
        .lock()
        .map(|t| !t.is_empty())
        .unwrap_or(false);
    if !outstanding {
        return Ok(());
    }
    // A stream never keyed was never noted.
    let Some(stream_key) = huc_existing_obj_key(&*ctx, stream) else {
        return Ok(());
    };
    // Scoped so the lock is released before the call below, which takes two
    // more process-global locks and can release a GC root. Same rule, and the
    // same reason, as the scoped guard in `https_recycle_carrier_by_key`.
    let row = {
        let Ok(mut table) = https_response_streams().lock() else {
            return Ok(());
        };
        table.remove(&stream_key)
    };
    if let Some(row) = row {
        https_recycle_carrier_by_key(
            ctx,
            HttpsCarrierKeys {
                peer_key: Some(row.peer_key),
                session_key: row.session_key,
            },
        );
    }
    Ok(())
}

/// [`https_recycle_carrier`] for a caller holding the carrier's KEYS rather
/// than the object — see [`huc_live_bais_event`], which is handed the response
/// stream and has no way back to the carrier except these keys.
fn https_recycle_carrier_by_key(ctx: &mut dyn NativeContext, carrier: HttpsCarrierKeys) {
    // Scoped, and NOT written as `if let Some(..) = https_peer_info().lock()
    // ...`: under Rust 2021's drop rules the guard produced in an `if let`
    // scrutinee lives to the end of the block, so the process-global lock would
    // still be held across the `forget_https_carrier_session` call below —
    // which takes another process-global lock and can release a GC root. That
    // is the "native holds a lock across a call that re-enters the VM" cycle
    // this workspace has already paid for once.
    if let Some(key) = carrier.peer_key {
        let mut table = https_peer_info().lock().unwrap();
        if let Some(info) = table.get_mut(&key) {
            info.recycled = true;
        }
    }
    crate::net_phase_e::forget_https_carrier_session_by_key(ctx, carrier.session_key);
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
fn https_ensure_exchanged_body(ctx: &mut dyn NativeContext, mut this: ObjectRef) {
    // Key before the guard — see `record_https_peer_info`. A carrier never
    // keyed has no row (gc-common w27-b).
    if let Some(key) = huc_existing_obj_key(&*ctx, this) {
        if https_peer_info().lock().unwrap().contains_key(&key) {
            return;
        }
    }
    if let Some(url_str) = huc_real_object_url(ctx, &mut this) {
        if url_str.starts_with("https://") {
            let _ = huc_real_perform(ctx, &mut this, &url_str);
        }
        return;
    }
    let _ = ensure_connected(ctx, &mut this);
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
fn https_has_session(ctx: &mut dyn NativeContext, this: &mut ObjectRef) -> bool {
    https_ensure_exchanged(ctx, this);
    let Some(key) = huc_existing_obj_key(&*ctx, *this) else {
        return false;
    };
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
    this: &mut ObjectRef,
) -> Result<Vec<Vec<u8>>, MethodCallFailed> {
    https_ensure_exchanged(ctx, this);
    let key = huc_existing_obj_key(&*ctx, *this);
    // A recycled entry is filtered out here rather than matched below, so it
    // lands on the `None` arm — `IllegalStateException: connection not yet
    // open`, which is HotSpot's measured post-`disconnect()` answer, and NOT
    // `SSLPeerUnverifiedException`, which would claim the connection is open
    // and the peer anonymous. A carrier never keyed has no entry.
    let found = key.and_then(|key| {
        https_peer_info()
            .lock()
            .unwrap()
            .get(&key)
            .filter(|info| !info.recycled)
            .map(|info| info.chain_der.clone())
    });
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
            let mut this = obj_arg(args, 0)?;
            let chain = https_peer_chain_or_throw(ctx, &mut this)?;
            // i6-L2: `X509Certificate[]`, HotSpot's runtime type here, so the
            // common `(X509Certificate[]) conn.getServerCertificates()` holds.
            let component = crate::lang_class::reflection_component_id(
                ctx,
                "java/security/cert/X509Certificate",
            );
            let arr = ctx.new_ref_array(component, chain.len());
            // `make_x509_mirror` runs the `X509CertImpl` constructor (Java, a
            // GC point): keep the result array pinned and re-read it per store.
            let arr_pin = ctx.pin_native_root(arr);
            for (i, der) in chain.iter().enumerate() {
                let mirror = match crate::keystore::make_x509_mirror(ctx, "peer", der) {
                    Ok(mirror) => mirror,
                    Err(e) => {
                        ctx.unpin_native_roots(arr_pin);
                        return Err(e);
                    }
                };
                let arr = ctx.read_native_pin(arr_pin, arr);
                ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
            }
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.unpin_native_roots(arr_pin);
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
            let mut this = obj_arg(args, 0)?;
            if !https_has_session(ctx, &mut this) {
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
            let key = huc_existing_obj_key(&*ctx, this);
            // `filter` before `map`: a recycled connection has no cipher suite to
            // report, and falls through to the refusal below. See
            // `https_recycle_carrier`.
            let cipher = key.and_then(|key| {
                https_peer_info()
                    .lock()
                    .unwrap()
                    .get(&key)
                    .filter(|i| !i.recycled)
                    .map(|i| i.cipher.clone())
            });
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
            let mut this = obj_arg(args, 0)?;
            let chain = https_peer_chain_or_throw(ctx, &mut this)?;
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
            let mut this = obj_arg(args, 0)?;
            if !https_has_session(ctx, &mut this) {
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

/// One VM's real-carrier side tables, all keyed by the carrier's (or, for
/// `fixed_owners`, the body stream's) weak lock key ([`huc_obj_key`]; the
/// identity hash until gc-common w27-b, which two live carriers of one VM
/// could share). A row goes with its key ([`forget_huc_obj_keys`]).
///
/// One set PER VM since gc-common w11-a
/// (`common-w10e-huc-real-carrier-tables-are-keyed-by-bare-identity-hash`).
/// These were four process-wide statics keyed by the bare identity hash, and
/// identity hashes collide across VMs by construction. VM B's
/// `URL.openConnection()` mint (`real_forget`) deleted VM A's in-flight
/// request state -- its headers, method and timeouts, and the open socket of a
/// half-written fixed-length upload -- and without a mint in between B's
/// connection read A's cached response. Dropped by
/// `forget_vm_http_url_connection_state`.
///
/// The `VmScoped` lock is held only for map operations. A fixed-length upload's
/// socket I/O runs under that stream's OWN lock (`LiveFixedStreamCell`), not
/// under the table's: the old `live_fixed_streams()` mutex was held across
/// `write_all` / `flush` / `shutdown`, so one slow upload stalled every other
/// connection's body writes in the process.
#[derive(Default)]
struct RealCarrierTables {
    results: HashMap<usize, RealResult>,
    reqs: HashMap<usize, RealReq>,
    fixed_streams: HashMap<usize, LiveFixedStreamCell>,
    /// Body-stream key -> carrier key.
    fixed_owners: HashMap<usize, usize>,
}

static REAL_CARRIERS: cratonvm_native_api::vm_scoped::VmScoped<RealCarrierTables> =
    cratonvm_native_api::vm_scoped::VmScoped::new();

/// Read VM `vm`'s `results` without creating its row.
fn real_results_peek<R>(vm: usize, f: impl FnOnce(&HashMap<usize, RealResult>) -> R) -> Option<R> {
    REAL_CARRIERS.peek(vm, |t| f(&t.results))
}

/// Mutate VM `vm`'s `results`.
fn real_results_with<R>(vm: usize, f: impl FnOnce(&mut HashMap<usize, RealResult>) -> R) -> R {
    REAL_CARRIERS.with(vm, |t| f(&mut t.results))
}

/// Carrier `this`'s cached result, projected through `f`, or `None` when it
/// has none (never keyed, or not performed). Never mints.
fn real_result_of<R>(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    f: impl FnOnce(&RealResult) -> R,
) -> Option<R> {
    let key = huc_existing_obj_key(ctx, this)?;
    real_results_peek(ctx.vm_identity(), |t| t.get(&key).map(f)).flatten()
}

/// Carrier `this`'s request row, projected through `f`, or `None` when it has
/// none. Never mints (a reader; [`with_real_req`] is the writer).
fn real_req_of<R>(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    f: impl FnOnce(&RealReq) -> R,
) -> Option<R> {
    let key = huc_existing_obj_key(ctx, this)?;
    real_reqs_peek(ctx.vm_identity(), |t| t.get(&key).map(f)).flatten()
}

/// Per-real-connection REQUEST state, keyed by the carrier's weak lock key
/// ([`huc_obj_key`]; `identity_hash_code(this)` until gc-common w27-b).
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

/// Read VM `vm`'s `reqs` without creating its row.
fn real_reqs_peek<R>(vm: usize, f: impl FnOnce(&HashMap<usize, RealReq>) -> R) -> Option<R> {
    REAL_CARRIERS.peek(vm, |t| f(&t.reqs))
}

/// Mutate VM `vm`'s `reqs`.
fn real_reqs_with<R>(vm: usize, f: impl FnOnce(&mut HashMap<usize, RealReq>) -> R) -> R {
    REAL_CARRIERS.with(vm, |t| f(&mut t.reqs))
}

/// The request method a real carrier will actually send.
///
/// **Why this is not just `RealReq::method`.** On 2026-09-11 lane L6 retired
/// `setRequestMethod`/`getRequestMethod` on `java/net/HttpURLConnection` — the
/// class `URL.openConnection()` mints — so on that carrier the JDK's own
/// bytecode runs and it writes the object's `method` FIELD. Our side table
/// never sees the call. The wire went on reading the side table, so:
///
/// ```text
///   setRequestMethod("PUT")   field=PUT   table=""     wire sent POST
///   setDoOutput(true) only    field=GET   table=POST   getRequestMethod() said GET
/// ```
///
/// — MEASURED, `L6HttpLoopbackSweep` rows 67 and 69, in opposite directions on
/// the same disagreement. One reader, field first, is what makes the two
/// agree; the writers below ([`huc_set_request_method`] and
/// `getOutputStream`'s GET→POST promotion) keep both copies in step so the
/// `sun.*` carriers — whose setter IS ours — are unaffected.
fn real_method(ctx: &dyn NativeContext, this: ObjectRef) -> Option<String> {
    if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "method") {
        if let Some(m) = ctx.read_string(s).filter(|m| !m.is_empty()) {
            return Some(m);
        }
    }
    real_req_of(ctx, this, |r| r.method.clone()).filter(|m| !m.is_empty())
}

/// The streaming mode a real carrier is in — the JDK's own fields first, for
/// the same reason as [`real_method`].
///
/// `setChunkedStreamingMode(I)` and `setFixedLengthStreamingMode(I)` are in the
/// same retirement table, so on the minted carrier those three fields are the
/// only record of the call. `-1` is each one's "not set" sentinel; the mint
/// site in `net_phase_e` writes them, because an ALLOCATED carrier arrives
/// with 0 there and 0 means "set to zero".
fn real_streaming_mode(ctx: &dyn NativeContext, this: ObjectRef) -> StreamingMode {
    let field_int = |name: &str| ctx.get_field_by_name(this, name).as_int();
    if let Some(chunk) = field_int("chunkLength").filter(|n| *n >= 0) {
        return StreamingMode::Chunked(chunk);
    }
    if let Some(fixed) = field_int("fixedContentLength").filter(|n| *n >= 0) {
        return StreamingMode::Fixed(fixed as u64);
    }
    if let Value::Long(fixed) = ctx.get_field_by_name(this, "fixedContentLengthLong") {
        if fixed >= 0 {
            return StreamingMode::Fixed(fixed as u64);
        }
    }
    real_req_of(ctx, this, |r| r.streaming).unwrap_or_default()
}

/// Real-carrier buffered request-body stream (the `ByteArrayOutputStream`
/// returned by `getOutputStream`), keyed by the carrier's weak lock key
/// ([`huc_obj_key`]; `identity_hash_code(this)` until gc-common w27-b). We
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
/// GC note (gc-common w10-e): each row holds a GLOBAL ROOT handle
/// (`add_global_root`) plus the last-seen address, and readers resolve the
/// CURRENT address through the handle (the stored address is the fallback for
/// mock contexts, whose handle is 0). It used to register the stream as a
/// VarHandle root, which (1) can never be released, so every request body a
/// VM ever buffered stayed reachable for the life of the VM, and (2) is keyed
/// by identity hash in one per-VM map, so a body stream whose hash matched a
/// real `VarHandle`'s replaced that `VarHandle`'s root. A global root is
/// released when its row goes (`disconnect`, the carrier being re-minted,
/// eviction) and never collides.
///
/// One table PER VM since gc-common w10-e
/// (`common-w8a-process-global-object-singletons-in-native-builtins`, the
/// `real_body_streams` row): identity hashes collide across VMs, so VM B's
/// connection read VM A's request body, an object in A's heap, and B's
/// constructor evicted A's row.
static REAL_BODY_STREAMS: cratonvm_native_api::vm_scoped::VmScoped<RealBodyStreams> =
    cratonvm_native_api::vm_scoped::VmScoped::new();

/// One VM's [`REAL_BODY_STREAMS`] rows, keyed by the carrier's weak lock key
/// ([`huc_obj_key`]; its identity hash until gc-common w27-b).
#[derive(Default)]
struct RealBodyStreams {
    next_seq: u64,
    rows: HashMap<usize, RealBodyStream>,
    /// Global-root handles of rows whose carrier DIED (gc-common w27-b): the
    /// lock-key sweep ([`forget_huc_obj_keys`]) drops such a row but cannot
    /// release a root -- that takes a `NativeContext` -- so it parks the
    /// handle here and the next [`real_body_stream_insert`] /
    /// [`real_body_stream_forget`] of the VM releases it. Before, a dead
    /// carrier's UNSENT row was never evicted (only sent rows are) and kept
    /// its body stream reachable for the life of the VM.
    orphaned_roots: Vec<usize>,
}

#[derive(Clone, Copy)]
struct RealBodyStream {
    /// `add_global_root` handle; 0 when the context has no global roots.
    root: usize,
    /// The stream's address when the row was written.
    addr: ObjectRef,
    /// Insertion order, for the bound below.
    seq: u64,
    /// `perform` has read the body. Only such a row is ever evicted.
    sent: bool,
}

/// Per-VM bound on sent-but-never-`disconnect`ed rows. Nothing tells this
/// table when a carrier dies, and most callers never call `disconnect`, so
/// past the bound the oldest SENT row is dropped (and its root released). A
/// row whose body has not been sent yet is never evicted: dropping it would
/// lose the request body.
const REAL_BODY_STREAMS_PER_VM: usize = 1024;

/// The current address of the body stream recorded for carrier `key`.
fn real_body_stream_get(ctx: &dyn NativeContext, key: usize) -> Option<ObjectRef> {
    let row = REAL_BODY_STREAMS
        .peek(ctx.vm_identity(), |t| t.rows.get(&key).copied())
        .flatten()?;
    Some(real_body_stream_resolve(ctx, row))
}

fn real_body_stream_resolve(ctx: &dyn NativeContext, row: RealBodyStream) -> ObjectRef {
    if row.root == 0 {
        return row.addr;
    }
    ctx.resolve_global_root(row.root).unwrap_or(row.addr)
}

/// Record `baos` as carrier `key`'s body stream, rooting it. Also releases
/// the roots of rows whose carrier died since ([`RealBodyStreams::orphaned_roots`]).
fn real_body_stream_insert(ctx: &mut dyn NativeContext, key: usize, baos: ObjectRef) {
    let root = ctx.add_global_root(baos);
    let released = REAL_BODY_STREAMS.with(ctx.vm_identity(), |t| {
        let mut released = std::mem::take(&mut t.orphaned_roots);
        if let Some(old) = t.rows.remove(&key) {
            released.push(old.root);
        }
        if t.rows.len() >= REAL_BODY_STREAMS_PER_VM {
            let oldest_sent = t
                .rows
                .iter()
                .filter(|(_, row)| row.sent)
                .min_by_key(|(_, row)| row.seq)
                .map(|(k, _)| *k);
            if let Some(evict) = oldest_sent.and_then(|k| t.rows.remove(&k)) {
                released.push(evict.root);
            }
        }
        let seq = t.next_seq;
        t.next_seq = seq.wrapping_add(1);
        t.rows.insert(
            key,
            RealBodyStream {
                root,
                addr: baos,
                seq,
                sent: false,
            },
        );
        released
    });
    for handle in released {
        if handle != 0 {
            ctx.remove_global_root(handle);
        }
    }
}

/// Drop carrier `key`'s body stream row (if `key` is `Some`) and release its
/// root, with the roots of rows whose carrier died since
/// ([`RealBodyStreams::orphaned_roots`]).
fn real_body_stream_forget(ctx: &mut dyn NativeContext, key: Option<usize>) {
    let vm = ctx.vm_identity();
    // Every carrier mint lands here; do not create a row for a VM that never
    // buffered a body.
    if !REAL_BODY_STREAMS.has_row(vm) {
        return;
    }
    let released = REAL_BODY_STREAMS.with(vm, |t| {
        let mut released = std::mem::take(&mut t.orphaned_roots);
        if let Some(row) = key.and_then(|k| t.rows.remove(&k)) {
            released.push(row.root);
        }
        released
    });
    for handle in released {
        if handle != 0 {
            ctx.remove_global_root(handle);
        }
    }
}

/// Drop VM `vm`'s `HttpURLConnection` side tables (VM teardown, from
/// `lib.rs::forget_vm_native_root_stores`). The global roots die with the
/// VM's own root table.
pub(crate) fn forget_vm_http_url_connection_state(vm: usize) {
    REAL_BODY_STREAMS.forget(vm);
    // gc-common w17-e: the VM's synthetic-carrier connection rows, response
    // bodies included.
    CONN_REGISTRY.forget(vm);
    // gc-common w11-a: results, requests and live fixed-length uploads (their
    // sockets close as the last `Arc` goes).
    REAL_CARRIERS.forget(vm);
    // gc-common w16-f: the VM's handshake records and response-stream rows
    // (each names its VM since gc-common w27-b). The lock-key teardown that
    // follows frees every key of the VM and would drop them too
    // ([`forget_huc_obj_keys`]); this keeps the teardown in one place.
    if let Ok(mut t) = https_peer_info().lock() {
        t.retain(|_, info| info.vm != vm);
    }
    if let Ok(mut t) = https_response_streams().lock() {
        t.retain(|_, row| row.vm != vm);
    }
    huc_keyed_vms().lock().retain(|v| *v != vm);
}

/// gc-common w27-b (`common-w26b-tls-sibling-tables-keyed-by-vm-folded-identity-hash`):
/// drop every row this file filed under a weak lock key the registry has just
/// FREED -- its object died (`lib.rs::sweep_lock_keys`, after every
/// collection) or its VM went (`lib.rs::forget_vm_lock_keys`). A freed key is
/// never minted again, so nothing could read these rows any more. `keys`
/// holds every weak key the sweep freed, of every kind; most name no row
/// here. Replaces the `t27_tls` `HucCarrier` weak owner (and its
/// `gc_forget_real_carrier` arm), keyed by the folded identity hash.
///
/// Drops: a dead carrier's handshake record, its synthetic connection rows,
/// its cached real-carrier response, request and live upload, and its
/// request-body stream row (whose global root is parked for the VM's next
/// body-stream call -- see [`RealBodyStreams::orphaned_roots`]); a dead
/// stream's response-stream association or fixed-length ownership; and a
/// response-stream association whose CARRIER died -- nothing can drain it
/// usefully any more (both of the carrier's keys are freed, and since
/// gc-common w28-a `net_phase_e`'s session table is keyed by the carrier's
/// weak lock key too, so no other carrier can be named by them).
///
/// Every table lock is taken on its own, never nested, never under the
/// lock-key registry's (both callers release it first). Response bodies and
/// upload sockets are dropped after their table's guard is released. Nothing
/// here reaches Java.
pub(crate) fn forget_huc_obj_keys(keys: &[usize]) {
    if keys.is_empty() {
        return;
    }
    let mut freed = FreedHucKeys::new(keys);
    // The process-wide tables.
    let dead_peers: Vec<HttpsPeerInfo> = https_peer_info()
        .lock()
        .map(|mut table| take_huc_rows(&mut *table, &mut freed))
        .unwrap_or_default();
    drop(dead_peers);
    if let Ok(mut table) = https_response_streams().lock() {
        if !table.is_empty() {
            table.retain(|stream, row| !freed.contains(*stream) && !freed.contains(row.peer_key));
        }
    }
    // The per-VM tables of every VM that ever keyed a row here.
    let vms: Vec<usize> = huc_keyed_vms().lock().to_vec();
    for vm in vms {
        let conns = conn_registry_mut(vm, |reg| reg.forget_owners(&mut freed));
        drop(conns);
        if REAL_CARRIERS.has_row(vm) {
            let (results, reqs, uploads) = REAL_CARRIERS.with(vm, |t| {
                let results = take_huc_rows(&mut t.results, &mut freed);
                let reqs = take_huc_rows(&mut t.reqs, &mut freed);
                let uploads = take_huc_rows(&mut t.fixed_streams, &mut freed);
                if !t.fixed_owners.is_empty() {
                    t.fixed_owners
                        .retain(|stream, carrier| !freed.contains(*stream) && !freed.contains(*carrier));
                }
                (results, reqs, uploads)
            });
            // Bodies and upload sockets go outside the table lock (a socket
            // closes as its last `Arc` drops).
            drop((results, reqs, uploads));
        }
        if REAL_BODY_STREAMS.has_row(vm) {
            REAL_BODY_STREAMS.with(vm, |t| {
                let dead = take_huc_rows(&mut t.rows, &mut freed);
                t.orphaned_roots
                    .extend(dead.into_iter().map(|row| row.root).filter(|h| *h != 0));
            });
        }
    }
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

/// One live upload behind its own lock, so its socket I/O never runs under
/// the `REAL_CARRIERS` table lock (see [`RealCarrierTables`]). The body
/// stream's identity maps to its owning connection through
/// `RealCarrierTables::fixed_owners`.
type LiveFixedStreamCell = Arc<Mutex<LiveFixedStream>>;

/// Lock one live upload; a poisoned lock (a panic mid-write) still hands the
/// stream back, whose byte counts say how far it got.
fn lock_live_fixed_stream(cell: &LiveFixedStreamCell) -> std::sync::MutexGuard<'_, LiveFixedStream> {
    cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// True if `this` is a real-JDK URLConnection carrier (field 0 holds the
/// `java/net/URL` object) rather than our synthetic carrier (field 0 = i32
/// conn-id). The real JDK constructor uses a different descriptor than our
/// `<init>(Ljava/net/URL;)V` native, so a genuine carrier never runs `huc_init`
/// and keeps the JDK layout (field 0 = `URLConnection.url`).
fn is_real_carrier(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    matches!(ctx.get_field(this, HUC_CONN_ID), Value::Object(Some(_)))
}

/// Fold the request properties the REAL `sun.net.www.URLConnection.requests`
/// (`MessageHeader`) holds into this carrier's `RealReq`.
///
/// Under `--jdk-only` `setRequestProperty` / `addRequestProperty` run the
/// JDK's own bytecode (real bytes beat the registered natives), which fills
/// `requests` -- and the native send path below reads only `real_reqs()`, so
/// every header the application set (`Authorization`, a framework's
/// `Framework-Name`, ...) was silently NOT sent. MEASURED 2026-09-18:
/// `getRequestProperties()` showed the header, the server never received it
/// (`ResourceTests$UrlResourceTests.canCustomizeHttpUrlConnectionForExists`).
/// A pair already present (same name, case-insensitively, same value) is not
/// added twice, so a run where the natives DID populate the table is unchanged.
///
/// # `this` is `&mut` (gen r4w3/rooting)
///
/// `getKey`/`getValue` are Java, so every iteration is a GC point: the
/// `MessageHeader` is pinned and re-read per call, and the carrier is pinned
/// and handed back so both this function's `identity_hash_code` key and the
/// CALLER's copy name the current address (the `native-api` rule on
/// `pin_native_root`; `stale-receiver-audit.py` flagged this funnel).
fn merge_real_message_headers(ctx: &mut dyn NativeContext, this: &mut ObjectRef) {
    let Value::Object(Some(mh)) = ctx.get_field_by_name(*this, "requests") else {
        return;
    };
    let Value::Int(n) = ctx.get_field_by_name(mh, "nkeys") else {
        return;
    };
    let this_pin = ctx.pin_native_root(*this);
    let mh_pin = ctx.pin_native_root(mh);
    let mut pairs: Vec<(String, String)> = Vec::new();
    for i in 0..n.max(0) {
        let text = |ctx: &mut dyn NativeContext, method: &str| -> Option<String> {
            let mh = ctx.read_native_pin(mh_pin, mh);
            match ctx.invoke_virtual(mh, method, "(I)Ljava/lang/String;", &[Value::Int(i)]) {
                Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
                _ => None,
            }
        };
        // A null key is the request line (`GET /x HTTP/1.1`), not a header.
        if let (Some(k), Some(v)) = (text(ctx, "getKey"), text(ctx, "getValue")) {
            pairs.push((k, v));
        }
    }
    *this = ctx.read_native_pin(this_pin, *this);
    ctx.unpin_native_roots(this_pin);
    if pairs.is_empty() {
        return;
    }
    with_real_req(ctx, *this, |r| {
        for (k, v) in pairs {
            if !r
                .headers
                .iter()
                .any(|(hk, hv)| hk.eq_ignore_ascii_case(&k) && *hv == v)
            {
                r.headers.push((k, v));
            }
        }
    });
}

/// Mutate this real carrier's `RealReq` entry (creating it on first use).
/// A writer: mints the carrier's key ([`huc_obj_key`]; `this` current).
fn with_real_req<R>(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    f: impl FnOnce(&mut RealReq) -> R,
) -> R {
    let key = huc_obj_key(ctx, this);
    real_reqs_with(ctx.vm_identity(), |t| f(t.entry(key).or_default()))
}

/// If `this` is a real-JDK URLConnection (field 0 is a `java/net/URL` object,
/// not our synthetic int conn-id), return its full external-form URL string via
/// `URL.toExternalForm()` (robust for both synthetic and real URL layouts).
fn huc_real_object_url(ctx: &mut dyn NativeContext, this: &mut ObjectRef) -> Option<String> {
    let url_obj = match ctx.get_field(*this, HUC_CONN_ID) {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    // `toExternalForm()` is ordinary Java and it allocates, so a moving young
    // collection can complete inside this call and relocate the carrier.
    //
    // `this` is a bare Rust local. The caller's `safe_native_call` pin keeps the
    // OBJECT alive and the collector remaps THAT pin — but not this copy, and
    // not the caller's. Every caller then keys a side table on
    // `identity_hash_code(this)` one statement later, which reads the mark word
    // at the address the collection moved away from. On the Generational
    // collector that address is in the semispace the flip emptied, and
    // `CRATONVM_GEN_UNCOMMIT` has handed the granule back to the OS, so the read
    // is a SIGSEGV rather than a wrong answer.
    //
    // `&mut ObjectRef` rather than a pin private to this function: pinning for
    // our own use fixes nothing when the CALLER keeps the pre-move copy, and
    // that is exactly the half that was missing.
    let this_pin = ctx.pin_native_root(*this);
    let out = match ctx.invoke_virtual(url_obj, "toExternalForm", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    };
    *this = ctx.read_native_pin(this_pin, *this);
    ctx.unpin_native_roots(this_pin);
    out
}

/// Read the buffered request body for a real-JDK carrier from the
/// `ByteArrayOutputStream` recorded by `getOutputStream` (synthetic BAOS layout:
/// field 0 = byte[] buf, field 1 = count). Empty if no body was written.
fn real_body_bytes(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<u8> {
    // A carrier never keyed has no body row.
    let Some(key) = huc_existing_obj_key(ctx, this) else {
        return Vec::new();
    };
    // Resolve the CURRENT address through the row's global root (the stored
    // copy is stale after any moving GC), and mark the row sent: from here on
    // the bound in `real_body_stream_insert` may evict it.
    let row = REAL_BODY_STREAMS
        .peek(ctx.vm_identity(), |t| t.rows.get(&key).copied())
        .flatten();
    let Some(row) = row else {
        return Vec::new();
    };
    REAL_BODY_STREAMS.with(ctx.vm_identity(), |t| {
        if let Some(r) = t.rows.get_mut(&key) {
            r.sent = true;
        }
    });
    let baos = real_body_stream_resolve(ctx, row);
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
    let vm = ctx.vm_identity();
    // Every `ByteArrayOutputStream` write in the process lands here. With no
    // live fixed-length upload in this VM -- the common case -- answer at
    // once, without the identity hash or the lock-key registry (gc-common
    // w27-b: the stream's key lookup takes that process-wide mutex).
    let uploading = REAL_CARRIERS
        .peek(vm, |t| !t.fixed_owners.is_empty())
        .unwrap_or(false);
    if !uploading {
        return Ok(false);
    }
    // A stream never keyed owns no upload.
    let Some(baos_key) = huc_existing_obj_key(&*ctx, baos) else {
        return Ok(false);
    };
    let Some(conn_key) = REAL_CARRIERS
        .peek(vm, |t| t.fixed_owners.get(&baos_key).copied())
        .flatten()
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
    // The cell is cloned out and the table lock released before any socket
    // I/O below (gc-common w11-a).
    let cell = REAL_CARRIERS
        .peek(vm, |t| t.fixed_streams.get(&conn_key).cloned())
        .flatten()
        .ok_or_else(|| ioex("HttpURLConnection fixed-length stream is no longer available"))?;
    let mut guard = lock_live_fixed_stream(&cell);
    let stream: &mut LiveFixedStream = &mut guard;

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

/// Take VM `vm`'s live upload for carrier `conn_key` out of the table. The
/// caller locks the returned cell (see [`lock_live_fixed_stream`]).
fn take_live_fixed_stream(vm: usize, conn_key: usize) -> Option<LiveFixedStreamCell> {
    if !REAL_CARRIERS.has_row(vm) {
        return None;
    }
    REAL_CARRIERS.with(vm, |t| {
        let stream = t.fixed_streams.remove(&conn_key);
        if stream.is_some() {
            t.fixed_owners.retain(|_, owner| *owner != conn_key);
        }
        stream
    })
}

fn forget_live_fixed_stream(vm: usize, conn_key: usize) {
    if !REAL_CARRIERS.has_row(vm) {
        return;
    }
    let removed = REAL_CARRIERS.with(vm, |t| {
        let removed = t.fixed_streams.remove(&conn_key);
        t.fixed_owners.retain(|_, owner| *owner != conn_key);
        removed
    });
    // The upload's socket closes as its last `Arc` drops: outside the table
    // lock (gc-common w27-b).
    drop(removed);
}

/// Open a plain HTTP connection and write the request head now.  HTTPS keeps
/// the established buffered path because its rustls stream may re-enter Java
/// for key-manager callbacks; moving that stateful handshake into a write hook
/// would require a separate TLS stream owner.  The Tomcat regression and the
/// JDK fixed-length contract exercised here are plain HTTP.
fn start_live_fixed_stream(
    ctx: &mut dyn NativeContext,
    mut this: ObjectRef,
    baos: ObjectRef,
    expected: u64,
) -> Result<bool, MethodCallFailed> {
    // Keyed BEFORE the Java calls below (`toExternalForm`, the header merge):
    // `baos` is a bare local they can move, and its key is the one thing about
    // it that survives a move (gc-common w10-e; its weak lock key since
    // w27-b).
    let baos_key = huc_obj_key(&*ctx, baos);
    let Some(url_str) = huc_real_object_url(ctx, &mut this) else {
        return Ok(false);
    };
    let parsed = parse_url(&url_str).map_err(ioex)?;
    if parsed.scheme != "http" {
        return Ok(false);
    }
    // `this` is current: `huc_real_object_url` handed it back re-read.
    let conn_key = huc_obj_key(&*ctx, this);
    let vm = ctx.vm_identity();
    if REAL_CARRIERS
        .peek(vm, |t| t.fixed_streams.contains_key(&conn_key))
        .unwrap_or(false)
    {
        return Ok(true);
    }
    merge_real_message_headers(ctx, &mut this);
    let req = real_reqs_peek(vm, |reqs| reqs.get(&conn_key).cloned())
        .flatten()
        .unwrap_or_default();
    let method_owned = real_method(ctx, this).unwrap_or_else(|| "POST".to_string());
    let method = method_owned.as_str();
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

    let cell: LiveFixedStreamCell = Arc::new(Mutex::new(LiveFixedStream {
        tcp,
        expected,
        written: 0,
        closed: false,
    }));
    REAL_CARRIERS.with(vm, |t| {
        t.fixed_streams.insert(conn_key, cell);
        t.fixed_owners.insert(baos_key, conn_key);
    });
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

/// [`huc_real_perform_inner`], with the carrier kept rooted across the WHOLE
/// exchange and the caller's `this` rewritten to the post-move address.
///
/// The inner body already pins `this` for its own redirect loop, under a comment
/// naming this very hazard: `perform` parks the thread in a GC-blocking region
/// for every socket wait, so a moving collection can complete mid-exchange. What
/// it never did was tell the CALLER. Every one of them holds a bare `ObjectRef`
/// local across this call and then reaches `identity_hash_code(this)` —
/// `huc_get_response_message`, `huc_get_input_stream` and
/// `huc_get_content_length` do it within one or two statements — so the fix
/// belongs at the boundary, not inside.
///
/// The window is at its widest exactly where it was measured: an HTTP request to
/// a host that does not answer parks here for the whole connect timeout.
fn huc_real_perform(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
    url_str: &str,
) -> Result<i32, MethodCallFailed> {
    // Taken BELOW the inner body's own pin, so the inner
    // `unpin_native_roots(this_pin)` — which releases its base and everything
    // above it — cannot release this one.
    let pin = ctx.pin_native_root(*this);
    let out = huc_real_perform_inner(ctx, *this, url_str);
    *this = ctx.read_native_pin(pin, *this);
    ctx.unpin_native_roots(pin);
    out
}

fn huc_real_perform_inner(
    ctx: &mut dyn NativeContext,
    mut this: ObjectRef,
    url_str: &str,
) -> Result<i32, MethodCallFailed> {
    // gc-common w12-a (`common-w11a-huc-real-results-hold-every-response-body-
    // for-the-life-of-a-vm`): the rows this exchange writes -- its cached
    // response, body included -- go once the carrier dies. Since gc-common
    // w27-b they are filed under the carrier's weak lock key, which the
    // lock-key sweep frees then ([`forget_huc_obj_keys`]); `this` is current
    // here: `huc_real_perform` hands it over before anything allocates.
    let key = huc_obj_key(&*ctx, this);
    let vm = ctx.vm_identity();
    if let Some(st) = real_results_peek(vm, |t| t.get(&key).map(|r| r.status)).flatten() {
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
    if let Some(live_cell) = take_live_fixed_stream(vm, key) {
        let mut live = lock_live_fixed_stream(&live_cell);
        // The head is already on the wire with `Content-Length: expected`, and
        // the peer is waiting for exactly that many bytes. If the live stream
        // did not see them, the body is not lost — it is in the request BAOS,
        // which is what every OTHER path sends. Push the remainder rather than
        // abandoning a request whose body this VM is holding.
        //
        // This is not a rare corner. In real-JDK mode `getOutputStream()`
        // hands back a genuine `java.io.ByteArrayOutputStream` and its writes
        // are the JDK's OWN bytecode, which dispatches no `BaosEvent` — so the
        // live stream observes NOTHING and `written` is 0 for every
        // fixed-length request. MEASURED: `L6HttpLoopbackSweep` row 64 read
        // "fixed-length stream has 0 bytes; expected 4" on a connection that
        // had been handed all four.
        // Set when the body write below fails with the peer having already
        // closed its read side (a broken pipe) — carried past the read below
        // so it can still be reported if no response is readable either, but
        // does not by itself abort the request: see the comment at the write
        // site for why.
        let mut write_broken_pipe: Option<std::io::Error> = None;
        if live.written < live.expected {
            let body = real_body_bytes(ctx, this);
            if body.len() as u64 == live.expected {
                use std::io::Write as _;
                let from = live.written as usize;
                ctx.begin_blocking_region();
                let pushed = live
                    .tcp
                    .write_all(&body[from..])
                    .and_then(|()| live.tcp.flush());
                ctx.end_blocking_region();
                match pushed {
                    Ok(()) => live.written = live.expected,
                    Err(e) => {
                        // The peer can send a complete response and close its
                        // read side (e.g. rejecting or fully satisfying the
                        // request) before this thread finishes streaming the
                        // body; the write then fails with a broken pipe even
                        // though a valid response is already sitting in the
                        // socket's receive buffer. The JDK tolerates exactly
                        // this rather than surfacing the write failure, so
                        // fall through to read whatever response is
                        // available and only report the write error if no
                        // response can be read either.
                        write_broken_pipe = Some(e);
                    }
                }
            }
        }
        if write_broken_pipe.is_none() && live.written != live.expected {
            return Err(ioex(format!(
                "HttpURLConnection fixed-length stream has {} bytes; expected {} before reading the response",
                live.written, live.expected
            )));
        }
        // `real_method`, not the side table alone — see its note: on the
        // carrier `URL.openConnection()` mints, `setRequestMethod` is the
        // JDK's own retired bytecode and writes the FIELD.
        let method = real_method(ctx, this).unwrap_or_default();
        ctx.begin_blocking_region();
        let response = read_response(&mut live.tcp, method.eq_ignore_ascii_case("HEAD"));
        ctx.end_blocking_region();
        return match response {
            Ok((status, headers, body)) => {
                let reason = LAST_REASON_PHRASE.with(|r| r.borrow().clone());
                let truncated = take_response_truncated();
                real_results_with(vm, |results| {
                    results.insert(
                        key,
                        RealResult {
                            status,
                            headers,
                            body,
                            reason,
                            truncated,
                        },
                    );
                });
                Ok(status)
            }
            Err(ref e) if e == READ_TIMEOUT_SENTINEL => {
                real_results_with(vm, |results| {
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
                });
                Err(socket_timeout_ex("Read timed out"))
            }
            // A fixed-length streaming request has already committed its head
            // and body to this connection.  If the peer aborts before a
            // response can be parsed, surface that transport failure as the
            // IOException HotSpot exposes to `postUrl`, rather than treating it
            // like a malformed buffered response and returning -1. When the
            // body write itself broke the pipe and no response could be read
            // either, the write failure is the more useful diagnostic.
            Err(e) => Err(match write_broken_pipe {
                Some(write_err) => ioex(format!(
                    "HttpURLConnection fixed-length body write failed: {write_err}"
                )),
                None => ioex(format!("HttpURLConnection streaming response failed: {e}")),
            }),
        };
    }
    merge_real_message_headers(ctx, &mut this);
    let req = real_reqs_peek(vm, |t| t.get(&key).cloned())
        .flatten()
        .unwrap_or_default();
    let mut method = real_method(ctx, this).unwrap_or_else(|| "GET".to_string());
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
    // The carrier's CURRENT address, taken while the pin is still live: the
    // success arm below writes its `connected` field, and nothing between here
    // and that write allocates.
    let this_settled = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let resp = final_resp.unwrap_or_else(|| Ok((310, Vec::new(), Vec::new())));
    match resp {
        Ok((status, headers, body)) => {
            let reason = LAST_REASON_PHRASE.with(|r| r.borrow().clone());
            let truncated = take_response_truncated();
            real_results_with(vm, |t| {
                t.insert(
                    key,
                    RealResult {
                        status,
                        headers,
                        body,
                        reason,
                        truncated,
                    },
                );
            });
            // The carrier IS connected now, and `URLConnection`'s own bytecode
            // is what enforces the consequences: `setRequestProperty`,
            // `addRequestProperty`, `getRequestProperties`, `setDoOutput`,
            // `setDoInput` and `setUseCaches` all begin with `checkConnected()`
            // and raise `IllegalStateException("Already connected")`.
            //
            // Those methods are declared on `URLConnection`, which this file
            // does not register, so they run as inherited JDK bytecode against
            // this VM's carrier — and the carrier's `connected` field was
            // written 0 at mint and never again. MEASURED
            // (`L6HttpLoopbackSweep` rows 41, 71, 73): every one of them
            // succeeded after `getResponseCode()`, took the caller's value, and
            // dropped it. Setting the field is the fix; a guard inside a native
            // the dispatch never consults is not.
            ctx.set_field_by_name(this_settled, "connected", Value::Int(1));
            // `getURL()` after a followed redirect names the FINAL URL.
            //
            // The JDK writes it in `followRedirect()` (`url = locUrl`), so
            // `getURL()` — inherited `URLConnection` bytecode reading the
            // carrier's own `url` field, exactly like `connected` above —
            // answers where the response actually came from. This loop
            // tracked the final URL in `current_url` and threw it away, so
            // `L6HttpLoopbackSweep` row 57 read back the 302 rather than the
            // 200 it followed to.
            //
            // Built before the field write and behind a fresh pin: the URL
            // constructor allocates and runs bytecode, so `this_settled` can
            // move under it.
            if current_url != url_str {
                let pin = ctx.pin_native_root(this_settled);
                let spec = ctx.create_string(&current_url);
                if let Ok(Some(Value::Object(Some(final_url)))) = ctx.new_object_initialized(
                    "java/net/URL",
                    "(Ljava/lang/String;)V",
                    &[Value::Object(Some(spec))],
                ) {
                    let this_now = ctx.read_native_pin(pin, this_settled);
                    ctx.set_field_by_name(this_now, "url", Value::Object(Some(final_url)));
                }
                ctx.unpin_native_roots(pin);
            }
            Ok(status)
        }
        // A read timeout maps to java.net.SocketTimeoutException (real-JDK
        // behaviour) — code such as TestConnector.testStop catches it
        // specifically. Cache the sentinel so later getters re-raise without
        // blocking for another full timeout.
        Err(ref e) if e == READ_TIMEOUT_SENTINEL => {
            real_results_with(vm, |t| {
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
            });
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
            // The JDK's message is the CONSTANT `Connection refused (connect
            // failed)` — `PlainSocketImpl` appends "(connect failed)" to the
            // OS `strerror` and nothing else. MEASURED on HotSpot 25.0.4+7
            // (`L6HttpLoopbackSweep` row 76); this VM forwarded Rust's
            // `io::Error` text, which names the address it dialled
            // (`connect 127.0.0.1:37761: Connection refused (os error 111)`).
            // Two defects in one string: an ephemeral PORT, which makes the
            // row differ from ITSELF across runs, and the same species as
            // `addr is of illegal length` — a helpfulness the caller cannot
            // ask for and the oracle does not have.
            let text = e.trim_start_matches(CONNECT_REFUSED_SENTINEL);
            let message = if text.contains("Connection refused") {
                "Connection refused (connect failed)".to_string()
            } else {
                text.to_string()
            };
            Err(RuntimeError::ConnectException { message }.into())
        }
        // A transport failure before a response is available is an IOException.
        Err(e) => Err(ioex(format!("HttpURLConnection response failed: {e}"))),
    }
}

/// Cached response body for a real-JDK connection (empty if not performed).
fn huc_real_body(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<u8> {
    real_result_of(ctx, this, |r| r.body.clone()).unwrap_or_default()
}

/// Whether the cached response for a real-JDK connection was truncated by the
/// peer (see `LAST_RESPONSE_TRUNCATED`).
fn huc_real_truncated(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    real_result_of(ctx, this, |r| r.truncated).unwrap_or(false)
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
    // Keyed before the stream's allocation can move the carrier (w16-f).
    let carrier_keys = carrier.map(|c| https_carrier_keys(&*ctx, c));
    let head = make_byte_array_input_stream(ctx, body);
    // Associate the BAIS — never the `SequenceInputStream` wrapper — with the
    // carrier: the BAIS is what `native-io` observes, and on the truncated
    // path the wrapper produces no `BaisEvent` of its own. The truncated case
    // is registered too, deliberately: its EOF still means the application is
    // done with the bytes that arrived, and the error tail that follows is a
    // read failure, not a reason to keep the connection's view open.
    if let Ok(Value::Object(Some(head_ref))) = head {
        note_response_stream_keyed(&*ctx, head_ref, carrier_keys);
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
    real_result_of(ctx, this, |r| r.headers.clone()).unwrap_or_default()
}

/// True once this real carrier has performed its request.
///
/// `URLConnection`'s setters are contracted to raise `IllegalStateException`
/// after connect, and every one of them here accepted the change silently and
/// dropped it: `setRequestProperty` after `getResponseCode()` looked like it
/// worked, `getRequestProperty` read the value back, and the header was never
/// sent. Measured (`L6HttpLoopbackSweep` rows 41, 71, 73).
fn real_is_connected(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    real_result_of(ctx, this, |_| ()).is_some()
}

/// The response headers as the JDK INDEXES them: element 0 is the status line,
/// whose key is null, and the real headers follow from index 1.
///
/// `URLConnection.getHeaderField(int)` and `getHeaderFieldKey(int)` walk
/// `sun.net.www.MessageHeader`, whose first entry is the status line stored
/// under a null key. This VM indexed the header list directly, so every index
/// was one too low and index 0 answered the `Date` header where HotSpot
/// answers `HTTP/1.1 200 OK`. MEASURED (`L6HttpLoopbackSweep` rows 15-17).
///
/// The same null key appears in `getHeaderFields()`, which is why callers can
/// read the status line out of the map.
fn huc_real_indexed_headers(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Vec<(Option<String>, String)> {
    let Some((status, reason, headers)) =
        real_result_of(ctx, this, |r| (r.status, r.reason.clone(), r.headers.clone()))
    else {
        return Vec::new();
    };
    let reason = if reason.is_empty() {
        status_reason(status)
    } else {
        reason
    };
    let mut out: Vec<(Option<String>, String)> = Vec::with_capacity(headers.len() + 1);
    out.push((None, format!("HTTP/1.1 {status} {reason}")));
    for (k, v) in headers {
        out.push((Some(k), v));
    }
    out
}

/// Drop all identity-keyed side-table state for a real carrier.
///
/// Called on `disconnect()` and — the reason it is `pub(crate)` — on every
/// carrier `URL.openConnection()` mints. These tables are keyed by identity
/// hash, which this VM derives per object and REUSES once the first object is
/// collected, so a fresh connection allocated where an old one died inherits
/// its method, its headers and its streaming mode. MEASURED
/// (`L6HttpLoopbackSweep`): a fresh connection's `setFixedLengthStreamingMode`
/// threw `IllegalStateException: Chunked encoding streaming mode set` naming a
/// mode nobody had set on it, and `setRequestMethod("PUT")` sent POST.
///
/// The mint site is the one moment a carrier is known to be new. The
/// constructor is NOT that moment: `URL.openConnection()` allocates the
/// carrier directly and never calls it, which is where the first attempt at
/// this fix went and why it changed nothing.
///
/// `&mut` since gc-common w10-e: dropping the body-stream row releases its
/// global root.
///
/// Scoped to the calling VM since gc-common w11-a: another VM's carrier with
/// the same identity hash is not this VM's business. Keyed by the carrier's
/// weak lock key since gc-common w27-b: a carrier never keyed has no rows (a
/// fresh one never inherits a dead one's, nor a live collider's), and the
/// lookup mints nothing. `this` must be current.
pub(crate) fn real_forget(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let key = huc_existing_obj_key(&*ctx, this);
    let vm = ctx.vm_identity();
    // Every mint lands here: do not create a row for a VM that never used a
    // real carrier's side tables.
    if let Some(key) = key {
        if REAL_CARRIERS.has_row(vm) {
            REAL_CARRIERS.with(vm, |t| {
                t.results.remove(&key);
                t.reqs.remove(&key);
            });
        }
    }
    // Also releases the roots of dead carriers' body streams.
    real_body_stream_forget(ctx, key);
    if let Some(key) = key {
        forget_live_fixed_stream(vm, key);
    }
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

/// The bytes of a Java `byte[]` (a request body on its way out). One bulk
/// `read_byte_array_into` rather than a `get_array_element` per byte
/// (gc-common w10-e: the per-byte walk was the cost of every POST body).
fn read_byte_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut out = vec![0u8; len];
    let n = ctx.read_byte_array_into(arr, 0, &mut out);
    out.truncate(n);
    out
}

/// A Java `byte[]` holding `bytes`, or a catchable `OutOfMemoryError`.
///
/// gc-common w10-e: a response body is sized by the SERVER, and the
/// infallible `new_array` this used hard-aborts the VM when the array cannot
/// fit, where HotSpot throws `OutOfMemoryError` from `getInputStream`. It does
/// not reclaim-and-retry: its callers hold raw references a collection would
/// strand. The copy is one bulk `write_byte_array_from` instead of a
/// `set_array_element` per byte.
fn new_byte_array(
    ctx: &mut dyn NativeContext,
    bytes: &[u8],
) -> Result<ObjectRef, MethodCallFailed> {
    let Some(arr) = ctx.try_new_array(ArrayElementType::Byte, bytes.len()) else {
        return Err(RuntimeError::OutOfMemoryError {
            message: format!("Java heap space (HTTP response body of {} bytes)", bytes.len()),
        }
        .into());
    };
    // Cannot fail: `arr` is a fresh `byte[]` of exactly `bytes.len()`.
    let _ = ctx.write_byte_array_from(arr, 0, bytes);
    Ok(arr)
}

/// Build a `java/io/ByteArrayInputStream` over `body` (4-field synthetic:
/// buf=0, pos=1, mark=2, count=3).
fn make_byte_array_input_stream(
    ctx: &mut dyn NativeContext,
    body: &[u8],
) -> Result<Value, MethodCallFailed> {
    let body_arr = new_byte_array(ctx, body)?;
    // The stream allocation can move `body_arr` (gc-common w10-e).
    let arr_pin = ctx.pin_native_root(body_arr);
    let stream = try_alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4);
    let body_arr = ctx.read_native_pin(arr_pin, body_arr);
    ctx.unpin_native_roots(arr_pin);
    let stream = stream?;
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
/// `getHeaderFields()`'s map.
///
/// Two properties of the JDK's that this used to miss, both measured on
/// HotSpot 25.0.4+7 (`L6HttpLoopbackSweep` rows 27-28):
///
/// * the STATUS LINE is in the map under a **null key**, and callers read it
///   from there;
/// * the map is **unmodifiable** — `Collections.unmodifiableMap` in the JDK,
///   and a bare `HashMap` here, so a caller could edit the response headers of
///   a connection and see its own edits back.
fn build_header_map(
    ctx: &mut dyn NativeContext,
    headers: &[(String, String)],
    status_line: Option<&str>,
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
    if let Some(line) = status_line {
        let vs = ctx.create_string(line);
        let vs_pin = ctx.pin_native_root(vs);
        let list = match ctx.new_object_initialized("java/util/ArrayList", "()V", &[])? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                ctx.unpin_native_roots(map_pin);
                return Err(ioex("getHeaderFields: could not allocate ArrayList"));
            }
        };
        let list_pin = ctx.pin_native_root(list);
        let vs = ctx.read_native_pin(vs_pin, vs);
        let list = ctx.read_native_pin(list_pin, list);
        ctx.invoke(
            "java/util/ArrayList",
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(list)), Value::Object(Some(vs))],
        )?;
        let map = ctx.read_native_pin(map_pin, map);
        let list = ctx.read_native_pin(list_pin, list);
        ctx.invoke(
            "java/util/HashMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(map)),
                Value::Object(None),
                Value::Object(Some(list)),
            ],
        )?;
    }
    let map = ctx.read_native_pin(map_pin, map);
    // `Collections.unmodifiableMap` is real bytecode and allocates, so the
    // map stays pinned across it.
    let wrapped = ctx.invoke(
        "java/util/Collections",
        "unmodifiableMap",
        "(Ljava/util/Map;)Ljava/util/Map;",
        &[Value::Object(Some(map))],
    );
    let map = ctx.read_native_pin(map_pin, map);
    ctx.unpin_native_roots(map_pin);
    match wrapped {
        Ok(Some(Value::Object(Some(view)))) => Ok(view),
        _ => Ok(map),
    }
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
    // The VM's default lives in a GC-rooted native slot, NOT in the real
    // JDK static field: writing that field does not stick on this VM
    // (measured — see `t27_tls::HUC_DEFAULT_FACTORY`), which silently
    // disabled this whole mechanism. One slot per VM (gc-common w9-b).
    let Some(factory) = instance_factory
        .or_else(|| crate::t27_tls::huc_default_ssl_socket_factory_in_vm(ctx.vm_identity()))
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
/// — see `docs/internal/jdk-only/E3-1-the-cipher-name-helper-and-its-real-denominator.md`.
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
    // The carrier too (gc-common w16-f): the session lookup below keys on it,
    // and the host String's allocation can move it. Released with
    // `verifier_pin`, which it sits above.
    let conn_pin = connection.map(|c| (ctx.pin_native_root(c), c));
    let host_s0 = ctx.create_string(host);
    let host_pin = ctx.pin_native_root(host_s0);
    let connection = conn_pin.map(|(pin, c)| ctx.read_native_pin(pin, c));

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
    // Pinned by `perform_with_retry` (gc-common w16-f): read it through
    // `PinnedCarrier::current` at each use, never hold the address.
    connection: Option<PinnedCarrier>,
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
        //
        // One capture drives the whole connection (gc-common w10-e): the
        // connection's own `setSSLSocketFactory` capture, else THIS VM's
        // `setDefaultSSLSocketFactory` capture. The restricted rebuild and the
        // post-handshake `TrustManager` gate below read the same capture, so
        // a connection-scoped `TrustManager` is the one consulted for that
        // connection. They used to read the process defaults, which let a
        // connection whose factory installed a pass-through-verified custom
        // `TrustManager` skip it whenever no default had one.
        //
        // Read through the pin (gc-common w16-f): the TCP connect above was a
        // GC-blocking region and the class warm-up allocates.
        let tls = PinnedCarrier::current(connection, &*ctx)
            .and_then(|connection| crate::t27_tls::huc_tls_for_connection(ctx, connection))
            .or_else(|| crate::t27_tls::huc_default_tls_in_vm(ctx.vm_identity()));
        let cfg = tls
            .as_ref()
            .and_then(|capture| capture.client_config.clone())
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
                match crate::t27_tls::build_huc_restricted_client_config(
                    tls.as_deref(),
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
        // `connection` is still used after the handshake (by
        // `record_https_peer_info` and `huc_verify_hostname`). It is pinned by
        // `perform_with_retry` and read through the pin right before those
        // uses (gc-common w16-f). It used to ride through
        // `end_blocking_region_refs`, which covered the parked socket waits
        // but not the Java that runs OUTSIDE them -- `process_new_packets`'
        // `KeyManager` callbacks and the `TrustManager` check -- so the
        // address handed to both helpers could predate a collection.
        let outcome = (|| -> Result<(i32, Vec<(String, String)>, Vec<u8>), String> {
            let active_ctx_guard = crate::t27_tls::set_active_native_context(ctx);
            while stream.conn.is_handshaking() {
                if std::time::Instant::now() > deadline {
                    return Err(format!(
                        "{TLS_HANDSHAKE_FAILURE_SENTINEL}TLS handshake timed out"
                    ));
                }
                if stream.conn.wants_write() {
                    ctx.begin_blocking_region();
                    let written = stream.conn.write_tls(&mut EintrIo::new(&mut stream.sock));
                    ctx.end_blocking_region();
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
                    ctx.begin_blocking_region();
                    let read = stream.conn.read_tls(&mut EintrIo::new(&mut stream.sock));
                    ctx.end_blocking_region();
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
            if let Some(tm_ctx_key) = tls.as_ref().and_then(|capture| capture.tm_ctx_key) {
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
                //
                // The carrier's CURRENT address: the `TrustManager` check above
                // and the `KeyManager` callbacks are Java (gc-common w16-f).
                let connection = PinnedCarrier::current(connection, &*ctx);
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
///
/// # The carrier is pinned here (gc-common w16-f)
///
/// `perform` parks in GC-blocking regions (the TCP connect, every handshake
/// read and write) and runs Java (`KeyManager` callbacks inside
/// `process_new_packets`, the `TrustManager` check), and it used the carrier
/// after all of them: `huc_tls_for_connection` keyed its capture lookup by the
/// identity hash of the pre-connect address, `record_https_peer_info` and
/// `huc_verify_hostname` received the pre-`TrustManager` address, and the
/// retry below handed the SECOND `perform` the address from before the whole
/// first exchange. One pin covers both attempts; `perform` reads the current
/// address through it at every use.
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
    let pinned = connection.map(|c| PinnedCarrier {
        pin: ctx.pin_native_root(c),
        fallback: c,
    });
    let out = perform_with_retry_pinned(
        ctx,
        pinned,
        parsed,
        method,
        headers,
        body,
        connect_timeout,
        read_timeout,
        tls_restrictions,
    );
    if let Some(p) = pinned {
        ctx.unpin_native_roots(p.pin);
    }
    out
}

/// A carrier `perform` must re-read after every GC point: the native pin
/// `perform_with_retry` took, plus the address it pinned (the mock contexts'
/// `read_native_pin` answers the fallback).
#[derive(Clone, Copy)]
struct PinnedCarrier {
    pin: usize,
    fallback: ObjectRef,
}

impl PinnedCarrier {
    /// The carrier's CURRENT address.
    fn current(pinned: Option<Self>, ctx: &dyn NativeContext) -> Option<ObjectRef> {
        pinned.map(|p| ctx.read_native_pin(p.pin, p.fallback))
    }
}

#[allow(clippy::too_many_arguments)]
fn perform_with_retry_pinned(
    ctx: &mut dyn NativeContext,
    connection: Option<PinnedCarrier>,
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

/// Perform a SYNTHETIC carrier's request once, and hand the carrier back at its
/// current address.
///
/// # `this` is `&mut` (gc-common w16-f)
///
/// The body is a GC point twice over: `huc_client_tls_restrictions` runs the
/// installed factory's `createSocket` (Java), and `perform` parks in a
/// GC-blocking region for every socket wait. The body used to write
/// `HUC_CONN_ID`/`HUC_CONNECTED` through the address from before both, so a
/// collection during the exchange left the carrier unconnected -- the next
/// accessor sent the request AGAIN -- and every caller then read the response
/// state through that same stale copy. The wrapper pins the carrier across the
/// body and rewrites the caller's copy, the `huc_real_perform` shape.
fn ensure_connected(ctx: &mut dyn NativeContext, this: &mut ObjectRef) -> MethodCallResult {
    let pin = ctx.pin_native_root(*this);
    let out = ensure_connected_body(ctx, pin, *this);
    *this = ctx.read_native_pin(pin, *this);
    ctx.unpin_native_roots(pin);
    out
}

fn ensure_connected_body(
    ctx: &mut dyn NativeContext,
    pin: usize,
    this: ObjectRef,
) -> MethodCallResult {
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
    // That may have run Java (gc-common w16-f): the current address.
    let this = ctx.read_native_pin(pin, this);
    // gc-common w17-e / w27-b: every row this exchange files -- the handshake
    // record `perform` writes for an `https` URL (`record_https_peer_info`),
    // even when the exchange then fails, and the connection row below
    // (`register_synthetic_conn`) -- is filed under the carrier's weak lock
    // key, minted by the writer, and goes when the lock-key sweep frees it
    // (the carrier died). Nothing to record up front any more.
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

    // `perform` parked in GC-blocking regions: write through the carrier's
    // CURRENT address (gc-common w16-f). Read BEFORE the row is filed
    // (gc-common w17-e): the carrier is the row's weak owner, recorded by
    // address, and nothing below allocates.
    let this = ctx.read_native_pin(pin, this);
    let id = register_synthetic_conn(
        ctx,
        this,
        ConnState {
            status,
            response_body: body_bytes,
            response_headers: headers,
            body_consumed: false,
            truncated: take_response_truncated(),
        },
    );
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
    // The calling VM's rows only (gc-common w17-e): ids are per VM.
    CONN_REGISTRY
        .peek(ctx.vm_identity(), |reg| reg.get(id).map(f))
        .flatten()
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
    //
    // gc-common w16-f: ONE scope roots the receiver and the URL for the whole
    // constructor. `toExternalForm()` is real Java and `create_string` below
    // allocates; the two pins this used to take were never released, and when
    // the URL did not answer "scheme://..." the synthetic arm below wrote every
    // slot -- and read the URL's field 0 -- through the addresses from BEFORE
    // that Java call.
    let url_arg = match args.get(1) {
        Some(Value::Object(Some(u))) => Some(*u),
        _ => None,
    };
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let url_h = url_arg.map(|u| scope.root(u));
    if let Some(url_h) = url_h.as_ref() {
        let url_obj = scope.get(url_h);
        let call = scope.invoke_virtual(url_obj, "toExternalForm", "()Ljava/lang/String;", &[]);
        let this = scope.get(&this_h);
        let url_obj = scope.get(url_h);
        let ctx = &mut *scope;
        if let Ok(Some(Value::Object(Some(s)))) = call {
            if let Some(full) = ctx.read_string(s) {
                if full.contains("://") {
                    ctx.set_field(this, HUC_CONN_ID, Value::Object(Some(url_obj)));
                    // A brand-new carrier must not inherit a DEAD one's request
                    // state. `real_reqs` is keyed by identity hash, which the
                    // VM derives per object and reuses once the first object is
                    // collected — so a fresh connection allocated where an old
                    // one died read back the old one's method, headers and
                    // streaming mode. MEASURED (`L6HttpLoopbackSweep`): a fresh
                    // connection's `setFixedLengthStreamingMode` threw
                    // `IllegalStateException: Chunked encoding streaming mode
                    // set` naming a mode nobody had set ON IT, and the next row
                    // threw the mirror-image message for the same reason.
                    //
                    // The constructor is the one moment we KNOW the carrier is
                    // new, so it is where the stale row has to go.
                    //
                    // The same argument applies to every other table keyed the
                    // same way: a stale `live_fixed_streams` row is read as
                    // "already connected" by both streaming setters, and a
                    // stale cached RESULT made `real_is_connected` answer true
                    // for a carrier that had sent nothing ("Already
                    // connected"). `real_forget` drops all of them, the result
                    // included (gc-common w11-a; the constructor used to skip
                    // `results` and the fixed-stream owner rows).
                    real_forget(ctx, this);
                    // And the handshake record (gc-common w16-f): a new
                    // carrier has not handshaked, whatever a dead one with the
                    // same identity hash left behind. Not in `real_forget`,
                    // which `disconnect()` also calls: there the RECYCLED row
                    // must survive (see `https_ensure_exchanged_body`). Keyed
                    // by the carrier's weak lock key since gc-common w27-b, so
                    // a new carrier has a row only if something filed one
                    // under its own key; the lookup mints nothing.
                    if let Some(key) = huc_existing_obj_key(&*ctx, this) {
                        forget_https_peer_row(key);
                    }
                    // This native REPLACES `HttpURLConnection(URL u)`, so
                    // everything that constructor's field initialisers would
                    // have written has to be written here — including the
                    // four `-1` sentinels. Without them a user subclass's
                    // `super(u)` produced a carrier whose
                    // `setFixedLengthStreamingMode` refused with "Chunked
                    // encoding streaming mode set". It runs AFTER the
                    // side-table eviction above and BEFORE the subclass's own
                    // initialisers, which is where the JDK puts it.
                    let url_keep = ctx.pin_native_root(url_obj);
                    let this = huc_write_declared_field_defaults(ctx, this);
                    let url_obj = ctx.read_native_pin(url_keep, url_obj);
                    ctx.unpin_native_roots(url_keep);
                    // `url` again: the helper does not touch it, and the
                    // write above happened before a `create_string` that can
                    // move either reference.
                    ctx.set_field(this, HUC_CONN_ID, Value::Object(Some(url_obj)));
                    return Ok(None);
                }
            }
        }
    }
    // The String first, then every write through the receiver's CURRENT
    // address (gc-common w16-f).
    let m = scope.create_string("GET");
    let this = scope.get(&this_h);
    let url_now = url_h.as_ref().map(|h| scope.get(h));
    let ctx = &mut *scope;
    // A new carrier has not handshaked (gc-common w16-f; see the real arm).
    if let Some(key) = huc_existing_obj_key(&*ctx, this) {
        forget_https_peer_row(key);
    }
    ctx.set_field(this, HUC_CONN_ID, Value::Int(-1));
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
    if let Some(url_obj) = url_now {
        let s_val = ctx.get_field(url_obj, 0);
        if let Value::Object(Some(_)) = s_val {
            ctx.set_field(this, HUC_URL_STR, s_val);
        }
    }
    Ok(None)
}

fn huc_connect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    // Real-JDK carrier: `connect()` only opens the socket on HotSpot and the
    // request is sent lazily by `getResponseCode`/`getInputStream`/the output
    // stream. We mirror that by deferring the actual `perform` — running it here
    // would (a) misread the synthetic slots `ensure_connected` consults and
    // (b) prematurely fix the request before the body/headers are fully staged.
    if is_real_carrier(ctx, this) {
        // Deferred, but not INVISIBLE. `URLConnection.connect()` sets
        // `connected = true`, and the inherited bytecode for
        // `setDoOutput`/`setDoInput`/`setUseCaches`/`setRequestProperty`
        // reads that field to refuse a late change — all of them retired onto
        // the JDK's own bodies on 2026-09-11, so the field is the only thing
        // they consult. Leaving it 0 made `connect(); setDoOutput(true)`
        // succeed (`L6HttpLoopbackSweep` row 73) on a connection that had
        // announced itself connected.
        ctx.set_field_by_name(this, "connected", Value::Int(1));
        return Ok(None);
    }
    ensure_connected(ctx, &mut this)
}

fn huc_get_response_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    // Real-JDK sun.net.www HttpURLConnection (field 0 is the real URL object):
    // perform from the real URL rather than misreading our synthetic HUC_* slots.
    if let Some(url_str) = huc_real_object_url(ctx, &mut this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            return Ok(Some(Value::Int(huc_real_perform(
                ctx, &mut this, &url_str,
            )?)));
        }
    }
    ensure_connected(ctx, &mut this)?;
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
    let mut this = obj_arg(args, 0)?;
    // Real-JDK carrier: derive from the cached perform result. Prefer the
    // reason phrase actually read off the wire (real servers often deviate
    // from the RFC's canonical phrase, e.g. OkHttp MockWebServer's default
    // "Server Error" for 500 vs. the RFC's "Internal Server Error" — real
    // HttpURLConnection.getResponseMessage() always returns exactly what the
    // server sent) — the hardcoded `status_reason` table is only a fallback
    // for when no reason was captured (e.g. the synthetic timeout result).
    if let Some(url_str) = huc_real_object_url(ctx, &mut this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            let status = huc_real_perform(ctx, &mut this, &url_str)?;
            let reason = real_result_of(&*ctx, this, |r| r.reason.clone())
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| status_reason(status));
            let s = ctx.create_string(&reason);
            return Ok(Some(Value::Object(Some(s))));
        }
    }
    ensure_connected(ctx, &mut this)?;
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
    let mut this = obj_arg(args, 0)?;

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
    if is_real_carrier(ctx, this) {
        // Recover the URL through its public external form first.  This works
        // for both real-JDK URLs and Craton's synthetic resource URLs, whereas
        // probing individual URL fields confuses a real `file:` URL's authority
        // or path with the complete URL.  `URLClassLoader.getResourceAsStream`
        // uses `openConnection().getInputStream()`, so treating a non-HTTP URL
        // as the HTTP carrier's empty response body makes inherited resources
        // appear as zero-byte streams (Hazelcast's filtered-loader XML config).
        let full = huc_real_object_url(ctx, &mut this);
        if let Some(full) = full.as_deref() {
            if full.starts_with("http://") || full.starts_with("https://") {
                let status = huc_real_perform(ctx, &mut this, full)?;
                // An error response has NO input stream: the JDK raises and
                // points the caller at `getErrorStream()` for the body. This
                // VM handed the error body back from `getInputStream()`, so
                // every `try { getInputStream() } catch (FileNotFoundException)`
                // — the standard way to test for a 404 — read the error page as
                // if it were the resource.
                if status >= 400 {
                    let class = if status == 404 || status == 410 {
                        "java/io/FileNotFoundException"
                    } else {
                        "java/io/IOException"
                    };
                    let message = if status == 404 || status == 410 {
                        full.to_string()
                    } else {
                        format!("Server returned HTTP response code: {status} for URL: {full}")
                    };
                    return Err(crate::phases_early::throw_jca_exc(ctx, class, &message));
                }
                let body = huc_real_body(ctx, this);
                let truncated = huc_real_truncated(ctx, this);
                return make_response_input_stream(ctx, &body, truncated, Some(this));
            }
        }
        // The URL is read out of the carrier only NOW (gc-common w16-f).
        // `huc_real_object_url` ran `toExternalForm()` -- Java -- and handed
        // `this` back at its current address; a URL copy taken before it named
        // the pre-move address. The old code re-read it on the `Some` arm but
        // peeked at, and dispatched `openStream` on, the stale copy when
        // `toExternalForm()` answered nothing. A carrier whose field no longer
        // holds an object falls through to the synthetic path below.
        if let Value::Object(Some(url_now)) = ctx.get_field(this, HUC_CONN_ID) {
            if full.is_some() {
                return ctx.invoke_virtual(url_now, "openStream", "()Ljava/io/InputStream;", &[]);
            }
            // Peek at the external form via the URL's full-URL string field
            // (field 5 in our URL synthetic), falling back to field 0.
            let url_str = match ctx.get_field(url_now, 5) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => match ctx.get_field(url_now, 0) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => String::new(),
                },
            };
            if !url_str.is_empty()
                && !url_str.starts_with("http://")
                && !url_str.starts_with("https://")
            {
                return ctx.invoke_virtual(url_now, "openStream", "()Ljava/io/InputStream;", &[]);
            }
        }
    }

    ensure_connected(ctx, &mut this)?;
    let (body_bytes, truncated) =
        with_state(ctx, this, |s| (s.response_body.clone(), s.truncated)).unwrap_or_default();
    // Mark consumed so a follow-up read doesn't double-pull.
    if let Value::Int(id) = ctx.get_field(this, HUC_CONN_ID) {
        conn_registry_mut(ctx.vm_identity(), |reg| {
            if let Some(state) = reg.get_mut(id) {
                state.body_consumed = true;
            }
        });
    }
    make_response_input_stream(ctx, &body_bytes, truncated, Some(this))
}

fn huc_get_error_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    // `getErrorStream()` NEVER connects. Its javadoc is explicit that it
    // answers null when the connection was not made, and callers rely on that
    // to ask "did this already fail?" without performing the request. This
    // VM's version performed it, so the question answered itself.
    if is_real_carrier(ctx, this) && !real_is_connected(ctx, this) {
        return Ok(Some(Value::Object(None)));
    }
    // Real-JDK carrier: serve the cached body when the response was an error.
    if let Some(url_str) = huc_real_object_url(ctx, &mut this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            let status = huc_real_perform(ctx, &mut this, &url_str)?;
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
    // Same four fields as before, through the one builder that pins the body
    // array across the stream allocation and throws OOME for a body that
    // cannot fit (gc-common w10-e).
    Ok(Some(make_byte_array_input_stream(ctx, &body_bytes)?))
}

fn huc_get_output_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
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
            // `ProtocolException` (an `IOException` subclass) with the JDK's own
            // wording, which names the fix. A bare `IOException` reads as a
            // transport failure and sends the caller looking at the network.
            return Err(protocol_ex(
                "cannot write to a URLConnection if doOutput=false - call setDoOutput(true)"
                    .to_string(),
            ));
        }
        // JDK semantics: opening the output stream promotes a still-default GET
        // to POST (see sun.net.www...HttpURLConnection.getOutputStream:
        // `if (method.equals("GET")) method = "POST"`). The harness's `postUrl`
        // relies on this — it sets only `setDoOutput(true)`, never the method —
        // so without this promotion the body is sent as a GET and the servlet
        // replies 405 Method Not Allowed.
        // The promotion has to land on the FIELD too, or `getRequestMethod()`
        // — the JDK's own retired bytecode on this carrier — keeps answering
        // GET for a request that goes out as a POST (`L6HttpLoopbackSweep`
        // row 67). See [`real_method`].
        // `this` is a bare local and every allocation below (the "POST"
        // string, the stream, its backing array) can move it; the fields
        // written and read after them are the carrier's (gc-common w10-e).
        // Pinned once, re-read after each allocating step.
        let this_pin = ctx.pin_native_root(this);
        if real_method(ctx, this).is_none_or(|m| m == "GET") {
            let post = ctx.create_string("POST");
            this = ctx.read_native_pin(this_pin, this);
            ctx.set_field_by_name(this, "method", Value::Object(Some(post)));
            with_real_req(ctx, this, |r| r.method = "POST".to_string());
        }
        // Minted: a body row is filed under it just below (`this` is current:
        // re-read after the last allocation above).
        let key = huc_obj_key(&*ctx, this);
        if let Some(existing) = real_body_stream_get(ctx, key) {
            ctx.unpin_native_roots(this_pin);
            return Ok(Some(Value::Object(Some(existing))));
        }
        let baos = match try_alloc_concurrent_synthetic(ctx, "java/io/ByteArrayOutputStream", 2) {
            Ok(baos) => baos,
            Err(e) => {
                ctx.unpin_native_roots(this_pin);
                return Err(e);
            }
        };
        // Family-1 fix (cce0079): `new_array` below can move the
        // still-unrooted `baos` — pin and refresh it before the field
        // stores and the registry insert.
        let baos_pin = ctx.pin_native_root(baos);
        let backing = ctx.new_array(ArrayElementType::Byte, 0);
        let baos = ctx.read_native_pin(baos_pin, baos);
        this = ctx.read_native_pin(this_pin, this);
        // Releases `baos_pin` too: it was pinned after `this_pin`.
        ctx.unpin_native_roots(this_pin);
        ctx.set_field(baos, 0, Value::Object(Some(backing)));
        ctx.set_field(baos, 1, Value::Int(0));
        real_body_stream_insert(ctx, key, baos);
        let streaming = real_streaming_mode(ctx, this);
        if let StreamingMode::Fixed(expected) = streaming {
            start_live_fixed_stream(ctx, this, baos, expected)?;
            // That ran Java (`toExternalForm`, the header merge), which can
            // move the stream; hand back its current address, which the row's
            // global root tracks.
            let baos = real_body_stream_get(ctx, key).unwrap_or(baos);
            return Ok(Some(Value::Object(Some(baos))));
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
    // Both allocations can move `this`, and the second can move `baos`: root
    // both and re-read them before the stores (gc-common w10-e; the real
    // carrier branch above already did this for `baos`). A scope since
    // gc-common w16-f, so the `?` below releases the root too.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let baos: ObjectRef =
        try_alloc_concurrent_synthetic(&mut *scope, "java/io/ByteArrayOutputStream", 2)?;
    let baos_h = scope.root(baos);
    let backing = scope.new_array(ArrayElementType::Byte, 0);
    let baos = scope.get(&baos_h);
    let this = scope.get(&this_h);
    scope.set_field(baos, 0, Value::Object(Some(backing)));
    scope.set_field(baos, 1, Value::Int(0));
    scope.set_field(this, HUC_REQ_BODY_STREAM, Value::Object(Some(baos)));
    Ok(Some(Value::Object(Some(baos))))
}

pub(crate) fn huc_get_header_field_named(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    // Real-JDK carrier: read from the cached perform result, not synthetic slots.
    // LAST duplicate wins, mirroring `sun.net.www.MessageHeader.findValue`'s
    // backwards iteration (see `content_length_of`'s doc for the MockWebServer
    // duplicate-Content-Length case that exposed this).
    if let Some(url_str) = huc_real_object_url(ctx, &mut this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            huc_real_perform(ctx, &mut this, &url_str)?;
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
        ensure_connected(ctx, &mut this)?;
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
    let mut this = obj_arg(args, 0)?;
    let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(-1);
    if idx < 0 {
        return Ok(Some(Value::Object(None)));
    }
    if let Some(url_str) = huc_real_object_url(ctx, &mut this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            huc_real_perform(ctx, &mut this, &url_str)?;
            let v = huc_real_indexed_headers(ctx, this)
                .get(idx as usize)
                .cloned();
            return Ok(Some(match v {
                Some((_k, val)) => Value::Object(Some(ctx.create_string(&val))),
                None => Value::Object(None),
            }));
        }
    }
    ensure_connected(ctx, &mut this)?;
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
    let mut this = obj_arg(args, 0)?;
    let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(-1);
    if idx < 0 {
        return Ok(Some(Value::Object(None)));
    }
    if let Some(url_str) = huc_real_object_url(ctx, &mut this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            huc_real_perform(ctx, &mut this, &url_str)?;
            let v = huc_real_indexed_headers(ctx, this)
                .get(idx as usize)
                .cloned();
            return Ok(Some(match v {
                // Index 0 is the status line, and its key is null — not the
                // empty string, which a caller comparing with `equals` would
                // read as a header named "".
                Some((Some(k), _)) => Value::Object(Some(ctx.create_string(&k))),
                _ => Value::Object(None),
            }));
        }
    }
    ensure_connected(ctx, &mut this)?;
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
    let mut this = obj_arg(args, 0)?;
    // URL lookup can enter real-JDK code and collect. Keep the carrier rooted
    // until the subsequent perform/header operations have consumed it.
    let this_pin = ctx.pin_native_root(this);
    let result = (|| -> MethodCallResult {
        let mut this = ctx.read_native_pin(this_pin, this);
        let headers = if let Some(url_str) = huc_real_object_url(ctx, &mut this) {
            let mut this = ctx.read_native_pin(this_pin, this);
            if url_str.starts_with("http://") || url_str.starts_with("https://") {
                huc_real_perform(ctx, &mut this, &url_str)?;
                let mut this = ctx.read_native_pin(this_pin, this);
                huc_real_headers(ctx, this)
            } else {
                ensure_connected(ctx, &mut this)?;
                let mut this = ctx.read_native_pin(this_pin, this);
                with_state(ctx, this, |s| s.response_headers.clone()).unwrap_or_default()
            }
        } else {
            ensure_connected(ctx, &mut this)?;
            let mut this = ctx.read_native_pin(this_pin, this);
            with_state(ctx, this, |s| s.response_headers.clone()).unwrap_or_default()
        };
        let this = ctx.read_native_pin(this_pin, this);
        let status_line = huc_real_indexed_headers(ctx, this)
            .first()
            .filter(|(k, _)| k.is_none())
            .map(|(_, v)| v.clone());
        let map = build_header_map(ctx, &headers, status_line.as_deref())?;
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
    let _ = body_len;
    headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .last()
        .and_then(|(_, v)| v.trim().parse::<i64>().ok())
        // NO header, no length: `getContentLength()` is `-1`, which is how a
        // caller learns the length is unknown and it must read to EOF. The
        // buffered body size used to stand in for it, so a chunked response
        // reported the length this VM happened to have received (5 for the
        // sweep's `abc`+`de`) and a `204 No Content` reported 0 where HotSpot
        // reports -1 — an answer indistinguishable from a real zero-length
        // entity.
        .unwrap_or(-1)
}

pub(crate) fn huc_get_content_length(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    if let Some(url_str) = huc_real_object_url(ctx, &mut this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            huc_real_perform(ctx, &mut this, &url_str)?;
            let n = content_length_of(&huc_real_headers(ctx, this), huc_real_body(ctx, this).len());
            return Ok(Some(Value::Int(n as i32)));
        }
    }
    if !matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        ensure_connected(ctx, &mut this)?;
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
    let mut this = obj_arg(args, 0)?;
    if let Some(url_str) = huc_real_object_url(ctx, &mut this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            huc_real_perform(ctx, &mut this, &url_str)?;
            let n = content_length_of(&huc_real_headers(ctx, this), huc_real_body(ctx, this).len());
            return Ok(Some(Value::Long(n)));
        }
    }
    if !matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        ensure_connected(ctx, &mut this)?;
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
            conn_registry_mut(ctx.vm_identity(), |reg| reg.remove(id));
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
    // The String is allocated once, before either arm writes, and the
    // receiver is read back after it (gc-common w16-f: both arms wrote -- and
    // the real arm keyed its side-table row by the identity hash read --
    // through the address from before the allocation).
    let real = is_real_carrier(ctx, this);
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let s = scope.create_string(&normalized);
    let this = scope.get(&this_h);
    if real {
        // BOTH copies. `real_method` reads the field first because on the
        // `java/net/HttpURLConnection` carrier only the JDK's own retired
        // bytecode writes it; on the `sun.*` carriers this native is the only
        // writer there is, and leaving the field behind would make the same
        // reader answer the stale default.
        scope.set_field_by_name(this, "method", Value::Object(Some(s)));
        with_real_req(&*scope, this, |r| r.method = normalized);
        return Ok(None);
    }
    scope.set_field(this, HUC_METHOD, Value::Object(Some(s)));
    Ok(None)
}

fn huc_get_request_method(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if is_real_carrier(ctx, this) {
        let m = real_method(ctx, this).unwrap_or_else(|| "GET".to_string());
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
    if is_real_carrier(ctx, this) && real_is_connected(ctx, this) {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "Already connected".into(),
        }
        .into());
    }
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
    huc_synthetic_store_header(ctx, this, &key, &value, true)
}

/// Store `key: value` in a SYNTHETIC carrier's `HUC_REQ_HEADERS` line array:
/// over the existing line for `key` when `replace` (`setRequestProperty`),
/// else in the first empty slot (`addRequestProperty`, and a `set` of a new
/// key).
///
/// gc-common w16-f: the one body both setters used to carry inline. The line
/// String and the lazily created array are allocations, and each arm then
/// wrote through the receiver from before them -- and stored a line held
/// across the array's allocation. The receiver and the line are rooted here
/// and read back after the last allocation.
fn huc_synthetic_store_header(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: &str,
    value: &str,
    replace: bool,
) -> MethodCallResult {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let line = scope.create_string(&format!("{key}: {value}"));
    let line_h = scope.root(line);
    let this = scope.get(&this_h);
    let arr = match scope.get_field(this, HUC_REQ_HEADERS) {
        Value::Object(Some(a)) => a,
        _ => {
            let a = scope.new_array(ArrayElementType::Reference, 32);
            let this = scope.get(&this_h);
            scope.set_field(this, HUC_REQ_HEADERS, Value::Object(Some(a)));
            a
        }
    };
    let line = scope.get(&line_h);
    let len = scope.array_length(arr);
    // Replace if the key already exists, else add to first empty slot.
    if replace {
        for i in 0..len {
            if let Value::Object(Some(s)) = scope.get_array_element(arr, i) {
                let existing = scope.read_string(s).unwrap_or_default();
                if let Some(colon) = existing.find(':') {
                    if existing[..colon].trim().eq_ignore_ascii_case(key) {
                        scope.set_array_element(arr, i, Value::Object(Some(line)));
                        return Ok(None);
                    }
                }
            }
        }
    }
    for i in 0..len {
        if matches!(scope.get_array_element(arr, i), Value::Object(None)) {
            scope.set_array_element(arr, i, Value::Object(Some(line)));
            return Ok(None);
        }
    }
    Ok(None)
}

fn huc_add_request_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if is_real_carrier(ctx, this) && real_is_connected(ctx, this) {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "Already connected".into(),
        }
        .into());
    }
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
    huc_synthetic_store_header(ctx, this, &key, &value, false)
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
    if is_real_carrier(ctx, this) && real_is_connected(ctx, this) {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "Already connected".into(),
        }
        .into());
    }
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
    let map = build_header_map(ctx, &headers, None)?;
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
    if is_real_carrier(ctx, this) && real_is_connected(ctx, this) {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "Already connected".into(),
        }
        .into());
    }
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
        with_real_req(&*ctx, this, |r| r.connect_timeout_ms = Some(v));
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
        with_real_req(&*ctx, this, |r| r.read_timeout_ms = Some(v));
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
    let long_overload = matches!(args.get(1), Some(Value::Long(_)));
    let length = match args.get(1) {
        Some(Value::Int(v)) => *v as i64,
        Some(Value::Long(v)) => *v,
        _ => return Err(iae("invalid content length")),
    };
    if !is_real_carrier(ctx, this) {
        // Synthetic carriers retain their historical buffered implementation.
        return Ok(None);
    }
    // The JDK's three refusals, in the JDK's ORDER and with the JDK's words.
    // Order is observable: `setFixedLengthStreamingMode(-1)` on a chunked
    // connection is "Chunked encoding streaming mode set", not "invalid
    // content length". The wording was observable too — HotSpot says
    // "invalid content length" where this said "setFixedLengthStreamingMode:
    // negative length" (`L6HttpLogicSweep` row 22).
    if huc_field_connected(ctx, this) {
        return Err(ise("Already connected"));
    }
    if matches!(real_streaming_mode(ctx, this), StreamingMode::Chunked(_)) {
        return Err(ise("Chunked encoding streaming mode set"));
    }
    if length < 0 {
        return Err(iae("invalid content length"));
    }
    // Both copies, for the reason [`real_method`] gives: the field is what
    // the JDK's own retired bytecode would have written, and it is what
    // `real_streaming_mode` reads first.
    if long_overload {
        ctx.set_field_by_name(this, "fixedContentLengthLong", Value::Long(length));
    } else {
        ctx.set_field_by_name(this, "fixedContentLength", Value::Int(length as i32));
    }
    with_real_req(ctx, this, |req| {
        req.streaming = StreamingMode::Fixed(length as u64)
    });
    Ok(None)
}

fn huc_set_chunked_streaming_mode(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let chunk_length = args.get(1).and_then(Value::as_int).unwrap_or(0);
    if !is_real_carrier(ctx, this) {
        return Ok(None);
    }
    if huc_field_connected(ctx, this) {
        return Err(ise("Already connected"));
    }
    if matches!(real_streaming_mode(ctx, this), StreamingMode::Fixed(_)) {
        return Err(ise("Fixed length streaming mode set"));
    }
    // `chunkLength = chunklen <= 0 ? DEFAULT_CHUNK_SIZE : chunklen` — the
    // JDK accepts a non-positive size and substitutes its own default rather
    // than refusing, which is why `setChunkedStreamingMode(-1)` and `(0)`
    // are both legal.
    let stored = if chunk_length <= 0 {
        HUC_DEFAULT_CHUNK_SIZE
    } else {
        chunk_length
    };
    ctx.set_field_by_name(this, "chunkLength", Value::Int(stored));
    with_real_req(ctx, this, |req| {
        req.streaming = StreamingMode::Chunked(stored)
    });
    Ok(None)
}

/// `sun.net.www.protocol.http.HttpURLConnection.DEFAULT_CHUNK_SIZE`.
const HUC_DEFAULT_CHUNK_SIZE: i32 = 4096;

/// The connection carrier classes this VM MINTS.
///
/// `URL.openConnection()` allocates one of the first three; the last two are
/// the real JDK classes an application can reach directly, and both are
/// registered here on purpose (`register_one`'s "some apps use the abstract
/// base class directly via reflection").
///
/// Anything else with these natives in its dispatch chain is a USER SUBCLASS,
/// and [`subclass_runs_its_own_bytecode`] is what keeps them out of it.
pub(crate) const VM_CONNECTION_CARRIERS: [&str; 6] = [
    "java/net/HttpURLConnection",
    "java/net/JarURLConnection",
    "java/net/URLConnection",
    "javax/net/ssl/HttpsURLConnection",
    "sun/net/www/protocol/http/HttpURLConnection",
    "sun/net/www/protocol/https/HttpsURLConnectionImpl",
];

/// `Some(result)` when the receiver is a user subclass and the call has been
/// handed to the JDK's own bytecode; `None` when this native should run.
///
/// **The defect.** `register_one(r, "java/net/HttpURLConnection")` exists so
/// applications that use the abstract base class through reflection work, and
/// dispatch probes the RECEIVER's class chain — so a test double that extends
/// `HttpURLConnection` and overrides `getHeaderField(String)` had its
/// `getContentLength()`, `getContentLengthLong()`, `getLastModified()`,
/// `setDoInput()` and `setUseCaches()` answered by natives that never
/// consulted the override. `L6HttpLogicSweep`'s `Fixture` is exactly that
/// shape and rows 87, 88, 101, 115 and 117 are exactly that: 87 and 88 opened
/// a socket to `fixture.invalid`, on an object that had already been given
/// every header it was going to be asked about.
///
/// **The discriminator is cheap.** This VM mints five carrier classes and a
/// subclass is none of them; the receiver's own class name settles it in one
/// lookup.
///
/// **The fallback has to be bytecode, not a hand-written body.**
/// `invoke_virtual_bytecode_only` resolves from the RECEIVER, so
/// `getContentLength()` reaches `java.net.URLConnection.getContentLength`,
/// which calls `getContentLengthLong()`, which this guard forwards again,
/// which calls `getHeaderFieldLong`, which calls `getHeaderField(String)` —
/// and THAT resolves to the subclass's override. Every step is the JDK's own
/// algorithm; nothing here re-implements `getHeaderFieldDate`'s date parsing
/// or the `content-length` fallbacks.
pub(crate) fn subclass_runs_its_own_bytecode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    name: &str,
    descriptor: &str,
) -> Option<MethodCallResult> {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return None;
    };
    let class_id = ctx.class_id_of_object(this);
    let cls = ctx.class_name_of_id(class_id)?;
    if VM_CONNECTION_CARRIERS.contains(&cls.as_str()) {
        return None;
    }
    // A JDK class is never "a user subclass", whatever its name. The
    // `java/net/URLConnection` registrations are inherited by every connection
    // impl in the image — `sun.net.www.protocol.file.FileURLConnection`,
    // `sun.net.www.protocol.jrt.JavaRuntimeURLConnection`, the jar ones — and
    // several of those carriers ARE this VM's own, minted elsewhere in
    // `net_phase_e`. Stepping aside for them would change behaviour this
    // wave has not measured. Loader 0/1 are Bootstrap and Extension.
    if ctx.loader_id_of_class(class_id) <= 1 {
        return None;
    }
    Some(ctx.invoke_virtual_bytecode_only(this, name, descriptor, &args[1..]))
}

/// Define one `sa_*` wrapper per triple, each guarding its body with
/// [`subclass_runs_its_own_bytecode`].
///
/// One generated `fn` per triple, because `NativeCallback` is a bare `fn`
/// pointer with no captures: the wrapper has to know its own name and
/// descriptor at runtime, and a closure that captured them could not be
/// registered.
///
/// **It generates the BODIES only, never the `r.register` calls**, and that is
/// not a style choice. `registrar_drift.rs` and `registrar_reachability.rs`
/// find registrations by SCANNING this source; they expand `for` loops and
/// they cannot expand a macro. Emitting the calls from here made 29 triples
/// vanish from both scanners, and `the_drift_baseline_has_no_stale_rows` went
/// red naming eleven `register_phase54_net_extras` pairs that still drift
/// exactly as much as they did before — the gate had simply been blinded to
/// the shipping half. `register_one` therefore keeps its 31 literal
/// `r.register(cls, …)` lines.
macro_rules! subclass_aware_bodies {
    ( $( ($fname:ident, $name:expr, $desc:expr, $inner:expr) ),+ $(,)? ) => {
        $(
            fn $fname(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                if let Some(forwarded) = subclass_runs_its_own_bytecode(ctx, args, $name, $desc) {
                    return forwarded;
                }
                let inner: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = $inner;
                inner(ctx, args)
            }
        )+
    };
}

/// Write the declared initial state of `java.net.URLConnection` and
/// `java.net.HttpURLConnection` onto a carrier that never ran their
/// constructors.
///
/// **Why a carrier needs this at all.** `URL.openConnection()` ALLOCATES its
/// carrier and returns it; no `<init>` runs. Every field therefore arrives
/// zeroed, and for four of them zero is a legal value that means the opposite
/// of "unset":
///
/// ```text
///   chunkLength            = -1   0 reads as "chunked mode is set"
///   fixedContentLength     = -1   0 reads as "fixed length 0 is set"
///   fixedContentLengthLong = -1   ditto
///   responseCode           = -1   0 reads as "HTTP 0"
/// ```
///
/// That went unnoticed while natives answered these methods. It became
/// visible the moment lane L6 retired thirteen of them onto the JDK's own
/// bodies (2026-09-11): the real `setChunkedStreamingMode` refuses when
/// `fixedContentLength != -1`, and the real `setFixedLengthStreamingMode`
/// refuses when `chunkLength != -1`, so BOTH refused, each naming the mode
/// the other had supposedly set, on connections where nobody had set either.
/// MEASURED — `L6HttpLoopbackSweep` rows 64, 65, 66 and `L6HttpLogicSweep`
/// row 155.
///
/// **By name, never by slot.** These are real JDK fields; this file's `HUC_*`
/// constants are a synthetic map that does not match their layout. See the
/// mint site in `net_phase_e`, whose comment tabulates what writing four of
/// them by index actually hit.
///
/// One list, two callers — the mint site and [`huc_init`], which is the
/// constructor a user subclass reaches through `super(u)`. A subclass carrier
/// has the same hole for the same reason.
pub(crate) fn huc_write_declared_field_defaults(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> ObjectRef {
    // `create_string` allocates, so it can move `this`; do it first, behind a
    // pin, and write everything afterwards.
    let pin = ctx.pin_native_root(this);
    let get = ctx.create_string("GET");
    let this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    ctx.set_field_by_name(this, "method", Value::Object(Some(get)));
    ctx.set_field_by_name(this, "doInput", Value::Int(1));
    ctx.set_field_by_name(this, "doOutput", Value::Int(0));
    ctx.set_field_by_name(this, "allowUserInteraction", Value::Int(0));
    // `URLConnection`'s initialiser is `useCaches = defaultUseCaches`, i.e.
    // true, and `HttpURLConnection`'s is `instanceFollowRedirects =
    // followRedirects`, also true. A carrier that answered `false` to either
    // was reporting a value the caller never chose.
    ctx.set_field_by_name(this, "useCaches", Value::Int(1));
    ctx.set_field_by_name(this, "instanceFollowRedirects", Value::Int(1));
    ctx.set_field_by_name(this, "ifModifiedSince", Value::Long(0));
    ctx.set_field_by_name(this, "connected", Value::Int(0));
    ctx.set_field_by_name(this, "chunkLength", Value::Int(-1));
    ctx.set_field_by_name(this, "fixedContentLength", Value::Int(-1));
    ctx.set_field_by_name(this, "fixedContentLengthLong", Value::Long(-1));
    ctx.set_field_by_name(this, "responseCode", Value::Int(-1));
    this
}

/// The carrier's OWN `connected` field — the one `URLConnection`'s inherited
/// bytecode reads and this file's response path writes.
///
/// The streaming setters used to ask `live_fixed_streams` instead, which is a
/// record of "a fixed-length body is mid-flight", not of "this connection is
/// connected": a plain buffered GET that had already fetched its response
/// answered false and let a caller set a streaming mode on it.
fn huc_field_connected(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    matches!(ctx.get_field_by_name(this, "connected"), Value::Int(v) if v != 0)
}

fn huc_set_instance_follow_redirects(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
    // The JDK's own `instanceFollowRedirects` field, BY NAME, on every
    // carrier — the same lesson as `connected`: `HttpURLConnection.
    // getInstanceFollowRedirects()` is one `getfield` of inherited bytecode
    // for any receiver this file does not serve, and a synthetic SLOT index
    // names a different field on a real class.
    ctx.set_field_by_name(this, "instanceFollowRedirects", Value::Int(v));
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
        // Key computed BEFORE the table lock (gc-common w11-a): nothing
        // VM-side runs under a `VmScoped` lock. A lookup: mints nothing.
        let follow = real_req_of(&*ctx, this, |req| req.follow_redirects).unwrap_or(true);
        return Ok(Some(Value::Int(if follow { 1 } else { 0 })));
    }
    // The JDK's field by NAME before the synthetic slot: `L6HttpLogicSweep`
    // row 154 read `false` from a freshly minted carrier where HotSpot reads
    // `true`, because slot 9 is `instanceFollowRedirects` only in the
    // SYNTHETIC layout, and the real field it aliases arrives zeroed on a
    // carrier that is ALLOCATED rather than constructed. The mint site now
    // writes that field (see `net_phase_e`'s `openConnection`), so a named
    // read is both correct here and what inherited bytecode already does.
    match ctx.get_field_by_name(this, "instanceFollowRedirects") {
        Value::Int(v) => Ok(Some(Value::Int(v))),
        _ => Ok(Some(ctx.get_field(this, HUC_INSTANCE_FOLLOW_REDIRECTS))),
    }
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

// One `sa_*` body per triple. The registrations stay literal in
// `register_one` below — see `subclass_aware_bodies`' note on the two source
// scanners that read them.
subclass_aware_bodies!(
    (sa_connect, "connect", "()V", huc_connect),
    (
        sa_get_response_code,
        "getResponseCode",
        "()I",
        huc_get_response_code
    ),
    (
        sa_get_response_message,
        "getResponseMessage",
        "()Ljava/lang/String;",
        huc_get_response_message
    ),
    (
        sa_get_input_stream,
        "getInputStream",
        "()Ljava/io/InputStream;",
        huc_get_input_stream
    ),
    (
        sa_get_error_stream,
        "getErrorStream",
        "()Ljava/io/InputStream;",
        huc_get_error_stream
    ),
    (
        sa_get_output_stream,
        "getOutputStream",
        "()Ljava/io/OutputStream;",
        huc_get_output_stream
    ),
    (
        sa_get_header_field_named,
        "getHeaderField",
        "(Ljava/lang/String;)Ljava/lang/String;",
        huc_get_header_field_named
    ),
    (
        sa_get_header_field_indexed,
        "getHeaderField",
        "(I)Ljava/lang/String;",
        huc_get_header_field_indexed
    ),
    (
        sa_get_header_field_key,
        "getHeaderFieldKey",
        "(I)Ljava/lang/String;",
        huc_get_header_field_key_indexed
    ),
    (
        sa_get_header_fields,
        "getHeaderFields",
        "()Ljava/util/Map;",
        huc_get_header_fields
    ),
    (
        sa_get_content_length,
        "getContentLength",
        "()I",
        huc_get_content_length
    ),
    (
        sa_get_content_length_long,
        "getContentLengthLong",
        "()J",
        huc_get_content_length_long
    ),
    (sa_disconnect, "disconnect", "()V", huc_disconnect),
    (
        sa_set_request_method,
        "setRequestMethod",
        "(Ljava/lang/String;)V",
        huc_set_request_method
    ),
    (
        sa_get_request_method,
        "getRequestMethod",
        "()Ljava/lang/String;",
        huc_get_request_method
    ),
    (
        sa_set_request_property,
        "setRequestProperty",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        huc_set_request_property
    ),
    (
        sa_add_request_property,
        "addRequestProperty",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        huc_add_request_property
    ),
    (
        sa_get_request_property,
        "getRequestProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        huc_get_request_property
    ),
    (sa_set_do_input, "setDoInput", "(Z)V", huc_set_do_input),
    (sa_set_do_output, "setDoOutput", "(Z)V", huc_set_do_output),
    (
        sa_set_connect_timeout,
        "setConnectTimeout",
        "(I)V",
        huc_set_connect_timeout
    ),
    (
        sa_set_read_timeout,
        "setReadTimeout",
        "(I)V",
        huc_set_read_timeout
    ),
    (
        sa_set_fixed_length_i,
        "setFixedLengthStreamingMode",
        "(I)V",
        huc_set_fixed_length_streaming_mode
    ),
    (
        sa_set_fixed_length_j,
        "setFixedLengthStreamingMode",
        "(J)V",
        huc_set_fixed_length_streaming_mode
    ),
    (
        sa_set_chunked,
        "setChunkedStreamingMode",
        "(I)V",
        huc_set_chunked_streaming_mode
    ),
    (
        sa_set_instance_follow_redirects,
        "setInstanceFollowRedirects",
        "(Z)V",
        huc_set_instance_follow_redirects
    ),
    (
        sa_get_instance_follow_redirects,
        "getInstanceFollowRedirects",
        "()Z",
        huc_get_instance_follow_redirects
    ),
    (sa_using_proxy, "usingProxy", "()Z", huc_using_proxy),
    (
        sa_get_request_properties,
        "getRequestProperties",
        "()Ljava/util/Map;",
        huc_get_request_properties
    ),
);

fn register_one(r: &mut NativeMethodRegistry, cls: &str) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // `<init>` is the one pair that is NOT subclass-guarded, and deliberately.
    // A constructor is never dispatched virtually: a subclass reaches this
    // through `super(u)`, and forwarding by RECEIVER would resolve back to the
    // subclass's own `<init>` and recurse. `huc_init` handles that caller
    // explicitly instead — it detects a real `java.net.URL` argument, keeps it
    // in the `url` field, and writes the field defaults the constructor it
    // replaces would have written.
    r.register(cls, "<init>", "(Ljava/net/URL;)V", huc_init);
    r.register(cls, "<init>", "()V", huc_init);
    // Everything below runs for a carrier this VM minted and steps aside for a
    // user subclass — see `subclass_runs_its_own_bytecode`. The guard is
    // uniform on purpose: wrapping only the five triples a probe happened to
    // catch would leave the same defect in the other twenty-four.
    //
    // The streaming setters now carry the JDK's own refusals, in the JDK's
    // order, and write the JDK's own fields. They used to be documented here
    // as unavoidable no-ops because "our synthetically constructed carrier
    // never runs URLConnection's field initializers, so those fields are 0
    // (not -1)". The premise was right and the conclusion was avoidable:
    // `huc_write_declared_field_defaults` writes the -1s.
    r.register(cls, "connect", "()V", sa_connect);
    r.register(cls, "getResponseCode", "()I", sa_get_response_code);
    r.register(
        cls,
        "getResponseMessage",
        "()Ljava/lang/String;",
        sa_get_response_message,
    );
    r.register(
        cls,
        "getInputStream",
        "()Ljava/io/InputStream;",
        sa_get_input_stream,
    );
    r.register(
        cls,
        "getErrorStream",
        "()Ljava/io/InputStream;",
        sa_get_error_stream,
    );
    r.register(
        cls,
        "getOutputStream",
        "()Ljava/io/OutputStream;",
        sa_get_output_stream,
    );
    r.register(
        cls,
        "getHeaderField",
        "(Ljava/lang/String;)Ljava/lang/String;",
        sa_get_header_field_named,
    );
    r.register(
        cls,
        "getHeaderField",
        "(I)Ljava/lang/String;",
        sa_get_header_field_indexed,
    );
    r.register(
        cls,
        "getHeaderFieldKey",
        "(I)Ljava/lang/String;",
        sa_get_header_field_key,
    );
    r.register(
        cls,
        "getHeaderFields",
        "()Ljava/util/Map;",
        sa_get_header_fields,
    );
    r.register(cls, "getContentLength", "()I", sa_get_content_length);
    r.register(
        cls,
        "getContentLengthLong",
        "()J",
        sa_get_content_length_long,
    );
    r.register(cls, "disconnect", "()V", sa_disconnect);
    r.register(
        cls,
        "setRequestMethod",
        "(Ljava/lang/String;)V",
        sa_set_request_method,
    );
    r.register(
        cls,
        "getRequestMethod",
        "()Ljava/lang/String;",
        sa_get_request_method,
    );
    r.register(
        cls,
        "setRequestProperty",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        sa_set_request_property,
    );
    r.register(
        cls,
        "addRequestProperty",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        sa_add_request_property,
    );
    r.register(
        cls,
        "getRequestProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        sa_get_request_property,
    );
    r.register(cls, "setDoInput", "(Z)V", sa_set_do_input);
    r.register(cls, "setDoOutput", "(Z)V", sa_set_do_output);
    r.register(cls, "setConnectTimeout", "(I)V", sa_set_connect_timeout);
    r.register(cls, "setReadTimeout", "(I)V", sa_set_read_timeout);
    r.register(
        cls,
        "setFixedLengthStreamingMode",
        "(I)V",
        sa_set_fixed_length_i,
    );
    r.register(
        cls,
        "setFixedLengthStreamingMode",
        "(J)V",
        sa_set_fixed_length_j,
    );
    r.register(cls, "setChunkedStreamingMode", "(I)V", sa_set_chunked);
    r.register(
        cls,
        "setInstanceFollowRedirects",
        "(Z)V",
        sa_set_instance_follow_redirects,
    );
    r.register(
        cls,
        "getInstanceFollowRedirects",
        "()Z",
        sa_get_instance_follow_redirects,
    );
    r.register(cls, "usingProxy", "()Z", sa_using_proxy);
    r.register(
        cls,
        "getRequestProperties",
        "()Ljava/util/Map;",
        sa_get_request_properties,
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
    r.register(cls, "equals", "(Ljava/lang/Object;)Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        // `Object.equals` is reference equality (`this == obj`). This compared
        // identity hashes, so two distinct live carriers that share one (a
        // 32-bit hash from a counter that wraps) were `equals` -- and a
        // `HashSet` / `HashMap` of connections merged them (gc-common w27-b).
        // Both are native arguments, so both addresses are current.
        let same = match args.get(1) {
            Some(Value::Object(Some(other))) => other.as_ptr() == this.as_ptr(),
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
        let chain = vec![vec![0x30u8, 0x01, 0x02]];

        record_https_peer_info(&ctx, Some(carrier), &chain, "TLS_AES_256_GCM_SHA384");
        note_response_stream(&ctx, stream, Some(carrier));
        // The rows are filed under the objects' weak lock keys (gc-common w27-b).
        let key = huc_existing_obj_key(&ctx, carrier).expect("the handshake was filed");
        let stream_key = huc_existing_obj_key(&ctx, stream).expect("the stream was noted");
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
            "at body EOF the CONNECTION-level view is torn down — every accessor throws \
             IllegalStateException: connection not yet open again"
        );
        assert!(
            https_peer_info().lock().unwrap().contains_key(&key),
            "the ROW must survive: https_ensure_exchanged reads a missing entry as \
             \"never handshaked\" and would re-issue the request over the network"
        );

        // Eof fires on EVERY exhausted read and a closed stream produces Close
        // as well, so the second and third events must find nothing to do.
        assert!(
            https_response_streams()
                .lock()
                .unwrap()
                .get(&stream_key)
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

        record_https_peer_info(
            &ctx,
            Some(carrier),
            &[vec![0x30u8]],
            "TLS_AES_128_GCM_SHA256",
        );
        let key = huc_existing_obj_key(&ctx, carrier).expect("the handshake was filed");
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
        let chain = vec![vec![0x30u8, 0x01, 0x02]];

        record_https_peer_info(&ctx, Some(carrier), &chain, "TLS_AES_256_GCM_SHA384");
        let key = huc_existing_obj_key(&ctx, carrier).expect("the handshake was filed");
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
        // NO header → -1, and the buffered body size is not a substitute for
        // it. This line asserted `5` — the body size — until 2026-09-11, when
        // `L6HttpLoopbackSweep` measured HotSpot 25.0.4+7 against a loopback
        // server: a chunked 200 answers -1 (rows 59) and a `204 No Content`
        // answers -1 (row 50), where this VM answered the bytes it happened to
        // have buffered and 0. Zero is the answer that matters: it is
        // indistinguishable from a real zero-length entity, and -1 is how the
        // JDK says "unknown, read to EOF".
        assert_eq!(content_length_of(&[], 5), -1);
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
        let id1 = reg.allocate(
            1,
            ConnState {
                status: 200,
                response_body: b"a".to_vec(),
                response_headers: vec![],
                body_consumed: false,
                truncated: false,
            },
        );
        let id2 = reg.allocate(
            2,
            ConnState {
                status: 404,
                response_body: b"b".to_vec(),
                response_headers: vec![],
                body_consumed: false,
                truncated: false,
            },
        );
        assert_ne!(id1, id2);
        assert_eq!(reg.get(id1).unwrap().status, 200);
        assert_eq!(reg.get(id2).unwrap().status, 404);
    }

    #[test]
    fn test_registry_remove_clears_state() {
        let mut reg = ConnRegistry::new();
        let id = reg.allocate(
            7,
            ConnState {
                status: 200,
                response_body: vec![],
                response_headers: vec![],
                body_consumed: false,
                truncated: false,
            },
        );
        assert!(reg.get(id).is_some());
        reg.remove(id);
        assert!(reg.get(id).is_none());
        assert!(reg.by_owner.is_empty(), "removal unlinks the owner index");
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
        let id = CONN_REGISTRY.with(ctx.vm_identity(), |reg| {
            reg.allocate(
                0,
                ConnState {
                    status: 404,
                    response_body: vec![],
                    response_headers: vec![],
                    body_consumed: false,
                    truncated: false,
                },
            )
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

/// gc-common w10-e (`common-w8a-process-global-object-singletons-in-native-
/// builtins`, the `real_body_streams` row): a real carrier's buffered request
/// body belongs to its own VM, is held by a releasable global root, and the
/// table is bounded without ever dropping an unsent body.
#[cfg(test)]
mod w10e_real_body_stream_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    const VM_A: usize = 0xE10B_0001;
    const VM_B: usize = 0xE10B_0002;
    const VM_C: usize = 0xE10B_0003;

    struct Teardown(&'static [usize]);
    impl Drop for Teardown {
        fn drop(&mut self) {
            for &vm in self.0 {
                forget_vm_http_url_connection_state(vm);
            }
        }
    }

    fn rows(vm: usize) -> usize {
        REAL_BODY_STREAMS.peek(vm, |t| t.rows.len()).unwrap_or(0)
    }

    #[test]
    fn body_streams_are_per_vm_and_release_their_root() {
        let _teardown = Teardown(&[VM_A, VM_B]);
        let mut a = crate::test_utils::mock_ctx();
        a.set_vm_identity(VM_A);
        let mut b = crate::test_utils::mock_ctx();
        b.set_vm_identity(VM_B);
        let baos_a = a.fresh_object_ref();

        // One carrier key in both VMs' tables.
        let key: usize = 0x0E10_B100;
        real_body_stream_insert(&mut a, key, baos_a);
        assert_eq!(real_body_stream_get(&a, key), Some(baos_a));
        assert_eq!(real_body_stream_get(&b, key), None, "VM B must not see A's body");

        // B minting a carrier with that key no longer evicts A's row.
        real_body_stream_forget(&mut b, Some(key));
        assert_eq!(real_body_stream_get(&a, key), Some(baos_a));
        assert_eq!(rows(VM_B), 0, "a forget creates no row");

        // A's own forget drops the row and releases its global root.
        let root = REAL_BODY_STREAMS
            .peek(VM_A, |t| t.rows.get(&key).map(|r| r.root))
            .flatten()
            .unwrap();
        real_body_stream_forget(&mut a, Some(key));
        assert_eq!(real_body_stream_get(&a, key), None);
        if root != 0 {
            assert_eq!(a.resolve_global_root(root), None, "root released");
        }
    }

    #[test]
    fn the_bound_evicts_only_sent_bodies() {
        let _teardown = Teardown(&[VM_C]);
        let mut c = crate::test_utils::mock_ctx();
        c.set_vm_identity(VM_C);
        let baos = c.fresh_object_ref();
        for key in 0..REAL_BODY_STREAMS_PER_VM {
            real_body_stream_insert(&mut c, key, baos);
        }
        // Nothing sent yet: the table grows past the bound rather than lose
        // a body that still has to go out.
        let over = REAL_BODY_STREAMS_PER_VM;
        real_body_stream_insert(&mut c, over, baos);
        assert_eq!(rows(VM_C), REAL_BODY_STREAMS_PER_VM + 1);
        // Once row 5 has been sent it is the one that goes.
        REAL_BODY_STREAMS.with(VM_C, |t| t.rows.get_mut(&5).unwrap().sent = true);
        real_body_stream_insert(&mut c, over + 1, baos);
        assert_eq!(rows(VM_C), REAL_BODY_STREAMS_PER_VM + 1);
        assert_eq!(real_body_stream_get(&c, 5), None);
        assert_eq!(real_body_stream_get(&c, 4), Some(baos));
    }
}

/// gc-common w11-a (`common-w10e-huc-real-carrier-tables-are-keyed-by-bare-
/// identity-hash`): a real carrier's request, cached response and live upload
/// rows belong to the VM that wrote them, so another VM's mint of a carrier
/// with the same identity hash leaves them alone.
#[cfg(test)]
mod w11a_real_carrier_vm_isolation_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    const VM_A: usize = 0xA11B_0001;
    const VM_B: usize = 0xA11B_0002;
    // `a_live_upload_belongs_to_its_vm` owns its own pair: the tests run in
    // parallel, and each `Teardown` forgets its VMs wholesale, so a shared
    // identity let one test wipe the other's rows mid-run.
    const VM_C: usize = 0xA11B_0003;
    const VM_D: usize = 0xA11B_0004;

    fn at(addr: usize) -> ObjectRef {
        // SAFETY: an identity-hash source only; the mock hashes the address
        // and never dereferences it.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// Clears the test's VMs even when an assertion fails (the rows, then the
    /// lock keys they are filed under since gc-common w27-b).
    struct Teardown(&'static [usize]);
    impl Drop for Teardown {
        fn drop(&mut self) {
            for &vm in self.0 {
                forget_vm_http_url_connection_state(vm);
                crate::forget_vm_lock_keys(vm);
            }
        }
    }

    #[test]
    fn a_mint_in_one_vm_leaves_another_vms_carrier_alone() {
        let _teardown = Teardown(&[VM_A, VM_B]);
        let mut a = crate::test_utils::mock_ctx();
        a.set_vm_identity(VM_A);
        let mut b = crate::test_utils::mock_ctx();
        b.set_vm_identity(VM_B);

        // One identity hash in both VMs (the mock's hash is the address
        // truncated to `i32`).
        let (conn_a, conn_b) = (at(0x1_A11B_1000), at(0x2_A11B_1000));
        assert_eq!(a.identity_hash_code(conn_a), b.identity_hash_code(conn_b));

        with_real_req(&a, conn_a, |req| {
            req.method = "PUT".to_string();
            req.read_timeout_ms = Some(1234);
        });
        // The carrier's weak lock key (gc-common w27-b), minted just above.
        let key = huc_existing_obj_key(&a, conn_a).expect("filed");
        real_results_with(VM_A, |t| {
            t.insert(
                key,
                RealResult {
                    status: 201,
                    headers: vec![("X-Vm".to_string(), "a".to_string())],
                    body: b"from-a".to_vec(),
                    reason: String::new(),
                    truncated: false,
                },
            );
        });

        // VM B reads none of it...
        assert!(!real_is_connected(&b, conn_b), "B must not see A's response");
        assert!(huc_real_body(&b, conn_b).is_empty());
        assert!(huc_real_headers(&b, conn_b).is_empty());

        // ...and B's `URL.openConnection()` mint of a carrier with that hash
        // (`real_forget`) no longer deletes A's in-flight request.
        real_forget(&mut b, conn_b);
        let method = real_reqs_peek(VM_A, |t| t.get(&key).map(|r| r.method.clone())).flatten();
        assert_eq!(method.as_deref(), Some("PUT"));
        assert!(real_is_connected(&a, conn_a));
        assert_eq!(huc_real_body(&a, conn_a), b"from-a".to_vec());
        assert!(!REAL_CARRIERS.has_row(VM_B), "a forget creates no row");

        // A's own mint does drop them.
        real_forget(&mut a, conn_a);
        assert!(!real_is_connected(&a, conn_a));
        assert_eq!(real_reqs_peek(VM_A, |t| t.contains_key(&key)), Some(false));
    }

    /// A live upload is found, taken and forgotten only through its own VM.
    #[test]
    fn a_live_upload_belongs_to_its_vm() {
        let _teardown = Teardown(&[VM_C, VM_D]);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let tcp = TcpStream::connect(listener.local_addr().unwrap()).expect("connect loopback");
        let key = 0x0A11_B200;
        let baos_key = 0x0A11_B201;
        REAL_CARRIERS.with(VM_C, |t| {
            t.fixed_streams.insert(
                key,
                Arc::new(Mutex::new(LiveFixedStream {
                    tcp,
                    expected: 4,
                    written: 0,
                    closed: false,
                })),
            );
            t.fixed_owners.insert(baos_key, key);
        });
        // Another VM's forget or take of the same key is a no-op for A.
        forget_live_fixed_stream(VM_D, key);
        assert!(take_live_fixed_stream(VM_D, key).is_none());
        let cell = take_live_fixed_stream(VM_C, key).expect("A's upload is still there");
        assert_eq!(lock_live_fixed_stream(&cell).expected, 4);
        assert_eq!(
            REAL_CARRIERS.peek(VM_C, |t| t.fixed_owners.contains_key(&baos_key)),
            Some(false),
            "taking the stream drops its owner row"
        );
    }
}

/// gc-common w12-a (`common-w11a-huc-real-results-hold-every-response-body-
/// for-the-life-of-a-vm`): a real carrier's cached response goes when the
/// carrier dies, without `disconnect()`.
#[cfg(test)]
mod w12a_real_carrier_sweep_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    const VM_A: usize = 0xA12C_0001;
    const VM_B: usize = 0xA12C_0002;

    fn at(addr: usize) -> ObjectRef {
        // SAFETY: an identity-hash source only; the mock hashes the address
        // and never dereferences it.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// Clears the test's VMs even when an assertion fails (the rows, then the
    /// lock keys they are filed under since gc-common w27-b).
    struct Teardown(&'static [usize]);
    impl Drop for Teardown {
        fn drop(&mut self) {
            for &vm in self.0 {
                forget_vm_http_url_connection_state(vm);
                crate::forget_vm_lock_keys(vm);
            }
        }
    }

    fn performed(ctx: &dyn NativeContext, conn: ObjectRef) {
        // What `huc_real_perform_inner` files: under the carrier's weak lock
        // key (gc-common w27-b; a `HucCarrier` weak owner before).
        let key = huc_obj_key(ctx, conn);
        let vm = ctx.vm_identity();
        real_reqs_with(vm, |t| {
            t.entry(key).or_default().method = "GET".to_string();
        });
        real_results_with(vm, |t| {
            t.insert(
                key,
                RealResult {
                    status: 200,
                    headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
                    body: vec![0x5A; 4096],
                    reason: String::new(),
                    truncated: false,
                },
            );
        });
    }

    fn result_rows(vm: usize) -> usize {
        real_results_peek(vm, |t| t.len()).unwrap_or(0)
    }

    /// N requests read without `disconnect()`: once the carriers die, the
    /// per-VM `results` map keeps only the live one's row. Another VM's
    /// collection judges none of them.
    #[test]
    fn dead_carriers_leave_no_response_body_behind() {
        let _teardown = Teardown(&[VM_A, VM_B]);
        let a = crate::test_utils::mock_ctx();
        a.set_vm_identity(VM_A);
        let live = at(0xA12C_1000);
        for i in 0..200usize {
            performed(&a, at(0xA12C_1000 + i * 16));
        }
        assert_eq!(result_rows(VM_A), 200);
        let dead = at(0xA12C_1000 + 16);
        let dead_key = huc_existing_obj_key(&a, dead).expect("filed");

        // The lock-key sweep of a VM that keyed nothing frees nothing.
        assert_eq!(crate::gc_sweep_lock_keys(VM_B, &|_| false), 0);
        assert_eq!(result_rows(VM_A), 200);

        crate::gc_sweep_lock_keys(VM_A, &|x| x == live.as_ptr() as usize);
        assert_eq!(result_rows(VM_A), 1);
        assert!(
            real_is_connected(&a, live),
            "the live carrier keeps its response"
        );
        assert_eq!(huc_real_body(&a, live).len(), 4096);
        assert!(!real_is_connected(&a, dead));
        assert_eq!(
            real_reqs_peek(VM_A, |t| t.contains_key(&dead_key)),
            Some(false)
        );
    }
}

/// gc-common w16-f: the stale-reference fixes in this file, where the mock can
/// express the move. `MockNativeContext` has no allocation hook, so only moves
/// inside a Java call (`invoke_virtual`) are simulated: the hook remaps the
/// native pins -- which is where the mock keeps handle-scope roots too -- from
/// the old address to a second object that plays the moved copy.
#[cfg(test)]
mod w16f_huc_stale_reference_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };
    use std::cell::Cell;

    thread_local! {
        /// (old, new) address pairs the `toExternalForm` hook moves.
        static W16F_MOVES: Cell<[(usize, usize); 2]> = const { Cell::new([(0, 0); 2]) };
        /// The receiver `openStream` was dispatched on.
        static W16F_OPEN_STREAM_RECEIVER: Cell<usize> = const { Cell::new(0) };
    }

    fn addr(o: ObjectRef) -> usize {
        o.as_ptr() as usize
    }

    /// `toExternalForm` moves both registered objects and answers null (not a
    /// "scheme://..." URL); `openStream` records its receiver.
    fn w16f_move_hook(
        ctx: &mut MockNativeContext,
        receiver: ObjectRef,
        method_name: &str,
        _descriptor: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        match method_name {
            "toExternalForm" => {
                for (old, new) in W16F_MOVES.with(|m| m.get()) {
                    if old != 0 {
                        ctx.remap_native_pin_addr_for_test(old, new);
                    }
                }
                Some(Ok(None))
            }
            "openStream" => {
                W16F_OPEN_STREAM_RECEIVER.with(|r| r.set(addr(receiver)));
                Some(Ok(Some(Value::Object(None))))
            }
            _ => None,
        }
    }

    /// `huc_init` with a URL whose `toExternalForm()` is not "scheme://...":
    /// the synthetic arm used to write every slot -- and read the URL's
    /// field 0 -- through the addresses from before that Java call.
    #[test]
    fn w16f_huc_init_synthetic_arm_writes_the_moved_receiver() {
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(0xF16_0101);
        let this_old = ctx.fresh_object_ref();
        let this_new = ctx.fresh_object_ref();
        let url_old = ctx.fresh_object_ref();
        let url_new = ctx.fresh_object_ref();
        let spec_old = ctx.create_string("old-spec");
        let spec_new = ctx.create_string("new-spec");
        ctx.set_field(url_old, 0, Value::Object(Some(spec_old)));
        ctx.set_field(url_new, 0, Value::Object(Some(spec_new)));
        W16F_MOVES.with(|m| {
            m.set([
                (addr(this_old), addr(this_new)),
                (addr(url_old), addr(url_new)),
            ])
        });
        ctx.set_invoke_virtual_hook(w16f_move_hook);

        let out = huc_init(
            &mut ctx,
            &[Value::Object(Some(this_old)), Value::Object(Some(url_old))],
        );
        W16F_MOVES.with(|m| m.set([(0, 0); 2]));

        assert!(matches!(out, Ok(None)));
        assert!(matches!(ctx.get_field(this_new, HUC_CONN_ID), Value::Int(-1)));
        assert!(matches!(
            ctx.get_field(this_new, HUC_METHOD),
            Value::Object(Some(_))
        ));
        assert!(matches!(
            ctx.get_field(this_new, HUC_INSTANCE_FOLLOW_REDIRECTS),
            Value::Int(1)
        ));
        assert!(
            matches!(ctx.get_field(this_new, HUC_URL_STR), Value::Object(Some(s)) if s == spec_new),
            "the URL string must be read from the URL's current address"
        );
        assert!(
            matches!(ctx.get_field(this_old, HUC_METHOD), Value::Int(0)),
            "nothing may be written through the vacated address"
        );
        assert_eq!(ctx.native_pin_count_for_test(), 0, "every root must be released");
    }

    /// `getInputStream` on a real carrier whose `toExternalForm()` answers
    /// nothing: the fallback peeked at, and dispatched `openStream` on, the URL
    /// copy read before that Java call.
    #[test]
    fn w16f_get_input_stream_fallback_opens_the_moved_url() {
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(0xF16_0102);
        let this_old = ctx.fresh_object_ref();
        let this_new = ctx.fresh_object_ref();
        let url_old = ctx.fresh_object_ref();
        let url_new = ctx.fresh_object_ref();
        // The collector copied the carrier: each copy names "its" URL.
        ctx.set_field(this_old, HUC_CONN_ID, Value::Object(Some(url_old)));
        ctx.set_field(this_new, HUC_CONN_ID, Value::Object(Some(url_new)));
        let spec = ctx.create_string("file:/tmp/w16f.xml");
        ctx.set_field(url_new, 5, Value::Object(Some(spec)));
        W16F_MOVES.with(|m| m.set([(addr(this_old), addr(this_new)), (0, 0)]));
        W16F_OPEN_STREAM_RECEIVER.with(|r| r.set(0));
        ctx.set_invoke_virtual_hook(w16f_move_hook);

        let out = huc_get_input_stream(&mut ctx, &[Value::Object(Some(this_old))]);
        W16F_MOVES.with(|m| m.set([(0, 0); 2]));

        assert!(
            out.is_ok(),
            "the file: URL must be opened, not sent as HTTP"
        );
        assert_eq!(
            W16F_OPEN_STREAM_RECEIVER.with(|r| r.get()),
            addr(url_new),
            "openStream must be dispatched on the URL the moved carrier holds"
        );
        assert_eq!(ctx.native_pin_count_for_test(), 0);
    }

    fn header_lines(ctx: &MockNativeContext, this: ObjectRef) -> Vec<String> {
        let Value::Object(Some(arr)) = ctx.get_field(this, HUC_REQ_HEADERS) else {
            return Vec::new();
        };
        (0..ctx.array_length(arr))
            .filter_map(|i| match ctx.get_array_element(arr, i) {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => None,
            })
            .collect()
    }

    fn w16f_header_call(
        ctx: &mut MockNativeContext,
        this: ObjectRef,
        add: bool,
        k: &str,
        v: &str,
    ) {
        let k = ctx.create_string(k);
        let v = ctx.create_string(v);
        let args = [
            Value::Object(Some(this)),
            Value::Object(Some(k)),
            Value::Object(Some(v)),
        ];
        let out = if add {
            huc_add_request_property(ctx, &args)
        } else {
            huc_set_request_property(ctx, &args)
        };
        assert!(matches!(out, Ok(None)));
    }

    /// The handshake table is per VM, and a dead carrier's row goes with its
    /// other real-carrier rows. VM B's carrier is given VM A's carrier's
    /// address (two mocks do not mint the same addresses, and the mock's
    /// identity hash is the address), so the two carriers have equal identity
    /// hashes -- the collision the bare-hash key could not tell apart.
    #[test]
    fn w16f_https_peer_rows_are_per_vm_and_die_with_the_carrier() {
        const VM_A: usize = 0xF16_0301;
        const VM_B: usize = 0xF16_0302;
        struct Teardown;
        impl Drop for Teardown {
            fn drop(&mut self) {
                for vm in [VM_A, VM_B] {
                    forget_vm_http_url_connection_state(vm);
                    crate::forget_vm_lock_keys(vm);
                }
            }
        }
        let _teardown = Teardown;
        let mut a = MockNativeContext::new();
        a.set_vm_identity(VM_A);
        let mut b = MockNativeContext::new();
        b.set_vm_identity(VM_B);
        let ca = a.fresh_object_ref();
        // Never dereferenced by `b`: the rows key on the identity hash and
        // the VM (through the weak lock key since gc-common w27-b).
        let cb = ca;
        assert_eq!(
            a.identity_hash_code(ca),
            b.identity_hash_code(cb),
            "precondition: the two carriers collide on identity hash"
        );
        let has = |key: Option<usize>| {
            key.is_some_and(|k| https_peer_info().lock().unwrap().contains_key(&k))
        };
        let chain = [vec![0x30u8, 0x01, 0x02]];

        record_https_peer_info(&a, Some(ca), &chain, "TLS_AES_128_GCM_SHA256");
        let key_a = huc_existing_obj_key(&a, ca);
        assert!(has(key_a));
        assert!(
            !has(huc_existing_obj_key(&b, cb)),
            "VM B's carrier must not find VM A's handshake"
        );

        // A dies: the lock-key sweep frees its key and drops its rows.
        crate::gc_sweep_lock_keys(VM_A, &|_| false);
        assert!(!has(key_a), "a dead carrier's handshake row goes");
        assert!(huc_existing_obj_key(&a, ca).is_none());

        // Teardown drops only the torn-down VM's rows.
        record_https_peer_info(&a, Some(ca), &chain, "TLS_AES_128_GCM_SHA256");
        record_https_peer_info(&b, Some(cb), &chain, "TLS_AES_128_GCM_SHA256");
        let (key_a, key_b) = (huc_existing_obj_key(&a, ca), huc_existing_obj_key(&b, cb));
        assert_ne!(key_a, key_b);
        forget_vm_http_url_connection_state(VM_A);
        assert!(!has(key_a));
        assert!(has(key_b), "another VM's row survives");
        forget_vm_http_url_connection_state(VM_B);
        assert!(!has(key_b));
    }

    /// The shared `huc_synthetic_store_header` keeps both setters' semantics:
    /// `set` replaces a key's line, `add` appends another, and neither leaves
    /// a root behind.
    #[test]
    fn w16f_synthetic_header_setters_replace_and_append() {
        let mut ctx = MockNativeContext::new();
        ctx.set_vm_identity(0xF16_0103);
        let this = ctx.fresh_object_ref();
        ctx.set_field(this, HUC_CONN_ID, Value::Int(-1));
        // The mock's `new_array(Reference, n)` fills with `Int(0)`, not null
        // (production fills with null), so the line array is seeded with a
        // null-filled one rather than left to the helper's lazy allocation.
        let lines = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 32);
        ctx.set_field(this, HUC_REQ_HEADERS, Value::Object(Some(lines)));
        w16f_header_call(&mut ctx, this, false, "X-A", "1");
        w16f_header_call(&mut ctx, this, false, "x-a", "2");
        assert_eq!(header_lines(&ctx, this), vec!["x-a: 2".to_string()]);
        w16f_header_call(&mut ctx, this, true, "X-A", "3");
        assert_eq!(
            header_lines(&ctx, this),
            vec!["x-a: 2".to_string(), "X-A: 3".to_string()]
        );
        assert_eq!(ctx.native_pin_count_for_test(), 0);
    }
}

/// gc-common w17-e (`common-w16f-synthetic-huc-connection-registry-holds-
/// response-bodies`): a SYNTHETIC carrier's connection row -- its whole
/// response body -- goes when the carrier dies, without `disconnect()`, and
/// with its VM at teardown. The synthetic twin of
/// `w12a_real_carrier_sweep_tests`.
#[cfg(test)]
mod w17e_synthetic_conn_sweep_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    const VM_A: usize = 0xE17_0001;
    const VM_B: usize = 0xE17_0002;
    const VM_C: usize = 0xE17_0003;
    const VM_D: usize = 0xE17_0004;

    fn at(addr: usize) -> ObjectRef {
        // SAFETY: an identity-hash and weak-owner source only; the mock hashes
        // the address and nothing here dereferences it. 16-byte steps keep it
        // 8-byte aligned (`ObjectRef::from_raw` debug-asserts that).
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// Clears the test's VMs even when an assertion fails (the rows, then the
    /// lock keys they are filed under since gc-common w27-b).
    struct Teardown(&'static [usize]);
    impl Drop for Teardown {
        fn drop(&mut self) {
            for &vm in self.0 {
                forget_vm_http_url_connection_state(vm);
                crate::forget_vm_lock_keys(vm);
            }
        }
    }

    fn exchange(body_len: usize) -> ConnState {
        ConnState {
            status: 200,
            response_body: vec![0x5A; body_len],
            response_headers: vec![("Content-Length".to_string(), body_len.to_string())],
            body_consumed: false,
            truncated: false,
        }
    }

    fn rows(vm: usize) -> usize {
        CONN_REGISTRY.peek(vm, |r| r.conns.len()).unwrap_or(0)
    }

    fn body_len(vm: usize, id: i32) -> Option<usize> {
        CONN_REGISTRY
            .peek(vm, |r| r.get(id).map(|s| s.response_body.len()))
            .flatten()
    }

    /// N synthetic exchanges read without `disconnect()`: once the carriers
    /// die, the VM's registry keeps only the live one's row. Another VM's
    /// collection judges none of them.
    #[test]
    fn w17e_dead_synthetic_carriers_leave_no_response_body_behind() {
        let _teardown = Teardown(&[VM_A]);
        let a = crate::test_utils::mock_ctx();
        a.set_vm_identity(VM_A);
        let live = at(0xE17_1000);
        let ids: Vec<i32> = (0..100usize)
            .map(|i| register_synthetic_conn(&a, at(0xE17_1000 + i * 16), exchange(4096)))
            .collect();
        assert_eq!(rows(VM_A), 100);

        // A VM that owns no rows here: its collection judges none of A's.
        assert_eq!(crate::gc_sweep_lock_keys(0xE17_00FF, &|_| false), 0);
        assert_eq!(rows(VM_A), 100, "another VM's collection judges none of them");

        // gc-common w27-b: the lock-key sweep frees the dead carriers' keys
        // (it counts slots) and drops their rows.
        let dropped = crate::gc_sweep_lock_keys(VM_A, &|x| x == live.as_ptr() as usize);
        assert!(dropped >= 99, "every dead carrier's key is freed (got {dropped})");
        assert_eq!(rows(VM_A), 1);
        assert_eq!(body_len(VM_A, ids[0]), Some(4096), "the live carrier keeps its body");
        for id in &ids[1..] {
            assert_eq!(body_len(VM_A, *id), None);
        }
        assert_eq!(
            CONN_REGISTRY.peek(VM_A, |r| r.by_owner.len()),
            Some(1),
            "the owner index shrinks with the rows"
        );
    }

    /// A row `disconnect()` already dropped leaves nothing behind when its
    /// carrier dies; one carrier's two exchanges go together with it; and
    /// (gc-common w27-b) two live carriers that share an identity hash keep
    /// SEPARATE rows, each going with its own carrier.
    #[test]
    fn w17e_disconnected_and_colliding_carriers() {
        let _teardown = Teardown(&[VM_C]);
        let c = crate::test_utils::mock_ctx();
        c.set_vm_identity(VM_C);
        let gone = at(0xE17_2000);
        let id = register_synthetic_conn(&c, gone, exchange(16));
        conn_registry_mut(VM_C, |r| r.remove(id));
        assert_eq!(rows(VM_C), 0);
        crate::gc_sweep_lock_keys(VM_C, &|_| false);
        assert_eq!(rows(VM_C), 0);
        assert_eq!(CONN_REGISTRY.peek(VM_C, |r| r.by_owner.len()), Some(0));

        // One carrier that connected twice: both rows under its key, and
        // both go with it.
        let twice = at(0xE17_3000);
        let first = register_synthetic_conn(&c, twice, exchange(16));
        let second = register_synthetic_conn(&c, twice, exchange(32));
        assert_ne!(first, second);
        crate::gc_sweep_lock_keys(VM_C, &|_| true);
        assert_eq!(rows(VM_C), 2);
        crate::gc_sweep_lock_keys(VM_C, &|_| false);
        assert_eq!(rows(VM_C), 0);

        // Two carriers 4 GiB apart share the mock's identity hash: separate
        // keys, and the dead one's row goes while the live one's stays.
        let (c1, c2) = (at(0x1_0E17_4000), at(0x2_0E17_4000));
        assert_eq!(
            c.identity_hash_code(c1),
            c.identity_hash_code(c2),
            "premise: the two carriers share an identity hash"
        );
        let id1 = register_synthetic_conn(&c, c1, exchange(16));
        let id2 = register_synthetic_conn(&c, c2, exchange(32));
        let live = c2.as_ptr() as usize;
        crate::gc_sweep_lock_keys(VM_C, &|x| x == live);
        assert_eq!(body_len(VM_C, id1), None, "the dead carrier's row went");
        assert_eq!(body_len(VM_C, id2), Some(32), "the live collider keeps its row");
    }

    /// A VM's connection rows are invisible to another VM's carrier holding
    /// the same id, and teardown drops only the torn-down VM's rows.
    #[test]
    fn w17e_rows_are_per_vm_and_dropped_at_teardown() {
        let _teardown = Teardown(&[VM_B, VM_D]);
        let mut b = MockNativeContext::new();
        b.set_vm_identity(VM_B);
        let mut d = MockNativeContext::new();
        d.set_vm_identity(VM_D);
        let cb = b.fresh_object_ref();
        let cd = d.fresh_object_ref();
        let id_b = register_synthetic_conn(&b, cb, exchange(8));
        b.set_field(cb, HUC_CONN_ID, Value::Int(id_b));
        // VM D's carrier names the same id without ever having filed it.
        d.set_field(cd, HUC_CONN_ID, Value::Int(id_b));
        assert_eq!(with_state(&b, cb, |s| s.response_body.len()), Some(8));
        assert_eq!(
            with_state(&d, cd, |s| s.response_body.len()),
            None,
            "another VM's row must not answer this VM's carrier"
        );
        let id_d = register_synthetic_conn(&d, cd, exchange(24));
        d.set_field(cd, HUC_CONN_ID, Value::Int(id_d));
        assert_eq!(with_state(&d, cd, |s| s.response_body.len()), Some(24));

        forget_vm_http_url_connection_state(VM_B);
        assert_eq!(rows(VM_B), 0);
        assert!(
            !CONN_REGISTRY.has_row(VM_B),
            "teardown drops the VM's whole registry"
        );
        assert_eq!(with_state(&b, cb, |s| s.response_body.len()), None);
        assert_eq!(
            with_state(&d, cd, |s| s.response_body.len()),
            Some(24),
            "another VM's rows survive the teardown"
        );
    }
}

/// gc-common w27-b (`common-w26b-tls-sibling-tables-keyed-by-vm-folded-identity-hash`):
/// every per-object table of this file is keyed by the object's weak lock
/// key, so two LIVE carriers of one VM that share an identity hash keep
/// separate rows, a freed key drops every row of its object (the handshake
/// record, the response-stream association, the synthetic connection, the
/// cached result and request, the request-body stream), and two VMs stay
/// apart.
///
/// The mock's identity hash is the address truncated to `i32`, so addresses
/// 4 GiB apart share it. The helpers under test touch only the identity hash
/// and the VM identity, so bare addresses stand in for carriers. Private VM
/// identities; the guard forgets only those.
#[cfg(test)]
mod w27b_huc_obj_key_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    const VM_A: usize = 0xB27B_0C01;
    const VM_B: usize = 0xB27B_0C02;
    const VM_C: usize = 0xB27B_0C03;
    const VM_D: usize = 0xB27B_0C04;
    const VM_E: usize = 0xB27B_0C05;

    fn at(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; the mock hashes the address.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn ctx_in(vm: usize) -> MockNativeContext {
        let c = crate::test_utils::mock_ctx();
        c.set_vm_identity(vm);
        c
    }

    /// Forgets this test's VMs only, even when an assertion fails: the rows
    /// first (as `forget_vm_native_root_stores` orders it), then the keys.
    struct Teardown(&'static [usize]);
    impl Drop for Teardown {
        fn drop(&mut self) {
            for &vm in self.0 {
                forget_vm_http_url_connection_state(vm);
                crate::forget_vm_lock_keys(vm);
            }
        }
    }

    fn result(status: i32) -> RealResult {
        RealResult {
            status,
            headers: Vec::new(),
            body: vec![0x42; 64],
            reason: String::new(),
            truncated: false,
        }
    }

    fn method_of(ctx: &MockNativeContext, carrier: ObjectRef) -> Option<String> {
        real_req_of(ctx, carrier, |r| r.method.clone())
    }

    fn has_peer_row(ctx: &MockNativeContext, carrier: ObjectRef) -> bool {
        huc_existing_obj_key(ctx, carrier)
            .is_some_and(|k| https_peer_info().lock().unwrap().contains_key(&k))
    }

    /// Two live real carriers of one VM with one identity hash: separate
    /// request, result and handshake rows, and a mint-time forget of one
    /// (`real_forget`) leaves the other's in-flight request alone.
    #[test]
    fn two_live_same_hash_carriers_keep_separate_rows() {
        let _teardown = Teardown(&[VM_A]);
        let mut a = ctx_in(VM_A);
        let (c1, c2) = (at(0x1_B27B_C000), at(0x2_B27B_C000));
        assert_eq!(
            a.identity_hash_code(c1),
            a.identity_hash_code(c2),
            "premise: the two carriers share an identity hash"
        );
        with_real_req(&a, c1, |r| r.method = "PUT".to_string());
        assert_eq!(method_of(&a, c2), None, "a live collider does not send the first one's method");
        with_real_req(&a, c2, |r| r.method = "DELETE".to_string());
        assert_eq!(method_of(&a, c1).as_deref(), Some("PUT"));
        assert_eq!(method_of(&a, c2).as_deref(), Some("DELETE"));

        let k1 = huc_existing_obj_key(&a, c1).expect("filed");
        real_results_with(VM_A, |t| {
            t.insert(k1, result(200));
        });
        assert!(real_is_connected(&a, c1));
        assert!(!real_is_connected(&a, c2), "the collider has not performed");

        record_https_peer_info(&a, Some(c1), &[vec![0x30u8]], "TLS_AES_128_GCM_SHA256");
        assert!(has_peer_row(&a, c1));
        assert!(
            !has_peer_row(&a, c2),
            "the collider has no handshake: `https_ensure_exchanged` runs its own"
        );

        real_forget(&mut a, c2);
        assert_eq!(method_of(&a, c2), None);
        assert_eq!(method_of(&a, c1).as_deref(), Some("PUT"), "the live collider's request stays");
        assert!(real_is_connected(&a, c1));
    }

    /// The lock-key sweep frees a dead carrier's key: every row filed under it
    /// goes (`lib.rs::sweep_lock_keys` -> `forget_huc_obj_keys`), a live
    /// stream's association with the dead carrier goes too, and the dead
    /// carrier's request-body root is parked, then released by the VM's next
    /// body-stream call. A live carrier's rows stay.
    #[test]
    fn a_freed_key_drops_every_row_of_its_carrier() {
        let _teardown = Teardown(&[VM_B]);
        let mut b = ctx_in(VM_B);
        let (dead, live, stream) = (at(0xB27B_D000), at(0xB27B_D100), at(0xB27B_D200));
        with_real_req(&b, dead, |r| r.do_output = true);
        with_real_req(&b, live, |r| r.do_output = true);
        let k_dead = huc_existing_obj_key(&b, dead).expect("filed");
        let k_live = huc_existing_obj_key(&b, live).expect("filed");
        real_results_with(VM_B, |t| {
            t.insert(k_dead, result(200));
        });
        record_https_peer_info(&b, Some(dead), &[vec![0x30u8]], "TLS_AES_128_GCM_SHA256");
        note_response_stream(&b, stream, Some(dead));
        let k_stream = huc_existing_obj_key(&b, stream).expect("noted");
        let conn = register_synthetic_conn(
            &b,
            dead,
            ConnState {
                status: 200,
                response_body: vec![0x5A; 128],
                response_headers: Vec::new(),
                body_consumed: false,
                truncated: false,
            },
        );
        let baos = b.fresh_object_ref();
        real_body_stream_insert(&mut b, k_dead, baos);
        let root = REAL_BODY_STREAMS
            .peek(VM_B, |t| t.rows.get(&k_dead).map(|r| r.root))
            .flatten()
            .expect("the body row");

        let (live_addr, stream_addr) = (live.as_ptr() as usize, stream.as_ptr() as usize);
        crate::gc_sweep_lock_keys(VM_B, &|x| x == live_addr || x == stream_addr);

        assert!(huc_existing_obj_key(&b, dead).is_none(), "the key was freed");
        assert_eq!(real_results_peek(VM_B, |t| t.contains_key(&k_dead)), Some(false));
        assert_eq!(real_reqs_peek(VM_B, |t| t.contains_key(&k_dead)), Some(false));
        assert_eq!(real_reqs_peek(VM_B, |t| t.contains_key(&k_live)), Some(true));
        assert!(!https_peer_info().lock().unwrap().contains_key(&k_dead));
        assert!(
            !https_response_streams().lock().unwrap().contains_key(&k_stream),
            "a live stream's association with a dead carrier goes"
        );
        assert_eq!(CONN_REGISTRY.peek(VM_B, |r| r.get(conn).is_some()), Some(false));
        assert_eq!(
            REAL_BODY_STREAMS.peek(VM_B, |t| t.rows.contains_key(&k_dead)),
            Some(false)
        );
        if root != 0 {
            assert_eq!(
                REAL_BODY_STREAMS.peek(VM_B, |t| t.orphaned_roots.contains(&root)),
                Some(true),
                "the dead carrier's body root waits for a context"
            );
        }
        real_body_stream_forget(&mut b, None);
        assert_eq!(
            REAL_BODY_STREAMS.peek(VM_B, |t| t.orphaned_roots.is_empty()),
            Some(true)
        );
        if root != 0 {
            assert_eq!(b.resolve_global_root(root), None, "released");
        }
    }

    /// The per-read observers mint no key for a stream they know nothing
    /// about, and the BAOS observer answers at once with no live upload.
    #[test]
    fn the_stream_observers_mint_nothing() {
        let _teardown = Teardown(&[VM_C]);
        let mut c = ctx_in(VM_C);
        let (baos, bais) = (at(0xB27B_E000), at(0xB27B_E100));
        assert!(matches!(
            huc_live_baos_event(&mut c, baos, BaosEvent::Flush),
            Ok(false)
        ));
        assert!(huc_existing_obj_key(&c, baos).is_none());
        assert!(huc_live_bais_event(
            &mut c,
            bais,
            cratonvm_native_api::registry::BaisEvent::Eof
        )
        .is_ok());
        assert!(huc_existing_obj_key(&c, bais).is_none());
    }

    /// One address in two VMs never shares a key; one VM's lock-key teardown
    /// drops exactly its own rows.
    #[test]
    fn two_vms_stay_separate() {
        let _teardown = Teardown(&[VM_D, VM_E]);
        let (d, e) = (ctx_in(VM_D), ctx_in(VM_E));
        let obj = at(0x1_B27B_F000);
        with_real_req(&d, obj, |r| r.method = "D".to_string());
        with_real_req(&e, obj, |r| r.method = "E".to_string());
        let (kd, ke) = (
            huc_existing_obj_key(&d, obj).expect("filed"),
            huc_existing_obj_key(&e, obj).expect("filed"),
        );
        assert_ne!(kd, ke);
        assert_eq!(method_of(&d, obj).as_deref(), Some("D"));
        assert_eq!(method_of(&e, obj).as_deref(), Some("E"));

        crate::forget_vm_lock_keys(VM_D);
        assert_eq!(real_reqs_peek(VM_D, |t| t.contains_key(&kd)), Some(false));
        assert_eq!(method_of(&e, obj).as_deref(), Some("E"));
    }
}
