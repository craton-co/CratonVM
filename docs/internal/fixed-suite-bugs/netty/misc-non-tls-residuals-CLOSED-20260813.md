# RESOLVED — the five "misc non-TLS residuals": two were already fixed, two were real defects, one is a throughput row re-filed

**Status:** ✅ CLOSED 2026-08-13 on `fix/netty-known-issues-retire-20260813`.
Retires `docs/known-issues/netty/misc-residuals-20260813.md`. All five rows now
match HotSpot 25 — two were already fixed on `dev`, two were real defects fixed
here, and the fifth was never a correctness failure and is closed by the
values-view carrier fix that landed the same day.

Re-measured on Azure host 2 (`20.80.105.49`, Linux) against `origin/dev`
`c4c972da7`, one class per process, HotSpot 25.0.3 on the identical classpath.

| class | page's claim (Windows, `ae2e1d9c8`) | CratonVM now | HotSpot now | verdict |
|---|---|---|---|---|
| `channel.unix.NativeInetAddressTest` | FAIL 1/2 | **2 ok** | 2 ok | already fixed on dev |
| `handler.codec.http2.Http2MultiplexTransportTest` | ABORTED 5 ok / 2 aborted | **6 ok / 3 aborted / 2 skipped** | 6 ok / 3 aborted / 2 skipped | matches exactly |
| `util.internal.JfrEventSafeTest` | FAIL 2 ok / 1 fail | **3 ok** | 3 ok | fixed here |
| `util.concurrent.DefaultThreadFactoryTest` | FAIL 4 ok / 1 fail | **5 ok** | 3 ok / 2 aborted | fixed here |
| `buffer.search.SearchProcessorTest` | FAIL 14 ok / 1 fail | 15 ok on a quiet box | 15 ok | fixed upstream, see below |

The page's own instinct — "no shared cause between these classes, treat each as
independent" — was right. Five rows, five unrelated stories.

---

## 1. `NativeInetAddressTest` — already fixed on dev

Fixed by `4a1d53355` ("Inet6Address keeps its scope id, and four more defects
behind it") before this page was re-measured. Nothing to do; the row was stale
by one day. See the retired `inet6address-drops-the-scope-id-FIXED-20260813`
write-up.

## 2. `Http2MultiplexTransportTest` — the delta was the platform, not the VM

On Linux both VMs find 11 tests, skip 2, and abort the same **3**:
`testSSLExceptionOpenSslTLSv13`, `testSSLExceptionOpenSslTLSv12`,
`testFireChannelReadAfterHandshakeSuccess_OPENSSL` — all three gated on
netty-tcnative/OpenSSL, which is absent here. CratonVM and HotSpot are identical
test for test.

The page measured 2 CratonVM aborts against 0 HotSpot aborts **on Windows**,
where the OpenSSL gate resolves differently. Whether a Windows-only delta
remains is untested here and would need a Windows binary to answer; on the
Linux host that carries this suite there is no gap to close.

## 3. `JfrEventSafeTest.enableDefaults` — `@Enabled(false)` was ignored

```
java.util.concurrent.ExecutionException: java.lang.Exception: Event mistakenly fired
	at io.netty.util.internal.JfrEventSafeTest.enableDefaults(JfrEventSafeTest.java:78)
```

The test opens a `RecordingStream`, commits a `@Enabled(false)`-annotated event
and an ordinary one, and asserts only the ordinary one is delivered.

`java_event_enabled` (`native-builtins/src/jfr.rs`) implemented the per-type
default as a hard-coded `true`. Half of that rule is right and was measured
against HotSpot when it landed: `jdk.jfr.Enabled` defaults to `true`, which is
why a bare `new Recording()` records custom events at all. The other half — a
class that says `@Enabled(false)` — had no representation, so an
explicitly-disabled type fired like any other.

Fixed by reading the annotation off the event class, memoised per class:
`jfr_event_type_default_enabled`. `jdk.jfr.Enabled` is `@Inherited` (unlike
`jdk.jfr.Name`, which the sibling `jfr_event_name` deliberately does not
inherit), so the lookup walks superclasses up to `jdk.jfr.Event`. The
admit-scan then reads "explicitly enabled by some recording, OR the type's own
default and not explicitly disabled".

## 4. `DefaultThreadFactoryTest` — a half-alive SecurityManager

```
expected: <java.lang.ThreadGroup[name=sticky,maxpri=10]>
 but was: <java.lang.ThreadGroup[name=wrong,maxpri=10]>
	at ...testDefaultThreadFactoryInheritsThreadGroupFromSecurityManager
```

The page flagged this row as "the odd one out: CratonVM actively fails a test
that HotSpot merely skips." That is exactly what it is, and the reason is a
deliberate CratonVM decision meeting an accidental gap.

HotSpot 25 skips both SecurityManager tests because `System.setSecurityManager`
there is `throw new UnsupportedOperationException` (JEP 486) and the tests catch
it into `Assumptions.assumeFalse`. **CratonVM deliberately did not adopt JEP
486** — its `Runtime.exec` / `ProcessBuilder.start` and Panama host-call gates
consult the installed manager for real, so refusing installation would remove
the only sandbox the VM has (the reasoning is recorded in full at
`security_manager::register_system_security`). So the tests run here.

Having decided to keep the manager alive, the VM then ignored the one hook that
manager has over thread construction: a `Thread` created with **no group of its
own** takes `SecurityManager.getThreadGroup()` first and the creating thread's
group only as a fallback — the JDK's rule up to 23, dropped in 24 along with the
SecurityManager itself. `populate_real_thread_holder`
(`native-builtins/src/lib.rs`), which is where CratonVM resolves a null group,
went straight to the creating thread's group.

Fixed there. The new branch is reachable **only once a SecurityManager is
installed**, which on HotSpot 25 cannot happen at all, so it cannot introduce a
divergence — it can only remove one. The default `SecurityManager.getThreadGroup()`
body is `Thread.currentThread().getThreadGroup()`, i.e. the fallback, so a
manager that does not override it changes nothing.

CratonVM now passes 5/5 where HotSpot passes 3 and skips 2. That is a *better*
result rather than a matching one, and it is the correct consequence of the
SecurityManager decision: the two skipped tests assert behaviour CratonVM
actually implements.

## 5. `SearchProcessorTest` — the ArrayList native-call tax, fixed upstream

Not a correctness failure, and not ungrouped either: every one of the 15 tests
computes the right answers, and the one that did not finish
(`testUniqueLen64Substrings[3] AHO_CORASIC`) was crossing the harness's
`-Djunit.jupiter.execution.timeout.default=120s` per-test cap.

```
java.util.concurrent.TimeoutException: testUniqueLen64Substrings(...Algorithm)
    timed out after 120 seconds
```

That it was only the cap, and not a wrong answer, was established rather than
assumed: the same 2016 needles run outside JUnit (`probes/AcProbe`, which
asserts the match position of every one) completed with **all 2016 assertions
passing**, in 523 s of Aho-Corasick factory construction against HotSpot's
254 ms.

### Where the time went

Splitting netty's `AhoCorasicSearchProcessorFactory` into its two build halves
and timing each (`probes/TrieSplit`, 60 needles, `origin/dev` `c4c972da7`):

| half | HotSpot | CratonVM | ratio |
|---|---|---|---|
| `buildTrie` — `ArrayList<Integer>`, 256 entries per trie node | 64 ms | 3283 ms | **51x** |
| `linkSuffixes` — `int[]` only | 21 ms | 15 ms | **0.7x** |

The `int[]` half is *faster* than HotSpot at this size. That one row rules out
the interpreter, the JIT and the algorithm, and leaves `ArrayList`.
`--dump-native-registry` over the same run agrees: **3 065 089 bridge-native
calls, of which 3 028 193 are three `java.util.ArrayList` methods** (`get`
1 015 328, `size` 1 009 899, `add` 1 002 966) — 98.8%. Against a byte-for-byte
equivalent hand-rolled list in the same process (`probes/AlSplit`),
`java.util.ArrayList` cost **957 ns/call** where the same body with no native
registration behind it cost **119 ns** — 8.0x, against HotSpot's 1.9x.

### It is fixed, by work that landed the same day

This row was first re-filed onto the then-OPEN
`arraylist-native-overhead-and-the-view-carrier-class-20260812` page as its
second witness. Before that landed, a concurrent branch fixed that page's
defect outright — collection views stopped being carrier-classed
`java.util.ArrayList`, which is what made the exact-class fast path sound — and
retired the page. See
[the record](arraylist-native-overhead-and-view-carrier-FIXED-20260813.md),
which already reports this class at **15/15**, and its two residual pages:
[collection-view carrier residuals](../../../known-issues/collection-view-carrier-residuals-20260813.md)
and [the VM-wide per-call cost](../../../known-issues/vm-per-call-dispatch-cost-20260813.md).

Re-measured here on the merged tree (that fix plus this branch's three):

```
arm                        wall    result   1-min load
HotSpot JDK 25             1.9 s   15/15    -
merged tree (this branch)  163 s   14/15    23-29
merged tree (this branch)  168 s   14/15    20-27
merged tree (this branch)  291 s   14/15    7 rising to 24
merged tree (this branch)  287 s   14/15    20-24
```

**Quote the cap, not the pass.** That record's own warning applies unchanged:
the fix moves the class from ~1.3x over the 120 s per-method cap to ~0.8x of
it. It clears the cap on a reasonably quiet box and does not on a busy one, and
a reader who sees this class red under a full parallel suite has not found a
regression. This host was at load 20-29 throughout this branch's runs.

---

## Repro

```bash
cd apps/netty-suite-runner
printf '%s\n' io.netty.buffer.search.SearchProcessorTest \
  io.netty.channel.unix.NativeInetAddressTest \
  io.netty.handler.codec.http2.Http2MultiplexTransportTest \
  io.netty.util.internal.JfrEventSafeTest \
  io.netty.util.concurrent.DefaultThreadFactoryTest > /tmp/misc.txt
CV_BIN=<cratonvm> bash run-netty-suite.sh --list /tmp/misc.txt --gc zgc --shards 1 --timeout 180 --out /tmp/repro
```

HotSpot baseline, same classpath:

```bash
CP=$(sed -n 2p common.args)
java -cp "$CP:." -Duser.timezone=UTC -Djunit.jupiter.execution.timeout.default=120s \
  CratonRunner <class>
```
