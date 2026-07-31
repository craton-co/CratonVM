# Spring AOT cluster — one residual chunk

| | |
|---|---|
| **Status** | OPEN — **1 of 20 chunks**. It does not finish, and it is not an AOT defect: see [Chunk 4](#chunk-4-the-only-one-left) below. The other 19 match HotSpot exactly, and chunk 6 beats it. |
| **Scope** | `org.springframework.test.context.aot.AotIntegrationTests#endToEndTestsForBeanOverrides` — 150 test classes, grouped by top-level class into 20 chunks. |
| **Measured** | 2026-07-31, Azure host `20.83.144.174`, real JDK 25, binaries `/data/data/wt-aot-20260730/localbin/cratonvm-spyfix-*.bin`. |
| **Predecessor** | This replaces `CRATONVM-SPRING-GENUINE-BUGLIST.md`, which was retired on 2026-07-30 once its list was down to this. Its history is under `docs/internal/fixed-suite-bugs/spring/`. |

## Current state

| chunk | CratonVM | HotSpot |
|--:|---|---|
| 0 | 38/38 | 38/38 |
| 1 | 37 found, 36 succ | same |
| 2 | 6 found, 1 succ | same |
| 3 | 42/42 | 42/42 |
| **4** | **does not finish** | 25 found, 24 succ |
| 5 | 20/20 | 20/20 |
| 6 | **33/33** | 33 found, 31 succ |
| 7–19 | identical to HotSpot in every case | |

`succ < found` rows are HotSpot's own probe artefacts — the TestNG engine's
`ServiceConfigurationError` under the forked TCCL, and *"non-public interface is
not defined by the given loader"* from a proxy. They are equal on both VMs,
which is the point of comparing per chunk rather than counting failures.

## Chunk 4: the only one left

`mockito.constructor.MockitoBeanByTypeLookupForConstructorParameters…` and its
neighbours. AOT processing completes (`PROBE aot-processing OK`) and the replay
never returns; killed at the 2400 s ceiling on every run.

It is **not** a hang and **not** an AOT bug. `CRATONVM_DEFAULT_WATCHDOG_SEC=420`
puts the main thread in `TestCompiler.compile` → in-process javac →
`JavaTokenizer` → … → `MockMethodAdvice` → `WeakConcurrentMap$LatentKey.hashCode`,
**spinning, not blocked**. That is
[`../internal/mockito-redefine-makes-every-call-40us-20260726.md`](../internal/mockito-redefine-makes-every-call-40us-20260726.md),
**fixed on 2026-07-31** — a redefined class was permanently barred from being
JIT-compiled and had its inline caches erased on every dispatch, which cost 46x
per call. Reproduce with `docs/known-issues/repros/redefine-call-cost/` (no
Mockito, Spring, JUnit or AOT). If this cluster still stalls, it is a different
cause; do not re-diagnose the redefinition cost here.

## Reproducing

```bash
cd /data/data/aot20260726
# one chunk (~2-10 min)
CRATONVM_BIN=<binary> TMO=1500 XMX=2g ./oneaot.sh tag $(tr '\n' ' ' < bochunks/chunk.009)
# one class, with stacks
PROBE_STACK=1 CRATONVM_BIN=<binary> ./oneaot.sh tag <fqcn> [<fqcn>$NestedTests]
# HotSpot baseline: same command with CRATONVM_BIN unset
```

`AotE2EProbe2` is `AotIntegrationTests.runEndToEndTests` cut down to the named
classes. Two harness rules that cost an hour each if broken: the
`ForkedProbeMain` wrapper is **mandatory** (the generated
`__TestContext001_BeanDefinitions` classes touch package-private members of the
test class), and a chunk **must** keep a class and its `@Nested` children
together — splitting them fails on HotSpot too.

`AotIntegrationTests#endToEndTestsForBeanOverrides` itself is not a debugging
loop: it did not complete in 3 h at `--Xmx 8g`.

## Closed 2026-07-30/31

Four defects, each with a standalone witness. Full rationale is in the commit
messages; the mechanisms are worth keeping because all four are the same shape —
**a loader-blind lookup in a process where two loaders define every name.**

1. **Every AOT probe died in 1.3 s** — `NullPointerException … "this.logger" is
   null` at `AbstractEnvironment.setActiveProfiles`. log4j's `StackLocator`
   walked the stack, a frame's `getDeclaringClass()` answered **null**, and
   `construct_real_standard_environment` **swallowed the resulting exception**,
   handing Spring a synthetic `StandardEnvironment` built with no constructor.
   `declaring_class_native` had been re-resolving a frame's class through
   `class_id_by_name`, which is ambiguity-strict; the frame's own ClassId was
   already resolved to a mirror and simply never consulted. The swallow now
   reports. *(`fix(stackwalker): a frame's declaring class comes from the frame`)*

2. **A CGLIB AOP proxy inherited the wrong copy of its superclass.** The Rust
   side modelled **no parent chain at all** for user-defined loaders, so
   resolution against already-defined classes saw only the loader's own
   namespace and the built-in chain, then fell through to a loader-blind global
   path that DEFINES a second copy in the application namespace. Witness:
   `/data/data/pcprobe` `ParentChainProbe` — `Bar.getSuperclass()` resolved to
   the application loader's `Foo` instead of its parent loader's, in one second,
   with no Spring involved. `CRATONVM_LOADER_PARENT_CHAIN=0` restores the old
   behaviour for A/B on a single binary.
   *(`fix(classloading): model user-loader parent chains for class RESOLUTION`)*

3. **AssertJ soft assertions could not build their ByteBuddy proxy** —
   `IllegalArgumentException: Could not create type` … `Cannot resolve T`.
   CratonVM's own ByteBuddy shims built their results with
   `new_object_initialized(<literal name>, …)`, which resolves globally and so
   returned the **application** loader's copy whoever called. Two copies of
   `TypeDefinition$Sort` then compare unequal, `getTypeVariables()` filters
   every type variable away, and a generic method looks non-generic.
   *(`fix(natives): ByteBuddy shims must build their result in the CALLER's loader`)*

4. **`@MockitoSpyBean` stubs vanished behind a Spring AOP proxy.** The spy was
   fine — a direct call returned the stub — but the call through the proxy ran
   the real method with **no interceptor frames at all**. The proxy's override
   is package-private and had been defined into `DynamicClassLoader` while its
   superclass lives in the fork loader: same package name, different runtime
   packages, so per JVMS §5.4.5 it does not override. HotSpot avoids this
   because `java.base` does not `opens java.lang`, so Spring CGLIB cannot use
   the reflective `ClassLoader.defineClass` and falls back to
   `Lookup.defineClass` on the neighbour class — which lands the proxy in its
   superclass's loader. CratonVM allowed the reflective call.
   `probes/DefineClassAccessProbe.java` is the witness.
   *(`fix(reflect): keep ClassLoader.defineClass encapsulated, as HotSpot does`)*

### Two investigation notes worth reusing

* **`setAccessible` has three competing natives** (`lang_class::set_accessible_impl`,
  `lib.rs`'s `native_set_accessible_write_override`, `lang_reflect`'s
  `AccessibleObject` variants). A guard added to one of them is silently
  bypassed — the first attempt at fix 4 looked like a no-op for exactly that
  reason.
* **Instrument the real test before writing a fourth synthetic probe.** Four
  successively closer standalone probes for fix 4 (plain spy → spy behind an AOP
  proxy → proxy in a child loader → package-private method) **all passed**; the
  bug needed the real AOT context. Compiling an instrumented copy of the failing
  test into a directory placed ahead of the suite on `-cp` costs two minutes and
  prints whatever you want from inside the real run. That is what cracked it.
