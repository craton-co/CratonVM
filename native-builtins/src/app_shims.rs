// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
    crate::phases_late::register_bc_gcm_block_cipher(registry);
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
    // BouncyCastle X448 field multiply/square fast-path for Ed448 regression.
    crate::phases_late::register_bc_ed25519(registry);
    // BouncyCastle BLAKE2s compression leaf fast-path for Blake2xs XOF vectors.
    crate::phases_late::register_bc_blake2s_digest(registry);
    // BouncyCastle Keccak absorb/extract/permutation fast-path for CSHAKE/KMAC.
    crate::phases_late::register_bc_keccak_digest(registry);
    // BouncyCastle SHA-256 compression leaf (Intrinsic). LMS/HSS builds
    // `SHA256Digest` directly rather than going through `MessageDigest`, so the
    // native JCA SHA-256 never sees this workload; see
    // `register_bc_sha256_digest` for why `processBlock` is the seam.
    crate::phases_late::register_bc_sha256_digest(registry);
    crate::phases_late::register_bc_long_digest(registry);
    crate::phases_late::register_bc_md5_digest(registry);
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
        cratonvm_native_api::NativeKind::SyntheticStub,
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

/// Quarkus `RunnerClassLoader.close()` closes each distinct `ClassLoadingResource`
/// once (Quarkus maps many package directories to the same resource).
fn quarkus_runner_class_loader_close(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[cratonvm_types::Value],
) -> cratonvm_types::error::MethodCallResult {
    let this = match args.first() {
        Some(cratonvm_types::Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let map = match ctx.get_field_by_name(this, "resourceDirectoryMap") {
        cratonvm_types::Value::Object(Some(m)) => m,
        _ => return Ok(None),
    };
    let values = match ctx.invoke_virtual(map, "values", "()Ljava/util/Collection;", &[])? {
        Some(cratonvm_types::Value::Object(Some(v))) => v,
        _ => return Ok(None),
    };
    let table = match ctx.invoke_virtual(values, "toArray", "()[Ljava/lang/Object;", &[])? {
        Some(cratonvm_types::Value::Object(Some(t))) => t,
        _ => return Ok(None),
    };

    let mut resources: Vec<cratonvm_types::ObjectRef> = Vec::new();
    for i in 0..ctx.array_length(table) {
        let entry = match ctx.get_array_element(table, i) {
            cratonvm_types::Value::Object(Some(e)) => e,
            _ => continue,
        };
        if ctx.object_is_array(entry) {
            for j in 0..ctx.array_length(entry) {
                if let cratonvm_types::Value::Object(Some(res)) = ctx.get_array_element(entry, j) {
                    if !resources.contains(&res) {
                        resources.push(res);
                    }
                }
            }
        } else if !resources.contains(&entry) {
            resources.push(entry);
        }
    }
    let base = match resources.first() {
        Some(first) => ctx.pin_native_root(*first),
        None => return Ok(None),
    };
    let mut handles = Vec::with_capacity(resources.len());
    handles.push(base);
    for res in &resources[1..] {
        handles.push(ctx.pin_native_root(*res));
    }

    let mut outcome = Ok(None);
    for (idx, handle) in handles.iter().enumerate() {
        let res = ctx.read_native_pin(*handle, resources[idx]);
        if let Err(e) = ctx.invoke_virtual(res, "close", "()V", &[]) {
            outcome = Err(e);
            break;
        }
    }
    ctx.unpin_native_roots(base);
    outcome
}

/// Register third-party application/library workarounds in [`NativeMethodRegistry`]
/// so core crates (`vm`, `jit`, `gc`, `classloading`) query generic capabilities
/// rather than naming third-party classes directly.
pub fn register_app_workarounds(registry: &mut NativeMethodRegistry) {
    use cratonvm_native_api::{AppWorkaroundEntry, AppWorkaroundKind, NativeKind};

    registry.register(
        "io/quarkus/bootstrap/runner/RunnerClassLoader",
        "close",
        "()V",
        quarkus_runner_class_loader_close,
        NativeKind::SyntheticStub,
    );

    let entries: &[(&'static str, &'static str, &'static str, AppWorkaroundKind, &'static str, &'static str)] = &[
        // Loader-isolated constant-pool resolution
        (
            "org/springframework/core/test/tools/CompileWithForkedClassLoaderClassLoader",
            "",
            "",
            AppWorkaroundKind::LoaderIsolatedConstantResolution,
            "",
            "Spring Test forked classloader isolates redefined classes per test method",
        ),
        (
            "org/springframework/boot/testsupport/classpath/ModifiedClassPathClassLoader",
            "",
            "",
            AppWorkaroundKind::LoaderIsolatedConstantResolution,
            "",
            "Spring Boot @ForkedClassPath/@ClassPathOverrides isolates classes per test method",
        ),
        // Inherited ClassLoader.loadClass bypass
        (
            "org/springframework/core/test/tools/DynamicClassLoader",
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            AppWorkaroundKind::InheritBaseClassLoaderLoadClass,
            "",
            "Spring DynamicClassLoader inherits ClassLoader.loadClass without overriding; preserve parent-first delegation",
        ),
        // Instrumentation transformer deduplication
        (
            "org/mockito/internal/creation/bytebuddy/InlineBytecodeGenerator",
            "",
            "",
            AppWorkaroundKind::SingletonInstrumentationTransformer,
            "",
            "Mockito inline maker state cannot be composed across multiple copies",
        ),
        // Non-constant invokedynamic bootstraps
        (
            "org/codehaus/groovy/vmplugin/v8/IndyInterface",
            "",
            "",
            AppWorkaroundKind::NonConstantIndyBootstrap,
            "",
            "Groovy 3+ IndyInterface requires dynamic MutableCallSite dispatch",
        ),
        (
            "org/codehaus/groovy/vmplugin/v7/IndyInterface",
            "",
            "",
            AppWorkaroundKind::NonConstantIndyBootstrap,
            "",
            "Groovy 2.x IndyInterface requires dynamic MutableCallSite dispatch",
        ),
        // Receiver-subtype native retargets (Jython / Surefire)
        (
            "org/python/core/PyNullImporter",
            "__getattr__",
            "(Ljava/lang/String;)Lorg/python/core/PyObject;",
            AppWorkaroundKind::ReceiverSubtypeNativeRetarget,
            "org/python/core/PyObject",
            "Jython PyNullImporter overrides __getattr__ natively",
        ),
        (
            "org/python/modules/zipimport/zipimporter",
            "__getattr__",
            "(Ljava/lang/String;)Lorg/python/core/PyObject;",
            AppWorkaroundKind::ReceiverSubtypeNativeRetarget,
            "org/python/core/PyObject",
            "Jython zipimporter overrides __getattr__ natively",
        ),
        (
            "org/python/core/PyModule",
            "__findattr__",
            "(Ljava/lang/String;)Lorg/python/core/PyObject;",
            AppWorkaroundKind::ReceiverSubtypeNativeRetarget,
            "org/python/core/PyObject",
            "Jython PyModule overrides __findattr__ natively",
        ),
        (
            "org/python/core/PyJavaType",
            "__findattr_ex__",
            "(Ljava/lang/String;)Lorg/python/core/PyObject;",
            AppWorkaroundKind::ReceiverSubtypeNativeRetarget,
            "org/python/core/PyType",
            "Jython PyJavaType overrides __findattr_ex__ natively",
        ),
        (
            "org/apache/maven/surefire/junitplatform/LazyLauncher",
            "discover",
            "(Lorg/junit/platform/launcher/LauncherDiscoveryRequest;)Lorg/junit/platform/launcher/TestPlan;",
            AppWorkaroundKind::ReceiverSubtypeNativeRetarget,
            "",
            "Surefire LazyLauncher.discover native override",
        ),
        // JNI memo exclusions
        (
            "org/apache/tomcat/jni/",
            "",
            "",
            AppWorkaroundKind::SkipJniMemo,
            "",
            "Tomcat native JNI stubs are excluded from JNI dispatch memo",
        ),
        (
            "io/netty/internal/tcnative/",
            "",
            "",
            AppWorkaroundKind::SkipJniMemo,
            "",
            "Netty tcnative stubs are excluded from JNI dispatch memo",
        ),
        (
            "org/springframework/boot/loader/",
            "",
            "",
            AppWorkaroundKind::SkipJniMemo,
            "",
            "Spring Boot loader excluded from JNI site memo",
        ),
        (
            "org/python/",
            "",
            "",
            AppWorkaroundKind::SkipJniMemo,
            "",
            "Jython classes excluded from JNI vsite memo",
        ),
        // Incompatible host JNI libraries
        (
            "org/apache/tomcat/jni/",
            "",
            "",
            AppWorkaroundKind::SkipIncompatibleHostJni,
            "",
            "Tomcat native host library pointers are ABI-incompatible with libffi",
        ),
        (
            "io/netty/internal/tcnative/",
            "",
            "",
            AppWorkaroundKind::SkipIncompatibleHostJniUntilRealLoad,
            "",
            "Netty tcnative host pointers skipped until real JNI_OnLoad",
        ),
        // Verifier pre-Java-7 exception superclass hints
        (
            "com/thoughtworks/xstream/converters/reflection/ObjectAccessException",
            "",
            "",
            AppWorkaroundKind::VerifierSuperclassHint,
            "com/thoughtworks/xstream/converters/ErrorWritingException",
            "XStream 1.4 Java-6 bytecode merges exception frames before loading",
        ),
        (
            "com/thoughtworks/xstream/converters/ConversionException",
            "",
            "",
            AppWorkaroundKind::VerifierSuperclassHint,
            "com/thoughtworks/xstream/converters/ErrorWritingException",
            "XStream 1.4 Java-6 bytecode merges exception frames before loading",
        ),
        (
            "com/thoughtworks/xstream/converters/ErrorWritingException",
            "",
            "",
            AppWorkaroundKind::VerifierSuperclassHint,
            "com/thoughtworks/xstream/XStreamException",
            "XStream 1.4 Java-6 bytecode merges exception frames before loading",
        ),
        (
            "com/thoughtworks/xstream/XStreamException",
            "",
            "",
            AppWorkaroundKind::VerifierSuperclassHint,
            "com/thoughtworks/xstream/core/BaseException",
            "XStream 1.4 Java-6 bytecode merges exception frames before loading",
        ),
        (
            "com/thoughtworks/xstream/core/BaseException",
            "",
            "",
            AppWorkaroundKind::VerifierSuperclassHint,
            "java/lang/RuntimeException",
            "XStream 1.4 Java-6 bytecode merges exception frames before loading",
        ),
        // Enterprise stub package prefixes
        ("org/jboss/", "", "", AppWorkaroundKind::EnterpriseStubPackagePrefix, "", "WildFly/JBoss optional linkage stub prefix"),
        ("org/wildfly/", "", "", AppWorkaroundKind::EnterpriseStubPackagePrefix, "", "WildFly optional linkage stub prefix"),
        ("org/xnio/", "", "", AppWorkaroundKind::EnterpriseStubPackagePrefix, "", "XNIO optional linkage stub prefix"),
        ("org/infinispan/", "", "", AppWorkaroundKind::EnterpriseStubPackagePrefix, "", "Infinispan optional linkage stub prefix"),
        ("io/quarkus/", "", "", AppWorkaroundKind::EnterpriseStubPackagePrefix, "", "Quarkus optional linkage stub prefix"),
        ("io/agroal/", "", "", AppWorkaroundKind::EnterpriseStubPackagePrefix, "", "Agroal optional linkage stub prefix"),
        ("io/undertow/", "", "", AppWorkaroundKind::EnterpriseStubPackagePrefix, "", "Undertow optional linkage stub prefix"),
        ("io/smallrye/", "", "", AppWorkaroundKind::EnterpriseStubPackagePrefix, "", "SmallRye optional linkage stub prefix"),
        // IMPL-JARS nested JAR prefixes (Elasticsearch)
        ("org/elasticsearch/xcontent", "", "", AppWorkaroundKind::ImplJarsPackagePrefix, "x-content", "Elasticsearch nested IMPL-JARS module"),
        ("org/elasticsearch/xpack", "", "", AppWorkaroundKind::ImplJarsPackagePrefix, "x-pack", "Elasticsearch nested IMPL-JARS module"),
        ("org/elasticsearch/transport", "", "", AppWorkaroundKind::ImplJarsPackagePrefix, "transport", "Elasticsearch nested IMPL-JARS module"),
        ("org/elasticsearch/common", "", "", AppWorkaroundKind::ImplJarsPackagePrefix, "common", "Elasticsearch nested IMPL-JARS module"),
        ("org/elasticsearch/core", "", "", AppWorkaroundKind::ImplJarsPackagePrefix, "core", "Elasticsearch nested IMPL-JARS module"),
        ("org/elasticsearch/", "", "", AppWorkaroundKind::ImplJarsPackagePrefix, "", "Elasticsearch nested IMPL-JARS prefix"),
        // Post-clinit fixup classes/prefixes
        ("io/quarkus/bootstrap/logging/InitialConfigurator", "DELAYED_HANDLER", "", AppWorkaroundKind::PostClinitFixup, "io/quarkus/bootstrap/logging/QuarkusDelayedHandler", "Quarkus DELAYED_HANDLER fixup"),
        ("org/jboss/modules/DefaultBootModuleLoaderHolder", "INSTANCE", "", AppWorkaroundKind::PostClinitFixup, "org/jboss/modules/LocalModuleLoader", "JBoss Modules LocalModuleLoader fixup"),
        ("org/jboss/msc/service/ServiceContainerImpl", "SERIAL,executorSeq", "atomic_int_1", AppWorkaroundKind::PostClinitFixup, "java/util/concurrent/atomic/AtomicInteger", "JBoss MSC ServiceContainerImpl fixup"),
        ("org/wildfly/security/auth/server/_private/ElytronMessages", "log", "", AppWorkaroundKind::PostClinitFixup, "org/wildfly/security/auth/server/_private/ElytronMessages_$logger", "WildFly ElytronMessages fixup"),
        ("org/jboss/msc/service/ServiceLogger", "ROOT,SERVICE,FAIL", "", AppWorkaroundKind::PostClinitFixup, "org/jboss/msc/service/ServiceLogger_$logger", "JBoss MSC ServiceLogger fixup"),
        ("org/slf4j/impl/", "", "", AppWorkaroundKind::PostClinitFixup, "", "SLF4J binder post-clinit fixup"),
        ("ch/qos/logback/", "", "", AppWorkaroundKind::PostClinitFixup, "", "Logback binder post-clinit fixup"),
        // Spring MergedAnnotation$Adapt.isIn bridge & AnnotationAttributes fallback
        (
            "org/springframework/core/annotation/MergedAnnotation$Adapt",
            "isIn",
            "([Lorg/springframework/core/annotation/MergedAnnotation$Adapt;)Z",
            AppWorkaroundKind::LoaderAwareEnumIsInBridge,
            "",
            "Spring MergedAnnotation$Adapt enum identity across forked classloaders",
        ),
        (
            "org/springframework/core/annotation/AnnotationAttributes",
            "",
            "",
            AppWorkaroundKind::AnnotationMapFallbackClass,
            "",
            "Spring AnnotationAttributes fallback map class for annotation proxy asMap",
        ),
        (
            "org/codehaus/groovy/vmplugin/v8/IndyInterface",
            "cast",
            "",
            AppWorkaroundKind::IndyBooleanCastRedirect,
            "org/codehaus/groovy/runtime/typehandling/DefaultTypeTransformation",
            "Groovy cast:(Object)Z coercion dispatches DefaultTypeTransformation.castToBoolean",
        ),
        (
            "org/jboss/logmanager/LogManager",
            "",
            "",
            AppWorkaroundKind::StubLayoutAlias,
            "java/util/logging/LogManager",
            "JBoss LogManager shares java.util.logging.LogManager synthetic stub layout",
        ),
    ];

    for &(class_pattern, method_name, descriptor, kind, aux, reason) in entries {
        registry.register_app_workaround(AppWorkaroundEntry {
            class_pattern,
            method_name,
            descriptor,
            library_version: None,
            reason,
            kind,
            aux,
        });
    }

    register_third_party_stub_layouts(registry);
}

fn register_third_party_stub_layouts(registry: &mut NativeMethodRegistry) {
    use cratonvm_native_api::SyntheticStubField as F;
    const fn f(name: &'static str, descriptor: &'static str) -> F {
        F {
            name,
            descriptor,
            is_public_static_final: false,
        }
    }
    const fn psf(name: &'static str, descriptor: &'static str) -> F {
        F {
            name,
            descriptor,
            is_public_static_final: true,
        }
    }

    static SERVICE_NAME: &[F] = &[
        f("segments", "[Ljava/lang/String;"),
        f("canonical", "Ljava/lang/String;"),
    ];
    static SERVICE_CONTROLLER: &[F] = &[
        f("name", "Lorg/jboss/msc/service/ServiceName;"),
        f("mode", "I"),
        f("state", "I"),
        f("value", "Ljava/lang/Object;"),
        f("listeners", "Ljava/util/ArrayList;"),
        f("_mscId", "J"),
    ];
    static SERVICE_CONTAINER: &[F] = &[
        f("services", "Ljava/util/concurrent/ConcurrentHashMap;"),
        f("workerPoolHandle", "J"),
    ];
    static MSC_CONTEXT: &[F] = &[f("controllerId", "J")];
    static JBOSS_MODULE_LOADER: &[F] = &[f("root", "Ljava/lang/String;")];
    static DEFAULT_BOOT_HOLDER: &[F] = &[psf("INSTANCE", "Lorg/jboss/modules/ModuleLoader;")];
    static JBOSS_MODULE: &[F] = &[
        f("name", "Ljava/lang/String;"),
        f("loader", "Lorg/jboss/modules/ModuleLoader;"),
        f("classLoader", "Lorg/jboss/modules/ModuleClassLoader;"),
        f("resourceRoots", "[Ljava/lang/String;"),
    ];
    static JBOSS_MCL: &[F] = &[f("module", "Lorg/jboss/modules/Module;")];
    static THROWABLE_2: &[F] = &[
        f("detailMessage", "Ljava/lang/String;"),
        f("cause", "Ljava/lang/Throwable;"),
    ];
    static DEPLOYMENT_UNIT: &[F] = &[
        f("name", "Ljava/lang/String;"),
        f("attachments", "Ljava/util/Map;"),
        f("serviceName", "Lorg/jboss/msc/service/ServiceName;"),
    ];
    static EMPTY: &[F] = &[];
    static ENHANCED_QUEUE_EXEC: &[F] = &[
        f("name", "Ljava/lang/String;"),
        f("coreSize", "I"),
        f("maxSize", "I"),
        f("tasksQueue", "Ljava/util/Queue;"),
    ];
    static JBOSS_LOGGER: &[F] = &[
        f("name", "Ljava/lang/String;"),
        f("parentHandle", "Ljava/lang/Object;"),
    ];
    static JBOSS_LEVEL: &[F] = &[f("name", "Ljava/lang/String;"), f("value", "I")];
    static MODEL_CONTROLLER: &[F] = &[f("state", "I")];
    static CONTROLLED_STATE: &[F] = &[f("stateOrdinal", "I")];
    static NAMING_STORE: &[F] = &[
        f("bindings", "Ljava/util/concurrent/ConcurrentHashMap;"),
        f("serviceBase", "Lorg/jboss/msc/service/ServiceName;"),
    ];
    static BIND_INFO: &[F] = &[
        f("binderServiceName", "Lorg/jboss/msc/service/ServiceName;"),
        f("bindingName", "Ljava/lang/String;"),
    ];
    static QUARKUS_RUNTIME_VALUE: &[F] = &[
        f("value", "Ljava/lang/Object;"),
        f("supplier", "Ljava/util/function/Supplier;"),
    ];
    static QUARKUS_APP_CONFIG: &[F] = &[
        f("name", "Ljava/lang/String;"),
        f("version", "Ljava/lang/String;"),
    ];
    static QUARKUS_DS_CONFIG: &[F] = &[
        f("jdbcUrl", "Ljava/lang/String;"),
        f("username", "Ljava/lang/String;"),
        f("password", "Ljava/lang/String;"),
        f("driver", "Ljava/lang/String;"),
    ];
    static QUARKUS_TIMING: &[F] = &[
        f("bootStart", "J"),
        f("bootStop", "J"),
        f("mainStart", "J"),
        f("mainStop", "J"),
    ];
    static QUARKUS_SERIALIZED_APP: &[F] = &[
        f("mainClass", "Ljava/lang/String;"),
        f("runnerClassLoader", "Lio/quarkus/bootstrap/runner/RunnerClassLoader;"),
    ];
    static QUARKUS_RUNNER_CL: &[F] = &[f("parent", "Ljava/lang/ClassLoader;")];

    static STUB_EXTRA_0: &[F] = &[f("pool", "Lio/agroal/pool/ConnectionPool;"), f("closed", "Z")];
    static STUB_EXTRA_1: &[F] = &[f("handlers", "Ljava/util/List;"), f("size", "I"), f("state", "I")];
    static STUB_EXTRA_2: &[F] = &[f("jdbcUrl", "Ljava/lang/String;"), f("driver", "Ljava/lang/String;"), f("username", "Ljava/lang/String;"), f("password", "Ljava/lang/String;"), f("minSize", "I"), f("maxSize", "I")];
    static STUB_EXTRA_3: &[F] = &[f("subPools", "Ljava/util/Map;")];
    static STUB_EXTRA_4: &[F] = &[f("connections", "Ljava/util/List;"), f("semaphore", "Ljava/util/concurrent/Semaphore;"), f("state", "I")];
    static STUB_EXTRA_5: &[F] = &[f("principals", "Ljava/util/Set;"), f("publicCreds", "Ljava/util/Set;"), f("privateCreds", "Ljava/util/Set;")];
    static STUB_EXTRA_6: &[F] = &[f("name", "Ljava/lang/String;"), f("subject", "Ljavax/security/auth/Subject;"), f("config", "Ljavax/security/auth/login/Configuration;")];
    static STUB_EXTRA_7: &[F] = &[f("loginModuleClassName", "Ljava/lang/String;"), f("options", "Ljava/util/Map;")];
    static STUB_EXTRA_8: &[F] = &[f("name", "Ljava/lang/String;"), f("authMgr", "Ljava/lang/Object;")];
    static STUB_EXTRA_9: &[F] = &[f("principal", "Ljava/security/Principal;"), f("roles", "Ljava/util/Set;")];
    static STUB_EXTRA_10: &[F] = &[f("jndiName", "Ljava/lang/String;"), f("poolHandle", "I"), f("state", "I")];
    static STUB_EXTRA_11: &[F] = &[f("currentTxId", "J")];
    static STUB_EXTRA_12: &[F] = &[f("singletonHandle", "J")];
    static STUB_EXTRA_13: &[F] = &[f("formatId", "I"), f("globalTransactionId", "J"), f("branchQualifier", "J")];
    static STUB_EXTRA_14: &[F] = &[f("listeners", "Ljava/lang/String;"), f("handler", "Lio/undertow/server/HttpHandler;"), f("workerThreads", "I"), f("ioThreads", "I"), f("boundFds", "J")];
    static STUB_EXTRA_15: &[F] = &[f("method", "Lio/undertow/util/HttpString;"), f("uri", "Ljava/lang/String;"), f("requestHeaders", "Lio/undertow/util/HeaderMap;"), f("requestBody", "Ljava/lang/String;"), f("responseStatus", "I"), f("responseHeaders", "Lio/undertow/util/HeaderMap;"), f("responseSender", "Lio/undertow/io/Sender;")];
    static STUB_EXTRA_16: &[F] = &[f("entriesMap", "J")];
    static STUB_EXTRA_17: &[F] = &[f("bytes", "Ljava/lang/String;")];
    static STUB_EXTRA_18: &[F] = &[f("name", "Ljava/lang/String;"), f("serverHandle", "J"), f("state", "I")];
    static STUB_EXTRA_19: &[F] = &[f("port", "I"), f("host", "Ljava/lang/String;"), f("boundAddress", "Ljava/net/InetSocketAddress;")];
    static STUB_EXTRA_20: &[F] = &[f("name", "Ljava/lang/String;"), f("ioThreadsArr", "[Lorg/xnio/XnioIoThread;"), f("taskThreadsCount", "I"), f("state", "I"), f("optionsHandle", "J")];
    static STUB_EXTRA_21: &[F] = &[f("name", "Ljava/lang/String;"), f("providerHandle", "J")];
    static STUB_EXTRA_22: &[F] = &[f("name", "Ljava/lang/String;"), f("ioThreadsArr", "[Lorg/xnio/XnioIoThread;"), f("taskThreadsCount", "I"), f("state", "I"), f("optionsHandle", "J")];
    static STUB_EXTRA_23: &[F] = &[f("channelId", "J"), f("selectionKey", "Ljava/nio/channels/SelectionKey;"), f("readListener", "Lorg/xnio/ChannelListener;"), f("readReadyFlag", "I"), f("readSuspended", "I")];
    static STUB_EXTRA_24: &[F] = &[f("channelId", "J"), f("selectionKey", "Ljava/nio/channels/SelectionKey;"), f("writeListener", "Lorg/xnio/ChannelListener;"), f("writeReadyFlag", "I"), f("writeSuspended", "I"), f("bufferedBytes", "J")];
    static STUB_EXTRA_25: &[F] = &[f("channelHandle", "Ljava/lang/Object;"), f("listenerSlotIndex", "I")];
    static STUB_EXTRA_26: &[F] = &[f("localAddress", "Ljava/net/SocketAddress;"), f("acceptListener", "Lorg/xnio/ChannelListener;"), f("closeListener", "Lorg/xnio/ChannelListener;"), f("open", "I"), f("resumed", "I"), f("worker", "Lorg/xnio/XnioWorker;"), f("listenerId", "J")];
    static STUB_EXTRA_27: &[F] = &[f("id", "J"), f("workerHandle", "Lorg/xnio/XnioWorker;"), f("selectorHandle", "J"), f("state", "I")];
    static STUB_EXTRA_28: &[F] = &[f("taskId", "J"), f("cancelled", "I")];
    static STUB_EXTRA_29: &[F] = &[f("declaringClass", "Ljava/lang/Class;"), f("name", "Ljava/lang/String;"), f("typeClass", "Ljava/lang/Class;")];
    static STUB_EXTRA_30: &[F] = &[f("entriesHandle", "J"), f("_f1", "Ljava/lang/Object;")];
    static STUB_EXTRA_31: &[F] = &[f("pendingHandle", "J"), f("_f1", "Ljava/lang/Object;")];
    static STUB_EXTRA_32: &[F] = &[f("status", "I"), f("resultSlot", "J"), f("notifierList", "J")];
    static STUB_EXTRA_33: &[F] = &[f("futureHandle", "J")];
    static STUB_EXTRA_35: &[F] = &[f("handle", "J"), f("configName", "Ljava/lang/String;"), f("started", "I")];
    static STUB_EXTRA_37: &[F] = &[f("handle", "J"), f("name", "Ljava/lang/String;")];
    static STUB_EXTRA_39: &[F] = &[f("name", "Ljava/lang/String;"), f("sizeLimit", "I"), f("ttlMs", "J")];
    static STUB_EXTRA_40: &[F] = &[f("name", "Ljava/lang/String;"), f("sizeLimit", "I"), f("ttlMs", "J")];
    static STUB_EXTRA_41: &[F] = &[f("siteName", "Ljava/lang/String;"), f("jmxEnabled", "I"), f("reserved", "I")];
    static STUB_EXTRA_42: &[F] = &[f("siteName", "Ljava/lang/String;"), f("jmxEnabled", "I"), f("reserved", "I")];
    static STUB_EXTRA_45: &[F] = &[f("containerId", "J")];
    static STUB_EXTRA_46: &[F] = &[f("containerId", "J"), f("beanRef", "Ljava/lang/Object;")];
    static STUB_EXTRA_47: &[F] = &[f("containerId", "J"), f("beanRef", "Ljava/lang/Object;")];
    static STUB_EXTRA_48: &[F] = &[f("containerId", "J"), f("beanRef", "Ljava/lang/Object;")];
    static STUB_EXTRA_49: &[F] = &[f("containerId", "J")];
    static STUB_EXTRA_50: &[F] = &[f("active", "I")];

    let layouts: &[(&'static str, &'static [F])] = &[
        ("org/jboss/msc/service/ServiceName", SERVICE_NAME),
        ("org/jboss/msc/service/ServiceController", SERVICE_CONTROLLER),
        ("org/jboss/msc/service/ServiceContainer", SERVICE_CONTAINER),
        ("org/jboss/msc/service/StartContext", MSC_CONTEXT),
        ("org/jboss/msc/service/StopContext", MSC_CONTEXT),
        ("org/jboss/modules/LocalModuleLoader", JBOSS_MODULE_LOADER),
        ("org/jboss/modules/ModuleLoader", JBOSS_MODULE_LOADER),
        ("org/jboss/modules/DefaultBootModuleLoaderHolder", DEFAULT_BOOT_HOLDER),
        ("org/jboss/modules/Module", JBOSS_MODULE),
        ("org/jboss/modules/ModuleClassLoader", JBOSS_MCL),
        ("org/jboss/modules/ModuleNotFoundException", THROWABLE_2),
        ("org/jboss/as/server/deployment/DeploymentUnit", DEPLOYMENT_UNIT),
        ("org/jboss/as/server/deployment/Services", EMPTY),
        ("org/jboss/threads/EnhancedQueueExecutor", ENHANCED_QUEUE_EXEC),
        ("org/jboss/logmanager/Logger", JBOSS_LOGGER),
        ("org/jboss/logmanager/Level", JBOSS_LEVEL),
        ("org/jboss/as/controller/ModelController", MODEL_CONTROLLER),
        ("org/jboss/as/controller/ControlledProcessState", CONTROLLED_STATE),
        ("org/jboss/as/naming/ServiceBasedNamingStore", NAMING_STORE),
        ("org/jboss/as/naming/deployment/ContextNames$BindInfo", BIND_INFO),
        ("io/quarkus/runtime/RuntimeValue", QUARKUS_RUNTIME_VALUE),
        ("io/quarkus/runtime/ApplicationConfig", QUARKUS_APP_CONFIG),
        ("io/quarkus/runtime/DataSourceRuntimeConfig", QUARKUS_DS_CONFIG),
        ("io/quarkus/runtime/Timing", QUARKUS_TIMING),
        ("io/quarkus/bootstrap/runner/SerializedApplication", QUARKUS_SERIALIZED_APP),
        ("io/quarkus/bootstrap/runner/RunnerClassLoader", QUARKUS_RUNNER_CL),
        ("io/agroal/api/AgroalDataSource", STUB_EXTRA_0),
        ("io/agroal/pool/ConnectionPool", STUB_EXTRA_1),
        ("io/agroal/api/configuration/AgroalDataSourceConfiguration", STUB_EXTRA_2),
        ("org/jboss/jca/core/connectionmanager/pool/AbstractPool", STUB_EXTRA_3),
        ("org/jboss/jca/core/connectionmanager/pool/ManagedConnectionPool", STUB_EXTRA_4),
        ("javax/security/auth/Subject", STUB_EXTRA_5),
        ("javax/security/auth/login/LoginContext", STUB_EXTRA_6),
        ("javax/security/auth/login/AppConfigurationEntry", STUB_EXTRA_7),
        ("org/jboss/as/security/SecurityDomainService", STUB_EXTRA_8),
        ("org/wildfly/security/auth/server/SecurityIdentity", STUB_EXTRA_9),
        ("org/jboss/as/connector/subsystems/datasources/DataSourceService", STUB_EXTRA_10),
        ("javax/transaction/TransactionManager", STUB_EXTRA_11),
        ("com/arjuna/ats/jta/TransactionManagerImple", STUB_EXTRA_12),
        ("javax/transaction/xa/Xid", STUB_EXTRA_13),
        ("io/undertow/Undertow", STUB_EXTRA_14),
        ("io/undertow/Undertow$Builder", STUB_EXTRA_14),
        ("io/undertow/server/HttpServerExchange", STUB_EXTRA_15),
        ("io/undertow/util/HeaderMap", STUB_EXTRA_16),
        ("io/undertow/util/HttpString", STUB_EXTRA_17),
        ("org/wildfly/extension/undertow/UndertowService", STUB_EXTRA_18),
        ("org/wildfly/extension/undertow/ListenerService", STUB_EXTRA_19),
        ("org/xnio/XnioWorker", STUB_EXTRA_20),
        ("org/xnio/Xnio", STUB_EXTRA_21),
        ("org/xnio/nio/NioXnioWorker", STUB_EXTRA_22),
        ("org/xnio/conduits/ConduitStreamSourceChannel", STUB_EXTRA_23),
        ("org/xnio/conduits/ConduitStreamSinkChannel", STUB_EXTRA_24),
        ("org/xnio/ChannelListener$Setter", STUB_EXTRA_25),
        ("org/xnio/channels/AcceptingChannel", STUB_EXTRA_26),
        ("org/xnio/XnioIoThread", STUB_EXTRA_27),
        ("org/xnio/nio/NioIoThread", STUB_EXTRA_27),
        ("org/xnio/XnioExecutor$Key", STUB_EXTRA_28),
        ("org/xnio/Option", STUB_EXTRA_29),
        ("org/xnio/OptionMap", STUB_EXTRA_30),
        ("org/xnio/OptionMap$Builder", STUB_EXTRA_31),
        ("org/xnio/IoFuture", STUB_EXTRA_32),
        ("org/xnio/FutureResult", STUB_EXTRA_33),
        ("org/xnio/Options", EMPTY),
        ("org/infinispan/manager/DefaultCacheManager", STUB_EXTRA_35),
        ("org/infinispan/manager/EmbeddedCacheManager", EMPTY),
        ("org/infinispan/cache/impl/CacheImpl", STUB_EXTRA_37),
        ("org/infinispan/Cache", EMPTY),
        ("org/infinispan/AdvancedCache", EMPTY),
        ("org/infinispan/configuration/cache/Configuration", STUB_EXTRA_39),
        ("org/infinispan/configuration/cache/ConfigurationBuilder", STUB_EXTRA_40),
        ("org/infinispan/configuration/global/GlobalConfiguration", STUB_EXTRA_41),
        ("org/infinispan/configuration/global/GlobalConfigurationBuilder", STUB_EXTRA_42),
        ("org/infinispan/notifications/cachelistener/CacheNotifier", EMPTY),
        ("io/quarkus/arc/Arc", EMPTY),
        ("io/quarkus/arc/ArcContainer", STUB_EXTRA_45),
        ("io/quarkus/arc/impl/ArcContainerImpl", STUB_EXTRA_45),
        ("io/quarkus/arc/InstanceHandle", STUB_EXTRA_46),
        ("io/quarkus/arc/impl/InstanceHandleImpl", STUB_EXTRA_46),
        ("io/quarkus/arc/InjectableBean", STUB_EXTRA_47),
        ("javax/enterprise/inject/Instance", STUB_EXTRA_48),
        ("jakarta/enterprise/inject/Instance", STUB_EXTRA_48),
        ("javax/enterprise/inject/spi/BeanManager", STUB_EXTRA_49),
        ("jakarta/enterprise/inject/spi/BeanManager", STUB_EXTRA_49),
        ("io/quarkus/arc/ManagedContext", STUB_EXTRA_50),
    ];

    for &(cls, fields) in layouts {
        registry.register_synthetic_stub_layout(cls, fields);
    }
}

#[cfg(test)]
mod tests;
