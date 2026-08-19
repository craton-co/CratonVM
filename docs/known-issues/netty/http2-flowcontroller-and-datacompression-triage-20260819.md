# HTTP/2 cluster triage: 2 classes are a harness gap (`-ea` never passed), 1 is CratonVM-specific and likely the known throughput ceiling

**Status: mixed — 2 closed as not-a-bug, 1 OPEN.** Investigated 2026-08-19
(Azure host, dev `b4d79475c`). Split out of `fail-hang-crash-rerun-20260817.md`'s
"3 HTTP/2 classes" untriaged entry.

## `UniformStreamByteDistributorFlowControllerTest` / `WeightedFairQueueRemoteFlowControllerTest` — NOT a CratonVM bug

Both extend `DefaultHttp2RemoteFlowControllerTest` and fail the identical 6
inherited test methods, including `invalidWeightTooSmallThrows()` and
`invalidParentStreamIdThrows()`:

```
org.opentest4j.AssertionFailedError: Expected java.lang.AssertionError to
  be thrown, but nothing was thrown.
```

These tests exercise netty's own internal `assert` statements (Java
language assertions, which are no-ops unless the JVM is launched with
`-ea`). **`common.args` does not pass `-ea`** (`grep -c '^-ea$'` → 0).
Confirmed on HotSpot:

```bash
java @common.args -Dcraton.batch=1 CratonRunner io.netty.handler.codec.http2.WeightedFairQueueRemoteFlowControllerTest
```

→ `found=34 started=34 ok=28 failed=6` — **identical** to CratonVM, same 6
methods failing. Not a VM defect; this harness has never enabled Java
assertions, so any netty test relying on internal `assert` statements
firing will fail the same way on both VMs. (A VM-side `-ea` fix already
landed once, `ea-flag-ignored-so-assert-never-fires-20260812-FIXED.md`, for
a different symptom — that fix only matters once `-ea` is actually passed,
which it never has been for this harness.)

**Disposition:** no VM fix applies. If these should pass, the harness fix
is adding `-ea` to `common.args` — worth doing once, since it plausibly
affects other classes with the same pattern, not just these two.

## `DataCompressionHttp2Test` — CratonVM-specific, 2/42, likely the known compression throughput ceiling

```
@@RESULT ... found=42 started=42 ok=40 failed=2 aborted=0
```

HotSpot: `found=42 started=42 ok=42 failed=0` — **CratonVM-specific**,
confirmed. Both failures are the same test method,
`encodingTooBigMessage(padding, "snappy")`, at two different padding
parameterizations. The failing assertion:

```java
assertTrue(serverLatch.await(5, SECONDS));   // DataCompressionHttp2Test.java:255
```

The test pushes 512 KiB of text through Snappy HTTP/2 content-encoding,
constrained to a 64 KiB (`inputLength/8`) max-allocation budget, and waits
up to **5 seconds** for the server side to finish. Not investigated with
an isolated timing run, but this has the identical shape (large-payload,
byte-level, Snappy-family compression, bound by a short wall-clock wait) to
the *already-documented* per-call dispatch throughput ceiling in
`compression-testhugedecompress-shared-timeout-20260816.md` — a
`ByteBuf.writeByte`-heavy loop measured at ~300x HotSpot's per-iteration
cost there. If the same mechanism applies here, this isn't a new defect,
just a shorter (5s vs 180s) budget than the compression cluster's timeout
happens to also blow.

**Not confirmed** — no isolated before/after timing measurement was taken
for this specific class. Worth a `CRATONVM_DBG_INVOKE_PHASES=1`-style check
or just a longer-`await()` experiment before assuming this is the *same*
cause rather than a coincidentally similar one.

## Related

- `fail-hang-crash-rerun-20260817.md` — where this cluster was first
  flagged as untriaged.
- `compression-testhugedecompress-shared-timeout-20260816.md` — the
  likely-shared root cause for `DataCompressionHttp2Test`'s residual.
- `ea-flag-ignored-so-assert-never-fires-20260812-FIXED.md` — the earlier,
  different `-ea`-shaped fix; does not help here since `-ea` is still never
  passed by this harness.
