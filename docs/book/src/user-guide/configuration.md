# Configuration

CratonVM is configured three ways:

1. **Command-line flags** — see the [Command-Line Reference](cli-reference.md).
2. **Java system properties** — `-D<key>=<value>`, readable with
   `System.getProperty`.
3. **Environment variables** in the `CRATONVM_*` namespace, plus a few standard
   ones (`JAVA_HOME`, `RUST_LOG`, …).

This chapter covers the environment variables and built-in defaults you are most
likely to touch. The complete, categorized table lives in the [Environment
Variables reference](../reference/environment-variables.md).

> Each `CRATONVM_*` switch is read **once at startup** and cached for the
> process lifetime — changing it mid-run has no effect.

## Boot & path variables

| Variable | Purpose |
|----------|---------|
| `JAVA_HOME` | Default for `--java-home`. If it contains `jmods/java.base.jmod` (or `lib/modules`), the launcher boots from that real JDK; otherwise it falls back to synthetic mode. |
| `CRATONVM_JAVA_HOME` | Overrides `JAVA_HOME` for the boot probe. Use it when a build tool points `JAVA_HOME` at a CratonVM shim tree but the boot modules should come from a real JDK. |
| `RUST_LOG` | Internal tracing log level (`trace`, `debug`, `info`, `warn`, `error`). Defaults to `warn`. |
| `RUST_MIN_STACK` | Minimum thread stack size in bytes. Set to `8388608` (8 MB) for deep-recursion workloads. |
| `RJ_MAX_STACK_DEPTH` | Maximum Java call depth before `StackOverflowError` (clamped to 64–65536). |

## Heap & GC

| Variable | Purpose |
|----------|---------|
| `CRATONVM_DEFAULT_HEAP_ERGONOMICS` | Set to `0` to disable the ergonomic default heap and revert to the fixed baseline. |
| `CRATONVM_DEFAULT_HEAP_MAX_MB` | Cap (in MiB) for the ergonomic default heap. |

See [Memory & Garbage Collection](memory-and-gc.md) for the full sizing rules.

## JIT tuning

| Variable | Purpose | Default |
|----------|---------|---------|
| `CRATONVM_DISABLE_JIT` | Interpreter-only execution (the `--nojit` flag sets this). | Off |
| `CRATONVM_JIT_THRESHOLD` | Invocation count at which a method becomes JIT-eligible. | `500` |
| `CRATONVM_JIT_CODE_CACHE_MAX_MB` | Upper bound (MiB) on retained JIT code; `0` means unbounded. | Built-in cap |
| `CRATONVM_NO_PRECISE_JIT_MAPS` | Revert to the older conservative GC stack scan (diagnostics only). | Precise maps on |
| `CRATONVM_DISABLE_INTRINSICS` | Skip interpreter intrinsic inline-cache entries. | Off |

See [The JIT Compiler](jit-compiler.md).

## Real-vs-synthetic backend gates

CratonVM prefers real JDK bytecode wherever a JDK is detected; a few subsystems
also ship an experimental Rust ("synthetic") implementation. These switches pick
which path a subsystem uses. **Leave them unset** unless you are reproducing a
synthetic-vs-real difference.

| Variable | Effect |
|----------|--------|
| `CRATONVM_NO_STUBS` | Drop every synthetic-stub native so calls fall through to real bytecode (or a clear `NoSuchMethodError`). Surfaces real gaps as errors — but only in the native registry; prefer [`--jdk-only`](jdk-only-mode.md) for the whole policy. |
| `CRATONVM_REAL_NET_SOCKETS` | Use real `java.net` socket bytecode instead of the synthetic socket layer. |
| `CRATONVM_REAL_AQS` | Route `AbstractQueuedSynchronizer` / `ReentrantLock` through real `java.util.concurrent` bytecode. |
| `CRATONVM_REAL_ANNOTATIONS` | Annotation reflection uses real proxy-backed annotation objects by default; set to `0` to use the old synthetic representation. |
| `CRATONVM_SYNTHETIC_*` (e.g. `_AQS`, `_RSA`, `_RAF`, …) | Per-subsystem force-synthetic opt-outs. |

The real path is the default wherever a real JDK is present; the synthetic
implementations are experimental opt-ins.

## Standard-library behavior

| Variable | Effect |
|----------|--------|
| `CRATONVM_EAGER_STREAMS` | Opt **out** of the lazy/short-circuiting `java.util.stream` pipeline back to the legacy eager one. Lazy mode is the default and matches HotSpot (intermediate ops defer; short-circuit terminals stop early). |
| `CRATONVM_ENABLE_ASSERTIONS` | Evaluate Java `assert` statements. Set for you by an unscoped `-ea` on the command line; setting it directly is only needed for a launcher that cannot pass VM flags. |

## Security & sandboxing

These harden the VM for untrusted or multi-tenant workloads and are **off by
default** (the default posture is JDK-faithful). See [Sandboxing &
Hardening](../security/sandboxing.md) for the full treatment.

| Variable | Effect |
|----------|--------|
| `CRATONVM_CONFINE_IO` | Fail-closed filesystem confinement to the working directory and registered roots. |
| `CRATONVM_BLOCK_PRIVATE_NETS` | Deny outbound connections to loopback and private (RFC 1918) ranges. |
| `CRATONVM_RESOLVE_OUTBOUND_HOST` | Resolve outbound hostnames and apply the per-IP egress policy (closes DNS-rebind bypass). |
| `CRATONVM_HTTP_MAX_BODY` / `CRATONVM_ZIP_MAX_ENTRY_BYTES` | Request-body and decompression-bomb caps. |
| `CRATONVM_REQUIRE_POLICY` | With a `SecurityManager` and no policy, deny instead of allow-all. |

## Built-in defaults

These compile-time defaults apply unless overridden:

| Setting | Value | Notes |
|---------|-------|-------|
| Initial heap | 16 MB | Initial committed heap |
| Max heap | Ergonomic (~¼ RAM, capped) by launcher; `256m` library default | Override with `-Xmx` |
| Max stack depth | 1024 frames | Override with `RJ_MAX_STACK_DEPTH` |
| GC algorithm | Generational | `G1` is opt-in via `-XX:+UseG1GC` |
| Verification | `remote` | Non-boot classes only |
| CDS / AOT | Off | |

## Debug-only toggles

Hundreds of `CRATONVM_DBG_*`, `CRATONVM_DIAG_*`, and `CRATONVM_TRACE_*` variables
exist for VM development (tracing, GC stress, JIT bisection). They are **not** a
supported configuration surface, may change without notice, and are
intentionally not documented here.
