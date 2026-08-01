# `spring-boot-devtools` residual failure cluster — FIXED

**Status: RETIRED 2026-08-01.** All five classes in the scope table pass in
both modes, 45/45 tests. The 2026-07-31 regression is resolved, and a third
failure — in a class this doc's own table listed as closed, but which nobody
had re-run since 2026-07-18 — was found and fixed in the same pass.

## What was actually open

| Class | State before this pass |
|---|---|
| `restart.ChangeableUrlsTests` | FAIL — `urlsFromJarClassPathAreConsidered` |
| `restart.classloader.RestartClassLoaderTests` | CRASH — process died, zero tests reported |
| `autoconfigure.DevToolsPooledDataSourceAutoConfigurationTests` | FAIL — `inMemoryDerbyIsShutdown` |
| `RemoteUrlPropertyExtractorTests` | passing |
| `restart.server.HttpRestartServerTests` | passing |

The third row was not in the 2026-07-31 regression note. It reproduces on a
binary built from `dev` without any of this branch's changes, so it is a
pre-existing hole in the doc's "retired scope" claim, not a regression from
this work.

## Root causes

### 1. `URLClassLoader.getURLs()` expanded a jar's manifest `Class-Path`

`classloader.rs::ucl_get_urls` ran its recorded URLs through
`expanded_manifest_urls`, which replaces any jar carrying a `Class-Path`
manifest with that manifest's resolved entries. Three observable consequences:

- the referring jar itself disappears from the list;
- entries are added with no existence check, so a `Class-Path` name that is
  not on disk becomes a URL;
- the entry is re-percent-encoded by `file_url_spec`, so an already-encoded
  `project%20space` came back as `project%2520space`.

`ChangeableUrls.fromClassLoader` walks `getURLs()` and then expands each jar's
manifest itself, so the pre-expanded list made it report six directories where
five were expected — including `does-not-exist/target/classes/`, a directory
the test deliberately never creates. The Java-side algorithm was correct
throughout: stage `[C]` of `probes/DevtoolsChangeableUrlsProbe.java` passes on
the unfixed binary; only stage `[A]` (`getURLs()`) fails.

`getURLs()` is specified as the URLs the loader was constructed with plus
whatever `addURL` appended. The real JDK resolves `Class-Path` lazily inside
`URLClassPath`, never through this public accessor.

The expansion had been added for Spring Boot's `ModifiedClassPathClassLoader`
(`@ClassPathExclusions`), which is handed the suite runner's manifest-only
pathing JAR and would otherwise filter a class path different from the one the
loader searches. It never applied through this native: the application loader
is `jdk.internal.loader.ClassLoaders$AppClassLoader`, which is not a
`URLClassLoader`, so `ModifiedClassPathClassLoader.doExtractUrls` falls to
`ManagementFactory.getRuntimeMXBean().getClassPath()` — identically on HotSpot
and CratonVM (`probes/RuntimeClassPathProbe.java`). The JDK-internal sibling
`URLClassPath.getURLs` is left alone.

### 2. A `LinkageError` from a fast-path invoke skipped every Java handler

The interpreter's stackless invoke fast paths (`0xb6`/`0xb7`/`0xb8`/`0xb9`)
route their own errors back into the loop through `pending_runtime_error` /
`pending_java_exception` and `continue`, so they never reach the per-opcode
conversion that guards the slow path at the bottom of
`execute_frame_from_index`. Their catch-all arm was `Err(e) => return Err(e)`,
which returned a `VmError::Linkage` straight out of the whole invocation — past
every enclosing frame's exception table, past `main`, and out at
`main-vm run()`.

Every `java.lang.LinkageError` subclass is an ordinary throwable (JVMS §5.4),
and the slow path already converted them. All ten fast-path arms now share
`classify_fastpath_invoke_error`.

`ClassLoader.defineClass1` rejecting bad magic is reached by `invokestatic`
from `ClassLoader.defineClass`, so `RestartClassLoaderTests.getUpdatedClass` —
`assertThatExceptionOfType(ClassFormatError.class).isThrownBy(() ->
Class.forName(..., reloadClassLoader))` — killed the process instead of
passing, and a `catch (Throwable)` wrapped directly around `defineClass` never
ran either.

`CRATONVM_DBG_LINKAGE=1` now traces a linkage error from the native that raised
it through the arm that routed it, with the Java frames at each point. Without
it this failure is indistinguishable from a VM crash — the top-level render is
the same either way.

### 3. A compiled callee's handler was resumed with only its parameters

`run_jit_callee_handler` resumes a compiled callee AT its own handler when an
exception escapes it, and rebuilt the handler frame from the callee's incoming
arguments alone. Every local the handler can still read — anything assigned
between method entry and the protected range, and anything reached by
branching out of the handler — came back zeroed.

That was sound only while the compile gate refused any method whose handler
reads a non-parameter local. `precise_handler_frames_enabled` (default on)
deliberately admits that population, on the promise that the throwing site
publishes a precise reason-9 exceptional frame. The interpreter's own drain
path, `route_jit_signal_exception`, already preferred that frame; this route —
taken when the callee was entered directly from compiled code — did not, so the
promise was only half kept. `local_handler_reads_unsafe_local=true` for the
witness method, i.e. the gate saw the hazard and the relaxation was supposed to
cover it.

Witness: Spring Boot's `BindConverter.convert` holds its enhanced-for iterator
in a compiler-generated local (slot 5) assigned at bci 12 — before its
protected range `[36, 58)` — and its handler at bci 62 falls through to
`goto 14`, the loop head that reads that local again. A delegate throwing a
`ConversionException` resumed the handler with slot 5 zeroed:

```
java.lang.NullPointerException: Cannot invoke "java.util.Iterator.hasNext()"
because "<local5>" is null
    at ...bind.BindConverter.convert(BindConverter.java:108)
```

Spring's `ConfigurationPropertiesBindingPostProcessor` reports that as
`ConfigurationPropertiesBindException: Could not bind properties to
'HikariDataSource'` while binding
`spring.datasource.hikari.validation-timeout`, the context fails to refresh on
its worker thread, and `AbstractDevToolsDataSourceAutoConfigurationTests
.getContext` sees `null` — which is all
`inMemoryDerbyIsShutdown` ever reported.

## How each was pinned down

- The `getURLs()` defect: the assertion diff alone is ambiguous — a
  double-escape plus a phantom entry can equally be a `URL`/`File.exists()`
  fault. `probes/DevtoolsChangeableUrlsProbe.java` splits the test into three
  independently-checkable stages, and only the first fails.
- The `ClassFormatError`: `probes/DefineClassWhereLostProbe.java` wraps a
  `catch (Throwable)` directly around `defineClass` inside `findClass`. It
  does not catch — which places the loss between the native and the immediately
  enclosing frame, and rules out everything above it.
- The JIT miscompile: `--nojit` passes and
  `CRATONVM_JIT_BISECT_SKIP=org/springframework/boot/context/properties/bind/BindConverter.convert`
  passes (both repeated), while the baseline fails 4/4. A source-shape replica
  (`probes/LoopHandlerIteratorLocalProbe.java`) does **not** reproduce, so the
  probe drives the real bytecode instead
  (`probes/BindConverterJitProbe.java`); its `refuse` mode takes the handler
  out of the picture and passes, which is what identified the mechanism.

## Verification

Binary `cratonvm-devtools-cluster-20260801-r6.exe`
(SHA-256 `755B7AC085B177C25DF74727B4C27C676ADC72F5634133CBCB5750E4431C8A6F`),
built from `fix/springboot-devtools-cluster-20260801`, against the shared
Spring Boot fixture at `apps\spring-boot`. Re-verified after merging 111
commits of `dev` forward, on
`cratonvm-devtools-cluster-20260801-r7.exe`
(SHA-256 `ED1186871C1A755AEE308205945B085C6193A670866FD35A70CDEFB89E39AB1F`):
same 45/45 in both modes, all probes still pass.

| Class | JIT | `--nojit` |
|---|---|---|
| `RemoteUrlPropertyExtractorTests` | 5/5 | 5/5 |
| `autoconfigure.DevToolsPooledDataSourceAutoConfigurationTests` | 11/11 | 11/11 |
| `restart.ChangeableUrlsTests` | 6/6 | 6/6 |
| `restart.classloader.RestartClassLoaderTests` | 17/17 | 17/17 |
| `restart.server.HttpRestartServerTests` | 6/6 | 6/6 |
| **Total** | **45/45** | **45/45** |

`RestartClassLoaderTests` reported 0 tests before (the process died during the
run); its 17 are all new signal, not a re-count.

### Module sweep

The whole `module/spring-boot-devtools` module (51 classes,
`.suite\devtools-full-module.tsv`, `RunName=devtools-cluster-verify-20260801`):
**49 PASS, 1 EMPTY, 1 FAIL**.

- EMPTY is `AbstractDevToolsDataSourceAutoConfigurationTests`, the abstract
  base — it declares no tests of its own and reads EMPTY on every run.
- FAIL is `DevToolsR2dbcAutoConfigurationTests`
  (`$Pooled.autoConfiguredInMemoryConnectionFactoryIsShutdown`), which is
  **not** in this doc's scope and **not** caused by this branch: it fails
  identically on a binary built from `dev` with none of these changes (2 runs
  each), and it fails under `--nojit` too, so it is neither of the two VM-level
  defects fixed here. It was PASS in the 2026-07-31 full-suite reference, so it
  is a separate regression that landed on `dev` in between; tracked separately.

### Regression sweep

Two of the three fixes are VM-level, so `core/spring-boot` (358 classes, the
largest module) was swept as well
(`RunName=devtools-cluster-coreregr-20260801`): **342 PASS, 7 EMPTY, 4 FAIL,
4 HANG, 1 CRASH**. Five classes differ from the 2026-07-31 full-suite
reference; every one was A/B'd against a binary built from the same base
commit with none of these changes:

| Class | ref | sweep | A/B verdict |
|---|---|---|---|
| `ConfigDataEnvironmentPostProcessorIntegrationTests` | PASS | CRASH | **repaired by these fixes** — 67/87 tests fail on the unmodified base, 87/87 pass with them (2 runs each). The sweep's CRASH is a `raw_vec` capacity panic at 4s under `-Parallel 4` plus a concurrent `cargo build`; it does not reproduce standalone. |
| `ConfigurationPropertiesTests` | PASS | FAIL | **improved** — 40 failures unmodified, 39 with these fixes, stable across 3 runs each, and the fixed set is a strict subset. (The post-`dev`-merge binary shows 41: two `…ConstructorParametersWith*DataUnitShouldBind` failures arriving with the 111 merged commits, not from here.) |
| `ConfigurationPropertiesBeanRegistrationAotProcessorTests` | PASS | HANG | **load artifact** — 9/9 passes on both binaries standalone, taking 187–195 s against the sweep's 300 s timeout. |
| `ApplicationConversionServiceTests` | FAIL | PASS | improvement, unattributed |
| `LambdaSafeTests` | FAIL | PASS | improvement, unattributed |

No class regressed. The remaining FAIL/HANG rows match the reference exactly.

### Probes

Probe results on the fixed binary, HotSpot as the control:

| Probe | Before | After |
|---|---|---|
| `DevtoolsChangeableUrlsProbe` | stage [A] FAIL | PASS |
| `DefineClassFormatErrorProbe` | VM killed at attempt 1 | 4/4 catchable |
| `DefineClassWhereLostProbe` | VM killed at stage 1 | both stages catch |
| `BindConverterJitProbe` | 199,491 / 200,000 calls wrong | 0 wrong |

## Files

- `native-builtins/src/classloader.rs` — `ucl_get_urls`
- `vm/src/runtime/interpreter.rs` — `classify_fastpath_invoke_error`,
  `run_jit_callee_handler`, `CRATONVM_DBG_LINKAGE`
- `vm/src/runtime/exceptions.rs` — `throw_linkage_error` fallback now says why
- `vm/src/vm/vm_exec.rs` — `CRATONVM_DBG_LINKAGE` at the native boundary
- `probes/DevtoolsChangeableUrlsProbe.java`, `probes/DevtoolsUrlCtxProbe.java`,
  `probes/RuntimeClassPathProbe.java`, `probes/AppLoaderUrlsProbe.java`
- `probes/DefineClassFormatErrorProbe.java`,
  `probes/DefineClassWhereLostProbe.java`,
  `probes/ClassFormatErrorShapeProbe.java`
- `probes/BindConverterJitProbe.java`,
  `probes/LoopHandlerIteratorLocalProbe.java`,
  `probes/UnmodifiableListIteratorJitProbe.java`,
  `probes/DerbyHikariBindProbe.java`
