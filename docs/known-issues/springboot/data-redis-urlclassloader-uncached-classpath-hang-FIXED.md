# `module/spring-boot-data-redis` HANG cluster — uncached `URLClassLoader` classpath rescan — FIXED 2026-07-23

## Symptom

Four `module/spring-boot-data-redis` classes HANG (300s suite timeout, zero
tests completed) on every rerun since at least 2026-07-17:

- `DataRedisAutoConfigurationTests`
- `DataRedisAutoConfigurationJedisTests`
- `DataRedisAutoConfigurationLettuceWithoutCommonsPool2Tests`
- `DataRedisHealthContributorAutoConfigurationTests`

Confirmed via `docs`/`apps/spring-boot-suite-runner/RESULTS-20260723.md`'s
2026-07-23 rerun and the earlier 2026-07-17 round — same 4 classes both
times, always `HANG`, never `FAIL`.

## Root cause (two compounding bugs, both in `native-builtins/src/`)

`module/spring-boot-data-redis`'s test classpath has ~121 jars (Lettuce +
Jedis + Netty + commons-pool2 + Spring SSL infra stack up unusually large
compared to most modules). Every one of these 4 classes' test methods
creates a fresh `ApplicationContext` via `ApplicationContextRunner`, which
triggers hundreds of `ClassUtils.isPresent()`/`ClassLoader.loadClass()`
calls per test (checking optional dependencies, evaluating
`@ConditionalOnClass`, etc.) — completely ordinary Spring Boot
auto-configuration bootstrap, not Redis-specific.

**Bug 1 (dominant, ~99% of the cost) — `classloader.rs`:**
`ucl_try_define_local_class` (backs `URLClassLoader.findClass`) and
`loader_local_resource_urls` (backs `findResource`/`getResources`) both
called `cratonvm_classloading::ClassPath::new(&paths)` **fresh on every
single invocation**, with no caching at all. `ClassPath::new` opens and
parses every classpath entry eagerly. On a 121-jar classpath this meant
every `isPresent()`/`loadClass()` call — not just the first — repeated the
full classpath scan. Measured: 100 mixed hit/miss `Class.forName` calls on
one `URLClassLoader` took ~10-12s on CratonVM vs ~126ms on HotSpot (a
~90x-ish gap, and growing with classpath size and call count — genuinely
unbounded for a class with dozens of test methods).

**Bug 2 (secondary, `phases_late.rs`):** `jar_contents_cached` (backing the
synthetic `JarFile.getEntry`/`getJarEntry`/`entries`/`stream`/`getManifest`
natives) eagerly `read_to_end`'d (fully DEFLATE-decompressed) **every
entry** in a jar on the first touch, even when the caller only wanted a
metadata/existence answer. A single `new JarFile(...)` on `testcontainers`
`.jar` (2.0.5, 12566 entries) cost ~600-750ms just from decompressing
thousands of unrelated class files nobody asked for; measured ~60-70us/entry,
scaling linearly with jar size. Also, `p98_read_jar_manifest` (called from
every `JarFile`/`ZipFile` `<init>`) did its own independent, uncached
`zip::ZipArchive::new` + manifest read, redundant with the metadata cache.

Neither bug is Redis-specific — they're general classloading/jar
infrastructure — but `spring-boot-data-redis`'s unusually large test
classpath was what tipped these 4 specific classes over the 300s suite
timeout while smaller-classpath modules mostly stayed under it (some
residuals in other modules, e.g. `DataJpaRepositoriesAutoConfigurationTests`,
`WebSocketMessagingAutoConfigurationTests`, were hitting the exact same
bugs — see "Residuals unmasked" below).

## Fix

- `native-builtins/src/classloader.rs`: added `cached_class_path_for_paths`,
  a `Mutex<HashMap<Vec<String>, Arc<ClassPath>>>` cache keyed by the exact
  `paths` vector (so `URLClassLoader.addURL` naturally gets its own fresh,
  correct entry — no explicit invalidation needed). Both
  `ucl_try_define_local_class` and `loader_local_resource_urls` now go
  through it instead of calling `ClassPath::new` directly.
- `native-builtins/src/phases_late.rs`: split `jar_contents_cached`'s
  per-entry record (`JarEntryRec`) into metadata-only (no `bytes` field);
  added a new `jar_entry_bytes_cached`, a per-(path, mtime, entry name)
  cache that decompresses lazily, only when `getInputStream` is actually
  called for that entry. `p98_read_jar_manifest` now reads the manifest via
  `jar_entry_bytes_cached` instead of its own independent
  `ZipArchive::new`+`read_to_end`.

## Verification

- Microbenchmark (100 mixed `Class.forName` calls over the data-redis test
  classpath): **~12000ms → 176ms** (HotSpot baseline: 126ms — now
  comparable).
- All 4 originally-HANG classes: `DataRedisAutoConfigurationLettuceWithoutCommonsPool2Tests`
  and `DataRedisHealthContributorAutoConfigurationTests` now `PASS` cleanly
  (~18-24s). `DataRedisAutoConfigurationTests` (56 tests) now completes and
  **passes 56/56** — just takes ~349s, 49s over the 300s suite-runner
  default (see "Residual" below; this is a genuine perf gap unrelated to
  the caching bug, not a hang). `DataRedisAutoConfigurationJedisTests` now
  completes in ~215s, 22/23 tests pass — the one remaining failure
  (pre-existing, unrelated, previously masked by the class-level hang) is
  **also now FIXED 2026-07-23**, see
  `../../internal/fixed-suite-bugs/springboot/data-redis-jedis-sslbundle-withpackageresources-classloader-leak-FIXED.md`.
  Full class now 23/23 PASS.
- Regression sweep: 82 classes (all `module/spring-boot-data-redis` +
  71 classes across the codebase using `@ClassPathExclusions`/
  `@ClassPathOverrides`, chosen because they most directly exercise the
  changed caching code). 41 PASS, 41 FAIL/HANG — **every single FAIL/HANG
  cross-checked against the pre-fix 2026-07-23 baseline
  (`apps/spring-boot-suite-runner/RESULTS-20260723.md`) and found already
  FAIL or HANG there** — zero new regressions.

### Residuals unmasked (not new — previously hidden behind the 300s HANG on these specific classes)

- `DataJpaRepositoriesAutoConfigurationTests`: HANG(300s) → FAIL(96s). **Partially triaged 2026-07-26, still FAILING** — see below.
- `WebSocketMessagingAutoConfigurationTests`: HANG(300s) → FAIL(184s). **Triaged 2026-07-26, still FAILING** — see below.

Both were already broken before this fix (confirmed via the 2026-07-23
baseline); the classloading fix just lets them run to completion fast
enough to show their real, different, unrelated failures instead of timing
out.

### Residual: `DataRedisAutoConfigurationTests` ~349s (> 300s default) — RESOLVED 2026-07-26

Re-verified on `origin/dev` @ `4d97f0eec` (Azure Linux host,
`/data/data/wt-redis-residuals-20260725`, binary
`cratonvm-redis-residuals-20260725`): **56/56 pass in 93s**, well under the
300s suite-runner default — no targeted fix needed. The general interpreter
dispatch-overhead gap this doc originally attributed the ~349s to has
apparently been closed by unrelated perf work landed on `dev` since
2026-07-23 (multiple `perf/*` branches merged 2026-07-25 — bytecode
quickening, invoke-dispatch caching, parallel young GC, etc. per the
project's roadmap tracking). No further action needed here; re-verify
against a full suite run rather than assume this stays fixed as `dev`
continues to move.

### Residual: `DataJpaRepositoriesAutoConfigurationTests` — real VM bug found + fixed, test still FAILS for a second, undiagnosed reason

**Triaged 2026-07-26** in a separate worktree
(`/data/data/wt-redis-residuals-20260725`, branch
`fix/redis-residuals-20260725`, binary `cratonvm-redis-residuals-20260725`,
forked from `origin/dev` @ `4d97f0eec`).

Root cause of the FAIL: `HibernateJpaConfiguration`'s `entityManagerFactory`
bean fails with `org.hibernate.HibernateException: Specified Annotation
type (org.hibernate.annotations.SQLInsert) does not have an override form`,
thrown from
`org.hibernate.boot.model.internal.DialectOverridesAnnotationHelper.getOverrideAnnotation`.
That method's `OVERRIDE_MAP` (`Map<Class<?>, Class<?>>`) is built once, in a
static initializer, by iterating `DialectOverride.class.getNestMembers()`
and reading each nested annotation type's `@OverridesAnnotation` value.

**Bug #1, found and FIXED — `Class.getNestMembers()` (`getNestMembers0`)
only ever returned `[self]` for classes belonging to a nest with more than
one member**, in `native-builtins/src/lang_class.rs`'s
`native_class_get_nest_members`:
- It queried `nest_member_names(class_id)` — the class-file `NestMembers`
  attribute — on `this` class's own `class_id`. But per JVMS, only the nest
  **host** carries `NestMembers`; a nest **member** only carries a
  `NestHost` back-pointer. Calling on a member (the overwhelmingly common
  case — nobody calls `getNestMembers()` on the literal host class) always
  found an empty attribute and silently fell back to `[self]`. Fixed by
  resolving the true host via `nest_host_name()` first (mirroring the
  existing `getNestHost0` logic), then querying the *host's* `NestMembers`.
- Even after that fix, each listed member name was resolved via a passive
  `class_id_by_name()` registry lookup, which only finds classes some other
  code has already caused to load. Nest members that are otherwise-untouched
  sibling types (exactly `DialectOverride`'s ~30 nested annotation-override
  types — nothing references most of them by name until this exact helper
  walks the nest) were silently dropped. Fixed by falling back to the
  defining class's own classloader (`loadClass`), mirroring the existing
  `native_class_get_permitted_subclasses` pattern for the structurally
  identical "resolve sibling class names, some possibly unloaded" problem.
- Verified standalone (hand-written repro with a 6-member nest: HotSpot and
  CratonVM now both report `nestMembers.length=7`) **and** against the real
  `org.hibernate.annotations.DialectOverride` class pulled from the actual
  `hibernate-core-7.3.4.Final.jar` on the test classpath (standalone driver:
  CratonVM now builds the *identical* 15-entry override map HotSpot does,
  and `getOverrideAnnotation(SQLInsert.class)` resolves correctly). Added a
  regression unit test,
  `lang_class::tests::class_get_nest_members_resolves_through_host_when_called_on_member`,
  plus mock support for the `nest_member_names` trait method in
  `test_utils.rs` (previously only `nest_host_name` had test-double support).

**This fix is real, generally applicable (any `Class.getNestMembers()` call
on a member of a multi-member nest was affected, not just Hibernate), and
landed** — but **`DataJpaRepositoriesAutoConfigurationTests` itself still
FAILs with the exact same `SQLInsert ... does not have an override form`
error after the fix**, at 8/9 tests. This means the standalone-verified
`OVERRIDE_MAP` build is not what's actually running inside the real Spring
context, or the lookup at `getOverrideAnnotation(SQLInsert.class)` time
uses a different `Class<SQLInsert>` object than whatever object ended up as
the map's key (default `Object.equals`/`hashCode` on `Class` is identity-
based, so any object mismatch — even for "the same class" by name — is a
guaranteed `HashMap` miss).

Ruled out via heavy `eprintln!`-gated instrumentation (all removed before
the final commit — reproduce with a temporary
`std::env::var("CRATONVM_DBG_...")`-gated debug block in
`native_class_get_nest_members` / `native_class_get_annotation` /
`descriptor_to_class_mirror` if picking this back up):
- `getNestMembers0` on the real `DialectOverride` class, inside the actual
  failing `DataJpaRepositoriesAutoConfigurationTests` run: returns the full,
  correct 33-entry array (host + 32 members) — confirmed via a
  `mirrors.len()` print right before the array is built.
- `Class.getAnnotation(OverridesAnnotation.class)` on
  `DialectOverride$SQLInsert`, in that same run: `class_annotations()`
  correctly includes
  `Lorg/hibernate/annotations/DialectOverride$OverridesAnnotation;` in its
  raw list, so the annotation lookup itself succeeds.
- `descriptor_to_class_mirror`'s "class not yet loaded, fall back to
  `synthetic_class_mirror` (a fresh, non-canonical `ObjectRef` every call)"
  path — the original suspect for an identity mismatch — is **not** hit at
  all for `SQLInsert`/`DialectOverride`-family classes in this run (all
  resolve via the `class_id_by_name` cache-hit branch). Defensively
  hardened anyway (re-checks the canonical registry before minting a
  synthetic mirror if `load_class`'s return shape doesn't match, since
  `load_class` may register the class as a side effect even when its return
  value here doesn't surface the mirror) — cheap, more correct, doesn't
  hurt, but not the root cause for this specific failure.
- The actual `Class<? extends Annotation>` value returned by
  `overrideAnnotation.value()` (read via the annotation dynamic-proxy
  invocation, `AnnotationElementValue::Class(desc)` in
  `lang_class.rs`) goes through a **third**, separate resolution path (not
  `descriptor_to_class_mirror`) — `class_id_by_name_near` /
  `class_id_by_name` / `ctx.load_class`, all of which route through the
  same canonical `get_or_create_class_mirror` I could find. Did not find a
  non-canonical mirror anywhere in this path either.

**Not yet checked** (best next step for whoever picks this up): this test
class carries `@ClassPathExclusions("spring-data-envers-*.jar")`, which
routes the *entire* test class through JUnit5's
`ModifiedClassPathExtension` — a nested `Launcher` re-runs the whole test
under a **second**, `ModifiedClassPathClassLoader` instance (see
`modifiedclasspath-aether-network-hang-cluster.md` in
`docs/internal/fixed-suite-bugs/springboot/` for the mechanism). Every
diagnostic above was traced against whichever load actually printed —
never confirmed *which* classloader's copy of
`DialectOverridesAnnotationHelper`/`SQLInsert`/`DialectOverride` was live
at the failure point, or whether the class gets loaded (and its static
`OVERRIDE_MAP` initializer run) under **both** loaders with only one of the
two runs actually observed by the instrumentation. Confirm this before
re-opening the "non-canonical mirror" hypothesis.

### Residual: `WebSocketMessagingAutoConfigurationTests` — real, reproducible corruption bug found, root cause NOT located

**Triaged 2026-07-26**, same worktree/binary as above. Root cause of the
FAIL: `subProtocolWebSocketHandler`'s `@Bean` factory method
(`WebSocketMessageBrokerConfigurationSupport.subProtocolWebSocketHandler(AbstractSubscribableChannel,
AbstractSubscribableChannel)`) needs Spring's by-parameter-name tie-break to
pick between 3 same-typed candidate beans (`clientInboundChannel`,
`clientOutboundChannel`, `brokerChannel`); when that tie-break silently
finds nothing, `NoUniqueBeanDefinitionException` ("found 3") — matching the
actual failure — always for `subProtocolWebSocketHandler`'s parameter 0.

**Confirmed real, standalone-reproducible bug** (not a suite-runner/harness
artifact): a driver that repeatedly constructs
`AnnotationConfigServletWebServerApplicationContext` with the exact
`WebSocketMessagingConfiguration` shape used by the real failing test
(`@EnableWebSocketMessageBroker` + `@ImportAutoConfiguration({Tomcat...,
WebSocketMessagingAutoConfiguration, DispatcherServletAutoConfiguration})`,
matching Spring Boot's own CGLIB-enhanced `@Configuration` machinery)
**succeeds on iterations 0 and 1, then fails identically on every
iteration from 2 onward, in the same JVM process.** This exactly matches
running ~13 `@Test` methods of one class in one `SbRunner` process (one
fresh context per method, same process/classloader). A companion raw-
reflection probe on `Method.getParameters()[i].getName()`/`isNamePresent()`
— called on a `Method` object obtained *once* before the loop, outside any
Spring machinery — starts returning synthetic `arg0`/`arg1` names from
iteration 3 onward, even though `isNamePresent()` still (incorrectly)
reports `true`.

**Root cause NOT found despite heavy instrumentation** (`CRATONVM_DBG_*`
gated `eprintln!`s in `method_parameters` (`vm/src/vm/vm_exec.rs`),
`native_parameter_is_name_present`/`build_parameter_array`
(`native-builtins/src/lang_reflect.rs`) — all removed before the final
commit, reproduce similarly if picking this back up):
- The raw class-file `MethodParameters` attribute read
  (`method_parameters()` in `vm_exec.rs`) returns the **correct**
  `[("clientInboundChannel", 0), ("clientOutboundChannel", 0)]` every
  single time it's called, including at the exact moment of the iteration-2
  failure (checked via a debug print right before the exception fires).
- `native_parameter_is_name_present` never returned `false` anywhere in an
  8-iteration, ~2500-line-of-trace run — every `Parameter.isNamePresent()`
  call, for every parameter, returned `true` with the correct name.
- **Fixed a real, independent bug found along the way** (kept — this is a
  correctness fix regardless of whether it's the root cause here):
  `native_parameter_is_name_present` read the *synthetic* slot-0 field
  first, falling back to the real by-name `"name"` field only if slot 0
  wasn't a String — backwards from `build_parameter_array`'s own write
  priority (by-name first; slot 0 is only written when the by-name write
  didn't land, i.e. pure-synthetic layout). Reading a slot that was never
  written on the common real-JDK-layout path is unsound regardless of
  whether it explains this specific bug. Flipped the read priority to match
  the write priority.
- Given both the attribute-read layer and the `isNamePresent`-read layer
  checked out correct at the exact failure moment, the corruption must be
  either (a) downstream in `MethodParameter`/`DefaultParameterNameDiscoverer`
  Java-side caching that this session did not instrument, or (b) something
  that corrupts the *specific* `Parameter`/`Method` objects Spring itself
  constructs (as opposed to the externally-held probe `Method`, which
  degrades later, at iteration 3, not 2) — possibly connected to the
  per-iteration CGLIB-enhanced-subclass churn (`WebSocketMessagingConfiguration$$SpringCGLIB$$N`,
  a fresh dynamically-defined class every context refresh), but no
  concrete mechanism was confirmed.

**Reproduction recipe for a future session** (avoids re-deriving from
scratch): compile a driver against the `module/spring-boot-websocket`
Gradle test classpath with `-parameters`, register
`WebSocketMessagingConfiguration`-shaped `@EnableWebSocketMessageBroker`
config on a fresh `AnnotationConfigServletWebServerApplicationContext` in a
loop (`server.port=0` / a `TomcatServletWebServerFactory(0)` bean to avoid
port collisions across iterations), call `context.getBean("subProtocolWebSocketHandler")`,
and watch it fail starting iteration 2 of ~8. GDB/native debugger attach at
the iteration-2 boundary, rather than more `eprintln!` sweeps, is probably
the more efficient next move given how much of the natural
Rust-native-code-level surface area is already ruled out.

## Worktree / branch

Original fix: `C:\craton\CratonVM-data-redis-fix-20260723`, branch
`fix/spring-boot-data-redis-hangs-20260723`, binary
`cratonvm-data-redis-fix-20260723.exe`.

2026-07-26 residual triage (Azure Linux host):
`/data/data/wt-redis-residuals-20260725`, branch
`fix/redis-residuals-20260725`, binary
`cratonvm-redis-residuals-20260725`.
