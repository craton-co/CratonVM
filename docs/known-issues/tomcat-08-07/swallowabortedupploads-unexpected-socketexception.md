# TestSwallowAbortedUploads — client sees `SocketException` when none expected

**Status:** OPEN. **Severity:** medium. **HotSpot:** PASS.

## Summary

`org.apache.catalina.core.TestSwallowAbortedUploads.testAbortedPOSTOKSwallow`
fails:
```
java.lang.AssertionError: Unlimited upload with swallow enabled generates client exception
  expected null, but was:<java.net.SocketException: SocketException: Connection aborted: write0:
  Программа на вашем хост-компьютере разорвала установленное подключение. (os error 10053)>
```
(Russian OS text = "The program on your host computer aborted an established
connection" — a standard Windows WSAECONNABORTED message, localized.)

The test's premise: when the server aborts reading an oversized/invalid
upload but has "swallow input" enabled, the *client* should NOT see a
connection-reset exception while writing its POST body — the server is
expected to keep the connection alive and drain (swallow) the unwanted
upload bytes rather than abruptly closing the socket. On CratonVM, the
client's write does hit a hard `SocketException` (`os error 10053`,
`WSAECONNABORTED`), meaning the server side closed/reset the connection
instead of swallowing and draining it.

Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`, 2026-07-07/08).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName swallowup `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.catalina.core.TestSwallowAbortedUploads
```

## Recommendation

Investigate Tomcat's "swallow input" mechanism
(`org.apache.catalina.connector.Request`'s input-swallowing path /
`Http11Processor`'s handling of an aborted request body) under CratonVM —
check whether CratonVM's socket/connector layer is closing the connection
too eagerly (e.g. on an unhandled exception during body read) instead of
continuing to read-and-discard bytes per Tomcat's intended swallow
semantics. Likely a real-socket / NIO-connector behavioral gap rather than a
pure VM correctness bug — worth checking against other socket-abort-related
findings from this investigation (`TestAccessLogValve`/`TestRewriteValve`'s
`-1` symptom, if related).

## 2026-07-09 worker isolation

Candidate root cause confirmed at the native socket contract level:
`native-io/src/socket_channel.rs::sc_close` unconditionally called
`TcpStream::shutdown(Shutdown::Both)` before dropping a Tomcat NIO
`SocketChannel`. On Windows, closing the read side while the peer is still
uploading request-body bytes is abortive/RST-prone and matches the client-side
`WSAECONNABORTED` (`os error 10053`) observed here.

Candidate fix in branch `codex/fix-tomcat0807-http-close-20260709-001`:
`sc_close` now sends only the write-side FIN and starts a bounded background
drain on a cloned stream (`lingering_channel_close`) so a client can finish
writing already-in-flight upload bytes without receiving a reset. This preserves
the earlier selector-wakeup requirement (the peer still sees EOF) without using
`Shutdown::Both`.

Focused proof run:

```powershell
$env:CARGO_TARGET_DIR='C:\craton\target-tomcat0807-http-private-20260709-001'
cargo test -p cratonvm-native-io socket_channel::tests -- --nocapture
# 7 passed, including tomcat0807_http_lingering_channel_close_drains_peer_upload
```

Connector smoke run:

```powershell
# Unique binary:
# C:\craton\target-tomcat0807-http-private-20260709-001\debug\cratonvm-tomcat0807-http-20260709-001.exe
# Unique probe class:
# C:\craton\smoke-cache-tomcat0807-http-20260709-001\fixture\Tomcat0807HttpSmoke.java
# Result:
# TOMCAT0807_HTTP_BODY=TOMCAT0807_HTTP_SMOKE_OK
```

The full `apps/tomcat-suite-runner` checkout is not present in this worktree,
so `org.apache.catalina.core.TestSwallowAbortedUploads` itself still needs an
isolated rerun before this note can be archived.

## 2026-07-09 Linux re-verify: original symptom confirmed fixed; found 2 more
## blockers (both now resolved) and 1 new, still-OPEN blocker

Re-verified on the Azure Linux build host against the real Tomcat fixture
(`/data/data/apps/tomcat`, real JDK 25, `org.junit.runner.JUnitCore
org.apache.catalina.core.TestSwallowAbortedUploads`, all 10 test methods).

**Original `WSAECONNABORTED` symptom: confirmed FIXED.** `sc_close`'s
`lingering_channel_close` (described above) landed on `dev` via commit
`27c3e9ac` ("fix(tomcat): unblock HttpServlet/DoHead family...",
2026-06-29) — well before this re-verify. `native-io/src/socket_channel.rs`
on current `dev` no longer calls `Shutdown::Both` unconditionally. This part
of the bug is done; do not re-investigate the socket-close path.

**New blocker #1 (found + fixed elsewhere): `ScheduledThreadPoolExecutor.getQueue()`
NPE blocked the test from even starting.** Running the class end-to-end (not
just the socket-close path) surfaced a *different* bug blocking every one of
the 10 methods before any request was even sent:
```
org.apache.catalina.LifecycleException: Failed to start component [StandardEngine[Tomcat]]
Caused by: java.lang.NullPointerException: Cannot invoke "java.util.concurrent.BlockingQueue.add(Object)" because
  the return value of "java.util.concurrent.ThreadPoolExecutor.getQueue()" is null
	at java.util.concurrent.ScheduledThreadPoolExecutor.delayedExecute(ScheduledThreadPoolExecutor.java:347)
	at java.util.concurrent.ScheduledThreadPoolExecutor.scheduleWithFixedDelay(ScheduledThreadPoolExecutor.java:667)
	at org.apache.tomcat.util.threads.ScheduledThreadPoolExecutor.scheduleWithFixedDelay(ScheduledThreadPoolExecutor.java:134)
	at org.apache.catalina.core.ContainerBase.startInternal(ContainerBase.java:764)
```
Root cause: `StandardServer.reconfigureUtilityExecutor()` builds Tomcat's
shared "Catalina-utility-" pool via direct
`new java.util.concurrent.ScheduledThreadPoolExecutor(threads, threadFactory)`
(the real JDK class — Tomcat's own `org.apache.tomcat.util.threads.ScheduledThreadPoolExecutor`
is a thin wrapper delegating to it, not a subclass). CratonVM had **three**
separate, redundant native `<init>` overrides for
`java/util/concurrent/ScheduledThreadPoolExecutor` (`native-builtins/src/phases_early.rs::register_scheduled_executor_natives`,
a real-JDK-mode inline duplicate in `vm/src/vm/vm_init.rs`, and a third,
better one — `native-builtins/src/phases_late.rs::register_p63_scheduled_executor`
— that is only reachable from the synthetic/fake-JDK bootstrap path and
never gets registered in real-JDK mode at all). The two natives that *do*
run in real-JDK mode only ever wrote two legacy synthetic slots
(`corePoolSize`, `shutdown`) and never invoked the real `ThreadPoolExecutor`
constructor chain, so the real, inherited `workQueue` field was left `null`
— exactly the "synthetic native shadows real bytecode" pattern behind the
`File.FS`/`ThreadGroup` bugs (see
`docs/internal/fixed-suite-bugs/file-fs-native-clinit-never-set-FIXED.md` and
`.../threadgroup-native-field-index-mismatch-FIXED.md`).

This was found and fixed **concurrently by a different session** (working the
`EnumSet.of()`/`allOf()` websocket-close bug, branch
`codex/fix-tomcat-wsclose-enumset-20260709-223225`), which dropped the whole
`ScheduledThreadPoolExecutor`/`Executors.newScheduledThreadPool`/`newSingleThreadScheduledExecutor`
native surface in real-JDK mode (via `NativeMethodRegistry`'s existing
`drop_real_layout_synthetic` mechanism — the same one already used for
`java/util/StringJoiner` and, in their change, `java/util/EnumSet`) so the
real JDK bytecode constructs and drives `ScheduledThreadPoolExecutor`
end-to-end. That fix was **independently verified in this session**, both
directly on their (uncommitted, at verification time) worktree and by
re-applying their diff onto a fresh `origin/dev` tip in a disposable
worktree:
- Standalone repro (`STPEProbe.java`: `new ScheduledThreadPoolExecutor(1, tf)`,
  `getQueue()`, `scheduleWithFixedDelay(...)`, `shutdown()`) — passes clean,
  `queue=[]` (non-null) instead of `queue=null`.
- Full `TestSwallowAbortedUploads` run: `getQueue()` NPE occurrences dropped
  from present-in-every-method to **0/10** methods.

**New blocker #2 (found, already fixed elsewhere by the time of this
re-verify): `ByteBuffer.put(int, byte)` `AbstractMethodError`.** Past the
`getQueue()` blocker, `NioEndpoint`'s socket read hit
`AbstractMethodError: method java/nio/ByteBuffer.put(IB)Ljava/nio/ByteBuffer;
has no Code attribute`. This turned out to be the same family as (and
plausibly the identical live-dispatch-site fix for)
`docs/internal/tomcat-08-07/bytebuffer-address-unset-aioobe.md`, fixed on
`dev` via commit `1f1b5d28`/merge `1d7134d0` ("Fix ByteBuffer heap address
initialization", 2026-07-09) — landed independently, unrelated to this
investigation. Re-verified with a worktree built from a fresh `origin/dev`
tip (which includes that fix) plus the `ScheduledThreadPoolExecutor` diff
above: `AbstractMethodError` occurrences also dropped to **0/10** methods.

**New blocker #3 — STILL OPEN, now the sole blocker.** With both of the
above fixed (verified together in one combined build off a fresh `dev` tip),
`TestSwallowAbortedUploads` still fails **100% (10/10 methods, `Tests run:
10, Failures: 20`)** with a new symptom, now hit on every single method:
```
ERROR [org.apache.tomcat.util.net.NioEndpoint] Error running socket processor
  (java/lang/NullPointerException: Cannot invoke "java.util.concurrent.locks.ReentrantLock.lock()" because "lock" is null)
```
Not yet root-caused. Notes for the next session:
- `org.apache.tomcat.util.net.SocketWrapperBase` declares
  `private final ReentrantLock lock = new ReentrantLock();`
  (`SocketWrapperBase.java:66`) — the only `ReentrantLock lock` field found
  in the reachable HTTP (non-websocket) request path via
  `grep -rl 'ReentrantLock lock' java/`.
- `javap -p -c` on the real compiled `SocketWrapperBase.class` confirms the
  constructor bytecode looks correct in isolation: `new ReentrantLock()` +
  `putfield lock` at bci 4–12, immediately after `invokespecial
  Object.<init>` at bci 0. `getLock()` is a plain `getfield` + `areturn`.
  No native `<init>` override is registered anywhere in the tree for
  `SocketWrapperBase`, `NioSocketWrapper`, or `ReentrantLock` that would
  explain a skipped/shadowed write (confirmed via
  `grep -rn 'SocketWrapperBase\|NioSocketWrapper'` across
  `native-builtins/src/*.rs` and `vm/src/vm/vm_init.rs` — the only hit is an
  unrelated TLS comment).
- Two untested hypotheses: (a) a GC/moving-object stale-field-write —
  `SocketWrapperBase` is constructed on the Poller/Acceptor thread but the
  failing `.lock()` call happens later, on a worker thread pulled from the
  executor pool, which is exactly the cross-thread-visibility shape of
  several already-fixed bugs in this codebase (see
  `docs/internal/fixed-suite-bugs/*stale-local*`,
  `*cat2-local-store-upper-half-leak*`); (b) a JIT miscompilation of the
  field-initializer store in `SocketWrapperBase.<init>` (interpreter-vs-JIT
  divergence). Neither has been tested — no repro has been isolated below
  the full Tomcat fixture yet; that's the recommended next step (a minimal
  `NioEndpoint`-driven socket accept/read probe, bypassing the rest of
  Tomcat, to confirm/rule out cross-thread visibility vs. JIT).

**Status stays OPEN.** Do not retire this doc until blocker #3 is fixed and
the full class passes clean. Verification commands used throughout (adjust
the binary path):
```bash
<EXE> --java-home /home/victor/jdk25 -Xmx2g -cp "$(cat /data/data/apps/tomcat/.suite/cp-linux-fixed.txt)" \
  org.junit.runner.JUnitCore org.apache.catalina.core.TestSwallowAbortedUploads
```

## 2026-07-10 blocker #3 root-caused: stale/lost local variable, not a field bug

Re-verified independently on a fresh worktree off `dev` (`4723e059`, includes
both blocker #1/#2 fixes above). Confirmed both prior findings still hold:
original `WSAECONNABORTED` symptom stays fixed, and blocker #3's NPE still
hits **10/10** methods, always as the single substantive failure per test
(the other failure each test shows is unrelated: `output/build/conf/` —
including `logging.properties` — doesn't exist in this fixture, so
`WebappClassLoader.clearReferences()`'s JULI config reset throws
`FileNotFoundException` on every test's teardown; that's a fixture gap, not
a VM bug, and independent of blocker #3).

**The two "untested hypotheses" from the note above are both wrong.** Four
isolated repro attempts (plain construct-then-read, cross-thread handoff via
a bare `Thread`, 200k-iteration JIT-warmup loop, and construction under heavy
concurrent GC pressure from a background thread allocating 10 MB arrays)
all failed to reproduce a null `lock` field for the exact
`SocketWrapperBase`/`NioSocketWrapper` field shape in isolation — ruling out
both "simple field-initializer never runs" and "naive cross-thread
final-field visibility gap" as the mechanism, and ruling out both plain JIT
warmup and GC pressure alone as sufficient triggers.

**Direct instrumentation of the real `SocketWrapperBase`/`SocketProcessorBase`
classes (recompiled and placed first on the classpath, same technique used
above for the test class itself) found the actual mechanism.** Two
experiments:
1. Patched `SocketWrapperBase.getLock()` to print
   `System.identityHashCode(this)` + the field value before returning. It
   *always* printed a valid, unlocked `ReentrantLock` — including on the
   exact call immediately preceding a crash reported one log line later.
   I.e. `getLock()` itself, when it has a print statement added (making it a
   bigger, non-trivial method), never returns null.
2. Patched `SocketProcessorBase.run()` itself —
   ```java
   Lock lock = socketWrapper.getLock();
   System.err.println("DBG_RUN ... lockLocal=" + lock + " thread=" + Thread.currentThread());
   lock.lock();  // <-- still NPEs here, in the SAME run(), on the SAME local variable
   ```
   The print, reading the **same local variable** one bytecode-source-line
   before `lock.lock()`, *always* showed a valid, non-null lock — yet
   `lock.lock()` on the very next line still threw `NullPointerException:
   lock is null`, 5/5 times (matching the number of real client connections
   across the run; all thread names reported as `main`, i.e. this
   connector/test config doesn't hand work off to a separate worker pool
   thread — the earlier "cross-thread executor hand-off" theory in the note
   above doesn't even apply here).

That is: the **same local variable, read twice in immediate succession**,
returns a valid reference the first time and `null` the second time, with a
`System.err.println` (itself a method call, likely a safepoint-poll/GC
opportunity or a JIT/interpreter-tier-transition point) as the only thing
between the two reads. This is not a construction bug, not a cross-thread
visibility bug, and not "the field is null" — it is a **stale/lost local
variable reference across what is very likely a GC safepoint or JIT
deopt/tier-transition boundary**, i.e. the same general bug family as the
already-(partially-)fixed
`docs/internal/fixed-suite-bugs/native-stale-local-family-and-persistent-singleton-roots.md`
and `rootsnap-cache-stale-reassigned-local-ame` — striking a new, not
previously covered code shape: "obtain a reference via a getter call, then
immediately invoke a method on it" inside a `final` `Runnable.run()` that's
almost certainly hot/JIT-compiled (`SocketProcessorBase.run()`, dispatched
once per socket event). Per the project's own roadmap notes, precise
GC/JIT root maps are **default-off** and known to have unsound gaps in
roughly a dozen call-shapes; this looks like one more.

**Tried and did NOT fix it:** `CRATONVM_STRICT_JIT_ROOTS=1` — same run
instead **hung** (killed by a 90s timeout after only 6 of 10 methods logged
the same NPE), so it's not a safe drop-in toggle for this case either
(likely just a stricter/slower assertion mode, not an alternate sound
implementation). No other precise-root-map env var was tried.

**Not fixed in this session.** This needs the GC/JIT root-scanning
subsystem itself (`vm/src/jit/conservative_roots.rs`, `gc/src/gen_heap.rs`)
investigated by someone who owns that subsystem's roadmap, with proper
regression coverage — not a blind patch from this investigation. Recommended
next step for whoever picks this up: reproduce with `CRATONVM_DBG_PRECISE=1`
/ `CRATONVM_DBG_VERIFY_OOP_MAPS=1` (both found via `grep` in
`conservative_roots.rs`, neither tried yet) against the instrumented
`SocketProcessorBase.run()` repro above — it's a clean, minimal, deterministic
repro (5/5 hit rate) that doesn't require the full suite runner, just the
Tomcat fixture + JUnitCore + this doc's classpath override technique.

Once this is fixed, re-run `TestSwallowAbortedUploads` for a real (not
NPE-masked) verdict on the swallow-uploads socket-close fix itself — in
particular whether `testAbortedUploadLimitedNoSwallow` /
`testAbortedPOST413NoSwallow` (which want a `SocketException` when Tomcat
intentionally aborts without swallowing) still pass once requests actually
reach `Http11Processor`, since `sc_close`'s lingering-drain can't currently
distinguish "please help me finish draining" from "the app wants this
aborted, don't drain" (both look identical at the raw-socket level — see the
2026-07-09 candidate-fix section above and Tomcat's own
`IdentityInputFilter.end()` / `checkSwallowInput()` — there is no separate
`SO_LINGER`-style signal to key off).

## 2026-07-10 WebSocket close-delay branch correction

The WebSocket close-delay investigation found a separate `lock is null` signal
that should not be conflated with the HTTP swallow-upload blocker above. In
`TestWsRemoteEndpointImplServerDeadlock`, the misleading `lock` message came
from `java.util.concurrent.LinkedBlockingDeque.clear()` on Tomcat's WebSocket
`WriteBuffer`, not from `SocketWrapperBase.lock`; direct `SocketWrapperBase`
construction/read probes kept its `lock` field non-null.

This branch drops the synthetic `LinkedBlockingDeque` fallback surface in
real-JDK mode so the real JDK constructor initializes `lock`, `notEmpty`,
`notFull`, and the linked-node fields. A pre-merge run of
`TestSwallowAbortedUploads` with the final WebSocket-close binary and a
temporary logging basedir no longer showed the old `NioEndpoint ...
ReentrantLock.lock() because "lock" is null` processor error, and instead
reached request/response assertions:

```text
Tests run: 10, Failures: 6
1) testAbortedPOSTOKSwallow
2) testAbortedUploadLimitedNoSwallow
3) testChunkedPUTLimit
4) testAbortedPOST413Swallow
5) testAbortedPOST413NoSwallow
6) testAbortedPOSTOKNoSwallow
```

Status remains OPEN (as of the section above). The next swallow-upload investigation should account for
the stale/lost-local finding above and then re-check these six behavioral
assertion failures once the HTTP socket processor path is stable.

## 2026-07-10: `TestRewriteValve` independently confirms the `lock is null` NPE is gone on current `dev`

Cross-checking the "lock is null" symptom mentioned above against a
completely different suite class:
`org.apache.catalina.valves.rewrite.TestRewriteValve` (see
`docs/known-issues/tomcat-08-07/accesslogvalve-rewritevalve-connection-failures.md`)
hit the exact same
```
ERROR [org.apache.tomcat.util.net.NioEndpoint] Error running socket processor
  (java/lang/NullPointerException: Cannot invoke "java.util.concurrent.locks.ReentrantLock.lock()" because "lock" is null)
```
on every one of its 121 tests when run against a `dev` tip that predated
commit `9cbbc82c` ("Fix Tomcat WebSocket close-delay blockers",
2026-07-10). Re-ran the identical repro (worktree
`/data/data/wt-rewritevalve-lockcheck`, branch
`investigate/rewritevalve-lockcheck-20260710`, off `origin/dev` @
`9bc80788`, which includes `9cbbc82c`):

```
grep -c 'lock is null' <full run output>   # -> 0
Tests run: 121,  Failures: 156   # down from 229 pre-9cbbc82c
```

**Zero** `lock is null` occurrences, in either a debug build (101/121 tests
reached before its own shorter timeout, zero NPEs) or a release build (full
121/121 in ~150s). Comparing the two runs' failure content directly (same
worktree lineage, same test class, same env):

| | pre-`9cbbc82c` | post-`9cbbc82c` |
|---|---|---|
| `lock is null` NPE | 100% of connections | **0** |
| `AssertionError: expected:<200> but was:<-1>` | 99 | 0 |
| `AssertionError: expected:<400> but was:<-1>` | 10 | 4 |
| `AssertionError: expected:<200> but was:<302>` | 0 | 15 (new) |
| `AssertionError: expected:<400> but was:<302>` | 0 | 6 (new) |
| `ComparisonFailure` (UTF-8 percent-encoding round-trip) | 0 | 11 (new) |
| `LifecycleException: A child container failed during stop` | 115 | 115 (unchanged — pre-existing, unrelated teardown noise present in both runs identically; not investigated) |

105 of the 109 tests that previously got a dead-connection `-1` (99+10) now
get a real HTTP response (mostly `302` where `200`/`400` was expected — a
new, narrower, unrelated behavioral bug, not investigated here); 4 still
show `-1`. This is strong independent
corroboration that `9cbbc82c` fixed (or at minimum reliably suppresses) the
`SocketWrapperBase`/`SocketProcessorBase` `lock is null` NPE for the
*general* Tomcat NIO-connector path, not just the WebSocket/swallow-upload
cases it was written for — consistent with the "worth checking if it
explains other mysteriously-100%-failing Tomcat NIO test classes" note near
the top of the "stale/lost local variable" section above.

**Open question, not resolved here:** `9cbbc82c` bundled three changes
(drop synthetic `java/io/StringReader` in real-JDK mode, drop synthetic
`LinkedBlockingDeque` in real-JDK mode, fix `AtomicReference.toString`).
This doc's own "WebSocket close-delay branch correction" section above
attributes the `SocketWrapperBase.lock` fix to the `LinkedBlockingDeque`
change specifically (via `TestWsRemoteEndpointImplServerDeadlock`'s
`WriteBuffer.clear()` NPE) — but that's a different class/lock than the one
this doc's "stale/lost local variable" section root-caused
(`SocketWrapperBase.lock` via `SocketProcessorBase.run()`), and the two
mechanisms were never obviously connected. It's also possible the
`StringReader` drop contributed (`TestRewriteValve`'s own
`RewriteValve.parse()` uses `BufferedReader`/`StringReader` directly, so
that class is live on its exact request path, unlike
`TestSwallowAbortedUploads`). **No bisection was done to isolate which of
the three changes (or the combination) is responsible.** Given the "stale
local variable across a GC safepoint/JIT tier-transition" theory was reached
from isolated repros that never included a live synthetic-layout-mismatched
object (`LinkedBlockingDeque` or `StringReader`) allocated nearby, an
alternative reading is that the true mechanism was always a
synthetic/real-layout heap-corruption effect from one of those two classes
(the same well-established bug family as `StringJoiner`/`EnumSet`/
`ScheduledThreadPoolExecutor` elsewhere in this codebase — see
[[cratonvm-real-switch-synthetic-stub-wins-by-default]]) — the deep GC/JIT
root-scanner theory may have been chasing a red herring that happened to
correlate with GC-safepoint timing. Whoever revisits `CRATONVM_DBG_PRECISE`/
`CRATONVM_DBG_VERIFY_OOP_MAPS` per the recommended next step above should
first try a plain bisection (rebuild with only the `LinkedBlockingDeque`
drop, or only the `StringReader` drop, and rerun `TestRewriteValve`) before
assuming the GC/JIT subsystem itself needs fixing — it may already be moot.

## 2026-07-10: bisected — `LinkedBlockingDeque` (not `StringReader`) fixed it; NOT a GC/JIT bug

Did the bisection the previous section asked for, directly against
`TestSwallowAbortedUploads` (not `TestRewriteValve`, but the same NPE). Two
isolated worktrees off the same `dev` tip (`d8e79b43`), each reverting
exactly one of `9cbbc82c`'s two synthetic-native drops back to the old
(buggy) behavior:

| Build | `StringReader` | `LinkedBlockingDeque` | `lock is null` count | `Tests run: 10, Failures:` |
|---|---|---|---|---|
| Variant A | synthetic (reverted) | dropped (real bytecode) | **0** | 10 |
| Variant B | dropped (real bytecode) | synthetic (reverted) | **9** | 6 |
| current `dev` (both dropped) | dropped | dropped | 0 (6/6 clean runs) | 6 |

Reverting `StringReader`'s drop alone (Variant A) does **not** bring the NPE
back. Reverting `LinkedBlockingDeque`'s drop alone (Variant B) **does** —
same signature, same log line, same "no response reaches the client"
symptom as the original bug. This conclusively settles the open question:
**`LinkedBlockingDeque`'s synthetic-native drop is the fix; `StringReader`
was never relevant to this particular NPE** (it likely explains
`TestRewriteValve`'s *own* separate `RewriteValve.parse()`/`BufferedReader`
issue instead, per that class's direct use of `StringReader`).

**The "deep GC/JIT root-scanning bug" theory in the section above was a
misdiagnosis.** There is no stale-local/safepoint bug to fix in
`vm/src/jit/conservative_roots.rs` or `gc/src/gen_heap.rs` — the isolated
repros in that section never reproduced the NPE in a standalone program
specifically because they never constructed a real
`java.util.concurrent.LinkedBlockingDeque` under the old synthetic native.
The actual mechanism is the same well-precedented "synthetic native writes a
fake small-field-count layout over a real, differently-laid-out JDK object"
corruption family as `StringJoiner`/`EnumSet`/`ScheduledThreadPoolExecutor`
(see `native-collections/src/lib.rs::native_lbq_init`, now dead code behind
`#[cfg(not(feature = "synthetic-jdk"))]` but still registered pre-fix — it
writes `Value::Int`/array refs into 4 hardcoded field indices [0..3] via
`ctx.set_field`, assuming its own fake `(head,tail,size,capacity)` layout
regardless of the real JDK class's actual field order/count).

The structural link to `SocketWrapperBase.lock` specifically:
`SocketWrapperBase` itself declares
`protected final WriteBuffer nonBlockingWriteBuffer = new WriteBuffer(bufferedWriteSize);`
(`SocketWrapperBase.java:130`), and `WriteBuffer`'s own constructor does
`private final LinkedBlockingDeque<ByteBufferHolder> buffers = new LinkedBlockingDeque<>();`
(`WriteBuffer.java:38`) — so **every** `NioSocketWrapper` construction
(i.e. every accepted connection, not just WebSocket) transitively
constructs a `LinkedBlockingDeque` as part of its own field-initializer
chain, in the same object-construction sequence as `lock`
(`SocketWrapperBase.java:66`). The exact byte-level path from the buggy
`LinkedBlockingDeque` native's field writes to `SocketWrapperBase.lock`
specifically reading back null was not traced at the Rust/interpreter level
(candidates: a field-index/write-cursor mixup across the nested `<init>`
call, or a heap-adjacency effect from the undersized synthetic write) — but
the bisection above proves causation empirically regardless of the exact
mechanism, and that's enough to close this out: **no VM/GC/JIT code change
is needed beyond what `9cbbc82c` already shipped.**

**Status of the swallow-upload behavior itself: still OPEN**, now cleanly
characterized without any NPE noise (`Tests run: 10, Failures: 6`, stable
across 6 consecutive runs on current `dev`):
- 3 failures are the real `sc_close` behavioral gap this doc is about:
  `testAbortedUploadLimitedNoSwallow`, `testAbortedPOST413NoSwallow`,
  `testChunkedPUTLimit` all want a `SocketException` when Tomcat
  intentionally aborts-without-swallowing, but the lingering-drain fix is
  now too gentle to ever produce one (see the analysis two sections above —
  `sc_close` cannot distinguish "help me finish draining" from "the app
  wants this aborted" from the raw socket alone).
- 3 failures (`testAbortedPOSTOKSwallow`, `testAbortedPOST413Swallow`,
  `testAbortedPOSTOKNoSwallow` — all `AbortedPOSTClient`/`AbortedPOSTServlet`
  tests, i.e. the simple non-multipart POST that never reads its own body)
  get a **completely empty response** (`responseLine == null`, `ex == null`,
  no exception, no error logged anywhere) regardless of the swallow setting —
  a **different, not-yet-diagnosed bug**, newly visible now that the NPE
  isn't masking it. Confirmed via the same debug-instrumented
  `TestSwallowAbortedUploads.java` technique used earlier in this doc
  (`DEBUG_RESPLINE=[null]`, `DEBUG_EX=[null]`, no server-side ERROR/WARN
  logged in between). Not investigated further this session — worth its own
  focused pass (does `doPost()` even run? does the response get written but
  never flushed to the socket? does swallowing the still-unread ~10 MB body
  block/starve the write somehow?).

The four `AbortedUploadClient`/multipart tests (`testChunkedPUTNoLimit`,
`testAbortedUploadUnlimitedSwallow`, `testAbortedUploadLimitedSwallow`,
`testAbortedUploadUnlimitedNoSwallow`) now pass their real assertions
cleanly.

## 2026-07-10: `AbortedPOSTClient` empty-response bug root-caused — not a Tomcat/VM logic bug, an interpreter-speed gap

Root-caused via layered live instrumentation of the real Tomcat classes
(`SocketProcessorBase.run()`, `Http11Processor.service()`/`prepareRequest()`,
`NioEndpoint`'s Poller timeout check, plus fine-grained client-side timing in
the test itself — same classpath-override technique used throughout this
doc). Chain of findings, each ruling out the previous hypothesis:

1. **`doPost()` never runs.** Instrumented `AbortedPOSTServlet.doPost()` to
   print on entry — it never fires for `testAbortedPOSTOKSwallow`.
2. **`Http11Processor.service()` never runs either.** Instrumented its
   `parseRequestLine()`/`parseHeaders()`/`badRequest()` call sites and the
   adapter-dispatch checkpoint — none fire. Ruled out header-parsing/bad-request
   theories (absolute-URI host mismatch, `maxPostSize`/`maxSwallowSize`
   connector-property ordering — tried reordering `AbortedPOSTClient.init()`
   to match `AbortedUploadClient`'s order, no effect).
3. **`SocketProcessorBase.run()` itself fires with `event=ERROR`, not
   `event=OPEN_READ`.** Every working connection (`AbortedUploadClient`,
   `testChunkedPUTNoLimit`) shows `OPEN_READ`; this one shows `ERROR`. Traced
   to `NioEndpoint.Poller`'s per-tick timeout scan
   (`NioEndpoint.java` around the `readTimeout || writeTimeout` check):
   the connector's read timeout is a hardcoded **3000ms**
   (`configuredReadTimeout=3000`, same value on every connection — not a
   config difference), and for this connection **zero bytes were ever read**
   in that window (`deltaRead` climbs 1ms → 1002ms → 2004ms → 3005ms with
   `lastRead` frozen the whole time), so the Poller gives up and dispatches
   `SocketEvent.ERROR` — which `AbstractProtocol$ConnectionHandler.process()`
   treats as "nothing to do, already handled" and returns `CLOSED` **without
   ever constructing an `Http11Processor`**. No error is logged anywhere
   because, from Tomcat's perspective, this is normal, expected connection-
   timeout handling, not a fault.
4. **The client hadn't sent a single byte in that window because it was
   still building the request string.** Added `System.nanoTime()` timing
   around every step of `AbortedPOSTClient.doRequest()`:
   ```
   DBG_TIMING init_ms=5975            (Tomcat startup — happens before connect(), irrelevant to the read-timeout clock)
   DBG_TIMING connect_ms=0            (TCP handshake — instant, this is when the server's read-timeout clock starts)
   DBG_TIMING content_build_ms=5108   (new String(body) — body is a 10,000,000-element char[])
   DBG_TIMING processRequest_ms=36    (once it finally reaches the write, the actual I/O is fast)
   ```
   **`new String(char[])` on a 10-million-element array takes ~5.1–6.5
   seconds** (measured across 3 separate runs) — i.e. *longer than the
   connector's 3-second read timeout, entirely before the client attempts
   to send anything*. That's the whole bug: the connection sits open with
   nothing arriving, times out, and gets torn down while the client is
   still busy in a **pure in-memory, non-network** operation.

**Root cause, precisely:** `java.lang.String(char[])`'s constructor has
**no CratonVM native override** — it runs as the real JDK's own bytecode
(`String(char[], int, int, Void)` → `StringUTF16.compress`/`toBytes`, a
per-character Latin-1-fits-in-a-byte check + copy loop; confirmed via
`native-builtins/src/lang_string.rs`'s own comment: *"the real-JDK
`StringUTF16` bytecode (e.g. the `String(char[])` constructor, which has no
native override)..."*). Called only 3 times total in this whole test class
(once per `AbortedPOSTClient` test) — nowhere near the JIT's default
500-invocation warm-up threshold (`CRATONVM_JIT_THRESHOLD`,
`vm/src/runtime/env_cache.rs:76`) — so this 10-million-iteration loop stays
fully interpreted every time, at roughly 500–650ns/char. **Tried
`CRATONVM_JIT_OSR=1`** (on-stack replacement, which could in principle
compile a hot loop mid-invocation without needing 500 separate calls) —
**did not help** (`content_build_ms` was 6464ms with OSR on vs. 5108ms
without, if anything slightly worse), so this loop shape either doesn't
qualify for CratonVM's current OSR triggering or the one-shot compile
overhead isn't worth it for a single invocation either way.

**This is not a Tomcat bug, not a socket/connector bug, and not the same
family as blocker #3 above** — it's a pure interpreter-throughput gap for a
specific, extremely common JDK bulk operation (turning a large `char[]`
into a `String`) that happens to collide with this one test harness's
hardcoded 3-second connector timeout. `AbortedUploadClient`'s tests build a
comparably-sized string too (`StringBuilder.append(char[])` +
`getBytes("UTF-8")` + `new String(byte[], "ASCII")`) but apparently take a
different, faster path — not confirmed why, but plausibly `StringBuilder`'s
bulk `append(char[])` and/or the byte[]-based `String` constructors *do*
have native overrides in this codebase (several are visible in
`native-builtins/src/deprecated_util.rs`/`lang_string.rs`), unlike the bare
`String(char[])` constructor.

**Not fixed this session.** The correct fix is a native intrinsic for
`String(char[])`/`StringUTF16.compress` (bulk Latin-1-fits check + copy in
Rust, bypassing the interpreted per-char loop) — but this is a
**very high-blast-radius change** (every `char[]`-to-`String` conversion in
every Java program run on CratonVM) that needs careful validation against
CratonVM's existing little-endian compact-string byte layout
(`vm/src/vm/vm_object.rs::create_java_string`) and the Latin-1/UTF-16 coder
selection semantics — not something to rush. Left for a dedicated
performance-focused session. Workaround for this specific test only: none
attempted (modifying the stock Tomcat test suite isn't appropriate).

**Practical takeaway:** treat this connector 3-second-read-timeout +
large-char-array-to-String pattern as a known trap for any other
CratonVM/Tomcat (or any embedded-server) test that builds a multi-megabyte
string via `new String(char[])` right before sending it — the interpreter
cost of that one conversion can alone exceed a short test-harness timeout,
producing a confusing "silent empty response, no error logged" symptom that
looks like a connector bug but isn't.

## 2026-07-10: `String(char[])`/`String(char[], int, int)` native intrinsic landed — root cause from the previous section FIXED

Implemented the fix the previous section left open: a CratonVM native intrinsic
for `java.lang.String`'s `char[]` constructors, replacing the interpreted
per-character `StringUTF16.compress`/`toBytes` loop with a bulk Rust
implementation.

**What changed** (branch `fix/string-char-array-ctor-intrinsic-20260710`,
merged to `dev`):
- `vm/src/vm/vm_object.rs`: extracted `populate_java_string_fields(shared,
  str_obj, units)` out of `try_alloc_java_string_object_from_units` — the same
  Latin1-fits-in-a-byte bulk scan + little-endian compact-string layout, now
  reusable against an *already-allocated* `String` object (not just a
  freshly-allocated one).
- `native-api/src/registry.rs`: new `NativeContext::init_string_from_units`
  trait method (default impl for mock/test contexts; VM override calls
  `populate_java_string_fields`).
- `native-builtins/src/lang_string.rs`: two new natives,
  `native_string_init_from_char_array` (`<init>([C)V`) and
  `native_string_init_from_char_array_range` (`<init>([CII)V`), registered in
  `register_string_utf16_natives`. Both bulk-read the source `char[]` via
  `NativeContext::read_char_array_into` (a single `copy_nonoverlapping` in the
  VM's override) and hand the raw `u16` units straight to
  `init_string_from_units` — no Rust `String`/`char` round-trip, so lone
  surrogates round-trip byte-for-byte exactly like the real JDK bytecode did.
  The range constructor replicates `checkBoundsOffCount`'s exact bounds-check
  semantics (factored into a shared `bounds_off_count_violation` helper) and
  both replicate the real constructors' `NullPointerException`-on-null-array
  behavior.

**Verification:**
1. A 43-case Java probe (null array, empty array, ASCII/Latin1, the 0xFF/0x100
   compact-string coder boundary, non-Latin1 (UTF16 coder), lone/unpaired
   surrogate round-tripping via `toCharArray()`, the 3-arg range constructor
   (substring semantics, zero count, full range), all 4 bounds-violation
   SIOOBE cases, downstream native consistency (`substring`, `indexOf`,
   `concat`, `StringBuilder.append`, `String.valueOf(char[])`,
   `String.copyValueOf(char[])`), and the actual bug scenario at scale (a
   10,000,000-element `char[]`, both all-Latin1 and all-non-Latin1) — all 43
   pass.
2. Measured perf on the same 10M-char arrays that took **5.1-6.5s** before
   this fix: **71ms** (Latin1 path) and **127ms** (UTF16 path) — roughly a
   **40-90x** speedup, comfortably under the Tomcat connector's 3-second read
   timeout that started this whole investigation.
3. Rust unit tests: `cratonvm-vm --lib vm_object::` (28/28 pass, including the
   existing `create_and_read_string*` family, now exercising the refactored
   `populate_java_string_fields` code path) and `cratonvm-native-builtins
   lang_string::` (80/80 pass).
4. Re-ran `org.apache.catalina.core.TestSwallowAbortedUploads` on the Azure
   Linux host, comparing a same-commit baseline binary (without this fix)
   against the patched one, isolating just the 3 `AbortedPOSTClient` methods
   (`testAbortedPOSTOKSwallow`, `testAbortedPOST413Swallow`,
   `testAbortedPOSTOKNoSwallow`) via a small `Request.method(...)`-based
   single-method JUnit runner (the class as a whole hits an unrelated
   pre-existing crash on `testChunkedPUTNoLimit` — see below — before reaching
   a full-class summary):

   | | baseline (no fix) | patched |
   |---|---|---|
   | `client.getResponseLine()` | `null` (connector timed out, no bytes ever arrived) | non-null (a real response line arrives) |
   | wall time | 5.7s – 10.7s (over the 3s connector timeout) | 0.35s – 0.46s after warmup (well under it) |

   **The specific root cause this doc identified — the interpreter-speed
   connector timeout — is confirmed fixed.** The client no longer stalls
   building the request string long enough to trip Tomcat's read timeout.

   **However, the 3 `AbortedPOSTClient` tests still fail**, for a *different*
   reason than before: `client.getResponseLine()` now returns a non-null but
   blank/space-filled string instead of a real `HTTP/1.1 200 OK`-style status
   line, so the `client.isResponse200()`/`isResponse413()` assertions still
   fail. This is a **separate, pre-existing bug** in the server's response to
   this specific large-body swallow-upload scenario, newly *reachable* (not
   newly *caused*) now that the timeout no longer masks it earlier — matching
   what the "2026-07-10: bisected" section above already anticipated
   ("a different, not-yet-diagnosed bug, newly visible once the NPE isn't
   masking it"). Not investigated further here — flagged as a fresh follow-up
   for whoever picks this up next; the char[]-constructor performance problem
   this section exists for is closed.

   Also confirmed, byte-for-byte identical on both the baseline and the
   patched binary (i.e. **definitely pre-existing, unrelated to this fix**):
   running the full `TestSwallowAbortedUploads` class via `JUnitCore` (not the
   isolated single-method runner) crashes the whole VM process on
   `testChunkedPUTNoLimit` with `Exception in thread "main"
   java/lang/Thread` / `NullPointerException: charset`, alongside a
   `cratonvm_gc::gen_heap` "corrupt header" warning
   (`mark_young: rejecting object ... implausible extent 40`) — a GC/heap
   integrity issue triggered by that test's malformed-request payload, not by
   `String(char[])`. Also worth a dedicated follow-up.
5. Ran 20 `org.springframework.util`/`util.xml` test classes (heavy String
   usage — `StringUtilsTests`, `AntPathMatcherTests`,
   `PropertyPlaceholderHelperTests`, `ObjectUtilsTests`, etc.) against the
   patched binary: 17/20 fully clean. The 3 with failures
   (`StringUtilsTests`, `ObjectUtilsTests`, `ExponentialBackOffTests`) were
   re-run against the same-commit baseline binary and fail *identically*
   (same methods, same assertion messages) — confirmed pre-existing
   (a `HIB-CV-32`-family array-formatting/heap-integrity gap unrelated to
   `char[]`-to-`String` construction), not a regression from this change.

**No known regressions from this change.** The two issues surfaced during
verification (the blank swallow-upload response, and the
`testChunkedPUTNoLimit` crash) both reproduce identically without this fix
and are unrelated to `String(char[])`/`StringUTF16` — left open as separate
follow-ups rather than expanding this change's scope.
