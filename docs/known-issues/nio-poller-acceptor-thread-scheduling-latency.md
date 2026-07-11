# NioEndpoint Acceptor/Poller threads appear serialized — ~2s latency before a freshly-accepted connection's first read

**Status:** OPEN. **Severity:** potentially high — this is not Tomcat-specific;
any multi-threaded application with one thread parked in a long blocking
native call (e.g. `Selector.select(timeout)`) while another thread needs to
make independent progress (e.g. `ServerSocketChannel.accept()`) may be
affected. **HotSpot:** not affected (reacts within milliseconds).

## Summary

Found while root-causing
[`tomcat-08-07/nonblockingreadignoreisready-async-error-response-completion-gap.md`](tomcat-08-07/nonblockingreadignoreisready-async-error-response-completion-gap.md).
That doc's own theory (container fails to flush an implicit response) is
**refuted** — see the update added to that doc. The real mechanism: CratonVM
takes roughly **2 seconds** from a client's `connect()` to the server
performing its first successful read of that connection's data, even though
the client sends its first bytes immediately. On real HotSpot the server
reacts within single-digit milliseconds. For any test/scenario that paces
writes faster than ~2s apart (a very common pattern for exercising
non-blocking-I/O edge cases), the client finishes its entire request *and
closes the connection* before CratonVM's Tomcat ever looks at it — the
server-side logic that the test is trying to exercise (isReady()-driven
partial reads, async error handling, etc.) never actually gets exercised
with genuinely partial/incremental data; it only ever sees a fully-arrived,
already-closed stream.

## Evidence

Correlated a Rust-side timeline (`eprintln!` added temporarily to
`native-io/src/socket_channel.rs::sc_read`/`ssc_accept` and
`native-io/src/nio_selector.rs::selector_select_native`/
`channel_register_native`, all keyed to
`SystemTime::now().duration_since(UNIX_EPOCH)` so they can be merged with
Java-side `System.currentTimeMillis()` prints) against a Java-side timeline
(temporarily instrumented copies of
`test/org/apache/catalina/nonblocking/TestNonBlockingAPI.java`'s
`DataWriter.next()` and `test/org/apache/catalina/startup/TomcatBaseTest.java`'s
`postUrl()`, recompiled with `javac` and placed first on the classpath —
no changes to the real, committed Tomcat fixture sources).

Repro: `org.apache.catalina.nonblocking.TestNonBlockingAPI.testNonBlockingReadIgnoreIsReady`
(or `testNonBlockingRead` — same `DataWriter(500, 5)` pacing, unrelated to
the `ignoreIsReady` flag that the other doc is about) via a small
`Request.method(Class, String)` + `JUnitCore` single-method runner:
```
$JAVA -cp "<javaout-with-instrumented-classes>;<cp.txt>" SingleMethodRunner \
  org.apache.catalina.nonblocking.TestNonBlockingAPI testNonBlockingReadIgnoreIsReady
```

A representative merged timeline (epoch ms):
```
[Rust ] selector_select_native ENTER id=1 timeout=1000 t=675793
[Rust ] selector_select_native EXIT  id=1 n=0        t=676799  dt=1006   <- full timeout, nothing happened
[Rust ] selector_select_native ENTER id=1 timeout=1000 t=676799
[Rust ] selector_select_native EXIT  id=1 n=0        t=677798  dt=999    <- full timeout again
[Rust ] selector_select_native ENTER id=1 timeout=1000 t=677799
[Rust ] ssc_accept ACCEPTED peer=127.0.0.1:62837     t=677801            <- accept() succeeds 2ms after the 3rd select starts
[Rust ] selector_select_native EXIT  id=1 n=0        t=677812  dt=13
[Rust ] channel_register_native net_fd=... ops=1     t=677813
[Rust ] selector_select_native ENTER id=1 timeout=1000 t=677813
[Rust ] selector_select_native EXIT  id=1 n=1        t=677813  dt=0      <- now fast: registered key found ready instantly
[Rust ] sc_read id=... n=178                          t=677838            <- full request (headers+complete body) in ONE read
```
The client's own write loop (separately, Java-side trace) shows genuine
~500ms-paced `Thread.sleep`-separated writes completing well before this —
i.e. **by the time the server does anything, the client has already
finished writing all 5 chunks and closed its stream.** ~2 seconds elapse
(two full `NioEndpoint` `selectorTimeout=1000`-ms cycles, back to back) with
the Acceptor's `accept()` apparently making **no progress** while the Poller
thread is separately parked in blocking `select()` calls — then, within
single-digit milliseconds of the Poller's *third* `select()` call starting,
`accept()` succeeds, registration happens, and the connection is read and
processed to completion. The fast tail (register → data-ready in 0ms, read
of the complete 178-byte request in one shot) is consistent and reproduces
every time; it's specifically the *initial* ~2s stall before the Acceptor
makes progress that's the anomaly.

## What's been ruled out

- **Not a JIT/interpreter warm-up artifact.** Running a normal warm-up test
  (`testNonBlockingRead`, ~9s, fully warms the connector/JIT) in the *same*
  JVM process immediately before `testNonBlockingReadIgnoreIsReady` (via a
  small multi-method runner reusing one `JUnitCore` instance) does **not**
  change anything — the second test still takes ~2.2s and fails identically.
  Each test creates its own fresh `Tomcat`/`NioEndpoint` instance, so this
  latency recurs per-`NioEndpoint`, not just on process cold-start.
- **Not a wakeup() correctness bug in isolation.** Direct Rust unit tests
  against `native-io/src/nio_selector.rs`:
  - `selector_wakeup()` reliably interrupts an in-progress blocking
    `selector_select(id, 10_000)` call within ~50ms when called from another
    thread (existing test `t19_7_a_wakeup_unblocks_concurrent_select`,
    confirmed with real elapsed-time instrumentation, not just the test's
    own (surprisingly loose) `<2s` assertion bound — actual: ~51ms).
  - A **new** test added during this investigation,
    `diag_register_new_stream_with_data_while_blocked_in_select` (in
    `native-io/src/nio_selector.rs`, **not currently committed** — see
    "Reproducing the diagnosis" below to re-add it), confirms: registering a
    brand-new stream (with data already written to it) *while* another
    thread is blocked in `select()`, then calling `wakeup()`, correctly and
    quickly (~51ms) interrupts the blocked call (which legitimately returns
    0 — the new key wasn't in that call's snapshot) — and a **second, fresh**
    `select()` call immediately afterward (33.8µs) correctly reports the new
    stream as ready. So: wakeup interrupts promptly, registration is
    immediate, and a fresh select() after registration is instant. None of
    the individual primitives are slow or wrong.
- **Not the request/response processing logic itself** — once the first
  read happens, the entire error-handling chain (`onDataAvailable` →
  `IllegalStateException` → `onError` → `onComplete` → `CLOSE_NOW` →
  socket close) completes in single-digit milliseconds, matching HotSpot's
  logic byte-for-byte (per the sibling doc's own finding).

## Not yet found

The exact mechanism causing the Acceptor thread to apparently make no
progress for ~2 seconds (two full Poller `selectorTimeout` cycles) while
the Poller thread is parked in blocking native `select()`/`WSAPoll` calls.
Candidates, none confirmed:
- A thread-scheduling/starvation issue specific to a thread parked in a
  long-timeout blocking native call (`WSAPoll` with `timeout_ms=1000`)
  somehow preventing *other* Java threads (here, the separate `Acceptor`
  thread doing its own independent blocking-but-short-poll `accept()` loop)
  from being scheduled/making progress concurrently.
- Something in the GC-safepoint/`begin_blocking_region()`/
  `end_blocking_region()` bracketing (used by both
  `selector_select_native` and `ssc_accept`'s blocking path) that
  inadvertently serializes threads using it, rather than truly letting them
  run independently.
- Something specific to `Thread.start()`/`Thread.setPriority()` handling for
  a freshly-created `Acceptor` thread (Tomcat sets
  `t.setPriority(Thread.NORM_PRIORITY)` before `.start()` — same priority as
  everything else on paper, but worth checking whether CratonVM's
  `setPriority0` native does anything unexpected).
- Something about `NioEndpoint.startInternal()`'s actual startup ordering
  under CratonVM specifically (Poller thread(s) started before Acceptor?
  Some other blocking step in between?) that isn't a "scheduling" bug per
  se but a genuine multi-second delay in reaching `startAcceptorThread()`.

None of these were tested — this needs someone with VM-core
threading/scheduling ownership, not a blind patch from a Tomcat-connector
investigation.

## Reproducing the diagnosis

1. Build a release `cratonvm.exe` from a fresh worktree branched off `dev`
   (see `reference_worktree_build_recipe` conventions — seed libffi-sys,
   `VCINSTALLDIR=" "`/`VSCMD_ARG_TGT_ARCH=" "`, prepend the MSVC linker dir).
2. Compile `SingleMethodRunner.java` (a ~15-line `Request.method` +
   `JUnitCore` wrapper) and drop it on the classpath alongside
   `apps/tomcat/.suite/cp.txt`.
3. Run `testNonBlockingReadIgnoreIsReady` (or `testNonBlockingRead`) once to
   confirm the ~2s-late first read (`CRATONVM_SOCKET_CAPTURE=<prefix>` also
   works but is lower-resolution than the eprintln timeline above).
4. To re-add the Rust-side timeline: `eprintln!` at the top of `sc_read`
   (only when `n > 0`), `ssc_accept` (right after a successful accept),
   `selector_select_native` (entry + exit, with `id`/`timeout`/result), and
   `channel_register_native` (entry) in `native-io/src/socket_channel.rs`
   and `native-io/src/nio_selector.rs` — all keyed on
   `SystemTime::now().duration_since(UNIX_EPOCH).as_millis()` for
   cross-process-run correlation. None of this instrumentation is currently
   committed (removed after use, per this repo's convention of not leaving
   debug scaffolding in shipped code) — re-add it fresh from this doc's
   description if picking this up.
5. For the Java-side timeline: copy
   `test/org/apache/catalina/nonblocking/TestNonBlockingAPI.java` and
   `test/org/apache/catalina/startup/TomcatBaseTest.java` to a scratch
   directory, add `System.err.println(...+System.currentTimeMillis())`
   calls to `DataWriter.next()` and `postUrl()`, `javac` them against
   `cp.txt`, and put the output directory **first** on the runtime
   classpath (shadows the originals; no changes to the committed fixture).

## Recommendation

Whoever owns CratonVM's thread-scheduling / GC-safepoint subsystem should
pick this up with proper instrumentation of `Thread.start()`/scheduling and
the `begin_blocking_region`/`end_blocking_region` machinery specifically for
the two-thread (Acceptor + Poller) interaction, rather than treating this as
a Tomcat/NIO-selector-specific bug. Given the potentially broad blast radius
(any app with a similar "one thread blocks on I/O with a timeout while
another thread needs to run concurrently" shape), this likely deserves
higher priority than its Tomcat-only trigger suggests.
