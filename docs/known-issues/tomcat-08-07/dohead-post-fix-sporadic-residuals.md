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
