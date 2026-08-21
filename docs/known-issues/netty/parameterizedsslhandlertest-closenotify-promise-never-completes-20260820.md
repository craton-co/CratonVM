# `ParameterizedSslHandlerTest.testCloseNotifyNotWaitForResponse` — a netty promise that never completes

**Status: OPEN**, and NARROW. Found 2026-08-20 on the Azure Linux host, as what
is left of the `openssl-key-material-and-engine-residuals` page's §D once the
selector-registry deadlock behind it was fixed. Filed separately because it is a
different failure with a different signature, and because the page it came from
would otherwise stay open on something it never described.

**1 stall in 8 runs**, against 6 in 8 for the same class on the pre-fix binary
in the same interleaved batch — and at a different test. See the retired page's
§D for the table.

## Why this is not the deadlock

The two are told apart by the VM's own watchdog without any further work, which
is the useful part of this record.

| | the deadlock (dev, 6/8 runs) | this (branch, 1/8 runs) |
|---|---|---|
| test in flight | `reentryOnHandshakeCompleteNioChannel` | `testCloseNotifyNotWaitForResponse` |
| `--stack-dump-on-timeout` | **0 thread(s) dumped** | **1 thread dumped**, with Java frames |
| native-call ring | two threads `STILL-IN-NATIVE(536s)` in `Selector.wakeup()` / `Selector.selectNow()` | **no thread still in a native** |
| event loops | parked in Rust, on a `parking_lot` futex | parked in `NioIoHandler.select`, i.e. working |

"0 threads dumped" means no Java thread reached an interpreter dispatch point —
every thread was in Rust. Here every event loop is exactly where an idle netty
reactor belongs, and one Java thread is waiting on a promise:

```
[89] io/netty/handler/ssl/ParameterizedSslHandlerTest.testCloseNotifyNotWaitForResponse@5
[90] io/netty/handler/ssl/ParameterizedSslHandlerTest.testCloseNotify@230
[91] io/netty/channel/DefaultChannelPromise.syncUninterruptibly@1
[93] io/netty/util/concurrent/DefaultPromise.syncUninterruptibly@1
[96] io/netty/util/concurrent/DefaultPromise.awaitUninterruptibly@31

tid=1320…1325  "multiThreadIoEventLoopGroup-82-{1..5}"  (5 frames each)
    io/netty/channel/nio/NioIoHandler.select@136
```

So this is not a lock: it is a completion that never arrives while the reactor
is alive and selecting. `testCloseNotify` uses `syncUninterruptibly()` on
`bind`/`connect`/`close` promises and `donePromise.get()` with no timeout, and
`@Timeout(30000)` is the only bound — which the page's own measurement protocol
disables (`-Djunit.jupiter.execution.timeout.mode=disabled`), on purpose, so a
stall stalls instead of being reported as a timeout with no cause. Under the
suite's ordinary settings this presents as a `TimeoutException`, not a hang.

## What it would take to close

- Get a second and third dump: **one observation is one observation**, and the
  BCI in the frame (`testCloseNotify@230`) has not been resolved to a specific
  `syncUninterruptibly()` call. `javap -c -l` on the test class turns the BCI
  into a line number and names which promise it is — bind, connect, or the
  `finally` block's `close()`.
- `CRATONVM_DBG_TLS_HS=1` on a stalled run: if the close_notify record is never
  produced or never consumed, that trace shows it, and this becomes a TLS
  close-path defect rather than a channel one.
- Rule the selector in or out properly. The event loops being IN `select` does
  not prove readiness is being delivered — a `channelRead` that never fires
  because `OP_READ` is never reported looks exactly like this from the outside.
  `CRATONVM_DBG_SELECTOR=1` prints `ENTER`/`EXIT n=` per cycle and settles it.

## Repro

```bash
cd /data/cratonvm/apps/netty-suite-runner
./gen-openssl-args.sh -o /tmp/ossl.args
java @/tmp/ossl.args OpenSslAvailabilityProbe          # isAvailable MUST be true

<cratonvm> --java-home /data/toolchain/jdk-25 --Xmx 1500m \
    --stack-dump-on-timeout=600 \
    @/tmp/ossl.args -XX:+UseG1GC \
    -Djunit.jupiter.execution.timeout.mode=disabled \
    PerTestProgressRunner io.netty.handler.ssl.ParameterizedSslHandlerTest
```

At roughly 1 run in 8 this needs a loop, and the loop needs
`PerTestProgressRunner`: without it a killed run does not name the test it was
inside, and the two stalls in the table above are indistinguishable.

## Related

- the retired `openssl-key-material-and-engine-residuals` write-up §D — the
  deadlock this is left over from, and the A/B that separates the two.
