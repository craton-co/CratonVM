# CratonVM — Configuration Reference

The `cratonvm` launcher accepts JVM-style flags. Some are HotSpot-compatible
(parsed before clap so the non-standard `-XX:+Foo` / `-agentlib:` spellings
work), some are clap-style long options.

```
cratonvm [OPTIONS] <CLASS_NAME> [ARGS...]
cratonvm [OPTIONS] --jar <FILE.jar> [ARGS...]
```

## Class loading

| Flag | Description | Default |
|------|-------------|---------|
| `--classpath <PATH>` / `-c <PATH>` / `--cp <PATH>` | Directories and JARs to search for `.class` files. Separator: `;` (Windows) or `:` (Unix). | `.` (current directory) |
| `--jar <FILE>` | Execute a JAR. Main class is read from `META-INF/MANIFEST.MF`. `-cp` is ignored when this is set. | — |
| `--Xbootclasspath <PATH>` | Override bootstrap classpath. | Auto-detected from `--java-home` |
| `--java-home <PATH>` | JDK installation for boot/ext classpath discovery and JMOD loading. | `JAVA_HOME` env var |
| `--synthetic-jdk` | Force synthetic (Rust-implemented) stdlib instead of loading real JDK classes from JMODs. Explicit opt-in to synthetic mode — only needed when overriding the detected default. | Auto: real JDK when `java.base.jmod` (or `lib/modules`) is found via `JAVA_HOME`/`CRATONVM_JAVA_HOME`/`java` on `PATH`; otherwise synthetic. See `detect_real_jdk` in `vm/src/config.rs`. |

## Heap and GC

| Flag | Description | Default |
|------|-------------|---------|
| `--Xmx <SIZE>` | Maximum heap size. Accepts `k`, `m`, `g` suffixes. | `256m` |
| `--verbose:gc` | Print GC activity to stderr. | Off |
| `-XX:+HeapDumpOnOutOfMemoryError` / `-XX:-...` | Write an HPROF dump on `OutOfMemoryError` before raising the Java exception. | Off (HotSpot-parity) |
| `-XX:HeapDumpPath=<PATH>` | Path for the HPROF dump. | `./java_pid<pid>.hprof` |

## Verification

| Flag | Description | Default |
|------|-------------|---------|
| `--noverify` | Skip bytecode verification (legacy alias for `-Xverify:none`). **Not recommended.** | Verify enabled |
| `--Xverify <MODE>` | Verification policy: `none`, `remote`, or `all`. `remote` (HotSpot default) verifies non-boot classes only. | `remote` |

## Class loading observability

| Flag | Description | Default |
|------|-------------|---------|
| `--verbose:class` | Print class loading trace to stderr. | Off |
| `--Xlog <SPEC>` | HotSpot-style unified logging (JEP 158/271). Example: `--Xlog gc*=info:stdout:time,level,tags`. Full spec parser at [`vm/src/runtime/unified_logging.rs`](../vm/src/runtime/unified_logging.rs). | Disabled |
| `--XX:AuditMissingNatives` | Log every ACC_NATIVE method invoked without a Rust implementation; printed on shutdown. | Off |
| `--dump-missing-natives <FILE>` | Dump the missing-natives audit to JSON. Implies `--XX:AuditMissingNatives`. | — |
| `--dump-missing-natives-grouped <FILE>` | Same as above, grouped by JDK module. | — |

## CDS / AOT (Project Leyden)

| Flag | Description | Default |
|------|-------------|---------|
| `--XX:SharedArchiveFile <PATH>` | CDS shared archive file. | — |
| `--Xshare <MODE>` | CDS mode: `off`, `on`, `auto`, `dump`. | `off` |
| `--XX:AOTMode <MODE>` | AOT compilation mode: `off`, `training`, `production` (JEPs 483/514/515). | `off` |
| `--XX:AOTCache <PATH>` | AOT cache file. Input in production mode, output in training mode. | — |
| `--XX:AOTCacheOutput <PATH>` | Override AOT cache output path (overrides `--XX:AOTCache` for writing in training). | — |

## JPMS (modules)

| Flag | Description |
|------|-------------|
| `--module-path <PATH>` / `-p <PATH>` | Directories and modular JARs to search for modules. |
| `--add-reads <MODULE=TARGET>` | Add a read edge between modules. May be repeated. |
| `--add-exports <MODULE/PKG=TARGET>` | Export a package to another module (or `ALL-UNNAMED`). May be repeated. |
| `--add-opens <MODULE/PKG=TARGET>` | Open a package for deep reflection. May be repeated. |
| `--add-modules <MODULE>` | Additional root modules to resolve. `ALL-MODULE-PATH` resolves everything on the module path. |

## Debug agents

| Flag | Description |
|------|-------------|
| `--jdwp-port <PORT>` | Start JDWP debug server on the given port (e.g. `5005`). |
| `--jdwp-suspend` | Suspend the VM at startup waiting for the debugger to attach (requires `--jdwp-port`). |
| `-agentlib:<spec>` | Native JVMTI agent (HotSpot syntax). |
| `-agentpath:<path>[=opts]` | Native JVMTI agent loaded from an absolute path. |
| `-javaagent:<jar>[=opts]` | Java agent JAR with a `Premain-Class` manifest header. |

## Native access (Panama FFI / `java.lang.foreign`)

| Flag | Description | Default |
|------|-------------|---------|
| `--enable-native-access=<module-list>` | Grant the named modules permission to call restricted methods in `java.lang.foreign` (Panama downcalls, upcall trampolines, library/symbol lookups). Comma-separated list of module names, plus the special token `ALL-UNNAMED` for the unnamed module (classpath code). May be repeated; entries from every occurrence accumulate. Mirrors HotSpot JEP 472. | Unnamed-module callers get a one-shot warning; named modules without a grant get `IllegalCallerException`. |

`PanamaAccessRegistry` (in [`native-builtins/src/panama.rs`](../native-builtins/src/panama.rs)) is the in-process source of truth. The launcher accumulates `--enable-native-access` values into it at startup via `panama_access_registry().enable_modules_csv(...)`. Every Panama host-call entry point (`DowncallHandle.invoke` / `invokeExact`, `Linker.upcallHandle`, `SymbolLookup.libraryLookup`, `SymbolLookup.loaderLookup`, `SymbolLookup.find`) consults the registry; denied callers get an `IllegalStateException` whose message starts with `"IllegalCallerException:"` (the dedicated runtime-error variant will be added in a follow-up).

Examples:

```
# Grant ALL classpath callers (matches HotSpot's legacy ergonomic).
cratonvm --enable-native-access=ALL-UNNAMED MyApp

# Grant a specific module, plus the unnamed module for testing.
cratonvm --enable-native-access=java.foreign \
         --enable-native-access=ALL-UNNAMED MyApp

# Equivalent — comma list within one flag.
cratonvm --enable-native-access=java.foreign,ALL-UNNAMED MyApp
```

**Deny-by-default rollout.** Today, an unnamed-module caller without an `--enable-native-access=ALL-UNNAMED` grant gets a one-shot stderr warning and proceeds, matching the OpenJDK 21 transition behavior. A future major CratonVM release will flip the unnamed-module default to **deny** (`IllegalCallerException`), tracking the JDK schedule (`enableNativeAccess` becoming mandatory). Pin your dependencies' module declarations now, or budget for an `--enable-native-access=ALL-UNNAMED` line in launcher scripts before that flip.

**SecurityManager interaction.** Even for granted modules, `SymbolLookup.libraryLookup(path, arena)` consults the installed `SecurityManager.checkLink(path)` before mapping a shared library — the same way `ProcessBuilder.start()` consults `checkExec` for command paths. A thrown `SecurityException` propagates to the Java caller; `setSecurityManager(null)` (the default) skips the consult.

## Container / cgroups

| Flag | Description | Default |
|------|-------------|---------|
| `--XX:-UseContainerSupport` | Disable cgroup memory/CPU limit awareness. | Container support enabled |

## Diagnostics / observability

| Flag | Description |
|------|-------------|
| `--stack-dump-on-timeout <SECONDS>` | Spawn a watchdog that dumps every interpreter thread's stack to stderr after the deadline and aborts the process. Used to diagnose silent hangs. Pass `0` to disable. A 120-second default is installed automatically; set `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1` to opt out. |

## System properties

`-D<key>=<value>` flags are extracted before clap parses arguments and
become Java system properties (`System.getProperty`).

```
cratonvm -Dfoo=bar -Dpath.separator=: MyApp
```

## Internal `VmConfig` defaults

These compile-time settings live in [`vm/src/config.rs`](../vm/src/config.rs)
and can be overridden by editing the `Default for VmConfig` impl:

| Setting | Value | Description |
|---------|-------|-------------|
| `max_heap_size` | 256 MB | `-Xmx` default |
| `initial_heap_size` | 16 MB | Initial committed heap |
| `max_stack_depth` | 1024 | Max call frames before `StackOverflowError`. Override via `RJ_MAX_STACK_DEPTH` env var (clamped to [64, 65536]). |
| `gc_algorithm` | `Generational` | Other choices in `GcAlgorithm` enum |
| `use_compressed_oops` | false | `-XX:+UseCompressedOops` |
| `use_compact_headers` | false | `-XX:+UseCompactObjectHeaders` |
| `use_synthetic_jdk` | true (library) / host-detected (launcher) | Library default (`VmConfig::default()`) keeps this `true` for hermetic tests. The `cratonvm` launcher calls `VmConfig::with_host_jdk_default()` which flips this to `false` whenever `detect_real_jdk()` finds `java.base.jmod` (or `lib/modules`) on the host. `--synthetic-jdk` forces synthetic. |
| `use_container_support` | true | Cgroup limits honoured by default |
| `xverify_mode` | `Remote` | See `XverifyMode` enum |
| `cds_mode` | `Off` | |
| `aot_mode` | `Off` | |
| `jit_aggressive_compilation` | false | When true, lifts blanket package bans in the JIT skip list |

## Environment variables

| Variable | Description |
|----------|-------------|
| `JAVA_HOME` | Default for `--java-home`. When the directory contains `jmods/java.base.jmod` (or `lib/modules`), the launcher boots from the real JDK; otherwise it falls back to synthetic stubs. |
| `CRATONVM_JAVA_HOME` | Overrides `JAVA_HOME` for the boot probe — set this when `JAVA_HOME` points at a cratonvm shim tree (Maven, Gradle) but the boot modules should come from a real JDK. |
| `RUST_MIN_STACK` | Minimum thread stack size. Set to `8388608` (8 MB) for deep-recursion tests. |
| `RUST_LOG` | Tracing log level (`trace`, `debug`, `info`, `warn`, `error`). Defaults to `WARN`. |
| `RJ_MAX_STACK_DEPTH` | Override `max_stack_depth` at startup (64–65536). |
| `CRATONVM_DISABLE_DEFAULT_WATCHDOG` | Set to `1` to disable the 120-second hang watchdog. |
| `CRATONVM_DEFAULT_WATCHDOG_SEC` | Override the default watchdog timeout. |
| `CRATONVM_EAGER_STREAMS` | Set to `1` to opt **out** of the lazy / short-circuiting synthetic `java.util.stream` pipeline and restore the legacy eager pipeline. Lazy mode is the **default** (keycloak-16 Part B): intermediate ops (`peek`/`map`/`filter`/`limit`/`skip`) defer instead of materialising, and short-circuit terminals (`findFirst`/`findAny`/`anyMatch`/`allMatch`/`noneMatch`) stop early — so `Stream.of(...).peek(p).findFirst()` runs `p` once, matching HotSpot. Eager-terminal results and exceptions are identical either way. |
| `CRATONVM_LAZY_STREAMS` | Explicit force-on for the lazy stream pipeline (it is already the default; this only matters to override `CRATONVM_EAGER_STREAMS`). |

### Behavior / experimental toggles

The table above lists the boot/path environment variables. The `CRATONVM_*`
namespace additionally carries a large set of behavior switches. The
user-facing ones — the knobs you might reasonably set when running an
application — are documented here. (Hundreds of `CRATONVM_DBG_*` /
`CRATONVM_DIAG_*` / `CRATONVM_TRACE_*` variables also exist; those are
**internal debug toggles**, not part of the supported surface — see the note at
the end of this section.) Each switch is read **once** at startup and cached;
changing it mid-run has no effect.

#### Real-vs-synthetic JDK gates

CratonVM can run real JDK bytecode or, for a few subsystems, fall back to a
Rust "synthetic" implementation. These flags pick which path a subsystem uses.
Most accept presence-as-on (set to any value to enable); the opt-out spellings
disable a default-on behavior.

| Variable | Description | Default |
|----------|-------------|---------|
| `CRATONVM_NO_STUBS` | Drop **every** `SyntheticStub` native at registration so calls fall through to real JDK bytecode (or a clear `NoSuchMethodError`) instead of a fake. Opt-in; surfaces real gaps as errors. `Intrinsic`/`Bridge` natives are unaffected. | Off (stubs present) |
| `CRATONVM_REAL_NET_SOCKETS` | Use the real `java.net` socket bytecode (the central registry drops the synthetic `java/net/Socket`/`ServerSocket` natives) instead of the synthetic socket layer. | Off (synthetic) |
| `CRATONVM_REAL_AQS` | Route `AbstractQueuedSynchronizer` / `ReentrantLock` etc. through real `java.util.concurrent` bytecode instead of the synthetic lock natives. | Off (synthetic) |
| `CRATONVM_REAL_ANNOTATIONS` | Annotation reflection uses real proxy-backed annotation objects by default; set to `0` to use the old synthetic representation. | On (real); opt out with `CRATONVM_REAL_ANNOTATIONS=0` or `CRATONVM_SYNTHETIC_ANNOTATIONS=1` |
| `CRATONVM_REAL_FORKJOINPOOL` | Drop the synthetic `ForkJoinPool` natives and run the real `java.util.concurrent` pool. **Experimental** (see Family A4 in known-issues). | Off (synthetic) |
| `CRATONVM_REAL_RAF` / `CRATONVM_SYNTHETIC_RAF` | Force real / synthetic `RandomAccessFile`. | Real (auto) |
| `CRATONVM_REAL_PROXY_SUPER` | Use the real `java.lang.reflect.Proxy` super-class path. Opt out to the synthetic experimental path with `=0`. | On (real) |
| `CRATONVM_EAGER_STREAMS` | Opt **out** of the lazy/short-circuiting synthetic `Stream` pipeline back to the legacy eager pipeline. (`CRATONVM_LAZY_STREAMS` force-enables lazy and wins if both are set.) | Lazy (HotSpot-faithful) |
| `CRATONVM_SYNTHETIC_*` (e.g. `CRATONVM_SYNTHETIC_AQS`, `_EC`, `_RSA`, `_RAF`, `_FILEWRITER`, `_QUARKUS_ARC`, `_SPRING_STARTUP`, …) | Per-subsystem **force-synthetic** opt-out switches: select the experimental Rust implementation for that one subsystem even when the real-JDK path is the default. | Off (real path) |

> The real path is the **default** wherever a real JDK is detected; the
> synthetic implementations are experimental opt-ins. Prefer leaving these
> unset unless you are reproducing a synthetic-vs-real difference.

#### JIT / GC tuning

| Variable | Description | Default |
|----------|-------------|---------|
| `CRATONVM_DISABLE_JIT` | Interpreter-only execution (the `--nojit` flag sets this). Useful for isolating whether a misbehaviour originates in the JIT. | Off (JIT on) |
| `CRATONVM_JIT_THRESHOLD` | Invocation count at which a method becomes JIT-eligible. Higher keeps short-lived code interpreted; `0` is clamped to `1`. | `500` |
| `CRATONVM_JIT_CODE_CACHE_MAX_MB` | Upper bound on total retained JIT code, in MiB; when reached, new methods stay interpreted. `0` disables the cap (unbounded). | (built-in cap) |
| `CRATONVM_DISABLE_INTRINSICS` | Prevent the interpreter from installing `Intrinsic` inline-cache entries, forcing ordinary native/bytecode dispatch (differential-test off-switch). | Off |
| `CRATONVM_NO_PRECISE_JIT_MAPS` | Opt **out** of precise JIT oop maps (revert to the older conservative stack scan). For diagnosing GC-root coverage under JIT. | Precise maps on |

#### Security / sandbox

These harden the VM for running untrusted bytecode or multi-tenant hosting.
All are **off by default** (the default posture is JDK-faithful single-tenant).

| Variable | Description | Default |
|----------|-------------|---------|
| `CRATONVM_CONFINE_IO` | Enable CWD file-I/O confinement (fail-closed): filesystem access is restricted to the working directory and explicitly registered sandbox roots. The certified hardening switch. | Off |
| `CRATONVM_UNTRUSTED_CODE` | Like `CRATONVM_CONFINE_IO` but warning-mode — auto-enables CWD confinement for untrusted-bytecode hosting. | Off |
| `CRATONVM_BLOCK_PRIVATE_NETS` | Additionally deny outbound connections to loopback and RFC1918 private ranges (the link-local cloud-metadata block always runs). | Off |
| `CRATONVM_RESOLVE_OUTBOUND_HOST` | Resolve outbound **hostnames** and apply the per-IP outbound policy to every resolved address (closes the DNS-alias / DNS-rebinding bypass). | Off (no DNS in policy) |
| `CRATONVM_REQUIRE_POLICY` | With a `SecurityManager` installed but no policy loaded, **deny** (fail-closed) instead of allow-all. Used by the certification profile. | Off (allow-all when no policy) |
| `CRATONVM_HARDEN_MANIFEST_CLASSPATH` | Drop JAR-manifest `Class-Path` entries that resolve outside the JAR's own directory (absolute roots, `file:/…`, `..` escapes). | Off |
| `CRATONVM_HTTP_MAX_BODY` | Max accepted HTTP request body, in bytes, for the built-in HTTP server; larger requests get a `413`. | `8 MiB` |
| `CRATONVM_ZIP_MAX_ENTRY_BYTES` | Per-entry uncompressed-size cap when inflating zip/jar entries (decompression-bomb guard). | `512 MiB` |
| `CRATONVM_MAX_INFLATED_BYTES` | Companion total-inflation cap for the zip/jar reader. | (built-in) |
| `CRATONVM_TRUST_PEM` | Path to a PEM trust bundle, consulted after the `javax.net.ssl.trustStore` sys-prop and before the JDK `cacerts`. | — |

#### Diagnostics / resource limits

| Variable | Description | Default |
|----------|-------------|---------|
| `CRATONVM_LOCK_ORDER_CHECK` | Opt in (release builds) to runtime lock-ordering deadlock detection. Truthy values: `1`/`true`/`yes`/`on`. Always on in debug builds. | Off (release) |
| `CRATONVM_RESOLVE_CACHE_CAP` | Capacity of the shared symbol-resolution cache (bounds the footprint against a key-minting adversary). Clamped to ≥ 1. | `65536` |
| `CRATONVM_ENABLE_ASSERTIONS` | Enable Java `assert` statement evaluation (the `-ea` analog) for the run. | Off |

> **Debug-only toggles.** Every `CRATONVM_DBG_*`, `CRATONVM_DIAG_*`,
> `CRATONVM_TRACE_*`, and `CRATONVM_*_DBG` variable is an **internal
> developer/debug switch** (tracing, GC stress, JIT bisection, etc.). They are
> not a supported configuration surface, may change or disappear without
> notice, and are intentionally not enumerated here. Discover them with
> `grep -rhoE "CRATONVM_[A-Z0-9_]+" --include=*.rs vm/ gc/ jit/ native-*` if you
> are working on the VM internals.
