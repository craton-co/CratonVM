# ES RestClient integ tests - JIT-only NullPointerException cluster

Status: FIXED (2026-07-10)

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


## Fix applied (2026-07-10)

**Root cause confirmed: hypothesis (b), but not where the doc's own lead pointed.**
`PoolingNHttpClientConnectionManager.releaseConnection` itself is NEVER
JIT-compiled — it contains an `athrow` plus a non-empty exception table, and
`jit/src/lib.rs`'s `try_compile_inner` bails wholesale on that combination
(the JIT has no exception-table-aware codegen at all). So the
`monitorenter`/`monitorexit`/catch-all-rethrow shape described above, and
the `pc=437` vs `pc=436` off-by-one in the `CRATONVM_DBG_ATHROW` dump, are
both real but are **not** the JIT-correctness bug — `releaseConnection`
always runs interpreted, and the `pc=437` artifact is a pre-existing,
JIT-independent diagnostic quirk (the `CRATONVM_DBG_ATHROW` printer reads a
frame's `.pc` field, which the interpreter's dispatch loop advances to the
next instruction before executing `athrow`'s body, instead of
`last_instr_pc`; reproduces identically under `--nojit`). Neither of those
findings needed a code change — they explain why the doc's specific lead
(monitor/exception-table PC-mapping) was a red herring, not a fix.

**The actual bug** is a general JIT null-check-elimination soundness gap in
`jit/src/x64.rs`'s `preceding_aload_nonnull_local` (used by both `ifnull`
and `ifnonnull` codegen to decide whether a `TEST reg,reg; Jcc` can be
elided because the tested value is provably non-null). It identified the
bytecode instruction immediately preceding a branch by reading a raw byte
at `code[pc - 1]` / `code[pc - 2]` and matching it against the `aload_0..3`
/ `aload <u8>` opcode encodings — **without verifying that offset was a real
instruction boundary**. A multi-byte instruction's trailing operand byte can
numerically collide with those opcodes. Concretely:
`org.apache.http.client.methods.HttpRequestWrapper.getParams()` (httpclient
4.5.14) is the standard lazy-init idiom:

```
0: aload_0
1: getfield #42          // params
4: ifnonnull 25
7: aload_0
8: aload_0
9: getfield #6            // original
12: invokeinterface HttpRequest.getParams
17: invokeinterface HttpParams.copy
22: putfield #42          // params
25: aload_0
26: getfield #42          // params
29: areturn
```

`getfield #42` encodes as bytes `[0xB4, 0x00, 0x2A]` — the low byte of
constant-pool index 42 is `0x2A`, which is *also* the `aload_0` opcode.
`preceding_aload_nonnull_local(code, pc=4)` read `code[3] == 0x2A`, wrongly
concluded the `ifnonnull`'s operand came from `aload_0` (`this`, always
non-null), and the codegen elided the null check into an **unconditional**
`JMP`, permanently skipping the lazy-initialization branch. Confirmed via
`CRATONVM_DBG_JIT_DISASM=HttpRequestWrapper.getParams` on the real
httpclient jar: before the fix, the compiled method's `ifnonnull` site
computed `getfield params` and then executed a bare `E9` (`JMP rel32`) with
no preceding `TEST`; after the fix, it emits the correct
`TEST RCX,RCX; JNE`. Every fresh `HttpRequestWrapper` (`params == null`)
JIT-compiled through this path returned `null` from `getParams()` instead
of lazily constructing it — the interpreted caller (e.g.
`RequestClientConnControl.process`) then legitimately NPE'd on
`request.getParams().getParameter(...)`, which is what produced the bare
`NullPointerException`s. `array_receiver_local` (the sibling helper used for
array null-check elision) had already been hardened against this exact
class of bug (`instruction_start_map`, "SOUNDNESS FIX (array_receiver_local)"
in `jit/src/x64.rs`) — `preceding_aload_nonnull_local` was the one sibling
site that had not received the same fix.

**Fix**: `preceding_aload_nonnull_local` now validates its backward-derived
candidate against `instruction_start_map` (the same forward-walked
instruction-boundary bitmap `array_receiver_local` already uses) before
trusting it, exactly mirroring that function's existing soundness pattern.
On any misalignment it returns `None`, so the caller conservatively keeps
the runtime `TEST`+`Jcc` (always sound, matching the function's own
"bailing out is always safe" doc comment). New regression tests added:
`x64::preceding_aload_nonnull_local_tests::{rejects_getfield_operand_collision,
accepts_genuine_aload_0, accepts_genuine_aload_u8,
rejects_putfield_operand_collision}`.

**Before/after** (`RestClientSingleHostIntegTests`, JIT enabled, 3 repeated runs each):
- Before: `Tests run: 13, Failures: 5` (consistently), including 3 spurious
  `NullPointerException`s all reading `"Cannot invoke
  \"org.apache.http.params.HttpParams.getParameter(String)\" because the
  return value of \"org.apache.http.HttpRequest.getParams()\" is null"` —
  `testAuthCredentialsAreNotClearedOnAuthChallenge`, `testPreemptiveAuthDisabled`,
  `testHeaders` (matching 2 of the doc's originally-listed "other JIT-only
  NPEs"; `testGetWithBody`/`testUrlWithoutLeadingSlash` are in the same
  class and cleared too, see below).
- After: `Tests run: 13, Failures: 2` (consistently across 3 runs) —
  `testPreemptiveAuthEnabled` and `testAuthCredentialsAreNotClearedOnAuthChallenge`
  both fail with `AssertionError: Expected: a string starting with "Basic" but: was
  null` — this **exactly matches `--nojit`'s pre-existing failure set**
  (same 2 tests, same assertion, unrelated pre-existing bug referenced by
  this doc's own intro as the separately-fixed `AtomicMarkableReference`
  cluster's neighbor). Zero `NullPointerException`s in any of 3 runs.
  `testManyAsyncRequests` intermittently shows 1-2 suppressed
  `org.apache.http.ConnectionClosedException: Connection is closed`
  failures out of many concurrent requests (0-2 across 6 runs total) —
  confirmed unrelated to this bug (not an NPE) and consistent with the
  doc's own note that this is "a concurrent, multi-threaded async test, so
  exact failure count is not expected to be perfectly stable."
- `RestClientMultipleHostsIntegTests`: `OK (4 tests)` after the fix,
  matching `--nojit`. (This class alone, run in isolation, did not
  reproduce the doc's originally-reported failures on this session's
  baseline binary either — likely because 4 requests alone don't cross the
  tiered-compile threshold to JIT-compile `HttpRequestWrapper.getParams()`
  within a fresh process; the failures were previously observed as part of
  a longer, already-warmed-up suite run. Running it back-to-back with
  `RestClientSingleHostIntegTests` in one process — the realistic
  full-suite condition — passes cleanly on the fixed binary, 17/17 modulo
  the same 2 pre-existing `AssertionError`s.)
- `testGetWithBody`/`testUrlWithoutLeadingSlash` (also listed in this doc
  as JIT-only failures): both are methods of `RestClientSingleHostIntegTests`
  and are absent from every fix-binary failure list above — same root
  cause, same fix.

**Regression check**: `cargo test -p cratonvm-jit --lib` — 893/893 passing
(889 pre-existing + 4 new), including the existing `test_ifnonnull_taken`/
`test_ifnull_taken`/`test_ifnull_with_aconst_null` end-to-end JIT-compile
tests (the legitimate elision case, `aload_N` directly preceding the
branch, still elides correctly) and all `null_check_elim`/
`array_receiver_local_tests` suites. (`jit/tests/ir_vs_singlepass.rs` fails
to compile on `dev` independent of this fix — confirmed via `git stash` —
a pre-existing signature mismatch from unrelated concurrent work, not
touched here.) Broader synchronized/concurrency-heavy smoke pass on the
Tomcat harness (`org.apache.tomcat.util.collections.TestSynchronizedQueue`,
`TestSynchronizedStack`, `org.apache.tomcat.util.threads.TestLimitLatch`,
`org.apache.tomcat.util.concurrent.TestKeyedReentrantReadWriteLock`,
`TestConcurrentLruCache`, `TestConcurrentDateFormat`,
`TestConcurrentMessageDigest`) — all pass under the fixed JIT binary.

Fix commit: see `jit/src/x64.rs`'s `preceding_aload_nonnull_local`.
