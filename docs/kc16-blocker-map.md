# KC16 Boot-Blocker Map (Session 94, 2026-04-26)

## Live status (Session 97, 2026-05-02 — partial: VMManagementImpl int-typed signatures)

A 6-agent batch was dispatched to close the remaining boot blockers
(ManagementFactory ULE, BigDecimal arithmetic, JIT regalloc-on-recursion,
JBoss LogManager classpath visibility / `getLogManager` factory / synthetic
shim). All 6 agents hit token limits before delivering working fixes; **only
1 of 6 (Item 1, ManagementFactory natives) shipped a net-positive partial**.

**RKC16N.12 partial — VMManagementImpl int-typed thread counters & uptime/
processors (LANDED Session 97).** `getLiveThreadCount`/`getPeakThreadCount`/
`getDaemonThreadCount` were registered with `()J` but JDK 25 declares them
as `()I`; the dispatcher missed all three. Re-registered with `()I`. Also
added `getUptime0()J` (returns real elapsed-millis) and
`getAvailableProcessors()I` (routes to Rust's
`std::thread::available_parallelism`). Added a
`System.loadLibrary` / `Runtime.loadLibrary*` override-allowlist entry in
`vm/src/vm/vm_exec.rs:4852-4882` so the JDK's `Class.forName` path through
`ManagementFactory.<clinit>`'s loadLibrary("management") call doesn't throw.
Files: `native-builtins/src/jmx.rs` (+93), `vm/src/vm/vm_exec.rs` (+28).
New tests: `vm/tests/management_factory_clinit.rs`,
`jmx_tests::test_vm_management_impl_int_typed_thread_counters`,
`jmx_tests::test_vm_management_impl_uptime_and_processors`.

**`ManagementFactory.<clinit>` UnsatisfiedLinkError still fires** post-fix
— at least one further missing native is reachable from the JMM init chain
(probably in `sun/management/MemoryPoolImpl`, `MemoryManagerImpl`,
`GarbageCollectorImpl`, or `HotSpotDiagnostic`). Diagnose via
`RUSTJVM_STRICT_SWALLOWS=1` to capture the failing native by name; the
existing `register_vm_management_impl` is the right home.

**Items NOT delivered by the Session 97 batch (need rework):**

- **RBIGDEC.1** — BigDecimal/BigInteger arithmetic on post-clinit-populated
  statics returns 0. Agent investigated `vm/src/vm/vm_util.rs::post_clinit_fixup`
  but didn't compile (`alloc_array_checked` doesn't exist; trivially
  fixable but the resulting binary still printed `0\n0\nOK` on
  `apps/bigdecimal_probe/BdProbe`). Worktree exists at
  `agent-a5574fe75239a0e58` for inspection; not merged.
- **RFJP.1** — `pool.invoke(RecursiveTask)` divide-and-conquer at depth ≥10
  returns 0. Worktree discarded by harness; nothing to inspect. Workaround
  `RUSTJVM_DISABLE_JIT=1` confirmed working in Session 96.
- **Block 2A** — JBoss LM JAR auto-discovery + classpath extension when
  `java.util.logging.manager=org.jboss.logmanager.LogManager` is set.
  Agent landed `discover_jboss_logmanager_jar()` in `vm/src/config.rs` and
  classpath-extension wiring in `vm/src/vm/vm_init.rs`, but with the system
  property set the KC16 boot warning still fires — the wiring activates but
  the JAR isn't reaching `Class.forName` from inside the JDK's
  `java.util.logging.LogManager.<clinit>`. Worktree at
  `agent-a72e1604a23f27bfc`; not merged.
- **Block 2B** (`getLogManager()` honors the property for arbitrary classes)
  — worktree discarded by harness.
- **Block 2C** (synthetic `org.jboss.logmanager.LogManager` shim fallback)
  — worktree discarded by harness.

**Operational lesson**: 6 simultaneous opus agents on multi-file
investigation tasks exhausted account capacity faster than agents could
iterate on first-attempt failures. Future batches should be ≤4 opus
agents on tightly-scoped fixes (each with a known repro and a clear
single-file root cause), or use sonnet for the easier items. Pure
add-only / cleanup tasks (like Session 96's RJ.1) ship reliably; deep
multi-system investigations (regalloc, classloader-ordering, native-init
chains) need either more budget per agent or up-front scoping into
smaller blocks.

## Live status (Session 96, 2026-05-02 — Object.get + BigDecimal cascade closed)

After a 5-agent parallel batch this session, two of the three remaining KC16
boot blockers from Session 95 are closed:

- **`Object.get(Object)Object` `NoSuchMethodError`** — RESOLVED. Root cause was
  `native-builtins/src/lang_system.rs::native_system_getenv_all` allocating
  the returned HashMap with `ClassId::new(0)` instead of routing through
  `ctx.ensure_class_initialized("java/util/HashMap")`. The dispatcher's
  stale-pointer detector then routed `Map.get(key)` invokeinterface (called
  from `WildFlySecurityManager.getSystemEnvironmentPrivileged()` →
  `org/jboss/as/server/ServerEnvironment.configureQualifiedHostName`) to
  `java/lang/Object`, which has no `get`. Pinned by
  `vm/tests/wp8_10_10_system_getenv_map_class.rs`.
- **`BigDecimal.<clinit>` NPE on `signum`** — RESOLVED (boot path; arithmetic
  still red — see RBIGDEC.1 below). Root causes were two: (a) a
  `set_static_by_name` indexing bug in `vm/src/vm/vm_util.rs` that used
  enumerate-indexing instead of static-only indexing, so JDK classes with
  interleaved static/instance fields (BigDecimal has `JLA`/`INFLATED`/
  `INFLATED_BIGINT` between instance fields) silently wrote statics into
  instance slots; (b) a missing post-clinit fixup arm for BigInteger and
  BigDecimal — when their `<clinit>` swallows, ZERO/ONE/TWO/NEGATIVE_ONE/TEN
  are populated with hand-built instances. The display WARN for the known-
  recoverable BigDecimal cascade is suppressed (counter still increments;
  `RUSTJVM_STRICT_SWALLOWS=1` still escalates).

Reproducer (unchanged):
```
target/release/rustjvm.exe --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot" \
   --Xmx 2g --jar /tmp/keycloak/keycloak-16.1.1/jboss-modules.jar -- \
   -mp /tmp/keycloak/keycloak-16.1.1/modules org.jboss.as.standalone \
   "-Djboss.home.dir=/tmp/keycloak/keycloak-16.1.1"
```

Output (post-merge, end of Session 96):
```
[rustjvm] stack-dump watchdog armed: will dump + abort after 45s
WARNING: Failed to load the specified log manager class org.jboss.logmanager.LogManager
WARN B6: silent-swallow ... class=java/lang/management/ManagementFactory exc=java/lang/UnsatisfiedLinkError
WARN Post-clinit fixup: BigDecimal ZERO/ONE/TWO/TEN populated (4/4)
WARN: main() completed with 2 swallowed VM error(s)
```

`main()` returns rc=0. **The WildFly server itself still does not start** —
`server.log` is not written, no port is bound. Two concrete next blockers,
each its own RKC16N.* item:

1. **`ManagementFactory.<clinit>` `UnsatisfiedLinkError`** — see Session 95
   live-status above. Likely needs another batch of `sun/management/*` natives
   (RKC16N.10 batch was incomplete). Probable source of the `java.lang.invoke`
   bootstrap-cycle wording in JEP 358 NPE messages downstream.
2. **JBoss LogManager wiring** — `java.util.logging.manager=org.jboss.logmanager.LogManager`
   is rejected at LogManager bootstrap because the class isn't on the system
   classpath that early. Substantial wiring exercise — see Session 95.

`RBIGDEC.1` (new follow-up): the post-clinit fixup populates the static
fields, but `BigDecimal.ONE.add(BigDecimal.TEN)` returns `0` instead of `11`,
and `BigInteger.TWO.multiply(BigInteger.TEN)` returns `0` instead of `20`.
Reproducer: `apps/bigdecimal_probe/BdProbe.java` (rc=0 today, prints `0\n0\nOK`
instead of `11\n20\nOK`). The `mag`/`signum`/`intCompact`/`intVal` fields
are populated with the right *values* but the JDK arithmetic methods still
read them as zeroes — likely either the field offsets in the fixup don't
match the JDK 25 layout or the in-place arithmetic fast-path bypasses the
fields the fixup writes.

`RFJP.1` (new): `apps/fjp_probe/FjpProbe.java` (`pool.invoke(RecursiveTask)` of
divide-and-conquer Long sum) returns `0` instead of `499999500000`.
Diagnosed in this session as a JIT correctness bug in deeply-recursive
`compute()` Long-arithmetic at depth ≥10. Workaround: `RUSTJVM_DISABLE_JIT=1`.
Pinned (failing) by `vm/tests/fjp_recursive.rs::fjp_probe_recursive_returns_correct_sum`
(`#[ignore]`-gated). Files investigated but not landed:
`vm/src/jit/x64.rs::flush_scratch_registers`, `vm/src/jit/helpers.rs::jit_invoke_dispatch`.

`RJ.1` (debug-print cleanup) — RESOLVED in this session. 22 leaked
`eprintln!("[WP*]" / "[FJPTRACE]" / etc.)` removed across native-builtins;
CI gate at `scripts/check-no-diag-prints.sh` enforces 0 hits.

`RSLF4J.1` (ClassLoader.getResources for classpath JARs) — RESOLVED. The fix
(override-allowlist in `vm/src/vm/vm_exec.rs`, JAR walker in
`native-builtins/src/classloader.rs::cl_get_resources`) had landed silently
in a prior commit; this session pinned it with a regression test
(`vm/tests/rslf4j1_get_resources.rs`) so it can't silently regress.
Verified end-to-end on Windows: `getResources("META-INF/services/foo.svc")`
returns 1 entry from a classpath JAR, with URL
`jar:file:/.../svctest.jar!/META-INF/services/foo.svc`.

## Live status (Session 95, 2026-04-29 — main() reaches exit 0; RKC16N.9–.13 landed)

After commits `8aad4c8` (RKC16N.9 + RKC16N.10), `01f091e` + `9971e41`
(CLI Java stack-trace renderer + per-thread `throwable_stacks` fallback),
`3fffef3` + `643e155` (sun.management.* batch + synthetic `VM$BufferPool`),
`17d85c9` (RKC16N.12), and `ba97403` (RKC16N.13), KC16 main() now **exits
0** for the first time in the project. Reproducer is unchanged. Output:

```
[rustjvm] stack-dump watchdog armed: will dump + abort after 45s
WARNING: Failed to load the specified log manager class org.jboss.logmanager.LogManager
WARN B6: silent-swallow ... class=java/lang/management/ManagementFactory exc=java/lang/UnsatisfiedLinkError
WARN B6: silent-swallow ... class=java/math/BigDecimal exc=java/lang/NullPointerException: Cannot read field 'signum' because the object is null
WARN NoSuchMethodError method="java/lang/Object.get(Ljava/lang/Object;)Ljava/lang/Object;"
WARN: main() completed with 2 swallowed VM error(s) ...
```

`main()` returns cleanly with exit 0. **The WildFly server itself does
not yet actually start** — the swallowed errors and the unloaded JBoss
LogManager (system property `java.util.logging.manager=org.jboss.logmanager.LogManager`
fails to resolve) prevent the ServiceContainer from spinning up and
silently drop any startup-banner logs.

**RKC16N.12** (commit `17d85c9`) — two fixes that together advanced
boot from `ClassNotFoundException` escaping uncaught at `Module.run`
PC 40 to a clean `main()` return:

- `native-builtins/src/lang_class.rs::native_class_for_name` was
  ignoring its `args[2]` (`ClassLoader`) parameter, going straight to
  `ctx.ensure_class_initialized` against bootstrap classpath. JBoss's
  `Module.run` calls `Class.forName(mainClassName, false,
  this.moduleClassLoader)` — without honoring the loader arg,
  `org.jboss.as.server.Main` was looked up in bootstrap (no module
  JARs there) and CNFE escaped uncaught. Patched to dispatch via
  `ctx.invoke_virtual(loader, "loadClass", ...)` when the loader arg
  is non-null. Routes through `ModuleClassLoader.loadClass` (already
  in `jboss_module_loader.rs`) which walks the visibility closure,
  registers all 70 resource roots reachable from
  `org.jboss.as.standalone`, and resolves through the system loader.
- `native-builtins/src/jboss_module_loader.rs::build_module_object`
  never populated `mainClassName` on the synthetic
  `org.jboss.modules.Module` instance. The real `Module` class
  (loaded from jboss-modules.jar) has many more fields than our
  4-slot synthetic; `Module.run(String[])` reads `getfield
  mainClassName` at PC 1, which on our synthetic returned the
  default-zero/null at the un-set slot, so we were calling
  `Class.forName("", false, mcl)`. Confirmed via a temporary
  `RUSTJVM_DBG_MCL=1` trace (now permanent, gated). Patched to
  `ctx.set_field_by_name(module, "mainClassName", main_str)` so the
  real layout is honored regardless of slot offset.

**RKC16N.13** (commit `ba97403`) — `java/lang/StackStreamFactory.checkStackWalkModes()Z`
was registered only on the inner `$AbstractStackWalker`. JDK 25 exposes
the same helper as a static native on the outer class too, called from
`StackStreamFactory.<clinit>`. Adding the outer registration drops the
swallowed-error count from 4 to 2 (the dropped pair was the failing
clinit + the downstream `$StackFrameTraverser.<clinit>` NPE).

**Frontier as of end of Session 95**: server bytecode runs but the
ServiceContainer doesn't actually spin up. Three concrete next blockers:

1. **`BigDecimal.<clinit>` NPE** ("Cannot read field 'signum' because the
   object is null"). Likely `BigInteger.ZERO`/`ONE`/`TEN` statics not
   populated in time, then `BigDecimal.<clinit>` constructs
   `new BigDecimal(BigInteger.ZERO, ...)` and the constructor reads
   `.signum` on null. Cascade from `BigInteger.<clinit>`.
2. **`java/lang/Object.get(Object)Object` `NoSuchMethodError`**. Looks
   like a `Map.get(key)` invokevirtual that resolved against the static
   type `Object` instead of the receiver's concrete `Map` class — likely
   an interpreter / vtable dispatch bug.
3. **JBoss LogManager wiring**. `java.util.logging.LogManager` rejects
   the system property `java.util.logging.manager=org.jboss.logmanager.LogManager`
   because the class isn't on the system classpath at LogManager
   bootstrap time (LogManager runs very early, before module-loader
   resource roots are visible to the system loader). Without this
   wiring all WildFly startup logs are silently dropped — even if the
   ServiceContainer starts, the user sees no banner.

Each is its own RKC16N.* item. (1) and (2) look like single-fix
investigations (1-3 iterations apiece); (3) is a substantial wiring
exercise. None block the next iteration of (1)/(2).

## Live status (Session 95, 2026-04-29 — RKC16N.9 + RKC16N.10 landed)

After commit `8aad4c8` ("RKC16N.9 + RKC16N.10") landed both fixes, KC16
boot now reaches a new opaque blocker: an empty-message NPE that escapes
from `main()` downstream of `ManagementFactory.<clinit>`. This is
**RKC16N.11**.

Reproducer (unchanged from Session 94):
```
target/release/rustjvm.exe --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot" \
   --Xmx 2g --jar /tmp/keycloak/keycloak-16.1.1/jboss-modules.jar -- \
   -mp /tmp/keycloak/keycloak-16.1.1/modules org.jboss.as.standalone \
   "-Djboss.home.dir=/tmp/keycloak/keycloak-16.1.1"
```

Observed (post-`8aad4c8`):
```
Exception in thread "main" java/lang/NullPointerException
```

No message, no cause, no Java stack trace. Note that **`ManagementFactory.<clinit>`
still records a silent-swallow `UnsatisfiedLinkError`** from one of its
internal `Class.forName` try/catch blocks (likely tolerated by design —
HotSpot's reference impl catches and swallows missing-impl probes during
that clinit). The fatal NPE that escapes to the CLI is **unrelated** to
that swallow and originates somewhere downstream — diagnostic gap is the
absence of a Java stack at the CLI exception print path (parallel agent
working on that).

**What was needed to get here** (capsule of the two fixes that landed in
`8aad4c8`):

- **RKC16N.9** — `org.jboss.modules.Module.<clinit>` NPE. The original
  Session 94 recon notes pointed at JBoss-Modules-specific helpers
  (`PathFilters.acceptAll()`, `DefaultBootModuleLoaderHolder` mirror)
  and that turned out to be **wrong**. Actual root cause was a chain
  of three independent gaps that combined to leave `Void.TYPE` null,
  which in turn surfaced as a `Module.<clinit>` NPE:
  - jimage header version-decoding bug in `reader/src/jimage.rs` —
    the version field was read as two `u16`s instead of a single `u32`
    split into HIGH=major / LOW=minor (inverted on little-endian disk).
  - missing `lib/modules` jimage fallback in
    `vm/src/config.rs::discover_boot_classpath` — only checked `rt.jar`
    and `jmods/`, missed JRE-style and jlink-trimmed runtimes including
    the Adoptium "JDK 25" header dist used in this reproducer.
  - new `obj_arg` backtrace diagnostic in `native-builtins/src/lib.rs`,
    gated on `RUSTJVM_DBG_NULL_NATIVE`, for future "which native got
    null" investigations.
- **RKC16N.10** — `ManagementFactory.<clinit>` UnsatisfiedLinkError
  cluster (5 + 4 missing JMX natives + a null bridge return). Added
  `sun/management/VMManagementImpl` (5 specific natives + 16 boolean
  `is*Supported`/`is*Enabled` returning `false` + 17 long counters
  returning `0`), `sun/management/MemoryImpl` (4 natives), wired into
  both real-JDK registration paths in `vm/src/vm/vm_init.rs`, and the
  legacy `Buffer$1.getDirectBufferPool()Ljdk/internal/misc/VM$BufferPool;`
  returning null in `native-builtins/src/shared_secrets_bridge.rs`.

Frontier as of 2026-04-29: **RKC16N.11** — diagnose the opaque
main()-thread NPE downstream of `ManagementFactory.<clinit>`. Serialised
after the parallel CLI-stack-trace work lands (otherwise pure recon is
blind).

## Live status (2026-04-26, Session 94 — fourth iteration; RVERIF.2 landed)

After RVERIF.2 (verifier subtype widening fix via JDK interface name table):

```
target/release/rustjvm.exe --java-home "C:/.../jdk-25.0.2.10-hotspot" \
   --Xmx 2g --jar /tmp/keycloak/keycloak-16.1.1/jboss-modules.jar -- \
   -mp /tmp/keycloak/keycloak-16.1.1/modules org.jboss.as.standalone \
   "-Djboss.home.dir=/tmp/keycloak/keycloak-16.1.1"
```

→ verifier passes. Single failure (no NoSuchMethodError, no other warnings):
```
B6: silent-swallow class=org/jboss/modules/Module
   exc=java/lang/NullPointerException: null object argument
Exception in thread "main" java/lang/NullPointerException
```

**RKC16N.9** — `org.jboss.modules.Module.<clinit>` NPE. The class init
is silently swallowed (B6 path), leaving `Module` in a partially-
initialised state, then `main()` references something that NPEs.

**Recon completed (2026-04-26 PM, Session 94 fifth iteration):**
- `RUSTJVM_STRICT_SWALLOWS=1` panics inside `common_superclass` →
  `ensure_class_initialized_shared`, confirming the NPE is during a
  cascading class-init triggered by `Module.<clinit>`.
- `Module.<clinit>` clinit chain (via `RUST_LOG=...vm_util=debug`):
  `Main` → `StartTimeHolder` → `StandardCharsets` →
  `DefaultBootModuleLoaderHolder` → `ModuleLoader` →
  `LocalModuleLoader` → `Module` (the NPE class).
- The NPE message "null object argument" comes from the generic
  `obj_arg` helper at `native-builtins/src/lib.rs:6036`, which means
  some native method is being called with a `Value::Object(None)`
  argument. Without a Java stack at the call site we can't tell which
  native — adding a per-native eprintln in `obj_arg` or recording the
  failing method name in `record_swallow` would expose this in <1
  iteration.
- `Module.<clinit>` bytecode (via `javap -c -p` on jboss-modules.jar):
  PC 0..125 sets up `MAIN_METHOD_TYPE` + `log` + `BOOT_MODULE_LOADER`
  + 6 RuntimePermission statics + 2 FastCopyHashSets. PC 128..156
  reads `jboss.modules.system.pkgs` via PropertyReadAction +
  AccessController.doPrivileged, allocates `ArrayList`, then calls
  `JDKSpecific.addInternalPackages(list)` (PC 154). Stubbed as no-op
  in Session 94 commit `<TBD>` — did not change the NPE, so the
  triggering native is elsewhere.
- Plausible remaining culprits: `MethodType.methodType(Class, Class)`
  (PC 6) returning null then later invocation NPEs; `AccessController.doPrivileged`
  arity mismatch; `FastCopyHashSet.<init>(I)V` taking null where it
  expects `this`. Add a Java-stack log in `obj_arg` to find out.



Keycloak 16.1.1 (WildFly / JBoss-Modules) under current `target/release/rustjvm.exe`
on Windows 11, JDK 25.0.2 (Adoptium) for boot classpath, default flags.

## Live status (2026-04-26, Session 94 — third iteration; RKC16N.1/3/5/8 + recon hacks landed)

After RKC16N.8 landed (`Class.desiredAssertionStatus()Z` + `System.initPhase1()V` stubs):

```
target/release/rustjvm.exe --java-home "C:/.../jdk-25.0.2.10-hotspot" \
   --Xmx 2g --jar /tmp/keycloak/keycloak-16.1.1/jboss-modules.jar -- \
   -mp /tmp/keycloak/keycloak-16.1.1/modules org.jboss.as.standalone \
   "-Djboss.home.dir=/tmp/keycloak/keycloak-16.1.1"
```

→ no NoSuchMethodError warnings. Single failure:
```
linkage error: verification error in org/jboss/modules/Module.getResources:
 at bytecode offset 294: expected ObjectRef("java/util/Collection") on stack,
 found ObjectRef("java/util/List")
```

That is **RVERIF.2** (subtype widening). Agent dispatched. With `--noverify`,
KC16 progresses to `org.jboss.modules.Module.<clinit>` NPE (RKC16N.9, follow-up).

`-version` (full clean run, no diagnostic warnings):
```
[rustjvm] stack-dump watchdog armed: will dump + abort after 45s
JBoss Modules version 2.0.0.Final
```

## Live status (2026-04-26, Session 94 — second iteration with recon hacks landed)

After Session 94 first-iteration work (Properties.load(Reader) stub +
String layout-neutral natives + override-allowlist + throwable ctor
stubs):

```
target/release/rustjvm.exe --java-home "C:/.../jdk-25.0.2.10-hotspot" \
   --Xmx 2g --jar /tmp/keycloak/keycloak-16.1.1/jboss-modules.jar -- \
   -mp /tmp/keycloak/keycloak-16.1.1/modules org.jboss.as.standalone \
   "-Djboss.home.dir=/tmp/keycloak/keycloak-16.1.1"
```

→ aborts at the bytecode verifier:
```
linkage error: verification error in org/jboss/modules/Module.getResources:
 at bytecode offset 294: expected ObjectRef("java/util/Collection") on stack,
 found ObjectRef("java/util/List")
```

This is the **RVERIF.1**-class bug (subtype widening): `List` extends
`Collection`, so the verifier should accept it.

With `--noverify`, KC16 progresses further to:
```
B6: silent-swallow — class=org/jboss/modules/Module exc=java/lang/NullPointerException
Exception in thread "main" java/lang/NullPointerException
```

`-version` (with the recon hacks) reaches the actual `main()` body and
prints `JBoss Modules version (unknown)` — the entirety of `Main.<clinit>`
runs cleanly.

**Frontier as of 2026-04-26 18:05 UTC**: WildFly module-loader inside
`org.jboss.modules.Module.<clinit>`. This is several phases past where
the previous KC16 baseline was stuck.

## Live status (2026-04-26)

Reproducer (worked from worktree root):
```
target/release/rustjvm.exe --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot" \
   --Xmx 2g --jar /tmp/keycloak/keycloak-16.1.1/jboss-modules.jar -- \
   -mp /tmp/keycloak/keycloak-16.1.1/modules org.jboss.as.standalone \
   "-Djboss.home.dir=/tmp/keycloak/keycloak-16.1.1"
```

Observed (~30 ms wall-clock to abort):
```
[rustjvm] stack-dump watchdog armed: will dump + abort after 45s
WARN  NoSuchMethodError method="java/lang/System.initPhase1()V"
WARN  NoSuchMethodError method="java/util/Properties.load(Ljava/io/Reader;)V"
Error in thread "main" linkage error: no such method:
   java/util/Properties.load(Ljava/io/Reader;)V
```

This is **earlier** than every blocker the previous (Session 93) revision of
this doc describes — the CHM `initTable` livelock at P0 module-loader
bootstrap is now unreachable because `org.jboss.modules.Main.<clinit>` aborts
during its `version.properties` load.

## Resolved since Session 93

| # | Status | Evidence |
|---|--------|----------|
| #3 KC16 `initPhase1` synthetic-stream fallback | RESOLVED | `WARN NoSuchMethodError java/lang/System.initPhase1()V` is now a non-fatal warning; main thread proceeds (it only re-fails downstream on Properties.load(Reader)). |
| #4 9 static MISSING natives | RESOLVED (WP0.3) | Coverage 191/191. |
| #5 Silent livelock / no watchdog | RESOLVED | First line of every run is `stack-dump watchdog armed: will dump + abort after 45s`. SIGTERM dump still TBD. |

## Open blockers (refreshed)

| # | Phase | Symptom | Root cause | Fix scope | Cmplx |
|---|-------|---------|------------|-----------|-------|
| 1 | Mod-loader pre-P0 | **`Properties.load(Ljava/io/Reader;)V` NoSuchMethodError** in `Main.<clinit>` reading `version.properties`. | `properties_sidetable.rs::register_properties_sidetable` registers `load(InputStream)` but not `load(Reader)`. | New native that drains the Reader via `invoke_virtual(read([CII)I)` and reuses `parse_properties`. See **RKC16N.1** in `roadmap-any-java-app.md`. | Easy |
| 2 | Mod-loader P0 (gated on #1) | CHM `initTable()` CAS livelock on `sizeCtl` (PC 0..41 spin ~5,740 /s). Identical bytecode to KC26 #1. | `Unsafe.compareAndSetInt` reads zero-alloc primitive slot as `Value::Object(None)` not `Int(0)`; CAS equality never holds. | Typed default at `gc::heap::alloc_object`, or typed `read_slot`. See **RKC16N.2**. | Hard |
| 3 | Mod-loader P0 (gated on #2) | Never opens any `module.xml`. | Downstream of #2. | — | — |
| 4 | Boot diff | Synthesize `[L…;` array classes on demand instead of JMOD scan. | KC26 Blocker #3 (still open across both KC16 and KC26). | `classloading/src/class_manager.rs`. See **RKC16N.3**. | Easy |
| 5 | Observability | SIGTERM/abort path still loses missing-natives audit dump. | Audit flush only on clean exit. | Flush on watchdog/SIGTERM. | Medium |

Workload (legacy reference): `jboss-modules.jar -jaxpmodule
javax.xml.jaxp-provider org.jboss.as.standalone -b 0.0.0.0`. JDK 21.0.6.
Reference: HotSpot `standalone\log\server.log` hit `WFLYSRV0025` in ~47 s.

---

## Historical notes (Session 93 / WP0.3, 2026-04-24)

**WP0.3 now closed** (N1..N4 + WP0.3 verification):
  * All 9 known MISSING natives registered. `s10_native_coverage_100_percent`
    runs: `Total ACC_NATIVE=191, Covered=191, Missing=0, Coverage=100.0%`.
  * Anchored grep `MISSING: .+-> Native\.` (roadmap pattern, legacy format)
    returns 0 — the real emitter at `vm/src/vm/vm_object.rs:719` emits
    `MISSING: <class>.<method><desc>` and the inventory scan returns empty.
  * Blocker #4 below is therefore RESOLVED.

## 1. WildFly phase timeline

`RUST_LOG=rustjvm_vm=trace` → `/tmp/kc16_d2_stderr.txt` (4.0 GB, 20.1 M lines, killed at 206 s).

| Phase | Event | Time |
|-------|-------|------|
| VM startup, 70 jmods, 267 classes | `NativeBridge Coverage 192/201`, 9 MISSING | 03:10:17.267 |
| JDK core clinits | `<clinit> java/util/Collections` | 03:10:17.380 |
| **`org.jboss.modules.Main.<clinit>`** | + `StartTimeHolder`, `Properties` | 03:10:17.383 |
| `ConcurrentHashMap.<clinit>` + `version.properties` | bytes=732 | 03:10:17.386 |
| StringUTF16 compress, Preconditions clinit | — | 03:10:17.444 |
| **LIVELOCK — `Ifeq(-41)` in `CHM.initTable()` CAS** | — | 03:10:17.449 |
| 206 s, 1,184,080 iterations (~5,740 /s) | — | kill |

**Phase reached: P0 Module-loader bootstrap.** Never parses `module.xml`,
never resolves `org.jboss.as.standalone`, never reaches MSC, subsystems,
Keycloak deployment, or HTTP listener.

## 2. Top 5 blockers

| # | Phase | Symptom | Root cause | Fix | Cmplx |
|---|-------|---------|------------|-----|-------|
| 1 | Mod-loader P0 | **CHM `initTable()` CAS livelock** on `sizeCtl`: PC 0..41 spin 5,740 /s. Identical bytecode to KC26 #1. | `Unsafe.compareAndSetInt` reads zero-alloc primitive slot as `Value::Object(None)` not `Int(0)`; CAS equality never holds. | Typed default at `alloc_object` or typed `read_slot`. | **Hard** |
| 2 | Mod-loader P0 | Never opens any `module.xml`. | Downstream of #1. | Gated on #1. | Hard |
| 3 | Bootstrap diff | **No `initPhase1` synthetic-stream fallback** (KC26 emits it). | Different `-jar` launch paths. | Align with KC26 after #1. | Medium |
| 4 | Natives | ~~9 static MISSING~~ **RESOLVED (WP0.3, Session 93)** — all 9 (`Class.getProtectionDomain0/getSigners/setSigners`, `Thread.sleep0`, 4× `AccessController.*`, `AtomicLong.VMSupportsCS8`) registered with real implementations (not stubs). Coverage now 191/191 = 100 %. | — | Done. | Done |
| 5 | Observability | Silent livelock: no WARN, no stuck-thread detector, `kill -9` loses audit. | No heartbeat, no SIGTERM flush. | `--XX:StuckThreadMs=N` + audit flush. | Medium |

## 3. New missing natives beyond the 9 known

**None.** 0 dynamic `UnsatisfiedLinkError`, 0 swallows in 4.0 GB trace —
only 10 startup-banner lines. Livelock precedes any further native dispatch.
Post-WP0.3: the original 9 banner lines are gone — the static scan now
reports 0 MISSING. Next time KC16 is re-run for a fresh census, any new
misses will be **dynamic** (reached only after the CHM livelock clears via
T19-wave-1).

## 4. Final state at 120 s (ran 206 s)

Process alive, 1 thread, ~5,740 ops/s. Interpreter looping PC 0..41 of
`CHM.initTable`. 26 classes fully clinit'd (vs KC26's 389 — KC16 trips
earlier, during `jboss-modules/Main`'s first CHM use, before JDK finishes
`j.u.concurrent`). 0 swallows, 0 exceptions, `server.log` not created,
stdout empty.

## 5. Shared-with-KC26 vs KC16-specific

| Blocker | KC26 | KC16 |
|---------|------|------|
| **CHM `initTable` CAS livelock (#1)** | YES | **YES — identical fix** |
| 9 static-scan MISSING natives | ~~YES~~ **RESOLVED** | ~~YES~~ **RESOLVED** (WP0.3) |
| `initPhase1` synthetic-stream fallback | YES | **NO — KC16-differential** |
| `[L…;` array synthesis (KC26 #3) | YES | unreachable |
| Silent livelock / no watchdog (#5) | YES | YES |
| WildFly MSC/Undertow/XNIO (T19.1/2/7) | N/A | deferred, gated on #1 |
| Quarkus ArC/static-init (T19.3/4) | YES | N/A |

**Both share the same primary blocker in the same JDK class.** Fixing #1
unblocks both; all version-specific work is behind that choke point.

## 6. Recommended next-session work

1. **Fix #1** per `docs/kc26-blocker-map.md` §5.2. Unblocks KC16 + KC26 together.
2. **#5** — `--XX:StuckThreadMs=N` + SIGTERM flush. Otherwise every future hang needs a 4 GB trace.
3. Re-run KC16 after #1 → expect P1 (`module.xml`) and fresh natives (JBoss-VFS, Log-Manager, Elytron).
4. **Defer T19.1 / T19.2 / T19.7** — unreachable today.
5. Re-run census; expect KC16 natives (JGroups, JBoss-Threads, Elytron) beyond the shared 9.

## Artefacts

* `/tmp/kc16_d2_stderr.txt` — 4.0 GB trace
* `/tmp/kc16_d2_stdout.txt` — 0 bytes
* `C:\craton\keycloak-16.1.1\standalone\log\server.log` — HotSpot reference
* `docs\kc26-blocker-map.md` — D1 peer
* `docs\kc-missing-natives-census.md` — static baseline

#1 gates T19.1-4; #5 is a new T19 observability sub-task. WP0.3 closed:
9 MISSING natives all registered — no residual T10 work on this list.
