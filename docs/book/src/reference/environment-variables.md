# Environment Variables

The complete, categorized reference for the environment variables CratonVM
reads. For the narrative version see [Configuration](../user-guide/configuration.md);
for command-line flags see the [Command-Line Reference](../user-guide/cli-reference.md).

> Each `CRATONVM_*` switch is read **once at startup** and cached for the
> process lifetime. Boolean switches generally treat *presence* as on, with
> `0`/`false`/`off`/`no` disabling.

## Boot & path

| Variable | Description |
|----------|-------------|
| `JAVA_HOME` | Default for `--java-home`. If it contains `jmods/java.base.jmod` (or `lib/modules`), the launcher boots from that real JDK; otherwise it falls back to synthetic mode. |
| `CRATONVM_JAVA_HOME` | Overrides `JAVA_HOME` for the boot probe — use when a build tool points `JAVA_HOME` at a CratonVM shim tree but boot modules should come from a real JDK. |
| `RUST_MIN_STACK` | Minimum thread stack size (bytes). Set to `8388608` (8 MB) for deep-recursion workloads. |
| `RUST_LOG` | Internal tracing level (`trace`/`debug`/`info`/`warn`/`error`). Default `warn`. |
| `RJ_MAX_STACK_DEPTH` | Max Java call depth before `StackOverflowError` (clamped to 64–65536). |

## Heap & GC

| Variable | Description |
|----------|-------------|
| `CRATONVM_DEFAULT_HEAP_ERGONOMICS` | Set to `0` to disable the ergonomic default heap (revert to the fixed 256 MB baseline). |
| `CRATONVM_DEFAULT_HEAP_MAX_MB` | Cap (MiB) for the ergonomic default heap. Floored at 256 MiB. |

## JIT & intrinsics

| Variable | Description | Default |
|----------|-------------|---------|
| `CRATONVM_DISABLE_JIT` | Interpreter-only execution (set by `--nojit`). | Off |
| `CRATONVM_JIT_THRESHOLD` | Invocation count at which a method becomes JIT-eligible (`0` clamps to `1`). | `500` |
| `CRATONVM_JIT_CODE_CACHE_MAX_MB` | Cap (MiB) on retained JIT code; `0` = unbounded. | Built-in cap |
| `CRATONVM_NO_PRECISE_JIT_MAPS` | Revert to the conservative GC stack scan (diagnostics). | Precise maps on |
| `CRATONVM_DISABLE_INTRINSICS` | Skip interpreter intrinsic inline-cache entries (differential testing). | Off |

## Real-vs-synthetic backend gates

| Variable | Description |
|----------|-------------|
| `CRATONVM_NO_STUBS` | Drop every synthetic-stub native so calls fall through to real bytecode (or a clear `NoSuchMethodError`). |
| `CRATONVM_REAL_NET_SOCKETS` | Use real `java.net` socket bytecode instead of the synthetic socket layer. |
| `CRATONVM_REAL_AQS` | Route `AbstractQueuedSynchronizer` / `ReentrantLock` through real `java.util.concurrent` bytecode. |
| `CRATONVM_REAL_ANNOTATIONS` | Annotation reflection uses real proxy-backed annotation objects by default; set to `0` to use the old synthetic representation. |
| `CRATONVM_REAL_FORKJOINPOOL` | Run the real `ForkJoinPool` (experimental). |
| `CRATONVM_REAL_RAF` / `CRATONVM_SYNTHETIC_RAF` | Force real / synthetic `RandomAccessFile`. |
| `CRATONVM_REAL_PROXY_SUPER` | Use the real `java.lang.reflect.Proxy` super-class path (`=0` opts out). |
| `CRATONVM_SYNTHETIC_*` (e.g. `_AQS`, `_EC`, `_RSA`, `_RAF`, `_FILEWRITER`, …) | Per-subsystem force-synthetic opt-outs. |
| `CRATONVM_SYNTHETIC_PQC` | Restore synthetic post-quantum stubs (default routes ML-KEM/ML-DSA to the real JDK SPIs). |

> The real path is the default wherever a real JDK is detected; synthetic
> implementations are experimental opt-ins. Leave these unset unless reproducing
> a synthetic-vs-real difference.

## Standard-library behavior

| Variable | Description |
|----------|-------------|
| `CRATONVM_EAGER_STREAMS` | Opt out of the lazy/short-circuiting `java.util.stream` pipeline (lazy is the default and matches HotSpot). |
| `CRATONVM_LAZY_STREAMS` | Explicit force-on for the lazy stream pipeline (already the default; wins over `CRATONVM_EAGER_STREAMS`). |
| `CRATONVM_ENABLE_ASSERTIONS` | Evaluate Java `assert` statements (the `-ea` analog). |

## Security & sandboxing

All off by default (the default posture is JDK-faithful). See [Sandboxing &
Hardening](../security/sandboxing.md).

| Variable | Description | Default |
|----------|-------------|---------|
| `CRATONVM_CONFINE_IO` | Fail-closed filesystem confinement to the CWD + registered roots. | Off |
| `CRATONVM_UNTRUSTED_CODE` | Same confinement, warning-mode instead of fail-closed. | Off |
| `CRATONVM_REQUIRE_POLICY` | With a `SecurityManager` and no policy, deny instead of allow-all. | Off |
| `CRATONVM_BLOCK_PRIVATE_NETS` | Deny outbound to loopback + RFC 1918 (the link-local metadata block is always on). | Off |
| `CRATONVM_RESOLVE_OUTBOUND_HOST` | Resolve outbound hostnames and apply the per-IP egress policy. | Off |
| `CRATONVM_HARDEN_MANIFEST_CLASSPATH` | Drop JAR-manifest `Class-Path` entries that escape the JAR's directory. | Off |
| `CRATONVM_HTTP_MAX_BODY` | Max inbound HTTP request body (bytes); larger → `413`. | 8 MiB |
| `CRATONVM_ZIP_MAX_ENTRY_BYTES` | Max per-entry inflated zip/JAR size (bytes); decompression-bomb guard. | 512 MiB |
| `CRATONVM_MAX_INFLATED_BYTES` | Companion total-inflation cap. | Built-in |
| `CRATONVM_TRUST_PEM` | Path to a PEM trust bundle for JAR-signature verification. | Unset |

## Diagnostics & resource limits

| Variable | Description | Default |
|----------|-------------|---------|
| `CRATONVM_DISABLE_DEFAULT_WATCHDOG` | Set to `1` to disable the automatic hang watchdog. | Off |
| `CRATONVM_DEFAULT_WATCHDOG_SEC` | Override the default watchdog timeout (seconds). | 120 |
| `CRATONVM_LOCK_ORDER_CHECK` | Opt into runtime lock-order deadlock detection in release builds (always on in debug). | Off (release) |
| `CRATONVM_RESOLVE_CACHE_CAP` | Capacity of the symbol-resolution cache (clamped ≥ 1). | 65536 |

## Debug-only toggles (unsupported)

Every `CRATONVM_DBG_*`, `CRATONVM_DIAG_*`, and `CRATONVM_TRACE_*` variable is an
**internal developer/debug switch** (tracing, GC stress, JIT bisection, and the
like). They are **not** a supported configuration surface, may change or
disappear without notice, and are intentionally not enumerated here.
