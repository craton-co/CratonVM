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
| `--jar <FILE>` | Execute a JAR. Main class is read from `../apps/META-INF/MANIFEST.MF`. `-cp` is ignored when this is set. | — |
| `--Xbootclasspath <PATH>` | Override bootstrap classpath. | Auto-detected from `--java-home` |
| `--java-home <PATH>` | JDK installation for boot/ext classpath discovery and JMOD loading. Does **not** select a mode — it only points the (already selected) real-JDK mode at a specific installation. | `CRATONVM_JAVA_HOME`, then `JAVA_HOME`, then `java` on `PATH` |
| `--real-jdk` | Load the real JDK class files from `jmods/` or `lib/modules`, with roughly **2,700** native registrations in Rust (`REAL_JDK_NATIVE_REGISTRATIONS` in [`vm/src/config.rs`](../vm/src/config.rs)). Already the default; pass it to be explicit. Fails loudly if no usable JDK is found. | **on** (`LAUNCHER_DEFAULT_JDK_MODE`) |
| `--synthetic-jdk` | Use the synthetic Rust standard library (~5,200 native stubs, no JDK needed) instead. Mutually exclusive with `--real-jdk`. Requires a build with the `synthetic-jdk` Cargo feature — otherwise the launch fails rather than starting a VM with no class library at all. | off |
| `--jdk-only` | Real JDK **and** real class bytes are authoritative: no class is fabricated without real bytes, and no synthetic-stub native is registered or invoked. Implies `--real-jdk`; conflicts with `--synthetic-jdk`. An internal diagnostic — see [JDK-only mode](#jdk-only-mode) below. | off (`LAUNCHER_DEFAULT_COMPATIBILITY_MODE` = `compatible`) |

## Differential mode

Run the same program under CratonVM **and** under a reference JDK, compare
stdout / stderr / exit status, and report the **first** divergence. Boots no VM
in the launcher process; both sides are children.

| Flag | Description | Default |
|------|-------------|---------|
| `--diff-hotspot` | Enable differential mode. Exit `0` identical, `1` diverged, `2` no usable reference JDK / invocation refused, `3` the CratonVM side was not self-consistent. Refuses `--synthetic-jdk`; works with `--jar` and `--jdk-only`. See [`testing/diff-hotspot.md`](testing/diff-hotspot.md). | off |
| `--diff-ignore <PATTERN>` | Mask any output line containing `PATTERN` on both sides before comparison. `*` is a wildcard; everything else is a literal substring — **not** a regex, because the launcher links no regex engine and promising syntax it cannot honour is worse than saying so. Repeatable. Requires `--diff-hotspot`. | none |
| `--diff-runs <N>` | How many times the CratonVM side runs, so the program's own nondeterminism is detected before a difference can be blamed on HotSpot. `1` disables the check. Requires `--diff-hotspot`. | `2` |
| `--diff-timeout <SECONDS>` | Per-child wall clock. An overrun renders as `<timeout>` on the exit channel, which never compares equal to a clean exit. Requires `--diff-hotspot`. | `120` |
| `--diff-java-arg <ARG>` | An extra argument passed to the reference `java` only (e.g. an `--add-opens` the CratonVM side does not need). Repeatable. Requires `--diff-hotspot`. | none |
| `--diff-strict` | Treat a difference that only the built-in nondeterminism maskers explain as a failure (exit `1` instead of `0`). Requires `--diff-hotspot`. | off |

The verdict is **byte-exact first**: the maskers (`identity-hash`, `hex-address`,
`thread-id`, `timestamp`, `absolute-path`) run only as a second opinion on a
failure, so no masker can turn a strict pass into a false pass. A reference
candidate whose `-version` banner says CratonVM is rejected and resolution
continues — this tree ships a `java`-named alias binary, and without that guard
the tool would compare CratonVM against itself and report a serene,
meaningless "no divergence".

> **The `--real-jdk` figure used to read "~300 native methods".** That was wrong
> by roughly 9x and is now pinned to a single constant with its derivation
> attached. Read it carefully: `REAL_JDK_NATIVE_REGISTRATIONS` counts
> registration *call sites* reached from the default build's registrars, not
> distinct registry keys — a method registered twice counts twice, so the true
> distinct-method count is somewhat lower, and six registrars defined outside
> `native-builtins/src` were not walked, so the number is a floor. For an exact
> per-run census run `--dump-native-registry`; that dump, not this table, is the
> authority.

## JDK-only mode

`--jdk-only` is an **internal diagnostic**, not a supported runtime mode.
`--real-jdk` remains the default and is unchanged by anything in this section.
Stage 1 is measurement-first: violations that cannot yet be enforced safely are
recorded and counted rather than made fatal. Normative semantics live in
[`feature-designs/jdk-only-mode.md`](feature-designs/jdk-only-mode.md); the
operator-facing walkthrough is [`jdk-only-migration.md`](jdk-only-migration.md).

### Two orthogonal axes

The VM has *two* independent settings that people routinely conflate:

| Axis | Type | Question it answers | Values |
|---|---|---|---|
| `JdkMode` | `vm::config::JdkMode` | **which class library** loads | `real-jdk` · `synthetic-jdk` |
| `CompatibilityMode` | `types::compat::CompatibilityMode` | **which substitutions** are permitted | `compatible` · `jdk-only` |

Neither implies the other, and neither is inferred from a Cargo feature, an
environment variable, or what happens to be installed on the host. The
resulting pair is read as one `ExecutionPolicy` value
(`VmConfig::execution_policy()`) by class loading, the native registry and
dispatch.

| Flags | `JdkMode` | `CompatibilityMode` |
|---|---|---|
| *(none)* | `real-jdk` | `compatible` |
| `--real-jdk` | `real-jdk` | `compatible` |
| `--synthetic-jdk` | `synthetic-jdk` | `compatible` |
| `--jdk-only` | `real-jdk` | `jdk-only` |
| `--jdk-only --synthetic-jdk` | **rejected at startup** | |

The last row is a configuration error, not a preference. `jdk-only` forbids
registering or invoking a `SyntheticStub` native, and the synthetic library
*is* ~5,200 such stubs, so the pair selects a VM with no usable class library.
`VmConfig::validate_compatibility()` rejects it before boot and names both
fixes; the VM never silently picks one, because the two corrections run
different class libraries with different semantics and different bug sets.

### Where the defaults come from

| Entry point | `JdkMode` default | `CompatibilityMode` default |
|---|---|---|
| `cratonvm` launcher (`VmConfig::for_launcher`) | `real-jdk` (`LAUNCHER_DEFAULT_JDK_MODE`) | `compatible` (`LAUNCHER_DEFAULT_COMPATIBILITY_MODE`) |
| embedding / in-tree tests (`VmConfig::default`) | `synthetic-jdk` (`EMBEDDED_DEFAULT_JDK_MODE`) | `compatible` (`EMBEDDED_DEFAULT_COMPATIBILITY_MODE`) |

The asymmetry is deliberate and is the point of declaring four constants rather
than two. The `JdkMode` pair *differs* by entry point because "which class
library loads" is a hermeticity question and the two entry points genuinely
want different answers. The `CompatibilityMode` pair is **deliberately
identical**: `jdk-only` rejects work that `compatible` accepts, so a caller
that did not ask for strictness must never be handed it. Strictness has exactly
one source — an explicit `--jdk-only`, or an explicit
`VmConfig::with_compatibility_mode(CompatibilityMode::JdkOnly)`.

`with_compatibility_mode` also **does not** rewrite `use_synthetic_jdk`, even
though `--jdk-only` implies a real JDK. Forcing `JdkMode::Real` there would
silently repair `--jdk-only --synthetic-jdk` into a real-JDK run and erase the
conflict the launcher is supposed to report. The setter records only what it
was asked for; `validate_compatibility()` is the backstop for any caller that
sets the two independently.

### Diagnostic flags

| Flag | Description | Default |
|------|-------------|---------|
| `--jdk-only-report <FILE>` | Write the violation/counter report as JSON (`schema_version` 1): `{ mode, jdk_feature, violations[], counts{} }`, with the class-origin buckets and the per-`NativeKind` invocation totals. Violations are sorted so the file is diff-stable. Works in **either** mode — under the default `compatible` mode it is a census of what strict mode *would* reject. | — |
| `--dump-class-origins <FILE>` | Write the class-origin census: one row per class the class manager holds (`name`, `origin`, `reason`, `requested_by`, `real_bytes_found`, `loader_id`), sorted. `origin` uses the stable `ClassOrigin::as_str()` tags — `boot-image`, `vm-array`, `generated-lambda`, `compatibility-stub`, … | — |
| `--trace-jdk-only` | Log each violation to stderr as it is picked up. Drained once after VM init (which is when registration refusals happen) and again at shutdown. | Off |
| `--explain-jdk-only` | Print the long-form operator-facing explanation for each violation instead of the one-line summary — **and leave absolute paths unredacted** in the report and census files. Paths are redacted without it, so treat a run with this flag as one whose artifacts carry local filesystem layout. | Off (paths redacted) |

`--dump-native-registry <FILE>` is not JDK-only-specific, but is the companion
census: `schema_version` 2 adds `registered_by`, `overwrote`, `invocations` and
`real_declaring_method` per entry, which is what turns "this stub exists" into
"this stub was actually dispatched".

The compatibility mode is also reported in the version banner
(`compatibility=jdk-only`), so a bug report can be read without guessing which
policy produced it.

### Embedding equivalents

There is no environment variable and no build feature for strict mode; every
entry point takes it explicitly.

| Surface | How to request `jdk-only` |
|---|---|
| launcher | `--jdk-only` |
| Rust (`cratonvm-embed` / `cratonvm-vm`) | `VmConfig::with_compatibility_mode(CompatibilityMode::JdkOnly)`, then `validate_compatibility()` before boot |
| C ABI (`libcratonvm`) | `cratonvm_create_with_compatibility(..., CRATONVM_COMPATIBILITY_JDK_ONLY)`, or the `--jdk-only` option string in `JavaVMInitArgs` |

The C-ABI values (`CRATONVM_COMPATIBILITY_COMPATIBLE` = 0,
`CRATONVM_COMPATIBILITY_JDK_ONLY` = 1) are a published, append-only part of the
ABI and are typed `cratonvm_jint` rather than a boolean so a third enforcement
posture can be added later without breaking a compiled host. Rust embedders
read the resolved pair back as `VmConfig::execution_policy()`. See
[EMBEDDING.md](EMBEDDING.md).

### `CRATONVM_REAL=-stubs` is not a substitute

`CRATONVM_REAL=-stubs` (equivalently `CRATONVM_NO_STUBS`) keeps working as a
**native-registry filter** and is not being removed. But it covers only one
third of the contract, and the launcher now prints a one-time note saying so
when it sees the variable without `--jdk-only`.

| Contract half | `CRATONVM_REAL=-stubs` | `--jdk-only` |
|---|---|---|
| Drop `SyntheticStub` native **registrations** | yes — silently | yes, with an attributed refusal record |
| Refuse a **fabricated compatibility class** (no real bytes) | **no** — cannot express it | yes (enforced in wave 1) |
| Refuse a registered native that **shadows real bytecode** at dispatch | **no** — cannot express it | counted in wave 1; the resolver routes every path |
| Structured, attributed violation report | no | `--jdk-only-report` / `--dump-class-origins` |
| Require a real JDK image, no silent fallback | no | yes |

The two are deliberately separate code paths in
`NativeMethodRegistry::register`, not one merged branch: the env var is an
operator's blunt "make the bucket vanish" switch and is silent by design, while
`jdk-only` is a policy that must produce evidence. Merging them would either
make the env var start allocating a violation per drop, or cost `jdk-only` its
provenance.

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
| `compatibility_mode` | `Compatible` — **both** launcher and embedded | Which substitutions are permitted, orthogonal to `use_synthetic_jdk`. Unlike the JDK-mode pair above, `LAUNCHER_DEFAULT_COMPATIBILITY_MODE` and `EMBEDDED_DEFAULT_COMPATIBILITY_MODE` are deliberately **equal**: strict mode rejects work that `Compatible` accepts, so it is never inherited, never inferred from a Cargo feature, and never read from the environment. Set it with `--jdk-only` or `VmConfig::with_compatibility_mode`; see [JDK-only mode](#jdk-only-mode). |
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

> **Adding a `CRATONVM_*` flag? The procedure is in the source, and it is four
> files.** The canonical, maintained-next-to-the-table version is the doc
> comment on `INVENTORY` in
> [`types/src/flag_groups.rs`](../types/src/flag_groups.rs), under *"Adding a
> `CRATONVM_*` flag: the four files, all of them"*. Two things to know before
> you start:
>
> * **Enforcement is `cargo test`, not `cargo check`.** `flag_declaration_guard.rs`,
>   `flag_surface.rs` and `flag_docs_generated.rs` are `assert!`s, so
>   `cargo check --all-targets` stays green while any of the four files is
>   missing its row.
> * **Declaration is bidirectional.** A literal with no row fails the guard, and
>   a **row with no reader** fails check 5 of `tools/flag-census/check-surface.sh`.
>   Land the declaration and its consumer in the same change.
>
> No `types/src/flags.rs` field is needed — `VmFlags::legacy_var_os` serves every
> declared name from one map.

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
| `CRATONVM_ENABLE_ASSERTIONS` | Enable Java `assert` statement evaluation for the run. An unscoped `-ea` / `-enableassertions` / `-esa` sets it; `-da` / `-dsa` clears it, overriding an inherited export. Scoped forms (`-ea:some.pkg`) are ignored — the switch is JVM-wide. |
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
| `-stubs` | Drop **every** `SyntheticStub` native at registration so calls fall through to real JDK bytecode (or a clear `NoSuchMethodError`) instead of a fake. Surfaces real gaps as errors. `Intrinsic`/`Bridge` natives are unaffected. Still supported, but it is a registry filter only — it cannot reject a fabricated compatibility class or stop a native shadowing real bytecode. Prefer `--jdk-only`; see [JDK-only mode](#jdk-only-mode) for the coverage table. | stubs present |
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
| `vectorize` | Emit AVX2 vector loops for the shapes the vector gate admits. Off by default: vector work multiplies wrong-code risk, so nothing is emitted unless it is asked for by name. | Off |
| `range-bce` | Admit the *guard-dominated range* reason for deleting a bounds check, on top of the reasons `bce` already applies. Off by default and deliberately opt-in: the reason has no differential run behind it, and a wrong elision is an out-of-bounds heap write. `-bce` still kills every reason including this one. | Off |
| `ir-linear-scan` | Run the linear-scan register allocator and use its result as a register read cache. FP values only. | Off |
| `-shadow-end-guard` | Opt **out** of the overflow bound both backends emit ahead of a shadow-stack push, so an overrunning push writes on through the allocator arena. Bisection only — it exists to confirm that a given failure *is* the overflow. | guard on |

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
| `confine-io` | Enable CWD file-I/O confinement (fail-closed): filesystem access is restricted to the working directory and explicitly registered sandbox roots. The confinement profile. | Off |
| `untrusted-code` | The strict defence-in-depth profile. Same fail-closed CWD confinement as `confine-io` — it aborts if confinement cannot be established, it does **not** warn and continue — and additionally implies `require-policy` and unconditionally denies host-native access (JNI library loads, `SymbolLookup`, FFM downcalls). Neither profile is an in-process sandbox; both still need an OS or container boundary. | Off |
| `block-private-nets` | Additionally deny outbound connections to loopback and RFC1918 private ranges (the link-local cloud-metadata block always runs). | Off |
| `require-policy` | With a `SecurityManager` installed but no policy loaded, **deny** (fail-closed) instead of allow-all. Implied by `untrusted-code`. | Off |
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
| `mockito-legacy-selectors` | `COMPAT` | Restore the older native overrides of Mockito's own selector methods: `LocationFactory.create` returns a `Java8LocationImpl` carrying the hardcoded string `"-> at <<unknown line>>"` instead of walking the stack, and `ModuleMemberAccessor.delegate` always returns `ReflectionMemberAccessor`. Off by default — the real selectors run and pick `LocationImpl` / `InstrumentationMemberAccessor` as on HotSpot, so Mockito failure messages name their real call site. Escape hatch only. | Off |
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
