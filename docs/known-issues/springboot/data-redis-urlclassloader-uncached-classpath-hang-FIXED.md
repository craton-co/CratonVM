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

#### RESOLVED 2026-07-26 (continuation session)

Root cause was exactly the "not yet checked" gap above: `DialectOverride`'s
nest host **and** its ~30 nested annotation-override members were being
resolved through `native_class_get_nest_members`'s (and the sibling
`native_class_get_permitted_subclasses`'s) **loader-blind global lookup
FIRST**, falling back to the correct per-loader `ClassLoader.loadClass()`
call only on a miss. Since `org.hibernate.annotations.DialectOverride` and
its member classes were already loaded under the *first* (non-isolated)
run of the test class before `ModifiedClassPathExtension`'s nested
`Launcher` re-ran the whole class under its own `ModifiedClassPathClassLoader`,
the blind lookup silently found and returned the **first** run's copies —
even when called from a nest host resolved under the **second**,
isolated loader. `DialectOverridesAnnotationHelper.OVERRIDE_MAP`'s
static initializer (running under the isolated loader, per the isolated
run's own class instance) ended up keyed by the **first** run's `Class`
objects, while `getOverrideAnnotation(SQLInsert.class)`'s lookup key came
from the isolated run's own `SQLInsert.class` literal — two
reference-unequal `Class` objects for "the same" class name, guaranteed
`HashMap` miss, matching the observed `does not have an override form`
failure exactly.

Fixed in `native-builtins/src/lang_class.rs` by adding
`resolve_nestmate_via_defining_loader` and using it in both
`native_class_get_nest_members` and `native_class_get_permitted_subclasses`:
the class's own defining loader's `loadClass()` is now tried **first**
(cheap even when already loaded, since `ClassLoader.loadClass` itself
checks `findLoadedClass` before delegating/defining), falling back to the
global lookup only when no Java-level loader object is available
(bootstrap-loaded classes). This can only ever return a *more* correct
answer than the old ordering — never a worse failure.

**Verified: `DataJpaRepositoriesAutoConfigurationTests` now passes 9/9**
(Azure Linux host, worktree `/data/data/wt-redis-issues-20260726`, branch
`fix/redis-residuals-20260726`, binary `cratonvm-redis-issues-20260726`,
forked from `origin/dev` @ `887cd01fa`). No further action needed here.

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

#### Continuation 2026-07-26: original corruption bug gone, THREE different residuals found instead

Re-verified on `origin/dev` @ `887cd01fa` (worktree
`/data/data/wt-redis-issues-20260726`, branch `fix/redis-residuals-20260726`,
binary `cratonvm-redis-issues-20260726`): the `NoUniqueBeanDefinitionException`/
`Parameter.getName()` corruption bug described above **no longer
reproduces** — `subProtocolWebSocketHandler`'s by-parameter-name tie-break
now works correctly across all 13 `@Test` methods in one process. Like the
`DataRedisAutoConfigurationTests` perf residual above, this was apparently
closed by unrelated `dev` work landed since 2026-07-26 (the same
`getNestMembers0`/nest-sibling-resolution fix documented above may well be
implicated, since Spring's `MethodParameter`/annotation-attribute
resolution also walks nest-mate metadata in places — not confirmed, but
plausible given the fix's blast radius).

With that mask gone, the class now reliably fails **2 of 13** tests (not 3
— see the `ArrayListSubList` fix below, landed this session, which closed
the third):

1. **FIXED this session — `cratonvm/internal/ArrayListSubList` (the
   `ArrayList.subList()` backed-view object added by the original fix
   above, see `native-collections/src/lib.rs`'s "ArrayList subList backed
   view" section) declared ZERO interfaces**, not even `List`, because its
   internal class name has no entry in `classloading/src/class_manager.rs`'s
   `jdk_interfaces()` match (which drives every synthetic stub's declared
   interface list) — it silently fell to that match's `_ => &[]` default.
   Any checkcast/instanceof against `List`/`Collection`/`Iterable` on a
   `subList()` result then failed with `ArrayListSubList cannot be cast to
   java.lang.Iterable`, reproducing in
   `webSocketMessageBrokerConfigurerOrdering` via AssertJ's
   `Iterable`-typed `satisfies`/`contains` overloads (`configurers.subList(3,
   5)`). Fixed by adding `"cratonvm/internal/ArrayListSubList" =>
   &["java/util/List", "java/util/RandomAccess"]` to `jdk_interfaces()`,
   mirroring the real `java.util.ArrayList$SubList` (`extends AbstractList
   implements RandomAccess`; `List`/`Collection`/`Iterable` come for free
   via `is_subclass_of`'s existing interface-of-interface recursion, which
   already walks a resolved interface's own super-interfaces). Verified:
   `webSocketMessageBrokerConfigurerOrdering` now passes; 3 repeated runs
   all showed the same 2 remaining failures below, never this one again.

2. **Root-caused via live `gdb` attach (2026-07-26 continuation, see below)
   — TCCL-leak hypothesis REFUTED; real cause is a loader-blind
   "resolve globally first" fallback reusing an isolated loader's class —
   NOT fixed.** `shouldUseJackson2WhenPreferred` fails with
   `IllegalArgumentException: argument type mismatch` constructing
   `WebSocketMessagingAutoConfiguration$Jackson2WebSocketMessageConverterConfiguration(ObjectMapper)`.
   See the "gdb investigation" subsection immediately below for the full
   trace methodology and evidence; summary:
   - `Thread.contextClassLoader` is **correctly** set-then-restored around
     each of the two `@ClassPathExclusions("jackson-*-3*")` methods'
     nested-`Launcher` runs (confirmed directly: a `gdb` breakpoint on
     `native_thread_set_context_class_loader`'s field write,
     `native-builtins/src/lib.rs:4539`, shows the main thread's
     `contextClassLoader` cleanly alternating isolated-loader →
     original-loader → isolated-loader → original-loader across the run,
     matching `ModifiedClassPathExtension.interceptMethod`'s real
     `try { setContextClassLoader(modified); runTest(); } finally {
     setContextClassLoader(original); }` source exactly). The original
     TCCL-leak-between-methods theory from the prior pass is **refuted**.
   - The real problem: `WebSocketMessagingAutoConfigurationTests` itself
     genuinely has two distinct copies alive in the one process — `ClassId
     411` (default/Application loader, used by all 11 non-excluded test
     methods) and a second id (isolated `ModifiedClassPathClassLoader`,
     used by the 2 excluded methods) — confirmed via a `gdb` breakpoint on
     `resolve_class_loader_aware` (`vm/src/runtime/interpreter.rs:19187`)
     filtered to the nested `WebSocketMessagingConfiguration` config class:
     only ever these two `referencing_class_id`s appear, exactly as
     expected. But `WebSocketMessagingAutoConfiguration` (imported via
     `@ImportAutoConfiguration({..., WebSocketMessagingAutoConfiguration.class,
     ...})` on the *nested* `WebSocketMessagingConfiguration` test-helper
     class) — and hence its nested `Jackson2WebSocketMessageConverterConfiguration`
     — resolves to the **exact same** `ClassId` (confirmed `near=6066`/`6067`
     across separate runs, byte-identical every time) for BOTH the isolated
     test's construction call and `shouldUseJackson2WhenPreferred`'s. A `gdb`
     breakpoint at the constructor-argument check itself
     (`native-builtins/src/lang_class.rs:3928`/`:3931`,
     `coerce_arg_strict`) proves this directly: `expected_cid` is **5009 on
     both calls** (the shared Jackson2Config's declared `ObjectMapper`
     parameter, resolved via the isolated loader), while `arg_cid` is 5009
     on the passing (isolated) call and **1521** (the default loader's own
     `ObjectMapper`, from `registerBean(ObjectMapper.class)` — a plain
     `ldc` in `shouldUseJackson2WhenPreferred`'s own, correctly-loader-2
     bytecode) on the failing call.
   - For `referencing_class_id = 411` (the default-loader test copy, used
     by `shouldUseJackson2WhenPreferred`), `resolve_class_loader_aware`
     shows `user_loader = None` (correctly: 411 is not itself isolated) —
     so resolution takes the "gate-off" branch, which tries
     `shared.load_class_concurrent(name)` (the **global, loader-blind**
     table) *before* falling back to 411's own defining loader. The
     intent (per that branch's own doc comment) is that this is safe
     because the global table is only supposed to contain built-in-loader
     classes — an isolated `ModifiedClassPathClassLoader`'s classes are
     not supposed to leak into it. `resolve_fast_path_class_id`
     (`classloading/src/class_manager.rs:2902`) is SUPPOSED to guard this
     exact case: it only accepts an existing `UserDefined`-loader answer
     as a substitute for the "global" one when
     `find_class_bytes_delegated(name).is_err()` (i.e. the class doesn't
     ALSO exist on the ordinary Bootstrap/Extension/Application classpath
     — the JSTL/`WebappClassLoader`-only-class case the comment
     describes). `WebSocketMessagingAutoConfigurationTests$WebSocketMessagingConfiguration`
     genuinely DOES exist on the ordinary Application classpath (it's a
     compiled test class under `build/classes/java/test`, on the same
     `-cp` SbRunner was launched with) — so by that guard's own logic,
     `find_class_bytes_delegated` should succeed and this fast path should
     correctly refuse the isolated loader's candidate, falling through to
     define a genuinely fresh, loader-411-owned copy instead. It
     evidently does not (or something downstream of it re-collapses back
     to the isolated copy) — **the exact point where this guard fails
     to fire, or gets bypassed, was not confirmed before this pass ran out
     of session budget** (the next breakpoint needed —
     `class_manager.rs:2911`/`:2926`, filtered to
     `WebSocketMessagingConfiguration` — was *designed* but never run: it
     sits on `resolve_fast_path_class_id`, an extremely hot path called on
     nearly every class-name resolution process-wide, and the two
     `gdb`-under-`hc0053dbg`-profile runs earlier in this pass needed
     45-55 minutes each with far fewer, more targeted breakpoints. Running
     it needs either a `gdb` session with `scheduler-locking on`/a longer
     time budget, or (more efficiently) temporarily instrumenting
     `resolve_fast_path_class_id` and `find_class_bytes_delegated` with an
     `eprintln!` gated on `name.contains("WebSocketMessagingConfiguration")`
     and rebuilding once, since by this point the exact function and exact
     two lines to check are already known precisely).
   - **Debugging technique note for whoever continues this**: `gdb`
     against the default release profile is nearly useless here —
     `debug = "line-tables-only"` plus fat LTO optimizes away most local
     variables (`<optimized out>`). Build with `cargo build --profile
     hc0053dbg` instead (already defined in the repo's `Cargo.toml` for
     exactly this purpose — full debuginfo, `opt-level = 1` overall,
     `opt-level = 0` for `native-builtins`/`native-io` specifically) — this
     is what made `expected_cid`/`arg_cid` readable at all. Even so, some
     locals in the `vm` crate (`opt-level = 1`) still show `<optimized
     out>` (e.g. `resolve_class_loader_aware`'s `known` binding) — when
     that happens, either move the breakpoint a few lines later to a
     point where the value is about to be *used* (works reliably for
     `native-builtins`, which is fully `opt-level = 0`), or breakpoint one
     frame up at the call site instead. A `gdb.execute("finish")` called
     from inside a Python `Breakpoint.stop()` handler to capture a return
     value directly does **not** work in this multi-threaded inferior
     ("Cannot execute this command while the selected thread is running") —
     use the call-site-local-variable approach instead. Filter breakpoints
     in Python (`Breakpoint.stop()` returning `False` to silently
     auto-continue) rather than with a `condition` string — GDB's Rust
     support cannot reliably evaluate `&str` content comparisons in a
     plain breakpoint condition, but reading the value via
     `frame.read_var(...)` and comparing with plain Python `in` works
     fine and is fast enough once restricted to a non-hot-path function.
3. **Likely the same root cause, not separately investigated —
   `basicMessagingWithJsonResponse` fails with `AssertionError: Response
   was not received within 30 seconds`** (a STOMP round-trip that silently
   never completes, rather than a startup exception) — consistent with the
   Jackson2 message converter configuration failing to apply correctly
   under the same loader confusion, so the JSON payload is never converted/
   delivered, rather than the context refresh itself throwing.

**Reproduction**: run the whole `WebSocketMessagingAutoConfigurationTests`
class (SbRunner or the suite runner) against the
`module/spring-boot-websocket` Gradle test classpath — reproduces
deterministically (3-4 repeated runs, same 2 core failures each time,
though the exact set/order of the OTHER 1-2 non-deterministic
`NoClassDefFoundError`/timeout failures varies run to run since JUnit
doesn't guarantee method order) once the `ArrayListSubList` fix above is
applied. `CRATONVM_DBG_COERCE=1` + `CRATONVM_DBG_UCLTRACE=1` give the
class-identity evidence for items 2-3 without needing new instrumentation
(no `gdb`/rebuild required); the `gdb` session above was needed only to
confirm/refute the TCCL-restore mechanism and pin down exactly which
resolution layer (`resolve_class_loader_aware`'s "gate-off" global-first
branch, `native-builtins/lang_class.rs`'s `coerce_arg_strict`) the
mismatch flows through.

## Worktree / branch

Original fix: `C:\craton\CratonVM-data-redis-fix-20260723`, branch
`fix/spring-boot-data-redis-hangs-20260723`, binary
`cratonvm-data-redis-fix-20260723.exe`.

2026-07-26 residual triage (Azure Linux host):
`/data/data/wt-redis-residuals-20260725`, branch
`fix/redis-residuals-20260725`, binary
`cratonvm-redis-residuals-20260725`.

2026-07-26 continuation (Azure Linux host):
`/data/data/wt-redis-issues-20260726`, branch
`fix/redis-residuals-20260726`, binary
`cratonvm-redis-issues-20260726`, forked from `origin/dev` @ `887cd01fa`.

2026-07-26 `gdb` investigation (Azure Linux host, no code changes — pure
root-cause tracing): `/data/data/wt-ws-tcclgdb-20260726`, branch
`fix/websocket-tcclleak-20260726`, forked from `origin/dev` @ `ce3adcf7f`.
Debug binary built with `cargo build --profile hc0053dbg`, frozen as
`target/hc0053dbg/cratonvm-tcclgdb-20260726` (this worktree's `target/`
was removed after this pass; rebuild with the same profile to resume).
