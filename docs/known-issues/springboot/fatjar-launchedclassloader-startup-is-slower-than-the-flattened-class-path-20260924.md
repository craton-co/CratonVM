# A Spring Boot fat jar starts 2.7x slower through the real `LaunchedClassLoader` than it did from the flattened class path — 2026-09-24

**Status: open, `--jdk-only` only. Performance, not correctness.** Opened by
`fatjar-classes-load-from-the-vms-flattened-class-path-not-launchedclassloader-FIXED-20260924.md`.
That fix made the application class path of `java -jar app.jar` the jar's root, as
on HotSpot, so Spring's `LaunchedClassLoader` now loads every application
class. The run's output is now HotSpot's. Its startup time is not.

## Numbers

`simple-fat.jar` is the Spring Boot `smoke-test` simple sample packaged as a fat
jar, with 36 nested-jar URLs. Azure linux host, JDK 25.0.4+7, idle (load
average 0.1), one fresh process per row, three runs each, interleaved:

| run | wall |
|---|---:|
| HotSpot 25 | 0.81–0.84 s |
| CratonVM, flattened class path (the build before the fix) | 4.38–4.44 s |
| CratonVM, `LaunchedClassLoader` (the fix) | 12.05–12.15 s |
| the same, `-Xint`, under light load | 25.0 s |

`--compatible` still flattens and is unchanged.

## Where the time goes

This is HotSpot's algorithm, not an extra one. `LaunchedClassLoader` is a
`URLClassLoader` over 36 `jar:nested:` URLs. Every class it loads walks
`URLClassPath`'s generic `Loader`s in order. Each step builds a `URL`, calls
`openConnection()` and asks Spring's `JarUrlConnection` whether the entry
exists. The flattened class path answered the same class from one hash
lookup in Rust. HotSpot does all of that walk and still finishes in 0.9 s.

Native-call census of the whole run (`--dump-native-registry`, `invocations`):
3,355,552 calls against 777,837 before the fix. The rows that grew most:

| native | before | after |
|---|---:|---:|
| `String.regionMatches(ZILjava/lang/String;II)Z` | 6,254 | 434,375 |
| `Object.getClass()` | 44,389 | 367,688 |
| `ArraysSupport.mismatch([BI[BII)I` | 10,921 | 327,221 |
| `Reference.reachabilityFence` | 35,526 | 205,496 |
| `ArraysSupport.vectorizedHashCode` | 11,263 | 148,722 |
| `Class.getClassLoader()` | 2,481 | 131,581 |
| `ClassLoader.getPlatformClassLoader()` | 2,388 | 131,507 |
| `URL.getDefaultPort()` | 1,482 | 126,716 |
| `URL.openConnection()` | 120 | 61,213 |

61,213 `openConnection` calls is about 1,700 class lookups times the 36 URLs.

Execution-sampler self time (`CRATONVM_PROFILE_SAMPLE_MS=5`, `-Xint`, so that
JIT inlining does not move samples between methods):

| method | share |
|---|---:|
| `URLClassPath$Loader.getResource` | 18.3 % |
| `JarUrlConnection.open` (Spring) | 16.4 % |
| `HeapByteBuffer.<init>` | 11.4 % |
| `UrlJarFiles$Cache.putIfAbsent` (Spring) | 10.5 % |
| `URLClassLoader.defineClass` | 4.7 % |

The sampler takes its sample at the next safepoint poll. A native called
directly from these small methods is therefore billed to them. That fits
`URL.openConnection()`, a `Bridge` over the real `handler.openConnection(this)`,
and the `URL` constructors called in `Loader.getResource`.

Measured and ruled out: a user-defined loader is not slow in itself. A
`URLClassLoader` microbenchmark over plain `file:` jars shows no per-class
penalty against the application loader.

## What would close it

- **Measure `URL.openConnection()` under `--jdk-only`.** Every URL the VM builds
  now carries a handler, so the real body, `handler.openConnection(this)`,
  would be correct for `jar:nested:`. The native's own carriers for http and
  `jar:file:` are what keep it registered. Retiring it is a wider change than
  this page, and it needs the HTTP suites.
- **Find the callers behind the `getPlatformClassLoader` / `getClassLoader`
  pairs.** 131 k each, about two per `openConnection`. A
  `VM.isSystemDomainLoader(x.getClassLoader())`-shaped check on the lookup
  path is the likely source.
- **The general levers:** interpreter and JIT throughput on string and URL
  parsing, which is most of what is left.

The page closes when the table's third row is within about 1.5x of the second.
