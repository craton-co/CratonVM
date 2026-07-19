# DoHead family — post-fix sporadic residuals (OPEN)

**Status: OPEN 2026-07-18.** The earlier 2026-07-17 closure was not
sufficient: a fresh isolated build still shows low-rate HTTP response loss and
read timeouts in the 64-class DoHead matrix. This record has consequently been
restored to `docs/known-issues` in accordance with the project issue policy.

The identity-hash collision hardening remains part of the current work. It
buckets native socket state by the stable hash, disambiguates by `ObjectRef`,
and roots/remaps references across moving GC. It does not, by itself, eliminate
the remaining transport residuals.

## 2026-07-17 superseded closure evidence

- Commit: `fbd790c7 fix(nio): disambiguate identity hash side tables`.
- Remote probe binary: `/data/data/cvm-dohead-postfix-eintr9-20260717`.
- Exact closure matrix: `/data/data/dohead-postfix-eintr9-full-20260717`.
  Configuration: 64 classes, one pass, two processes, `-Xmx1g`, 900-second
  class timeout. `summary.txt` contains 64 `PASS` records, zero `FAIL`,
  `TIMEOUT`, or `CRASH` records, and ends with `ALL_DONE` at 16:58:17 UTC.
- The prior focused pressure reproducer
  `TestHttpServletDoHeadInvalidWrite511ValidWrite511` passed eight concurrent
  c9 runs (two independent lanes, four passes each), including the c7/c8
  header, HTTP/2 EOF, and selector-stall trigger.
- `cargo test -p cratonvm-native-io --lib -- --test-threads=1`: 349 passed,
  zero failed.

The sections below are the historical open checkpoint and observations that
led to this resolution.


## 2026-07-16 closure attempt checkpoint

**Status: OPEN.** This note must stay in `docs/known-issues`: the current
two-process / 1 GiB Windows stress oracle still produces sporadic HTTP/2
mid-frame EOFs. In the final observed eight-class batch,
`TestHttpServletDoHeadInvalidWrite0ValidWrite1` failed twice and
`TestHttpServletDoHeadInvalidWrite0ValidWrite511` failed once, each with
`End of input stream with [9] bytes left`, out of 288 parameterizations.
Each was in `testDoHeadHttp2`; the server log had no application exception.
Immediate isolated reruns of `0 -> 1`, `0 -> 511`, and `1 -> 1023` passed,
including a two-process rerun of `1 -> {1023,1024}`. This is a low-rate
runtime transport residual, not a completed closure.

### Changes in this checkpoint

1. Prevented a late synthetic `URI.toURL()` registration from replacing the
   real URL-aware implementation. That removed
   `NoSuchMethodError: java/lang/Object.toExternalForm()` and the associated
   dropped HTTP/2 responses. The exact `1023 -> 512` class passed 288/288 and
   the focused `511/512/513/1024` group passed 1,152/1,152.
2. Pinned `URLClassLoader` construction inputs through allocating
   initialization steps, including `ucp` creation, to address the observed
   `WebappLoader.buildClassPath` null-`ucp` path under moving GC.
3. Made in-flight selector close wakeups return normally instead of throwing
   `ClosedSelectorException` into Tomcat `Poller.destroy`. The focused native
   selector suite passed 24/24, and the full `1 -> *` pressure batch no longer
   showed the LifecycleException.
4. Rooted scalar and gathering `SocketChannel` Java buffers across native I/O
   and reload them before buffer updates. `cargo check -p cratonvm-native-io`
   passes. The gathering-write change has not yet been built into a fresh
   release binary or credited as a fix for the remaining EOF residual.

### Evidence and next gate

- `cargo test -p cratonvm-native-builtins --lib`: 2,998 local and 2,999
  isolated-Azure passes before this checkpoint's final socket changes.
- `cargo test -p cratonvm-native-io --lib selector -- --test-threads=1`:
  24 passed after the selector change.
- Four early two-process batches passed 9,216 parameterized cases. A later
  batch exposed the selector, header, and HTTP/2 residuals; the first two were
  removed by the changes above, while EOF remains sporadic.

Do not archive or move this document until a newly built binary containing the
gathering-write root fix completes the full 64-class two-process matrix without
transport, header, selector, loader, or native-stack residuals.

**Historical status:** OPEN (low priority, low rate). **Severity:** low. **Context:**
after the 2026-07-15 fixes (thread-identity aliasing + dying-thread
card-buffer loss, see
`docs/internal/tomcat-08-07/dohead-residual-http2-midrun-hang-FIXED.md`),
the 64-class family was swept plus ~10 further full-class runs — roughly
21,000 Tomcat start/stop cycles on a heavily loaded box (3 concurrent
suite runs, active cryptominer infection). The freed-while-live disease is
gone (corruption telemetry silent). What remains is a catalogue of
UNRELATED singletons, each appearing 1-2 times total, each with ZERO
`gen_heap` containment / stale-pointer telemetry in its run:

1. **WinSock 10053 connection abort mid-read** (2×) — the long-documented
   environmental host-abort flake family (present at the same rate in every
   historical sweep, incl. pre-regression baselines).
2. **`LifecycleException: Protocol handler stop failed` ←
   `IOException: ClosedSelectorException` at `NioEndpoint$Poller.destroy`**
   (2×: one at `-MaxHeap 2g`, one in the family sweep) — a teardown
   ordering race: the poller's selector is already closed when `destroy()`
   runs. Plausibly a CratonVM NIO selector close/wakeup ordering nit;
   worth a look if it climbs above singleton rate.
3. **Sporadic header-count `AssertionError`** (`expected:<4> but was:<3>`,
   `expected:<2> but was:<0>`; 2×, non-reproducing on immediate rerun) —
   response header set off-by-N under heavy load; distinct from the FIXED
   deterministic OSW eager-flush `<2> vs <3>` cluster (that one was
   152/288 deterministic; these are 1/288 singletons).
4. **`IOException: End of input stream with [9] bytes left`** (1×) —
   mid-read disconnect singleton, historical environmental family.
5. **`NPE: Cannot invoke URLClassPath.getURLs() because this.ucp is null`**
   at `WebappClassLoaderBase.getURLs` ← `WebappLoader.buildClassPath`
   during context start (1×) — a `URLClassLoader` observed before/without
   its `ucp` being initialized. Not the (fixed) null-parent HTTP loader
   issue. Candidate: constructor-bypass or field-init ordering on the
   `WebappClassLoader` subclass path.
6. **`EXCEPTION_STACK_OVERFLOW` on an http-exec thread** (1×,
   `TestHttpServletDoHeadInvalidWrite512ValidWrite513` param 3, faulting
   RVA `0x129C76D` on the session's pre-merge binary, symbol unresolved) —
   native-side stack exhaustion, no Java SOE trace printed. Not the fixed
   `ScheduledThreadPoolExecutor.shutdown` self-recursion (that fix was in
   the binary). Needs its own capture (`--stack-dump-on-timeout` won't
   help; a live cdb attach or a bigger `[rust] stack` diagnostic would).

## Reproduction

Standard family runs, e.g.:

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -RunName x -TimeoutSec 900 `
  -Parallel 2 -MaxHeap 1g -ClassFilter 'TestHttpServletDoHeadInvalidWrite'
```

Expect ≥95% of classes 288/288; the shapes above appear as isolated
287/288 singletons (or the rare crash face #6). None reproduced on a
targeted rerun of the affected class this session.


## 2026-07-18 clean-worktree progress and current residuals

**Status: OPEN.** The final clean attempt was run from a fresh worktree
`/data/wt-dohead-residuals-clean-20260718` on branch
`fix/dohead-residuals-clean-20260718`, created directly from
`origin/dev` at `ef60e4792`. This was necessary because the older purportedly
isolated worktree acquired unrelated concurrent edits; its C17 binaries and
results are discarded as invalid evidence.

### Current changes under test

1. Collision-safe, GC-rooted buckets for the native server-socket port table
   and the `SocketChannel` wrapper/back-reference table. Rows are matched by
   the actual Java `ObjectRef`, not only its non-unique identity hash.
2. Selector close now writes its wakeup byte, retains epoll/self-pipe handles
   while `epoll_wait` is in flight, and releases those handles only after the
   final in-flight selector call returns. This removes the direct close-vs-wait
   descriptor-reuse race; it does not yet explain every HTTP transport loss.

The clean selector regression suite passed:

```text
cargo test -p cratonvm-native-io --lib nio_selector -- --test-threads=1
20 passed; 0 failed
```

The fresh release binary was
`/data/data/cvm-dohead-clean-c18-20260718`, built with unique target directory
`/data/data/target-dohead-clean-c18-20260718`. A six-pass focused run of the
previous C17-only `0 -> 1024` failure class was clean:
`/data/data/dohead-clean-c18-0-1024-focus-20260718` (6/6 PASS).

### Full clean matrix — residuals remain

The decisive run
`/data/data/dohead-clean-c18-full-20260718` used two processes, `-Xmx1g`, one
pass, and a 900-second per-class timeout. It completed all 64 classes and
ended `ALL_DONE`, but reported four failures (60 pass, 4 fail):

| Class | Parameterized failure face |
| --- | --- |
| `1023 -> 1` | `HttpURLConnection response failed: connection closed before response head` |
| `0 -> 1025` | `SocketTimeoutException: Read timed out` |
| `513 -> 1025` | `SocketTimeoutException: Read timed out` |
| `513 -> 512` | `SocketTimeoutException: Read timed out` |

Each failing class completed 288 parameterizations with one failing case. The
retained logs are the corresponding `p1-*.log` files under the matrix output
above. The host load was modest during the later sweep (roughly 5–10 on the
16-core host), so these cannot be dismissed as only the earlier severe host
contention. No DoHead probe process remained after the matrix; the requested
post-attempt cleanup also found no matching runner or binary process.

**Next diagnostic gate:** trace the server-side close/selector and socket I/O
sequence for the four residual faces from this clean branch. Do not move this
record back under `docs/internal` or claim the family fixed until a newly built
clean binary completes the same full 64-class matrix with zero transport,
header, timeout, selector, loader, or native-stack residuals.

## 2026-07-18 C20/C21 follow-up — interest-change wakeup and narrowed residuals

**Status: OPEN.** C20 adds a Linux selector interest-change nudge: after a
successful `epoll_ctl(EPOLL_CTL_MOD)`, `selector_set_interest()` writes one byte
to the selector self-pipe without setting the public sticky `woken` flag. This
addresses the specific partial-gathering-write interstice in which Tomcat arms
`OP_WRITE` while its poller is already blocked in `epoll_wait`; `epoll_ctl(MOD)`
alone does not reliably wake that wait. The earlier retained HTTP/2 close trace
showed the whole response body followed by only a partial final GOAWAY frame,
which is consistent with the last readiness transition not being observed before
the connection is closed.

The change is intentionally limited to an internal readiness re-check: it does
not make `Selector.wakeup()` sticky and therefore does not alter its Java-visible
return contract. The focused regression suite passed:

```text
CARGO_TARGET_DIR=/data/data/target-dohead-c20-test-20260718 \
  cargo test -p cratonvm-native-io --lib nio_selector -- --test-threads=1
20 passed; 0 failed
```

The unique C20 binary was `/data/data/cvm-dohead-c20-writewakeup-20260718`.
Its focused two-process, four-pass `0 -> {1,1023,1024,1025}` exercise completed
16/16 class runs clean at
`/data/data/dohead-c20-writewakeup-0-1-family-n2x4-20260718`, including the
previously retained HTTP/2 EOF face. This is strong targeted evidence, but not a
full-family closure.

The first full C20 matrix reached 42 classes and failed at
`TestHttpServletDoHeadInvalidWrite1ValidWrite513`, parameter
`testDoHead[39: 0 false true 16 false 1 BUFFER 513 true]`, with the HTTP/1
header-count assertion `expected:<4> but was:<3>`.

### C21 wire/header diagnostic result

An uncommitted diagnostic-only `HttpURLConnection` header trace was built as
`/data/data/cvm-dohead-c21-hucdiag-20260718` and removed before this commit.
The retained run is
`/data/data/dohead-c21-hucdiag-1-family-n2x6-20260718`. It completed all eight
`1 -> {0,1,511,512,513,1023,1024,1025}` classes in pass 1 and the same set in
pass 2 before the harness stopped on its first failed exit. It recorded:

| Pass/class | Exact residual |
| --- | --- |
| pass 1, `1 -> 0` | Header-count failure at `testDoHead[60: 0 false true 16384 false 1 NONE 0 false]`: expected 4, got 5. The raw GET and HEAD headers were identical five-entry lists (`Content-Type`, `Content-Length`, `Date`, `Keep-Alive`, `Connection`). |
| pass 1, `1 -> 512` | `connection closed before response head` on HEAD, parameter 79. |
| pass 2, `1 -> 0` | `SocketTimeoutException: Read timed out` on HEAD, parameter 15, after 361.828 seconds. |

The raw-header result rules out the HTTP response parser and wire transport as
the direct cause of the header-count failure. In the interval after the matching
headers are returned and before the assertion, the VM logged guarded
out-of-bounds `get_field`/`set_field` accesses on zero-field `java/lang/Object`
receivers. The next implementation gate is therefore the Java map/collection
copy path (`HttpURLConnection.getHeaderFields()` -> `CaseInsensitiveKeyMap`),
including its object-layout/receiver handling, while separately retaining the
selector wakeup change for the HTTP/2 partial-write face.

After this diagnostic attempt completed, every running DoHead VM process was
terminated; a `/proc` executable-and-command-line sweep confirmed that no DoHead
VM process remained.

## 2026-07-18 C23-C29 map-layout closure and remaining transport residuals

**Status: OPEN.** This checkpoint closes the sporadic HTTP header-map loss, but
does not yet close the independent HTTP/2 EOF/timeout family. The document
therefore remains under docs/known-issues.

### Validated changes

1. The selector phase-3 readiness safety-net now checks each missing interest
   bit rather than treating any pre-existing ready bit as complete. Together
   with C20's non-sticky post-epoll_ctl(MOD) self-pipe nudge, the focused
   selector suite remains clean: cargo test -p cratonvm-native-io --lib
   nio_selector -- --test-threads=1 reported 20/20.
2. HttpURLConnection.getHeaderFields() now uses HashMap consistently rather
   than constructing a LinkedHashMap while invoking HashMap.put.
3. The decisive header bug was in native collection storage, not the response
   parser: map_alloc_node() allocated ClassId(0) / java/lang/Object nodes and
   then wrote HashMap node fields. The heap now correctly gives Object a
   zero-slot layout, so those writes were guarded/dropped, intermittently
   removing map entries such as Date. Nodes are now allocated as
   java/util/HashMap$Node.
4. Live entry snapshots root key, value, and source-map references across their
   allocating entry creation, preventing pre-move references from being
   published after a moving collection.

### Evidence

- C28's two-class pressure run reproduced the old header error once (23/24)
  and emitted the zero-slot guard records. It established that only rooting the
  live entry helper was insufficient.
- C29 binary: /data/data/cvm-dohead-c29-mapnode-20260718.
- C29 targeted pressure:
  /data/data/dohead-c29-mapnode-headerpair-n2x4-20260718;
  1 -> 1025 and 1025 -> 1025, two processes, four passes: **8/8 PASS**,
  zero failures. The completed attempt left no DoHead process.
- Focused native validation:
  cratonvm-native-collections map filter **20/20**, and
  cratonvm-native-builtins http_url_connection **30/30**.
- C29 full JIT matrix:
  /data/data/dohead-c29-mapnode-full64-n2x1-20260718; 64 classes, two
  processes, -Xmx1g, one pass, 240-second class timeout, ALL_DONE.
  Result: **61 PASS, 3 residuals**:
  - 511 -> 1023: HTTP/2 End of input stream with [9] bytes left.
  - 512 -> 0: timeout.
  - 513 -> 511: timeout.
  The first EOF coincided with a guarded zero-slot write at index 4, so the
  remaining transport diagnosis must identify that distinct raw-object layout
  producer before assigning the failures to selector or network timing.

After the C29 matrix finished, the explicit DoHead process sweep found no
matching runner or VM process. Do not move this record to docs/internal until
a newly built binary completes the 64-class matrix with zero residuals,
followed by a relevant --nojit control.

## 2026-07-18 C32 system-environment map node residual

C32 closes the remaining zero-slot HashMap node producer in
native-builtins/lang_system. System.getenv() could receive an apparent
HashMap$Node initialization success carrying Object class ID 0; its field
writes were silently dropped. The path now unconditionally obtains the named
synthetic HashMap$Node layout. C32 mixed transport pressure
(511->1023, 512->0, 513->511; two processes, eight passes) completed 24/24
PASS with ALL_DONE and no matching DoHead process remaining.

## 2026-07-18 C33 full-matrix checkpoint

**Status: OPEN.** C32's system-environment map-node fix removes the guarded
zero-slot node writes seen in the previous matrix, but it does not yet close
the entire DoHead family. The document remains under `docs/known-issues`.

- C32 focused JIT pressure:
  `/data/data/dohead-c32-systemnode-transport-n2x8-20260718`, covering
  `511 -> 1023`, `512 -> 0`, and `513 -> 511` with two processes and eight
  passes, completed **24/24 PASS** with `ALL_DONE`.
- C32 full JIT matrix:
  `/data/data/dohead-c32-systemnode-full64-n2x1-20260718`, 64 boundary
  classes, two processes, one pass, `-Xmx1g`, and a 240-second class timeout,
  completed with `ALL_DONE`: **63 PASS, 1 FAIL**.
- The sole residual is `1023 -> 0`, parameter
  `testDoHead[29: 0 false false 16,384 false 1,023 FULL 0 true]`, which
  asserts three headers but receives two (`expected:<3> but was:<2>`). This is
  an HTTP/1 FULL/keep-alive header-map loss, distinct from C29's zero-slot
  producer and from the C32 focused transport cases.
- Completion cleanup found no matching DoHead VM or runner process. No
  `--nojit` control was run because the JIT full matrix remains non-zero.

## 2026-07-18 C34 System.getenv fallback-layout closure

**Status: OPEN.** The C34 fallback allocation repair closes C33's `1023 -> 0`
header-map loss, but the independent partial-write/transport family remains.

- The `System.getenv()` legacy fallback allocated a three-slot HashMap and
  four-slot HashMap$Node with `ClassId(0)` after real layout resolution failed.
  C34 now uses named synthetic layouts for both objects.
- Exact JIT stress for the former residual:
  `/data/data/dohead-c34-systemenvfallback-1023to0-n2x8-20260718`, two
  processes and eight passes, completed **8/8 PASS** with `ALL_DONE`.
- C34 full JIT matrix:
  `/data/data/dohead-c34-systemenvfallback-full64-n2x1-20260718`, completed
  **60 PASS, 4 FAIL**. `1023 -> 0` now passes. Current residuals are
  `1023 -> 511`, `1024 -> 1023`, and `1 -> 1025` (each EOF with nine bytes
  left), plus `512 -> 1` (expected HTTP 200, got -1). Two residual logs retain
  guarded zero-slot accesses at field indices 4 or 5; their producer remains
  to be identified before changing selector behavior.
- Completion cleanup found no matching DoHead VM or runner process. No
  `--nojit` control was run because the JIT full matrix remains non-zero.

## 2026-07-18/19 C35-C38 — OSR exception-table regression found and fixed;
## sporadic header-count family confirmed still open, unrelated

**Status: OPEN**, but the family is smaller than it appeared going into this
round. This session took over the investigation in a fresh continuation
worktree (`/data/wt-dohead-mapresiduals-20260718`, fast-forwarded to
`origin/dev`), since the worktree named in the original hand-off
(`cvm-dohead-postfix-residuals-20260717`) was the already-abandoned,
contaminated worktree referenced above under the 2026-07-18 clean-worktree
entry, not the active lineage.

**A fresh C35 baseline build at then-current `dev` HEAD (`139ac143e`, 164
commits past the C34 checkpoint) showed 39 PASS / 25 FAIL on the full 64-class
matrix — a massive, 100%-deterministic regression, not the low-rate sporadic
family this document tracks.** Every failure shared the exact same shape:
`useWriter=false` (raw `OutputStream`, not `PrintWriter`), `resetType` in
`{BUFFER, FULL}` (never `NONE`), and enough `invalidWriteCount` bytes to
overflow the response buffer before the reset call. `--nojit` passed the
same class 288/288, isolating it to JIT.

**Root cause:** `compile_osr_artifact()` in `vm/src/runtime/interpreter.rs`
compiles a method via on-stack-replacement (OSR) when a hot loop inside it
triggers tiered compilation mid-execution. Unlike the method-entry JIT path
(which already bails whenever the method has a non-empty exception table),
the OSR path only bailed on `scan.has_athrow` (the method throwing directly)
— it did **not** bail when the method merely *contains* a `try/catch` around
a call to something else that throws. `compile_with_param_slots` (the OSR
backend entry point) has no exception-table parameter at all, so an
OSR-compiled artifact **never** carries handler ranges: when a callee (e.g.
`Response.resetBuffer()`, throwing a completely normal, real-bytecode
`IllegalStateException`) unwinds into an OSR-compiled caller frame, the
runtime finds no matching catch and the exception escapes uncaught, even
though the source has a textually-correct `catch (IllegalStateException)`.
Tomcat's container then aborts the connection mid-response, which is what
surfaced as `HttpURLConnection response failed: chunked: socket closed
mid-header` on the client.

This bug is not new, but was masked: before `d6f642695` (part of the
`perf/halfgap-20260717` round, landed between the C34 checkpoint and this
session), **any** method referencing an `ldc` string constant was
unconditionally OSR-denied, so a method like `HeadTestServlet.doGet`
(string constants for `"* invalid data *"`, `"text/plain"`, etc., plus a hot
`for` loop over `invalidWriteCount`) never got OSR-compiled and always ran
this code path interpreted (correctly). Once `d6f642695` fixed that
unrelated OSR-denial bug, `doGet` started succeeding OSR compilation and
immediately exposed the pre-existing exception-table gap.

**Fix** (`eda677f45`, merged to `dev` as `62693f104`): added an RBC.6b guard
in `compile_osr_artifact` — look up the method's real Code attribute and
bail OSR (permanently bail-list, matching the sibling RBC bails) whenever
its `exception_table` is non-empty, mirroring the method-entry gate exactly.
Verified:
- Isolated reruns of `TestHttpServletDoHeadInvalidWrite1023ValidWrite0`:
  3/3 PASS pre-merge, 3/3 PASS post-merge (after merging 5 unrelated
  concurrent commits from other sessions into this branch).
- Full 64-class matrix: 39 PASS / 25 FAIL (pre-fix) → 58 PASS / 6 FAIL
  (post-fix, pre-merge) → 55 PASS / 9 FAIL (post-fix, post-merge, different
  run). The post-fix failures are NOT the class this fix targeted (which
  passed cleanly in both post-fix runs) and are NOT reproducible in
  isolation (4/4 clean reruns of one).

**The remaining post-fix failures are the pre-existing sporadic family this
document already tracks**, not a new regression:
- 8 of 9 residuals in the post-merge full-matrix run were the historical
  header-count off-by-one assertion (`expected:<N> but was:<N±1>`, both
  directions), always exactly 1/288 per class, always on the `useWriter=true`
  (`PrintWriter`) path — a different code path than the OSR bug this session
  fixed.
- 1 was the historical `IOException: End of input stream with [9] bytes
  left to read` EOF singleton (same family as the original 2026-07-15
  catalogue's item 4 and the C29/C34-era EOF residuals).
- Checked for correlation with the zero-slot/`ClassId(0)` map-node bug
  family (the one C29/C32/C34 closed several producers of): the
  `cratonvm::gc::guard` WARN lines present in these failing logs are a
  fixed 4-line pattern (`index=3,1,0,1` on the same two objects) that
  appears identically at the *start* of every test case in every class,
  pass or fail — routine JUnit/Tomcat-bootstrap noise, not correlated with
  the actual failure. A narrow always-on backtrace diagnostic was added to
  `gc/src/gen_heap.rs`'s OOB guards (fires unconditionally, no env var
  needed, only for the specific `num_slots == 0 && index ∈ {4, 5}` shape
  matching the C34-era residual note) to help pin this down in a future
  round if it recurs with that exact shape; it did not fire in this
  session's repro attempts, so the current header-count residual likely has
  a different root cause than the already-fixed map-node producers.

**Next diagnostic gate:** the header-count residual needs its own repro
strategy — single-class isolated reruns don't reproduce it (confirmed 4/4
clean), so it likely needs sustained multi-pass, multi-process pressure
across the full class list (matching how it was originally found) rather
than a targeted single-class rerun. Do not move this record out of
`docs/known-issues` until a full 64-class two-process matrix completes with
zero residuals of any kind, followed by a `--nojit` control.
