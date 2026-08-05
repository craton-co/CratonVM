# `TestSsl.testPost` — the connection that dies mid-transfer is a GC pause tripping Tomcat's wall clock (SETTLED)

| | |
|---|---|
| **Status** | ✅ SETTLED — mechanism confirmed by experiment, 2026-08-05. Not a TLS defect. |
| **Filed** | 2026-08-04 as `docs/known-issues/tomcat/testssl-testpost-connection-dies-under-concurrent-bulk-tls-20260804.md` |
| **HotSpot** | PASS. `testPost[JSSE]` in 4.2–5.0 s (measured here, `RunMethods` on the method alone) |
| **CratonVM** | `testPost[JSSE]` 199–241 s, fails intermittently |
| **Residuals** | two, both pre-existing and both filed elsewhere — see "What is left" |

## What the original page asked, and the answer

It proposed one experiment: *"raise `connectionTimeout` well past 20 s in a local
copy of the test — if the failures stop, the mechanism is settled."* It also
said that if raising it did **not** stop them, "this becomes a real
connection-handling defect under concurrent bulk TLS, which is a much more
serious finding."

The failures stop. `probes/TlsPostShapeProbe.java::timeoutDiscriminator` runs
testPost's exact client — 8 threads, 16 MiB each, 128 KiB blocks with
`sleep(10)`, single-byte readback — against a connector whose
`connectionTimeout` is the only thing that varies. Arms **interleaved**, same
binary, same machine, no `CRATONVM_DBG` (see "Traps" below):

| `connectionTimeout` | rounds | dead connections | rounds affected |
|---|---|---|---|
| **20 000** (what testPost sets) | 6 | **9** | 3 of 6 |
| **300 000** | 6 | **0** | 0 of 6 |

## The mechanism

Tomcat's Poller times a connection out on **wall clock**:

```java
// NioEndpoint.java, Poller.timeout()
long delta = now - socketWrapper.getLastWrite();
long timeout = socketWrapper.getWriteTimeout();
if (timeout > 0 && delta > timeout) { writeTimeout = true; }
```

`NioEndpoint` seeds both the read and the write timeout from
`getConnectionTimeout()` (`NioEndpoint.java:584-585`), so testPost's
`connector.setProperty("connectionTimeout", "20000")` sets exactly this window.

A stop-the-world collection stops the client threads **and** Tomcat's worker and
poller threads together — but not the clock. So a pause long enough (or a
cluster of them) makes the server time out a connection nobody was neglecting,
and close it. The client, which was frozen in the same pause, wakes to a stream
that has ended: a clean EOF at whatever byte it had reached, correct data up to
that point, and no client-side exception. That is the reported symptom exactly.

Measured pause distribution on runs that **passed** (`CRATONVM_DBG=gcpause`):

| run | collections | longest pause | total |
|---|---|---|---|
| 1 | 13 | **6 466 ms** | 18.1 s |
| 2 | 23 | 4 102 ms | 19.1 s |
| 3 | 24 | 2 982 ms | 31.3 s |
| 4 | 20 | 5 246 ms | 23.6 s |

A single 6.5 s pause against a 20 s window, on a run with margin to spare.
HotSpot runs the identical wall-clock check and never trips it, because its
pauses are milliseconds.

**The clustering is what makes this conclusive.** In the worst round, 7 of 8
connections died at nearly the same offset — three at the *identical* byte
1 605 552, the rest within ~180 KB:

```
[timeout] round=0 Byte in position [1605552] had value [-1]
[timeout] round=0 Byte in position [1572784] had value [-1]
[timeout] round=0 Byte in position [1556400] had value [-1]
[timeout] round=0 Byte in position [1605552] had value [-1]
[timeout] round=0 Byte in position [1605552] had value [-1]
[timeout] round=0 Byte in position [1736624] had value [-1]
[timeout] round=0 Byte in position [12812208] had value [-1]
```

Independent faults in a TLS or socket path do not synchronise on one offset
across seven sockets. One global stall does.

## A correction to the original page

It concluded: *"if the transfer were not 50× slow, the timeout could not be
reached in the first place"* — i.e. that throughput is the thing to fix because
speed is the cause. The data does not support that.

Across the twelve discriminator rounds, **wall time does not predict failure**.
The round with 7 deaths was the *fastest of all twelve* (198.3 s). The two
slowest rounds (302.0 s and 286.2 s) had zero. Speed changes how many seconds of
window a pause can land in; it is not the proximate cause. The direct lever is
**pause length**, which is a GC matter, not a TLS one.

Throughput work is still worth doing — it lowers exposure and lowers the
allocation rate that drives the pauses — but it will not, on its own, make this
test reliable, and a change that speeds testPost up should not be read as having
fixed it.

## What was actually fixed here

Two real defects were found while attributing the slowness. Neither causes the
connection death; both are genuine and are merged with this page.

### 1. Per-element array copies on the bulk TLS paths

Four sites moved bytes one at a time through virtual `get_array_element` /
`set_array_element` (plus a `Value` box each), where a bulk intrinsic does a
single `copy_nonoverlapping`:

* `t27_tls::bb_bytes_range` / `bb_put_bytes` heap arms — the rustls `SSLEngine`'s
  application-data path, i.e. **every byte Tomcat's JSSE connector wraps or
  unwraps**, twice (app buffer ↔ net buffer). Tomcat hands the engine real-JDK
  `HeapByteBuffer`s (see `08-jsse-nio-sslengine-bytebuffer-FIXED.md`), so this is
  always the heap arm.
* `SSLSocketInputStream.read([BII)` and `SSLSocketOutputStream.write([BII)`.
* `net_phase_e::java_byte_array_to_vec` — under every
  `Socket.getOutputStream().write(byte[],int,int)` in the VM.

`native-io`'s socket-channel buffer helpers had already been migrated to these
intrinsics; the TLS engine's own helpers were missed at the time.

Measured (`TlsPostShapeProbe.shapes`): bulk-8 KiB readback **126 → 86 ns/byte**,
write phase **60.5 → 31.6 s** summed across 8 threads.

The bulk arms also stopped lying about how much they moved. The old loops let
the per-element guards drop an overrunning tail and still returned the full
requested count — the read side then padded with zeros and handed them to rustls
as plaintext. Pinned by `bb_heap_arms_report_only_the_bytes_that_fit`, which
injects the overrun.

### 2. Native field reads on synthetic receivers re-took a global lock, forever

`resolve_field_descriptor_byte_cached` refused to memoize any answer derived from
a `is_synthetic_stub` class, because a stub can later be promoted to the real
class. So every `NativeContext::get_field` on a synthetic receiver re-took the
process-global `class_manager` read lock to re-derive an answer that in practice
never changes — and CratonVM's own stream stand-ins (`SSLSocketInputStream` and
friends) are all synthetic. `testPost` takes that lock ~134 million times.

The fix makes the memo *safe* rather than forbidden: `class_origin_epoch()`,
bumped by `Class::set_origin` (the documented single writer of the stub flag) and
by `ClassStore::add` (a new class can be a descendant's previously-missing
ancestor). The thread-local descriptor tiers carry the epoch, so a transient
answer is memoized per thread and retires the moment provenance moves.

Measured with `TlsPostShapeProbe.nativeFloor`, which prices `available()` (one
field read + one table lookup) against `flush()` (a registered **no-op** native
on the same receiver, so the difference is the body and nothing else), arms
interleaved, 3 runs each:

| | 1 thread | 8 threads |
|---|---|---|
| before | 85.5 / 162.1 / 167.5 → **~138 ns** | 588.5 / 1311.8 / 1165.5 → **~1022 ns** |
| after | −19.6 / 5.6 / 109.3 → **~32 ns** | 212.3 / 338.4 / 403.3 → **~318 ns** |

~70% off the field-read body; the negative value means the field read became
indistinguishable from the empty native. This is VM-wide, not TLS-specific.

## Where the single-byte cost actually goes

`testPost` reads 16 MiB **one byte at a time** on each of 8 threads — ~134
million native calls. Priced against the no-op native on the same receiver:

| | 1 thread | 8 threads |
|---|---|---|
| Java→native transition (`flush()`, empty body) | **549 ns** | **2155 ns** |
| field read + readahead lookup (`available() − flush()`) | 181 ns | 1112 ns |
| the byte copy (`read() − available()`) | ~84 ns | ~0 (noise) |

**67% of it is the transition itself**, before the callback body runs. No change
inside the TLS layer can reach that; it is the pre-existing native-call floor
(`perf/uninlined-call-floor-*`, `perf/per-call-dispatch-floor-*`). The numbers
above are offered to that work.

Two candidate explanations were tested and **refuted** — recorded so nobody
spends the evening on them again:

* **The readahead's process-global mutex.** Sharding it 64 ways by stream id
  moved `available − flush` from 167.5 → 180.7 ns (1 thread) and 1165.5 → 1111.7
  (8 threads): inside the noise. Reverted; the A/B is in a comment at
  `servlet::tls_readahead`.
* **The tls_id side-table fallback.** Counters over a full run: `field_hits ==
  calls` exactly (16 777 216 of 16 777 216). It never fires. The readahead is
  also working as designed — 1024 refills for 16 MiB, average 16 384 bytes.

## Traps this investigation hit

* **`CRATONVM_DBG` output perturbs the failure.** Six consecutive testPost runs
  with `gcpause` + Tomcat FINE logging on produced **zero** failures, while the
  same binary fails ~1 in 3 clean. The instrumented runs were also *slower*
  (272–396 s vs 199–241 s), so this is not "faster ⇒ safer". Reproduce clean.
* **Tomcat's Poller timeout branch has no log statement** (`NioEndpoint.java`
  ~1219-1235). The original page's "no server-side timeout or close message
  appears in the log window" is true at *every* log level, so no amount of
  server logging can confirm this. That is why the discriminator exists.
* **`samply` needs ETW elevation on Windows** and cannot profile this.
* The `SocketTimeoutException` that shows up in a FINE log is `unlockAccept`
  during `Tomcat.stop` at teardown — unrelated to the mid-transfer close.

## What is left

Neither residual is a TLS problem, and neither is new:

1. **Young-generation pause length.** Every collection in these runs logs
   `[moving-young] fallback … NON-MOVING sweep (no compaction, free-list
   allocation)`, reasons `innermost-rbp-belongs-to-unguarded-callee` and
   `xt-helper-window-conservative-scan` — a live JIT frame could not prove a
   complete rewritable root map. Pauses of 2.9–6.5 s against a 20 s connector
   window are what makes this test flake. See the moving-young work and
   `reference_non_moving_young_sweep_degenerates_into_an_unusable_free_list`.
2. **The native-call floor** — 549 ns for an empty native at 1 thread, 2155 ns
   at 8. Owned by the call-floor worktrees named above.

## Reproduction

```bash
apps/tomcat-suite-runner/run-one.ps1 -Main RunMethods -Args2 org.apache.tomcat.util.net.TestSsl,testPost -Exe <cratonvm.exe>
```

The discriminator and the cost decomposition:

```bash
apps/tomcat-suite-runner/run-one.ps1 -Main RunMethods -Args2 org.apache.tomcat.util.net.TlsPostShapeProbe,timeoutDiscriminator -ExtraCp <shapecls> -Exe <cratonvm.exe>
```

`PROBE_CONNECTION_TIMEOUT` (default 20000) and `PROBE_ROUNDS` (default 3) are
read from the environment — the suite runner fixes the JVM argument list on
purpose, and a repro that edits it is a different experiment. `nativeFloor` and
`shapes` are the other two methods on that probe.

## Not to be confused with

* `testClientInitiatedRenegotiation[JSSE]` — the by-design TLS 1.2 renegotiation
  gap, `testssl-client-initiated-renegotiation-FIXED.md`.
* The 90 `gen_heap::get_field: out-of-bounds field read` warnings the original
  page listed as a residual. Already fixed on `dev` by `37bc6fb84` before this
  work started; verified 0 warnings across all baseline runs here.
* The four `TestSsl` failures on the Azure Linux fixture,
  `testssl-four-new-failures-on-dev-20260804-FIXED.md`.
