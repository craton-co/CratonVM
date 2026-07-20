# `PropertiesLauncher` appears to ignore `loader.path`, launches the wrong app — FIXED

**Status: FIXED 2026-07-20.** Originally opened 2026-07-17 as
`docs/known-issues/springboot/propertieslauncher-loader-path-ignored-wrong-app-launched.md`;
retired here per the known-issues triage rule (all 11 originally-tracked
failures now fixed, no residual). Branch
`fix/propertieslauncher-loaderpath-20260720`.

## Original symptom (2026-07-17)

11 of `PropertiesLauncherTests`' 14 failures shared one signature: the test
sets `System.setProperty("loader.path", ...)` to point at a fixture jar whose
`main()` prints `"Hello World"`, launches via `PropertiesLauncher`, and
polls (`Awaitility`) for that output — but consistently observed
`"Hello Other World"` instead, the output of a *different* fixture jar the
test never pointed `loader.path` at, or a bare `ClassNotFoundException:
demo.Application` (the intended app class was never on the classpath the
launcher actually used).

8 of those 11 were fixed as a side effect of the unrelated classpath-URL
enumeration fix (`spring-boot-loader-classpath-url-enumeration-empty-cluster-FIXED.md`,
2026-07-19), leaving 4 residual failures re-measured that day:
`testUserSpecifiedNestedJarPath`, `testUserSpecifiedClassLoader`,
`classPathWithoutLoaderPathDefaultsToJarLauncherIncludes`, and
`testUserSpecifiedClassPathOrder`. This doc covers the root-cause and fix
for those final 4 (full class: 32/32 `PropertiesLauncherTests` PASS).

## Root causes (2 independent bugs, 3 fixes)

### Bug 1 — `extract_url_path`'s naive `.replace("/!", "!/")` corrupts directory-shaped nested URLs

`testUserSpecifiedNestedJarPath` and `classPathWithoutLoaderPathDefaultsToJarLauncherIncludes`
both resolve a classpath entry via Spring's
`JarUrl.create(file, "BOOT-INF/classes/")` — a nested location pointing at a
**directory** inside a jar (not a further nested jar), producing a URL spec
`jar:nested:<jar>/!BOOT-INF/classes/!/`. The `!/` after the entry name
(`BOOT-INF/classes/`) is a *second*, legitimate boundary marker (root of the
nested location); the entry name's own trailing `/` sits immediately before
it.

`native-builtins/src/classloader.rs::extract_url_path` normalizes this spec
with `.replace("/!", "!/")` — a **global** replace. The first `/!` (between
the outer jar and the entry name) is the intended target, but the entry
name's trailing `/` followed by the URL's own trailing `!/` ALSO forms a
`/!` match, so the global replace corrupts it too:
`BOOT-INF/classes/!/` → `BOOT-INF/classes!//` (the entry's trailing slash is
eaten). `classloading/src/class_path.rs::parse_jar_subdir_spec` then splits
on the first `!/` and uses everything after it as the nested-entry prefix —
`BOOT-INF/classes!//` never matches any real zip entry name, so the jar's
classpath entry silently contributed **zero** classes.

**Fix**: change `.replace("/!", "!/")` to `.replacen("/!", "!/", 1)` — only
the first (genuine) boundary marker is a normalization target.

A second, narrower fix was needed in `parse_jar_subdir_spec` for symmetry:
once `extract_url_path` correctly preserves `BOOT-INF/classes/!/` (entry
name intact, `.replacen` leaves the trailing `!/` alone), the prefix half of
`parse_jar_subdir_spec`'s `find("!/")` split still includes that trailing
`!/` verbatim (`BOOT-INF/classes/!/`) unless stripped. Added a
`.strip_suffix("!/")` on the split prefix.

### Bug 2 — real-JDK-mode `ClassLoader.loadClass` never delegates to a user-defined parent

`testUserSpecifiedClassLoader` and `testUserSpecifiedClassPathOrder` both use
`loader.classLoader=java.net.URLClassLoader`, which `PropertiesLauncher`
wraps around the real `LaunchedClassLoader`
(`new URLClassLoader(NO_URLS, parent=launchedClassLoader)` —
`wrapWithCustomClassLoader`). `Class.forName(mainClassName, false,
outerLoader)` then dispatches to the **base** `java.lang.ClassLoader.loadClass`
native, since plain `java.net.URLClassLoader` has no override of its own.

CratonVM has two independent, mode-gated implementations of that base
native: `classloader.rs::cl_load_class_base_delegation` (registered by
`register_classloader_natives`) and `classloader_real.rs::cl_real_load_class_base`
(registered by `register_classloader_real_natives`, which wins in real-JDK
mode — confirmed empirically via debug tracing that only the latter ever
ran). The real-JDK-mode implementation's "standard VM class loading" step
answers exclusively through CratonVM's flat global class store — which,
BY DESIGN, deliberately excludes a custom loader's own recorded URLs (so one
temporary loader's classes don't leak into another's namespace, per
`ucl_try_define_local_class`'s own doc comment). It never invoked a
user-defined **parent** loader's own `loadClass`, so the outer wrapper's
lookup fell straight from "not in the empty-by-design global store" to a
bare `ClassNotFoundException` — the parent's own jar(s) were never consulted
at all.

**Fix**: added a new delegation step (`classloader_real.rs::cl_real_load_class_base`,
step 0, before the global-store lookup) that invokes `parent.loadClass(name)`
via `ctx.invoke_virtual` whenever `parent` is a genuinely user-defined
loader, matching JVMS §5.3.2's parent-first order. Scoped to user-defined
parents only (builtin app/platform/bootstrap parents are unaffected and keep
the faster global-store path); a parent miss or exception is swallowed so
every existing fallback (global store, `findClass` override, deferred
resolution) still runs exactly as before.

A matching (but, per the empirical dispatch trace above, currently
dead-in-real-JDK-mode) bug was also fixed in the sibling synthetic-JDK-mode
implementation, `classloader.rs::classloader_parent`: it treated a `null`
by-name read of the real `"parent"` field as definitive "no parent," never
falling back to the numeric `CL_PARENT_REF` slot that ordinary constructors
(`cl_init_parent`, `ucl_setup`, ...) actually populate — the same class of
bug, fixed for correctness/symmetry even though the currently-live real-JDK
registration set (`classloader_real.rs`) already sets the named field
directly and isn't affected.

## Verification (2026-07-20)

Full `PropertiesLauncherTests` class: **32/32 PASS** (was 28/32).

Zero-regression check: ran all 29 sibling classes in the `launch`,
`net.protocol.jar`, `net.protocol.nested`, and `jar` packages against both a
clean baseline binary (git-stashed fixes) and the fixed binary. Every
failure count was **identical** between the two runs except
`PropertiesLauncherTests` (4 → 0); the pre-existing failures in
`LaunchedClassLoaderTests`, `LauncherTests`, `net.protocol.jar.HandlerTests`,
`JarUrlClassLoaderTests`, `JarUrlConnectionTests`, `UrlJarFilesTests`,
`jar.NestedJarFileTests`, `jar.SecurityInfoTests`, and
`MetaInfVersionsInfoTests` are unrelated, unchanged by this fix, and remain
open under their own tracking (see e.g.
[`project_securityinfo_jarsig_datainputstream_close_20260712`] for the
`SecurityInfoTests`/`NestedJarFileTests` residual).

## Affected classes (original)

| module | class |
|---|---|
| loader/spring-boot-loader | org.springframework.boot.loader.launch.PropertiesLauncherTests |

(4 residual tests fixed by this pass: `testUserSpecifiedNestedJarPath`,
`testUserSpecifiedClassLoader`, `classPathWithoutLoaderPathDefaultsToJarLauncherIncludes`,
`testUserSpecifiedClassPathOrder`.)
