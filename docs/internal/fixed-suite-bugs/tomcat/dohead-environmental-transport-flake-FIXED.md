# DoHead family — transport flake and moving-GC root gaps (FIXED)

**Status: CLOSED — 2026-07-22.**

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

## Final validation

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

No known DoHead transport-flake residual remains.
