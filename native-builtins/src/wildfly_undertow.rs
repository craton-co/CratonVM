// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.2.d — WildFly Undertow HTTP subsystem + IO integration.
//!
//! Undertow is WildFly's embedded HTTP server. For Keycloak 16 it binds to
//! `:8080` (http) and `:8443` (https) and routes requests into the Keycloak
//! war's servlets. This module provides enough of Undertow's surface that the
//! bytecode path can bind a port, accept connections, parse HTTP/1.1, and
//! dispatch into a `HttpHandler` chain.
//!
//! # Architecture
//!
//! ```text
//!   Undertow.builder()
//!     .addHttpListener(port, host)     [configure HTTP listener]
//!     .setHandler(rootHandler)         [root handler chain]
//!     .setWorkerThreads(16)            [worker pool size]
//!     .build()                         [→ Undertow]
//!     .start()                         [bind + accept]
//!
//!   Undertow.start() path (this module):
//!     1. parse_socket_addr("host:port")  (T19.5 helper)
//!     2. socket0() + bind0() + listen() via net.rs native surface
//!     3. spawn undertow-accept thread  (accept loop)
//!     4. per connection: spawn worker from bounded pool
//!     5. worker parses request line + headers + body, builds
//!        HttpServerExchange, dispatches handler.handleRequest(exchange)
//!     6. worker writes response status + headers + body, closes.
//! ```
//!
//! # HTTP/1.1 parse state machine
//!
//! Each connection is read in three phases:
//!
//! 1. **Request line** — `METHOD SP PATH SP VERSION CRLF`. Bounded at 8 KiB.
//! 2. **Header section** — `NAME: VALUE CRLF` repeated until `CRLF CRLF`.
//!    Bounded at 32 KiB total, 100 headers max.
//! 3. **Entity body** — `Content-Length` bytes (or `Transfer-Encoding:
//!    chunked`). Bounded at 10 MiB default (configurable).
//!
//! Violations → `400 Bad Request` (parse error) or `413 Payload Too Large`
//! (body over cap). Malformed request line → `400 Bad Request`.
//!
//! # Security hardening
//!
//! * **Request size caps** — request line ≤ 8 KiB, header section ≤ 32 KiB,
//!   header count ≤ 100, body ≤ 10 MiB.
//! * **CRLF injection** — response header values containing `\r` or `\n`
//!   are rejected at write time (HTTP response splitting).
//! * **Host validation** — incoming `Host:` header must match the bound
//!   host/port (or be allowed via the `*` wildcard).
//! * **Accept-loop backoff** — on `EMFILE`/`ENOBUFS` we sleep 100 ms before
//!   retrying to avoid tight-looping under fd exhaustion.
//! * **Panic safety** — `handler.handleRequest` is wrapped in
//!   `catch_unwind(AssertUnwindSafe)`; a panic yields `500 Internal Server
//!   Error` instead of crashing the process.
//! * **TLS** — `addHttpsListener` stashes the SSLContext for T19.9 to pick
//!   up once TLS server support is wired.
//!
//! # Synthetic-stub field layouts (see `classloading/src/class_manager.rs`)
//!
//! | Class                                                          | # | Slots                                                                                  |
//! |---------------------------------------------------------------|---|----------------------------------------------------------------------------------------|
//! | `io/undertow/Undertow`                                        | 5 | listeners, handler, worker_threads, io_threads, bound_fds                              |
//! | `io/undertow/server/HttpServerExchange`                       | 7 | method, uri, request_headers, request_body, response_status, response_headers, sender  |
//! | `io/undertow/util/HeaderMap`                                  | 1 | entries_map                                                                            |
//! | `io/undertow/util/HttpString`                                 | 1 | bytes                                                                                  |
//! | `org/wildfly/extension/undertow/UndertowService`              | 3 | name, server_handle, state                                                             |
//! | `org/wildfly/extension/undertow/ListenerService`              | 3 | port, host, bound_address                                                              |

#![allow(clippy::needless_pass_by_value)]

use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use cratonvm_native_api::{NativeContext, NativeHandleScope, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

use crate::{obj_arg, try_alloc_concurrent_synthetic};

// ---------------------------------------------------------------------------
// Class names
// ---------------------------------------------------------------------------

const CLS_UNDERTOW: &str = "io/undertow/Undertow";
const CLS_UNDERTOW_BUILDER: &str = "io/undertow/Undertow$Builder";
const CLS_EXCHANGE: &str = "io/undertow/server/HttpServerExchange";
const CLS_HEADER_MAP: &str = "io/undertow/util/HeaderMap";
const CLS_HTTP_STRING: &str = "io/undertow/util/HttpString";
const CLS_HEADERS: &str = "io/undertow/util/Headers";
const CLS_SENDER: &str = "io/undertow/io/Sender";
const CLS_UNDERTOW_SVC: &str = "org/wildfly/extension/undertow/UndertowService";
const CLS_LISTENER_SVC: &str = "org/wildfly/extension/undertow/ListenerService";

// ---------------------------------------------------------------------------
// Synthetic field offsets (mirrored in class_manager.rs)
// ---------------------------------------------------------------------------

// Undertow
const UND_FIELD_LISTENERS: usize = 0;
const UND_FIELD_HANDLER: usize = 1;
const UND_FIELD_WORKER_THREADS: usize = 2;
const UND_FIELD_IO_THREADS: usize = 3;
const UND_FIELD_BOUND_FDS: usize = 4;
const UND_NUM_SLOTS: usize = 5;

// HttpServerExchange
const EX_FIELD_METHOD: usize = 0;
const EX_FIELD_URI: usize = 1;
const EX_FIELD_REQUEST_HEADERS: usize = 2;
const EX_FIELD_REQUEST_BODY: usize = 3;
const EX_FIELD_RESPONSE_STATUS: usize = 4;
const EX_FIELD_RESPONSE_HEADERS: usize = 5;
const EX_FIELD_RESPONSE_SENDER: usize = 6;
const EX_NUM_SLOTS: usize = 7;

// HeaderMap — single field stores an id into the global header-map registry
// so we can mutate entries via &dyn NativeContext without moving the state
// onto the Java heap (which would need synchronized Map semantics).
const HM_FIELD_ID: usize = 0;
const HM_NUM_SLOTS: usize = 1;

// HttpString — 1 field (bytes String)
const HS_FIELD_BYTES: usize = 0;
const HS_NUM_SLOTS: usize = 1;

// UndertowService
const US_FIELD_NAME: usize = 0;
const US_FIELD_SERVER_HANDLE: usize = 1;
const US_FIELD_STATE: usize = 2;
const US_NUM_SLOTS: usize = 3;

// ListenerService
const LS_FIELD_PORT: usize = 0;
const LS_FIELD_HOST: usize = 1;
const LS_FIELD_BOUND_ADDRESS: usize = 2;
const LS_NUM_SLOTS: usize = 3;

// ---------------------------------------------------------------------------
// Limits (security-hardened — see module docstring)
// ---------------------------------------------------------------------------

/// Maximum bytes in a single HTTP request line (`METHOD SP PATH SP VERSION CRLF`).
pub const MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;

/// Maximum cumulative bytes consumed by the header section.
pub const MAX_HEADER_SECTION_BYTES: usize = 32 * 1024;

/// Maximum number of header lines per request.
pub const MAX_HEADER_COUNT: usize = 100;

/// Default body cap (bytes). Configurable per-listener in production; for
/// tests we always use the default.
pub const DEFAULT_MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Listener state — tracks a bound TcpListener + its accept thread.
// ---------------------------------------------------------------------------

/// An in-flight Undertow listener.
///
/// `listener` is kept in an `Arc<Mutex<Option<...>>>` so we can move it in/out
/// on shutdown without cloning the OS fd.
pub struct Listener {
    pub id: u64,
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub bound_addr: Option<SocketAddr>,
    /// Owning listener. `None` after `stop()` drops the socket.
    pub listener: Arc<Mutex<Option<TcpListener>>>,
}

/// A live Undertow server instance.
pub struct UndertowInstance {
    pub id: u64,
    /// The VM that built it (gc-common w20-g): its rows go at that VM's
    /// teardown ([`forget_vm_undertow_rows`]).
    pub vm: usize,
    pub listener_spec: String,
    pub listeners: Vec<Listener>,
    pub handler_obj_raw: usize,
    pub worker_threads: u32,
    pub io_threads: u32,
    pub running: bool,
}

impl UndertowInstance {
    fn stop(&mut self) {
        for l in &self.listeners {
            // Drop the TcpListener — any background accept thread will see a
            // closed socket on its next call and exit cleanly.
            let mut guard = l.listener.lock().unwrap_or_else(|e| e.into_inner());
            *guard = None;
        }
        self.running = false;
    }
}

fn undertow_instances() -> &'static Mutex<HashMap<u64, UndertowInstance>> {
    static T: OnceLock<Mutex<HashMap<u64, UndertowInstance>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_id() -> u64 {
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::SeqCst)
}

/// An `Undertow` / `Undertow$Builder` object's row key: its VM and its WEAK
/// LOCK KEY (`crate::gc_stable_weak_lock_key`).
///
/// gc-common w29-d (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 10): the object half used to be the identity hash. Two live objects of
/// one VM can share one, and then one server answered the other's instance id
/// (and so its listeners and `stop()`), and one builder the other's
/// configuration. A weak lock key is unique per live object and is never
/// minted again once its object dies.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct UndertowObjKey {
    vm: usize,
    key: usize,
}

/// `obj`'s row key, minting its weak lock key on first use (the put paths).
/// Always computed BEFORE a table lock is taken.
fn undertow_obj_key(ctx: &dyn NativeContext, obj: ObjectRef) -> UndertowObjKey {
    UndertowObjKey {
        vm: ctx.vm_identity(),
        key: crate::gc_stable_weak_lock_key(ctx, obj)
            .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails")),
    }
}

/// `obj`'s row key if it was ever keyed, else `None` (it has no row): the read
/// paths mint nothing.
fn existing_undertow_obj_key(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<UndertowObjKey> {
    crate::existing_weak_lock_key(ctx, obj).map(|key| UndertowObjKey {
        vm: ctx.vm_identity(),
        key,
    })
}

fn undertow_obj_registry() -> &'static Mutex<HashMap<UndertowObjKey, u64>> {
    static R: OnceLock<Mutex<HashMap<UndertowObjKey, u64>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember_undertow_instance(ctx: &dyn NativeContext, obj: ObjectRef, id: u64) {
    let key = undertow_obj_key(ctx, obj);
    undertow_obj_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, id);
}

fn undertow_instance_id_of(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<u64> {
    let row = existing_undertow_obj_key(ctx, obj).and_then(|key| {
        undertow_obj_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .copied()
    });
    row.or_else(|| match ctx.get_field(obj, UND_FIELD_BOUND_FDS) {
        Value::Long(id) => Some(id as u64),
        _ => None,
    })
}

/// Drop the `Undertow` instance-id rows filed under `freed` weak lock keys
/// (their objects are dead; the instance itself is stopped by `stop()` or at
/// VM teardown). This table has no owner address, so the post-collection
/// sweep cannot judge it; before gc-common w29-d its rows lived until VM
/// teardown. Called by `lib.rs::{sweep_lock_keys, forget_vm_lock_keys}`.
pub fn forget_undertow_obj_keys(freed: &[usize]) {
    if freed.is_empty() {
        return;
    }
    let mut registry = undertow_obj_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if registry.is_empty() {
        return;
    }
    let freed: std::collections::HashSet<usize> = freed.iter().copied().collect();
    registry.retain(|key, _| !freed.contains(&key.key));
}

#[derive(Clone)]
struct UndertowBuilderConfig {
    listener_spec: String,
    handler_obj_raw: usize,
    worker_threads: u32,
    io_threads: u32,
    /// The `Undertow$Builder`'s address when last touched: a WEAK owner,
    /// never dereferenced -- re-addressed through the collection's pointer
    /// map, or judged dead, by [`gc_sweep_undertow_rows`] (gc-common w20-g).
    owner: usize,
}

impl Default for UndertowBuilderConfig {
    fn default() -> Self {
        Self {
            listener_spec: String::new(),
            handler_obj_raw: 0,
            worker_threads: 16,
            io_threads: 2,
            owner: 0,
        }
    }
}

fn undertow_builder_configs() -> &'static Mutex<HashMap<UndertowObjKey, UndertowBuilderConfig>> {
    static C: OnceLock<Mutex<HashMap<UndertowObjKey, UndertowBuilderConfig>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn builder_config_of(ctx: &dyn NativeContext, obj: ObjectRef) -> UndertowBuilderConfig {
    let Some(key) = existing_undertow_obj_key(ctx, obj) else {
        return UndertowBuilderConfig::default();
    };
    undertow_builder_configs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned()
        .unwrap_or_default()
}

fn update_builder_config(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    update: impl FnOnce(&mut UndertowBuilderConfig),
) {
    // The key (the lock-key registry's mutex) before the table lock, never
    // under it.
    let key = undertow_obj_key(ctx, obj);
    let mut configs = undertow_builder_configs()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let config = configs.entry(key).or_default();
    config.owner = obj.as_ptr() as usize;
    update(config);
}

// ---------------------------------------------------------------------------
// Header map registry — keeps the mutable HashMap<String,Vec<String>> in Rust
// rather than sprinkling it across Java heap objects.
//
// gc-common w20-g (`common-w19d-undertow-header-map-rows-never-die`): nothing
// ever removed a row, so every `HeaderMap` (two per HTTP exchange) left one
// row here and one in [`header_map_obj_registry`] for the life of the
// process, and a disposed VM kept all of its rows. Now each row carries its
// VM ([`forget_vm_undertow_rows`] drops them at teardown), and each object
// row its map's address as a weak owner: [`gc_sweep_undertow_rows`] drops a
// dead map's object row and its header row together after each collection.
// ---------------------------------------------------------------------------

/// One header map's entries, tagged with the VM that minted it.
struct HeaderMapRows {
    vm: usize,
    entries: HashMap<String, Vec<String>>,
}

fn header_maps() -> &'static Mutex<HashMap<u64, HeaderMapRows>> {
    static T: OnceLock<Mutex<HashMap<u64, HeaderMapRows>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn alloc_header_map_id(vm: usize) -> u64 {
    let id = next_id();
    header_maps()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            id,
            HeaderMapRows {
                vm,
                entries: HashMap::new(),
            },
        );
    id
}

/// A `HeaderMap` object's row key: its VM and its weak lock key. gc-common
/// w29-d: it used to be the identity hash, and then a new map whose hash
/// matched a LIVE map's replaced that map's row in
/// [`remember_header_map_obj`] and FREED its headers (`header_map_free`): the
/// live map then read the new map's headers, another request's cookies and
/// `Authorization` included.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct HeaderMapObjKey {
    vm: usize,
    key: usize,
}

/// A `HeaderMap` object's row: its header-map id and its address as a WEAK
/// owner (never dereferenced; see [`gc_sweep_undertow_rows`]).
#[derive(Clone, Copy, Debug)]
struct HeaderMapObjRow {
    id: u64,
    owner: usize,
}

/// `obj`'s row key, minting its weak lock key on first use (the put path).
fn header_map_obj_key(ctx: &dyn NativeContext, obj: ObjectRef) -> HeaderMapObjKey {
    HeaderMapObjKey {
        vm: ctx.vm_identity(),
        key: crate::gc_stable_weak_lock_key(ctx, obj)
            .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails")),
    }
}

/// `obj`'s row key if it was ever keyed (the read path mints nothing).
fn existing_header_map_obj_key(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<HeaderMapObjKey> {
    crate::existing_weak_lock_key(ctx, obj).map(|key| HeaderMapObjKey {
        vm: ctx.vm_identity(),
        key,
    })
}

fn header_map_obj_registry() -> &'static Mutex<HashMap<HeaderMapObjKey, HeaderMapObjRow>> {
    static R: OnceLock<Mutex<HashMap<HeaderMapObjKey, HeaderMapObjRow>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember_header_map_obj(ctx: &dyn NativeContext, obj: ObjectRef, id: u64) {
    let row = HeaderMapObjRow {
        id,
        owner: obj.as_ptr() as usize,
    };
    // The key before the table lock (gc-common w29-d).
    let key = header_map_obj_key(ctx, obj);
    let replaced = header_map_obj_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, row);
    // A re-`<init>` of the same map: the previous id is reachable from nothing
    // now. (Since gc-common w29-d no OTHER map can share the key.)
    if let Some(old) = replaced.filter(|old| old.id != id) {
        header_map_free(old.id);
    }
}

fn header_map_id_of(ctx: &dyn NativeContext, obj: ObjectRef) -> u64 {
    let row = existing_header_map_obj_key(ctx, obj).and_then(|key| {
        header_map_obj_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .map(|row| row.id)
    });
    row.unwrap_or_else(|| ctx.get_field(obj, HM_FIELD_ID).as_long().unwrap_or(0) as u64)
}

/// Put a header into the referenced map. Rejects values containing CR/LF to
/// prevent HTTP response splitting.
pub fn header_map_put(id: u64, name: &str, value: &str) -> Result<(), &'static str> {
    if value.contains('\r') || value.contains('\n') {
        return Err("CRLF injection in header value");
    }
    let mut t = header_maps().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(m) = t.get_mut(&id) {
        m.entries
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push(value.to_string());
    }
    Ok(())
}

/// Read the first value for a header (case-insensitive), or `None`.
pub fn header_map_get(id: u64, name: &str) -> Option<String> {
    let t = header_maps().lock().unwrap_or_else(|e| e.into_inner());
    t.get(&id)
        .and_then(|m| m.entries.get(&name.to_ascii_lowercase()))
        .and_then(|v| v.first().cloned())
}

/// Read the value at a specific header index (case-insensitive), or `None`.
pub fn header_map_get_at(id: u64, name: &str, index: usize) -> Option<String> {
    let t = header_maps().lock().unwrap_or_else(|e| e.into_inner());
    t.get(&id)
        .and_then(|m| m.entries.get(&name.to_ascii_lowercase()))
        .and_then(|v| v.get(index).cloned())
}

/// Count values for a header (case-insensitive).
pub fn header_map_count(id: u64, name: &str) -> usize {
    let t = header_maps().lock().unwrap_or_else(|e| e.into_inner());
    t.get(&id)
        .and_then(|m| m.entries.get(&name.to_ascii_lowercase()))
        .map_or(0, Vec::len)
}

/// Free a header map id (release the inner HashMap so closed exchanges
/// don't pin memory).
pub fn header_map_free(id: u64) {
    header_maps()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
}

/// VM teardown (gc-common w20-g): drop every row `vm` minted in this file's
/// five tables. A still-bound listener of the VM's Undertow instances is
/// closed AFTER the table guard drops (lesson x: never close an OS handle
/// under a lock another path needs). Called from
/// `forget_vm_native_root_stores`.
pub fn forget_vm_undertow_rows(vm: usize) {
    header_map_obj_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|key, _| key.vm != vm);
    header_maps()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|_, rows| rows.vm != vm);
    undertow_obj_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|key, _| key.vm != vm);
    undertow_builder_configs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|key, _| key.vm != vm);
    let dropped: Vec<UndertowInstance> = {
        let mut instances = undertow_instances()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let ids: Vec<u64> = instances
            .iter()
            .filter(|(_, inst)| inst.vm == vm)
            .map(|(&id, _)| id)
            .collect();
        ids.into_iter()
            .filter_map(|id| instances.remove(&id))
            .collect()
    };
    for mut inst in dropped {
        inst.stop();
    }
}

/// The post-collection weak-row sweep for this file's `HeaderMap` and
/// `Undertow$Builder` rows (gc-common w20-g).
///
/// The contract of the other weak-row sweeps
/// (`phases_late::concurrent::gc_sweep_submission_publisher_rows`,
/// `phases_late::zip_streams::gc_sweep_zip_rows`): owners are PRE-remap
/// addresses; one that is a key of `pointer_map` is re-addressed and kept; any
/// other owner of `vm` is judged by `is_live` with the table locks RELEASED; a
/// dead owner's row is dropped only if it still names that owner (an `<init>`
/// may have re-filed the key meanwhile). Another VM's rows are never judged. A
/// dropped `HeaderMap` row takes its header-map entries with it.
///
/// Meant for the VM's three weak-row sites, after
/// `phases_late::concurrent::gc_sweep_submission_publisher_rows`
/// (`docs/feature-designs/native-dispatch-architecture.md`).
/// The rows hold no `ObjectRef` and root nothing, so an unwired sweep only
/// leaks (the pre-w20 behaviour); it never breaks a lookup. Returns how many
/// rows were dropped.
pub fn gc_sweep_undertow_rows(
    vm: usize,
    pointer_map: &cratonvm_types::PointerMap,
    is_live: &dyn Fn(usize) -> bool,
) -> usize {
    // HeaderMap object rows.
    let judged: Vec<(HeaderMapObjKey, usize)> = {
        let mut registry = header_map_obj_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut judged = Vec::new();
        for (key, row) in registry.iter_mut().filter(|(key, _)| key.vm == vm) {
            if let Some(&to) = pointer_map.get(&row.owner) {
                row.owner = to;
                continue;
            }
            judged.push((*key, row.owner));
        }
        judged
    };
    let dead: Vec<(HeaderMapObjKey, usize)> = judged
        .into_iter()
        .filter(|&(_, owner)| !is_live(owner))
        .collect();
    let freed_ids: Vec<u64> = if dead.is_empty() {
        Vec::new()
    } else {
        let mut registry = header_map_obj_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        dead.into_iter()
            .filter_map(|(key, owner)| {
                if registry.get(&key).map(|row| row.owner) != Some(owner) {
                    return None;                }
                registry.remove(&key).map(|row| row.id)            })
            .collect()
    };
    if !freed_ids.is_empty() {
        let mut maps = header_maps().lock().unwrap_or_else(|e| e.into_inner());
        for id in &freed_ids {
            maps.remove(id);
        }
    }

    // Undertow$Builder configuration rows.
    let judged: Vec<(UndertowObjKey, usize)> = {
        let mut configs = undertow_builder_configs()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut judged = Vec::new();
        for (key, config) in configs.iter_mut().filter(|(key, _)| key.vm == vm) {
            if let Some(&to) = pointer_map.get(&config.owner) {
                config.owner = to;
                continue;
            }
            judged.push((*key, config.owner));
        }
        judged
    };
    let dead: Vec<(UndertowObjKey, usize)> = judged
        .into_iter()
        .filter(|&(_, owner)| !is_live(owner))
        .collect();
    let mut dropped_configs = 0;
    if !dead.is_empty() {
        let mut configs = undertow_builder_configs()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (key, owner) in dead {
            if configs.get(&key).map(|config| config.owner) == Some(owner) {
                configs.remove(&key);
                dropped_configs += 1;
            }
        }
    }
    freed_ids.len() + dropped_configs
}

// ---------------------------------------------------------------------------
// HTTP/1.1 request parser
// ---------------------------------------------------------------------------

/// The parsed head of an HTTP/1.1 request — everything above the body.
#[derive(Debug, Clone)]
pub struct ParsedRequest {
    pub method: String,
    pub uri: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
    /// Byte length of the body, derived from the `Content-Length` header.
    /// `None` when no length was announced (treated as 0 for methods
    /// without a body).
    pub content_length: Option<usize>,
}

/// Errors raised by the HTTP/1.1 head parser.
#[derive(Debug, PartialEq, Eq)]
pub enum HttpParseError {
    /// Request line too long (> `MAX_REQUEST_LINE_BYTES`).
    RequestLineTooLarge,
    /// Headers section too long (> `MAX_HEADER_SECTION_BYTES`).
    HeaderSectionTooLarge,
    /// Too many header lines (> `MAX_HEADER_COUNT`).
    TooManyHeaders,
    /// Malformed request line (missing SP / wrong field count).
    MalformedRequestLine,
    /// Malformed header (missing `:` or bare `\r` / `\n`).
    MalformedHeader,
    /// Raw bytes were not valid ASCII (reject to avoid ambiguity).
    InvalidAscii,
    /// `Content-Length` value was not a non-negative integer.
    InvalidContentLength,
    /// Body exceeds the configured cap.
    BodyTooLarge,
}

impl HttpParseError {
    /// HTTP status code appropriate to this error.
    pub fn status_code(&self) -> u16 {
        match self {
            HttpParseError::BodyTooLarge => 413,
            _ => 400,
        }
    }
}

/// Parse a full HTTP/1.1 request head (request line + headers) from
/// `bytes`. On success returns `(ParsedRequest, head_byte_len)` where
/// `head_byte_len` is the offset into `bytes` where the body begins.
///
/// The body itself is not read here — the caller is expected to either
/// slice the remaining bytes when `content_length` fits in memory, or
/// stream the body separately.
pub fn parse_http_request_head(bytes: &[u8]) -> Result<(ParsedRequest, usize), HttpParseError> {
    // --- Phase 1: request line ---
    let line_end = find_crlf(bytes, 0).ok_or(HttpParseError::MalformedRequestLine)?;
    if line_end > MAX_REQUEST_LINE_BYTES {
        return Err(HttpParseError::RequestLineTooLarge);
    }
    let line = &bytes[..line_end];
    if !line.is_ascii() {
        return Err(HttpParseError::InvalidAscii);
    }
    let line_str = std::str::from_utf8(line).map_err(|_| HttpParseError::InvalidAscii)?;
    let parts: Vec<&str> = line_str.splitn(3, ' ').collect();
    if parts.len() != 3 {
        return Err(HttpParseError::MalformedRequestLine);
    }
    let method = parts[0].to_string();
    let uri = parts[1].to_string();
    let version = parts[2].to_string();
    if method.is_empty() || uri.is_empty() || !version.starts_with("HTTP/") {
        return Err(HttpParseError::MalformedRequestLine);
    }

    // --- Phase 2: headers ---
    let mut pos = line_end + 2; // skip CRLF after request line
    let headers_start = pos;
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        // Empty CRLF terminates the header section.
        if pos + 2 <= bytes.len() && &bytes[pos..pos + 2] == b"\r\n" {
            pos += 2;
            break;
        }
        let hend = find_crlf(bytes, pos).ok_or(HttpParseError::MalformedHeader)?;
        if hend - headers_start > MAX_HEADER_SECTION_BYTES {
            return Err(HttpParseError::HeaderSectionTooLarge);
        }
        if headers.len() >= MAX_HEADER_COUNT {
            return Err(HttpParseError::TooManyHeaders);
        }
        let hline = &bytes[pos..hend];
        let hline_str = std::str::from_utf8(hline).map_err(|_| HttpParseError::InvalidAscii)?;
        let colon = hline_str.find(':').ok_or(HttpParseError::MalformedHeader)?;
        let name = hline_str[..colon].trim();
        let value = hline_str[colon + 1..].trim();
        if name.is_empty() || name.contains(|c: char| c.is_ascii_whitespace()) {
            return Err(HttpParseError::MalformedHeader);
        }
        // Header values are already free of \r / \n because we split on CRLF;
        // but reject embedded NULs defensively.
        if value.contains('\0') {
            return Err(HttpParseError::MalformedHeader);
        }
        headers.push((name.to_string(), value.to_string()));
        pos = hend + 2;
    }

    // --- Phase 3: Content-Length parse ---
    let mut content_length: Option<usize> = None;
    for (k, v) in &headers {
        if k.eq_ignore_ascii_case("content-length") {
            let n: usize = v
                .trim()
                .parse()
                .map_err(|_| HttpParseError::InvalidContentLength)?;
            if n > DEFAULT_MAX_BODY_BYTES {
                return Err(HttpParseError::BodyTooLarge);
            }
            content_length = Some(n);
            break;
        }
    }

    Ok((
        ParsedRequest {
            method,
            uri,
            version,
            headers,
            content_length,
        },
        pos,
    ))
}

fn find_crlf(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < bytes.len() {
        if bytes[i] == b'\r' && bytes[i + 1] == b'\n' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Validate that `host` (the bound host) accepts a request's Host header.
/// An empty bound host (`""`) or `"*"` wildcard accepts anything; otherwise
/// the two must match host-and-port with case-insensitivity on the host.
pub fn host_header_accepted(bound_host: &str, bound_port: u16, host_header: &str) -> bool {
    if bound_host.is_empty() || bound_host == "*" {
        return true;
    }
    let hh = host_header.trim();
    if hh.is_empty() {
        return false;
    }
    // Split off optional port.
    let (hh_host, hh_port) = match hh.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok()),
        None => (hh, None),
    };
    if !hh_host.eq_ignore_ascii_case(bound_host) {
        return false;
    }
    match hh_port {
        Some(p) => p == bound_port,
        None => true, // no port in header — treat as default
    }
}

/// Build a wire-format HTTP/1.1 response: status line + headers + body.
/// Rejects CRLF injection in header values (response splitting).
pub fn build_http_response(
    status: u16,
    reason: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::with_capacity(256 + body.len());
    out.extend_from_slice(format!("HTTP/1.1 {status} {reason}\r\n").as_bytes());
    let mut seen_cl = false;
    for (k, v) in headers {
        if v.contains('\r') || v.contains('\n') {
            return Err("CRLF injection in response header value");
        }
        if k.eq_ignore_ascii_case("content-length") {
            seen_cl = true;
        }
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    if !seen_cl {
        out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Natives — Undertow / Builder
// ---------------------------------------------------------------------------

fn native_undertow_builder(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let obj = try_alloc_concurrent_synthetic(ctx, CLS_UNDERTOW_BUILDER, UND_NUM_SLOTS)?;
    // The real Undertow$Builder layout is not compatible with the native
    // bridge's compact fields. Keep all bridge state outside the Java object.
    // The key before the table lock (gc-common w29-d).
    let key = undertow_obj_key(ctx, obj);
    undertow_builder_configs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            key,
            UndertowBuilderConfig {
                owner: obj.as_ptr() as usize,
                ..UndertowBuilderConfig::default()
            },
        );
    Ok(Some(Value::Object(Some(obj))))
}

fn native_builder_set_socket_option(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // CratonVM's native Undertow bridge does not model XNIO option maps yet.
    // Keep the builder chain intact; the listener bridge handles the socket.
    Ok(Some(Value::Object(Some(this))))
}

fn native_builder_add_http_listener(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    let host = match args.get(2).copied().unwrap_or(Value::Object(None)) {
        Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
        _ => "0.0.0.0".to_string(),
    };
    update_builder_config(ctx, this, |config| {
        config.listener_spec = format!("{host}:{port}:http");
    });
    Ok(Some(Value::Object(Some(this))))
}

fn native_builder_add_https_listener(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    let host = match args.get(2).copied().unwrap_or(Value::Object(None)) {
        Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
        _ => "0.0.0.0".to_string(),
    };
    update_builder_config(ctx, this, |config| {
        config.listener_spec = format!("{host}:{port}:https");
    });
    Ok(Some(Value::Object(Some(this))))
}

fn native_builder_set_handler(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let raw = match args.get(1).copied() {
        Some(Value::Object(Some(handler))) => handler.as_ptr() as usize,
        _ => 0,
    };
    update_builder_config(_ctx, this, |config| config.handler_obj_raw = raw);
    Ok(Some(Value::Object(Some(this))))
}

fn native_builder_set_worker_threads(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let threads = args.get(1).and_then(|v| v.as_int()).unwrap_or(16).max(1) as u32;
    update_builder_config(ctx, this, |config| config.worker_threads = threads);
    Ok(Some(Value::Object(Some(this))))
}

fn native_builder_set_io_threads(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let threads = args.get(1).and_then(|v| v.as_int()).unwrap_or(2).max(1) as u32;
    update_builder_config(ctx, this, |config| config.io_threads = threads);
    Ok(Some(Value::Object(Some(this))))
}

fn native_builder_build(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let config = builder_config_of(ctx, this);
    let undertow = try_alloc_concurrent_synthetic(ctx, CLS_UNDERTOW, UND_NUM_SLOTS)?;
    let id = next_id();
    // See `undertow_instance_id_of`: real Undertow fields have incompatible
    // types, so the instance association lives in the identity side table.
    remember_undertow_instance(ctx, undertow, id);
    let vm = ctx.vm_identity();
    undertow_instances()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            id,
            UndertowInstance {
                id,
                vm,
                listener_spec: config.listener_spec,
                listeners: Vec::new(),
                handler_obj_raw: config.handler_obj_raw,
                worker_threads: config.worker_threads,
                io_threads: config.io_threads,
                running: false,
            },
        );
    Ok(Some(Value::Object(Some(undertow))))
}

fn native_undertow_start(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = undertow_instance_id_of(ctx, this).ok_or_else(|| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: "Undertow.start: instance id missing".into(),
        })
    })?;
    let listeners_txt = undertow_instances()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .map(|instance| instance.listener_spec.clone())
        .unwrap_or_default();
    // Format: "host:port:scheme" (one listener today; extendable).
    let parts: Vec<&str> = listeners_txt.rsplitn(3, ':').collect();
    if parts.len() != 3 {
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: format!("Undertow.start: malformed listener spec '{listeners_txt}'"),
        }));
    }
    let scheme = parts[0];
    let port: u16 = parts[1].parse().unwrap_or(0);
    let host = parts[2];

    let bind_addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&bind_addr).map_err(|e| {
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IOException {
            message: format!("Undertow.start: bind {bind_addr} failed: {e}"),
        }))
    })?;
    let bound = listener.local_addr().ok();

    {
        let mut map = undertow_instances()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(inst) = map.get_mut(&id) {
            inst.listeners.push(Listener {
                id: next_id(),
                host: host.to_string(),
                port: bound.map(|a| a.port()).unwrap_or(port),
                tls: scheme == "https",
                bound_addr: bound,
                listener: Arc::new(Mutex::new(Some(listener))),
            });
            inst.running = true;
        }
    }
    Ok(None)
}

fn native_undertow_stop(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let Some(id) = undertow_instance_id_of(ctx, this) else {
        return Ok(None);
    };
    let mut map = undertow_instances()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(inst) = map.get_mut(&id) {
        inst.stop();
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Natives — HeaderMap / HttpString
// ---------------------------------------------------------------------------

fn native_header_map_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = alloc_header_map_id(ctx.vm_identity());
    remember_header_map_obj(ctx, this, id);
    if !class_has_field(ctx, CLS_HEADER_MAP, "table") {
        ctx.set_field(this, HM_FIELD_ID, Value::Long(id as i64));
    }
    // gc-common w19-d: the receiver is rooted across the table's allocation;
    // both stores below went through its entry address.
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let table = scope.new_ref_array(cratonvm_types::ClassId::new(0), 16);
    let this = scope.get(&this_h);
    scope.set_field_by_name(this, "table", Value::Object(Some(table)));
    scope.set_field_by_name(this, "size", Value::Int(0));
    Ok(None)
}

fn native_header_map_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = header_map_id_of(ctx, this);
    let name = read_http_string(ctx, args.get(1).copied()).unwrap_or_default();
    let value = match args.get(2).copied() {
        Some(Value::Object(Some(o))) => ctx.read_string(o).unwrap_or_default(),
        _ => String::new(),
    };
    header_map_put(id, &name, &value).map_err(|msg| {
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IOException {
            message: msg.to_string(),
        }))
    })?;
    if class_has_field(ctx, CLS_HEADER_MAP, "table") {
        // gc-common w19-d: `remove` is Java. The name and value were pinned
        // but never read back, and `addLast` was handed the ORIGINAL `args`
        // (receiver, name and value at their pre-`remove` addresses), so a
        // collection inside `remove` made `addLast` run on the vacated copy
        // of the map. All three are now rooted and passed current.
        let mut scope = NativeHandleScope::new(ctx);
        let arg_handles: Vec<Option<cratonvm_native_api::NativeHandle>> = args
            .iter()
            .take(3)
            .map(|v| match v {
                Value::Object(Some(o)) => Some(scope.root(*o)),
                _ => None,
            })
            .collect();
        let _ = scope.invoke(
            CLS_HEADER_MAP,
            "remove",
            "(Lio/undertow/util/HttpString;)Ljava/util/Collection;",
            &args[..2],
        )?;
        let current: Vec<Value> = args
            .iter()
            .take(3)
            .zip(&arg_handles)
            .map(|(v, h)| match h {
                Some(h) => Value::Object(Some(scope.get(h))),
                None => *v,
            })
            .collect();
        return if matches!(args.get(2), Some(Value::Object(None)) | None) {
            Ok(Some(current[0]))
        } else {
            scope.invoke(
                CLS_HEADER_MAP,
                "addLast",
                "(Lio/undertow/util/HttpString;Ljava/lang/String;)Lio/undertow/util/HeaderMap;",
                &current,
            )
        };
    }
    Ok(Some(Value::Object(Some(this))))
}

fn native_header_map_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = header_map_id_of(ctx, this);
    let name = read_http_string(ctx, args.get(1).copied()).unwrap_or_default();
    let value = match args.get(2).copied() {
        Some(Value::Object(Some(o))) => ctx.read_string(o).unwrap_or_default(),
        _ => String::new(),
    };
    header_map_put(id, &name, &value).map_err(|msg| {
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IOException {
            message: msg.to_string(),
        }))
    })?;
    if class_has_field(ctx, CLS_HEADER_MAP, "table") {
        return ctx.invoke(
            CLS_HEADER_MAP,
            "addLast",
            "(Lio/undertow/util/HttpString;Ljava/lang/String;)Lio/undertow/util/HeaderMap;",
            args,
        );
    }
    Ok(Some(Value::Object(Some(this))))
}

fn native_header_map_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = header_map_id_of(ctx, this);
    let name = read_http_string(ctx, args.get(1).copied()).unwrap_or_default();
    Ok(Some(Value::Int(if header_map_get(id, &name).is_some() {
        1
    } else {
        0
    })))
}

fn native_header_map_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = header_map_id_of(ctx, this);
    let name = read_http_string(ctx, args.get(1).copied()).unwrap_or_default();
    match header_map_get(id, &name) {
        Some(v) => {
            let s = ctx.create_string(&v);
            Ok(Some(Value::Object(Some(s))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn native_header_map_get_indexed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = header_map_id_of(ctx, this);
    let name = read_http_string(ctx, args.get(1).copied()).unwrap_or_default();
    let index = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
    if index < 0 {
        return Ok(Some(Value::Object(None)));
    }
    match header_map_get_at(id, &name, index as usize) {
        Some(v) => {
            let s = ctx.create_string(&v);
            Ok(Some(Value::Object(Some(s))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn native_header_map_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = header_map_id_of(ctx, this);
    let name = read_http_string(ctx, args.get(1).copied()).unwrap_or_default();
    Ok(Some(Value::Int(header_map_count(id, &name) as i32)))
}

fn http_string_higher(b: u8) -> i32 {
    if b.is_ascii_lowercase() {
        (b & 0xdf) as i32
    } else {
        b as i32
    }
}

fn http_string_hash_code(bytes: &[u8]) -> i32 {
    let mut hash = 17i32;
    for b in bytes {
        hash = hash.wrapping_mul(17).wrapping_add(http_string_higher(*b));
    }
    hash
}

fn native_http_string_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // `this` must survive the `create_string` call in the `string_obj` match
    // below (a GC-triggering allocation), and both `this` and `string_obj`
    // must survive the `new_array` allocation further down before either is
    // read again for the `set_field*` calls.
    let this_pin = ctx.pin_native_root(this);
    let text_arg = args.get(1).copied().unwrap_or(Value::Object(None));
    let text = match text_arg {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let string_obj = match text_arg {
        Value::Object(Some(s)) if ctx.read_string(s).is_some() => s,
        _ => ctx.create_string(&text),
    };
    let string_obj_pin = ctx.pin_native_root(string_obj);

    if class_has_field(ctx, CLS_HTTP_STRING, "bytes") {
        let bytes = text.as_bytes();
        let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
        for (i, b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
        }
        let this = ctx.read_native_pin(this_pin, this);
        let string_obj = ctx.read_native_pin(string_obj_pin, string_obj);
        ctx.set_field_by_name(this, "bytes", Value::Object(Some(arr)));
        ctx.set_field_by_name(this, "hashCode", Value::Int(http_string_hash_code(bytes)));
        ctx.set_field_by_name(this, "orderInt", Value::Int(0));
        ctx.set_field_by_name(this, "string", Value::Object(Some(string_obj)));
    } else {
        let this = ctx.read_native_pin(this_pin, this);
        let string_obj = ctx.read_native_pin(string_obj_pin, string_obj);
        ctx.set_field(this, HS_FIELD_BYTES, Value::Object(Some(string_obj)));
        ctx.set_field_by_name(this, "string", Value::Object(Some(string_obj)));
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

fn http_string_text(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "string") {
        if ctx.read_string(s).is_some() {
            return Some(s);
        }
    }

    match ctx.get_field(this, HS_FIELD_BYTES) {
        Value::Object(Some(s)) if ctx.read_string(s).is_some() => Some(s),
        Value::Object(Some(bytes)) => {
            let len = ctx.array_length(bytes);
            let mut out = String::with_capacity(len);
            for i in 0..len {
                let b = ctx.get_array_element(bytes, i).as_int().unwrap_or(0) as u8;
                out.push(char::from(b));
            }
            // gc-common w19-d: the receiver is rooted across the String's
            // allocation, so the cache lands on the live object (it was
            // written through the pre-allocation address).
            let mut scope = NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let s = scope.create_string(&out);
            let this = scope.get(&this_h);
            scope.set_field_by_name(this, "string", Value::Object(Some(s)));
            Some(s)
        }
        _ => None,
    }
}

fn native_http_string_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Object(http_string_text(ctx, this))))
}

fn native_http_string_append_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let buffer = obj_arg(args, 1)?;
    let text = read_http_string(ctx, Some(Value::Object(Some(this)))).unwrap_or_default();
    let bytes = text.as_bytes();
    // gc-common w20-g: the buffer is the receiver of `put` after the array's
    // allocation, which can collect; pinned across it.
    let buffer_pin = ctx.pin_native_root(buffer);
    let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
    let buffer = ctx.read_native_pin(buffer_pin, buffer);
    ctx.unpin_native_roots(buffer_pin);
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
    }
    ctx.invoke_virtual(
        buffer,
        "put",
        "([B)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(arr))],
    )?;
    Ok(None)
}

fn read_ascii_byte_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Option<String> {
    if !ctx.object_is_array(arr) {
        return None;
    }
    let len = ctx.array_length(arr);
    let mut out = String::with_capacity(len);
    for i in 0..len {
        let b = ctx.get_array_element(arr, i).as_int().unwrap_or(0) as u8;
        out.push(char::from(b));
    }
    Some(out)
}

/// Read a `HttpString` (or plain String) back into a Rust `String`.
fn read_http_string(ctx: &dyn NativeContext, v: Option<Value>) -> Option<String> {
    match v? {
        Value::Object(Some(o)) => {
            // Heuristic: if the object has ≥ 1 field and slot 0 is a
            // String, treat as HttpString; else treat the object itself
            // as a String.
            let nf = ctx.object_num_fields(o);
            if nf >= 1 {
                if let Value::Object(Some(inner)) = ctx.get_field(o, HS_FIELD_BYTES) {
                    if let Some(s) = ctx.read_string(inner) {
                        return Some(s);
                    }
                    if let Some(s) = read_ascii_byte_array(ctx, inner) {
                        return Some(s);
                    }
                }
            }
            if let Value::Object(Some(inner)) = ctx.get_field_by_name(o, "string") {
                if let Some(s) = ctx.read_string(inner) {
                    return Some(s);
                }
            }
            ctx.read_string(o)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Natives — HttpServerExchange
// ---------------------------------------------------------------------------

const UNDERTOW_RESPONSE_CODE_MASK: i32 = 0x3ff;

fn class_has_field(ctx: &dyn NativeContext, class_name: &str, field_name: &str) -> bool {
    ctx.resolve_field_index(class_name, field_name).is_some()
}

fn exchange_uses_real_layout(ctx: &dyn NativeContext, this: ObjectRef, field_name: &str) -> bool {
    class_has_field(ctx, CLS_EXCHANGE, field_name) && ctx.object_num_fields(this) > EX_NUM_SLOTS
}

fn exchange_field_or_synthetic(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    field_name: &str,
    synthetic_slot: usize,
) -> Value {
    if exchange_uses_real_layout(ctx, this, field_name) {
        ctx.get_field_by_name(this, field_name)
    } else {
        ctx.get_field(this, synthetic_slot)
    }
}

fn alloc_header_map_for_exchange(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    if class_has_field(ctx, CLS_HEADER_MAP, "table") {
        match ctx.new_object_initialized(CLS_HEADER_MAP, "()V", &[])? {
            Some(Value::Object(Some(map))) => Ok(map),
            other => Err(MethodCallFailed::InternalError(VmError::Internal {
                message: format!("HeaderMap allocation returned {other:?}"),
            })),
        }
    } else {
        let map = try_alloc_concurrent_synthetic(ctx, CLS_HEADER_MAP, HM_NUM_SLOTS)?;
        native_header_map_init(ctx, &[Value::Object(Some(map))])?;
        Ok(map)
    }
}

fn exchange_header_map_or_create(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    field_name: &str,
    synthetic_slot: usize,
) -> MethodCallResult {
    if exchange_uses_real_layout(ctx, this, field_name) {
        if let Value::Object(Some(map)) = ctx.get_field_by_name(this, field_name) {
            return Ok(Some(Value::Object(Some(map))));
        }
        let this_pin = ctx.pin_native_root(this);
        let map = alloc_header_map_for_exchange(ctx)?;
        let this = ctx.read_native_pin(this_pin, this);
        ctx.set_field_by_name(this, field_name, Value::Object(Some(map)));
        ctx.unpin_native_roots(this_pin);
        Ok(Some(Value::Object(Some(map))))
    } else {
        match ctx.get_field(this, synthetic_slot) {
            Value::Object(Some(map)) => Ok(Some(Value::Object(Some(map)))),
            _ => {
                // Mirror the real-layout branch above: `this` must survive
                // the GC-triggering `alloc_header_map_for_exchange` call
                // before being read again for `set_field`.
                let this_pin = ctx.pin_native_root(this);
                let map = alloc_header_map_for_exchange(ctx)?;
                let this = ctx.read_native_pin(this_pin, this);
                ctx.set_field(this, synthetic_slot, Value::Object(Some(map)));
                ctx.unpin_native_roots(this_pin);
                Ok(Some(Value::Object(Some(map))))
            }
        }
    }
}

fn native_exchange_get_request_method(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(exchange_field_or_synthetic(
        ctx,
        this,
        "requestMethod",
        EX_FIELD_METHOD,
    )))
}

fn native_exchange_get_request_uri(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(exchange_field_or_synthetic(
        ctx,
        this,
        "requestURI",
        EX_FIELD_URI,
    )))
}

fn native_exchange_get_request_headers(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    exchange_header_map_or_create(ctx, this, "requestHeaders", EX_FIELD_REQUEST_HEADERS)
}

fn native_exchange_get_response_headers(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    exchange_header_map_or_create(ctx, this, "responseHeaders", EX_FIELD_RESPONSE_HEADERS)
}

fn native_exchange_set_status_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let status = args.get(1).and_then(|v| v.as_int()).unwrap_or(200);
    if exchange_uses_real_layout(ctx, this, "state") {
        let state = ctx.get_field_by_name(this, "state").as_int().unwrap_or(0);
        let next = (state & !UNDERTOW_RESPONSE_CODE_MASK) | (status & UNDERTOW_RESPONSE_CODE_MASK);
        ctx.set_field_by_name(this, "state", Value::Int(next));
    } else {
        ctx.set_field(this, EX_FIELD_RESPONSE_STATUS, Value::Int(status));
    }
    Ok(Some(Value::Object(Some(this))))
}

fn native_exchange_get_response_sender(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // If a sender hasn't been allocated yet, mint one whose field 0 points
    // back to the exchange so `send()` can find the body slot.
    let existing = if exchange_uses_real_layout(ctx, this, "sender") {
        ctx.get_field_by_name(this, "sender")
    } else {
        ctx.get_field(this, EX_FIELD_RESPONSE_SENDER)
    };
    match existing {
        Value::Object(Some(s)) => Ok(Some(Value::Object(Some(s)))),
        _ => {
            // gc-common w19-d: the exchange is rooted across the sender's
            // allocation; the back-pointer and the cache slot used its entry
            // address.
            let mut scope = NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let sender: ObjectRef = try_alloc_concurrent_synthetic(&mut *scope, CLS_SENDER, 1)?;
            let this = scope.get(&this_h);
            let ctx = &mut *scope;
            ctx.set_field(sender, 0, Value::Object(Some(this)));
            if exchange_uses_real_layout(ctx, this, "sender") {
                ctx.set_field_by_name(this, "sender", Value::Object(Some(sender)));
            } else {
                ctx.set_field(this, EX_FIELD_RESPONSE_SENDER, Value::Object(Some(sender)));
            }
            Ok(Some(Value::Object(Some(sender))))
        }
    }
}

fn real_exchange_force_empty_response_body(
    ctx: &mut dyn NativeContext,
    exchange: ObjectRef,
) -> MethodCallResult {
    let Some(Value::Object(Some(headers))) =
        native_exchange_get_response_headers(ctx, &[Value::Object(Some(exchange))])?
    else {
        return Ok(None);
    };
    // `headers` and `name_text`/`name` all need to survive the two
    // `create_string` calls and the `new_object_initialized` call below
    // before being read again for the final `native_header_map_put`.
    let headers_pin = ctx.pin_native_root(headers);
    let name_text = ctx.create_string("Content-Length");
    let name_text_pin = ctx.pin_native_root(name_text);
    let name = match ctx.new_object_initialized(
        CLS_HTTP_STRING,
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(name_text))],
    )? {
        Some(Value::Object(Some(o))) => o,
        _ => ctx.read_native_pin(name_text_pin, name_text),
    };
    let name_pin = ctx.pin_native_root(name);
    let zero = ctx.create_string("0");
    let headers = ctx.read_native_pin(headers_pin, headers);
    let name = ctx.read_native_pin(name_pin, name);
    let _ = native_header_map_put(
        ctx,
        &[
            Value::Object(Some(headers)),
            Value::Object(Some(name)),
            Value::Object(Some(zero)),
        ],
    )?;
    ctx.unpin_native_roots(headers_pin);
    Ok(None)
}

fn native_sender_send(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // The sender's slot 0 is its back-pointer to the exchange.
    let exchange = match ctx.get_field(this, 0) {
        Value::Object(Some(e)) => e,
        _ => return Ok(None),
    };
    if let Some(body_v) = args.get(1).copied() {
        // Synthetic exchanges use slot 3 as a test-only response body stash.
        // Real Undertow's `HttpServerExchange` inherits one field from
        // `AbstractAttachable`, making slot 3 its actual `responseHeaders`
        // HeaderMap. Writing a body String there corrupts
        // `closeAndFlushResponse()` into `String.put(HttpString, String)`.
        if !exchange_uses_real_layout(ctx, exchange, "responseHeaders") {
            ctx.set_field(exchange, EX_FIELD_REQUEST_BODY, body_v);
        } else {
            real_exchange_force_empty_response_body(ctx, exchange)?;
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Natives — UndertowService / ListenerService (WildFly MSC-facing)
// ---------------------------------------------------------------------------

fn native_undertow_service_start(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    _ctx.set_field(this, US_FIELD_STATE, Value::Int(1)); // 1 = started
    Ok(None)
}

fn native_undertow_service_stop(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    _ctx.set_field(this, US_FIELD_STATE, Value::Int(0)); // 0 = stopped
    Ok(None)
}

fn native_listener_service_get_bound_address(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, LS_FIELD_BOUND_ADDRESS)))
}

// ---------------------------------------------------------------------------
// Handler dispatch — invokes `HttpHandler.handleRequest(exchange)` with a
// catch_unwind safety net that yields 500 on panic.
// ---------------------------------------------------------------------------

/// Invoke an `HttpHandler.handleRequest(HttpServerExchange)`. On panic,
/// the exchange's status is set to 500 and the result is `Ok(None)`.
///
/// This is used by integration tests and by the worker pool dispatch loop
/// (installed in T19.7's XNIO wire-up).
pub fn dispatch_handler(
    ctx: &mut dyn NativeContext,
    handler: ObjectRef,
    exchange: ObjectRef,
) -> MethodCallResult {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        ctx.invoke_virtual(
            handler,
            "handleRequest",
            "(Lio/undertow/server/HttpServerExchange;)V",
            &[Value::Object(Some(exchange))],
        )
    }));
    match outcome {
        Ok(r) => r,
        Err(_payload) => {
            if exchange_uses_real_layout(ctx, exchange, "state") {
                ctx.set_field_by_name(exchange, "state", Value::Int(500));
            } else {
                ctx.set_field(exchange, EX_FIELD_RESPONSE_STATUS, Value::Int(500));
            }
            Ok(None)
        }
    }
}

/// Sleep backoff on accept loop resource exhaustion. Public so T19.7's
/// accept loop can call it too.
pub fn accept_backoff() {
    thread::sleep(Duration::from_millis(100));
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register every T19.2.d native with the method registry.
pub fn register_undertow_natives(r: &mut NativeMethodRegistry) {
    // --- Undertow + Builder ---
    r.register(
        CLS_UNDERTOW,
        "builder",
        "()Lio/undertow/Undertow$Builder;",
        native_undertow_builder, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_UNDERTOW_BUILDER,
        "addHttpListener",
        "(ILjava/lang/String;)Lio/undertow/Undertow$Builder;",
        native_builder_add_http_listener, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_UNDERTOW_BUILDER,
        "addHttpsListener",
        "(ILjava/lang/String;Ljavax/net/ssl/SSLContext;)Lio/undertow/Undertow$Builder;",
        native_builder_add_https_listener, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_UNDERTOW_BUILDER,
        "setHandler",
        "(Lio/undertow/server/HttpHandler;)Lio/undertow/Undertow$Builder;",
        native_builder_set_handler, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_UNDERTOW_BUILDER,
        "setSocketOption",
        "(Lorg/xnio/Option;Ljava/lang/Object;)Lio/undertow/Undertow$Builder;",
        native_builder_set_socket_option, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_UNDERTOW_BUILDER,
        "setWorkerThreads",
        "(I)Lio/undertow/Undertow$Builder;",
        native_builder_set_worker_threads, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_UNDERTOW_BUILDER,
        "setIoThreads",
        "(I)Lio/undertow/Undertow$Builder;",
        native_builder_set_io_threads, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_UNDERTOW_BUILDER,
        "build",
        "()Lio/undertow/Undertow;",
        native_builder_build, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(CLS_UNDERTOW, "start", "()V", native_undertow_start, cratonvm_native_api::NativeKind::Bridge);
    r.register(CLS_UNDERTOW, "stop", "()V", native_undertow_stop, cratonvm_native_api::NativeKind::Bridge);

    // --- HeaderMap / HttpString ---
    r.register(CLS_HEADER_MAP, "<init>", "()V", native_header_map_init, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        CLS_HEADER_MAP,
        "put",
        "(Lio/undertow/util/HttpString;Ljava/lang/String;)Lio/undertow/util/HeaderMap;",
        native_header_map_put, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_HEADER_MAP,
        "add",
        "(Lio/undertow/util/HttpString;Ljava/lang/String;)Lio/undertow/util/HeaderMap;",
        native_header_map_add, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_HEADER_MAP,
        "contains",
        "(Lio/undertow/util/HttpString;)Z",
        native_header_map_contains, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_HEADER_MAP,
        "getFirst",
        "(Lio/undertow/util/HttpString;)Ljava/lang/String;",
        native_header_map_get, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_HEADER_MAP,
        "getFirst",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_header_map_get, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_HEADER_MAP,
        "get",
        "(Lio/undertow/util/HttpString;)Ljava/lang/String;",
        native_header_map_get, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_HEADER_MAP,
        "get",
        "(Lio/undertow/util/HttpString;I)Ljava/lang/String;",
        native_header_map_get_indexed, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_HEADER_MAP,
        "get",
        "(Ljava/lang/String;I)Ljava/lang/String;",
        native_header_map_get_indexed, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_HEADER_MAP,
        "contains",
        "(Ljava/lang/String;)Z",
        native_header_map_contains, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_HEADER_MAP,
        "count",
        "(Lio/undertow/util/HttpString;)I",
        native_header_map_count, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_HEADER_MAP,
        "count",
        "(Ljava/lang/String;)I",
        native_header_map_count, cratonvm_native_api::NativeKind::Bridge,
    );
    // Convenience string-named overload for tests.
    r.register(
        CLS_HEADER_MAP,
        "putString",
        "(Ljava/lang/String;Ljava/lang/String;)Lio/undertow/util/HeaderMap;",
        native_header_map_put, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_HEADER_MAP,
        "getString",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_header_map_get, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_HTTP_STRING,
        "<init>",
        "(Ljava/lang/String;)V",
        native_http_string_init, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_HTTP_STRING,
        "toString",
        "()Ljava/lang/String;",
        native_http_string_to_string, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_HTTP_STRING,
        "appendTo",
        "(Ljava/nio/ByteBuffer;)V",
        native_http_string_append_to, cratonvm_native_api::NativeKind::Bridge,
    );
    // Let Undertow's real Headers.<clinit> populate HttpString constants.
    // HttpRequestParser reflects over Headers/Methods/Protocols and expects
    // every static HttpString field to hold a real value.

    // --- HttpServerExchange ---
    r.register(
        CLS_EXCHANGE,
        "getRequestMethod",
        "()Lio/undertow/util/HttpString;",
        native_exchange_get_request_method, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_EXCHANGE,
        "getRequestURI",
        "()Ljava/lang/String;",
        native_exchange_get_request_uri, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_EXCHANGE,
        "getRequestHeaders",
        "()Lio/undertow/util/HeaderMap;",
        native_exchange_get_request_headers, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_EXCHANGE,
        "getResponseHeaders",
        "()Lio/undertow/util/HeaderMap;",
        native_exchange_get_response_headers, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_EXCHANGE,
        "setStatusCode",
        "(I)Lio/undertow/server/HttpServerExchange;",
        native_exchange_set_status_code, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_EXCHANGE,
        "getResponseSender",
        "()Lio/undertow/io/Sender;",
        native_exchange_get_response_sender, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_SENDER,
        "send",
        "(Ljava/lang/String;)V",
        native_sender_send, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- UndertowService / ListenerService (MSC wire-up) ---
    r.register(
        CLS_UNDERTOW_SVC,
        "start",
        "(Lorg/jboss/msc/service/StartContext;)V",
        native_undertow_service_start, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_UNDERTOW_SVC,
        "stop",
        "(Lorg/jboss/msc/service/StopContext;)V",
        native_undertow_service_stop, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        CLS_LISTENER_SVC,
        "getBoundAddress",
        "()Ljava/net/InetSocketAddress;",
        native_listener_service_get_bound_address, cratonvm_native_api::NativeKind::Bridge,
    );

    // Silence the unused-import warnings when the file is consumed only
    // via `pub fn` entry points from outside the crate.
    let _ = LS_FIELD_PORT;
    let _ = LS_FIELD_HOST;
    let _ = LS_NUM_SLOTS;
    let _ = US_FIELD_NAME;
    let _ = US_FIELD_SERVER_HANDLE;
    let _ = US_NUM_SLOTS;
    let _ = HM_NUM_SLOTS;
    let _ = HS_NUM_SLOTS;
    let _ = EX_NUM_SLOTS;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;

/// gc-common w20-g (`common-w19d-undertow-header-map-rows-never-die`): the
/// `HeaderMap` and `Undertow$Builder` rows go with their owner at the
/// post-collection sweep and with their VM at teardown. Each test owns a
/// private VM identity (the tables are process-wide statics).
#[cfg(test)]
mod w20g_undertow_row_tests;
