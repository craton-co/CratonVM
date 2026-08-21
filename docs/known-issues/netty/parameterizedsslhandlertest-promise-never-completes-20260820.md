# `ParameterizedSslHandlerTest` — a netty promise that never completes, ~1 run in 14

**Status: OPEN**, and NARROW. What is left of the
`openssl-key-material-and-engine-residuals` page's §D once the selector-registry
deadlock behind it was fixed. Filed separately because it is a different failure
with a different signature.

**Renamed and re-diagnosed 2026-08-21 with a second observation; the first
diagnosis was wrong.** The page was called
`…-closenotify-promise-never-completes` because the one run it had stalled
inside `testCloseNotifyNotWaitForResponse`. Resolving the BCI says it was never
the close path, and a 20-run loop landed on a different test method again.

## The rate, and the wait sites

| batch | runs | stalls | test in flight | resolved wait site |
|---|---:|---:|---|---|
| 2026-08-20 A/B | 8 | 1 | `testCloseNotifyNotWaitForResponse #9` | `testCloseNotify@230` |
| 2026-08-21 loop | 20 | 1 | `testAlertProducedAndSend #6` | `testAlertProducedAndSend@233` |

**2 stalls in 28 runs**, on a different parameterisation AND a different test
method each time. `javap -c -l` turns each BCI into one line of source, and the
two are not the same wait:

```java
// testCloseNotify:485 — BCI 230, the FIRST observation.
// Not the close path at all: the client's CONNECT future.
        }).connect(sc.localAddress()).syncUninterruptibly().channel();

// testAlertProducedAndSend:362 — BCI 233, the second.
// A promise the CLIENT's exceptionCaught completes when it sees an SSLException,
// i.e. when the server's fatal alert arrives and is surfaced.
            promise.syncUninterruptibly();
```

"The close_notify never arrives" was an artifact of one sample, and the method
name in the progress line is what produced it. **Resolve the BCI before reading
a method name as a diagnosis** — that is the transferable half of this entry.

What the two share is only the shape: one Java thread waiting on a netty promise
while the reactor is alive and selecting.

## Why this is not the deadlock that was fixed

The VM's own watchdog separates them with no further work:

| | the deadlock (6/8 runs, pre-fix) | this (2/28, post-fix) |
|---|---|---|
| `--stack-dump-on-timeout` | **0 thread(s) dumped** | **1 thread dumped**, with Java frames |
| native-call ring | two threads `STILL-IN-NATIVE(536s)` in `Selector.wakeup()` / `selectNow()` | **no thread still in a native** |
| event loops | parked in Rust on a `parking_lot` futex | parked in `NioIoHandler.select`, i.e. working |
| test in flight | always `reentryOnHandshakeCompleteNioChannel` | a different one each time |

"0 threads dumped" means no Java thread reached an interpreter dispatch point —
every thread was in Rust. Here the reactor threads are exactly where an idle
netty reactor belongs, and the dump names the waiting frame outright.

## What the second observation adds

`testAlertProducedAndSend` is not new to this codebase.
`t27_tls::reject_peer_with_fatal_alert`'s docstring records it as the test that
**blocked forever** (~170× HotSpot's 6 s) before CratonVM queued a fatal alert
at all, because it waits for an `SSLException` derived from that alert. The
alert record is emitted by the NEXT `wrap`, which netty performs only because
`setHandshakeFailure` → `ctx.close()` → `closeOutboundAndChannel` flushes an
empty buffer through the engine.

That is a race by construction: the alert is queued in one place and flushed by
a chain of netty callbacks in another, and an intermittent version of the old
failure is what a lost or too-late flush would look like. **A hypothesis, not a
finding** — it explains one of the two wait sites and says nothing about the
connect one.

## What it would take to close

- **Decide whether the two wait sites are one defect.** A connect future and an
  alert-delivery promise are different enough that assuming one cause is the
  mistake this page has already made once. More samples, binned by wait site, is
  the cheap way; at ~1 in 14 that is a 40-run loop, ~90 minutes.
- Instrument the SELECTOR, not the TLS layer. `CRATONVM_DBG_SELECTOR=1` prints
  `ENTER`/`EXIT n=` per cycle plus `SET_INTEREST` / `WAKEUP`, which answers "was
  readiness ever reported for this channel" for both wait sites. It is chatty
  enough to alter timing — keep the tail, and treat a run that stops stalling
  under it as evidence about the instrument, not about the defect.
- For the alert site specifically: does the queued fatal alert ever reach a
  `wrap`? A counter on `queue_fatal_alert` against records actually emitted
  separates "never queued" from "queued and never flushed".

## Repro

```bash
cd /data/cratonvm/apps/netty-suite-runner
./gen-openssl-args.sh -o /tmp/ossl.args
java @/tmp/ossl.args OpenSslAvailabilityProbe          # isAvailable MUST be true

<cratonvm> --java-home /data/toolchain/jdk-25 --Xmx 1500m \
    --stack-dump-on-timeout=420 \
    @/tmp/ossl.args -XX:+UseG1GC \
    -Djunit.jupiter.execution.timeout.mode=disabled \
    PerTestProgressRunner io.netty.handler.ssl.ParameterizedSslHandlerTest
```

At ~1 run in 14 this needs a loop, and the loop needs `PerTestProgressRunner`:
without it a killed run does not name the test it was inside.
`--stack-dump-on-timeout` is what makes a stalled run worth keeping.

## Related

- the retired `openssl-key-material-and-engine-residuals` write-up §D — the
  deadlock this is left over from, and the A/B that separates the two.
