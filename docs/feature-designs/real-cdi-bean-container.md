# Real-Bytecode CDI / Bean Container (retire the per-framework shim cluster)

Status: in progress (increment 1 landed). XL, and policy-sensitive (intersects
the "no synthetic stubs" project rule). The goal is to delete a cluster of
per-framework native short-circuits by making the real container bytecode run.

> **Increment 1 (interface static-final init; observability shim retired) — LANDED.**
> Scope: the lowest-risk first scalp of Step 1's general fix.
> - Verified that the GENERAL VM path for a non-constant `static final` field on
>   an *interface* is correct: the `getstatic` opcode resolves the field to its
>   declaring interface and calls `ensure_class_initialized_shared`, and
>   `initialize_class_shared` runs the interface `<clinit>` regardless of
>   interface status (JVMS §5.5 / §5.4.3.2). No interpreter change was needed —
>   the gap the Spring shim *named* (interface static-final not initialised) is
>   not the operative failure; the operative one is the NESTED concrete-class
>   `DefaultApplicationStartup.<clinit>` NPE (swallow + `post_clinit_fixup`
>   backfill), which remains covered by the Spring shim and is the next scalp.
> - Added a framework-independent regression test pinning the general behavior:
>   `vm/tests/iface_static_final_init.rs` (fixture
>   `vm/tests/resources/cratonvm/IfaceStaticFinalInit.java`). It reads a
>   non-constant `static final int VALUE = compute()` on an interface via a
>   single `getstatic` and asserts the value is the computed `42`, not the
>   prepared default `0`.
> - Retired the pure-observability piece of the Spring shim: the debug-gated
>   `warn_shim_first_use()` first-use warning in
>   `native-builtins/src/spring_startup_bootstrap.rs` (it announced the
>   now-verified general gap). The functional `getApplicationStartup`/`start`/
>   `tag`/`end` natives and the `post_clinit_fixup` carrier write are RETAINED —
>   removing them would re-expose the nested-`<clinit>` NPE and abort Spring Boot.

> **Increment 2 (Step 2, gated `CRATONVM_REAL_SPRING_STARTUP`) — LANDED.**
> Scope: make the real `DefaultApplicationStartup.<clinit>` chain run behind a
> default-OFF env gate, so the swallow + `post_clinit_fixup` backfill + no-op
> startup-metrics natives are no longer needed when the gate is ON. The default
> (gate unset) path is unchanged byte-for-byte.
>
> - **Concrete root cause.** Disassembling Spring Framework 5.3.31
>   (`spring-core`, the Spring Boot 2.7.x line the shim targets) shows the entire
>   `<clinit>` chain is *trivial* bytecode — no I/O, no reflection, no resource
>   loading:
>     - `ApplicationStartup.<clinit>`: `new DefaultApplicationStartup; dup;
>       invokespecial <init>; putstatic DEFAULT`.
>     - `DefaultApplicationStartup.<clinit>`: `new DefaultStartupStep; dup;
>       invokespecial <init>; putstatic DEFAULT_STARTUP_STEP`.
>     - `DefaultStartupStep.<init>`: `Object.<init>` then `new DefaultTags; dup;
>       invokespecial <init>; putfield TAGS`.
>     - `DefaultTags.<init>`: just `Object.<init>` (all nested classes are
>       `static`, so there is no enclosing-instance `this$0` to bind).
>   There is no genuine deep VM bug in *executing* this chain. The operative
>   failure was that the chain was **never given the chance to run on the real
>   path**: the no-op `getApplicationStartup()` native (registered in
>   `spring_startup_bootstrap.rs` and force-routed by the `check_override` chain
>   in `vm_exec.rs`) shadowed the real `getstatic ApplicationStartup.DEFAULT`
>   getter, so the interface `<clinit>` → nested `DefaultApplicationStartup
>   .<clinit>` chain was bypassed; and on the Spring Boot fat-jar path, where the
>   nested inner classes are loaded from a nested JAR, the `<clinit>` that *did*
>   run was swallowed by the lenient `<clinit>` path and `post_clinit_fixup`
>   stamped a synthetic `DEFAULT`. So the "nested-`<clinit>` NPE" was really a
>   shadow-and-swallow interaction, not an un-runnable bytecode chain. With the
>   gate ON the real chain completes on its own.
> - **The gate (`CRATONVM_REAL_SPRING_STARTUP`, default OFF).** Three coordinated
>   suppressions, all no-ops when the var is unset:
>     1. `native-builtins/src/spring_startup_bootstrap.rs::register` — the no-op
>        `getApplicationStartup`/`start`/`tag`/`end`/`getName`/`getId`/
>        `getParentId`/`getTags` natives are wrapped in `if !real_spring_startup()`
>        (mirroring the `CRATONVM_REAL_AQS` registration gate). The
>        environment/bean-factory/property-source natives in the same module cover
>        *separate* gaps and remain registered.
>     2. `vm/src/vm/vm_exec.rs` `check_override` chain — the `getApplicationStartup`
>        arm and the `start|tag|end|getName|getTags` arm are guarded with
>        `!real_spring_startup()` so the no-op natives are not force-shadowed over
>        the real bytecode.
>     3. `vm/src/vm/vm_util.rs` — `clinit_swallow_has_recovery` drops
>        `ApplicationStartup`/`DefaultApplicationStartup` from the lenient swallow
>        allowlist when the gate is ON (so a failed `<clinit>` propagates per JVMS
>        §5.5 instead of being swallowed), and the `post_clinit_fixup`
>        `ApplicationStartup` arm early-returns (so `DEFAULT` is never synthetically
>        backfilled). The shared gate accessor is
>        `crate::runtime::env_cache::real_spring_startup()`.
> - **Test.** `vm/tests/nested_clinit_startup.rs` with a framework-independent
>   fixture `vm/tests/resources/cratonvm/NestedClinitStartup.java` (real-JDK
>   bytecode, JDK 25 / class major 69) whose `Startup`/`DefaultStartup`/
>   `DefaultStep`/`DefaultTags` classes mirror Spring's `ApplicationStartup`/
>   `DefaultApplicationStartup`/`DefaultStartupStep`/`DefaultTags` byte-for-byte in
>   `<clinit>` shape. The flag-ON test (in-process) drives a single `getstatic
>   Startup.DEFAULT` and asserts the whole nested chain completes (probe `true`)
>   without the shim; the flag-OFF test (subprocess, env-isolated, skips if the
>   binary is unbuilt) asserts the existing shim + swallow + fixup fallback still
>   completes the probe.
> - **Not yet done (deferred):** flipping the gate default to ON and deleting the
>   startup-metrics natives + the `post_clinit_fixup` ApplicationStartup arm (Step
>   3) — gated on validating the Spring Boot battery with the flag ON.

## Goal

Run the **real** CDI / dependency-injection / service-container bytecode of
Quarkus ArC, Spring, WildFly Core, JBoss MSC, Infinispan, and Agroal — and
retire the hand-written native shims that currently short-circuit each
framework's bootstrap. Each shim is a per-app landmine (slot-keyed synthetic
objects, ignored configuration, no real lifecycle) that diverges from upstream
behavior the moment an app exercises a path the shim didn't anticipate.

## Current state (cited)

There is a documented cluster of intentional, per-framework native shims under
`native-builtins/src/`. Each is a real implementation of a *narrow boot path*,
not the framework:

- **`quarkus_arc.rs`** — Quarkus 3.x ArC (build-time CDI). Implements just
  `Arc.initialize()` / `Arc.container()` / `ArcContainer.instance(Class)` /
  `beanManager()` so Keycloak 26 boot returns. Module doc: "provides enough of
  ArC for that path to return. It does NOT [implement the container]".
- **`spring_startup_bootstrap.rs`** — explicitly labelled
  "⚠ INTENTIONAL APP-COMPATIBILITY SHIM — NOT a faithful implementation ⚠".
  Installs a no-op `ApplicationStartup`/`StartupStep`; every `start()/tag()/end()`
  is discarded. The doc states the underlying VM bug it works around: a
  `static final` field on an *interface* (`ApplicationStartup.DEFAULT`) that the
  bootstrap can't initialize.
- **`wildfly_core.rs`** — WildFly Core kernel (Deployment / Threads / Logging)
  natives; mirrors `DeploymentUnit` attachment maps by identity, wraps thread
  pools.
- **`jboss_msc.rs`** — JBoss MSC `ServiceContainer` / `ServiceController` state
  machine (`New→Down→Starting→Up`), `ServiceName` indexing, dependency DAG —
  reimplemented in Rust.
- **`infinispan_local.rs`** — Infinispan local-mode caches backed by a
  process-wide `RwLock<HashMap>`; `getCache/put/get/remove/addListener`.
- **`agroal_pool.rs`** — Agroal JDBC pool short-circuited to a Rust
  `AgroalPoolRegistry`; each `AgroalDataSource` stores an `i32` handle in a
  synthetic `pool` field.
- Adjacent shims in the same cluster: `ironjacamar_pool.rs`, `vertx_eventloop.rs`,
  `logmanager.rs`.

Why this matters (policy): `MEMORY.md` "feedback: no synthetic stubs" and the
"synthetic stub removal" project entry record the standing rule — fake-main
shims are forbidden; fix the underlying VM bug so real bytecode runs. This
cluster is the largest remaining concentration of exactly that pattern. Several
shim docs (e.g. `spring_startup_bootstrap.rs`) even name the specific VM bug they
paper over.

## Design

The shims exist because the *underlying VM gaps* make the real bytecode fail.
Retiring the cluster is therefore not "delete the shims" — it's "fix the gaps
the shims hide, framework by framework, then delete the shim." Order by gap, not
by framework.

### Step 0 — enumerate the real VM gaps each shim hides

Each shim's module doc names (or implies) the bug. Build a gap inventory:

- **`static final` field on an interface not initialized**
  (`spring_startup_bootstrap.rs` doc: `ApplicationStartup.DEFAULT`). This is a
  *general* `<clinit>`/interface-static bug; fixing it removes the Spring
  startup shim entirely and likely helps many frameworks.
- **Build-time-generated bean classes** (Quarkus ArC generates bean/registration
  classes at build time). Running the real ArC means loading and executing those
  generated classes — a classloading + generated-bytecode-execution gap, not a
  container gap.
- **Service-container concurrency** (MSC's worker-pool + async `complete()`):
  needs the real `ExecutorService`/thread + AQS machinery (cf. `CRATONVM_REAL_AQS`
  in `MEMORY.md` Gradle entry) rather than a Rust state machine.
- **Datasource/pool** (Agroal/IronJacamar): needs real `java.sql` + the JDBC
  driver bytecode to run, backed by the VM's real socket/file layers
  (`CRATONVM_REAL_NET_SOCKETS`).
- **Cache** (Infinispan local): the real local-mode cache is plain
  `ConcurrentHashMap`-backed Java — running it needs no native cache at all once
  the surrounding container boots.

### Step 1 — fix the general VM bugs first

The interface-`static-final`-`<clinit>` bug and the generated-class
loading/execution path are *shared* enablers. Fix these against a minimal repro
(not the full framework) so the fix is principled and regression-tested in
isolation, then confirm the dependent shim can be removed.

### Step 2 — retire shims framework-by-framework behind a gate

For each framework, add an opt-in gate (mirroring `CRATONVM_REAL_AQS`,
`CRATONVM_REAL_ANNOTATIONS`, `CRATONVM_REAL_JCA`) that **routes to real
bytecode** instead of the shim:

- `CRATONVM_REAL_SPRING_STARTUP` → drop the no-op `ApplicationStartup`, run the
  real `org.springframework.core.metrics` once the interface-static bug is fixed.
- `CRATONVM_REAL_ARC` → load Quarkus's generated bean classes, run real
  `Arc.initialize()`.
- `CRATONVM_REAL_MSC` → run real `ServiceContainer` bytecode on real executors.
- `CRATONVM_REAL_AGROAL` → real Agroal + JDBC driver.
- `CRATONVM_REAL_INFINISPAN` → real local-mode cache (likely free once the
  container boots).

Validate each gate against the corresponding suite (Keycloak 16/26 boot, Spring
Boot battery, WildFly boot diagnostic — see `docs/internal/wildfly-boot-
diagnostic.md`, `docs/internal/kc16-blocker-map.md`,
`docs/internal/kc26-blocker-map.md`) before flipping its default.

### Step 3 — delete the shim modules

Once a framework's real path is default-on and its suite is green, delete the
shim module and its registrations. The `--dump-native-registry` census
(`MEMORY.md` "synthetic stub removal") tracks the shrinkage.

## Implementation steps (ordered)

1. **Gap inventory** (Step 0): per-shim, the exact VM bug it hides, with a
   pointer to a minimal repro.
2. **Fix interface `static final` `<clinit>`** — highest leverage; unblocks the
   Spring startup shim and probably others. Minimal repro + isolated fix.
3. **Fix generated-class load/execute** for build-time CDI (ArC) — minimal repro.
4. **Per-framework real-path gate** (Step 2), starting with Spring startup (most
   clearly a pure-observability shim, lowest risk), then MSC, ArC, Agroal,
   Infinispan, Vert.x, IronJacamar.
5. **Validate** each gate against its boot suite; flip default when green.
6. **Delete the shim module** (Step 3) once default-on + green; update the
   native-registry census.

## Risks

- **This is the riskiest doc to execute**: each shim hides one or more *real*
  VM bugs, so "remove the shim" can re-expose a boot failure. The gate +
  per-framework rollout is mandatory; never delete a shim before its real path is
  green.
- **Concurrency/lifecycle correctness**: MSC's async service DAG and Agroal's
  pool are genuinely concurrent; the real bytecode needs the real AQS/executor
  paths to be solid (they are partially gated today).
- **Build-time-generated bytecode** (ArC, Spring AOT) may exercise classloading
  and reflection corners that no current app does — expect new gaps to surface.
- **Performance**: real containers do more work than the shims; the shims were
  partly chosen for fast boot. Measure boot time after each flip.
- **Scope creep**: this touches six+ frameworks. Treat each as an independent,
  separately-landable sub-project sharing the Step-1 general fixes.

## Effort

XL, and the largest of the nine. Best framed as a program: the two general
fixes (Step 1) are M–L each and unblock multiple frameworks; each per-framework
retirement is M and independently landable behind its gate. The Spring startup
shim is the recommended first scalp (pure observability, the bug is named).
