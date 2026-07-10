# CratonVM: Synthetic Methods & Runtime Modes

This document covers every method the VM implements natively in Rust (rather than
executing real JDK bytecode), and every runtime mode / configuration flag.

---

## Part 1 — Synthetic Methods

There are five distinct tiers, from "pure performance shortcut" to "legacy stub":

---

### Tier 1: Interpreter Intrinsics (fast-path table)

**Source**: `native-builtins/src/intrinsics/` — 8 files.

These are dispatched via an enum tag at IC-fill time (once per call site), then hit a
direct `match` arm with no `HashMap` probe.  Behavior is byte-for-byte identical to the
normal native-registry path — the handlers delegate to the same `lang_*` functions;
they exist purely for dispatch speed.

| Class | Methods |
|---|---|
| `java/lang/Object` | `getClass`, `hashCode` |
| `java/lang/String` | `length`, `charAt`, `isEmpty`, `indexOf` (×4 overloads), `lastIndexOf` |
| `java/lang/System` | `arraycopy` |
| `java/lang/StringBuilder` | `append` (String, int, char, long, bool, Object), `toString`, `length` |
| `java/lang/Integer` | `parseInt`, `valueOf`, `intValue` |
| `java/lang/Long` | `parseLong`, `valueOf`, `longValue` |
| `java/lang/Math` | `abs` (I/J/D/F), `min`/`max` (I/J), `sqrt`, `floor`, `ceil`, `sin`, `cos`, `log`, `pow` |

**~35 entry points total.**

---

### Tier 2: Essential Natives (~150, always active in real-JDK mode)

**Source**: `native-builtins/src/lib.rs` (39 KLOC, ~60 registration functions) plus
dedicated files.

These are the Rust-backed methods that must exist even when running real JDK bytecode.
They provide the JVM support surface that genuinely cannot be implemented in bytecode —
the JDK's own `.class` files call down into them via `native` declarations.

#### Object lifecycle & monitors
- `Object.<init>`, `registerNatives`, `hashCode0`, `clone`
- `Object.notify`, `notifyAll`, `wait` (3 overloads)

#### Class metadata & classloading
- `Class.registerNatives`, `getName0`, `forName0`, `getPrimitiveClass`
- `Class.getSuperclass0`, `getInterfaces0`, `getComponentType0`, `isInterface`, `isArray`, `isPrimitive`
- `ClassLoader.registerNatives`, `defineClass1`, `findBootstrapClass`

#### Thread & synchronization
- `Thread.registerNatives`, `start0`, `isAlive0`, `currentThread`, `sleep0`, `interrupt0`, `isInterrupted0`

#### System & runtime
- `System.currentTimeMillis`, `nanoTime`, `arraycopy`, `identityHashCode`, `initProperties`
- `System.setIn0`, `setOut0`, `setErr0`
- `Runtime.availableProcessors`, `freeMemory`, `totalMemory`, `gc`

#### Reflection
- `Field.get`/`set` (all primitive types), `Method.invoke0`, `Constructor.newInstance0`
- `Class.getDeclaredFields0`, `getDeclaredMethods0`, `getDeclaredConstructors0`

#### Floating-point bit conversions (no bytecode form exists)
- `Float.floatToRawIntBits`, `intBitsToFloat`
- `Double.doubleToRawLongBits`, `longBitsToDouble`

#### Unsafe / memory
- `Unsafe.*` (compareAndSet, put/get for all primitives, allocateMemory, etc.)

#### GC support
- `Reference.refersTo0`, `Reference.clear0`, `Reference.get0`

#### Crypto hardware bridge
Source: `sunec_point.rs`, `sunec_intpoly.rs`, `zip_crc32c.rs`, `biginteger_intrinsics.rs`
- SunEC P-256 point arithmetic (active when `route_ec_to_real()` is true; disabled by `CRATONVM_SYNTHETIC_EC=1`)
- BigInteger arithmetic: add, subtract, multiply, divide, modPow, gcd, shift, toString
- CRC32C hardware acceleration

---

### Tier 3: Synthetic Stubs (`synthetic-jdk` feature — DEFAULT OFF)

**Source**: guarded by `#[cfg(feature = "synthetic-jdk")]` throughout `lib.rs`,
`native-collections/`, `native-io/`.

When this feature is enabled, Rust implementations replace real JDK bytecode
class-by-class.  This is the legacy mode predating the NEW-5 jimage reader and is
**intentionally absent from the default feature set** (see the comment in
`vm/Cargo.toml`).

Some subsystems remain registered in the default build but have runtime kill-switches
that redirect to real JDK bytecode:

| Subsystem | Default state | Override flag |
|---|---|---|
| RustCrypto EC | ON (RustCrypto) | `CRATONVM_SYNTHETIC_EC=1` → revert to legacy stub |
| RustCrypto PQC | ON (RustCrypto) | `CRATONVM_SYNTHETIC_PQC=1` → revert to legacy stub |
| RustCrypto RSA | ON (RustCrypto) | `CRATONVM_SYNTHETIC_RSA=1` → revert to legacy stub |
| Synthetic ForkJoinPool | ON (synthetic) | `CRATONVM_REAL_FORKJOINPOOL` → use real JDK |
| Synthetic ReentrantLock / AQS | ON (synthetic) | `CRATONVM_REAL_AQS` → use real JDK |
| Synthetic socket layer | ON (synthetic) | `CRATONVM_REAL_NET_SOCKETS` → use real JDK |
| Synthetic RandomAccessFile | ON (synthetic) | `CRATONVM_REAL_RAF=1` → use real JDK |
| Synthetic annotation dispatch | OFF by default | `CRATONVM_SYNTHETIC_ANNOTATIONS=1` or `CRATONVM_REAL_ANNOTATIONS=0` → use legacy synthetic annotation objects |

---

### Tier 4: Framework / App-Specific Extras

**Source**: `native-builtins/src/*_extras.rs` and `wildfly_*.rs`, `jboss_*.rs`,
`xnio_*.rs`, `quarkus_*.rs`, etc. — ~100 files.

Shims, bootstrap patches, and compatibility bridges for specific applications.  Most are
registered unconditionally but are thin delegates or no-ops when the target app is not
running.

#### WildFly / JBoss stack (~15 files)
`wildfly_core.rs`, `wildfly_undertow.rs`, `wildfly_datasources_tx.rs`,
`wildfly_security.rs`, `wildfly_naming.rs`, `wildfly_method_synth.rs`,
`jboss_msc.rs`, `jboss_module_loader.rs`, `jboss_extras.rs`, `jboss_logmanager.rs`,
`jboss_jdkspecific.rs`, `jboss_resource_loader.rs`, `jboss_module_xml.rs`,
`xnio_worker.rs`, `xnio_async.rs`, `xnio_conduits.rs`, `xnio_io_thread.rs`,
`ironjacamar_pool.rs`

#### App servers & frameworks
`spring_startup_bootstrap.rs`, `quarkus_arc.rs`, `quarkus_staticinit.rs`,
`jetty_extras.rs`, `servlet.rs`, `glassfish_extras.rs`, `liberty_extras.rs`,
`keycloak16_extras.rs`, `vertx_eventloop.rs`, `letsgo_compat.rs`

#### Distributed systems
`cassandra_extras.rs`, `spark_extras.rs`, `hadoop_extras.rs`,
`elasticsearch_extras.rs`, `neo4j_extras.rs`, `hazelcast_extras.rs`,
`ignite_extras.rs`, `hbase_extras.rs`, `activemq_extras.rs`, `rabbitmq_extras.rs`,
`flink_extras.rs`, `infinispan_local.rs`, `agroal_pool.rs`

#### Build tools / IDEs / other
`gradle_extras.rs`, `eclipse_extras.rs`, `netbeans_extras.rs`, `jenkins_extras.rs`,
`nexus_extras.rs`, `sonar_extras.rs`, `bytebuddy_extras.rs`, `cglib_extras.rs`,
`cglib_enhancer.rs`, `grpc_extras.rs`, `arduino_extras.rs`, `bluej_extras.rs`,
`demo_extras.rs`, `freemind_extras.rs`, `jedit_extras.rs`, `jdownloader_extras.rs`,
`mindustry_extras.rs`, `picocli` (via `CRATONVM_DBG_PICOCLI_STYLE`)

---

### Tier 5: Experimental / Incomplete (feature-gated)

| File(s) | Feature | In default build | Status |
|---|---|---|---|
| `serialization.rs` | `experimental-serialization` | YES | Partial — ObjectInputStream/ObjectOutputStream |
| `aot.rs`, `aot_pipeline.rs` | `experimental-aot` | YES | GraalVM compat stubs |
| `jmx.rs`, `jmx_openmbean.rs` | `experimental-jmx` | YES | Partial JMX bean registration |
| `tls.rs`, `tls_impl.rs`, `t27_tls.rs` | `experimental-tls` | YES | TLS/SSL |
| `bc_aes.rs`, `bc_chacha.rs`, `bc_newhope.rs`, `bc_newhope_tables.rs` | `legacy-synthetic-crypto` | **NO** | Old Bouncy Castle replacements |
| `craton_gpu.rs` | `gpu-offload` | **NO** | GPU marshalling |
| `jdk25_concurrency.rs`, `jdk25_language.rs`, `jdk25_patterns.rs`, `unsafe_jdk25.rs` | (none) | YES | JDK 25 forward-compat stubs |

---

## Part 2 — Runtime Modes

The VM has five independent configuration axes.

---

### Axis 1: JIT compilation

| Flag | Default | Effect |
|---|---|---|
| `CRATONVM_DISABLE_JIT` | off | Full interpreter mode |
| `CRATONVM_JIT_THRESHOLD` | `2` | Invocation count before compile |
| `CRATONVM_JIT_VIRTUAL_TIERUP` | `1` (on) | Layer-B tier-up for virtual methods |
| `CRATONVM_PRECISE_JIT_MAPS` | off | Precise GC maps for JIT frames |

JIT sub-optimizations (each can be disabled independently):

| Flag | What it disables |
|---|---|
| `CRATONVM_DISABLE_AALOAD_LICM` | Array-load LICM hoisting |
| `CRATONVM_DISABLE_ARITH_LICM` | Arithmetic LICM hoisting |
| `CRATONVM_DISABLE_SCALAR_REPLACEMENT` | Scalar replacement / escape analysis |
| `CRATONVM_DISABLE_UNROLL` | Loop unrolling |
| `CRATONVM_JIT_NO_BCE` | Bounds-check elimination |
| `CRATONVM_JIT_NO_DUPX` | `dup_x1` / `dup_x2` canonicalization |
| `CRATONVM_JIT_NO_DUP_X1` | `dup_x1` canon only |
| `CRATONVM_JIT_NO_DUP_X2` | `dup_x2` canon only |
| `CRATONVM_JIT_NO_LONG_INTRINSICS` | Long-type intrinsic lowering |
| `CRATONVM_JIT_NO_SPEC_BCE` | Speculative BCE |
| `CRATONVM_JIT_DISABLE_INLINE_NEW` | Inline object allocation |
| `CRATONVM_INLINE_ALLOW_STATIC` | Static-method inlining |

---

### Axis 2: JDK bytecode source (compile-time feature)

| Build | Feature flag | What runs for `java.*` classes |
|---|---|---|
| **Default** | *(none)* | Real JDK bytecode from `$JAVA_HOME/lib/modules`; ~150 essential natives in Rust |
| **Legacy synthetic** | `--features synthetic-jdk` | Rust stubs replace real bytecode entirely |

---

### Axis 3: Per-subsystem real-vs-synthetic overrides (runtime)

Implemented via `RealSelector` in `vm/src/runtime/env_cache.rs`.  These allow
differential testing even in the default build — set a flag to run real JDK bytecode
for one subsystem while everything else stays on synthetic paths.

| Flag | Direction | Subsystem |
|---|---|---|
| `CRATONVM_REAL=all` | → real | All synthetic stubs bypassed |
| `CRATONVM_REAL=jca` | → real | `java/security/`, `javax/crypto/`, `sun/security/` |
| `CRATONVM_REAL=<classname>` | → real | Exact class (comma-separated list) |
| `CRATONVM_REAL_JCA` | → real | Legacy alias for `CRATONVM_REAL=jca` |
| `CRATONVM_REAL_FORKJOINPOOL` | → real | ForkJoinPool + work-stealing |
| `CRATONVM_REAL_NET_SOCKETS` | → real | `sun/nio/ch/Net`, socket layer |
| `CRATONVM_REAL_AQS` | → real | AbstractQueuedSynchronizer → ReentrantLock etc. |
| `CRATONVM_REAL_RAF=1` | → real | RandomAccessFile |
| `CRATONVM_REAL_ANNOTATIONS=0` | → synthetic | Annotation dispatch opt-out |
| `CRATONVM_ECLIPSE_REAL=1` | → real | Eclipse JDT-specific natives |

---

### Axis 4: Crypto backend

| Flag | Default | Effect |
|---|---|---|
| *(unset)* | RustCrypto | EC, PQC, RSA all via RustCrypto |
| `CRATONVM_SYNTHETIC_EC=1` | off | Revert to legacy synthetic EC stubs |
| `CRATONVM_SYNTHETIC_PQC=1` | off | Revert to legacy synthetic PQC stubs |
| `CRATONVM_SYNTHETIC_RSA=1` | off | Revert to legacy bare-interface RSA stubs |
| `CRATONVM_NATIVE_EC_MULTIPLY` | off | Force-enable P-256 native multiply |

---

### Axis 5: WildFly / JBoss-specific modes

| Flag | Default | Effect |
|---|---|---|
| `CRATONVM_USE_WILDFLY_MAIN_SHIM=1` | off | Re-enable legacy WildFly main shim |
| `CRATONVM_USE_WILDFLY_SYNTH_BYTECODE=1` | off | Re-enable WildFly synthetic bytecode patches |
| `CRATONVM_USE_WILDFLY_REFLECT_SHIM=1` | off | Re-enable WildFly reflection shim |
| `CRATONVM_MSC_REAL_START` | off | Use real MSC `service.start` (vs synthetic pump) |
| `CRATONVM_SKIP_JBOSS_PLUMBING=1` | off | Skip JBoss modules plumbing registration |
| `CRATONVM_WILDFLY_SHORTCIRCUIT=1` | off | Emergency no-op fallback for WildFly dispatch |

---

## Part 3 — Diagnostic Flags

All `CRATONVM_DBG_*` and `CRATONVM_DIAG_*` flags.  None change correctness; all are
off by default.

### JIT diagnostics
`CRATONVM_DBG_JIT_DISASM`, `CRATONVM_DBG_JIT_DISPATCH`, `CRATONVM_DBG_JIT_ENTRY`,
`CRATONVM_DBG_JIT_COMPILE`, `CRATONVM_DBG_JITC`, `CRATONVM_DBG_JIT_CODE`,
`CRATONVM_DBG_JIT_GEN`, `CRATONVM_DBG_JIT_LDC`, `CRATONVM_DBG_JIT_MIC`,
`CRATONVM_DBG_JIT_NAMES`, `CRATONVM_DBG_JIT_PUTFIELD`, `CRATONVM_DBG_JIT_ALLOC`,
`CRATONVM_DBG_MIC_PROF`, `CRATONVM_DBG_DEOPT`, `CRATONVM_DBG_OSR`,
`CRATONVM_DBG_DUMP_JIT`, `CRATONVM_DBG_DUPX_METHODS`, `CRATONVM_DBG_BB`,
`CRATONVM_DBG_BBLP`

JIT bisection: `CRATONVM_JIT_BISECT_SKIP`, `CRATONVM_JIT_BISECT_ONLY`,
`CRATONVM_JIT_ALLOW_PACKAGES`, `CRATONVM_JIT_UNBAN_JUNITCORE`, `CRATONVM_JIT_FREE_CODE`,
`CRATONVM_JIT_DUPX_EAGER_CANON`, `CRATONVM_JIT_MAIN_INLINE`

### GC & memory diagnostics
`CRATONVM_DBG_GC_STRESS`, `CRATONVM_GC_VERIFY_STALE`, `CRATONVM_DBG_SWEEP_ZERO`,
`CRATONVM_DBG_SWEEP_EDGES`, `CRATONVM_DBG_FORCE_MOVING`, `CRATONVM_DBG_GCWRITE`,
`CRATONVM_DBG_FWDGUARD`, `CRATONVM_FWD_RESOLVE_STRICT`, `CRATONVM_DBG_SEEDHUNT`,
`CRATONVM_DBG_SEED_ALL_OLD`, `CRATONVM_DBG_RSET_AUDIT`, `CRATONVM_DBG_PRECISE`,
`CRATONVM_GC_ARRAY_GUARD_BT`, `CRATONVM_DBG_HEAPCOPY`, `CRATONVM_DBG_HEAP_STALE`,
`CRATONVM_DBG_MEMWATCH`, `CRATONVM_DBG_YOUNGSCAN`, `CRATONVM_DBG_BLOCKGC`,
`CRATONVM_DBG_FULLSTACK_SCAN`, `CRATONVM_DBG_ANONALLOC`, `CRATONVM_DBG_VALIDATE_NEW`,
`CRATONVM_DBG_VERIFY_OOP_MAPS`, `CRATONVM_DBG_NO_PRUNE`

Shadow stack: `CRATONVM_SHADOW_STACK`, `CRATONVM_SHADOW_PIN`, `CRATONVM_SHADOW_NOPUSH`,
`CRATONVM_SHADOW_NORELOAD`, `CRATONVM_SHADOW_NO_SAVEBASE`, `CRATONVM_SHADOW_OSR_TRACK`,
`CRATONVM_SHADOW_RAW_RELOAD`, `CRATONVM_SHADOW_SENTINEL`, `CRATONVM_SHADOW_WATCH`,
`CRATONVM_DBG_SHADOW`, `CRATONVM_DBG_SHADOW2`, `CRATONVM_DBG_SHADOW_DEPTH`

Selective promotion: `CRATONVM_SELECTIVE_PROMOTE`, `CRATONVM_NO_SELECTIVE_PROMOTE`,
`CRATONVM_SP_STATS`, `CRATONVM_SP_TRACE`, `CRATONVM_SP_VERIFY`, `CRATONVM_SP_NO_COALESCE`

Root snapshots: `CRATONVM_DBG_ROOTSNAP`, `CRATONVM_ROOTSNAP_CACHE`,
`CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC`, `CRATONVM_SKIP_REDUNDANT_NATIVE_SNAPSHOT`,
`CRATONVM_NO_JIT_SCAN_CACHE`, `CRATONVM_DBG_LONGROOT`, `CRATONVM_LONGROOT_STRICT`,
`CRATONVM_YOUNGSCAN_STRIDE`

### Interpreter & dispatch diagnostics
`CRATONVM_DBG_OVERLAY`, `CRATONVM_DBG_OVERLAY_ALL`, `CRATONVM_DBG_BADRECV`,
`CRATONVM_DBG_FIELDADDR`, `CRATONVM_DBG_STRAYSTACK`, `CRATONVM_DBG_ARRSTORE`,
`CRATONVM_DBG_ARRLEN`, `CRATONVM_DBG_STALELONG`, `CRATONVM_DBG_STALE_RECV`,
`CRATONVM_DBG_NULLTHIS`, `CRATONVM_DBG_OOBFIELD`, `CRATONVM_DBG_BADREF`,
`CRATONVM_DBG_NULL_NATIVE`, `CRATONVM_DBG_NO_REFPROC`, `CRATONVM_DBG_NO_CLEANERS`,
`CRATONVM_FRAME_TRACE`, `CRATONVM_DBG_CORRUPT_FRAMES`, `CRATONVM_DEBUG_STACK_TAG`,
`CRATONVM_DBG_UNDERFLOW`, `CRATONVM_DBG_STACKLESS`, `CRATONVM_DBG_RESUME_PC`,
`CRATONVM_DBG_MONENTER`, `CRATONVM_DBG_VDISP`, `CRATONVM_DBG_CCSPROBE`,
`CRATONVM_DBG_STTRACE`, `CRATONVM_DEBUG_STACKWALK`, `CRATONVM_DEBUG_SFI`,
`CRATONVM_DBG_LAMBDA`, `CRATONVM_DBG_NOCODE`, `CRATONVM_DBG_MODSTATIC`,
`CRATONVM_DBG_AIOOBE`, `CRATONVM_DBG_AIOOBE2`, `CRATONVM_DBG_ARRAYCOPY`,
`CRATONVM_DBG_CLONE`, `CRATONVM_DBG_COMPONENT_TYPE`, `CRATONVM_DBG_OBJECTS`,
`CRATONVM_DBG_OBJ_EQUALS`, `CRATONVM_DBG_EQE`, `CRATONVM_DBG_TOARRAY`,
`CRATONVM_DBG_SBLOAD`, `CRATONVM_DBG_STREAMSUPP`, `CRATONVM_DBG_ASSERTEQ`,
`CRATONVM_DBG_CAPVAL`, `CRATONVM_DBG_INVOKE_COERCE`, `CRATONVM_DBG_METHOD_INVOKE_BOX`,
`CRATONVM_DBG_MINVOKE`, `CRATONVM_DBG_DUPX_METHODS`

### Exception diagnostics
`CRATONVM_DBG_NPE_TRACE`, `CRATONVM_DBG_NPE_INVOKE`, `CRATONVM_DBG_NPE_STACK`,
`CRATONVM_DBG_WF_NPE`, `CRATONVM_DBG_NSME`, `CRATONVM_DBG_NCDFE`,
`CRATONVM_DBG_CCE`, `CRATONVM_DBG_ATHROW`, `CRATONVM_DBG_SOE`,
`CRATONVM_IAE_TRACE`, `CRATONVM_NSEE_TRACE`, `CRATONVM_DBG_AIOOBE`,
`CRATONVM_DBG_AIOOBE2`, `CRATONVM_STRICT_SWALLOWS`

### Method resolution & reflection diagnostics
`CRATONVM_DBG_PROXY`, `CRATONVM_DBG_LOOKUP`, `CRATONVM_DBG_MCL`,
`CRATONVM_DBG_UCLREG`, `CRATONVM_DBG_URLCL`, `CRATONVM_DBG_GETRESOURCES`,
`CRATONVM_DBG_CALLER`, `CRATONVM_DBG_DOPRIV`, `CRATONVM_DBG_FSP`,
`CRATONVM_ANN_TRACE`, `CRATONVM_DIAG_JCA`, `CRATONVM_DIAG_SERVICELOADER`,
`CRATONVM_DIAG_PROPERTIES`, `CRATONVM_DIAG_METHOD_INVOKE_NULL`

### Crypto & encoding diagnostics
`CRATONVM_DBG_CHARSET`, `CRATONVM_DBG_ECWATCH`, `CRATONVM_DBG_ECWATCH_NATIVE`,
`CRATONVM_DBG_PBE`, `CRATONVM_DBG_PBSTART`, `CRATONVM_BD_DEBUG`, `CRATONVM_DBG_TOHEX`,
`CRATONVM_DBG_LOGPROV`, `CRATONVM_DBG_SEEDHUNT`, `CRATONVM_DBG_SEL`

### Network & I/O diagnostics
`CRATONVM_DBG_NET`, `CRATONVM_DBG_NIO_BIND`, `CRATONVM_DBG_RAF_INIT`,
`CRATONVM_DBG_RAF_GETFD`, `CRATONVM_DBG_JLM`

### Framework-specific diagnostics
`CRATONVM_DBG_WF`, `CRATONVM_DBG_WF_NPE`, `CRATONVM_DBG_MSC`, `CRATONVM_MSC_DBG`,
`CRATONVM_DBG_JETTY`, `CRATONVM_DBG_JETTY2`, `CRATONVM_DBG_CATALINA`,
`CRATONVM_DBG_LETSGO`, `CRATONVM_SPRING_DBG`, `CRATONVM_S111_DBG`,
`CRATONVM_DBG_PICOCLI_STYLE`, `CRATONVM_DBG_UTE`, `CRATONVM_DBG_THREADSTART`,
`CRATONVM_DBG_EXIT`, `CRATONVM_DBG_SBLOAD`, `CRATONVM_TRACE_SB_FILTER`,
`CRATONVM_TRACE_UNIMPLEMENTED`, `CRATONVM_DBG_ASSERTEQ`

### Other control flags
`CRATONVM_JAVA_HOME`, `CRATONVM_EXEC_DEPTH_CEILING`, `CRATONVM_ENABLE_ASSERTIONS`,
`CRATONVM_LENIENT_CLINIT`, `CRATONVM_MODULES_MARKER`, `CRATONVM_STACK`,
`CRATONVM_SOFT_EXIT`, `CRATONVM_SYMBOLIZE`, `CRATONVM_SYMBOLIZE_DBG`,
`CRATONVM_GPU_TRACE_BYTES`, `CRATONVM_BUGS`, `CRATONVM_EQE_SYNC_EXECUTE`,
`CRATONVM_AWAIT_NO_SHORTCIRCUIT`, `CRATONVM_FORCE_WIN_BUILD`,
`CRATONVM_JBOSS_BOOT_LOG_FILE`, `CRATONVM_JBOSS_BRUTE_FORCE_JARS`,
`CRATONVM_JBOSS_MP_ROOT`, `CRATONVM_USE_KC16_MAIN_SHIM`,
`CRATONVM_UEH_DEBUG`, `CRATONVM_DBG_PBSTART`

---

## Part 4 — Consolidation Problems

Three specific inconsistencies are worth resolving.

### Problem A: Per-subsystem `CRATONVM_REAL_*` flags are redundant with `CRATONVM_REAL`

`CRATONVM_REAL` already supports `=jca`, `=all`, or exact class names and is handled
centrally by `RealSelector` in `vm/src/runtime/env_cache.rs`.  However, eight
per-subsystem flags bypass it and gate at the registration call site in `lib.rs`:

```
CRATONVM_REAL_JCA           (already a documented legacy alias in env_cache.rs)
CRATONVM_REAL_FORKJOINPOOL
CRATONVM_REAL_NET_SOCKETS
CRATONVM_REAL_AQS
CRATONVM_REAL_RAF
CRATONVM_REAL_ANNOTATIONS
CRATONVM_ECLIPSE_REAL
```

**Proposed fix**: add group tokens (`fjp`, `net`, `aqs`, `raf`, `annotations`, `eclipse`)
to `RealSelector::parse` and deprecate the individual flags.  `CRATONVM_REAL_JCA` is
already on this path.

### Problem B: Inverted naming convention for crypto

`CRATONVM_SYNTHETIC_EC=1` means "use the legacy fake" (less real), while
`CRATONVM_REAL_JCA` means "use real JDK" (more real).  The conventions are opposite,
which makes the matrix of crypto flags confusing.

**Proposed fix**: rename `CRATONVM_SYNTHETIC_{EC,PQC,RSA}` to
`CRATONVM_REAL_{EC,PQC,RSA}` (absent = RustCrypto default; set = prefer real JDK),
consistent with the rest of the REAL family.  Keep the old names as no-op aliases for
one release cycle.

### Problem C: WildFly mode flags are scattered across six files with no central registry

`CRATONVM_USE_WILDFLY_MAIN_SHIM`, `CRATONVM_USE_WILDFLY_SYNTH_BYTECODE`,
`CRATONVM_USE_WILDFLY_REFLECT_SHIM`, `CRATONVM_WILDFLY_SHORTCIRCUIT`,
`CRATONVM_SKIP_JBOSS_PLUMBING`, `CRATONVM_MSC_REAL_START` each live in a different
source file with no shared parsing or documentation.

**Proposed fix**: a single `CRATONVM_WILDFLY=<token-list>` following the same
comma-token model as `CRATONVM_REAL`, parsed once at startup into a cached struct.

---

## Summary

| Category | Count |
|---|---|
| Interpreter intrinsic table entries | ~35 |
| Essential natives (real-JDK mode) | ~150 |
| Framework / app extras source files | ~100 |
| `CRATONVM_DBG_*` diagnostic flags | ~80 |
| Other control / mode flags | ~120 |
| **Total `CRATONVM_*` env vars** | **~200** |
| Compile-time feature flags | 11 |
| Per-subsystem REAL flags (consolidation candidates) | 8 |
