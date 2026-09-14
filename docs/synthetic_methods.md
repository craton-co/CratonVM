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
- SunEC P-256 point arithmetic (active when `route_ec_to_real()` is true; disabled by `CRATONVM_REAL=-ec`)
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
| RustCrypto EC | ON (RustCrypto) | `CRATONVM_REAL=-ec` → revert to legacy stub |
| RustCrypto PQC | ON (RustCrypto) | `CRATONVM_REAL=-pqc` → revert to legacy stub |
| RustCrypto RSA | ON (RustCrypto) | `CRATONVM_REAL=-rsa` → revert to legacy stub |
| Synthetic ForkJoinPool | ON (synthetic) | `CRATONVM_REAL=forkjoinpool` → use real JDK |
| Synthetic ReentrantLock / AQS | ON (synthetic) | `CRATONVM_REAL=aqs` → use real JDK |
| Synthetic socket layer | ON (synthetic) | `CRATONVM_REAL=net-sockets` → use real JDK |
| Synthetic RandomAccessFile | ON (synthetic) | `CRATONVM_REAL=raf` → use real JDK |
| Synthetic annotation dispatch | OFF by default | `CRATONVM_REAL=-annotations` → use legacy synthetic annotation objects |

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

The `experimental-*` features left the `cratonvm-vm` default set, so an ordinary
`cargo build` no longer requests them and each must be asked for explicitly
(`--features experimental-aot`, and so on). `synthetic-jdk` still implies
`management`, because the legacy synthetic surface includes JMX bootstrap
classes.

Whether that actually removes code depends on the feature. In
`cratonvm-native-builtins`, `jmx`/`jmx_openmbean`, `aot`/`aot_pipeline` and
`serialization` are `#[cfg]`-gated module declarations, so dropping the feature
stops compiling them.

Two features were renamed because their names described an intent
the code does not have. Both old names remain as back-compat aliases so existing
`--features` invocations keep building; both are slated for removal in 0.4.

| Old name | New name | Why the old name was wrong |
|---|---|---|
| `experimental-jmx` | `management` | Not an experiment — it is default-on and load-bearing — and not confined to JMX: it gates the `sun.management` natives behind `java.lang.management` as well as the `javax.management` beans layered on top. |
| `experimental-tls` | `deprecated-noop-tls` | It gates nothing at all. There is not one `#[cfg(feature = ...)]` site for it anywhere in the tree. |

`management` stays in the default set because
`java.lang.management.ManagementFactory` is core JDK API the JDK's own bootstrap
reaches. Removing it makes `getMemoryPoolMXBeans()` die with
`UnsatisfiedLinkError: sun/management/VMManagementImpl.getVersion0()`, which
`vm/tests/wave1_a_jmx_mxbeans.rs` pins.

`deprecated-noop-tls` (NEW-13) enables nothing: the native-tls-backed
`javax.net.ssl` implementation is always compiled and registered. It is retained
only so downstream feature requests and `check-cfg` keep resolving.

| File(s) | Feature | In default build | Status |
|---|---|---|---|
| `serialization.rs` | `experimental-serialization` | **NO** | Partial — ObjectInputStream/ObjectOutputStream |
| `aot.rs`, `aot_pipeline.rs` | `experimental-aot` | **NO** | GraalVM compat stubs |
| `jmx.rs`, `jmx_openmbean.rs` | `management` (was `experimental-jmx`) | **YES** — required by `ManagementFactory` | Partial JMX bean registration |
| `tls.rs`, `tls_impl.rs`, `t27_tls.rs` | none — `deprecated-noop-tls` (was `experimental-tls`) gates nothing | **YES — always compiled** | TLS/SSL |
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

Every per-subsystem override is now a token in `CRATONVM_REAL`; the plain form
selects the real implementation and `-token` selects the synthetic shim. See
[`docs/flag-tokens.md`](flag-tokens.md#cratonvm_real) for the full list.

| Flag | Direction | Subsystem |
|---|---|---|
| `CRATONVM_REAL=all` | → real | All synthetic stubs bypassed |
| `CRATONVM_REAL=-stubs` | → real | Drop every `SyntheticStub` native at registration |
| `CRATONVM_REAL=jca` | → real | `java/security/`, `javax/crypto/`, `sun/security/` |
| `CRATONVM_REAL=<classname>` | → real | Exact class (comma-separated list) |
| `CRATONVM_REAL=forkjoinpool` | → real | ForkJoinPool + work-stealing |
| `CRATONVM_REAL=net-sockets` | → real | `sun/nio/ch/Net`, socket layer |
| `CRATONVM_REAL=aqs` / `-aqs` | → real / synthetic | AbstractQueuedSynchronizer → ReentrantLock etc. |
| `CRATONVM_REAL=raf` / `-raf` | → real / synthetic | RandomAccessFile |
| `CRATONVM_REAL=-annotations` | → synthetic | Annotation dispatch opt-out |

> `CRATONVM_ECLIPSE_REAL` used to be listed here. It has **no read site** —
> nothing in the VM has ever consulted it, and neither have the other 33
> per-application `CRATONVM_<APP>_REAL` variables that `scripts/real-run-all.sh`
> exported. Use `CRATONVM_REAL=-stubs`, which is what that script now does.

---

### Axis 4: Crypto backend

| Flag | Default | Effect |
|---|---|---|
| *(unset)* | RustCrypto | EC, PQC, RSA all via RustCrypto |
| `CRATONVM_REAL=-ec` | off | Revert to legacy synthetic EC stubs |
| `CRATONVM_REAL=-pqc` | off | Revert to legacy synthetic PQC stubs |
| `CRATONVM_REAL=-rsa` | off | Revert to legacy bare-interface RSA stubs |
| `CRATONVM_JIT=native-ec-multiply` | off | Force-enable P-256 native multiply |

---

### Axis 5: WildFly / JBoss-specific modes

| Flag | Default | Effect |
|---|---|---|
| `CRATONVM_REAL=use-wildfly-synth-bytecode` | off | Re-enable WildFly synthetic bytecode patches |
| `CRATONVM_REAL=use-wildfly-reflect-shim` | off | Re-enable WildFly reflection shim |
| `CRATONVM_REAL=msc-real-start` | **on** | Use real MSC `service.start` (vs synthetic pump); `-msc-real-start` for the pump |

> `CRATONVM_USE_WILDFLY_MAIN_SHIM`, `CRATONVM_SKIP_JBOSS_PLUMBING` and
> `CRATONVM_WILDFLY_SHORTCIRCUIT` used to be listed here with a documented
> effect. All three have **no read site** — setting them has never done
> anything. They are removed rather than re-spelled.

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
`CRATONVM_JIT_ALLOW_PACKAGES`, `CRATONVM_JIT_FREE_CODE`,
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
`CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC`,
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

## Part 4 — Consolidation (resolved)

This part used to propose three fixes. All three are implemented; the surface
is now ten grouped variables plus five scalars, defined once in
[`types/src/flag_groups.rs`](../types/src/flag_groups.rs) and listed in
[`docs/flag-tokens.md`](flag-tokens.md).

### Problem A: per-subsystem `CRATONVM_REAL_*` flags were redundant with `CRATONVM_REAL` — **fixed**

`CRATONVM_REAL` already supported `all`, `jca` and exact class names via
`RealSelector` in `vm/src/runtime/env_cache.rs`, but eight per-subsystem flags
bypassed it and gated at the registration call site in `lib.rs`. Each is now a
token in the same variable: `CRATONVM_REAL=forkjoinpool,net-sockets,aqs,raf`.
`RealSelector::parse` still reads `all` / `jca` / class names off the raw value,
so nothing about that path changed.

One of the eight, `CRATONVM_ECLIPSE_REAL`, turned out to have no read site at
all — along with 33 sibling `CRATONVM_<APP>_REAL` variables that
`scripts/real-run-all.sh` exported. That script now uses `CRATONVM_REAL=-stubs`.

### Problem B: inverted naming convention for crypto — **fixed**

`CRATONVM_SYNTHETIC_EC=1` meant "use the legacy fake" while `CRATONVM_REAL_JCA`
meant "use real JDK": two opposite conventions in one matrix.

The fix was not a rename. Each subsystem is now **one token stated positively**,
with the direction carried by a `-` prefix rather than by the name:
`CRATONVM_REAL=ec` for real, `CRATONVM_REAL=-ec` for the synthetic shim. The
four subsystems that had *both* a `REAL_` and a `SYNTHETIC_` variable — AQS,
ANNOTATIONS, AGROAL, VERTX — collapse into one token each, and
`types/src/flag_groups.rs` records both legacy names so existing runbooks keep
working. `every_token_is_unique` asserts the merge stays merged.

### Problem C: WildFly mode flags scattered across six files — **fixed**

The six variables had no shared parsing and no shared documentation. They are
now tokens in `CRATONVM_REAL` and `CRATONVM_COMPAT`, parsed once at startup into
`cratonvm_types::flags::VmFlags`.

Three of the six — `CRATONVM_USE_WILDFLY_MAIN_SHIM`,
`CRATONVM_SKIP_JBOSS_PLUMBING`, `CRATONVM_WILDFLY_SHORTCIRCUIT` — had no read
site. They were documented here with a stated effect and did nothing. Removed.

### What stops it growing back

`tools/flag-census/check-surface.sh` runs in CI and fails if a
`std::env::var("CRATONVM_…")` call site appears without a token, or if these
docs name a token that does not exist. The second half is the one that matters
here: a documented flag that reads nothing is how `CRATONVM_PRECISE_JIT_MAPS`
stayed in the docs for months while the code read the inverse
`CRATONVM_NO_PRECISE_JIT_MAPS`.

---

## Summary

| Category | Count |
|---|---|
| Interpreter intrinsic table entries | ~35 |
| Essential natives (real-JDK mode) | ~150 |
| Framework / app extras source files | ~100 |
| Compile-time feature flags | 11 |
| **`CRATONVM_*` environment variables you can set** | **15** |
| …ten grouped ones, carrying this many tokens between them | 550 |
| …plus five scalars (`JAVA_HOME`, `BIN`, `MAVEN_REPO_LOCAL`, `ENABLE_ASSERTIONS`, `DISABLE_JIT`) | 5 |
| Per-application `CRATONVM_<APP>_REAL` variables, all dead, all removed | 34 |

The 15 is the number to hold. It was 692 identifiers before the consolidation —
559 with a read site, 133 that nothing had ever read. `types/tests/flag_surface.rs`
and `tools/flag-census/check-surface.sh` pin it.
