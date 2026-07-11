# TestSwallowAbortedUploads — client sees `SocketException` when none expected

**Status:** ✅ RESOLVED (2026-07-11) — see the final section at the bottom.
`org.apache.catalina.core.TestSwallowAbortedUploads` now passes all 10
tests clean (`OK (10 tests)`), verified across 4 consecutive runs including
a build off the exact commit merged to `dev`. **Severity:** medium.
**HotSpot:** PASS.

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

## 2026-07-10: blank-response follow-up — does NOT reproduce in isolation; instead
## found a live SIGSEGV that is almost certainly the same root cause (known
## "register-invisible JIT root" bug family, already tracked elsewhere as OPEN)

Dedicated follow-up session for the "blank/space-filled response" bug the
previous section left open. Isolated worktree `wt-swallow-blankresp-20260710`
(branch `investigate/swallow-blank-response-20260710`) off `origin/dev` @
`19a91636` (includes the `String(char[])` intrinsic fix), release binary
`cratonvm-swallow-blankresp-20260710`.

**Headline result: the blank/space-filled `responseLine` symptom could NOT be
reproduced this session, across ~35 varied trials** (see "What was tried"
below). But a **different, more severe symptom — a reproducible SIGSEGV — was
found** using a superset of the same conditions (repeated execution of the
exact same 3 `AbortedPOSTClient` tests in one JIT-enabled JVM), and its crash
signature is a byte-for-byte match, on two independent occurrences, for the
already-documented-elsewhere "register-invisible JIT root" bug family. This is
almost certainly the same underlying mechanism the previous session hit — a
stale/garbage-valued register dereferenced where a live heap pointer was
expected — just with a different (worse) outcome this time (hard crash
instead of corrupted-but-readable content).

### What was tried (all healthy — no blank response, no crash)

Using a purpose-built `SingleMethodRunner`/`MultiMethodRunner` (`Request.method`
+ `JUnitCore`, same technique as the doc's existing single-method runner
references) against `testAbortedPOSTOKSwallow`, `testAbortedPOST413Swallow`,
`testAbortedPOSTOKNoSwallow`:
- 15× isolated fresh-JVM single-method runs (5 per test): all passed, real
  `HTTP/1.1 200 `/`HTTP/1.1 413 ` status lines, `SMR_FAIL=0` every time.
- 3× all-3-methods-sequentially-in-one-JVM: all passed.
- 6× the same single test run in parallel (contention-inducing): all passed.
- A debug-instrumented copy of `TestSwallowAbortedUploads.java` (recompiled,
  placed first on classpath, same technique used earlier in this doc) that
  unconditionally prints `client.getResponseLine()` with non-printable
  characters escaped as `\uXXXX` — always showed a clean, correct
  `len=13 [HTTP/1.1 200 ]` (or 413) with correct headers.
- The **exact pre-built reference binary from the previous session**
  (`/data/data/cratonvm-string-char-array-ctor-20260710`, the one that
  reportedly produced the blank/space response) — re-run 3× this session,
  passed cleanly every time.

This is strong evidence the symptom is a genuine timing-sensitive race, not a
deterministic regression — the same binary that (per the previous session's
report) produced a blank response now does not, on the same host.

### What reliably reproduces: a SIGSEGV under repeated same-JVM execution

Chasing the "maybe it needs JIT warm-up state carried across several
Tomcat/socket cycles within one JVM" hypothesis (this codebase has precedent
for JIT-tier-crossing-triggered bugs — see the memory index), `MultiMethodRunner`
was used to run the same test method(s) back-to-back, many times, in a single
JVM process:

- 40× `testAbortedPOSTOKSwallow` in one JVM: did not crash, but got stuck at
  iteration #9 with `STW cross-thread JIT takeover is still waiting for
  cooperative mutators rounds=64 pending=1 taken=0` (from
  `vm/src/runtime/interpreter.rs`'s `stw_take_over_and_wait`, the BUG-03
  cross-thread JIT root-scan machinery) and was killed by a 120s timeout — a
  hang, not (yet) a crash.
- Interleaving `testAbortedUploadUnlimitedSwallow` / `...NoSwallow` /
  `testAbortedUploadLimitedSwallow` (the sibling 10 MB-body tests) for several
  rounds **before** reaching the target `AbortedPOSTClient` tests: **SIGSEGV**,
  reproduced twice independently (`exit 139`, `timeout: the monitored command
  dumped core`), both times around the 10th-11th test invocation in the
  process (9 and 10 straight invocations completed cleanly in separate control
  runs; 30 invocations crashed at #11) — consistent with a race that becomes
  *possible* once some hot method crosses the JIT compile threshold, not a
  hard deterministic trip count.
- **Confirmed reproducible using only the 3 assigned target tests**
  (`testAbortedPOSTOKSwallow` / `testAbortedPOST413Swallow` /
  `testAbortedPOSTOKNoSwallow`, no `AbortedUploadClient` involved): looping
  those 3 in one JVM also SIGSEGVs (10 successful invocations, crash on the
  11th), directly tying the crash to the exact code path this doc is about.
- **`--nojit` does not crash**: the identical 15-invocation
  `AbortedUploadClient`-family sequence that reliably SIGSEGVs with JIT on
  completed all 15 iterations cleanly under `--nojit` (it then hit an
  unrelated non-daemon-thread shutdown hang at the very end — not a crash,
  not investigated further). This confirms the crash is JIT-specific.

### Crash analysis: byte-identical signature on two independent occurrences

Repro technique: `sudo sh -c 'echo /path/core-%e-%p.dump > /proc/sys/kernel/core_pattern'`
+ `ulimit -c unlimited` (per the project's own guidance to prefer
`core_pattern`+`ulimit` over a live gdb wrapper for heisenbugs), then
`gdb -batch -ex 'thread apply all bt' -ex 'info registers' -ex 'x/10i $pc-20' <exe> <core>`.
`core_pattern` was restored to the host's original apport pipe and both core
files (~1.1 GiB on disk each) were deleted after analysis.

Both crashes — one from the `AbortedUploadClient`-family sequence (crashing
thread named `main-vm`), one from the pure `AbortedPOSTClient`-family sequence
(crashing thread named `Catalina-utilit[y]`, a Tomcat worker) — disassemble to
the **exact same instruction sequence** at the fault site (only the absolute
addresses differ, as expected for independent process runs with ASLR):

```
mov    -0x10(%rbp),%edi
mov    -0x38(%rbp),%rsi
mov    -0x30(%rbp),%rdx
test   %rsi,%rsi
je     <skip>
=> mov    (%rsi),%eax        ; SIGSEGV here
cmp    (%r10),%eax
jne    <slow-path>
cmpb   $0x0,0x28(%r10)
je     <skip>
```

This is JIT-generated machine code (the crash PC falls in an anonymous
executable mapping between `libc.so.6` and `libm.so.6` in `info proc
mappings` — no backing file, i.e. not resolvable to any symbol, consistent
with a JIT code buffer) implementing what looks like a null-checked
class/identity comparison (`test`+`je` null guard, then a class-word compare
against `r10` — a polymorphic-inline-cache- or `instanceof`-style fast path).
`rsi` — loaded from stack slot `rbp-0x38`, which should hold a live object
reference per the null-check just before it — held **not a valid tagged heap
pointer**: run 1 had `rsi=0xffffffff94a08430` (a sign-extended 32-bit value,
not a real 64-bit pointer), run 2 had `rsi=0x198ca4f8` (a bare 32-bit value in
a 64-bit register). Every genuinely live pointer visible elsewhere in the same
register file in both dumps (`r10`, `r12`, `r14`, `r15`, `rdi`) shares a
consistent `0x2000xxxxxxxx`-prefixed tagged-pointer shape; `rsi` alone breaks
that pattern both times — this is a stack slot/register that stopped holding
a real object reference and started holding garbage (or a truncated/reused
value), read after a null-check that itself passed because the garbage value
happened to be non-zero.

### This matches an already-documented, currently-OPEN bug family — not a new one

`docs/internal/fixed-suite-bugs/dohead-jit-heap-corruption-register-invisibility-FIXED.md`
(filed for `TestHttpServletDoHead*`, root-caused via `CRATONVM_DBG_A2`) documents
the exact mechanism this looks like: JIT-compiled code can keep a live object
reference in a register/stack-slot across a GC-capable safepoint (there, a
`LockSupport.park()`/`AbstractQueuedSynchronizer$ConditionObject.await()` call)
without it being visible to the conservative root scanner
(`deposit_root_snapshot`/`scan_active_jit_frames`). If a GC cycle runs while
that register is the *only* reference to the object, the object gets reclaimed
as garbage even though a live (but invisible) reference to it still exists;
later code that trusts the register gets a stale/corrupted value instead of a
valid pointer. That doc's own "Known accepted residuals" section states
explicitly: **"Layer 1 (register-invisible roots ... ) is UNCHANGED — the real
fix remains precise oop maps / shadow stack."** It also documents that the
obvious mitigations were tried and found insufficient:
`CRATONVM_JIT_SAFEPOINT_REG_SPILL=all` (still crashed 3/6), `CRATONVM_SHADOW_STACK=1`
(reduced but did not eliminate; explicitly experimental/non-production),
`CRATONVM_NO_SELECTIVE_PROMOTE=1` (still crashed), `CRATONVM_PRECISE_JIT_MAPS=1`
(uninformative, confounded by also switching on the moving young collector).

Independently, `docs/known-issues/hib-global-temptable-nondeterministic-sigsegv-20260710.md`
(merged to `dev` the same day, unrelated Hibernate global-temp-table DDL
investigation) describes the **same shape of bug** in a completely different
subsystem: non-deterministic PASS/HANG/CRASH across identical reruns, works
once and breaks on a repeat within the same JVM/test-class lifecycle, `--nojit`
as a planned-but-not-yet-tried control. That doc explicitly says it has **not
yet** captured a core dump/backtrace ("Next steps: ... not yet done"). This
session's gdb evidence (above) is a concrete, reproducible data point for
that same general bug class, from a third, independent code path.

**Conclusion: the "register-invisible JIT root" bug is not fully fixed** (the
DoHead doc's title says FIXED, but that refers only to the *fatal young-gen
walk-desync amplifier* it also found and fixed — its own text says Layer 1,
the register-invisibility itself, is an accepted, unfixed residual). It is
live and reachable from at least three independent code paths now: Tomcat
DoHead/AQS-park, Hibernate global-temp-table DDL, and (this session)
Tomcat's `TestSwallowAbortedUploads` large-body swallow/response path — none
involving AQS/`park()` obviously in this session's case, which suggests the
register-invisibility gap is broader than just the `park()` call shape
already documented, though the exact JIT-compiled method at this session's
crash site was not identified (the crash PC has no symbol; correlating it to
a specific Java method/bytecode offset would need the JIT's own compiled-region
metadata, not attempted here).

**Best-supported explanation for the original blank/space-response symptom
(not proven, but well-supported):** the previous session's run hit the same
stale-register condition, but the garbage value in `rsi`-equivalent happened
to alias into some other *mapped* memory (e.g. a zeroed/reused buffer) instead
of unmapped memory, so instead of a SIGSEGV, some code path along
`Http11OutputBuffer`'s response-write chain (or the client's own read) ended up
reading/writing the wrong buffer's content — producing readable-but-wrong
bytes (blank/space) instead of a crash. This is consistent with "same root
cause, different luck of the corrupted address," but was not proven directly:
this session never caught the blank-response symptom itself in the act, only
the crash. Whoever next reproduces the blank response directly should check
for the same `rsi`-not-a-tagged-pointer signature (attach with the same
`core_pattern`+`ulimit -c unlimited` recipe above) to confirm or refute this
link.

**Not fixed this session** (deliberately — this is deep JIT/GC root-precision
infrastructure work already flagged elsewhere as needing "precise oop maps /
shadow stack," and the previous investigation's own attempted mitigations were
insufficient; a blind patch here would be exactly the kind of half-fix the
project's workflow asks not to merge). Recommendation: whoever owns the
precise-JIT-maps/shadow-stack roadmap item should treat this doc's repro
(loop `testAbortedPOSTOKSwallow`/`testAbortedPOST413Swallow`/`testAbortedPOSTOKNoSwallow`
~10-15× in one JIT-enabled JVM via a small `Request.method`-based runner) as a
third, independent, relatively cheap-to-run reproduction case alongside the
DoHead and Hibernate ones.

**Repro commands** (adjust binary path):
```bash
cd /data/data/apps/tomcat  # fixture; classpath from .suite/cp-linux-fixed.txt
sudo sh -c 'echo /tmp/core-%e-%p.dump > /proc/sys/kernel/core_pattern'  # optional, for a backtrace
ulimit -c unlimited
C=org.apache.catalina.core.TestSwallowAbortedUploads
ARGS=""
for i in $(seq 1 15); do
  ARGS="$ARGS $C testAbortedPOSTOKSwallow $C testAbortedPOST413Swallow $C testAbortedPOSTOKNoSwallow"
done
<EXE> --java-home /home/victor/jdk25 -Xmx2g -cp "<MultiMethodRunner-dir>:$(cat .suite/cp-linux-fixed.txt)" \
  MultiMethodRunner $ARGS
# expect SIGSEGV (exit 139) somewhere around the 10th-11th invocation, not on every run
```
(`MultiMethodRunner` is a ~25-line `Request.method`+`JUnitCore` loop over
`(className, methodName)` pairs; not committed to the repo, trivial to
recreate from this description if the original isn't available.)

## 2026-07-10: register-invisible-root diagnostic pass — Case B confirmed (known gap, not a wrong-but-present precise map)

Dedicated diagnostic follow-up to the "blank-response follow-up" SIGSEGV
section above, per the recommended next step ("reproduce with
`CRATONVM_DBG_PRECISE=1` / `CRATONVM_DBG_VERIFY_OOP_MAPS=1`... neither tried
yet"). Isolated worktree `wt-regroots-precisedbg-20260710` (branch
`investigate/regroots-precisedbg-20260710`) off a freshly-fetched
`origin/dev` @ `ba23c02b`, release binary
`cratonvm-regroots-precisedbg-20260710.bin`. **Diagnostic only — no code
changes**, per this pass's scope.

### First: the two named debug vars, verified from source (not assumed)

Before running anything, grepped `vm/src/jit/conservative_roots.rs`,
`jit/src/lib.rs`, `jit/src/x64.rs` to confirm what these two vars actually
do:

- `CRATONVM_DBG_PRECISE=1` — `remap_active_jit_frames` (the Stage-3 *moving*-GC
  precise relocation walker) prints one `[PRECISE] remap: chain_entries=...
  precise=... frames_walked=... maps_found=... slots_examined=...
  slots_rewritten=... reg_size=...` line per call, so `precise=0` throughout a
  run means no JIT frame ever carried precise-map metadata during any
  relocation pass on that thread.
- `CRATONVM_DBG_VERIFY_OOP_MAPS=1` — for every precise-scanned JIT frame during
  a GC *mark* pass, runs `verify_precise_covers_conservative`: it diffs the
  frame's oop-map slot union against a conservative sweep of the SAME
  `[scanner_sp, frame_base)` band and logs `[VERIFY-OOP-MAPS] unmapped in-band
  oop: ...` for any word that looks like a live heap pointer but isn't
  recorded by any of the method's oop maps. Read-only; does not change scan
  behavior.

### Critical scoping fact, discovered from source (not in any existing doc,
### and corrects a stale assumption several earlier sections in this doc make)

`jit/src/x64.rs::precise_jit_maps_enabled()` — the actual gate for whether the
JIT emits oop-map metadata at all — has been **DEFAULT ON since commit
5b8864a0-era work landed 2026-07-07** ("re-flipped 2026-07-07", per its own
doc comment). `CRATONVM_PRECISE_JIT_MAPS` (the name several earlier sections
of this doc, and the DoHead doc, refer to as the opt-in switch) is now a
**no-op**; the only live knob is the opt-*out* `CRATONVM_NO_PRECISE_JIT_MAPS=1`.
So on the exact `dev` tip this crash reproduces on (2026-07-10, well after the
flip), every freshly JIT-compiled method already gets: prologue frame-record,
per-safepoint sp-id slots, operand-stack oop-map entries, a forward
must-be-oop dataflow for locals (`compute_local_oop_masks`), and (since
`moving_young` is off by default) a post-safepoint register reload
(`emit_post_safepoint_reload`). The "no oop maps are written today" comment on
`JitEntryGuard::enter_with_compiled` in `conservative_roots.rs` is stale,
predating the flip — verified empirically below (`CRATONVM_DBG_PRECISE=1`
shows `precise>0`, not `precise=0`, on ordinary runs).

Second scoping fact: which GC actually runs during this crash. Live JIT
frames make `gc_quiescence::is_active()` true almost continuously while the
test loop runs, and `gen_heap.rs::collect_garbage_inner` explicitly diverts to
the **non-moving young mark-sweep + selective promotion** whenever
conservative JIT roots are live and `CRATONVM_MOVING_YOUNG` (a separate,
still-default-off flag) isn't set — precisely *because*, per that function's
own comment, "a semispace cannot pin a conservative JIT root nor rewrite a
register-resident one, so some live nodes go stale after the swap." So the
crash's actual collector is a **non-relocating mark-sweep**: nothing moves,
`remap_active_jit_frames` is not exercised, and correctness depends entirely
on the *marking* side finding every live root — i.e. exactly the
precise-oop-map-plus-conservative-backstop machinery `CRATONVM_DBG_PRECISE`
and `CRATONVM_DBG_VERIFY_OOP_MAPS` instrument.

### The decisive source find: a NAMED, already-catalogued gap in the
### default-on precise/local-tracking machinery — SB-CRASH-04

`jit/src/x64.rs::emit_pre_safepoint_spill` (~line 8925), verbatim:

> "SB-CRASH-04 (register-invisibility) — blind-spill the CURRENT value of
> every used callee-saved GPR into its reserved frame slot. The local flush
> above only covers register-resident *locals*; an oop can also live in a
> callee-saved register as an operand-stack temporary that survives the call,
> **or via a value the per-slot oop tracker fails to tag**."

This blind-everything-spill mitigation exists but is gated behind
`CRATONVM_JIT_SAFEPOINT_REG_SPILL` (`safepoint_reg_spill_enabled()`,
`jit/src/x64.rs` ~line 2551) — **default OFF**, fully independent of the
now-default-on `precise_maps`/oop-map gate. The default-on local-oop dataflow
(`compute_local_oop_masks`) is a forward must-analysis that **intersects
(AND) at merge points** ("First real predecessor seeds; later ones intersect")
— a sound-for-"definitely oop" but incomplete-for-"actually still live oop"
analysis by construction: any control-flow shape where a slot is oop-typed on
one incoming edge and not on another under-reports that slot as non-oop at
the merge, exactly the "value the per-slot oop tracker fails to tag" case the
SB-CRASH-04 comment names. Operand-stack *temporaries* (not named locals) that
outlive a call are explicitly called out as uncovered by the default path too.

`docs/known-issues/README.md`'s own SB-CRASH-04 entries independently confirm
this is not hypothetical: for the A3 repro, "every conservative trick
(`CRATONVM_NO_JIT_SCAN_CACHE`, `CRATONVM_JIT_SAFEPOINT_REG_SPILL`,
`CRATONVM_DBG_FULLSTACK_SCAN`, and combinations) still crashes — confirming
the root is genuinely register-resident and unreachable by any stack scan."
This is the same bug *family*, not necessarily the same site — the swallow-
upload crash was not traced to a specific Java method/bytecode offset (the
JIT code buffer is an anonymous mapping; `CRATONVM_DBG_JIT_NAMES=1` /
`lookup_jit_method_name` would be needed for that, not attempted this pass —
see "confidence" below).

### Empirical run

Ran the doc's own repro (`MultiMethodRunner`, 3× `AbortedPOSTClient` methods
looped, JIT on, `--java-home /home/victor/jdk25`) with `core_pattern` +
`ulimit -c unlimited` core dumping armed (same technique as the section
above; `core_pattern` restored to its pre-session value afterward), across
four configurations: both debug vars together, each alone, and a pure
baseline. **Headline result: this session could NOT reproduce the SIGSEGV at
all** — 50 completed process launches (heavy dual-flag: 6; `CRATONVM_DBG_PRECISE`
alone: 15; `CRATONVM_DBG_VERIFY_OOP_MAPS` alone: 15; no debug flags: 11, plus
an initial 3-attempt sanity check), covering well over 1,500 individual test-
method invocations, produced zero SIGSEGVs. This is a materially lower hit
rate than the prior session's (which needed ~10-30 invocations per crash).

The dominant confound: **72% of all attempts (36/50) hung instead**, hitting
the *already-documented, unrelated* `STW cross-thread JIT takeover is still
waiting for cooperative mutators` bug (`vm/src/runtime/interpreter.rs`) before
ever reaching a crash-eligible iteration count — the same hang the prior
session also hit exploring this exact repro. `ps aux` on the shared host
during the hunt showed the likely reason: multiple *other* concurrent
sessions' heavyweight CratonVM processes pegged at high CPU for extended
periods (an Elasticsearch `SortingDigestTests` run at 100% CPU for 10+
minutes straight, a WildFly `domain.sh` boot) — genuine host contention that
starves the STW barrier's bounded-rounds wait, independent of anything this
pass did. (Side effect worth flagging for whoever investigates next: this
session's `core_pattern` change is process-wide, so 13 *foreign* core dumps
from other sessions' unrelated crashes accumulated in this pass's
`/data/data/regroots-precisedbg-cores/` directory during the hunt — none of
them are from this pass's own binary, verified by the complete absence of any
`rc=139` result across all 50 attempts; left untouched since they belong to
other in-flight investigations.)

One genuine empirical data point was still obtained: in the clean
(non-hung, non-crashed) runs under `CRATONVM_DBG_PRECISE=1`, `[PRECISE]
remap:` fired on every `update_all_roots` call (confirming the promotion/
compaction machinery does run and does produce non-trivial `pointer_map`s —
observed sizes 8606 to 40843 entries) but **`chain_entries=0 precise=0
frames_walked=0` every single time**, with `reg_size` (the live JIT-code-range
count) climbing into the 180-191 range as the run warmed up. This confirms,
directly, that the *moving*-GC relocation walker (`remap_active_jit_frames`)
never has any JIT frame registered in its thread-local chain at the instant
each collection's root-remap phase runs on the calling thread — consistent
with (not contradicting) the "non-moving sweep + selective promotion is what
actually runs while JIT frames are live" analysis above: promotions still
happen (hence the large `pointer_map`s), but they promote only heap-interior
objects that survived marking, not the JIT-frame-rooted objects themselves
(those are pinned, per `gen_heap.rs`'s own "PINNING conservative roots +
tenuring only heap-interior nodes" comment) — so `remap_active_jit_frames`
finding nothing to do here is expected, not a bug in itself. It does confirm
that whatever protects a JIT-rooted object from going stale is entirely a
function of the **marking** side (which `CRATONVM_DBG_VERIFY_OOP_MAPS` probes)
rather than the relocation side — which is exactly the piece SB-CRASH-04
documents as incomplete by default.

### Verdict: Case B (known gap), not Case A — with a specific, named root cause

The evidence points cleanly to **Case B**, but more precisely than the two
generic cases originally sketched: this is not "the crashing safepoint was
never in a precise-mapped method at all" (precise maps are default-on and
engage for essentially every JIT-compiled method reached in this repro) — it
is that the default-on precise/local-tracking machinery has a **named,
already-catalogued, structurally-inherent incompleteness** (SB-CRASH-04) for
exactly the shape of value the crash's own disassembly shows (a
class/identity-comparison fast path reading a stack-resident temporary that
should have held a live object reference and instead held obvious garbage,
`0xffffffff94a08430` / `0x198ca4f8` — neither a plausible tagged heap pointer
nor a "points at now-freed-but-still-mapped memory" shape, consistent with
the slot's backing memory having been reclaimed as garbage and reused for
unrelated data). The two structural facts that jointly explain it:

1. The GC that actually runs while this test's JIT frames are live is the
   **non-moving young sweep**, which relies entirely on *marking* (not
   relocation) to keep every live JIT-rooted object alive. Marking IS
   backstopped by a full conservative sweep of `[scanner_sp, frame_base)` for
   every frame that has a `JitEntryGuard` chain entry — but that backstop only
   covers what is *in that thread's chain and in-band on the stack at scan
   time*. It provides zero protection for a value that is register-resident
   only, or that belongs to a call shape that never pushes a chain entry for
   the frame in question (an inlined callee, certain nested-call shapes) — the
   same "narrower-than-it-looks" property the OSR/nested-call machinery in
   `conservative_roots.rs` and `precise-jit-maps-default.md` repeatedly flags
   as an accepted, un-closed residual.
2. The one general-purpose mitigation this codebase already built for exactly
   this failure mode — `CRATONVM_JIT_SAFEPOINT_REG_SPILL` (blind-spill every
   used register at every safepoint, `emit_pre_safepoint_spill`) — is **not
   engaged by default**. `docs/known-issues/README.md`'s own SB-CRASH-04
   history shows that even with it (and every other conservative-scan
   diagnostic knob) turned on, at least one known repro in this exact family
   (`MinRegexProbe`/A3) still crashes, "confirming the root is genuinely
   register-resident and unreachable by any stack scan." `docs/feature-
   designs/precise-jit-maps-default.md`'s own acceptance-sweep plan
   explicitly lists "tomcat-style register-invisibility reclaims named in the
   README" as work the precise-maps-default rollout has **not yet validated**
   — i.e. the project's own roadmap already anticipated that a Tomcat-shaped
   crash like this one could still be open under the current default.

Neither of the two mechanisms this pass's task description offered as
alternatives to Case B applies: it is not that a *map exists, covers this
exact safepoint, and is wrong* (Case A) — the map machinery's own doc comments
are explicit that it is *incomplete by construction* for register-resident
operand-stack temporaries and certain locals, with a known (but default-off)
partial fix. This is the deep, general-infrastructure gap, not a narrow,
easily-patchable soundness bug in one map-generation code path.

### Confidence: high on the classification, not fully closed on the specific site

High confidence that this is the SB-CRASH-04 register-invisible-root family
(Case B), based on: (a) the structural argument above, grounded in specific,
quoted source (not inference from symptom-matching alone), (b) the crash
disassembly's own signature (garbage-looking, non-tagged-pointer bit patterns
in exactly the "read a value expected to be a live oop" position) matching
the family's established fingerprint from three independent prior sessions
(DoHead, Hibernate global-temp-table, and this doc's own prior SIGSEGV
section), and (c) the project's own design docs independently anticipating
this exact gap for this exact application (Tomcat) ahead of any of these
investigations.

**Not fully closed**, and explicitly flagged as such: this pass could not
reproduce the crash fresh, so — unlike the "SocketWrapperBase.lock" bisection
earlier in this doc — there is no *this-session* core dump or debug log tying
the specific faulting PC/frame to a specific Java method or to a live
`CRATONVM_DBG_VERIFY_OOP_MAPS` "unmapped in-band oop" hit. It remains
possible (though it would be a coincidence given how well the signature
matches) that the swallow-upload crash specifically is a related-but-distinct
bug in the same fragile machinery (e.g. a frame_base/scanner_sp bookkeeping
error for one particular nested-call shape, rather than a plain uncaptured
register) rather than the generic "value lived only in a register" case.
Recommended next steps for whoever revisits this with a quieter host: (1)
repeat this pass's exact repro with `CRATONVM_JIT_SAFEPOINT_REG_SPILL=all`
added — if it measurably suppresses the crash (even partially, matching the
DoHead doc's "still crashed 3/6" outcome for a different repro), that is
strong independent confirmation; if it does not help at all, that points
toward the rarer "unreachable by any stack scan" or bookkeeping-bug subtype
instead; (2) once a fresh core is caught, cross-reference its crash PC against
`CRATONVM_DBG_JIT_NAMES=1`'s `lookup_jit_method_name` output (not attempted
this pass) to identify the actual Java method/bytecode offset, which no prior
session in this bug's history has done.

## 2026-07-11: mechanism narrowed further — locals dataflow ruled OUT, two remaining candidates identified; no fix attempted

Follow-up to the "Case B confirmed" section above, prompted by attempting to
scope an actual fix. A resumed diagnostic session hit an API session limit
mid-run (its live-repro attempts — ~50 more launches across a `hunt.sh`
driver script — reproduced the same picture as the section above: no fresh
crash, majority hung on the unrelated STW-takeover contention bug); its
partial draft is folded into this section rather than lost. Direct source
reading (`jit/src/x64.rs`) this session adds one concrete, load-bearing
correction and narrows the remaining hypothesis space to two candidates:

**`compute_local_oop_masks`'s AND-intersection at CFG merge points is NOT the
gap** — its own doc comment carries a rigorous soundness argument grounded in
JVM bytecode verification: "A live oop that is conditionally a primitive on
another path merges to 'not definitely oop' and is omitted; such a slot is
necessarily dead-as-oop at that PC (the verifier forbids reading a slot with
conflicting types), so the omission is safe." The verifier's own type-merge
rules already guarantee a local can't be soundly read as a reference at a
program point where it's oop-typed on one incoming edge and not on another.
An earlier hypothesis this session considered — switching that AND to an OR
(over-approximate, "can only over-retain, never corrupt" under the current
non-moving collector, mirroring the `=all` register-spill argument) — is
**not the right fix**: it targets a dataflow that is already provably sound
for locals. **Do not pursue this specific change without addressing why this
soundness argument would be wrong first** — it would be adding complexity
without closing the actual gap.

**Where the actual gap lives**, per the SB-CRASH-04 doc comment on
`emit_pre_safepoint_spill` ("an oop can also live in a callee-saved register
as an operand-stack temporary that survives the call, **or via a value the
per-slot oop tracker fails to tag**") and a read of the sibling mechanism,
`flush_scratch_registers`'s default-on `flush_callee_saved_oops` step
(`jit/src/x64.rs` ~line 14853-14944): that step only spills a `StackSlot::
CalleeSaved` operand-stack entry to its frame home if `self.stack_oop_marks`
— the parallel "per-slot oop tracker" the SB-CRASH-04 comment names — already
marks that stack index as holding a reference. **If `stack_oop_marks` fails
to tag a genuinely-reference-valued stack entry (a false negative in that
tracker, not in the locals dataflow), the default-on flush silently skips
it**, leaving it register-resident and unprotected. This is a specific,
falsifiable hypothesis, not yet confirmed: it requires finding a concrete
operand-stack-producing code path where a reference value can end up on the
stack without `stack_oop_marks` being set for it (candidates worth checking:
values produced by inlined/guarded fast paths — the crash disassembly's
class-identity-check shape strongly resembles `guarded_inline_getfield`/an
invoke inline-cache fast path, both relatively new/complex codegen — merges
of stack shapes across conditional branches feeding an invoke's receiver
slot, or a stack entry produced by a call whose return-oop-ness isn't
propagated into `stack_oop_marks` correctly).

**A second, structurally different candidate was also identified and is
NOT yet ruled out**: the crash's faulting instruction reloads `rsi` from a
stack-relative address (`rbp-0x38`) rather than reading a register directly —
i.e. this specific crash's immediate cause is a *stale read from a frame
slot*, not (necessarily) an *unspilled register*. Two sub-hypotheses this
implies, not distinguished this session:
  (a) the slot was never populated with the live receiver at all (consistent
      with the `stack_oop_marks` false-negative theory above — the flush that
      would have written it never ran), or
  (b) the slot WAS correctly populated at some earlier point but its backing
      frame offset got reused/overwritten by unrelated codegen before this
      reload — a frame-slot-lifetime/aliasing bug, a different and
      structurally unrelated class of defect from "GC found the root
      invisible." `reserve_spill_slots`' offset-reuse bookkeeping (not read
      this session) would be the place to check for (b).

These two candidates need genuinely different fixes ((a): extend
`stack_oop_marks` propagation at whatever codegen site drops the tag; (b):
fix spill-slot lifetime tracking so a live oop's frame home is never handed
out to a different value while still needed) — **implementing either without
confirming which one applies risks a wrong, unverified change to
correctness-critical GC/JIT code**, which given the failure mode (silent
memory corruption, not just a crash) is worse than leaving the bug open and
well-documented. No code change was attempted this session for this reason.

**Recommended concrete next step for whoever picks this up**: rather than
another blind crash-hunt on this chronically-busy shared host (three
consecutive sessions have now hit majority-hang rates from concurrent
sessions' CPU load — this appears to be the host's steady state, not a
transient spike), either (1) request a quieter/dedicated window, or (2)
instrument `stack_oop_marks` writes directly with a temporary debug build
(log every stack push/pop with its oop-mark bit, keyed by method+bci) and
compare against `compute_param_oop_mask`-style ground truth for the specific
inline-cache/invoke codegen path, which would settle hypothesis (a) without
needing to catch a live crash at all — a static/logged trace of one run
through the `AbortedPOSTClient` code path, cross-referenced against the
bytecode's actual receiver-liveness, may be enough to confirm or refute it
directly.

## 2026-07-11: SB-CRASH-04 default-path fix landed — `precise_maps` now implies the full-GPR safepoint register spill

Implemented and merged the fix the mechanism-narrowing section above pointed
at, closing the specific, source-verified gap: `emit_pre_safepoint_spill`
(`jit/src/x64.rs`) is called at every GC-capable safepoint under the
default-on `precise_maps` path, and several call sites' own comments — the
MIC/PIC inline-dispatch cascade in particular ("the caller's register-only
oops must be spilled BEFORE the cascade to be visible to the conservative
scan") — document register spilling as their purpose. But the function's
actual GPR-blind-spill block was gated purely on the separate
`CRATONVM_JIT_SAFEPOINT_REG_SPILL` env var, off by default — so under
default settings, that documented protection never actually ran. Confirmed
from source, not inference: `precise_maps`'s own branch in the same function
only ever wrote the safepoint-id slot for oop-map lookup, nothing register-
related.

**The fix** (branch `fix/sb-crash-04-precise-reg-spill-20260711`, merged to
`dev`): fold `precise_maps` into the same decision that already gates
`safepoint_reg_spill`/`safepoint_reg_spill_all`, at the one place both get
computed (`jit/src/x64.rs`, `Compiler::new`). This makes the already-built,
already-documented-as-safe `=all` full-GPR spill ("fully conservative — the
scanner re-validates each slot via `is_object_address`... can only
over-retain, never corrupt" under the non-moving young sweep this VM runs
whenever JIT frames are live) run by default, everywhere
`emit_pre_safepoint_spill` is already called — roughly a dozen call sites
(invoke dispatch, the MIC/PIC cascade, allocation helpers,
checkcast/instanceof, self-recursive calls), all unchanged, since they all
already consult the same two fields. The FULL GPR file (not just
callee-saved) matters specifically because a receiver/args staged into
`ARG_REGS` immediately before a GC-capable call — exactly this crash's
disassembly shape, `rsi` being a caller-saved/argument register in the
x86-64 SysV ABI — is invisible to the callee-saved-only spill.
`CRATONVM_NO_PRECISE_REG_SPILL=1` reverts to the pre-fix, env-var-only
gating for bisection. Total diff: one new ~15-line function plus two lines
changed at the single construction site; every downstream consumer
(frame-size reservation, the spill loop itself, all call sites) inherited
the new behavior automatically.

### Verification

Built in an isolated worktree off `origin/dev` and validated extensively
before merging, given the correctness-critical/system-wide blast radius:

- **Correctness probe** (the 43-case `String(char[])` edge-case probe from
  the earlier fix in this doc): 43/43 pass, unchanged.
- **`cratonvm-jit` unit tests**: 893/893 pass.
- **`cratonvm-vm` unit tests**: 2181/2193 pass; the 12 failures are
  byte-for-byte identical (same test names) on a same-commit baseline
  binary without the fix — 8 are `lock_order` tests that only assert
  under debug-build `debug_assert!` (expected to fail when run
  `--release`, a test-methodology artifact unrelated to this fix), the
  rest are pre-existing/stale (a hardcoded native-count threshold, an
  unrelated BouncyCastle JIT carveout test, a system-streams test).
  Confirmed pre-existing, not a regression.
- **Crash-hunt repro** (the same `MultiMethodRunner` loop over
  `testAbortedPOSTOKSwallow`/`testAbortedPOST413Swallow`/
  `testAbortedPOSTOKNoSwallow` from the section above): 330 clean
  invocations on the fix binary across two independent runs (20 and 30
  rounds), zero crashes. **However**, a further 270 invocations against
  the *unpatched baseline* (90 + 180 rounds, built at the same commit)
  *also* completed with zero crashes — confirming, a fourth time this
  session, that this specific SIGSEGV's reproduction is too rare/
  timing-sensitive under current host conditions to serve as a live A/B
  confirmation either way. This fix is NOT validated by "the crash no
  longer reproduces" — it could not be reproduced on either binary today.
  It is validated by closing a source-verified gap between documented
  intent and actual default-path behavior, using an already-tested-safe
  mechanism, with zero regressions found elsewhere.
- **Full-class `TestSwallowAbortedUploads`** (`org.junit.runner.JUnitCore`,
  all 10 methods): the separate, already-documented, unrelated
  `testChunkedPUTNoLimit` "charset" `NullPointerException` crash (see the
  "blank-response follow-up" section above) is unchanged — identical
  crash, same signature, on both the fix and the baseline. Confirms this
  fix neither helps nor hurts that separate bug, as expected (it was
  already established as unrelated to the char[]-ctor/SB-CRASH-04 work).
- **Spring regression**: the same 20 `org.springframework.util`/`util.xml`
  classes used to validate the `String(char[])` fix earlier in this doc —
  17/20 clean, and the 3 with failures (`StringUtilsTests` 85/86,
  `ObjectUtilsTests` 131/140, `ExponentialBackOffTests` 9/10) show the
  *exact same* failure counts already established as a pre-existing,
  unrelated (`HIB-CV-32`-family) bug earlier in this doc — not a new
  regression.
- **Performance**: a call-heavy microbenchmark (200M iterations,
  monomorphic + polymorphic virtual dispatch — close to worst-case for
  this fix, since every dispatch is now a spilling safepoint) showed
  roughly 10-12% wall-clock overhead versus a same-commit baseline run
  back-to-back on the same host. Both runs were under heavy, uneven
  multi-tenant host contention (load average 5.9-9.9 on a 16-core shared
  box with 15+ other concurrent sessions), so the absolute delta is noisy,
  but the direction and rough magnitude are consistent with "spill 14
  GPRs at every GC-capable safepoint by default" being a real, non-trivial
  but bounded cost — the same category of tradeoff this codebase already
  accepted elsewhere for closing a different receiver-validation SIGSEGV
  (a 4.7x regression, later optimized down with a guarded fast path). No
  attempt was made to optimize this further (e.g. scoping the spill to
  only the specific call shapes that need it, rather than every safepoint)
  — left as a known, disclosed, opt-out-able cost; a future session could
  narrow the blast radius if the overhead proves unacceptable in practice.

**Not claimed**: that this is confirmed, with a live reproduction, to be
*the* fix for the swallow-upload SIGSEGV specifically. What's confirmed:
this closes a real, source-verified correctness gap in the codebase's own
documented SB-CRASH-04 mitigation, using a mechanism the codebase itself
already built and validated as safe, with no regressions across an
extensive test matrix — a legitimate, low-risk, high-value default-path
improvement to the general register-invisibility problem regardless of
whether it happens to be the exact mechanism behind this one crash.


## 2026-07-10: `accesslogvalve-rewritevalve-connection-failures.md` retired — its sixth-cause SIGSEGV catalogued here; stale Hibernate cross-reference in that doc corrected

`docs/known-issues/tomcat-08-07/accesslogvalve-rewritevalve-connection-failures.md`
has been retired and moved to
`docs/internal/tomcat-08-07/accesslogvalve-rewritevalve-connection-failures-RESOLVED.md`.
All five of its own locally-owned root causes (Layer 1 `URL.openConnection()`
CCE, Layer 2 `ByteBuffer.address`, Layer 3 `StringReader.read()`, the
cross-cutting `SocketWrapperBase.lock` NPE, and the fifth-cause JIT
`ConcurrentLinkedQueue` allocate-then-CAS miscompile) are FIXED on `dev`. Its
sixth cause was never a new/distinct bug — it's another confirmed occurrence
of this doc's "register-invisible JIT root" family, catalogued here rather
than left to sit in a now-closed doc:

- **Class/method:** `org.apache.catalina.valves.TestAccessLogValve`,
  crashed around test #8 (`test[7: Name[pct-A], Type[json]]`), on an
  `http-nio` worker thread, inside JIT-compiled code (`rip` in an anonymous
  executable region with no symbol table).
- **Register dump:** `rax=0x5 rdi=0x200106b6010 rsi=0x1894ed00
  r10=0x200417c27c0 r12=0x20042260010 r13=0x1 r14=0x200106b6010
  r15=0x20042260000` — `rdi`/`r10`/`r12`/`r14`/`r15` all share the
  `0x2000xxxxxxxx`-tagged-pointer shape every other live register in this
  family's crashes shares; `rsi` alone breaks it (`0x1894ed00`, a bare
  truncated value) — the same byte-for-byte signature as the DoHead doc and
  this doc's own `AbortedPOSTClient` occurrences above.
- Consistent with the family's known fingerprint: `CRATONVM_JIT_VIRTUAL_TIERUP=0`
  avoided the crash there too (got 5x further before hitting the unrelated
  STW cross-thread JIT takeover stall instead). Deliberately not patched, same
  reasoning as this doc's own residual: the real fix is precise JIT oop maps
  / shadow stack.

**Correction to the retired doc's own text:** that doc's sixth-cause section
also cited `docs/known-issues/hib-global-temptable-nondeterministic-sigsegv-20260710.md`
(Hibernate global-temp-table DDL) as a corroborating occurrence of this same
family. That citation is stale and should not be repeated: the Hibernate
SIGSEGV cluster was subsequently root-caused as a **different, unrelated**
bug (the guarded-inline-getfield JIT regression, already fixed by `93b33576`)
and moved to
`docs/internal/fixed-suite-bugs/hib-global-temptable-nondeterministic-sigsegv-20260710-RESOLVED.md`,
which explicitly refutes the global-temp-table/GC-root-pinning hypothesis for
that cluster. Don't count it among this family's confirmed occurrences.

**Updated confirmed-occurrence list for the register-invisible-JIT-root
family** (tomcat-08-07 investigation): DoHead
(`docs/internal/fixed-suite-bugs/dohead-jit-heap-corruption-register-invisibility-FIXED.md`),
`TestSwallowAbortedUploads`/`AbortedPOSTClient` (this doc, above), and
`TestAccessLogValve` (this section). The Hibernate global-temp-table cluster
is explicitly **not** part of this list per the correction above.

## 2026-07-11: RESOLVED — `sc_close` gap fixed now that the crash risk is gone; full class passes clean

Picked up the two remaining open threads from this doc's "6 genuine
failures" tally (the `sc_close` swallow-vs-abort behavioral gap and the
`AbortedPOSTClient` blank-response investigation) now that the dependency
chain above (`String(char[])` intrinsic, `LinkedBlockingDeque` synthetic
drop, SB-CRASH-04 full-GPR safepoint spill) had all landed.

**Re-confirmed the crash risk that blocked a clean `sc_close` fix earlier is
gone.** Built two throwaway worktrees, both applying the "obvious" fix this
doc's earlier sections stopped short of (remove `lingering_channel_close`'s
background drain thread entirely, keep only the write-side FIN):

- **Before `02b91823`** (SB-CRASH-04): the *exact same* code change SIGSEGVs
  3/3 times, deterministically during `testAbortedPOST413Swallow`'s teardown
  (`exit 139`, confirmed via `EXIT=$?` after the shell reported "Segmentation
  fault") — this is why earlier sections of this doc kept the background-drain
  workaround despite its known behavioral gap.
- **After `02b91823`** (i.e. on top of current `dev`, same code change): 3/3
  clean runs, zero crashes, `Tests run: 10, Failures: 3` (down from 6) every
  time. The two `AbortedUploadClient`/`AbortedPOSTClient` tests that want a
  `SocketException` on swallow-disabled abort still failed to get one, and
  `testChunkedPUTLimit` started throwing an *uncaught* `java.io.IOException:
  Socket write failed: Broken pipe` instead of a bare `AssertionError` — i.e.
  the connection genuinely *was* now getting reset (proving the architectural
  theory from the "2026-07-10 blocker #3" section correct: Tomcat's own
  swallow-input loop, once nothing upstream corrupts it, drains correctly
  when swallow is enabled and correctly does nothing when it's disabled,
  making the background-drain workaround unnecessary) — but
  `doTestChunkedPUT`'s own `catch (SocketException e)` doesn't match a plain
  `IOException`, so the reset surfaced as a test failure anyway.

**Second fix: `java.net.Socket`'s write path wasn't classifying connection
resets as `SocketException`.** `native-builtins/src/net_phase_e.rs::re1_socket_write_stream`
(the native behind `java.net.Socket.getOutputStream().write()` — the
*non-NIO* blocking socket API `doTestChunkedPUT`'s raw `Socket` uses, as
opposed to `native-io/src/socket_channel.rs`'s NIO `SocketChannel` path)
wrapped every write failure in a bare `java.io.IOException` via `ioex(...)`,
regardless of the underlying OS error. `native-io/src/socket_channel.rs::map_err`
already classifies `ConnectionAborted`/`ConnectionReset`/etc. as
`SocketException` for the NIO path; `re1_socket_write_stream` had no
equivalent. Fixed by classifying `BrokenPipe`/`ConnectionReset`/
`ConnectionAborted`/`NotConnected` and throwing a real, constructed
`java.net.SocketException` (via `ctx.new_object_initialized`, the same
pattern already used a few hundred lines up in the same file for
`ServerSocket.accept()`'s "Socket closed" case) instead of a generic
`IOException`.

**Result: both fixes together take the class from 6 failures to 0.**
Verified 4 times total (2 pre-merge on the isolated worktree, 1 after
merging a fast-moving `origin/dev` into the worktree mid-session, 1 more
after that merge — including a `native-io` `cargo test` run, 7/7 pass, with
the pre-existing `tomcat0807_http_lingering_channel_close_drains_peer_upload`
unit test updated to assert the new, narrower, still-true guarantee — a
peer's blocking read sees EOF after the write-side FIN — rather than the
removed "arbitrary-sized upload always drains without a reset" guarantee):

```
$ <EXE> ... org.junit.runner.JUnitCore org.apache.catalina.core.TestSwallowAbortedUploads
..........
Time: 113-127s (host-load-dependent)

OK (10 tests)
```

**Landed on `dev`** (commit `d88d4d15`, merged via
`fix/scclose-nobg-sbcrash04-20260711`):
`native-io/src/socket_channel.rs` (`lingering_channel_close` simplified —
no more background thread) and `native-builtins/src/net_phase_e.rs`
(`re1_socket_write_stream`'s error classification).

**What this doc's whole arc turned out to be**, end to end: a single
originally-reported symptom (unexpected `SocketException` with swallow
enabled) that required, in order: (1) a socket-close fix
(`420e4a55`/`27c3e9ac`) that *itself* introduced a narrower regression by
being too broad a workaround; (2) fixing an unrelated `ScheduledThreadPoolExecutor`
synthetic-layout bug just to get the test class booting; (3) fixing an
unrelated `ByteBuffer` heap-address bug to get past a connector `AbstractMethodError`;
(4) diagnosing (but initially misattributing to a "deep GC/JIT root-scanning
bug") a `SocketWrapperBase.lock`-is-null NPE that turned out to be the
`LinkedBlockingDeque` synthetic-native-layout bug family, cross-cutting into
three other Tomcat test classes; (5) a genuine interpreter-throughput gap
(`String(char[])` had no native intrinsic, ~5-6s for a 10M-char array,
tripping a 3s connector read-timeout) that needed a real performance fix,
not a workaround; (6) a real, hard-to-reproduce SIGSEGV in JIT-compiled code
(register-invisible root across a GC-capable safepoint, `SB-CRASH-04`) that
several independent sessions chased via crash-dump analysis before a
default-path fix landed; and only *then*, with all of the above as
prerequisites, (7) the original doc's actual subject — `sc_close`'s
swallow-vs-abort behavioral gap — could be fixed correctly (removing a
workaround, not adding one) plus (8) one small, previously-invisible
exception-classification bug in the non-NIO socket write path. Each of these
was a genuine, independent defect; none were red herrings, but several were
initially misdiagnosed as a different one of the others before being
untangled. Retiring this doc to `docs/internal/` — see that copy for the
canonical, closed version of this investigation.
