# DoHead family — transport flake and moving-GC root gaps (FIXED)

**Status: CLOSED — 2026-07-23.**

This issue was opened as a low-rate environmental transport flake after
isolated `SocketTimeoutException`/connection failures had no contemporaneous VM
guard signal. The moving-GC canary proved that classification incomplete:
shutdown, reflection, reference-queue, and HTTP-header paths held raw
`ObjectRef` values across GC-capable native boundaries. The client timeouts were
consequences of those panics, not an accepted environmental residual.

## Root cause and fixes

The initial closure rooted native I/O receivers, arrays, close/flush state,
JULI/LogManager and `MessageBytes` values, the real `HttpURLConnection` carrier,
and the `HashSet.remove`/`HashMap.remove` inputs reached from Tomcat
`ThreadPoolExecutor.processWorkerExit`.

The final v21 residual sweep closed the remaining paths:

- `ClassLoader` namespace lookup now requires a real `ClassLoader` subclass
  before touching the synthetic loader-id slot. A non-loader receiver (notably
  `String`) can no longer cause an out-of-bounds field probe during reflective
  DoHead loading.
- `Objects.hash(Object[])` pins and reloads its array around virtual element
  `hashCode()` calls.
- `Reference`/`ReferenceQueue` native paths use the real-JDK field names where
  present and pin/reload queue and dequeued-reference objects. Crucially, the
  blocked `remove` loops use the receiver rewritten by
  `end_blocking_region_refs` after wake-up, instead of a stale pre-park
  snapshot.
- `HashMap.remove` reloads the bucket node before reading its key, roots the
  freshly loaded node key during equality dispatch, and reloads the requested
  key. This closes the stale-node path reached by `processWorkerExit`.

Each native root is read back after an operation that can allocate or collect;
no raw from-space address is reused across such a boundary.

## Prior v21 validation

Azure host, isolated worktree `/data/wt-dohead-transport-flake-20260721`, v21
runtime `/data/cvm-dohead-transport-flake-20260721-final-v21`
(SHA-256 `75ebb042150c0c8027e926c6e26367fdaaa1fe90713f1d0b10f049b5cf0df59d`):

- Full 64-class DoHead invalid-write matrix, two fresh workers, `-Xmx1g`,
  interpreter-only, `CANARY_WORKER=0`, `CRATONVM_DBG_STALE_OBJREF=1`,
  `CRATONVM_DBG_OOBFIELD=Object`, and `RUST_BACKTRACE=1`: **64/64 PASS**,
  `ALL_DONE` (18:31:54–19:21:47), zero failures, unknowns, timeouts, crashes,
  stale-object assertions, or out-of-bounds-field diagnostics.
- The equivalent full two-worker JIT matrix under the same stale/OOB guards:
  **64/64 PASS**, `ALL_DONE` (20:18:16), zero failures, unknowns, timeouts, or
  crashes.
- `cargo test --release -p cratonvm-native-io -p
  cratonvm-native-collections -p cratonvm-native-builtins --lib --
  --test-threads=1`: **3,493 passed, 0 failed** (3,063 collections, 74 I/O,
  356 builtins; 7 ignored total).

The v21 evidence was a preliminary closure; the v30 sweep below completes
the later-found residual.

## Final v30 residual closure

The v21 sweep did not yet cover the result returned by
`ExecutorService.submit(Callable)`. `completed_executor_future` retained that
returned `ObjectRef` while allocating and completing a `CompletableFuture`.
When `WebappClassLoaderBase.clearReferencesJdbc` submitted its cleanup callable,
a moving collection could therefore leave the completion path with a stale
reference. The native builtin now pins and reloads the returned object and the
future across each allocating or virtual-completion boundary, and releases the
roots on both success and error paths.

The direct full-JIT reproduction also exposed a distinct compiler defect in
JUnit's `TestClass.collectAnnotatedMethodValues`: the compiled enhanced-for
iterator local could become null, producing a `NullPointerException` and a
failure-accumulation cascade. This is not a missing Java or Tomcat feature.
The VM now keeps only that exact JUnit helper interpreted; the original Tomcat
methods remain eligible for JIT compilation. An environment-only equivalent
guard independently reproduced the passing behavior before the source guard
was added.

Azure host, isolated worktree `/data/wt-dohead-transport-flake-20260721`, v30
runtime `/data/cvm-dohead-transport-flake-20260721-final-v30`
(SHA-256 `f03f608f5bd5023b2afad21102434630f5b4221a04ad4183fc2177d5290dbce2`):

- Direct normal-JIT `TestHttpServletDoHeadInvalidWrite1023ValidWrite1023`:
  **288/288 PASS** in 120 seconds, with no diagnostic or bisection environment
  override.
- Exact stale-reference no-JIT canary
  `TestHttpServletDoHeadInvalidWrite513ValidWrite511`: **288/288 PASS** in 77
  seconds with stale-`ObjectRef` and out-of-bounds-field diagnostics enabled.
- Full 64-class DoHead invalid-write matrix, two fresh workers, `-Xmx1g`,
  interpreter-only, and `CRATONVM_DBG_STALE_OBJREF=1` plus
  `CRATONVM_DBG_OOBFIELD=Object`: **64/64 PASS**, `ALL_DONE`
  (02:36:48–03:16:59), with zero failures, unknowns, timeouts, crashes, stale
  references, or field-bound diagnostics.
- The same full matrix in normal JIT mode under those diagnostics: **64/64
  PASS**, `ALL_DONE` (04:14:50), with zero failures, unknowns, timeouts, or
  crashes.

- `cargo test --release -p cratonvm-native-io -p
  cratonvm-native-collections -p cratonvm-native-builtins --lib --
  --test-threads=1`: **3,504 passed, 0 failed** (3,074 collections, 74 I/O,
  356 builtins; 7 ignored total).
- Focused release VM regression
  `tomcat_dohead_junit_iterator_helper_is_unconditionally_interpreted`:
  **1 passed, 0 failed** (2,351 filtered), proving the exact guard applies in
  both conservative and aggressive JIT policies.

## Rebased delivery validation (v31)

The closure branch was cleanly rebased onto the current `origin/dev` before
delivery. Because that base included concurrent GC and native changes, the
complete matrix and source regressions were rebuilt and rerun rather than
assuming the v30 evidence transferred.

Runtime `/data/cvm-dohead-transport-flake-20260721-final-v31`
(SHA-256 `9ba2f8654c09d68ee57bf47119426a590902f3b1904e8bffa5b6ae7d4ef14ca2`):

- Full 64-class no-JIT matrix with stale-reference and field-bound diagnostics:
  **64/64 PASS**, `ALL_DONE` (04:41:42–05:26:28), zero failures, unknowns,
  timeouts, crashes, or diagnostics.
- Full equivalent normal-JIT matrix with the same diagnostics: **64/64 PASS**,
  `ALL_DONE` (05:26:47–06:22:19), zero failures, unknowns, timeouts, crashes,
  or diagnostics.
- Isolated native-library regression suite: **3,509 passed, 0 failed**
  (3,079 collections, 74 I/O, 356 builtins; 7 ignored).
- Focused release VM JIT-skip regression: **1 passed, 0 failed**
  (2,351 filtered).

No known DoHead transport-flake residual remains.
