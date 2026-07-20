# `PropertiesLauncherTests`-adjacent jar/classpath loading — broad regression, unrelated to `loader.path` doc, JIT perf branch suspected

**Status: OPEN — found 2026-07-20, bisection narrowed but not pinned to an
exact commit.**

## Symptom

Discovered while verifying
[`propertieslauncher-loader-path-ignored-wrong-app-launched-FIXED.md`](../../internal/springboot/propertieslauncher-loader-path-ignored-wrong-app-launched-FIXED.md)
(branch `fix/propertieslauncher-loaderpath-20260720`): that fix was verified
clean (32/32 `PropertiesLauncherTests` PASS, zero regressions across 29
sibling classes) against `dev` at commit `939f61817`. Re-verifying against
`dev` a short time later (commit `99a4c4108` and beyond, including the
current tip as of this writing) shows **15/32 `PropertiesLauncherTests`
failures** — most of them tests that were passing before, including (per a
clean side-by-side diff) `testUserSpecifiedJarPath`,
`testUserSpecifiedWildcardPath`, `testUserSpecifiedJarPathWithDot`,
`testUserSpecifiedRootOfJarPath`, `testUserSpecifiedRootOfJarPathWithDot`,
`testUserSpecifiedRootOfJarPathWithDotAndJarPrefix`,
`testUserSpecifiedJarFileWithNestedArchives`,
`testUserSpecifiedDirectoryContainingJarFileWithNestedArchives`,
`jarFilesPresentInBootInfLibsAndNotInClasspathIndexShouldBeAddedAfterBootInfClasses`,
`explodedJarShouldPreserveClasspathOrderWhenIndexPresent`,
`customClassLoaderAndExplodedJarAndShouldPreserveClasspathOrderWhenIndexPresent`,
plus the 4 tests the `propertieslauncher-loader-path-ignored-wrong-app-launched`
fix specifically targets (`testUserSpecifiedNestedJarPath`,
`testUserSpecifiedClassLoader`,
`classPathWithoutLoaderPathDefaultsToJarLauncherIncludes`,
`testUserSpecifiedClassPathOrder`).

**This is NOT caused by the `propertieslauncher-loader-path-ignored-wrong-app-launched`
fix.** Confirmed by building `dev` at `99a4c4108` (all the concurrent churn
between `939f61817` and `99a4c4108`, but WITHOUT that fix's 3-file diff
applied at all, so `PropertiesLauncherTests` is running its ORIGINAL,
pre-fix code) — it already shows 15/32 failures, not the original 4. That
fix's own correctness is separately, conclusively verified (see its doc);
its 4 target tests just happen to also be swept up by this broader,
unrelated regression when run against current `dev`.

## Bisection (manual, commit-by-commit rebuild+test — this repo's history is
heavily multi-branch/merge-commit-dense, so a plain `git bisect` was not
attempted)

| Commit | `PropertiesLauncherTests` result |
|---|---|
| `34bce2630` (reactor-netty-hang branch merge point) | 4/32 fail (original, expected) |
| `49d7834e9` (reactor-netty jar-URI/`FileSystemProvider.getPath` fix) | 4/32 fail — clean |
| `f9836e296` (CloudFoundry SSL/MockWebServer fix, on an independent branch line) | **15/32 fail** |
| `cf3a44e2a` (`f9836e296`'s own direct parent — `perf(jit): halve Binary Trees HotSpot gap`) | **15/32 fail** |
| `c2e358bce` (`cf3a44e2a`'s own direct parent — `merge: ObjectName key property accessor closure`) | **15/32 fail** |

`c2e358bce`'s own ancestry goes back into a large, old accumulated branch
(`perf/halfgap-residuals-20260718`, dated 2026-07-18) merged into `dev` via
`645f4e7e4`/`dd2d45fa1`. That branch is NOT an ancestor of the
`34bce2630`/`49d7834e9` line I confirmed clean (`git merge-base --is-ancestor`
returns false both ways), so the two "clean" and "broken" points sit on
genuinely divergent history — a real `git bisect --first-parent` or a
proper merge-base-aware bisection (not attempted here — would need many
more ~5-8min rebuild cycles) is needed to pin the exact commit.

**JIT hypothesis RULED OUT**: `--nojit` against current `dev` still shows
15/32 failures (identical to JIT-on), so despite `cf3a44e2a`/`c2e358bce`
sitting in the bisection range and the commit message mentioning JIT perf
work, this is **not** a JIT codegen correctness bug (rules out the
`reference_hot_op_helperization_trap` pattern from prior sessions). The
regression is in the interpreter or a native override path instead. The
`perf/halfgap-residuals-20260718` branch merge point is still the best
available bisection lead (see table above) but its actual root cause within
that branch — or possibly in the independently-merged CloudFoundry SSL/
MockWebServer cluster (`f9836e296`, `2c6f9aeaf`, `dfdb8bf2b`, `48df5752b`,
`fff26fbf5`, tested only at their combined merge point) — was not pinned
down further.

Re-tested against the absolute latest `dev` tip available at investigation
time (`a13afa980`, which also includes an unrelated JIT miscompilation fix
from a concurrent session — `a8165d607`, `com.sun.tools.javac.jvm.ClassReader.readClass`/
`ClassFinder.complete` — see `docs/known-issues/springboot/` AOT-cluster
docs): still 15/32 fail, so that fix does not address this regression.

## Suggested next steps

1. `--nojit` already ruled out a JIT codegen bug (see above) — do not
   re-investigate that angle without new evidence.
2. Individually bisect the CloudFoundry SSL/MockWebServer cluster commits
   (`f9836e296`, `2c6f9aeaf`, `dfdb8bf2b`, `48df5752b`, `fff26fbf5`) — only
   their combined merge point was tested here (as `f9836e296`, which is
   actually their oldest/bottom commit per the graph, already showing
   15/32 fail) — the true introduction point could be earlier still, in
   `f9836e296`'s own parent chain (`perf/halfgap-residuals-20260718`,
   dated 2026-07-18) which was NOT individually walked past `c2e358bce`.
3. A proper `git bisect` run (accepting the multi-hour cost of ~15-20 rebuild
   cycles given this repo's commit density) starting from `dd2d45fa1`
   (`perf/halfgap-residuals-20260718`'s own dev-merge point, since
   `34bce2630` is not an ancestor of that branch) would pin the exact commit.
4. Given `--nojit` doesn't help, a faster diagnostic than full bisection:
   add targeted `eprintln!`/trace instrumentation to whichever native
   handles `ClassLoader.loadClass`/`URLClassLoader` construction/
   `JarFileArchive.getClassPathUrls` (see
   [`propertieslauncher-loader-path-ignored-wrong-app-launched-FIXED.md`](../../internal/springboot/propertieslauncher-loader-path-ignored-wrong-app-launched-FIXED.md)
   for the relevant functions/files this investigation already mapped:
   `native-builtins/src/classloader.rs`, `classloader_real.rs`,
   `classloading/src/class_path.rs`, `native-builtins/src/phases_late.rs`)
   and compare live trace output between `34bce2630` (clean) and current
   `dev` (broken) for one of the newly-failing tests
   (`testUserSpecifiedJarPath` is a good, simple starting point — it was
   passing before and involves no nested-jar or custom-classloader
   complexity).

## Affected classes

`org.springframework.boot.loader.launch.PropertiesLauncherTests` (15 of 32,
see symptom section for full list) — likely other classes exercising
`JarFileArchive`/`ExplodedArchive`/`URLClassLoader` jar/nested-jar loading
too, not yet swept.
