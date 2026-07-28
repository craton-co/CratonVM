# `BeanDefinitionLoader` package-name sources — FIXED

**Status: CLOSED — 2026-07-28.** Retired from
`docs/known-issues/springboot/core-spring-boot-beandefinitionloader-package-scan-empty-20260723.md`.

## Symptom (as filed 2026-07-23)

```
java.lang.IllegalArgumentException: Invalid source 'org.springframework.boot.sampleconfig'
	at org.springframework.boot.BeanDefinitionLoader.load(BeanDefinitionLoader.java:204)
	at org.springframework.boot.SimpleMainTests.basePackageScan(SimpleMainTests.java:53)
```

Three test methods, all passing a **package name** as a
`SpringApplication.run(Object...)` source:

| Module | Class | Method | Failure |
|---|---|---|---|
| core/spring-boot | `org.springframework.boot.BeanDefinitionLoaderTests` | `loadPackageName` | `IllegalArgumentException: Invalid source '…'` |
| core/spring-boot | `org.springframework.boot.BeanDefinitionLoaderTests` | `loadPackageNameWithoutDot` | `BeanDefinitionStoreException` → `FileNotFoundException: class path resource [sampleconfig] cannot be opened` |
| core/spring-boot | `org.springframework.boot.SimpleMainTests` | `basePackageScan` | `IllegalArgumentException: Invalid source '…'` |

## The filed root cause was WRONG

The original doc hypothesised that CratonVM's classpath resource enumeration
did not honour the suite runner's manifest-only **pathing jar**, so
`PathMatchingResourcePatternResolver.getResources("<pkg>/*.class")` came back
empty. That is not what was happening, and the pathing jar is not involved at
all — a probe run under a hand-built pathing jar (same
`Created-By: IntelliJ IDEA` + `file:` URL manifest shape the runner emits)
returns the identical, correct resource set to a plain `-cp`, and matches
HotSpot URL for URL:

```
cl.getResources("org/springframework/boot/sampleconfig/")
  -> file:/…/build/classes/java/test/org/springframework/boot/sampleconfig/
pmrpr.getResources("org/springframework/boot/sampleconfig/*.class")
  -> MyComponent.class, MyNamedComponent.class, package-info.class
```

## The actual root cause

`ClassLoader.getDefinedPackage(String)` was a native stub returning
**unconditional `null`** (`native-builtins/src/lang_class.rs`,
`i2_classloader_get_defined_package`), installed so ByteBuddy's
`JavaDispatcher.<clinit>` would stop NPE-ing on the JDK's uninitialised
private `packages` map.

`BeanDefinitionLoader` consults that method twice, and an always-null answer
produces **both** observed failures directly:

* `findPackage(source)` (`BeanDefinitionLoader.java:259-281`) returns
  `getClass().getClassLoader().getDefinedPackage(source)` as its **final**
  result. Its resource-scan fallback exists only to *force the package to be
  defined* (`Class.forName(<pkg>.<first class file>)`) before that second
  lookup. With the lookup pinned to null, the fallback's success is discarded,
  `findPackage` returns null, and `load(CharSequence)` falls through to
  `throw new IllegalArgumentException("Invalid source '" + resolvedSource + "'")`
  — `loadPackageName` and `basePackageScan`.
* `isLoadCandidate(Resource)` uses `getDefinedPackage(path) == null` as its
  *only* guard for the dotless case: it is what stops a package **directory**
  named `sampleconfig` from being handed to the XML bean-definition reader.
  With null, `sampleconfig/` was parsed as an XML document — hence
  `IOException parsing XML document from class path resource [sampleconfig]`
  / `FileNotFoundException` in `loadPackageNameWithoutDot`.

Note the second bullet is why the doc saw *two different* exception shapes for
what is one cause.

**Fixed on dev by `2a44df5cc` ("fix spring boot core residual controls",
2026-07-23 20:46)**, which replaced the stub with a classpath-backed probe:
a package is reported as defined when `<pkg>/*.class` matches at least one
class file. The doc's failing run (`craton-rerun-20260723`) predates that
commit. `9a0c51a7d` (manifest `Class-Path:` expansion for ad hoc
classloaders, same day, 18:44) is what makes the probe work **through** the
pathing jar, so both commits are needed for the suite-runner configuration —
but the pathing jar was never the failing half.

## Residuals found and fixed here (2026-07-28)

`2a44df5cc` closed the cluster but left the replacement native diverging from
HotSpot in three measurable ways. Measured with a standalone probe against the
same classpath, HotSpot `jdk-25.0.3.9` vs CratonVM:

| Probe | HotSpot | CratonVM (before) | CratonVM (after) |
|---|---|---|---|
| `cl.getDefinedPackage(p) == cl.getDefinedPackage(p)` | `true` | **`false`** | `true` |
| `c.getPackage() == cl.getDefinedPackage(c.getPackageName())` | `true` | **`false`** | `true` |
| `cl.getDefinedPackages().length` (after one lookup) | `2` | **`0`** | `1` † |
| `new URLClassLoader("empty", new URL[0], null).getDefinedPackage(p)` | `null` | **non-null `Package`** | `null` |

† Still not byte-identical to HotSpot, and deliberately so: CratonVM has no
per-loader record of which packages a loader has *defined*, only of which ones
it has been asked about, so `getDefinedPackages()` reports the memo for that
loader — a subset. The contradiction that mattered (a `Package` from the
singular form that the plural form denied) is gone; closing the remaining gap
needs real define-time package tracking in the class-definition path, which is
out of scope here.

1. **No interning.** The JDK guarantees one `Package` instance per
   (defining loader, package name); both `Class.getPackage()` and
   `ClassLoader.getDefinedPackage()` hand back that same object. CratonVM
   synthesised a fresh `Package` on **every** call from either entry point, so
   `==` never held and an `IdentityHashMap` keyed on a `Package` silently
   grew a new entry per lookup. (`Package.equals`/`hashCode` overrides — added
   earlier for exactly this reason — masked the value-comparison half of it.)
   Fixed with a `(loader-namespace-id, package-name) -> Package` memo shared by
   both natives, holding JNI-global-ref handles so the GC keeps and remaps the
   entries. A memo entry created by the ClassLoader side is *thin* (name +
   module + `NULL_VERSION_INFO`, all it can build without a class mirror);
   `Class.getPackage()` **upgrades that same object in place** with the
   manifest attributes and `packageInfo` rather than replacing it, so identity
   survives and no `getImplementationVersion()` caller loses data. A
   fully-built entry is returned untouched, matching HotSpot's "first definer
   wins" — a same-package class from a different jar does not overwrite the
   first one's manifest attributes. Side benefit: `Class.getPackage()` no
   longer repeats up to six manifest lookups plus a `package-info` class load
   per call.
2. **`getDefinedPackages()` contradicted `getDefinedPackage()`** — it was a
   hardcoded empty array while the singular form returned non-null for the
   same loader. It now reports that loader's memo contents.
   `ClassLoader.getPackages()` deliberately stays unconditionally empty (split
   into `i2_classloader_get_packages_empty`): that override exists to stop the
   real JDK's stream-pipeline bytecode leaking a `ReferencePipeline$Head` into
   jboss-modules' `ConcurrentClassLoader.<clinit>`, which is a separate
   concern.
3. **The probe ignored the receiving loader.** `getDefinedPackage` does **not**
   delegate in the JDK, but CratonVM answered it from the VM-global classpath,
   so *every* loader — including a deliberately isolated one — claimed *every*
   package in the process. New
   `classloader::package_class_files_visible_to_loader` scopes the probe:
   built-in (bootstrap/platform/application) loaders keep the global answer
   because they *are* the global classpath; a loader with its own recorded URLs
   is probed against exactly those (already pathing-jar aware, via
   `loader_local_resource_urls`); a loader with a positively-recorded but empty
   URL set answers `null` like HotSpot; and anything whose URL view we cannot
   observe falls back to the previous global answer, so the change can only
   narrow over-reporting, never introduce a new miss.

Also fixed in passing: synthetic-JDK mode still registered its own
always-`null` `cl_get_defined_package`, so the two boot modes disagreed. It now
delegates to the real-JDK implementation.

Unchanged, and deliberately: `getNamedPackage(String,Module)` /
`definePackage(String,Module)` still synthesise a fresh `Package` per call.
They take a caller-supplied module that they overwrite, and their results are
popped by `postDefineClass`; interning them would risk cross-contaminating
module identity for no measured gain.

## Verification

Binary `cratonvm-sbpkgscan.exe`, worktree `CratonVM-sb-pkgscan-20260727`,
branch `fix/sb-beandefloader-pkgscan-20260727`, JDK
`jdk-25.0.3.9-hotspot`.

* Both doc classes pass in full under **both** classpath shapes (plain `-cp`
  and a suite-runner-style pathing jar), and match the HotSpot baseline
  exactly:

  | Class | CratonVM | HotSpot |
  |---|---|---|
  | `BeanDefinitionLoaderTests` | 13/13 | 13/13 |
  | `SimpleMainTests` | 5/5 | 5/5 |

  Repeated 3× through the pathing jar with identical results (no flake).
* Fidelity probe table above, re-measured after the fix.
* New unit tests in `native-builtins/src/lang_class.rs`:
  `i2_classloader_get_defined_package_returns_null_without_class_files`,
  `…_rejects_malformed_names`, `…_memo_is_identity_stable`,
  `i2_classloader_get_packages_stays_empty`.
* Regression sweep, A/B against a clean `origin/dev` build of the same base
  commit (`ccf774db3`, built in worktree `CratonVM-sb-pkgscan-base-20260727`
  as `cratonvm-sbpkgscan-base.exe`): 115 `core/spring-boot` test classes —
  every top-level `org.springframework.boot.*Tests`, plus every class whose
  name mentions ClassLoader / Package / Resource / Classpath / Reflect /
  Annotation / Logging / Origin / Jar. `Class.getPackage()` interning is the
  broad-blast-radius part of this change (it feeds every
  `getImplementationVersion()` caller), so the sweep was deliberately weighted
  toward that surface.

  | | baseline `origin/dev` | with this change |
  |---|---|---|
  | PASS | 113 | 113 |
  | FAIL | 1 | 1 |
  | NORESULT (hang, timed out at 8 min) | 1 | 1 |

  **Zero differences** — same status, same test counts, and the same five
  failing method names in the one FAIL class. Both non-PASS entries are
  pre-existing and untouched by this work:
  `SpringApplicationTests` (5/102 failing: two
  `…FailureCausesApplicationFailedEventToBePublished`, two
  `applicationListenerFromContextIsCalledWhenContextFailsRefresh…`, and
  `specificApplicationContextInitializer`'s `IllegalStateException: No generic
  type found for initializr of type … $$Lambda/0x80000680`), and
  `OriginTrackedYamlLoaderTests`, which hangs identically on both binaries.

Note on the test build: at the base commit (`ccf774db3`), three `ConnState`
initialisers in `native-builtins/src/http_url_connection.rs`'s test module were
missing the `truncated` field, which broke `cargo test -p
cratonvm-native-builtins` for the whole crate. It was fixed locally to run the
unit tests above, then dropped on integration — `dev` had landed the same fix
concurrently.
