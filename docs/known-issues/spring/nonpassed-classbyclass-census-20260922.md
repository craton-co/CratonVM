# Spring Framework suite — class-by-class non-passed census, 2026-09-22

| | |
|---|---|
| **Measured** | 2026-09-22, Azure Linux, commit `1c8a3404c`, `--jdk-only`-default, real JDK 25, all defaults, `--category all`, single-class-per-fork (`BATCH=1` default) |
| **Census** | classes: **LOADERR=9, OK=2801, FAIL=37, TIMEOUT=1** (2848 total) — test-methods: found=30683, passed=30055, failed=457 |
| **Source** | `apps/spring-suite-runner/out/classbyclass-default-20260922-jit-real-all-20260922-021216/results.tsv` (columns: `class status found succ fail skip abort ms mode`) |

No prior class-by-class baseline exists for this suite to diff against (unlike Tomcat and H2) — this page is a first census, not a regression check.

## LOADERR — all 9 are one package, `org.springframework.aop.target.*`

| class |
|---|
| `CommonsPool2TargetSourceProxyTests` |
| `CommonsPool2TargetSourceTests` |
| `HotSwappableTargetSourceTests` |
| `LazyCreationTargetSourceTests` |
| `LazyInitTargetSourceTests` |
| `PrototypeBasedTargetSourceTests` |
| `PrototypeTargetSourceTests` |
| `ThreadLocalTargetSourceTests` |
| `dynamic.RefreshableTargetSourceTests` |

Every one of these 9 has `found=0 succ=0 fail=0` — they never got far enough to discover a single test, which is what `LOADERR` means here (a classloading failure, not a test failure). All 9 living in exactly one package, including two (`CommonsPool2*`) that name a specific dependency (`commons-pool2`), suggests one shared cause — most likely a single missing or misresolved classpath entry that this whole package depends on, not 9 independent defects. Not yet root-caused; checking this suite's classpath dump (`run-suite.sh dumpcp`/`check-cp`) against what `org.springframework.aop.target` actually needs is the obvious next step.

## FAIL — the AOT/code-generation cluster is roughly half of the 37

Spring's AOT (ahead-of-time) infrastructure generates Java source at test time and then compiles and loads it back — a much heavier, more JDK-internals-sensitive path than an ordinary bean test. A large fraction of the 37 FAILs sit squarely in that machinery:

| class | found/succ/fail | ms |
|---|---:|---:|
| `beans.factory.aot.BeanDefinitionMethodGeneratorTests` | 34/3/31 | 5486 |
| `beans.factory.aot.BeanDefinitionPropertiesCodeGeneratorTests` | 47/0/47 | 4266 |
| `beans.factory.aot.BeanDefinitionPropertyValueCodeGeneratorDelegatesTests` | 44/0/44 | 3249 |
| `beans.factory.aot.BeanRegistrationsAotContributionTests` | 14/8/6 | 127692 |
| `beans.factory.aot.CodeWarningsTests` | 23/17/6 | 1561 |
| `beans.factory.aot.InstanceSupplierCodeGeneratorKotlinTests` | 4/1/3 | 2131 |
| `beans.factory.aot.InstanceSupplierCodeGeneratorTests` | 26/0/24 (2 abort) | 3755 |
| `beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests` | 14/1/13 | 7411 |
| `context.annotation.CommonAnnotationBeanRegistrationAotContributionTests` | 8/1/7 | 8320 |
| `context.annotation.ConfigurationClassPostProcessorAotContributionTests` | 20/8/12 | 15846 |
| `context.aot.ApplicationContextAotGeneratorTests` | 40/3/37 | 66152 |
| `context.index.processor.CandidateComponentsIndexerTests` | 24/0/24 | 2885 |
| `orm.jpa.persistenceunit.PersistenceManagedTypesBeanRegistrationAotProcessorTests` | 2/1/1 | 2707 |
| `orm.jpa.support.InjectionCodeGeneratorTests` | 10/2/8 | 1614 |
| `orm.jpa.support.PersistenceAnnotationBeanPostProcessorAotContributionTests` | 8/2/6 | 29207 |
| `test.context.aot.AotIntegrationTests` | 4/0/2 (2 abort) | 29885 |
| `test.context.aot.TestClassScannerTests` | 7/6/1 | 79034 |
| `test.context.aot.TestContextAotGeneratorIntegrationTests` | 4/0/4 | 56165 |
| `core.test.tools.CompiledTests` | 14/5/9 | 1088 |
| `core.test.tools.TestCompilerTests` | 22/2/20 | 1707 |
| `web.service.registry.GroupsMetadataValueDelegateTests` | 8/0/8 | 5421 |
| `web.service.registry.HttpServiceProxyRegistrationAotProcessorTests` | 5/3/2 | 12675 |
| `web.service.registry.ImportHttpServiceRegistrarTests` | 5/3/2 | 11285 |

That's 22 of the 37 FAIL rows. `core.test.tools.CompiledTests`/`TestCompilerTests` test the compile-and-load step itself, not a specific bean feature — their presence in this cluster is a strong hint the shared mechanism is at the "compile/load the generated code" layer, not in any individual AOT contributor. **Not yet root-caused.** Worth a single-class repro on `core.test.tools.TestCompilerTests` (the most generic of the group, 20/22 sub-tests failing) before chasing the other 21 individually.

## `TestCompilerTests`/`CandidateComponentsIndexerTests` — root-caused on Windows, six carrier-sharing/stub bugs deep; a NEW perf cliff sits behind them

**Platform note**: this census's own header says it was measured on Azure Linux. The investigation below ran on a Windows worktree (`C:\craton\cratonvm\.claude\worktrees\jdk-only-known-issues-34809f`), because that is where this session executed — several of the bugs found and fixed are Windows-only (`sun/nio/fs/WindowsPath`; Unix's `UnixPath` never had the analogous defect, since it already stored/scanned `/` uniformly). The FAIL/ms numbers in this doc's tables are from the Linux run and are **not directly comparable** to anything measured in this section; every comparison below is against this SAME worktree's own earlier binaries, never against the Linux census numbers.

Six real bugs stacked on top of each other blocked in-process `javac` compilation (the mechanism `TestCompiler`/`CandidateComponentsIndexer`/the whole AOT cluster above shares) from ever reaching real work, each masking the next:

1. `sun/nio/fs/WindowsFileSystemProvider` minted as the SAME concrete class for the real, jar, and jrt providers (carrier-sharing) — `getScheme`/`newFileSystem(URI,Map)` dispatched to the wrong provider's real bytecode. Fixed: un-retired those two rows in `native-api/src/retired_shadow.rs`'s `RETIRED_SHADOW_L4_WINFS_TRIPLES` (23→17 rows) once the carrier-sharing root cause was understood.
2. Synthetic jar/jrt `FileSystem`s had no `defaultDirectory()`/`defaultRoot()` natives. Fixed: added both to `native-builtins/src/phases_late/nio_file.rs`.
3. Same carrier-sharing defect for `newDirectoryStream`/`readAttributes(Class)`/`newByteChannel`/`WindowsDirectoryStream.close`+`iterator`. Fixed: further un-retirement in `retired_shadow.rs` (`RETIRED_SHADOW_L4_FILES2_TRIPLES` 57→55 rows).
4. jar/jrt `Path` objects never had `root`/`kind`/`offsets` populated — real `WindowsPath.getFileName()` NPE'd on a null `root`. Fixed in `p57_write_path_fields` (`nio_file.rs`): a `RELATIVE`/`""` fallback for the vfs case.
5. `native_system_module_reader_list` (`native-builtins/src/jboss_jdkspecific.rs`) was an unconditional-empty stub (a deliberate 2026-07-08 choice, "avoid synthetic system module reader scan failures") — `javac`'s own `Locations$SystemModulesLocationHandler` calls exactly this to read `java.lang`'s platform classes, and an empty answer is indistinguishable from "the package doesn't exist" (`compiler.err.no.java.lang`). Fixed: it now answers from the real jimage via the new `jrt_module_resource_list` (`nio_file.rs`).
6. Once (5) let real module data flow, `javac`'s OWN module-name index (built via `path.getFileName().toString()` over a `Files.newDirectoryStream(jrtRoot)` listing) got the WHOLE sentinel-encoded path string back as "the name", not `"java.base"`. Root cause, found via `javap -c` on real `WindowsPath`: `getFileName()`/`getParent()` do not consult the `offsets` cache at all — both scan `this.path` directly for a literal `'\\'` via `String.lastIndexOf(92)` (already documented in `retired_shadow.rs`'s `RETIRED_SHADOW_L4_WINDOWSPATH_TRIPLES`, which retired exactly this pair on exactly this finding — but only for the REAL-path case, since a jar/jrt sentinel string was deliberately left in this VM's internal `/` form, so that scan always found zero `\` and returned the entire string). Fixed: `p57_write_path_fields` now stores a vfs Path's Java-visible string with `/` converted to `\` (Windows only), matching what makes the real scan land on the right boundary; `vfs_decode` (`nio_file.rs`) undoes the conversion on every string it reads back, so every one of its dozen-plus Rust-side callers (`jarfs_decode`/`jrtfs_decode`) is unaffected.

All six fixes were verified empirically (standalone `Repro*.java` probes against `FileSystems.getFileSystem(URI.create("jrt:/"))`, `ModuleFinder.ofSystem()`, and `StandardJavaFileManager.listLocationsForModules`) — `getFileName()` on a jrt module directory entry now returns the clean module name (`"java.base"`), and `listLocationsForModules(SYSTEM_MODULES)` sees all 70 modules by name where it previously saw none.

**New finding — a performance cliff, not a correctness bug**: with all six fixes in, `TestCompilerTests` no longer fails fast (~1.7s on this worktree's earliest binary, matching this doc's Linux number) — it now runs past ten minutes without finishing (measured: two independent 600s-capped runs via `run-suite.sh`, both timed out; a bisection against this worktree's own binary chain (`apps/spring-suite-runner/frozen/vm-spring-hotspot-identical-0{1,5,6,7}.exe`, all reproducible from this branch's history) confirmed binaries 01/05/06 (bugs 1–5 fixed, bug 6 still broken or absent) all complete `TestCompilerTests` in 18–34s with the SAME `FAIL 22/2/20` signature as the earliest binary; only binary 07 (bug 6's fix) hangs). Bugs 1–6 ALL had to be fixed before in-process `javac` could get past its early "Unable to find package java.lang in platform classes" failure at all — so binaries 01–06 were failing fast for the same reason as the original census, never reaching real compilation. Binary 07 is the first to actually attempt real work.

### Follow-up profiling session (same day) — narrowed but not yet root-caused

A dedicated profiling pass ruled out every cheap explanation and narrowed the cost to one real `javac` method, using two tools already in this VM that are worth knowing about for the next session:

- **`--stack-sample-ms=<ms>`**: samples every interpreter thread's Java frame chain to stderr on an interval, no abort, doesn't need the process to exit — the right tool for a hang, unlike `CRATONVM_PROFILE_SAMPLE_MS` (`vm/src/runtime/exec_sampler.rs`), which only reports at a clean exit `report_at_exit()` never reaches on a killed process. Pair with `--nojit` for a full interpreted stack (JIT-compiled frames aren't walked) or read a `note=... active JIT call(s)` line for how much of the depth is opaque.
- **`--stack-dump-on-timeout=<sec>`** and **`--verbose:gc`** are the adjacent tools (one-shot dump on a watchdog abort; GC pause logging) — both checked this round, see below.

**What the profile shows.** 75 one-second samples (`--nojit`) against `TestCompilerTests`'s classpath: **31/75 (41%) sit inside `com.sun.tools.javac.comp.Modules.retrieveRequiresTransitive`**, and that method's frame appears **59–68 times nested in a single stack** (`grep -c` per dump). Read chronologically, that nesting count **falls steadily** — roughly one level drained every 4–6 seconds (68→59 over 63 samples in the `--nojit` run; 63→47 over 64 samples in a JIT-on run with `--stack-sample-ms`) — so this is not a livelock spinning on one frame; it is real, if glacially slow, forward progress through a real call chain. At ~68 levels × ~5s/level, ONE such chain costs on the order of 5–6 minutes.

**What was ruled out, each via a standalone repro run against both HotSpot and this worktree's binary (all deleted after use — `MemoRepro.java`/`IndyRepro.java`/`JrtWalkRepro.java`, reproducible from this paragraph if needed again):**

- **Not HashMap identity-hash/memoization.** `retrieveRequiresTransitive`'s own bytecode (`javap -c` on real `Modules`) does `requiresTransitiveCache.get(msym)` but never `.put()` in this method — the cache is populated by its caller. A repro mimicking the exact shape (diamond fan-in: 60 nodes × 20 shared mid-nodes × 1 shared base, `HashMap<Object,Set<Object>>` memoized) gave **identical call counts (1280) and cache size (81) on CratonVM and HotSpot**, 42ms vs 2ms — memoization works correctly; general interpreter overhead, not a caching bug.
- **Not `invokedynamic`/lambda-metafactory relinking.** `retrieveRequiresTransitive` calls `Assert.checkNonNull(Object, Supplier<String>)` via a method-reference lambda at every visited node (offset 113, `invokedynamic ... get:...Supplier`). A repro calling the same pattern ~4970 times through 70×70-deep recursion (matching the measured 4026 real calls) ran in **11ms on CratonVM vs 15ms on HotSpot** — call-site caching is not being defeated.
- **Not GC.** `--verbose:gc` printed **zero GC lines** in a 40s window that included several minutes of the slow chain (by wall-clock proportion) — no collection has run yet; this workload isn't allocating enough to trigger one in this timeframe, so GC pause cost is not in the picture.
- **Not redundant native jrt reads.** A call counter added to `jrtfs_read` (`native-builtins/src/phases_late/nio_file.rs`, guarded by `CRATONVM_DBG_JRT_READ_COUNT=1`, left in place — off by default) recorded **zero calls** in both a 45s dev-build run and a 60s release-build run. Whatever `Symbol$ModuleSymbol.complete()` (called once per visited node, bytecode offset 90) is doing, it is not reading jimage file bytes through this VM's own file-read path in the observed window — the module graph's `requires` data is apparently already available without a fresh jimage read here.
- **Not the `Files.walkFileTree` `maxDepth` bug** a concurrent session found and fixed for jar archives the same day (`docs/internal/fixed-suite-bugs/hibernate/storedproc-resultmapping-javac-walk-maxdepth-jit-20260723-FIXED.md`, and see the cross-reference below) — a direct repro of `Files.walkFileTree(jrtModulesPath/java.base, Set.of(), 1, visitor)` with `SKIP_SUBTREE` on the root correctly visited only 8 entries in 170ms; `maxDepth` is honoured for jrt paths too.
- **`RegularEnumSet.contains`** (11/75 leaf samples, called from `Directive$RequiresDirective.isTransitive()`) has no native override in this codebase (checked `native-collections/src/lib.rs`, `native-builtins/src/phases_early.rs` — all `RegularEnumSet` hits there are state-inspection/reflection support, not a `contains` shadow), so it is running real bitmask bytecode; its sample share is call-count attribution, not a slow implementation.

**What is confirmed, not yet explained:** the AVERAGE per-call cost inside this chain is far higher than any of the above would predict — roughly 75ms/call by (68 levels × ~5s) / (~4026 total `retrieveRequiresTransitive` occurrences across the whole run), when a HashMap-memoized, EnumSet-checking, iterator-walking method over ~70 small objects should cost low-single-digit microseconds even in a naive interpreter. The likely remaining candidate, not yet tested: `Symbol$ModuleSymbol.complete()`'s own completer-guard (`if (completer != NULL_COMPLETER) { run once; reset }`) not actually being idempotent in this VM for repeated calls on the same symbol — which would make the SAME module's real (non-file-read, so probably `SystemModules`-baked in-memory) directive population redo real work on every visit instead of no-op'ing after the first, compounding once per nested level. Testing this needs either a native-side call counter on whatever backs `ModuleDescriptor`/`ModuleElement` completion (not yet located) or a proper sampling profiler attached to a live process, neither available in this session's environment (no `perf`, no elevated `wpr`, per `exec_sampler.rs`'s own module doc).

**Also confirmed:** a 20-minute (1200s) run in the ACTUAL failing mode (`--jit on`, no `--nojit`, matching `run-suite.sh`'s `jit-real`) still did not finish (`timeout` killed it, exit 124) — so this is not "just needs a longer cap" the way several Tomcat/H2 TIMEOUTs turned out to be; it is a genuine, severe per-compile-invocation cost. Since `TestCompiler`/`ToolProvider.getSystemJavaCompiler().getTask(...)` creates a fresh `Context`/`Symtab` per call (real `javac` architecture — not cached across compilations), and `TestCompilerTests` has 22 sub-tests each presumably issuing its own compile, this ~5-minute-or-worse cost likely multiplies by up to 22 within one JVM process, which would explain why even 20 minutes only gets partway through.

**Cross-reference — a related, distinct finding from a concurrent session the same day:** `docs/known-issues/hibernate/h2-inprocess-javac-jrt-modules-listing-gap-20260922.md` hits the identical mechanism (`Locations$SystemModulesLocationHandler.initSystemModules`) from the Hibernate/H2 side and describes two OTHER bugs in the same area: a jar/jrt `DirectoryStream` carrier-sharing defect (their §2, independently discovered and fixed there — overlapping with, but implemented differently from, this doc's bug 3 above; both landed on `dev` the same day and merged without conflict, confirmed by a post-merge `cargo test -p cratonvm-native-api --lib retired_shadow`, 128/128) and a `WindowsPath.getAbsolutePath()` → `FileSystem.defaultDirectory()` `NoSuchMethodError` on the jrt `FileSystem` singleton (their §3, explicitly left unfixed there). This doc's bug 2 (`defaultDirectory`/`defaultRoot` natives registered directly on `java/nio/file/FileSystem`, the exact abstract class the jrt singleton is minted as) likely already closes their §3 as a side effect — this session's own repros never hit that `NoSuchMethodError` — but that overlap has not been explicitly cross-verified against their `JavacProbe.java` repro.

**Not yet root-caused.** `TestCompilerTests`/`CandidateComponentsIndexerTests` (and plausibly several of the other 20 AOT-cluster classes above that share the same compile-in-process mechanism) cannot be expected to complete in bounded time until the `retrieveRequiresTransitive`/`Symbol.complete()` per-call cost is understood. Next steps for whoever picks this up: (1) find and instrument whatever backs `ModuleElement`/`ModuleDescriptor` completion for a system module (not `jrtfs_read` — confirmed unused here) with a call counter, same pattern as `CRATONVM_DBG_JRT_READ_COUNT`; (2) if that's flat too, the cost may be generic interpreter dispatch overhead for this bytecode shape (many small virtual calls, `List`/iterator churn) rather than anything module-specific, in which case the fix is architectural (JIT tier-up admission for `Symbol.complete()`-adjacent methods, the same angle the Hibernate `storedproc` fix above used for a narrower case) rather than a targeted native patch.

A broader regression sample (`run-suite.sh run --category all --start 0 --count 40`, integration-tests + spring-aop modules, 525s wall) after landing bugs 1–6 showed no new failures: `OK=35 FAIL=3 EMPTY=2`, and all 3 FAILs are the SAME already-tracked `Path.getFileSystem()` NPE described in the next section, unrelated to this fix chain. `PathMatchingResourcePatternResolverTests` itself improved against this worktree's own pre-fix binary (9/22 sub-tests passing → 13/22), though the remaining 9 failures are that same NPE.

## `PathMatchingResourcePatternResolverTests` — likely the SAME bug as Spring Boot's `Path.getFileSystem()` NPE

```
org.springframework.core.io.support.PathMatchingResourcePatternResolverTests  FAIL  22/19/3  13191ms
```

This exact class was named directly in this session's Spring Boot investigation as one of the things `Path.getFileSystem()` returning null under `--jdk-only` breaks (`docs/known-issues/springboot/nonpassed-classbyclass-census-20260922.md`, already being fixed as background task `task_568916e9`). Given the class name match and that this is precisely the resource-scanning code the Spring Boot bug describes, **this is very likely the same defect surfacing in the framework's own test suite**, not a new one. Worth confirming once that fix lands — if this class doesn't also go green, that's the signal it's a second, different bug wearing the same class name.

## Other FAILs, not yet clustered

| class | found/succ/fail | ms |
|---|---:|---:|
| `aop.scope.ScopedProxyBeanRegistrationAotProcessorTests` | 5/1/4 | 2199 |
| `aot.nativex.FileNativeConfigurationWriterTests` | 9/3/6 | 505 |
| `cache.jcache.JCacheEhCacheAnnotationTests` | 68/65/2 (1 skip) | 43877 |
| `context.annotation.ComponentScanAnnotationIntegrationTests` | 25/10/15 | 9231 |
| `context.annotation.ComponentScanParserBeanDefinitionDefaultsTests` | 12/0/12 | 7588 |
| `context.annotation.ComponentScanParserTests` | 8/1/7 | 6843 |
| `context.annotation.InitDestroyMethodLifecycleTests` | 11/9/2 | 8447 |
| `jdbc.datasource.init.H2DatabasePopulatorTests` | 16/15/1 | 3570 |
| `messaging.rsocket.RSocketClientToServerCoroutinesIntegrationTests` | 9/5/4 | 24741 |
| `util.FileSystemUtilsTests` | 2/1/1 | 286 |
| `web.client.RestTemplateIntegrationTests` | 125/121/1 (3 skip) | 201003 |
| `web.client.support.RestClientProxyRegistryIntegrationTests` | 5/4/1 | 3504 |
| `web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests` | 160/79/81 | 330206 |

`RequestMappingMessageConversionIntegrationTests` stands out — 81 of 160 sub-tests failed, by far the highest fail-count of anything in this census. Worth investigating first among this group, purely by blast radius.

## TIMEOUT — 1

`org.springframework.web.client.RestClientIntegrationTests` — killed at 180000ms (180s, this suite's per-class cap). Not yet checked against a longer cap to see whether it's a real hang or just a slow class, the same distinction the Tomcat/H2 pages above had to make repeatedly this session.

## Open items, in priority order

1. **The `aop.target` LOADERR cluster (9 classes) is almost certainly one classpath defect**, not nine — cheapest item on this page to close out.
2. **The AOT/code-generation cluster (22 of 37 FAILs) is almost certainly one or a few shared mechanisms**, not 22 independent bugs — start with `TestCompilerTests`.
3. **Cross-check `PathMatchingResourcePatternResolverTests` against the Spring Boot `Path.getFileSystem()` fix** (`task_568916e9`) once that lands — do not re-investigate it independently until then.
4. `RequestMappingMessageConversionIntegrationTests` (81/160 failed) is the single highest-blast-radius undiagnosed row.
5. Confirm whether `RestClientIntegrationTests`'s TIMEOUT is a real hang or just needs more than 180s, the way several Tomcat/H2 HANGs this session turned out to just need a wider cap.
