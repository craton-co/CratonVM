# Tomcat `TestHttpServletDoHead*` (legacy HEAD) — StreamEncoder eager-flush broke the byte-count commit threshold (FIXED)

**Update (2026-07-13, later the same day): the eager-flush behaviour this
doc describes came BACK — via a different file — and is re-fixed.** The
same 8-parameter family (useLegacy=true, useWriter=true, resetType=FULL,
bufferSize 16/8192 — params 46/47/58/59/118/119/130/131) failed again with
the same signature pairs (`testDoHead`: `expected:<2> but was:<3>`;
`testDoHeadHttp2`: GET commits `content-length: 8192` while the paired
HEAD goes chunked), deterministically on both Windows and Linux. The
regression is NOT in `native-io/src/stream_encoder.rs` (whose buffering —
since rewritten to by-name field access + an identity-hash-keyed side
table — is correct and was verified innocent): commit `b448f2039` ("Fix
WildFly process-controller bootstrap residuals", 2026-07-09) added a full
`java.io.OutputStreamWriter` native surface to
`register_essential_natives` (native-builtins/src/lib.rs): `<init>`×3,
`write`×4, `flush`, `close`. Registered natives shadow real bytecode at
every interpreter dispatch site (WP0.1 native-override-priority), so
real-JDK mode stopped running the real OSW bytecode → the
`sun.nio.cs.StreamEncoder` shim (and its pending-byte buffering) went
completely unreached. The replacement
(`write_bytes_from_output_stream_writer`) encodes EVERY `write()` call
straight to the wrapped stream — one underlying `write([BII)` per Writer
call, i.e. exactly the eager-flush behaviour this doc's fix removed. It
also hard-codes UTF-8 (`String::into_bytes()`, ignoring the writer's
charset) and its `<init>` clobbers real slot 0 (`Writer.writeBuffer`)
while never creating the `se` field.

Standalone confirmation (1024 × 16-char `Writer` writes into a counting
OutputStream): dev tip delivered 1024 × 16-byte underlying writes vs
HotSpot's 32 × 512 — for BOTH `PrintWriter` and bare `OutputStreamWriter`
paths.

**Re-fix:** gate the entire OSW block under
`cfg!(feature = "synthetic-jdk")` ("fix(io): gate the OutputStreamWriter
native surface to synthetic-JDK builds", branch
`fix/dohead-family-regressions-v2-20260713`, same precedent as the
BufferedInputStream block beside it). Post-fix: the standalone repro
batches 32 × 512 exactly like HotSpot, and
`TestHttpServletDoHeadInvalidWrite1024ValidWrite512` runs 288 tests with
zero commit-threshold failures on the Windows suite runner. The same
branch fixes the `javax/net/SocketFactory`-under-RNS Socket cluster (see
the main doc); the `Logger` handler corruption was independently fixed on
dev (`d94712f2a`).

**Note (2026-07-13, earlier):** restored from `docs/internal/fixed-suite-bugs/`
alongside `dohead-jit-heap-corruption-register-invisibility.md`. This
specific fix (commit `1773d3df2`) is confirmed merged to `dev` and remains
accurate — the byte-count commit-threshold bug it describes is genuinely
fixed. It's back in `known-issues` only for continuity: the class this doc
validated (`TestHttpServletDoHeadInvalidWrite1024ValidWrite512`) still
fails today, but via an unrelated, newly-found issue (`Socket`/HTTP2
test-connection handling — see the main doc) that this fix neither causes
nor addresses.

Status: **FIXED** on branch `dev` (this fix). Root cause is entirely in
`native-io/src/stream_encoder.rs` (CratonVM's real-mode shim for
`sun.nio.cs.StreamEncoder`) — no Tomcat/Servlet-API source was touched.

## Symptom

`jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite1024ValidWrite512`
(suite index 50) previously HUNG/CRASHED at short timeouts (120s/600s), which
is the tracked, separate GC non-moving-sweep corruption family documented in
`dohead-jit-heap-corruption-register-invisibility-FIXED.md`. At a 1200s timeout it
actually COMPLETES in ~674s and produces 16 deterministic (non-flaky)
failures — a different bug, confirmed unrelated to the GC-corruption family:
in the baseline run
(`apps/tomcat/.suite/results/hangonly1200/real-jit/…log.err`), the one GC
corruption WARN line (line 2253) falls between two unrelated
`testDoHeadHttp2` parameterizations, nowhere near any of the 16 failing test
cases.

All 16 failures are pairs of `testDoHead[N]` (`AssertionError:
expected:<1> but was:<2>`, a header-count mismatch) and `testDoHeadHttp2[N]`
(`ComparisonFailure`: GET commits with `content-length: 8192` while the
paired HEAD instead ends up `content-type` with no content-length, i.e.
chunked). All 16 share `useLegacy=true`, `useWriter=true`, `resetType=FULL`;
`bufferSize` is 16 or 8192 (never 16384 — that variant passed even before the
fix, for reasons explained below).

## Root cause

`HttpServlet.doHead()`'s legacy path wraps the response in `NoBodyResponse` /
`NoBodyOutputStream`, whose `checkCommit()` flips the response to committed
the first time a running byte counter exceeds the (specially adjusted)
buffer size. The counter is fed by however many bytes arrive per call to the
underlying `OutputStream.write(byte[], int, int)` — which, for a
`Writer`-based servlet, is driven by `java.io.OutputStreamWriter` →
`sun.nio.cs.StreamEncoder`.

Real JDK's `StreamEncoder` buffers encoded bytes into an internal `ByteBuffer`
(`INITIAL_BYTE_BUFFER_CAPACITY = 512`, growable to `MAX_BYTE_BUFFER_CAPACITY =
8192`) and only calls the underlying stream's `write` when that buffer
actually fills. Writing 1024 separate 16-byte `Writer.print()` calls reaches
the underlying stream via ~31 batched 512-byte writes on real HotSpot — NOT
1024 individual 16-byte writes.

CratonVM's `native-io/src/stream_encoder.rs` is a real-mode native shim that
replaces `sun.nio.cs.StreamEncoder`'s method surface entirely (the real
bytecode reaches into unimplemented `sun.nio.ch` internals). Before this fix,
its `write_bytes()` encoded and forwarded to the underlying
`OutputStream.write([BII)V` on **every single call** — no internal
buffering at all. For the DoHead test's `bufferSize` formula
(`adjustedBufferSize = bufferSize + originalBufferSize - 512`, engineered so
GET's real commit threshold and HEAD's virtual byte-counter threshold line
up), this changed the byte-count granularity at which
`NoBodyOutputStream.checkCommit()` observes crossing its threshold — flushing
one 16-byte batch at a time instead of one 512-byte batch, the running total
overshoots the exact boundary the test's math assumes, and HEAD's
`resp.reset()` throws `IllegalStateException` (already committed) in cases
where real Tomcat/HotSpot's `resp.reset()` succeeds cleanly (or vice versa).
`bufferSize=16384` produced an adjusted threshold (24064) far enough above the
total written bytes (16384) that the eager-flush granularity difference never
crossed the threshold either way — hence that variant passed even with the
bug.

Confirmed via a standalone repro with **no Tomcat/Servlet API at all** — a
bare `OutputStreamWriter` wrapping a counting `OutputStream`, writing 1024
16-byte strings — reproduced the exact same 16-bytes-per-underlying-write
pattern on CratonVM vs. 512-bytes-per-underlying-write on real HotSpot.

## Fix

`native-io/src/stream_encoder.rs`: added a Rust-side side-table
(`se_table()`, keyed by a stable per-encoder `int` id — the same pattern
`stream_decoder.rs` already uses for its charset-name/carry state) holding a
`Vec<u8>` pending-bytes buffer per encoder. `write_bytes()` now appends
encoded bytes to that buffer via a new `buffer_and_maybe_flush()` and only
calls the underlying stream's `write` when the buffer is actually full
(mirroring real `growByteBufferIfNeeded`/`writeBytes` — starts at 512 bytes,
grows toward 8192 only when a single write wouldn't otherwise fit, and does
**not** eagerly flush an exactly-full buffer — real `StreamEncoder` only
flushes on the *next* write attempt that overflows, so a request that stops
writing right when the buffer reaches capacity produces one fewer batch than
an eager "flush when full" implementation would, which mattered for getting
byte-for-byte parity with HotSpot's trace). `flush()`/`close()` now also
deliver any still-pending bytes first (previously a no-op from the buffering
layer's perspective, since there was nothing to flush).

The pending buffer cannot live in a new `StreamEncoder` object field: the
encoder is allocated with the REAL `sun/nio/cs/StreamEncoder` class id (so
`OutputStreamWriter`/`PrintWriter` bytecode dispatches to these natives), and
`ctx.get_field`/`set_field` at an index are descriptor-aware — they address
the REAL class's Nth declared field, whatever type the VM's internal layout
actually assigns to that index, NOT a bare scratch slot. This is *not* the
same as the field's declaration order in `javap` output — an earlier version
of this fix picked slot index 4 by reasoning from `javap
sun.nio.cs.StreamEncoder`'s field listing (predicting the primitive
`maxBufferCapacity` field) and got a real REFERENCE-typed field instead: the
side-table id written there silently coerced to `Object(None)` (read back as
0) instead of erroring, so two concurrent `StreamEncoder`s (e.g. the DoHead
test's invalid-write-loop encoder and the fresh encoder `NoBodyPrintWriter`
constructs after `resp.reset()`) shared one side-table entry and a stray
512-byte batch from the first encoder's abandoned buffer leaked into the
second encoder's output — a regression that only showed up on the
`bufferSize=16384` variant (which the eager-flush bug itself didn't break)
during validation. Fixed by re-verifying empirically (temporarily writing
sentinel ints 0–8 and diffing before/after each by-name field write this
file makes — `encoder`, `haveLeftoverChar`, `leftoverChar`) which indices
round-trip an `int` at all (2, 6, 8) and which of those are untouched by any
by-name write in this file (2). The id now lives at index 2.

## Validation

- Standalone `OutputStreamWriter`/`PrintWriter` repro (no Tomcat): batches now
  match HotSpot exactly (`write(byte[],0,512)` chunks, same byte offsets).
- Fast single-parameter JUnit driver (custom `Filter` + `JUnitCore`, reusing
  compiled classes — avoids the ~11 min full-class run per iteration):
  indices 46 (`bufferSize=16`), 70 (`bufferSize=16384`, a regression caught
  and fixed during validation — see above), 130 (`bufferSize=8192`) all pass
  `testDoHead` + `testDoHeadHttp2`.
- Full class through the real suite runner:
  `jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite1024ValidWrite512`
  → `OK (288 tests)`, 413.8s (JUnit-reported time), matching HotSpot's
  baseline (`OK (288 tests)`) exactly.
- Sibling spot checks: `TestHttpServletDoHeadInvalidWrite0ValidWrite0` PASS;
  `org.apache.catalina.connector.TestOutputBuffer` PASS (exercises the
  surrogate-pair carry logic this fix's code path shares);
  `org.apache.catalina.connector.TestCoyoteAdapter` PASS.
- General suite health spot check (first 15 `jakarta.el.*` classes): 13/15
  PASS; the 2 pre-existing failures (`TestBeanSupport`,
  `TestImportHandlerStandardPackages`) don't reference
  `OutputStreamWriter`/`PrintWriter` at all and are unrelated (confirmed via
  old pre-fix run `rerun0701a`, which shows the identical failure pattern).
- **Known pre-existing, unrelated failure** found during sibling spot-checks:
  `TestHttpServletDoHeadInvalidWrite1023ValidWrite1023` has 2 failures
  (`testDoHeadHttp2[0]`, `testDoHeadHttp2[25]`), both `useLegacy=false` (the
  container's native HEAD handling, not the legacy `HttpServlet.doHead()`
  path this fix touches) — confirmed present in a pre-fix run
  (`apps/tomcat/.suite/results/rerun0701a/…`) and real HotSpot passes all 288
  tests in that class, so this is a genuine, separate CratonVM bug. Flagged
  for follow-up investigation, not fixed here.

## Repro

```
cd apps/tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all `
  -RunName dohead-streamencoder -Start 50 -Count 1 -TimeoutSec 900 -Parallel 1
# logs: apps/tomcat/.suite/results/dohead-streamencoder/real-jit/jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite1024ValidWrite512.log
```

For fast iteration on a single parameterization instead of the full ~7 min
class, compile a small driver using `org.junit.runner.Request.aClass(cls)` +
a custom `org.junit.runner.manipulation.Filter` matching a substring like
`"[46:"` in the test name, and run it via `cratonvm.exe -cp <classpath>
<Driver> <FQCN> <filter-substring>` against the already-compiled test classes
in `apps/tomcat/output/testclasses` (no `ant test-compile` needed between
runs unless the `.java` sources themselves change).

Related: `dohead-jit-heap-corruption-register-invisibility-FIXED.md` (the SEPARATE,
previously-tracked GC-corruption family this bug was initially confused with
— see the log-line evidence above for why they're unrelated).
