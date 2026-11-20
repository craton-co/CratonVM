// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Built-in native method implementations.
//!
//! These are the essential native methods needed for the minimum viable
//! interpreter. More will be added as standard library support grows.

#![allow(clippy::collapsible_if, clippy::needless_range_loop, dead_code)]

use crate::lang_class::ClassTextName;
use cratonvm_native_api::{
    NativeCallback, NativeContext, NativeHandleScope, NativeKind, NativeMethodRegistry,
};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::ClassId;
use cratonvm_types::{ObjectRef, Value};
#[cfg(test)]
mod bootstrap_property_fallback_tests;
/// Per-call-site capability gates for this crate's native surface.
///
/// `grep capability_gate:: native-builtins/src` is the complete list of gated
/// sites here; `docs/security/capability-wiring.md` records which of the audit
/// rows in `docs/security/native-capabilities.md` they close.
pub mod buffer_session;
pub mod capability_gate;
pub mod case_map;
/// One proleptic-Gregorian calendar for the crate. Four modules had their
/// own copy of the same wrong one; see the module doc.
pub mod civil_date;
/// Per-thread, per-VM memo of the name-derived class facts `Object.hashCode`
/// and `Class.isArray` ask per call (JIT round 17 wave 2, lane nio2;
/// `CRATONVM_CLASS_FACTS_MEMO`, default ON).
pub mod class_facts_memo;
pub mod compat_carriers;
/// Descriptor-safe instance-field readers.
///
/// `NativeContext::get_field_by_name` is NOT descriptor-aware: an unwritten
/// reference slot reads back as `Value::Int(0)` through it and as
/// `Value::Object(None)` through the indexed `get_field`. Natives that test a
/// by-name read for null, or return one straight out of a reference-typed
/// method, are asking the wrong question and get a plausible answer. See
/// `docs/feature-designs/by-name-field-reads.md`.
pub mod field_read;
/// The proleptic Julian calendar and `GregorianCalendar`'s pre-1582 hybrid
/// cutover. Sibling to `civil_date`, which must stay pure proleptic Gregorian
/// (it backs `java.time`); see the module doc.
pub mod julian_date;
pub mod lang_class;
pub mod lang_string;
/// Per-thread, per-VM memo of `class mirror -> ClassId` keyed by the GC pause
/// generation (JIT round 17 wave 8, lane nio3;
/// `CRATONVM_CLASS_ID_FROM_MIRROR_MEMO`, opt-in). Public: the VM's
/// `NativeContext::class_id_from_mirror` is its one caller.
pub mod mirror_id_memo;
/// zr z11/nd: the natives the VM's native funnel may re-run after a
/// collection when one of their allocations refused (ZGC).
pub mod oom_restartable;
// WP2.1: java.lang.reflect full coverage — supplements lang_class.rs with
// new natives (trySetAccessible, canAccess, getEnclosingClass, Parameter
// helpers, Executable.getParameters, Method.getDefaultValue, etc.).
pub mod lang_reflect;
// WP4.7: Real StampedLock + ReentrantReadWriteLock backends.
pub use cratonvm_native_builtins_crypto::{bc_aes, bc_chacha, bc_newhope};
pub mod lang_invoke;
// Interpreter round i4 wave 89, lane L4: `CRATONVM_DBG=mh-finder-census`.
pub mod mh_finder_census;
// Interpreter round i6 wave 116, lane L5: `CRATONVM_DBG=zip-stream-census`.
pub mod zip_stream_census;
pub mod lang_math;
pub mod lang_misc;
pub mod lang_system;
/// gcg g3/k: the lock-key registry's shards (`CRATONVM_LOCK_KEY_SHARDS`).
pub mod lock_key_shards;
pub mod phases_early;
pub mod phases_late;
/// The served reflective doors' per-phase census
/// (`CRATONVM_REFLECT_DOOR_PHASE_STATS`; interpreter round i3 wave 77, lane L3).
pub mod reflect_door_phases;
/// `jdk.internal.reflect.Reflection`'s `fieldFilterMap`/`methodFilterMap`,
/// reimplemented — see `core-reflection-has-no-member-filter-20260911.md`.
pub mod reflect_filter;
/// The VM's stamps for that census's `gap` and `call` splits (interpreter
/// round i3 wave 81, lane L3).
pub use reflect_door_phases::{reflect_door_gate_stamp, ReflectDoorGatePoint};
/// `jdk.internal.util.Preconditions` — bounds checks that honour the exception
/// formatter their caller passed, rather than throwing one class for everyone.
pub mod preconditions;
pub mod stamped_lock;
pub mod tzdb;
pub mod uncaught_handlers;
#[cfg(feature = "synthetic-jdk")]
pub mod util_time;
// T19_K3_PROPS_SIDETABLE — robust java.util.Properties storage so
// KeycloakMain.<clinit>'s Version.<clinit> path
// (Class.getResourceAsStream → Properties.load → getProperty) returns
// the loaded value instead of NPE.
pub mod charset;
pub mod classloader_value_sidetable;
pub mod ffm_fallback_linker;
pub mod properties_sidetable;
pub mod servlet;
pub mod sysprops_merge;
// T19_H14_OPENMBEAN — JMX OpenType / MXBean introspector translation
// natives (always compiled, since real-JDK mode also reaches the
// MXBean introspection path during KC16 boot).
pub mod date_format_fast;
pub mod http2;
pub mod t27_tls;
pub mod t27_tls_cbc;
pub mod tls_carriers;
// Which TLS alert a client-side `TrustManager` rejection becomes. Shared by the
// verifier-time path (`PassthroughServerCertVerifier::verify_server_cert`) and
// the post-handshake one (`reject_peer_with_fatal_alert`), which had already
// drifted apart once — the newer path answered `access_denied` for every
// rejection where the older one answered `certificate_unknown`.
pub mod t3_impl;
pub mod tls;
pub mod tls_cert_alert;
// Deny-by-default guard for the JSSE socket-factory surface: an
// `SSLSocketFactory`/`SSLServerSocketFactory` overload with no TLS bridge must
// raise rather than inherit `javax.net.{Socket,ServerSocket}Factory`'s
// plaintext implementation. See `tls-and-jca-failure-audit.md`.
pub mod tls_deny;
// Step 1 of the limb-based BigInteger rewrite (biginteger-limb-rewrite-scope.md).
// Additive only — nothing routes through it yet; later steps migrate the
// BigInteger natives off the O(digits^2) decimal-string primitives onto this.
pub mod bigint;
// Montgomery modular arithmetic shared by both limb bignums (`bigint::BigInt`
// behind java.math.BigInteger.modPow, and `crypto_impl::BigUint` behind the
// native RSA private-key path). Retires
// `perf/biginteger-modpow-has-no-montgomery-reduction-20260817`.
pub mod montgomery;
// Real crypto primitives (RustCrypto SHA/AES/RSA/ECDSA/X509/keystore). Always
// compiled — the always-on JCA/TLS/x509 paths depend on it. Previously this was
// a submodule of the feature-gated `crypto` module, which broke the
// `--no-default-features` build; it now lives at the top level. There is no
// `crypto.rs` re-export shim: the comment used to promise one, and the
// `legacy-synthetic-crypto` call sites that still said `crate::crypto::`
// were simply unbuildable under that feature until 2026-07-30.
pub mod classfile_api;
#[allow(dead_code)]
pub mod classloader;
pub mod classloader_real;
// RFC 8439 ChaCha20 / Poly1305 / ChaCha20-Poly1305 AEAD. Separate from
// `crypto_impl` because it is a self-contained primitive with the RFC's own
// vectors beside it, and because `crypto_impl`'s ChaCha was a keystream-only
// SecureRandom fallback that could not encrypt anything.
pub mod chacha20;
pub mod crypto_impl;
// SBR wave 5 (lane cryptoeng): reviewed `Intrinsic`s for the SUN/SunJCE
// `@IntrinsicCandidate` kernels (digest compression, AES and its modes,
// GHASH, ChaCha20, Poly1305). See the module doc.
pub mod crypto_intrinsics;
pub mod jboss_module_xml;
pub mod jboss_resource_loader;
// T19.H4 — LocalModuleLoader + DefaultBootModuleLoaderHolder.INSTANCE.
// Unblocks KC16 "Cannot invoke loadModule on null" by returning a real
// `LocalModuleLoader` from the boot holder and wiring `loadModule`
// to parse `<mp>/<module>/main/module.xml` into a populated Module.
pub mod jboss_module_loader;
pub mod jboss_msc;
pub mod wildfly_core;
// T19.2.b — WildFly Naming (JNDI) subsystem.  InitialContext /
// ServiceBasedNamingStore / ContextNames natives sitting on top of
// T19.1's MSC container + a process-wide binding store.
pub mod service_loader;
pub mod wildfly_naming;
// WP7.1 — DriverManager + ServiceLoader registration for JDBC SPI.
#[cfg(feature = "experimental-aot")]
pub mod aot;
#[cfg(feature = "experimental-aot")]
pub mod aot_pipeline;
#[cfg(feature = "experimental-aot")]
pub mod cds;
pub mod generics;
pub mod graalvm_compat;
pub mod jdbc;
pub mod jdk25_concurrency;
pub mod jdk25_language;
pub mod jdk25_patterns;
pub mod letsgo_compat;
pub mod locale_bootstrap;
pub mod locale_resources;
pub mod throwable_mint;
// Round nio20 wave 3, lane L5: opt-in windowed `ScopedMemoryAccess` twins.
pub mod scoped_twins;
// `register_synthetic_overrides` (gated `synthetic-jdk`) calls
// `serialization::register_byte_array_output_stream`, so the module must also be
// compiled in under `synthetic-jdk` — otherwise `--features synthetic-jdk` fails
// to build (E0433: unresolved module `serialization`), which is what bitrotted
// the synthetic-jdk gated test build.
#[cfg(any(feature = "experimental-serialization", feature = "synthetic-jdk"))]
pub mod serialization;
pub mod spring_startup_bootstrap;
pub mod unsafe_jdk25;
pub mod unsafe_natives;
pub mod vector_api;
// The real-JDK half of the Vector API: `vector_api` is the synthetic
// implementation and is unreachable when the real `jdk.incubator.vector` is on
// the module path, because a real receiver is a concrete `Int256Vector`. This
// one intercepts `jdk.internal.vm.vector.VectorSupport`, which every lane
// operation funnels through in BOTH modes.
pub mod vector_support_intrinsics;
// WP2.3-B — `MethodHandles.Lookup.defineClass` /
// `defineHiddenClass` / `defineHiddenClassWithClassData` natives.
// Routes through `NativeContext::define_class_full` with
// skip_verification + nest-host inheritance from the lookup class.
pub mod boot_loader;
/// Third-party compression JNI shims (snappy-java + zstd-jni) for Kafka codecs.
pub mod compression_native;
/// JDK-25 surface baselines (`scripts/baselines/jdk25-*.tsv`) plus the `audit`
/// / `audit_off_surface` checks that compare our registrations against them.
/// Declared here deliberately: without this line the module compiles in no
/// build, so its tests are not a gate on anything.
pub mod jdk_baseline;
pub mod keystore;
pub mod lookup_define;
pub mod security_manager;
pub mod system_bootstrap;
/// `java.util.zip.CRC32C` native overrides (Castagnoli CRC-32C).
pub mod zip_crc32c;
pub mod zip_real;
// WP6.7 — `java.security.SecureRandom` (OS CSPRNG) + `java.util.Random`
// (deterministic LCG matching the JDK seed contract).  Lives outside the
// `legacy-synthetic-crypto` cfg-gate so it is always compiled in real-JDK
// mode where the previous `lib.rs` Random aliases produced non-
// deterministic output.
pub mod securerandom;
// WP5.3: X509KeyManager / X509TrustManager per-connection. Added here so the
// module is reachable from `cargo test -p cratonvm-native-builtins --lib
// x509_manager`.  Wave-coordinator wires the registration call separately.
pub mod x509_manager;
// WP6.x — JCA umbrella.  Currently only `jca::cipher` is wired
// (Cipher class-init shim used by the WP6.3 probe at
// apps/cipher_probe/CipherProbe.java).  Future signature/mac modules
// land alongside `cipher` here.
pub mod jca;
pub mod apps_h2;
pub mod deprecated_internal;
pub mod deprecated_io_util;
pub mod deprecated_lang;
pub mod deprecated_util;
pub mod deprecated_verify;
pub mod net_phase_e;
// CGLIB / Spring `ConfigurationClassEnhancer.enhance` minimal bytecode emitter.
// Registered AFTER `net_phase_e::register_phase_e_networking` so it wins.
pub mod cglib_enhancer;
// T16.8: Reference types (Reference / Weak / Soft / Phantom / ReferenceQueue)
pub mod reference;
// Phase 3 (Item P3-4): `craton.gpu.internal.Native` shims. The whole
// module is feature-gated; with `gpu-offload` off the registrar is a
// no-op and no GPU code is compiled.
pub mod craton_gpu;
// T16.9: Stream terminal / Flow.Subscriber overrides
pub mod streams;
// WP4.5 — periodic-task pump driven from inside `Thread.sleep` /
// `awaitTermination` so `ScheduledExecutorService.scheduleAtFixedRate`
// fires its runnable on schedule despite `NativeContext` being non-Send.
pub mod scheduled_pump;
// T19.3: Quarkus static-init replay (io.quarkus.runtime.RuntimeValue / StartupContext / Timing)
pub mod quarkus_staticinit;
// T19.4: Quarkus ArC CDI container — Arc.initialize(), container(), beanManager(),
// ArcContainer.instance(Class), InstanceHandle, BeanManager, @Inject field walk.
pub mod quarkus_arc;
// T19.8: Agroal (Quarkus) + IronJacamar (WildFly) JDBC connection pool natives.
pub mod agroal_pool;
pub mod ironjacamar_pool;
// T19.2.e: WildFly Datasources subsystem (JNDI-bound DataSourceService) +
// Narayana-shaped JTA TransactionManager / UserTransaction / XAResource.
pub mod wildfly_datasources_tx;
// T19.2.c: WildFly Security — JAAS LoginContext / LoginModule chain, Subject /
// Principal, SecurityDomainService, and SecurityIdentity surface.
pub mod wildfly_security;
// T19.2.d: WildFly Undertow HTTP subsystem + IO integration (listener lifecycle,
// HTTP/1.1 parser, HttpHandler dispatch with panic safety).
pub mod wildfly_undertow;
// T19.7.b: JBoss XNIO worker — top-level XnioWorker with bounded I/O + task
// thread pools. The per-I/O-thread event loop body is T19.7.c territory; this
// module owns the thread lifecycle, round-robin dispatch, and OptionMap
// resolution.
pub mod xnio_worker;
// T19.7.c: XnioIoThread + event-loop internals (select-driven task queue,
// scheduled-task min-heap, panic-safe dispatch, cross-thread execute wakeup,
// XnioExecutor.Key cancellation).
pub mod xnio_io_thread;
// gcj j3/rt3: the per-VM dead-thread queues of the XNIO / Vert.x threads.
// T19.7.d: XNIO Conduit stream channels (ConduitStreamSourceChannel /
// ConduitStreamSinkChannel) + ChannelListener dispatch the event loop uses
// to invoke Java-side I/O callbacks with panic safety.
pub mod xnio_conduits;
// T19.7.e: XNIO OptionMap + IoFuture + Options + XnioExecutor. Configuration
// and async-future primitives shared by the worker (T19.7.b), IO thread
// (T19.7.c), and conduit (T19.7.d) layers.
pub mod xnio_async;
// T19.10: Infinispan local-mode cache — DefaultCacheManager + Cache backed
// by a per-VM, ordered-lock `HashMap` store with LRU eviction, TTL, and
// listener dispatch; Java values are global roots of the owning VM
// (gc-common w30-a). Keycloak realm/session caches.
pub mod infinispan_local;
// T19.6: Vert.x / Netty NioEventLoop affinity scheduler natives
// (io.vertx.core.impl.VertxImpl + io.netty.channel.nio.NioEventLoop).
pub mod vertx_eventloop;
// T19.H2: StackWalker boot-time natives (getInstance variants + getCallerClass)
// that unblock KC16 / WildFly / JBoss Modules StackWalker.<clinit> feature
// detection path.
pub mod lang_stackwalker;
pub mod stack_walker;
// T19.H2: JBoss Modules `JDKSpecific` boot-time natives — ModuleLayer.boot()
// / findModule / Module.getPackages with pre-populated JDK package set and
// module-name input validation.
pub mod jboss_jdkspecific;
// T19.H3: java.util.logging.LogManager + org.jboss.logmanager.LogManager
// singleton + Logger registry (fixes KC26 ClassCastException where
// LogManager.getLogManager() was returning a Class mirror).
pub mod logmanager;
// T19.H5: AtomicReferenceFieldUpdater / AtomicIntegerFieldUpdater /
// AtomicLongFieldUpdater `newUpdater` factories — unblock
// `org.jboss.logmanager.ExtHandler.<clinit>` ClassCastException on
// reflective cast.
pub mod atomic_updater;
// The Windows attach provider's two `listVirtualMachines()` natives.
// `#![cfg(windows)]` inside the module, and gated again here, because the
// LINUX class of the same name declares neither: a `Bridge` registered
// against a method the runtime image does not have is what the bridge
// ratchet exists to catch. See the module's own doc comment for the
// `UnsatisfiedLinkError` this closes and for why only two of the four are
// registered.
#[cfg(windows)]
pub mod attach_provider;
// T19_H13_BIGINTEGER_INTRINSICS — `java.math.BigInteger.implSquareToLen` /
// `shiftLeftImplWorker` / `shiftRightImplWorker` / `implMulAdd` / `mulAdd`
// HotSpot-equivalent native overrides. KC16 boot path constructs a 2048-bit
// BigInteger during the SunJCE provider's <clinit> and squares it; without
// the intrinsic the JDK's Java fallback bytecode pegged main thread CPU for
// tens of seconds, missing the watchdog safepoint. Real HotSpot replaces
// these with hand-rolled C2 — we provide spec-correct Rust equivalents.
pub mod biginteger_intrinsics;
// Byte-identical native intrinsic for the SunEC P-256 Montgomery field
// multiply/square (dominant cost of EC keygen/sign/verify).
pub use cratonvm_native_builtins_security::sunec_intpoly;
// Gated (default-off) coarse native EC scalar-multiply via the p256 crate,
// bypassing the one-time generator-table precompute.
pub use cratonvm_native_builtins_security::sunec_point;

// WP1.4 — `jdk.internal.access.SharedSecrets` bridge: 15 *Access
// interface singletons + every per-interface method.  Unblocks
// callers that look up `SharedSecrets.getJavaLangAccess()` /
// `getJavaSecurityAccess()` / etc. during boot and invokeinterface
// a method on the returned singleton.  See
// `vm/src/runtime/shared_secrets.rs` for the architecture.
pub mod shared_secrets_bridge;

// WP5.5: jdk.internal.net.http.* — JDK 11+ java.net.http.HttpClient
// implementation with HTTP/1.1, HTTP/2, ALPN-driven dispatch, sync + async
// send, and a process-wide connection pool. Reuses HPACK static table from
// http2.rs (read-only).
pub mod http_client;
// WP5.6: sun.net.www.protocol.http(s).HttpURLConnection — legacy
// java.net.URL.openConnection() native surface. Reuses the same TCP+TLS
// plumbing as http_client but with the older blocking semantics.
pub mod http_url_connection;
// WP5.7: java.net.PlainSocketImpl / sun.nio.ch.NioSocketImpl /
// java.net.PlainServerSocketImpl — legacy classical-blocking-I/O surface
// the public java.net.Socket / ServerSocket delegate to.
pub mod plain_socket;
// WP5.8: java.net.Inet4AddressImpl / Inet6AddressImpl real getaddrinfo + PTR.
pub mod inet_address;
// WP5.9: sun.net.spi.DefaultProxySelector — JVM args + env-driven proxy
// selection with NO_PROXY / nonProxyHosts pattern matching.
pub mod proxy_selector;
// Wave1.D: javax.xml.stream (StAX) cursor API backed by quick-xml. Required
// by Maven, Spring, Hibernate, every build/runtime that consumes XML.
pub mod xml_stax;

pub mod antlr_intrinsics;
/// Third-party application-shim registration seams (BouncyCastle,
/// JBoss/WildFly/XNIO, the JCA pools, ANTLR/ByteBuddy/Mockito/Hibernate).
/// Grouping only — every family below is still registered unconditionally.
pub mod app_shims;
/// Interpreter intrinsic table — fast-path dispatch for hot JDK methods.
/// See `feature_roadmap_interpreter_intrinsic_table.md`.
pub mod intrinsics;
pub mod logging_shims;
pub mod lucene_es;
pub mod math_bignum;
pub mod messaging_shims;
pub mod net_uri_inet;
pub mod orm_hibernate;
pub mod reflect_annotations;
pub mod regex_matcher;
pub mod test_frameworks;
pub mod unsafe_natives_ext;
pub mod util_concurrent_ext;
pub mod xml_xerces;
pub use antlr_intrinsics::*;
pub use logging_shims::*;
pub use lucene_es::*;
pub use math_bignum::*;
pub use messaging_shims::*;
pub use net_uri_inet::*;
pub use orm_hibernate::*;
pub use reflect_annotations::*;
pub use regex_matcher::matcher_realjdk_leaf_search_cached;
pub use regex_matcher::*;
pub use test_frameworks::*;
pub use unsafe_natives_ext::*;
pub use util_concurrent_ext::*;
pub use xml_xerces::*;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

#[cfg(test)]
mod mapper_memo_apply_tests;

use http2::*;
use lang_class::*;
use lang_invoke::*;
use lang_math::*;
use lang_misc::*;
use lang_string::*;
use lang_system::*;
use panama::*;
use phases_early::*;
use phases_late::*;
use servlet::*;
use tls::*;
#[cfg(feature = "synthetic-jdk")]
use util_time::*;
// ---------------------------------------------------------------------------
// Context ClassLoader storage (Thread.get/setContextClassLoader)
// ---------------------------------------------------------------------------

use std::sync::Mutex;
use std::sync::OnceLock;
#[cfg(test)]
mod context_class_loader_tests;
// ===========================================================================
// G32 — java.util.Formatter's lifecycle: the sink's TYPE and the closed flag
// ===========================================================================

#[cfg(test)]
mod g32_formatter_tests;
#[cfg(test)]
mod bare_native_library_name_tests;
#[cfg(test)]
mod r19_funnel3_fresh_result_tests;
#[cfg(test)]
mod r20_qmisc_getclass_reports_only_itself_tests;
/// Interpreter round i3 wave 75, lane L4: `CRATONVM_OBJECT_CLONE_CLASS_FACTS`.
/// The switch is set explicitly; the end-to-end rows are
/// `L4W75CloneClassFactsBench.java`.
#[cfg(test)]
mod i75_l4_clone_class_facts_tests;
/// `cargo test -j 5 -p cratonvm-native-builtins --lib gci_i30_rp_`.
#[cfg(test)]
mod gci_i30_rp_owner_list_tests;
/// zt t7/cl2: the print-loss notes (`CRATONVM_DBG_PRINT_LOSS`).
#[cfg(test)]
mod zt7cl2_print_loss_tests;
#[cfg(test)]
mod class_atomic_side_store_tests;
// Reviewed scalar Internal bodies: retain the session and base as native
// arguments throughout the call (the JDK's reachabilityFence), keep the close
// window over both the authoritative session check and the Unsafe operation,
// and publish a verdict only after that check and the per-VM capability check.
macro_rules! scoped_scalar_access {
    ($name:ident, $access:ident) => {
        pub fn $name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                    let _window = crate::ffm_fast::AccessWindow::open();
            let epoch = crate::ffm_fast::epoch();
            crate::phases_late::foreign_ffm::p67_check_scoped_session_arg(ctx, args, 1)?;
            crate::capability_gate::gate_raw_memory_any(ctx)?;
            if let Some(Value::Object(Some(session))) = args.get(1) {
                crate::phases_late::foreign_ffm::p67_note_checked_session(ctx, *session, epoch);
            }
            let adapted = scoped_memory_access_unsafe_args(args);
            $access(ctx, &adapted)
        }
    };
}
scoped_scalar_access!(native_scoped_memory_get_short, native_unsafe_get_short_mb);

/// FIX(test-isolation): the `unsafe_arena` off-heap store is process-global and
/// reuses freed addresses, so tests that allocate→free→assert-evicted race when
/// a concurrent test reallocates the just-freed address mid-assertion. All
/// arena-mutating tests (across `deprecated_internal` and `unsafe_natives`)
/// serialize on this shared lock so each test's alloc/free/assert window is
/// exclusive. Test-only; production paths are unaffected.
#[cfg(test)]
pub fn arena_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod i66_vm_internal_wait_tests;
#[cfg(test)]
mod java_predefined_class_tests;
#[cfg(test)]
mod w18f_string_hashset_tests;
#[cfg(test)]
mod cyclic_barrier_tests;
#[cfg(test)]
mod base64_tests;
#[cfg(test)]
mod r15w7_drain_a_charset_name_tests;
/// Round 14 wave 2 (lane bigdec): the two legacy conversions' fast roads
/// against their digit-at-a-time loops (reproduced here as the reference).
#[cfg(test)]
mod r14w2_bigdec_mag_words_tests;
#[cfg(test)]
mod async_handoff_sleep_tests;
/// `LockLevel`-free leaf: no path holds it while calling into the VM or taking
/// another lock. The sweeps evaluate their liveness predicate (which may take
/// the old-gen lock) with it released.
///
/// gcg g3/k: the registry is [`lock_key_shards`]' shard array. A per-key
/// function locks `lock_key_shards::lock_key_shard(hash)`; a whole-registry
/// walker visits `lock_key_shards::lock_key_shards_in_use()` one shard at a
/// time. With `CRATONVM_LOCK_KEY_SHARDS` off (the default) both are shard 0,
/// the old single registry. This accessor (shard 0) is for the test helpers
/// that inspect the registry directly.
#[cfg(test)]
fn lock_key_registry() -> &'static parking_lot::Mutex<LockKeyRegistry> {
    lock_key_shards::unsharded_registry()
}

#[cfg(test)]
mod class_module_slot_tests;
/// `addr` lies in one of `spans` (`(start, len)`, sorted by `start`,
/// non-overlapping) -- `vm/src/memory/addr_keyed.rs::in_spans`, with a bounds
/// check first because most slots lie outside a slice's range. The registry
/// itself reads its address index since common-w26c item 1; this is the
/// reference the tests compare the index path against.
#[cfg(test)]
#[inline]
fn lock_key_in_spans(addr: usize, spans: &[(usize, usize)]) -> bool {
    let (Some(&(lo, _)), Some(&(last, last_len))) = (spans.first(), spans.last()) else {
        return false;
    };
    if addr < lo || addr >= last.saturating_add(last_len) {
        return false;
    }
    let i = spans.partition_point(|&(start, _)| start <= addr);
    i > 0 && addr - spans[i - 1].0 < spans[i - 1].1
}

/// gc-common w14-c (`common-w13a-lock-key-registry-keeps-a-slot-per-object-ever-keyed`).
/// The mock's identity hash is the address truncated to `i32`, so addresses
/// 4 GiB apart share a hash (a relocation keeps it, as in the real heap), and
/// an address with zero low bits has hash 0. Never dereferenced. Each test
/// has its own VM identity: the registry is process-wide.
#[cfg(test)]
mod w14c_lock_key_sweep_tests;

/// gc-common w9-b (`common-w8a-identity-hash-keyed-registries-are-shared-across-vms`).
#[cfg(test)]
mod w9b_lock_key_vm_isolation_tests;

/// gc-common w25-a (`common-w24a-lock-key-registry-merges-live-identity-hash-colliders`).
/// The mock's identity hash is the address truncated to `i32`, so two
/// addresses 4 GiB apart share a (non-zero) hash, as two objects do in a real
/// heap once its identity-hash counter has wrapped; a move keeps the low 32
/// bits, as a real move keeps the hash. Never dereferenced. Each test owns its
/// VM identity: the registry and the `stamped_lock` tables are process-wide.
#[cfg(test)]
mod w25a_lock_key_collider_tests;

/// gc-common w26-c (`common-w25a-lock-key-sweep-walks-the-whole-registry-per-concurrent-sweep-slice`):
/// the freed-span entry point a concurrent-sweep slice calls. Same mock
/// conventions as the modules above (identity hash = the address truncated to
/// `i32`; never dereferenced); each test owns its VM identities, because the
/// registry and the `stamped_lock` tables are process-wide.
#[cfg(test)]
mod w26c_lock_key_span_drop_tests;
#[cfg(test)]
mod striped64_base_slot_tests;
#[cfg(test)]
mod r14_compat_exception_to_string_tests;
#[cfg(test)]
mod list_itr_protocol_tests;
#[cfg(test)]
mod proxy_class_cache_unload_tests;
#[cfg(test)]
mod vector_support_essential_tests;

#[cfg(test)]
mod base64_encoder_tests;

#[cfg(test)]
mod nio_heap_byte_buffer_tests;

#[cfg(test)]
mod charset_alias_tests;

#[cfg(test)]
mod reflection_field_essential_tests;

#[cfg(test)]
mod i74_l2_stack_trace_element_of_registration_tests;

#[cfg(test)]
mod i76_l2_stack_trace_element_to_string_registration_tests;

#[cfg(test)]
mod panama_essential_tests;

#[cfg(test)]
mod liquibase_checksum_tests;

// ===========================================================================
// T2.3.5 — java.util.Random natives
// ===========================================================================
#[cfg(test)]
mod t2_random_tests;

// ===========================================================================
// T2.6 — java.security.* acceptance tests
//
// These tests pin the roadmap acceptance criteria for T2.6.19 and T2.6.20
// and also cross-check the SHA-3 and Cipher additions from T2.6.2/4/5. They
// exercise the Rust-level primitives the synthetic natives delegate to, so
// a failure here means the crypto backend itself is wrong regardless of
// how the JVM dispatch is wired.
// ===========================================================================
#[cfg(test)]
mod t2_6_crypto_acceptance_tests;
/// gc-common w21-a (`common-w20f-process-wide-caches-of-vm-local-global-root-handles-FIXED-20260923`).
#[cfg(test)]
mod w21a_charset_cache_tests;
/// `Integer.toString(int, int)` / `Long.toString(long, int)` conformance.
///
/// Every expectation here was read off real JDK 25.0.3, not derived from this
/// implementation.
#[cfg(test)]
mod radix_to_string_tests;

/// The Throwable `<init>` rule is allowed to exist at ONE place in this crate.
///
/// Background: the rule "register these four fixed `<init>` descriptors on a
/// list of throwable class names" was implemented at five sites. E34 replaced
/// site 1 (`lang_misc::register_throwable_subclass_natives`) with a per-class
/// table derived from JDK 25 by reflection, and mirrored it into the synthetic
/// stub declarations in `classloading/src/class_manager.rs`. E43 deleted the
/// other copies in this file. The copies were not harmless: they ran LATER, and
/// `NativeMethodRegistry::register` is last-write-wins with no unregister API,
/// so the blanket four buried the measured table for ~52 classes and kept 85
/// descriptors registered that the real JDK class does not declare at all.
///
/// The failure mode this test exists to catch is the one that already happened
/// four times: someone needs one more exception constructor, finds a
/// convenient list of exception class names in this file, and adds a blanket
/// loop next to it. That is silent — nothing errors, the registry just answers
/// with the wrong body for fifty classes.
///
/// This is a SOURCE witness. It reads this crate's own working-tree sources
/// rather than a built registry, because the two boot paths that reach the
/// collapsed sites live behind different feature configurations
/// (`register_synthetic_overrides` is `#[cfg(feature = "synthetic-jdk")]`,
/// `reflect_annotations`' promotion is the real-JDK one) and a `#[cfg(feature
/// = ...)]` test only guards the configuration it is compiled into. A source
/// scan guards both, and it runs with no VM boot.
///
/// Non-vacuity, since a source witness that finds nothing passes loudly:
/// every lookup below panics with a named message when it misses, the two
/// extracted function bodies are size-checked, the parsed table is
/// row-count-checked, and the allow-list is checked to be exactly as small as
/// it claims — including that its one member is genuinely absent from the
/// table, so the exemption expires the moment the table absorbs it.
#[cfg(test)]
mod throwable_ctor_single_table_witness;

#[cfg(test)]
mod g23_nomination_witnesses;

#[cfg(test)]
mod g52_object_clone_tests;

/// JIT round 21 wave 8, lane reflect (`CRATONVM_MH_CTOR_REGISTERS_FINALIZABLE`):
/// the `linkToSpecial` of a `<init>` member offers its receiver to the
/// finalizer registration hook once, before the constructor runs; any other
/// member, and a null receiver, offer nothing.
#[cfg(test)]
mod r21_link_to_special_ctor_registration_tests;

/// gc-common w20-c (`common-w19a-lib-rs-identity-hash-keyed-side-tables`).
/// The mock's identity hash is the address truncated to `i32`, so one fake
/// address in two VMs is two objects with one identity hash -- what every
/// heap's sequential numbering produces. The addresses are never
/// dereferenced and end in 0. Each test has its own VM identities, because
/// the lock-key registry and these tables are process-wide.
#[cfg(test)]
mod w20c_lib_side_table_tests;

/// gc-common w29-b (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// lane 2): the `lib.rs` core-object-state tables moved off `(VM, identity
/// hash)` keys. The mock's identity hash is the address truncated to `i32`
/// (lesson qq), so two addresses 4 GiB apart share a hash, as two live objects
/// of one heap do once its counter has wrapped; the premise is asserted first
/// (lesson mm). Fake addresses are never dereferenced. Every test owns its VM
/// identity (`0xB29_2xxx`) and forgets it in a guard: the lock-key registry
/// and these tables are process-wide.
#[cfg(test)]
mod w29b_core_object_state_tests;
/// Interpreter round i5 wave 103, lane L6 (page
/// `i102-L6-scoped-value-cache-outlives-its-thread`): the evidence that a
/// thread's scoped-value cache dies with it, without a census run.
#[cfg(test)]
mod i103_l6_scoped_value_cache_tests;
#[cfg(test)]
mod r22w11_intrinsics_last_index_of_str_tests;

#[cfg(test)]
mod jr21_reflect3_agree_memo_tests;

/// netty-parity (2026-10-28): the `--compatible` fast-boot phase-1 arm published
/// `VM.savedProps` before `VM` was loaded, so the write was dropped and the
/// first `ClassLoaders.<clinit>` threw `IllegalStateException: Not yet
/// initialized` (every `String.format("%d", ..)` under the CLDR adapter).
#[cfg(test)]
mod fast_boot_saved_props_tests;

#[cfg(test)]
mod i45x_module_qualified_null_target_tests;

/// hash21: `Object.hashCode` owes the synthetic reflection stubs their STRUCTURAL
/// hash, and nothing else -- in particular not an array whose COMPONENT class is
/// one of those interfaces (an array's header class id is its component's).
#[cfg(test)]
mod hash21_object_hash_code_stub_tests;

#[cfg(test)]
mod addr_memo_tests;

mod lib_parts;
#[allow(unused_imports)]
pub use lib_parts::dbg_toarray_enabled::*;
#[allow(unused_imports)]
use lib_parts::dbg_toarray_enabled::*;
#[allow(unused_imports)]
pub use lib_parts::essential_dur_parse::*;
#[allow(unused_imports)]
use lib_parts::essential_dur_parse::*;
#[allow(unused_imports)]
pub use lib_parts::register_essential_natives_with::*;
#[allow(unused_imports)]
use lib_parts::register_essential_natives_with::*;
#[allow(unused_imports)]
pub use lib_parts::formatter_alloc_sink::*;
#[allow(unused_imports)]
use lib_parts::formatter_alloc_sink::*;
#[allow(unused_imports)]
pub use lib_parts::arrays_range_or_throw::*;
#[allow(unused_imports)]
use lib_parts::arrays_range_or_throw::*;
#[allow(unused_imports)]
pub use lib_parts::native_printwriter_flush::*;
#[allow(unused_imports)]
use lib_parts::native_printwriter_flush::*;
#[allow(unused_imports)]
pub use lib_parts::matcher_captures::*;
#[allow(unused_imports)]
use lib_parts::matcher_captures::*;
#[allow(unused_imports)]
pub use lib_parts::charset_concrete_or_synthetic::*;
#[allow(unused_imports)]
use lib_parts::charset_concrete_or_synthetic::*;
#[allow(unused_imports)]
pub use lib_parts::lock_key_for::*;
#[allow(unused_imports)]
use lib_parts::lock_key_for::*;
#[allow(unused_imports)]
pub use lib_parts::native_invocation_handler_invoke::*;
#[allow(unused_imports)]
use lib_parts::native_invocation_handler_invoke::*;

pub mod ffm_fast;
pub mod panama;
pub mod panama_libffi;
pub mod panama_upcall;
pub mod hooks;
