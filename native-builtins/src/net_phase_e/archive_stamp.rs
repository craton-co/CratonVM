#[allow(unused_imports)]
use super::*;


// ---------------------------------------------------------------------------
// Bug 2 fix: JAR cache to make Spring Boot fat-jar autoconfig walk fast.
//
// Spring's `ClassLoader.getResources("META-INF/spring.factories")` opens
// every nested JAR in the fat JAR and reads META-INF/spring.factories.
// Without caching, each open re-reads the full outer fat JAR (often 50-100 MB)
// from disk *and* re-parses its zip directory — O(N²) behavior that takes
// 120s+ to startup.  With caching, the outer fat JAR is read once and
// inner-JAR bytes are extracted on demand.
// ---------------------------------------------------------------------------

/// Identity stamp for an on-disk archive: (last-modified, length).
///
/// **Why every jar/war byte cache in the VM is keyed on this and not on the
/// path alone.** The caches below are keyed by absolute path and were
/// originally never invalidated, on the assumption that a classpath jar is
/// immutable for the VM's lifetime. That assumption is false for an
/// application *server*: Tomcat's auto-deployer replaces
/// `<appBase>/<app>.war` in place and redeploys, and its own test suite does
/// exactly that — `HostConfigAutomaticDeploymentBaseTest.createWar()` writes a
/// DIFFERENT war to the SAME `<appBase>/myapp.war` for each `@Test` method in
/// the class. Every method after the first therefore saw the FIRST method's
/// archive: `TestHostConfigAutomaticDeploymentUnpackWAR.testUnpackWARTTF`
/// read `unpackWAR="false"` out of a war whose `META-INF/context.xml` says
/// `"true"`, so the webapp was never expanded and the test failed — while
/// passing in isolation, and passing on HotSpot, which has no such cache.
///
/// A `metadata()` call per lookup is orders of magnitude cheaper than the
/// multi-MB re-read + zip re-parse these caches exist to avoid, so correctness
/// here costs effectively nothing.
pub fn archive_stamp(path: &str) -> (u64, u64) {
    match std::fs::metadata(path) {
        Ok(m) => {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            (mtime, m.len())
        }
        // Unreadable: stamp (0,0). A later successful stat produces a
        // different stamp, so the entry is refreshed rather than pinned.
        Err(_) => (0, 0),
    }
}

/// Cache of outer JAR (path, [`archive_stamp`]) -> raw bytes (kept alive for
/// the process lifetime). Spring Boot fat JARs are at most ~150 MB; caching
/// one is cheap relative to the disk re-reads it saves.
pub fn outer_jar_bytes_cache() -> &'static Mutex<HashMap<(String, u64, u64), Arc<Vec<u8>>>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, u64, u64), Arc<Vec<u8>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Cache of nested JAR entry (outer_jar + "!" + inner_entry, outer's
/// [`archive_stamp`]) -> raw bytes.
pub fn nested_jar_bytes_cache() -> &'static Mutex<HashMap<(String, u64, u64), Arc<Vec<u8>>>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, u64, u64), Arc<Vec<u8>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn cached_outer_jar(path: &str) -> std::io::Result<Arc<Vec<u8>>> {
    let (mtime, len) = archive_stamp(path);
    let key = (path.to_string(), mtime, len);
    {
        let cache = outer_jar_bytes_cache().lock();
        if let Some(b) = cache.get(&key) {
            return Ok(b.clone());
        }
    }
    let bytes = std::fs::read(path)?;
    let arc = Arc::new(bytes);
    let mut cache = outer_jar_bytes_cache().lock();
    // Drop any stale generation of the same path so a long-lived server that
    // redeploys repeatedly does not accumulate every past version's bytes.
    cache.retain(|(p, _, _), _| p != path);
    cache.insert(key, arc.clone());
    Ok(arc)
}

pub fn cached_nested_jar(outer: &str, inner_entry: &str) -> std::io::Result<Arc<Vec<u8>>> {
    let (mtime, len) = archive_stamp(outer);
    let key = (format!("{outer}!{inner_entry}"), mtime, len);
    {
        let cache = nested_jar_bytes_cache().lock();
        if let Some(b) = cache.get(&key) {
            return Ok(b.clone());
        }
    }
    let outer_bytes = cached_outer_jar(outer)?;
    let cursor = std::io::Cursor::new(outer_bytes.as_slice());
    let mut zip = zip::ZipArchive::new(cursor)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let mut entry = zip
        .by_name(inner_entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e.to_string()))?;
    let mut buf = Vec::with_capacity(entry.size().min(1 << 27) as usize);
    entry
        .read_to_end(&mut buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    let arc = Arc::new(buf);
    let mut cache = nested_jar_bytes_cache().lock();
    let key_name = key.0.clone();
    cache.retain(|(k, _, _), _| *k != key_name);
    cache.insert(key, arc.clone());
    Ok(arc)
}

/// Returns true if the Spring fat-jar debug prints (`URLRES-DBG`, `OSTR-DBG`,
/// `CCE-DBG`) should be emitted. Off by default — these printlns themselves
/// dominate startup time for Spring Boot fat JARs (hundreds of lines per
/// second).  Enable by setting `CRATONVM_SPRING_DBG=1`.
#[inline]
pub fn spring_dbg_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| crate::nbflags().spring_dbg)
}


// ---------------------------------------------------------------------------
// Synthetic field layouts used by Phase E.
// ---------------------------------------------------------------------------

pub const IA_HOST: usize = 0;
pub const IA_ADDR: usize = 1;

/// The side table's host slot for an `InetAddress` that carries NO hostName —
/// the JDK's null `InetAddressHolder.hostName`.
///
/// An empty string rather than an `Option` because the table is
/// `(String, String)` and several readers already treat an empty host as
/// "nothing stored" (`inet_addr_field_string_or` substitutes its default,
/// `p52_isa_host_from_addr` falls through to the IP). `populate_inet_holder`
/// translates it back to a genuine `null` in the real-JDK `holder`, so
/// un-overridden `InetAddress.toString()` bytecode renders `/ip` exactly as
/// HotSpot does.
pub const NO_HOST_NAME: &str = "";

pub const ISA_HOST: usize = 0;
pub const ISA_PORT: usize = 1;

/// The list `SSLSocket.getSupportedCipherSuites()` returns for our synthetic
/// client socket (`ssl_sock_supported_cipher_suites` in phases_late.rs — kept
/// in sync manually since the two functions live in different files serving
/// different halves of the same `javax/net/ssl/SSLSocket` class). Used by
/// `setEnabledCipherSuites` below to distinguish a real cipher restriction
/// from a caller just re-asserting "use everything you support".
pub const CLIENT_SUPPORTED_CIPHER_SUITES: &[&str] = &[
    "TLS_AES_128_GCM_SHA256",
    "TLS_AES_256_GCM_SHA384",
    "TLS_CHACHA20_POLY1305_SHA256",
    "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
    "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
    "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
    // T-CBC.1: real CBC-mode suites, see t27_tls_cbc /
    // rustls-cbc-cipher-suites-not-supported.md
    "TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA256",
    "TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256",
    "TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA384",
    "TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA384",
];

pub const SOCK_HOST: usize = 0;
pub const SOCK_PORT: usize = 1;
pub const SOCK_LOCAL_PORT: usize = 2;
pub const SOCK_CLOSED: usize = 3;
pub const SOCK_STREAM_ID: usize = 4;

pub const SS_PORT: usize = 0;
pub const SS_BACKLOG: usize = 1;
pub const SS_CLOSED: usize = 2;
pub const SS_LISTENER_ID: usize = 3;

// ---------------------------------------------------------------------------
// W3-A2 side-tables — bypass the synthetic-vs-real-JDK field-layout
// collision by storing Socket / ServerSocket state in process-wide HashMaps
// keyed by ObjectRef. Synthetic field slots collide with real-JDK private
// fields. Side-tables are independent of layout.
//
// The example this comment used to give was wrong in a way that matters:
// real `ServerSocket` slot 0 is NOT `boolean created`, it is `impl`, the
// reference-typed `SocketImpl`. MEASURED 2026-08-17, `javap -p
// java.net.ServerSocket` on JDK 25, declaration order:
//
//     0 impl (SocketImpl)   1 created (boolean)   2 bound (boolean)
//     3 closed (boolean)    4 socketLock (Object) 5 options (Set)
//
// A `Value::Int` written into slot 0 is therefore DROPPED by the field guard
// rather than merely misfiled, `impl` stays null, and every inherited method
// whose bytecode reads `getImpl()` throws NPE. That is not hypothetical: it
// is how `RSslLiveSession` died on `ServerSocket.setSoTimeout` in the
// `javax.net.ssl.SSLServerSocket` twin of this surface. See
// `docs/known-issues/jdk-only/` record `G16-1` (2026-08-17).
// ---------------------------------------------------------------------------

#[derive(Default, Debug, Clone)]
pub struct SockSide {
    pub host_id: i32, // unused, kept for layout stability
    // Remote host string. Side-tabled (not written to the object's real
    // field slot 0) because real JDK 25 java.net.Socket's field slot 0
    // is `impl` (a SocketImpl) -- writing a host String there via
    // ctx.set_field(_, SOCK_HOST, ...) corrupts `impl`, so any later
    // unregistered Socket method that falls through to real bytecode
    // (e.g. getImpl(), called internally by many Socket accessors)
    // invokes methods on a String receiver instead of a SocketImpl,
    // producing a NoSuchMethodError that names String for a method that
    // plainly does not exist on it (e.g. create(Z)V). See
    // bug-h2-nosuchmethoderror-cross-class-dispatch-FIXED.md.
    pub host: String,
    pub port: i32,
    pub local_port: i32,
    pub closed: i32,
    pub stream_id: i32,
    /// Java's SO_TIMEOUT setting. Unlike the TCP stream id, this is meaningful
    /// before connect and must survive the stream installation transition.
    pub read_timeout_ms: i32,
    pub enabled_protocols: Option<Vec<String>>,
    pub tcp_no_delay: bool,
    pub keep_alive: bool,
    pub input_shutdown: i32,
    pub output_shutdown: i32,
    /// `(level, optname, value)` triples set through `Socket.setOption` while
    /// this Socket had no OS descriptor yet, in call order.
    ///
    /// A `new Socket()` has no fd in THIS surface until `connect` allocates its
    /// `TcpStream`, but it does on HotSpot: `Socket.getImpl()` runs
    /// `createImpl(true)`, so `setOption` before `connect` is legal and the
    /// value is still in effect on the connected socket afterwards. Apache
    /// HttpClient 5's `DefaultHttpClientConnectionOperator.configureSocket`
    /// does exactly that for the `TCP_KEEPIDLE`/`TCP_KEEPINTERVAL`/
    /// `TCP_KEEPCOUNT` family — every request through Spring's
    /// `RestClient` — so refusing it failed all 160 tests of
    /// `RequestMappingMessageConversionIntegrationTests` with
    /// *"Socket.setOption: socket is not connected"*. Same retained-setting
    /// treatment as `read_timeout_ms` above, applied by
    /// [`apply_pending_socket_options`] once the stream exists.
    pub pending_options: Vec<(i32, i32, i32)>,
    /// The address of the object this row belongs to, as of its last write
    /// through [`sock_set`] (`0` = never written with the object in hand, so
    /// never judged), or [`SOCK_OWNER_GONE`] once the object died while one of
    /// its socket streams still lived. [`gc_sweep_net_socket_rows`] follows it
    /// through a collection's pointer map and drops the row once the object
    /// (and every stream of it) is dead (gc-common w16-a,
    /// `common-w15a-dropped-unclosed-socket-keeps-its-fd-and-rows`).
    pub owner: usize,
    /// The plain `servlet::s2_registry` stream at `stream_id` was opened for
    /// THIS socket (`connect`, `accept`), so it is this row's to close when the
    /// socket dies unclosed, as HotSpot's `SocketCleanable` closes a collected
    /// `NioSocketImpl`'s fd. `false` for a TLS id and for a stream some other
    /// object handed this row, which the sweep never closes.
    pub owns_stream: bool,
}

/// [`SockSide::owner`] of a row whose socket died while a stream of it was
/// still live: an unaligned value no object address can take.
pub const SOCK_OWNER_GONE: usize = 1;

#[derive(Default, Debug, Clone)]
pub struct SsSide {
    pub port: i32,
    pub backlog: i32,
    pub closed: i32,
    pub listener_id: i32,
    /// 1 when one of this surface's own `ServerSocket` constructors built the
    /// receiver — i.e. it really is a plain `ServerSocket` whose whole state
    /// lives here.
    ///
    /// A `ServerSocketChannel.socket()` adapter is also a `java.net.ServerSocket`
    /// and also reaches these natives (native-io registers wrappers for some
    /// methods but not `accept`/`setSoTimeout`), yet its state lives in
    /// native-io's channel registry. It gets a side-table entry the moment
    /// anything here writes one — `setSoTimeout` does — so "has an entry" is NOT
    /// the same question. Only a `0` here means "not ours, do not answer for it".
    pub constructed: i32,
    /// 1 once a bind has succeeded. Distinct from `listener_id >= 0`, which
    /// `close()` resets: `ServerSocket.isBound()` reports whether the socket
    /// was *ever* bound and stays true afterwards ("this method will continue
    /// to return true after the socket is closed"), and `getLocalPort()` /
    /// `getLocalSocketAddress()` keep answering off the retained `port` /
    /// `host` for exactly that reason.
    pub bound: i32,
    /// The bound local address, retained past `close()` alongside `port`.
    pub host: String,
    /// SO_REUSEADDR as last requested through `setReuseAddress`. Java allows
    /// the option to be set on an UNBOUND `ServerSocket` (that is in fact the
    /// only ordering where it changes bind behaviour), and there is no OS
    /// handle to hold it before `re2_bind_listener` runs — so the requested
    /// value is retained here, applied to the socket the bind creates, and the
    /// getter falls back to it whenever the live listener cannot answer.
    /// `-1` = never set by the caller.
    pub reuse_address: i32,
    /// SO_RCVBUF as last requested through `setReceiveBufferSize`; same
    /// before-bind rationale as `reuse_address`. `-1` = never set.
    pub recv_buffer_size: i32,
    /// SO_TIMEOUT (accept timeout) in ms, `0` = infinite. Held per RECEIVER,
    /// not per listener id: `setSoTimeout` is legal on an unbound
    /// `ServerSocket` (`new ServerSocket(); setSoTimeout(ms); bind(addr)`), and
    /// a listener-id-keyed store dropped that value on the floor — the later
    /// `accept()` then blocked forever instead of timing out.
    pub so_timeout: i32,
    /// The address of the receiver this row belongs to, as of its last write
    /// through [`ss_set`] (`0` = never written with the receiver in hand, so
    /// never judged). [`gc_sweep_net_socket_rows`] follows it through a
    /// collection's pointer map and, once the receiver is dead, drops the row
    /// and closes a listener it still holds, as HotSpot's cleaner closes the
    /// descriptor of a collected `ServerSocket` (gc-common w18-f,
    /// `common-w17a-dropped-open-server-socket-keeps-its-listener-and-row`).
    pub owner: usize,
}

/// A GC-stable identity for a Java object, used as a side-table key.
///
/// `pub` because `http_url_connection` has to hold one ACROSS the life
/// of a response stream — see `forget_https_carrier_session_by_key`. Both
/// fields are plain integers and both survive relocation, which is the whole
/// reason this shape exists rather than a raw `ObjectRef`.
///
/// gc-common w28-a (`common-w27b-net-phase-e-native-obj-key-tables-merge-live-identity-hash-colliders`):
/// the object half is the object's WEAK LOCK KEY (`crate::gc_stable_weak_lock_key`),
/// no longer its identity hash. The hash is 32 bits from a per-heap counter
/// that wraps, so two live sockets / carriers of one VM could share it and
/// with it one row (a read on socket A consumed socket B's bytes; carrier B's
/// handshake released carrier A's cached `SSLSession`). A lock key is minted
/// per object, follows the object across moves (the registry is re-addressed
/// after every moving collection) and is freed -- never minted again -- when
/// the object dies, which is when [`forget_net_obj_keys`] drops its rows.
///
/// Write paths mint the key ([`mint_native_obj_key`], for a CURRENT object
/// only, before any table guard); read and remove paths never do
/// ([`existing_native_obj_key`]): an object that was never keyed has no row.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NativeObjKey {
    pub vm: usize,
    /// The object's weak lock key; unique across VMs (one generation counter
    /// for the whole registry).
    pub key: usize,
}

/// The key a WRITE path files `obj`'s row under, minted on first use.
///
/// `obj` must be CURRENT (the receiver at a native's entry, freshly
/// allocated, or re-read through a pin): a stale address is keyed as another
/// object, whose slot the next sweep frees -- and with it the row. Takes the
/// process-wide lock-key registry mutex (a leaf), so compute it BEFORE any
/// table guard of this file.
pub fn mint_native_obj_key(ctx: &dyn NativeContext, obj: ObjectRef) -> NativeObjKey {
    let key = crate::gc_stable_weak_lock_key(ctx, obj)
        .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"));
    NativeObjKey {
        vm: ctx.vm_identity(),
        key,
    }
}

/// The key [`mint_native_obj_key`] would answer for `obj`, or `None` when the
/// calling VM never keyed it -- and then no table of this file has a row for
/// it. Never mints; for read and remove paths. Same lock rule as
/// [`mint_native_obj_key`].
pub fn existing_native_obj_key(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
) -> Option<NativeObjKey> {
    crate::existing_weak_lock_key(ctx, obj).map(|key| NativeObjKey {
        vm: ctx.vm_identity(),
        key,
    })
}

// GC note (gc-followups-20260706): Socket state used to be keyed by the raw
// ObjectRef address. A moving GC can relocate a connected Socket between
// connect()/accept() and a later getInputStream()/getOutputStream(), making the
// lookup miss and default to stream_id=-1 ("not connected"). Key Socket state
// by VM identity + System.identityHashCode instead; both are stable across
// relocation, and the payload contains only plain integers. (`ss_side_table`
// below is keyed the same way since gc-common w9-b; the `DatagramSocket` /
// `InetAddress` tables keep `ObjectRef` keys, remapped after every move and
// swept by `sweep_object_keyed_rows` since w17-a.) Since gc-common w28-a the
// object half is the socket's weak lock key, not its identity hash (see
// [`NativeObjKey`]).
pub fn sock_side_table() -> &'static Mutex<HashMap<NativeObjKey, SockSide>> {
    static T: OnceLock<Mutex<HashMap<NativeObjKey, SockSide>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

// `ServerSocket` state, keyed like `sock_side_table` (gc-common w9-b,
// `common-w8a-identity-hash-keyed-registries-are-shared-across-vms`). It was
// keyed by the identity hash alone, and identity hashes are numbered from the
// same seed in every heap: VM B's `ServerSocket` read VM A's fd, port and
// closed state, so B's `accept()` could run on A's listening fd.
pub fn ss_side_table() -> &'static Mutex<HashMap<NativeObjKey, SsSide>> {
    static T: OnceLock<Mutex<HashMap<NativeObjKey, SsSide>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn sock_default() -> SockSide {
    SockSide {
        host_id: 0,
        host: String::new(),
        port: 0,
        local_port: 0,
        closed: 0,
        stream_id: -1,
        read_timeout_ms: 0,
        enabled_protocols: None,
        tcp_no_delay: false,
        keep_alive: false,
        input_shutdown: 0,
        output_shutdown: 0,
        pending_options: Vec::new(),
        owner: 0,
        owns_stream: false,
    }
}

/// Apply — and clear — every option `Socket.setOption` retained while this
/// Socket had no descriptor. Called right after a `connect` publishes the
/// stream id, the same point `read_timeout_ms` is replayed at.
///
/// A failing `setsockopt` here is deliberately not fatal: the connection is
/// already up, and HotSpot would have applied these to the pre-connect fd where
/// a rejection would have surfaced earlier. The option is dropped and the
/// connect stands.
pub fn apply_pending_socket_options(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let pending = {
        let mut taken = Vec::new();
        sock_set(ctx, this, |s| {
            taken = std::mem::take(&mut s.pending_options)
        });
        taken
    };
    if pending.is_empty() {
        return;
    }
    let Some(fd) = sock_raw_descriptor(ctx, this) else {
        return;
    };
    for (level, opt, value) in pending {
        let _ = sock_set_option_int(fd, level, opt, value);
    }
}

pub fn sock_get(ctx: &dyn NativeContext, this: ObjectRef) -> SockSide {
    match existing_native_obj_key(ctx, this) {
        Some(key) => sock_get_key(key),
        // Never keyed, so never written: the default row.
        None => sock_default(),
    }
}

/// Answer `f` of `this`'s row, read under the table lock WITHOUT cloning
/// the row (gc-common w28-a): `SockSide` owns a `String` and a `Vec`, so
/// [`sock_get`]'s clone is two heap allocations, which every `Socket`
/// accessor that wanted one integer used to pay. `f` runs under the lock:
/// it must only read the row. The default row when there is none.
pub fn sock_peek<R>(ctx: &dyn NativeContext, this: ObjectRef, f: impl FnOnce(&SockSide) -> R) -> R {
    match existing_native_obj_key(ctx, this) {
        Some(key) => sock_peek_key(key, f),
        None => f(&sock_default()),
    }
}

/// [`sock_peek`] by the socket's key (the socket streams).
pub fn sock_peek_key<R>(key: NativeObjKey, f: impl FnOnce(&SockSide) -> R) -> R {
    let t = sock_side_table().lock();
    match t.get(&key) {
        Some(row) => f(row),
        None => f(&sock_default()),
    }
}

/// [`sock_get`] by the socket's key, for callers that hold the key and not
/// the object (the socket streams, gc-common w15-a).
pub fn sock_get_key(key: NativeObjKey) -> SockSide {
    let t = sock_side_table().lock();
    t.get(&key).cloned().unwrap_or_else(sock_default)
}

/// Write `this`'s row (minting its key). `this` must be current: see
/// [`mint_native_obj_key`].
pub fn sock_set<F: FnOnce(&mut SockSide)>(ctx: &dyn NativeContext, this: ObjectRef, f: F) {
    sock_set_key(mint_native_obj_key(ctx, this), f)
}

/// [`sock_set`], and record `this`'s address as the row's weak owner
/// ([`SockSide::owner`], gc-common w16-a), which [`gc_sweep_net_socket_rows`]
/// keeps current across moves from then on.
///
/// Only for a `this` that is CURRENT: freshly allocated, the unmodified
/// receiver at a native's entry, or re-read through a pin after the last
/// allocation or blocking region. A stale address would make the sweep judge a
/// LIVE socket dead and close its connection. [`sock_set`] records nothing, so
/// a row only ever written through it is never judged (it is dropped at VM
/// teardown, as before).
pub fn sock_set_owned<F: FnOnce(&mut SockSide)>(ctx: &dyn NativeContext, this: ObjectRef, f: F) {
    let owner = this.as_ptr() as usize;
    sock_set_key(mint_native_obj_key(ctx, this), |s| {
        s.owner = owner;
        f(s)
    })
}

/// [`sock_set`] by the socket's key.
pub fn sock_set_key<F: FnOnce(&mut SockSide)>(key: NativeObjKey, f: F) {
    let mut t = sock_side_table().lock();
    let entry = t.entry(key).or_insert_with(sock_default);
    f(entry);
}

/// The host `this` socket was asked to connect to (what a client `SSLSocket` identifies the peer against),
/// or `None` for a socket that never recorded one.
pub fn sock_dialled_host(ctx: &dyn NativeContext, this: ObjectRef) -> Option<String> {
    let key = existing_native_obj_key(ctx, this)?;
    let host = sock_get_key(key).host;
    (!host.is_empty()).then_some(host)
}

/// FIX (client-cipher-restriction): read the `s2_registry`/rustls stream id
/// backing a `Socket`/`SSLSocket` object created through this module (-1 if
/// not connected/tracked). Used by `http_url_connection::perform` to route
/// HTTPS I/O through a socket obtained by up-calling a real, caller-installed
/// `SSLSocketFactory.createSocket` (instead of `perform`'s own internal
/// connection) when that factory might apply configuration — like cipher
/// restriction via `SSLSocket.setEnabledCipherSuites` — that only takes
/// effect through the real Java call chain.
pub fn sock_stream_id_for_upcall(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    // Read the one `i32` under the lock instead of `sock_get`'s whole-struct
    // clone. `SockSide` owns a `String` and a `Vec`, so the clone was two heap
    // allocations per call to answer a question about a single integer — and
    // this is the per-call fallback under `SSLSocketInputStream.read()I`, whose
    // callers read megabytes one byte at a time.
    //
    // gc-common w28-a: the key is looked up (never minted) BEFORE the table
    // guard; it takes the lock-key registry mutex.
    let Some(key) = existing_native_obj_key(ctx, this) else {
        return -1;
    };
    sock_side_table()
        .lock()
        .get(&key)
        .map(|s| s.stream_id)
        .unwrap_or(-1)
}

// ---------------------------------------------------------------------------
// HttpsURLConnection "factory probe" mode
// ---------------------------------------------------------------------------
//
// FIX (tls-handshake-enforcement-gap, doc 21). Real JSSE routes every
// `HttpsURLConnection` HTTPS request through the installed
// `SSLSocketFactory.createSocket(...)`, so whatever that factory's own Java
// code does to the socket — most importantly `setEnabledCipherSuites` /
// `setEnabledProtocols` — really constrains the handshake. CratonVM's native
// `HttpURLConnection` (`http_url_connection::perform`) instead owns its
// rustls connection end to end, and that connection is the ONLY one with the
// full client feature set (Java `KeyManager` consultation for mTLS, captured
// trust roots, TLS-ticket reuse). Replacing it with a socket produced by an
// up-called factory would silently drop all of that.
//
// So: up-call the factory purely as a PROBE. While probe mode is on,
// `SSLSocketFactory.createSocket(String,int)` returns an unconnected
// `SSLSocket` carrier (no TCP connect, no handshake, nothing to deadlock on
// re-entrantly), the factory's own `setEnabledCipherSuites`/
// `setEnabledProtocols` calls land in `probe_restrictions()` instead of
// forcing a reconnect, and `http_url_connection` then applies exactly those
// restrictions when it builds its own — fully-featured — `ClientConfig`.
// One connection, real Java semantics, no second handshake.
//
// Thread-local because the probe brackets a single synchronous
// `invoke_virtual` on the calling thread.
thread_local! {
    pub static HUC_FACTORY_PROBE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn set_huc_factory_probe_mode(on: bool) {
    HUC_FACTORY_PROBE.with(|c| c.set(on));
}

pub fn huc_factory_probe_mode() -> bool {
    HUC_FACTORY_PROBE.with(|c| c.get())
}

/// Cipher-suite / protocol restrictions recorded against a probe socket,
/// keyed the same way as `sock_side_table`. Kept out of `SockSide` so the
/// hot, frequently-cloned socket state doesn't grow two `Vec`s for a case
/// only `HttpsURLConnection` probing hits.
pub type ProbeRestrictions = (Vec<String>, Vec<String>);
pub fn probe_restrictions() -> &'static Mutex<HashMap<NativeObjKey, ProbeRestrictions>> {
    static T: OnceLock<Mutex<HashMap<NativeObjKey, ProbeRestrictions>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// True when `this` is a probe socket (created during probe mode and never
/// connected) — the setters below then record rather than reconnect.
///
/// gc-common w28-a: the key is looked up, never minted, and BEFORE the table
/// guard (the four accessors below used to compute it inside the guard's
/// statement). A socket never keyed is no probe socket: the probe mints its
/// key when it files the entry.
pub fn is_probe_socket(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let Some(key) = existing_native_obj_key(ctx, this) else {
        return false;
    };
    probe_restrictions().lock().contains_key(&key)
}

pub fn record_probe_ciphers(ctx: &mut dyn NativeContext, this: ObjectRef, ciphers: Vec<String>) {
    let key = if huc_factory_probe_mode() {
        mint_native_obj_key(ctx, this)
    } else {
        let Some(key) = existing_native_obj_key(ctx, this) else {
            return;
        };
        key
    };
    let mut table = probe_restrictions().lock();
    if huc_factory_probe_mode() {
        table.entry(key).or_insert_with(|| (Vec::new(), Vec::new())).0 = ciphers;
    } else if let Some(e) = table.get_mut(&key) {
        e.0 = ciphers;
    }
}

pub fn record_probe_protocols(ctx: &mut dyn NativeContext, this: ObjectRef, protocols: Vec<String>) {
    let key = if huc_factory_probe_mode() {
        mint_native_obj_key(ctx, this)
    } else {
        let Some(key) = existing_native_obj_key(ctx, this) else {
            return;
        };
        key
    };
    let mut table = probe_restrictions().lock();
    if huc_factory_probe_mode() {
        table.entry(key).or_insert_with(|| (Vec::new(), Vec::new())).1 = protocols;
    } else if let Some(e) = table.get_mut(&key) {
        e.1 = protocols;
    }
}

/// Consume the restrictions the factory applied to the probe socket.
pub fn take_probe_restrictions(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<ProbeRestrictions> {
    let key = existing_native_obj_key(ctx, this)?;
    probe_restrictions().lock().remove(&key)
}

/// Transfer an accepted plain Socket's TCP stream to a TLS layer.
pub fn take_raw_socket_stream_for_tls(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<std::net::TcpStream, String> {
    let side = sock_get(ctx, this);
    if side.stream_id >= 0 {
        let stream = crate::servlet::s2_registry()
            .lock()
            .streams
            .remove(&side.stream_id)
            .ok_or_else(|| "wrapped Socket stream is not available".to_string())?;
        let tcp = match std::sync::Arc::try_unwrap(stream) {
            Ok(tcp) => tcp,
            Err(shared) => shared
                .try_clone()
                .map_err(|e| format!("clone wrapped Socket stream: {e}"))?,
        };
        sock_set(ctx, this, |s| {
            s.stream_id = -1;
            s.closed = 1;
            // The TLS layer owns the stream now.
            s.owns_stream = false;
        });
        return Ok(tcp);
    }

    // Real JDK ServerSocket.accept() creates a NioSocketImpl whose TCP stream
    // is owned by native-io's sun.nio.ch.Net registry, not this module's
    // legacy s2 registry.  Extract its FileDescriptor and hand the stream to
    // rustls before MockWebServer calls SSLSocketFactory.createSocket(Socket,
    // ...).
    let mut implementation = match ctx.get_field_by_name(this, "impl") {
        Value::Object(Some(implementation)) => implementation,
        _ => return Err("wrapped Socket is not connected".to_string()),
    };
    // FIX (mysql-connector-j STARTTLS layered-socket handshake): a plain
    // client-side `new Socket()` + `.connect(SocketAddress, timeout)` (the
    // shape `com.mysql.cj.protocol.StandardSocketFactory` uses to build the
    // socket it later upgrades to TLS) does NOT get a bare `NioSocketImpl` in
    // `impl` the way `ServerSocket.accept()` does. Since (at least) JDK 13,
    // `Socket.createImpl()` always wraps the real impl in
    // `java.net.SocksSocketImpl` (a `DelegatingSocketImpl`) so a SOCKS proxy
    // configured after construction is still honored — CONFIRMED via
    // reflection on both HotSpot and CratonVM: `impl.getClass()` is
    // `java.net.SocksSocketImpl` and `impl.fd` itself is null on BOTH VMs;
    // the live `FileDescriptor` sits one level deeper, at
    // `impl.delegate.fd`. This extraction only ever looked at `impl.fd`
    // directly, so it always answered "wrapped Socket has no
    // FileDescriptor" for this socket shape and silently fell back to
    // `PendingLayeredStream::DialFresh` (a brand-new TCP connection to the
    // same host:port). That fallback is invisible for a protocol that
    // speaks TLS as the very first thing on the wire (a fresh dial looks
    // identical to the caller's original connection), but MySQL's protocol
    // upgrades an ALREADY-connected, already-used-for-plaintext socket to
    // TLS mid-stream (the SSLRequest packet) — the fresh dial instead hands
    // rustls the server's plaintext initial-handshake packet as if it were
    // the first TLS record, which fails immediately with exactly the
    // observed `SSLHandshakeException: handshake process: received corrupt
    // message of type InvalidContentType` (rustls's `Display` for
    // `InvalidMessage::InvalidContentType`, produced when the first bytes
    // read aren't a valid TLS record). Unwrap `delegate` (bounded, in case a
    // real SOCKS chain or a future JDK adds another layer) before giving up.
    let mut descriptor = None;
    for _ in 0..4 {
        match ctx.get_field_by_name(implementation, "fd") {
            Value::Object(Some(d)) => {
                descriptor = Some(d);
                break;
            }
            _ => match ctx.get_field_by_name(implementation, "delegate") {
                Value::Object(Some(next)) => implementation = next,
                _ => break,
            },
        }
    }
    let descriptor = match descriptor {
        Some(descriptor) => descriptor,
        None => return Err("wrapped Socket has no FileDescriptor".to_string()),
    };
    let fd = match ctx.get_field_by_name(descriptor, "fd") {
        Value::Int(fd) if fd >= 0 => fd,
        _ => match ctx.get_field_by_name(descriptor, "handle") {
            Value::Long(fd) if (0..=i32::MAX as i64).contains(&fd) => fd as i32,
            _ => return Err("wrapped Socket FileDescriptor has no Net fd".to_string()),
        },
    };
    cratonvm_native_io::net::take_stream_for_tls(fd)
}

/// FIX (netty-client-socket-write-after-close): companion to
/// [`sock_stream_id_for_upcall`] for `phases_late.rs`'s NEW-13
/// `javax/net/ssl/SSLSocket` stream/lifecycle natives (`getInputStream`,
/// `getOutputStream`, `close`, `isClosed`, `isConnected`). Those read/write
/// raw object fields (`NEW13_SOCK_TLSID`/`NEW13_SOCK_CLOSED`), which is fine
/// for a socket `new13_do_create_socket` built itself — but
/// `SSLSocketFactory.createSocket(String,int)` is ALSO registered here (this
/// module registers `register_phase_e_networking` after `register_p68_ssl`,
/// so this implementation wins for that exact (class,name,descriptor) key)
/// and builds the socket through the side table above, leaving those raw
/// fields at their default `Object(None)`. Real bytecode method resolution
/// then finds phases_late.rs's `getOutputStream`/`close`/etc — they're
/// registered on the concrete `javax/net/ssl/SSLSocket` class, a more
/// specific match than anything this module registers on the `java/net/
/// Socket` superclass — so those raw-field readers ran regardless of which
/// factory built the object, read the never-populated field, and treated
/// every write on a `createSocket(host,port)`-obtained socket as though the
/// stream had already been closed. These accessors let phases_late.rs fall
/// back to the side table when the raw field isn't a valid entry.
pub fn sock_mark_closed_for_upcall(ctx: &dyn NativeContext, this: ObjectRef) {
    sock_set(ctx, this, |s| {
        s.closed = 1;
        s.stream_id = -1;
    });
}

pub fn sock_is_closed_for_upcall(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    sock_peek(ctx, this, |s| s.closed) != 0
}

/// FIX (netty-client-socket-write-after-close): let `phases_late.rs`'s
/// `new13_do_create_socket` record its connect result HERE instead of (only)
/// in a raw object field. `alloc_concurrent_synthetic` sizes a "synthetic"
/// object using the REAL loaded class's actual field count/layout when the
/// class is loadable (`ctx.class_num_total_fields`) — see its own doc
/// comment — so field index 2 on a `javax/net/ssl/SSLSocket` lands wherever
/// the real class hierarchy's own 3rd field actually is, not a slot we
/// control. Traced with a same-native-call immediate readback
/// (CRATONVM_DBG_TLS_SOCK): `ctx.set_field(sock, 2, Value::Int(tls_id))`
/// followed instantly by `ctx.get_field(sock, 2)` — no Java code, no GC,
/// same call — already read back `Object(None)`, proving the object's real
/// field #2 is reference-typed and the GC/field-layout guard silently drops
/// a mismatched-type write rather than erroring. This is exactly the
/// collision this file's own `SockSide` table was introduced to avoid for
/// plain `java.net.Socket`/`ServerSocket`/`DatagramSocket`; `SSLSocket`
/// needs the same treatment.
pub fn sock_set_for_create(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    port: i32,
    stream_id: i32,
) {
    sock_set_for_create_with_local_port(ctx, this, port, 0, stream_id);
}

/// Same as [`sock_set_for_create`] but also records the real local (client)
/// port, for callers that already resolved it from the live `TcpStream`
/// (e.g. `phases_early.rs`'s `phase52_socket_connect`) instead of always
/// defaulting to 0.
pub fn sock_set_for_create_with_local_port(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    port: i32,
    local_port: i32,
    stream_id: i32,
) {
    sock_set(ctx, this, |s| {
        s.port = port;
        s.local_port = local_port;
        s.closed = 0;
        s.stream_id = stream_id;
        // The id is the caller's (a TLS id, or a stream it opened): the
        // owner-death sweep never closes it (gc-common w16-a). See
        // `sock_set_for_create_owning_stream` for a plain stream the caller
        // hands over.
        s.owns_stream = false;
    });
}

/// [`sock_set_for_create_with_local_port`] for a PLAIN `s2_registry` stream
/// the caller opened for `this` and hands over: the row owns it, so a socket
/// dropped without `close()` has it closed by [`gc_sweep_net_socket_rows`]
/// (gc-common w16-a). For `phases_early`'s `SocketFactory.createSocket`
/// (`handoff-w16a-socket-factory-socket-owns-its-stream`).
pub fn sock_set_for_create_owning_stream(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    port: i32,
    local_port: i32,
    stream_id: i32,
) {
    // `this` must be current: the row records its address (see
    // `sock_set_owned`).
    sock_set_owned(ctx, this, |s| {
        s.port = port;
        s.local_port = local_port;
        s.closed = 0;
        s.stream_id = stream_id;
        s.owns_stream = (0..crate::servlet::RUSTLS_SOCK_ID_BASE).contains(&stream_id);
    });
}

pub fn ss_default() -> SsSide {
    SsSide {
        port: -1,
        backlog: 50,
        closed: 0,
        listener_id: -1,
        constructed: 0,
        bound: 0,
        host: String::new(),
        reuse_address: -1,
        recv_buffer_size: -1,
        so_timeout: 0,
        owner: 0,
    }
}

pub fn ss_get(ctx: &dyn NativeContext, this: ObjectRef) -> SsSide {
    let Some(key) = existing_native_obj_key(ctx, this) else {
        return ss_default();
    };
    let t = ss_side_table().lock();
    t.get(&key).cloned().unwrap_or_else(ss_default)
}

/// Whether this receiver has an entry in the side table — i.e. whether the RE.2
/// surface has ever handled it. Every RE.2 constructor writes one, so a `false`
/// means the socket came from somewhere else (the phase-53 4-field surface) and
/// its state has to be read from its object fields instead.
pub fn ss_tracked(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let Some(key) = existing_native_obj_key(ctx, this) else {
        return false;
    };
    ss_side_table().lock().contains_key(&key)
}

/// Write `this`'s row, and record `this`'s address as the row's weak owner
/// ([`SsSide::owner`], gc-common w18-f), which [`gc_sweep_net_socket_rows`]
/// keeps current across moves from then on.
///
/// `this` must be CURRENT: the unmodified receiver at a native's entry, or
/// re-read through a pin after the last allocation. A stale address would make
/// the sweep judge a LIVE socket dead and close its listener. Every caller in
/// this file passes one (the constructors re-read it from
/// `re1_init_socket_locks`; `re2_bind_listener`'s callers allocate nothing
/// before it; the option setters and `close` write at entry).
pub fn ss_set<F: FnOnce(&mut SsSide)>(ctx: &dyn NativeContext, this: ObjectRef, f: F) {
    let key = mint_native_obj_key(ctx, this);
    let owner = this.as_ptr() as usize;
    let mut t = ss_side_table().lock();
    let entry = t.entry(key).or_insert_with(ss_default);
    entry.owner = owner;
    f(entry);
}

// `DatagramSocket` side-table — same rationale as `SockSide`/`SsSide` above.
// The synthetic natives stored port/closed/timeout/fd in object slots
// `DS_PORT=0 / DS_CLOSED=1 / DS_TIMEOUT=2 / DS_FD=3`, but in real-JDK mode the
// loaded `java.net.DatagramSocket` (JDK 17+) has a single instance field
// (`delegate`), so slots 1/2/3 are out of bounds — the writes were dropped by
// the GC guard, the `fd` was lost, and every send/receive saw a "closed"
// socket. Keying the state by `ObjectRef` makes it layout-independent. (The
// synthetic-JDK `register_p72_datagram` path keeps its 4-field-layout natives;
// only this real-JDK `register_re7_datagram_socket` set is converted.)
#[derive(Default, Debug, Clone, Copy)]
pub struct DsSide {
    pub port: i32,    // local port (was DS_PORT)
    pub closed: i32,  // 0 = open, 1 = closed (was DS_CLOSED)
    pub timeout: i32, // SO_TIMEOUT ms (was DS_TIMEOUT)
    pub fd: i32,      // udp fd handle; -1 = closed/unset (was DS_FD)
    /// SO_BROADCAST as last requested through `setBroadcast`. Pushed to the
    /// real UDP fd too, but `FileDescriptorTable` exposes no read-back, so the
    /// getter answers from here. `-1` = never set, which reads back as the
    /// JDK's default of `false`.
    pub broadcast: i32,
    /// 1 once `connect()` has associated the socket with a peer, 0 after
    /// `disconnect()`. Tracked here rather than derived from the OS because
    /// `isConnected()` must keep answering after the peer goes away, which is
    /// what the JDK specifies ("this method will continue to return true after
    /// the socket is closed").
    pub connected: i32,
    /// SO_REUSEADDR as last requested through `setReuseAddress`. The option is
    /// pushed to the real UDP fd as well, but `FileDescriptorTable` exposes no
    /// read-back for it, so the getter answers from here. `-1` = never set.
    pub reuse_address: i32,
}

/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0, the leaf level) because no
/// lock is ever taken while this one is held, and this one is never held across
/// a re-entry into the VM. Every acquisition site has been read: `ds_get`
/// copies a `DsSide` out and drops the guard; `ds_set` runs a closure and every
/// one of its fourteen callers assigns plain `i32` fields and touches no `ctx`;
/// `gc_update_ds_refs` calls the pure `rekey`; [`sweep_object_keyed_rows`]
/// copies keys into a `Vec`, or removes rows, in one statement per row. The
/// GC remap runs with the heap lock (L8) held, which is legal — L0 < L8 is
/// the descending order the wrapper asserts. The sweep asks the heap only with
/// this lock RELEASED (the reverse nesting would be illegal).
///
/// Each value carries the `vm_identity` of the VM that last wrote the row
/// (gc-common w8-a, row `datagram-sockets`): the table is shared by every VM in
/// the process, and only the owning VM's collections sweep and re-key a row.
/// The rows are WEAK since gc-common w17-a: nothing roots the key.
/// `ds_set` re-stamps the owner on every write, so a row a torn-down VM left at
/// an address another heap now uses is adopted by the first write through it.
pub fn ds_side_table() -> &'static OrderedPlMutex<HashMap<ObjectRef, (usize, DsSide)>> {
    static T: OnceLock<OrderedPlMutex<HashMap<ObjectRef, (usize, DsSide)>>> = OnceLock::new();
    T.get_or_init(|| OrderedPlMutex::new(HashMap::new(), LockLevel::Scratch))
}

/// The peer a `DatagramSocket` was last `connect`ed to, as `(numeric host,
/// port)`.
///
/// Separate from [`DsSide`] only because that struct is `Copy` and this is a
/// `String`; the lifecycle is the same — written by both `connect` overloads,
/// cleared by `disconnect`, and deliberately NOT cleared by `close`, because
/// the JDK specifies `getPort`/`getInetAddress` keep answering after the socket
/// is closed (same rule that keeps `isConnected()` true).
/// Same `LockLevel::Scratch` claim as [`ds_side_table`], and the same four
/// call shapes — `ds_peer` clones out, `ds_set_peer`/`ds_clear_peer` are one
/// statement each, and the two GC hooks read keys or `rekey`.
/// Values lead with the owning `vm_identity`, as [`ds_side_table`]'s do.
pub fn ds_peer_table() -> &'static OrderedPlMutex<HashMap<ObjectRef, (usize, String, i32)>> {
    static T: OnceLock<OrderedPlMutex<HashMap<ObjectRef, (usize, String, i32)>>> = OnceLock::new();
    T.get_or_init(|| OrderedPlMutex::new(HashMap::new(), LockLevel::Scratch))
}

/// The connected peer, or `None` when this socket has never connected or has
/// disconnected since. The lock is released before returning: every caller goes
/// on to allocate, and holding a process-global mutex across an allocation
/// (which can enter the collector, and on a cold VM a class load) is a lock
/// cycle waiting to happen.
pub fn ds_peer(this: ObjectRef) -> Option<(String, i32)> {
    ds_peer_table()
        .lock()
        .get(&this)
        .map(|(_, host, port)| (host.clone(), *port))
}

/// Record VM `vm`'s socket `this` as connected to `host:port`.
pub fn ds_set_peer(vm: usize, this: ObjectRef, host: &str, port: i32) {
    let mut t = ds_peer_table().lock();
    t.insert(this, (vm, host.to_string(), port));
    DS_PEER_WRITE_EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// Write epochs of the three object-keyed tables (gc-common w17-a). Each is
/// bumped UNDER its table's lock by every write through the table's setter
/// (`ds_set`, `ds_set_peer`, `inet_addr_set`), and read under the same lock by
/// [`sweep_object_keyed_rows`]: the sweep judges its candidates with the lock
/// released, and drops them only if the table has not been written since it
/// collected them. Outside a pause a mutator can allocate a new socket or
/// address on a just-freed address and file a row under it; an unchanged epoch
/// proves that did not happen, a changed one defers the drop to the next sweep
/// (the pause-site sweep always sees an unchanged epoch: mutators are stopped).
/// Removals do not bump: removing a row cannot make a dead key's row live.
pub static DS_WRITE_EPOCH: AtomicU64 = AtomicU64::new(0);
pub static DS_PEER_WRITE_EPOCH: AtomicU64 = AtomicU64::new(0);
pub static INET_WRITE_EPOCH: AtomicU64 = AtomicU64::new(0);

pub fn ds_clear_peer(this: ObjectRef) {
    ds_peer_table().lock().remove(&this);
}

/// The scan half of the `datagram-sockets` root source: roots NOTHING since
/// gc-common w17-a (`common-w16a-inet-address-and-datagram-rows-root-their-keys`).
///
/// Both `DatagramSocket` tables are `HashMap<ObjectRef, _>`, so their keys ARE
/// addresses: a moving collection that relocates a `DatagramSocket` strands its
/// entry, and a dead entry later collides with whatever object the allocator
/// places on that address — a silent wrong answer, not a lookup miss
/// (`docs/threading/objectref-concurrency-contract.md` §7.3). Both tables carry
/// state the JDK requires to survive `close()` (`getPort`/`getInetAddress` keep
/// answering, same rule that keeps `isConnected()` true), so neither can be
/// cleared on close to sidestep this.
///
/// This used to push every key of `vm` as a root, which answered the collision
/// by keeping every `DatagramSocket` ever created alive until VM teardown. The
/// rows are WEAK now: [`gc_sweep_net_socket_rows`] (its
/// `sweep_object_keyed_rows` pass) drops the rows of a socket the collection
/// did not keep, before any allocation can reuse its address, and queues the
/// UDP descriptor of one dropped open for closing; the remap half,
/// [`gc_update_ds_refs`], still re-keys the survivors. The row stays in
/// `native_roots.rs` for that remap; the scan is kept, empty, so the row's
/// shape (and the `datagram_socket_side_tables_are_a_registered_root_source`
/// inventory test) is unchanged.
pub fn gc_scan_ds_roots(_vm: usize, _out: &mut Vec<ObjectRef>) {}

/// Companion to [`gc_scan_ds_roots`]: re-key both tables through the
/// collector's relocation map so a lookup on the relocated `DatagramSocket`
/// still resolves.
/// Takes `cratonvm_types::PointerMap` (an `FxHashMap`), not a std `HashMap`:
/// that is what `native_roots::VmRemapFn` is typed as. This landed as a plain
/// `HashMap<usize, usize>` in `addrkeyed-20260807`, concurrently with the
/// change that retyped `VmRemapFn`, and the two merged into a `dev` on which
/// `cargo test -p cratonvm-vm --lib` did not compile at all (E0308 at the
/// `root_source!("datagram-sockets", ...)` row — `RandomState` vs
/// `BuildHasherDefault<FxHasher>`). Neither branch was wrong alone.
///
/// Only `vm`'s rows are re-keyed (gc-common w8-a). They are all taken out
/// before any is put back, so a relocated key never collides with a
/// not-yet-relocated one.
pub fn gc_update_ds_refs(vm: usize, pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() {
        return;
    }
    fn rekey<V>(
        map: &mut std::collections::HashMap<ObjectRef, V>,
        pointer_map: &cratonvm_types::PointerMap,
        owned: impl Fn(&V) -> bool,
    ) {
        let mine: Vec<ObjectRef> = map
            .iter()
            .filter(|&(_, v)| owned(v))
            .map(|(k, _)| *k)
            .collect();
        let drained: Vec<(ObjectRef, V)> = mine
            .into_iter()
            .filter_map(|k| map.remove(&k).map(|v| (k, v)))
            .collect();
        for (k, v) in drained {
            let nk = pointer_map
                .get(&(k.as_ptr() as usize))
                .map(|&n| unsafe { ObjectRef::from_raw(n as *mut u8) })
                .unwrap_or(k);
            map.insert(nk, v);
        }
    }
    rekey(
        &mut ds_side_table().lock(),
        pointer_map,
        |(owner, _): &(usize, DsSide)| *owner == vm,
    );
    rekey(
        &mut ds_peer_table().lock(),
        pointer_map,
        |(owner, _, _): &(usize, String, i32)| *owner == vm,
    );
}

/// Per-VM teardown for the `datagram-sockets` and `inet-addresses` rows: drop
/// every row `vm` owns. Idempotent (gc-common w8-a).
///
/// gc-common w9-b: also the `Socket` / `ServerSocket` state and the stream ->
/// owner rows, which are keyed by `NativeObjKey` and were never dropped: a
/// later VM with the same identity cannot exist (`vm_identity` is never
/// recycled), so this only returns their memory. (Since gc-common w15-a a
/// stream-owner row holds the owner's key, not a global-root handle.)
pub fn forget_vm_socket_side_tables(vm: usize) {
    sock_side_table().lock().retain(|k, _| k.vm != vm);
    ss_side_table().lock().retain(|k, _| k.vm != vm);
    {
        let mut owners = stream_owner_table().lock();
        owners.by_stream.retain(|k, _| k.vm != vm);
        owners.by_owner.retain(|k, _| k.vm != vm);
        owners.addrs.retain(|k, _| k.vm != vm);
    }
    // gc-common w16-a: the work the owner-death sweep queued for this VM. Its
    // global-root table goes with the VM, and `servlet::forget_vm_s2_sockets`
    // (the next teardown step) closes every stream the VM opened.
    {
        let mut deferred = net_deferred().lock();
        deferred.roots.remove(&vm);
        deferred.streams.retain(|&(owner, _)| owner != vm);
        // gc-common w17-a: the VM's `FileDescriptorTable` goes with the VM
        // and closes what it still holds.
        deferred.udp_fds.remove(&vm);
        // gc-common w18-f: `servlet::forget_vm_s2_sockets` closes every
        // listener the VM bound.
        deferred.listeners.retain(|&(owner, _)| owner != vm);
    }
    // gc-common w16-a (`common-w15a-http-server-outlives-its-vm`): a started,
    // never-stopped `HttpServer` of this VM. See `forget_vm_http_servers`.
    forget_vm_http_servers(vm);
    ssc_owner_table().lock().retain(|k, _| k.vm != vm);
    ssc_side_table().lock().retain(|(k, _), _| k.vm != vm);
    ssc_carrier_roots().lock().retain(|(k, _), _| k.vm != vm);
    re5_client_shutdown_table().lock().retain(|k, _| k.vm != vm);
    sss_option_delegates().lock().retain(|k, _| k.vm != vm);
    // gc-common w15-a: two more `NativeObjKey`-keyed tables this teardown
    // missed. An `HttpsURLConnection` carrier's recorded handshake (its peer
    // chain DER, strings and an `SSLSession` root handle) outlived its VM
    // whenever the carrier was never recycled, and a probe socket's
    // restrictions whenever the probe was abandoned.
    https_carrier_sessions().lock().retain(|k, _| k.vm != vm);
    probe_restrictions().lock().retain(|k, _| k.vm != vm);
    ds_side_table().lock().retain(|_, (owner, _)| *owner != vm);
    ds_peer_table()
        .lock()
        .retain(|_, (owner, _, _)| *owner != vm);
    inet_addr_side_table()
        .lock()
        .retain(|_, (owner, _, _)| *owner != vm);
}

/// gc-common w28-a (`common-w27b-net-phase-e-native-obj-key-tables-merge-live-identity-hash-colliders`):
/// every [`NativeObjKey`] row filed under a weak lock key the registry has
/// just FREED -- its object died (`lib.rs::sweep_lock_keys`, after every
/// collection, BEFORE [`gc_sweep_net_socket_rows`] at each of the three
/// sites) or its VM went (`lib.rs::forget_vm_lock_keys`). A freed key is never
/// minted again, so nothing can read these rows any more; `keys` holds every
/// weak key the sweep freed, of every kind, and most name no row here.
///
/// Does for a freed key what the owner-death sweep does for a dead owner
/// address (the two agree: same predicate, same sites), so a row written
/// without an owner address (`sock_set`) goes with its object too instead of
/// waiting for VM teardown:
///
/// * a stream's `stream_owner_table` rows; then a socket's `sock_side_table`
///   row, unless one of its streams still lives -- then it is marked
///   `SOCK_OWNER_GONE` and the owner-death sweep drops it once the last stream
///   dies. A dropped row's own plain stream (`owns_stream`) is queued for
///   closing; its `probe_restrictions` entry goes too;
/// * a `ServerSocket`'s `ss_side_table` row, its listener queued for closing
///   (lesson x: never closed under a table lock);
/// * an `SSLContext`'s `ssc_carrier_roots` rows (roots queued for release), a
///   carrier's `ssc_owner_table` binding, and then every `ssc_side_table` row
///   of the VMs concerned that no binding names;
/// * an `SSLServerSocket`'s `sss_option_delegates` row (root queued), an
///   `HttpClient`'s `re5_client_shutdown_table` row, an `HttpsURLConnection`
///   carrier's `https_carrier_sessions` row (its `SSLSession` root queued).
///
/// Queued work lands in [`net_deferred`]: the owner-death sweep that follows
/// at every sweep site closes the streams and listeners (under `try_lock`),
/// and the VM's next [`net_release_deferred`] releases the roots. Every table
/// lock is taken on its own (the binding / side pair nested as in the
/// owner-death sweep), never under the lock-key registry's; nothing here
/// reaches Java or allocates on the heap.
pub fn forget_net_obj_keys(keys: &[usize]) {
    if keys.is_empty() {
        return;
    }
    // Built on first use: a sweep that frees keys usually finds these tables
    // empty.
    let set: std::cell::OnceCell<std::collections::HashSet<usize>> = std::cell::OnceCell::new();
    let freed = |k: &NativeObjKey| {
        set.get_or_init(|| keys.iter().copied().collect())
            .contains(&k.key)
    };

    let mut roots: Vec<(usize, usize)> = Vec::new();
    let mut streams: Vec<(usize, i32)> = Vec::new();
    let mut listeners: Vec<(usize, i32)> = Vec::new();

    // 1. Dead streams' owner rows, then the sockets that still have a stream.
    let owners_with_streams: std::collections::HashSet<NativeObjKey> = {
        let mut guard = stream_owner_table().lock();
        let t = &mut *guard;
        if t.by_stream.is_empty() && t.by_owner.is_empty() {
            std::collections::HashSet::new()
        } else {
            let dead_streams: Vec<NativeObjKey> =
                t.by_stream.keys().filter(|k| freed(*k)).copied().collect();
            for s in dead_streams {
                t.addrs.remove(&s);
                if let Some(owner) = t.by_stream.remove(&s) {
                    if let Some(list) = t.by_owner.get_mut(&owner) {
                        list.retain(|x| *x != s);
                        if list.is_empty() {
                            t.by_owner.remove(&owner);
                        }
                    }
                }
            }
            t.addrs.retain(|k, _| !freed(k));
            t.by_owner.keys().filter(|k| freed(*k)).copied().collect()
        }
    };
    // 2. Sockets (and the TLS socket streams, which have rows of their own).
    {
        let mut t = sock_side_table().lock();
        if !t.is_empty() {
            let hit: Vec<NativeObjKey> = t.keys().filter(|k| freed(*k)).copied().collect();
            for k in hit {
                if owners_with_streams.contains(&k) {
                    if let Some(row) = t.get_mut(&k) {
                        row.owner = SOCK_OWNER_GONE;
                    }
                } else if let Some(row) = t.remove(&k) {
                    if row.owns_stream && row.stream_id >= 0 {
                        streams.push((k.vm, row.stream_id));
                    }
                }
            }
        }
    }
    {
        let mut probes = probe_restrictions().lock();
        if !probes.is_empty() {
            probes.retain(|k, _| !freed(k));
        }
    }
    // 3. Server sockets.
    {
        let mut t = ss_side_table().lock();
        if !t.is_empty() {
            let hit: Vec<NativeObjKey> = t.keys().filter(|k| freed(*k)).copied().collect();
            for k in hit {
                if let Some(row) = t.remove(&k) {
                    if row.listener_id >= 0 {
                        listeners.push((k.vm, row.listener_id));
                    }
                }
            }
        }
    }
    // 4. SSL session contexts.
    {
        let mut t = ssc_carrier_roots().lock();
        if !t.is_empty() {
            let hit: Vec<SscKey> = t.keys().filter(|k| freed(&k.0)).copied().collect();
            for k in hit {
                if let Some(row) = t.remove(&k) {
                    roots.push((k.0.vm, row.handle));
                }
            }
        }
    }
    {
        // The side rows are retained under the bindings' lock, as in the
        // owner-death sweep, so a binding `ssc_set` files meanwhile is seen
        // before its side row can exist.
        let mut bindings = ssc_owner_table().lock();
        if !bindings.is_empty() {
            let dead: Vec<NativeObjKey> = bindings.keys().filter(|k| freed(*k)).copied().collect();
            if !dead.is_empty() {
                let vms: std::collections::HashSet<usize> = dead.iter().map(|k| k.vm).collect();
                for k in &dead {
                    bindings.remove(k);
                }
                let named: std::collections::HashSet<SscKey> = bindings
                    .iter()
                    .filter(|(k, _)| vms.contains(&k.vm))
                    .map(|(_, b)| b.key)
                    .collect();
                ssc_side_table()
                    .lock()
                    .retain(|key, _| !vms.contains(&(key.0).vm) || named.contains(key));
            }
        }
    }
    // 5. `SSLServerSocket` delegates, `HttpClient` shutdown rows, recorded
    // `https` handshakes.
    {
        let mut t = sss_option_delegates().lock();
        if !t.is_empty() {
            let hit: Vec<NativeObjKey> = t.keys().filter(|k| freed(*k)).copied().collect();
            for k in hit {
                if let Some(e) = t.remove(&k) {
                    roots.push((k.vm, e.root));
                }
            }
        }
    }
    {
        let mut t = re5_client_shutdown_table().lock();
        if !t.is_empty() {
            t.retain(|k, _| !freed(k));
        }
    }
    {
        let mut t = https_carrier_sessions().lock();
        if !t.is_empty() {
            let hit: Vec<NativeObjKey> = t.keys().filter(|k| freed(*k)).copied().collect();
            for k in hit {
                if let Some(s) = t.remove(&k) {
                    if s.session_root != 0 {
                        roots.push((k.vm, s.session_root));
                    }
                }
            }
        }
    }
    // 6. Queue what needs a `NativeContext` or the socket registry.
    if !roots.is_empty() || !streams.is_empty() || !listeners.is_empty() {
        let mut deferred = net_deferred().lock();
        for (vm, handle) in roots {
            deferred.roots.entry(vm).or_default().push(handle);
        }
        deferred.streams.extend(streams);
        deferred.listeners.extend(listeners);
    }
}

/// Work [`gc_sweep_net_socket_rows`] leaves behind (gc-common w16-a). The sweep
/// runs after a collection with no `NativeContext`, so it cannot release a
/// global root, and it must not wait on `servlet::s2_registry` inside a pause,
/// so a stream it could not close at once waits here too.
#[derive(Default)]
pub struct NetDeferred {
    /// Per VM: global-root handles whose row is gone (an `SSLSessionContext`
    /// carrier whose `SSLContext` died, an `SSLServerSocket` option delegate
    /// whose socket died). Released by the VM's next [`net_release_deferred`].
    pub roots: HashMap<usize, Vec<usize>>,
    /// `(vm, s2 stream id)`: the plain streams of sockets dropped without
    /// `close()`. Closing needs no `NativeContext`, so any drain closes any
    /// VM's; the VM is kept for the teardown filter.
    pub streams: Vec<(usize, i32)>,
    /// Per VM: the UDP descriptors of `DatagramSocket`s dropped without
    /// `close()` (gc-common w17-a). They live in the VM's own
    /// `FileDescriptorTable`, which only a `NativeContext` of that VM reaches,
    /// so the VM's next [`net_release_deferred`] closes them (HotSpot's
    /// `DatagramChannelImpl` cleaner does the same for a collected socket).
    pub udp_fds: HashMap<usize, Vec<i32>>,
    /// `(vm, s2 listener id)`: the listeners of plain `ServerSocket`s dropped
    /// without `close()` (gc-common w18-f), whose removal found the socket
    /// registry busy. Like `streams`, closing needs no `NativeContext`, so any
    /// drain closes any VM's; the VM is kept for the teardown filter and for
    /// the owner check in [`net_close_listeners`].
    pub listeners: Vec<(usize, i32)>,
}

/// `LockLevel::Scratch`: every acquisition moves `usize`/`i32` rows in or out
/// in one statement, and nothing else is locked or called while it is held.
pub fn net_deferred() -> &'static OrderedPlMutex<NetDeferred> {
    static T: OnceLock<OrderedPlMutex<NetDeferred>> = OnceLock::new();
    T.get_or_init(|| OrderedPlMutex::new(NetDeferred::default(), LockLevel::Scratch))
}

/// Release the global roots the sweep queued for this VM and close the streams
/// it queued (gc-common w16-a). Allocates nothing and runs no Java, so a
/// caller's raw `ObjectRef`s stay current across it. Called from the natives
/// that create what the sweep releases (`ssc_carrier`,
/// `sss_option_delegate_root`, the `DatagramSocket` constructors and `bind`)
/// and from `Socket` connect / accept.
pub fn net_release_deferred(ctx: &mut dyn NativeContext) {
    let vm = ctx.vm_identity();
    let (roots, streams, udp_fds, listeners) = {
        let mut deferred = net_deferred().lock();
        if deferred.roots.is_empty()
            && deferred.streams.is_empty()
            && deferred.udp_fds.is_empty()
            && deferred.listeners.is_empty()
        {
            return;
        }
        (
            deferred.roots.remove(&vm).unwrap_or_default(),
            std::mem::take(&mut deferred.streams),
            deferred.udp_fds.remove(&vm).unwrap_or_default(),
            std::mem::take(&mut deferred.listeners),
        )
    };
    for handle in roots {
        ctx.remove_global_root(handle);
    }
    net_close_streams(&streams, true);
    net_close_listeners(&listeners, true);
    // Outside the deferred lock: a close is a syscall. Socket ids are
    // private, minted upward and never reused (`fd_table::os_value_socket`;
    // the JIT r23 wave 4 review reverted wave 4b's OS keys for sockets), so a
    // queued id cannot name a newer socket; an id `close()` already released
    // is a no-op. Should sockets ever take OS keys again, this stays sound
    // only because a queued id comes from a `closed == 0` row whose owner died
    // and `DatagramSocket.close` is that row's only closer -- or move it to
    // `close_if_generation` with a generation stored in the row.
    for fd in udp_fds {
        let _ = ctx.fd_table().close(fd as u32);
    }
}

/// Remove the given plain streams from `servlet::s2_registry` and shut them
/// down, as `re1_close_socket` does. With `wait == false` the registry lock is
/// only TRIED, and `false` is answered when it was busy (nothing closed).
pub fn net_close_streams(ids: &[(usize, i32)], wait: bool) -> bool {
    if ids.is_empty() {
        return true;
    }
    let mut reg = if wait {
        crate::servlet::s2_registry().lock()
    } else {
        match crate::servlet::s2_registry().try_lock() {
            Some(reg) => reg,
            None => return false,
        }
    };
    let closed: Vec<Arc<TcpStream>> = ids
        .iter()
        .filter_map(|(_, id)| reg.streams.remove(id))
        .collect();
    drop(reg);
    // Outside the registry lock: a shutdown is a syscall. Dropping the last
    // `Arc` closes the descriptor; the shutdown also wakes a straggler.
    for stream in closed {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    true
}

/// Remove the given listeners from `servlet::s2_registry` and close them
/// (gc-common w18-f): the listeners of plain `ServerSocket`s dropped without
/// `close()`, so their ports can be bound again. With `wait == false` the
/// registry lock is only TRIED, and `false` is answered when it was busy
/// (nothing closed).
///
/// A listener is removed only while its registry owner row still names the
/// queuing VM: ids are minted upward and a live listener's owner row is never
/// pruned, so this can only matter after an id wrap, and then it keeps a queued
/// id from closing another VM's listener. The listeners are dropped (closing
/// their descriptors) after the registry lock is released (lesson x).
pub fn net_close_listeners(ids: &[(usize, i32)], wait: bool) -> bool {
    if ids.is_empty() {
        return true;
    }
    let mut reg = if wait {
        crate::servlet::s2_registry().lock()
    } else {
        match crate::servlet::s2_registry().try_lock() {
            Some(reg) => reg,
            None => return false,
        }
    };
    let mut closed: Vec<TcpListener> = Vec::with_capacity(ids.len());
    for &(vm, id) in ids {
        if reg.owners.get(&id) == Some(&vm) {
            if let Some(listener) = reg.listeners.remove(&id) {
                closed.push(listener);
            }
        }
    }
    drop(reg);
    drop(closed);
    true
}

/// The `ObjectRef`-keyed half of [`gc_sweep_net_socket_rows`] (gc-common w17-a,
/// `common-w16a-inet-address-and-datagram-rows-root-their-keys`): the rows of
/// `ds_side_table`, `ds_peer_table` and `inet_addr_side_table` are weak on
/// their key, and this is what drops the row of a `DatagramSocket` or
/// `InetAddress` mirror the collection did not keep.
///
/// Keys are PRE-remap here, as every owner in this sweep is. A key in
/// `pointer_map` moved and is kept as it is: the `datagram-sockets` /
/// `inet-addresses` remap (`gc_update_ds_refs` / `gc_update_inet_addr_refs`,
/// run by `update_all_roots` after `process_references_after_gc`) re-keys it
/// afterwards. Any other key of `vm` is judged by `is_live`, with every table
/// lock released, and dropped when it is dead — unless its table was written
/// since the keys were collected (see [`DS_WRITE_EPOCH`]).
///
/// Only `vm`'s rows. Returns how many rows went, and the UDP descriptors of the
/// `DatagramSocket`s among them that were dropped OPEN (a `close()`d socket
/// already released its descriptor and records `fd = -1`); the caller queues
/// those for the VM's next [`net_release_deferred`].
///
/// A dropped row's `getPort()` / `getInetAddress()` answer is not lost: the
/// socket it answered for is unreachable, so nothing can ask. A CLOSED but
/// reachable socket keeps its rows, as the JDK requires.
pub fn sweep_object_keyed_rows(
    vm: usize,
    pointer_map: &cratonvm_types::PointerMap,
    is_live: &dyn Fn(usize) -> bool,
) -> (usize, Vec<i32>) {
    let unmoved = |k: &ObjectRef| !pointer_map.contains_key(&(k.as_ptr() as usize));

    // 1. Collect this VM's unmoved keys, one table at a time, with the
    // table's write epoch.
    let mut ds_cands: Vec<ObjectRef> = Vec::new();
    let ds_epoch = {
        let t = ds_side_table().lock();
        for (k, (owner, _)) in t.iter() {
            if *owner == vm && unmoved(k) {
                ds_cands.push(*k);
            }
        }
        DS_WRITE_EPOCH.load(Ordering::Relaxed)
    };
    let mut peer_cands: Vec<ObjectRef> = Vec::new();
    let peer_epoch = {
        let t = ds_peer_table().lock();
        for (k, (owner, _, _)) in t.iter() {
            if *owner == vm && unmoved(k) {
                peer_cands.push(*k);
            }
        }
        DS_PEER_WRITE_EPOCH.load(Ordering::Relaxed)
    };
    let mut inet_cands: Vec<ObjectRef> = Vec::new();
    let inet_epoch = {
        let t = inet_addr_side_table().lock();
        for (k, (owner, _, _)) in t.iter() {
            if *owner == vm && unmoved(k) {
                inet_cands.push(*k);
            }
        }
        INET_WRITE_EPOCH.load(Ordering::Relaxed)
    };

    // 2. Judge, with no lock held.
    let dead = |cands: Vec<ObjectRef>| -> Vec<ObjectRef> {
        cands
            .into_iter()
            .filter(|k| !is_live(k.as_ptr() as usize))
            .collect()
    };
    let dead_ds = dead(ds_cands);
    let dead_peers = dead(peer_cands);
    let dead_inets = dead(inet_cands);

    // 3. Drop, table by table, only if the table is exactly as collected.
    let mut dropped = 0usize;
    let mut open_fds: Vec<i32> = Vec::new();
    if !dead_ds.is_empty() {
        let mut t = ds_side_table().lock();
        if DS_WRITE_EPOCH.load(Ordering::Relaxed) == ds_epoch {
            for k in dead_ds {
                if let Some((_, side)) = t.remove(&k) {
                    if side.closed == 0 && side.fd >= 0 {
                        open_fds.push(side.fd);
                    }
                    dropped += 1;
                }
            }
        }
    }
    if !dead_peers.is_empty() {
        let mut t = ds_peer_table().lock();
        if DS_PEER_WRITE_EPOCH.load(Ordering::Relaxed) == peer_epoch {
            for k in dead_peers {
                if t.remove(&k).is_some() {
                    dropped += 1;
                }
            }
        }
    }
    if !dead_inets.is_empty() {
        let mut t = inet_addr_side_table().lock();
        if INET_WRITE_EPOCH.load(Ordering::Relaxed) == inet_epoch {
            for k in dead_inets {
                if t.remove(&k).is_some() {
                    dropped += 1;
                }
            }
        }
    }
    (dropped, open_fds)
}

/// The post-collection owner-death sweep of this file's socket rows
/// (gc-common w16-a), the `gc_sweep_io_side_tables` contract: owners are
/// PRE-remap addresses; an owner in `pointer_map` moved, and its row is
/// re-addressed; any other owner is judged by `is_live`, which is evaluated
/// with every table lock RELEASED. Only `vm`'s rows are judged. Returns how
/// many rows went.
///
/// * `sock_side_table` (`common-w15a-dropped-unclosed-socket-keeps-its-fd-and-rows`):
///   a row written through `sock_set_owned` goes with its object. A socket
///   whose stream is still live is kept (HotSpot's stream references its
///   socket), marked `SOCK_OWNER_GONE`, until that stream dies. A dead
///   socket that opened its own plain stream (`owns_stream`) has the stream
///   closed, which is what HotSpot's `SocketCleanable` does for a collected
///   `NioSocketImpl`, so the peer sees a FIN and the descriptor is returned.
/// * the stream -> owner rows of a dead socket stream;
/// * `ssc_carrier_roots` (`common-w15a-ssl-carriers-and-server-socket-delegates-rooted-until-teardown`):
///   the root of a dead `SSLContext`'s carrier is queued for release; then
///   the binding of a dead carrier, and every `ssc_side_table` row no binding
///   names;
/// * `sss_option_delegates`: the delegate root of an `SSLServerSocket` that
///   died open is queued for release;
/// * `re5_client_shutdown_table` (`common-w16a-closed-httpclient-rows-outlive-the-client`):
///   a dead `HttpClient`'s shutdown row;
/// * gc-common w17-a (`common-w16a-inet-address-and-datagram-rows-root-their-keys`):
///   the `ObjectRef`-keyed `DatagramSocket` and `InetAddress` rows of a dead
///   key (`sweep_object_keyed_rows`); a socket dropped open has its UDP
///   descriptor queued for closing;
/// * gc-common w17-a: a dead `HttpsURLConnection` carrier's recorded handshake,
///   and the root of the `SSLSession` it cached (`https_carrier_sessions`);
///   and the unclaimed `probe_restrictions` entry of a dead probe socket, with
///   the socket's row.
/// * gc-common w18-f (`common-w17a-dropped-open-server-socket-keeps-its-listener-and-row`):
///   a dead plain `ServerSocket`'s `ss_side_table` row (closed or not), and
///   its `server_socket_ports` row (`native-api`, which no longer roots the
///   socket). A listener it still held (dropped without `close()`) is removed
///   from `servlet::s2_registry` under `try_lock` and closed outside the lock,
///   or queued for the next sweep / `net_release_deferred` when the registry
///   is busy.
///
/// Queued roots and descriptors are released by the VM's next
/// `net_release_deferred`; a
/// stream is closed here unless the socket registry is busy, and then by the
/// next drain.
///
/// Since gc-common w28-a the `NativeObjKey` rows are keyed by weak lock keys,
/// and [`forget_net_obj_keys`] -- run by the lock-key sweep, which precedes
/// this one at every site -- has usually dropped a dead object's rows already
/// (and queued its stream, listener or roots, which step 4 below then closes).
/// This sweep stays for what the key sweep leaves: the `ObjectRef`-keyed rows,
/// the re-addressing of the owners, and a socket kept `SOCK_OWNER_GONE` until
/// its last stream dies.
///
/// Wired into the three weak-row sweep sites after the `com.sun.net.httpserver`
/// link rows (`handoff-w16a-net-socket-rows-sweep-call-sites`). ALL THREE
/// sites are needed together: the `process_references_after_gc` one is what
/// re-addresses a moved owner, and without it the other two would judge a
/// moved LIVE socket by its vacated address and close its connection.
pub fn gc_sweep_net_socket_rows(
    vm: usize,
    pointer_map: &cratonvm_types::PointerMap,
    is_live: &dyn Fn(usize) -> bool,
) -> usize {
    // 0. The object-keyed rows (gc-common w17-a): collected, judged and
    // dropped on their own, since their keys are the objects themselves.
    let (keyed_dropped, udp_fds) = sweep_object_keyed_rows(vm, pointer_map, is_live);
    // gc-common w18-f: the cross-crate bound-port rows are weak on their
    // `ServerSocket` too (they rooted it until then); same contract, their own
    // table and epoch.
    let ports_dropped =
        cratonvm_native_api::server_socket_ports::gc_sweep_rows(vm, pointer_map, is_live);

    // 1. Re-address moved owners and collect the rest, one table at a time.
    // gc-common w18-f: the plain `ServerSocket` rows.
    let mut ss_cands: Vec<(NativeObjKey, usize)> = Vec::new();
    for (k, row) in ss_side_table().lock().iter_mut() {
        if k.vm != vm || row.owner == 0 {
            continue;
        }
        match pointer_map.get(&row.owner) {
            Some(&to) => row.owner = to,
            None => ss_cands.push((*k, row.owner)),
        }
    }
    let mut sock_cands: Vec<(NativeObjKey, usize)> = Vec::new();
    let mut gone_socks: Vec<NativeObjKey> = Vec::new();
    for (k, row) in sock_side_table().lock().iter_mut() {
        if k.vm != vm || row.owner == 0 {
            continue;
        }
        if row.owner == SOCK_OWNER_GONE {
            gone_socks.push(*k);
            continue;
        }
        match pointer_map.get(&row.owner) {
            Some(&to) => row.owner = to,
            None => sock_cands.push((*k, row.owner)),
        }
    }
    let mut stream_cands: Vec<(NativeObjKey, usize)> = Vec::new();
    for (k, addr) in stream_owner_table().lock().addrs.iter_mut() {
        if k.vm != vm || *addr == 0 {
            continue;
        }
        match pointer_map.get(&*addr) {
            Some(&to) => *addr = to,
            None => stream_cands.push((*k, *addr)),
        }
    }
    let mut binding_cands: Vec<(NativeObjKey, usize)> = Vec::new();
    for (k, binding) in ssc_owner_table().lock().iter_mut() {
        if k.vm != vm {
            continue;
        }
        match pointer_map.get(&binding.carrier) {
            Some(&to) => binding.carrier = to,
            None => binding_cands.push((*k, binding.carrier)),
        }
    }
    let mut root_cands: Vec<(SscKey, usize)> = Vec::new();
    for (k, row) in ssc_carrier_roots().lock().iter_mut() {
        if (k.0).vm != vm {
            continue;
        }
        match pointer_map.get(&row.owner) {
            Some(&to) => row.owner = to,
            None => root_cands.push((*k, row.owner)),
        }
    }
    let mut delegate_cands: Vec<(NativeObjKey, usize)> = Vec::new();
    for (k, entry) in sss_option_delegates().lock().iter_mut() {
        if k.vm != vm || entry.owner == 0 {
            continue;
        }
        match pointer_map.get(&entry.owner) {
            Some(&to) => entry.owner = to,
            None => delegate_cands.push((*k, entry.owner)),
        }
    }
    let mut client_cands: Vec<(NativeObjKey, usize)> = Vec::new();
    for (k, owner) in re5_client_shutdown_table().lock().iter_mut() {
        if k.vm != vm || *owner == 0 {
            continue;
        }
        match pointer_map.get(&*owner) {
            Some(&to) => *owner = to,
            None => client_cands.push((*k, *owner)),
        }
    }
    let mut carrier_cands: Vec<(NativeObjKey, usize)> = Vec::new();
    for (k, session) in https_carrier_sessions().lock().iter_mut() {
        if k.vm != vm || session.owner == 0 {
            continue;
        }
        match pointer_map.get(&session.owner) {
            Some(&to) => session.owner = to,
            None => carrier_cands.push((*k, session.owner)),
        }
    }

    // 2. Judge, with no lock held.
    let dead = |cands: Vec<(NativeObjKey, usize)>| -> Vec<(NativeObjKey, usize)> {
        cands.into_iter().filter(|&(_, a)| !is_live(a)).collect()
    };
    let dead_socks = dead(sock_cands);
    let dead_server_socks = dead(ss_cands);
    let dead_streams = dead(stream_cands);
    let dead_bindings = dead(binding_cands);
    let dead_delegates = dead(delegate_cands);
    let dead_clients = dead(client_cands);
    let dead_carriers = dead(carrier_cands);
    let dead_roots: Vec<(SscKey, usize)> = root_cands
        .into_iter()
        .filter(|&(_, a)| !is_live(a))
        .collect();

    // 3. Drop what is still as judged. A row re-addressed or re-written in
    // between (a mutator, during an out-of-pause reclamation) is left alone.
    let mut dropped = keyed_dropped + ports_dropped;
    // gc-common w18-f: a dead plain `ServerSocket`'s row, closed or not (a
    // closed one's row used to stay, and a later socket with its identity hash
    // inherited `closed = 1` and threw "Socket is closed" on its first bind).
    // A listener it still holds (dropped without `close()`) is closed below.
    let mut listeners_to_close: Vec<(usize, i32)> = Vec::new();
    if !dead_server_socks.is_empty() {
        let mut t = ss_side_table().lock();
        for (k, addr) in dead_server_socks {
            if t.get(&k).is_some_and(|row| row.owner == addr) {
                if let Some(row) = t.remove(&k) {
                    if row.listener_id >= 0 {
                        listeners_to_close.push((vm, row.listener_id));
                    }
                    dropped += 1;
                }
            }
        }
    }
    let owners_with_streams: std::collections::HashSet<NativeObjKey> = {
        let mut guard = stream_owner_table().lock();
        let t = &mut *guard;
        for (k, addr) in dead_streams {
            if t.addrs.get(&k) != Some(&addr) {
                continue;
            }
            t.addrs.remove(&k);
            if let Some(owner) = t.by_stream.remove(&k) {
                if let Some(list) = t.by_owner.get_mut(&owner) {
                    list.retain(|s| *s != k);
                    if list.is_empty() {
                        t.by_owner.remove(&owner);
                    }
                }
            }
            dropped += 1;
        }
        t.by_owner.keys().filter(|k| k.vm == vm).copied().collect()
    };
    let mut to_close: Vec<(usize, i32)> = Vec::new();
    let mut dead_sock_keys: Vec<NativeObjKey> = Vec::new();
    {
        let mut t = sock_side_table().lock();
        let mut remove: Vec<NativeObjKey> = Vec::new();
        for (k, addr) in dead_socks {
            if let Some(row) = t.get_mut(&k) {
                if row.owner != addr {
                    continue;
                }
                if owners_with_streams.contains(&k) {
                    row.owner = SOCK_OWNER_GONE;
                } else {
                    remove.push(k);
                }
            }
        }
        for k in gone_socks {
            let still_gone = t.get(&k).is_some_and(|row| row.owner == SOCK_OWNER_GONE);
            if still_gone && !owners_with_streams.contains(&k) {
                remove.push(k);
            }
        }
        for k in remove {
            if let Some(row) = t.remove(&k) {
                if row.owns_stream && row.stream_id >= 0 {
                    to_close.push((vm, row.stream_id));
                }
                dead_sock_keys.push(k);
                dropped += 1;
            }
        }
    }
    // gc-common w17-a: the probe restrictions of a probe socket that died
    // unclaimed (`take_probe_restrictions` never ran for it) go with its row,
    // under their own lock, after the socket table's is released. Not counted:
    // they are part of the socket's state.
    if !dead_sock_keys.is_empty() {
        let mut probes = probe_restrictions().lock();
        for k in &dead_sock_keys {
            probes.remove(k);
        }
    }
    let mut released: Vec<usize> = Vec::new();
    {
        let mut t = ssc_carrier_roots().lock();
        for (k, owner) in dead_roots {
            if t.get(&k).is_some_and(|row| row.owner == owner) {
                if let Some(row) = t.remove(&k) {
                    released.push(row.handle);
                    dropped += 1;
                }
            }
        }
    }
    {
        // The side rows are retained under the bindings' lock, so a binding
        // `ssc_set` files meanwhile is seen before its side row can exist.
        let mut bindings = ssc_owner_table().lock();
        for (k, carrier) in dead_bindings {
            if bindings.get(&k).is_some_and(|b| b.carrier == carrier) {
                bindings.remove(&k);
                dropped += 1;
            }
        }
        let named: std::collections::HashSet<SscKey> = bindings
            .iter()
            .filter(|(k, _)| k.vm == vm)
            .map(|(_, b)| b.key)
            .collect();
        let mut sides = ssc_side_table().lock();
        let before = sides.len();
        sides.retain(|key, _| (key.0).vm != vm || named.contains(key));
        dropped += before - sides.len();
    }
    {
        let mut t = sss_option_delegates().lock();
        for (k, owner) in dead_delegates {
            if t.get(&k).is_some_and(|e| e.owner == owner) {
                if let Some(e) = t.remove(&k) {
                    released.push(e.root);
                    dropped += 1;
                }
            }
        }
    }
    {
        let mut t = re5_client_shutdown_table().lock();
        for (k, owner) in dead_clients {
            if t.get(&k) == Some(&owner) {
                t.remove(&k);
                dropped += 1;
            }
        }
    }
    {
        // A dead carrier's recorded handshake, and the `SSLSession` it cached
        // (the application may still hold that session; it keeps it alive
        // itself). `record_https_carrier_session` re-stamps the owner, so a
        // carrier recorded again meanwhile is left alone.
        let mut t = https_carrier_sessions().lock();
        for (k, owner) in dead_carriers {
            if t.get(&k).is_some_and(|s| s.owner == owner) {
                if let Some(s) = t.remove(&k) {
                    if s.session_root != 0 {
                        released.push(s.session_root);
                    }
                    dropped += 1;
                }
            }
        }
    }

    // 4. Queue what needs a `NativeContext`, and close the streams now if the
    // socket registry is free.
    if !released.is_empty() || !to_close.is_empty() || !udp_fds.is_empty() {
        let mut deferred = net_deferred().lock();
        if !released.is_empty() {
            deferred.roots.entry(vm).or_default().extend(released);
        }
        deferred.streams.extend(to_close);
        if !udp_fds.is_empty() {
            deferred.udp_fds.entry(vm).or_default().extend(udp_fds);
        }
    }
    let pending = std::mem::take(&mut net_deferred().lock().streams);
    if !net_close_streams(&pending, false) {
        net_deferred().lock().streams.extend(pending);
    }
    // gc-common w18-f: the listeners of dead `ServerSocket`s, and any an
    // earlier sweep found the registry too busy to close. Tried, never waited
    // for (this may run inside a pause); a busy registry defers them to the
    // next sweep or `net_release_deferred`.
    let mut listeners = std::mem::take(&mut net_deferred().lock().listeners);
    listeners.extend(listeners_to_close);
    if !net_close_listeners(&listeners, false) {
        net_deferred().lock().listeners.extend(listeners);
    }
    dropped
}

/// `javax.net.ssl.SSLSessionContext` cache tuning, as configured through
/// `setSessionCacheSize`/`setSessionTimeout`. Side-tabled for the same reason
/// as the socket state above, and one more: the carrier is an instance of the
/// real `SSLSessionContext`, which is an INTERFACE declaring zero fields, so
/// there are no instance slots to write at all.
///
/// The defaults used to be 0/0, on the reading that 0 is this API's spelling
/// of "unlimited" / "no expiry". MEASURED 2026-08-17 (probe `G16Ctx`, HotSpot
/// 25.0.3+9-LTS): a freshly `init`-ed `SSLContext` answers **20480** and
/// **86400** on both its client and its server context, on the first call,
/// before anything is configured. 0 is a value a caller can SET — and setting
/// it does read back as 0 on HotSpot, so the two are distinguishable states
/// and the old default was reporting the configured one as the initial one.
#[derive(Debug, Clone, Copy)]
pub struct SscSide {
    pub cache_size: i32,
    pub timeout_secs: i32,
}

/// `sun.security.ssl.SSLSessionContextImpl`'s initial cache bound. MEASURED,
/// not derived: it is `javax.net.ssl.sessionCacheSize`'s built-in value.
pub const SSC_DEFAULT_CACHE_SIZE: i32 = 20480;

/// The initial session lifetime in SECONDS (24 h). MEASURED alongside the
/// cache bound above.
pub const SSC_DEFAULT_TIMEOUT_SECS: i32 = 86400;

impl Default for SscSide {
    fn default() -> Self {
        SscSide {
            cache_size: SSC_DEFAULT_CACHE_SIZE,
            timeout_secs: SSC_DEFAULT_TIMEOUT_SECS,
        }
    }
}

/// Which logical session context a carrier stands for: the key of the owning
/// `SSLContext` plus a tag distinguishing its client context (0)
/// from its server context (1). A carrier that never came from
/// `get{Client,Server}SessionContext` keys off its own identity under tag 2,
/// so it still round-trips against itself and cannot collide with a real
/// SSLContext entry.
///
/// The owner half is a `NativeObjKey` (VM + identity hash) since gc-common
/// w9-b. It was the bare identity hash, so VM B's `SSLContext` read VM A's
/// session-cache tuning, and `ssc_carrier_roots` handed B a global-root
/// HANDLE minted in A, which B resolved in its own root table. Since
/// gc-common w28-a the `NativeObjKey` carries the object's weak lock key, not
/// its identity hash, so two live `SSLContext`s sharing a hash no longer share
/// tuning or a carrier.
pub type SscKey = (NativeObjKey, u8);

pub const SSC_TAG_CLIENT: u8 = 0;
pub const SSC_TAG_SERVER: u8 = 1;
pub const SSC_TAG_ORPHAN: u8 = 2;

/// `LockLevel::Scratch`, on the same reading. `ssc_key` — the only `ctx` call
/// on either path — runs BEFORE the guard is taken in both `ssc_get` and
/// `ssc_set`, and `ssc_set`'s four callers assign one `i32` field each.
pub fn ssc_side_table() -> &'static OrderedPlMutex<HashMap<SscKey, SscSide>> {
    static T: OnceLock<OrderedPlMutex<HashMap<SscKey, SscSide>>> = OnceLock::new();
    T.get_or_init(|| OrderedPlMutex::new(HashMap::new(), LockLevel::Scratch))
}

/// Carrier identity -> the `SscKey` it stands for. `get{Client,Server}
/// SessionContext` allocate a FRESH carrier on every call, so without this
/// indirection `ctx.getServerSessionContext().setSessionCacheSize(n)` followed
/// by a second `ctx.getServerSessionContext().getSessionCacheSize()` would
/// read a different object's (empty) entry — the exact set/get contradiction
/// this table exists to remove. Only plain `i32`s are stored, so the table
/// needs no GC roots and survives object relocation (a `NativeObjKey` is
/// stable across a move).
///
/// gc-common w16-a (`common-w15a-ssl-carriers-and-server-socket-delegates-
/// rooted-until-teardown`): each binding also records the carrier's address,
/// its weak owner. [`gc_sweep_net_socket_rows`] drops the binding once the
/// carrier is dead, and then every `ssc_side_table` row no binding names any
/// more. An orphan carrier's tuning (`SSC_TAG_ORPHAN`) is bound to the carrier
/// itself on its first write, so it lives exactly as long.
pub fn ssc_owner_table() -> &'static Mutex<HashMap<NativeObjKey, SscBinding>> {
    static T: OnceLock<Mutex<HashMap<NativeObjKey, SscBinding>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One `ssc_owner_table` row: the logical session context a carrier stands
/// for, and the carrier's current address (kept current by the sweep).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SscBinding {
    pub key: SscKey,
    pub carrier: usize,
}

/// `pub` for the session-accessor lanes outside this file: a carrier
/// minted anywhere else has to be bound to its owning `SSLContext` here, or
/// `ssc_key` files it under `SSC_TAG_ORPHAN` and it round-trips only against
/// itself.
///
/// `carrier` must be CURRENT (gc-common w16-a): its address becomes the
/// binding's weak owner.
///
/// `owner` must be current too: both keys are minted here (gc-common w28-a),
/// before the table guard.
pub fn ssc_bind(ctx: &dyn NativeContext, carrier: ObjectRef, owner: ObjectRef, tag: u8) {
    let carrier_id = mint_native_obj_key(ctx, carrier);
    let key = (mint_native_obj_key(ctx, owner), tag);
    ssc_owner_table().lock().insert(
        carrier_id,
        SscBinding {
            key,
            carrier: carrier.as_ptr() as usize,
        },
    );
}

/// The logical context `this` stands for, or `None` for a carrier never keyed
/// (gc-common w28-a: looked up, never minted), which has no tuning row.
pub fn ssc_key(ctx: &dyn NativeContext, this: ObjectRef) -> Option<SscKey> {
    let carrier_id = existing_native_obj_key(ctx, this)?;
    let bound = ssc_owner_table().lock().get(&carrier_id).map(|b| b.key);
    Some(bound.unwrap_or((carrier_id, SSC_TAG_ORPHAN)))
}

pub fn ssc_get(ctx: &dyn NativeContext, this: ObjectRef) -> SscSide {
    let Some(key) = ssc_key(ctx, this) else {
        return SscSide::default();
    };
    ssc_side_table()
        .lock()
        .get(&key)
        .copied()
        .unwrap_or_default()
}

/// `this` is the receiver of a setter native at its entry, i.e. current.
pub fn ssc_set<F: FnOnce(&mut SscSide)>(ctx: &dyn NativeContext, this: ObjectRef, f: F) {
    let carrier_id = mint_native_obj_key(ctx, this);
    let key = {
        let mut owners = ssc_owner_table().lock();
        let bound = owners.get(&carrier_id).map(|b| b.key);
        match bound {
            Some(key) => key,
            None => {
                // gc-common w16-a: an orphan's tuning is bound to the orphan
                // itself, so the sweep drops it with the carrier. It used to
                // stay until VM teardown.
                let key = (carrier_id, SSC_TAG_ORPHAN);
                owners.insert(
                    carrier_id,
                    SscBinding {
                        key,
                        carrier: this.as_ptr() as usize,
                    },
                );
                key
            }
        }
    };
    let mut t = ssc_side_table().lock();
    f(t.entry(key).or_default());
}

/// One `SscKey` -> the ONE carrier object that stands for it, as a global-root
/// handle.
///
/// MEASURED 2026-08-17 (probe `G16Ctx`, HotSpot 25.0.3+9-LTS):
///
/// ```text
/// ctx.getServerSessionContext() == ctx.getServerSessionContext()   ->  true
/// ```
///
/// Every call used to mint a FRESH carrier, so that row read `false` here.
/// `ssc_owner_table` already made the state round-trip across two different
/// carriers, which is why the values agreed while the identity did not — and
/// identity is not cosmetic on this API: a caller that caches the context and
/// later compares, or uses it as a map key, silently accumulates one entry per
/// call.
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Both acquisitions copy a
/// `usize` root handle in or out in a single statement; the
/// `ctx.resolve_global_root` calls that surround them run on the next
/// statement, with no guard held.
///
/// gc-common w16-a: each row also records the owning `SSLContext`'s address.
/// The root is what makes `getServerSessionContext()` answer the same object
/// every time, and that only matters while the context lives: once
/// [`gc_sweep_net_socket_rows`] finds the context dead it drops the row and
/// queues the root for release ([`net_release_deferred`]). The carrier then
/// lives only as long as the application holds it. It used to stay rooted,
/// with its rows, until VM teardown.
pub fn ssc_carrier_roots(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<SscKey, SscCarrierRoot>> {
    static T: OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<HashMap<SscKey, SscCarrierRoot>>,
    > = OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// One `ssc_carrier_roots` row: the carrier's global-root handle and the
/// owning `SSLContext`'s current address (kept current by the sweep).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SscCarrierRoot {
    pub handle: usize,
    pub owner: usize,
}

/// The carrier for `(owner, tag)`, minted once and handed back thereafter.
///
/// GC-safety: `try_alloc_concurrent_synthetic` can relocate `owner`, so the
/// caller pins it and this reads the forwarded address back before binding it
/// (`ssc_bind` mints keys at the address it is given). The carrier itself is
/// remembered as a root HANDLE, never a bare `ObjectRef`.
pub fn ssc_carrier(
    ctx: &mut dyn NativeContext,
    owner: ObjectRef,
    tag: u8,
) -> Result<ObjectRef, MethodCallFailed> {
    // gc-common w16-a: release the roots the last sweep queued for this VM.
    // `remove_global_root` allocates nothing, so `owner` stays current.
    net_release_deferred(ctx);
    // Minted (gc-common w28-a): the miss path files a row under it after the
    // allocation below, and a lock key follows `owner` across a move.
    let key = (mint_native_obj_key(&*ctx, owner), tag);
    let known = ssc_carrier_roots().lock().get(&key).copied();
    if let Some(row) = known {
        if let Some(carrier) = ctx.resolve_global_root(row.handle) {
            return Ok(carrier);
        }
    }
    let owner_pin = ctx.pin_native_root(owner);
    let carrier = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSessionContext", 0)?;
    let owner = ctx.read_native_pin(owner_pin, owner);
    ssc_bind(ctx, carrier, owner, tag);
    ctx.unpin_native_roots(owner_pin);
    let root = ctx.add_global_root(carrier);
    if root == 0 {
        // No root available: hand back the fresh carrier rather than fail. It
        // still round-trips through `ssc_owner_table`; only the identity row
        // degrades to the pre-2026-08-17 behaviour.
        return Ok(carrier);
    }
    ssc_carrier_roots().lock().insert(
        key,
        SscCarrierRoot {
            handle: root,
            owner: owner.as_ptr() as usize,
        },
    );
    Ok(ctx.resolve_global_root(root).unwrap_or(carrier))
}

pub fn ds_default() -> DsSide {
    DsSide {
        port: 0,
        closed: 0,
        timeout: 0,
        fd: -1,
        broadcast: -1,
        connected: 0,
        reuse_address: -1,
    }
}

pub fn ds_get(this: ObjectRef) -> DsSide {
    ds_side_table()
        .lock()
        .get(&this)
        .map(|&(_, side)| side)
        .unwrap_or_else(ds_default)
}

/// Update VM `vm`'s socket `this`, stamping `vm` as the row's owner (see
/// [`ds_side_table`]).
pub fn ds_set<F: FnOnce(&mut DsSide)>(vm: usize, this: ObjectRef, f: F) {
    let mut t = ds_side_table().lock();
    let entry = t.entry(this).or_insert_with(|| (vm, ds_default()));
    entry.0 = vm;
    f(&mut entry.1);
    DS_WRITE_EPOCH.fetch_add(1, Ordering::Relaxed);
}

// Map Socket$SocketInputStream / Socket$SocketOutputStream synthetic
// instance -> owner Socket. The real-JDK inner classes have their own
// fields (`parent`, `in`/`out`); we cannot use raw slot indices safely.
//
// GC-safety fix: both `stream` and `owner` are raw `ObjectRef`s that a moving
// GC can relocate at any point between `getInputStream()`/`getOutputStream()`
// (which populate this table) and a much-later `read`/`write` call (which
// looks it up) — `is`/`os` are freshly-allocated, short-lived objects, prime
// young-gen relocation candidates. Keying by the raw `stream` pointer went
// stale the moment `is`/`os` moved (lookup miss -> spurious "has no owner"),
// and returning the raw `owner` pointer went stale the moment the Socket
// moved (a dispatch against the relocated, now-reclaimed address -> "Stale
// pointer detected ... falling back to CP class java/net/Socket"). Both
// reproduced via org.apache.catalina.realm.TestJNDIRealmIntegration once the
// GC-barrier accept-loop fix let the test run long enough to trigger a GC
// mid-stream. Fix: key by `identity_hash_code` (GC-stable) and hold the value
// as a global GC root (remapped by the collector on every move), resolved
// fresh at each read — the same pattern `register_var_handle_root`/
// `read_var_handle_root` use for the identical hazard on VarHandle statics.
//
// gc-common w9-b: keyed by `NativeObjKey` (VM + identity hash). A global-root
// HANDLE is only meaningful in the VM that minted it, and the old identity-
// hash-only key let VM B's stream find VM A's handle and resolve it in B's own
// global-root table -- an unrelated object, or nothing, so `read` ran on the
// wrong socket (`common-w8a-identity-hash-keyed-registries-are-shared-across-vms`).
//
// gc-common w15-a (`common-w14a-socket-stream-owner-roots-keep-every-socket-
// alive`): the value is the owner's `NativeObjKey`, no longer a global-root
// handle to the owner. Every stream native only ever used the owner to reach
// its `sock_side_table` row, which is keyed by that same `NativeObjKey`, so
// the object itself was never needed -- and the root it was held by was
// released by nothing, not even VM teardown, so every `Socket` that ever
// handed out a stream stayed reachable, with everything it referenced, for
// the life of the VM. Rows are now dropped when the socket closes (through
// either stream or `Socket.close`, which all end in `re1_close_socket`), by
// way of the `by_owner` reverse index, so a server that closes its
// connections keeps no row per connection either.
//
// gc-common w28-a: both keys are weak lock keys now (see `NativeObjKey`), so
// two live streams -- or two live sockets -- sharing an identity hash no
// longer share a row, and a stream's or socket's rows go when its key is
// freed (`forget_net_obj_keys`).
#[derive(Default)]
pub struct StreamOwners {
    /// Stream -> the `Socket` it reads or writes.
    pub by_stream: HashMap<NativeObjKey, NativeObjKey>,
    /// `Socket` -> every stream `by_stream` maps to it, for the close-time
    /// release. A stream key whose `by_stream` row now names another owner
    /// (a stream re-registered under another socket) is skipped.
    pub by_owner: HashMap<NativeObjKey, Vec<NativeObjKey>>,
    /// Stream -> the stream object's address (gc-common w16-a), for the
    /// streams `getInputStream` / `getOutputStream` hand out. On HotSpot a
    /// `Socket$SocketInputStream` references its socket (`parent`), so a
    /// socket whose only remaining reference is one of its streams is still
    /// reachable; this VM's synthetic stream holds no such field, so
    /// [`gc_sweep_net_socket_rows`] keeps a socket's rows while any of its
    /// streams lives, and drops a dead stream's row here. A stream without an
    /// address here is never judged.
    pub addrs: HashMap<NativeObjKey, usize>,
}

pub fn stream_owner_table() -> &'static Mutex<StreamOwners> {
    static T: OnceLock<Mutex<StreamOwners>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(StreamOwners::default()))
}

/// [`stream_owner_set`], recording the stream object's CURRENT address
/// (`0` = none) as its weak owner (gc-common w16-a; see `StreamOwners::addrs`).
pub fn stream_owner_set_at(stream: NativeObjKey, owner: NativeObjKey, stream_addr: usize) {
    let mut t = stream_owner_table().lock();
    if stream_addr != 0 {
        t.addrs.insert(stream, stream_addr);
    } else {
        t.addrs.remove(&stream);
    }
    if let Some(previous) = t.by_stream.insert(stream, owner) {
        // The stream re-registered under another socket: unlink it from its
        // old owner so that owner's close does not drop this row.
        if previous != owner {
            if let Some(list) = t.by_owner.get_mut(&previous) {
                list.retain(|s| *s != stream);
                if list.is_empty() {
                    t.by_owner.remove(&previous);
                }
            }
        }
    }
    let list = t.by_owner.entry(owner).or_default();
    if !list.contains(&stream) {
        list.push(stream);
    }
}

/// The key of the `Socket` `stream` belongs to; `None` once that socket has
/// closed (or for a stream this file never handed out).
pub fn stream_owner_get(ctx: &dyn NativeContext, stream: ObjectRef) -> Option<NativeObjKey> {
    // gc-common w28-a: looked up, never minted; a stream never keyed was
    // never handed out by this file.
    let key = existing_native_obj_key(ctx, stream)?;
    stream_owner_table().lock().by_stream.get(&key).copied()
}

/// Drop every stream row of the `Socket` `owner`: it has closed.
pub fn stream_owners_forget_owner(owner: NativeObjKey) {
    let mut t = stream_owner_table().lock();
    let Some(streams) = t.by_owner.remove(&owner) else {
        return;
    };
    for s in streams {
        if t.by_stream.get(&s) == Some(&owner) {
            t.by_stream.remove(&s);
            t.addrs.remove(&s);
        }
    }
}

/// A read or write on a socket stream whose socket has closed: HotSpot's
/// `NioSocketImpl.ensureOpenAndConnected` answer.
pub fn re1_stream_owner_or_closed(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
) -> Result<NativeObjKey, MethodCallFailed> {
    match stream_owner_get(&*ctx, stream) {
        Some(owner) => Ok(owner),
        None => Err(re1_socket_exception(ctx, "Socket closed")),
    }
}

// InetAddress side-table — same rationale as the Socket / ServerSocket
// tables above. `java.net.InetAddress` is a real bootstrap class whose only
// instance fields are `holder` (a `java.net.InetAddress$InetAddressHolder`)
// and `holder6`/cached-lookup state — NOT plain `hostName` / `address`
// String fields. Writing the host/IP Strings straight into instance slots
// 0/1 puts a `String` in the `holder` slot; when a real-JDK `InetAddress`
// (or `Inet4Address`) method that we do not natively override runs, its
// bytecode does `getfield holder; invokevirtual InetAddressHolder.getXxx()`
// and the sub-`invokevirtual` retargets onto `java/lang/String`, raising a
// bogus `NoSuchMethodError java/lang/String.getHostName()`.
//
// Keeping host/IP in this ObjectRef-keyed table leaves the real-JDK
// instance slots at their zero-initialised (null) defaults, so any
// real-JDK InetAddress bytecode that does run sees the spec-correct
// "uninitialised holder" shape instead of a poisoned String.
//
// Values are `(owning vm_identity, hostName, ipAddress)`: the table is shared
// by every VM in the process, and only the owning VM's collections sweep and
// re-key a row (gc-common w8-a, row `inet-addresses`). The rows are WEAK since
// gc-common w17-a: nothing roots the key, and `sweep_object_keyed_rows` drops
// the row of a mirror a collection did not keep.
pub fn inet_addr_side_table() -> &'static Mutex<HashMap<ObjectRef, (usize, String, String)>> {
    static T: OnceLock<Mutex<HashMap<ObjectRef, (usize, String, String)>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record VM `vm`'s InetAddress `(hostName, ipAddress)` in the side table.
pub fn inet_addr_set(vm: usize, this: ObjectRef, host: &str, ip: &str) {
    let mut t = inet_addr_side_table().lock();
    t.insert(this, (vm, host.to_string(), ip.to_string()));
    INET_WRITE_EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// `pub` re-export of [`inet_addr_set`] for sibling modules that
/// allocate InetAddress mirrors (e.g. `inet_address.rs`'s real DNS resolver).
///
/// Prefer [`alloc_inet_address_external`] for fresh allocations — it also
/// populates the real-JDK `holder`. Use this bare setter only when the
/// `InetAddress` object already exists. The row belongs to `ctx`'s VM.
pub fn inet_addr_set_external(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    host: &str,
    ip: &str,
) {
    inet_addr_set(ctx.vm_identity(), this, host, ip);
}

/// `pub` allocation helper for sibling modules (`inet_address.rs`,
/// `phases_early.rs`, `phases_late.rs`) that need to mint an `InetAddress`
/// mirror. Routes through [`alloc_inet_address`] so every mirror gets BOTH
/// the side-table entry AND a real-JDK `InetAddress$InetAddressHolder` —
/// never a bare `String` written into the typed `holder` slot.
pub fn alloc_inet_address_external(
    ctx: &mut dyn NativeContext,
    host: &str,
    ip: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    Ok(alloc_inet_address(ctx, host, ip)?)
}

/// Mint an `InetAddress` mirror that remembers **no hostName**, the way the JDK
/// leaves `holder.hostName` null for an address nobody supplied a name for.
///
/// `InetAddress.toString()` is
/// `Objects.toString(holder().getHostName(), "") + "/" + getHostAddress()`, so
/// this is directly observable: HotSpot prints `/127.0.0.1` for
/// `getByName("127.0.0.1")` and `getByAddress(byte[])`, and for every address
/// the socket layer decodes from a peer's raw IP. CratonVM used to hand
/// `alloc_inet_address(ip, ip)` for all of those, which printed
/// `127.0.0.1/127.0.0.1`.
///
/// **This is NOT the same question as "does host equal ip".** The wildcard
/// `InetAddress.anyLocalAddress()` genuinely carries `hostName = "0.0.0.0"`
/// (HotSpot prints `0.0.0.0/0.0.0.0`), so a render-time `host == ip` test would
/// get that row backwards. The distinction is "was a name supplied", which is
/// only decidable HERE, at construction. Sites that want the named wildcard
/// keep calling [`alloc_inet_address_external`] with an explicit host.
pub fn alloc_inet_address_unnamed(
    ctx: &mut dyn NativeContext,
    ip: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    Ok(alloc_inet_address(ctx, NO_HOST_NAME, ip)?)
}

/// Mint a mirror for a RESOLVING entry point (`getByName`, `getAllByName`,
/// `InetSocketAddress(String,int)`, …), where whether a name exists is decided
/// by what the caller passed: a numeric literal is not a name, anything else is.
///
/// `getByName("0.0.0.0")` therefore yields an UNNAMED wildcard (HotSpot:
/// `/0.0.0.0`) even though `anyLocalAddress()` yields a named one — same IP,
/// opposite answer, and the reason this decision cannot be deferred to the
/// renderer.
pub fn alloc_inet_address_for_input(
    ctx: &mut dyn NativeContext,
    input: &str,
    ip: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    if host_input_is_numeric_literal(input) {
        alloc_inet_address(ctx, NO_HOST_NAME, ip)
    } else {
        alloc_inet_address(ctx, input, ip)
    }
}

/// True when `input` is an IP literal rather than a hostname, i.e. the JDK
/// would not have a name to remember. Accepts the bracketed IPv6 form
/// (`[::1]`) and a scoped literal (`fe80::1%3`), both of which
/// `getByName` takes and neither of which parses bare.
pub fn host_input_is_numeric_literal(input: &str) -> bool {
    let bare = input
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(input);
    let unscoped = bare.split('%').next().unwrap_or(bare);
    if unscoped.is_empty() {
        return false;
    }
    // `Ipv4Addr::from_str` is STRICTER than the JDK: it takes four decimal
    // octets and nothing else, so `1.2.3`, `1.2`, `16909060` and `01.2.3.4`
    // all failed here and were remembered as host NAMES — HotSpot prints
    // `/1.2.0.3` for `getByName("1.2.3")` and this VM printed `1.2.3/1.2.0.3`.
    jdk_numeric_format_v4(unscoped).is_some() || unscoped.parse::<IpAddr>().is_ok()
}

/// `sun.net.util.IPAddressUtil.textToNumericFormatV4`, which is the JDK's own
/// answer to "is this text an IPv4 literal".
///
/// It is neither `Ipv4Addr::from_str` (too strict: no 1-, 2- or 3-part forms)
/// nor `inet_aton` (too lax: hex and octal). The rules, all four of them:
///
/// * at most 15 characters, and only decimal digits and `.`;
/// * one to four parts, none empty;
/// * every part but the last is an octet (`< 256`);
/// * the last part fills the REMAINING bytes, so `1.2.3` is `1.2.0.3` and
///   `16909060` is `1.2.3.4`.
///
/// A leading zero is not octal here — `01.2.3.4` is `1.2.3.4` — because the
/// digits are accumulated base 10. That is measured, not assumed: HotSpot
/// 25.0.4+7 answers `/1.2.3.4` for it.
pub fn jdk_numeric_format_v4(src: &str) -> Option<Ipv4Addr> {
    if src.is_empty() || src.len() > 15 {
        return None;
    }
    let mut res = [0u8; 4];
    let mut tmp: u64 = 0;
    let mut cur = 0usize;
    let mut new_octet = true;
    for c in src.chars() {
        if c == '.' {
            if new_octet || tmp > 0xff || cur == 3 {
                return None;
            }
            res[cur] = (tmp & 0xff) as u8;
            cur += 1;
            tmp = 0;
            new_octet = true;
        } else {
            let digit = c.to_digit(10)?;
            tmp = tmp * 10 + u64::from(digit);
            new_octet = false;
        }
    }
    if new_octet || tmp >= (1u64 << ((4 - cur) * 8)) {
        return None;
    }
    match cur {
        0 => res = (tmp as u32).to_be_bytes(),
        1 => {
            let b = (tmp as u32).to_be_bytes();
            res[1] = b[1];
            res[2] = b[2];
            res[3] = b[3];
        }
        2 => {
            let b = (tmp as u32).to_be_bytes();
            res[2] = b[2];
            res[3] = b[3];
        }
        _ => res[3] = (tmp & 0xff) as u8,
    }
    Some(Ipv4Addr::from(res))
}

/// True for the text `inet_aton(3)` accepts and the JDK deliberately does not:
/// hexadecimal (`0x7f.0.0.1`) and octal (`0177.0.0.1`) parts.
///
/// This matters because the fallback for "not a literal" is a NAME lookup, and
/// `getaddrinfo` runs `inet_aton` first — so without this check the C library
/// resolves `0x7f.0.0.1` to 127.0.0.1 and the VM answers where HotSpot raises.
/// The JDK raises `UnknownHostException` carrying the BARE host text (no
/// resolver suffix), which is how this case is told apart from a DNS miss.
pub fn bsd_parsable_v4(src: &str) -> bool {
    if src.is_empty() {
        return false;
    }
    let parts: Vec<&str> = src.split('.').collect();
    if parts.len() > 4 {
        return false;
    }
    let mut saw_alternate_radix = false;
    for part in &parts {
        let (digits, radix) =
            if let Some(hex) = part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
                saw_alternate_radix = true;
                (hex, 16)
            } else if part.len() > 1 && part.starts_with('0') {
                saw_alternate_radix = true;
                (&part[1..], 8)
            } else {
                (*part, 10)
            };
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
            return false;
        }
    }
    saw_alternate_radix
}

/// The JDK's `UnknownHostException` text for a failed NAME lookup is
/// `"<host>: <resolver message>"`. Rust's `io::Error` for the same call reads
/// `"failed to lookup address information: Name or service not known"`, so the
/// VM printed a message with an extra clause in the middle of it. The resolver
/// text is the part after the last `": "`.
pub fn resolver_message(err: &std::io::Error) -> String {
    let text = err.to_string();
    match text.rfind(": ") {
        Some(i) => text[i + 2..].to_string(),
        None => text,
    }
}

/// `getByName`'s literal screen, shared by every resolving entry point.
///
/// Returns `Ok(Some(ip))` for a literal, `Ok(None)` when the text is a name to
/// look up, and `Err` when the JDK rejects it as a malformed literal WITHOUT
/// consulting the resolver — the two cases being a BSD-only IPv4 form and
/// anything carrying a colon that is not a valid IPv6 literal.
pub fn literal_screen(host: &str) -> Result<Option<IpAddr>, MethodCallFailed> {
    if let Some(v4) = jdk_numeric_format_v4(host) {
        return Ok(Some(IpAddr::V4(v4)));
    }
    let bracketed = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(v6) = bracketed.parse::<Ipv6Addr>() {
        return Ok(Some(IpAddr::V6(v6)));
    }
    if bsd_parsable_v4(host) {
        return Err(uhex(host.to_string()));
    }
    // A colon cannot appear in a host NAME, so the JDK never falls through to
    // the resolver for one: `1::2::3` and `1:2:3:4:5:6:7` are malformed
    // literals, not names that happen not to resolve.
    if host.contains(':') && host_input_scope(host).is_none() {
        return Err(uhex(format!("{host}: invalid IPv6 address literal")));
    }
    Ok(None)
}

/// The `%scope` of a textual address literal, if it carries one.
///
/// `InetAddress.getByName("fe80::1%1")` and `getByName("fe80::1%eth0")` both
/// produce an `Inet6Address` whose `getScopeId()`/`getHostAddress()` remember
/// the scope on HotSpot. [`resolve_host`] answers with a bare
/// `std::net::IpAddr`, which cannot carry one, so the scope must be taken from
/// the INPUT text and threaded to the allocator separately.
pub fn host_input_scope(input: &str) -> Option<&str> {
    let bare = input
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(input);
    match bare.split_once('%') {
        Some((_, scope)) if !scope.is_empty() => Some(scope),
        _ => None,
    }
}

/// Split a possibly-scoped numeric address text into `(address, scope)`.
///
/// The `InetAddress` side table stores the scope glued onto the address text
/// (`fe80:0:0:0:0:0:0:1%eth0`) because it is the only per-mirror store that
/// already follows the mirror across a move (remapped; weak since gc-common
/// w17-a, so it also goes with the mirror). Every consumer of the ADDRESS goes
/// through [`inet_addr_resolve`], which splits here and hands back the bare
/// half, so the storage choice is invisible to them: `getAddress()`, `equals`,
/// `hashCode` and the `native-io` bind/connect decoders keep parsing a plain
/// `IpAddr`. Only [`inet6_scope_suffix`] reads the other half.
pub fn split_addr_scope(text: &str) -> (&str, Option<&str>) {
    match text.split_once('%') {
        Some((addr, scope)) if !scope.is_empty() => (addr, Some(scope)),
        _ => (text, None),
    }
}

/// `pub` re-export of [`resolve_host`] for sibling modules that need
/// to resolve a hostname/literal to an IP string without duplicating the
/// IPv4/IPv6-literal-then-DNS-fallback logic (used by `phases_early.rs`'s
/// synthetic `InetSocketAddress(String,int)` constructor — see its call site
/// for why an unconditionally-unresolved address is wrong there).
pub fn resolve_host_external(host: &str) -> Option<String> {
    resolve_host(host).ok().map(|ip| ip.to_string())
}

/// `pub` re-export of [`inet_addr_resolve`] so sibling modules can
/// render an `InetAddress` mirror the way HotSpot's `InetAddress.toString()`
/// does (`hostName + "/" + hostAddress`) without duplicating the
/// side-table-then-`holder` lookup order.
pub fn inet_addr_resolve_external(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<(String, String)> {
    inet_addr_resolve(ctx, this)
}

/// [`inet_addr_get`] for a socket-layer reader that has only ever seen
/// CratonVM-minted mirrors: the side table alone under `--compatible`
/// (unchanged), [`inet_addr_resolve`] under `--real-jdk`.
///
/// SBR wave 2 (lane `netaddr`) retires the `InetAddress` factories and
/// accessors under `--real-jdk`, so an address reaching a socket native there
/// may be one the JDK's own bytecode built (`getByName("230.0.0.1")` is
/// `new Inet4Address(null, bytes)`): no side-table row, and slot 1 is not a
/// String. The side-table-only readers fell through to "invalid address".
/// `inet_addr_resolve` consults the side table FIRST, so a mirror answers
/// exactly as before.
pub fn inet_addr_get_for_socket(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<(String, String)> {
    if ctx.is_jdk_only() {
        inet_addr_resolve(ctx, this)
    } else {
        inet_addr_get(this)
    }
}

/// Read an InetAddress's `(hostName, ipAddress)` from the side table.
/// Returns `None` for an InetAddress we never recorded.
///
/// `pub` so the duplicate InetAddress natives registered in
/// `phases_early.rs` can consult the same table — otherwise they would read
/// the (now intentionally unpopulated) instance slots and return null.
pub fn inet_addr_get(this: ObjectRef) -> Option<(String, String)> {
    // The stored address text may carry an IPv6 `%scope` (see
    // [`split_addr_scope`]). Strip it here so EVERY consumer of this
    // accessor — `net_channels`' channel decoders, `plain_socket`'s connect
    // target, `InetSocketAddress.getHostString()` — keeps receiving the bare
    // numeric form it parses today. Only [`inet6_scope_suffix`] reads the raw
    // entry.
    let (host, ip) = inet_addr_get_raw(this)?;
    let (bare, _) = split_addr_scope(&ip);
    Some((host, bare.to_string()))
}

/// The side-table entry exactly as stored, scope suffix included.
pub fn inet_addr_get_raw(this: ObjectRef) -> Option<(String, String)> {
    inet_addr_side_table()
        .lock()
        .get(&this)
        .map(|(_, host, ip)| (host.clone(), ip.clone()))
}

/// GC root scan for [`inet_addr_side_table`]. Same stale-pointer hazard this
/// pattern already fixes elsewhere in this file (`gc_scan_re10_handler_roots`)
/// and in `lib.rs` (`gc_scan_locale_roots`): the table is a process-global
/// `HashMap<ObjectRef, _>` keyed by the synthetic mirror's identity, invisible
/// to every other scan, so a moving young GC that relocates a live
/// `InetAddress` mirror leaves the table keyed on a vacated from-space slot —
/// `getHostAddress()`/`getAddress()`/`toString()` then silently miss the table
/// and fall through to the real-JDK `holder` fallback in [`inet_addr_resolve`],
/// which reports `0.0.0.0` instead of the mirror's real address (ES
/// `InetAddressRandomBinaryDocValuesRangeQueryTests` CONTAINS-query false
/// negative). Remap companion: [`gc_update_inet_addr_refs`].
///
/// Roots NOTHING since gc-common w17-a
/// (`common-w16a-inet-address-and-datagram-rows-root-their-keys`). Pushing
/// every key of the collecting VM kept every `InetAddress` mirror this VM ever
/// minted alive until teardown (one per `getByName`, per
/// `Socket.getInetAddress`, ...), where HotSpot collects each once it is
/// unreachable. The rows are WEAK now: [`gc_sweep_net_socket_rows`] (its
/// `sweep_object_keyed_rows` pass) drops the row of a mirror the collection
/// did not keep, before any allocation can reuse its address, and the remap
/// above still re-keys the survivors. The scan stays, empty, so the
/// `inet-addresses` row in `native_roots.rs` keeps its shape for the remap.
pub fn gc_scan_inet_addr_roots(_vm: usize, _out: &mut Vec<ObjectRef>) {}

/// Companion to [`gc_scan_inet_addr_roots`]: after a moving collection,
/// re-key the side table so lookups keyed on the OLD `ObjectRef` still
/// resolve — the mirror's identity is now the relocated address. Only `vm`'s
/// rows, all taken out before any is put back (gc-common w8-a).
pub fn gc_update_inet_addr_refs(vm: usize, pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() {
        return;
    }
    let mut map = inet_addr_side_table().lock();
    let mine: Vec<ObjectRef> = map
        .iter()
        .filter(|(_, (owner, _, _))| *owner == vm)
        .map(|(k, _)| *k)
        .collect();
    let drained: Vec<(ObjectRef, (usize, String, String))> = mine
        .into_iter()
        .filter_map(|k| map.remove(&k).map(|v| (k, v)))
        .collect();
    for (k, v) in drained {
        let nk = pointer_map
            .get(&(k.as_ptr() as usize))
            .map(|&n| unsafe { ObjectRef::from_raw(n as *mut u8) })
            .unwrap_or(k);
        map.insert(nk, v);
    }
}

/// Read an InetAddress's `(hostName, ipAddress)` resolving through every
/// known layout: ObjectRef-keyed side table first, then the real-JDK
/// `holder`/`holder6` reference fields (`InetAddress$InetAddressHolder.hostName`
/// + `.address`/`.family`, `Inet6Address$Inet6AddressHolder.ipaddress`), then
/// `None`.
///
/// This is the layout-aware reader the report calls for: an `InetAddress`
/// that was allocated by real-JDK `<init>` (not by `alloc_inet_address`) —
/// e.g. via the un-overridden `InetAddress.getByAddress(String, byte[])`
/// two-arg factory — still resolves correctly because `populate_inet_holder`
/// mirrors host/IP into the real `holder`/`holder6`. `pub` so sibling
/// modules' duplicate InetAddress natives consult the same path.
pub fn inet_addr_resolve(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<(String, String)> {
    if let Some(pair) = inet_addr_get(this) {
        return Some(pair);
    }
    // Real-JDK `holder` path. `InetAddress$InetAddressHolder` carries
    // `String hostName`, `int address`, `int family`.
    if let Value::Object(Some(holder)) = ctx.get_field_by_name(this, "holder") {
        let host = match ctx.get_field_by_name(holder, "hostName") {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        };
        // A real-JDK `Inet6Address` stores its 16-byte address in a SEPARATE
        // `holder6` field (`Inet6Address$Inet6AddressHolder.ipaddress`); the
        // base `holder.address` int is always 0 for v6 (see
        // `populate_inet_holder`). Without checking `holder6` first, any
        // Inet6Address that reaches this fallback reads `address == 0` and
        // reports "0.0.0.0" instead of its real 16-byte value.
        let ip = match ctx.get_field_by_name(this, "holder6") {
            Value::Object(Some(holder6)) => match ctx.get_field_by_name(holder6, "ipaddress") {
                Value::Object(Some(arr)) if ctx.array_length(arr) == 16 => {
                    let mut octets = [0u8; 16];
                    for (i, o) in octets.iter_mut().enumerate() {
                        *o = match ctx.get_array_element(arr, i) {
                            Value::Int(b) => (b & 0xff) as u8,
                            _ => 0,
                        };
                    }
                    // NOT `hotspot_ip_string`: that folds a v4-mapped address
                    // to its dotted quad, which is right for `getByName` and
                    // `InetAddress.getByAddress` (both hand back an
                    // `Inet4Address`) and WRONG here. This branch only runs
                    // for an object the real JDK already built as an
                    // `Inet6Address` — `Inet6Address.getByAddress(String,
                    // byte[], int)` is not intercepted — and the JDK
                    // specifies that factory to keep the 16-byte form.
                    // Folding it left `getAddress()` handing back 4 bytes for
                    // an `instanceof Inet6Address` receiver.
                    Some(ipv6_uncompressed_text(&std::net::Ipv6Addr::from(octets)))
                }
                _ => None,
            },
            _ => match ctx.get_field_by_name(holder, "address") {
                Value::Int(packed) => {
                    // `InetAddressHolder.address` is the IPv4 address packed
                    // big-endian into an int (Inet4Address layout).
                    let b = (packed as u32).to_be_bytes();
                    Some(std::net::Ipv4Addr::new(b[0], b[1], b[2], b[3]).to_string())
                }
                _ => None,
            },
        };
        if host.is_some() || ip.is_some() {
            return Some((host.unwrap_or_default(), ip.unwrap_or_default()));
        }
    }
    None
}

/// IPv4 / IPv6 family discriminant matching real-JDK
/// `InetAddress.IPv4 == 1` / `InetAddress.IPv6 == 2`.
pub const IA_FAMILY_V4: i32 = 1;
pub const IA_FAMILY_V6: i32 = 2;

/// Populate a real-JDK `InetAddress$InetAddressHolder` into the `holder`
/// reference field of a CratonVM-synthesised `InetAddress` mirror.
///
/// The side table remains the source of truth for the natives we override,
/// but un-overridden real-JDK `InetAddress` / `Inet4Address` bytecode reads
/// state straight out of `this.holder` — e.g. the `final` accessor
/// `InetAddress.getHostName()` is `holder().getHostName()`. Leaving `holder`
/// null makes that bytecode NPE (or dispatch a method against a null
/// receiver, surfacing as the bogus `NoSuchMethodError
/// java/lang/Object.toLowerCase` Hazelcast's `DefaultAddressPicker` trips
/// when it calls `inetAddress.getHostName().toLowerCase(Locale)`).
///
/// Mirrors the `alloc_inet_socket_address` holder-population pattern.
pub fn populate_inet_holder(
    ctx: &mut dyn NativeContext,
    ia: ObjectRef,
    host: &str,
    ip: &str,
    // NOT named `scope`: the local `NativeHandleScope` below owns that name.
    scope_arg: Option<&str>,
) -> Result<ObjectRef, MethodCallFailed> {
    // Cross-call GC-safety (2026-08-04). Every `alloc_concurrent_synthetic` /
    // `create_string` / `new_array` below ALLOCATES, and the first one is
    // especially dangerous: on a cold VM it also loads and initialises
    // `java/net/InetAddress$InetAddressHolder`, which runs a lot of Java and
    // therefore reliably triggers a moving young collection. Holding `ia` (and
    // `holder`) as bare `ObjectRef` locals across that meant `holder` was
    // written into a VACATED from-space copy of the address mirror, and the
    // caller was handed the stale `ia` too.
    //
    // Downstream that reads as the defect this function's callers were filed
    // for: `new ServerSocket(0)` → `new InetSocketAddress(null, 0)` stored the
    // stale mirror in the `InetSocketAddress` holder, `getAddress()` then
    // answered **null** while `isUnresolved()` still answered false, and
    // `sun.nio.ch.Net.bind`'s first act — `addr.isLinkLocalAddress()` — threw
    // `NullPointerException: … because "addr" is null`. See
    // `serversocket-bind-null-inetaddress-net-sockets-FIXED.md`.
    //
    // Returns the CURRENT (post-GC) address of `ia` so the caller propagates
    // the live reference instead of its own stale copy.
    let mut scope = NativeHandleScope::new(ctx);
    let ia_h = scope.root(ia);
    // Only populate if the class actually declares a `holder` field — i.e.
    // a real-JDK `InetAddress` is loaded. With a purely synthetic stub the
    // field is absent and `set_field_by_name` is a harmless no-op anyway.
    let holder =
        try_alloc_concurrent_synthetic(&mut *scope, "java/net/InetAddress$InetAddressHolder", 3)?;
    let holder_h = scope.root(holder);
    // An absent hostName must be a genuine `null`, not an empty String: the
    // real-JDK readers test the field for null, not for emptiness.
    // `InetAddress.toString()` is
    // `Objects.toString(holder().getHostName(), "") + "/" + getHostAddress()`
    // — an empty String is non-null and renders identically, but
    // `InetSocketAddressHolder.getHostString()` is
    // `addr.holder().getHostName() != null ? … : addr.getHostAddress()`, and an
    // empty String there would make `getHostString()` answer "" instead of the
    // IP. See `NO_HOST_NAME`.
    let host_val = if host.is_empty() {
        Value::Object(None)
    } else {
        let host_str = scope.create_string(host);
        Value::Object(Some(host_str))
    };
    let holder_cur = scope.get(&holder_h);
    scope.set_field_by_name(holder_cur, "hostName", host_val);
    // SBR wave 2 (lane `netaddr`): `originalHostName` too, under `--real-jdk`.
    // The JDK writes the pair together everywhere a name is supplied
    // (`InetAddressHolder(String, int, int)` and `init` in Java,
    // `setInetAddress_hostName` in `net_util.c` for the addresses its natives
    // mint), and the retired `InetAddress$1.getOriginalHostName` reads it
    // straight off the holder: left null, a mirror named `localhost` answered
    // `null` where HotSpot answers `localhost` (TLS takes its SNI host from it).
    // `--compatible` keeps the bridge, which never read the holder.
    if scope.is_jdk_only() {
        let holder_cur = scope.get(&holder_h);
        scope.set_field_by_name(holder_cur, "originalHostName", host_val);
    }
    // `address` is the IPv4 address packed big-endian into an int; for IPv6
    // it stays 0 (the bytes live in the separate `Inet6Address` holder).
    let parsed = ip.parse::<std::net::IpAddr>();
    let (packed, family) = match parsed {
        Ok(std::net::IpAddr::V4(v4)) => (i32::from_be_bytes(v4.octets()), IA_FAMILY_V4),
        Ok(std::net::IpAddr::V6(_)) => (0, IA_FAMILY_V6),
        Err(_) => (0, IA_FAMILY_V4),
    };
    let holder_cur = scope.get(&holder_h);
    scope.set_field_by_name(holder_cur, "address", Value::Int(packed));
    let holder_cur = scope.get(&holder_h);
    scope.set_field_by_name(holder_cur, "family", Value::Int(family));
    let ia_cur = scope.get(&ia_h);
    let holder_cur = scope.get(&holder_h);
    scope.set_field_by_name(ia_cur, "holder", Value::Object(Some(holder_cur)));

    // NIO-SERVER-SOCKET (IPv6): a real-JDK `Inet6Address` stores its 16-byte
    // address in a SEPARATE `holder6` field
    // (`Inet6Address$Inet6AddressHolder { byte[16] ipaddress; int scope_id; …}`),
    // NOT in the base `holder` (whose `address` int is 0 for v6). Un-overridden
    // real-JDK Inet6Address bytecode — `isLinkLocalAddress()`, `getScopeId()`,
    // and the address checks `NioSocketImpl.bind`/`connect` run on the real
    // socket path — dereferences `holder6`; leaving it null NPEs before bind0
    // is ever reached. Populate it so the real path resolves v6 correctly.
    if let Ok(std::net::IpAddr::V6(v6)) = parsed {
        let h6 = try_alloc_concurrent_synthetic(
            &mut *scope,
            "java/net/Inet6Address$Inet6AddressHolder",
            5,
        )?;
        let h6_h = scope.root(h6);
        let octets = v6.octets();
        let arr = scope.new_array(ArrayElementType::Byte, octets.len());
        let arr_h = scope.root(arr);
        for (i, b) in octets.iter().enumerate() {
            let arr_cur = scope.get(&arr_h);
            scope.set_array_element(arr_cur, i, Value::Int(*b as i32));
        }
        let h6_cur = scope.get(&h6_h);
        let arr_cur = scope.get(&arr_h);
        scope.set_field_by_name(h6_cur, "ipaddress", Value::Object(Some(arr_cur)));
        // Loopback / global addresses carry no scope. When the caller DID
        // supply one it must reach `holder6`, because `Inet6Address
        // .getScopeId()` is un-intercepted real-JDK bytecode reading exactly
        // this field: leaving it 0 made `getByName("fe80::1%1").getScopeId()`
        // answer 0 where HotSpot answers 1. An interface NAME is resolved to
        // its kernel index the way the JDK's own `Inet6Address(String, byte[],
        // NetworkInterface)` ctor does; `scope_ifname` itself stays null
        // because minting a `NetworkInterface` here would recurse through
        // `re8_make_interface`, which allocates `InetAddress`es of its own.
        // The rendered `%eth0` form comes from the side table instead — see
        // [`inet6_scope_suffix`].
        let scope_id = scope_arg
            .and_then(|s| s.parse::<i32>().ok().or_else(|| Some(re8_iface_index(s))))
            .filter(|id| *id > 0);
        let (id, id_set) = match scope_id {
            Some(id) => (id, 1),
            // A `%name` we could not resolve still means "scoped": the flag
            // drives HotSpot's `%` rendering, and the side table supplies the
            // text, so dropping it here would silently unscope the address.
            None if scope_arg.is_some() => (0, 1),
            None => (0, 0),
        };
        let h6_cur = scope.get(&h6_h);
        scope.set_field_by_name(h6_cur, "scope_id", Value::Int(id));
        let h6_cur = scope.get(&h6_h);
        scope.set_field_by_name(h6_cur, "scope_id_set", Value::Int(id_set));
        let ia_cur = scope.get(&ia_h);
        let h6_cur = scope.get(&h6_h);
        scope.set_field_by_name(ia_cur, "holder6", Value::Object(Some(h6_cur)));
    }
    Ok(scope.get(&ia_h))
}

/// Read one logical InetAddress field (`IA_HOST` or `IA_ADDR`) — side table
/// first, then the real-JDK `holder` reference field, finally falling back
/// to the legacy synthetic instance slot for any InetAddress object not
/// built by `alloc_inet_address`.
pub fn inet_addr_field(ctx: &mut dyn NativeContext, this: ObjectRef, which: usize) -> Value {
    if let Some((host, ip)) = inet_addr_resolve(ctx, this) {
        let s = if which == IA_HOST { host } else { ip };
        return Value::Object(Some(ctx.create_string(&s)));
    }
    ctx.get_field(this, which)
}

/// The `%…` suffix HotSpot's `Inet6Address$Inet6AddressHolder.getHostAddress()`
/// appends, or `None` for an address that carries no scope.
///
/// The JDK's own body is
///
/// ```java
/// String s = numericToTextFormat(ipaddress);
/// if (scope_ifname != null) s = s + "%" + scope_ifname.getName();
/// else if (scope_id_set)    s = s + "%" + scope_id;
/// ```
///
/// — note the ORDER (a named interface wins over the numeric id) and note that
/// the numeric branch is gated on `scope_id_set`, **not** on `scope_id != 0`.
/// `Inet6AddressHolder.init` sets the flag for any `scope_id >= 0`, so
/// `Inet6Address.getByAddress(host, addr, 0)` — exactly what
/// `io.netty.channel.unix.NativeInetAddress.address()` calls for a link-local
/// peer — must render `%0`, while `getByAddress(host, addr, -1)` must render no
/// suffix at all. A `scope_id != 0` test gets that pair backwards in both
/// directions.
///
/// Only the SUFFIX is produced here; the numeric text itself stays in the
/// side table / `holder6` unscoped, because every other reader of it
/// (`getAddress()`, `equals`, `hashCode`, `isLoopbackAddress`, and the
/// `native-io` bind/connect decoders) parses it as a bare `IpAddr` and a
/// `%scope` suffix would make that parse fail.
pub fn inet6_scope_suffix(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<String> {
    // A CratonVM-minted mirror carries its scope in the side table, glued to
    // the address text: it is built from a textual literal (`getByName
    // ("fe80::1%eth0")`) or from `getifaddrs`, neither of which produces the
    // `NetworkInterface` object `holder6.scope_ifname` would need — and
    // building one here would recurse, since `re8_make_interface` mints
    // `InetAddress`es of its own.
    if let Some((_, ip)) = inet_addr_get_raw(this) {
        if let (_, Some(scope)) = split_addr_scope(&ip) {
            return Some(scope.to_string());
        }
    }
    let holder6 = match ctx.get_field_by_name(this, "holder6") {
        Value::Object(Some(h)) => h,
        _ => return None,
    };
    // A named scope interface wins, exactly as the JDK orders it. Read the
    // `name` FIELD rather than calling `getName()`: every `NetworkInterface`
    // this VM hands out is a REAL-layout object built through the JDK's own
    // `(String,int,InetAddress[])` constructor (see `re8_make_interface`), so
    // the field is there — and an `invoke_virtual` here would run Java, hence
    // possibly a moving collection, in the middle of a caller that is holding
    // a Java `String` reference for the base address text.
    if let Value::Object(Some(nif)) = ctx.get_field_by_name(holder6, "scope_ifname") {
        if let Value::Object(Some(s)) = ctx.get_field_by_name(nif, "name") {
            if let Some(name) = ctx.read_string(s) {
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
    }
    match ctx.get_field_by_name(holder6, "scope_id_set") {
        Value::Int(set) if set != 0 => match ctx.get_field_by_name(holder6, "scope_id") {
            Value::Int(id) => Some(id.to_string()),
            _ => None,
        },
        _ => None,
    }
}

/// The numeric text of an InetAddress mirror WITH the IPv6 scope suffix —
/// i.e. what `getHostAddress()` must answer.
///
/// Every public renderer goes through here: `getHostAddress()` on all three
/// registered classes, the `getHostName()` no-name fallback (HotSpot reaches
/// the same place by failing a reverse lookup and returning `getHostAddress()`)
/// and `toString()`'s right-hand half. They previously each read `IA_ADDR`
/// directly and so each dropped the scope independently.
///
/// `pub` for the `phases_early.rs` duplicates of the same three natives.
pub fn inet_addr_scoped_text(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    default: &str,
) -> String {
    let ip = inet_addr_field_string_or(ctx, this, IA_ADDR, default);
    match inet6_scope_suffix(ctx, this) {
        // `contains('%')` guards the (currently unreachable) case of a mirror
        // whose stored text is already scoped — never append twice.
        Some(scope) if !ip.is_empty() && !ip.contains('%') => format!("{ip}%{scope}"),
        _ => ip,
    }
}

/// `getHostAddress()` for every InetAddress class we register it on.
///
/// `pub` because the duplicate registration in `phases_early.rs` must
/// answer identically — `getHostName` had six registrations and the winning
/// pair was on the concrete subclasses (see
/// `inetaddress-tostring-hostname-literal-addresses-FIXED.md`),
/// so a scope fix applied to only one of them is invisible half the time.
pub fn inet_addr_host_address_value(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    // `inet_addr_field` keeps the legacy fallback: an address object we never
    // recorded and that has no `holder` still answers from its raw slot.
    let base = inet_addr_field(ctx, this, IA_ADDR);
    let Some(scope) = inet6_scope_suffix(ctx, this) else {
        return base;
    };
    let Value::Object(Some(s)) = base else {
        return base;
    };
    match ctx.read_string(s) {
        // `contains('%')` never appends twice.
        Some(text) if !text.is_empty() && !text.contains('%') => {
            Value::Object(Some(ctx.create_string(&format!("{text}%{scope}"))))
        }
        _ => base,
    }
}

/// `InetAddress.getHostName()` / `getCanonicalHostName()`.
///
/// A mirror that carries a name answers with it verbatim. One that does NOT
/// (see [`NO_HOST_NAME`]) must fall back to the numeric text — reading the
/// empty host straight out of the side table would answer `""`, and this
/// method is declared to return a hostname, never nothing. HotSpot reaches the
/// same place by a different route: it attempts a reverse lookup and returns
/// `getHostAddress()` when there is no PTR record.
///
/// **Deliberately does not cache.** HotSpot writes its reverse-lookup answer
/// back into `holder.hostName`, so a later `toString()` on the same object
/// prints the discovered name. Replicating that here would write the *IP* into
/// the holder and flip `toString()` from `/127.0.0.1` to `127.0.0.1/127.0.0.1`
/// for any address some internal caller happened to ask the name of —
/// reintroducing the very divergence this file was corrected for, at an
/// unpredictable moment. A stable `toString()` is worth the one lost mutation;
/// the difference is recorded in
/// `inetaddress-tostring-hostname-literal-addresses-FIXED.md`.
pub fn inet_addr_host_name_value(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    let name = inet_addr_field_string_or(ctx, this, IA_HOST, "");
    if !name.is_empty() {
        return Value::Object(Some(ctx.create_string(&name)));
    }
    // The no-name fallback is `getHostAddress()`, scope suffix and all —
    // `io.netty.channel.unix.NativeInetAddressTest.testLinkOnlyAddressIncludeScopeId`
    // asserts exactly this through `getHostName()`, not through
    // `getHostAddress()`.
    let ip = inet_addr_scoped_text(ctx, this, "");
    Value::Object(Some(ctx.create_string(&ip)))
}

/// String form of one logical InetAddress field, with a default for the
/// missing/empty case. Mirrors `read_field_string_or` but consults the
/// side table and the real-JDK `holder` first.
pub fn inet_addr_field_string_or(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    which: usize,
    default: &str,
) -> String {
    if let Some((host, ip)) = inet_addr_resolve(ctx, this) {
        let s = if which == IA_HOST { host } else { ip };
        return if s.is_empty() { default.to_string() } else { s };
    }
    read_field_string_or(ctx, this, which, default)
}

pub const SEL_OPEN: usize = 0;

pub const HS_ADDRESS: usize = 0;
pub const HS_STARTED: usize = 1;
pub const HS_CONTEXTS: usize = 2;
pub const HS_SERVER_ID: usize = 3;
pub const HS_PORT: usize = 4;
pub const HS_IMPL_CLASS: &str = "sun/net/httpserver/HttpServerImpl";
/// Executor supplied through `HttpServer.setExecutor`. It is retained so the
/// public `getExecutor` contract is coherent even though the native server's
/// VM dispatcher owns the actual request-draining threads.
pub const HS_EXECUTOR: usize = 5;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn ioex<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::IOException {
        message: message.into(),
    }
    .into()
}

/// A real, catchable `java.net.SocketException`.
///
/// `RuntimeError` has no `SocketException` variant, and the distinction is not
/// cosmetic: the JDK throws this concrete subtype for every "socket is closed"
/// / "already bound" / "not bound" refusal, and callers catch it by type
/// (okhttp's `MockWebServer` accept loop, Tomcat's aborted-upload swallow).
/// A bare `IOException` whose message merely reads like one escapes that catch.
///
/// The freshly built exception is pinned before it is handed back: the caller's
/// Java frame has no catch-local root for it yet, and `new_object_initialized`
/// has already released its constructor pin.
pub fn socket_ex<S: AsRef<str>>(
    ctx: &mut dyn NativeContext,
    message: S,
) -> cratonvm_types::error::MethodCallFailed {
    let jmsg = ctx.create_string(message.as_ref());
    cratonvm_native_api::throwable_builder::throw_new(
        ctx,
        "java/net/SocketException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(jmsg))],
        || ioex(message.as_ref().to_string()),
    )
}

/// Classify a UDP `recv` failure the way the JDK does: an expired `SO_TIMEOUT`
/// (`WSAETIMEDOUT` on Windows, `EAGAIN`/`EWOULDBLOCK` on Unix) is
/// `java.net.SocketTimeoutException`, everything else a plain IOException.
/// Polling receivers distinguish the two — see `native-io::net::udp_recv_error`
/// for the Tribes membership case a bare IOException broke.
pub fn udp_recv_ex(e: std::io::Error) -> cratonvm_types::error::MethodCallFailed {
    if matches!(
        e.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) {
        return RuntimeError::SocketTimeoutException {
            message: "Receive timed out".into(),
        }
        .into();
    }
    ioex(format!("UDP recv: {e}"))
}

/// Throw the concrete `java.net.UnknownHostException` (a subclass of
/// IOException). Code that catches `UnknownHostException` specifically (e.g.
/// Tomcat `NetMask`) misses a bare IOException, so host-resolution failures
/// must use this rather than `ioex("UnknownHostException: ...")`.
pub fn uhex<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::UnknownHostException {
        message: message.into(),
    }
    .into()
}
/// A missing jar/zip entry must surface as `java.io.FileNotFoundException`
/// (a subclass of `IOException`), matching the real JDK's
/// `JarURLConnection.getInputStream()` contract. Callers such as SmallRye
/// Config rely on `catch (FileNotFoundException)` to *skip* an absent
/// profile-specific resource; a plain `IOException` is rethrown and aborts boot.
pub fn fnfex<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::FileNotFoundException {
        path: message.into(),
    }
    .into()
}
/// Map a `zip::ZipArchive::by_name` failure to the right Java exception:
/// `ZipError::FileNotFound` → `FileNotFoundException`, anything else → `IOException`.
pub fn zip_entry_err(
    entry: &str,
    container: &str,
    e: zip::result::ZipError,
) -> cratonvm_types::error::MethodCallFailed {
    match e {
        zip::result::ZipError::FileNotFound => fnfex(format!(
            "entry {entry} in {container}: specified file not found in archive"
        )),
        other => ioex(format!(
            "URL.openStream: entry {entry} in {container}: {other}"
        )),
    }
}
/// Step aside for a user subclass of `java.net.URLConnection`.
pub fn npe<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::NullPointerException {
        message: Some(message.into()),
    }
    .into()
}
pub fn iae<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: message.into(),
    }
    .into()
}

pub fn value_to_string(ctx: &dyn NativeContext, v: Value) -> Option<String> {
    match v {
        Value::Object(Some(o)) => ctx.read_string(o),
        _ => None,
    }
}
pub fn read_field_string(ctx: &dyn NativeContext, obj: ObjectRef, field: usize) -> Option<String> {
    value_to_string(ctx, ctx.get_field(obj, field))
}
pub fn read_field_string_or(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    field: usize,
    default: &str,
) -> String {
    read_field_string(ctx, obj, field).unwrap_or_else(|| default.to_string())
}
pub fn value_or_string(ctx: &dyn NativeContext, v: Value, default: &str) -> String {
    value_to_string(ctx, v).unwrap_or_else(|| default.to_string())
}

/// The `URLStreamHandler` in `url.handler` when it is *application-provided*:
/// non-null and not one of the JDK's built-in `sun.net.*` handlers.
///
/// The built-in ones are excluded because their schemes (file:, jar:, http:,
/// …) are resolved by the dedicated arms of `openStream`/`openConnection`, and
/// routing them back through the real handler would re-enter the very natives
/// this is a fallback for.
pub fn url_app_handler(ctx: &dyn NativeContext, url: ObjectRef) -> Option<ObjectRef> {
    let handler = match ctx.get_field_by_name(url, "handler") {
        Value::Object(Some(h)) => h,
        _ => return None,
    };
    let hclass = ctx
        .class_name_of_id(ctx.class_id_of_object(handler))
        .unwrap_or_default();
    (!hclass.starts_with("sun/net/")).then_some(handler)
}

/// The `jar:file:` spelling the `URL.openStream` text reader resolves, for the
/// part of a Spring Boot `jar:nested:` URL after its scheme (`rest`).
///
/// Spring Boot writes `jar:nested:/outer.jar/!<entry>!/<resource>`. The
/// generic spelling of that is `jar:file:/outer.jar!/<entry>!/<resource>`,
/// which the reader opens as a jar nested in a jar. But in the `nested:`
/// grammar `<entry>` may also be a directory of the outer jar -- Spring Boot's
/// own `ZipContent` serves a directory entry as a virtual archive of what lies
/// under it -- and `BOOT-INF/classes/`, where every application class of a fat
/// jar lives, is one. Opening it as a jar failed with "invalid Zip archive:
/// Could not find EOCD". A directory entry name ends with `/` (zip), so that
/// case becomes the single-level `jar:file:/outer.jar!/<entry><resource>`,
/// the same bytes Spring Boot's `JarUrlConnection` reads.
/// `interpreter-L6-compatible-fat-jar-reads-boot-inf-classes-as-a-nested-jar-FIXED-20261013.md`.
pub fn nested_url_as_jar_file(rest: &str) -> String {
    let generic = rest.replacen("/!", "!/", 1);
    if let Some(sep) = generic.find("!/") {
        let after = &generic[sep + 2..];
        if let Some(sep2) = after.find("!/") {
            let entry = &after[..sep2];
            if entry.ends_with('/') {
                return format!(
                    "jar:file:{}!/{}{}",
                    &generic[..sep],
                    entry,
                    &after[sep2 + 2..]
                );
            }
        }
    }
    format!("jar:file:{generic}")
}

/// A `file:` URL that carries a handler, as every URL the JDK's constructor
/// builds does. The `openConnection` native answers such a URL with the JDK's
/// own `FileURLConnection`.
pub fn url_is_handled_file(ctx: &mut dyn NativeContext, url: ObjectRef) -> bool {
    if !matches!(
        ctx.get_field_by_name(url, "handler"),
        Value::Object(Some(_))
    ) {
        return false;
    }
    match ctx.get_field_by_name(url, "protocol") {
        Value::Object(Some(p)) => ctx.read_string(p).is_some_and(|p| p == "file"),
        _ => false,
    }
}

/// The JDK's own `jar` handler of a `jar:file:` URL it can open itself, where
/// the `JdkJarUrlConnections` row is in force over the JDK's URL loaders
/// (interpreter round i2 wave 57, lane L5): the URL's `URL.openConnection`
/// is that handler's `JarURLConnection` and its `openStream` the JDK body,
/// as on HotSpot. Only a single-level `jar:file:<archive>!/<entry>` whose
/// archive is not a `.jmod`: the nested `!/…!/` and jmod spellings are the
/// VM's own extensions (`openStream`'s text reader serves them), which the
/// JDK's handler refuses. `None` otherwise, including a URL without the
/// handler (one the VM synthesised) and an application handler's URL (the
/// callers' own arm).
pub fn jdk_jar_file_handler(ctx: &mut dyn NativeContext, url: ObjectRef) -> Option<ObjectRef> {
    if !ctx.jdk_url_class_loaders()
        || !ctx.aligned(cratonvm_types::HotSpotAlignment::JdkJarUrlConnections)
    {
        return None;
    }
    jdk_jar_file_handler_of(ctx, url)
}

#[inline(never)]
pub fn jdk_jar_file_handler_of(ctx: &mut dyn NativeContext, url: ObjectRef) -> Option<ObjectRef> {
    let handler = match ctx.get_field_by_name(url, "handler") {
        Value::Object(Some(h)) => h,
        _ => return None,
    };
    if ctx
        .class_name_of_id(ctx.class_id_of_object(handler))
        .as_deref()
        != Some("sun/net/www/protocol/jar/Handler")
    {
        return None;
    }
    let file = match ctx.get_field_by_name(url, "file") {
        Value::Object(Some(f)) => ctx.read_string(f)?,
        _ => return None,
    };
    let (archive, entry) = file.strip_prefix("file:")?.split_once("!/")?;
    if entry.contains("!/") || archive.ends_with(".jmod") {
        return None;
    }
    Some(handler)
}

/// The final `java.net.JarURLConnection.getJarFileURL()` / `getEntryName()`
/// are `return jarFileURL;` / `return entryName;`, fields its constructor
/// parsed from the URL. The natives standing for them exist for the VM's
/// carrier (allocated with the abstract class's name, never constructed),
/// but they also answered a CONSTRUCTED connection -- the JDK's
/// `sun.net.www.protocol.jar.JarURLConnection`, an application subclass --
/// in both modes, a fresh `URL` per call where HotSpot answers the same one
/// (interpreter round i2 wave 57, lane L5; probe `L5W57JarResourceStreams`,
/// `jarfileurl`). Such a receiver answers its field.
pub fn constructed_jar_connection_field(
    ctx: &dyn NativeContext,
    conn: ObjectRef,
    field: &str,
) -> Option<Value> {
    let class = ctx.class_name_of_id(ctx.class_id_of_object(conn))?;
    if class == "java/net/JarURLConnection" {
        return None;
    }
    // The field `java.net.JarURLConnection` DECLARES, not the first one of
    // that name up the receiver's chain: Spring Boot's `JarUrlConnection`
    // declares its own `private final String entryName`, which is still null
    // when its constructor runs `this.entryName = getEntryName()`. Read by
    // name, that shadowing field answered null, so every nested-jar entry
    // connection read as its jar's root and a class's bytes were the nested
    // archive's (`ClassFormatError: Incompatible magic value 1347093252`,
    // `--real-jdk`; `Malformed \uxxxx encoding` in `--compatible`):
    // interpreter round i2 wave 58, lane L5, the wave-57 regression of the
    // Spring Boot fat jar
    // (`docs/feature-designs/class-resolution-and-caching.md`).
    let slot = ctx.resolve_field_index("java/net/JarURLConnection", field)?;
    Some(ctx.get_field(conn, slot))
}

/// Is `conn` a CONSTRUCTED `JarURLConnection` (an application subclass, or
/// the JDK's own `sun.net.www.protocol.jar.JarURLConnection`) rather than the
/// VM's carrier, which is allocated under the abstract class's own name? The
/// `getJarEntry()` / `getContentLength()` / `getContentLengthLong()` stand-ins
/// registered on `java.net.JarURLConnection` for the carrier then run the JDK
/// bodies -- `getJarFile().getJarEntry(entryName)`, `URLConnection`'s header
/// read -- for a subclass that does not override them, as on HotSpot; they
/// answered it from its URL (interpreter round i2 wave 58, lane L5; probe
/// `L5W58JarConnectionSubclass`).
pub fn is_constructed_jar_connection(ctx: &dyn NativeContext, conn: ObjectRef) -> bool {
    ctx.class_name_of_id(ctx.class_id_of_object(conn))
        .is_some_and(|class| class != "java/net/JarURLConnection")
}

/// If `url` carries a non-null, *application-provided* `URLStreamHandler`,
/// invoke its `openConnection(URL)` and return the resulting `URLConnection`.
///
/// This mirrors the real `java.net.URL.openConnection()` = `handler
/// .openConnection(this)` for schemes CratonVM does not resolve natively. The
/// canonical user is ShrinkWrap's in-memory `archive:` handler
/// (`ShrinkWrapClassLoader`), whose connection reads resources straight out of a
/// heap-resident `JavaArchive` — there is no `file:`/`jar:` path to fall back
/// to. `URL`s that CratonVM synthesises (`build_synthetic_url`,
/// `alloc_concurrent_synthetic`) never populate `handler`, and JDK built-in
/// handlers live under `sun.net.www.protocol.*`; both are excluded so this only
/// fires for genuinely app-supplied handlers.
///
/// Returns:
///   * `Ok(Some(conn))` — a custom handler is present; `conn` is its
///     `URLConnection` value (possibly `Value::Object(None)` if the handler
///     itself returned null).
///   * `Ok(None)` — no custom handler; the caller should use its own
///     (string-based) resolution path.
///   * `Err(e)` — the handler threw (propagate; e.g. `FileNotFoundException`).
pub fn url_custom_handler_connection(
    ctx: &mut dyn NativeContext,
    url: ObjectRef,
) -> Result<Option<Value>, cratonvm_types::error::MethodCallFailed> {
    let Some(handler) = url_app_handler(ctx, url) else {
        return Ok(None);
    };
    // `handler.openConnection(url)` — `invoke_virtual` prepends the receiver, so
    // `args` is just the URL. A null/absent handler method or a thrown
    // exception both propagate to the caller via `?`.
    let conn = ctx.invoke_virtual(
        handler,
        "openConnection",
        "(Ljava/net/URL;)Ljava/net/URLConnection;",
        &[Value::Object(Some(url))],
    )?;
    Ok(Some(conn.unwrap_or(Value::Object(None))))
}

/// Resolve the jar-file component of a `jar:...!/entry` URL when that
/// component is itself a Tomcat `war:file:<war-path>*/<entry-in-war>`
/// reference (e.g. `jar:war:file:/x.war*/WEB-INF/lib/test.jar!/entry` — a jar
/// packaged inside a WAR). Returns the nested jar's raw bytes read out of the
/// enclosing WAR's zip central directory, or `None` if `jar_raw` isn't a
/// `war:` reference (the plain on-disk-file case is handled separately by
/// callers). Mirrors `UriUtil.warToJar`'s `*/` → `!/` translation, but reads
/// the referenced entry's bytes (via the existing nested-jar cache) instead
/// of just rewriting the URL string, since the entry lives inside another
/// archive rather than directly on disk.
pub fn war_nested_jar_bytes(jar_raw: &str) -> Option<Arc<Vec<u8>>> {
    let rest = jar_raw.strip_prefix("war:")?.trim_start_matches("file:");
    let mut wparts = rest.splitn(2, "*/");
    let war_path_raw = wparts.next()?;
    let inner_entry = wparts.next()?;
    if inner_entry.is_empty() {
        return None;
    }
    // `file:` URLs prefix a leading `/` before a Windows drive letter
    // (`/C:/…`); try the trimmed form first, then the raw form for POSIX.
    let trimmed = war_path_raw.trim_start_matches('/');
    let war_disk = if std::path::Path::new(trimmed).exists() {
        trimmed.to_string()
    } else if std::path::Path::new(war_path_raw).exists() {
        war_path_raw.to_string()
    } else {
        trimmed.to_string()
    };
    cached_nested_jar(&war_disk, inner_entry).ok()
}

/// Build a `java/util/jar/JarEntry` from an already-opened zip archive, or
/// `Value::Object(None)` if `lookup` isn't present. Shared by the plain
/// on-disk and WAR-nested-jar lookup paths in `jar_url_lookup_entry`.
pub fn jar_entry_value_from_archive<R: std::io::Read + std::io::Seek>(
    ctx: &mut dyn NativeContext,
    archive: &mut zip::ZipArchive<R>,
    lookup: &str,
) -> Result<Value, MethodCallFailed> {
    let (name, size, csize, method) = match archive.by_name(lookup) {
        Ok(entry) => {
            let name = entry
                .name()
                .map_err(|e| RuntimeError::IOException {
                    message: e.to_string(),
                })?
                .into_owned();
            let size = entry.size() as i64;
            let csize = entry.compressed_size() as i64;
            #[allow(deprecated)]
            let method = entry.compression().to_u16() as i32;
            (name, size, csize, method)
        }
        Err(_) => return Ok(Value::Object(None)),
    };
    // gc-common w20-d: the name first, rooted across the entry's allocation
    // (the entry was held raw across the String's).
    let name_s = ctx.create_string(&name);
    let mut scope = NativeHandleScope::new(ctx);
    let name_h = scope.root(name_s);
    let je = try_alloc_concurrent_synthetic(&mut *scope, "java/util/jar/JarEntry", 4)?;
    let name_s = scope.get(&name_h);
    let ctx = &mut *scope;
    ctx.set_field(je, 0, Value::Object(Some(name_s)));
    ctx.set_field(je, 1, Value::Long(size));
    ctx.set_field(je, 2, Value::Long(csize));
    ctx.set_field(je, 3, Value::Int(method));
    Ok(Value::Object(Some(je)))
}

/// Parse a `jar:[file:]<path>!/<entry>` external form and return the entry's
/// uncompressed size from the zip central directory, or `None` if the URL is
/// not a resolvable jar-entry URL. Used by `JarURLConnection.getContentLength*`.
///
/// `<path>` is usually a plain on-disk jar/zip file, but Tomcat's `war:`
/// nested-archive scheme (see `war_nested_jar_bytes`) produces
/// `war:file:<war-path>*/<entry-in-war>` here instead — a jar packaged inside
/// a WAR (e.g. `jar:war:file:/x.war*/WEB-INF/lib/test.jar!/META-INF/…`, from
/// `WarURLConnection` wrapping the jar it points into). Handle that case by
/// reading the nested jar's bytes out of the WAR first, then treating those
/// bytes as the archive to look the entry up in.
pub fn jar_url_entry_size(ext: &str) -> Option<i64> {
    let after = ext
        .strip_prefix("jar:file:")
        .or_else(|| ext.strip_prefix("jar:"))?;
    let mut parts = after.splitn(2, "!/");
    let jar_raw_enc = parts.next()?.trim_start_matches("file:");
    let entry_name = parts.next()?;
    if entry_name.is_empty() {
        return None;
    }
    // Percent-decode the jar's own file path — see the matching fix in
    // URL.openStream's jar:file: handler (TestDeployTask.bug58086a) for why
    // a `%20` must resolve back to a literal space before touching disk.
    let jar_raw_owned = uri_percent_decode(jar_raw_enc);
    let jar_raw = jar_raw_owned.as_str();
    if let Some(bytes) = war_nested_jar_bytes(jar_raw) {
        let cursor = std::io::Cursor::new(bytes.as_slice());
        let mut archive = zip::ZipArchive::new(cursor).ok()?;
        let entry = archive.by_name(entry_name).ok()?;
        return Some(entry.size() as i64);
    }
    // `file:` URLs prefix a leading `/` before a Windows drive letter
    // (`/C:/…`); try the trimmed form first, then the raw form for POSIX.
    let trimmed = jar_raw.trim_start_matches('/');
    let disk = if std::path::Path::new(trimmed).exists() {
        trimmed.to_string()
    } else if std::path::Path::new(jar_raw).exists() {
        jar_raw.to_string()
    } else {
        trimmed.to_string()
    };
    let file = std::fs::File::open(&disk).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let lookup = jmod_zip_entry_name(&disk, entry_name);
    let entry = archive.by_name(&lookup).ok()?;
    Some(entry.size() as i64)
}

/// Recover the originating `jar:…!/entry` external form from a synthetic
/// `JarURLConnection` (URL stored in field `HUC_URL`).
pub fn jar_url_conn_ext(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    let url_obj = match ctx.get_field(this, HUC_URL) {
        Value::Object(Some(o)) => o,
        _ => return String::new(),
    };
    let mut ext = read_field_string_or(ctx, url_obj, 5, "");
    if !ext.starts_with("jar:") {
        if let Ok(Some(Value::Object(Some(o)))) =
            ctx.invoke_virtual(url_obj, "toExternalForm", "()Ljava/lang/String;", &[])
        {
            ext = ctx.read_string(o).unwrap_or_default();
        }
    }
    ext
}

pub fn file_url_path(url: &str) -> Option<String> {
    let raw = url.strip_prefix("file:")?;
    let without_host = raw.strip_prefix("//").unwrap_or(raw);
    let path = if cfg!(windows) {
        without_host.trim_start_matches('/').to_string()
    } else {
        without_host.to_string()
    };
    Some(path)
}

/// Carrier class `URL.openConnection()` hands out for `jrt:` URLs -- the same
/// class the real JDK's jrt protocol handler returns, so callers that test
/// `instanceof HttpURLConnection` (Spring's `AbstractFileResolvingResource`)
/// correctly see a plain `URLConnection`.
pub const JRT_URL_CONNECTION: &str = "sun/net/www/protocol/jrt/JavaRuntimeURLConnection";

pub fn synthetic_resource_url_content_len(ctx: &mut dyn NativeContext, url: &str) -> i64 {
    if let Some(path) = file_url_path(url) {
        return std::fs::metadata(path)
            .map(|m| m.len() as i64)
            .unwrap_or(-1);
    }

    let resource = if let Some(rest) = url.strip_prefix("jrt:") {
        let path = rest.trim_start_matches('/');
        match path.split_once('/') {
            Some((_module, entry)) => entry,
            None => path,
        }
    } else if let Some(rest) = url
        .strip_prefix("classpath:")
        .or_else(|| url.strip_prefix("resource:"))
    {
        rest.trim_start_matches('/')    } else {
        return -1;
    };

    ctx.find_resource(resource)
        .map(|bytes| bytes.len() as i64)
        .unwrap_or(-1)
}

pub fn synthetic_resource_url_last_modified(url: &str) -> i64 {
    let Some(path) = file_url_path(url) else {
        return 0;
    };
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    let Ok(modified) = meta.modified() else {
        return 0;
    };
    match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_millis().min(i64::MAX as u128) as i64,
        Err(_) => 0,
    }
}

/// `JarURLConnection.getJarEntry()` — build the `java/util/jar/JarEntry` for the
/// entry named in the `jar:…!/entry` URL by reading the zip central directory.
/// Returns `Value::Object(None)` when the URL has no entry or the jar/entry is
/// missing. Spring's `AbstractFileResolvingResource.checkReadable()` reads this
/// (then `JarEntry.isDirectory()`) for jar resources — not `getContentLength`.
/// `.jmod` archives store their class/resource entries under a `classes/`
/// prefix, but a `getResource` URL into a jmod omits it (e.g.
/// `…/java.base.jmod!/java/lang/Object.class`). Map the requested entry to its
/// real on-disk zip-entry name, mirroring `URL.openStream` (line ~3931) and
/// `find_resource`. Non-jmod jars and already-prefixed names pass through.
///
/// Without this, `JarURLConnection.getJarEntry()`/`getContentLength()` on a
/// jmod URL (the form CratonVM hands back from `getResource` for a JDK
/// runtime class — HotSpot uses `jrt:` instead) found nothing, so Spring's
/// `AbstractFileResolvingResource.checkReadable()` jar branch
/// (`getJarEntry() != null`) reported a perfectly readable JDK class resource
/// as NOT readable (ModuleResourceTests.existingClassFileResource —
/// `ClassPathResource("java/beans/Introspector.class").isReadable()`).
pub fn jmod_zip_entry_name<'a>(disk: &str, entry: &'a str) -> std::borrow::Cow<'a, str> {
    if disk.ends_with(".jmod") && !entry.starts_with("classes/") {
        std::borrow::Cow::Owned(format!("classes/{entry}"))
    } else {
        std::borrow::Cow::Borrowed(entry)
    }
}

pub fn jar_url_lookup_entry(ctx: &mut dyn NativeContext, ext: &str) -> Result<Value, MethodCallFailed> {
    let after = match ext
        .strip_prefix("jar:file:")
        .or_else(|| ext.strip_prefix("jar:"))
    {
        Some(a) => a,
        None => return Ok(Value::Object(None)),
    };
    let mut parts = after.splitn(2, "!/");
    let jar_raw = match parts.next() {
        Some(p) => p.trim_start_matches("file:"),
        None => return Ok(Value::Object(None)),
    };
    let entry_name = match parts.next() {
        Some(e) if !e.is_empty() => e,
        _ => return Ok(Value::Object(None)),
    };
    // Tomcat `war:` nested-jar case (see `war_nested_jar_bytes`): the jar
    // component is packaged inside a WAR rather than sitting directly on
    // disk, so its bytes must come from the enclosing WAR's zip entry.
    if let Some(bytes) = war_nested_jar_bytes(jar_raw) {
        let cursor = std::io::Cursor::new(bytes.as_slice());
        let mut archive = match zip::ZipArchive::new(cursor) {
            Ok(a) => a,
            Err(_) => return Ok(Value::Object(None)),
        };
        return Ok(jar_entry_value_from_archive(ctx, &mut archive, entry_name)?);
    }
    let trimmed = jar_raw.trim_start_matches('/');
    let disk = if std::path::Path::new(trimmed).exists() {
        trimmed.to_string()
    } else if std::path::Path::new(jar_raw).exists() {
        jar_raw.to_string()
    } else {
        trimmed.to_string()
    };
    let file = match std::fs::File::open(&disk) {
        Ok(f) => f,
        Err(_) => return Ok(Value::Object(None)),
    };
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(_) => return Ok(Value::Object(None)),
    };
    // Extract the entry metadata into owned values, then drop the `archive`
    // borrow before doing any `ctx` allocation (mirrors p59_jar_collect_entries).
    let lookup = jmod_zip_entry_name(&disk, entry_name);
    Ok(jar_entry_value_from_archive(ctx, &mut archive, &lookup)?)
}

/// Recover the originating `jar:…!/entry` URL from a synthetic
/// `JarURLConnection` (field `HUC_URL`) and return the jar entry's size, or
/// `-1` when it can't be resolved (matching `URLConnection` semantics).
pub fn jar_url_conn_entry_size(ctx: &mut dyn NativeContext, this: ObjectRef) -> i64 {
    let ext = jar_url_conn_ext(ctx, this);
    jar_url_entry_size(&ext).unwrap_or(-1)
}

pub fn read_inet_socket_address(
    ctx: &dyn NativeContext,
    sa: ObjectRef,
) -> Result<(String, i32), cratonvm_types::error::MethodCallFailed> {
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
                        // CratonVM-synthesised InetAddress: host/IP live in the
                        // ObjectRef-keyed side table (instance slots are the
                        // real-JDK `holder` reference fields — see
                        // `inet_addr_side_table`).
                        //
                        // SBR wave 2 (lane `netaddr`): under `--real-jdk` the
                        // address may be one the JDK's own bytecode built
                        // (`getByName("127.0.0.1")` is `new Inet4Address(null,
                        // bytes)` once the `InetAddress` rows are retired), with
                        // no side-table row. The raw-slot fallbacks below then
                        // read `holder.originalHostName` (slot 0, null for a
                        // literal) and answered "0.0.0.0", i.e. the WILDCARD.
                        // `inet_addr_resolve` reads the side table first and the
                        // real `holder`/`holder6` after, so a mirror answers
                        // exactly as before.
                        let pair = if ctx.is_jdk_only() {
                            inet_addr_resolve(ctx, addr_obj)
                        } else {
                            inet_addr_get(addr_obj)
                        };
                        if let Some((host, ip)) = pair {
                            if !host.is_empty() {
                                resolved = Some(host);
                            } else if !ip.is_empty() {
                                resolved = Some(ip);
                            }
                        }
                        // Legacy synthetic InetAddress: slot 0 = hostName
                        // String, slot 1 = ip String (kept for objects not
                        // built by `alloc_inet_address`).
                        if resolved.is_none() {
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
                        }
                        // real-JDK InetAddress: slot 0 = InetAddressHolder; the
                        // holder's slot 0 is hostName, slot 1 packs address bytes.
                        if resolved.is_none() {
                            if let Value::Object(Some(inner_holder)) = ctx.get_field(addr_obj, 0) {
                                if let Value::Object(Some(name_obj)) =
                                    ctx.get_field(inner_holder, 0)
                                {
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
    // Port: a real-JDK `InetSocketAddress` declares a single `holder` field at
    // slot 0; the port lives at `holder.port` (slot 2). Its slot 1 (our legacy
    // `ISA_PORT`) is therefore out of the declared layout and reads back as a
    // stale `Int(0)` — which the previous "read ISA_PORT first" logic accepted,
    // so `Socket.connect(new InetSocketAddress(host, port))` targeted port 0
    // (`ConnectException: host:0`). Mirror the host branch above: when the
    // slot-0 value is a real `InetSocketAddressHolder` object (i.e. not a
    // String, which is the legacy-synthetic layout), read the port from
    // `holder.port`; only fall back to the direct `ISA_PORT` slot for the
    // legacy synthetic layout (host String at slot 0, port int at slot 1).
    let port = match holder_val {
        Value::Object(Some(holder)) if ctx.read_string(holder).is_none() => {
            match ctx.get_field(holder, 2) {
                Value::Int(n) => n,
                Value::Long(n) => n as i32,
                // Holder carried no int port — fall back to the direct slot.
                _ => match ctx.get_field(sa, ISA_PORT) {
                    Value::Int(n) => n,
                    Value::Long(n) => n as i32,
                    _ => 0,
                },
            }
        }
        _ => match ctx.get_field(sa, ISA_PORT) {
            Value::Int(n) => n,
            Value::Long(n) => n as i32,
            _ => 0,
        },
    };
    Ok((host, port))
}

pub fn java_byte_array_to_vec(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    offset: i32,
    length: i32,
) -> Result<Vec<u8>, cratonvm_types::error::MethodCallFailed> {
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
    // PERF: one bulk copy instead of one virtual `get_array_element` (plus a
    // `Value` box) per byte. This is the marshalling step under every
    // `Socket.getOutputStream().write(byte[], int, int)`, so its per-byte cost
    // is paid by every bulk socket write in the VM. The range is already
    // bounds-checked above, so the intrinsic copies exactly `ln` bytes; a short
    // return means `arr` is not a byte[] and the old loop's zero-fill would have
    // put bytes on the wire the caller never supplied.
    let mut out = vec![0u8; ln];
    let copied = ctx.read_byte_array_into(arr, off, &mut out);
    if copied != ln {
        return Err(iae(format!(
            "byte[] slice not readable: off={off} len={ln} copied={copied}"
        )));
    }
    Ok(out)
}

pub fn socket_dbg_bytes(data: &[u8]) -> String {
    let preview_len = data.len().min(32);
    let mut preview = String::new();
    for (idx, byte) in data.iter().take(preview_len).enumerate() {
        if idx != 0 {
            preview.push(' ');
        }
        preview.push_str(&format!("{byte:02x}"));
    }
    if data.len() > preview_len {
        preview.push_str(" ...");
    }
    preview
}

pub fn copy_bytes_into_java_array(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    offset: i32,
    data: &[u8],
) -> Result<usize, cratonvm_types::error::MethodCallFailed> {
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

pub fn new_java_byte_array(ctx: &mut dyn NativeContext, data: &[u8]) -> ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Byte, data.len());
    for (i, b) in data.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
    }
    arr
}

/// Canonicalise an IP address string into the exact textual form HotSpot's
/// `InetAddress` stores, so `getHostAddress()` / `toString()` are byte-identical:
///
///   * IPv4-mapped IPv6 (`::ffff:a.b.c.d`) is folded to its IPv4 dotted-quad,
///     matching HotSpot: `getByName` / `getByAddress` / the socket peer decoder
///     all hand back an `Inet4Address` (4-byte `getAddress()`) for a v4-mapped
///     address. Without the fold our mirror stays a 16-byte `Inet6Address`, so
///     any IPv4 CIDR test — e.g. Tomcat's `RemoteIpFilter` matching a dual-stack
///     loopback peer against `127.0.0.0/8` — silently fails on
///     `NetMask.matches`'s 4-vs-16 length guard.
///   * Genuine IPv6 is rendered in HotSpot's FULL eight-group form
///     (`Inet6Address.numericToTextFormat`: each 16-bit group as minimal
///     lowercase hex, joined by `:`, with NO `::` zero-compression), e.g.
///     `::1` → `0:0:0:0:0:0:0:1`, `fe80::1` → `fe80:0:0:0:0:0:0:1`. Rust's
///     `Ipv6Addr::to_string()` would otherwise emit the RFC-5952 compressed
///     form (`::1`), diverging from HotSpot's `getHostAddress()`.
///   * Plain IPv4 and unparseable hosts pass through unchanged.
pub fn hotspot_ip_string(ip: &str) -> String {
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => ipv6_uncompressed_text(&v6),
        },
        Ok(std::net::IpAddr::V4(v4)) => v4.to_string(),
        Err(_) => ip.to_string(),
    }
}

/// HotSpot's `Inet6Address.numericToTextFormat` — eight 16-bit groups as
/// minimal lowercase hex joined by `:`, with NO `::` zero-compression and
/// **no v4-mapped fold**.
///
/// Split out of [`hotspot_ip_string`] because the fold is a property of the
/// *entry point*, not of the address: `getByName` / `InetAddress.getByAddress`
/// fold a v4-mapped address to an `Inet4Address`, but an object that is
/// ALREADY an `Inet6Address` — one the real JDK built via
/// `Inet6Address.getByAddress(String, byte[], int)`, which is not intercepted
/// — must keep all sixteen bytes. Folding its text made
/// `inet_addr_address_bytes` re-parse four bytes out of a v6 object, so
/// `getAddress().length` was 4 on an `instanceof Inet6Address` receiver:
/// internally inconsistent, and `io.netty.util.NetUtil.toAddressString`
/// indexes 16 (`ArrayIndexOutOfBoundsException: Index 4 out of bounds for
/// length 4`, `NetUtilTest.testIpv4MappedIp6GetByName`).
pub fn ipv6_uncompressed_text(v6: &std::net::Ipv6Addr) -> String {
    v6.segments()
        .iter()
        .map(|seg| format!("{seg:x}"))
        .collect::<Vec<_>>()
        .join(":")
}

pub fn alloc_inet_address(
    ctx: &mut dyn NativeContext,
    host: &str,
    ip: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    // An IPv6 `%scope` travels with the address text through every caller
    // (`getByName("fe80::1%eth0")`, the `getifaddrs` interface enumeration).
    // Split it off before normalising — `hotspot_ip_string` parses the text,
    // and a scoped literal parses as nothing, which used to silently classify
    // the mirror as an `Inet4Address`.
    let (ip_bare, scope) = split_addr_scope(ip);
    // Canonicalise the address string to HotSpot's exact textual form (v4-mapped
    // fold + uncompressed IPv6) — see [`hotspot_ip_string`] — so the mirror is an
    // `Inet4Address` with a 4-byte `getAddress()` where appropriate and
    // `getHostAddress()` is byte-identical to HotSpot.
    let ip_norm = hotspot_ip_string(ip_bare);
    let ip = ip_norm.as_str();
    // A scope only exists on IPv6. A v4-mapped address folded to a dotted quad
    // above is an `Inet4Address` now and must not keep one.
    let scope = match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(_)) => scope,
        _ => None,
    };
    // Allocate the *concrete* address class so `instanceof Inet4Address`
    // checks (e.g. Hazelcast's `DefaultAddressPicker`) and virtual dispatch
    // resolve correctly. A bare `InetAddress` is abstract in real-JDK.
    let class_name = match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(_)) => "java/net/Inet6Address",
        _ => "java/net/Inet4Address",
    };
    let ia = try_alloc_concurrent_synthetic(ctx, class_name, 2)?;
    // Record host/IP in the ObjectRef-keyed side table — source of truth for
    // the natives we override. Do NOT write bare Strings into instance slots
    // 0/1: those are the real-JDK `holder` reference fields, and a String
    // there poisons real-JDK InetAddress bytecode dispatch (bogus
    // `NoSuchMethodError java/lang/String.getHostName()`). See
    // `inet_addr_side_table()` for the full rationale.
    //
    // The scope goes back onto the stored text; `inet_addr_resolve` splits it
    // off again for every address consumer, and only `inet6_scope_suffix`
    // reads it (see [`split_addr_scope`]).
    match scope {
        Some(s) => inet_addr_set(ctx.vm_identity(), ia, host, &format!("{ip}%{s}")),
        None => inet_addr_set(ctx.vm_identity(), ia, host, ip),
    }
    // Additionally populate a *real* `InetAddress$InetAddressHolder` so any
    // un-overridden real-JDK `InetAddress` / `Inet4Address` bytecode (the
    // `final` `getHostName()` accessor, `toString()`, …) reads a consistent
    // shape instead of dereferencing a null `holder`.
    //
    // `populate_inet_holder` allocates, so it RETURNS the mirror's current
    // address: returning our own pre-call `ia` handed every caller a stale
    // reference under a moving young GC. `inet_addr_set` keys the side table
    // on the pre-GC identity; the table's remap (`gc_update_inet_addr_refs`)
    // follows the relocation on its own. The table does NOT keep the mirror
    // alive (gc-common w17-a: its rows are weak): `populate_inet_holder` roots
    // it across its own allocations, and the caller owns it from the return.
    Ok(populate_inet_holder(ctx, ia, host, ip, scope)?)
}

/// `InetAddress.getByAddress(byte[])` — construct a concrete, layout-correct
/// address mirror from exactly four or sixteen raw octets.
///
/// Keep this as the sole implementation and registration of the one-argument
/// factory. In particular, IPv6 must pass through [`alloc_inet_address`],
/// which applies HotSpot's uncompressed eight-group text representation.
pub fn native_inet_get_by_address(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A NULL array is UnknownHostException, not NullPointerException.
    // `InetAddress.getByAddress(byte[])` delegates to the two-argument form,
    // whose body is `if (addr != null) { .. }` followed by an unconditional
    // `throw new UnknownHostException("addr is of illegal length")` -- so null
    // falls out of the same door as a 5-byte array. MEASURED against HotSpot
    // 25.0.3+9 (`probes/InetFamilySweep.java`): HotSpot UnknownHostException,
    // this VM NullPointerException, in BOTH modes. `obj_arg` raises the NPE for
    // every one of its ~3900 call sites, so the check has to be here.
    let Some(Value::Object(Some(arr))) = args.first() else {
        return Err(uhex("addr is of illegal length".to_string()));
    };
    let arr = *arr;
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
        // The JDK throws UnknownHostException here, not IllegalArgumentException
        // (`InetAddress.getByAddress` declares `throws UnknownHostException` and
        // uses it for the bad-length case). Real callers catch it by that type;
        // an IAE escapes their catch and propagates as an unrelated failure.
        // MEASURED on HotSpot 25.0.4+7 (`L6InetSweep`): the message carries NO
        // length. `InetAddress.getByAddress` throws a constant string, and the
        // six rows that reach it here differed on the `: {len}` this VM added.
        let _ = len;
        return Err(uhex("addr is of illegal length".to_string()));
    };
    // Normalize to HotSpot's numeric text before storing anything, or a caller
    // that reads the address (such as Jetty's connector setup) observes Rust's
    // RFC-5952-compressed IPv6 form instead of the JDK's uncompressed one.
    // (This comment used to say the text was stored in "both logical fields";
    // it is not, since `e092b0f3b` — see below.)
    let ip_text = hotspot_ip_string(&ip_str);
    // `getByAddress(byte[])` is handed raw octets and NO name, so the mirror
    // must not remember one — HotSpot prints `/1.2.3.4`. The two-argument
    // factory `getByAddress(String, byte[])` is a different method and keeps
    // its host, however bogus (it is never resolved).
    let obj = alloc_inet_address_unnamed(ctx, &ip_text)?;
    Ok(Some(Value::Object(Some(obj))))
}

pub fn alloc_inet_socket_address(
    ctx: &mut dyn NativeContext,
    host: &str,
    port: i32,
) -> Result<ObjectRef, MethodCallFailed> {
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
    //
    // Cross-call GC-safety: `alloc_concurrent_synthetic` / `create_string`
    // allocate and can move everything already in hand, so `isa` and `holder`
    // are rooted and re-read before every write. See `populate_inet_holder`
    // for the failure this shape produced when it was missing.
    let mut scope = NativeHandleScope::new(ctx);
    let isa = try_alloc_concurrent_synthetic(&mut *scope, "java/net/InetSocketAddress", 2)?;
    let isa_h = scope.root(isa);
    let holder = try_alloc_concurrent_synthetic(
        &mut *scope,
        "java/net/InetSocketAddress$InetSocketAddressHolder",
        3,
    )?;
    let holder_h = scope.root(holder);
    let h = scope.create_string(host);
    let holder_cur = scope.get(&holder_h);
    scope.set_field(holder_cur, 0, Value::Object(Some(h)));
    scope.set_field(holder_cur, 1, Value::Object(None));
    scope.set_field(holder_cur, 2, Value::Int(port));
    let isa_cur = scope.get(&isa_h);
    scope.set_field(isa_cur, ISA_HOST, Value::Object(Some(holder_cur)));
    scope.set_field(isa_cur, ISA_PORT, Value::Int(port));
    Ok(isa_cur)
}

/// Like [`alloc_inet_socket_address`], but populates the holder's `addr`
/// field with a REAL resolved `InetAddress` (`ip`) instead of leaving it
/// null. Real JDK's `ServerSocket`/`HttpServer.getAddress()` always returns
/// a fully-resolved address reflecting what the socket is actually bound to
/// (HotSpot: `/127.0.0.1:PORT`, both `getHostString()` and `getAddress()`
/// populated) — an unresolved echo (`hostString` = the raw ctor string,
/// `getAddress()` = null) breaks any caller that reconnects using the
/// server's own reported address (e.g. `RestClient.builder(new
/// HttpHost(address.getHostString(), ...))`): `getHostString()` on an
/// unresolved address returns the original hostname, and a caller-side TLS
/// connect that re-resolves it can land somewhere else entirely.
pub fn alloc_inet_socket_address_resolved(
    ctx: &mut dyn NativeContext,
    host: &str,
    ip: &str,
    port: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    // Cross-call GC-safety: see `alloc_inet_socket_address`. `alloc_inet_address`
    // in particular runs class initialisation on a cold VM, so everything held
    // across it must be rooted.
    let mut scope = NativeHandleScope::new(ctx);
    let isa = try_alloc_concurrent_synthetic(&mut *scope, "java/net/InetSocketAddress", 2)?;
    let isa_h = scope.root(isa);
    let holder = try_alloc_concurrent_synthetic(
        &mut *scope,
        "java/net/InetSocketAddress$InetSocketAddressHolder",
        3,
    )?;
    let holder_h = scope.root(holder);
    let h = scope.create_string(host);
    let h_h = scope.root(h);
    let addr = alloc_inet_address(&mut *scope, host, ip);
    let addr_h = scope.root(addr?);
    let holder_cur = scope.get(&holder_h);
    let h_cur = scope.get(&h_h);
    let addr_cur = scope.get(&addr_h);
    scope.set_field(holder_cur, 0, Value::Object(Some(h_cur)));
    scope.set_field(holder_cur, 1, Value::Object(Some(addr_cur)));
    scope.set_field(holder_cur, 2, Value::Int(port));
    let isa_cur = scope.get(&isa_h);
    scope.set_field(isa_cur, ISA_HOST, Value::Object(Some(holder_cur)));
    scope.set_field(isa_cur, ISA_PORT, Value::Int(port));
    Ok(isa_cur)
}

pub fn resolve_host(host: &str) -> Result<IpAddr, cratonvm_types::error::MethodCallFailed> {
    if host.is_empty() || host == "localhost" {
        return Ok(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
    // The JDK's literal rules, ahead of the resolver: `getaddrinfo` runs
    // `inet_aton` on numeric-looking text and accepts forms the JDK dropped,
    // so a screen that runs AFTER it cannot see what it swallowed.
    match literal_screen(host)? {
        Some(ip) => return Ok(ip),
        None => {}
    }
    // A SCOPED IPv6 literal — `fe80::1%14`, `fe80::1%eth0`,
    // `fe80::1%{04C70698-…}` on Windows, where our interface names are the
    // adapter GUIDs. `Ipv6Addr::from_str` rejects the suffix, so without this
    // the whole text fell through to DNS and came back
    // `UnknownHostException` — including for text this VM had just PRODUCED
    // itself: `Inet6Address.getHostAddress()` renders `%<interface name>`, and
    // anything that puts an address on the wire as text and parses it back
    // (netty's SOCKS4 proxy handler, every `URI`-shaped config) round-trips
    // through exactly this call.
    //
    // HotSpot's rule, which this follows: a numeric scope is taken as-is, and
    // a NAMED scope has to name an interface that exists — an unknown name is
    // an unknown host, not scope 0. Scanning the interfaces costs a syscall,
    // and only a literal carrying a `%` ever reaches it.
    if let Some((bare, scope)) = host.split_once('%') {
        if !scope.is_empty() {
            if let Ok(v6) = bare.parse::<Ipv6Addr>() {
                let numeric = scope.bytes().all(|b| b.is_ascii_digit());
                if numeric || re8_scan_host_ifaces().iter().any(|i| i.name == scope) {
                    return Ok(IpAddr::V6(v6));
                }
                return Err(uhex(host.to_string()));
            }
        }
    }
    let lookup = format!("{host}:0");
    let mut iter = std::net::ToSocketAddrs::to_socket_addrs(&lookup.as_str())
        .map_err(|e| uhex(format!("{host}: {}", resolver_message(&e))))?;
    match iter.next() {
        Some(sa) => Ok(sa.ip()),
        None => Err(uhex(format!("{host}"))),
    }
}

/// The host name this VM reports to Java code.
///
/// Delegates to [`crate::resolve_real_hostname`], which is the crate-wide
/// source of truth (its own doc already claims `getLocalHost` and
/// `NetworkInterface.getNetworkInterfaces` share it). This function used to
/// probe `COMPUTERNAME`/`HOSTNAME` itself, in the opposite precedence and with
/// no `hostname(1)` fallback -- so on a Linux host where `HOSTNAME` is not
/// exported, the phase-E `InetAddress.getLocalHost()` here answered
/// `"localhost"` while `net_uri_inet`'s registration of the *same* method
/// answered the real machine name. Which one a program saw depended only on
/// registration order. Sharing one resolver removes the divergence and the two
/// uncached `getenv` probes per call (the shared resolver latches its result).
pub fn hostname_string() -> String {
    crate::resolve_real_hostname()
}


// ---------------------------------------------------------------------------
// Top-level
// ---------------------------------------------------------------------------

pub fn register_phase_e_networking(registry: &mut NativeMethodRegistry) {
    register_re1_socket(registry);
    register_re2_server_socket(registry);
    register_re3_inet_address(registry);
    register_re4_url_http(registry);
    register_uri_natives(registry);
    register_re5_http_client(registry);
    register_re6_ssl_context(registry);
    register_ssl_server_socket_options(registry);
    register_re7_datagram_socket(registry);
    register_re8_network_interface(registry);
    register_re9_nio_selector(registry);
    register_re10_http_server(registry);
}

// ===========================================================================
// java.net.URI natives
//
// CratonVM's class loading sometimes cannot find java/net/URI methods from
// the real JDK bytecode (they appear as "no such method" linkage errors).
// We register our own implementations that parse the raw URI string stored
// at field 6 of our synthetic URI objects (scheme=0, host=1, port=2,
// path=3, query=4, fragment=5, raw=6).
//
// JDK-ONLY-LAYOUT: that seven-slot model is a FABRICATION and none of its
// indices survive on a real `java.net.URI`, which declares
// `scheme, fragment, authority, userInfo, host, port, path, query, …`. So
// every raw-slot access below is gated on [`uri_has_synthetic_layout`], asked
// by NAME. Measured 2026-08-10 with `CRATONVM_DBG=overlay,overlay-all`:
// eleven reads of slot 6 (`path`) and three of slot 2 (`authority`) per run of
// `probes/W2ResidualCensusProbe`, all on real-layout receivers.
// ===========================================================================

/// Does `uri` have OUR fabricated seven-slot `java/net/URI` layout rather than
/// the real class's?
///
/// Asked by NAME, never by field count: `alloc_concurrent_synthetic` returns at
/// least the requested slot count either way, and `define_class_with_options`
/// pads a real `URI` up to the model's width, so a count test cannot separate
/// the two layouts. That mistake shipped completely inert once already, in the
/// first `VarHandle` guard.
///
/// A real `java.net.URI` declares `schemeSpecificPart`; a VM-fabricated stub has
/// generated `_fN` placeholders and declares nothing of the sort.
pub fn uri_has_synthetic_layout(ctx: &dyn NativeContext, uri: ObjectRef) -> bool {
    let class_id = ctx.class_id_of_object(uri);
    !ctx.declared_fields(class_id)
        .iter()
        .any(|f| !f.is_static && f.name == "schemeSpecificPart")
}

/// Read the raw URI string from a URI object.
///
/// The real-JDK `string` field is tried first, by name. The raw-slot fallbacks
/// (field 6, then 5, then 0) address OUR fabricated model and are reached only
/// when the receiver actually has it — on a real `java.net.URI` slot 6 is
/// `path` and slot 5 is `port`, so reading them answers the wrong field and,
/// for slot 5, a primitive where a `String` was expected.
/// The Java `String` OBJECT holding this URI's full text, when there is one.
///
/// Mirrors [`uri_raw_string`]'s search order exactly and returns the object
/// rather than a decoded copy, for the one caller whose answer IS that string:
/// `URI.toString()`.
///
/// WHY THIS EXISTS. A Rust `String` cannot hold an unpaired surrogate, so
/// `read_string` -> `create_string` is a lossy round trip. MEASURED on
/// `RJdkBridge1`'s `surrog` family at `3fcc8d90f`: `new URI` over a path
/// carrying a lone high surrogate came back with U+FFFD in its place, where
/// HotSpot returns the 12-character string with the surrogate intact. The
/// text was never lost on the OBJECT -- a real `java.net.URI` caches it in
/// its `string` field and our synthetic layout keeps it in slot 6/5/0 --
/// only in the decode. Handing the object back preserves it by construction,
/// and allocates nothing.
///
/// Same defect class as `G55-1`'s, and its N1. The difference is that this
/// caller does not need to REASON about the text, so it needs no units
/// reader: it needs to stop copying.
pub fn uri_raw_string_object(ctx: &dyn NativeContext, uri: ObjectRef) -> Option<ObjectRef> {
    if let Value::Object(Some(s)) = ctx.get_field_by_name(uri, "string") {
        if ctx.read_string(s).is_some_and(|r| !r.is_empty()) {
            return Some(s);
        }
    }
    if !uri_has_synthetic_layout(ctx, uri) {
        return None;
    }
    for &idx in &[6usize, 5usize] {
        if let Value::Object(Some(s)) = ctx.get_field(uri, idx) {
            if ctx.read_string(s).is_some_and(|r| !r.is_empty()) {
                return Some(s);
            }
        }
    }
    // Slot 0 is the scheme on both layouts and only answers here when it
    // happens to hold a whole URI -- the same guard `uri_raw_string` applies,
    // kept identical so the two cannot disagree about which slot won.
    if let Value::Object(Some(s)) = ctx.get_field(uri, 0) {
        if ctx
            .read_string(s)
            .is_some_and(|v| v.contains(":/") || v.contains(":\\"))
        {
            return Some(s);
        }
    }
    None
}

/// `java.net.URI.defineString()`, read from a real `URI`'s own fields: the
/// text `toString()` would cache for a URI real bytecode assembled without it.
/// `None` when every component is null (an empty placeholder).
///
/// ```text
///   [scheme ":"] ( opaque: schemeSpecificPart
///                | ["//" (host form | authority)] [path] ["?" query] )
///   ["#" fragment]
/// ```
///
/// `isOpaque()` is `path == null`, and the host form brackets an IPv6 literal
/// that is not already bracketed -- both exactly as the JDK spells them.
pub fn uri_define_string(ctx: &dyn NativeContext, uri: ObjectRef) -> Option<String> {
    let text = |name: &str| match ctx.get_field_by_name(uri, name) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    };
    let scheme = text("scheme");
    let ssp = text("schemeSpecificPart");
    let host = text("host");
    let user_info = text("userInfo");
    let authority = text("authority");
    let path = text("path");
    let query = text("query");
    let fragment = text("fragment");
    if scheme.is_none()
        && ssp.is_none()
        && host.is_none()
        && authority.is_none()
        && path.is_none()
        && query.is_none()
        && fragment.is_none()
    {
        return None;
    }
    let port = match ctx.get_field_by_name(uri, "port") {
        Value::Int(p) => p,
        _ => -1,
    };
    let mut sb = String::new();
    if let Some(s) = &scheme {
        sb.push_str(s);
        sb.push(':');
    }
    match &path {
        None => sb.push_str(ssp.as_deref().unwrap_or("null")),
        Some(p) => {
            if let Some(h) = &host {
                sb.push_str("//");
                if let Some(u) = &user_info {
                    sb.push_str(u);
                    sb.push('@');
                }
                let brackets = h.contains(':') && !h.starts_with('[') && !h.ends_with(']');
                if brackets {
                    sb.push('[');
                }
                sb.push_str(h);
                if brackets {
                    sb.push(']');
                }
                if port != -1 {
                    sb.push(':');
                    sb.push_str(&port.to_string());
                }
            } else if let Some(a) = &authority {
                sb.push_str("//");
                sb.push_str(a);
            }
            sb.push_str(p);
            if let Some(q) = &query {
                sb.push('?');
                sb.push_str(q);
            }
        }
    }
    if let Some(f) = &fragment {
        sb.push('#');
        sb.push_str(f);
    }
    Some(sb)
}

