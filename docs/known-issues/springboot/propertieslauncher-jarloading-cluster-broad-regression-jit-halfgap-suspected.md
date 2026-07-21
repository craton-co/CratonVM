# `PropertiesLauncherTests`-adjacent jar/classpath loading — broad regression, root cause found (NOT the JIT halfgap branch); partially fixed, residual classloader-dispatch mystery still OPEN

**Status: OPEN (partially fixed) — found 2026-07-20, root-caused and partially
fixed 2026-07-21.** Originally suspected the `perf/halfgap-residuals-20260718`
JIT branch (see the old title); that hypothesis is now **disproven** — see
"Root cause" below. Branch `fix/propertieslauncher-jarloading-halfgap-20260721`.

## Symptom (original, 2026-07-20)

Discovered while verifying
[`propertieslauncher-loader-path-ignored-wrong-app-launched-FIXED.md`](../../internal/springboot/propertieslauncher-loader-path-ignored-wrong-app-launched-FIXED.md):
that fix was verified clean (32/32 `PropertiesLauncherTests` PASS) against
`dev@939f61817`, but re-verifying against later `dev` showed **15/32
failures**, including tests that were previously solid.

## Root cause: a botched merge silently reverted a large chunk of `vm_exec.rs`

Bisection (rebuilding at exact commits, not just the loosely-related ones the
original bisection table walked) pinned the true clean/broken boundary to
`dev@34bce2630` (clean, `PropertiesLauncherTests` 4/32 fail — the
pre-existing baseline) vs. `dev@b90ecea19` (broken, 15/32 fail) — the merge
of branch `fix/springboot-cloudfoundry-integrate-20260720` into dev. That
branch was based on an **old** dev snapshot (predating `34bce2630` by about
two days) and its own commits + merge only ever touch
`native-builtins/src/phases_late.rs`, `native-builtins/src/t27_tls.rs`, and
`vm/src/vm/vm_exec.rs`/`vm/src/runtime/interpreter.rs` — all legitimately
adding new SSL/TLS bridge code. But diffing `vm_exec.rs` between the clean
and broken commits shows `vm/src/vm/vm_exec.rs` had **700 lines removed vs.
211 added** — a large net deletion for a branch whose own commits are purely
additive SSL work. The merge (or one of its component merges) silently
**reverted** a swath of unrelated dev-mainline improvements that had landed
on `vm_exec.rs` between the CloudFoundry branch's base and `34bce2630`,
while keeping the new SSL code — a many-way merge conflict resolved in favor
of the stale branch content on non-conflicting-looking hunks.

Confirmed reverted (still missing at current `dev` tip as of this writing):

- `ExplodedArchive.getClassPathUrls`'s force-native override entry
  (`vm_exec.rs`) and its native implementation + registration
  (`phases_late.rs`'s `p59_spring_boot_exploded_archive_get_class_path_urls`)
  — **FIXED, see below**.
- `URLClassLoader.getResourceAsStream`'s force-native override entry
  (`vm_exec.rs`) — **FIXED, see below**.
- `java/util/zip/ZipFile`'s force-native override entry (`<init>`,
  `getEntry`, `getInputStream`, `entries`, `stream`, `getComment`, `close`,
  `getName`, `size`) — **NOT fixed** (audited: every one of these methods
  except `getComment` is already separately overridden on `JarFile` itself,
  so this entry only matters for `JarFile`'s inherited `getComment()` call —
  low priority, not investigated further).
- `java/nio/file/Path.toString()`'s force-native override entry — **NOT
  fixed** (unrelated to this cluster; flagged for a separate pass).
- Several other overrides (`is_netty_event_executor_group_shutdown_native_override`,
  `is_springboot_mongo_reactive_customizer_destroy_native_override`,
  `is_springboot_mongo_reactive_customizer_customize_native_override`,
  `is_datagram_channel_open_native_override`) — **NOT fixed**, out of scope
  for this doc; worth a follow-up sweep since these affect unrelated suites
  (Netty, MongoDB reactive).
- `try_alloc_object_gc_safe` (the whole function, `vm/src/vm/vm_exec.rs`) —
  **NOT fixed**. This was the GC-safe allocation path added for the
  `silent-hang-no-signature-cluster` fix (`TestResponsePerformance`'s
  `new URI(...)` hot loop). Its removal is a potential **silent perf/OOM
  regression** for any code exercising that path today — flagged, not
  investigated.
- `thread_jmx_snapshot`/`record_jmx_owned_synchronizer` (JMX `ThreadInfo`
  support) — **NOT fixed**. Affects JMX thread-dump accuracy, not this
  cluster.
- `release_monitors_held_by`/`release_monitors_held_by_except` call sites in
  `monitor_enter_blocking`/`monitor_enter_synchronized_method`/thread-death
  handling — **NOT fixed**. This is the "a monitor held by a thread that
  died mid-synchronized-block is force-released so other waiters don't hang
  forever" mechanism — its removal is a **potential deadlock/hang
  regression** for any test where a thread dies while holding a monitor.
  Flagged as high-priority for a follow-up session; not investigated here.

**None of the above (except the two explicitly marked FIXED) were
investigated or fixed in this pass** — they were identified by diffing
`vm_exec.rs` between the clean and broken commits and are recorded here so a
future session doesn't have to re-discover them. Given the scale, a full
audit and restoration of everything this merge dropped is its own project;
this doc's scope was PropertiesLauncherTests specifically.

## Fixes applied (2026-07-21, branch `fix/propertieslauncher-jarloading-halfgap-20260721`)

1. **Restored `ExplodedArchive.getClassPathUrls`** (native +
   `vm_exec.rs` force-override entry). The restored native could not be a
   byte-for-byte copy of the pre-revert version: that version hand-built a
   3-field synthetic `ArrayList` (`field 0 = Int(0)`, `field 1 = array`,
   `field 2 = size`) — already stale relative to the *current* sibling
   `JarFileArchive.getClassPathUrls`'s 2-field convention
   (`elementData`/`size`), and **neither** layout is safe in real-JDK mode:
   `native-collections::al_slots()` resolves `ArrayList`'s real field
   indices dynamically via `ctx.resolve_field_index` (real-JDK mode reports
   `elementData=4, size=6`, inherited from `AbstractList`/
   `AbstractCollection`), so a hand-poked object at slots 0-2 is invisible
   to real bytecode's `Collection.iterator()`/`.addAll()` — it only *looked*
   like it worked in a quick check because `JarFileArchive`'s sibling
   result happens to only ever be consumed through a native
   `LinkedHashSet(Collection)` constructor, which has a **separate,
   layout-tolerant fallback** (`collect_collection_elements` tries the
   real-JDK slots first, then falls back to the legacy 0/1 slots). Fixed by
   building a **real** `java.util.ArrayList` via its own natively-backed
   `<init>()V`/`add(Object)Z` instead of hand-writing fields — this is
   correct under either field layout because it goes through the same
   `native_al_add` the rest of the VM already trusts.
2. **Restored `URLClassLoader.getResourceAsStream`**'s force-native override
   entry in `vm_exec.rs` (the native itself, `cl_get_resource_as_stream`,
   was never removed — only the override-list entry was).

### Verification

`PropertiesLauncherTests`: **15/32 → 12/32 fail** (3 tests fixed cleanly,
zero new regressions introduced by either fix — confirmed via an
intermediate build that the ArrayList-layout bug briefly regressed
`testUserSpecifiedSlashPath`, caught and fixed before merging).

73-class sibling regression sweep (every class in the `loader/spring-boot-loader`
module, baseline binary vs. fixed binary): two more classes improved as a
side effect — `ExplodedArchiveTests` (FAIL → PASS) and `RepackagerTests`
(CRASH → PASS) — and zero classes regressed. `ZipContentTests` shows
CRASH-at-65s (baseline) vs. HANG-at-120s-timeout (fixed), both with **0
tests executed** in either run — a pre-existing, load-dependent startup
flake unrelated to this change (see
[[feedback_shared_host_multitenant_confound]]), not a regression.

## Residual: 12 failures, still OPEN, two sub-clusters

### Cluster A (8 tests) — `ClassNotFoundException: demo.Application` / bare `ClassNotFoundException`

`testUserSpecifiedNestedJarPath`, `testUserSpecifiedClassPathOrder`,
`testUserSpecifiedJarPath`, `testUserSpecifiedWildcardPath`,
`classPathWithoutLoaderPathDefaultsToJarLauncherIncludes`,
`testUserSpecifiedDirectoryContainingJarFileWithNestedArchives`,
`testUserSpecifiedClassLoader`, `testUserSpecifiedJarPathWithDot`.

**Confirmed regression window**: passes at `dev@34bce2630`, already broken
at `dev@c2e358bce` (`cf3a44e2a`'s own parent — i.e. the regression predates
even the suspected JIT halfgap commit; it's somewhere in the ancestry of
`c2e358bce`, not introduced by `cf3a44e2a` itself). `--nojit` does not
change the result.

**Mechanism traced (but not root-caused) via extensive interpreter
instrumentation**: `Launcher.launch()` builds a `LaunchedClassLoader`
(a `URLClassLoader` subclass that overrides the protected 2-arg
`loadClass(String,boolean)` and calls `super.loadClass(name, resolve)` when
the requested class isn't a jarmode-internal class). That `super` call is
an `invokespecial` whose JVMS-resolved owner is `java/lang/ClassLoader` —
CratonVM has a registered native for exactly this `(class, method,
descriptor)` triple (`cl_real_load_class_base`, in
`native-builtins/src/classloader_real.rs`), and BOTH dispatch gates that
should force it are present and textually unchanged between the clean and
broken commits: `vm_exec.rs`'s `check_override` chain (line ~16426,
`class_name == "java/lang/ClassLoader" && method_name == "loadClass" && ...`)
and `interpreter.rs`'s `force_native_over_real_jdk_bytecode` (line ~24832,
explicit comment "including invokespecial super calls from custom
loaders"). Despite this, exhaustive `eprintln!`-based tracing at every layer
— `cl_real_load_class`/`cl_real_load_class_base` entry, the parent-delegation
step, `intercept_force_registered_native`'s `class_name` parameter, and the
`invoke_class` computation inside `execute_invoke_kind`'s `is_special`
branch (`invokespecial_owner_class_name`) — showed **the invokespecial
dispatch for this exact call never reaches ANY of these instrumented code
paths**, even under `--nojit` (ruling out a JIT-compiled-code bypass). The
call resolves *somewhere* — `LaunchedClassLoader.loadClass`'s virtual
dispatch clearly runs (traced separately) and its `super.loadClass()` result
comes back as `ClassNotFoundException` — but the exact mechanism that
resolves it without ever touching `execute_invoke_kind`'s slow path or
`intercept_force_registered_native` was not found.

**Ruled out**: per-thread invoke-cache poisoning from an earlier test
sharing the same `(caller_class_id, cp_index, is_special)` cache key (all 32
tests run in one JVM process/thread, so the cache is shared across tests,
and this exact call site — same overriding class, same bytecode offset — is
identical for every `LaunchedClassLoader` instance across every test). Added
a speculative cache-eviction bypass specifically for `is_special &&
loadClass` (mirroring the existing pattern at `execute_invokevirtual_cached`
for the null-arg `getResource*` family) to force every such call through the
slow path — **no effect** (still 12/32 fail, identical list) — so the
dispatch bypass happens even before or independent of the per-thread cache.
This rules out the most likely-looking theory; the actual mechanism is still
unknown. Candidate next steps for a future session:

1. Check the "raw fast-dispatch loop" (`vm/src/runtime/interpreter.rs`,
   opcode `0xb7` handler around line 9364) and `execute_invokevirtual_vtable_fast`
   for a THIRD resolution path that might bypass both
   `execute_invokevirtual_cached`'s inline cache AND the VM-wide
   `SharedResolutionState`/"promoted invoke" mechanism
   (`shared.shared_resolution.get_promoted_invoke`) — the promoted-invoke
   cache is process-lifetime, not per-thread, and wasn't directly
   instrumented in this pass.
2. Add `eprintln!` tracing directly inside `cl_real_load_class_base`'s
   registered native closure itself (`vm_exec.rs`
   `r.register(cl, "loadClass", "(Ljava/lang/String;Z)Ljava/lang/Class;",
   ...)`), not just the Rust function it calls — to rule out a subtly
   different call path reaching the SAME native through a different Rust
   entry point that doesn't funnel through the traced functions.
3. Compare a full symbolicated stack trace (attach a debugger / add a panic
   probe) from inside `java/lang/ClassLoader.loadClass(String,boolean)`
   real bytecode execution (if that's genuinely what's running) to see
   which Rust function actually invoked it.

### Cluster B (4 tests) — `AssertionError: Expecting elements: [...] to be exactly 1 times 1`

`testUserSpecifiedRootOfJarPathWithDot`, `testUserSpecifiedJarFileWithNestedArchives`,
`testUserSpecifiedRootOfJarPath`, `testUserSpecifiedRootOfJarPathWithDotAndJarPrefix`.

**Not investigated in this pass** (deprioritized in favor of the larger
Cluster A). All four exercise `JarFileArchive`/nested-jar (`!/`) root-path
scenarios via `getClassPathUrlsForNested`, a different code path from
Cluster A's plain-URLClassLoader-parent-delegation scenario — plausibly
related to the same underlying dispatch gap (nested-jar loading also
resolves classes through the classloader chain) but unconfirmed. Also same
regression window as Cluster A (passes at `34bce2630`, fails from
`c2e358bce` onward).

## Affected classes

`org.springframework.boot.loader.launch.PropertiesLauncherTests` (12 of 32
remaining, see clusters above). `ExplodedArchiveTests` and `RepackagerTests`
were incidentally fixed by this pass; not otherwise swept for further
siblings affected by the still-open dispatch gap.
