# ES failure family - RestClient single-host async timeout

Status: FIXED

Date observed: 2026-07-09

## Fix applied (2026-07-10)

Root cause: `native-builtins/src/lib.rs`'s `AtomicMarkableReference` native
overlay (`register_atomic_markable_ref_natives` and the `native_amr_*`
functions) modelled the class with the OLD, pre-JDK9 2-field layout
(`reference` at slot 0, `mark` as a raw `Int` at slot 1). Real JDK 9+
`AtomicMarkableReference` declares exactly **one** instance field —
`private volatile Pair<V> pair`, where `Pair` bundles the reference and the
mark together (confirmed via `javap -p` against the JDK 25 install used on
the build host). CratonVM allocates `AtomicMarkableReference` instances
sized to the REAL class's declared field count (1 slot), so every native
write to slot 1 was silently dropped by the GC's out-of-bounds-write guard
(`gen_heap::set_field: ... out-of-bounds field write dropped`) — the `mark`
bit could never actually change. `get(boolean[])` also had no native
registration at all, so real bytecode ran unintercepted against slot 0
expecting a `Pair` object and instead found the raw (and already-corrupted)
reference, throwing `ClassCastException`.

Apache httpclient's `AbstractExecutionAwareRequest.abort()` (used
transitively by the ES low-level `RestClient`'s async request path via
`Cancellable`/`HttpRequestBase`) is a `while (!cancellableRef.isMarked())`
spin loop that calls `cancellableRef.compareAndSet(c, c, false, true)` and
only exits once `isMarked()` observes `true`. With the mark write
permanently dropped, `isMarked()` never became `true` and the loop spun
forever, burning CPU and starving CratonVM's HTTP dispatcher threads —
this is what produced the `testManyAsyncRequests` "timeout waiting for
requests to be sent" failure (interpreter mode) and is a very plausible
contributor to the JIT SIGSEGV (see below): a JIT-compiled `putfield` on
this same out-of-bounds slot 1 does not necessarily have the interpreter's
guarded bounds check, so it can corrupt adjacent heap memory directly
instead of safely dropping the write.

Fix: reimplemented `AtomicMarkableReference`'s native overlay to match the
real 1-field `Pair`-based layout, mirroring the (already-correct) pattern
already used for `AtomicStampedReference` in the same file
(`asr_alloc_pair`/`asr_read_pair`, fixed in an earlier session for the
identical bug class). New `amr_alloc_pair`/`amr_read_pair` allocate/read a
real-layout `AtomicMarkableReference$Pair { reference@0, mark@1 }` object
and store *that* in the AMR instance's single slot 0. Also added the
missing `get([Z)Ljava/lang/Object;` native registration.

Verified with a minimal, dependency-free repro (`AtomicMarkableReference`
construct + `compareAndSet`/`set`/`get`/`attemptMark`) matching real JDK
25 output exactly post-fix (previously: mark writes were dropped, and
`get(boolean[])` threw `ClassCastException`).

Commit: `72a8e406` on branch `fix/es-restclient-singlehost-timeout-20260710`.

### Interpreter (`--nojit`) — timeout FIXED

`RestClientSingleHostIntegTests`: was `FAIL, rc=1, 32.153s,
"timeout waiting for requests to be sent"`. Now completes in ~4s with 13
tests run, 2 failures — both unrelated pre-existing bugs
(`testPreemptiveAuthEnabled`, `testAuthCredentialsAreNotClearedOnAuthChallenge`
assert `Authorization` header starts with `"Basic"` but is `null` — a
separate preemptive-Basic-auth-caching gap, not part of this family; not
investigated further here). `testManyAsyncRequests` and
`testCancelAsyncRequest` (the two methods that exercise the
`AtomicMarkableReference`-backed cancellation/abort path) both PASS.

`RestClientMultipleHostsIntegTests` (a sibling class using the same
low-level client machinery): now 4/4 PASS under `--nojit` (previously noted
elsewhere as 3/4 — this fix appears to have resolved that residual too,
though it was not this session's primary target).

### JIT — original SIGSEGV crash FIXED, but a SEPARATE JIT-only residual remains

`RestClientSingleHostIntegTests` under JIT: was `CRASH, rc=139 (SIGSEGV),
15.754s, no Java-level exception captured`. Now consistently `rc=1` across
3 repeated runs — a normal JUnit failure report, no crash. The crash is
fixed by this same change: the out-of-bounds slot-1 write that the
interpreter safely dropped is a plausible (and no longer reachable, since
the write is now in-bounds) JIT SIGSEGV cause.

However, `RestClientSingleHostIntegTests` and `RestClientMultipleHostsIntegTests`
do **not** fully pass under JIT — several test methods (including
`testManyAsyncRequests`) throw bare `NullPointerException`s (no message, no
useful stack trace) inside Apache httpasyncclient's connection-pool release
path, a failure mode that does **not** reproduce under `--nojit` and is
unrelated to `AtomicMarkableReference`. This is tracked as a new, separate,
OPEN issue:
`docs/known-issues/elasticsearch-suite/ES-FAIL-20260710-restclient-jit-connectionpool-npe.md`.

## Original signal (2026-07-09 probe)

- `java.lang.AssertionError: timeout waiting for requests to be sent`

Full rerun count:
- Run: `es-nonpassed-rerun-20260708-191002`
- HTTP connection/timeout FAIL rows: 2 of 1064 total FAIL rows.
- Rows: `RestClientGzipCompressionTests` with `ConnectionClosedException`, and `RestClientSingleHostIntegTests` with the async timeout.

Current-dev proof (2026-07-09):
- Probe run: `es-faildocs-probe-20260709-073704`
- HotSpot `RestClientGzipCompressionTests`: PASS, rc=0, 0.835s.
- HotSpot `RestClientSingleHostIntegTests`: PASS, rc=0, 0.941s.
- CratonVM JIT `RestClientGzipCompressionTests`: PASS, rc=0, 8.113s.
- CratonVM JIT `RestClientSingleHostIntegTests`: CRASH, rc=139, 15.754s, no Java-level exception captured.
- CratonVM --nojit `RestClientGzipCompressionTests`: PASS, rc=0, 8.107s.
- CratonVM --nojit `RestClientSingleHostIntegTests`: FAIL, rc=1, 32.153s, `timeout waiting for requests to be sent`.

## Regression check (2026-07-10, post-fix)

- `RestClientGzipCompressionTests`: PASS under both `--nojit` (5/5, 0.379s)
  and JIT (5/5, 0.252s) — no regression.
- Rest of `client/rest` test package (`--nojit`): all classes pass except
  `RestClientMultipleHostsTests`, `RestClientTests`, `RestClientSingleHostTests`
  — all three fail solely on a pre-existing, unrelated Mockito/ByteBuddy gap
  (`AbstractMethodError: java/lang/reflect/TypeVariable.getAnnotatedBounds()
  ... has no Code attribute`, surfacing as `MockitoException: cannot mock
  ... CloseableHttpAsyncClient`), not touched by this fix.
