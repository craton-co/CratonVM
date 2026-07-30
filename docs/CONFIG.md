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
| `--java-home <PATH>` | JDK installation for boot/ext classpath discovery and JMOD loading. Does **not** select a mode — it only points the (already selected) real-JDK mode at a specific installation. | `CRATONVM_JAVA_HOME`, then `JAVA_HOME`, then `java` on `PATH` |
| `--real-jdk` | Load the real JDK class files from `jmods/` or `lib/modules` (~300 native methods in Rust). Already the default; pass it to be explicit. Fails loudly if no usable JDK is found. | **on** (`LAUNCHER_DEFAULT_JDK_MODE`) |
| `--synthetic-jdk` | Use the synthetic Rust standard library (~5,200 native stubs, no JDK needed) instead. Mutually exclusive with `--real-jdk`. Requires a build with the `synthetic-jdk` Cargo feature — otherwise the launch fails rather than starting a VM with no class library at all. | off |

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
| `use_synthetic_jdk` | true (library) / **false — real-JDK, always** (launcher) | **Never host-detected.** Library/embedding default (`VmConfig::default()`, `EMBEDDED_DEFAULT_JDK_MODE`) stays `true` so the in-tree suite is hermetic. The `cratonvm` launcher and the C embedding API use `VmConfig::for_launcher()` (`LAUNCHER_DEFAULT_JDK_MODE`), which is **real-JDK unconditionally**. Select the other mode with `--real-jdk` / `--synthetic-jdk` (mutually exclusive). If the selected mode is unavailable — no usable JDK for `--real-jdk`, or a build without the `synthetic-jdk` Cargo feature for `--synthetic-jdk` — the launch is a **hard error** naming everything searched; there is no silent fallback to the other class library. |
| `use_container_support` | true | Cgroup limits honoured by default |
| `xverify_mode` | `Remote` | See `XverifyMode` enum |
| `cds_mode` | `Off` | |
| `aot_mode` | `Off` | |
| `jit_aggressive_compilation` | false | When true, lifts blanket package bans in the JIT skip list |


## Environment variables

Everything the VM reads from the environment is one of **fifteen** variables:
ten grouped ones that take a comma-separated token list, and five scalars.

```sh
CRATONVM_JIT=-bce,unroll,threshold=200
CRATONVM_DBG=gc-stress=65536,loader-trace
CRATONVM_REAL=aqs,-agroal
```

| Form | Meaning |
| --- | --- |
| `token` or `+token` | switch it **on** |
| `-token` | switch it **off** |
| `token=value` | switch it on with a value |
| `all` | every token in that group |

A token the group does not define prints one line on stderr and is otherwise
ignored. [`docs/flag-tokens.md`](flag-tokens.md) lists every token in every
group; the tables below cover the ones worth setting when *running* an
application rather than debugging the VM.

### The ten grouped variables

| Variable | Covers | Tokens |
| --- | --- | ---: |
| `CRATONVM_DBG` | tracing, dumps, GC stress, JIT bisection — nothing here can change a program's result | 342 |
| `CRATONVM_JIT` | compiler passes, tiering, deopt, precise oop maps, shadow stack | 96 |
| `CRATONVM_REAL` | real JDK bytecode vs synthetic Rust shim, per subsystem | 25 |
| `CRATONVM_GC` | collector selection, heap sizing, barriers, object layout | 22 |
| `CRATONVM_THREADS` | threading, async handoff, watchdog, lock-order checking | 13 |
| `CRATONVM_LOADER` | class loading, resolution, verification | 10 |
| `CRATONVM_IO` | files, sockets, HTTP, zip | 9 |
| `CRATONVM_TEST` | soak / difftest harness; never set in production | 9 |
| `CRATONVM_COMPAT` | per-application workarounds (JBoss, Spring, Quarkus) | 8 |
| `CRATONVM_SECURITY` | sandboxing, trust anchors, policy | 7 |

### The five scalars

These are a path, a location or a master switch, not a knob with an on/off
sense, so they keep their own name.

| Variable | Description |
|----------|-------------|
| `CRATONVM_JAVA_HOME` | Overrides `JAVA_HOME` for the boot probe — set this when `JAVA_HOME` points at a cratonvm shim tree (Maven, Gradle) but the boot modules should come from a real JDK. |
| `CRATONVM_BIN` | Path to the `cratonvm` binary, for harnesses that re-exec it. |
| `CRATONVM_MAVEN_REPO_LOCAL` | Local Maven repository root. |
| `CRATONVM_ENABLE_ASSERTIONS` | Enable Java `assert` statement evaluation (the `-ea` analog) for the run. |
| `CRATONVM_DISABLE_JIT` | Interpreter-only execution (the `--nojit` flag sets this). Useful for isolating whether a misbehaviour originates in the JIT. |

### Not `CRATONVM_*`

| Variable | Description |
|----------|-------------|
| `JAVA_HOME` | Default for `--java-home`. Real-JDK mode requires the directory to contain `jmods/java.base.jmod` **or** `lib/modules`; a JDK 8-style `lib/rt.jar`-only tree is rejected. If nothing usable is found the launch **fails** with the full search list — it does *not* fall back to synthetic stubs. |
| `RUST_MIN_STACK` | Minimum thread stack size. Set to `8388608` (8 MB) for deep-recursion tests. |
| `RUST_LOG` | Tracing log level (`trace`, `debug`, `info`, `warn`, `error`). Defaults to `WARN`. |
| `RJ_MAX_STACK_DEPTH` | Override `max_stack_depth` at startup (64–65536). |

### `CRATONVM_REAL` — real-vs-synthetic JDK gates

CratonVM can run real JDK bytecode or, for a few subsystems, fall back to a
Rust "synthetic" implementation. The real path is the **default** wherever a
real JDK is detected; the synthetic implementations are experimental opt-ins.
Prefer leaving these unset unless you are reproducing a synthetic-vs-real
difference.

`CRATONVM_REAL` also accepts `all`, `jca`, and bare internal-form class names
(`java/util/stream/Collectors`) — it did so before this consolidation, and that
is where the grouped-variable syntax came from.

| Token | Description | Default |
|-------|-------------|---------|
| `-stubs` | Drop **every** `SyntheticStub` native at registration so calls fall through to real JDK bytecode (or a clear `NoSuchMethodError`) instead of a fake. Surfaces real gaps as errors. `Intrinsic`/`Bridge` natives are unaffected. | stubs present |
| `net-sockets` / `-net-sockets` | Use the real `java.net` socket bytecode (the central registry drops the synthetic `java/net/Socket`/`ServerSocket` natives) instead of the synthetic socket layer. | real |
| `aqs` / `-aqs` | Route `AbstractQueuedSynchronizer` / `ReentrantLock` etc. through real `java.util.concurrent` bytecode instead of the synthetic lock natives. | real |
| `annotations` / `-annotations` | Annotation reflection uses real proxy-backed annotation objects. | real |
| `forkjoinpool` / `-forkjoinpool` | Drop the synthetic `ForkJoinPool` natives and run the real `java.util.concurrent` pool. The `-` form also seeds Weld's `threadPoolType=NONE`, because the synthetic pool cannot service `commonPool().invokeAll`. | real |
| `raf` / `-raf` | Force real / synthetic `RandomAccessFile`. | real |
| `proxy-super` / `-proxy-super` | Use the real `java.lang.reflect.Proxy` super-class path. | real |
| `agroal`, `vertx`, `ec`, `rsa`, `dsa`, `pqc`, `filewriter`, `buffered-writer`, `quarkus-arc`, … | Per-subsystem selection; the `-` form picks the experimental Rust implementation for that one subsystem. | see `docs/flag-tokens.md` |

### `CRATONVM_JIT` — compiler tuning

| Token | Description | Default |
|-------|-------------|---------|
| `threshold=N` | Invocation count at which a method becomes JIT-eligible. Higher keeps short-lived code interpreted; `0` is clamped to `1`. | `500` |
| `code-cache-max-mb=N` | Upper bound on total retained JIT code, in MiB; when reached, new methods stay interpreted. `0` disables the cap. | (built-in cap) |
| `-intrinsics` | Prevent the interpreter from installing `Intrinsic` inline-cache entries, forcing ordinary native/bytecode dispatch (differential-test off-switch). | on |
| `-precise-jit-maps` | Opt **out** of precise JIT oop maps (revert to the older conservative stack scan). For diagnosing GC-root coverage under JIT. | precise maps on |
| `-bce`, `-licm`, `-unroll`, `-reassoc`, `-scalar-new` | Turn off an individual optimisation pass. | on |

> Before this consolidation, five of those tokens had **no working spelling**:
> `CRATONVM_PRECISE_JIT_MAPS`, `CRATONVM_JIT_INLINE_PUTFIELD`,
> `CRATONVM_JIT_SCAN_CACHE`, `CRATONVM_PRECISE_INLINE_FRAME_RECORD` and
> `CRATONVM_SELECTIVE_PROMOTE` were documented for months but read by nothing —
> a default flip had replaced each with an inverted `NO_*` variable and only the
> opt-out half was renamed. `-precise-jit-maps` and friends now reach the key
> the code actually reads.

### `CRATONVM_SECURITY` — hardening

These harden the VM for running untrusted bytecode or multi-tenant hosting. All
are **off by default** (the default posture is JDK-faithful single-tenant).

| Token | Description | Default |
|-------|-------------|---------|
| `confine-io` | Enable CWD file-I/O confinement (fail-closed): filesystem access is restricted to the working directory and explicitly registered sandbox roots. The certified hardening switch. | Off |
| `untrusted-code` | Like `confine-io` but warning-mode — auto-enables CWD confinement for untrusted-bytecode hosting. | Off |
| `block-private-nets` | Additionally deny outbound connections to loopback and RFC1918 private ranges (the link-local cloud-metadata block always runs). | Off |
| `require-policy` | With a `SecurityManager` installed but no policy loaded, **deny** (fail-closed) instead of allow-all. Used by the certification profile. | Off |
| `harden-manifest-classpath` | Drop JAR-manifest `Class-Path` entries that resolve outside the JAR's own directory (absolute roots, `file:/…`, `..` escapes). | Off |
| `trust-pem=PATH` | PEM trust bundle, consulted after the `javax.net.ssl.trustStore` sys-prop and before the JDK `cacerts`. | — |

### `CRATONVM_IO` — resource limits

| Token | Description | Default |
|-------|-------------|---------|
| `resolve-outbound-host` | Resolve outbound **hostnames** and apply the per-IP outbound policy to every resolved address (closes the DNS-alias / DNS-rebinding bypass). | Off (no DNS in policy) |
| `http-max-body=N` | Max accepted HTTP request body, in bytes, for the built-in HTTP server; larger requests get a `413`. | `8 MiB` |
| `zip-max-entry-bytes=N` | Per-entry uncompressed-size cap when inflating zip/jar entries (decompression-bomb guard). | `512 MiB` |

### `CRATONVM_COMPAT`, `CRATONVM_THREADS`, `CRATONVM_LOADER`, `CRATONVM_GC`

| Token | Group | Description | Default |
|-------|-------|-------------|---------|
| `-lazy-streams` / `eager-streams` | `COMPAT` | Opt **out** of the lazy / short-circuiting synthetic `java.util.stream` pipeline back to the legacy eager pipeline. Lazy is the default (keycloak-16 Part B): intermediate ops (`peek`/`map`/`filter`/`limit`/`skip`) defer instead of materialising, and short-circuit terminals stop early — so `Stream.of(...).peek(p).findFirst()` runs `p` once, matching HotSpot. Eager-terminal results and exceptions are identical either way. | lazy |
| `mockito-legacy-selectors` | `COMPAT` | Restore the pre-2026-07-27 native overrides of Mockito's own selector methods: `LocationFactory.create` returns a `Java8LocationImpl` carrying the hardcoded string `"-> at <<unknown line>>"` instead of walking the stack, and `ModuleMemberAccessor.delegate` always returns `ReflectionMemberAccessor`. Off by default — the real selectors run and pick `LocationImpl` / `InstrumentationMemberAccessor` as on HotSpot, so Mockito failure messages name their real call site. Escape hatch only. | Off |
| `-default-watchdog` | `THREADS` | Disable the 120-second hang watchdog. | on |
| `default-watchdog-sec=N` | `THREADS` | Override the default watchdog timeout. | `120` |
| `lock-order-check` | `THREADS` | Opt in (release builds) to runtime lock-ordering deadlock detection. Truthy values: `1`/`true`/`yes`/`on`. Always on in debug builds. | Off (release) |
| `resolve-cache-cap=N` | `LOADER` | Capacity of the shared symbol-resolution cache (bounds the footprint against a key-minting adversary). Clamped to ≥ 1. | `65536` |
| `max-inflated-bytes=N` | `GC` | Total-inflation cap for the zip/jar reader. | (built-in) |

### Legacy per-flag variables

Every token expands to the per-flag variable that used to be the surface, so a
runbook exporting `CRATONVM_DBG_GC_STRESS=65536` keeps working — that variable
is exactly what `CRATONVM_DBG=gc-stress=65536` writes. The launcher prints one
line naming the grouped spelling when it sees one; silence it with
`CRATONVM_DBG=-deprecations`.

A grouped variable **wins** over a legacy one, which is what lets
`CRATONVM_DBG=-heap-stale` mask a stale `CRATONVM_DBG_HEAP_STALE=1` inherited
from a parent shell.

> **Debug tokens.** The 342 tokens in `CRATONVM_DBG` are internal
> developer switches (tracing, GC stress, JIT bisection). They may change or
> disappear without notice.
>
> The surface reached 692 identifiers by growing roughly one per fixed bug
> with no retirement path; the fifteen variables above are pinned by
> `types/tests/flag_surface.rs`, so a new raw `std::env::var("CRATONVM_…")`
> call site is a deliberate two-file edit.
