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
