# Environment Variables

The complete, categorized reference for the environment variables CratonVM
reads. For the narrative version see [Configuration](../user-guide/configuration.md);
for command-line flags see the [Command-Line Reference](../user-guide/cli-reference.md).

Everything CratonVM reads is one of **fifteen** variables: ten grouped ones that
take a comma-separated token list, and five scalars.

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
ignored. Every token is listed in
[`docs/flag-tokens.md`](https://github.com/craton-co/cratonvm/blob/main/docs/flag-tokens.md);
this page covers the ones worth setting when running an application.

> Each variable is read **once at startup** and cached for the process lifetime.

## The ten grouped variables

| Variable | Covers |
|----------|--------|
| `CRATONVM_DBG` | tracing, dumps, GC stress, JIT bisection — nothing here can change a program's result |
| `CRATONVM_JIT` | compiler passes, tiering, deopt, precise oop maps, shadow stack |
| `CRATONVM_REAL` | real JDK bytecode vs synthetic Rust shim, per subsystem |
| `CRATONVM_GC` | collector selection, heap sizing, barriers, object layout |
| `CRATONVM_THREADS` | threading, async handoff, watchdog, lock-order checking |
| `CRATONVM_LOADER` | class loading, resolution, verification |
| `CRATONVM_IO` | files, sockets, HTTP, zip |
| `CRATONVM_TEST` | soak / difftest harness; never set in production |
| `CRATONVM_COMPAT` | per-application workarounds (JBoss, Spring, Quarkus) |
| `CRATONVM_SECURITY` | sandboxing, trust anchors, policy |

## Boot & path

| Variable | Description |
|----------|-------------|
| `JAVA_HOME` | Default for `--java-home`. If it contains `jmods/java.base.jmod` (or `lib/modules`), the launcher boots from that real JDK; otherwise it falls back to synthetic mode. |
| `CRATONVM_JAVA_HOME` | Overrides `JAVA_HOME` for the boot probe — use when a build tool points `JAVA_HOME` at a CratonVM shim tree but boot modules should come from a real JDK. |
| `CRATONVM_BIN` | Path to the `cratonvm` binary, for harnesses that re-exec it. |
| `CRATONVM_MAVEN_REPO_LOCAL` | Local Maven repository root. |
| `RUST_MIN_STACK` | Minimum thread stack size (bytes). Set to `8388608` (8 MB) for deep-recursion workloads. |
| `RUST_LOG` | Internal tracing level (`trace`/`debug`/`info`/`warn`/`error`). Default `warn`. |
| `RJ_MAX_STACK_DEPTH` | Max Java call depth before `StackOverflowError` (clamped to 64–65536). |

## Heap & GC — `CRATONVM_GC`

| Token | Description |
|-------|-------------|
| `-default-heap-ergonomics` | Disable the ergonomic default heap (revert to the fixed 256 MB baseline). |
| `default-heap-max-mb=N` | Cap (MiB) for the ergonomic default heap. Floored at 256 MiB. |
| `max-inflated-bytes=N` | Total-inflation cap for the zip/JAR reader. |

## JIT & intrinsics — `CRATONVM_JIT`

| Variable / token | Description | Default |
|------------------|-------------|---------|
| `CRATONVM_DISABLE_JIT` | Interpreter-only execution (set by `--nojit`). A scalar, not a token — it is the master switch. | Off |
| `threshold=N` | Invocation count at which a method becomes JIT-eligible (`0` clamps to `1`). | `500` |
| `code-cache-max-mb=N` | Cap (MiB) on retained JIT code; `0` = unbounded. | Built-in cap |
| `-precise-jit-maps` | Revert to the conservative GC stack scan (diagnostics). | Precise maps on |
| `-intrinsics` | Skip interpreter intrinsic inline-cache entries (differential testing). | On |
| `-bce`, `-licm`, `-unroll`, `-reassoc` | Turn off an individual optimisation pass. | On |

## Real-vs-synthetic backend gates — `CRATONVM_REAL`

The real path is the default wherever a real JDK is detected; synthetic
implementations are experimental opt-ins. Leave these unset unless reproducing
a synthetic-vs-real difference.

`CRATONVM_REAL` also accepts `all`, `jca`, and bare internal-form class names
(`java/util/stream/Collectors`).

| Token | Description |
|-------|-------------|
| `-stubs` | Drop every synthetic-stub native so calls fall through to real bytecode (or a clear `NoSuchMethodError`). This is the native-registry third of [JDK-only mode](../user-guide/jdk-only-mode.md); it cannot express the class-loading or dispatch half, and setting it without `--jdk-only` now prints a one-time note saying so. |
| `net-sockets` | Use real `java.net` socket bytecode instead of the synthetic socket layer. |
| `aqs` / `-aqs` | Route `AbstractQueuedSynchronizer` / `ReentrantLock` through real `java.util.concurrent` bytecode. |
| `annotations` / `-annotations` | Annotation reflection uses real proxy-backed annotation objects; `-annotations` restores the old synthetic representation. |
| `forkjoinpool` | Run the real `ForkJoinPool` (experimental). |
| `raf` / `-raf` | Force real / synthetic `RandomAccessFile`. |
| `proxy-super` / `-proxy-super` | Use the real `java.lang.reflect.Proxy` super-class path. |
| `-ec`, `-rsa`, `-filewriter`, `-agroal`, `-vertx`, … | Per-subsystem force-synthetic opt-outs. |
| `-pqc` | Restore synthetic post-quantum stubs (default routes ML-KEM/ML-DSA to the real JDK SPIs). |

> `aqs`, `annotations`, `agroal` and `vertx` each used to be **two** variables —
> a `CRATONVM_REAL_*` and a `CRATONVM_SYNTHETIC_*` — read together at a single
> call site. They are one token now; the old spellings still work.

## Standard-library behavior — `CRATONVM_COMPAT`

| Variable / token | Description |
|------------------|-------------|
| `-lazy-streams` / `eager-streams` | Opt out of the lazy/short-circuiting `java.util.stream` pipeline (lazy is the default and matches HotSpot). |
| `CRATONVM_ENABLE_ASSERTIONS` | Evaluate Java `assert` statements. A scalar. An unscoped `-ea` on the command line sets it; `-da` clears it, inherited value and all. |

## Security & sandboxing — `CRATONVM_SECURITY` and `CRATONVM_IO`

All off by default (the default posture is JDK-faithful). See [Sandboxing &
Hardening](../security/sandboxing.md).

| Token | Group | Description | Default |
|-------|-------|-------------|---------|
| `confine-io` | `SECURITY` | Fail-closed filesystem confinement to the CWD + registered roots. | Off |
| `untrusted-code` | `SECURITY` | Same confinement, warning-mode instead of fail-closed. | Off |
| `require-policy` | `SECURITY` | With a `SecurityManager` and no policy, deny instead of allow-all. | Off |
| `block-private-nets` | `SECURITY` | Deny outbound to loopback + RFC 1918 (the link-local metadata block is always on). | Off |
| `harden-manifest-classpath` | `SECURITY` | Drop JAR-manifest `Class-Path` entries that escape the JAR's directory. | Off |
| `trust-pem=PATH` | `SECURITY` | PEM trust bundle for JAR-signature verification. | Unset |
| `resolve-outbound-host` | `IO` | Resolve outbound hostnames and apply the per-IP egress policy. | Off |
| `http-max-body=N` | `IO` | Max inbound HTTP request body (bytes); larger → `413`. | 8 MiB |
| `zip-max-entry-bytes=N` | `IO` | Max per-entry inflated zip/JAR size (bytes); decompression-bomb guard. | 512 MiB |

## Diagnostics & resource limits

| Token | Group | Description | Default |
|-------|-------|-------------|---------|
| `-default-watchdog` | `THREADS` | Disable the automatic hang watchdog. | On |
| `default-watchdog-sec=N` | `THREADS` | Override the default watchdog timeout (seconds). | 120 |
| `lock-order-check` | `THREADS` | Opt into runtime lock-order deadlock detection in release builds (always on in debug). | Off (release) |
| `resolve-cache-cap=N` | `LOADER` | Capacity of the symbol-resolution cache (clamped ≥ 1). | 65536 |

## Legacy per-flag variables

Before this surface was consolidated there were 692 distinct `CRATONVM_*`
identifiers — 559 with a read site, 133 that nothing had ever read. Each token
above expands to the per-flag variable that used to be the surface, so an
existing script exporting `CRATONVM_DBG_GC_STRESS=65536` keeps working: that
variable is exactly what `CRATONVM_DBG=gc-stress=65536` writes. The launcher
prints one line naming the grouped spelling; silence it with
`CRATONVM_DBG=-deprecations`.

A grouped variable **wins** over a legacy one, which is what lets
`CRATONVM_DBG=-heap-stale` mask a stale `CRATONVM_DBG_HEAP_STALE=1` inherited
from a parent shell.

## Debug tokens (unsupported)

The 342 tokens in `CRATONVM_DBG` are **internal developer switches** (tracing,
GC stress, JIT bisection, and the like). They are not a supported configuration
surface and may change or disappear without notice.
