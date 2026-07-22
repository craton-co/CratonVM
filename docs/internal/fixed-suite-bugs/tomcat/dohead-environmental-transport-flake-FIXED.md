# DoHead family — transport flake and moving-GC root gaps (FIXED)

**Status: CLOSED — 2026-07-22.**

This issue was opened as a low-rate environmental transport flake after isolated
`SocketTimeoutException`/connection failures had no contemporaneous VM guard
signal. The stricter moving-GC canary subsequently proved that classification
incomplete: shutdown and HTTP-header paths held raw `ObjectRef` values across
GC-capable native boundaries. The client timeouts were consequences of those
panics, not an accepted environmental residual.

## Root cause and fixes

- Pin and reload native I/O receivers, arrays, and close/flush state across
  virtual calls in the DoHead request/response path.
- Keep JULI/LogManager receivers rooted across logger creation, registration,
  root-handler installation, and cache publication.
- Root `MessageBytes` and JULI handler receivers across virtual/allocating
  operations.
- Root both the `HashSet.remove` inputs and its nested `HashMap.remove` inputs
  during `ThreadPoolExecutor.processWorkerExit`.
- Root the real `HttpURLConnection` carrier across URL lookup, request
  execution, and header-map retrieval; this fixed the traced
  `huc_real_perform -> identity_hash_code` stale receiver.

All roots are read back after GC-capable calls, rather than reusing a raw
from-space address.

## Validation

Azure host, isolated worktree and v7 runtime
`/data/cvm-dohead-transport-flake-20260721-rootpins-v7`
(SHA-256 `16c56856233278fb9b61d235a72700750d7618b6a33d83ea9cc9f71a3c018ba8`):

- Full 64-class DoHead invalid-write matrix, two workers, interpreter-only,
  `CANARY_WORKER=0`, `CRATONVM_DBG_STALE_OBJREF=1`, and `RUST_BACKTRACE=1`:
  **64/64 PASS**, `ALL_DONE`, zero failures, unknowns, timeouts, stale-object
  assertions, VM panics, `gen_heap::set_field`, and `RESID-DIAG` hits.
- Full equivalent two-worker JIT matrix: **64/64 PASS**, `ALL_DONE`, zero
  failures, unknowns, or timeouts.
- Focused two-worker interpreter/canary stress of the traced
  `TestHttpServletDoHeadInvalidWrite512ValidWrite513` case: **5/5 PASS**.
- `cargo test -p cratonvm-native-io -p cratonvm-native-collections
  -p cratonvm-native-builtins --lib -- --test-threads=1`: all suites passed.

The formerly affected `TestHttpServletDoHeadInvalidWrite1ValidWrite511` and
`TestHttpServletDoHeadInvalidWrite512ValidWrite513` cases both passed in the
final full interpreter matrix.
