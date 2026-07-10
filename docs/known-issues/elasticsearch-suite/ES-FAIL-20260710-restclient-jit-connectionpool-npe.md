# ES RestClient integ tests - JIT-only NullPointerException cluster

Status: OPEN

Date observed: 2026-07-10

## Signal

Under CratonVM **with JIT enabled**, `org.elasticsearch.client.RestClientSingleHostIntegTests`
and `org.elasticsearch.client.RestClientMultipleHostsIntegTests` fail with a
cluster of bare `java.lang.NullPointerException`s (no message, and the
reported bytecode PC does not correspond to a plausible NPE site — see
below). The **same classes pass this specific failure mode under
`--nojit`** (interpreter-only): 0 NPEs there. This is a JIT-specific
correctness bug, distinct from (and discovered while verifying the fix for)
the `AtomicMarkableReference` layout bug fixed in
`docs/internal/fixed-suite-bugs/restclient-singlehost-timeout-FIXED.md`
(that fix eliminated a hang under `--nojit` and a SIGSEGV crash under JIT
for the same test class — this is a NEW, separate residual exposed once the
crash stopped masking it).

## Repro

```
cratonvm --java-home /home/victor/jdk25 --Xmx 2g \
  -c <client/rest test classpath, see below> \
  org.junit.runner.JUnitCore org.elasticsearch.client.RestClientSingleHostIntegTests
```

Classpath: `apps/elasticsearch/client/rest/build/craton-testcp.txt` (module
must already be gradle-compiled; a working compiled copy was found on the
build host at
`/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch`
— strip CRLF line endings before joining with `:`, the file has Windows
line endings even on the Linux checkout).

Repeated 3x on `victor@20.83.144.174` (2026-07-10): consistently `rc=1`,
`Tests run: 13, Failures: 18-20` (small variance run to run — this is a
concurrent, multi-threaded async test, so exact failure count is not
expected to be perfectly stable). No crash (rc=139) in any of the 3 runs.

`RestClientMultipleHostsIntegTests` shows the same pattern under JIT
(`testNodeSelector`: `RuntimeException` caused by `NullPointerException`;
`testAsyncRequests`: bare `AssertionError`; `testSyncRequests`:
`CancellationException`) while passing 4/4 under `--nojit`.

## Evidence

`CRATONVM_DBG_ATHROW=1` on `testManyAsyncRequests` shows the NPE is thrown
inside Apache httpasyncclient's connection-release path:

```
ATHROW class=java/lang/NullPointerException msg="<no msg>"
  ATHROW-STK[15] org/apache/http/impl/nio/conn/PoolingNHttpClientConnectionManager.releaseConnection pc=437
  ATHROW-STK[14] org/apache/http/impl/nio/client/AbstractClientExchangeHandler.releaseConnection pc=191
  ATHROW-STK[13] org/apache/http/impl/nio/client/MainClientExec.responseCompleted pc=61
  ATHROW-STK[12] org/apache/http/impl/nio/client/DefaultClientExchangeHandlerImpl.responseCompleted pc=14
  ATHROW-STK[11] org/apache/http/nio/protocol/HttpAsyncRequestExecutor.processResponse pc=19
  ATHROW-STK[10] org/apache/http/nio/protocol/HttpAsyncRequestExecutor.inputReady pc=74
  ATHROW-STK[9] org/apache/http/impl/nio/DefaultNHttpClientConnection.consumeInput pc=228
  ATHROW-STK[8] org/apache/http/impl/nio/client/InternalIODispatch.onInputReady pc=8
  ATHROW-STK[7] org/apache/http/impl/nio/client/InternalIODispatch.onInputReady pc=8
  ATHROW-STK[6] org/apache/http/impl/nio/reactor/AbstractIODispatch.inputReady pc=35
  ATHROW-STK[5] org/apache/http/impl/nio/reactor/BaseIOReactor.readable pc=23
  ATHROW-STK[4] org/apache/http/impl/nio/reactor/AbstractIOReactor.processEvent pc=48
  ATHROW-STK[3] org/apache/http/impl/nio/reactor/AbstractIOReactor.processEvents pc=31
  ATHROW-STK[2] org/apache/http/impl/nio/reactor/AbstractIOReactor.execute pc=83
  ATHROW-STK[1] org/apache/http/impl/nio/reactor/BaseIOReactor.execute pc=16
```

**`pc=437` in `PoolingNHttpClientConnectionManager.releaseConnection` cannot
be a real NPE site.** Disassembly (`javap -p -c`,
`httpasyncclient-4.1.5.jar`) shows the method's bytecode is exactly 438
bytes (offsets 0-437), and offset 437 is a bare `return`:

```
       420: aload         10
       422: athrow
       423: aload         6
       425: monitorexit
       426: goto          437
       429: astore        11
       431: aload         6
       433: monitorexit
       434: aload         11
       436: athrow
       437: return
```

`437: return` is the very last instruction of the method and cannot throw
`NullPointerException`. `releaseConnection` is a `synchronized` method
whose body is entirely inside a `monitorenter`/`monitorexit` region with a
3-entry exception table (a `finally`-style monitor-release-on-exception
pattern). The reported `pc=437` for the innermost (throwing) frame is
strong evidence that CratonVM's JIT is reporting an **incorrect bytecode PC
for this frame at the moment of the fault** — most plausibly a PC/exception-
unwind-mapping gap specific to JIT-compiled `synchronized` methods with
`monitorenter`/`monitorexit` exception-table entries, rather than a genuine
null-dereference at that source location. This looks like it belongs to
the same general family as this codebase's other precise-JIT-PC-mapping
bugs (see `reference_jit_dispatch_exception_routing.md`,
`reference_coupled_deopt_moving_spine.md`,
`reference_guard_surviving_sr.md` in local session memory) but was not
chased further into JIT internals this session — the interpreter-mode fix
and the JIT-crash fix were verified working and are a complete, safe,
independently-valuable unit of work; this residual needs a dedicated
session with JIT disassembly (`CRATONVM_DBG_JIT_DISASM`) of
`PoolingNHttpClientConnectionManager.releaseConnection` to find the actual
PC-map gap.

Not yet established whether this NPE cluster pre-dates the
`AtomicMarkableReference` fix (masked by the SIGSEGV crash, which killed
the process before most test methods could run) or is somehow newly
introduced by it — but the thrown-from code (`PoolingNHttpClientConnectionManager`,
`AbstractNIOConnPool`, `CPool`/`CPoolEntry`) has no relation to
`AtomicMarkableReference` at all, and the interpreter-mode run of the exact
same test class with the exact same fix produces zero NPEs — both facts
point to a pre-existing, JIT-only bug rather than a regression from that
fix.

## Next steps

1. `CRATONVM_DBG_JIT_DISASM` dump of `PoolingNHttpClientConnectionManager.releaseConnection`
   (and ideally a second, simpler synchronized-method-with-finally repro
   outside ES/Apache) to compare the JIT's PC map against the bytecode
   exception table above.
2. Check whether the same defect explains other JIT-only NPEs in this run
   (`testGetWithBody`, `testUrlWithoutLeadingSlash`, `testHeaders`,
   `testPreemptiveAuthDisabled` all show bare NPEs too — worth checking
   whether they share the same shape, i.e. synchronized methods with
   exception tables, or are a different bug each).
3. Once fixed, re-run `RestClientSingleHostIntegTests` and
   `RestClientMultipleHostsIntegTests` under JIT and fold this doc into
   `docs/internal/`.
