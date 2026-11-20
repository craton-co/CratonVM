// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Phase E — Networking natives (roadmap items RE.1 .. RE.10).
//!
//! This module implements the ten Phase-E items from
//! `roadmap-any-java-app.md` as ten self-contained subphases, each with
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
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, StreamOwned};
use cratonvm_native_api::{NativeContext, NativeHandleScope, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::lock_order::{LockLevel, OrderedPlMutex};
use cratonvm_types::{ArrayElementType, ClassId, ObjectKind, ObjectRef, Value};

use cratonvm_native_io::eintr::{is_eintr, retry_eintr, EintrIo, EintrStream};

use crate::servlet::{s2_alloc_listener_for_vm, s2_alloc_stream_for_vm, s2_registry};
use crate::{obj_arg, try_alloc_concurrent_synthetic};
/// Record that `stream` belongs to the `Socket` whose key is `owner`, with no
/// stream address (so the stream is never judged). Test-only since gc-common
/// w16-a: the natives use [`stream_owner_set_at`].
#[cfg(test)]
fn stream_owner_set(stream: NativeObjKey, owner: NativeObjKey) {
    stream_owner_set_at(stream, owner, 0);
}

///
/// One line at the top of a registration whose class an application can
/// extend. See `http_url_connection::subclass_runs_its_own_bytecode` for the
/// defect and the discriminator; the short version is that dispatch probes the
/// RECEIVER's class chain, so a native registered on `URLConnection` for this
/// VM's own carriers also claims every method a test double inherits — and
/// answers it without ever consulting the double's overrides.
macro_rules! uc_subclass_guard {
    ($ctx:expr, $args:expr, $name:literal, $desc:literal) => {
        if let Some(forwarded) =
            crate::http_url_connection::subclass_runs_its_own_bytecode($ctx, $args, $name, $desc)
        {
            return forwarded;
        }
    };
}

#[cfg(test)]
mod hostname_string_tests;

/// `fatjar-nested-classes-directory-url-openstream-FIXED-20260924.md`.
#[cfg(test)]
mod url_resource_mode_tests;
// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
/// gc-common w8-a: the `http-handlers`, `datagram-sockets` and `inet-addresses`
/// root rows scan and remap only the collecting VM's rows. Addresses are
/// fabricated and never dereferenced; every test removes what it inserted.
#[cfg(test)]
mod w8a_root_row_isolation_tests;

/// gc-common w9-b (`common-w8a-identity-hash-keyed-registries-are-shared-across-vms`):
/// the `ServerSocket` state and the stream -> owner handles are keyed by VM +
/// identity hash, so two VMs' objects that share an identity hash (every heap
/// numbers hashes from the same seed) no longer share a row.
#[cfg(test)]
mod w9b_socket_key_isolation_tests;

/// gc-common w15-a (`common-w14a-socket-stream-owner-roots-keep-every-socket-alive`):
/// a socket stream remembers its `Socket` by key, takes no global root, and
/// its row goes when the socket closes.
#[cfg(test)]
mod w15a_socket_stream_owner_tests;

/// gc-common w16-a: the owner-death sweep of the socket rows
/// (`common-w15a-dropped-unclosed-socket-keeps-its-fd-and-rows`), the
/// `SSLSessionContext` carriers and `SSLServerSocket` delegates
/// (`common-w15a-ssl-carriers-and-server-socket-delegates-rooted-until-teardown`)
/// and the `HttpServer` teardown (`common-w15a-http-server-outlives-its-vm`).
/// One VM identity per test; each test's teardown drops every row of its VMs.
#[cfg(test)]
mod w16a_net_owner_death_tests;

/// gc-common w16-a
/// (`common-w15a-httpclient-extended-trustmanager-gets-the-two-argument-check`):
/// the `HttpClient` trust gate mints the `SSLEngine` HotSpot's client would, so
/// an application `X509ExtendedTrustManager` gets the three-argument check.
/// BUILD DEPENDENCY: `t27_tls::run_client_trust_check_for_chain_with_engine`
/// (`handoff-w16a-t27-client-trust-check-with-engine`).
#[cfg(test)]
mod w16a_trust_gate_engine_tests;

/// gc-common w17-a (`common-w16a-inet-address-and-datagram-rows-root-their-keys`):
/// the `DatagramSocket` and `InetAddress` rows are weak on their key, and a
/// dead `HttpsURLConnection` carrier's recorded handshake goes with it. One VM
/// identity per test; each test's teardown drops every row of its VMs.
#[cfg(test)]
mod w17a_object_keyed_row_tests;

/// gc-common w18-f (`common-w17a-dropped-open-server-socket-keeps-its-listener-and-row`):
/// a plain `ServerSocket`'s rows (`ss_side_table` here, `server_socket_ports`
/// in `native-api`) are weak on the socket, a dead one's listener is closed,
/// and a closed but reachable one keeps its `isBound()` / `getLocalPort()`.
#[cfg(test)]
mod w18f_server_socket_row_tests;

/// gc-common w19-d handoff: `make_uri` / `make_uri_units` root the URI across
/// `uri_publish_named` (and, for the units twin, across each component String)
/// and hand back its address after them. The mock never moves an object at an
/// allocation, so these check the by-name text on a real-layout URI and that
/// no root is left behind.
#[cfg(test)]
mod w19d_uri_builder_tests;

#[cfg(test)]
mod w20d_receiver_relocation_tests;

/// gc-common w28-a (`common-w27b-net-phase-e-native-obj-key-tables-merge-live-identity-hash-colliders`):
/// the `NativeObjKey` tables are keyed by weak lock keys. The mock's identity
/// hash is the address truncated to `i32`, so two addresses 4 GiB apart are
/// two live objects sharing one identity hash (never dereferenced). Each test
/// has private VM identities, and its guard forgets only those VMs.
#[cfg(test)]
mod w28a_net_obj_key_tests;

mod archive_stamp;
#[allow(unused_imports)]
pub use archive_stamp::*;
#[allow(unused_imports)]
use archive_stamp::*;
mod uri_raw_string;
#[allow(unused_imports)]
pub use uri_raw_string::*;
#[allow(unused_imports)]
use uri_raw_string::*;
mod accept_and_socket_ops;
#[allow(unused_imports)]
pub use accept_and_socket_ops::*;
#[allow(unused_imports)]
use accept_and_socket_ops::*;
mod register_re4_url_http;
#[allow(unused_imports)]
pub use register_re4_url_http::*;
#[allow(unused_imports)]
use register_re4_url_http::*;
mod header;
#[allow(unused_imports)]
pub use header::*;
#[allow(unused_imports)]
use header::*;
mod inet_address_numeric_literal;
#[allow(unused_imports)]
pub use inet_address_numeric_literal::*;
#[allow(unused_imports)]
use inet_address_numeric_literal::*;
mod real_jdk_response_body;
#[allow(unused_imports)]
pub use real_jdk_response_body::*;
#[allow(unused_imports)]
use real_jdk_response_body::*;
