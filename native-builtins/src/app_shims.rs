// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Third-party application-shim registration seams.
//!
//! The built-ins crate contains compatibility overrides for BouncyCastle,
//! JBoss/WildFly/XNIO, connection pools, ANTLR, ByteBuddy/Mockito, and
//! Hibernate. Registering all of them on every boot used memory and made
//! unrelated programs vulnerable to accidental overrides.
//!
//! This module is that seam, made explicit: each family's registrars are
//! collected behind exactly one `pub fn` so that turning the family off —
//! whether by moving it to another crate or by making the call conditional
//! on the library being on the classpath — is a one-line change at a single
//! site instead of an archaeology exercise across 12 000 lines of
//! `register_essential_natives`.
//!
//! VM bootstrap selects these families from indexed application-classpath
//! witnesses before bytecode executes. The existence-only probe neither
//! inflates JAR entries nor loads witness classes. Core JDK registrations
//! formerly mixed into the datasource, naming, and security packs have
//! dedicated unconditional registrars, so an absent application pack cannot
//! remove a platform bridge.
//!
//! [reachability audit]: native-builtins-reachability.md
//!
//! # Why selection happens at bootstrap
//!
//! The registry becomes immutable after VM construction and interpreter call
//! sites memoize bytecode/native dispatch. Late registration would require
//! synchronized registry mutation plus cache invalidation. Bootstrap
//! selection avoids both: the registry is complete before any dispatch
//! decision can be cached.
//!
//! # Deliberate non-members
//!
//! * The Xerces intrinsics (`crate::xml_xerces`, `lib.rs` `XERCES_*`) are
//!   **not** an application shim. Every class they register is
//!   `com/sun/org/apache/xerces/internal/**` or `jdk/xml/internal/**` — the
//!   JDK's own `java.xml` module, present in every JDK. The audit filed
//!   them under `native-appshim-data`; that classification is wrong and
//!   gating them on a classpath probe would be a no-op at best.
//! * `crate::servlet`'s Jython/ScriptEngine block registers
//!   `javax/script/ScriptEngineManager` (`java.scripting`) alongside the
//!   `org/python/**` shims, so it is not a pure family either.
//! * `crate::jboss_jdkspecific` registers `java/lang/module/Configuration`,
//!   `java/lang/module/ResolvedModule` and
//!   `jdk/internal/module/SystemModuleFinders$SystemModuleReader` — JDK
//!   module-system bridges that merely happen to be motivated by JBoss
//!   Modules. Not severable.

use cratonvm_native_api::NativeMethodRegistry;

/// Immutable application compatibility-pack selection made during VM boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShimSelection(u8);

impl ShimSelection {
    pub const NONE: Self = Self(0);
    pub const ALL: Self = Self((1 << ShimFamily::ALL.len()) - 1);

    /// Build a selection from existence-only resource probes.
    pub fn from_resource_probe(mut contains: impl FnMut(&str) -> bool) -> Self {
        let mut bits = 0;
        for family in ShimFamily::ALL {
            if family
                .witness_resources()
                .iter()
                .any(|resource| contains(resource))
            {
                bits |= 1 << family.index();
            }
        }
        Self(bits)
    }

    pub fn includes(self, family: ShimFamily) -> bool {
        self.0 & (1 << family.index()) != 0
    }
}

/// A third-party compatibility family selected during VM bootstrap.
///
/// The metadata defines what to probe for and the namespace the selected pack
/// is allowed to own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShimFamily {
    /// ANTLR 4 (incl. Groovy's shaded copy), ByteBuddy, Mockito, Hibernate.
    AppIntrinsics,
    /// BouncyCastle crypto kernels (registered `Intrinsic`; the
    /// `org/bouncycastle` package is JIT-banned, so these leaves would
    /// otherwise run interpreted).
    BouncyCastle,
    /// Agroal / IronJacamar / Infinispan / Vert.x / WildFly datasources +
    /// Narayana JTA.
    DataSourcePools,
    /// JBoss Modules resource loading, JBoss MSC, WildFly Core / Naming /
    /// Security / Undertow, and the XNIO worker, io-thread, conduit and
    /// async layers.
    JBossWildFlyXnio,
}

impl ShimFamily {
    /// Every family, in the order their registrars run during
    /// `register_essential_natives`.
    pub const ALL: [ShimFamily; 4] = [
        ShimFamily::AppIntrinsics,
        ShimFamily::BouncyCastle,
        ShimFamily::DataSourcePools,
        ShimFamily::JBossWildFlyXnio,
    ];

    const fn index(self) -> u8 {
        match self {
            ShimFamily::AppIntrinsics => 0,
            ShimFamily::BouncyCastle => 1,
            ShimFamily::DataSourcePools => 2,
            ShimFamily::JBossWildFlyXnio => 3,
        }
    }

    /// Classpath resources whose presence means this family's library is
    /// actually in play.
    ///
    /// These are resource names, not class names, so they can be answered
    /// by a single `entry_index.contains(..)` probe per classpath entry
    /// (`classloading/src/class_path.rs:338`) with no inflate — the cheap
    /// path documented in the module header. Any one of them matching is
    /// enough; they are alternatives, not a conjunction (a family can be
    /// pulled in by more than one artifact, e.g. ANTLR either directly or
    /// via Groovy's shaded copy).
    pub fn witness_resources(self) -> &'static [&'static str] {
        match self {
            ShimFamily::AppIntrinsics => &[
                "org/antlr/v4/runtime/CommonToken.class",
                "groovyjarjarantlr4/v4/runtime/atn/ATNState.class",
                "net/bytebuddy/description/method/MethodDescription.class",
                "org/mockito/internal/creation/bytebuddy/MockMethodAdvice.class",
                "org/hibernate/spi/NavigablePath.class",
            ],
            ShimFamily::BouncyCastle => &[
                "org/bouncycastle/crypto/engines/AESEngine.class",
                "org/bouncycastle/math/ec/ECPoint.class",
                "org/springframework/security/crypto/bcrypt/BCrypt.class",
            ],
            ShimFamily::DataSourcePools => &[
                "io/agroal/pool/ConnectionPool.class",
                "org/jboss/jca/core/connectionmanager/pool/AbstractPool.class",
                "org/infinispan/manager/DefaultCacheManager.class",
                "io/vertx/core/impl/VertxImpl.class",
                "com/arjuna/ats/jta/TransactionManagerImple.class",
            ],
            ShimFamily::JBossWildFlyXnio => &[
                "org/jboss/msc/service/ServiceName.class",
                "org/jboss/modules/ResourceRootFactory.class",
                "org/xnio/OptionMap.class",
                "io/undertow/Undertow.class",
            ],
        }
    }

    /// Internal-name package prefixes this family registers natives into.
    ///
    /// Every class the family's registrars touch is under one of these
    /// prefixes. Tests enforce that invariant so a new out-of-family target
    /// cannot silently widen the pack's ownership.
    pub fn owned_prefixes(self) -> &'static [&'static str] {
        match self {
            ShimFamily::AppIntrinsics => &[
                "org/antlr/v4/",
                "groovyjarjarantlr4/",
                "net/bytebuddy/",
                "org/mockito/",
                "org/hibernate/",
            ],
            ShimFamily::BouncyCastle => &[
                "org/bouncycastle/",
                // Spring Security vendors BCrypt's expensive key schedule
                // under its own package; `register_bc_bcrypt_generator`
                // covers both copies.
                "org/springframework/security/crypto/bcrypt/",
            ],
            ShimFamily::DataSourcePools => &[
                "io/agroal/",
                "io/vertx/",
                "org/infinispan/",
                "org/jboss/jca/",
                "org/jboss/as/connector/",
                "com/arjuna/",
                // Jakarta EE (not JDK): these ship with the app server, not
                // with the JDK, so they are severable along with the family.
                "javax/resource/",
                "javax/transaction/",
            ],
            ShimFamily::JBossWildFlyXnio => &[
                "org/jboss/",
                "org/wildfly/",
                "org/xnio/",
                "io/undertow/",
                // VM-internal accept/poll pumps that exist only to drive the
                // XNIO event loop.
                "cratonvm/xnio/",
            ],
        }
    }

    /// JDK-module classes owned by this application family.
    ///
    /// This must remain empty. Platform bridges belong to the unconditional
    /// core registrars, never to a classpath-selected compatibility pack.
    pub fn jdk_entanglements(self) -> &'static [&'static str] {
        &[]
    }

    /// Whether this family can be skipped without removing a platform native.
    pub fn is_severable(self) -> bool {
        true
    }

    /// Run this family's registrars. Equivalent to calling the matching
    /// free function directly.
    pub fn register(self, registry: &mut NativeMethodRegistry) {
        match self {
            ShimFamily::AppIntrinsics => register_app_intrinsic_shims(registry),
            ShimFamily::BouncyCastle => register_bouncycastle_shims(registry),
            ShimFamily::DataSourcePools => register_datasource_pool_shims(registry),
            ShimFamily::JBossWildFlyXnio => register_jboss_wildfly_xnio_shims(registry),
        }
    }
}

/// ANTLR / ByteBuddy / Mockito / Hibernate bytecode-equivalent intrinsics.
///
/// Called from inside `register_essential_natives`' existing
/// `with_category(Intrinsic, ..)` scope, so this function must not set a
/// category of its own — the ambient `Intrinsic` is the whole point.
pub fn register_app_intrinsic_shims(registry: &mut NativeMethodRegistry) {
    crate::antlr_intrinsics::register_antlr_prediction_context_intrinsics(registry);
    crate::antlr_intrinsics::register_antlr_token_intrinsics(registry);
    crate::test_frameworks::register_bytebuddy_method_token_intrinsics(registry);
    crate::test_frameworks::register_mockito_debugging_intrinsics(registry);
    crate::orm_hibernate::register_hibernate_testing_util_intrinsics(registry);
    crate::orm_hibernate::register_hibernate_models_intrinsics(registry);
}

/// BouncyCastle crypto kernels.
///
/// Every one of these exists for the same reason: the `org/bouncycastle`
/// package is JIT-banned (a value-model collision in its F2m EC path), so
/// the arithmetic leaves would otherwise run interpreted and dominate the
/// crypto suites. The per-registrar rationale is kept inline rather than
/// summarized away — it is the record of *why* each kernel was worth a
/// native port.
pub fn register_bouncycastle_shims(registry: &mut NativeMethodRegistry) {
    // BouncyCastle RSA-keygen small-factor prime pre-screen fast-path (Intrinsic).
    // BC is JIT-banned (value-model collision in its F2m EC path, unrelated), so
    // `Primes.implHasAnySmallFactors` otherwise runs interpreted and dominates
    // RSA key generation. Faithful single-word-mod reimplementation; see the fn doc.
    crate::phases_late::register_bc_primes_small_factors(registry);
    // BouncyCastle binary-field EC uses LongArray for generic F2m arithmetic.
    // Keep the org/bouncycastle JIT ban intact, but run the small polynomial
    // multiply/square/reduce/inverse leaves natively so math-ec and EC crypto
    // regression do not spend minutes in interpreted bit loops.
    crate::phases_late::register_bc_long_array(registry);
    // BouncyCastle generic prime-field EC uses ECFieldElement.Fp bytecode for
    // every point add/double. Keep BC bytecode JIT-banned, but run the field
    // arithmetic leaves with the same limb BigInteger core used by java.math.
    crate::phases_late::register_bc_fp_field_element(registry);
    // The generic prime-field point formulas remain hot in complete EC math
    // tests after the field leaves are native; route just those methods natively.
    crate::phases_late::register_bc_fp_point(registry);
    // Same treatment for generic binary-field ECFieldElement.F2m wrappers: this
    // avoids spending the math-ec suite in interpreted field-element glue around
    // the native LongArray polynomial leaves.
    crate::phases_late::register_bc_f2m_field_element(registry);
    // Generic binary-field ECPoint.F2m add/double is the hot Lambda-projective
    // point layer above the native F2m field-element leaves.
    crate::phases_late::register_bc_f2m_point(registry);
    // Keep the BC package JIT ban in place, but run the high-level Shamir JSF
    // driver loop natively above the native EC point methods.
    crate::phases_late::register_bc_ec_algorithms(registry);
    // Custom SEC binary curves bypass ECFieldElement.F2m and call static
    // SecT*Field kernels directly from point add/double code. Route those
    // polynomial kernels through the same native GF(2^m) engine.
    crate::phases_late::register_bc_sect_field_kernels(registry);
    // The inherited ECPoint.timesPow2 loop otherwise spends complete binary
    // curve tests in interpreted SecT*Point.twice glue around those kernels.
    crate::phases_late::register_bc_sect_point_methods(registry);
    // BouncyCastle AESEngine single-block transform fast-path (Intrinsic). Same
    // JIT-ban rationale: the interpreted T-table AES otherwise dominates AESTest's
    // Monte-Carlo stress. Verbatim FIPS-197-validated port of encrypt/decryptBlock.
    crate::phases_late::register_bc_aes_engine(registry);
    // CBC mode wrapper fast-path for AES-backed MAC/encryption loops. This keeps
    // the BC JIT ban intact while avoiding interpreted CBC bytecode above the
    // already-native AES block transform.
    crate::phases_late::register_bc_cbc_block_cipher(registry);
    // BouncyCastle GOST3412_2015Engine single-block transform fast-path
    // (Intrinsic). Keeps the BC package JIT ban while removing the interpreted
    // block-cipher loop that dominates GOST3412Test CTR stress.
    crate::phases_late::register_bc_gost3412_engine(registry);
    // BouncyCastle SM4Engine single-block transform fast-path for crypto regression.
    crate::phases_late::register_bc_sm4_engine(registry);
    // BouncyCastle XTEAEngine single-block transform fast-path for CipherStreamTest.
    crate::phases_late::register_bc_xtea_engine(registry);
    // BouncyCastle Strings UTF-8 transcode fast-path (Intrinsic) — dominates
    // AESTest.testCounter's growing-string round-trips once AES is native.
    crate::phases_late::register_bc_strings_utf8(registry);
    crate::phases_late::register_bc_arrays_helpers(registry);
    crate::phases_late::register_bc_param_helpers(registry);
    // BouncyCastle byte/int packing helpers used by block ciphers and digests.
    crate::phases_late::register_bc_pack_helpers(registry);
    // BouncyCastle X25519 field multiply fast-path for Ed25519/X25519 regression.
    crate::phases_late::register_bc_x25519_field(registry);
    // BouncyCastle X448 field multiply/square fast-path for Ed448 regression.
    crate::phases_late::register_bc_x448_field(registry);
    // BouncyCastle BLAKE2s compression leaf fast-path for Blake2xs XOF vectors.
    crate::phases_late::register_bc_blake2s_digest(registry);
    // BouncyCastle Keccak absorb/extract/permutation fast-path for CSHAKE/KMAC.
    crate::phases_late::register_bc_keccak_digest(registry);
    // BouncyCastle SHA-256 compression leaf (Intrinsic). LMS/HSS builds
    // `SHA256Digest` directly rather than going through `MessageDigest`, so the
    // native JCA SHA-256 never sees this workload; see
    // `register_bc_sha256_digest` for why `processBlock` is the seam.
    crate::phases_late::register_bc_sha256_digest(registry);
    // BouncyCastle legacy GOST3411 compression-block fast-path for the
    // million-'a' digest regression under the org/bouncycastle JIT ban.
    crate::phases_late::register_bc_gost3411_digest(registry);
    // BouncyCastle Whirlpool update/compression fast-path for the million-'a'
    // digest regression under the same package JIT ban.
    crate::phases_late::register_bc_whirlpool_digest(registry);
    // BouncyCastle Poly1305 accumulator/finalization fast-path for standalone
    // Poly1305 and ChaCha20-Poly1305 regression vectors under the BC JIT ban.
    crate::phases_late::register_bc_poly1305(registry);
    // BouncyCastle SCrypt SMix/BlockMix fast-path for crypto regression.
    crate::phases_late::register_bc_scrypt_generator(registry);
    // BouncyCastle Argon2 block-round fast-path for crypto regression.
    crate::phases_late::register_bc_argon2_bytes_generator(registry);
    // BouncyCastle PKCS#5 v2 PBKDF2/SHA-1 KDF fast-path for crypto regression.
    crate::phases_late::register_bc_pkcs5s2_parameters_generator(registry);
    // BouncyCastle PKCS#12 SHA-1 KDF fast-path for crypto regression vectors.
    crate::phases_late::register_bc_pkcs12_parameters_generator(registry);
    // BouncyCastle BCrypt expensive key schedule fast-path for crypto regression.
    crate::phases_late::register_bc_bcrypt_generator(registry);
    // BouncyCastle CTR-mode (SICBlockCipher) per-byte loop fast-path (Intrinsic) —
    // the sole remaining hot frame in AESTest.testCounter once AES+Strings are native.
    crate::phases_late::register_bc_sic_ctr(registry);
    // BouncyCastle DigestRandomGenerator synchronized PRNG fast-path
    // (Intrinsic). This keeps the package JIT ban intact while avoiding the
    // million-call interpreted monitor body in DigestRandomNumberTest.
    crate::phases_late::register_bc_digest_random_generator(registry);
    // BouncyCastle ChaCha permutation fast-path (Intrinsic). Same JIT-ban
    // rationale: the interpreted ChaCha core (dozens of Integers.rotateLeft
    // calls per block) dominates the SPHINCS-256 PQC RegressionTest (PRG via
    // ChaChaEngine.chachaCore + hash via Permute.permute/HashFunctions).
    // Verbatim port, RFC 8439- and HotSpot-validated.
    crate::phases_late::register_bc_chacha(registry);
    // BouncyCastle NewHope lattice fast-path (Intrinsic). The NTT
    // (Poly.toNTT/fromNTT) + SHAKE128 sampler (Poly.uniform) dominate the PQC
    // RegressionTest's NewHopeTest once ChaCha is native. Verbatim port of
    // NTT/Reduce + the `sha3` crate's SHAKE128, validated against HotSpot.
    crate::phases_late::register_bc_newhope(registry);
}

/// JDBC connection pools, the embedded cache, the Vert.x/Netty event loop
/// and the WildFly datasources + Narayana JTA glue.
///
/// The two `real_*` guards are opt-in escapes that let the *real* library
/// bytecode run instead of the shim; they are preserved verbatim, so this
/// function registers exactly what the inline sequence did.
pub fn register_datasource_pool_shims(registry: &mut NativeMethodRegistry) {
    // T19.8: Agroal (Quarkus) + IronJacamar (WildFly) JDBC pool natives.
    // real-cdi-bean-container (Keycloak Gap 8): under `CRATONVM_REAL_AGROAL`
    // the shim is suppressed so the real `io.agroal.pool.*` container bytecode
    // runs over the real `org.h2.Driver`. The shim's interface-typed config
    // object otherwise collides with the real `DataSourceProvider` bytecode
    // (AbstractMethodError on `dataSourceImplementation()`). See `real_agroal`.
    if !crate::real_agroal() {
        crate::agroal_pool::register_agroal_natives(registry);
    }
    crate::ironjacamar_pool::register_ironjacamar_natives(registry);
    // T19.10: Infinispan local-mode cache (DefaultCacheManager + Cache).
    crate::infinispan_local::register_infinispan_natives(registry);
    // T19.6: Vert.x / Netty NioEventLoop affinity scheduler natives.
    // Under CRATONVM_REAL_VERTX the synthetic loop is suppressed so the real Netty
    // NioEventLoop.run() drives Selector.select() over our selector (see real_vertx).
    if !crate::real_vertx() {
        crate::vertx_eventloop::register_vertx_eventloop_natives(registry);
    }
    // T19.2.e: WildFly Datasources subsystem + Narayana JTA glue.
    crate::wildfly_datasources_tx::register_wildfly_datasources_tx_natives(registry);
}

/// JBoss Modules, MSC, WildFly Core/Naming/Security/Undertow and the four
/// XNIO layers.
///
/// This is the largest single family (~48 kLoC of module source) and the
/// one a `HelloWorld` has the least use for. JAAS and JNDI are registered
/// separately and are not part of this pack.
pub fn register_jboss_wildfly_xnio_shims(registry: &mut NativeMethodRegistry) {
    // --- RA.4 / RA.5: JBoss Modules `module.xml` parser + ResourceRootFactory.
    // We bypass the MXParser path (which relies on NIO CharBuffer internals
    // the VM is still stabilizing) by parsing module.xml in Rust and calling
    // back into ModuleSpec$Builder / DependencySpec / ResourceLoaders via
    // `ctx.invoke`.
    registry.register(
        "org/jboss/modules/xml/ModuleXmlParser",
        "parseModuleXml",
        "(Lorg/jboss/modules/xml/ModuleXmlParser$ResourceRootFactory;Lorg/jboss/modules/ModuleLoader;Ljava/lang/String;Ljava/io/File;Ljava/io/File;)Lorg/jboss/modules/ModuleSpec;",
        crate::jboss_module_xml::native_parse_module_xml,
    );
    crate::jboss_resource_loader::register_resource_loader_natives(registry);

    // T19.1: JBoss MSC (Modular Service Container) — the async service
    // orchestrator WildFly / Keycloak 16 enters right after Main.main().
    // Registers ServiceName / ServiceContainer / ServiceController /
    // StartContext natives and spins up the msc-worker thread pool
    // lazily on first `ServiceContainer.create()` call.
    crate::jboss_msc::register_jboss_msc_natives(registry);

    // T19.2.a: WildFly Core kernel — Deployment + Threads + Logging.
    // Runs right after MSC so DeploymentUnit / EnhancedQueueExecutor /
    // LogManager + Logger / ControlledProcessState natives are in place
    // before naming / security / undertow register their services.
    crate::wildfly_core::register_wildfly_core_natives(registry);

    // T19.2.b: WildFly Naming (JNDI) — InitialContext + ServiceBasedNamingStore
    // + ContextNames natives. Runs after the Core kernel so the MSC container
    // and DeploymentUnit layout exist before any subsystem calls `bind()`.
    // Enforces the Log4Shell-class JNDI-injection allowlist at the native
    // boundary: only `java:`, `java:jboss/`, `java:comp/`, `java:global/`,
    // `java:app/`, `java:module/` absolute names are accepted.
    crate::wildfly_naming::register_wildfly_naming_natives(registry);

    // T19.2.c: WildFly Security — JAAS LoginContext / LoginModule chain,
    // Subject / Principal sets, SecurityDomainService / ApplicationPolicy
    // lookup, and SecurityIdentity.runAs. Bootstrap internal-auth path for
    // Keycloak 16; real identity still comes from KC's own realm.
    crate::wildfly_security::register_wildfly_security_natives(registry);

    // T19.2.d: WildFly Undertow — HTTP server builder, HttpHandler dispatch,
    // HeaderMap / HttpString / Sender surface, UndertowService and
    // ListenerService lifecycle. HTTP/1.1 request head parser with size caps
    // (8 KiB line / 32 KiB headers / 100 headers / 10 MiB body), CRLF-injection
    // rejection on response headers, Host-header validation, and panic-safe
    // handler dispatch (500 on panic instead of process crash). The accept
    // loop that will drive live request handling is deferred to T19.7 (XNIO
    // worker); here we expose `dispatch_handler` / `accept_backoff` as the
    // integration points.
    crate::wildfly_undertow::register_undertow_natives(registry);

    // T19.7.b: JBoss XNIO worker — top-level XnioWorker with bounded I/O
    // + task thread pools (default 4 + 16, clamped at 128 + 1024), round-
    // robin I/O-thread dispatch, panic-safe task submission, and graceful
    // shutdown/awaitTermination semantics. Provides the scaffolding
    // T19.7.c (io_thread event loop), T19.7.d (conduits), and T19.7.e
    // (OptionMap builder) plug into.
    crate::xnio_worker::register_xnio_worker_natives(registry);

    // T19.7.c: XnioIoThread / NioIoThread / XnioExecutor$Key natives —
    // execute(), executeAfter(), executeAtTime(), currentThread(),
    // Key.remove(). Event loop uses a BinaryHeap<Reverse<ScheduledTask>>
    // min-heap for timers + Mutex<VecDeque<IoTask>> for immediate tasks;
    // cross-thread execute wakes the Selector via a coalesced AtomicBool.
    crate::xnio_io_thread::register_xnio_io_thread_natives(registry);

    // T19.7.d: XNIO Conduit stream channels — read/write through
    // ConduitStreamSourceChannel / ConduitStreamSinkChannel, ChannelListener
    // dispatch (panic-safe), resume/suspend semantics, half-close via
    // shutdownReads/Writes, and flush reporting based on a buffered-bytes
    // counter. Listener callbacks are dispatched through `invoke_virtual`
    // wrapped in `catch_unwind` so a buggy listener can't crash the loop.
    crate::xnio_conduits::register_xnio_conduits_natives(registry);

    // T19.7.e: XNIO OptionMap + IoFuture + Options + XnioExecutor primitives.
    // OptionMap is immutable-after-build (Arc<HashMap>-backed); IoFuture has
    // an AtomicU8 monotonic state machine (WAITING → DONE/CANCELLED/FAILED)
    // with Condvar-wake on completion and panic-isolated notifier dispatch.
    // Populates Options.<WORKER_IO_THREADS, BACKLOG, TCP_NODELAY, …> on first
    // access so xnio_worker can read defaults.
    crate::xnio_async::register_xnio_async_natives(registry);
}

#[cfg(test)]
mod tests {
    use super::*;
    use cratonvm_native_api::NativeKind;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// Internal-name prefixes that belong to the JDK, not to any
    /// application. A family registering into one of these cannot be
    /// skipped without taking the native away from unrelated programs.
    const JDK_PREFIXES: [&str; 5] = ["java/", "javax/", "jdk/", "sun/", "com/sun/"];

    /// `javax/` is shared: `javax.naming` / `javax.sql` / `javax.script` are
    /// JDK modules, while `javax.transaction` / `javax.resource` are Jakarta
    /// EE artifacts that ship with the app server. Only the JDK ones matter
    /// for severability.
    const JAKARTA_NOT_JDK: [&str; 2] = ["javax/transaction/", "javax/resource/"];

    fn is_jdk_class(class: &str) -> bool {
        if JAKARTA_NOT_JDK.iter().any(|p| class.starts_with(p)) {
            return false;
        }
        JDK_PREFIXES.iter().any(|p| class.starts_with(p))
    }

    /// A registry that registers what the registrars ask for, regardless of
    /// the ambient environment.
    ///
    /// `NativeMethodRegistry::new()` latches `CRATONVM_NO_STUBS`
    /// (`native-api/src/registry.rs:3979`), and under it `register()` silently
    /// drops every `SyntheticStub` registration — which includes the JNDI
    /// entanglement, since `wildfly_naming` never sets a category and so
    /// inherits the registry's `SyntheticStub` default
    /// (`registry.rs:3879`). Pinning it off keeps these tests measuring the
    /// registrars rather than the shell they happen to run in.
    fn probe_registry() -> NativeMethodRegistry {
        let mut registry = NativeMethodRegistry::new();
        registry.set_drop_synthetic_stubs(false);
        registry
    }

    fn registered_classes(family: ShimFamily) -> Vec<String> {
        let mut registry = probe_registry();
        family.register(&mut registry);
        let mut classes: Vec<String> = registry
            .dump_registrations()
            .into_iter()
            .map(|(class, _, _, _)| class.to_string())
            .collect();
        classes.sort();
        classes.dedup();
        classes
    }

    fn registered_keys(family: ShimFamily) -> Vec<(String, String, String)> {
        let mut registry = probe_registry();
        family.register(&mut registry);
        registry
            .dump_registrations()
            .into_iter()
            .map(|(class, method, descriptor, _)| {
                (
                    class.to_string(),
                    method.to_string(),
                    descriptor.to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn every_family_registers_something() {
        for family in ShimFamily::ALL {
            assert!(
                !registered_classes(family).is_empty(),
                "{family:?} registered nothing — the seam has drifted away from \
                 its registrars"
            );
        }
    }

    /// A selected family owns its own packages and nothing else.
    #[test]
    fn families_register_only_into_owned_prefixes() {
        for family in ShimFamily::ALL {
            let owned = family.owned_prefixes();
            for class in registered_classes(family) {
                let in_family = owned.iter().any(|p| class.starts_with(p));
                assert!(
                    in_family,
                    "{family:?} registers `{class}` outside its owned prefixes \
                     {owned:?}; platform bridges must use a core registrar"
                );
            }
        }
    }

    #[test]
    fn application_families_have_no_jdk_entanglements() {
        for family in ShimFamily::ALL {
            assert!(family.jdk_entanglements().is_empty(), "{family:?}");
        }
    }

    /// The declared seam must agree with the classes each pack registers.
    #[test]
    fn severability_matches_measured_jdk_entanglement() {
        for family in ShimFamily::ALL {
            let has_jdk_class = registered_classes(family).iter().any(|c| is_jdk_class(c));
            assert_eq!(
                family.is_severable(),
                !has_jdk_class,
                "{family:?}: is_severable() disagrees with what it actually registers"
            );
        }
        assert!(ShimFamily::AppIntrinsics.is_severable());
        assert!(ShimFamily::BouncyCastle.is_severable());
        assert!(ShimFamily::DataSourcePools.is_severable());
        assert!(ShimFamily::JBossWildFlyXnio.is_severable());
    }

    #[test]
    fn witness_selection_is_family_local() {
        let selected = ShimSelection::from_resource_probe(|resource| {
            resource == "org/bouncycastle/crypto/engines/AESEngine.class"
                || resource == "org/xnio/OptionMap.class"
        });
        assert!(!selected.includes(ShimFamily::AppIntrinsics));
        assert!(selected.includes(ShimFamily::BouncyCastle));
        assert!(!selected.includes(ShimFamily::DataSourcePools));
        assert!(selected.includes(ShimFamily::JBossWildFlyXnio));
        assert_eq!(ShimSelection::NONE.0, 0);
        for family in ShimFamily::ALL {
            assert!(ShimSelection::ALL.includes(family));
        }
    }

    #[test]
    fn no_application_packs_keeps_jdk_entanglements_registered() {
        let mut registry = probe_registry();
        crate::register_essential_natives_with_shims(&mut registry, ShimSelection::NONE);
        let registrations: std::collections::HashSet<(String, String, String)> = registry
            .dump_registrations()
            .into_iter()
            .map(|(class, method, descriptor, _)| {
                (
                    class.to_string(),
                    method.to_string(),
                    descriptor.to_string(),
                )
            })
            .collect();

        for jdk_class in [
            "java/security/AccessControlContext",
            "javax/security/auth/Subject",
            "javax/security/auth/login/LoginContext",
            "javax/sql/DataSource",
        ] {
            assert!(
                registrations.iter().any(|(class, _, _)| class == jdk_class),
                "core JDK bridge {jdk_class} disappeared with application packs disabled"
            );
        }
        // Real-JDK InitialContext must retain its provider-selection bytecode;
        // the in-memory naming bridge has only the synthetic JDK field layout.
        #[cfg(feature = "synthetic-jdk")]
        {
            let jdk_class = "javax/naming/InitialContext";
            assert!(
                registrations.iter().any(|(class, _, _)| class == jdk_class),
                "core JDK bridge {jdk_class} disappeared with application packs disabled"
            );
        }
        let mut leaks = Vec::new();
        for family in ShimFamily::ALL {
            for key in registered_keys(family) {
                if registrations.contains(&key) {
                    leaks.push(format!("{family:?}:{key:?}"));
                }
            }
        }
        assert!(
            leaks.is_empty(),
            "application pack classes leaked into core: {leaks:?}"
        );
    }

    /// `NativeKind` is ambient (`current_category` persists across
    /// `set_category`), which is precisely what made the `d8092acb` tag
    /// audit unsound. Grouping the call sequence must not disturb it: every
    /// family's registrars save and restore the category, so the ambient
    /// value a caller had before is the value it has after.
    ///
    /// `None` is in the seed list and is the interesting one. It is what the
    /// registry now carries outside any scope, and "no scope covers this
    /// registration" is a state a registrar can leak just as easily as a wrong
    /// kind — leaking a *category* over it would make the census report those
    /// rows as adjudicated by whoever the leak belonged to.
    #[test]
    fn grouping_does_not_leak_an_ambient_category() {
        for seed in [
            None,
            Some(NativeKind::SyntheticStub),
            Some(NativeKind::Bridge),
            Some(NativeKind::Intrinsic),
        ] {
            for family in ShimFamily::ALL {
                let mut registry = probe_registry();
                registry.set_category(seed);
                family.register(&mut registry);
                assert_eq!(
                    registry.current_category(),
                    seed,
                    "{family:?} leaked an ambient category (seeded {seed:?})"
                );
            }
        }
    }

    /// A witness is only useful if it names a class the family actually
    /// serves, so a classpath probe for it means what the gating change
    /// would assume it means.
    #[test]
    fn witness_resources_are_class_files_under_an_owned_prefix() {
        for family in ShimFamily::ALL {
            let witnesses = family.witness_resources();
            assert!(
                !witnesses.is_empty(),
                "{family:?} has no classpath witness, so it could never be probed for"
            );
            for witness in witnesses {
                assert!(
                    witness.ends_with(".class"),
                    "{family:?} witness `{witness}` is not a .class resource name; \
                     entry_index holds exact archive entry names"
                );
                assert!(
                    !witness.starts_with('/'),
                    "{family:?} witness `{witness}` must be a bare internal name"
                );
                assert!(
                    family
                        .owned_prefixes()
                        .iter()
                        .any(|p| witness.starts_with(p)),
                    "{family:?} witness `{witness}` is outside the family's own \
                     packages, so its presence would not imply the family is in use"
                );
            }
        }
    }

    /// Guards the classification correction in the module header: the
    /// Xerces intrinsics are `java.xml`, so they are not a candidate family
    /// and must never be added to `ShimFamily`.
    #[test]
    fn jdk_bundled_xerces_is_not_a_shim_family() {
        assert!(
            is_jdk_class("com/sun/org/apache/xerces/internal/impl/dtd/models/CMStateSet"),
            "the JDK-bundled Xerces must classify as a JDK class"
        );
        assert!(is_jdk_class("jdk/xml/internal/XMLLimitAnalyzer"));
        for family in ShimFamily::ALL {
            assert!(
                !family
                    .owned_prefixes()
                    .iter()
                    .any(|p| p.starts_with("com/sun/") || p.starts_with("jdk/")),
                "{family:?} claims a JDK-internal package as its own"
            );
        }
    }

    #[test]
    fn jakarta_ee_packages_are_not_treated_as_jdk() {
        assert!(!is_jdk_class("javax/transaction/TransactionManager"));
        assert!(!is_jdk_class("javax/resource/spi/ManagedConnection"));
        assert!(is_jdk_class("javax/sql/DataSource"));
        assert!(is_jdk_class("javax/naming/InitialContext"));
        assert!(is_jdk_class("java/security/AccessControlContext"));
    }
}
