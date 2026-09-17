# Quarkus: `IllegalStateException: SRCFG00017` in `ConfigLauncherSession` — the batch harness's real defect was a native fd leak, not a config-registration bug

**Status: FIXED (2026-09-17), with the original diagnosis corrected.** The
symptom this page named ("batch test launcher processes abort mid-run") is
fixed. The specific exception the page led with, `SRCFG00017`, was never
reproduced — see §3 for what to do if it resurfaces.

## 1. The original hypothesis, and why it doesn't hold up

The original page attributed this to `SmallRyeConfigProviderResolver`
persisting a registered `Config` for a `ClassLoader` key across
`LauncherSession`s that a batch harness (`CratonRunner <class>...`, one JUnit
Platform `LauncherSession` opened per class via `LauncherFactory.create()`'s
`SessionPerRequestLauncher`) opens and closes many times in one process.

That doesn't survive contact with the actual code:

* `javap` on `smallrye-config-core-3.18.1.jar`'s
  `SmallRyeConfigProviderResolver` shows `configsForClassLoader` is a
  **plain instance field** (`private final Map<ClassLoader, Config>
  configsForClassLoader`, a fresh `ConcurrentHashMap` per constructor call),
  not a static/shared table. `io.quarkus.test.config.ConfigLauncherSession
  .launcherSessionOpened` installs a **brand-new**
  `ThreadLocalConfigSourceProvider` (`extends
  SmallRyeConfigProviderResolver`) every time it runs, so every session's
  `registerConfig` writes into an empty map it alone owns — there is no
  shared state for two sessions to collide over.
* Confirmed directly: a minimal probe (`LauncherFactory.create()` +
  `execute()` twice in one process, printing the installed resolver's
  identity) shows two distinct `SmallRyeConfigProviderResolver` instances
  and zero exceptions, on both a real JDK 25 and this VM.
* A 40-class batch built from real Quarkus test classes
  (`io.quarkus.deployment.pkg.steps.*`, `io.quarkus.aesh.deployment.*`, …)
  run through `CratonRunner` under both a real JDK and this VM never raised
  `SRCFG00017` either — it raised something else (§2).

## 2. What actually aborts a multi-class batch run

Running that same 40-class batch through `CratonRunner` on the *unpatched*
VM reliably crashed at the 9th–10th class with:

```
java.io.FileNotFoundException: .../META-INF/services/org.junit.platform.launcher.LauncherSessionListener (Too many open files)
Exception in thread "main" java.util.ServiceConfigurationError: org.junit.platform.launcher.LauncherSessionListener: Error accessing configuration file
```

`/proc/<pid>/fd` at the moment of the crash showed **1023 of the process's
1024 available file descriptors** all pointing at classpath `.jar` files —
the real `ulimit -n`, not some internal accounting cap. The same 40-class
batch under a real JDK 25 held **4** file descriptors the entire time.

Root cause: this VM's real-JDK-mode `java.util.zip.ZipFile`/
`java.util.jar.JarFile` bridges never de-duplicated by path.
`native-io/src/zip_real_jar.rs::open_and_register` — which owns the plain
`ZipFile` path (the doc comment two functions over already noted `JarFile`
itself is intercepted later by `native-builtins`'s `register_p59_jar`,
i.e. a *different* registration wins for `JarFile`) — called `File::open`
and minted a fresh handle in `jar_table()` for **every** `new
ZipFile(path)`/`new JarFile(path)`, even a repeat open of a path already
open elsewhere, and never released it until that specific Java object's own
`close()`. A single ordinary JVM process touches a small, bounded set of
jars this way and never notices. `ServiceLoader.load()` — called once per
`LauncherSession` for five separate SPI families
(`LauncherSessionListener`, `TestEngine`, `PostDiscoveryFilter`,
`LauncherDiscoveryListener`, `TestExecutionListener`) — reads
`META-INF/services/*` out of *every* jar on the classpath, so a harness that
opens many sessions in one process (this repo's own
`apps/quarkus-suite-runner`'s ad hoc multi-class batch invocations, used to
re-verify NOSTART classes faster than one-class-per-fork) touches thousands
of distinct jars over the process's life and exhausted the descriptor table
well before finishing. Real JDK doesn't hit this because its own native zip
source cache (`zsrc`) de-duplicates by path exactly the way this bridge
didn't.

**Fix** (both are `open`-time, path-keyed, reference-counted-by-`Arc`
caches — no change in what gets read, only in whether re-opening the same
path costs a new descriptor):

* `native-io/src/zip_real_jar.rs`: `JarState.archive` is now a
  `SharedArchive` (`Arc<Mutex<zip::ZipArchive<File>>>`). `open_and_register`
  checks a new `shared_archive_cache()` (`HashMap<PathBuf,
  Weak<Mutex<ZipArchive<File>>>>`) before opening; a still-live entry for
  the same path is reused, so N `ZipFile`/`JarFile` objects for one path
  share one descriptor, and it closes once none of them (and no cache
  entry) still holds it — the `Weak` reference means a fully-closed path
  falls out of the cache on its own, no eviction pass needed.
* `native-builtins/src/phases_late/jar_manifest.rs`:
  `jar_entry_bytes_cached`'s companion `ARCHIVES` cache (used for
  `getInputStream` on a `JarFile`'s entries) had the same shape — path-keyed
  but never evicted — bounded it with a FIFO cap
  (`CRATONVM_JAR_ARCHIVE_CACHE_CAP`, default 256) so this sibling cache
  can't independently reach the same failure on a workload that reads
  entries from thousands of distinct jars via the real `JarFile` API.

## 3. Verification

* Full `cratonvm-native-io` (534 tests) and `cratonvm-native-builtins`
  (4280 tests) unit suites: unchanged, all green.
* The exact 40-class batch that crashed at class #9–10 pre-fix: ran clean
  past that point (verified through 20 classes with a 400s cap, and through
  12 with a full `@@BATCHEND`; `Too many open files` count: 0 in every
  post-fix run). `/proc/<pid>/fd` stayed in the tens instead of climbing
  past 1000.
* A larger soak batch (first 200 classes of `testlist.txt`, real Quarkus
  test classes across `io.quarkus.aesh.*`/`io.quarkus.aesh.ssh.*`/etc.)
  ran for its 900s cap and got through 45 classes before that cap ended it
  — 5x past the pre-fix crash point, zero `Too many open files`, zero
  `SRCFG00017`, `/proc/<pid>/fd` staying in the tens the entire time
  instead of climbing into the thousands.

## 4. If `SRCFG00017` shows up again

It was never reproduced here, across a 40-class real-Quarkus batch, a
200-class soak batch, and a hand-built two-session probe targeting the
exact code path — on this VM or on a real JDK 25 with the identical
harness. If it recurs, it is unlikely to be *this* mechanism (§1 rules out
the shared-map theory with `javap` and a passing probe); check first
whether the run also shows fd pressure or any other resource exhaustion
nearby, since a starved process can fail in whatever call happens to need a
resource next, not necessarily the same one each time.
